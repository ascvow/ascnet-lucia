//! 上下文构建：把融合结果打包成 Agent 可用的上下文。
//!
//! # 为什么不是「取前十条」
//!
//! Phase 3 对容器与其成员都产出 chunk——`impl PermissionManager` 与它的
//! `PermissionManager::check` 都在索引里，前者的行区间完整包含后者。融合结果因此天然含
//! 父子重叠，直接返回等于把同一段代码发两遍。在 token 预算下这不是瑕疵而是硬伤：
//! 重复内容挤掉的是本来能放进去的其他相关代码。
//!
//! 因此本模块依次做：**父子抑制 → 区间合并 → 多样性约束 → 正文回读与一致性校验 →
//! token 预算打包**。
//!
//! # 惰性一致性校验
//!
//! 索引是内存驻留的快照，而仓库可能被外部编辑（编辑器保存、`git checkout`），宿主又不提供
//! 文件变更事件。架构第 8 问给出的兜底是：**命中结果在返回前重读并比对内容哈希**，
//! 不一致就即时重解析该文件并修正位置。代价被限制在命中集合（通常十几个文件）而不是
//! 整个仓库。本模块实现的正是这条。

#[cfg(test)]
mod tests;

use crate::index::StructuralIndex;
use crate::model::{CodeChunk, SymbolKind};
use crate::parser::parse_file;
use crate::repository::content_hash;
use crate::retrieval::{tokenize_is_cjk, FusionDebug, SearchResult};
use agent_plugin::PluginHostApi;
use std::collections::BTreeMap;

/// 上下文构建参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextConfig {
    /// token 预算上限。
    pub token_budget: u32,
    /// 最多返回的条目数。
    pub max_results: usize,
    /// 同一文件最多贡献的条目数。
    ///
    /// 多样性约束：没有它，一个大文件里的十几个函数会占满全部预算，
    /// 而"这个功能涉及哪几个文件"恰恰是 Agent 最需要的信息。
    pub max_per_file: usize,
    /// 同文件两段区间之间夹着的行数不超过该值时合并。
    ///
    /// 默认 0：只合并重叠或紧邻的区间。取值过大会把中间隔着空行的两个独立方法并成一段，
    /// 丢掉按符号归属结果的能力。
    pub merge_gap_lines: u32,
    /// 单条片段最多保留的行数。
    ///
    /// 设为 0 时不返回任何上下文条目。
    pub max_snippet_lines: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            token_budget: 8_000,
            max_results: 20,
            max_per_file: 4,
            merge_gap_lines: 0,
            max_snippet_lines: 120,
        }
    }
}

/// 一条打包好的上下文条目。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEntry {
    /// 仓库相对路径。
    pub file_path: String,
    /// 起始行，从 1 开始。
    pub start_line: u32,
    /// 结束行，闭区间。
    pub end_line: u32,
    /// 语言标识。
    pub language: String,
    /// 符号短名。
    pub symbol_name: Option<String>,
    /// 符号限定名。
    pub qualified_symbol: Option<String>,
    /// 符号种类。
    pub symbol_kind: Option<SymbolKind>,
    /// 声明签名。
    pub signature: Option<String>,
    /// 文档注释。
    pub doc_comment: Option<String>,
    /// 代码片段正文。
    pub snippet: String,
    /// 片段是否因超过行数上限被截断。
    pub truncated: bool,
    /// 该条目是否因文件被外部修改而重新定位过。
    pub relocated: bool,
    /// 融合分数。
    pub score: f32,
    /// 估算 token 数。
    pub estimated_tokens: u32,
    /// 内部诊断；不进入 Agent 可见输出。
    pub debug: FusionDebug,
}

/// 上下文构建统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextStats {
    /// 输入候选数。
    pub candidates: u32,
    /// 最终返回条目数。
    pub returned: u32,
    /// 因父子包含被抑制的条目数。
    pub suppressed_contained: u32,
    /// 因区间重叠或相邻被合并的条目数。
    pub merged: u32,
    /// 因同文件配额被丢弃的条目数。
    pub dropped_by_diversity: u32,
    /// 因 token 预算被丢弃的条目数。
    pub dropped_by_budget: u32,
    /// 因文件已删除或符号已消失被丢弃的条目数。
    pub dropped_stale: u32,
    /// 因文件被外部修改而重新定位的条目数。
    pub relocated: u32,
    /// 实际读取的文件数。
    pub files_read: u32,
    /// 估算的总 token 数。
    pub estimated_tokens: u32,
}

/// 一次上下文构建的产物。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextBundle {
    /// 打包好的条目，按分数降序。
    pub entries: Vec<ContextEntry>,
    /// 构建统计。
    pub stats: ContextStats,
}

/// 估算一段文本的 token 数。
///
/// 插件里没有 tokenizer，也不适合为此引入一个（词表体积远超收益）。这里用经验系数：
/// ASCII 约每 4 字符一个 token，中日韩约每字一个 token。两者密度差一个量级，
/// 用同一套系数会让含中文注释的代码估算偏离数倍。
///
/// **估算刻意偏保守（向上取整）**：宁可少放一条也不要超出预算——超出预算的后果是
/// 调用方截断，截断点不可控，可能把一条结果切成半截。
pub fn estimate_tokens(text: &str) -> u32 {
    let mut ascii = 0usize;
    let mut cjk = 0usize;
    for character in text.chars() {
        if tokenize_is_cjk(character) {
            cjk += 1;
        } else {
            ascii += 1;
        }
    }
    u32::try_from(ascii.div_ceil(4) + cjk).unwrap_or(u32::MAX)
}

/// 把融合结果打包成上下文。
///
/// 不返回错误：文件读不到或符号已消失的条目被丢弃并计入统计，其余照常返回。
/// 一个被删掉的文件不该让整次检索失败。
pub fn build(
    host: &dyn PluginHostApi,
    index: &StructuralIndex,
    results: Vec<SearchResult>,
    config: &ContextConfig,
) -> ContextBundle {
    let mut stats = ContextStats {
        candidates: u32::try_from(results.len()).unwrap_or(u32::MAX),
        ..ContextStats::default()
    };

    let selected = select(results, config, &mut stats);
    if config.max_results == 0 || config.max_snippet_lines == 0 {
        return ContextBundle {
            entries: Vec::new(),
            stats,
        };
    }

    let mut sources = SourceCache::default();
    let mut entries = Vec::new();
    let mut used_tokens = 0u32;
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();

    for item in selected {
        if entries.len() >= config.max_results {
            break;
        }
        if per_file.get(&item.file_path).copied().unwrap_or(0) >= config.max_per_file {
            stats.dropped_by_diversity += 1;
            continue;
        }

        let Some(resolved) = resolve(host, index, &item, config, &mut sources, &mut stats) else {
            stats.dropped_stale += 1;
            continue;
        };
        let cost = estimate_tokens(&resolved.snippet);
        // 预算不足时跳过而不是中断：后面可能还有更短的条目放得下，
        // 而按分数降序处理保证先放的是更相关的。
        if used_tokens.saturating_add(cost) > config.token_budget {
            stats.dropped_by_budget += 1;
            continue;
        }
        used_tokens += cost;
        *per_file.entry(item.file_path.clone()).or_insert(0) += 1;

        entries.push(ContextEntry {
            file_path: item.file_path,
            start_line: resolved.start_line,
            end_line: resolved.end_line,
            language: resolved.language,
            symbol_name: resolved.symbol_name,
            qualified_symbol: resolved.qualified_symbol,
            symbol_kind: resolved.symbol_kind,
            signature: resolved.signature,
            doc_comment: resolved.doc_comment,
            snippet: resolved.snippet,
            truncated: resolved.truncated,
            relocated: resolved.relocated,
            score: item.score,
            estimated_tokens: cost,
            debug: item.debug,
        });
    }

    stats.files_read = sources.reads;
    stats.returned = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    stats.estimated_tokens = used_tokens;
    ContextBundle { entries, stats }
}

/// 经过抑制、合并与多样性约束后的候选。
#[derive(Debug, Clone)]
struct Selected {
    /// 命中的稳定 chunk ID。
    chunk_id: String,
    /// 仓库相对路径。
    file_path: String,
    /// 起始行。
    start_line: u32,
    /// 结束行。
    end_line: u32,
    /// 语言标识。
    language: String,
    /// 符号短名。
    symbol_name: Option<String>,
    /// 符号限定名。
    qualified_symbol: Option<String>,
    /// 符号种类。
    symbol_kind: Option<SymbolKind>,
    /// 声明签名。
    signature: Option<String>,
    /// 文档注释。
    doc_comment: Option<String>,
    /// 融合分数。
    score: f32,
    /// 内部诊断。
    debug: FusionDebug,
}

/// 执行去重、父子抑制与区间合并。
///
/// 数量与多样性约束必须等条目成功回读后再施加：前排候选可能已失效或超出 token 预算，
/// 提前截断会让后面的有效候选无法补位。
fn select(
    results: Vec<SearchResult>,
    config: &ContextConfig,
    stats: &mut ContextStats,
) -> Vec<Selected> {
    let mut kept: Vec<Selected> = Vec::new();

    // 输入已按分数降序；逐条尝试并入已保留集合，因此保留的总是更高分的那一条。
    for result in results {
        let candidate = Selected {
            chunk_id: result.chunk_id,
            file_path: result.file_path,
            start_line: result.start_line,
            end_line: result.end_line,
            language: result.language,
            symbol_name: result.symbol_name,
            qualified_symbol: result.qualified_symbol,
            symbol_kind: result.symbol_kind,
            signature: result.signature,
            doc_comment: result.doc_comment,
            score: result.score,
            debug: result.debug,
        };

        let contained = kept.iter().position(|existing| {
            existing.file_path == candidate.file_path
                && (contains(existing, &candidate) || contains(&candidate, existing))
        });
        if let Some(index) = contained {
            // 父子包含：默认保留已在集合里的那条（分数更高）。被包含的一方内容已经在
            // 保留的区间里，丢弃它不损失信息，只省 token。
            //
            // 但"已经在区间里"有个前提：那个区间真的会被完整展示。容器 chunk（`impl` 块、
            // class）动辄数百行，超过 `max_snippet_lines` 后会从起始行硬截断，目标方法很
            // 可能整段落在截断之外——于是结果里出现的是容器开头几十行，真正要找的方法反而
            // 不见了。这种情况下把窗口收窄到被包含的那一条：宁可少给容器的头部，
            // 也不能把用户要的符号截掉。
            let container = &kept[index];
            let container_span = container.end_line.saturating_sub(container.start_line) + 1;
            let candidate_span = candidate.end_line.saturating_sub(candidate.start_line) + 1;
            if contains(container, &candidate)
                && container_span > config.max_snippet_lines
                && candidate_span <= config.max_snippet_lines
            {
                kept[index].start_line = candidate.start_line;
                kept[index].end_line = candidate.end_line;
                // 定位元数据一并换成被包含的那条，否则重定位与展示会指向容器符号。
                kept[index].chunk_id = candidate.chunk_id;
                kept[index].symbol_name = candidate.symbol_name;
                kept[index].qualified_symbol = candidate.qualified_symbol;
                kept[index].symbol_kind = candidate.symbol_kind;
                kept[index].signature = candidate.signature;
                kept[index].doc_comment = candidate.doc_comment;
            }
            stats.suppressed_contained += 1;
            continue;
        }

        // 候选可能同时桥接多个已保留区间。反复扩展并集直到收敛，确保合并后的集合仍满足
        // “同文件区间互不重叠”的不变量，而不是只合并第一个命中项后留下重复行。
        let mut merged_start = candidate.start_line;
        let mut merged_end = candidate.end_line;
        loop {
            let before = (merged_start, merged_end);
            let merged_range = candidate.with_range(merged_start, merged_end);
            for existing in &kept {
                if existing.file_path == candidate.file_path
                    && overlaps_or_adjacent(existing, &merged_range, config.merge_gap_lines)
                {
                    merged_start = merged_start.min(existing.start_line);
                    merged_end = merged_end.max(existing.end_line);
                }
            }
            if before == (merged_start, merged_end) {
                break;
            }
        }

        let related: Vec<usize> = kept
            .iter()
            .enumerate()
            .filter_map(|(index, existing)| {
                let merged_range = candidate.with_range(merged_start, merged_end);
                (existing.file_path == candidate.file_path
                    && overlaps_or_adjacent(existing, &merged_range, config.merge_gap_lines))
                .then_some(index)
            })
            .collect();
        let Some(&primary) = related.first() else {
            kept.push(candidate);
            continue;
        };

        // `kept` 保持输入排名顺序，首个关联项分数最高；沿用它的符号元数据，只扩展正文范围。
        kept[primary].start_line = merged_start;
        kept[primary].end_line = merged_end;
        for &index in related.iter().skip(1).rev() {
            kept.remove(index);
        }
        stats.merged = stats
            .merged
            .saturating_add(u32::try_from(related.len()).unwrap_or(u32::MAX));
    }
    kept
}

impl Selected {
    /// 复制候选并临时替换区间，供传递性合并计算使用。
    fn with_range(&self, start_line: u32, end_line: u32) -> Self {
        let mut range = self.clone();
        range.start_line = start_line;
        range.end_line = end_line;
        range
    }
}

/// 判断两段区间是否重叠，或中间相隔不超过 `gap` 行。
///
/// `gap` 是两段之间**严格夹着**的行数：紧邻（中间无行）时为 0。
fn overlaps_or_adjacent(left: &Selected, right: &Selected, gap: u32) -> bool {
    let start = left.start_line.max(right.start_line);
    let end = left.end_line.min(right.end_line);
    if start <= end {
        return true;
    }
    start.saturating_sub(end).saturating_sub(1) <= gap
}

/// 判断 `outer` 的区间是否完整包含 `inner`。
fn contains(outer: &Selected, inner: &Selected) -> bool {
    outer.start_line <= inner.start_line && outer.end_line >= inner.end_line
}

/// 回读得到的片段。
struct Resolved {
    /// 片段正文。
    snippet: String,
    /// 实际起始行。
    start_line: u32,
    /// 实际结束行。
    end_line: u32,
    /// 是否被行数上限截断。
    truncated: bool,
    /// 是否因文件被外部修改而重新定位。
    relocated: bool,
    /// 当前文件中的语言标识。
    language: String,
    /// 当前文件中的符号短名。
    symbol_name: Option<String>,
    /// 当前文件中的符号限定名。
    qualified_symbol: Option<String>,
    /// 当前文件中的符号种类。
    symbol_kind: Option<SymbolKind>,
    /// 当前文件中的声明签名。
    signature: Option<String>,
    /// 当前文件中的文档注释。
    doc_comment: Option<String>,
}

/// 一个已读文件的状态。
struct FileState {
    /// 当前磁盘上的正文。
    content: String,
    /// 相对索引建立时是否已被外部修改。
    changed: bool,
    /// 文件已变时重新解析得到的定位表：稳定 chunk ID → 当前 chunk。
    ///
    /// 只在文件确实变化时构建，未变更的文件不付出任何解析代价。
    relocations: BTreeMap<String, CodeChunk>,
}

/// 已读文件的缓存。
///
/// 一致性校验**按文件而不是按条目**进行：文件的内容哈希已经在结构索引里，比对一次就能
/// 判定整个文件是否变化。按条目校验则要为每条结果单独切片比对，且对**合并过的区间无从
/// 比对**——那正是这套设计要堵住的漏洞：合并会让校验被静默跳过。
#[derive(Default)]
struct SourceCache {
    /// 路径 → 文件状态；读取失败记为 `None`。
    files: BTreeMap<String, Option<FileState>>,
    /// 实际发起的读取次数。
    reads: u32,
}

impl SourceCache {
    /// 读取文件并完成一致性校验，同一路径只发起一次宿主调用。
    fn state(
        &mut self,
        host: &dyn PluginHostApi,
        index: &StructuralIndex,
        path: &str,
    ) -> Option<&FileState> {
        if !self.files.contains_key(path) {
            let state = match index.file(path) {
                Some(record) => {
                    self.reads += 1;
                    host.read_file(path).ok().map(|content| {
                        let changed = content_hash(&content) != record.content_hash;
                        // 只有确实变化的文件才重新解析，代价被限制在这一个文件上。
                        let relocations = if changed {
                            parse_file(index.repository(), path, record.language, &content)
                                .chunks
                                .into_iter()
                                .map(|chunk| (chunk.id.clone(), chunk))
                                .collect()
                        } else {
                            BTreeMap::new()
                        };
                        FileState {
                            content,
                            changed,
                            relocations,
                        }
                    })
                }
                None => None,
            };
            self.files.insert(path.to_string(), state);
        }
        self.files.get(path).and_then(Option::as_ref)
    }
}

/// 回读正文并做一致性校验。
///
/// 文件内容哈希不一致说明它在索引之后被外部修改过。此时按限定名从重解析结果里重新定位
/// 符号：位置修正后结果依然准确。符号已不存在时返回 `None`，由调用方丢弃——
/// **返回一段指向错误位置的代码比不返回更糟**。
fn resolve(
    host: &dyn PluginHostApi,
    index: &StructuralIndex,
    item: &Selected,
    config: &ContextConfig,
    sources: &mut SourceCache,
    stats: &mut ContextStats,
) -> Option<Resolved> {
    let state = sources.state(host, index, &item.file_path)?;
    let lines: Vec<&str> = state.content.lines().collect();

    let (start, end, relocated_chunk) = if state.changed {
        // 合并过的区间只保留主符号的新位置：合并边界是多条结果的并集，文件变化后无从
        // 还原，退回单符号定位是可解释的降级。
        let chunk = state.relocations.get(&item.chunk_id)?;
        (chunk.start_line, chunk.end_line, Some(chunk))
    } else {
        (item.start_line, item.end_line, None)
    };
    let relocated = relocated_chunk.is_some();
    if relocated {
        stats.relocated += 1;
    }

    let (snippet, actual_end, truncated) = take_lines(&lines, start, end, config.max_snippet_lines);
    if snippet.trim().is_empty() {
        return None;
    }
    Some(Resolved {
        snippet,
        start_line: start,
        end_line: actual_end,
        truncated,
        relocated,
        language: relocated_chunk
            .map(|chunk| chunk.language.clone())
            .unwrap_or_else(|| item.language.clone()),
        symbol_name: relocated_chunk
            .map(|chunk| chunk.symbol_name.clone())
            .unwrap_or_else(|| item.symbol_name.clone()),
        qualified_symbol: relocated_chunk
            .map(|chunk| chunk.qualified_symbol.clone())
            .unwrap_or_else(|| item.qualified_symbol.clone()),
        symbol_kind: relocated_chunk
            .map(|chunk| chunk.symbol_kind)
            .unwrap_or(item.symbol_kind),
        signature: relocated_chunk
            .map(|chunk| chunk.signature.clone())
            .unwrap_or_else(|| item.signature.clone()),
        doc_comment: relocated_chunk
            .map(|chunk| chunk.doc_comment.clone())
            .unwrap_or_else(|| item.doc_comment.clone()),
    })
}

/// 取闭区间行范围的正文。
fn slice_lines(lines: &[&str], start_line: u32, end_line: u32) -> String {
    let start = start_line.saturating_sub(1) as usize;
    let end = (end_line as usize).min(lines.len());
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

/// 取行范围并按上限截断，返回正文、实际结束行与是否截断。
fn take_lines(
    lines: &[&str],
    start_line: u32,
    end_line: u32,
    max_lines: u32,
) -> (String, u32, bool) {
    if max_lines == 0 || start_line == 0 || end_line < start_line {
        return (String::new(), start_line.saturating_sub(1), false);
    }
    let last_line = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let actual_end = end_line.min(last_line);
    if start_line > actual_end {
        return (String::new(), actual_end, false);
    }
    let available = actual_end.saturating_sub(start_line).saturating_add(1);
    if available <= max_lines {
        return (
            slice_lines(lines, start_line, actual_end),
            actual_end,
            false,
        );
    }
    let capped_end = start_line.saturating_add(max_lines).saturating_sub(1);
    let mut snippet = slice_lines(lines, start_line, capped_end);
    // 明确标注截断，避免模型把半截函数当作完整实现。
    snippet.push_str("\n// …（片段已截断）");
    (snippet, capped_end, true)
}
