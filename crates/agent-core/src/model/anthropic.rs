//! Anthropic Messages protocol adapter.
//! Anthropic Messages 协议适配器。

use super::util::{
    decode_json_response, endpoint_url, file_attachment_fallback_text, header_map,
    merge_provider_options,
};
use super::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelProviderConfig, ModelRequest,
    ModelResponse, ProviderAdapter, ProviderBilling, ReasoningLevel, TokenUsage, ToolChoice,
};
use agent_tool::{ToolCall, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use uuid::Uuid;

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Claude Code CLI 请求使用的客户端标识，用于兼容依赖官方客户端身份的模型网关。
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.80 (external, cli)";

/// Adapter for Anthropic Messages API.
/// Anthropic Messages API 适配器。
pub struct AnthropicMessagesAdapter {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    extra_headers: HeaderMap,
}

impl AnthropicMessagesAdapter {
    /// Create a new adapter.
    /// 创建适配器。
    pub fn new(config: ModelProviderConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            base_url: config
                .base_url
                .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
            api_key: config.api_key,
            extra_headers: header_map(config.extra_headers)?,
        })
    }
}

#[async_trait]
impl ChatModel for AnthropicMessagesAdapter {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        let body = build_messages_request(req)?;
        let url = endpoint_url(&self.base_url, "/messages");
        let response = self
            .client
            .post(url)
            .headers(self.extra_headers.clone())
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send Anthropic Messages request")?;
        let value = decode_json_response(response, "Anthropic Messages").await?;

        parse_messages_response(value)
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicMessagesAdapter {
    fn name(&self) -> &'static str {
        "anthropic-messages"
    }
}

fn build_messages_request(req: ModelRequest) -> Result<Value> {
    let mut max_tokens = req.max_tokens.unwrap_or(4096);

    let mut body = json!({
        "model": &req.model,
        "messages": messages_to_anthropic(&req.messages),
        "stream": false,
    });

    if let Some(system) = req.system {
        body["system"] = json!(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(tool_to_anthropic).collect());
        body["tool_choice"] = tool_choice_to_anthropic(&req.tool_choice);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }

    // 推理级别映射：Off 不添加 thinking 参数，其余按预算档位设置
    match req.reasoning {
        ReasoningLevel::Off => {}
        ReasoningLevel::Low => {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": 2048 });
            max_tokens = max_tokens.max(4096);
        }
        ReasoningLevel::Medium => {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": 8192 });
            max_tokens = max_tokens.max(12288);
        }
        ReasoningLevel::High => {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": 16384 });
            max_tokens = max_tokens.max(20480);
        }
    }

    body["max_tokens"] = json!(max_tokens);

    merge_provider_options(&mut body, req.provider_options);
    Ok(body)
}

fn messages_to_anthropic(messages: &[super::ModelMessage]) -> Value {
    let mut out = Vec::new();

    for message in messages {
        match &message.role {
            MessageRole::System => {
                // Anthropic uses a top-level system field. If a system message appears
                // inside the transcript, keep it as a user-visible note instead of dropping it.
                // Anthropic 使用顶层 system 字段。如果 transcript 内出现 system 消息，
                // 这里保留为普通 user note，避免静默丢弃。
                let text = message.text_content();
                if !text.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": format!("[system]\n{text}") }]
                    }));
                }
            }
            MessageRole::Developer => {
                let text = message.text_content();
                if !text.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": format!("[developer]\n{text}") }]
                    }));
                }
            }
            MessageRole::User => {
                let blocks = blocks_to_anthropic_content(&message.content, false);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
            }
            MessageRole::Assistant => {
                let blocks = blocks_to_anthropic_content(&message.content, false);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            MessageRole::Tool => {
                let blocks = blocks_to_anthropic_content(&message.content, true);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
            }
        }
    }

    Value::Array(out)
}

fn blocks_to_anthropic_content(blocks: &[ContentBlock], tool_result_mode: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !tool_result_mode => {
                out.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::ToolCall { call } if !tool_result_mode => {
                out.push(json!({
                    "type": "tool_use",
                    "id": &call.id,
                    "name": &call.name,
                    "input": &call.args,
                }));
            }
            ContentBlock::ToolResult { result } => {
                out.push(json!({
                    "type": "tool_result",
                    "tool_use_id": &result.call_id,
                    "content": result.content_text(),
                    "is_error": result.is_error,
                }));
            }
            ContentBlock::Image { media_type, data } if !tool_result_mode => {
                out.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    },
                }));
            }
            ContentBlock::File {
                name,
                media_type,
                data,
            } if !tool_result_mode => {
                // PDF 走原生 document 输入；其余类型降级为文本或占位说明。
                if media_type == "application/pdf" {
                    out.push(json!({
                        "type": "document",
                        "title": name,
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        },
                    }));
                } else {
                    out.push(json!({
                        "type": "text",
                        "text": file_attachment_fallback_text(name, media_type, data),
                    }));
                }
            }
            _ => {}
        }
    }
    out
}

fn tool_to_anthropic(tool: &ToolSpec) -> Value {
    json!({
        "name": &tool.name,
        "description": &tool.description,
        "input_schema": &tool.input_schema,
    })
}

fn tool_choice_to_anthropic(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
    }
}

fn parse_messages_response(value: Value) -> Result<ModelResponse> {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();

    if let Some(blocks) = value.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            content.push(ContentBlock::Text {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "thinking" => {
                    let thinking = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let signature = block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    if !thinking.is_empty() {
                        content.push(ContentBlock::Thinking {
                            thinking,
                            signature,
                        });
                    }
                }
                "tool_use" => {
                    let call = parse_tool_use(block)?;
                    content.push(ContentBlock::ToolCall { call: call.clone() });
                    tool_calls.push(call);
                }
                _ => {}
            }
        }
    }

    let stop_reason = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish_reason = match stop_reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::Refusal,
        _ if !tool_calls.is_empty() => FinishReason::ToolCalls,
        _ => FinishReason::Unknown,
    };

    let usage = value.get("usage").map(|usage| TokenUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        total_tokens: None,
    });
    let billing = ProviderBilling::from_provider_response(&value);

    Ok(ModelResponse {
        content,
        tool_calls,
        finish_reason,
        usage,
        billing,
        raw_provider_response: Some(value),
    })
}

fn parse_tool_use(block: &Value) -> Result<ToolCall> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Anthropic tool_use missing name"))?
        .to_string();
    let args = block.get("input").cloned().unwrap_or_else(|| json!({}));
    Ok(ToolCall::new(id, name, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 图片附件映射为 Anthropic base64 image 块。
    #[test]
    fn image_block_maps_to_base64_image() {
        let blocks = vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "aGVsbG8=".to_string(),
        }];

        let out = blocks_to_anthropic_content(&blocks, false);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "image");
        assert_eq!(out[0]["source"]["type"], "base64");
        assert_eq!(out[0]["source"]["media_type"], "image/png");
        assert_eq!(out[0]["source"]["data"], "aGVsbG8=");
    }

    /// PDF 附件映射为原生 document 块并携带标题。
    #[test]
    fn pdf_file_maps_to_document() {
        let blocks = vec![ContentBlock::File {
            name: "报告.pdf".to_string(),
            media_type: "application/pdf".to_string(),
            data: "cGRm".to_string(),
        }];

        let out = blocks_to_anthropic_content(&blocks, false);

        assert_eq!(out[0]["type"], "document");
        assert_eq!(out[0]["title"], "报告.pdf");
        assert_eq!(out[0]["source"]["media_type"], "application/pdf");
    }

    /// 文本附件降级为内联文本块，保留文件名和内容。
    #[test]
    fn text_file_is_inlined_as_text() {
        // "你好" 的 UTF-8 base64
        let blocks = vec![ContentBlock::File {
            name: "note.txt".to_string(),
            media_type: "text/plain".to_string(),
            data: "5L2g5aW9".to_string(),
        }];

        let out = blocks_to_anthropic_content(&blocks, false);

        assert_eq!(out[0]["type"], "text");
        let text = out[0]["text"].as_str().expect("应为文本");
        assert!(text.contains("note.txt"));
        assert!(text.contains("你好"));
    }

    /// 二进制附件不会被静默丢弃，而是生成占位说明。
    #[test]
    fn binary_file_becomes_placeholder_text() {
        let blocks = vec![ContentBlock::File {
            name: "app.bin".to_string(),
            media_type: "application/octet-stream".to_string(),
            data: "AAEC".to_string(),
        }];

        let out = blocks_to_anthropic_content(&blocks, false);

        assert_eq!(out[0]["type"], "text");
        let text = out[0]["text"].as_str().expect("应为文本");
        assert!(text.contains("app.bin"));
        assert!(text.contains("无法内联发送"));
    }
}
