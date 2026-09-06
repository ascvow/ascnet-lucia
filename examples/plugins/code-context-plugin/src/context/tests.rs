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
use agent_plugin::{AgentPlugin, ToolCall, ToolResult};
use serde_json::{json, Value};

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

/// 建立会产生 SFC 语义窗口的真实解析装置，用固定模型分隔离文件配额的执行时机。
///
/// 前三个窗口覆盖长函数的相邻区间，提交函数位于其后；其他文件提供足够候选，
/// 使低分的整函数与跨方法窗口不会进入上下文，避免测试依赖召回时的偶然排序。
fn quota_tool_fixture() -> (RepoHost, crate::CodeContextPlugin) {
    let mut source = String::from(
        "<script setup>\nconst adjustmentRows = [];\n\n/** 收集待保存的调整内容。 */\nfunction collectChanges() {\n",
    );
    for _ in 0..185 {
        source.push_str("  adjustmentRows.push(1);\n");
    }
    source.push_str(
        "}\n\n/** 保存调整内容并返回实际提交结果。 */\nasync function commitChanges() {\n  const changed = adjustmentRows.filter(value => value > 0);\n  const response = await saveAdjustments(changed);\n  adjustmentRows.length = 0;\n  return response;\n}\n\n",
    );
    for index in 0..6 {
        source.push_str(&format!(
            "/** 保存调整内容的独立处理步骤。 */\nfunction prepareStep{index}() {{\n  const changed = adjustmentRows.filter(value => value > {index});\n  const count = changed.length;\n  adjustmentRows.push(count);\n  return count;\n}}\n\n"
        ));
    }
    source.push_str("</script>\n");
    let mut files = vec![("src/Adjustments.vue".to_string(), source)];
    for index in 0..100 {
        files.push((
            format!("src/step_{index}.rs"),
            format!(
                "/// 保存调整内容的独立处理。\npub fn save_step_{index}(value: u32) -> u32 {{\n    let adjusted = value + 1;\n    adjusted\n}}\n"
            ),
        ));
    }
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&borrowed).with_http_handler(|request| {
        let body: Value = serde_json::from_str(request.body.as_deref().expect("模型请求正文"))
            .expect("模型请求必须是 JSON");
        let response = match request.path.as_str() {
            "embeddings" => {
                let count = body["input"].as_array().map_or(1, Vec::len);
                json!({"data": (0..count).map(|index| json!({
                    "index": index, "embedding": [1.0]
                })).collect::<Vec<_>>()})
            }
            "rerank" => {
                let documents = body["documents"].as_array().expect("重排必须携带源码");
                assert!(documents.len() >= 100, "必须对真实候选池执行模型重排");
                let results: Vec<_> = documents
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let document = value.as_str().expect("重排文档必须是文本");
                        // 固定假模型只控制候选名次；分块、召回、HTTP 和正文输出均走生产代码。
                        let relevance = if document.contains("\n符号：commitChanges\n") {
                            0.8
                        } else if document.contains("\n符号：prepareStep") {
                            0.7
                        } else if document.starts_with("文件：src/Adjustments.vue\n") {
                            if !document.contains("\n符号：")
                                && document.contains("adjustmentRows.push(1)")
                                && !document.contains("function commitChanges")
                            {
                                0.99
                            } else {
                                0.001
                            }
                        } else {
                            0.1
                        };
                        json!({"index": index, "relevance_score": relevance})
                    })
                    .collect();
                json!({"results": results})
            }
            path => panic!("测试禁止访问其他模型端点：{path}"),
        };
        crate::testing::http_json(200, &response.to_string())
    });
    let mut context = crate::testing::activation_context("/repo/quota-regression");
    for (key, value) in [
        ("embedding_dimension", "1"),
        ("embedding_request_interval_ms", "0"),
        ("embedding_max_retries", "0"),
        ("embedding_retry_base_ms", "0"),
        ("http_endpoint_id", "test-endpoint"),
        ("rerank_model", "test-reranker"),
    ] {
        context.metadata.insert(key.into(), value.into());
    }
    let mut plugin = crate::CodeContextPlugin::default();
    plugin.activate(&host, context).expect("激活配额回归装置");
    for _ in 0..20 {
        plugin.advance_index(&host);
        if plugin.index_status() != crate::model::IndexStatus::Indexing {
            break;
        }
    }
    assert_eq!(plugin.index_status(), crate::model::IndexStatus::Ready);
    (host, plugin)
}

/// 通过公开工具路由发起固定自然语言查询；参数变化只影响输出约束。
fn quota_tool_call(
    plugin: &mut crate::CodeContextPlugin,
    host: &RepoHost,
    max_results: usize,
    token_budget: u32,
) -> ToolResult {
    let result = plugin
        .call_tool_with_host(
            host,
            ToolCall::new(
                "quota-regression",
                crate::tool::TOOL_NAME,
                json!({
                    "query": "怎样保存调整内容",
                    "max_results": max_results,
                    "token_budget": token_budget,
                }),
            ),
        )
        .expect("真实 code_search 路由不得失败");
    assert!(!result.is_error);
    assert!(
        result.content.get("note").is_none(),
        "不得依靠降级结果通过测试"
    );
    result
}

/// SFC 窗口在合并前不得耗尽文件配额；完整提交函数必须真正出现在工具返回正文中。
#[test]
fn code_search_merges_windows_before_enforcing_file_quota() {
    let (host, mut plugin) = quota_tool_fixture();
    let direct = plugin.search_execution(&host, "怎样保存调整内容", None, 40);
    assert!(direct.diagnostics.rerank_applied, "装置必须经过模型重排");
    let mut same_file: Vec<_> = direct
        .results
        .iter()
        .filter(|result| result.file_path == "src/Adjustments.vue")
        .collect();
    assert_eq!(same_file.len(), 3, "公开原始列表仍保留既有每文件三项配额");
    assert!(
        same_file.iter().all(|result| result.symbol_name.is_none()),
        "前三个高分窗口必须先于完整方法，才能复现提前配额丢失正文的问题"
    );
    same_file.sort_by_key(|result| result.start_line);
    assert!(same_file.windows(2).all(|pair| {
        pair[0].end_line.saturating_add(1) >= pair[1].start_line
            && pair[1].end_line.saturating_add(1) >= pair[0].start_line
    }));

    let result = quota_tool_call(&mut plugin, &host, 10, 8_000);
    let entries = result.content["results"].as_array().expect("工具结果数组");
    let target = entries
        .iter()
        .find(|entry| entry["symbol"] == "commitChanges")
        .expect("窗口合并后必须保留完整提交函数");
    let snippet = target["snippet"].as_str().expect("真实源码片段");
    assert!(snippet.contains("async function commitChanges()"));
    assert!(snippet.contains("const changed = adjustmentRows.filter"));
    assert!(snippet.contains("await saveAdjustments(changed)"));
    assert!(snippet.contains("adjustmentRows.length = 0"));
    assert!(snippet.contains("return response;"));
    assert!(snippet.trim_end().ends_with('}'));
    assert_eq!(target["lines"], target["hydrated_lines"]);
    assert_ne!(target["truncated"], true);
    assert_eq!(entries.len(), 10, "重叠抑制后应由其他文件补满请求数量");
    let same_file_count = entries
        .iter()
        .filter(|entry| entry["file"] == "src/Adjustments.vue")
        .count();
    assert_eq!(same_file_count, 4, "上下文合并后仍执行最终每文件四项配额");
    let stats = &result.details.expect("工具必须携带构建统计")["stats"];
    assert!(stats["merged"].as_u64().unwrap() >= 2);
    assert!(stats["dropped_by_diversity"].as_u64().unwrap() > 0);
    assert!(stats["estimated_tokens"].as_u64().unwrap() <= 8_000);
    assert_eq!(
        host.http_requests()
            .iter()
            .filter(|request| request.path == "rerank")
            .count(),
        1,
        "同一候选池应复用响应；延后文件配额不得增加模型调用"
    );
}

/// 延后文件配额后，真实工具仍必须分别服从结果数量和小 token 预算。
#[test]
fn code_search_deferred_file_quota_preserves_output_limits() {
    let (host, mut plugin) = quota_tool_fixture();
    let limited = quota_tool_call(&mut plugin, &host, 2, 8_000);
    assert_eq!(limited.content["results"].as_array().unwrap().len(), 2);

    let budgeted = quota_tool_call(&mut plugin, &host, 10, 500);
    let entries = budgeted.content["results"].as_array().unwrap();
    let stats = &budgeted.details.expect("工具必须携带预算统计")["stats"];
    assert!(!entries.is_empty(), "大窗口预算不足时仍可返回后续短方法");
    assert!(entries.len() <= 10);
    assert!(stats["dropped_by_budget"].as_u64().unwrap() > 0);
    assert!(stats["estimated_tokens"].as_u64().unwrap() <= 500);
    let mut per_file = std::collections::BTreeMap::new();
    let mut snippet_tokens = 0;
    for entry in entries {
        *per_file.entry(entry["file"].as_str().unwrap()).or_insert(0) += 1;
        snippet_tokens += estimate_tokens(entry["snippet"].as_str().unwrap());
    }
    assert!(per_file.values().all(|count| *count <= 4));
    assert!(snippet_tokens <= 500, "按实际返回源码复算也不得超过预算");
}

/// 前四十条原始候选都来自同文件时，工具仍应让后续其他文件补满最终结果。
///
/// 大文件经真实文本解析产生四十余个窗口；固定模型仅控制排序，避免将最终结果数量
/// 乘四当成上下文合并前的硬截断。若恢复旧的前四十条截断，本测试最多返回四条。
#[test]
fn code_search_keeps_other_files_after_forty_same_file_windows() {
    let mut source = String::from("public class AdjustmentSteps {\n");
    for index in 0..540 {
        source.push_str(&format!(
            "    /** 保存调整内容。 */\n    public int step{index}(int value) {{\n        int adjusted = value + {index};\n        return adjusted;\n    }}\n"
        ));
    }
    source.push_str("}\n");
    let mut files = vec![("src/AdjustmentSteps.java".to_string(), source)];
    for index in 0..60 {
        files.push((
            format!("src/followup_{index}.rs"),
            format!(
                "/// 保存调整内容并返回处理结果。\npub fn save_followup_{index}(value: u32) -> u32 {{\n    let changed = value + {index};\n    changed\n}}\n"
            ),
        ));
    }
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&borrowed).with_http_handler(|request| {
        let body: Value = serde_json::from_str(request.body.as_deref().expect("模型请求正文"))
            .expect("模型请求必须是 JSON");
        let response = match request.path.as_str() {
            "embeddings" => {
                let count = body["input"].as_array().map_or(1, Vec::len);
                json!({"data": (0..count).map(|index| json!({
                    "index": index, "embedding": [1.0]
                })).collect::<Vec<_>>()})
            }
            "rerank" => {
                let documents = body["documents"].as_array().expect("实际重排候选文档");
                let same_file_count = documents
                    .iter()
                    .filter(|document| document.as_str().unwrap().starts_with("文件：src/AdjustmentSteps.java\n"))
                    .count();
                assert!(same_file_count > 40, "装置必须有超过四十个同文件窗口进入模型池");
                assert!(documents.len() - same_file_count >= 6, "同一模型池必须还含其他文件的有效候选");
                assert!(documents.len() <= 150, "不得扩大既有模型候选池以通过测试");
                json!({"results": documents.iter().enumerate().map(|(index, document)| {
                    let relevance = if document.as_str().unwrap().starts_with("文件：src/AdjustmentSteps.java\n") {
                        0.99
                    } else {
                        0.1
                    };
                    json!({"index": index, "relevance_score": relevance})
                }).collect::<Vec<_>>()})
            }
            path => panic!("测试禁止访问其他端点：{path}"),
        };
        crate::testing::http_json(200, &response.to_string())
    });
    let mut context = crate::testing::activation_context("/repo/context-candidate-budget");
    for (key, value) in [
        ("embedding_dimension", "1"),
        ("embedding_request_interval_ms", "0"),
        ("embedding_max_retries", "0"),
        ("embedding_retry_base_ms", "0"),
        ("http_endpoint_id", "test-endpoint"),
        ("rerank_model", "test-reranker"),
    ] {
        context.metadata.insert(key.into(), value.into());
    }
    let mut plugin = crate::CodeContextPlugin::default();
    plugin.activate(&host, context).expect("激活候选补位装置");
    for _ in 0..20 {
        plugin.advance_index(&host);
        if plugin.index_status() != crate::model::IndexStatus::Indexing {
            break;
        }
    }
    assert_eq!(plugin.index_status(), crate::model::IndexStatus::Ready);

    let result = quota_tool_call(&mut plugin, &host, 10, 8_000);
    let entries = result.content["results"]
        .as_array()
        .expect("真实工具返回数组");
    assert_eq!(
        entries.len(),
        10,
        "前四十条同文件窗口合并后，必须允许后续其他文件补位"
    );
    let mut per_file = std::collections::BTreeMap::new();
    for entry in entries {
        let path = entry["file"].as_str().expect("实际文件路径");
        *per_file.entry(path).or_insert(0) += 1;
        if path != "src/AdjustmentSteps.java" {
            let snippet = entry["snippet"].as_str().expect("其他文件实际源码");
            assert!(snippet.contains("pub fn save_followup_"));
            assert!(snippet.contains("let changed = value +"));
            assert!(snippet.trim_end().ends_with('}'));
        }
    }
    assert!(per_file.values().all(|count| *count <= 4));
    assert!(
        per_file
            .keys()
            .filter(|path| **path != "src/AdjustmentSteps.java")
            .count()
            >= 6
    );
    let stats = &result.details.expect("上下文统计")["stats"];
    assert!(stats["candidates"].as_u64().unwrap() > 40);
    assert!(stats["dropped_by_diversity"].as_u64().unwrap() > 0);
    assert!(stats["estimated_tokens"].as_u64().unwrap() <= 8_000);
    assert_eq!(
        host.http_requests()
            .iter()
            .filter(|request| request.path == "rerank")
            .count(),
        1
    );
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
    let selected = select(
        &index,
        vec![first, second],
        &ContextConfig::default(),
        &mut stats,
    );

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
    let selected = select(
        &index,
        vec![first, second],
        &ContextConfig::default(),
        &mut stats,
    );

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
    let selected = select(
        &index,
        vec![parent, child],
        &ContextConfig::default(),
        &mut stats,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(
        (selected[0].start_line, selected[0].end_line),
        (4, 20),
        "分数更高的父区间应被保留"
    );
    assert_eq!(stats.suppressed_contained, 1);
}

/// 生成逐行可辨的普通文本，隔离模型排序和语言解析对区间合并测试的影响。
fn numbered_source(lines: u32) -> String {
    (1..=lines)
        .map(|line| format!("处理步骤 {line}\n"))
        .collect()
}

/// 构造不带完整符号承诺的窗口，用显式分数稳定验证高低分候选的覆盖顺序。
fn text_window(chunk: &crate::model::CodeChunk, start: u32, end: u32, score: f32) -> SearchResult {
    let mut result = synthetic(chunk, start, end);
    result.symbol_name = None;
    result.qualified_symbol = None;
    result.symbol_kind = None;
    result.signature = None;
    result.doc_comment = None;
    result.score = score;
    result
}

/// 汇总实际可见行并拒绝重复行，检查合并与裁剪没有把重复内容转移到不同条目。
fn visible_lines(bundle: &ContextBundle) -> std::collections::BTreeSet<u32> {
    let mut lines = std::collections::BTreeSet::new();
    for entry in &bundle.entries {
        for line in entry.start_line..=entry.end_line {
            assert!(lines.insert(line), "同文件第 {line} 行重复返回");
        }
    }
    lines
}

/// 连续加入低分前缀时，高分尾段不能被推到行数上限之外；其他窗口仍可独立保留。
#[test]
fn lower_ranked_prefixes_cannot_hide_a_complete_tail_hit() {
    let source = numbered_source(300);
    let (host, index, _) = fixture(&[("src/steps.txt", source.as_str())]);
    let chunk = index.chunks().next().expect("文本窗口");
    let ranges = [
        (280, 300),
        (230, 279),
        (180, 229),
        (130, 179),
        (80, 129),
        (1, 79),
    ];
    let results = ranges
        .into_iter()
        .enumerate()
        .map(|(rank, (start, end))| text_window(chunk, start, end, 10.0 - rank as f32))
        .collect();
    let config = ContextConfig {
        max_snippet_lines: 100,
        max_per_file: 10,
        ..ContextConfig::default()
    };

    let bundle = build(&host, &index, results, &config);
    let first = &bundle.entries[0];
    assert!(first.start_line <= 280 && first.end_line >= 300);
    assert!(
        !first.truncated,
        "此前完整可见的高分目标不能因后续合并被截断"
    );
    assert_eq!(visible_lines(&bundle), (1..=300).collect());
    assert!(bundle
        .entries
        .iter()
        .all(|entry| entry.end_line - entry.start_line < 100));
}

/// 超长桥接保留两侧高分窗口，全部独有行按行限拆成互不重叠的剩余片段。
#[test]
fn oversized_bridge_preserves_all_uncovered_segments_within_line_limit() {
    let source = numbered_source(48);
    let (host, index, _) = fixture(&[("src/steps.txt", source.as_str())]);
    let chunk = index.chunks().next().expect("文本窗口");
    let results = vec![
        text_window(chunk, 5, 6, 3.0),
        text_window(chunk, 40, 41, 2.0),
        text_window(chunk, 1, 48, 1.0),
    ];
    let config = ContextConfig {
        max_snippet_lines: 10,
        max_per_file: 10,
        ..ContextConfig::default()
    };

    let bundle = build(&host, &index, results, &config);
    let ranges: Vec<_> = bundle
        .entries
        .iter()
        .map(|entry| (entry.start_line, entry.end_line))
        .collect();
    assert_eq!(
        ranges,
        [
            (5, 6),
            (40, 41),
            (1, 4),
            (7, 16),
            (17, 26),
            (27, 36),
            (37, 39),
            (42, 48)
        ]
    );
    assert_eq!(visible_lines(&bundle), (1..=48).collect());
    assert!(bundle.entries[..2].iter().all(|entry| !entry.truncated));
    assert!(bundle.entries[2..].iter().all(|entry| entry.truncated));
    assert!(bundle
        .entries
        .windows(2)
        .all(|pair| pair[0].score >= pair[1].score));
    assert_eq!(bundle.stats.merged, 0, "拒绝超长并集不应报告发生了合并");
}

/// 并集恰好等于行限时仍应合并，避免把正常跨窗口正文无故切碎。
#[test]
fn bridge_at_exact_line_limit_keeps_one_complete_entry() {
    let source = numbered_source(15);
    let (host, index, _) = fixture(&[("src/steps.txt", source.as_str())]);
    let chunk = index.chunks().next().expect("文本窗口");
    let results = vec![
        text_window(chunk, 1, 4, 3.0),
        text_window(chunk, 12, 15, 2.0),
        text_window(chunk, 4, 12, 1.0),
    ];
    let config = ContextConfig {
        max_snippet_lines: 15,
        ..ContextConfig::default()
    };
    let bundle = build(&host, &index, results, &config);

    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(visible_lines(&bundle), (1..=15).collect());
    assert!(!bundle.entries[0].truncated);
    assert_eq!(bundle.stats.merged, 2);
}

/// 函数被去重裁剪后保留完整目标范围，同时去掉完整符号元数据并明确说明截断。
#[test]
fn clipped_function_fragments_report_original_target_without_full_symbol_metadata() {
    let body: String = (1..=30)
        .map(|line| format!("    let step_{line} = {line};\n"))
        .collect();
    let source = format!("pub fn evaluate() {{\n{body}}}\n");
    let (host, index, _) = fixture(&[("src/evaluate.rs", source.as_str())]);
    let chunk = index
        .chunks()
        .find(|chunk| chunk.symbol_name.as_deref() == Some("evaluate"))
        .expect("完整函数");
    let results = vec![
        text_window(chunk, 8, 10, 3.0),
        text_window(chunk, 22, 24, 2.0),
        synthetic(chunk, 1, 32),
    ];
    let config = ContextConfig {
        max_snippet_lines: 12,
        max_per_file: 10,
        ..ContextConfig::default()
    };
    let bundle = build(&host, &index, results.clone(), &config);
    assert_eq!(visible_lines(&bundle), (1..=32).collect());
    assert_eq!(bundle.entries.len(), 5);
    for entry in &bundle.entries[2..] {
        assert_eq!(
            (entry.retrieval_start_line, entry.retrieval_end_line),
            (1, 32)
        );
        assert_eq!(
            (entry.hydrated_start_line, entry.hydrated_end_line),
            (1, 32)
        );
        assert!(entry.truncated && entry.snippet.contains("片段已截断"));
        assert!(entry.symbol_name.is_none() && entry.qualified_symbol.is_none());
        assert!(entry.symbol_kind.is_none() && entry.signature.is_none());
        assert!(entry.doc_comment.is_none());
        assert_eq!(
            entry.snippet.lines().count() as u32,
            entry.end_line - entry.start_line + 2,
            "只在实际片段末尾追加一行截断说明"
        );
    }

    // 分片后的结果仍由现有构建循环执行结果数、文件名额和 token 预算限制。
    let limited = build(
        &host,
        &index,
        results,
        &ContextConfig {
            max_results: 4,
            max_per_file: 3,
            ..config
        },
    );
    assert_eq!(limited.entries.len(), 3);
    assert_eq!(limited.stats.dropped_by_diversity, 2);
    assert!(limited.stats.estimated_tokens <= config.token_budget);
}

/// 裁剪片依赖旧快照中的覆盖关系；文件变更后不得按原 chunk 重新扩成完整函数。
#[test]
fn clipped_fragments_are_dropped_when_source_changes() {
    let body: String = (1..=30)
        .map(|line| format!("    let step_{line} = {line};\n"))
        .collect();
    let source = format!("pub fn evaluate() {{\n{body}}}\n");
    let (_, index, _) = fixture(&[("src/evaluate.rs", source.as_str())]);
    let chunk = index
        .chunks()
        .find(|chunk| chunk.symbol_name.as_deref() == Some("evaluate"))
        .expect("完整函数");
    let config = ContextConfig {
        max_snippet_lines: 12,
        ..ContextConfig::default()
    };
    let selected = select(
        &index,
        vec![text_window(chunk, 8, 10, 2.0), synthetic(chunk, 1, 32)],
        &config,
        &mut ContextStats::default(),
    );
    let changed_source = format!("// 函数位置发生变化\n{source}");
    let changed = RepoHost::new(&[("src/evaluate.rs", changed_source.as_str())]);
    let mut cache = SourceCache::default();
    let mut stats = ContextStats::default();
    let fragments: Vec<_> = selected
        .iter()
        .filter(|item| item.partial_target.is_some())
        .collect();
    assert!(!fragments.is_empty());
    for fragment in fragments {
        assert!(resolve(&changed, &index, fragment, &config, &mut cache, &mut stats).is_none());
    }
    let full = select(
        &index,
        vec![synthetic(chunk, 1, 32)],
        &config,
        &mut ContextStats::default(),
    );
    let relocated = resolve(&changed, &index, &full[0], &config, &mut cache, &mut stats)
        .expect("没有裁剪的完整符号仍可正常重定位");
    assert!(relocated.relocated);
    assert_eq!(
        (relocated.hydrated_start_line, relocated.hydrated_end_line),
        (2, 33)
    );
}

// ---------------------------------------------------------------------------
// 多样性与预算
// ---------------------------------------------------------------------------

/// 方法定位只收窄近似同分父块，默认模式和低相关子项仍保留父块。
#[test]
fn near_tied_methods_are_narrowed_only_for_location_queries() {
    let (_, index, _) = fixture(FIXTURE);
    let chunk = index.chunks().next().expect("代码块");
    for (enabled, child_score, expected) in [
        (true, 0.9998, (20, 29)),
        (false, 0.9998, (1, 100)),
        (true, 0.5, (1, 100)),
    ] {
        let mut parent = synthetic(chunk, 1, 100);
        parent.symbol_kind = Some(crate::model::SymbolKind::Constant);
        parent.debug.model_rerank_score = Some(0.9999);
        let mut child = synthetic(chunk, 20, 29);
        child.symbol_kind = Some(crate::model::SymbolKind::Function);
        child.debug.model_rerank_score = Some(child_score);
        let selected = select(
            &index,
            vec![parent, child],
            &ContextConfig {
                prefer_method_locations: enabled,
                ..ContextConfig::default()
            },
            &mut ContextStats::default(),
        );
        assert_eq!(selected.len(), 1);
        assert_eq!((selected[0].start_line, selected[0].end_line), expected);
    }
}

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
            role: crate::retrieval::ChunkRole::from_chunk(chunk),
            score: 10.0 - rank as f32,
            debug: FusionDebug {
                ranks: Vec::new(),
                rrf_score: 10.0 - rank as f32,
                symbol_boost: 0.0,
                path_boost: 0.0,
                rerank_boost: 0.0,
                role_boost: 0.0,
                model_rerank_score: None,
                model_rerank_delta: 0.0,
                penalty: 0.0,
                rerank_matched_tokens: 0,
                rerank_query_tokens: 0,
                symbol_match: None,
                role: crate::retrieval::ChunkRole::from_chunk(chunk),
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
    assert_eq!(
        (entry.retrieval_start_line, entry.retrieval_end_line),
        (entry.hydrated_start_line, entry.hydrated_end_line)
    );
    assert!(
        entry.end_line < entry.hydrated_end_line,
        "截断后仍必须保留完整目标范围"
    );
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

/// 检索命中符号内部片段时，最终上下文必须回填完整符号正文并保留原命中范围。
#[test]
fn symbol_hit_hydrates_complete_body() {
    let (host, index, _) = fixture(FIXTURE);
    let chunk = index
        .chunks()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("PermissionManager::check"))
        .expect("固定装置应含 check 方法");
    let retrieval_start = chunk.start_line + 1;
    let retrieval_end = chunk.end_line - 1;
    let result = synthetic(chunk, retrieval_start, retrieval_end);

    let bundle = build(&host, &index, vec![result], &ContextConfig::default());
    let entry = bundle.entries.first().expect("应返回完整符号");

    assert_eq!(
        (entry.retrieval_start_line, entry.retrieval_end_line),
        (retrieval_start, retrieval_end)
    );
    assert_eq!(
        (entry.hydrated_start_line, entry.hydrated_end_line),
        (chunk.start_line, chunk.end_line)
    );
    assert_eq!(
        (entry.start_line, entry.end_line),
        (chunk.start_line, chunk.end_line)
    );
    assert!(entry.snippet.contains("pub fn check"));
    assert!(entry.snippet.contains("tool_call_id.is_empty()"));
    assert_eq!(bundle.stats.hydrated, 1);
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
    assert_eq!(entry.role, crate::retrieval::ChunkRole::Implementation);
    assert!(entry.retrieval_start_line > 0);
    assert!(entry.hydrated_end_line >= entry.hydrated_start_line);
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
        role: crate::retrieval::ChunkRole::from_chunk(chunk),
        score: 1.0,
        debug: FusionDebug {
            ranks: Vec::new(),
            rrf_score: 1.0,
            symbol_boost: 0.0,
            path_boost: 0.0,
            rerank_boost: 0.0,
            role_boost: 0.0,
            model_rerank_score: None,
            model_rerank_delta: 0.0,
            penalty: 0.0,
            rerank_matched_tokens: 0,
            rerank_query_tokens: 0,
            symbol_match: None,
            role: crate::retrieval::ChunkRole::from_chunk(chunk),
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
        &index,
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

/// 超长容器中的低分类型别名不能触发方法定位例外，把已有正文替换成一行类型定义。
#[test]
fn oversized_container_is_not_replaced_by_a_lower_ranked_type_alias() {
    let (_, index, _) = fixture(FIXTURE);
    let container = index
        .chunks()
        .find(|chunk| chunk.symbol_kind == Some(SymbolKind::Impl))
        .expect("结构容器");
    let mut alias = synthetic(container, 7, 7);
    alias.symbol_name = Some("DisplayMode".into());
    alias.qualified_symbol = Some("DisplayMode".into());
    alias.symbol_kind = Some(SymbolKind::TypeAlias);
    let selected = select(
        &index,
        vec![synthetic(container, 1, 300), alias],
        &ContextConfig::default(),
        &mut ContextStats::default(),
    );

    assert_eq!(selected.len(), 1);
    assert_eq!((selected[0].start_line, selected[0].end_line), (1, 300));
    assert_ne!(selected[0].symbol_name.as_deref(), Some("DisplayMode"));
    assert_eq!(selected[0].symbol_kind, Some(SymbolKind::Impl));
}

/// 长函数的前缀不可代表不可见尾部：语义尾窗必须独立返回，且源变后不能整体回填旧裁剪片。
#[test]
fn long_function_keeps_semantic_tail_beyond_its_visible_prefix() {
    let body: String = (1..=298)
        .map(|line| format!("    let stage_{line} = {line};\n"))
        .collect();
    let source = format!("pub fn perform_steps() {{\n{body}}}\n");
    let (host, index, _) = fixture(&[("src/steps.rs", source.as_str())]);
    let chunk = index
        .chunks()
        .find(|chunk| chunk.symbol_name.as_deref() == Some("perform_steps"))
        .expect("完整长函数");
    assert_eq!((chunk.start_line, chunk.end_line), (1, 300));
    let mut function = synthetic(chunk, 1, 300);
    function.score = 2.0;
    let results = vec![function, text_window(chunk, 270, 300, 1.0)];
    let config = ContextConfig::default();
    let bundle = build(&host, &index, results.clone(), &config);

    assert_eq!(bundle.entries.len(), 2);
    assert_eq!(
        (bundle.entries[0].start_line, bundle.entries[0].end_line),
        (1, 240)
    );
    assert_eq!(
        (bundle.entries[1].start_line, bundle.entries[1].end_line),
        (270, 300)
    );
    let expected = (1..=240).chain(270..=300).collect();
    assert_eq!(visible_lines(&bundle), expected);
    assert!(bundle.entries[1].snippet.contains("let stage_298 = 298"));
    assert_eq!(
        (
            bundle.entries[1].retrieval_start_line,
            bundle.entries[1].retrieval_end_line
        ),
        (270, 300),
        "实际尾窗的原召回范围应保留"
    );
    for entry in &bundle.entries {
        assert_eq!(
            (entry.hydrated_start_line, entry.hydrated_end_line),
            (1, 300)
        );
        assert!(entry.truncated);
        assert!(entry.symbol_name.is_none() && entry.signature.is_none());
    }

    let shifted = format!("// 新增的文件说明\n{source}");
    let modified = RepoHost::new(&[("src/steps.rs", shifted.as_str())]);
    let stale = build(&modified, &index, results, &config);
    assert!(
        stale.entries.is_empty(),
        "依赖旧可见区间的前缀和尾窗均应保守丢弃"
    );
    assert_eq!(stale.stats.dropped_stale, 2);
}

/// 尾窗跨越可见前缀边界时只返回未覆盖部分，避免用完整目标范围制造重复行。
#[test]
fn tail_window_overlapping_visible_prefix_keeps_only_new_lines() {
    let source = numbered_source(300);
    let (host, index, _) = fixture(&[("src/steps.txt", source.as_str())]);
    let chunk = index.chunks().next().expect("文本窗口");
    let bundle = build(
        &host,
        &index,
        vec![
            text_window(chunk, 1, 300, 2.0),
            text_window(chunk, 230, 260, 1.0),
        ],
        &ContextConfig::default(),
    );

    assert_eq!(bundle.entries.len(), 2);
    assert_eq!(
        (bundle.entries[1].start_line, bundle.entries[1].end_line),
        (241, 260)
    );
    assert_eq!(visible_lines(&bundle), (1..=260).collect());
    assert_eq!(
        (
            bundle.entries[1].hydrated_start_line,
            bundle.entries[1].hydrated_end_line
        ),
        (1, 300)
    );
    assert!(bundle.entries[1].truncated);
}

/// 低分窗口伸出原长目标时也要按真实前缀去重，不能丢掉它与不可见尾部重合的行。
#[test]
fn partly_overlapping_window_is_not_hidden_by_invisible_target_tail() {
    let source = numbered_source(340);
    let (host, index, _) = fixture(&[("src/steps.txt", source.as_str())]);
    let chunk = index.chunks().next().expect("文本窗口");
    let bundle = build(
        &host,
        &index,
        vec![
            text_window(chunk, 1, 300, 2.0),
            text_window(chunk, 270, 340, 1.0),
        ],
        &ContextConfig::default(),
    );

    assert_eq!(bundle.entries.len(), 2);
    assert_eq!(
        (bundle.entries[1].start_line, bundle.entries[1].end_line),
        (270, 340)
    );
    assert_eq!(visible_lines(&bundle), (1..=240).chain(270..=340).collect());
    assert!(bundle.entries[0].truncated);
    assert!(!bundle.entries[1].truncated, "新窗口的完整范围没有被裁剪");
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
        &index,
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
