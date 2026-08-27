//! 跨 provider 消息转换。
//!
//! 参考 pi-ai `transformMessages`：当对话从一个 provider 切换到另一个时，
//! 某些内容块需要降级或规范化。本模块不关心消息来源——只根据**目标 provider**
//! 的能力约束做统一变换。

use super::types::{ContentBlock, MessageRole, ModelMessage};
use std::collections::HashSet;

/// tool call ID 的最大长度（Anthropic 限制 64 字符）。
const MAX_TOOL_CALL_ID_LEN: usize = 64;

/// 将消息序列清洗为任意 provider 都可接受的格式。
///
/// 变换规则：
/// 1. Thinking 块：丢弃 signature（跨 provider 不可复用），保留 thinking 文本
/// 2. Tool call ID：截断到 `MAX_TOOL_CALL_ID_LEN`，同步更新对应 ToolResult
/// 3. 孤立 tool call：如果 assistant 调用了工具但没有对应的 ToolResult，补全一条错误结果
/// 4. 跳过内容为空的 assistant 消息
pub fn transform_messages(messages: &[ModelMessage]) -> Vec<ModelMessage> {
    let mut result = Vec::with_capacity(messages.len());

    // 第一遍：收集已有 ToolResult 的 call_id 集合
    let existing_result_ids: HashSet<&str> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { result } => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();

    // 第二遍：转换
    for msg in messages {
        match msg.role {
            MessageRole::Assistant => {
                let mut new_content = Vec::with_capacity(msg.content.len());
                let mut orphan_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        // Thinking 块：丢弃 signature，保留文本
                        ContentBlock::Thinking { thinking, .. } => {
                            if !thinking.is_empty() {
                                new_content.push(ContentBlock::Thinking {
                                    thinking: thinking.clone(),
                                    signature: None,
                                });
                            }
                        }
                        // Tool call：截断 ID + 检查孤立
                        ContentBlock::ToolCall { call } => {
                            let mut call = call.clone();
                            if call.id.len() > MAX_TOOL_CALL_ID_LEN {
                                call.id = call.id[..MAX_TOOL_CALL_ID_LEN].to_string();
                            }
                            if !existing_result_ids.contains(call.id.as_str()) {
                                orphan_calls.push(call.clone());
                            }
                            new_content.push(ContentBlock::ToolCall { call });
                        }
                        other => {
                            new_content.push(other.clone());
                        }
                    }
                }

                if !new_content.is_empty() {
                    result.push(ModelMessage {
                        role: MessageRole::Assistant,
                        content: new_content,
                    });
                }

                // 为孤立 tool call 补全错误结果
                if !orphan_calls.is_empty() {
                    let orphan_results: Vec<ContentBlock> = orphan_calls
                        .into_iter()
                        .map(|call| ContentBlock::ToolResult {
                            result: agent_tool::ToolResult::error(
                                call.id,
                                call.name,
                                "No result provided",
                            ),
                        })
                        .collect();
                    result.push(ModelMessage {
                        role: MessageRole::Tool,
                        content: orphan_results,
                    });
                }
            }
            MessageRole::Tool => {
                // Tool result：同步截断 call_id
                let new_content: Vec<ContentBlock> = msg
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::ToolResult { result } => {
                            let mut r = result.clone();
                            if r.call_id.len() > MAX_TOOL_CALL_ID_LEN {
                                r.call_id = r.call_id[..MAX_TOOL_CALL_ID_LEN].to_string();
                            }
                            ContentBlock::ToolResult { result: r }
                        }
                        other => other.clone(),
                    })
                    .collect();
                result.push(ModelMessage {
                    role: MessageRole::Tool,
                    content: new_content,
                });
            }
            _ => {
                result.push(msg.clone());
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tool::{ToolCall, ToolResult};

    /// Thinking 块的 signature 在转换后被丢弃。
    #[test]
    fn thinking_signature_is_stripped() {
        let messages = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "分析中...".into(),
                    signature: Some("sig_abc".into()),
                },
                ContentBlock::Text {
                    text: "结论".into(),
                },
            ],
        }];

        let result = transform_messages(&messages);
        let thinking = &result[0].content[0];
        match thinking {
            ContentBlock::Thinking { signature, .. } => {
                assert!(signature.is_none());
            }
            _ => panic!("应该是 Thinking 块"),
        }
    }

    /// 超长 tool call ID 被截断到 64 字符，ToolResult 同步截断。
    #[test]
    fn long_tool_call_id_is_truncated() {
        let long_id = "x".repeat(100);
        let messages = vec![
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall::new(long_id.clone(), "test", serde_json::json!({})),
                }],
            },
            ModelMessage {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: ToolResult::success(long_id.clone(), "test", serde_json::json!("ok")),
                }],
            },
        ];

        let result = transform_messages(&messages);
        if let ContentBlock::ToolCall { call } = &result[0].content[0] {
            assert_eq!(call.id.len(), 64);
        }
        if let ContentBlock::ToolResult { result: r } = &result[1].content[0] {
            assert_eq!(r.call_id.len(), 64);
        }
    }

    /// 孤立 tool call（无对应 ToolResult）会被补全一条错误结果。
    #[test]
    fn orphan_tool_call_gets_error_result() {
        let messages = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                call: ToolCall::new("call_1", "read_file", serde_json::json!({})),
            }],
        }];

        let result = transform_messages(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].role, MessageRole::Tool);
        if let ContentBlock::ToolResult { result: r } = &result[1].content[0] {
            assert!(r.is_error);
        }
    }
}
