//! 插件只读会话访问的宿主端口与投影。
//!
//! 应用注入的是一个**只读**端口而不是 `SessionStore`：后者带有 `save` 与 `delete`，
//! 把它交给插件宿主意味着宿主持有了写入历史会话的能力，即使当前没有代码去调用。
//! 这里用类型而不是纪律来保证插件路径不可能写会话。
//!
//! 投影与脱敏留在本 crate（ADR-0001 可信不可变）内完成，不下放给应用，
//! 避免不同宿主各自实现出宽窄不一的脱敏口径。

use agent_core::model::{ContentBlock, ModelMessage};
use agent_core::session::Session;
use agent_plugin_protocol::{
    PluginSessionMessage, PluginSessionPage, PluginSessionSummary, PluginSessionToolCall,
    PluginSessionToolResult,
};
use anyhow::Result;
use async_trait::async_trait;

/// 单次读取允许返回的最大消息数。
///
/// 单条消息可能携带完整文件正文或长 shell 输出，因此按条数限制并不足以约束体积，
/// 还需要配合 [`MAX_SESSION_PAGE_BYTES`] 一起收敛。
pub const MAX_SESSION_PAGE_MESSAGES: usize = 200;

/// 单次读取允许返回的最大投影字节数。
///
/// 达到上限时提前结束当前页，让调用方按偏移量继续读取，而不是把超大响应
/// 一次性塞进插件线性内存。
pub const MAX_SESSION_PAGE_BYTES: usize = 1024 * 1024;

/// 单条工具结果或消息正文允许投影的最大字符数。
const MAX_SESSION_TEXT_CHARS: usize = 20_000;

/// 会话选择所需的轻量元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSessionDescriptor {
    /// 会话稳定标识。
    pub id: String,
    /// 可供展示的标题。
    pub title: Option<String>,
    /// 记录创建时间，单位为 UNIX epoch 毫秒。
    pub created_at_ms: u64,
    /// 最近一次保存时间，单位为 UNIX epoch 毫秒。
    pub updated_at_ms: u64,
    /// 最近一次保存后的修订号。
    pub revision: u64,
    /// 会话内消息总数。
    pub message_count: usize,
}

impl From<&PluginSessionDescriptor> for PluginSessionSummary {
    fn from(descriptor: &PluginSessionDescriptor) -> Self {
        Self {
            id: descriptor.id.clone(),
            title: descriptor.title.clone(),
            created_at_ms: descriptor.created_at_ms,
            updated_at_ms: descriptor.updated_at_ms,
            revision: descriptor.revision,
            message_count: descriptor.message_count,
        }
    }
}

/// 应用向插件宿主注入的只读会话来源。
///
/// 实现方只需暴露列举与读取；宿主负责投影、脱敏、分页和配额，应用无需重复实现。
#[async_trait]
pub trait PluginSessionSource: Send + Sync {
    /// 列出当前项目的全部会话摘要。
    ///
    /// # Errors
    ///
    /// 底层存储不可读时返回错误。
    async fn list_sessions(&self) -> Result<Vec<PluginSessionDescriptor>>;

    /// 按标识读取完整会话；不存在时返回 `None`。
    ///
    /// # Errors
    ///
    /// 标识非法、记录损坏或底层存储不可读时返回错误。
    async fn load_session(&self, id: &str) -> Result<Option<Session>>;
}

/// 按显示字符数截断长文本，并标注截断量。
fn truncate_text(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_SESSION_TEXT_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_SESSION_TEXT_CHARS).collect();
    format!("{kept}\n…（已截断 {} 字）", total - MAX_SESSION_TEXT_CHARS)
}

/// 把一条会话消息投影为插件可见结构。
///
/// 丢弃模型私有推理块：推理可能携带只能回传给来源路由的签名，暴露给插件既没有
/// 检索价值，又会让私有推理越过它的路由边界。图片只保留存在标记，不投影字节。
fn project_message(index: usize, message: &ModelMessage) -> PluginSessionMessage {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    let mut has_attachments = false;

    for block in &message.content {
        match block {
            ContentBlock::Text { text: value } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
            ContentBlock::ToolCall { call } => tool_calls.push(PluginSessionToolCall {
                name: call.name.clone(),
                arguments: truncate_text(&call.args.to_string()),
            }),
            ContentBlock::ToolResult { result } => tool_results.push(PluginSessionToolResult {
                name: result.name.clone(),
                is_error: result.is_error,
                content: truncate_text(&result.content.to_string()),
            }),
            ContentBlock::Thinking { .. } => {}
            _ => has_attachments = true,
        }
    }

    PluginSessionMessage {
        index,
        role: message.role.as_str().to_string(),
        text: truncate_text(&text),
        tool_calls,
        tool_results,
        has_attachments,
    }
}

/// 把会话投影为分页后的插件可见结构。
///
/// `offset` 越界时返回空页而不是错误：调用方按页推进到末尾是正常控制流，
/// 不应当被当作失败处理。
pub fn project_session(
    id: &str,
    session: &Session,
    offset: usize,
    limit: usize,
) -> PluginSessionPage {
    let messages = session.messages();
    let limit = limit.clamp(1, MAX_SESSION_PAGE_MESSAGES);
    let mut projected = Vec::new();
    let mut bytes = 0usize;

    for (offset_index, message) in messages.iter().skip(offset).take(limit).enumerate() {
        let item = project_message(offset + offset_index, message);
        bytes += item.text.len()
            + item
                .tool_calls
                .iter()
                .map(|call| call.name.len() + call.arguments.len())
                .sum::<usize>()
            + item
                .tool_results
                .iter()
                .map(|result| result.name.len() + result.content.len())
                .sum::<usize>();
        projected.push(item);
        // 至少返回一条，避免单条超限时调用方永远无法推进。
        if bytes >= MAX_SESSION_PAGE_BYTES {
            break;
        }
    }

    PluginSessionPage {
        id: id.to_string(),
        message_count: messages.len(),
        messages: projected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::model::MessageRole;
    use agent_tool::{ToolCall, ToolResult};
    use serde_json::json;

    /// 构造带各类内容块的会话。
    fn mixed_session() -> Session {
        Session::from_parts(
            Some("system".into()),
            vec![
                ModelMessage::text(MessageRole::User, "查一下构建命令"),
                ModelMessage {
                    role: MessageRole::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "私有推理不得外泄".into(),
                            signature: Some("sig".into()),
                        },
                        ContentBlock::Text {
                            text: "我来看看".into(),
                        },
                        ContentBlock::ToolCall {
                            call: ToolCall::new("c1", "shell", json!({"command": "bun run build"})),
                        },
                    ],
                },
                ModelMessage {
                    role: MessageRole::Tool,
                    content: vec![ContentBlock::ToolResult {
                        result: ToolResult::success("c1", "shell", json!({"stdout": "ok"})),
                    }],
                },
            ],
        )
    }

    /// 模型私有推理必须被丢弃：它带有只能回传来源路由的签名，不能经插件外流。
    #[test]
    fn projection_drops_private_thinking() {
        let page = project_session("s1", &mixed_session(), 0, 50);

        let serialized = serde_json::to_string(&page).expect("序列化投影");
        assert!(!serialized.contains("私有推理不得外泄"), "{serialized}");
        assert!(!serialized.contains("sig"), "{serialized}");
        assert!(serialized.contains("我来看看"));
    }

    /// 工具调用与结果都要投影，KB 需要它们才能还原"当时做了什么"。
    #[test]
    fn projection_keeps_tool_calls_and_results() {
        let page = project_session("s1", &mixed_session(), 0, 50);

        assert_eq!(page.messages[1].tool_calls[0].name, "shell");
        assert!(page.messages[1].tool_calls[0]
            .arguments
            .contains("bun run build"));
        assert_eq!(page.messages[2].tool_results[0].name, "shell");
        assert!(!page.messages[2].tool_results[0].is_error);
    }

    /// 偏移量越界返回空页而不是错误——按页读到末尾是正常控制流。
    #[test]
    fn projection_past_end_returns_empty_page() {
        let page = project_session("s1", &mixed_session(), 99, 50);

        assert!(page.messages.is_empty());
        assert_eq!(page.message_count, 3);
    }

    /// 超长正文必须截断并标注，避免单条消息撑爆插件线性内存。
    #[test]
    fn projection_truncates_oversized_text() {
        let session = Session::from_parts(
            None,
            vec![ModelMessage::text(
                MessageRole::User,
                "x".repeat(MAX_SESSION_TEXT_CHARS + 500),
            )],
        );

        let page = project_session("s1", &session, 0, 50);

        assert!(page.messages[0].text.contains("已截断"));
        assert!(page.messages[0].text.chars().count() < MAX_SESSION_TEXT_CHARS + 100);
    }

    /// 单页体积达到上限即提前收尾，但至少返回一条以保证调用方能推进。
    #[test]
    fn projection_stops_at_byte_budget_but_always_advances() {
        // 单条正文被截断到 MAX_SESSION_TEXT_CHARS，因此要超过 1 MiB 页预算需要足够多条。
        let big = "x".repeat(MAX_SESSION_TEXT_CHARS);
        let total = MAX_SESSION_PAGE_BYTES / MAX_SESSION_TEXT_CHARS + 20;
        let session = Session::from_parts(
            None,
            (0..total)
                .map(|_| ModelMessage::text(MessageRole::User, big.clone()))
                .collect(),
        );

        let page = project_session("s1", &session, 0, MAX_SESSION_PAGE_MESSAGES);

        assert!(!page.messages.is_empty(), "至少返回一条");
        assert!(page.messages.len() < total, "超出字节预算应提前收尾");
    }
}
