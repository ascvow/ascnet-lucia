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

/// 服务商私有推理块的来源模型路由。
///
/// 私有签名只允许回传给生成该块的同一逻辑 provider、模型与规范化端点；该类型记录
/// 不含凭据的端点指纹，不包含 API Key、原始 URL 或其他连接配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ModelRoute {
    /// ModelGateway 中注册的逻辑 provider 名称。
    provider: String,
    /// 生成私有推理块时使用的模型 ID。
    model: String,
    /// 适配器提供的不透明端点指纹；旧会话缺失时不能匹配带指纹的新路由。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

/// 封装在既有 `Thinking.signature` 字段中的服务商私有推理载荷。
///
/// 该内部信封避免给公开 [`ContentBlock`] 增加变体或字段，同时保留来源路由、原始签名
/// 和加密推理块的有序会话语义。外部调用方应把 `signature` 始终视为不透明字符串。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PrivateReasoning {
    /// 可带空展示文本、但必须原样回传服务商签名的推理块。
    SignedThinking {
        /// 服务商返回的原始签名。
        signature: String,
        /// 生成该签名的完整来源路由。
        source_route: ModelRoute,
    },
    /// 服务商加密后不可展示、但工具循环要求原样回传的推理块。
    RedactedThinking {
        /// 服务商返回的不透明加密数据。
        data: String,
        /// 生成该加密块的完整来源路由。
        source_route: ModelRoute,
    },
}

impl PrivateReasoning {
    /// 返回私有载荷绑定的完整来源路由。
    pub(crate) fn source_route(&self) -> &ModelRoute {
        match self {
            Self::SignedThinking { source_route, .. }
            | Self::RedactedThinking { source_route, .. } => source_route,
        }
    }
}

/// 内部私有推理信封的版本前缀；未知或旧版原始签名按未绑定来源处理。
const PRIVATE_REASONING_PREFIX: &str = "lucia-private-reasoning-v1:";

/// 把来源绑定的私有推理编码进公开协议既有的不透明签名字段。
#[cfg(any(feature = "anthropic", test))]
pub(crate) fn encode_private_reasoning(reasoning: &PrivateReasoning) -> String {
    // 该枚举只包含字符串与结构体，序列化为 JSON Value 不存在失败分支。
    let value = serde_json::to_value(reasoning).expect("私有推理信封必须可序列化");
    format!("{PRIVATE_REASONING_PREFIX}{value}")
}

/// 从既有签名字段解析当前版本的内部私有推理信封。
///
/// 返回 `None` 表示普通签名、旧会话签名或无效信封；调用方必须按来源未知关闭复用。
pub(crate) fn decode_private_reasoning(signature: &str) -> Option<PrivateReasoning> {
    let encoded = signature.strip_prefix(PRIVATE_REASONING_PREFIX)?;
    serde_json::from_str(encoded).ok()
}

impl ModelRoute {
    /// 创建不包含连接凭据的模型路由标识。
    #[cfg(test)]
    pub(crate) fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            scope: None,
        }
    }

    /// 创建带非秘密适配器端点指纹的精确模型路由。
    #[cfg(feature = "anthropic")]
    pub(crate) fn scoped(
        provider: impl Into<String>,
        model: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            scope: Some(scope.into()),
        }
    }

    /// 判断目标逻辑 provider 与模型是否一致；端点 scope 必须在适配器边界另行比较。
    pub(crate) fn matches_logical_route(&self, provider: &str, model: &str) -> bool {
        self.provider == provider && self.model == model
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
    /// OpenAI 加密推理签名等）。该字段是不透明载荷，可能由 Core 封装来源绑定；调用方
    /// 不应解析或修改。带签名的块只能回传给其来源路由，跨路由时整个块必须丢弃，
    /// 不能把私有推理自动转换为可见文本。
    Thinking {
        /// 模型返回的完整推理文本；是否向最终用户展示由应用决定。
        thinking: String,
        /// 跨轮验证推理块的可选不透明签名，只能回传给同一路由。
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
    /// 创建绑定完整来源路由的服务商签名推理块。
    #[cfg(any(feature = "anthropic", test))]
    pub(crate) fn private_signed_thinking(
        thinking: impl Into<String>,
        signature: impl Into<String>,
        source_route: ModelRoute,
    ) -> Self {
        let private = PrivateReasoning::SignedThinking {
            signature: signature.into(),
            source_route,
        };
        Self::Thinking {
            thinking: thinking.into(),
            signature: Some(encode_private_reasoning(&private)),
        }
    }

    /// 创建绑定完整来源路由的服务商加密推理块。
    #[cfg(any(feature = "anthropic", test))]
    pub(crate) fn private_redacted_thinking(
        data: impl Into<String>,
        source_route: ModelRoute,
    ) -> Self {
        let private = PrivateReasoning::RedactedThinking {
            data: data.into(),
            source_route,
        };
        Self::Thinking {
            thinking: String::new(),
            signature: Some(encode_private_reasoning(&private)),
        }
    }

    /// 解析当前块中由 Core 封装的服务商私有推理载荷。
    pub(crate) fn private_reasoning(&self) -> Option<PrivateReasoning> {
        let Self::Thinking {
            signature: Some(signature),
            ..
        } = self
        else {
            return None;
        };
        decode_private_reasoning(signature)
    }

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
    /// 最小预算推理。
    Minimal,
    /// 低预算推理。
    Low,
    /// 中等预算推理。
    Medium,
    /// 高预算推理。
    High,
    /// 超高预算推理。
    XHigh,
    /// 服务商允许的最大预算推理。
    Max,
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

/// 模型提示缓存的保留策略。
///
/// Provider 适配器负责把该中立策略映射为各自协议字段；不支持缓存的适配器可以忽略。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheRetention {
    /// 不发送任何显式提示缓存控制字段。
    None,
    /// 使用服务商默认的短期缓存保留时间。
    #[default]
    Short,
    /// 请求服务商支持的较长缓存保留时间。
    Long,
}

/// 目标网关的厂商类型。
///
/// 各家网关的提示缓存路由约定互不相同：官方 OpenAI 读 body 里的 `prompt_cache_key`，
/// OpenRouter 和多数兼容网关只认 HTTP header，且 header 名称还不一样。调用方声明网关
/// 厂商，适配器据此选择整套缓存行为，不需要逐项配置。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayKind {
    /// 官方 OpenAI API。
    OpenAi,
    /// OpenRouter 聚合网关。
    OpenRouter,
    /// 官方 Anthropic API。
    Anthropic,
    /// 其他 OpenAI 协议兼容网关；自建代理与第三方中转都归到这一类。
    #[default]
    OpenAiCompatible,
}

impl GatewayKind {
    /// 返回该网关使用的会话亲和 header 格式。
    pub fn session_affinity(self) -> SessionAffinity {
        match self {
            // 官方端点按 body 里的 prompt_cache_key 路由，额外 header 没有作用。
            Self::OpenAi | Self::Anthropic => SessionAffinity::Off,
            Self::OpenRouter => SessionAffinity::OpenRouter,
            Self::OpenAiCompatible => SessionAffinity::OpenAi,
        }
    }

    /// 返回该网关是否读取 body 中的 `prompt_cache_key`。
    ///
    /// 兼容网关大多忽略这个 OpenAI 专有字段，缓存路由完全依赖 header；对它们照发只是把
    /// 未知字段推给网关，没有收益。
    pub fn reads_prompt_cache_key(self) -> bool {
        matches!(self, Self::OpenAi)
    }
}

/// 服务商侧会话亲和 header 的格式，由 [`GatewayKind`] 决定。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAffinity {
    /// 不发送任何会话亲和 header。
    #[default]
    Off,
    /// 发送 `session_id`、`x-client-request-id` 和 `x-session-affinity`。
    OpenAi,
    /// 只发送 `x-client-request-id` 和 `x-session-affinity`。
    OpenAiNoSession,
    /// 发送 `x-session-id`。
    OpenRouter,
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

    /// 本轮提示缓存保留策略；默认使用服务商的短期缓存。
    #[serde(default)]
    pub cache_retention: PromptCacheRetention,

    /// 会话级路由标识，供支持会话亲和的服务商做提示缓存分组。
    ///
    /// 同一会话的所有请求必须使用同一个值，不同会话必须不同：服务商据此把请求路由到
    /// 持有该会话前缀缓存的机器。取值应当是稳定的会话 ID，不能按请求内容推导——内容
    /// 一变键就变，路由随之改变，缓存反而全部落空。`None` 表示不发送该字段，由服务商
    /// 自行按前缀隐式缓存。
    #[serde(default)]
    pub session_id: Option<String>,

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
            cache_retention: PromptCacheRetention::Short,
            session_id: None,
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
    /// 未从提示缓存读取或写入的输入 token 数；未提供时为 `None`。
    pub input_tokens: Option<u64>,
    /// 服务商报告的输出 token 数；未提供时为 `None`。
    pub output_tokens: Option<u64>,
    /// 服务商报告或由输入输出相加得到的总 token 数。
    pub total_tokens: Option<u64>,
    /// 从服务商提示缓存读取的 token 数；未提供时为 `None`。
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    /// 本次写入服务商提示缓存的 token 数；未提供时为 `None`。
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

impl TokenUsage {
    /// Add usage from another model response.
    /// 累加另一次模型响应的用量。
    pub fn add_assign(&mut self, other: &TokenUsage) {
        add_token_field(&mut self.input_tokens, other.input_tokens);
        add_token_field(&mut self.output_tokens, other.output_tokens);
        add_token_field(&mut self.cache_read_tokens, other.cache_read_tokens);
        add_token_field(&mut self.cache_write_tokens, other.cache_write_tokens);

        let other_total =
            other
                .total_tokens
                .or_else(|| match (other.input_tokens, other.output_tokens) {
                    (Some(input), Some(output)) => Some(
                        input
                            .saturating_add(output)
                            .saturating_add(other.cache_read_tokens.unwrap_or(0))
                            .saturating_add(other.cache_write_tokens.unwrap_or(0)),
                    ),
                    _ => None,
                });
        add_token_field(&mut self.total_tokens, other_total);

        // 某些服务商不会返回 total_tokens；输入输出已知时按全部分项自动推导。
        if self.total_tokens.is_none() {
            if let (Some(input), Some(output)) = (self.input_tokens, self.output_tokens) {
                self.total_tokens = Some(
                    input
                        .saturating_add(output)
                        .saturating_add(self.cache_read_tokens.unwrap_or(0))
                        .saturating_add(self.cache_write_tokens.unwrap_or(0)),
                );
            }
        }
    }

    /// Return true when no usage field is present.
    /// 当没有任何用量字段时返回 true。
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.total_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
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
