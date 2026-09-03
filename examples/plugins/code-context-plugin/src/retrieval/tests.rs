//! 符号检索与词法检索的单元测试。
//!
//! Phase 5 的 Definition of Done 要求六类查询都有合理结果：精确符号、部分符号、标识符、
//! 文件路径词、源码关键字、文档注释关键字。下面按类逐一验证，并覆盖排序确定性、
//! 路径过滤与降级行为。

use super::*;
use crate::index::IndexBuild;
use crate::model::RepositoryIdentity;
use crate::repository::{ScanBudget, ScanProgress};
use crate::testing::RepoHost;

/// 不设限的预算，用于一次跑完。
const UNLIMITED: ScanBudget = ScanBudget {
    max_host_calls: u32::MAX,
    max_bytes_read: u64::MAX,
};

/// 一个模拟 Coding Agent 仓库的固定装置。
///
/// 内容刻意贴近真实项目：权限检查、工具执行、会话存储，且注释是中文——本仓库的注释就是
/// 中文，用英文装置测不出中日韩切词是否真的可用。
const FIXTURE: &[(&str, &str)] = &[
    (
        "src/permission/manager.rs",
        r#"
use crate::rules::PermissionRule;

/// 判断一次工具调用是否需要用户审批。
pub struct PermissionManager {
    rules: Vec<PermissionRule>,
}

impl PermissionManager {
    /// 检查工具调用的权限，必要时要求审批。
    pub fn check(&self, tool_call_id: &str) -> bool {
        self.rules.iter().any(|rule| rule.matches(tool_call_id))
    }
}
"#,
    ),
    (
        "src/permission/rules.rs",
        r#"
/// 一条权限规则。
pub struct PermissionRule {
    pub pattern: String,
}

pub const ALLOW_ALWAYS: &str = "allow_always";

impl PermissionRule {
    /// 判断规则是否匹配给定的工具。
    pub fn matches(&self, name: &str) -> bool {
        name.starts_with(&self.pattern)
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

/// 建立固定装置的检索索引。
fn fixture_index() -> (crate::index::StructuralIndex, RetrievalIndex) {
    let host = RepoHost::new(FIXTURE);
    let mut build = IndexBuild::with_default_limits(repository());
    assert_eq!(
        build.advance(&host, UNLIMITED),
        ScanProgress::Complete,
        "固定装置应一次构建完成"
    );
    build.into_parts()
}

/// 把候选还原成 `文件路径#限定名` 便于断言。
fn labels(index: &crate::index::StructuralIndex, candidates: &[SearchCandidate]) -> Vec<String> {
    candidates
        .iter()
        .filter_map(|candidate| index.chunk(&candidate.chunk_id))
        .map(|chunk| {
            format!(
                "{}#{}",
                chunk.file_path,
                chunk.qualified_symbol.as_deref().unwrap_or("-")
            )
        })
        .collect()
}

/// 断言候选中存在满足条件的标签。
fn assert_any(labels: &[String], needle: &str) {
    assert!(
        labels.iter().any(|label| label.contains(needle)),
        "候选 {labels:?} 应包含 {needle}"
    );
}

// ---------------------------------------------------------------------------
// 六类查询
// ---------------------------------------------------------------------------

/// 精确符号：限定名查询必须命中且排在首位。
#[test]
fn exact_qualified_symbol_ranks_first() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_symbols("PermissionManager::check", None, 10);

    assert!(!found.is_empty());
    let first = index.chunk(&found[0].chunk_id).expect("首位候选应有 chunk");
    assert_eq!(
        first.qualified_symbol.as_deref(),
        Some("PermissionManager::check")
    );
    assert_eq!(found[0].symbol_match, Some(SymbolMatch::ExactQualified));
}

/// 精确符号：短名查询必须命中对应符号。
#[test]
fn exact_symbol_name_is_found() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_symbols("PermissionRule", None, 10);
    assert_any(&labels(&index, &found), "PermissionRule");
}

/// 部分符号：前缀查询必须命中。
#[test]
fn partial_symbol_prefix_is_found() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_symbols("Permission", None, 20);
    let labels = labels(&index, &found);

    assert_any(&labels, "PermissionManager");
    assert_any(&labels, "PermissionRule");
}

/// 部分符号：方法短名必须能命中限定名的后缀。
#[test]
fn method_name_matches_qualified_suffix() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_symbols("check", None, 10);
    assert_any(&labels(&index, &found), "PermissionManager::check");
}

/// 标识符：全大写常量必须可被检索。
#[test]
fn screaming_snake_case_identifier_is_found() {
    let (index, retrieval) = fixture_index();

    let symbols = retrieval.search_symbols("ALLOW_ALWAYS", None, 10);
    assert_any(&labels(&index, &symbols), "ALLOW_ALWAYS");

    let lexical = retrieval.search_lexical("ALLOW_ALWAYS", None, 10);
    assert_any(&labels(&index, &lexical), "rules.rs");
}

/// 标识符：蛇形参数名只出现在函数体里，必须靠词法检索命中。
#[test]
fn snake_case_identifier_in_body_is_found() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_lexical("tool_call_id", None, 10);
    assert_any(&labels(&index, &found), "PermissionManager::check");
}

/// 源码关键字：只出现在函数体内的自由函数必须可被检索到。
///
/// 这条验证的是"索引正文但不驻留正文"确实成立——`execute_command` 在
/// `ToolExecutor::execute` 的函数体里被调用，如果倒排索引不覆盖正文就找不到。
#[test]
fn source_keyword_inside_body_is_found() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_lexical("execute_command", None, 10);
    let labels = labels(&index, &found);

    assert_any(&labels, "executor.rs");
    assert!(
        labels.iter().any(|label| label.contains("execute")),
        "候选 {labels:?} 应包含调用处或定义处"
    );
}

/// 文件路径词：路径中的词必须可被检索。
#[test]
fn file_path_term_is_searchable() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_lexical("session store", None, 10);
    assert_any(&labels(&index, &found), "src/session/store.rs");
}

/// 文档注释关键字：中文注释必须可被检索。
#[test]
fn chinese_doc_comment_keyword_is_found() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_lexical("审批", None, 10);
    assert_any(&labels(&index, &found), "manager.rs");
}

/// 文档注释关键字：中文短语必须命中对应符号。
#[test]
fn chinese_doc_phrase_is_found() {
    let (index, retrieval) = fixture_index();
    let found = retrieval.search_lexical("会话状态", None, 10);
    assert_any(&labels(&index, &found), "SessionStore");
}

/// 自然语言查询必须能同时驱动两路检索。
#[test]
fn natural_language_query_reaches_both_channels() {
    let (index, retrieval) = fixture_index();
    let channels = retrieval.search_all("where is permission checked for a tool", None, 10);

    assert_eq!(channels.len(), 2, "两路通道必须都返回列表");
    let symbol_labels = labels(&index, &channels[0]);
    let lexical_labels = labels(&index, &channels[1]);
    assert!(
        !symbol_labels.is_empty() || !lexical_labels.is_empty(),
        "自然语言查询至少应有一路命中"
    );
    assert_any(&lexical_labels, "permission");
}

// ---------------------------------------------------------------------------
// 候选契约
// ---------------------------------------------------------------------------

/// 候选必须带上通道内排名，从 1 开始且连续。
#[test]
fn candidates_carry_dense_ranks() {
    let (_, retrieval) = fixture_index();
    let found = retrieval.search_lexical("permission", None, 5);

    assert!(!found.is_empty());
    for (offset, candidate) in found.iter().enumerate() {
        assert_eq!(candidate.rank, offset as u32 + 1, "排名必须从 1 开始且连续");
        assert_eq!(candidate.source, CandidateSource::Lexical);
    }
}

/// 符号候选必须标注匹配方式，词法候选必须不标注。
#[test]
fn candidate_source_and_match_are_consistent() {
    let (_, retrieval) = fixture_index();

    for candidate in retrieval.search_symbols("check", None, 5) {
        assert_eq!(candidate.source, CandidateSource::Symbol);
        assert!(candidate.symbol_match.is_some());
    }
    for candidate in retrieval.search_lexical("check", None, 5) {
        assert_eq!(candidate.source, CandidateSource::Lexical);
        assert!(candidate.symbol_match.is_none());
    }
}

/// 精确匹配的分数必须严格高于前缀匹配。
#[test]
fn exact_matches_outrank_prefix_matches() {
    assert!(SymbolMatch::ExactQualified.score() > SymbolMatch::ExactName.score());
    assert!(SymbolMatch::ExactName.score() > SymbolMatch::QualifiedSuffix.score());
    assert!(SymbolMatch::QualifiedSuffix.score() > SymbolMatch::NamePrefix.score());
}

/// 同一查询必须产出完全确定的结果顺序。
#[test]
fn results_are_deterministic() {
    let (_, retrieval) = fixture_index();
    let first = retrieval.search_lexical("permission rule matches", None, 10);
    let second = retrieval.search_lexical("permission rule matches", None, 10);
    assert_eq!(first, second);

    let symbols_first = retrieval.search_symbols("Permission", None, 10);
    let symbols_second = retrieval.search_symbols("Permission", None, 10);
    assert_eq!(symbols_first, symbols_second);
}

/// 结果数量必须受 limit 约束。
#[test]
fn limit_is_respected() {
    let (_, retrieval) = fixture_index();
    assert!(retrieval.search_lexical("permission", None, 2).len() <= 2);
    assert!(retrieval.search_symbols("Permission", None, 1).len() <= 1);
    assert!(retrieval.search_lexical("permission", None, 0).is_empty());
}

/// 路径过滤必须同时作用于两路通道。
#[test]
fn path_scope_filters_both_channels() {
    let (index, retrieval) = fixture_index();

    let symbols = retrieval.search_symbols("Permission", Some("src/permission/"), 20);
    let lexical = retrieval.search_lexical("permission", Some("src/permission/"), 20);

    for label in labels(&index, &symbols)
        .iter()
        .chain(labels(&index, &lexical).iter())
    {
        assert!(label.starts_with("src/permission/"), "越界结果 {label}");
    }
    // 过滤条件之外的文件必须被排除。
    let all = retrieval.search_lexical("permission", None, 20);
    assert!(
        labels(&index, &all).len() >= labels(&index, &lexical).len(),
        "不加过滤的结果不应少于加过滤的结果"
    );
}

/// 路径过滤命中不到任何文件时必须返回空而不是忽略过滤条件。
#[test]
fn path_scope_can_exclude_everything() {
    let (_, retrieval) = fixture_index();
    assert!(retrieval
        .search_lexical("permission", Some("does/not/exist/"), 10)
        .is_empty());
    assert!(retrieval
        .search_symbols("Permission", Some("does/not/exist/"), 10)
        .is_empty());
}

/// 检索不到内容时必须返回空列表而不是 panic。
#[test]
fn unmatched_query_returns_empty() {
    let (_, retrieval) = fixture_index();
    assert!(retrieval
        .search_lexical("zzzzznonexistenttoken", None, 10)
        .is_empty());
    assert!(retrieval
        .search_symbols("zzzzznonexistenttoken", None, 10)
        .is_empty());
}

/// 空查询与纯符号查询不得 panic。
#[test]
fn degenerate_queries_are_safe() {
    let (_, retrieval) = fixture_index();
    for query in ["", "   ", "!!!", "??"] {
        assert!(retrieval.search_lexical(query, None, 10).is_empty());
        assert!(retrieval.search_symbols(query, None, 10).is_empty());
    }
}

/// 空索引上检索必须安全返回空。
#[test]
fn empty_index_returns_empty() {
    let mut retrieval = RetrievalIndex::new();
    retrieval.finalize();

    assert!(retrieval.is_empty());
    assert!(retrieval.search_lexical("anything", None, 10).is_empty());
    assert!(retrieval.search_symbols("anything", None, 10).is_empty());
}

/// 未 finalize 的符号索引必须返回空，而不是给出错误排序的结果。
#[test]
fn symbol_search_requires_finalize() {
    let host = RepoHost::new(FIXTURE);
    let mut build = IndexBuild::with_default_limits(repository());
    // 只推进一步，构建未完成因此尚未 finalize。
    build.advance(
        &host,
        ScanBudget {
            max_host_calls: 2,
            max_bytes_read: u64::MAX,
        },
    );

    assert!(
        build
            .retrieval()
            .search_symbols("Permission", None, 10)
            .is_empty(),
        "未完成构建时符号检索必须返回空而不是错序结果"
    );
}

/// 索引规模必须如实反映内容。
#[test]
fn stats_reflect_index_contents() {
    let (_, retrieval) = fixture_index();
    let stats = retrieval.stats();

    assert!(stats.symbols > 0, "固定装置含多个符号");
    assert!(stats.documents > 0);
    assert!(stats.terms > 0);
    assert!(!stats.truncated);
}

/// 文本兜底的 chunk 不进符号表，但必须进词法索引。
#[test]
fn text_tier_chunks_are_lexical_only() {
    let host = RepoHost::new(&[("docs/guide.md", "# 权限说明\n\n描述审批流程。\n")]);
    let mut build = IndexBuild::with_default_limits(repository());
    build.advance(&host, UNLIMITED);
    let (index, retrieval) = build.into_parts();

    assert_eq!(retrieval.stats().symbols, 0, "文本块没有符号");
    assert!(retrieval.stats().documents > 0);
    let found = retrieval.search_lexical("审批流程", None, 10);
    assert_any(&labels(&index, &found), "docs/guide.md");
}
