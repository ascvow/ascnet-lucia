//! Provider-neutral 模型请求、消息、内容块、响应和计量类型。
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
    /// 最高优先级的系统指令，由应用或 Agent 配置提供。
    System,
    /// 应用或扩展注入的开发者指令，优先级低于系统指令。
    Developer,
    /// 最终用户输入，包括文本和附件。
    User,
    /// 模型生成的文本、推理或工具调用。
    Assistant,
    /// 宿主执行工具后返回给模型的结果。
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
    Text {
        /// UTF-8 文本；适配器负责按目标协议进行转义和分块。
        text: String,
    },

    /// Thinking / reasoning content emitted by the model.
    /// 模型输出的思维链/推理内容。
    ///
    /// `signature` 用于跨轮次复用思维上下文（Anthropic `thinkingSignature`、
    /// OpenAI 加密推理签名等）。跨 provider 转换时应丢弃 signature 并将
    /// thinking 降级为纯文本。
    Thinking {
        /// 模型返回的完整推理文本；是否向最终用户展示由应用决定。
        thinking: String,
        /// 服务商用于跨轮验证推理块的可选签名，只能回传给同一服务商。
        signature: Option<String>,
    },

    /// Tool call block emitted by the model.
    /// 模型输出的工具调用块。
    ToolCall {
        /// 模型请求宿主执行的结构化工具调用。
        call: ToolCall,
    },

    /// 宿主工具或扩展返回的工具结果块。
    ToolResult {
        /// 与先前调用 ID 对应的成功值或错误结果。
        result: ToolResult,
    },

    /// 用户提供的图片附件。
    ///
    /// `data` 为 base64 编码的原始图片字节；`media_type` 为标准 MIME 类型
    /// （如 `image/png`）。适配器负责映射为各 provider 的图片输入格式。
    Image {
        /// 图片 MIME 类型，例如 `image/png` 或 `image/jpeg`。
        media_type: String,
        /// 原始图片字节的 base64 编码，不包含 data URL 前缀。
        data: String,
    },

    /// 用户提供的文件附件。
    ///
    /// `data` 为 base64 编码的原始文件字节。适配器按 `media_type` 选择映射：
    /// 原生支持的类型（如 PDF）走 provider 文档输入，文本类型内联为文本，
    /// 其余类型降级为占位说明，不会静默丢弃。
    File {
        /// 提供给模型和诊断事件的原始文件名，不作为宿主文件路径使用。
        name: String,
        /// 文件 MIME 类型，用于选择 provider 输入映射。
        media_type: String,
        /// 原始文件字节的 base64 编码，不包含 data URL 前缀。
        data: String,
    },
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
    /// 消息在会话协议中的角色，决定适配器使用的目标消息类型。
    pub role: MessageRole,
    /// 有序内容块；顺序在跨服务商转换时必须保持。
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model decide.
    /// 让模型自行决定。
    #[default]
    Auto,

    /// Prevent tool calls.
    /// 禁止工具调用。
    None,

    /// Require at least one tool call.
    /// 要求至少调用一个工具。
    Required,

    /// Force a specific tool.
    /// 强制调用指定工具。
    Tool {
        /// 必须调用的公开工具名称；应与请求中的 [`ToolSpec`] 名称一致。
        name: String,
    },
}

/// Request sent from ReAct loop to a model adapter.
/// ReAct loop 发送给模型适配器的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// 发送给服务商的模型 ID，不是逻辑 provider 名称。
    pub model: String,
    /// 本轮系统提示；适配器按目标协议放入专用字段或系统消息。
    pub system: Option<String>,
    /// 组成模型上下文的有序消息列表。
    pub messages: Vec<ModelMessage>,
    /// 本轮允许模型调用的工具定义。
    pub tools: Vec<ToolSpec>,
    /// 自动、禁用或强制指定工具的选择策略。
    pub tool_choice: ToolChoice,
    /// 本轮最大输出 token 数；`None` 使用服务商默认值。
    pub max_tokens: Option<u32>,
    /// 本轮采样温度；`None` 使用服务商默认值。
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
    /// 模型自然停止或命中停止序列。
    Stop,
    /// 模型要求执行一个或多个工具调用。
    ToolCalls,
    /// 响应达到输出长度限制。
    Length,
    /// 模型拒绝处理请求。
    Refusal,
    /// 服务商报告请求或生成错误。
    Error,
    /// 服务商返回了当前适配器无法识别的停止原因。
    Unknown,
}

/// Token usage, if a provider returns it.
/// Token 用量；如果服务商返回该信息则填充。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// 服务商报告的输入 token 数；未提供时为 `None`。
    pub input_tokens: Option<u64>,
    /// 服务商报告的输出 token 数；未提供时为 `None`。
    pub output_tokens: Option<u64>,
    /// 服务商报告或由输入输出相加得到的总 token 数。
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
            && self.fields.as_object().is_none_or(Map::is_empty)
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
    /// 服务商无关的有序内容块。
    pub content: Vec<ContentBlock>,
    /// 从内容块中提取并规范化后的工具调用列表。
    pub tool_calls: Vec<ToolCall>,
    /// 规范化后的停止原因，用于决定 ReAct 是否继续。
    pub finish_reason: FinishReason,
    /// 服务商返回的 token 用量；服务商未提供时为 `None`。
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
