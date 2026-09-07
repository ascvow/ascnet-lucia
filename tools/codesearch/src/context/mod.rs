//! 上下文组装与版本化读取。
//!
//! Context Builder 只**选择和组织**已经存在的代码，不生成任何解释、调用关系或代码。
//!
//! 组装步骤：
//!
//! 1. 把候选代码块还原为元数据行，并丢弃已经失效的版本；
//! 2. 需要时扩展到包含该片段的函数或类型（默认最多一跳）；
//! 3. 合并同一版本内相邻或重叠的范围；
//! 4. 按内容去重；
//! 5. 查询要求测试时为测试候选保留额度；
//! 6. 按相关性与长度预算选择结果，只在行边界缩减内容。
//!
//! 所有返回的代码都从对应版本的源码快照切片，因此代码、行号与 `file_hash` 必定同源。

use std::collections::HashSet;

use crate::config::RetrievalConfig;
use crate::error::{Error, Result};
use crate::retrieval::{Candidate, Intent, QueryPlan};
use crate::store::query::ChunkRow;
use crate::store::Store;
use crate::types::{
    estimate_tokens, ChunkKind, MatchSource, ReadExpand, ReadRequest, ReadResponse, SearchResult,
};

/// `code_ref` 的格式前缀。格式变化必须换前缀，避免旧引用被误解析。
pub const CODE_REF_PREFIX: &str = "cs1";

/// `code_ref` 中携带的内容哈希前缀长度。
const HASH_PREFIX_LEN: usize = 16;

/// 上下文扩展的最大跳数。第一版只做父子包含关系的一跳扩展。
const MAX_EXPAND_HOPS: usize = 1;

/// 测试意图下为测试候选保留的结果比例（分母）。
const TEST_RESERVE_DIVISOR: usize = 3;

/// 指向确定源码版本的代码引用。
///
/// 形如 `cs1:<chunk_id>:<file_hash 前 16 位>:<start_line>-<end_line>`。
/// 只有 "路径 + 行号" 是不够的：内容哈希把引用钉在一个具体版本上，
/// 文件被修改后旧引用不会被当作最新代码返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRef {
    /// 代码块 ID（chunk_version_id）。
    pub chunk_id: i64,
    /// 内容哈希前缀。
    pub hash_prefix: String,
    /// 起始行（从 1 开始，含）。
    pub start_line: u32,
    /// 结束行（从 1 开始，含）。
    pub end_line: u32,
}

impl CodeRef {
    /// 构造引用，哈希自动截断到固定前缀长度。
    pub fn new(chunk_id: i64, file_hash: &str, start_line: u32, end_line: u32) -> Self {
        let hash_prefix = file_hash
            .get(..HASH_PREFIX_LEN)
            .unwrap_or(file_hash)
            .to_string();
        Self {
            chunk_id,
            hash_prefix,
            start_line,
            end_line,
        }
    }

    /// 解析引用文本。
    ///
    /// 格式错误返回 [`Error::InvalidReference`]，调用方可据此区分
    /// "引用写错了" 与 "引用指向的版本已消失"（[`Error::ReferenceExpired`]）。
    pub fn parse(text: &str) -> Result<Self> {
        let invalid = |detail: &str| {
            Error::InvalidReference(format!(
                "{detail}；期望格式 {CODE_REF_PREFIX}:<chunk_id>:<hash>:<start>-<end>"
            ))
        };
        let trimmed = text.trim();
        let mut parts = trimmed.split(':');
        let prefix = parts.next().unwrap_or_default();
        if prefix != CODE_REF_PREFIX {
            return Err(invalid(&format!("未知的引用前缀 `{prefix}`")));
        }
        let chunk_id: i64 = parts
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| invalid("代码块 ID 不是整数"))?;
        if chunk_id <= 0 {
            return Err(invalid("代码块 ID 必须为正数"));
        }
        let hash_prefix = parts
            .next()
            .filter(|value| !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| invalid("内容哈希前缀缺失或不是十六进制"))?
            .to_string();
        let lines = parts.next().ok_or_else(|| invalid("行范围缺失"))?;
        if parts.next().is_some() {
            return Err(invalid("引用包含多余字段"));
        }
        let (start, end) = lines
            .split_once('-')
            .ok_or_else(|| invalid("行范围必须形如 <start>-<end>"))?;
        let start_line: u32 = start.parse().map_err(|_| invalid("起始行不是整数"))?;
        let end_line: u32 = end.parse().map_err(|_| invalid("结束行不是整数"))?;
        if start_line == 0 || end_line < start_line {
            return Err(invalid("行号必须从 1 开始且结束行不小于起始行"));
        }
        Ok(Self {
            chunk_id,
            hash_prefix,
            start_line,
            end_line,
        })
    }
}

impl std::fmt::Display for CodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{CODE_REF_PREFIX}:{}:{}:{}-{}",
            self.chunk_id, self.hash_prefix, self.start_line, self.end_line
        )
    }
}

/// 组装输入。
#[derive(Debug)]
pub struct AssembleInput<'a> {
    /// 融合后的候选，按相关度降序。
    pub candidates: &'a [Candidate],
    /// 查询分析结果。
    pub plan: &'a QueryPlan,
    /// 最终返回的结果数上限。
    pub max_results: usize,
    /// 估算 token 预算。
    pub token_budget: usize,
    /// 检索配置。
    pub config: &'a RetrievalConfig,
}

/// 组装结果。
#[derive(Debug, Default)]
pub struct AssembleOutput {
    /// 最终结果。
    pub results: Vec<SearchResult>,
    /// 是否有结果因预算被丢弃或截断。
    pub truncated: bool,
    /// 结果内容合计估算 token 数。
    pub total_estimated_tokens: usize,
}

/// 组装过程中的一个待选片段。
#[derive(Debug, Clone)]
struct Selection {
    chunk_id: i64,
    revision_id: i64,
    path: String,
    language: crate::types::Language,
    kind: ChunkKind,
    symbol: Option<String>,
    file_hash: String,
    start_byte: i64,
    end_byte: i64,
    start_line: u32,
    end_line: u32,
    score: f64,
    matched_by: Vec<MatchSource>,
    exact: bool,
}

impl Selection {
    fn from_row(row: &ChunkRow, candidate: &Candidate) -> Self {
        Self {
            chunk_id: row.id,
            revision_id: row.revision_id,
            path: row.path.clone(),
            language: row.language,
            kind: row.kind,
            symbol: row.symbol.clone(),
            file_hash: row.file_hash.clone(),
            start_byte: row.start_byte,
            end_byte: row.end_byte,
            start_line: row.start_line,
            end_line: row.end_line,
            score: candidate.score,
            matched_by: candidate.matched_by.clone(),
            exact: candidate.exact,
        }
    }
}

/// 组装最终结果。
pub fn assemble(store: &Store, input: &AssembleInput<'_>) -> Result<AssembleOutput> {
    if input.candidates.is_empty() {
        return Ok(AssembleOutput::default());
    }
    let ids: Vec<i64> = input.candidates.iter().map(|c| c.chunk_id).collect();
    let rows = store.chunks_by_ids(&ids)?;

    // 只保留仍然有效的当前版本：旧向量即使暂时残留在 USearch，也会在这里被过滤掉。
    let mut selections: Vec<Selection> = Vec::new();
    for candidate in input.candidates {
        let Some(row) = rows.iter().find(|row| row.id == candidate.chunk_id) else {
            continue;
        };
        if !row.is_current {
            continue;
        }
        selections.push(Selection::from_row(row, candidate));
    }
    if selections.is_empty() {
        return Ok(AssembleOutput::default());
    }

    expand_to_containing_symbols(store, &mut selections)?;
    let mut merged = merge_adjacent(selections);
    // 融合分数降序；同分时按路径与起始行保证确定性。
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.path.cmp(&b.path))
            .then(a.start_line.cmp(&b.start_line))
    });
    let ordered = reserve_test_slots(merged, input.plan, input.max_results);

    let mut output = AssembleOutput::default();
    let mut seen_content: HashSet<String> = HashSet::new();
    let mut used_tokens = 0usize;

    for selection in ordered {
        if output.results.len() >= input.max_results {
            output.truncated = true;
            break;
        }
        let content = store.chunk_content(
            selection.revision_id,
            selection.start_byte,
            selection.end_byte,
        )?;
        // 空片段不占用名额，也不应中断后续候选。
        if content.trim().is_empty() {
            continue;
        }
        // 内容级去重：不同候选可能指向同一段代码（例如概览块与定义块）。
        let fingerprint = blake3::hash(content.as_bytes()).to_hex().to_string();
        if !seen_content.insert(fingerprint) {
            continue;
        }

        let code_ref = CodeRef::new(
            selection.chunk_id,
            &selection.file_hash,
            selection.start_line,
            selection.end_line,
        );
        let overhead = wrapper_tokens(&selection, &code_ref);
        let remaining = input.token_budget.saturating_sub(used_tokens);
        if remaining <= overhead {
            output.truncated = true;
            break;
        }
        let content_budget = remaining - overhead;
        let (content, truncated, end_line) =
            truncate_to_lines(&content, selection.start_line, content_budget);
        if content.is_empty() {
            output.truncated = true;
            break;
        }
        let estimated_tokens = estimate_tokens(&content) + overhead;
        used_tokens += estimated_tokens;
        if truncated {
            output.truncated = true;
        }
        // 截断后引用只覆盖真实返回的行，保证 read_code 拿到的范围与结果一致。
        let effective_ref = if truncated {
            CodeRef::new(
                selection.chunk_id,
                &selection.file_hash,
                selection.start_line,
                end_line,
            )
        } else {
            code_ref
        };
        output.results.push(SearchResult {
            code_ref: effective_ref.to_string(),
            path: selection.path.clone(),
            language: selection.language,
            symbol: selection.symbol.clone(),
            kind: selection.kind,
            file_hash: selection.file_hash.clone(),
            revision_id: selection.revision_id,
            start_line: selection.start_line,
            end_line,
            matched_by: selection.matched_by.clone(),
            score: selection.score,
            content,
            truncated,
            estimated_tokens,
        });
    }
    output.total_estimated_tokens = used_tokens;
    Ok(output)
}

/// 把碎片化的块扩展到包含它的定义。
///
/// 只处理拆分片段与结构概览：这类块单独看缺少签名与父作用域信息。
/// 扩展最多一跳，并且只在同一版本内查找，因此不会引入跨版本的代码。
fn expand_to_containing_symbols(store: &Store, selections: &mut [Selection]) -> Result<()> {
    for _ in 0..MAX_EXPAND_HOPS {
        for selection in selections.iter_mut() {
            if !matches!(selection.kind, ChunkKind::Part | ChunkKind::Overview) {
                continue;
            }
            let Some(parent) = store.containing_definition(
                selection.revision_id,
                selection.start_byte,
                selection.end_byte,
            )?
            else {
                continue;
            };
            if parent.id == selection.chunk_id {
                continue;
            }
            selection.start_byte = parent.start_byte;
            selection.end_byte = parent.end_byte;
            selection.start_line = parent.start_line;
            selection.end_line = parent.end_line;
            selection.kind = parent.kind;
            if parent.symbol.is_some() {
                selection.symbol = parent.symbol.clone();
            }
            if !selection.matched_by.contains(&MatchSource::Expanded) {
                selection.matched_by.push(MatchSource::Expanded);
                selection.matched_by.sort_unstable();
            }
        }
    }
    Ok(())
}

/// 合并同一版本内相邻或重叠的范围。
///
/// 只合并重叠或紧邻（间隔不超过一行）的片段，避免把两个无关定义粘在一起。
fn merge_adjacent(mut selections: Vec<Selection>) -> Vec<Selection> {
    selections.sort_by(|a, b| {
        a.revision_id
            .cmp(&b.revision_id)
            .then(a.start_byte.cmp(&b.start_byte))
            .then(a.end_byte.cmp(&b.end_byte))
    });
    let mut merged: Vec<Selection> = Vec::with_capacity(selections.len());
    for selection in selections {
        let Some(last) = merged.last_mut() else {
            merged.push(selection);
            continue;
        };
        let same_revision = last.revision_id == selection.revision_id;
        let overlaps = selection.start_byte <= last.end_byte;
        let adjacent_lines = selection.start_line <= last.end_line.saturating_add(1);
        if same_revision && overlaps && adjacent_lines {
            last.end_byte = last.end_byte.max(selection.end_byte);
            last.end_line = last.end_line.max(selection.end_line);
            last.start_byte = last.start_byte.min(selection.start_byte);
            last.start_line = last.start_line.min(selection.start_line);
            last.score = last.score.max(selection.score);
            last.exact |= selection.exact;
            // 合并后保留信息量更大的定义类别与符号名。
            if last.symbol.is_none() && selection.symbol.is_some() {
                last.symbol = selection.symbol.clone();
                last.kind = selection.kind;
            }
            for source in selection.matched_by {
                if !last.matched_by.contains(&source) {
                    last.matched_by.push(source);
                }
            }
            last.matched_by.sort_unstable();
        } else {
            merged.push(selection);
        }
    }
    merged
}

/// 查询要求测试时，为测试相关候选保留部分名额。
///
/// 仅调整顺序，不改变候选集合，也不伪造分数。
fn reserve_test_slots(
    selections: Vec<Selection>,
    plan: &QueryPlan,
    max_results: usize,
) -> Vec<Selection> {
    if !plan.intents.contains(&Intent::Test) || max_results == 0 {
        return selections;
    }
    let reserve = (max_results / TEST_RESERVE_DIVISOR).max(1);
    let (tests, others): (Vec<Selection>, Vec<Selection>) =
        selections.into_iter().partition(looks_like_test);
    if tests.is_empty() {
        return others;
    }
    let mut ordered = Vec::with_capacity(tests.len() + others.len());
    let mut tests = tests.into_iter();
    let others = others.into_iter();
    // 先放满保留额度，其余按原有相关度顺序交回。
    for _ in 0..reserve {
        if let Some(test) = tests.next() {
            ordered.push(test);
        }
    }
    ordered.extend(others);
    ordered.extend(tests);
    ordered
}

/// 路径或符号是否看起来属于测试代码。这是启发式判断，只用于分配候选额度。
fn looks_like_test(selection: &Selection) -> bool {
    let path = selection.path.to_ascii_lowercase();
    let symbol = selection
        .symbol
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    path.contains("test")
        || path.contains("spec")
        || path.contains("__tests__")
        || symbol.starts_with("test")
        || symbol.contains("::test")
}

/// 估算单条结果包装元数据的 token 数。
///
/// 预算必须包含代码之外的包装信息，否则实际输出会超出调用方的上限。
fn wrapper_tokens(selection: &Selection, code_ref: &CodeRef) -> usize {
    let header = format!(
        "{} {} {}:{}-{} {}",
        code_ref,
        selection.path,
        selection.language.as_str(),
        selection.start_line,
        selection.end_line,
        selection.symbol.as_deref().unwrap_or("")
    );
    estimate_tokens(&header)
}

/// 按整行缩减内容到预算之内。
///
/// 返回 `(内容, 是否截断, 实际结束行)`。只在行边界缩减，绝不从任意字符位置切断代码，
/// 因此也不会切断多字节字符。
fn truncate_to_lines(content: &str, start_line: u32, budget: usize) -> (String, bool, u32) {
    if estimate_tokens(content) <= budget {
        let end_line = start_line + line_count(content).saturating_sub(1);
        return (content.to_string(), false, end_line.max(start_line));
    }
    let mut kept = String::new();
    let mut kept_lines = 0u32;
    for line in content.split_inclusive('\n') {
        let candidate_tokens = estimate_tokens(&kept) + estimate_tokens(line);
        if candidate_tokens > budget && kept_lines > 0 {
            break;
        }
        if candidate_tokens > budget && kept_lines == 0 {
            // 单行就超预算：保留该行以免返回空内容，并标记截断。
            kept.push_str(line);
            kept_lines += 1;
            break;
        }
        kept.push_str(line);
        kept_lines += 1;
    }
    let end_line = start_line + kept_lines.saturating_sub(1);
    (kept, true, end_line.max(start_line))
}

/// 统计文本行数（末尾无换行时最后一段也算一行）。
fn line_count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.matches('\n').count();
    let extra = usize::from(!text.ends_with('\n'));
    u32::try_from(newlines + extra).unwrap_or(u32::MAX)
}

/// 每一行起始字节偏移；额外追加一个末尾哨兵，便于取右边界。
fn line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    if *offsets.last().unwrap_or(&0) != content.len() {
        offsets.push(content.len());
    }
    offsets
}

/// 把 1 开始的闭区间行范围转换为字节区间 `[start, end)`。
///
/// 行号超出文件范围时收敛到文件末尾，不会越界。
fn line_range_to_bytes(content: &str, start_line: u32, end_line: u32) -> (usize, usize) {
    let offsets = line_offsets(content);
    let last_index = offsets.len().saturating_sub(1);
    let start_index = (start_line.max(1) as usize - 1).min(last_index);
    let end_index = (end_line.max(start_line) as usize).min(last_index);
    (offsets[start_index], offsets[end_index])
}

/// 按引用读取对应版本的源码。
///
/// 默认读取引用钉住的那个版本：即使文件已经改变，也返回当时的代码，
/// 并通过 `is_current` 告知该版本是否仍是当前采集版本。
/// 引用指向的代码块或版本已不存在时返回 [`Error::ReferenceExpired`]，
/// 绝不用最新文件冒充旧引用。
pub fn read(store: &Store, request: &ReadRequest, default_budget: usize) -> Result<ReadResponse> {
    let code_ref = CodeRef::parse(&request.code_ref)?;
    let row = store
        .chunk_by_id(code_ref.chunk_id)?
        .ok_or_else(|| Error::ReferenceExpired(format!("代码块 {} 已不存在", code_ref.chunk_id)))?;
    if !row.file_hash.starts_with(&code_ref.hash_prefix) {
        return Err(Error::ReferenceExpired(format!(
            "引用的内容哈希 {} 与代码块 {} 当前所属版本不匹配",
            code_ref.hash_prefix, code_ref.chunk_id
        )));
    }
    let (content, file_line_count) = store
        .revision_content(row.revision_id)?
        .ok_or_else(|| Error::ReferenceExpired(format!("版本 {} 已不存在", row.revision_id)))?;

    let mut symbol = row.symbol.clone();
    let (start_line, end_line) = match request.expand {
        ReadExpand::None => (code_ref.start_line, code_ref.end_line),
        ReadExpand::File => (1, file_line_count.max(1)),
        ReadExpand::ContainingSymbol => {
            let (start_byte, end_byte) =
                line_range_to_bytes(&content, code_ref.start_line, code_ref.end_line);
            match store.containing_definition(
                row.revision_id,
                start_byte as i64,
                end_byte as i64,
            )? {
                Some(parent) => {
                    if parent.symbol.is_some() {
                        symbol = parent.symbol.clone();
                    }
                    (parent.start_line, parent.end_line)
                }
                None => (code_ref.start_line, code_ref.end_line),
            }
        }
    };

    let (start_byte, end_byte) = line_range_to_bytes(&content, start_line, end_line);
    let slice = content
        .get(start_byte..end_byte)
        .ok_or_else(|| Error::Internal("行范围未落在字符边界上".to_string()))?;

    let budget = request.token_budget.unwrap_or(default_budget);
    let (body, truncated, actual_end) = truncate_to_lines(slice, start_line, budget);
    let omitted = if truncated && actual_end < end_line {
        Some(format!("已省略 {}-{} 行", actual_end + 1, end_line))
    } else if truncated {
        Some("内容因 token 预算被截断".to_string())
    } else {
        None
    };
    Ok(ReadResponse {
        code_ref: CodeRef::new(row.id, &row.file_hash, start_line, actual_end).to_string(),
        path: row.path.clone(),
        language: row.language,
        symbol,
        file_hash: row.file_hash.clone(),
        revision_id: row.revision_id,
        start_line,
        end_line: actual_end,
        file_line_count,
        estimated_tokens: estimate_tokens(&body),
        content: body,
        truncated,
        omitted,
        estimated: true,
        is_current: row.is_current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_ref_round_trips() {
        let reference = CodeRef::new(42, "0123456789abcdef0123", 10, 20);
        let text = reference.to_string();
        assert_eq!(text, "cs1:42:0123456789abcdef:10-20");
        assert_eq!(CodeRef::parse(&text).expect("parse"), reference);
    }

    #[test]
    fn code_ref_rejects_malformed_input() {
        for bad in [
            "",
            "cs0:1:abcd:1-2",
            "cs1:x:abcd:1-2",
            "cs1:1:zzzz:1-2",
            "cs1:1:abcd:2-1",
            "cs1:1:abcd:0-2",
            "cs1:1:abcd",
            "cs1:1:abcd:1-2:extra",
            "cs1:-3:abcd:1-2",
        ] {
            assert!(
                matches!(CodeRef::parse(bad), Err(Error::InvalidReference(_))),
                "`{bad}` 应被判定为非法引用"
            );
        }
    }

    #[test]
    fn line_offsets_handle_missing_trailing_newline() {
        // 末尾有换行。
        let (start, end) = line_range_to_bytes("a\nb\nc\n", 2, 2);
        assert_eq!(&"a\nb\nc\n"[start..end], "b\n");
        // 末尾无换行时最后一行仍可取到。
        let (start, end) = line_range_to_bytes("a\nb\nc", 3, 3);
        assert_eq!(&"a\nb\nc"[start..end], "c");
        // 超出范围收敛到末尾。
        let (start, end) = line_range_to_bytes("a\nb\n", 1, 99);
        assert_eq!(&"a\nb\n"[start..end], "a\nb\n");
    }

    #[test]
    fn line_range_handles_crlf_and_multibyte() {
        let text = "第一行\r\n第二行\r\n";
        let (start, end) = line_range_to_bytes(text, 2, 2);
        assert_eq!(&text[start..end], "第二行\r\n");
    }

    #[test]
    fn truncate_keeps_whole_lines() {
        let content = "line one\nline two\nline three\n";
        let (kept, truncated, end_line) = truncate_to_lines(content, 5, 4);
        assert!(truncated);
        // 只保留整行，不会从中间切断。
        assert!(kept.ends_with('\n'));
        assert!(content.starts_with(&kept));
        assert!(end_line >= 5);

        let (kept, truncated, end_line) = truncate_to_lines(content, 5, 1000);
        assert!(!truncated);
        assert_eq!(kept, content);
        assert_eq!(end_line, 7);
    }

    #[test]
    fn truncate_never_returns_empty_for_single_long_line() {
        let content = "a_very_long_single_line_without_any_newline_characters_at_all";
        let (kept, truncated, _) = truncate_to_lines(content, 1, 1);
        assert!(truncated);
        assert_eq!(kept, content);
    }

    #[test]
    fn line_count_counts_final_line_without_newline() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\nb\n"), 2);
    }
}
