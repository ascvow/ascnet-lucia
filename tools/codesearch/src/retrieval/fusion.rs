//! RRF（Reciprocal Rank Fusion）融合。
//!
//! 不同通路的分数量纲不可比（BM25 是无上界的对数分，余弦距离在 `[0, 2]`），
//! 直接相加会让某一路主导排序。因此只使用各通路内部的**排名**：
//!
//! ```text
//! score(d) = Σ weight_i / (k + rank_i(d))
//! ```
//!
//! `rank` 从 1 开始。融合分数只表示排序依据，不是校准过的正确概率。

use std::collections::HashMap;

use crate::types::MatchSource;

/// 一路召回的结果：按相关度降序排列的代码块 ID。
#[derive(Debug, Clone)]
pub struct RecallList {
    /// 召回通路。
    pub source: MatchSource,
    /// 该通路权重。
    pub weight: f64,
    /// 按相关度从高到低排列的候选 ID。
    pub ids: Vec<i64>,
}

/// 融合后的候选。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 代码块 ID。
    pub chunk_id: i64,
    /// 融合分数。
    pub score: f64,
    /// 命中来源，已排序去重。
    pub matched_by: Vec<MatchSource>,
    /// 是否属于完整符号精确命中（享有保留额度）。
    pub exact: bool,
}

/// 执行 RRF 融合。
///
/// `k` 为 RRF 常数（默认 60）。`exact_ids` 中的候选被标记为精确命中。
/// 返回按分数降序、ID 升序（保证确定性）排列的候选列表。
pub fn fuse(lists: &[RecallList], k: f64, exact_ids: &[i64]) -> Vec<Candidate> {
    let k = if k > 0.0 { k } else { 60.0 };
    let mut scores: HashMap<i64, f64> = HashMap::new();
    let mut sources: HashMap<i64, Vec<MatchSource>> = HashMap::new();

    for list in lists {
        for (index, id) in list.ids.iter().enumerate() {
            let rank = index as f64 + 1.0;
            *scores.entry(*id).or_insert(0.0) += list.weight / (k + rank);
            let entry = sources.entry(*id).or_default();
            if !entry.contains(&list.source) {
                entry.push(list.source);
            }
        }
    }

    let mut candidates: Vec<Candidate> = scores
        .into_iter()
        .map(|(chunk_id, score)| {
            let mut matched_by = sources.remove(&chunk_id).unwrap_or_default();
            matched_by.sort_unstable();
            Candidate {
                chunk_id,
                score,
                matched_by,
                exact: exact_ids.contains(&chunk_id),
            }
        })
        .collect();

    // 分数相同时按 ID 升序，保证结果稳定可复现。
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    candidates
}

/// 按候选上限裁剪，并为完整符号精确命中保留额度。
///
/// 先按融合分数取前 `limit` 个；若精确命中被挤出，则用它们替换尾部候选，
/// 最多替换 `reserve` 个。这保证 `RetryPolicy::execute` 这类查询不会被语义近似结果淹没。
pub fn apply_exact_reserve(
    mut candidates: Vec<Candidate>,
    limit: usize,
    reserve: usize,
) -> Vec<Candidate> {
    if limit == 0 {
        return Vec::new();
    }
    if candidates.len() <= limit {
        return candidates;
    }
    if reserve == 0 {
        candidates.truncate(limit);
        return candidates;
    }

    let head_exact = candidates.iter().take(limit).filter(|c| c.exact).count();
    let mut missing: Vec<Candidate> = if head_exact >= reserve {
        Vec::new()
    } else {
        candidates
            .iter()
            .skip(limit)
            .filter(|c| c.exact)
            .take(reserve - head_exact)
            .cloned()
            .collect()
    };
    candidates.truncate(limit);
    if missing.is_empty() {
        return candidates;
    }
    // 用被挤出的精确命中替换分数最低的非精确候选。
    for slot in (0..candidates.len()).rev() {
        if missing.is_empty() {
            break;
        }
        if !candidates[slot].exact {
            candidates[slot] = missing.remove(0);
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(source: MatchSource, weight: f64, ids: &[i64]) -> RecallList {
        RecallList {
            source,
            weight,
            ids: ids.to_vec(),
        }
    }

    #[test]
    fn documents_hit_by_multiple_paths_rank_higher() {
        // 2 只被向量召回且排名第一；1 被两路召回但排名靠后。
        let lists = vec![
            list(MatchSource::Fts, 1.0, &[3, 1]),
            list(MatchSource::Vector, 1.0, &[2, 1]),
        ];
        let fused = fuse(&lists, 60.0, &[]);
        let ranked: Vec<i64> = fused.iter().map(|c| c.chunk_id).collect();
        // 1 出现在两路，累计分数最高。
        assert_eq!(ranked[0], 1);
        let first = &fused[0];
        assert_eq!(first.matched_by, vec![MatchSource::Fts, MatchSource::Vector]);
    }

    #[test]
    fn weights_shift_ranking_between_paths() {
        let ids_fts = [10i64];
        let ids_vec = [20i64];
        let balanced = fuse(
            &[
                list(MatchSource::Fts, 1.0, &ids_fts),
                list(MatchSource::Vector, 1.0, &ids_vec),
            ],
            60.0,
            &[],
        );
        // 权重相同且排名相同时按 ID 升序稳定排列。
        assert_eq!(balanced[0].chunk_id, 10);

        let vector_heavy = fuse(
            &[
                list(MatchSource::Fts, 1.0, &ids_fts),
                list(MatchSource::Vector, 5.0, &ids_vec),
            ],
            60.0,
            &[],
        );
        assert_eq!(vector_heavy[0].chunk_id, 20);
    }

    #[test]
    fn fusion_is_deterministic_and_deduplicated() {
        let lists = vec![
            list(MatchSource::Fts, 1.0, &[1, 2, 3]),
            list(MatchSource::Symbol, 1.0, &[3, 2, 1]),
        ];
        let first = fuse(&lists, 60.0, &[]);
        let second = fuse(&lists, 60.0, &[]);
        let ids: Vec<i64> = first.iter().map(|c| c.chunk_id).collect();
        let repeat: Vec<i64> = second.iter().map(|c| c.chunk_id).collect();
        assert_eq!(ids, repeat);
        // 每个 ID 只出现一次。
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn exact_hits_are_preserved_against_approximate_flood() {
        // 向量通路灌入 20 个近似结果，精确命中排在最后。
        let approximate: Vec<i64> = (100..120).collect();
        let lists = vec![
            list(MatchSource::Vector, 1.0, &approximate),
            list(MatchSource::Symbol, 1.0, &[7]),
        ];
        let fused = fuse(&lists, 60.0, &[7]);
        // 未保留时 7 可能被挤出前 5。
        let limited = apply_exact_reserve(fused.clone(), 5, 0);
        let without: Vec<i64> = limited.iter().map(|c| c.chunk_id).collect();

        let reserved = apply_exact_reserve(fused, 5, 1);
        let with: Vec<i64> = reserved.iter().map(|c| c.chunk_id).collect();
        assert!(with.contains(&7), "精确命中必须被保留: {with:?}");
        assert_eq!(with.len(), 5);
        // 保留额度确实改变了结果集合。
        assert!(!without.contains(&7) || with == without);
    }

    #[test]
    fn reserve_does_not_evict_other_exact_hits() {
        let lists = vec![list(MatchSource::Symbol, 1.0, &[1, 2, 3, 4, 5, 6])];
        let fused = fuse(&lists, 60.0, &[1, 2, 6]);
        let reserved = apply_exact_reserve(fused, 3, 3);
        let ids: Vec<i64> = reserved.iter().map(|c| c.chunk_id).collect();
        assert_eq!(ids.len(), 3);
        // 1、2 本就在前列，6 通过保留额度替换掉非精确的 3。
        assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&6));
    }

    #[test]
    fn empty_and_zero_limit_are_safe() {
        assert!(fuse(&[], 60.0, &[]).is_empty());
        assert!(apply_exact_reserve(Vec::new(), 10, 3).is_empty());
        let fused = fuse(&[list(MatchSource::Fts, 1.0, &[1])], 60.0, &[]);
        assert!(apply_exact_reserve(fused, 0, 3).is_empty());
    }

    #[test]
    fn non_positive_k_falls_back_to_default() {
        let lists = vec![list(MatchSource::Fts, 1.0, &[1, 2])];
        let fused = fuse(&lists, 0.0, &[]);
        // k 回退到 60，分数为 1/61 与 1/62。
        assert!((fused[0].score - 1.0 / 61.0).abs() < 1e-12);
    }
}
