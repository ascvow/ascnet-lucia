//! 检索：查询分析、三路召回与 RRF 融合。
//!
//! 三路召回相互独立，任一路不可用时其余仍然工作：
//!
//! - **精确**：符号表、路径、受作用域限制的源码字面量子串；
//! - **FTS5**：路径 / 符号 / 文档 / 代码四个字段的拆词全文检索；
//! - **向量**：USearch 近邻检索，仅在存在可用 Embedding 空间时启用。
//!
//! 三路结果只用排名参与 [`fusion`] 的 RRF 合并，不直接相加 BM25 与余弦距离。
//! 所有候选在融合前都已按当前版本与作用域过滤。

pub mod fusion;
pub mod prior;
pub mod query;
pub mod tokenize;

use std::collections::{HashMap, HashSet};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::RetrievalConfig;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::types::{Language, MatchSource, ScopeFilter};
use crate::vector::VectorIndex;

pub use fusion::{Candidate, RecallList};
pub use query::{analyze, Intent, QueryKind, QueryPlan};

/// 作用域集合小于该阈值时，直接精确计算向量距离而不走 HNSW 近邻。
const EXACT_VECTOR_THRESHOLD: usize = 256;
/// 单个精确通路的候选上限。
const EXACT_PATH_LIMIT: usize = 64;

/// 检索输入。
#[derive(Debug)]
pub struct RetrievalInput<'a> {
    /// 查询分析结果。
    pub plan: &'a QueryPlan,
    /// 查询向量；为 `None` 表示向量通路不可用。
    pub query_vector: Option<&'a [f32]>,
    /// 当前 Embedding 空间；为 `None` 表示尚未建立向量索引。
    pub space_id: Option<&'a str>,
}

/// 检索结果。
#[derive(Debug, Default)]
pub struct RetrievalOutcome {
    /// 融合并裁剪后的候选。
    pub candidates: Vec<Candidate>,
    /// 本次是否真实使用了向量召回。
    pub vector_used: bool,
    /// 降级或诊断说明。
    pub notes: Vec<String>,
}

/// 路径与语言作用域匹配器。
///
/// include 为空表示不限制；exclude 优先级高于 include。
#[derive(Debug)]
pub struct ScopeMatcher {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    languages: Vec<Language>,
}

impl ScopeMatcher {
    /// 编译作用域过滤。非法 glob 会返回 [`Error::InvalidQuery`]。
    pub fn new(scope: &ScopeFilter) -> Result<Self> {
        Ok(Self {
            include: build_glob_set(&scope.include)?,
            exclude: build_glob_set(&scope.exclude)?,
            languages: scope.languages.clone(),
        })
    }

    /// 是否没有任何限制。
    pub fn is_unrestricted(&self) -> bool {
        self.include.is_none() && self.exclude.is_none() && self.languages.is_empty()
    }

    /// 判断一个路径与语言是否在作用域内。
    pub fn matches(&self, path: &str, language: Language) -> bool {
        if !self.languages.is_empty() && !self.languages.contains(&language) {
            return false;
        }
        if let Some(exclude) = &self.exclude {
            if exclude.is_match(path) {
                return false;
            }
        }
        match &self.include {
            Some(include) => include.is_match(path),
            None => true,
        }
    }
}

fn build_glob_set(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|e| Error::InvalidQuery(format!("glob `{pattern}` 无效: {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| Error::InvalidQuery(format!("glob 编译失败: {e}")))?;
    Ok(Some(set))
}

/// 执行三路召回与融合。
pub fn retrieve(
    store: &Store,
    vector_index: Option<&VectorIndex>,
    config: &RetrievalConfig,
    input: &RetrievalInput<'_>,
) -> Result<RetrievalOutcome> {
    let plan = input.plan;
    let matcher = ScopeMatcher::new(&plan.scope)?;
    let mut outcome = RetrievalOutcome::default();

    // 作用域允许集合：一次查询取回，供向量过滤回调与各通路复用。
    // 只有在需要向量召回或存在作用域限制时才构建，避免小查询扫全表。
    let needs_allowed_set = input.query_vector.is_some() || !matcher.is_unrestricted();
    let allowed: Option<HashSet<i64>> = if needs_allowed_set {
        let scopes = store.current_chunk_scopes()?;
        Some(
            scopes
                .into_iter()
                .filter(|row| matcher.matches(&row.path, row.language))
                .map(|row| row.id)
                .collect(),
        )
    } else {
        None
    };
    let keep = |id: i64| -> bool {
        match &allowed {
            Some(set) => set.contains(&id),
            None => true,
        }
    };

    let mut lists: Vec<RecallList> = Vec::new();
    let mut exact_ids: Vec<i64> = Vec::new();

    // ---- A. 精确召回 ----
    let mut exact: Vec<i64> = Vec::new();
    for symbol in &plan.symbols {
        // 限定名的层级分隔符各语言不同（`::`/`.`/`\`/`#`），使用者不该被要求
        // 先想清楚"这个语言用哪个"再搜。原写法排在最前，变体只是补充。
        let spellings = std::iter::once(symbol.clone())
            .chain(query::separator_variants(symbol))
            .collect::<Vec<_>>();
        for spelling in &spellings {
            for (chunk_id, is_exact) in store.symbol_candidates(spelling, EXACT_PATH_LIMIT)? {
                if !keep(chunk_id) {
                    continue;
                }
                push_unique(&mut exact, chunk_id);
                if is_exact && !exact_ids.contains(&chunk_id) {
                    exact_ids.push(chunk_id);
                }
            }
        }
        // 完整符号未命中时，退回后缀匹配（查询 `execute` 命中 `RetryPolicy::execute`）。
        if exact.is_empty() {
            for chunk_id in store.symbol_suffix_candidates(symbol, EXACT_PATH_LIMIT)? {
                if keep(chunk_id) {
                    push_unique(&mut exact, chunk_id);
                }
            }
        }
    }
    for path in &plan.paths {
        for chunk_id in store.path_candidates(path, EXACT_PATH_LIMIT)? {
            if keep(chunk_id) {
                push_unique(&mut exact, chunk_id);
            }
        }
    }
    for literal in &plan.literals {
        let hits =
            store.literal_candidates(literal, config.literal_scan_limit, EXACT_PATH_LIMIT)?;
        for chunk_id in hits {
            if keep(chunk_id) {
                push_unique(&mut exact, chunk_id);
                // 字面量原样命中源码，视为精确证据。
                if !exact_ids.contains(&chunk_id) {
                    exact_ids.push(chunk_id);
                }
            }
        }
    }
    if !exact.is_empty() {
        lists.push(RecallList {
            source: if plan.symbols.is_empty() && plan.paths.is_empty() {
                MatchSource::Literal
            } else if plan.symbols.is_empty() {
                MatchSource::Path
            } else {
                MatchSource::Symbol
            },
            weight: config.weight_exact,
            ids: exact,
        });
    }

    // ---- B. FTS5 召回 ----
    if let Some(expression) = tokenize::build_match_expression(&plan.terms) {
        // 表达式由受控词条构造并已转义，用户原始查询不会进入 FTS 语法。
        match store.fts_candidates(&expression, config.fts_top_k) {
            Ok(hits) => {
                let ids: Vec<i64> = hits.into_iter().filter(|id| keep(*id)).collect();
                if !ids.is_empty() {
                    lists.push(RecallList {
                        source: MatchSource::Fts,
                        weight: config.weight_fts,
                        ids,
                    });
                }
            }
            Err(error) => {
                // 单路失败不应让整次查询失败。
                outcome.notes.push(format!("FTS 召回不可用: {error}"));
            }
        }
    }

    // ---- C. 向量召回 ----
    match (input.query_vector, vector_index, input.space_id) {
        (Some(query_vector), Some(index), Some(space_id)) => {
            let allowed_set = allowed.as_ref().expect("需要向量召回时必定已构建允许集合");
            match vector_recall(store, index, config, query_vector, space_id, allowed_set) {
                Ok(ids) => {
                    if !ids.is_empty() {
                        outcome.vector_used = true;
                        lists.push(RecallList {
                            source: MatchSource::Vector,
                            weight: config.weight_vector,
                            ids,
                        });
                    } else if index.is_empty() {
                        outcome
                            .notes
                            .push("向量索引为空，语义召回未生效".to_string());
                    }
                }
                Err(error) => {
                    outcome.notes.push(format!("向量召回不可用: {error}"));
                }
            }
        }
        _ => {
            outcome
                .notes
                .push("未启用向量召回，仅使用符号与全文检索".to_string());
        }
    }

    // 旁路记录本次调用的原始排序，不重新检索或向搜索传入评测真值。
    crate::diagnostics::record("recall", || serde_json::json!(lists));
    let fused = fusion::fuse(&lists, config.rrf_k, &exact_ids);
    crate::diagnostics::record("fused", || serde_json::json!(fused));
    // 内容类型先验必须在截断之前生效：先截断再降权，被文档挤出候选集的代码
    // 就再也回不来了。
    let fused = apply_content_prior(store, config, fused)?;
    crate::diagnostics::record("prior", || serde_json::json!(fused));
    outcome.candidates =
        fusion::apply_exact_reserve(fused, config.candidate_limit, config.exact_reserve);
    crate::diagnostics::record("candidates", || serde_json::json!(outcome.candidates));
    Ok(outcome)
}

/// 按内容类型对融合结果做乘性修正。
///
/// 只为当前候选取一次元数据，不扫全表。取元数据失败时原样返回：排序修正是
/// 锦上添花，不该让整次查询失败。
fn apply_content_prior(
    store: &Store,
    config: &RetrievalConfig,
    candidates: Vec<Candidate>,
) -> Result<Vec<Candidate>> {
    if candidates.is_empty() || config.prose_weight == 1.0 {
        return Ok(candidates);
    }
    let ids: Vec<i64> = candidates.iter().map(|c| c.chunk_id).collect();
    let rows = match store.chunks_by_ids(&ids) {
        Ok(rows) => rows,
        Err(_) => return Ok(candidates),
    };
    let languages: HashMap<i64, Language> = rows.iter().map(|row| (row.id, row.language)).collect();
    Ok(prior::apply(candidates, &languages, config.prose_weight))
}

/// 向量召回。
///
/// 作用域集合很小时直接精确计算距离；否则用 USearch 近邻检索并对不足结果做有界 over-fetch。
fn vector_recall(
    store: &Store,
    index: &VectorIndex,
    config: &RetrievalConfig,
    query_vector: &[f32],
    space_id: &str,
    allowed: &HashSet<i64>,
) -> Result<Vec<i64>> {
    if allowed.is_empty() {
        return Ok(Vec::new());
    }
    let wanted = config.vector_top_k;

    // 范围很小时精确计算：避免 HNSW 在强过滤下召回不稳定。
    if allowed.len() <= EXACT_VECTOR_THRESHOLD {
        let ids: Vec<i64> = allowed.iter().copied().collect();
        let vectors = store.vectors_for_chunks(space_id, &ids)?;
        let mut scored: Vec<(i64, f32)> = vectors
            .into_iter()
            .map(|(id, vector)| {
                let similarity = crate::models::cosine_similarity(query_vector, &vector);
                (id, similarity)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        scored.truncate(wanted);
        return Ok(scored.into_iter().map(|(id, _)| id).collect());
    }

    // HNSW 过滤检索。结果不足时按倍数放大，倍数有上限，保证最坏情况工作量有界。
    let mut multiplier = 1usize;
    let mut hits;
    loop {
        let count = wanted.saturating_mul(multiplier).min(allowed.len());
        hits = index.search(query_vector, count, allowed)?;
        if hits.len() >= wanted
            || multiplier >= config.vector_overfetch_max
            || count >= allowed.len()
        {
            break;
        }
        multiplier *= 2;
    }
    hits.truncate(wanted);
    Ok(hits.into_iter().map(|(id, _)| id).collect())
}

fn push_unique(list: &mut Vec<i64>, value: i64) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests;
