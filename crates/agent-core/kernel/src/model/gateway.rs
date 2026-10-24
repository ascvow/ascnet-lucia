//! ModelGateway routes provider-neutral requests to concrete protocol adapters.
//! ModelGateway 把与服务商无关的请求路由到具体协议适配器。

use super::{ChatModel, GatewayKind, ModelRequest, ModelResponse, ProviderAdapter};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// Provider family or protocol id.
/// 服务商协议类型。
///
/// 内置协议是封闭变体；其余字符串统一归入 [`ProviderKind::Custom`]，由应用通过
/// [`ModelGateway::register_factory`] 注册的自定义工厂构造适配器。序列化始终使用
/// kebab-case 协议 ID，未知 ID 反序列化为 [`ProviderKind::Custom`]，保证配置向后兼容。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// OpenAI 官方接口，可选择 Responses 或 Chat Completions 协议。
    OpenAi,
    /// 实现 OpenAI Chat Completions 兼容协议的第三方或本地服务。
    OpenAiCompatible,
    /// Anthropic Messages 接口。
    Anthropic,
    /// Google Generative AI（Gemini）generateContent 接口。
    Google,
    /// Amazon Bedrock Converse 接口。
    Bedrock,
    /// 未内置的协议 ID，由调用方注册的自定义工厂解析。
    Custom(String),
}

impl ProviderKind {
    /// 返回该协议的稳定 kebab-case ID，同时作为工厂注册表的键。
    pub fn protocol_id(&self) -> &str {
        match self {
            Self::OpenAi => "open-ai",
            Self::OpenAiCompatible => "open-ai-compatible",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Bedrock => "bedrock",
            Self::Custom(id) => id,
        }
    }
}

impl serde::Serialize for ProviderKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.protocol_id())
    }
}

impl<'de> serde::Deserialize<'de> for ProviderKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "open-ai" => Self::OpenAi,
            "open-ai-compatible" => Self::OpenAiCompatible,
            "anthropic" => Self::Anthropic,
            "google" => Self::Google,
            "bedrock" => Self::Bedrock,
            other => Self::Custom(other.to_string()),
        })
    }
}

/// OpenAI-compatible protocol variant.
/// OpenAI 兼容协议变体。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OpenAiProtocol {
    /// OpenAI Responses API；支持 Responses 特有的输入和推理字段。
    #[default]
    Responses,
    /// OpenAI Chat Completions API 及其兼容实现。
    ChatCompletions,
}

/// Configuration used to create a provider adapter.
/// 创建服务商适配器所需的配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// Logical provider name used by AgentOptions.provider, e.g. "openai".
    /// 逻辑服务商名称，由 AgentOptions.provider 引用，例如 "openai"。
    pub name: String,

    /// Provider family.
    /// 服务商类型。
    pub kind: ProviderKind,

    /// API key value. Prefer loading it from environment before building this struct.
    /// API key 值。建议先从环境变量读取后再构造该结构。
    pub api_key: String,

    /// Optional base URL. Do not include the endpoint path.
    /// 可选 base URL，不要包含具体 endpoint path。
    pub base_url: Option<String>,

    /// OpenAI protocol variant. Ignored for Anthropic.
    /// OpenAI 协议变体。Anthropic 会忽略这个字段。
    #[serde(default)]
    pub openai_protocol: OpenAiProtocol,

    /// Extra HTTP headers sent to the provider.
    /// 发送给服务商的额外 HTTP header。
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,

    /// 目标网关厂商；决定提示缓存键与会话亲和 header 的整套约定。
    ///
    /// 默认按通用 OpenAI 兼容网关处理。指向官方 OpenAI 或 OpenRouter 时应如实声明，
    /// 否则会按错误的约定发送缓存路由信息。
    #[serde(default)]
    pub gateway: GatewayKind,

    /// AWS 区域；仅 Bedrock 协议使用，例如 `us-east-1`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,

    /// AWS 访问密钥 ID；仅 Bedrock 协议使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_access_key_id: Option<String>,

    /// AWS 秘密访问密钥；仅 Bedrock 协议使用。建议通过环境变量注入后构造。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_secret_access_key: Option<String>,

    /// AWS 临时会话令牌；仅 Bedrock 协议使用，长期凭据留空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_session_token: Option<String>,
}

impl ModelProviderConfig {
    /// Build a generic provider config.
    /// 构建通用服务商配置。
    pub fn new(name: impl Into<String>, kind: ProviderKind, api_key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind,
            api_key: api_key.into(),
            base_url: None,
            openai_protocol: OpenAiProtocol::default(),
            extra_headers: HashMap::new(),
            gateway: GatewayKind::default(),
            aws_region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }
    }

    /// 声明目标网关厂商，决定提示缓存路由约定。
    pub fn with_gateway(mut self, gateway: GatewayKind) -> Self {
        self.gateway = gateway;
        self
    }

    /// Build an OpenAI Responses provider config.
    /// 构建 OpenAI Responses 服务商配置。
    pub fn openai(name: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new(name, ProviderKind::OpenAi, api_key)
            .with_openai_protocol(OpenAiProtocol::Responses)
    }

    /// Build an OpenAI-compatible Chat Completions provider config.
    /// 构建 OpenAI-compatible Chat Completions 服务商配置。
    pub fn openai_compatible(
        name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::new(name, ProviderKind::OpenAiCompatible, api_key)
            .with_base_url(base_url)
            .with_openai_protocol(OpenAiProtocol::ChatCompletions)
    }

    /// Build an Anthropic Messages provider config.
    /// 构建 Anthropic Messages 服务商配置。
    pub fn anthropic(name: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new(name, ProviderKind::Anthropic, api_key)
    }

    /// Build a Google Generative AI provider config.
    /// 构建 Google Generative AI 服务商配置。
    pub fn google(name: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new(name, ProviderKind::Google, api_key)
    }

    /// Build an Amazon Bedrock Converse provider config.
    ///
    /// `api_key` 字段对 Bedrock 无意义，凭据通过 AWS 字段提供。
    pub fn bedrock(
        name: impl Into<String>,
        region: impl Into<String>,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        Self {
            kind: ProviderKind::Bedrock,
            aws_region: Some(region.into()),
            aws_access_key_id: Some(access_key_id.into()),
            aws_secret_access_key: Some(secret_access_key.into()),
            ..Self::new(name, ProviderKind::Bedrock, String::new())
        }
    }

    /// Set base URL.
    /// 设置 base URL。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Set OpenAI protocol variant.
    /// 设置 OpenAI 协议变体。
    pub fn with_openai_protocol(mut self, protocol: OpenAiProtocol) -> Self {
        self.openai_protocol = protocol;
        self
    }

    /// Add one extra HTTP header.
    /// 添加一个额外 HTTP header。
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.insert(key.into(), value.into());
        self
    }
}

/// Registry and router for model providers.
/// 模型服务商注册表和路由器。
///
/// 网关同时持有协议适配器实例和协议工厂：内置协议工厂在 [`ModelGateway::new`]
/// 时注册，应用可通过 [`ModelGateway::register_factory`] 注册自定义协议工厂，
/// 使新协议在不修改网关的情况下接入（开闭原则）。
///
/// 注册表使用内部读写锁：克隆共享同一份注册状态，因此运行期通过 [`Self::upsert`]
/// 完成的热切换对全部克隆可见，包括派生 Agent 与插件模型路由。
#[derive(Clone, Default)]
pub struct ModelGateway {
    providers: Arc<RwLock<HashMap<String, Arc<dyn ProviderAdapter>>>>,
    factories: Arc<RwLock<HashMap<String, Arc<dyn ProviderAdapterFactory>>>>,
}

/// 根据服务商配置构造协议适配器的工厂。
///
/// 工厂按 [`ProviderKind::protocol_id`] 注册进 [`ModelGateway`]；一个工厂实例可以
/// 服务多种协议变体（例如 OpenAI 系同时覆盖官方与兼容网关），具体变体在
/// [`ProviderAdapterFactory::build`] 内部根据配置决定。
pub trait ProviderAdapterFactory: Send + Sync {
    /// 该工厂负责的协议类型。
    fn kind(&self) -> ProviderKind;

    /// 根据配置构造一个协议适配器。
    ///
    /// 错误：配置缺少该协议必需的字段（凭据、区域等）或对应协议 feature 未启用时返回。
    fn build(&self, config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>>;
}

/// OpenAI 系协议适配器工厂，覆盖官方 OpenAI 与 OpenAI 兼容网关。
#[cfg(feature = "openai")]
#[derive(Debug, Clone)]
struct OpenAiAdapterFactory {
    /// 工厂注册时声明的协议类型；同一实现按两种协议 ID 分别注册。
    kind: ProviderKind,
}

#[cfg(feature = "openai")]
impl ProviderAdapterFactory for OpenAiAdapterFactory {
    fn kind(&self) -> ProviderKind {
        self.kind.clone()
    }

    fn build(&self, config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>> {
        #[cfg(feature = "openai")]
        {
            use super::provider::openai::{OpenAiChatCompletionsAdapter, OpenAiResponsesAdapter};
            match config.openai_protocol.clone() {
                OpenAiProtocol::Responses => Ok(Arc::new(OpenAiResponsesAdapter::new(config)?)),
                OpenAiProtocol::ChatCompletions => {
                    Ok(Arc::new(OpenAiChatCompletionsAdapter::new(config)?))
                }
            }
        }

        #[cfg(not(feature = "openai"))]
        {
            let _ = config;
            anyhow::bail!("openai feature is disabled")
        }
    }
}

/// Anthropic Messages 协议适配器工厂。
#[cfg(feature = "anthropic")]
#[derive(Debug, Clone, Copy, Default)]
struct AnthropicAdapterFactory;

#[cfg(feature = "anthropic")]
impl ProviderAdapterFactory for AnthropicAdapterFactory {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn build(&self, config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>> {
        #[cfg(feature = "anthropic")]
        {
            use super::provider::anthropic::AnthropicMessagesAdapter;
            Ok(Arc::new(AnthropicMessagesAdapter::new(config)?))
        }

        #[cfg(not(feature = "anthropic"))]
        {
            let _ = config;
            anyhow::bail!("anthropic feature is disabled")
        }
    }
}

/// Google Generative AI 协议适配器工厂。
#[cfg(feature = "google")]
#[derive(Debug, Clone, Copy, Default)]
struct GoogleAdapterFactory;

#[cfg(feature = "google")]
impl ProviderAdapterFactory for GoogleAdapterFactory {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Google
    }

    fn build(&self, config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>> {
        #[cfg(feature = "google")]
        {
            use super::provider::google::GoogleGenerativeAdapter;
            Ok(Arc::new(GoogleGenerativeAdapter::new(config)?))
        }

        #[cfg(not(feature = "google"))]
        {
            let _ = config;
            anyhow::bail!("google feature is disabled")
        }
    }
}

/// Amazon Bedrock Converse 协议适配器工厂。
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Copy, Default)]
struct BedrockAdapterFactory;

#[cfg(feature = "bedrock")]
impl ProviderAdapterFactory for BedrockAdapterFactory {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Bedrock
    }

    fn build(&self, config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>> {
        #[cfg(feature = "bedrock")]
        {
            use super::provider::bedrock::BedrockConverseAdapter;
            Ok(Arc::new(BedrockConverseAdapter::new(config)?))
        }

        #[cfg(not(feature = "bedrock"))]
        {
            let _ = config;
            anyhow::bail!("bedrock feature is disabled")
        }
    }
}

impl ModelGateway {
    /// Create an empty gateway with built-in protocol factories registered.
    ///
    /// 创建空网关，并注册随 feature 启用的内置协议工厂。
    pub fn new() -> Self {
        let gateway = Self::default();
        #[cfg(feature = "openai")]
        {
            gateway.register_factory(OpenAiAdapterFactory {
                kind: ProviderKind::OpenAi,
            });
            gateway.register_factory(OpenAiAdapterFactory {
                kind: ProviderKind::OpenAiCompatible,
            });
        }
        #[cfg(feature = "anthropic")]
        gateway.register_factory(AnthropicAdapterFactory);
        #[cfg(feature = "google")]
        gateway.register_factory(GoogleAdapterFactory);
        #[cfg(feature = "bedrock")]
        gateway.register_factory(BedrockAdapterFactory);
        gateway
    }

    /// 注册或替换一个协议工厂；键取 `factory.kind().protocol_id()`。
    pub fn register_factory(&self, factory: impl ProviderAdapterFactory + 'static) -> &Self {
        self.factories
            .write()
            .expect("协议工厂注册表锁不应中毒")
            .insert(factory.kind().protocol_id().to_string(), Arc::new(factory));
        self
    }

    /// Build a gateway with one provider config.
    /// 使用一个服务商配置构建网关。
    pub fn from_config(config: ModelProviderConfig) -> Result<Self> {
        let gateway = Self::new();
        gateway.register_from_config(config)?;
        Ok(gateway)
    }

    /// Register an adapter under a logical provider name.
    /// 使用逻辑服务商名称注册适配器。
    ///
    /// # Errors
    ///
    /// 名称非法或已被注册时返回错误。
    pub fn register(
        &self,
        provider_name: impl Into<String>,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Result<&Self> {
        let provider_name = provider_name.into();
        validate_provider_name(&provider_name)?;
        let mut providers = self.providers.write().expect("服务商注册表锁不应中毒");
        if providers.contains_key(&provider_name) {
            return Err(anyhow!("duplicated provider: {provider_name}"));
        }
        providers.insert(provider_name, adapter);
        Ok(self)
    }

    /// Register or replace an adapter under a logical provider name.
    /// 使用逻辑服务商名称注册或替换适配器；运行期热切换上游使用该入口。
    ///
    /// # Errors
    ///
    /// 名称非法时返回错误。
    pub fn upsert(
        &self,
        provider_name: impl Into<String>,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Result<&Self> {
        let provider_name = provider_name.into();
        validate_provider_name(&provider_name)?;
        self.providers
            .write()
            .expect("服务商注册表锁不应中毒")
            .insert(provider_name, adapter);
        Ok(self)
    }

    /// Create and register an adapter from config.
    /// 根据配置创建并注册适配器。
    ///
    /// # Errors
    ///
    /// 名称重复或适配器构造失败时返回错误。
    pub fn register_from_config(&self, config: ModelProviderConfig) -> Result<&Self> {
        let name = config.name.clone();
        let adapter = self.adapter_from_config(config)?;
        self.register(name, adapter)
    }

    /// Create and register-or-replace an adapter from config.
    /// 根据配置创建并注册或替换适配器。
    ///
    /// # Errors
    ///
    /// 适配器构造失败时返回错误。
    pub fn upsert_from_config(&self, config: ModelProviderConfig) -> Result<&Self> {
        let name = config.name.clone();
        let adapter = self.adapter_from_config(config)?;
        self.upsert(name, adapter)
    }

    /// Remove one provider.
    /// 移除一个服务商。
    pub fn remove(&self, provider_name: &str) -> Option<Arc<dyn ProviderAdapter>> {
        self.providers
            .write()
            .expect("服务商注册表锁不应中毒")
            .remove(provider_name)
    }

    /// Build an adapter from the registered protocol factories.
    ///
    /// 根据已注册的协议工厂构造适配器。错误：协议 ID 没有对应工厂（未启用 feature
    /// 或未注册自定义工厂）时返回。
    ///
    /// # Errors
    ///
    /// 协议未注册或工厂构造失败时返回错误。
    fn adapter_from_config(&self, config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>> {
        let protocol_id = config.kind.protocol_id();
        let factory = self
            .factories
            .read()
            .expect("协议工厂注册表锁不应中毒")
            .get(protocol_id)
            .ok_or_else(|| {
                anyhow!("unsupported provider protocol `{protocol_id}`: no factory registered")
            })?
            .clone();
        factory.build(config)
    }

    /// Call one registered provider.
    /// 调用一个已注册服务商。
    ///
    /// # Errors
    ///
    /// provider 未注册时返回错误；模型调用错误原样传播。
    pub async fn complete(&self, provider: &str, req: ModelRequest) -> Result<ModelResponse> {
        let adapter = self
            .providers
            .read()
            .expect("服务商注册表锁不应中毒")
            .get(provider)
            .ok_or_else(|| anyhow!("unknown model provider: {provider}"))?
            .clone();
        // 逻辑路由只从 Gateway 的实际注册键传入，不能信任请求或插件自行声明。
        adapter.complete_for_provider(provider, req).await
    }

    /// 流式调用一个已注册服务商。
    ///
    /// 错误：provider 未注册时返回 Err；模型调用错误通过事件流传递。
    pub async fn stream(
        &self,
        provider: &str,
        req: ModelRequest,
    ) -> Result<super::stream::ModelEventStream> {
        let adapter = self
            .providers
            .read()
            .expect("服务商注册表锁不应中毒")
            .get(provider)
            .ok_or_else(|| anyhow!("unknown model provider: {provider}"))?
            .clone();
        // 与非流式路径保持相同的可信路由绑定。
        Ok(adapter.stream_for_provider(provider, req).await)
    }

    /// Whether a provider exists.
    /// 服务商是否存在。
    pub fn contains(&self, provider: &str) -> bool {
        self.providers
            .read()
            .expect("服务商注册表锁不应中毒")
            .contains_key(provider)
    }

    /// Return provider names.
    /// 返回所有服务商名称。
    pub fn provider_names(&self) -> Vec<String> {
        self.providers
            .read()
            .expect("服务商注册表锁不应中毒")
            .keys()
            .cloned()
            .collect()
    }
}

fn validate_provider_name(provider_name: &str) -> Result<()> {
    if provider_name.trim().is_empty() {
        return Err(anyhow!("provider name cannot be empty"));
    }
    Ok(())
}

#[async_trait]
impl ChatModel for ModelGateway {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        self.complete("default", req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelEventStream;
    use std::sync::Mutex;

    /// 记录 Gateway 传入的可信注册别名，普通 ChatModel 路径不参与断言。
    struct RoutedProvider {
        observed: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ChatModel for RoutedProvider {
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse::text("未路由"))
        }
    }

    #[async_trait]
    impl ProviderAdapter for RoutedProvider {
        fn name(&self) -> &'static str {
            "routed-test"
        }

        async fn complete_for_provider(
            &self,
            logical_provider: &str,
            _req: ModelRequest,
        ) -> Result<ModelResponse> {
            self.observed
                .lock()
                .expect("路由记录锁不应中毒")
                .push(logical_provider.to_string());
            Ok(ModelResponse::text("完成"))
        }

        async fn stream_for_provider(
            &self,
            logical_provider: &str,
            _req: ModelRequest,
        ) -> ModelEventStream {
            self.observed
                .lock()
                .expect("路由记录锁不应中毒")
                .push(logical_provider.to_string());
            let (sender, stream) = ModelEventStream::channel();
            sender.done(ModelResponse::text("完成"));
            stream
        }
    }

    /// 自定义注册别名必须由 Gateway 同时绑定到非流式与流式适配器调用。
    #[tokio::test]
    async fn gateway_binds_registered_provider_alias() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let gateway = ModelGateway::new();
        gateway
            .register(
                "custom-anthropic",
                Arc::new(RoutedProvider {
                    observed: Arc::clone(&observed),
                }),
            )
            .expect("应注册自定义 provider 别名");

        gateway
            .complete(
                "custom-anthropic",
                ModelRequest::new("test-model", Vec::new()),
            )
            .await
            .expect("非流式路由应成功");
        gateway
            .stream(
                "custom-anthropic",
                ModelRequest::new("test-model", Vec::new()),
            )
            .await
            .expect("流式路由应成功")
            .result()
            .await
            .expect("流式响应应成功");

        assert_eq!(
            *observed.lock().expect("路由记录锁不应中毒"),
            vec!["custom-anthropic", "custom-anthropic"]
        );
    }
}
