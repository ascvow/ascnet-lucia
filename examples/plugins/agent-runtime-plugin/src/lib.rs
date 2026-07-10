//! Lucia Agent Runtime 宿主能力的 WASM Guest 示例。
//!
//! 插件只负责把模型工具调用映射到类型化 [`PluginHostApi`]；模型、服务商配置、
//! Agent 权限和运行资源始终由 Host 注册的 profile 决定。

use anyhow::{anyhow, Context};
use agent_plugin::{
    export_plugin, AgentContinueRequest, AgentId, AgentMessageRequest, AgentSpawnRequest,
    AgentPlugin, PluginHostApi, Result, ToolCall, ToolResult, ToolSpec,
};
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
                "返回当前插件 controller 的可信 Agent ID。",
            ),
            ToolSpec::new(
                "agent_runtime_spawn",
                "使用受限 worker profile 异步启动派生 Agent。调用立即返回 Agent ID，不等待任务完成；随后使用 agent_runtime_status 和 agent_runtime_result 轮询。",
                json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "交给派生 Agent 的首次用户输入。",
                            "minLength": 1
                        }
                    },
                    "required": ["input"],
                    "additionalProperties": false
                }),
            ),
            ToolSpec::new(
                "agent_runtime_continue",
                "从成功终态 Agent 的私有会话异步创建后续运行；调用立即返回新句柄。",
                json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "已成功结束且当前 controller 有权管理的 Agent ID。",
                            "minLength": 1
                        },
                        "input": {
                            "type": "string",
                            "description": "追加到目标会话的新用户输入。",
                            "minLength": 1
                        }
                    },
                    "required": ["target", "input"],
                    "additionalProperties": false
                }),
            ),
            target_tool(
                "agent_runtime_status",
                "查询 controller 或其后代 Agent 的当前状态、谱系与有效权限。",
            ),
            target_tool(
                "agent_runtime_result",
                "读取派生 Agent 的幂等终态结果；任务尚未结束时返回 completed=false。",
            ),
            target_tool(
                "agent_runtime_cancel",
                "级联取消 controller 的指定后代任务，并返回本次是否执行了取消。",
            ),
            ToolSpec::new(
                "agent_runtime_send",
                "以当前 controller 的可信身份向同一 owner、同一派生树内的 Agent 发送结构化消息。",
                json!({
                    "type": "object",
                    "properties": {
                        "recipient": {
                            "type": "string",
                            "description": "接收者 Agent ID。",
                            "minLength": 1
                        },
                        "topic": {
                            "type": "string",
                            "description": "由插件协议解释的非空消息主题。",
                            "minLength": 1
                        },
                        "payload": {
                            "description": "由插件协议解释的任意 JSON 载荷。"
                        }
                    },
                    "required": ["recipient", "topic"],
                    "additionalProperties": false
                }),
            ),
            empty_tool(
                "agent_runtime_receive",
                "非阻塞读取 controller 邮箱中的下一条消息；空邮箱立即返回 available=false。",
            ),
        ]
    }

    /// 使用类型化 Host API 执行短控制面操作。
    ///
    /// 该入口不会在 Guest 内等待后台 Agent 完成；派生、观察和收信均为单次调用。
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
                        "next": "使用 agent_runtime_status 或 agent_runtime_result 轮询，禁止在插件内阻塞等待。"
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
                        "next": "使用 agent_runtime_status 或 agent_runtime_result 轮询后续运行。"
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
            "agent_runtime_send" => {
                let recipient = AgentId::parse(required_string(&call.args, "recipient")?)?;
                let topic = required_string(&call.args, "topic")?.to_string();
                let payload = call.args.get("payload").cloned().unwrap_or(Value::Null);
                let message_id = host.send_agent_message(&AgentMessageRequest {
                    recipient,
                    topic,
                    payload,
                })?;
                success(call, json!({ "message_id": message_id }))
            }
            "agent_runtime_receive" => {
                let message = host.try_receive_agent_message()?;
                success(
                    call,
                    json!({
                        "available": message.is_some(),
                        "message": message
                    }),
                )
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
                    "description": "由 Agent Runtime 返回的不透明 Agent ID。",
                    "minLength": 1
                }
            },
            "required": ["target"],
            "additionalProperties": false
        }),
    )
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
