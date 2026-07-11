//! 模型适配器公共工具函数。
//!
//! 本模块提取了 OpenAI 与 Anthropic 适配器共用的辅助函数，
//! 避免在多个适配器中重复相同的实现。

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::{Map, Value};
use std::str::FromStr;

/// 响应体预览的最大字符数，避免过长正文淹没错误信息。
const RESPONSE_BODY_PREVIEW_CHARS: usize = 500;

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
    let body = response
        .text()
        .await
        .with_context(|| format!("failed to read {protocol} response body"))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// SSE 中 response.completed 事件的 response 字段被提取。
    #[test]
    fn sse_extracts_completed_response() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n",
        );

        let value = parse_sse_final_json(body).expect("应该提取到最终响应");
        assert_eq!(value["status"], "completed");
    }

    /// 没有 completed 事件时回退到最后一个 data 事件。
    #[test]
    fn sse_falls_back_to_last_event() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n",
            "data: [DONE]\n",
        );

        let value = parse_sse_final_json(body).expect("应该回退到最后事件");
        assert_eq!(value["status"], "in_progress");
    }

    /// 完全无法解析时返回 None。
    #[test]
    fn sse_returns_none_for_garbage() {
        assert!(parse_sse_final_json("not sse at all").is_none());
    }

    /// completed 事件缺少文本时，用 output_text.delta 聚合补全。
    #[test]
    fn sse_aggregates_deltas_when_completed_has_no_text() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"好\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n",
        );

        let value = parse_sse_final_json(body).expect("应该提取到最终响应");
        let text = value["output"][0]["content"][0]["text"]
            .as_str()
            .expect("应该有聚合文本");
        assert_eq!(text, "你好");
    }

    /// completed 事件已有文本时不重复注入聚合文本。
    #[test]
    fn sse_keeps_existing_output_text() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"abc\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"完整文本\"}]}]}}\n",
        );

        let value = parse_sse_final_json(body).expect("应该提取到最终响应");
        let output = value["output"].as_array().expect("output 应为数组");
        assert_eq!(output.len(), 1);
    }
}
