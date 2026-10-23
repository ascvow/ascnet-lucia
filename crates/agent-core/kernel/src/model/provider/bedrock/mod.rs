//! Amazon Bedrock Converse API 协议适配器。
//!
//! 使用 SigV4 对 `bedrock-runtime.{region}.amazonaws.com` 的 Converse 请求签名。
//! 首版只实现非流式 Converse 调用；流式路径继承 [`ChatModel::stream`] 的默认
//! 包装（等待完整响应后一次性下发事件），后续可替换为 Converse 事件流。

use super::support::{file_attachment_fallback_text, header_map};
use crate::model::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelEventStream, ModelProviderConfig,
    ModelRequest, ModelResponse, ModelStreamEvent, ProviderAdapter, ProviderBilling, TokenUsage,
    ToolChoice,
};
use agent_tool::{ToolCall, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, CONTENT_TYPE};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// SigV4 服务名；Bedrock Runtime 固定为 bedrock。
const SIGV4_SERVICE: &str = "bedrock";
/// SigV4 算法标识。
const SIGV4_ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Amazon Bedrock Converse 适配器。
#[derive(Debug)]
pub struct BedrockConverseAdapter {
    client: reqwest::Client,
    /// 注册到网关的逻辑服务商名称，保留用于后续诊断与路由扩展。
    #[allow(dead_code)]
    provider_name: String,
    base_url: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    extra_headers: HeaderMap,
}

impl BedrockConverseAdapter {
    /// Create a new adapter.
    ///
    /// 错误：缺少 `aws_region`、`aws_access_key_id` 或 `aws_secret_access_key` 时返回。
    pub fn new(config: ModelProviderConfig) -> Result<Self> {
        let region = config
            .aws_region
            .clone()
            .ok_or_else(|| anyhow!("Bedrock provider `{}` requires aws_region", config.name))?;
        let access_key_id = config.aws_access_key_id.clone().ok_or_else(|| {
            anyhow!(
                "Bedrock provider `{}` requires aws_access_key_id",
                config.name
            )
        })?;
        let secret_access_key = config.aws_secret_access_key.clone().ok_or_else(|| {
            anyhow!(
                "Bedrock provider `{}` requires aws_secret_access_key",
                config.name
            )
        })?;
        let base_url = config
            .base_url
            .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"));
        Ok(Self {
            client: reqwest::Client::new(),
            provider_name: config.name,
            base_url,
            region,
            access_key_id,
            secret_access_key,
            session_token: config.aws_session_token.filter(|token| !token.is_empty()),
            extra_headers: header_map(config.extra_headers)?,
        })
    }

    /// 对一次 Converse 请求做 SigV4 签名并发送。
    async fn converse(&self, model_id: &str, body: &Value) -> Result<reqwest::Response> {
        let path = format!("/model/{}/converse", encode_path_segment(model_id));
        let payload = serde_json::to_vec(body).context("Bedrock 请求体序列化失败")?;
        let (headers, url) = self.sign_request(&path, &payload)?;

        self.client
            .post(url)
            .headers(headers)
            .header(CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .context("failed to send Bedrock Converse request")
    }

    /// 构造 SigV4 签名后的请求头与最终 URL。
    ///
    /// 实现遵循 AWS SigV4 规范：对 POST、规范化路径、空查询串与固定参与签名的
    /// header（host、x-amz-date，可选 x-amz-security-token）计算 HMAC 链。
    fn sign_request(&self, path: &str, payload: &[u8]) -> Result<(HeaderMap, String)> {
        use reqwest::header::{HeaderName, HOST};

        let host = self
            .base_url
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("系统时间早于 Unix 纪元，无法生成 SigV4 时间戳")?;
        let amz_date = aws_timestamp(now.as_secs());
        let date_stamp = amz_date[..8].to_string();
        let payload_hash = hex::encode(Sha256::digest(payload));

        let mut signed_headers_map: Vec<(String, String)> = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(token) = &self.session_token {
            signed_headers_map.push(("x-amz-security-token".to_string(), token.clone()));
        }
        signed_headers_map.sort();
        let signed_headers = signed_headers_map
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers = signed_headers_map
            .iter()
            .map(|(key, value)| format!("{key}:{value}\n"))
            .collect::<String>();

        let canonical_request =
            format!("POST\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let scope = format!(
            "{date_stamp}/{}/{}/aws4_request",
            self.region, SIGV4_SERVICE
        );
        let string_to_sign = format!(
            "{SIGV4_ALGORITHM}\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let mut key = hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        key = hmac_sha256(&key, self.region.as_bytes());
        key = hmac_sha256(&key, SIGV4_SERVICE.as_bytes());
        key = hmac_sha256(&key, b"aws4_request");
        let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

        let mut headers = self.extra_headers.clone();
        headers.insert(HOST, host.parse().context("Bedrock host header 非法")?);
        headers.insert(
            HeaderName::from_static("x-amz-date"),
            amz_date.parse().context("x-amz-date 值非法")?,
        );
        if let Some(token) = &self.session_token {
            headers.insert(
                HeaderName::from_static("x-amz-security-token"),
                token.parse().context("x-amz-security-token 值非法")?,
            );
        }
        headers.insert(
            HeaderName::from_static("authorization"),
            format!(
                "{SIGV4_ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                self.access_key_id
            )
            .parse()
            .context("authorization 值非法")?,
        );
        Ok((headers, url))
    }

    /// 完成一次 Converse 调用并解析响应。
    async fn complete_once(&self, req: ModelRequest) -> Result<ModelResponse> {
        let (model_id, body) = build_converse_request(req)?;
        let response = self.converse(&model_id, &body).await?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read Bedrock response body")?;
        if !status.is_success() {
            anyhow::bail!(
                "Bedrock Converse returned an error status (status: {status}, body preview: {})",
                super::support::response_body_preview(&text)
            );
        }
        let value: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to decode Bedrock Converse JSON (body preview: {})",
                super::support::response_body_preview(&text)
            )
        })?;
        parse_converse_response(value)
    }
}

#[async_trait]
impl ChatModel for BedrockConverseAdapter {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        self.complete_once(req).await
    }
}

#[async_trait]
impl ProviderAdapter for BedrockConverseAdapter {
    fn name(&self) -> &'static str {
        "bedrock-converse"
    }

    async fn stream_for_provider(
        &self,
        _logical_provider: &str,
        req: ModelRequest,
    ) -> ModelEventStream {
        // 首版流式路径：等待完整 Converse 响应后一次性下发事件。
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::Start);
        match self.complete_once(req).await {
            Ok(response) => tx.done(response),
            Err(error) => tx.error(format!("{error:#}")),
        }
        stream
    }
}

/// 对路径段做 URL 编码，保留 Bedrock 推理模型 ID 中合法的字符。
///
/// AWS SDK 使用原始模型 ID 或 ARN；冒号等分隔符必须编码，斜杠（ARN 内）保持原样。
fn encode_path_segment(model_id: &str) -> String {
    model_id
        .split('/')
        .map(crate::model::provider::support::url_encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// HMAC-SHA256；输出长度恒为 32 字节，调用方按实际长度截取。
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC 可接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// 把 Unix 秒时间戳格式化为 SigV4 的 `YYYYMMDDTHHMMSSZ`。
fn aws_timestamp(unix_seconds: u64) -> String {
    // SigV4 只在签名瞬间使用 UTC 时间；这里直接由秒数推导，不依赖本地时区。
    let days_since_epoch = unix_seconds / 86_400;
    let secs_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days_since_epoch as i64);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60
    )
}

/// 把 Unix 天数转换为公历日期（Howard Hinnant 的 civil_from_days 算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把 provider-neutral 请求构建为 Converse 请求体。
///
/// 返回推理模型 ID 与请求体二元组。
pub(crate) fn build_converse_request(mut req: ModelRequest) -> Result<(String, Value)> {
    let model_id = std::mem::take(&mut req.model);
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = req.system.take() {
        system_parts.push(system);
    }

    let mut messages: Vec<Value> = Vec::new();
    for message in &req.messages {
        match message.role {
            MessageRole::System | MessageRole::Developer => {
                system_parts.push(message.text_content());
            }
            MessageRole::User => {
                messages.push(json!({
                    "role": "user",
                    "content": user_content(&message.content)?,
                }));
            }
            MessageRole::Assistant => {
                messages.push(json!({
                    "role": "assistant",
                    "content": assistant_content(&message.content),
                }));
            }
            MessageRole::Tool => {
                let mut content = Vec::new();
                for block in &message.content {
                    if let ContentBlock::ToolResult { result } = block {
                        content.push(tool_result_block(
                            &result.call_id,
                            &result.content,
                            result.is_error,
                        ));
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert("messages".to_string(), Value::Array(messages));
    let system_text = system_parts
        .iter()
        .filter(|part| !part.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if !system_text.is_empty() {
        body.insert("system".to_string(), json!([{ "text": system_text }]));
    }
    if !req.tools.is_empty() {
        body.insert(
            "toolConfig".to_string(),
            json!({
                "tools": req.tools.iter().map(tool_to_bedrock).collect::<Vec<_>>(),
            }),
        );
        if let Some(choice) = tool_choice_to_bedrock(&req.tool_choice) {
            if let Some(config) = body.get_mut("toolConfig").and_then(Value::as_object_mut) {
                config.insert("toolChoice".to_string(), choice);
            }
        }
    }

    let mut inference = Map::new();
    if let Some(max_tokens) = req.max_tokens {
        inference.insert("maxTokens".to_string(), Value::from(max_tokens));
    }
    if let Some(temperature) = req.temperature {
        inference.insert("temperature".to_string(), Value::from(temperature));
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".to_string(), Value::Object(inference));
    }

    if let Value::Object(extra) = req.provider_options {
        for (key, value) in extra {
            body.insert(key, value);
        }
    }
    Ok((model_id, Value::Object(body)))
}

/// 把用户消息内容块映射为 Converse content 数组。
fn user_content(blocks: &[ContentBlock]) -> Result<Vec<Value>> {
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => content.push(json!({ "text": text })),
            ContentBlock::Image { media_type, data } => {
                let (format, raw) = image_format_and_data(media_type, data)?;
                content.push(json!({
                    "image": { "format": format, "source": { "bytes": raw } },
                }));
            }
            ContentBlock::File {
                name,
                media_type,
                data,
            } => {
                if media_type == "application/pdf" {
                    content.push(json!({
                        "document": {
                            "format": "pdf",
                            "name": sanitize_document_name(name),
                            "source": { "bytes": data },
                        },
                    }));
                } else {
                    content.push(json!({
                        "text": file_attachment_fallback_text(name, media_type, data),
                    }));
                }
            }
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => {}
            ContentBlock::ToolResult { result } => {
                content.push(tool_result_block(
                    &result.call_id,
                    &result.content,
                    result.is_error,
                ));
            }
        }
    }
    Ok(content)
}

/// 把 assistant 消息内容块映射为 Converse content 数组；签名推理块不回传。
fn assistant_content(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => content.push(json!({ "text": text })),
            ContentBlock::ToolCall { call } => content.push(json!({
                "toolUse": {
                    "toolUseId": call.id,
                    "name": call.name,
                    "input": call.args,
                },
            })),
            _ => {}
        }
    }
    content
}

/// 构造 toolResult content 块；对象载荷原样返回，其余包装为 JSON 文本。
fn tool_result_block(call_id: &str, content: &Value, is_error: bool) -> Value {
    let content_value = match content {
        Value::String(text) => json!([{ "text": text }]),
        Value::Array(items) if items.iter().all(Value::is_object) => content.clone(),
        other => json!([{ "json": other }]),
    };
    json!({
        "toolResult": {
            "toolUseId": call_id,
            "content": content_value,
            "status": if is_error { "error" } else { "success" },
        },
    })
}

/// 把工具定义映射为 Bedrock toolSpec。
fn tool_to_bedrock(tool: &ToolSpec) -> Value {
    json!({
        "toolSpec": {
            "name": tool.name,
            "description": tool.description,
            "inputSchema": { "json": tool.input_schema },
        },
    })
}

/// 把中立工具选择映射为 Bedrock toolChoice；Auto 缺省不发送。
fn tool_choice_to_bedrock(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::None => Some(json!({ "any": {} })),
        ToolChoice::Required => Some(json!({ "any": {} })),
        ToolChoice::Tool { name } => Some(json!({ "tool": { "name": name } })),
    }
}

/// 校验并把图片 MIME 类型转换为 Bedrock format 标识。
fn image_format_and_data(media_type: &str, data: &str) -> Result<(String, String)> {
    let format = match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => anyhow::bail!("Bedrock 不支持的图片类型：{other}"),
    };
    Ok((format.to_string(), data.to_string()))
}

/// Bedrock 文档名只允许字母数字与少量空白符号，其余替换为下划线。
fn sanitize_document_name(name: &str) -> String {
    let stem = name.rsplit('.').next().unwrap_or(name);
    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '(' | ')' | '\'' | '"') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "document".to_string()
    } else {
        sanitized
    }
}

/// 解析 Converse 响应为 provider-neutral 响应。
pub(crate) fn parse_converse_response(value: Value) -> Result<ModelResponse> {
    let output = value
        .get("output")
        .and_then(|output| output.get("message"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut content: Vec<ContentBlock> = Vec::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    if let Some(blocks) = output.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    content.push(ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
            } else if let Some(tool_use) = block.get("toolUse") {
                let call = ToolCall::new(
                    tool_use
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    tool_use
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    tool_use.get("input").cloned().unwrap_or_else(|| json!({})),
                );
                content.push(ContentBlock::ToolCall { call: call.clone() });
                calls.push(call);
            } else if let Some(reasoning) = block.get("reasoningContent") {
                if let Some(text) = reasoning
                    .get("reasoningText")
                    .and_then(|text| text.get("text"))
                    .and_then(Value::as_str)
                {
                    content.push(ContentBlock::Thinking {
                        thinking: text.to_string(),
                        signature: None,
                    });
                }
            }
        }
    }

    let finish_reason = match value.get("stopReason").and_then(Value::as_str) {
        Some("tool_use") => FinishReason::ToolCalls,
        Some("max_tokens") => FinishReason::Length,
        Some("end_turn") | Some("stop_sequence") | Some("complete") => FinishReason::Stop,
        Some("content_filtered") | Some("guardrail_intervened") => FinishReason::Refusal,
        Some(_) => FinishReason::Unknown,
        None => FinishReason::Stop,
    };

    let usage = value.get("usage").map(|usage| TokenUsage {
        input_tokens: usage.get("inputTokens").and_then(Value::as_u64),
        output_tokens: usage.get("outputTokens").and_then(Value::as_u64),
        total_tokens: usage.get("totalTokens").and_then(Value::as_u64),
        cache_read_tokens: usage.get("cacheReadInputTokens").and_then(Value::as_u64),
        cache_write_tokens: usage.get("cacheWriteInputTokens").and_then(Value::as_u64),
    });

    let raw_provider_response = Some(value);
    Ok(ModelResponse {
        content,
        tool_calls: calls,
        finish_reason,
        usage,
        billing: raw_provider_response
            .as_ref()
            .and_then(ProviderBilling::from_provider_response),
        raw_provider_response,
    })
}

#[cfg(test)]
mod tests;
