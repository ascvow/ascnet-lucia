//! Google Generative AI（Gemini）generateContent 协议适配器。
//!
//! 把 provider-neutral 请求映射为 `models/{model}:generateContent` 请求体，
//! 并通过 `:streamGenerateContent?alt=sse` 提供原生 SSE 流式输出。

use super::support::{
    decode_json_response, endpoint_url, file_attachment_fallback_text, header_map,
};
use crate::model::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelEventStream, ModelProviderConfig,
    ModelRequest, ModelResponse, ModelStreamEvent, ProviderAdapter, ProviderBilling,
    ReasoningLevel, TokenUsage, ToolChoice,
};
use agent_tool::{ToolCall, ToolSpec};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_TYPE};
use serde_json::{json, Map, Value};
use uuid::Uuid;

/// Google Generative AI 官方 API base URL。
const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Google Generative AI（Gemini）generateContent 适配器。
pub struct GoogleGenerativeAdapter {
    client: reqwest::Client,
    /// 注册到网关的逻辑服务商名称，保留用于后续诊断与路由扩展。
    #[allow(dead_code)]
    provider_name: String,
    base_url: String,
    api_key: String,
    extra_headers: HeaderMap,
}

impl GoogleGenerativeAdapter {
    /// Create a new adapter.
    /// 创建适配器。
    pub fn new(config: ModelProviderConfig) -> Result<Self> {
        Ok(Self {
            client: super::support::http_client()?,
            provider_name: config.name,
            base_url: config
                .base_url
                .unwrap_or_else(|| DEFAULT_GOOGLE_BASE_URL.to_string()),
            api_key: config.api_key,
            extra_headers: header_map(config.extra_headers)?,
        })
    }

    /// 构造 generateContent 或 streamGenerateContent 的完整 URL。
    fn endpoint(&self, model: &str, method: &str) -> String {
        endpoint_url(&self.base_url, &format!("/models/{model}:{method}"))
    }

    /// 使用给定 method 发送一次生成请求并返回响应 JSON。
    async fn generate(&self, model: &str, method: &str, body: &Value) -> Result<reqwest::Response> {
        self.client
            .post(self.endpoint(model, method))
            .headers(self.extra_headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header("x-goog-api-key", self.api_key.as_str())
            .json(body)
            .send()
            .await
            .context("failed to send Google Generative AI request")
    }

    /// 完成一次非流式 generateContent 调用。
    async fn complete_once(&self, req: ModelRequest) -> Result<ModelResponse> {
        let (model, body) = build_generate_request(req)?;
        let response = self.generate(&model, "generateContent", &body).await?;
        let value = decode_json_response(response, "Google Generative AI").await?;
        parse_generate_response(value)
    }
}

#[async_trait]
impl ChatModel for GoogleGenerativeAdapter {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        self.complete_once(req).await
    }
}

#[async_trait]
impl ProviderAdapter for GoogleGenerativeAdapter {
    fn name(&self) -> &'static str {
        "google-generative"
    }

    async fn stream_for_provider(
        &self,
        _logical_provider: &str,
        req: ModelRequest,
    ) -> ModelEventStream {
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::Start);
        match self.stream_once(req, &tx).await {
            Ok(response) => tx.done(response),
            Err(error) => tx.fail(error),
        }
        stream
    }
}

impl GoogleGenerativeAdapter {
    /// 消费一次 SSE 流式响应，把增量推入事件通道并返回最终聚合响应。
    async fn stream_once(
        &self,
        req: ModelRequest,
        tx: &crate::model::ModelEventSender,
    ) -> Result<ModelResponse> {
        let (model, body) = build_generate_request(req)?;
        let response = self
            .generate(&model, "streamGenerateContent?alt=sse", &body)
            .await?;

        let status = response.status();
        if !status.is_success() {
            let url = response.url().clone();
            let text = super::support::read_response_text(response, "Google Generative AI", true)
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(super::support::http_status_error(status, format!(
                "Google Generative AI returned an error status (status: {status}, url: {url}, body preview: {})",
                super::support::response_body_preview(&text)
            )));
        }

        let mut text = String::new();
        let mut thinking = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut finish: Option<FinishReason> = None;
        let mut usage: Option<TokenUsage> = None;
        let mut raw: Vec<Value> = Vec::new();
        let mut blocked = false;

        let chunks = response
            .bytes_stream()
            .map(|chunk| chunk.context("failed to read Google SSE chunk"));
        super::support::for_each_sse_data(chunks, |payload| {
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                return Ok(());
            }
            let value =
                serde_json::from_str::<Value>(payload).context("invalid Google SSE JSON")?;
            if let Some(error) = value.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Google stream failed");
                let message = super::support::response_body_preview(message);
                if let Some(status) = error
                    .get("code")
                    .and_then(Value::as_u64)
                    .and_then(|code| u16::try_from(code).ok())
                    .and_then(|code| reqwest::StatusCode::from_u16(code).ok())
                {
                    return Err(super::support::http_status_error(status, message));
                }
                anyhow::bail!(message);
            }
            blocked |= value.pointer("/promptFeedback/blockReason").is_some();
            if let Some(chunk_usage) = parse_usage(&value) {
                usage = Some(chunk_usage);
            }
            if let Some(reason) = parse_finish_reason(&value) {
                finish = Some(reason);
            }
            for event in parse_stream_parts(&value, &mut text, &mut thinking, &mut calls) {
                tx.send(event);
            }
            raw.push(value);
            Ok(())
        })
        .await?;

        // EOF 不是成功终态；服务商明确拦截则报永久错误，避免重放安全拒绝。
        if blocked {
            anyhow::bail!("Google prompt was blocked by the provider");
        }
        if finish.is_none() {
            return Err(crate::model::TransientModelError::new(
                "Google stream ended without finishReason",
            )
            .into());
        }
        let response = assemble_response(text, thinking, calls, finish, usage, raw);
        Ok(response)
    }
}

/// 把 provider-neutral 请求构建为 generateContent 请求体。
///
/// 返回目标模型 ID 与请求体二元组，模型 ID 由适配器用于拼接 endpoint。
pub(crate) fn build_generate_request(mut req: ModelRequest) -> Result<(String, Value)> {
    let model = std::mem::take(&mut req.model);
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = req.system.take() {
        system_parts.push(system);
    }

    let mut contents: Vec<Value> = Vec::new();
    for message in &req.messages {
        match message.role {
            MessageRole::System | MessageRole::Developer => {
                // 系统与开发者指令统一进入 systemInstruction，保持与官方语义一致。
                system_parts.push(message.text_content());
            }
            MessageRole::User => {
                contents.push(json!({
                    "role": "user",
                    "parts": user_parts(&message.content)?,
                }));
            }
            MessageRole::Assistant => {
                contents.push(json!({
                    "role": "model",
                    "parts": assistant_parts(&message.content),
                }));
            }
            MessageRole::Tool => {
                // Google 把工具结果放在 user 角色的 functionResponse part 里。
                let mut parts = Vec::new();
                for block in &message.content {
                    if let ContentBlock::ToolResult { result } = block {
                        parts.push(function_response_part(
                            result.name.clone(),
                            &result.content,
                            result.is_error,
                        ));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(contents));
    let system_text = system_parts
        .iter()
        .filter(|part| !part.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if !system_text.is_empty() {
        body.insert(
            "systemInstruction".to_string(),
            json!({ "parts": [{ "text": system_text }] }),
        );
    }
    if !req.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            json!([{
                "functionDeclarations": req.tools.iter().map(tool_to_google).collect::<Vec<_>>(),
            }]),
        );
    }
    if let Some(config) = function_calling_config(&req.tool_choice) {
        body.insert(
            "toolConfig".to_string(),
            json!({ "functionCallingConfig": config }),
        );
    }

    let mut generation = Map::new();
    if let Some(max_tokens) = req.max_tokens {
        generation.insert("maxOutputTokens".to_string(), Value::from(max_tokens));
    }
    if let Some(temperature) = req.temperature {
        generation.insert("temperature".to_string(), Value::from(temperature));
    }
    if let Some(thinking) = thinking_config(req.reasoning) {
        generation.insert("thinkingConfig".to_string(), thinking);
    }
    if !generation.is_empty() {
        body.insert("generationConfig".to_string(), Value::Object(generation));
    }

    merge_provider_options_into(&mut body, req.provider_options);
    // maxOutputTokens 与预算收口放最后，避免 provider_options 绕过宿主预算。
    if let Some(max_tokens) = req.max_tokens {
        if let Some(config) = body
            .get_mut("generationConfig")
            .and_then(Value::as_object_mut)
        {
            config.insert("maxOutputTokens".to_string(), Value::from(max_tokens));
        }
    }

    let value = Value::Object(body);
    Ok((model, value))
}

/// 浅合并 `provider_options`，仅接受对象类型。
fn merge_provider_options_into(body: &mut Map<String, Value>, provider_options: Value) {
    if let Value::Object(extra) = provider_options {
        for (key, value) in extra {
            body.insert(key, value);
        }
    }
}

/// 把用户消息内容块映射为 Google parts。
fn user_parts(blocks: &[ContentBlock]) -> Result<Vec<Value>> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(json!({ "text": text })),
            ContentBlock::Image { media_type, data } => {
                parts.push(json!({
                    "inlineData": { "mimeType": media_type, "data": data },
                }));
            }
            ContentBlock::File {
                name,
                media_type,
                data,
            } => {
                if media_type == "application/pdf" {
                    parts.push(json!({
                        "inlineData": { "mimeType": media_type, "data": data },
                    }));
                } else {
                    parts.push(
                        json!({ "text": file_attachment_fallback_text(name, media_type, data) }),
                    );
                }
            }
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => {}
            ContentBlock::ToolResult { result } => {
                parts.push(function_response_part(
                    result.name.clone(),
                    &result.content,
                    result.is_error,
                ));
            }
        }
    }
    Ok(parts)
}

/// 把 assistant 消息内容块映射为 Google parts；签名推理块不跨协议回传。
fn assistant_parts(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(json!({ "text": text })),
            ContentBlock::ToolCall { call } => parts.push(json!({
                "functionCall": { "name": call.name, "args": call.args },
            })),
            ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                // 思维链文本可以回传为带 thought 标记的 part；签名仅对来源协议有效。
                parts.push(json!({ "text": thinking, "thought": true }));
            }
            _ => {}
        }
    }
    parts
}

/// 构造工具结果对应的 functionResponse part。
///
/// Google 要求 `response` 是 JSON 对象：对象载荷原样使用，其余类型包装为 `result`；
/// 失败结果附加 `error` 标记，保证模型能感知工具失败。
fn function_response_part(name: String, content: &Value, is_error: bool) -> Value {
    let mut response = match content {
        Value::Object(map) => map.clone(),
        other => Map::from_iter([("result".to_string(), other.clone())]),
    };
    if is_error {
        response.insert("error".to_string(), Value::Bool(true));
    }
    json!({
        "functionResponse": { "name": name, "response": response },
    })
}

/// 把工具定义映射为 functionDeclaration。
fn tool_to_google(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
    })
}

/// 把中立工具选择策略映射为 functionCallingConfig；Auto 缺省时不发送该字段。
fn function_calling_config(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::None => Some(json!({ "mode": "NONE" })),
        ToolChoice::Required => Some(json!({ "mode": "ANY" })),
        ToolChoice::Tool { name } => Some(json!({
            "mode": "ANY",
            "allowedFunctionNames": [name],
        })),
    }
}

/// 把推理等级映射为 thinkingConfig；关闭时保持缺省以兼容不支持 thinking 的模型。
fn thinking_config(level: ReasoningLevel) -> Option<Value> {
    let budget: i32 = match level {
        ReasoningLevel::Off => return None,
        ReasoningLevel::Minimal => 512,
        ReasoningLevel::Low => 1_024,
        ReasoningLevel::Medium => 8_192,
        ReasoningLevel::High => 16_384,
        ReasoningLevel::XHigh => 24_576,
        ReasoningLevel::Max => -1,
    };
    Some(json!({ "thinkingBudget": budget, "includeThoughts": true }))
}

/// 解析响应中的 usageMetadata。
pub(crate) fn parse_usage(value: &Value) -> Option<TokenUsage> {
    let usage = value.get("usageMetadata")?;
    Some(TokenUsage {
        input_tokens: usage.get("promptTokenCount").and_then(Value::as_u64),
        output_tokens: usage.get("candidatesTokenCount").and_then(Value::as_u64),
        total_tokens: usage.get("totalTokenCount").and_then(Value::as_u64),
        cache_read_tokens: usage.get("cachedContentTokenCount").and_then(Value::as_u64),
        cache_write_tokens: None,
    })
}

/// 解析响应中的 finishReason 并映射为中立停止原因。
pub(crate) fn parse_finish_reason(value: &Value) -> Option<FinishReason> {
    let reason = value
        .get("candidates")?
        .as_array()?
        .first()?
        .get("finishReason")?
        .as_str()?;
    Some(match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "SPII" | "BLOCKLIST" => {
            FinishReason::Refusal
        }
        "MALFORMED_FUNCTION_CALL" => FinishReason::Error,
        _ => FinishReason::Unknown,
    })
}

/// 处理一个流式分片的 parts：聚合文本并生成流式事件。
fn parse_stream_parts(
    value: &Value,
    text: &mut String,
    thinking: &mut String,
    calls: &mut Vec<ToolCall>,
) -> Vec<ModelStreamEvent> {
    let mut events = Vec::new();
    let Some(parts) = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    else {
        return events;
    };
    for part in parts {
        if let Some(call) = part.get("functionCall") {
            let call = ToolCall::new(
                format!("google-call-{}", Uuid::new_v4()),
                call.get("name").and_then(Value::as_str).unwrap_or_default(),
                call.get("args").cloned().unwrap_or_else(|| json!({})),
            );
            events.push(ModelStreamEvent::ToolCallEnd {
                index: calls.len(),
                call: call.clone(),
            });
            calls.push(call);
            continue;
        }
        let Some(chunk) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        if chunk.is_empty() {
            continue;
        }
        if part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            thinking.push_str(chunk);
            events.push(ModelStreamEvent::ThinkingDelta {
                index: 0,
                delta: chunk.to_string(),
            });
        } else {
            text.push_str(chunk);
            events.push(ModelStreamEvent::TextDelta {
                index: 0,
                delta: chunk.to_string(),
            });
        }
    }
    events
}

/// 把聚合结果组装为最终 provider-neutral 响应。
fn assemble_response(
    text: String,
    thinking: String,
    calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    usage: Option<TokenUsage>,
    raw: Vec<Value>,
) -> ModelResponse {
    let mut content = Vec::new();
    if !thinking.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking,
            signature: None,
        });
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    content.extend(
        calls
            .iter()
            .cloned()
            .map(|call| ContentBlock::ToolCall { call }),
    );
    let finish_reason = if !calls.is_empty() {
        FinishReason::ToolCalls
    } else {
        finish.unwrap_or(FinishReason::Stop)
    };
    let raw_provider_response = if raw.is_empty() {
        None
    } else {
        Some(Value::Array(raw))
    };
    ModelResponse {
        content,
        tool_calls: calls,
        finish_reason,
        usage,
        billing: raw_provider_response
            .as_ref()
            .and_then(ProviderBilling::from_provider_response),
        raw_provider_response,
    }
}

/// 解析一次完整 generateContent 响应。
pub(crate) fn parse_generate_response(value: Value) -> Result<ModelResponse> {
    if let Some(error) = value.get("error") {
        anyhow::bail!(
            "Google Generative AI returned an error: {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        );
    }

    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .cloned()
        .unwrap_or(Value::Null);

    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    if let Some(parts) = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(call) = part.get("functionCall") {
                calls.push(ToolCall::new(
                    format!("google-call-{}", Uuid::new_v4()),
                    call.get("name").and_then(Value::as_str).unwrap_or_default(),
                    call.get("args").cloned().unwrap_or_else(|| json!({})),
                ));
            } else if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                if part
                    .get("thought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    thinking.push_str(chunk);
                } else {
                    text.push_str(chunk);
                }
            }
        }
    }

    let finish = parse_finish_reason(&value);
    let usage = parse_usage(&value);
    Ok(assemble_response(
        text,
        thinking,
        calls,
        finish,
        usage,
        vec![value],
    ))
}

#[cfg(test)]
mod tests;
