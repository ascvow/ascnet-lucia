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

use super::util::{
    decode_json_response, endpoint_url, ensure_output_text, header_map, merge_provider_options,
};
use super::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelEventSender, ModelEventStream,
    ModelProviderConfig, ModelRequest, ModelResponse, ModelStreamEvent, ProviderAdapter,
    ProviderBilling, ProviderKind, ReasoningLevel, TokenUsage, ToolChoice,
};
use agent_tool::{ToolCall, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use uuid::Uuid;

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

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

/// OpenAI 流式协议类型，用于选择对应的 SSE 聚合器。
#[derive(Clone, Copy)]
enum OpenAiStreamProtocol {
    Responses,
    ChatCompletions,
}

/// 创建一个立即返回错误终止事件的模型流。
fn failed_model_stream(error: anyhow::Error) -> ModelEventStream {
    let (sender, stream) = ModelEventStream::channel();
    sender.error(format!("{error:#}"));
    stream
}

/// 在后台发送 OpenAI 流式请求，并将 SSE 聚合为最终模型响应。
fn spawn_openai_stream(
    client: reqwest::Client,
    url: String,
    api_key: String,
    headers: HeaderMap,
    body: Value,
    protocol: OpenAiStreamProtocol,
) -> ModelEventStream {
    let (sender, stream) = ModelEventStream::channel();
    tokio::spawn(async move {
        sender.send(ModelStreamEvent::Start);
        let result = async {
            let response = client
                .post(url)
                .headers(headers)
                .header(AUTHORIZATION, format!("Bearer {api_key}"))
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .context("failed to send OpenAI streaming request")?;
            let response = require_successful_stream_response(response).await?;
            match protocol {
                OpenAiStreamProtocol::Responses => {
                    consume_responses_stream(response, &sender).await
                }
                OpenAiStreamProtocol::ChatCompletions => {
                    consume_chat_completions_stream(response, &sender).await
                }
            }
        }
        .await;

        match result {
            Ok(response) => sender.done(response),
            Err(error) => sender.error(format!("{error:#}")),
        }
    });
    stream
}

/// 检查流式请求状态码，并在失败时保留服务商返回的错误正文。
async fn require_successful_stream_response(
    response: reqwest::Response,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!(
        "OpenAI streaming request failed with status {status}: {body}"
    ))
}

/// 按 SSE 行边界增量读取响应，避免网络分块截断 UTF-8 字符或事件。
async fn for_each_sse_data<F>(response: reqwest::Response, mut handler: F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let mut bytes = response.bytes_stream();
    let mut pending = Vec::new();
    let mut data_lines = Vec::new();

    while let Some(chunk) = bytes.next().await {
        pending.extend_from_slice(&chunk.context("failed to read OpenAI SSE chunk")?);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            process_sse_line(&line, &mut data_lines, &mut handler)?;
        }
    }

    if !pending.is_empty() {
        process_sse_line(&pending, &mut data_lines, &mut handler)?;
    }
    dispatch_sse_data(&mut data_lines, &mut handler)
}

/// 处理一行 SSE 内容，并在空行处提交完整 data 事件。
fn process_sse_line<F>(line: &[u8], data_lines: &mut Vec<String>, handler: &mut F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    if line.is_empty() {
        return dispatch_sse_data(data_lines, handler);
    }
    if let Some(data) = line.strip_prefix(b"data:") {
        let data = data.strip_prefix(b" ").unwrap_or(data);
        data_lines.push(String::from_utf8(data.to_vec()).context("OpenAI SSE data 不是 UTF-8")?);
    }
    Ok(())
}

/// 合并 SSE 的多行 data 字段并交给协议解析器。
fn dispatch_sse_data<F>(data_lines: &mut Vec<String>, handler: &mut F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    if data_lines.is_empty() {
        return Ok(());
    }
    let data = data_lines.join("\n");
    data_lines.clear();
    handler(&data)
}

/// Responses API SSE 的聚合状态。
#[derive(Default)]
struct ResponsesStreamState {
    text: String,
    function_items: Vec<Value>,
    completed_response: Option<Value>,
}

/// 消费 Responses API SSE，实时发送增量并返回完整响应。
async fn consume_responses_stream(
    response: reqwest::Response,
    sender: &ModelEventSender,
) -> Result<ModelResponse> {
    let mut state = ResponsesStreamState::default();
    for_each_sse_data(response, |data| {
        handle_responses_sse_data(data, sender, &mut state)
    })
    .await?;

    if let Some(mut response) = state.completed_response {
        ensure_output_text(&mut response, &state.text);
        return parse_responses_response(response);
    }

    let mut output = Vec::new();
    if !state.text.is_empty() {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": state.text }]
        }));
    }
    output.extend(state.function_items);
    parse_responses_response(json!({ "status": "completed", "output": output }))
}

/// 解析一条 Responses API SSE data 事件并更新聚合状态。
fn handle_responses_sse_data(
    data: &str,
    sender: &ModelEventSender,
    state: &mut ResponsesStreamState,
) -> Result<()> {
    if data == "[DONE]" {
        return Ok(());
    }
    let event: Value =
        serde_json::from_str(data).context("failed to parse OpenAI Responses SSE")?;
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "response.output_text.delta" => {
            let delta = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !delta.is_empty() {
                state.text.push_str(delta);
                sender.send(ModelStreamEvent::TextDelta {
                    index: event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize,
                    delta: delta.to_string(),
                });
            }
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            let delta = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !delta.is_empty() {
                sender.send(ModelStreamEvent::ThinkingDelta {
                    index: event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize,
                    delta: delta.to_string(),
                });
            }
        }
        "response.function_call_arguments.delta" => {
            let delta = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            sender.send(ModelStreamEvent::ToolCallDelta {
                index: event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize,
                delta: delta.to_string(),
            });
        }
        "response.output_item.done" => {
            if let Some(item) = event
                .get("item")
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            {
                let call = parse_openai_function_call_item(item)?;
                sender.send(ModelStreamEvent::ToolCallEnd {
                    index: event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize,
                    call,
                });
                state.function_items.push(item.clone());
            }
        }
        "response.completed" => {
            state.completed_response = event.get("response").cloned();
        }
        "error" | "response.failed" => {
            let message = event
                .pointer("/error/message")
                .or_else(|| event.pointer("/response/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Responses stream failed");
            return Err(anyhow!(message.to_string()));
        }
        _ => {}
    }
    Ok(())
}

/// Chat Completions 工具调用的流式聚合状态。
#[derive(Default)]
struct ChatToolStreamState {
    id: String,
    name: String,
    arguments: String,
}

/// Chat Completions SSE 的聚合状态。
#[derive(Default)]
struct ChatCompletionsStreamState {
    text: String,
    tools: Vec<ChatToolStreamState>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

/// 消费 Chat Completions SSE，实时发送增量并返回完整响应。
async fn consume_chat_completions_stream(
    response: reqwest::Response,
    sender: &ModelEventSender,
) -> Result<ModelResponse> {
    let mut state = ChatCompletionsStreamState::default();
    for_each_sse_data(response, |data| {
        handle_chat_completions_sse_data(data, sender, &mut state)
    })
    .await?;

    let mut tool_values = Vec::new();
    for (index, tool) in state.tools.into_iter().enumerate() {
        let id = if tool.id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            tool.id
        };
        let arguments = if tool.arguments.is_empty() {
            "{}".to_string()
        } else {
            tool.arguments
        };
        let args =
            serde_json::from_str(&arguments).unwrap_or_else(|_| json!({ "_raw": arguments }));
        sender.send(ModelStreamEvent::ToolCallEnd {
            index,
            call: ToolCall::new(id.clone(), tool.name.clone(), args),
        });
        tool_values.push(json!({
            "id": id,
            "type": "function",
            "function": { "name": tool.name, "arguments": arguments }
        }));
    }

    let finish_reason = state.finish_reason.unwrap_or_else(|| {
        if tool_values.is_empty() {
            "stop".to_string()
        } else {
            "tool_calls".to_string()
        }
    });
    let mut message = json!({ "role": "assistant", "content": state.text });
    if !tool_values.is_empty() {
        message["tool_calls"] = Value::Array(tool_values);
    }
    let mut value = json!({
        "choices": [{ "message": message, "finish_reason": finish_reason }]
    });
    if let Some(usage) = state.usage {
        value["usage"] = usage;
    }
    parse_chat_completions_response(value)
}

/// 解析一条 Chat Completions SSE data 事件并更新聚合状态。
fn handle_chat_completions_sse_data(
    data: &str,
    sender: &ModelEventSender,
    state: &mut ChatCompletionsStreamState,
) -> Result<()> {
    if data == "[DONE]" {
        return Ok(());
    }
    let event: Value =
        serde_json::from_str(data).context("failed to parse OpenAI Chat Completions SSE")?;
    if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
        return Err(anyhow!(message.to_string()));
    }
    if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
        state.usage = Some(usage.clone());
    }
    let Some(choice) = event
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(());
    };
    let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        state.text.push_str(text);
        sender.send(ModelStreamEvent::TextDelta {
            index: choice.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            delta: text.to_string(),
        });
    }
    if let Some(thinking) = delta.get("reasoning_content").and_then(Value::as_str) {
        sender.send(ModelStreamEvent::ThinkingDelta {
            index: choice.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            delta: thinking.to_string(),
        });
    }
    if let Some(tool_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_delta in tool_deltas {
            let index = tool_delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if state.tools.len() <= index {
                state
                    .tools
                    .resize_with(index + 1, ChatToolStreamState::default);
            }
            let tool = &mut state.tools[index];
            if let Some(id) = tool_delta.get("id").and_then(Value::as_str) {
                tool.id.push_str(id);
            }
            if let Some(name) = tool_delta.pointer("/function/name").and_then(Value::as_str) {
                tool.name.push_str(name);
            }
            if let Some(arguments) = tool_delta
                .pointer("/function/arguments")
                .and_then(Value::as_str)
            {
                tool.arguments.push_str(arguments);
                sender.send(ModelStreamEvent::ToolCallDelta {
                    index,
                    delta: arguments.to_string(),
                });
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = Some(reason.to_string());
    }
    Ok(())
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

fn messages_to_responses_input(messages: &[super::ModelMessage]) -> Result<Value> {
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
                push_text_input(&mut input, "user", message.text_content());
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

fn message_to_openai_chat_messages(message: &super::ModelMessage) -> Result<Vec<Value>> {
    let mut out = Vec::new();

    match &message.role {
        MessageRole::System | MessageRole::Developer | MessageRole::User => {
            let text = message.text_content();
            if !text.is_empty() {
                out.push(json!({
                    "role": message.role.as_str(),
                    "content": text,
                }));
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
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 裸域名形式的 OpenAI 兼容地址会自动补 `/v1`。
    #[test]
    fn normalize_openai_base_url_adds_v1_to_origin() {
        let base_url = normalize_openai_base_url(Some("https://api.phrolova.uno".to_string()))
            .expect("base URL should be valid");

        assert_eq!(base_url, "https://api.phrolova.uno/v1");
    }

    /// 已带 `/v1` 的地址不会重复追加版本前缀。
    #[test]
    fn normalize_openai_base_url_keeps_existing_v1() {
        let base_url = normalize_openai_base_url(Some("http://localhost:11434/v1/".to_string()))
            .expect("base URL should be valid");

        assert_eq!(base_url, "http://localhost:11434/v1");
    }

    /// 网关前缀路径会保留，并在其后追加 `/v1`。
    #[test]
    fn normalize_openai_base_url_adds_v1_after_gateway_prefix() {
        let base_url = normalize_openai_base_url(Some("https://example.com/openai".to_string()))
            .expect("base URL should be valid");

        assert_eq!(base_url, "https://example.com/openai/v1");
    }

    /// endpoint 拼接使用规范化后的 base URL，避免遗漏版本路径。
    #[test]
    fn endpoint_url_uses_normalized_base_url() {
        let base_url = normalize_openai_base_url(Some("https://api.phrolova.uno".to_string()))
            .expect("base URL should be valid");

        assert_eq!(
            endpoint_url(&base_url, "/responses"),
            "https://api.phrolova.uno/v1/responses"
        );
    }

    /// 官方 OpenAI 地址选择完整参数白名单。
    #[test]
    fn allowed_params_official_openai_is_full() {
        let params = responses_allowed_params(&ProviderKind::OpenAi, "https://api.openai.com/v1");
        assert!(params.contains(&"max_output_tokens"));
        assert!(params.contains(&"store"));
    }

    /// 非官方 base URL 选择兼容参数白名单，不含 max_output_tokens。
    #[test]
    fn allowed_params_proxy_uses_compatible() {
        let params = responses_allowed_params(&ProviderKind::OpenAi, "https://api.phrolova.uno/v1");
        assert!(!params.contains(&"max_output_tokens"));
        assert!(!params.contains(&"store"));
    }

    /// OpenAiCompatible 类型始终使用兼容参数白名单。
    #[test]
    fn allowed_params_compatible_kind_uses_compatible() {
        let params =
            responses_allowed_params(&ProviderKind::OpenAiCompatible, "https://localhost:8080/v1");
        assert!(!params.contains(&"max_output_tokens"));
    }

    /// filter_params 移除不在白名单中的键。
    #[test]
    fn filter_params_drops_unsupported_keys() {
        let mut body = json!({
            "model": "gpt-4",
            "input": [],
            "max_output_tokens": 32,
            "store": true,
            "temperature": 0.7,
        });
        filter_params(&mut body, RESPONSES_PARAMS_COMPATIBLE);

        assert!(body.get("model").is_some());
        assert!(body.get("input").is_some());
        assert!(body.get("temperature").is_some());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("store").is_none());
    }

    /// filter_params 对完整白名单不会丢弃标准参数。
    #[test]
    fn filter_params_keeps_all_with_full_list() {
        let mut body = json!({
            "model": "gpt-4",
            "input": [],
            "max_output_tokens": 32,
            "store": true,
        });
        filter_params(&mut body, RESPONSES_PARAMS_FULL);

        assert!(body.get("max_output_tokens").is_some());
        assert!(body.get("store").is_some());
    }

    /// Responses SSE 文本增量会实时发送并进入最终聚合文本。
    #[tokio::test]
    async fn responses_sse_emits_and_aggregates_text_delta() {
        let (sender, mut stream) = ModelEventStream::channel();
        let mut state = ResponsesStreamState::default();

        handle_responses_sse_data(
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"你好"}"#,
            &sender,
            &mut state,
        )
        .expect("应该解析文本增量");

        assert_eq!(state.text, "你好");
        match stream.next().await.expect("应该收到流事件") {
            ModelStreamEvent::TextDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta, "你好");
            }
            event => panic!("收到非预期事件: {event:?}"),
        }
    }

    /// Chat Completions SSE 会按索引拼接工具名称和 JSON 参数片段。
    #[test]
    fn chat_completions_sse_aggregates_tool_call_fragments() {
        let (sender, _stream) = ModelEventStream::channel();
        let mut state = ChatCompletionsStreamState::default();
        let first = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"echo","arguments":"{\"text\":"}}]}}]}"#;
        let second = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;

        handle_chat_completions_sse_data(first, &sender, &mut state).expect("应该解析首个工具片段");
        handle_chat_completions_sse_data(second, &sender, &mut state)
            .expect("应该解析第二个工具片段");

        assert_eq!(state.tools.len(), 1);
        assert_eq!(state.tools[0].id, "call_1");
        assert_eq!(state.tools[0].name, "echo");
        assert_eq!(state.tools[0].arguments, r#"{"text":"hi"}"#);
        assert_eq!(state.finish_reason.as_deref(), Some("tool_calls"));
    }
}
