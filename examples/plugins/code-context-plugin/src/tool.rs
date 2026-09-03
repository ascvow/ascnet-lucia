//! `code_search` 工具契约。
//!
//! 这个 schema 一旦暴露给模型，后续修改成本很高，因此在骨架阶段就按最终形态确定。
//!
//! # 参数只暴露意图，不暴露实现
//!
//! `query`、`max_results`、`token_budget`、`path_scope` 描述的都是调用方的意图。
//! 检索策略参数（词法与语义的权重、RRF 常数、召回深度）永远属于插件内部：把它们
//! 暴露出去，等于让模型去调一个它无法评估效果的旋钮，同时把内部实现焊死在工具契约上。

use crate::error::CodeContextError;
use crate::index::IndexStats;
use crate::model::IndexStatus;
use agent_plugin::{ToolCall, ToolPermissionScope, ToolResult, ToolSpec};
use serde_json::{json, Value};

/// 工具在模型侧的名称。
pub const TOOL_NAME: &str = "code_search";

/// 单次检索允许返回的最大结果数。
const MAX_RESULTS_LIMIT: u64 = 50;
/// 未指定时返回的结果数。
const DEFAULT_MAX_RESULTS: u64 = 10;
/// 单次检索允许占用的最小与最大 token 预算。
const MIN_TOKEN_BUDGET: u64 = 500;
const MAX_TOKEN_BUDGET: u64 = 50_000;
/// 未指定时的 token 预算。
const DEFAULT_TOKEN_BUDGET: u64 = 8_000;
/// 查询字符串的最大长度，避免把整段文件当作查询提交。
const MAX_QUERY_CHARS: usize = 1_000;
/// 路径范围允许的最大条目数。
const MAX_PATH_SCOPE_ENTRIES: usize = 32;

/// 一次已校验的检索请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSearchRequest {
    /// 自然语言或标识符查询。
    pub query: String,
    /// 期望返回的结果数上限。
    pub max_results: u64,
    /// 结果正文允许占用的 token 预算。
    pub token_budget: u64,
    /// 限定检索范围的仓库相对路径前缀；为空表示整个仓库。
    pub path_scope: Vec<String>,
}

impl CodeSearchRequest {
    /// 解析并校验模型提交的参数。
    ///
    /// 越界的数值收敛到合法区间而不是报错——模型给出 `max_results: 500` 时，返回 50 条
    /// 显然比让这次调用失败更有用。但缺失或空白的 `query` 必须失败：那说明模型没想清楚
    /// 要找什么，替它猜一个查询只会浪费一轮。
    ///
    /// # Errors
    ///
    /// `query` 缺失、非字符串、空白或超长时返回 [`CodeContextError::InvalidConfiguration`]。
    pub fn parse(args: &Value) -> Result<Self, CodeContextError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if query.is_empty() {
            return Err(CodeContextError::InvalidConfiguration(
                "code_search 需要非空的 query".into(),
            ));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(CodeContextError::InvalidConfiguration(format!(
                "code_search 的 query 超过 {MAX_QUERY_CHARS} 字上限"
            )));
        }

        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS_LIMIT);
        let token_budget = args
            .get("token_budget")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_BUDGET)
            .clamp(MIN_TOKEN_BUDGET, MAX_TOKEN_BUDGET);
        let path_scope = args
            .get("path_scope")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .take(MAX_PATH_SCOPE_ENTRIES)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            query: query.to_string(),
            max_results,
            token_budget,
            path_scope,
        })
    }
}

/// 返回 `code_search` 的工具定义。
///
/// 声明的 scope 必须与 manifest `[capabilities.tool_scopes]` 一致：Host 会比对二者，
/// 不一致时直接拒绝加载插件。
pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_NAME,
        "按自然语言或标识符检索当前仓库，返回最相关的代码位置（文件、行区间与符号）。\
         适合回答「这个行为实现在哪里」这类问题，用它替代多轮 grep 与读文件。\
         索引尚未建立时会明确返回不可用状态，此时请改用常规检索工具。",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "要查找的内容。可以是自然语言问题，也可以是符号名。"
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_RESULTS_LIMIT,
                    "description": "返回的代码位置数量上限，缺省为 10。"
                },
                "token_budget": {
                    "type": "integer",
                    "minimum": MIN_TOKEN_BUDGET,
                    "maximum": MAX_TOKEN_BUDGET,
                    "description": "结果正文允许占用的 token 预算，缺省为 8000。"
                },
                "path_scope": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "限定检索范围的仓库相对路径前缀，缺省为整个仓库。"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Read)
}

/// 构造索引不可用时的工具结果。
///
/// 用错误结果而不是「成功但结果为空」：后者会被模型读成「仓库里没有相关代码」，
/// 从而得出错误结论并停止查找。错误结果配合明确的后续动作建议，模型会退回常规检索。
pub fn unavailable_result(call: &ToolCall, status: IndexStatus, detail: &str) -> ToolResult {
    unavailable_result_with_index(call, status, detail, None)
}

/// 构造带索引规模的不可用结果。
///
/// `index` 是当前结构索引的规模；索引尚未建成时为 `None`。这组数字既让用户知道引擎在
/// 干什么，也是 smoke test 验证扫描与解析真的跑通的唯一可观测出口——没有它，
/// "索引建成了"就只能靠推断。
pub fn unavailable_result_with_index(
    call: &ToolCall,
    status: IndexStatus,
    detail: &str,
    index: Option<IndexStats>,
) -> ToolResult {
    let mut details = json!({
        "status": status.as_str(),
        "results": [],
        "detail": detail,
    });
    if let Some(stats) = index {
        details["index"] = json!({
            "files": stats.files,
            "chunks": stats.chunks,
            "symbols": stats.symbols,
            "structural_files": stats.structural_files,
            "text_files": stats.text_files,
            "truncated": stats.truncated,
        });
    }
    ToolResult::error(
        call.id.clone(),
        call.name.clone(),
        format!("代码索引不可用（{}）：{detail}", status.as_str()),
    )
    .with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一次工具调用。
    fn call(args: Value) -> ToolCall {
        ToolCall::new("call-1", TOOL_NAME, args)
    }

    /// 工具 scope 必须与 manifest 声明一致，否则 Host 会拒绝加载。
    #[test]
    fn tool_declares_read_scope() {
        assert_eq!(spec().permission_scope, ToolPermissionScope::Read);
        assert_eq!(spec().name, TOOL_NAME);
    }

    /// 工具 schema 不得暴露任何内部检索参数。
    #[test]
    fn schema_hides_internal_retrieval_knobs() {
        let schema = serde_json::to_string(&spec().input_schema).expect("序列化工具 schema");
        for internal in [
            "bm25_weight",
            "vector_weight",
            "rrf_k",
            "embedding_top_k",
            "rerank",
        ] {
            assert!(!schema.contains(internal), "schema 泄漏内部参数 {internal}");
        }
    }

    /// 只给 query 时必须得到可用的默认值。
    #[test]
    fn defaults_are_applied() {
        let request =
            CodeSearchRequest::parse(&json!({"query": "权限在哪里检查"})).expect("解析最小请求");
        assert_eq!(request.max_results, DEFAULT_MAX_RESULTS);
        assert_eq!(request.token_budget, DEFAULT_TOKEN_BUDGET);
        assert!(request.path_scope.is_empty());
    }

    /// 越界数值必须收敛到合法区间，而不是让调用失败。
    #[test]
    fn out_of_range_values_are_clamped() {
        let request = CodeSearchRequest::parse(&json!({
            "query": "x",
            "max_results": 5_000,
            "token_budget": 1
        }))
        .expect("越界请求应被收敛而不是拒绝");
        assert_eq!(request.max_results, MAX_RESULTS_LIMIT);
        assert_eq!(request.token_budget, MIN_TOKEN_BUDGET);
    }

    /// 空白与缺失的 query 必须被拒绝。
    #[test]
    fn blank_query_is_rejected() {
        assert!(CodeSearchRequest::parse(&json!({"query": "   "})).is_err());
        assert!(CodeSearchRequest::parse(&json!({})).is_err());
    }

    /// 超长 query 必须被拒绝，避免把整段文件当查询提交。
    #[test]
    fn oversized_query_is_rejected() {
        let query = "a".repeat(MAX_QUERY_CHARS + 1);
        assert!(CodeSearchRequest::parse(&json!({ "query": query })).is_err());
    }

    /// 路径范围必须过滤空白项并限制条目数。
    #[test]
    fn path_scope_is_normalized() {
        let entries = (0..MAX_PATH_SCOPE_ENTRIES + 10)
            .map(|index| json!(format!("src/{index}")))
            .chain([json!("  "), json!("")])
            .collect::<Vec<_>>();
        let request = CodeSearchRequest::parse(&json!({"query": "x", "path_scope": entries}))
            .expect("解析路径范围");
        assert_eq!(request.path_scope.len(), MAX_PATH_SCOPE_ENTRIES);
        assert!(request
            .path_scope
            .iter()
            .all(|entry| !entry.trim().is_empty()));
    }

    /// 不可用结果必须是错误，且不得让模型误读成「没有匹配」。
    #[test]
    fn unavailable_result_is_an_error_with_status() {
        let result = unavailable_result(
            &call(json!({"query": "x"})),
            IndexStatus::NotIndexed,
            "索引尚未建立",
        );
        assert!(result.is_error);
        let details = result.details.expect("不可用结果应带结构化细节");
        assert_eq!(details["status"], "not_indexed");
        assert_eq!(details["results"], json!([]));
    }
}
