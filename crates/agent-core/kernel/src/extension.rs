//! Agent 的宿主无关扩展点。
//!
//! core 只定义运行循环需要的最小接口，不感知具体扩展格式、加载方式或用户界面。

use crate::{event::AgentEvent, model::ModelMessage};
use agent_tool::{ToolCall, ToolDecision, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

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

/// 将多个扩展组合为一个扩展，按注册顺序转发每个钩子。
///
/// 与 [`crate::event::CompositeEventSink`] 对称：应用层可以在插件宿主之外
/// 叠加自己的扩展（如工具策略、审计），而不必替换已挂载的扩展。
///
/// 组合语义：
/// - `prompt_messages` / `list_tools` / `drain_events`：按顺序拼接所有结果；
/// - `call_tool`：第一个返回 `Some` 的扩展生效，后续不再询问；
/// - `before_tool`：按顺序串联决策——任何扩展返回 `Block` 立即短路，
///   `Rewrite` 的结果作为后续扩展的输入继续检查；
/// - `after_tool` / `on_event`：全部依次调用。
#[derive(Default)]
pub struct CompositeAgentExtension {
    extensions: Vec<Arc<dyn AgentExtension>>,
}

impl CompositeAgentExtension {
    /// 创建空的组合扩展。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个扩展；钩子按追加顺序执行。
    pub fn push(&mut self, extension: Arc<dyn AgentExtension>) -> &mut Self {
        self.extensions.push(extension);
        self
    }
}

#[async_trait]
impl AgentExtension for CompositeAgentExtension {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        let mut messages = Vec::new();
        for extension in &self.extensions {
            messages.extend(extension.prompt_messages().await?);
        }
        Ok(messages)
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        for extension in &self.extensions {
            tools.extend(extension.list_tools().await?);
        }
        Ok(tools)
    }

    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        for extension in &self.extensions {
            if let Some(result) = extension.call_tool(call.clone()).await? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        // 串联决策：Rewrite 结果继续交给后续扩展检查，保证 allowlist
        // 等安全扩展始终看到最终将要执行的调用。
        let mut current: Option<ToolCall> = None;
        for extension in &self.extensions {
            let decision = extension
                .before_tool(current.as_ref().unwrap_or(call))
                .await?;
            match decision {
                ToolDecision::Allow => {}
                ToolDecision::Block { reason } => return Ok(ToolDecision::Block { reason }),
                ToolDecision::CancelRun { reason } => {
                    return Ok(ToolDecision::CancelRun { reason });
                }
                ToolDecision::Rewrite { call } => current = Some(call),
            }
        }
        Ok(match current {
            Some(call) => ToolDecision::Rewrite { call },
            None => ToolDecision::Allow,
        })
    }

    async fn after_tool(&self, result: &ToolResult) -> Result<()> {
        for extension in &self.extensions {
            extension.after_tool(result).await?;
        }
        Ok(())
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        for extension in &self.extensions {
            extension.on_event(event).await?;
        }
        Ok(())
    }

    async fn drain_events(&self) -> Result<Vec<Value>> {
        let mut payloads = Vec::new();
        for extension in &self.extensions {
            payloads.extend(extension.drain_events().await?);
        }
        Ok(payloads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageRole;
    use serde_json::json;

    /// 将指定工具调用重写为固定名称的测试扩展。
    struct RewriteExtension {
        from: &'static str,
        to: &'static str,
    }

    #[async_trait]
    impl AgentExtension for RewriteExtension {
        async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
            if call.name == self.from {
                let mut call = call.clone();
                call.name = self.to.to_string();
                return Ok(ToolDecision::Rewrite { call });
            }
            Ok(ToolDecision::Allow)
        }
    }

    /// 阻止指定工具名称的测试扩展。
    struct BlockExtension {
        name: &'static str,
    }

    #[async_trait]
    impl AgentExtension for BlockExtension {
        async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
            if call.name == self.name {
                return Ok(ToolDecision::Block {
                    reason: format!("禁止调用 {}", self.name),
                });
            }
            Ok(ToolDecision::Allow)
        }
    }

    /// 提供固定提示与工具处理的测试扩展。
    struct FixedExtension {
        prompt: &'static str,
        tool: Option<&'static str>,
    }

    #[async_trait]
    impl AgentExtension for FixedExtension {
        async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
            Ok(vec![ModelMessage::text(
                MessageRole::Developer,
                self.prompt,
            )])
        }

        async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
            Ok(self
                .tool
                .filter(|name| *name == call.name)
                .map(|name| ToolResult::success(call.id, name, json!({"from": self.prompt}))))
        }
    }

    /// 组合前置决策：重写结果继续交给后续扩展检查，Block 短路。
    #[tokio::test]
    async fn composite_before_tool_chains_rewrite_into_block() {
        let mut composite = CompositeAgentExtension::new();
        composite.push(Arc::new(RewriteExtension {
            from: "raw",
            to: "restricted",
        }));
        composite.push(Arc::new(BlockExtension { name: "restricted" }));

        let decision = composite
            .before_tool(&ToolCall::new("call_1", "raw", json!({})))
            .await
            .expect("决策应成功");

        assert!(matches!(decision, ToolDecision::Block { .. }));
    }

    /// 只有重写时返回最终重写结果；无扩展介入时返回 Allow。
    #[tokio::test]
    async fn composite_before_tool_returns_final_rewrite_or_allow() {
        let mut composite = CompositeAgentExtension::new();
        composite.push(Arc::new(RewriteExtension {
            from: "raw",
            to: "safe",
        }));

        let rewritten = composite
            .before_tool(&ToolCall::new("call_1", "raw", json!({})))
            .await
            .expect("决策应成功");
        assert!(matches!(
            rewritten,
            ToolDecision::Rewrite { call } if call.name == "safe"
        ));

        let untouched = composite
            .before_tool(&ToolCall::new("call_2", "other", json!({})))
            .await
            .expect("决策应成功");
        assert_eq!(untouched, ToolDecision::Allow);
    }

    /// call_tool 由第一个能处理的扩展生效；提示按注册顺序拼接。
    #[tokio::test]
    async fn composite_concatenates_prompts_and_first_tool_wins() {
        let mut composite = CompositeAgentExtension::new();
        composite.push(Arc::new(FixedExtension {
            prompt: "第一",
            tool: Some("shared"),
        }));
        composite.push(Arc::new(FixedExtension {
            prompt: "第二",
            tool: Some("shared"),
        }));

        let prompts = composite.prompt_messages().await.expect("提示应成功");
        assert_eq!(
            prompts
                .iter()
                .map(ModelMessage::text_content)
                .collect::<Vec<_>>(),
            vec!["第一", "第二"]
        );

        let result = composite
            .call_tool(ToolCall::new("call_1", "shared", json!({})))
            .await
            .expect("调用应成功")
            .expect("应有扩展处理");
        assert_eq!(result.content, json!({"from": "第一"}));
    }
}
