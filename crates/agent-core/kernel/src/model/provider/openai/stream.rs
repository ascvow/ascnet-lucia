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
    sender.error(format!("{error:#}"));
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
            Err(error) => sender.error(format!("{error:#}")),
        }
    });
    stream.with_request_task(request_task)
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
pub(super) struct ResponsesStreamState {
    pub(super) text: String,
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
