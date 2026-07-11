//! Session state for the ReAct loop.
//! ReAct loop 的会话状态。

use crate::model::{ContentBlock, MessageRole, ModelMessage};
use agent_tool::ToolResult;
use serde::{Deserialize, Serialize};

/// Provider-neutral conversation transcript.
/// 与服务商无关的会话记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(default)]
    messages: Vec<ModelMessage>,
}

impl Session {
    /// Create an empty session.
    /// 创建空会话。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从系统提示词和完整消息列表恢复会话。
    ///
    /// 该方法不校验消息顺序，持久化层和上下文插件可以据此恢复经过转换的会话。
    pub fn from_parts(system: Option<String>, messages: Vec<ModelMessage>) -> Self {
        Self { system, messages }
    }

    /// 将会话拆分为系统提示词和完整消息列表。
    ///
    /// 返回值保留消息顺序，可用于持久化或交给外部上下文管理器处理。
    pub fn into_parts(self) -> (Option<String>, Vec<ModelMessage>) {
        (self.system, self.messages)
    }

    /// Set the top-level system prompt.
    /// 设置顶层 system prompt。
    pub fn set_system(&mut self, system: impl Into<String>) {
        self.system = Some(system.into());
    }

    /// Get the system prompt.
    /// 获取 system prompt。
    pub fn system(&self) -> Option<&String> {
        self.system.as_ref()
    }

    /// Push a user message.
    /// 追加用户消息。
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages
            .push(ModelMessage::text(MessageRole::User, text.into()));
    }

    /// Push an assistant text message.
    /// 追加 assistant 文本消息。
    pub fn push_assistant_text(&mut self, text: impl Into<String>) {
        self.messages
            .push(ModelMessage::text(MessageRole::Assistant, text.into()));
    }

    /// Push assistant content blocks, including tool calls.
    /// 追加 assistant 内容块，包括工具调用。
    pub fn push_assistant_blocks(&mut self, content: Vec<ContentBlock>) {
        if !content.is_empty() {
            self.messages.push(ModelMessage {
                role: MessageRole::Assistant,
                content,
            });
        }
    }

    /// Push a single tool result message.
    /// 追加单个工具结果消息。
    pub fn push_tool_result(&mut self, result: ToolResult) {
        self.push_tool_results(vec![result]);
    }

    /// Push one tool message containing all results from a model turn.
    /// 追加一个工具消息，包含本轮模型请求产生的所有工具结果。
    pub fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        if !results.is_empty() {
            self.messages.push(ModelMessage::tool_results(results));
        }
    }

    /// Read all model messages.
    /// 读取所有模型消息。
    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// Clone messages for a model request.
    /// 为模型请求克隆消息。
    pub fn model_messages(&self) -> Vec<ModelMessage> {
        self.messages.clone()
    }

    /// Return the last assistant text.
    /// 返回最后一次 assistant 文本。
    pub fn last_assistant_text(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|message| matches!(&message.role, MessageRole::Assistant))
            .map(ModelMessage::text_content)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip_preserves_session() {
        let mut session = Session::new();
        session.set_system("保持简洁");
        session.push_user("你好");
        session.push_assistant_text("你好");

        let (system, messages) = session.clone().into_parts();

        assert_eq!(Session::from_parts(system, messages), session);
    }

    #[test]
    fn serde_round_trip_preserves_session() {
        let mut session = Session::new();
        session.set_system("使用工具前先检查参数");
        session.push_user("读取配置");
        session.push_assistant_text("配置已读取");

        let encoded = serde_json::to_string(&session).expect("会话应该可以序列化");
        let decoded: Session = serde_json::from_str(&encoded).expect("会话应该可以反序列化");

        assert_eq!(decoded, session);
    }
}
