//! 上下文构建的单元与集成测试。
//!
//! Phase 10 的 Definition of Done：输出满足 token 预算、不含大量重复代码、父子重叠被抑制。
//! 此外验证架构第 8 问的惰性一致性校验——文件被外部修改后位置能被修正。

use super::*;
use crate::index::IndexBuild;
use crate::model::RepositoryIdentity;
use crate::repository::{ScanBudget, ScanProgress};
use crate::retrieval::RetrievalIndex;
use crate::testing::RepoHost;

/// 不设限的预算，用于一次跑完。
const UNLIMITED: ScanBudget = ScanBudget {
    max_host_calls: u32::MAX,
    max_bytes_read: u64::MAX,
};

/// 固定装置：含 impl 块与其方法，天然产生父子重叠。
const FIXTURE: &[(&str, &str)] = &[
    (
        "src/permission/manager.rs",
        r#"/// 判断一次工具调用是否需要用户审批。
pub struct PermissionManager;

impl PermissionManager {
    /// 检查工具调用的权限，必要时要求审批。
    pub fn check(&self, tool_call_id: &str) -> bool {
        tool_call_id.is_empty()
    }

    /// 记录一次审批决定。
    pub fn record(&self, decision: bool) -> bool {
        decision
    }
}
"#,
    ),
    (
        "src/session/store.rs",
        r#"/// 会话状态的持久化。
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

/// 建立固定装置的索引。
fn fixture(files: &[(&str, &str)]) -> (RepoHost, StructuralIndex, RetrievalIndex) {
    let host = RepoHost::new(files);
    let mut build = IndexBuild::with_default_limits(repository());
    assert_eq!(
        build.advance(&host, UNLIMITED),
        ScanProgress::Complete,
        "固定装置应一次构建完成"
    );
    let (index, retrieval) = build.into_parts();
    (host, index, retrieval)
}

/// 检索并打包。
fn search_and_build(
    host: &RepoHost,
    index: &StructuralIndex,
    retrieval: &RetrievalIndex,
    query: &str,
    config: &ContextConfig,
) -> ContextBundle {
    let results = retrieval.search(index, query, None, 50);
    build(host, index, results, config)
}

// ---------------------------------------------------------------------------
// 父子抑制与区间合并
// ---------------------------------------------------------------------------

/// 父子重叠必须被抑制，同一段代码不得返回两次。
#[test]
fn parent_child_overlap_is_suppressed() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let bundle = search_and_build(
        &host,
        &index,
        &retrieval,
        "PermissionManager check",
        &ContextConfig::default(),
    );

    assert!(bundle.stats.suppressed_contained > 0, "应发生父子抑制");
    // 同一文件内不得出现互相包含的区间。
    for (outer_index, outer) in bundle.entries.iter().enumerate() {
        for (inner_index, inner) in bundle.entries.iter().enumerate() {
            if outer_index == inner_index || outer.file_path != inner.file_path {
                continue;
            }
            let contained =
                outer.start_line <= inner.start_line && outer.end_line >= inner.end_line;
            assert!(
                !contained,
                "{}:{}-{} 包含了 {}:{}-{}",
                outer.file_path,
                outer.start_line,
                outer.end_line,
                inner.file_path,
                inner.start_line,
                inner.end_line
            );
        }
    }
}

/// 返回的代码行不得大量重复。
#[test]
fn returned_lines_do_not_repeat() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let bundle = search_and_build(
        &host,
        &index,
        &retrieval,
        "permission manager check record",
        &ContextConfig::default(),
    );

    let mut seen: Vec<(String, u32)> = Vec::new();
    for entry in &bundle.entries {
        for line in entry.start_line..=entry.end_line {
            let key = (entry.file_path.clone(), line);
            assert!(
                !seen.contains(&key),
                "{}:{line} 被返回了两次",
                entry.file_path
            );
            seen.push(key);
        }
    }
}

/// 相邻区间必须被合并成一段。
#[test]
fn adjacent_ranges_are_merged() {
    let (_host, index, _) = fixture(FIXTURE);
    let chunk = index
        .chunks()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("PermissionManager::check"))
        .expect("固定装置应含 check");

    // 构造两条紧邻但互不包含的结果（中间无夹行）。
    let mut first = synthetic(chunk, 6, 9);
    first.score = 2.0;
    let mut second = synthetic(chunk, 10, 12);
    second.score = 1.0;

    let mut stats = ContextStats::default();
    let selected = select(vec![first, second], &ContextConfig::default(), &mut stats);

    assert_eq!(selected.len(), 1, "相邻区间应合并为一段");
    assert_eq!(selected[0].start_line, 6);
    assert_eq!(selected[0].end_line, 12);
    assert_eq!(stats.merged, 1);
}

/// 相距较远的区间不得被合并。
#[test]
fn distant_ranges_are_not_merged() {
    let (_, index, _) = fixture(FIXTURE);
    let chunk = index.chunks().next().expect("应有 chunk");

    let first = synthetic(chunk, 1, 3);
    let second = synthetic(chunk, 40, 45);

    let mut stats = ContextStats::default();
    let selected = select(vec![first, second], &ContextConfig::default(), &mut stats);

    assert_eq!(selected.len(), 2);
    assert_eq!(stats.merged, 0);
}

/// 父子包含时必须保留分数更高的那条。
#[test]
fn containment_keeps_the_higher_scoring_entry() {
    let (_, index, _) = fixture(FIXTURE);
    let chunk = index.chunks().next().expect("应有 chunk");

    // 输入按分数降序：父区间分数更高。
    let mut parent = synthetic(chunk, 4, 20);
    parent.score = 5.0;
    let mut child = synthetic(chunk, 6, 9);
    child.score = 1.0;

    let mut stats = ContextStats::default();
    let selected = select(vec![parent, child], &ContextConfig::default(), &mut stats);

    assert_eq!(selected.len(), 1);
    assert_eq!(
        (selected[0].start_line, selected[0].end_line),
        (4, 20),
        "分数更高的父区间应被保留"
    );
    assert_eq!(stats.suppressed_contained, 1);
}

// ---------------------------------------------------------------------------
// 多样性与预算
// ---------------------------------------------------------------------------

/// 单个文件的贡献必须受配额约束。
#[test]
fn diversity_caps_per_file_contribution() {
    let (host, index, _) = fixture(FIXTURE);

    // 收集同一文件的多个真实 chunk。
    let chunks: Vec<_> = index
        .chunks()
        .filter(|chunk| chunk.file_path == "src/permission/manager.rs")
        .take(6)
        .collect();

    // 为每个 chunk 构造一条结果，使用真实的行号范围。
    let results: Vec<SearchResult> = chunks
        .iter()
        .enumerate()
        .map(|(rank, chunk)| SearchResult {
            chunk_id: chunk.id.clone(),
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
            score: 10.0 - rank as f32,
            debug: FusionDebug {
                ranks: Vec::new(),
                rrf_score: 10.0 - rank as f32,
                symbol_boost: 0.0,
                path_boost: 0.0,
                rerank_boost: 0.0,
                rerank_matched_tokens: 0,
                rerank_query_tokens: 0,
                symbol_match: None,
            },
        })
        .collect();

    let config = ContextConfig {
        max_per_file: 2,
        ..ContextConfig::default()
    };
    let bundle = build(&host, &index, results, &config);

    assert!(
        bundle.entries.len() <= 2,
        "同文件最多贡献 2 条，实际 {}",
        bundle.entries.len()
    );
    assert!(
        bundle.stats.dropped_by_diversity >= 1,
        "应有条目因多样性约束被丢弃"
    );
}

/// 条目总数必须受 max_results 约束。
#[test]
fn max_results_is_respected() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let config = ContextConfig {
        max_results: 1,
        ..ContextConfig::default()
    };
    let bundle = search_and_build(&host, &index, &retrieval, "permission", &config);
    assert!(bundle.entries.len() <= 1);
}

/// 输出必须满足 token 预算。
#[test]
fn output_respects_token_budget() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let config = ContextConfig {
        token_budget: 20,
        ..ContextConfig::default()
    };
    let bundle = search_and_build(&host, &index, &retrieval, "permission session", &config);

    assert!(
        bundle.stats.estimated_tokens <= config.token_budget,
        "估算 {} 超过预算 {}",
        bundle.stats.estimated_tokens,
        config.token_budget
    );
    let actual: u32 = bundle
        .entries
        .iter()
        .map(|entry| estimate_tokens(&entry.snippet))
        .sum();
    assert_eq!(actual, bundle.stats.estimated_tokens, "统计必须与实际一致");
}

/// 预算为 0 时必须返回空而不是 panic。
#[test]
fn zero_budget_returns_nothing() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let config = ContextConfig {
        token_budget: 0,
        ..ContextConfig::default()
    };
    let bundle = search_and_build(&host, &index, &retrieval, "permission", &config);

    assert!(bundle.entries.is_empty());
    assert!(bundle.stats.dropped_by_budget > 0);
}

/// 预算不足时应跳过大条目继续尝试小条目，而不是直接中断。
#[test]
fn budget_skips_oversized_entries_and_continues() {
    let (host, index, _) = fixture(FIXTURE);
    let big = index
        .chunks()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("PermissionManager"))
        .expect("应有 impl 块");
    let small = index
        .chunks()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("SessionStore::persist"))
        .expect("应有 persist");

    // 大条目在前、小条目在后；预算只够放下小的。
    let mut first = synthetic(big, big.start_line, big.end_line);
    first.score = 9.0;
    let mut second = synthetic(small, small.start_line, small.end_line);
    second.file_path = small.file_path.clone();
    second.score = 1.0;

    let config = ContextConfig {
        token_budget: estimate_tokens("    /// 保存会话状态到磁盘。\n    pub fn persist(&self) {}")
            + 2,
        ..ContextConfig::default()
    };
    let bundle = build(&host, &index, vec![first, second], &config);

    assert_eq!(bundle.entries.len(), 1, "小条目应被放入");
    assert_eq!(bundle.entries[0].file_path, "src/session/store.rs");
    assert_eq!(bundle.stats.dropped_by_budget, 1);
}

/// 超长片段必须被截断并明确标注。
#[test]
fn oversized_snippet_is_truncated_and_marked() {
    let long_body: String = (0..300)
        .map(|index| format!("    let value{index} = {index};\n"))
        .collect();
    let source = format!("pub fn huge() {{\n{long_body}}}\n");
    let files = vec![("src/huge.rs", source.as_str())];
    let (host, index, retrieval) = fixture(&files);

    let config = ContextConfig {
        max_snippet_lines: 20,
        ..ContextConfig::default()
    };
    let bundle = search_and_build(&host, &index, &retrieval, "huge", &config);

    let entry = bundle.entries.first().expect("应有结果");
    assert!(entry.truncated, "超长片段必须标记截断");
    assert!(entry.snippet.contains("片段已截断"), "必须明确标注");
    assert!(entry.snippet.lines().count() <= 21, "行数应受上限约束");
}

// ---------------------------------------------------------------------------
// 惰性一致性校验
// ---------------------------------------------------------------------------

/// 未修改的文件必须原样返回，且不触发重新定位。
#[test]
fn unchanged_file_is_returned_verbatim() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let bundle = search_and_build(
        &host,
        &index,
        &retrieval,
        "PermissionManager::check",
        &ContextConfig::default(),
    );

    let entry = bundle.entries.first().expect("应有结果");
    assert!(!entry.relocated);
    assert_eq!(bundle.stats.relocated, 0);
    assert!(entry.snippet.contains("pub fn check"), "{}", entry.snippet);
}

/// 文件被外部修改后，符号位置必须被重新定位到正确的行。
///
/// 这是架构第 8 问的惰性一致性校验：索引是快照，仓库可能被编辑器或
/// `git checkout` 改动，而宿主不提供文件变更事件。
#[test]
fn externally_modified_file_is_relocated() {
    let (_, index, retrieval) = fixture(FIXTURE);
    let results = retrieval.search(&index, "PermissionManager::check", None, 10);
    assert!(!results.is_empty());

    // 在文件开头插入若干行，使全部符号下移。
    let shifted = format!(
        "// 新增的第一行\n// 新增的第二行\n// 新增的第三行\n{}",
        FIXTURE[0].1
    );
    let modified = RepoHost::new(&[("src/permission/manager.rs", shifted.as_str()), FIXTURE[1]]);

    let bundle = build(&modified, &index, results, &ContextConfig::default());
    let entry = bundle
        .entries
        .iter()
        .find(|entry| entry.qualified_symbol.as_deref() == Some("PermissionManager::check"))
        .expect("重新定位后仍应返回该符号");

    assert!(entry.relocated, "位置变化必须被识别");
    assert!(bundle.stats.relocated > 0);
    assert!(
        entry.snippet.contains("pub fn check"),
        "重新定位后必须指向正确的代码，实际：{}",
        entry.snippet
    );
    assert_eq!(entry.start_line, 8, "符号应下移 3 行（从第 5 行到第 8 行）");
}

/// 符号已被删除时必须丢弃该条目，而不是返回错误位置的代码。
#[test]
fn deleted_symbol_is_dropped() {
    let (_, index, retrieval) = fixture(FIXTURE);
    let results = retrieval.search(&index, "PermissionManager::check", None, 10);
    assert!(!results.is_empty());

    // 把 check 方法整个删掉。
    let without_check = r#"/// 判断一次工具调用是否需要用户审批。
pub struct PermissionManager;

impl PermissionManager {
    /// 记录一次审批决定。
    pub fn record(&self, decision: bool) -> bool {
        decision
    }
}
"#;
    let modified = RepoHost::new(&[("src/permission/manager.rs", without_check), FIXTURE[1]]);

    let bundle = build(&modified, &index, results, &ContextConfig::default());

    assert!(
        bundle
            .entries
            .iter()
            .all(|entry| entry.qualified_symbol.as_deref() != Some("PermissionManager::check")),
        "已删除的符号不得被返回"
    );
    assert!(bundle.stats.dropped_stale > 0);
}

/// 文件已删除时必须丢弃相关条目，且不让整次构建失败。
#[test]
fn deleted_file_is_dropped_without_failing() {
    let (_, index, retrieval) = fixture(FIXTURE);
    let results = retrieval.search(&index, "permission session", None, 10);
    assert!(!results.is_empty());

    // 只保留另一个文件。
    let modified = RepoHost::new(&[FIXTURE[1]]);
    let bundle = build(&modified, &index, results, &ContextConfig::default());

    assert!(
        bundle
            .entries
            .iter()
            .all(|entry| entry.file_path != "src/permission/manager.rs"),
        "已删除文件的条目不得被返回"
    );
    assert!(bundle.stats.dropped_stale > 0);
}

/// 同一文件的多个条目只应触发一次读取。
#[test]
fn each_file_is_read_once() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let before = host.file_calls();

    let bundle = search_and_build(
        &host,
        &index,
        &retrieval,
        "permission manager check record",
        &ContextConfig::default(),
    );

    let reads = host.file_calls() - before;
    assert_eq!(
        reads, bundle.stats.files_read,
        "统计的读取次数必须与实际一致"
    );
    assert!(
        bundle.stats.files_read as usize <= bundle.entries.len().max(1),
        "同一文件不得被重复读取"
    );
}

// ---------------------------------------------------------------------------
// token 估算与边界
// ---------------------------------------------------------------------------

/// token 估算必须区分 ASCII 与中日韩密度。
#[test]
fn token_estimation_distinguishes_script_density() {
    // 8 个 ASCII 字符约 2 个 token。
    assert_eq!(estimate_tokens("abcdefgh"), 2);
    // 4 个汉字约 4 个 token，远高于同字符数的 ASCII。
    assert_eq!(estimate_tokens("权限检查"), 4);
    assert!(estimate_tokens("权限检查") > estimate_tokens("abcd"));
}

/// token 估算必须向上取整，宁可高估也不低估。
#[test]
fn token_estimation_rounds_up() {
    assert_eq!(estimate_tokens("a"), 1, "1 字符也应计 1 token");
    assert_eq!(estimate_tokens("abcde"), 2, "5 字符应向上取整为 2");
    assert_eq!(estimate_tokens(""), 0);
}

/// 空输入必须安全返回空结果。
#[test]
fn empty_input_returns_empty_bundle() {
    let (host, index, _) = fixture(FIXTURE);
    let bundle = build(&host, &index, Vec::new(), &ContextConfig::default());

    assert!(bundle.entries.is_empty());
    assert_eq!(bundle.stats.candidates, 0);
    assert_eq!(bundle.stats.files_read, 0);
}

/// 结果必须保持按分数降序。
#[test]
fn entries_stay_sorted_by_score() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let bundle = search_and_build(
        &host,
        &index,
        &retrieval,
        "permission session store",
        &ContextConfig::default(),
    );

    for pair in bundle.entries.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "结果必须按分数降序：{} < {}",
            pair[0].score,
            pair[1].score
        );
    }
}

/// 同一输入必须产出完全相同的上下文。
#[test]
fn context_building_is_deterministic() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let config = ContextConfig::default();
    let first = search_and_build(&host, &index, &retrieval, "permission", &config);
    let second = search_and_build(&host, &index, &retrieval, "permission", &config);
    assert_eq!(first, second);
}

/// 条目必须带上定位信息与片段，供 Agent 直接使用。
#[test]
fn entries_carry_location_and_snippet() {
    let (host, index, retrieval) = fixture(FIXTURE);
    let bundle = search_and_build(
        &host,
        &index,
        &retrieval,
        "PermissionManager::check",
        &ContextConfig::default(),
    );

    let entry = bundle.entries.first().expect("应有结果");
    assert_eq!(entry.file_path, "src/permission/manager.rs");
    assert!(entry.start_line > 0);
    assert!(entry.end_line >= entry.start_line);
    assert_eq!(entry.language, "rust");
    assert!(!entry.snippet.is_empty());
    assert!(entry.estimated_tokens > 0);
}

/// 构造一条指向给定 chunk、但区间可覆盖的合成结果。
fn synthetic(chunk: &crate::model::CodeChunk, start: u32, end: u32) -> SearchResult {
    SearchResult {
        chunk_id: chunk.id.clone(),
        file_path: chunk.file_path.clone(),
        start_line: start,
        end_line: end,
        language: chunk.language.clone(),
        symbol_name: chunk.symbol_name.clone(),
        qualified_symbol: chunk.qualified_symbol.clone(),
        symbol_kind: chunk.symbol_kind,
        parent_symbol: chunk.parent_symbol.clone(),
        signature: chunk.signature.clone(),
        doc_comment: chunk.doc_comment.clone(),
        score: 1.0,
        debug: FusionDebug {
            ranks: Vec::new(),
            rrf_score: 1.0,
            symbol_boost: 0.0,
            path_boost: 0.0,
            rerank_boost: 0.0,
            rerank_matched_tokens: 0,
            rerank_query_tokens: 0,
            symbol_match: None,
        },
    }
}

/// 容器会被截断时，父子抑制必须把窗口收窄到被包含的子块。
///
/// 这是"容器排第 1、目标排第 2"的成因：`impl` 块或 class 动辄数百行，被保留后从起始行
/// 硬截断，用户真正要找的方法整段落在截断之外——结果里出现的是容器开头几十行，目标符号
/// 反而不见了。宁可少给容器头部，也不能把目标截掉。
#[test]
fn oversized_container_is_narrowed_to_the_contained_child() {
    let mut stats = ContextStats::default();
    let config = ContextConfig {
        max_snippet_lines: 20,
        ..ContextConfig::default()
    };
    let (_host, index, _retrieval) = fixture(FIXTURE);
    let container = index
        .chunks()
        .find(|chunk| chunk.symbol_kind == Some(crate::model::SymbolKind::Impl))
        .expect("固定装置必须含 impl 容器");
    let method = index
        .chunks()
        .find(|chunk| chunk.symbol_name.as_deref() == Some("check"))
        .expect("固定装置必须含 check 方法");

    // 容器 200 行远超 20 行上限，子块只有 5 行；容器分数更高，排在前面。
    let selected = select(
        vec![synthetic(container, 1, 200), synthetic(method, 40, 44)],
        &config,
        &mut stats,
    );

    assert_eq!(selected.len(), 1, "父子包含仍应只保留一条");
    assert_eq!(stats.suppressed_contained, 1);
    assert_eq!(
        (selected[0].start_line, selected[0].end_line),
        (40, 44),
        "窗口必须收窄到子块区间，否则目标符号会被截断掉"
    );
    assert_eq!(
        selected[0].symbol_name.as_deref(),
        Some("check"),
        "元数据必须一并换成子块，否则展示与重定位会指向容器符号"
    );
}

/// 容器本身放得下时不应被收窄——完整容器比孤立方法信息更全。
#[test]
fn container_within_snippet_limit_is_kept_whole() {
    let mut stats = ContextStats::default();
    let config = ContextConfig {
        max_snippet_lines: 200,
        ..ContextConfig::default()
    };
    let (_host, index, _retrieval) = fixture(FIXTURE);
    let container = index
        .chunks()
        .find(|chunk| chunk.symbol_kind == Some(crate::model::SymbolKind::Impl))
        .expect("固定装置必须含 impl 容器");
    let method = index
        .chunks()
        .find(|chunk| chunk.symbol_name.as_deref() == Some("check"))
        .expect("固定装置必须含 check 方法");

    let selected = select(
        vec![synthetic(container, 1, 100), synthetic(method, 40, 44)],
        &config,
        &mut stats,
    );

    assert_eq!(
        (selected[0].start_line, selected[0].end_line),
        (1, 100),
        "容器完整展示得下时必须保持原样"
    );
}
