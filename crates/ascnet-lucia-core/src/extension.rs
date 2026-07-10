//! Agent 的宿主无关扩展点。
//!
//! core 只定义运行循环需要的最小接口，不感知具体扩展格式、加载方式或用户界面。

use crate::{event::AgentEvent, model::ModelMessage};
use agent_tool::{ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 工具执行前由扩展返回的通用决策。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDecision {
    /// 原样允许工具调用。
    #[default]
    Allow,
    /// 阻止工具调用，并将原因作为工具错误返回给模型。
    Block { reason: String },
    /// 在执行前重写工具调用。
    Rewrite { call: ToolCall },
}

/// Agent 运行循环支持的最小宿主扩展接口。
///
/// 实现方可以补充工具、调整工具调用或观察生命周期事件。core 不关心实现方是
/// 静态组件、动态模块还是应用层测试替身。
#[async_trait]
pub trait AgentExtension: Send + Sync {
    /// 返回本次模型请求前需要注入的提示消息。
    ///
    /// 消息不会写入会话历史，扩展可以在运行期间动态更新贡献内容。
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        Ok(Vec::new())
    }

    /// 返回扩展提供的工具定义。
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(Vec::new())
    }

    /// 执行扩展提供的工具；不能处理时返回 `None`。
    async fn call_tool(&self, _call: ToolCall) -> Result<Option<ToolResult>> {
        Ok(None)
    }

    /// 在工具执行前检查或重写调用。
    async fn before_tool(&self, _call: &ToolCall) -> Result<ToolDecision> {
        Ok(ToolDecision::Allow)
    }

    /// 在任意工具执行后接收结果。
    async fn after_tool(&self, _result: &ToolResult) -> Result<()> {
        Ok(())
    }

    /// 接收 Agent 生命周期事件。
    async fn on_event(&self, _event: &AgentEvent) -> Result<()> {
        Ok(())
    }

    /// 取出扩展等待发布的结构化事件载荷。
    ///
    /// Core 会将每个载荷包装为 [`crate::event::AgentEventKind::Extension`] 并写入
    /// 当前事件 sink。返回后实现方必须移除这些事件，避免重复发布。
    async fn drain_events(&self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}

/// 不执行任何操作的默认 Agent 扩展。
#[derive(Debug, Clone, Default)]
pub struct NoopAgentExtension;

#[async_trait]
impl AgentExtension for NoopAgentExtension {}
