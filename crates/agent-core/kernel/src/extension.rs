//! Agent 的宿主无关贡献扩展点。
//!
//! core 只定义运行循环需要的最小接口，不感知具体扩展格式、加载方式或用户界面。
//!
//! 本接口只保留**贡献型**能力：向模型提供提示、工具定义、工具实现和结构化事件载荷。
//! **决策型**能力（准入、改写、取消、观察）全部由 [`crate::bus`] 上的可取消事件承担，
//! 因此权限插件、审计扩展与界面都以同一套 [`agent_event::EventListener`] 契约接入，
//! 不再需要在这里增加专用钩子。

use crate::model::ModelMessage;
use agent_tool::{ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Agent 运行循环支持的最小宿主贡献接口。
///
/// 实现方可以补充提示、工具定义与工具实现。core 不关心实现方是静态组件、动态模块
/// 还是应用层测试替身。
#[async_trait]
pub trait AgentExtension: Send + Sync {
    /// 返回本次模型请求前需要注入的提示消息。
    ///
    /// 消息不会写入会话历史，扩展可以在运行期间动态更新贡献内容。
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        Ok(Vec::new())
    }

    /// 返回指定运行在模型请求前需要注入的提示消息。
    ///
    /// 默认转发旧版无运行参数接口，保持现有扩展兼容；需要隔离并发运行的扩展应覆盖此方法。
    async fn prompt_messages_for_run(&self, _run_id: &str) -> Result<Vec<ModelMessage>> {
        self.prompt_messages().await
    }

    /// 返回扩展提供的工具定义。
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(Vec::new())
    }

    /// 返回指定运行可见的扩展工具定义。
    ///
    /// 默认转发旧版接口；动态宿主可据此冻结并选择对应运行的能力快照。
    async fn list_tools_for_run(&self, _run_id: &str) -> Result<Vec<ToolSpec>> {
        self.list_tools().await
    }

    /// 执行扩展提供的工具；不能处理时返回 `None`。
    async fn call_tool(&self, _call: ToolCall) -> Result<Option<ToolResult>> {
        Ok(None)
    }

    /// 在指定运行中执行扩展工具；不能处理时返回 `None`。
    ///
    /// `run_id` 由 Core 生成并绑定，扩展不得信任模型或历史消息中的同名字段。
    async fn call_tool_for_run(&self, _run_id: &str, call: ToolCall) -> Result<Option<ToolResult>> {
        self.call_tool(call).await
    }

    /// 取出扩展等待发布的结构化事件载荷。
    ///
    /// Core 会将每个载荷包装为 [`crate::event::AgentEventKind::Extension`] 并写入
    /// 当前事件 sink。返回后实现方必须移除这些事件，避免重复发布。
    async fn drain_events(&self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }

    /// 取出指定运行等待发布的结构化事件载荷。
    async fn drain_events_for_run(&self, _run_id: &str) -> Result<Vec<Value>> {
        self.drain_events().await
    }

    /// 释放指定运行持有的扩展资源与能力快照。
    ///
    /// Core 会在成功、取消、失败和超时路径统一调用；默认实现无需清理。
    async fn finish_run(&self, _run_id: &str) -> Result<()> {
        Ok(())
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
/// 叠加自己的扩展（如内置工具集、审计），而不必替换已挂载的扩展。
///
/// 组合语义：
/// - `prompt_messages` / `list_tools` / `drain_events`：按顺序拼接所有结果；
/// - `call_tool`：第一个返回 `Some` 的扩展生效，后续不再询问；
/// - `finish_run`：即使前序扩展清理失败也继续通知其余扩展，最后合并错误。
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

    async fn prompt_messages_for_run(&self, run_id: &str) -> Result<Vec<ModelMessage>> {
        let mut messages = Vec::new();
        for extension in &self.extensions {
            messages.extend(extension.prompt_messages_for_run(run_id).await?);
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

    async fn list_tools_for_run(&self, run_id: &str) -> Result<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        for extension in &self.extensions {
            tools.extend(extension.list_tools_for_run(run_id).await?);
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

    async fn call_tool_for_run(&self, run_id: &str, call: ToolCall) -> Result<Option<ToolResult>> {
        for extension in &self.extensions {
            if let Some(result) = extension.call_tool_for_run(run_id, call.clone()).await? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    async fn drain_events(&self) -> Result<Vec<Value>> {
        let mut payloads = Vec::new();
        for extension in &self.extensions {
            payloads.extend(extension.drain_events().await?);
        }
        Ok(payloads)
    }

    async fn drain_events_for_run(&self, run_id: &str) -> Result<Vec<Value>> {
        let mut payloads = Vec::new();
        for extension in &self.extensions {
            payloads.extend(extension.drain_events_for_run(run_id).await?);
        }
        Ok(payloads)
    }

    async fn finish_run(&self, run_id: &str) -> Result<()> {
        let mut errors = Vec::new();
        for extension in &self.extensions {
            if let Err(error) = extension.finish_run(run_id).await {
                errors.push(format!("{error:#}"));
            }
        }
        if !errors.is_empty() {
            return Err(anyhow::anyhow!("运行扩展清理失败：{}", errors.join("；")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageRole;
    use serde_json::json;

    /// 提供固定提示与工具处理的测试扩展。
    struct FixedExtension {
        prompt: &'static str,
        tool: Option<&'static str>,
    }

    /// 记录清理调用并可注入错误，验证组合扩展不会因前序失败跳过后续安全收尾。
    struct CleanupExtension {
        name: &'static str,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        fail: bool,
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

    #[async_trait]
    impl AgentExtension for CleanupExtension {
        async fn finish_run(&self, run_id: &str) -> Result<()> {
            self.calls
                .lock()
                .expect("扩展清理调用锁不应中毒")
                .push(format!("{}:{run_id}", self.name));
            if self.fail {
                Err(anyhow::anyhow!("{} 清理失败", self.name))
            } else {
                Ok(())
            }
        }
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

    /// 前序扩展清理失败时仍必须执行后续扩展，并在全部尝试后返回聚合错误。
    #[tokio::test]
    async fn composite_finish_run_attempts_every_extension_after_failure() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut composite = CompositeAgentExtension::new();
        composite.push(Arc::new(CleanupExtension {
            name: "first",
            calls: Arc::clone(&calls),
            fail: true,
        }));
        composite.push(Arc::new(CleanupExtension {
            name: "permission",
            calls: Arc::clone(&calls),
            fail: false,
        }));

        let error = composite
            .finish_run("run-cleanup")
            .await
            .expect_err("前序失败应在全部清理后返回错误");

        assert!(error.to_string().contains("first 清理失败"));
        assert_eq!(
            calls.lock().expect("扩展清理调用锁不应中毒").as_slice(),
            ["first:run-cleanup", "permission:run-cleanup"]
        );
    }
}
