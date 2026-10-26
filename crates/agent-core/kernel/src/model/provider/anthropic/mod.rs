//! Anthropic Messages API 协议适配器。

use super::support::{
    decode_json_response, endpoint_url, enforce_output_budget, file_attachment_fallback_text,
    header_map, merge_provider_options,
};
use crate::model::types::{ModelRoute, PrivateReasoning};
use crate::model::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelEventStream, ModelProviderConfig,
    ModelRequest, ModelResponse, ModelStreamEvent, PromptCacheRetention, ProviderAdapter,
    ProviderBilling, ReasoningLevel, TokenUsage, ToolChoice,
};
use agent_tool::{ToolCall, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic extended thinking 接受的最小思考预算。
const MIN_THINKING_BUDGET_TOKENS: u32 = 1_024;
/// 启用 extended thinking 时为最终可见回答保留的最小输出预算。
const MIN_VISIBLE_OUTPUT_TOKENS: u32 = 1_024;
/// Claude Code CLI 请求使用的客户端标识，用于兼容依赖官方客户端身份的模型网关。
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.80 (external, cli)";

/// Adapter for Anthropic Messages API.
/// Anthropic Messages API 适配器。
pub struct AnthropicMessagesAdapter {
    client: reqwest::Client,
    provider_name: String,
    route_scope: String,
    base_url: String,
    api_key: String,
    extra_headers: HeaderMap,
}

impl AnthropicMessagesAdapter {
    /// Create a new adapter.
    /// 创建适配器。
    pub fn new(config: ModelProviderConfig) -> Result<Self> {
        let base_url = config
            .base_url
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string());
        Ok(Self {
            client: super::support::http_client()?,
            provider_name: config.name,
            route_scope: anthropic_route_scope(&base_url)?,
            base_url,
            api_key: config.api_key,
            extra_headers: header_map(config.extra_headers)?,
        })
    }

    /// 使用可信逻辑 provider 构造精确来源路由并完成一次 Anthropic 请求。
    async fn complete_with_route(
        &self,
        logical_provider: &str,
        req: ModelRequest,
    ) -> Result<ModelResponse> {
        let source_route = ModelRoute::scoped(logical_provider, &req.model, &self.route_scope);
        let body = build_messages_request_for_route(req, Some(&source_route))?;
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

        parse_messages_response(value, &source_route)
    }
}

#[async_trait]
impl ChatModel for AnthropicMessagesAdapter {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        self.complete_with_route(&self.provider_name, req).await
    }
}

/// 对不含凭据的规范化端点生成稳定指纹，防止同名 provider 重绑后复用旧签名。
fn anthropic_route_scope(base_url: &str) -> Result<String> {
    let mut endpoint =
        reqwest::Url::parse(base_url).context("Anthropic base_url 必须是绝对 URL")?;
    endpoint
        .set_password(None)
        .map_err(|_| anyhow!("Anthropic base_url 无法移除密码信息"))?;
    endpoint
        .set_username("")
        .map_err(|_| anyhow!("Anthropic base_url 无法移除用户信息"))?;
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let normalized_endpoint = endpoint.as_str().trim_end_matches('/');

    Ok(format!(
        "{:x}",
        Sha256::digest(normalized_endpoint.as_bytes())
    ))
}

#[async_trait]
impl ProviderAdapter for AnthropicMessagesAdapter {
    fn name(&self) -> &'static str {
        "anthropic-messages"
    }

    async fn complete_for_provider(
        &self,
        logical_provider: &str,
        req: ModelRequest,
    ) -> Result<ModelResponse> {
        self.complete_with_route(logical_provider, req).await
    }

    async fn stream_for_provider(
        &self,
        logical_provider: &str,
        req: ModelRequest,
    ) -> ModelEventStream {
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::Start);
        match self.complete_with_route(logical_provider, req).await {
            Ok(response) => tx.done(response),
            Err(error) => tx.fail(error),
        }
        stream
    }
}

#[cfg(test)]
fn build_messages_request(req: ModelRequest) -> Result<Value> {
    build_messages_request_for_route(req, None)
}

/// 构建 Anthropic 请求，并在最终协议边界再次校验私有推理块的目标路由。
fn build_messages_request_for_route(
    req: ModelRequest,
    target_route: Option<&ModelRoute>,
) -> Result<Value> {
    validate_private_reasoning_for_route(&req.messages, target_route)?;
    let trusted_max_tokens = req.max_tokens;
    let cache_control = anthropic_cache_control(req.cache_retention);
    let mut max_tokens = req.max_tokens.unwrap_or(4096);

    let mut body = json!({
        "model": &req.model,
        "messages": messages_to_anthropic(&req.messages, target_route),
        "stream": false,
    });

    if let Some(system) = req.system {
        body["system"] = match &cache_control {
            Some(cache_control) => json!([{
                "type": "text",
                "text": system,
                "cache_control": cache_control,
            }]),
            None => json!(system),
        };
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(tool_to_anthropic).collect());
        body["tool_choice"] = tool_choice_to_anthropic(&req.tool_choice);
        if let Some(cache_control) = &cache_control {
            if let Some(last_tool) = body["tools"]
                .as_array_mut()
                .and_then(|tools| tools.last_mut())
            {
                last_tool["cache_control"] = cache_control.clone();
            }
        }
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }

    apply_reasoning_budget(
        &mut body,
        req.reasoning,
        trusted_max_tokens,
        &mut max_tokens,
    )?;

    body["max_tokens"] = json!(max_tokens);

    if let Some(cache_control) = &cache_control {
        mark_last_anthropic_user_content(&mut body["messages"], cache_control);
    }

    merge_provider_options(&mut body, req.provider_options);
    if let Some(max_tokens) = trusted_max_tokens {
        // provider_options 属于扩展面，不能覆盖 Core 已审批并计入累计预算的硬上限。
        enforce_output_budget(&mut body, "max_tokens", max_tokens);
    }
    Ok(body)
}

/// 在最终 wire 构建边界拒绝会命中当前路由、但缺少有效私有载荷的 Assistant 块。
fn validate_private_reasoning_for_route(
    messages: &[crate::model::ModelMessage],
    target_route: Option<&ModelRoute>,
) -> Result<()> {
    let Some(target_route) = target_route else {
        return Ok(());
    };

    for message in messages {
        if message.role != MessageRole::Assistant {
            continue;
        }
        for block in &message.content {
            let Some(private) = block.private_reasoning() else {
                continue;
            };
            if private.source_route() != target_route {
                continue;
            }
            match private {
                PrivateReasoning::SignedThinking { signature, .. } => {
                    if signature.is_empty() {
                        return Err(anyhow!(
                            "Anthropic history thinking block missing signature"
                        ));
                    }
                }
                PrivateReasoning::RedactedThinking { data, .. } => {
                    if data.is_empty() {
                        return Err(anyhow!(
                            "Anthropic history redacted_thinking block missing data"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// 映射 Anthropic 思考档位，并保证显式输出上限不会被适配器隐式扩大。
///
/// 未设置 `trusted_max_tokens` 时保留旧的自动扩容行为；设置后会在该上限内压缩思考预算，
/// 同时至少保留一段可见回答预算。上限不足以满足 Anthropic 最小思考预算时返回错误。
fn apply_reasoning_budget(
    body: &mut Value,
    reasoning: ReasoningLevel,
    trusted_max_tokens: Option<u32>,
    max_tokens: &mut u32,
) -> Result<()> {
    let Some((desired_budget, automatic_max_tokens)) = (match reasoning {
        ReasoningLevel::Off => None,
        ReasoningLevel::Minimal => Some((1_024, 2_048)),
        ReasoningLevel::Low => Some((2_048, 4_096)),
        ReasoningLevel::Medium => Some((8_192, 12_288)),
        ReasoningLevel::High => Some((16_384, 20_480)),
        ReasoningLevel::XHigh => Some((32_768, 36_864)),
        ReasoningLevel::Max => Some((65_536, 69_632)),
    }) else {
        return Ok(());
    };

    let budget = match trusted_max_tokens {
        Some(limit) => {
            let available = limit
                .checked_sub(MIN_VISIBLE_OUTPUT_TOKENS)
                .ok_or_else(|| {
                    anyhow!(
                        "Anthropic 推理请求的 max_tokens 至少需要 {}，当前为 {limit}",
                        MIN_THINKING_BUDGET_TOKENS + MIN_VISIBLE_OUTPUT_TOKENS
                    )
                })?;
            if available < MIN_THINKING_BUDGET_TOKENS {
                return Err(anyhow!(
                    "Anthropic 推理请求的 max_tokens 至少需要 {}，当前为 {limit}",
                    MIN_THINKING_BUDGET_TOKENS + MIN_VISIBLE_OUTPUT_TOKENS
                ));
            }
            desired_budget.min(available)
        }
        None => {
            *max_tokens = (*max_tokens).max(automatic_max_tokens);
            desired_budget
        }
    };
    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    Ok(())
}

/// Anthropic 内容块所在的消息位置，私有推理只允许出现在 assistant 历史中。
#[derive(Clone, Copy)]
enum AnthropicContentMode {
    /// 用户输入以及可降级的普通内容。
    Input,
    /// Anthropic 模型先前返回的 assistant 内容。
    Assistant,
    /// 宿主执行工具后返回的结果。
    ToolResult,
}

impl AnthropicContentMode {
    /// 当前模式是否允许普通文本、附件和工具调用块。
    fn allows_regular_content(self) -> bool {
        !matches!(self, Self::ToolResult)
    }

    /// 当前模式是否允许回传经同路由校验的私有推理块。
    fn allows_private_reasoning(self) -> bool {
        matches!(self, Self::Assistant)
    }
}

fn messages_to_anthropic(
    messages: &[crate::model::ModelMessage],
    target_route: Option<&ModelRoute>,
) -> Value {
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
                let blocks = blocks_to_anthropic_content(
                    &message.content,
                    AnthropicContentMode::Input,
                    target_route,
                );
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
            }
            MessageRole::Assistant => {
                let blocks = blocks_to_anthropic_content(
                    &message.content,
                    AnthropicContentMode::Assistant,
                    target_route,
                );
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            MessageRole::Tool => {
                let blocks = blocks_to_anthropic_content(
                    &message.content,
                    AnthropicContentMode::ToolResult,
                    target_route,
                );
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
            }
        }
    }

    Value::Array(out)
}

fn blocks_to_anthropic_content(
    blocks: &[ContentBlock],
    mode: AnthropicContentMode,
    target_route: Option<&ModelRoute>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } if mode.allows_regular_content() => {
                out.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::Thinking { thinking, .. } if mode.allows_private_reasoning() => {
                let Some(private) = block.private_reasoning() else {
                    continue;
                };
                if !target_route.is_some_and(|target| target == private.source_route()) {
                    continue;
                }
                match private {
                    PrivateReasoning::SignedThinking { signature, .. } => {
                        out.push(json!({
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": signature,
                        }));
                    }
                    PrivateReasoning::RedactedThinking { data, .. } => {
                        out.push(json!({ "type": "redacted_thinking", "data": data }));
                    }
                }
            }
            ContentBlock::ToolCall { call } if mode.allows_regular_content() => {
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
            ContentBlock::Image { media_type, data } if mode.allows_regular_content() => {
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
            } if mode.allows_regular_content() => {
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

/// 把中立缓存保留策略转换为 Anthropic 显式缓存断点。
fn anthropic_cache_control(retention: PromptCacheRetention) -> Option<Value> {
    match retention {
        PromptCacheRetention::None => None,
        PromptCacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        PromptCacheRetention::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

/// 在最近一条 user/tool-result 消息的最后一个内容块上标记会话历史缓存断点。
fn mark_last_anthropic_user_content(messages: &mut Value, cache_control: &Value) {
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    let Some(message) = messages.iter_mut().rev().find(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|content| !content.is_empty())
    }) else {
        return;
    };
    let Some(last_content) = message
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|content| content.last_mut())
    else {
        return;
    };
    last_content["cache_control"] = cache_control.clone();
}

fn parse_messages_response(value: Value, source_route: &ModelRoute) -> Result<ModelResponse> {
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
                        .ok_or_else(|| anyhow!("Anthropic thinking block missing thinking"))?
                        .to_string();
                    let signature = block
                        .get("signature")
                        .and_then(Value::as_str)
                        .filter(|signature| !signature.is_empty())
                        .ok_or_else(|| anyhow!("Anthropic thinking block missing signature"))?
                        .to_string();
                    content.push(ContentBlock::private_signed_thinking(
                        thinking,
                        signature,
                        source_route.clone(),
                    ));
                }
                "redacted_thinking" => {
                    let data = block
                        .get("data")
                        .and_then(Value::as_str)
                        .filter(|data| !data.is_empty())
                        .ok_or_else(|| anyhow!("Anthropic redacted_thinking block missing data"))?;
                    content.push(ContentBlock::private_redacted_thinking(
                        data,
                        source_route.clone(),
                    ));
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

    let usage = value.get("usage").map(|usage| {
        let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
        let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
        let cache_read_tokens = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
        let cache_write_tokens = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64);
        let total_tokens = [
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
        ]
        .into_iter()
        .flatten()
        .reduce(u64::saturating_add);
        TokenUsage {
            input_tokens,
            output_tokens,
            total_tokens,
            cache_read_tokens,
            cache_write_tokens,
        }
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
mod tests;
