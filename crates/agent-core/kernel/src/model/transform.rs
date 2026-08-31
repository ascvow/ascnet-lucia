//! 跨 provider 消息转换。
//!
//! 参考 pi-ai `transformMessages`：当对话从一个 provider 切换到另一个时，
//! 某些内容块需要降级或规范化。本模块不关心消息来源——只根据**目标 provider**
//! 的能力约束做统一变换。

use super::types::{ContentBlock, MessageRole, ModelMessage};
use std::collections::{HashMap, HashSet};

/// tool call ID 的最大 UTF-8 字节数（Anthropic 限制 64 字节）。
const MAX_TOOL_CALL_ID_LEN: usize = 64;

/// 返回不超过指定 UTF-8 字节数且始终落在字符边界上的前缀。
fn utf8_prefix_at_most_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// 为超长 tool call ID 分配不超过上限且在当前消息序列内唯一的规范 ID。
///
/// 冲突后缀会占用长度预算，因此会重新按 UTF-8 字符边界缩短原始前缀。
fn allocate_unique_tool_call_id(original_id: &str, used_ids: &HashSet<String>) -> String {
    let base = utf8_prefix_at_most_bytes(original_id, MAX_TOOL_CALL_ID_LEN);
    if !used_ids.contains(base) {
        return base.to_string();
    }

    for collision_index in 1..=usize::MAX {
        let suffix = format!("~{collision_index}");
        let prefix = utf8_prefix_at_most_bytes(
            original_id,
            MAX_TOOL_CALL_ID_LEN.saturating_sub(suffix.len()),
        );
        let candidate = format!("{prefix}{suffix}");
        if !used_ids.contains(&candidate) {
            return candidate;
        }
    }

    unreachable!("usize 范围内没有可用的 tool call ID")
}

/// 建立原始 tool call ID 到规范 ID 的稳定映射，供调用与结果共同使用。
///
/// 合法短 ID 会先被保留，避免超长 ID 的截断前缀抢占已有合法标识；超长 ID
/// 再按消息出现顺序分配唯一值，从而保证碰撞时调用与原始结果不会串线。
fn build_tool_call_id_map(messages: &[ModelMessage]) -> HashMap<String, String> {
    let original_ids: Vec<&str> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call } => Some(call.id.as_str()),
            ContentBlock::ToolResult { result } => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    let mut id_map = HashMap::with_capacity(original_ids.len());
    let mut used_ids = HashSet::with_capacity(original_ids.len());

    for original_id in &original_ids {
        if original_id.len() <= MAX_TOOL_CALL_ID_LEN {
            used_ids.insert((*original_id).to_string());
            id_map.insert((*original_id).to_string(), (*original_id).to_string());
        }
    }

    for original_id in original_ids {
        if id_map.contains_key(original_id) {
            continue;
        }

        let normalized_id = allocate_unique_tool_call_id(original_id, &used_ids);
        used_ids.insert(normalized_id.clone());
        id_map.insert(original_id.to_string(), normalized_id);
    }

    id_map
}

/// 将消息序列清洗为任意 provider 都可接受的格式。
///
/// 变换规则：
/// 1. Thinking 块：仅通过逻辑 provider/model 预筛内部私有信封，最终端点 scope 由适配器校验
/// 2. 加密推理块：没有可移植文本，逻辑目标未知或不匹配时直接丢弃
/// 3. Tool call ID：规范化到 `MAX_TOOL_CALL_ID_LEN` 字节内，并同步更新对应 ToolResult
/// 4. 孤立 tool call：如果 assistant 调用了工具但没有对应的 ToolResult，补全一条错误结果
/// 5. 跳过内容为空的 assistant 消息
pub fn transform_messages(messages: &[ModelMessage]) -> Vec<ModelMessage> {
    transform_messages_for_target(messages, None)
}

/// 按目标逻辑模型路由清洗消息，并为适配器保留可能同源的服务商私有推理块。
///
/// 带旧版原始签名但缺少内部来源信封，或来源与目标 provider/model 任一不同时，整个
/// 私有块都会被丢弃。端点 scope 只能由具体适配器掌握，因此适配器必须在 wire 边界
/// 再次比较完整来源路由。
pub fn transform_messages_for_route(
    messages: &[ModelMessage],
    provider: &str,
    model: &str,
) -> Vec<ModelMessage> {
    transform_messages_for_target(messages, Some((provider, model)))
}

/// 执行共享消息清洗；目标逻辑路由只做私有推理块的第一层候选过滤。
fn transform_messages_for_target(
    messages: &[ModelMessage],
    target_route: Option<(&str, &str)>,
) -> Vec<ModelMessage> {
    let mut result = Vec::with_capacity(messages.len());
    let tool_call_id_map = build_tool_call_id_map(messages);

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
                        // 仅同一逻辑路由的内部私有信封进入候选集；旧原始签名按未知来源丢弃。
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            let private_reasoning = block.private_reasoning();
                            let same_route = private_reasoning.as_ref().is_some_and(|private| {
                                target_route.is_some_and(|(provider, model)| {
                                    private
                                        .source_route()
                                        .matches_logical_route(provider, model)
                                })
                            });
                            if same_route {
                                // display=omitted 或加密推理时 thinking 可以为空，信封仍须原样回传。
                                new_content.push(block.clone());
                            } else if signature.is_none() && !thinking.is_empty() {
                                // 无任何供应商私有标记的通用推理文本可以继续留在中立上下文。
                                new_content.push(ContentBlock::Thinking {
                                    thinking: thinking.clone(),
                                    signature: None,
                                });
                            }
                        }
                        // Tool call：统一规范 ID，并用原始 ID 检查是否存在结果。
                        ContentBlock::ToolCall { call } => {
                            let has_result = existing_result_ids.contains(call.id.as_str());
                            let mut call = call.clone();
                            call.id = tool_call_id_map
                                .get(call.id.as_str())
                                .cloned()
                                .unwrap_or_else(|| call.id.clone());
                            if !has_result {
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
                // Tool result：通过同一映射同步规范 call_id。
                let new_content: Vec<ContentBlock> = msg
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { result } => {
                            let mut r = result.clone();
                            r.call_id = tool_call_id_map
                                .get(r.call_id.as_str())
                                .cloned()
                                .unwrap_or_else(|| r.call_id.clone());
                            Some(ContentBlock::ToolResult { result: r })
                        }
                        ContentBlock::Thinking { .. } => None,
                        other => Some(other.clone()),
                    })
                    .collect();
                result.push(ModelMessage {
                    role: MessageRole::Tool,
                    content: new_content,
                });
            }
            _ => {
                // 私有推理块只可能是模型生成的 assistant 内容；其他角色一律安全降级。
                let content = msg
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking {
                            thinking,
                            signature: None,
                        } if !thinking.is_empty() => Some(ContentBlock::Thinking {
                            thinking: thinking.clone(),
                            signature: None,
                        }),
                        ContentBlock::Thinking { .. } => None,
                        other => Some(other.clone()),
                    })
                    .collect();
                result.push(ModelMessage {
                    role: msg.role.clone(),
                    content,
                });
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tool::{ToolCall, ToolResult};

    /// 读取测试消息中指定位置的 tool call ID，并在结构不符合预期时立即失败。
    fn tool_call_id(message: &ModelMessage, index: usize) -> &str {
        match &message.content[index] {
            ContentBlock::ToolCall { call } => call.id.as_str(),
            _ => panic!("预期为 ToolCall 内容块"),
        }
    }

    /// 读取测试消息中指定位置的 tool result call ID，并在结构不符合预期时立即失败。
    fn tool_result_call_id(message: &ModelMessage, index: usize) -> &str {
        match &message.content[index] {
            ContentBlock::ToolResult { result } => result.call_id.as_str(),
            _ => panic!("预期为 ToolResult 内容块"),
        }
    }

    /// 目标路由未知时不得保留或暴露带供应商签名的私有推理块。
    #[test]
    fn private_thinking_is_dropped_without_matching_route() {
        let source_route = super::super::types::ModelRoute::new("anthropic-main", "claude-a");
        let messages = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::private_signed_thinking("分析中...", "sig_abc", source_route.clone()),
                ContentBlock::private_redacted_thinking("opaque-data", source_route),
                ContentBlock::Text {
                    text: "结论".into(),
                },
            ],
        }];

        let result = transform_messages(&messages);

        assert_eq!(result[0].content.len(), 1);
        assert!(matches!(result[0].content[0], ContentBlock::Text { .. }));
    }

    /// 同一 provider/model 路由必须保留签名与加密推理块的原始顺序和值。
    #[test]
    fn same_route_preserves_provider_private_reasoning() {
        let route = super::super::types::ModelRoute::new("anthropic-main", "claude-a");
        let messages = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::private_signed_thinking("内部推理", "sig-private", route.clone()),
                ContentBlock::private_redacted_thinking("encrypted-private", route),
                ContentBlock::Text {
                    text: "准备调用工具".into(),
                },
            ],
        }];

        let result = transform_messages_for_route(&messages, "anthropic-main", "claude-a");

        assert_eq!(result, messages);
    }

    /// 即使 provider 相同，只要模型不同也必须丢弃全部私有推理块。
    #[test]
    fn different_model_cannot_reuse_provider_private_reasoning() {
        let route = super::super::types::ModelRoute::new("anthropic-main", "claude-a");
        let messages = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::private_signed_thinking("可降级文本", "sig-private", route.clone()),
                ContentBlock::private_redacted_thinking("encrypted-private", route),
            ],
        }];

        let result = transform_messages_for_route(&messages, "anthropic-main", "claude-b");

        assert!(result.is_empty());
    }

    /// ASCII 超长 ID 规范化后调用与结果仍精确配对，且不会补出伪孤立结果。
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
        assert_eq!(result.len(), 2);
        let call_id = tool_call_id(&result[0], 0);
        let result_id = tool_result_call_id(&result[1], 0);
        assert_eq!(call_id.len(), MAX_TOOL_CALL_ID_LEN);
        assert_eq!(call_id, result_id);
    }

    /// 多字节超长 ID 必须在 UTF-8 字符边界截断，并保持调用与结果配对。
    #[test]
    fn multibyte_tool_call_id_is_truncated_at_utf8_boundary() {
        let long_id = "工具调用".repeat(20);
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
                    result: ToolResult::success(long_id, "test", serde_json::json!("ok")),
                }],
            },
        ];

        let result = transform_messages(&messages);
        assert_eq!(result.len(), 2);
        let call_id = tool_call_id(&result[0], 0);
        let result_id = tool_result_call_id(&result[1], 0);
        assert!(call_id.len() <= MAX_TOOL_CALL_ID_LEN);
        assert_eq!(call_id, result_id);
    }

    /// 共享相同 64 字节前缀的超长 ID 必须获得不同规范 ID，结果仍跟随原调用。
    #[test]
    fn colliding_truncated_ids_keep_results_with_original_calls() {
        let shared_prefix = "x".repeat(MAX_TOOL_CALL_ID_LEN);
        let first_id = format!("{shared_prefix}-first");
        let second_id = format!("{shared_prefix}-second");
        let messages = vec![
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::ToolCall {
                        call: ToolCall::new(first_id.clone(), "first", serde_json::json!({})),
                    },
                    ContentBlock::ToolCall {
                        call: ToolCall::new(second_id.clone(), "second", serde_json::json!({})),
                    },
                ],
            },
            ModelMessage {
                role: MessageRole::Tool,
                content: vec![
                    ContentBlock::ToolResult {
                        result: ToolResult::success(
                            second_id,
                            "second",
                            serde_json::json!("second-result"),
                        ),
                    },
                    ContentBlock::ToolResult {
                        result: ToolResult::success(
                            first_id,
                            "first",
                            serde_json::json!("first-result"),
                        ),
                    },
                ],
            },
        ];

        let result = transform_messages(&messages);
        assert_eq!(result.len(), 2);
        let first_call_id = tool_call_id(&result[0], 0);
        let second_call_id = tool_call_id(&result[0], 1);
        assert_ne!(first_call_id, second_call_id);
        assert!(first_call_id.len() <= MAX_TOOL_CALL_ID_LEN);
        assert!(second_call_id.len() <= MAX_TOOL_CALL_ID_LEN);
        assert_eq!(tool_result_call_id(&result[1], 0), second_call_id);
        assert_eq!(tool_result_call_id(&result[1], 1), first_call_id);
    }

    /// 合法 64 字节 ID 必须原样保留，超长同前缀 ID 应避让并获得唯一后缀。
    #[test]
    fn valid_max_length_id_is_not_taken_by_longer_id() {
        let valid_id = "v".repeat(MAX_TOOL_CALL_ID_LEN);
        let long_id = format!("{valid_id}-long");
        let messages = vec![
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::ToolCall {
                        call: ToolCall::new(long_id.clone(), "long", serde_json::json!({})),
                    },
                    ContentBlock::ToolCall {
                        call: ToolCall::new(valid_id.clone(), "valid", serde_json::json!({})),
                    },
                ],
            },
            ModelMessage {
                role: MessageRole::Tool,
                content: vec![
                    ContentBlock::ToolResult {
                        result: ToolResult::success(
                            long_id,
                            "long",
                            serde_json::json!("long-result"),
                        ),
                    },
                    ContentBlock::ToolResult {
                        result: ToolResult::success(
                            valid_id.clone(),
                            "valid",
                            serde_json::json!("valid-result"),
                        ),
                    },
                ],
            },
        ];

        let result = transform_messages(&messages);
        assert_eq!(result.len(), 2);
        let long_call_id = tool_call_id(&result[0], 0);
        let valid_call_id = tool_call_id(&result[0], 1);
        assert_eq!(valid_call_id, valid_id);
        assert_ne!(long_call_id, valid_call_id);
        assert!(long_call_id.len() <= MAX_TOOL_CALL_ID_LEN);
        assert_eq!(tool_result_call_id(&result[1], 0), long_call_id);
        assert_eq!(tool_result_call_id(&result[1], 1), valid_call_id);
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
        assert_eq!(result[1].content.len(), 1);
        let ContentBlock::ToolResult { result: r } = &result[1].content[0] else {
            panic!("预期为补全的 ToolResult 内容块");
        };
        assert!(r.is_error);
        assert_eq!(r.call_id, "call_1");
    }
}
