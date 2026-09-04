//! 融合层的单元与集成测试。
//!
//! Phase 9 的 Definition of Done：融合结果稳定确定、单通道缺席时正常工作、
//! 精确符号匹配不会被词法结果挤下去。此外验证"任意通道子集"不是声称而是事实——
//! 直接喂一路合成语义候选，确认三路融合无需改动代码。

use super::fusion::{candidate_limit, fuse};
use super::*;
use crate::index::{IndexBuild, StructuralIndex};
use crate::model::RepositoryIdentity;
use crate::repository::{ScanBudget, ScanProgress};
use crate::testing::RepoHost;

/// 不设限的预算，用于一次跑完。
const UNLIMITED: ScanBudget = ScanBudget {
    max_host_calls: u32::MAX,
    max_bytes_read: u64::MAX,
};

/// 与 Phase 5 相同的固定装置，便于两阶段结果对照。
const FIXTURE: &[(&str, &str)] = &[
    (
        "src/permission/manager.rs",
        r#"
/// 判断一次工具调用是否需要用户审批。
pub struct PermissionManager;

impl PermissionManager {
    /// 检查工具调用的权限，必要时要求审批。
    pub fn check(&self, tool_call_id: &str) -> bool {
        tool_call_id.is_empty()
    }
}
"#,
    ),
    (
        "src/permission/rules.rs",
        r#"
/// 一条权限规则。
pub struct PermissionRule;

impl PermissionRule {
    /// 判断规则是否匹配给定的工具。
    pub fn matches(&self, name: &str) -> bool {
        name.is_empty()
    }
}
"#,
    ),
    (
        "src/tool/executor.rs",
        r#"
/// 执行已获批准的工具调用。
pub struct ToolExecutor;

impl ToolExecutor {
    /// 真正执行一次命令。
    pub fn execute(&self, command: &str) -> String {
        execute_command(command)
    }
}

fn execute_command(command: &str) -> String {
    command.to_string()
}
"#,
    ),
    (
        "src/session/store.rs",
        r#"
/// 会话状态的持久化。
pub struct SessionStore;

impl SessionStore {
    /// 保存会话状态到磁盘。
    pub fn persist(&self) {}
}
"#,
    ),
];

/// 构造测试用仓库身份。
fn repository() -> RepositoryIdentity {
    RepositoryIdentity::from_workspace_root("/repo/test").expect("测试仓库身份")
}

/// 建立固定装置的结构索引与检索索引。
fn fixture() -> (StructuralIndex, RetrievalIndex) {
    let host = RepoHost::new(FIXTURE);
    let mut build = IndexBuild::with_default_limits(repository());
    assert_eq!(
        build.advance(&host, UNLIMITED),
        ScanProgress::Complete,
        "固定装置应一次构建完成"
    );
    build.into_parts()
}

/// 返回结果的限定名列表。
fn names(results: &[SearchResult]) -> Vec<String> {
    results
        .iter()
        .map(|result| {
            result
                .qualified_symbol
                .clone()
                .unwrap_or_else(|| result.file_path.clone())
        })
        .collect()
}

/// 构造一条合成候选。
fn candidate(chunk_id: &str, source: CandidateSource, rank: u32) -> SearchCandidate {
    SearchCandidate {
        chunk_id: chunk_id.to_string(),
        source,
        rank,
        score: 1.0 / rank as f32,
        symbol_match: None,
    }
}

/// 取固定装置中某个限定名对应的 chunk ID。
fn chunk_id_of(index: &StructuralIndex, qualified: &str) -> String {
    index
        .chunks()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some(qualified))
        .unwrap_or_else(|| panic!("固定装置应含符号 {qualified}"))
        .id
        .clone()
}

// ---------------------------------------------------------------------------
// RRF 基本性质
// ---------------------------------------------------------------------------

/// 多路都命中的结果必须胜过只有一路命中的结果。
///
/// 这是 RRF 的核心取向：跨通道一致性比单通道排名更能说明相关性。
#[test]
fn agreement_across_channels_outranks_single_channel() {
    let (index, _) = fixture();
    let both = chunk_id_of(&index, "PermissionManager::check");
    let single = chunk_id_of(&index, "SessionStore::persist");

    let channels = vec![
        vec![
            candidate(&single, CandidateSource::Symbol, 1),
            candidate(&both, CandidateSource::Symbol, 5),
        ],
        vec![candidate(&both, CandidateSource::Lexical, 5)],
    ];

    let results = fuse(&channels, "", &index, &FusionConfig::default(), 10);

    assert_eq!(
        results[0].chunk_id,
        both,
        "两路各排第 5 应胜过单路第 1，实际 {:?}",
        names(&results)
    );
}

/// 同一通道内排名靠前的结果必须得分更高。
#[test]
fn better_rank_scores_higher_within_a_channel() {
    let (index, _) = fixture();
    let first = chunk_id_of(&index, "PermissionManager::check");
    let second = chunk_id_of(&index, "PermissionRule::matches");

    let channels = vec![vec![
        candidate(&first, CandidateSource::Lexical, 1),
        candidate(&second, CandidateSource::Lexical, 2),
    ]];

    let results = fuse(&channels, "", &index, &FusionConfig::default(), 10);

    assert_eq!(results[0].chunk_id, first);
    assert!(results[0].score > results[1].score);
}

/// 融合必须在任意通道子集上工作，包括空输入与单通道。
#[test]
fn fusion_works_on_any_channel_subset() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "PermissionManager::check");

    // 没有通道。
    assert!(fuse(&[], "", &index, &FusionConfig::default(), 10).is_empty());
    // 全是空通道。
    assert!(fuse(&[vec![], vec![]], "", &index, &FusionConfig::default(), 10).is_empty());
    // 只有一路。
    let single = fuse(
        &[vec![candidate(&target, CandidateSource::Lexical, 1)]],
        "",
        &index,
        &FusionConfig::default(),
        10,
    );
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].chunk_id, target);
}

/// 三路融合必须无需改动代码即可工作。
///
/// 这条把"为语义分支预留"从声称变成事实：直接喂一路合成语义候选，
/// 融合层照常处理，Phase 8 落地时不需要重构。
#[test]
fn three_channel_fusion_needs_no_code_change() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "ToolExecutor::execute");
    let other = chunk_id_of(&index, "SessionStore::persist");

    let channels = vec![
        vec![candidate(&other, CandidateSource::Symbol, 1)],
        vec![candidate(&target, CandidateSource::Lexical, 2)],
        vec![candidate(&target, CandidateSource::Semantic, 1)],
    ];

    let results = fuse(&channels, "", &index, &FusionConfig::default(), 10);

    assert_eq!(
        results[0].chunk_id,
        target,
        "两路命中应胜过单路，实际 {:?}",
        names(&results)
    );
    let sources: Vec<CandidateSource> = results[0]
        .debug
        .ranks
        .iter()
        .map(|(source, _)| *source)
        .collect();
    assert!(sources.contains(&CandidateSource::Semantic));
    assert!(sources.contains(&CandidateSource::Lexical));
}

/// 结果顺序必须完全确定。
#[test]
fn fusion_is_deterministic() {
    let (index, retrieval) = fixture();
    let first = retrieval.search(&index, "permission check", None, 10);
    let second = retrieval.search(&index, "permission check", None, 10);
    assert_eq!(first, second);
}

/// 分数相同的结果必须按 chunk ID 稳定排序。
#[test]
fn ties_are_broken_deterministically() {
    let (index, _) = fixture();
    let left = chunk_id_of(&index, "PermissionManager::check");
    let right = chunk_id_of(&index, "PermissionRule::matches");

    let channels = vec![vec![
        candidate(&left, CandidateSource::Lexical, 3),
        candidate(&right, CandidateSource::Lexical, 3),
    ]];

    let results = fuse(&channels, "", &index, &FusionConfig::default(), 10);
    assert_eq!(results[0].score, results[1].score, "前提：分数相同");
    assert!(
        results[0].chunk_id < results[1].chunk_id,
        "分数相同时必须按 chunk ID 升序"
    );
}

/// 结果数量必须受 limit 约束。
#[test]
fn limit_is_respected() {
    let (index, retrieval) = fixture();
    assert!(retrieval.search(&index, "permission", None, 2).len() <= 2);
    assert!(retrieval.search(&index, "permission", None, 0).is_empty());
}

/// 候选池必须多于最终结果，且扩展受上限约束。
#[test]
fn candidate_pool_exceeds_result_limit() {
    assert!(candidate_limit(5) > 5, "否则融合退化成简单拼接");
    assert_eq!(candidate_limit(100), 200, "扩展倍数必须受上限约束");
    // 请求数本身超过上限时以请求数为准：候选池少于结果数会让结果无谓地变少。
    assert_eq!(candidate_limit(1000), 1000);
    assert_eq!(candidate_limit(0), 0);
    for limit in [1, 7, 50, 199, 200, 201, 500] {
        assert!(
            candidate_limit(limit) >= limit,
            "候选池不得少于结果数，limit={limit}"
        );
    }
}

// ---------------------------------------------------------------------------
// 提升
// ---------------------------------------------------------------------------

/// 精确符号匹配不得被词法结果挤下去。
///
/// 这是 Definition of Done 的硬性要求：用户查一个确切的符号名时，那个符号必须排第一，
/// 即使它只被符号通道命中，而竞争者被多路命中。
#[test]
fn exact_symbol_match_outranks_multi_channel_lexical() {
    let (index, _) = fixture();
    let exact = chunk_id_of(&index, "PermissionManager::check");
    let noisy = chunk_id_of(&index, "PermissionRule::matches");

    let mut exact_candidate = candidate(&exact, CandidateSource::Symbol, 1);
    exact_candidate.symbol_match = Some(SymbolMatch::ExactQualified);

    let channels = vec![
        vec![exact_candidate],
        // 竞争者被词法通道以最好的排名命中。
        vec![candidate(&noisy, CandidateSource::Lexical, 1)],
        vec![candidate(&noisy, CandidateSource::Semantic, 1)],
    ];

    let results = fuse(
        &channels,
        "PermissionManager::check",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert_eq!(
        results[0].chunk_id,
        exact,
        "精确符号匹配必须压过双路命中，实际 {:?}",
        names(&results)
    );
    assert!(results[0].debug.symbol_boost > 0.0);
}

/// 自然语言里夹带的普通符号词不得靠符号与词法重复计分压过真正的语义命中。
#[test]
fn natural_language_prefers_semantic_match_over_generic_symbol_word() {
    let (index, _) = fixture();
    let generic = chunk_id_of(&index, "PermissionManager::check");
    let relevant = chunk_id_of(&index, "SessionStore::persist");
    let mut generic_symbol = candidate(&generic, CandidateSource::Symbol, 1);
    generic_symbol.symbol_match = Some(SymbolMatch::ExactName);
    let channels = vec![
        vec![generic_symbol],
        vec![candidate(&generic, CandidateSource::Lexical, 1)],
        vec![candidate(&relevant, CandidateSource::Semantic, 1)],
    ];

    let results = fuse(
        &channels,
        "一个agent对话",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert_eq!(results[0].chunk_id, relevant, "实际 {:?}", names(&results));
    let generic_result = results
        .iter()
        .find(|result| result.chunk_id == generic)
        .expect("普通符号候选应仍被保留");
    assert_eq!(generic_result.debug.symbol_boost, 0.0);
}

/// 语义通道缺席时必须恢复符号通道标准权重，保证纯词法降级仍能合并代码导航信号。
#[test]
fn natural_language_fallback_keeps_full_symbol_weight() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "PermissionManager::check");
    let competitor = chunk_id_of(&index, "SessionStore::persist");
    let mut target_symbol = candidate(&target, CandidateSource::Symbol, 20);
    target_symbol.symbol_match = Some(SymbolMatch::NamePrefix);
    let channels = vec![
        vec![target_symbol],
        vec![
            candidate(&competitor, CandidateSource::Lexical, 1),
            candidate(&target, CandidateSource::Lexical, 20),
        ],
    ];

    let results = fuse(
        &channels,
        "查找权限处理逻辑",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert_eq!(results[0].chunk_id, target, "实际 {:?}", names(&results));
    assert_eq!(results[0].debug.symbol_boost, 0.0);
}

/// 纯词法降级也不能把自然语言中的普通精确词重复当成代码导航意图。
#[test]
fn natural_language_fallback_still_suppresses_generic_exact_symbol() {
    let (index, _) = fixture();
    let generic = chunk_id_of(&index, "PermissionManager::check");
    let relevant = chunk_id_of(&index, "SessionStore::persist");
    let mut generic_symbol = candidate(&generic, CandidateSource::Symbol, 20);
    generic_symbol.symbol_match = Some(SymbolMatch::ExactName);
    let channels = vec![
        vec![generic_symbol],
        vec![
            candidate(&relevant, CandidateSource::Lexical, 1),
            candidate(&generic, CandidateSource::Lexical, 20),
        ],
    ];

    let results = fuse(
        &channels,
        "一个agent对话",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert_eq!(results[0].chunk_id, relevant, "实际 {:?}", names(&results));
}

/// 弱符号匹配不得获得提升。
///
/// 后缀与前缀匹配本就是弱信号，再提升会让"查 check 命中所有名字带 check 的东西"
/// 淹没真正的目标。
#[test]
fn weak_symbol_matches_get_no_boost() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "PermissionManager::check");

    for weak in [SymbolMatch::QualifiedSuffix, SymbolMatch::NamePrefix] {
        let mut item = candidate(&target, CandidateSource::Symbol, 1);
        item.symbol_match = Some(weak);
        let results = fuse(&[vec![item]], "", &index, &FusionConfig::default(), 10);
        assert_eq!(results[0].debug.symbol_boost, 0.0, "{weak:?} 不应获得提升");
    }
}

/// 精确限定名的提升必须强于精确短名。
#[test]
fn qualified_match_boost_exceeds_name_match() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "PermissionManager::check");

    let boost_of = |kind: SymbolMatch| {
        let mut item = candidate(&target, CandidateSource::Symbol, 1);
        item.symbol_match = Some(kind);
        fuse(
            &[vec![item]],
            "PermissionManager::check",
            &index,
            &FusionConfig::default(),
            10,
        )[0]
        .debug
        .symbol_boost
    };

    assert!(boost_of(SymbolMatch::ExactQualified) > boost_of(SymbolMatch::ExactName));
}

/// 查询词命中路径必须获得提升。
#[test]
fn path_match_receives_boost() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "SessionStore::persist");

    let channels = vec![vec![candidate(&target, CandidateSource::Lexical, 1)]];
    let with_path = fuse(
        &channels,
        "session store",
        &index,
        &FusionConfig::default(),
        10,
    );
    let without_path = fuse(
        &channels,
        "unrelated words",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert!(with_path[0].debug.path_boost > 0.0, "路径命中应获得提升");
    assert_eq!(without_path[0].debug.path_boost, 0.0);
    assert!(with_path[0].score > without_path[0].score);
}

/// 路径提升必须弱于精确符号提升。
///
/// 路径相关是有用信号，但让它盖过符号命中会让"搜某个函数"变成"搜某个目录"。
#[test]
fn path_boost_is_weaker_than_symbol_boost() {
    let config = FusionConfig::default();
    assert!(config.metadata_rerank_boost < config.path_boost);
    assert!(config.path_boost < config.name_match_boost);
    assert!(config.name_match_boost < config.exact_symbol_boost);
    assert!(config.natural_language_symbol_weight < 1.0);
    assert!(config.natural_language_semantic_weight > 1.0);
}

/// 元数据覆盖更多查询词的候选应在原始分数接近时获得更高排名。
#[test]
fn metadata_rerank_promotes_better_query_coverage() {
    let (index, _) = fixture();
    let relevant = chunk_id_of(&index, "PermissionManager::check");
    let noisy = chunk_id_of(&index, "PermissionRule::matches");
    let channels = vec![vec![
        candidate(&noisy, CandidateSource::Lexical, 1),
        candidate(&relevant, CandidateSource::Lexical, 5),
    ]];

    let results = fuse(
        &channels,
        "工具 权限 审批",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert_eq!(results[0].chunk_id, relevant, "实际 {:?}", names(&results));
    assert_eq!(results[0].debug.rerank_matched_tokens, 3);
    assert_eq!(results[0].debug.rerank_query_tokens, 3);
    assert!(results[0].debug.rerank_boost > results[1].debug.rerank_boost);
}

/// 轻量重排只能调整接近的候选，不能覆盖原始融合形成的大幅领先。
#[test]
fn metadata_rerank_remains_secondary_to_fusion_score() {
    let (index, _) = fixture();
    let first = chunk_id_of(&index, "PermissionRule::matches");
    let metadata_match = chunk_id_of(&index, "SessionStore");
    let channels = vec![vec![
        candidate(&first, CandidateSource::Lexical, 1),
        candidate(&metadata_match, CandidateSource::Lexical, 200),
    ]];

    let results = fuse(
        &channels,
        "会话 状态 持久化",
        &index,
        &FusionConfig::default(),
        10,
    );

    assert_eq!(results[0].chunk_id, first, "实际 {:?}", names(&results));
    let matched = results
        .iter()
        .find(|result| result.chunk_id == metadata_match)
        .expect("元数据候选应保留在结果中");
    assert_eq!(matched.debug.rerank_matched_tokens, 4);
    assert_eq!(matched.debug.rerank_query_tokens, 4);
    assert!(matched.debug.rerank_boost > 0.0);
}

/// 全部提升必须以 rank-1 当量表达，使量级可读。
#[test]
fn boosts_are_expressed_in_rank_one_units() {
    let config = FusionConfig::default();
    let unit = config.rank_one_unit();

    assert!(
        (unit - 1.0 / 61.0).abs() < 1e-6,
        "默认 k=60 时单位应为 1/61"
    );
    // 精确符号提升相当于额外在三路里各排第一，足以压过双路中等命中。
    assert!(config.exact_symbol_boost * unit > 2.0 * unit);
}

// ---------------------------------------------------------------------------
// 内部诊断
// ---------------------------------------------------------------------------

/// 诊断必须记录每一路的命中排名。
#[test]
fn debug_records_every_channel_rank() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "PermissionManager::check");

    let channels = vec![
        vec![candidate(&target, CandidateSource::Symbol, 2)],
        vec![candidate(&target, CandidateSource::Lexical, 7)],
    ];

    let results = fuse(&channels, "", &index, &FusionConfig::default(), 10);
    let ranks = &results[0].debug.ranks;

    assert_eq!(ranks.len(), 2);
    assert!(ranks.contains(&(CandidateSource::Symbol, 2)));
    assert!(ranks.contains(&(CandidateSource::Lexical, 7)));
}

/// 诊断理由必须可读且反映实际构成。
#[test]
fn debug_reason_is_readable() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "SessionStore::persist");
    let mut item = candidate(&target, CandidateSource::Symbol, 1);
    item.symbol_match = Some(SymbolMatch::ExactQualified);

    let results = fuse(
        &[vec![item]],
        "session",
        &index,
        &FusionConfig::default(),
        10,
    );
    let reason = results[0].debug.reason();

    assert!(reason.contains("symbol#1"), "{reason}");
    assert!(reason.contains("符号提升"), "{reason}");
    assert!(reason.contains("路径提升"), "{reason}");
    assert!(reason.contains("元数据重排"), "{reason}");
}

/// 分数必须等于 RRF 分与各项提升之和，使诊断可核对。
#[test]
fn score_decomposes_into_its_parts() {
    let (index, _) = fixture();
    let target = chunk_id_of(&index, "SessionStore::persist");
    let mut item = candidate(&target, CandidateSource::Symbol, 3);
    item.symbol_match = Some(SymbolMatch::ExactName);

    let results = fuse(
        &[vec![item]],
        "session store",
        &index,
        &FusionConfig::default(),
        10,
    );
    let result = &results[0];
    let sum = result.debug.rrf_score
        + result.debug.symbol_boost
        + result.debug.path_boost
        + result.debug.rerank_boost
        - result.debug.penalty;

    assert!(
        (result.score - sum).abs() < 1e-6,
        "分数 {} 应等于各部分之和 {sum}",
        result.score
    );
}

// ---------------------------------------------------------------------------
// 端到端
// ---------------------------------------------------------------------------

/// 端到端混合检索必须把精确符号排在首位。
#[test]
fn end_to_end_search_ranks_exact_symbol_first() {
    let (index, retrieval) = fixture();
    let results = retrieval.search(&index, "PermissionManager::check", None, 10);

    assert!(!results.is_empty());
    assert_eq!(
        results[0].qualified_symbol.as_deref(),
        Some("PermissionManager::check"),
        "实际 {:?}",
        names(&results)
    );
}

/// 端到端结果必须带上完整定位信息，供上下文构建器使用。
#[test]
fn results_carry_location_metadata() {
    let (index, retrieval) = fixture();
    let results = retrieval.search(&index, "PermissionManager::check", None, 5);
    let result = &results[0];

    assert_eq!(result.file_path, "src/permission/manager.rs");
    assert!(result.start_line > 0);
    assert!(result.end_line >= result.start_line);
    assert_eq!(result.language, "rust");
    assert_eq!(result.symbol_name.as_deref(), Some("check"));
    assert_eq!(result.parent_symbol.as_deref(), Some("PermissionManager"));
    assert!(result.signature.is_some());
    assert!(result.doc_comment.is_some());
}

/// 自然语言查询必须返回合理结果。
#[test]
fn natural_language_query_returns_relevant_results() {
    let (index, retrieval) = fixture();
    let results = retrieval.search(&index, "where is permission checked for a tool", None, 5);

    assert!(!results.is_empty());
    assert!(
        names(&results)
            .iter()
            .any(|name| name.contains("Permission")),
        "实际 {:?}",
        names(&results)
    );
}

/// 路径过滤必须贯穿融合。
#[test]
fn path_scope_survives_fusion() {
    let (index, retrieval) = fixture();
    let results = retrieval.search(&index, "permission", Some("src/permission/"), 10);

    assert!(!results.is_empty());
    for result in &results {
        assert!(
            result.file_path.starts_with("src/permission/"),
            "越界结果 {}",
            result.file_path
        );
    }
}

/// 检索不到内容时必须返回空而不是 panic。
#[test]
fn unmatched_query_returns_empty() {
    let (index, retrieval) = fixture();
    assert!(retrieval
        .search(&index, "zzzzznonexistenttoken", None, 10)
        .is_empty());
}

/// 空索引上检索必须安全返回空。
#[test]
fn empty_index_returns_empty() {
    let index = StructuralIndex::new(repository());
    let mut retrieval = RetrievalIndex::new();
    retrieval.finalize();

    assert!(retrieval.search(&index, "anything", None, 10).is_empty());
}

/// 候选指向索引中不存在的 chunk 时必须被跳过而不是 panic。
///
/// 这在增量索引期间可能真实发生：检索索引尚未与结构索引同步。
#[test]
fn stale_candidates_are_skipped() {
    let (index, _) = fixture();
    let valid = chunk_id_of(&index, "PermissionManager::check");

    let channels = vec![vec![
        candidate("chunk-that-no-longer-exists", CandidateSource::Lexical, 1),
        candidate(&valid, CandidateSource::Lexical, 2),
    ]];

    let results = fuse(&channels, "", &index, &FusionConfig::default(), 10);

    assert_eq!(results.len(), 1, "失效候选必须被跳过");
    assert_eq!(results[0].chunk_id, valid);
}

/// 标识符形态判定必须认出真实标识符，同时放过普通英文词与全大写缩写。
///
/// 这条边界直接决定自然语言查询能不能拿到符号提升：放得太松，"RRF 融合"里的 `RRF`
/// 会被当成标识符，把提升发给一大批同名常量；收得太紧，"parse_file 代码解析"就退回
/// 只能返回调用点的旧行为。
#[test]
fn identifier_shape_detection_separates_symbols_from_plain_words() {
    for query in [
        "parse_file 代码解析",
        "工具函数 formatDate 定义在哪",
        "HTTPServer 怎么启动",
        "PermissionManager::check 在哪",
        "看看 agent.rs 里的实现",
    ] {
        assert!(
            !super::AnalyzedQuery::analyze(query).identifiers.is_empty(),
            "`{query}` 应被识别为夹带标识符"
        );
    }
    for query in [
        "RRF 融合",
        "HTTP 客户端",
        "嵌入向量 provider",
        "索引 manifest 持久化",
        "Session 是怎么保存的",
        "权限检查流程",
    ] {
        assert!(
            super::AnalyzedQuery::analyze(query).identifiers.is_empty(),
            "`{query}` 不应被识别为夹带标识符"
        );
    }
}

/// 自然语言查询夹带标识符时，符号精确命中必须拿到折减后的提升而不是零。
///
/// 归零正是"返回各使用点、函数本体没出现"的成因：调用点所在的 chunk 更短，BM25 长度
/// 归一化更占便宜，定义本身反而排在后面。
#[test]
fn natural_language_with_identifier_keeps_reduced_symbol_boost() {
    let config = FusionConfig::default();
    let identifier = super::fusion::symbol_boost_of(
        Some(SymbolMatch::ExactName),
        super::QueryIntent::Identifier,
        true,
        &config,
    );
    let mixed = super::fusion::symbol_boost_of(
        Some(SymbolMatch::ExactName),
        super::QueryIntent::Mixed,
        true,
        &config,
    );
    let plain = super::fusion::symbol_boost_of(
        Some(SymbolMatch::ExactName),
        super::QueryIntent::NaturalLanguage,
        false,
        &config,
    );

    assert!(mixed > 0.0, "夹带标识符时必须保留提升");
    assert!(mixed < identifier, "混合查询的提升必须弱于纯标识符查询");
    assert_eq!(plain, 0.0, "不含标识符的自然语言查询不得获得符号提升");
}
