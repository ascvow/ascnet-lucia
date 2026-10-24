//! Core 在 ReAct 循环各决策点发布的 `core.*` 事件。
//!
//! Core 只发布事件并遵守取消结果，不知道谁在监听。宿主策略、Runtime 权限门禁与
//! WASM 插件都以同一套 [`agent_event::EventListener`] 契约接入，因此 core 不再需要
//! 为每类扩展保留专用钩子。
//!
//! # 取消语义
//!
//! 每个事件在被取消时的后果由 core 定义，监听者只负责表达"取消"这一个动作：
//!
//! | 事件 | 取消后果 |
//! |---|---|
//! | [`RunStartEvent`] | 运行不进入循环，直接以取消终态收尾 |
//! | [`TurnStartEvent`] | 当前轮不再请求模型，运行以取消终态收尾 |
//! | [`ModelRequestEvent`] | 本次请求不发往 provider，运行以取消终态收尾 |
//! | [`ToolCallEvent`] | 工具不执行，返回权限拒绝结果；`canceller` 即拒绝者 |
//! | [`MessageInjectEvent`] | 该条 steering 或 follow-up 不注入会话 |
//!
//! [`RunFinishEvent`]、[`TurnFinishEvent`]、[`ModelResponseEvent`]、[`ToolResultEvent`]
//! 与 [`AgentRecordEvent`] 是既成事实的通告，core 忽略它们的取消状态。
//!
//! # 信任边界
//!
//! 不可信监听者只能改写各事件 `merge_patch` 白名单内的字段。工具调用 ID、运行 ID 与
//! 权限 scope 不在任何白名单中，且 core 在分发返回后会重新绑定可信值，因此插件无法
//! 借改写把结果关联到其他调用或提升权限。

use crate::{
    event::AgentEvent,
    model::{FinishReason, ModelRequest, TokenUsage},
};
use agent_event::declare_event;
use agent_tool::{ToolCall, ToolResult};
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::Value;

/// 从补丁中读取一个可选字符串白名单字段。
///
/// 字段缺失保持原值，显式 `null` 清空，其他非字符串取值视为非法补丁。
fn merge_optional_text(target: &mut Option<String>, patch: &Value, field: &str) -> Result<()> {
    let Some(value) = patch.get(field) else {
        return Ok(());
    };
    if value.is_null() {
        *target = None;
        return Ok(());
    }
    *target = Some(
        value
            .as_str()
            .ok_or_else(|| anyhow!("事件字段 `{field}` 必须是字符串"))?
            .to_string(),
    );
    Ok(())
}

/// 一次 Agent 运行即将开始。
#[derive(Debug, Clone, Serialize)]
pub struct RunStartEvent {
    /// 本次运行的稳定 ID，由 core 生成。
    pub run_id: String,
    /// 逻辑服务商名称。
    pub provider: String,
    /// 模型 ID。
    pub model: String,
}

declare_event!(RunStartEvent, "core.run.start");

/// 一次 Agent 运行已结束。
#[derive(Debug, Clone, Serialize)]
pub struct RunFinishEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// 实际使用的模型步数。
    pub steps_used: usize,
    /// 是否因取消请求提前收尾。
    pub cancelled: bool,
    /// 本次运行汇总的 token 用量。
    pub usage: TokenUsage,
    /// 运行所绑定的会话路由键。
    ///
    /// 运行结束是插件回收会话级状态的唯一时机，但 `run_id` 本身不携带会话身份。
    /// Core 在这里带上应用绑定的可信值，插件才能把回收范围限定在对应会话；未绑定
    /// 会话的运行（例如纯 CLI）保持 `None`，插件应退回不区分会话的处理方式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

declare_event!(RunFinishEvent, "core.run.finish");

/// 一轮 ReAct 即将开始；一轮包含一次模型调用及其触发的工具执行。
#[derive(Debug, Clone, Serialize)]
pub struct TurnStartEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
}

declare_event!(TurnStartEvent, "core.turn.start");

/// 一轮 ReAct 已结束。
#[derive(Debug, Clone, Serialize)]
pub struct TurnFinishEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
}

declare_event!(TurnFinishEvent, "core.turn.finish");

/// 一次模型请求即将发往 provider。
///
/// 事件只暴露请求的规模与路由信息。完整消息体不进入总线：它包含会话全文，
/// 广播给任意订阅者会绕过上下文加载器为插件设定的可见性边界。
#[derive(Debug, Clone, Serialize)]
pub struct ModelRequestEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 逻辑服务商名称。
    pub provider: String,
    /// 模型 ID。
    pub model: String,
    /// 请求携带的消息条数。
    pub message_count: usize,
    /// 本轮暴露给模型的工具数量。
    pub tool_count: usize,
    /// 本次请求的最大输出 token 数。
    pub max_tokens: Option<u32>,
}

impl ModelRequestEvent {
    /// 根据即将发出的请求构造事件。
    pub fn new(
        run_id: impl Into<String>,
        step: usize,
        provider: impl Into<String>,
        request: &ModelRequest,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            step,
            provider: provider.into(),
            model: request.model.clone(),
            message_count: request.messages.len(),
            tool_count: request.tools.len(),
            max_tokens: request.max_tokens,
        }
    }
}

declare_event!(ModelRequestEvent, "core.model.request");

/// 模型已返回完整响应。
#[derive(Debug, Clone, Serialize)]
pub struct ModelResponseEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 服务商返回的停止原因。
    pub finish_reason: FinishReason,
    /// 响应中的工具调用数量。
    pub tool_call_count: usize,
    /// 响应可见正文的字节长度。
    pub text_len: usize,
}

declare_event!(ModelResponseEvent, "core.model.response");

/// 一次工具调用即将执行。
///
/// 这是 core 唯一的工具准入决策点，取代旧的 `before_tool` 钩子：
///
/// - 不取消且不改写 → 放行；
/// - 改写 `call.name` 或 `call.args` → 以改写后的调用执行，随后重新校验执行策略；
/// - 取消 → 拒绝执行，`canceller` 记录拒绝者，`block_reason` 供界面展示，
///   `agent_guidance` 只注入下一次模型请求；
/// - 取消并置 `cancel_run` → 同时结束整个运行。
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 即将执行的工具调用；只有 `name` 与 `args` 对不可信监听者开放。
    pub call: ToolCall,
    /// 拒绝执行时面向可信界面的详细原因；不写入会话，也不广播给其他监听者。
    pub block_reason: Option<String>,
    /// 拒绝执行时只注入下一次模型请求的脱敏指导；不写入会话历史。
    pub agent_guidance: Option<String>,
    /// 拒绝执行的同时是否结束整个运行。
    pub cancel_run: bool,
}

impl ToolCallEvent {
    /// 构造一次工具准入事件。
    pub fn new(run_id: impl Into<String>, step: usize, call: ToolCall) -> Self {
        Self {
            run_id: run_id.into(),
            step,
            call,
            block_reason: None,
            agent_guidance: None,
            cancel_run: false,
        }
    }

    /// 合并不可信监听者的补丁。
    ///
    /// 白名单仅包含工具名、工具参数与三个拒绝说明字段。调用 ID、运行 ID 和权限 scope
    /// 被刻意排除：它们是 provider 结果配对与权限判定的可信关联键，改写会让插件把结果
    /// 关联到其他调用或声明未被授予的权限。
    fn merge_patch(&mut self, patch: &Value) -> Result<()> {
        if let Some(call) = patch.get("call") {
            if let Some(name) = call.get("name") {
                let name = name
                    .as_str()
                    .ok_or_else(|| anyhow!("工具名必须是字符串"))?
                    .trim();
                if name.is_empty() {
                    return Err(anyhow!("改写后的工具名不能为空"));
                }
                self.call.name = name.to_string();
            }
            if let Some(args) = call.get("args") {
                self.call.args = args.clone();
            }
        }
        merge_optional_text(&mut self.block_reason, patch, "block_reason")?;
        merge_optional_text(&mut self.agent_guidance, patch, "agent_guidance")?;
        if let Some(cancel_run) = patch.get("cancel_run") {
            self.cancel_run = cancel_run
                .as_bool()
                .ok_or_else(|| anyhow!("cancel_run 必须是布尔值"))?;
        }
        Ok(())
    }
}

declare_event!(
    ToolCallEvent,
    "core.tool.call",
    merge = ToolCallEvent::merge_patch
);

/// 一次工具调用已产生终态结果。
///
/// 广播给全部监听者，因此携带的是脱敏结果：被拒绝的调用只会看到稳定拒绝文案，
/// 不包含仅面向用户的审批原因。完整结果只写入可信 [`crate::event::EventSink`]。
#[derive(Debug, Clone, Serialize)]
pub struct ToolResultEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 脱敏后的工具结果。
    pub result: ToolResult,
    /// 由 core 注入的实际执行来源：`native`、`plugin`、`runtime` 或策略拒绝来源。
    pub origin: String,
}

declare_event!(ToolResultEvent, "core.tool.result");

/// 待注入会话的用户消息类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectKind {
    /// 在工具执行间隙插入并跳过剩余工具的 steering 消息。
    Steering,
    /// 在任务完成后继续循环的 follow-up 消息。
    FollowUp,
}

/// 一条 steering 或 follow-up 消息即将注入会话。
#[derive(Debug, Clone, Serialize)]
pub struct MessageInjectEvent {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 注入类型。
    pub kind: InjectKind,
    /// 即将写入会话的消息正文；对不可信监听者开放改写。
    pub text: String,
}

impl MessageInjectEvent {
    /// 构造一次消息注入事件。
    pub fn new(
        run_id: impl Into<String>,
        step: usize,
        kind: InjectKind,
        text: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            step,
            kind,
            text: text.into(),
        }
    }

    /// 合并不可信监听者的补丁；只允许改写消息正文。
    fn merge_patch(&mut self, patch: &Value) -> Result<()> {
        if let Some(text) = patch.get("text") {
            self.text = text
                .as_str()
                .ok_or_else(|| anyhow!("注入消息正文必须是字符串"))?
                .to_string();
        }
        Ok(())
    }
}

declare_event!(
    MessageInjectEvent,
    "core.message.inject",
    merge = MessageInjectEvent::merge_patch
);

/// core 写入事件 sink 的结构化记录的广播副本。
///
/// 用于替代旧的 `AgentExtension::on_event`：需要观察模型增量、计费用量、工具输出等
/// 细粒度记录的监听者订阅本事件即可，不必为每种记录新增专用事件。core 忽略它的取消
/// 状态——记录已经发生，取消没有意义。
#[derive(Debug, Clone, Serialize)]
pub struct AgentRecordEvent {
    /// 已写入 sink 的事件记录；工具终态使用脱敏副本。
    pub record: AgentEvent,
}

declare_event!(AgentRecordEvent, "core.agent.event");

#[cfg(test)]
mod tests {
    use super::*;
    use agent_event::EventData;
    use agent_tool::ToolPermissionScope;
    use serde_json::json;

    /// 补丁只能改写工具名与参数；调用身份与权限 scope 必须保持宿主绑定值。
    #[test]
    fn tool_call_merge_rejects_identity_fields() {
        let mut call = ToolCall::new("call-1", "shell", json!({ "command": "ls" }));
        call.bind_permission_scope(ToolPermissionScope::Read);
        call.bind_run_id("run-1");
        let mut event = ToolCallEvent::new("run-1", 0, call);

        event
            .merge(&json!({
                "call": {
                    "id": "伪造调用",
                    "name": "read_file",
                    "args": { "path": "/tmp/a" },
                    "run_id": "伪造运行",
                    "permission_scope": "unrestricted"
                },
                "block_reason": "路径不在允许范围内",
                "cancel_run": true
            }))
            .expect("白名单补丁应合并成功");

        assert_eq!(event.call.id, "call-1");
        assert_eq!(event.call.name, "read_file");
        assert_eq!(event.call.args, json!({ "path": "/tmp/a" }));
        assert_eq!(event.call.run_id.as_deref(), Some("run-1"));
        assert_eq!(event.call.permission_scope, ToolPermissionScope::Read);
        assert_eq!(event.block_reason.as_deref(), Some("路径不在允许范围内"));
        assert!(event.cancel_run);
    }

    /// 白名单字段取值非法时补丁整体报错，事件保持原状交由调用方按 fail-closed 处理。
    #[test]
    fn tool_call_merge_rejects_invalid_values() {
        let mut event = ToolCallEvent::new("run-1", 0, ToolCall::new("call-1", "shell", json!({})));

        assert!(event.merge(&json!({ "call": { "name": "  " } })).is_err());
        assert!(event.merge(&json!({ "cancel_run": "yes" })).is_err());
        assert_eq!(event.call.name, "shell");
        assert!(!event.cancel_run);
    }

    /// 只读事件忽略任何补丁。
    #[test]
    fn record_event_is_read_only() {
        let mut event = AgentRecordEvent {
            record: AgentEvent::new(
                "run-1",
                crate::event::AgentEventKind::RunStarted,
                0,
                json!({}),
            ),
        };

        event
            .merge(&json!({ "record": { "run_id": "伪造运行" } }))
            .expect("只读事件应忽略补丁");

        assert_eq!(event.record.run_id, "run-1");
    }
}
