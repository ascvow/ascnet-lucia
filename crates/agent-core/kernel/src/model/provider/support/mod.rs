//! 模型适配器共享 HTTP、SSE 与附件降级工具。
//!
//! 本模块提取了 OpenAI 与 Anthropic 适配器共用的辅助函数，
//! 避免在多个适配器中重复相同的实现。

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::{Map, Value};
use std::str::FromStr;

use futures_util::StreamExt;

/// 响应体预览的最大字符数，避免过长正文淹没错误信息。
const RESPONSE_BODY_PREVIEW_CHARS: usize = 500;
/// 单次传输的正文上限，不是 Agent 生命周期累计配额。
pub(super) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const ERROR_BODY_BYTES: usize = 2048;

/// 保留 TLS 验证；连接 30 秒、读取空闲 120 秒，不自动转发凭证到重定向目标。
pub(super) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build provider HTTP client")
}

/// 成功正文逐块校验大小，错误正文达到短预览上限后立即停止读取。
pub(super) async fn read_response_text(
    response: reqwest::Response,
    protocol: &str,
    error_preview: bool,
) -> Result<String> {
    let limit = if error_preview {
        ERROR_BODY_BYTES
    } else {
        MAX_RESPONSE_BYTES
    };
    let chunks = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(anyhow::Error::from));
    let bytes = read_bounded_chunks(chunks, limit, error_preview)
        .await
        .with_context(|| format!("failed to read {protocol} response body"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn read_bounded_chunks<S, B>(mut chunks: S, limit: usize, preview: bool) -> Result<Vec<u8>>
where
    S: futures_util::Stream<Item = Result<B>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        let remaining = limit - body.len();
        if !preview && chunk.len() > remaining {
            return Err(anyhow!("provider response exceeds {limit} bytes"));
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if preview && body.len() == limit {
            break;
        }
    }
    Ok(body)
}

/// 按新到达字节解帧，避免逐行搬移剩余缓冲；累计输入含注释和未闭合行。
#[cfg(any(feature = "openai", feature = "google", test))]
pub(super) async fn for_each_sse_data<S, B, F>(chunks: S, handler: F) -> Result<()>
where
    S: futures_util::Stream<Item = Result<B>> + Unpin,
    B: AsRef<[u8]>,
    F: FnMut(&str) -> Result<()>,
{
    consume_sse_chunks(chunks, MAX_RESPONSE_BYTES, handler).await
}

#[cfg(any(feature = "openai", feature = "google", test))]
async fn consume_sse_chunks<S, B, F>(mut chunks: S, limit: usize, mut handler: F) -> Result<()>
where
    S: futures_util::Stream<Item = Result<B>> + Unpin,
    B: AsRef<[u8]>,
    F: FnMut(&str) -> Result<()>,
{
    let mut received = 0usize;
    let mut pending = Vec::new();
    let mut event = None;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        if chunk.len() > limit - received {
            return Err(anyhow!("SSE response exceeds {limit} bytes"));
        }
        received += chunk.len();
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'\n' {
                pending.extend_from_slice(&chunk[start..index]);
                process_sse_line(&pending, &mut event, &mut handler)?;
                pending.clear();
                start = index + 1;
            }
        }
        pending.extend_from_slice(&chunk[start..]);
    }
    if !pending.is_empty() {
        process_sse_line(&pending, &mut event, &mut handler)?;
    }
    if let Some(data) = event {
        handler(&data)?;
    }
    Ok(())
}

/// 完整行到齐后才解码 UTF-8；多行 data 保持顺序并以换行连接。
#[cfg(any(feature = "openai", feature = "google", test))]
fn process_sse_line<F>(line: &[u8], event: &mut Option<String>, handler: &mut F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        if let Some(data) = event.take() {
            handler(&data)?;
        }
    } else if let Some(data) = line.strip_prefix(b"data:") {
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let data = std::str::from_utf8(data).context("SSE data 不是 UTF-8")?;
        if let Some(buffer) = event.as_mut() {
            buffer.push('\n');
            buffer.push_str(data);
        } else {
            *event = Some(data.to_owned());
        }
    }
    Ok(())
}

/// 解码 HTTP JSON 响应，并在 HTTP 或 JSON 错误时保留可诊断的响应摘要。
///
/// `protocol` 参数用于错误信息中标识请求来源（如 "OpenAI Responses"、"Anthropic Messages"）。
pub(super) async fn decode_json_response(
    response: reqwest::Response,
    protocol: &str,
) -> Result<Value> {
    let status = response.status();
    let url = response.url().clone();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>")
        .to_string();
    let body = read_response_text(response, protocol, !status.is_success()).await?;

    if !status.is_success() {
        return Err(anyhow!(
            "{protocol} returned an error status (status: {status}, url: {url}, content-type: {content_type}, body preview: {})",
            response_body_preview(&body)
        ));
    }

    // 部分网关无视 `stream: false` 强制返回 SSE；从事件流中提取最终响应对象。
    if content_type.contains("text/event-stream") {
        return parse_sse_final_json(&body).ok_or_else(|| {
            anyhow!(
                "failed to extract final response from {protocol} SSE (status: {status}, url: {url}, body preview: {})",
                response_body_preview(&body)
            )
        });
    }

    serde_json::from_str(&body).with_context(|| {
        format!(
            "failed to decode {protocol} JSON (status: {status}, url: {url}, content-type: {content_type}, body preview: {})",
            response_body_preview(&body)
        )
    })
}

/// 从 SSE 响应体中提取最终响应 JSON。
///
/// 优先取 `response.completed` 事件的 `response` 字段（OpenAI Responses API）；
/// 找不到时回退为最后一个可解析的 data 事件（若含 `response` 字段则取其值）。
/// 部分网关的 completed 事件里 output 缺少文本，此时用 `response.output_text.delta`
/// 事件聚合出的文本补全。
fn parse_sse_final_json(body: &str) -> Option<Value> {
    let mut fallback = None;
    let mut aggregated_text = String::new();

    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    aggregated_text.push_str(delta);
                }
            }
            Some("response.completed") => {
                if let Some(response) = value.get("response") {
                    let mut response = response.clone();
                    ensure_output_text(&mut response, &aggregated_text);
                    return Some(response);
                }
            }
            _ => {}
        }
        fallback = Some(value);
    }

    let mut value = fallback.map(|value| value.get("response").cloned().unwrap_or(value))?;
    ensure_output_text(&mut value, &aggregated_text);
    Some(value)
}

/// 如果响应 output 中没有任何文本，则把 SSE 聚合文本补进去。
pub(super) fn ensure_output_text(response: &mut Value, aggregated: &str) {
    if aggregated.is_empty() {
        return;
    }
    let has_text = response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                        })
                    })
            })
        });
    if has_text {
        return;
    }

    let message = serde_json::json!({
        "type": "message",
        "content": [{ "type": "output_text", "text": aggregated }]
    });
    match response.get_mut("output") {
        Some(Value::Array(items)) => items.push(message),
        _ => {
            response["output"] = Value::Array(vec![message]);
        }
    }
}

/// 生成适合错误信息展示的响应体预览，避免过长正文淹没真正的错误原因。
pub(super) fn response_body_preview(body: &str) -> String {
    let mut chars = body.chars();
    let mut preview: String = chars.by_ref().take(RESPONSE_BODY_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview.replace('\r', "\\r").replace('\n', "\\n")
}

/// 将 `HashMap<String, String>` 转换为 reqwest `HeaderMap`。
pub(super) fn header_map(headers: std::collections::HashMap<String, String>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        map.insert(
            HeaderName::from_str(&key).with_context(|| format!("invalid header name: {key}"))?,
            HeaderValue::from_str(&value)
                .with_context(|| format!("invalid header value for {key}"))?,
        );
    }
    Ok(map)
}

/// 浅合并 `provider_options` 到请求体，仅处理 Object 类型的值。
///
/// 合并在参数白名单过滤之后执行，因此用户可通过
/// `provider_options` 注入任意 provider 特有参数，不受白名单约束。
pub(super) fn merge_provider_options(body: &mut Value, provider_options: Value) {
    if !provider_options.is_object() {
        return;
    }
    let Some(body_obj) = body.as_object_mut() else {
        return;
    };
    if let Value::Object(extra) = provider_options {
        merge_object(body_obj, extra);
    }
}

/// 恢复可信输出上限，并把已出现的常见 token 别名和多候选参数收窄到同一预算。
///
/// 仅处理请求体顶层字段；调用方应在合并 `provider_options` 后调用。不存在的兼容字段不会
/// 被主动添加，避免改变原生 provider 协议；已出现的生成数量字段固定为一，防止一次请求
/// 生成多份候选却只按单份 `max_tokens` 预留资源。
pub(super) fn enforce_output_budget(body: &mut Value, primary_key: &str, max_tokens: u32) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    object.insert(primary_key.to_string(), Value::from(max_tokens));
    for alias in [
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
        "max_new_tokens",
        "max_length",
    ] {
        if object.contains_key(alias) {
            object.insert(alias.to_string(), Value::from(max_tokens));
        }
    }
    for multiplier in [
        "n",
        "best_of",
        "num_return_sequences",
        "candidate_count",
        "num_generations",
        "number_of_responses",
    ] {
        if object.contains_key(multiplier) {
            object.insert(multiplier.to_string(), Value::from(1));
        }
    }
}

/// 将 `src` 的所有键值对浅拷贝到 `dst`，同名键覆盖。
pub(super) fn merge_object(dst: &mut Map<String, Value>, src: Map<String, Value>) {
    for (key, value) in src {
        dst.insert(key, value);
    }
}

/// 拼接已规范化的 base URL 与具体 endpoint path。
pub(super) fn endpoint_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

/// 对 URL path 组件做百分号编码；未保留字符保持原样。
///
/// 只有 Bedrock 的推理模型 ID 路径需要这个编码，因此与 `bedrock` feature 同生同灭：
/// `lucia` 以 `default-features = false` 依赖 core 时不带 Bedrock，函数若仍然存在，
/// 打包构建里会一直报一条 unused 告警。
#[cfg(feature = "bedrock")]
pub(super) fn url_encode_component(component: &str) -> String {
    let mut encoded = String::with_capacity(component.len());
    for byte in component.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// 判断附件媒体类型是否可以安全地内联为纯文本。
pub(super) fn is_text_media_type(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
        || matches!(
            media_type,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/toml"
                | "application/yaml"
                | "application/x-yaml"
        )
}

/// 将 provider 不原生支持的文件附件降级为文本。
///
/// 文本类型解码后内联文件内容；解码失败或二进制类型生成占位说明，
/// 保证模型始终能看到附件存在，而不是被静默丢弃。
pub(super) fn file_attachment_fallback_text(name: &str, media_type: &str, data: &str) -> String {
    use base64::Engine;

    if is_text_media_type(media_type) {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        if let Some(text) = decoded {
            return format!("[附件 {name}]\n{text}");
        }
    }
    format!("[附件 {name}（{media_type}）无法内联发送，内容已省略]")
}

#[cfg(test)]
mod tests;
