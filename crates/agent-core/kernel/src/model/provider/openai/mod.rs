//! OpenAI protocol adapters.
//! OpenAI 协议适配器。
//!
//! 本模块提供两个适配器：
//! 1. Responses API：面向新 agent 的主要 OpenAI 协议。
//! 2. Chat Completions API：适合本地模型网关等 OpenAI-compatible 服务。
//!
//! 参数适配策略参考 LiteLLM：为每种 provider 维护一份"支持的参数白名单"，
//! 构建请求时始终使用标准 Responses API 字段名，发送前按白名单过滤，
//! 不在白名单中的参数静默丢弃。用户可通过 `provider_options` 手动注入
//! provider 特有参数（在过滤之后合并，不受白名单约束）。

mod stream;

use self::stream::{failed_model_stream, spawn_openai_stream, OpenAiStreamProtocol};
use super::support::{
    decode_json_response, endpoint_url, file_attachment_fallback_text, header_map,
    merge_provider_options,
};
use crate::model::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelEventStream, ModelProviderConfig,
    ModelRequest, ModelResponse, ProviderAdapter, ProviderBilling, ProviderKind, ReasoningLevel,
    TokenUsage, ToolChoice,
};
use agent_tool::{ToolCall, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use uuid::Uuid;

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Codex Desktop 请求使用的客户端标识，用于兼容依赖官方客户端身份的模型网关。
const CODEX_DESKTOP_USER_AGENT: &str =
    "Codex Desktop/0.144.0-alpha.4 (Mac OS 26.5.2; arm64) unknown (Codex Desktop; 26.707.31428)";

// ---------------------------------------------------------------------------
// Responses API 参数白名单（参考 LiteLLM get_supported_openai_params）
// ---------------------------------------------------------------------------

/// 官方 OpenAI Responses API 支持的全部顶层参数。
const RESPONSES_PARAMS_FULL: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "max_output_tokens",
    "temperature",
    "top_p",
    "stream",
    "reasoning",
    "metadata",
    "store",
    "truncation",
    "user",
    "text",
    "include",
    "parallel_tool_calls",
    "previous_response_id",
];

/// 兼容网关 Responses API 端点通常支持的核心参数子集。
/// 来源：LiteLLM `LiteLLMCompletionResponsesConfig.get_supported_openai_params`。
const RESPONSES_PARAMS_COMPATIBLE: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "stream",
    "reasoning",
    "metadata",
    "user",
    "text",
    "parallel_tool_calls",
];

/// 按白名单过滤请求体，移除 provider 不支持的顶层参数。
fn filter_params(body: &mut Value, allowed: &[&str]) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let dropped: Vec<String> = obj
        .keys()
        .filter(|k| !allowed.contains(&k.as_str()))
        .cloned()
        .collect();
    if !dropped.is_empty() {
        tracing::debug!(
            params = ?dropped,
            "丢弃 provider 不支持的 Responses API 参数"
        );
        for key in &dropped {
            obj.remove(key);
        }
    }
}

/// 根据 ProviderKind 和 base URL 选择参数白名单。
fn responses_allowed_params(kind: &ProviderKind, base_url: &str) -> &'static [&'static str] {
    match kind {
        ProviderKind::OpenAi if is_official_openai_url(base_url) => RESPONSES_PARAMS_FULL,
        _ => RESPONSES_PARAMS_COMPATIBLE,
    }
}

/// 判断 base URL 是否指向官方 OpenAI API。
fn is_official_openai_url(base_url: &str) -> bool {
    base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .starts_with("api.openai.com")
}

// ---------------------------------------------------------------------------
// OpenAI Responses API adapter
// ---------------------------------------------------------------------------

/// OpenAI Responses API 适配器。
pub struct OpenAiResponsesAdapter {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    extra_headers: HeaderMap,
    /// 当前 provider 支持的 Responses API 参数白名单。
    allowed_params: &'static [&'static str],
}

impl OpenAiResponsesAdapter {
    /// 创建适配器。
    pub fn new(config: ModelProviderConfig) -> Result<Self> {
        let base_url = normalize_openai_base_url(config.base_url)?;
        let allowed_params = responses_allowed_params(&config.kind, &base_url);
        Ok(Self {
            client: reqwest::Client::new(),
            base_url,
            api_key: config.api_key,
            extra_headers: header_map(config.extra_headers)?,
            allowed_params,
        })
    }
}

#[async_trait]
impl ChatModel for OpenAiResponsesAdapter {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        let body = build_responses_request(req, self.allowed_params)?;
        let url = endpoint_url(&self.base_url, "/responses");
        let response = self
            .client
            .post(url)
            .headers(self.extra_headers.clone())
            .header(USER_AGENT, CODEX_DESKTOP_USER_AGENT)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send OpenAI Responses request")?;
        let value = decode_json_response(response, "OpenAI Responses").await?;

        parse_responses_response(value)
    }

    /// 通过 Responses API SSE 实时返回文本、推理和工具调用增量。
    async fn stream(&self, req: ModelRequest) -> ModelEventStream {
        let mut body = match build_responses_request(req, self.allowed_params) {
            Ok(body) => body,
            Err(error) => return failed_model_stream(error),
        };
        body["stream"] = json!(true);
        spawn_openai_stream(
            self.client.clone(),
            endpoint_url(&self.base_url, "/responses"),
            self.api_key.clone(),
            self.extra_headers.clone(),
            body,
            OpenAiStreamProtocol::Responses,
        )
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiResponsesAdapter {
    fn name(&self) -> &'static str {
        "openai-responses"
    }
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions API adapter
// ---------------------------------------------------------------------------

/// OpenAI Chat Completions API 适配器。
pub struct OpenAiChatCompletionsAdapter {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    extra_headers: HeaderMap,
}

impl OpenAiChatCompletionsAdapter {
    /// 创建适配器。
    pub fn new(config: ModelProviderConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            base_url: normalize_openai_base_url(config.base_url)?,
            api_key: config.api_key,
            extra_headers: header_map(config.extra_headers)?,
        })
    }
}

#[async_trait]
impl ChatModel for OpenAiChatCompletionsAdapter {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        let body = build_chat_completions_request(req)?;
        let url = endpoint_url(&self.base_url, "/chat/completions");
        let response = self
            .client
            .post(url)
            .headers(self.extra_headers.clone())
            .header(USER_AGENT, CODEX_DESKTOP_USER_AGENT)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send OpenAI Chat Completions request")?;
        let value = decode_json_response(response, "OpenAI Chat Completions").await?;

        parse_chat_completions_response(value)
    }

    /// 通过 Chat Completions SSE 实时返回文本和工具调用增量。
    async fn stream(&self, req: ModelRequest) -> ModelEventStream {
        let mut body = match build_chat_completions_request(req) {
            Ok(body) => body,
            Err(error) => return failed_model_stream(error),
        };
        body["stream"] = json!(true);
        spawn_openai_stream(
            self.client.clone(),
            endpoint_url(&self.base_url, "/chat/completions"),
            self.api_key.clone(),
            self.extra_headers.clone(),
            body,
            OpenAiStreamProtocol::ChatCompletions,
        )
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiChatCompletionsAdapter {
    fn name(&self) -> &'static str {
        "openai-chat-completions"
    }
}

// ---------------------------------------------------------------------------
// 请求构建
// ---------------------------------------------------------------------------

/// 构建 Responses API 请求体。
///
/// 始终使用标准 Responses API 字段名（如 `max_output_tokens`），
/// 然后按 `allowed_params` 白名单过滤，最后合并 `provider_options`。
/// 过滤在 `provider_options` 合并之前执行，因此用户可通过
/// `provider_options` 注入任意 provider 特有参数，不受白名单约束。
fn build_responses_request(req: ModelRequest, allowed_params: &[&str]) -> Result<Value> {
    let mut body = json!({
        "model": &req.model,
        "input": messages_to_responses_input(&req.messages)?,
        "stream": false,
    });

    if let Some(system) = req.system {
        body["instructions"] = json!(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(tool_to_openai_responses).collect());
        body["tool_choice"] = tool_choice_to_openai_responses(&req.tool_choice);
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }

    // 推理级别映射：Off 不添加字段，其余映射为 OpenAI reasoning.effort
    match req.reasoning {
        ReasoningLevel::Off => {}
        ReasoningLevel::Low => {
            body["reasoning"] = json!({ "effort": "low" });
        }
        ReasoningLevel::Medium => {
            body["reasoning"] = json!({ "effort": "medium" });
        }
        ReasoningLevel::High => {
            body["reasoning"] = json!({ "effort": "high" });
        }
    }

    // 按白名单过滤不支持的参数
    filter_params(&mut body, allowed_params);

    // provider_options 在过滤之后合并，不受白名单约束
    merge_provider_options(&mut body, req.provider_options);
    Ok(body)
}

fn build_chat_completions_request(req: ModelRequest) -> Result<Value> {
    let mut messages = Vec::new();

    if let Some(system) = req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }

    for message in &req.messages {
        messages.extend(message_to_openai_chat_messages(message)?);
    }

    let mut body = json!({
        "model": &req.model,
        "messages": messages,
    });

    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(tool_to_openai_chat).collect());
        body["tool_choice"] = tool_choice_to_openai_chat(&req.tool_choice);
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }

    merge_provider_options(&mut body, req.provider_options);
    Ok(body)
}

// ---------------------------------------------------------------------------
// 消息转换
// ---------------------------------------------------------------------------

fn messages_to_responses_input(messages: &[crate::model::ModelMessage]) -> Result<Value> {
    let mut input = Vec::new();

    for message in messages {
        match &message.role {
            MessageRole::System => {
                push_text_input(&mut input, "system", message.text_content());
            }
            MessageRole::Developer => {
                push_text_input(&mut input, "developer", message.text_content());
            }
            MessageRole::User => {
                let parts = user_blocks_to_responses_content(&message.content);
                if !parts.is_empty() {
                    input.push(json!({ "role": "user", "content": parts }));
                }
            }
            MessageRole::Assistant => {
                let text = message.text_content();
                if !text.is_empty() {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }]
                    }));
                }
                for block in &message.content {
                    if let ContentBlock::ToolCall { call } = block {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": &call.id,
                            "name": &call.name,
                            "arguments": call.args_json_string(),
                        }));
                    }
                }
            }
            MessageRole::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult { result } = block {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": &result.call_id,
                            "output": result.content_text(),
                        }));
                    }
                }
            }
        }
    }

    Ok(Value::Array(input))
}

fn push_text_input(input: &mut Vec<Value>, role: &str, text: String) {
    if !text.is_empty() {
        input.push(json!({
            "role": role,
            "content": [{ "type": "input_text", "text": text }]
        }));
    }
}

/// 将用户消息内容块转换为 Responses API 的 `input` 内容部件。
///
/// 图片映射为 data URL 形式的 `input_image`，PDF 走原生 `input_file`，
/// 其余文件类型降级为内联文本或占位说明。
fn user_blocks_to_responses_content(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                parts.push(json!({ "type": "input_text", "text": text }));
            }
            ContentBlock::Image { media_type, data } => {
                parts.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                }));
            }
            ContentBlock::File {
                name,
                media_type,
                data,
            } => {
                if media_type == "application/pdf" {
                    parts.push(json!({
                        "type": "input_file",
                        "filename": name,
                        "file_data": format!("data:{media_type};base64,{data}"),
                    }));
                } else {
                    parts.push(json!({
                        "type": "input_text",
                        "text": file_attachment_fallback_text(name, media_type, data),
                    }));
                }
            }
            _ => {}
        }
    }
    parts
}

/// 将用户消息内容块转换为 Chat Completions 的 content 部件数组。
///
/// 与 Responses 版本的差异只在字段命名：图片使用 `image_url` 对象，PDF 使用
/// `file` 部件（官方 OpenAI 支持），其余文件类型降级为内联文本或占位说明。
fn user_blocks_to_chat_content(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                parts.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::Image { media_type, data } => {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{data}") },
                }));
            }
            ContentBlock::File {
                name,
                media_type,
                data,
            } => {
                if media_type == "application/pdf" {
                    parts.push(json!({
                        "type": "file",
                        "file": {
                            "filename": name,
                            "file_data": format!("data:{media_type};base64,{data}"),
                        },
                    }));
                } else {
                    parts.push(json!({
                        "type": "text",
                        "text": file_attachment_fallback_text(name, media_type, data),
                    }));
                }
            }
            _ => {}
        }
    }
    parts
}

fn message_to_openai_chat_messages(message: &crate::model::ModelMessage) -> Result<Vec<Value>> {
    let mut out = Vec::new();

    match &message.role {
        MessageRole::System | MessageRole::Developer => {
            let text = message.text_content();
            if !text.is_empty() {
                out.push(json!({
                    "role": message.role.as_str(),
                    "content": text,
                }));
            }
        }
        MessageRole::User => {
            let has_attachment = message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Image { .. } | ContentBlock::File { .. }
                )
            });
            if has_attachment {
                let parts = user_blocks_to_chat_content(&message.content);
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            } else {
                // 纯文本保持字符串 content，兼容不支持数组 content 的网关。
                let text = message.text_content();
                if !text.is_empty() {
                    out.push(json!({ "role": "user", "content": text }));
                }
            }
        }
        MessageRole::Assistant => {
            let text = message.text_content();
            let tool_calls = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { call } => Some(json!({
                        "id": &call.id,
                        "type": "function",
                        "function": {
                            "name": &call.name,
                            "arguments": call.args_json_string(),
                        }
                    })),
                    _ => None,
                })
                .collect::<Vec<_>>();

            let mut msg = json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::String(text) },
            });
            if !tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(tool_calls);
            }
            out.push(msg);
        }
        MessageRole::Tool => {
            for block in &message.content {
                if let ContentBlock::ToolResult { result } = block {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": &result.call_id,
                        "content": result.content_text(),
                    }));
                }
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// 工具转换
// ---------------------------------------------------------------------------

fn tool_to_openai_responses(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": &tool.name,
        "description": &tool.description,
        "parameters": &tool.input_schema,
    })
}

fn tool_to_openai_chat(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": &tool.name,
            "description": &tool.description,
            "parameters": &tool.input_schema,
        }
    })
}

fn tool_choice_to_openai_responses(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({ "type": "function", "name": name }),
    }
}

fn tool_choice_to_openai_chat(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({
            "type": "function",
            "function": { "name": name }
        }),
    }
}

// ---------------------------------------------------------------------------
// 响应解析
// ---------------------------------------------------------------------------

fn parse_responses_response(value: Value) -> Result<ModelResponse> {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();

    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
            match item_type {
                "message" => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            let part_type =
                                part.get("type").and_then(Value::as_str).unwrap_or_default();
                            if matches!(part_type, "output_text" | "text") {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        content.push(ContentBlock::Text {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let call = parse_openai_function_call_item(item)?;
                    content.push(ContentBlock::ToolCall { call: call.clone() });
                    tool_calls.push(call);
                }
                _ => {}
            }
        }
    }

    let finish_reason = if !tool_calls.is_empty() {
        FinishReason::ToolCalls
    } else {
        match value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "completed" => FinishReason::Stop,
            "incomplete" => FinishReason::Length,
            _ => FinishReason::Unknown,
        }
    };

    let usage = parse_openai_usage(value.get("usage"));
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

fn parse_openai_function_call_item(item: &Value) -> Result<ToolCall> {
    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenAI function_call item missing name"))?
        .to_string();
    let args_json = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let args = serde_json::from_str(args_json).unwrap_or_else(|_| json!({ "_raw": args_json }));
    Ok(ToolCall::new(id, name, args))
}

fn parse_chat_completions_response(value: Value) -> Result<ModelResponse> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| anyhow!("OpenAI Chat Completions response missing choices[0]"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| anyhow!("OpenAI Chat Completions response missing message"))?;

    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call_value in calls {
            let id = call_value
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let function = call_value
                .get("function")
                .ok_or_else(|| anyhow!("OpenAI tool_call missing function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("OpenAI tool_call missing function.name"))?
                .to_string();
            let args_json = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let args =
                serde_json::from_str(args_json).unwrap_or_else(|_| json!({ "_raw": args_json }));
            let call = ToolCall::new(id, name, args);
            content.push(ContentBlock::ToolCall { call: call.clone() });
            tool_calls.push(call);
        }
    }

    let finish_reason = match choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::Refusal,
        _ if !tool_calls.is_empty() => FinishReason::ToolCalls,
        _ => FinishReason::Unknown,
    };

    let usage = parse_openai_usage(value.get("usage"));
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

fn parse_openai_usage(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?;
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

/// 规范化 OpenAI base URL；未包含 `/v1` 的地址会自动追加该版本前缀。
fn normalize_openai_base_url(base_url: Option<String>) -> Result<String> {
    let base_url = base_url.unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
    let base_url = base_url.trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&base_url)
        .with_context(|| format!("invalid OpenAI base URL: {base_url}"))?;

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(anyhow!(
            "OpenAI base URL must not include query or fragment"
        ));
    }

    let path = parsed.path().trim_end_matches('/');
    if path == "/v1" || path.ends_with("/v1") {
        Ok(base_url)
    } else {
        Ok(format!("{base_url}/v1"))
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests;
