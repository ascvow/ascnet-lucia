//! 检索：符号检索与词法检索。
//!
//! 本模块产出的是**内部候选** [`SearchCandidate`]，不是 Agent 可见的结果。这条边界是刻意的：
//! Phase 9 的融合层要在多路候选之上做 RRF，Phase 10 再把融合结果打包成上下文。把候选直接
//! 绑到工具返回值上，会让融合层无处安放。
//!
//! # 两路通道
//!
//! ```text
//! 符号检索  —— "这个名字定义在哪"    精确 / 后缀 / 前缀匹配
//! 词法检索  —— "哪些代码提到了这些词"  BM25 倒排
//! ```
//!
//! 两路都在语义分支缺席时可用，因此 V1 交付的检索能力不依赖任何嵌入 provider——这正是
//! `architecture.md` 第 13 问要求的 `Degraded` 形态。

// 插件入口在最终截断前复用配额选择，不向其他 crate 暴露融合实现。
pub(crate) mod fusion;
mod lexical;
mod query;
mod role;
mod symbol;
mod tokenize;

#[cfg(test)]
mod fusion_tests;
#[cfg(test)]
mod tests;

pub use fusion::{FusionConfig, FusionDebug, SearchResult};
pub use query::{
    AnalyzedQuery, ConfidenceLevel, PendingDirective, QueryGoal, QueryIntent, RetrievalConfidence,
    SearchPath, SearchPlan,
};
pub use role::ChunkRole;
pub use symbol::SymbolMatch;
pub(crate) use tokenize::is_cjk as tokenize_is_cjk;
pub use tokenize::tokenize;

use crate::index::StructuralIndex;
use crate::model::CodeChunk;
use crate::semantic::SemanticIndex;
use lexical::LexicalIndex;
use std::collections::BTreeSet;
use symbol::SymbolIndex;

/// 候选的来源通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateSource {
    /// 符号检索。
    Symbol,
    /// 词法检索。
    Lexical,
    /// 语义检索。
    ///
    /// V1 不产出该来源——语义索引依赖的宿主能力尚未落地（见 `PROGRESS.md` 的
    /// Blocked Phases）。变体现在就存在，是为了让融合层的"任意通道子集"能被**实际测试**
    /// 而不只是被声称：`fusion` 的测试会喂一路合成语义候选，验证三路融合无需改动代码。
    Semantic,
}

impl CandidateSource {
    /// 返回稳定的机器可读名称，用于内部诊断输出。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
        }
    }
}

/// 一个内部检索候选。
///
/// `rank` 从 1 开始，是**该候选在自己通道内的排名**。融合层用的是排名而不是分数：
/// 不同通道的分数量纲不可比（BM25 分数无上界，符号匹配分在 0..1），只有排名可比。
/// 这也是选 RRF 而不是加权分数融合的直接原因。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchCandidate {
    /// 命中的 chunk ID。
    pub chunk_id: String,
    /// 来源通道。
    pub source: CandidateSource,
    /// 通道内排名，从 1 开始。
    pub rank: u32,
    /// 通道原生分数，仅用于诊断，不跨通道比较。
    pub score: f32,
    /// 符号通道的匹配方式；词法通道为 `None`。
    pub symbol_match: Option<SymbolMatch>,
}

/// 一次检索各阶段的候选快照。
///
/// 各通道保留通道内排名顺序；`union` 按 chunk ID 稳定排序，只表达候选是否存在；
/// `post_fusion` 与 `final_results` 保留排序顺序。benchmark 可据此区分召回器缺失、union
/// 丢失、融合降级和最终 Top-K 截断，不再只能从最终结果反推原因。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateStages {
    /// 符号通道候选，按通道内排名排列。
    pub symbol: Vec<String>,
    /// 词法通道候选，按通道内排名排列。
    pub lexical: Vec<String>,
    /// 语义通道候选，按通道内排名排列。
    pub semantic: Vec<String>,
    /// 三路候选去重后的并集，按 chunk ID 排列。
    pub union: Vec<String>,
    /// 融合排序后的完整候选池，尚未执行最终 Top-K 截断。
    pub post_fusion: Vec<String>,
    /// 最终返回候选，按最终排名排列。
    pub final_results: Vec<String>,
}

impl CandidateStages {
    /// 从分路候选建立通道与 union 快照。
    ///
    /// 同一 chunk 在多个通道出现时只在 `union` 中保留一次；通道快照本身不去重，便于暴露
    /// 召回器内部异常。该方法没有外部副作用。
    fn from_channels(channels: &[Vec<SearchCandidate>]) -> Self {
        let mut stages = Self::default();
        let mut union = BTreeSet::new();
        for candidate in channels.iter().flatten() {
            union.insert(candidate.chunk_id.clone());
            let target = match candidate.source {
                CandidateSource::Symbol => &mut stages.symbol,
                CandidateSource::Lexical => &mut stages.lexical,
                CandidateSource::Semantic => &mut stages.semantic,
            };
            target.push(candidate.chunk_id.clone());
        }
        stages.union = union.into_iter().collect();
        stages
    }
}

/// 一次带阶段诊断的检索结果。
///
/// `results` 与原有检索 API 的返回完全一致；`stages` 仅增加可观测性，不参与排序。
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalRun {
    /// 最终排序结果。
    pub results: Vec<SearchResult>,
    /// 各召回与排序阶段的候选快照。
    pub stages: CandidateStages,
}

/// 检索索引规模。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetrievalStats {
    /// 符号表中的符号数。
    pub symbols: u32,
    /// 倒排索引覆盖的文档数。
    pub documents: u32,
    /// 不同词元数。
    pub terms: u32,
    /// 是否因触及上限而截断。
    pub truncated: bool,
}

/// 符号与词法两路检索的索引。
#[derive(Debug, Default)]
pub struct RetrievalIndex {
    /// 符号索引。
    symbols: SymbolIndex,
    /// 词法倒排索引。
    lexical: LexicalIndex,
}

impl RetrievalIndex {
    /// 创建空索引。
    pub fn new() -> Self {
        Self {
            symbols: SymbolIndex::new(),
            lexical: LexicalIndex::new(),
        }
    }

    /// 把一个 chunk 连同正文加入两路索引。
    ///
    /// `body` 只在切词时被使用，调用返回后即可丢弃。语义专用复合块被跳过：它们只用于
    /// 向量召回，若再次进入符号或词法通道，会与其保留的原子成员重复计分。
    pub fn index_chunk(&mut self, chunk: &CodeChunk, body: &str) {
        if chunk.semantic_only {
            return;
        }
        self.symbols.index_chunk(chunk);
        self.lexical.index_chunk(chunk, body);
    }

    /// 移除指定 chunk 在两路检索索引中的记录。
    ///
    /// 词法 posting 会在删除后重映射文档下标，符号排序表则标记为待重建；调用方应在
    /// 当前增量批次结束时调用 [`Self::finalize`]。返回实际移除的词法文档数。
    pub fn remove_chunks(&mut self, chunk_ids: &[String]) -> usize {
        self.symbols.remove_chunks(chunk_ids);
        self.lexical.remove_chunks(chunk_ids)
    }

    /// 移除指定文件在两路检索索引中的全部记录。
    pub fn remove_file(&mut self, path: &str) -> usize {
        self.symbols.remove_file(path);
        self.lexical.remove_file(path)
    }

    /// 完成索引构建。
    ///
    /// 必须在全部写入之后、首次检索之前调用；否则符号检索会因排序未就绪而返回空结果。
    pub fn finalize(&mut self) {
        self.symbols.finalize();
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty() && self.lexical.is_empty()
    }

    /// 返回包含指定词元的文档数。
    ///
    /// 供查询分析判断一个中文组合是否真的存在于本仓库，见
    /// [`AnalyzedQuery::resolve_leading_directive`]。
    pub fn document_frequency(&self, term: &str) -> usize {
        self.lexical.document_frequency(term)
    }

    /// 返回索引规模。
    pub fn stats(&self) -> RetrievalStats {
        RetrievalStats {
            symbols: u32::try_from(self.symbols.len()).unwrap_or(u32::MAX),
            documents: u32::try_from(self.lexical.len()).unwrap_or(u32::MAX),
            terms: u32::try_from(self.lexical.term_count()).unwrap_or(u32::MAX),
            truncated: self.lexical.is_truncated(),
        }
    }

    /// 执行符号检索。
    pub fn search_symbols(
        &self,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<SearchCandidate> {
        self.symbols
            .search(query, path_scope, limit)
            .into_iter()
            .enumerate()
            .map(|(index, (chunk_id, score, kind))| SearchCandidate {
                chunk_id: chunk_id.to_string(),
                source: CandidateSource::Symbol,
                rank: u32::try_from(index + 1).unwrap_or(u32::MAX),
                score,
                symbol_match: Some(kind),
            })
            .collect()
    }

    /// 执行词法检索。
    pub fn search_lexical(
        &self,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<SearchCandidate> {
        self.lexical
            .search(query, path_scope, limit)
            .into_iter()
            .enumerate()
            .map(|(index, (chunk_id, score))| SearchCandidate {
                chunk_id: chunk_id.to_string(),
                source: CandidateSource::Lexical,
                rank: u32::try_from(index + 1).unwrap_or(u32::MAX),
                score,
                symbol_match: None,
            })
            .collect()
    }

    /// 执行语义检索。
    ///
    /// `semantic` 未就绪或查询向量与库内向量维度不符时返回空列表——空通道对 RRF 是
    /// 合法输入，融合结果退化为词法与符号两路，这正是降级形态。
    pub fn search_semantic(
        &self,
        semantic: &SemanticIndex,
        query_vector: &[f32],
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<SearchCandidate> {
        if !semantic.is_ready() {
            return Vec::new();
        }
        semantic
            .search(query_vector, path_scope, limit)
            .into_iter()
            .enumerate()
            .map(|(index, (chunk_id, score))| SearchCandidate {
                chunk_id: chunk_id.to_string(),
                source: CandidateSource::Semantic,
                rank: u32::try_from(index + 1).unwrap_or(u32::MAX),
                score,
                symbol_match: None,
            })
            .collect()
    }

    /// 同时执行两路检索，返回各自**未经融合**的候选列表。
    ///
    /// 这是融合层的输入，也是内部检索诊断的数据来源：想知道"这条结果从哪来"，
    /// 对比本方法与 [`RetrievalIndex::search`] 的输出即可。
    pub fn search_all(
        &self,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<Vec<SearchCandidate>> {
        vec![
            self.search_symbols(query, path_scope, limit),
            self.search_lexical(query, path_scope, limit),
        ]
    }

    /// 执行三路检索，语义通道缺席时自动退化为两路。
    ///
    /// `query_vector` 为 `None` 表示嵌入不可用（未配置 provider、端点失败或索引未建），
    /// 此时结果与 [`RetrievalIndex::search_all`] 完全一致。
    pub fn search_all_with_semantic(
        &self,
        semantic: &SemanticIndex,
        query: &str,
        query_vector: Option<&[f32]>,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<Vec<SearchCandidate>> {
        let mut channels = self.search_all(query, path_scope, limit);
        if let Some(vector) = query_vector {
            let candidates = self.search_semantic(semantic, vector, path_scope, limit);
            if !candidates.is_empty() {
                channels.push(candidates);
            }
        }
        channels
    }

    /// 执行混合检索：多路候选 → RRF 融合 → 稳定排序。
    ///
    /// 语义通道缺席时结果依然有意义——RRF 在任意通道子集上都成立，这是它相对加权分数
    /// 融合的关键优势（见 [`fusion`] 模块文档）。
    pub fn search(
        &self,
        index: &StructuralIndex,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        self.search_with(index, query, path_scope, limit, &FusionConfig::default())
    }

    /// 用指定融合参数执行混合检索。
    ///
    /// 参数化只服务于调参与 benchmark；正常路径一律使用 [`FusionConfig::default`]。
    pub fn search_with(
        &self,
        index: &StructuralIndex,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
        config: &FusionConfig,
    ) -> Vec<SearchResult> {
        self.search_with_diagnostics(index, query, path_scope, limit, config)
            .results
    }

    /// 用指定融合参数执行两路检索，并返回完整候选阶段诊断。
    ///
    /// 排序行为与 [`RetrievalIndex::search_with`] 相同；额外快照用于 benchmark 归因。
    pub fn search_with_diagnostics(
        &self,
        index: &StructuralIndex,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
        config: &FusionConfig,
    ) -> RetrievalRun {
        // 各通道取比最终结果更多的候选，否则融合退化成简单拼接。
        let channels = self.search_all(query, path_scope, fusion::candidate_limit(limit));
        fuse_with_diagnostics(&channels, query, index, config, limit)
    }

    /// 执行含语义通道的三路混合检索。
    ///
    /// `query_vector` 为 `None` 时结果与 [`RetrievalIndex::search`] 完全一致——RRF 在任意
    /// 通道子集上都成立，因此嵌入不可用不影响检索可用性，只影响召回质量。
    pub fn search_hybrid(
        &self,
        index: &StructuralIndex,
        semantic: &SemanticIndex,
        query: &str,
        query_vector: Option<&[f32]>,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        self.search_hybrid_with_diagnostics(index, semantic, query, query_vector, path_scope, limit)
            .results
    }

    /// 执行含语义通道的三路混合检索，并返回完整候选阶段诊断。
    ///
    /// 各通道先独立取候选，再按 chunk ID 去重形成 union；语义候选只会增加并集，不能删除
    /// 已由符号或词法召回的候选。最终 Top-K 截断发生在完整融合排序之后。
    pub fn search_hybrid_with_diagnostics(
        &self,
        index: &StructuralIndex,
        semantic: &SemanticIndex,
        query: &str,
        query_vector: Option<&[f32]>,
        path_scope: Option<&str>,
        limit: usize,
    ) -> RetrievalRun {
        let channels = self.search_all_with_semantic(
            semantic,
            query,
            query_vector,
            path_scope,
            fusion::candidate_limit(limit),
        );
        fuse_with_diagnostics(&channels, query, index, &FusionConfig::default(), limit)
    }

    /// 按查询计划执行分路召回、union 与融合排序。
    ///
    /// 每个通道使用计划中的独立配额；被关闭的通道不会执行。`query_vector` 只在语义通道
    /// 启用时读取，调用方因此可以在生成向量前先确定计划，避免 Fast Path 产生网络开销。
    pub fn search_planned(
        &self,
        index: &StructuralIndex,
        semantic: &SemanticIndex,
        query: &str,
        query_vector: Option<&[f32]>,
        path_scope: Option<&str>,
        plan: &SearchPlan,
    ) -> RetrievalRun {
        let mut channels = Vec::with_capacity(3);
        if plan.enable_symbol {
            channels.push(self.search_symbols(query, path_scope, plan.symbol_limit));
        }
        if plan.enable_lexical {
            channels.push(self.search_lexical(query, path_scope, plan.lexical_limit));
        }
        if plan.enable_semantic {
            if let Some(vector) = query_vector {
                let candidates =
                    self.search_semantic(semantic, vector, path_scope, plan.semantic_limit);
                if !candidates.is_empty() {
                    channels.push(candidates);
                }
            }
        }
        // 模型尚未判断源码相关性时，不因同文件命中密集而淘汰高分业务方法。
        // 候选总量仍由计划限制；最终展示多样性由重排后与上下文层负责。
        let config = FusionConfig {
            max_per_file: if plan.allow_rerank {
                0
            } else {
                FusionConfig::default().max_per_file
            },
            ..FusionConfig::default()
        };
        fuse_with_diagnostics(&channels, query, index, &config, plan.candidate_pool_limit)
    }
}

/// 对分路候选执行完整融合，并在最终截断前后记录候选身份。
///
/// `fusion::fuse` 的排序仍是唯一排序实现；这里把上限临时放宽到 union 大小，以便记录
/// post-fusion 全序，再截断到调用方要求的 `limit`。因此新增诊断不会产生第二套排序逻辑。
///
/// 同文件名额上限必须施加在**这一步**而不是 `fuse` 内部：`fuse` 收到的上限是 union 全量，
/// 它眼里"没有需要淘汰的候选"，配额自然不生效；真正决定谁被淘汰的是下面这次截断。
fn fuse_with_diagnostics(
    channels: &[Vec<SearchCandidate>],
    query: &str,
    index: &StructuralIndex,
    config: &FusionConfig,
    limit: usize,
) -> RetrievalRun {
    let mut stages = CandidateStages::from_channels(channels);
    let fusion_limit = stages.union.len().max(limit);
    let results = fusion::fuse(channels, query, index, config, fusion_limit);
    stages.post_fusion = results
        .iter()
        .map(|result| result.chunk_id.clone())
        .collect();
    let results = fusion::select_with_file_quota(results, limit, config.max_per_file);
    stages.final_results = results
        .iter()
        .map(|result| result.chunk_id.clone())
        .collect();
    RetrievalRun { results, stages }
}
