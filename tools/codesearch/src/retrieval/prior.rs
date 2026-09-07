//! 内容类型先验。
//!
//! RRF 只使用各通路内部的排名，隐含假设各通路对所有候选一视同仁。这一点在
//! 代码与文档混合的语料上不成立：自然语言查询与 Markdown 的向量相似度天然
//! 高于与代码的相似度（两边都是散文），因此文档会系统性地压过真正的实现。
//! 实测在一个 uni-app 项目里查「购物车数量增减」，前三条全部是组件 README，
//! 真实的加减实现一条都没进前列。
//!
//! 所以在融合之后、截断之前对散文做一次乘性降权。三条边界：
//!
//! 1. 只降权，不排除。文档确实是答案时（查「怎么配置环境变量」）仍然排得上来。
//! 2. 精确命中不降权。字面量或完整符号原样命中时，内容类型不该再影响判断。
//! 3. 乘性修正对同质候选集是恒等变换——纯文档仓库里所有候选都是散文，
//!    乘同一个系数不改变顺序。因此不需要额外判断「这个索引是不是以代码为主」。
//!
//! 曾经一并降权过仓库内的第三方目录（`uni_modules`、`vendor`、`libs`）。
//! 实测该维度在评测集上没有任何收益，并且会造成实际损害：查「文件上传」时，
//! 真正实现上传的是装在 `uni_modules` 里的组件，降权把它压到了项目 README
//! 之下。当初促使加入该规则的 vendored 大文本块抢占首位，根因是符号通路没有
//! 触发（见 `retrieval::query::separator_variants`），修好之后噪声自然消失。
//! 因此不保留该维度：一个无法验证收益、又会埋掉正确答案的旋钮，比没有更糟。

use std::collections::HashMap;

use crate::retrieval::fusion::Candidate;
use crate::types::Language;

/// 按内容类型对融合分数做乘性修正。
///
/// `attributes` 给出候选的语言，缺失的候选按不降权处理：宁可少降一档，
/// 也不要因为元数据取不到就把候选排到后面。修正后重新排序，保持与
/// `fusion::fuse` 一致的稳定序（分数降序、ID 升序）。
pub fn apply(
    mut candidates: Vec<Candidate>,
    languages: &HashMap<i64, Language>,
    prose_weight: f64,
) -> Vec<Candidate> {
    for candidate in &mut candidates {
        // 精确命中不参与降权。
        if candidate.exact {
            continue;
        }
        if languages
            .get(&candidate.chunk_id)
            .copied()
            .is_some_and(is_prose)
        {
            candidate.score *= prose_weight;
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

/// 是否为散文内容。
///
/// 只认 Markdown 与纯文本。配置类（JSON/YAML/TOML）不算散文：查
/// 「超时时间配置在哪」时配置文件本来就是正确答案。
fn is_prose(language: Language) -> bool {
    matches!(language, Language::Markdown | Language::Text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MatchSource;

    fn candidate(chunk_id: i64, score: f64, exact: bool) -> Candidate {
        Candidate {
            chunk_id,
            score,
            matched_by: vec![MatchSource::Vector],
            exact,
        }
    }

    /// 分数接近时代码要压过文档。
    #[test]
    fn code_outranks_prose_at_comparable_scores() {
        let candidates = vec![candidate(1, 0.0200, false), candidate(2, 0.0185, false)];
        let languages = HashMap::from([(1, Language::Markdown), (2, Language::Vue)]);
        let ranked = apply(candidates, &languages, 0.85);
        assert_eq!(ranked[0].chunk_id, 2, "代码应排在文档前面");
    }

    /// 文档明显更相关时仍然排第一，降权不是排除。
    #[test]
    fn clearly_better_prose_still_wins() {
        let candidates = vec![candidate(1, 0.050, false), candidate(2, 0.018, false)];
        let languages = HashMap::from([(1, Language::Markdown), (2, Language::Rust)]);
        let ranked = apply(candidates, &languages, 0.85);
        assert_eq!(ranked[0].chunk_id, 1);
    }

    /// 候选同质时乘性修正是恒等变换，纯文档仓库不受影响。
    #[test]
    fn uniform_content_keeps_original_order() {
        let candidates = vec![
            candidate(1, 0.030, false),
            candidate(2, 0.020, false),
            candidate(3, 0.010, false),
        ];
        let languages = HashMap::from([
            (1, Language::Markdown),
            (2, Language::Markdown),
            (3, Language::Markdown),
        ]);
        let ranked = apply(candidates, &languages, 0.85);
        let ids: Vec<i64> = ranked.iter().map(|c| c.chunk_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// 精确命中不降权。
    #[test]
    fn exact_hits_are_not_penalised() {
        let candidates = vec![candidate(1, 0.0200, true), candidate(2, 0.0195, false)];
        let languages = HashMap::from([(1, Language::Markdown), (2, Language::Vue)]);
        let ranked = apply(candidates, &languages, 0.85);
        assert_eq!(ranked[0].chunk_id, 1, "字面量原样命中文档时不应被降权");
    }

    /// 配置文件不算散文。
    #[test]
    fn configuration_is_not_treated_as_prose() {
        assert!(!is_prose(Language::Json));
        assert!(!is_prose(Language::Yaml));
        assert!(!is_prose(Language::Toml));
        assert!(is_prose(Language::Markdown));
        assert!(is_prose(Language::Text));
    }

    /// 元数据缺失时不降权。
    #[test]
    fn missing_attributes_are_left_alone() {
        let candidates = vec![candidate(1, 0.020, false)];
        let ranked = apply(candidates, &HashMap::new(), 0.85);
        assert!((ranked[0].score - 0.020).abs() < f64::EPSILON);
    }
}
