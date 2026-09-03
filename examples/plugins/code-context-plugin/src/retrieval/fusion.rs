//! 候选融合：Reciprocal Rank Fusion。
//!
//! # 为什么是 RRF 而不是加权分数
//!
//! 各通道的分数量纲不可比：BM25 分数无上界且随语料规模漂移，符号匹配分固定在 0..1，
//! 将来的语义通道是余弦相似度。把它们加权相加需要先归一化，而归一化本身又依赖分布假设——
//! 换一个仓库就失效。
//!
//! RRF 只用**排名**：`score = Σ 1 / (k + rank)`。这带来一条关键性质——
//! **它在任意通道子集上都成立**。语义分支缺席时，公式不需要改，权重不需要重新配，
//! 结果依然有意义。这正是架构第 13 问要求的降级形态，也是 Phase 5 让
//! [`SearchCandidate`] 携带排名而不只是分数的原因。
//!
//! # 全部常量集中在这里
//!
//! 融合的每一个数值都在 [`FusionConfig`] 里，并以 [`FusionConfig::rank_one_unit`] 为单位
//! 表达，使"这个提升相当于在另一路里排第一"成为可读的量级说明，而不是散落各处的魔数。

use super::{CandidateSource, SearchCandidate, SymbolMatch};
use crate::index::StructuralIndex;
use crate::model::SymbolKind;

/// RRF 的排名平滑常量。
///
/// 沿用检索文献的常用取值 60。它让"在两路里都中等靠前"胜过"只在一路里排第一"——
/// 对代码检索是正确的取向：同时被符号与词法命中的代码块，相关性通常确实更高。
const RRF_K: f32 = 60.0;

/// 各通道送入融合的候选数相对最终结果数的倍数。
///
/// 融合需要比最终结果更多的原始候选才有取舍空间；只取 `limit` 条会让融合退化成
/// 简单拼接。
const CANDIDATE_MULTIPLIER: usize = 4;

/// 各通道送入融合的候选数上限。
const MAX_CANDIDATES_PER_CHANNEL: usize = 200;

/// 融合参数。
///
/// 提升值以 [`FusionConfig::rank_one_unit`] 为单位表达。一个单位等于"在某一路里排第一"
/// 所贡献的分数，因此 `3.0` 的含义是"相当于额外在三路里各排第一"，量级一目了然。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusionConfig {
    /// RRF 平滑常量。
    pub rrf_k: f32,
    /// 精确符号匹配的提升，单位为 rank-1 当量。
    ///
    /// 存在的理由：RRF 天然偏好"多路都命中"，但用户查 `PermissionManager::check` 时，
    /// 那个符号本身必须排第一，即使它只被符号通道命中。取 3.0 使单路精确匹配
    /// 稳定压过双路中等匹配（两路各排第一约 2.0 当量）。
    pub exact_symbol_boost: f32,
    /// 短名精确匹配的提升，单位为 rank-1 当量。
    pub name_match_boost: f32,
    /// 查询词命中文件路径的提升，单位为 rank-1 当量。
    ///
    /// 取 1.0：路径相关是有用信号但远弱于符号命中，不应盖过内容匹配。
    pub path_boost: f32,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            rrf_k: RRF_K,
            exact_symbol_boost: 3.0,
            name_match_boost: 1.5,
            path_boost: 1.0,
        }
    }
}

impl FusionConfig {
    /// 返回"在某一路里排第一"对应的 RRF 分数。
    ///
    /// 全部提升以它为单位，使配置值表达的是相对量级而不是绝对魔数。
    pub fn rank_one_unit(&self) -> f32 {
        1.0 / (self.rrf_k + 1.0)
    }
}

/// 融合过程的内部诊断。
///
/// **不对 Agent 暴露。** 它服务的是开发者调参与 Phase 14 的 benchmark 归因：
/// 一条结果为什么排在这个位置，必须能拆开看，否则调权重只能靠猜。
#[derive(Debug, Clone, PartialEq)]
pub struct FusionDebug {
    /// 各通道的命中排名，按通道名升序排列。
    pub ranks: Vec<(CandidateSource, u32)>,
    /// RRF 原始分。
    pub rrf_score: f32,
    /// 精确符号提升的实际加成。
    pub symbol_boost: f32,
    /// 路径提升的实际加成。
    pub path_boost: f32,
    /// 符号通道的匹配方式。
    pub symbol_match: Option<SymbolMatch>,
}

impl FusionDebug {
    /// 返回该结果排在当前位置的可读理由。
    pub fn reason(&self) -> String {
        let mut parts: Vec<String> = self
            .ranks
            .iter()
            .map(|(source, rank)| format!("{}#{rank}", source.as_str()))
            .collect();
        if self.symbol_boost > 0.0 {
            parts.push(format!("符号提升 {:.3}", self.symbol_boost));
        }
        if self.path_boost > 0.0 {
            parts.push(format!("路径提升 {:.3}", self.path_boost));
        }
        parts.join(" + ")
    }
}

/// 一条融合后的检索结果。
///
/// 这是**内部形态**：它带着定位信息与诊断，但不含正文。正文由 Phase 10 的上下文构建器
/// 按需重读并打包——见存储 ADR 的"不驻留 chunk 全文"。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// 命中的 chunk ID。
    pub chunk_id: String,
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
    /// 父符号限定名。
    pub parent_symbol: Option<String>,
    /// 声明签名。
    pub signature: Option<String>,
    /// 文档注释。
    pub doc_comment: Option<String>,
    /// 融合后的最终分数。
    pub score: f32,
    /// 内部诊断。
    pub debug: FusionDebug,
}

/// 返回各通道应当检索的候选条数。
///
/// 两条不变量：候选池**不得少于**最终结果数（否则根本填不满结果），
/// 且扩展倍数受上限约束。请求数本身超过上限时以请求数为准——此时限制候选池
/// 只会让结果无谓地变少。
pub fn candidate_limit(limit: usize) -> usize {
    let expanded = limit.saturating_mul(CANDIDATE_MULTIPLIER);
    expanded.min(MAX_CANDIDATES_PER_CHANNEL.max(limit))
}

/// 融合多路候选。
///
/// `channels` 可以是任意条数、任意组合，**允许为空或只有一路**：RRF 在子集上依然成立。
/// `index` 用于取回 chunk 元数据并计算路径提升。
///
/// 返回按分数降序、分数相同时按 chunk ID 升序的结果，最多 `limit` 条。
pub fn fuse(
    channels: &[Vec<SearchCandidate>],
    query: &str,
    index: &StructuralIndex,
    config: &FusionConfig,
    limit: usize,
) -> Vec<SearchResult> {
    if limit == 0 {
        return Vec::new();
    }
    let unit = config.rank_one_unit();
    let query_tokens = super::tokenize::tokenize(query);

    // 按 chunk 聚合各通道的排名。用 Vec 而不是 HashMap：候选量在数百级，
    // 线性查找足够快，且天然保持插入顺序，便于结果确定。
    let mut merged: Vec<Accumulator> = Vec::new();
    for channel in channels {
        for candidate in channel {
            let slot = match merged
                .iter_mut()
                .find(|item| item.chunk_id == candidate.chunk_id)
            {
                Some(existing) => existing,
                None => {
                    merged.push(Accumulator::new(&candidate.chunk_id));
                    merged.last_mut().expect("刚插入必然存在")
                }
            };
            slot.rrf_score += 1.0 / (config.rrf_k + candidate.rank as f32);
            slot.ranks.push((candidate.source, candidate.rank));
            // 同一 chunk 可能被符号通道以多种方式命中，保留最强的那一种。
            if let Some(kind) = candidate.symbol_match {
                slot.symbol_match = Some(match slot.symbol_match {
                    Some(existing) if existing <= kind => existing,
                    _ => kind,
                });
            }
        }
    }

    let mut results: Vec<SearchResult> = merged
        .into_iter()
        .filter_map(|item| {
            let chunk = index.chunk(&item.chunk_id)?;
            let symbol_boost = symbol_boost_of(item.symbol_match, config) * unit;
            let path_boost = path_boost_of(&query_tokens, &chunk.file_path, config) * unit;
            let mut ranks = item.ranks;
            ranks.sort_by_key(|(source, rank)| (source.as_str(), *rank));

            Some(SearchResult {
                chunk_id: item.chunk_id,
                file_path: chunk.file_path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                language: chunk.language.clone(),
                symbol_name: chunk.symbol_name.clone(),
                qualified_symbol: chunk.qualified_symbol.clone(),
                symbol_kind: chunk.symbol_kind,
                parent_symbol: chunk.parent_symbol.clone(),
                signature: chunk.signature.clone(),
                doc_comment: chunk.doc_comment.clone(),
                score: item.rrf_score + symbol_boost + path_boost,
                debug: FusionDebug {
                    ranks,
                    rrf_score: item.rrf_score,
                    symbol_boost,
                    path_boost,
                    symbol_match: item.symbol_match,
                },
            })
        })
        .collect();

    // 分数相同时按 chunk ID 排序，保证同一查询在任何进程都得到同一顺序。
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.chunk_id.cmp(&right.chunk_id))
    });
    results.truncate(limit);
    results
}

/// 聚合中的单个 chunk。
struct Accumulator {
    /// chunk ID。
    chunk_id: String,
    /// 累计 RRF 分。
    rrf_score: f32,
    /// 各通道排名。
    ranks: Vec<(CandidateSource, u32)>,
    /// 符号通道中最强的匹配方式。
    symbol_match: Option<SymbolMatch>,
}

impl Accumulator {
    /// 创建空聚合。
    fn new(chunk_id: &str) -> Self {
        Self {
            chunk_id: chunk_id.to_string(),
            rrf_score: 0.0,
            ranks: Vec::new(),
            symbol_match: None,
        }
    }
}

/// 返回符号匹配对应的提升倍数（rank-1 当量）。
///
/// 只有精确匹配获得提升。后缀与前缀匹配不提升——它们本就是弱信号，再提升会让
/// "查 check 命中所有名字里带 check 的东西"淹没真正的目标。
fn symbol_boost_of(symbol_match: Option<SymbolMatch>, config: &FusionConfig) -> f32 {
    match symbol_match {
        Some(SymbolMatch::ExactQualified) => config.exact_symbol_boost,
        Some(SymbolMatch::ExactName) => config.name_match_boost,
        _ => 0.0,
    }
}

/// 返回路径匹配对应的提升倍数（rank-1 当量）。
///
/// 按命中比例缩放：查询里越大比例的词出现在路径上，提升越强。这让
/// "session store" 命中 `src/session/store.rs` 得到接近满额的提升，
/// 而只碰巧撞上一个词的路径几乎不受影响。
fn path_boost_of(query_tokens: &[String], file_path: &str, config: &FusionConfig) -> f32 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let path_tokens = super::tokenize::tokenize(file_path);
    if path_tokens.is_empty() {
        return 0.0;
    }
    let mut matched = 0usize;
    let mut considered = 0usize;
    for token in query_tokens {
        considered += 1;
        if path_tokens.contains(token) {
            matched += 1;
        }
    }
    if matched == 0 {
        return 0.0;
    }
    config.path_boost * (matched as f32 / considered as f32)
}
