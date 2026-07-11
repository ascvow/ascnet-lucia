//! Internal model IR.
//! 内部模型中间表示。
//!
//! This is the only shape the ReAct loop understands. Provider adapters translate
//! this IR to OpenAI Responses, OpenAI Chat Completions, or Anthropic Messages.
//!
//! ReAct loop 只理解这一套结构。具体服务商适配器负责把它转换为
//! OpenAI Responses、OpenAI Chat Completions 或 Anthropic Messages。

use agent_tool::{ToolCall, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Message role in the provider-neutral transcript.
/// 与服务商无关的会话角色。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// Convert to a common string representation.
    /// 转为常见字符串表示。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// Content block in a message.
/// 消息中的内容块。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content.
    /// 普通文本内容。
    Text { text: String },

    /// Thinking / reasoning content emitted by the model.
    /// 模型输出的思维链/推理内容。
    ///
    /// `signature` 用于跨轮次复用思维上下文（Anthropic `thinkingSignature`、
    /// OpenAI 加密推理签名等）。跨 provider 转换时应丢弃 signature 并将
    /// thinking 降级为纯文本。
    Thinking {
        thinking: String,
        signature: Option<String>,
    },

    /// Tool call block emitted by the model.
    /// 模型输出的工具调用块。
    ToolCall { call: ToolCall },

    /// 宿主工具或扩展返回的工具结果块。
    ToolResult { result: ToolResult },
}

impl ContentBlock {
    /// Extract text if this is a text block.
    /// 如果是文本块，则取出文本。
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Extract thinking content if this is a thinking block.
    /// 如果是思维链块，则取出推理内容。
    pub fn thinking(&self) -> Option<&str> {
        match self {
            Self::Thinking { thinking, .. } => Some(thinking),
            _ => None,
        }
    }
}

/// A message in the provider-neutral transcript.
/// 与服务商无关的会话消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

impl ModelMessage {
    /// Create a text message.
    /// 创建文本消息。
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create an assistant message that contains model tool calls.
    /// 创建包含模型工具调用的 assistant 消息。
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: calls
                .into_iter()
                .map(|call| ContentBlock::ToolCall { call })
                .collect(),
        }
    }

    /// Create a tool result message from a single result.
    /// 根据单个结果创建工具消息。
    pub fn tool_result(result: ToolResult) -> Self {
        Self::tool_results(vec![result])
    }

    /// Create a tool result message from multiple results.
    /// 根据多个结果创建工具消息。
    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: results
                .into_iter()
                .map(|result| ContentBlock::ToolResult { result })
                .collect(),
        }
    }

    /// Concatenate text blocks.
    /// 拼接所有文本块。
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Concatenate thinking blocks.
    /// 拼接所有思维链块。
    pub fn thinking_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::thinking)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 推理/思维链级别。
///
/// 参考 pi-ai `ThinkingLevel`，提供跨 provider 的统一推理控制。
/// 各适配器负责将该枚举映射为 provider 特有参数：
/// - Anthropic：`thinking.budget_tokens` + `thinking.type`
/// - OpenAI Responses：`reasoning.effort`
/// - Chat Completions：静默忽略
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    /// 不启用推理。
    #[default]
    Off,
    /// 低预算推理。
    Low,
    /// 中等预算推理。
    Medium,
    /// 高预算推理。
    High,
}

/// How the model should use tools.
/// 模型应该如何使用工具。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model decide.
    /// 让模型自行决定。
    Auto,

    /// Prevent tool calls.
    /// 禁止工具调用。
    None,

    /// Require at least one tool call.
    /// 要求至少调用一个工具。
    Required,

    /// Force a specific tool.
    /// 强制调用指定工具。
    Tool { name: String },
}

impl Default for ToolChoice {
    fn default() -> Self {
        Self::Auto
    }
}

/// Request sent from ReAct loop to a model adapter.
/// ReAct loop 发送给模型适配器的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,

    /// 推理/思维链级别。各适配器负责映射为 provider 特有参数。
    #[serde(default)]
    pub reasoning: ReasoningLevel,

    /// Provider-specific escape hatch. Adapters shallow-merge this object into the wire request.
    /// 服务商专属逃生口。适配器会把这个对象浅合并到实际网络请求里。
    #[serde(default)]
    pub provider_options: Value,
}

impl ModelRequest {
    /// Construct a basic request.
    /// 构造基础请求。
    pub fn new(model: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_tokens: None,
            temperature: None,
            reasoning: ReasoningLevel::Off,
            provider_options: Value::Object(Default::default()),
        }
    }
}

/// Why the model stopped.
/// 模型停止生成的原因。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Refusal,
    Error,
    Unknown,
}

/// Token usage, if a provider returns it.
/// Token 用量；如果服务商返回该信息则填充。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl TokenUsage {
    /// Add usage from another model response.
    /// 累加另一次模型响应的用量。
    pub fn add_assign(&mut self, other: &TokenUsage) {
        add_token_field(&mut self.input_tokens, other.input_tokens);
        add_token_field(&mut self.output_tokens, other.output_tokens);

        let other_total =
            other
                .total_tokens
                .or_else(|| match (other.input_tokens, other.output_tokens) {
                    (Some(input), Some(output)) => Some(input + output),
                    _ => None,
                });
        add_token_field(&mut self.total_tokens, other_total);

        // Some providers omit total_tokens. Derive it when input and output are both known.
        // 某些服务商不会返回 total_tokens；如果 input/output 都已知，则自动推导。
        if self.total_tokens.is_none() {
            if let (Some(input), Some(output)) = (self.input_tokens, self.output_tokens) {
                self.total_tokens = Some(input + output);
            }
        }
    }

    /// Return true when no usage field is present.
    /// 当没有任何用量字段时返回 true。
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none() && self.output_tokens.is_none() && self.total_tokens.is_none()
    }
}

fn add_token_field(target: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *target = Some(target.unwrap_or(0) + value);
    }
}

/// Provider-reported billing or cost fields, if the API returns them.
/// 服务商接口返回的计费或费用字段；如果接口返回则填充。
///
/// The core does not estimate or calculate local cost. It only normalizes a few
/// common fields and preserves the provider-reported billing object in `fields`.
/// core 不做本地费用估算或计算。这里只归一化少量常见字段，并把服务商返回的
/// 计费字段原样保存在 `fields` 中。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderBilling {
    /// Best-effort amount parsed from provider fields such as cost/amount/charge.
    /// 从 cost/amount/charge 等服务商字段中尽力解析出的金额。
    pub amount: Option<f64>,

    /// Currency code reported by the provider, if any.
    /// 服务商返回的货币代码，如果存在。
    pub currency: Option<String>,

    /// Provider-reported billing/cost-related fields, keyed by dotted JSON path.
    /// 服务商返回的计费/费用相关字段，key 为点分 JSON 路径。
    pub fields: Value,
}

impl ProviderBilling {
    /// Extract billing/cost fields from a raw provider response.
    /// 从原始服务商响应中提取计费/费用字段。
    pub fn from_provider_response(response: &Value) -> Option<Self> {
        let mut fields = Map::new();
        collect_billing_fields("", response, &mut fields, 0);
        if fields.is_empty() {
            return None;
        }

        let amount = find_amount(&fields);
        let currency = find_currency(&fields);
        Some(Self {
            amount,
            currency,
            fields: Value::Object(fields),
        })
    }

    /// Return true if no billing data is available.
    /// 如果没有任何计费数据则返回 true。
    pub fn is_empty(&self) -> bool {
        self.amount.is_none()
            && self.currency.is_none()
            && self.fields.as_object().map_or(true, Map::is_empty)
    }
}

fn collect_billing_fields(
    prefix: &str,
    value: &Value,
    fields: &mut Map<String, Value>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }

    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };

                if is_billing_key(key) {
                    fields.insert(path.clone(), child.clone());
                }
                collect_billing_fields(&path, child, fields, depth + 1);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_billing_fields(&format!("{prefix}[{index}]"), child, fields, depth + 1);
            }
        }
        _ => {}
    }
}

fn is_billing_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("billing")
        || key.contains("billable")
        || key.contains("cost")
        || key.contains("amount")
        || key.contains("charge")
        || key.contains("currency")
        || key.contains("price")
        || key.contains("spent")
        || key.contains("spend")
        || key.contains("fee")
}

fn find_amount(fields: &Map<String, Value>) -> Option<f64> {
    let priority = [
        "total_cost",
        "total_amount",
        "billed_amount",
        "billable_cost",
        "cost",
        "amount",
        "charge",
        "spent",
        "fee",
        "price",
    ];

    for needle in priority {
        for (path, value) in fields {
            let lower = path.to_ascii_lowercase();
            if lower.ends_with(needle) || lower.contains(&format!(".{needle}")) {
                if let Some(amount) = value_as_f64(value) {
                    return Some(amount);
                }
            }
        }
    }

    None
}

fn find_currency(fields: &Map<String, Value>) -> Option<String> {
    for (path, value) in fields {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with("currency") || lower.ends_with("currency_code") {
            if let Some(currency) = value.as_str() {
                if !currency.trim().is_empty() {
                    return Some(currency.to_string());
                }
            }
        }
    }
    None
}

fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

/// Provider-neutral response.
/// 与服务商无关的模型响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Option<TokenUsage>,

    /// Provider-reported billing or cost fields, if present.
    /// 服务商返回的计费或费用字段，如果存在则填充。
    pub billing: Option<ProviderBilling>,

    /// Raw provider response for debugging. Keep disabled in logs unless redacted.
    /// 原始服务商响应，便于调试。写日志前需要脱敏。
    pub raw_provider_response: Option<Value>,
}

impl ModelResponse {
    /// Build a final text response.
    /// 构造最终文本响应。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
            billing: None,
            raw_provider_response: None,
        }
    }

    /// Build a response that requests tool calls.
    /// 构造请求工具调用的响应。
    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            content: calls
                .iter()
                .cloned()
                .map(|call| ContentBlock::ToolCall { call })
                .collect(),
            tool_calls: calls,
            finish_reason: FinishReason::ToolCalls,
            usage: None,
            billing: None,
            raw_provider_response: None,
        }
    }

    /// Concatenate all text blocks.
    /// 拼接所有文本块。
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
