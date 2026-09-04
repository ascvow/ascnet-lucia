//! 候选融合与轻量重排：Reciprocal Rank Fusion + 元数据覆盖率。
//!
//! # 为什么是 RRF 而不是加权分数
//!
//! 各通道的分数量纲不可比：BM25 分数无上界且随语料规模漂移，符号匹配分固定在 0..1，
//! 将来的语义通道是余弦相似度。把它们加权相加需要先归一化，而归一化本身又依赖分布假设——
//! 换一个仓库就失效。
//!
//! RRF 只用**排名**：`score = Σ weight(channel, intent) / (k + rank)`。这带来一条关键性质——
//! **它在任意通道子集上都成立**。语义分支缺席时，公式不需要改，权重不需要重新配，
//! 结果依然有意义。通道权重只区分“独立代码标识符”和“自然语言描述”：前者保留符号
//! 精确匹配，后者降低夹带普通代码词造成的符号重复计分，并提高语义通道的影响。这正是
//! 架构第 13 问要求的降级形态，也是 Phase 5 让
//! [`SearchCandidate`] 携带排名而不只是分数的原因。
//!
//! # 全部常量集中在这里
//!
//! 融合的每一个数值都在 [`FusionConfig`] 里，并以 [`FusionConfig::rank_one_unit`] 为单位
//! 表达，使"这个提升相当于在另一路里排第一"成为可读的量级说明，而不是散落各处的魔数。
//!
//! RRF 完成跨通道融合后，再用符号、签名、文档注释和路径对查询词的覆盖率做一次确定性
//! 重排。它不调用模型或网络，只负责在分数接近的候选之间优先展示元数据更贴近查询意图的
//! 代码块；加成严格小于一路第一名的 RRF 分值，避免重排覆盖原始召回顺序。

use super::{
    AnalyzedQuery, CandidateSource, ChunkRole, QueryGoal, QueryIntent, SearchCandidate, SymbolMatch,
};
use crate::index::StructuralIndex;
use crate::model::{CodeChunk, SymbolKind};
use std::collections::BTreeSet;

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
    /// 有语义通道时，自然语言查询中的符号通道权重。
    ///
    /// 自然语言经常夹带 `agent`、`session` 这类普通代码词；它们同时命中符号和 BM25 时
    /// 不应被当作用户明确查询了该符号，因此只保留弱提示。语义通道缺席时，前缀与后缀
    /// 符号恢复标准权重；普通精确符号仍使用本权重，避免重复放大自然语言中的代码词。
    pub natural_language_symbol_weight: f32,
    /// 自然语言查询中的语义通道权重。
    ///
    /// 取 1.75 使语义第一名能够压过“符号第一名 + 词法第一名”的普通词重复命中。
    pub natural_language_semantic_weight: f32,
    /// 查询以中日韩文字为主时的语义通道权重。
    ///
    /// 中文查询词**不可能**通过 BM25 匹配英文标识符：词法那一路能命中的只有注释里的
    /// 偶然共现，是噪声而非信号。实测中，语义通道把正确结果召回进了候选池（Recall@5
    /// 从 0.750 升到 0.844），却因为与词法等权而排不到第一（Recall@1 纹丝不动）。
    /// 取 3.5 让语义在这类查询上主导排序。
    pub cjk_semantic_weight: f32,
    /// 查询以中日韩文字为主时的词法通道权重。
    ///
    /// 不置零：文档注释是中文的仓库里，BM25 仍能提供真实信号。取 0.4 把它降为辅助。
    pub cjk_lexical_weight: f32,
    /// 查询词命中文件路径的提升，单位为 rank-1 当量。
    ///
    /// 取 1.0：路径相关是有用信号但远弱于符号命中，不应盖过内容匹配。
    pub path_boost: f32,
    /// 自然语言查询中夹带标识符时，符号精确提升的折减比例。
    ///
    /// 取 0.5：足以让"工具函数 formatDate 定义在哪"把定义排到使用点之前，又不至于让
    /// 一个顺带提到的标识符压过语义与词法通道的共同判断。
    pub natural_language_identifier_ratio: f32,
    /// 测试代码在非测试意图查询下的分数系数。
    ///
    /// 测试的文档注释常把被测行为描述得比实现更清楚，因此在自然语言检索里会压过实现
    /// 代码——实测中"代码块是怎么切分出来的"的语义第一名是一条测试。用户问"某功能实现
    /// 在哪"时要的几乎总是实现。
    ///
    /// 用**乘性**系数而不是减去固定值：候选分数的绝对量级随通道数与命中排名变化很大
    /// （实测同一次检索里 top 分数只有 rank-1 当量的一半），减去固定值会让惩罚在低分区
    /// 压过一切、把本该保留的结果挤出结果集。乘性系数则只改变相对位置。
    ///
    /// 查询确实指向测试时（含"测试"、`test`、`spec` 等词）该系数不生效。
    pub test_code_weight: f32,
    /// 过短代码块在自然语言查询下的分数系数。
    ///
    /// 两行的常量声明配一句精炼中文注释，与短中文查询的余弦相似度很高，但作为"这个功能
    /// 实现在哪"的答案毫无用处。实测中 `SEMANTIC_KEY`、`MAX_SOURCE_CHARS` 这类块反复挤进
    /// 前五。仅对自然语言查询生效——标识符查询里，一个两行常量完全可能就是目标本身。
    pub short_chunk_weight: f32,
    /// 触发短块系数的行数上限。
    pub short_chunk_lines: u32,
    /// 元数据完整覆盖查询词时的重排提升，单位为 rank-1 当量。
    ///
    /// 取 0.75：足以调整分数接近的候选，但小于一路第一名与末位候选的最大分差，确保
    /// RRF 召回仍是主信号。这里只检查已驻留的元数据，不回读正文，也不调用外部服务。
    pub metadata_rerank_boost: f32,
    /// 代码角色与查询目标完全一致时的提升，单位为 rank-1 当量。
    pub role_match_boost: f32,
    /// 实现、行为或调用点查询下说明性文档的分数系数。
    pub documentation_weight: f32,
    /// 实现查询下纯类型定义的分数系数。
    pub non_implementation_weight: f32,
    /// 实现或行为查询下配置文件的分数系数。
    pub config_weight: f32,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            rrf_k: RRF_K,
            exact_symbol_boost: 3.0,
            name_match_boost: 1.5,
            natural_language_symbol_weight: 0.25,
            natural_language_semantic_weight: 1.75,
            cjk_semantic_weight: 3.5,
            cjk_lexical_weight: 0.4,
            test_code_weight: 0.55,
            short_chunk_weight: 0.8,
            short_chunk_lines: 6,
            natural_language_identifier_ratio: 0.5,
            path_boost: 1.0,
            metadata_rerank_boost: 0.75,
            role_match_boost: 1.0,
            documentation_weight: 0.4,
            non_implementation_weight: 0.82,
            config_weight: 0.7,
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
    /// 元数据覆盖率产生的实际重排加成。
    pub rerank_boost: f32,
    /// 代码角色与查询目标匹配产生的实际加成。
    pub role_boost: f32,
    /// 测试代码与过短代码块产生的实际扣分。
    ///
    /// 与各项加成一样进入诊断：分数必须能被完整拆解，否则调参只能靠猜。
    pub penalty: f32,
    /// 元数据至少命中的去重查询词数量。
    pub rerank_matched_tokens: usize,
    /// 参与元数据重排的去重查询词总数。
    pub rerank_query_tokens: usize,
    /// 符号通道的匹配方式。
    pub symbol_match: Option<SymbolMatch>,
    /// 由结构元数据推导出的代码角色。
    pub role: ChunkRole,
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
        if self.rerank_boost > 0.0 {
            parts.push(format!(
                "元数据重排 {:.3} ({}/{})",
                self.rerank_boost, self.rerank_matched_tokens, self.rerank_query_tokens
            ));
        }
        if self.role_boost > 0.0 {
            parts.push(format!(
                "角色提升 {} {:.3}",
                self.role.as_str(),
                self.role_boost
            ));
        }
        if self.penalty > 0.0 {
            parts.push(format!("扣分 {:.3}", self.penalty));
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
    /// 由结构元数据推导出的代码角色。
    pub role: ChunkRole,
    /// 当前最终排序分数；启用模型重排时为相关度分数，否则为本地融合分数。
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
    let rerank_query_tokens: BTreeSet<String> =
        super::tokenize::tokenize_query(query).into_iter().collect();
    let analyzed = AnalyzedQuery::analyze(query);
    let intent = analyzed.intent;
    let query_has_identifier = !analyzed.identifiers.is_empty();
    let wants_tests = query_targets_tests(query);
    let semantic_available = channels
        .iter()
        .flatten()
        .any(|candidate| candidate.source == CandidateSource::Semantic);

    // 各类 boost 以"在某一路里排第一"为单位。通道权重被调低时（例如中文查询下的词法
    // 通道），RRF 分数整体缩小，而 boost 若保持不变就会反客为主，把召回顺序盖掉。
    // 因此按本次实际生效的最大通道权重缩放，使"boost 弱于一路第一名"这条不变量与
    // 权重方案无关。
    let weight_scale = channels
        .iter()
        .flatten()
        .map(|candidate| {
            channel_weight(
                candidate.source,
                candidate.symbol_match,
                intent,
                analyzed.cjk_dominant,
                semantic_available,
                config,
            )
        })
        .fold(0.0f32, f32::max)
        .max(f32::MIN_POSITIVE);
    let boost_unit = unit * weight_scale;

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
            slot.rrf_score += channel_weight(
                candidate.source,
                candidate.symbol_match,
                intent,
                analyzed.cjk_dominant,
                semantic_available,
                config,
            ) / (config.rrf_k + candidate.rank as f32);
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
            let symbol_boost =
                symbol_boost_of(item.symbol_match, intent, query_has_identifier, config)
                    * boost_unit;
            let path_boost = path_boost_of(&query_tokens, &chunk.file_path, config) * boost_unit;
            let rerank = metadata_rerank_of(&rerank_query_tokens, chunk, intent, config);
            let rerank_boost = rerank.boost_units * boost_unit;
            let role = ChunkRole::from_chunk(chunk);
            let role_boost = role_boost_of(analyzed.goal, role, config) * boost_unit;
            let raw = item.rrf_score + symbol_boost + path_boost + rerank_boost + role_boost;
            let weight = score_weight_of(chunk, role, analyzed.goal, intent, wants_tests, config);
            // 记为扣分而不是系数，使 `score = 各项之和 - penalty` 这条诊断不变量继续成立。
            let penalty = raw * (1.0 - weight);
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
                role,
                score: raw - penalty,
                debug: FusionDebug {
                    ranks,
                    rrf_score: item.rrf_score,
                    symbol_boost,
                    path_boost,
                    rerank_boost,
                    role_boost,
                    penalty,
                    rerank_matched_tokens: rerank.matched_tokens,
                    rerank_query_tokens: rerank.query_tokens,
                    symbol_match: item.symbol_match,
                    role,
                },
            })
        })
        .collect();

    // 分数相同时按角色、路径、符号和 chunk ID 决胜，避免 HashMap 或解析插入顺序造成漂移。
    results.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                role_priority(analyzed.goal, left.role)
                    .cmp(&role_priority(analyzed.goal, right.role))
            })
            .then_with(|| left.file_path.cmp(&right.file_path))
            .then_with(|| {
                left.qualified_symbol
                    .as_deref()
                    .or(left.symbol_name.as_deref())
                    .cmp(
                        &right
                            .qualified_symbol
                            .as_deref()
                            .or(right.symbol_name.as_deref()),
                    )
            })
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

/// 一条候选的元数据重排结果。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct MetadataRerank {
    /// 尚未换算为实际 RRF 分数的 rank-1 当量。
    boost_units: f32,
    /// 至少命中一个元数据字段的查询词数量。
    matched_tokens: usize,
    /// 参与计算的去重查询词总数。
    query_tokens: usize,
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
///
/// # 自然语言查询里夹带的标识符
///
/// 早期实现对自然语言查询一律返回 0，代价是"工具函数 formatDate 定义在哪"这类**混合
/// 查询**完全拿不到定义优先：词法通道会把每一处调用点都召回，而定义本身往往因为函数体
/// 更长、BM25 长度归一化更吃亏而排在后面，于是结果全是使用点、没有函数本体。
///
/// 现在改为：自然语言查询中，只要有一个**具备标识符特征**的查询词与该 chunk 的符号精确
/// 相同，就给一份折减后的提升。判定见 [`has_identifier_shaped_token`]——它要求驼峰、
/// 下划线、限定分隔符或足够长度，因此 `agent`、`check`、`session` 这类普通英文词不会
/// 触发，而 `formatDate`、`refresh_token`、`PermissionManager` 会。
pub(crate) fn symbol_boost_of(
    symbol_match: Option<SymbolMatch>,
    intent: QueryIntent,
    query_has_identifier: bool,
    config: &FusionConfig,
) -> f32 {
    let base = match symbol_match {
        Some(SymbolMatch::ExactQualified) => config.exact_symbol_boost,
        Some(SymbolMatch::ExactName) => config.name_match_boost,
        _ => return 0.0,
    };
    match intent {
        QueryIntent::Identifier | QueryIntent::QualifiedSymbol => base,
        // 折减而不是等同：自然语言查询里的标识符是"顺带提到"，不像纯标识符查询那样
        // 明确表达了导航意图，不应压过语义与词法通道的共同判断。
        QueryIntent::Mixed if query_has_identifier => {
            base * config.natural_language_identifier_ratio
        }
        QueryIntent::NaturalLanguage | QueryIntent::Mixed => 0.0,
    }
}

/// 判断查询是否明确指向测试代码。
///
/// 只在用户确实要找测试时放行测试代码的正常排名；这条判断存在的意义是让降权**可被
/// 用户意图关闭**，而不是一刀切地埋掉整个测试目录。
fn query_targets_tests(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    ["测试", "单测", "用例", "test", "spec", "assert", "fixture"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 返回该代码块的分数系数，`1.0` 表示不惩罚。
///
/// 两类惩罚都只在自然语言查询下生效：标识符查询表达的是精确导航意图，用户查
/// `MAX_SOURCE_CHARS` 时那个两行常量就是答案本身，此时任何"块太短"的惩罚都是错的。
fn score_weight_of(
    chunk: &CodeChunk,
    role: ChunkRole,
    goal: QueryGoal,
    intent: QueryIntent,
    wants_tests: bool,
    config: &FusionConfig,
) -> f32 {
    if matches!(
        intent,
        QueryIntent::Identifier | QueryIntent::QualifiedSymbol
    ) {
        return 1.0;
    }
    let mut weight = 1.0;
    if chunk.is_test && !wants_tests {
        weight *= config.test_code_weight;
    }
    let span = chunk.end_line.saturating_sub(chunk.start_line) + 1;
    if span <= config.short_chunk_lines {
        weight *= config.short_chunk_weight;
    }
    match (goal, role) {
        (
            QueryGoal::Implementation | QueryGoal::Behavior | QueryGoal::CallSite,
            ChunkRole::Documentation,
        ) => weight *= config.documentation_weight,
        (QueryGoal::Definition | QueryGoal::TypeLookup, ChunkRole::Documentation) => {
            weight *= config.documentation_weight.max(0.65)
        }
        (QueryGoal::Implementation, ChunkRole::TypeDefinition) => {
            weight *= config.non_implementation_weight
        }
        (QueryGoal::Implementation | QueryGoal::Behavior, ChunkRole::Config) => {
            weight *= config.config_weight
        }
        _ => {}
    }
    weight
}

/// 返回代码角色与查询目标匹配时的提升倍数。
fn role_boost_of(goal: QueryGoal, role: ChunkRole, config: &FusionConfig) -> f32 {
    let strength = match (goal, role) {
        (QueryGoal::Definition, ChunkRole::Definition | ChunkRole::TypeDefinition) => 1.25,
        (QueryGoal::TypeLookup, ChunkRole::TypeDefinition) => 1.5,
        (QueryGoal::Implementation | QueryGoal::Behavior, ChunkRole::Implementation) => 1.5,
        (QueryGoal::CallSite, ChunkRole::CallSite) => 1.5,
        (QueryGoal::CallSite, ChunkRole::Implementation) => 0.35,
        (QueryGoal::Concept, ChunkRole::Documentation) => 1.0,
        (QueryGoal::Concept, ChunkRole::TypeDefinition | ChunkRole::Definition) => 0.5,
        _ => 0.0,
    };
    strength * config.role_match_boost
}

/// 返回分数完全相同时的稳定角色优先级，数值越小越优先。
fn role_priority(goal: QueryGoal, role: ChunkRole) -> u8 {
    match goal {
        QueryGoal::Implementation | QueryGoal::Behavior => match role {
            ChunkRole::Implementation => 0,
            ChunkRole::Definition | ChunkRole::TypeDefinition => 1,
            ChunkRole::CallSite => 2,
            ChunkRole::Other => 3,
            ChunkRole::Test => 4,
            ChunkRole::Config => 5,
            ChunkRole::Documentation => 6,
        },
        QueryGoal::Definition | QueryGoal::TypeLookup => match role {
            ChunkRole::Definition | ChunkRole::TypeDefinition => 0,
            ChunkRole::Implementation => 1,
            ChunkRole::Other => 2,
            ChunkRole::CallSite => 3,
            ChunkRole::Config => 4,
            ChunkRole::Test => 5,
            ChunkRole::Documentation => 6,
        },
        QueryGoal::CallSite => match role {
            ChunkRole::CallSite => 0,
            ChunkRole::Implementation => 1,
            ChunkRole::Definition | ChunkRole::TypeDefinition => 2,
            ChunkRole::Other => 3,
            ChunkRole::Test => 4,
            ChunkRole::Config => 5,
            ChunkRole::Documentation => 6,
        },
        QueryGoal::Concept => match role {
            ChunkRole::Documentation => 0,
            ChunkRole::TypeDefinition | ChunkRole::Definition => 1,
            ChunkRole::Implementation => 2,
            ChunkRole::Config => 3,
            ChunkRole::Other => 4,
            ChunkRole::Test => 5,
            ChunkRole::CallSite => 6,
        },
    }
}

/// 返回指定查询意图下某个检索通道的 RRF 权重。
fn channel_weight(
    source: CandidateSource,
    symbol_match: Option<SymbolMatch>,
    intent: QueryIntent,
    cjk_dominant: bool,
    semantic_available: bool,
    config: &FusionConfig,
) -> f32 {
    if matches!(
        intent,
        QueryIntent::Identifier | QueryIntent::QualifiedSymbol
    ) {
        return 1.0;
    }
    match source {
        CandidateSource::Symbol if semantic_available => config.natural_language_symbol_weight,
        CandidateSource::Symbol
            if matches!(
                symbol_match,
                Some(SymbolMatch::ExactQualified | SymbolMatch::ExactName)
            ) =>
        {
            config.natural_language_symbol_weight
        }
        CandidateSource::Symbol => 1.0,
        CandidateSource::Lexical if cjk_dominant => config.cjk_lexical_weight,
        CandidateSource::Lexical => 1.0,
        CandidateSource::Semantic if cjk_dominant => config.cjk_semantic_weight,
        CandidateSource::Semantic => config.natural_language_semantic_weight,
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

/// 根据查询词对代码块元数据的覆盖率计算二次重排提升。
///
/// 标识符查询更信任符号字段；自然语言查询更信任文档注释。每个查询词只采用命中字段中的
/// 最高权重，避免同一文本同时出现在限定名和签名时被重复计分。路径已拥有独立提升，因此
/// 在这里仅保留较弱权重，作为分数接近时的稳定辅助信号。
fn metadata_rerank_of(
    query_tokens: &BTreeSet<String>,
    chunk: &CodeChunk,
    intent: QueryIntent,
    config: &FusionConfig,
) -> MetadataRerank {
    if query_tokens.is_empty() {
        return MetadataRerank::default();
    }

    let mut symbol_tokens = BTreeSet::new();
    for value in [
        chunk.symbol_name.as_deref(),
        chunk.qualified_symbol.as_deref(),
        chunk.parent_symbol.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        symbol_tokens.extend(super::tokenize::tokenize(value));
    }
    let signature_tokens: BTreeSet<String> = chunk
        .signature
        .as_deref()
        .into_iter()
        .flat_map(super::tokenize::tokenize)
        .collect();
    let doc_tokens: BTreeSet<String> = chunk
        .doc_comment
        .as_deref()
        .into_iter()
        .flat_map(super::tokenize::tokenize)
        .collect();
    let path_tokens: BTreeSet<String> = super::tokenize::tokenize(&chunk.file_path)
        .into_iter()
        .collect();
    let (symbol_weight, signature_weight, doc_weight, path_weight) = match intent {
        QueryIntent::Identifier | QueryIntent::QualifiedSymbol => (1.0, 0.8, 0.45, 0.25),
        // 中文查询与英文符号名没有词面重叠，元数据里唯一可能匹配的就是中文文档注释。
        QueryIntent::NaturalLanguage | QueryIntent::Mixed => (0.55, 0.7, 1.0, 0.25),
    };

    let mut weighted_matches = 0.0f32;
    let mut matched_tokens = 0usize;
    for token in query_tokens {
        let mut weight = 0.0f32;
        if symbol_tokens.contains(token) {
            weight = weight.max(symbol_weight);
        }
        if signature_tokens.contains(token) {
            weight = weight.max(signature_weight);
        }
        if doc_tokens.contains(token) {
            weight = weight.max(doc_weight);
        }
        if path_tokens.contains(token) {
            weight = weight.max(path_weight);
        }
        if weight > 0.0 {
            matched_tokens += 1;
            weighted_matches += weight;
        }
    }

    MetadataRerank {
        boost_units: config.metadata_rerank_boost * (weighted_matches / query_tokens.len() as f32),
        matched_tokens,
        query_tokens: query_tokens.len(),
    }
}
