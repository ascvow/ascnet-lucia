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
use crate::model::{CodeChunk, Symbol, SymbolKind};
use crate::parser::parse_file;
use crate::repository::content_hash;
use crate::retrieval::{tokenize_is_cjk, ChunkRole, FusionDebug, SearchResult};
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
    /// 自然语言定位时，模型近似同分的容器与方法优先展示明显更短的方法。
    pub prefer_method_locations: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            token_budget: 8_000,
            max_results: 20,
            max_per_file: 4,
            merge_gap_lines: 0,
            max_snippet_lines: 240,
            prefer_method_locations: false,
        }
    }
}

/// 一条打包好的上下文条目。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEntry {
    /// 仓库相对路径。
    pub file_path: String,
    /// 实际返回正文的起始行，从 1 开始。
    pub start_line: u32,
    /// 实际返回正文的结束行，闭区间。
    pub end_line: u32,
    /// 检索阶段命中代码块的起始行。
    pub retrieval_start_line: u32,
    /// 检索阶段命中代码块的结束行。
    pub retrieval_end_line: u32,
    /// 结构解析得到的完整正文目标起始行。
    pub hydrated_start_line: u32,
    /// 结构解析得到的完整正文目标结束行；即使正文被截断也保留完整目标范围。
    pub hydrated_end_line: u32,
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
    /// 该结果在代码结构中的角色。
    pub role: ChunkRole,
    /// 代码片段正文。
    pub snippet: String,
    /// 是否因行数上限或重叠去重而只返回完整目标的一部分。
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
    /// 从检索代码块扩展到完整符号范围的条目数。
    pub hydrated: u32,
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

    let selected = select(index, results, config, &mut stats);
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
            retrieval_start_line: resolved.retrieval_start_line,
            retrieval_end_line: resolved.retrieval_end_line,
            hydrated_start_line: resolved.hydrated_start_line,
            hydrated_end_line: resolved.hydrated_end_line,
            language: resolved.language,
            symbol_name: resolved.symbol_name,
            qualified_symbol: resolved.qualified_symbol,
            symbol_kind: resolved.symbol_kind,
            signature: resolved.signature,
            doc_comment: resolved.doc_comment,
            role: item.role,
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
    /// 重叠裁剪前的完整目标；裁剪片只展示实际区间，文件变化后不能按原符号整体重定位。
    partial_target: Option<(u32, u32)>,
    /// 检索阶段命中代码块的起始行。
    retrieval_start_line: u32,
    /// 检索阶段命中代码块的结束行。
    retrieval_end_line: u32,
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
    /// 代码结构角色。
    role: ChunkRole,
    /// 融合分数。
    score: f32,
    /// 内部诊断。
    debug: FusionDebug,
}

/// 根据候选的结构元数据解析完整符号范围。
///
/// 只有原检索范围完整落在符号范围内时才扩展，避免用陈旧或人工合并的异常区间覆盖正确定位。
/// 限定名优先；缺少限定名时仅接受文件内唯一的同名、同种类符号。
fn hydration_range(index: &StructuralIndex, result: &SearchResult) -> Option<(u32, u32)> {
    let symbol = matching_symbol(
        index.symbols_of(&result.file_path),
        result.qualified_symbol.as_deref(),
        result.symbol_name.as_deref(),
        result.symbol_kind,
    )?;
    (symbol.start_line <= result.start_line && symbol.end_line >= result.end_line)
        .then_some((symbol.start_line, symbol.end_line))
}

/// 在一个文件的符号表中解析候选主符号。
fn matching_symbol<'a>(
    symbols: &'a [Symbol],
    qualified_symbol: Option<&str>,
    symbol_name: Option<&str>,
    symbol_kind: Option<SymbolKind>,
) -> Option<&'a Symbol> {
    if let Some(qualified) = qualified_symbol {
        let mut matches = symbols
            .iter()
            .filter(|symbol| symbol.qualified_name == qualified);
        let first = matches.next()?;
        return matches
            .find(|symbol| symbol_kind.is_some_and(|kind| symbol.kind == kind))
            .or(Some(first));
    }

    let name = symbol_name?;
    let mut matches = symbols
        .iter()
        .filter(|symbol| symbol.name == name && symbol_kind.is_none_or(|kind| symbol.kind == kind));
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
}

/// 执行去重、父子抑制与区间合并。
///
/// 数量与多样性约束必须等条目成功回读后再施加：前排候选可能已失效或超出 token 预算，
/// 提前截断会让后面的有效候选无法补位。
fn select(
    index: &StructuralIndex,
    results: Vec<SearchResult>,
    config: &ContextConfig,
    stats: &mut ContextStats,
) -> Vec<Selected> {
    let mut kept: Vec<Selected> = Vec::new();

    // 输入已按分数降序；逐条尝试并入已保留集合，因此保留的总是更高分的那一条。
    for result in results {
        let retrieval_start_line = result.start_line;
        let retrieval_end_line = result.end_line;
        let (start_line, end_line) =
            hydration_range(index, &result).unwrap_or((retrieval_start_line, retrieval_end_line));
        let candidate = Selected {
            chunk_id: result.chunk_id,
            file_path: result.file_path,
            start_line,
            end_line,
            partial_target: None,
            retrieval_start_line,
            retrieval_end_line,
            language: result.language,
            symbol_name: result.symbol_name,
            qualified_symbol: result.qualified_symbol,
            symbol_kind: result.symbol_kind,
            signature: result.signature,
            doc_comment: result.doc_comment,
            role: result.role,
            score: result.score,
            debug: result.debug,
        };

        // 先计算连通并集，包含多个已选区间的低分桥接也必须经过行限检查。
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
        let merge_fits =
            merged_end.saturating_sub(merged_start).saturating_add(1) <= config.max_snippet_lines;

        let contained = kept.iter().position(|existing| {
            existing.file_path == candidate.file_path
                && (contains(existing, &candidate)
                    || (merge_fits && contains(&candidate, existing)))
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
            // 收窄只适用于结构容器中的方法，不能让低分类型别名或普通文本替换已有正文。
            let is_contained_method = candidate.symbol_kind == Some(SymbolKind::Function)
                && matches!(
                    container.symbol_kind,
                    Some(
                        SymbolKind::Constant
                            | SymbolKind::Struct
                            | SymbolKind::Impl
                            | SymbolKind::Module
                    )
                );
            // 放得下的容器仅在模型分数近似相等时收窄，不提升明显低相关的方法。
            let near_tied_method = config.prefer_method_locations
                && is_contained_method
                && container_span >= candidate_span.saturating_mul(4)
                && matches!((container.debug.model_rerank_score, candidate.debug.model_rerank_score),
                    (Some(parent), Some(child)) if parent > 0.0 && child > 0.0 && (parent - child).abs() <= 0.01);
            if contains(container, &candidate)
                && ((container_span > config.max_snippet_lines && is_contained_method)
                    || near_tied_method)
                && candidate_span <= config.max_snippet_lines
            {
                kept[index].start_line = candidate.start_line;
                kept[index].end_line = candidate.end_line;
                kept[index].partial_target = candidate.partial_target;
                kept[index].retrieval_start_line = candidate.retrieval_start_line;
                kept[index].retrieval_end_line = candidate.retrieval_end_line;
                // 定位元数据一并换成被包含的那条，否则重定位与展示会指向容器符号。
                kept[index].chunk_id = candidate.chunk_id;
                kept[index].symbol_name = candidate.symbol_name;
                kept[index].qualified_symbol = candidate.qualified_symbol;
                kept[index].symbol_kind = candidate.symbol_kind;
                kept[index].signature = candidate.signature;
                kept[index].doc_comment = candidate.doc_comment;
                kept[index].role = candidate.role;
            } else if config.max_snippet_lines > 0
                && contains(container, &candidate)
                && container_span > config.max_snippet_lines
                && candidate.end_line
                    > container
                        .start_line
                        .saturating_add(config.max_snippet_lines - 1)
            {
                // 完整函数与语义尾窗也可能互相包含，但前缀截断后的实际正文没有覆盖尾窗。
                // 固定高分条目的真实前缀，再按可见覆盖扣除尾窗；不能拿不可见行抑制已召回正文。
                let target = container
                    .partial_target
                    .unwrap_or((container.start_line, container.end_line));
                let visible_end = container
                    .start_line
                    .saturating_add(config.max_snippet_lines - 1);
                kept[index] = container.partial_range(container.start_line, visible_end);
                let mut remainder = candidate;
                remainder.partial_target = Some(target);
                kept.extend(remainder.uncovered_fragments(&kept, config.max_snippet_lines));
                continue;
            }
            stats.suppressed_contained += 1;
            continue;
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

        if !merge_fits {
            // 前方的低分窗口不能把高分命中推到截断之外。保留既有区间，仅追加未覆盖正文；
            // 一个桥接候选可能留下多段，不能只保留第一段或原样追加重叠部分。
            // 部分重叠同样只能扣除实际前缀，不能用尚未展示的超长目标范围遮掉新候选。
            if config.max_snippet_lines > 0 {
                for &index in &related {
                    let existing = &kept[index];
                    let visible_end = existing
                        .start_line
                        .saturating_add(config.max_snippet_lines - 1);
                    if existing.end_line > visible_end {
                        kept[index] = existing.partial_range(existing.start_line, visible_end);
                    }
                }
            }
            kept.extend(candidate.uncovered_fragments(&kept, config.max_snippet_lines));
            continue;
        }

        // `kept` 保持输入排名顺序，首个关联项分数最高；沿用它的符号元数据，只扩展正文范围。
        kept[primary].start_line = merged_start;
        kept[primary].end_line = merged_end;
        if let Some((start, end)) = kept[primary].partial_target.as_mut() {
            *start = (*start).min(merged_start);
            *end = (*end).max(merged_end);
        }
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

    /// 保留原完整目标并截取实际区间；部分正文不能继续声明自己是完整符号定义。
    fn partial_range(&self, start_line: u32, end_line: u32) -> Self {
        let mut fragment = self.with_range(start_line, end_line);
        fragment.partial_target = Some(
            self.partial_target
                .unwrap_or((self.start_line, self.end_line)),
        );
        fragment.symbol_name = None;
        fragment.qualified_symbol = None;
        fragment.symbol_kind = None;
        fragment.signature = None;
        fragment.doc_comment = None;
        fragment
    }

    /// 扣除同文件已有覆盖，按行限保留所有非空余段；裁剪片不冒充完整符号定义。
    ///
    /// 原检索范围和完整目标用于说明缺失内容；实际余段独立保留，外部修改后保守丢弃。
    fn uncovered_fragments(&self, kept: &[Self], max_lines: u32) -> Vec<Self> {
        if max_lines == 0 {
            return Vec::new();
        }
        let mut ranges = vec![(self.start_line, self.end_line)];
        for existing in kept.iter().filter(|item| item.file_path == self.file_path) {
            let mut remaining = Vec::new();
            for (start, end) in ranges {
                if existing.end_line < start || existing.start_line > end {
                    remaining.push((start, end));
                    continue;
                }
                if start < existing.start_line {
                    remaining.push((start, existing.start_line - 1));
                }
                if end > existing.end_line {
                    remaining.push((existing.end_line + 1, end));
                }
            }
            ranges = remaining;
        }

        let mut fragments = Vec::new();
        for (mut start, end) in ranges {
            while start <= end {
                let fragment_end = end.min(start.saturating_add(max_lines - 1));
                let fragment = if self.partial_target.is_some()
                    || (start, fragment_end) != (self.start_line, self.end_line)
                {
                    self.partial_range(start, fragment_end)
                } else {
                    self.with_range(start, fragment_end)
                };
                fragments.push(fragment);
                if fragment_end == end {
                    break;
                }
                start = fragment_end + 1;
            }
        }
        fragments
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
    /// 当前文件中实际命中的检索范围起始行。
    retrieval_start_line: u32,
    /// 当前文件中实际命中的检索范围结束行。
    retrieval_end_line: u32,
    /// 完整符号正文目标的起始行。
    hydrated_start_line: u32,
    /// 完整符号正文目标的结束行。
    hydrated_end_line: u32,
    /// 是否因行数上限或重叠去重而只返回部分目标。
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
    /// 文件已变时重新解析得到的当前符号表。
    symbols: Vec<Symbol>,
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
                        let (relocations, symbols) = if changed {
                            let parsed =
                                parse_file(index.repository(), path, record.language, &content);
                            (
                                parsed
                                    .chunks
                                    .into_iter()
                                    .map(|chunk| (chunk.id.clone(), chunk))
                                    .collect(),
                                parsed.symbols,
                            )
                        } else {
                            (BTreeMap::new(), Vec::new())
                        };
                        FileState {
                            content,
                            changed,
                            relocations,
                            symbols,
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

/// 在重解析后的代码块中定位原候选，用限定名与唯一短名兜底稳定 ID 变化。
fn matching_chunk<'a>(state: &'a FileState, item: &Selected) -> Option<&'a CodeChunk> {
    if let Some(chunk) = state.relocations.get(&item.chunk_id) {
        return Some(chunk);
    }
    if let Some(qualified) = item.qualified_symbol.as_deref() {
        if let Some(chunk) = state.relocations.values().find(|chunk| {
            chunk.qualified_symbol.as_deref() == Some(qualified)
                && item
                    .symbol_kind
                    .is_none_or(|kind| chunk.symbol_kind == Some(kind))
        }) {
            return Some(chunk);
        }
    }
    let name = item.symbol_name.as_deref()?;
    let mut matches = state.relocations.values().filter(|chunk| {
        chunk.symbol_name.as_deref() == Some(name)
            && item
                .symbol_kind
                .is_none_or(|kind| chunk.symbol_kind == Some(kind))
    });
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
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
    // 裁剪边界来自旧快照中其他高分条目的覆盖，单独重定位原 chunk 会重新引入重复或错行。
    if state.changed && item.partial_target.is_some() {
        return None;
    }
    let lines: Vec<&str> = state.content.lines().collect();

    let (
        retrieval_start_line,
        retrieval_end_line,
        hydrated_start_line,
        hydrated_end_line,
        relocated_chunk,
    ) = if state.changed {
        // 合并过的区间只保留主符号的新位置：合并边界是多条结果的并集，文件变化后无从
        // 还原，退回单符号定位是可解释的降级。
        let chunk = matching_chunk(state, item);
        let has_symbol = item.qualified_symbol.is_some() || item.symbol_name.is_some();
        if has_symbol {
            let symbol = matching_symbol(
                &state.symbols,
                item.qualified_symbol.as_deref(),
                item.symbol_name.as_deref(),
                item.symbol_kind,
            )?;
            let retrieval = chunk
                .map(|chunk| (chunk.start_line, chunk.end_line))
                .unwrap_or((symbol.start_line, symbol.end_line));
            (
                retrieval.0,
                retrieval.1,
                symbol.start_line,
                symbol.end_line,
                chunk,
            )
        } else {
            let chunk = chunk?;
            (
                chunk.start_line,
                chunk.end_line,
                chunk.start_line,
                chunk.end_line,
                Some(chunk),
            )
        }
    } else {
        let target = item
            .partial_target
            .unwrap_or((item.start_line, item.end_line));
        (
            item.retrieval_start_line,
            item.retrieval_end_line,
            target.0,
            target.1,
            None,
        )
    };
    let relocated = state.changed;
    if relocated {
        stats.relocated += 1;
    }
    if (retrieval_start_line, retrieval_end_line) != (hydrated_start_line, hydrated_end_line) {
        stats.hydrated += 1;
    }

    let (actual_start, selected_end) = if item.partial_target.is_some() {
        (item.start_line, item.end_line)
    } else {
        (hydrated_start_line, hydrated_end_line)
    };
    let (mut snippet, actual_end, line_truncated) =
        take_lines(&lines, actual_start, selected_end, config.max_snippet_lines);
    if snippet.trim().is_empty() {
        return None;
    }
    let truncated = line_truncated || item.partial_target.is_some();
    if truncated && !line_truncated {
        snippet.push_str("\n// …（片段已截断）");
    }
    Some(Resolved {
        snippet,
        start_line: actual_start,
        end_line: actual_end,
        retrieval_start_line,
        retrieval_end_line,
        hydrated_start_line,
        hydrated_end_line,
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
