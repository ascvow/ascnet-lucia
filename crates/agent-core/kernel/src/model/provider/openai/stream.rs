//! OpenAI Responses 与 Chat Completions 共用的 SSE 传输和增量聚合。

use super::super::support::ensure_output_text;
use super::{
    parse_chat_completions_response, parse_openai_function_call_item, parse_responses_response,
    CODEX_DESKTOP_USER_AGENT,
};
use crate::model::{ModelEventSender, ModelEventStream, ModelResponse, ModelStreamEvent};
use agent_tool::ToolCall;
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// OpenAI 流式协议类型，用于选择对应的 SSE 聚合器。
#[derive(Clone, Copy)]
pub(super) enum OpenAiStreamProtocol {
    Responses,
    ChatCompletions,
}

/// 创建一个立即返回错误终止事件的模型流。
pub(super) fn failed_model_stream(error: anyhow::Error) -> ModelEventStream {
    let (sender, stream) = ModelEventStream::channel();
    sender.fail(error);
    stream
}

/// 在后台发送 OpenAI 流式请求，并将 SSE 聚合为最终模型响应。
pub(super) fn spawn_openai_stream(
    client: reqwest::Client,
    url: String,
    api_key: String,
    headers: HeaderMap,
    body: Value,
    protocol: OpenAiStreamProtocol,
) -> ModelEventStream {
    let (sender, stream) = ModelEventStream::channel();
    let request_task = tokio::spawn(async move {
        sender.send(ModelStreamEvent::Start);
        let result = async {
            let response = client
                .post(url)
                .headers(headers)
                .header(USER_AGENT, CODEX_DESKTOP_USER_AGENT)
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
            Err(error) => sender.fail(error),
        }
    });
    stream.with_request_task(request_task)
}

/// 检查状态码，错误只保留短前缀，避免远程正文撑满错误通道。
async fn require_successful_stream_response(
    response: reqwest::Response,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = super::super::support::read_response_text(response, "OpenAI streaming", true)
        .await
        .unwrap_or_else(|_| "<unreadable body>".into());
    Err(super::super::support::http_status_error(
        status,
        format!(
            "OpenAI streaming request failed with status {status}: {}",
            super::super::support::response_body_preview(&body)
        ),
    ))
}

/// 共享线性解帧器处理跨块 UTF-8 并限制累计响应大小。
async fn for_each_sse_data<F>(response: reqwest::Response, handler: F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let chunks = response
        .bytes_stream()
        .map(|chunk| chunk.context("failed to read OpenAI SSE chunk"));
    super::super::support::for_each_sse_data(chunks, handler).await
}

/// Responses API SSE 的聚合状态。
#[derive(Default)]
pub(super) struct ResponsesStreamState {
    pub(super) text: String,
    function_items: BTreeMap<usize, Value>,
    ended_function_items: BTreeSet<usize>,
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

    finalize_responses_stream_state(state)
}

/// 将已消费完的 Responses 流状态转换为统一模型响应。
pub(super) fn finalize_responses_stream_state(
    state: ResponsesStreamState,
) -> Result<ModelResponse> {
    if let Some(mut response) = state.completed_response {
        ensure_output_text(&mut response, &state.text);
        ensure_function_items(&mut response, &state.function_items);
        return parse_responses_response(response);
    }

    Err(crate::model::TransientModelError::new(
        "OpenAI Responses stream ended without response.completed event",
    )
    .into())
}

/// 解析一条 Responses API SSE data 事件并更新聚合状态。
pub(super) fn handle_responses_sse_data(
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
            append_responses_text(&event, delta, sender, state);
        }
        "response.output_text.done" => {
            let text = event
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            append_completed_responses_text(&event, text, sender, state);
        }
        "response.content_part.done" => {
            let text = event
                .pointer("/part/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            append_completed_responses_text(&event, text, sender, state);
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
            let index = responses_output_index(&event);
            if let Some(item) = state.function_items.get_mut(&index) {
                if let Some(Value::String(arguments)) = item.get_mut("arguments") {
                    arguments.push_str(delta);
                } else {
                    item["arguments"] = Value::String(delta.to_owned());
                }
            }
            sender.send(ModelStreamEvent::ToolCallDelta {
                index,
                delta: delta.to_string(),
            });
        }
        "response.function_call_arguments.done" => {
            let index = responses_output_index(&event);
            if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                if let Some(item) = state.function_items.get_mut(&index) {
                    item["arguments"] = Value::String(arguments.to_string());
                }
            }
            finish_responses_function_item(index, sender, state)?;
        }
        "response.output_item.added" => {
            let index = responses_output_index(&event);
            if let Some(item) = event
                .get("item")
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            {
                state.function_items.insert(index, item.clone());
            }
        }
        "response.output_item.done" => {
            let index = responses_output_index(&event);
            if let Some(item) = event
                .get("item")
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            {
                state.function_items.insert(index, item.clone());
                finish_responses_function_item(index, sender, state)?;
            } else if let Some(parts) = event.pointer("/item/content").and_then(Value::as_array) {
                let text = parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>();
                append_completed_responses_text(&event, &text, sender, state);
            }
        }
        "response.completed" | "response.incomplete" => {
            // 输出长度到限等有终态的响应不是断线，不能重试同一请求来绕过模型上限。
            state.completed_response = event.get("response").cloned();
        }
        "error" | "response.failed" => {
            let error = event
                .get("error")
                .or_else(|| event.pointer("/response/error"))
                .unwrap_or(&event);
            return Err(openai_stream_error(error, "OpenAI Responses stream failed"));
        }
        _ => {}
    }
    Ok(())
}

/// HTTP 已成功建流时，仍需按结构化错误码识别临时服务故障；自然语言正文不参与分类。
fn openai_stream_error(error: &Value, fallback: &str) -> anyhow::Error {
    let message = super::super::support::response_body_preview(
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(fallback),
    );
    // 明确错误码优先于宽泛的 type，余额不足等永久错误不能被 server_error 类型掩盖。
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str));
    if matches!(
        code,
        Some("server_error" | "internal_server_error" | "rate_limit_exceeded" | "overloaded_error")
    ) {
        crate::model::TransientModelError::new(message).into()
    } else {
        anyhow!(message)
    }
}

/// 追加 Responses 正文并发送对应增量；空文本不会产生事件。
fn append_responses_text(
    event: &Value,
    text: &str,
    sender: &ModelEventSender,
    state: &mut ResponsesStreamState,
) {
    if text.is_empty() {
        return;
    }
    state.text.push_str(text);
    sender.send(ModelStreamEvent::TextDelta {
        index: event
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        delta: text.to_string(),
    });
}

/// 用完整正文事件补齐尚未通过增量收到的后缀，避免标准结束事件造成空响应或重复文本。
fn append_completed_responses_text(
    event: &Value,
    text: &str,
    sender: &ModelEventSender,
    state: &mut ResponsesStreamState,
) {
    let missing = text.strip_prefix(&state.text).unwrap_or_default();
    append_responses_text(event, missing, sender, state);
}

/// 返回 Responses 事件对应的输出位置；兼容网关缺省时归入首个输出项。
fn responses_output_index(event: &Value) -> usize {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

/// 完成一个已聚合的函数调用并只发送一次结束事件。
fn finish_responses_function_item(
    index: usize,
    sender: &ModelEventSender,
    state: &mut ResponsesStreamState,
) -> Result<()> {
    if state.ended_function_items.contains(&index) {
        return Ok(());
    }
    let Some(item) = state.function_items.get(&index) else {
        return Ok(());
    };
    let call = parse_openai_function_call_item(item)?;
    state.ended_function_items.insert(index);
    sender.send(ModelStreamEvent::ToolCallEnd { index, call });
    Ok(())
}

/// 将流中独立完成的函数调用补入最终响应，避免精简 `response.completed` 丢失工具调用。
fn ensure_function_items(response: &mut Value, function_items: &BTreeMap<usize, Value>) {
    if function_items.is_empty() {
        return;
    }
    if !response.get("output").is_some_and(Value::is_array) {
        response["output"] = Value::Array(Vec::new());
    }
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return;
    };
    for item in function_items.values() {
        let identity = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str);
        let existing_index = identity.and_then(|identity| {
            output.iter().position(|existing| {
                existing
                    .get("call_id")
                    .or_else(|| existing.get("id"))
                    .and_then(Value::as_str)
                    == Some(identity)
            })
        });
        if let Some(existing_index) = existing_index {
            output[existing_index] = item.clone();
        } else {
            output.push(item.clone());
        }
    }
}

/// Chat Completions 工具调用的流式聚合状态。
#[derive(Default)]
pub(super) struct ChatToolStreamState {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

/// Chat Completions SSE 的聚合状态。
#[derive(Default)]
pub(super) struct ChatCompletionsStreamState {
    text: String,
    pub(super) tools: Vec<ChatToolStreamState>,
    pub(super) finish_reason: Option<String>,
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
    // 没有 finish_reason 的 EOF 不代表完成，尤其不能执行仅收到一半的工具调用。
    if state.finish_reason.is_none() {
        return Err(crate::model::TransientModelError::new(
            "OpenAI Chat Completions stream ended without finish_reason",
        )
        .into());
    }

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
pub(super) fn handle_chat_completions_sse_data(
    data: &str,
    sender: &ModelEventSender,
    state: &mut ChatCompletionsStreamState,
) -> Result<()> {
    if data == "[DONE]" {
        return Ok(());
    }
    let event: Value =
        serde_json::from_str(data).context("failed to parse OpenAI Chat Completions SSE")?;
    if let Some(error) = event.get("error") {
        return Err(openai_stream_error(
            error,
            "OpenAI Chat Completions stream failed",
        ));
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
            let raw_index = tool_delta.get("index").and_then(Value::as_u64).unwrap_or(0);
            let index = usize::try_from(raw_index).context("tool index exceeds address space")?;
            // 稀疏索引不能用几个输入字节驱动无界 Vec 扩容。
            if index >= 4096 {
                return Err(anyhow!("tool stream index exceeds 4096 slots"));
            }
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
