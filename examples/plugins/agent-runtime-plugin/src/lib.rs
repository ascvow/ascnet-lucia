//! Lucia Agent Runtime 宿主能力的 WASM Guest 示例。
//!
//! 插件只负责把模型工具调用映射到类型化 [`PluginHostApi`]；模型、服务商配置、
//! Agent 权限和运行资源始终由 Host 注册的 profile 决定。

use agent_plugin::{
    export_plugin, AgentContinueRequest, AgentId, AgentPlugin, AgentSpawnRequest, PluginHostApi,
    Result, ToolCall, ToolPermissionScope, ToolResult, ToolSpec,
};
use anyhow::{anyhow, Context};
use serde_json::{json, Value};

/// manifest 允许该插件请求的唯一 Agent 派生策略。
const WORKER_PROFILE: &str = "worker";

/// 展示 Agent Runtime 控制面能力的无状态插件。
#[derive(Default)]
struct AgentRuntimePlugin;

impl AgentPlugin for AgentRuntimePlugin {
    /// 返回供模型调用的 Agent Runtime 控制面工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![
            empty_tool(
                "agent_runtime_identity",
                "<tool_usage><purpose>Return the current plugin controller's host-bound Agent ID.</purpose></tool_usage>",
            ),
            ToolSpec::new(
                "agent_runtime_spawn",
                "<tool_usage><purpose>Start a child Agent asynchronously with the restricted worker profile.</purpose><workflow>Provide a complete task and use delegation only when it adds value. The returned ID confirms startup, not completion. Do independent work before checking status; retrieve agent_runtime_result before using the outcome. Avoid tight polling loops.</workflow><constraints>The Host controls the profile, identity, permissions, and resource limits.</constraints></tool_usage>",
                json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "Complete initial task for the child Agent, including scope and expected deliverables.",
                            "minLength": 1
                        }
                    },
                    "required": ["input"],
                    "additionalProperties": false
                }),
            )
            .with_permission_scope(ToolPermissionScope::Control),
            ToolSpec::new(
                "agent_runtime_continue",
                "<tool_usage><purpose>Start a continuation from a successfully completed Agent's private session.</purpose><result>Return a new execution handle immediately; retrieve its terminal result before claiming completion.</result><constraints>The target must be manageable by the current controller; inherited permissions remain host-enforced.</constraints></tool_usage>",
                json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Successfully completed Agent ID manageable by the current controller.",
                            "minLength": 1
                        },
                        "input": {
                            "type": "string",
                            "description": "New user input to append to the target session.",
                            "minLength": 1
                        }
                    },
                    "required": ["target", "input"],
                    "additionalProperties": false
                }),
            )
            .with_permission_scope(ToolPermissionScope::Control),
            target_tool(
                "agent_runtime_status",
                "<tool_usage><purpose>Inspect the controller or a descendant Agent's status, lineage, and effective permissions.</purpose><constraints>Avoid repeated polling without a reason to expect progress.</constraints></tool_usage>",
            ),
            target_tool(
                "agent_runtime_result",
                "<tool_usage><purpose>Read a child Agent's idempotent terminal result.</purpose><result>completed=false means the task is still pending, not successful.</result><workflow>Inspect the outcome and its evidence before reporting completion.</workflow></tool_usage>",
            ),
            target_tool(
                "agent_runtime_cancel",
                "<tool_usage><purpose>Cancel a selected descendant and its subtree; return whether cancellation was performed.</purpose><constraints>Cancellation does not undo completed side effects.</constraints></tool_usage>",
            ),
        ]
    }

    /// 使用类型化 Host API 执行短控制面操作。
    ///
    /// 该入口不会在 Guest 内等待后台 Agent 完成；派生、继续和观察均为单次调用。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        let operation = call.name.clone();
        match operation.as_str() {
            "agent_runtime_identity" => {
                let identity = host.agent_identity()?;
                success(call, json!({ "controller_id": identity.as_str() }))
            }
            "agent_runtime_spawn" => {
                let input = required_string(&call.args, "input")?;
                let handle = host.spawn_agent(&AgentSpawnRequest::new(WORKER_PROFILE, input))?;
                success(
                    call,
                    json!({
                        "handle": handle,
                        "profile": WORKER_PROFILE,
                        "completed": false,
                        "next": "<next_step>Do independent work, then check agent_runtime_status or retrieve agent_runtime_result. Avoid tight polling; do not block inside the plugin.</next_step>"
                    }),
                )
            }
            "agent_runtime_continue" => {
                let target = target_id(&call.args)?;
                let input = required_string(&call.args, "input")?;
                let handle = host.continue_agent(&AgentContinueRequest::new(target, input))?;
                success(
                    call,
                    json!({
                        "handle": handle,
                        "completed": false,
                        "next": "<next_step>Use the new handle for agent_runtime_status or agent_runtime_result. Avoid tight polling and verify the terminal outcome.</next_step>"
                    }),
                )
            }
            "agent_runtime_status" => {
                let target = target_id(&call.args)?;
                let snapshot = host.agent_status(&target)?;
                success(call, json!({ "snapshot": snapshot }))
            }
            "agent_runtime_result" => {
                let target = target_id(&call.args)?;
                let outcome = host.agent_result(&target)?;
                success(
                    call,
                    json!({
                        "completed": outcome.is_some(),
                        "outcome": outcome
                    }),
                )
            }
            "agent_runtime_cancel" => {
                let target = target_id(&call.args)?;
                let cancelled = host.cancel_agent(&target)?;
                success(call, json!({ "cancelled": cancelled }))
            }
            _ => Ok(ToolResult::error(
                call.id,
                call.name,
                format!("未知 Agent Runtime 工具：{operation}"),
            )),
        }
    }
}

/// 创建一个不接受参数的工具定义。
fn empty_tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        name,
        description,
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 创建一个只接收不透明 Agent ID 的工具定义。
fn target_tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        name,
        description,
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Opaque Agent ID returned by Agent Runtime.",
                    "minLength": 1
                }
            },
            "required": ["target"],
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 读取并校验工具参数中的必填非空字符串。
///
/// 参数不存在、类型错误或只包含空白时返回可直接交给模型修正的错误。
fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("参数 `{key}` 必须是字符串"))?;
    if value.trim().is_empty() {
        return Err(anyhow!("参数 `{key}` 不能为空"));
    }
    Ok(value)
}

/// 从工具参数恢复 Runtime 返回的不透明 Agent ID。
fn target_id(args: &Value) -> Result<AgentId> {
    AgentId::parse(required_string(args, "target")?)
}

/// 构造保留原调用 ID 和工具名的成功结果。
fn success(call: ToolCall, content: Value) -> Result<ToolResult> {
    Ok(ToolResult::success(call.id, call.name, content))
}

export_plugin!(AgentRuntimePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    /// Runtime 工具只调用 Host 受限控制面，必须统一声明 control scope。
    #[test]
    fn runtime_tools_declare_control_scope() {
        let tools = AgentRuntimePlugin.list_tools();
        assert!(!tools.is_empty());
        assert!(tools.iter().all(|tool| tool.description.is_ascii()
            && tool.description.starts_with("<tool_usage>")
            && tool.description.ends_with("</tool_usage>")));
        assert!(tools
            .iter()
            .all(|tool| tool.permission_scope == ToolPermissionScope::Control));
    }
}
