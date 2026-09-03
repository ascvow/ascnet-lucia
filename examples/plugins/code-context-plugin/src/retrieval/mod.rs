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

mod fusion;
mod lexical;
mod symbol;
mod tokenize;

#[cfg(test)]
mod fusion_tests;
#[cfg(test)]
mod tests;

pub use fusion::{FusionConfig, FusionDebug, SearchResult};
pub use symbol::SymbolMatch;
pub use tokenize::tokenize;
pub(crate) use tokenize::is_cjk as tokenize_is_cjk;

use crate::index::StructuralIndex;
use crate::model::CodeChunk;
use lexical::LexicalIndex;
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
    /// `body` 只在切词时被使用，调用返回后即可丢弃。
    pub fn index_chunk(&mut self, chunk: &CodeChunk, body: &str) {
        self.symbols.index_chunk(chunk);
        self.lexical.index_chunk(chunk, body);
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
        // 各通道取比最终结果更多的候选，否则融合退化成简单拼接。
        let channels = self.search_all(query, path_scope, fusion::candidate_limit(limit));
        fusion::fuse(&channels, query, index, config, limit)
    }
}
