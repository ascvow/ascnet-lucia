//! ModelGateway routes provider-neutral requests to concrete protocol adapters.
//! ModelGateway 把与服务商无关的请求路由到具体协议适配器。

use super::{ChatModel, ModelRequest, ModelResponse, ProviderAdapter};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

/// Provider family.
/// 服务商类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// OpenAI 官方接口，可选择 Responses 或 Chat Completions 协议。
    OpenAi,
    /// 实现 OpenAI Chat Completions 兼容协议的第三方或本地服务。
    OpenAiCompatible,
    /// Anthropic Messages 接口。
    Anthropic,
}

/// OpenAI-compatible protocol variant.
/// OpenAI 兼容协议变体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OpenAiProtocol {
    /// OpenAI Responses API；支持 Responses 特有的输入和推理字段。
    Responses,
    /// OpenAI Chat Completions API 及其兼容实现。
    ChatCompletions,
}

impl Default for OpenAiProtocol {
    fn default() -> Self {
        Self::Responses
    }
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
        }
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
#[derive(Clone, Default)]
pub struct ModelGateway {
    providers: HashMap<String, Arc<dyn ProviderAdapter>>,
}

impl ModelGateway {
    /// Create an empty gateway.
    /// 创建空网关。
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a gateway with one provider config.
    /// 使用一个服务商配置构建网关。
    pub fn from_config(config: ModelProviderConfig) -> Result<Self> {
        let mut gateway = Self::new();
        gateway.register_from_config(config)?;
        Ok(gateway)
    }

    /// Register an adapter under a logical provider name.
    /// 使用逻辑服务商名称注册适配器。
    pub fn register(
        &mut self,
        provider_name: impl Into<String>,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Result<&mut Self> {
        let provider_name = provider_name.into();
        validate_provider_name(&provider_name)?;
        if self.providers.contains_key(&provider_name) {
            return Err(anyhow!("duplicated provider: {provider_name}"));
        }
        self.providers.insert(provider_name, adapter);
        Ok(self)
    }

    /// Register or replace an adapter under a logical provider name.
    /// 使用逻辑服务商名称注册或替换适配器。
    pub fn upsert(
        &mut self,
        provider_name: impl Into<String>,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Result<&mut Self> {
        let provider_name = provider_name.into();
        validate_provider_name(&provider_name)?;
        self.providers.insert(provider_name, adapter);
        Ok(self)
    }

    /// Create and register an adapter from config.
    /// 根据配置创建并注册适配器。
    pub fn register_from_config(&mut self, config: ModelProviderConfig) -> Result<&mut Self> {
        let name = config.name.clone();
        let adapter = Self::adapter_from_config(config)?;
        self.register(name, adapter)
    }

    /// Create and register-or-replace an adapter from config.
    /// 根据配置创建并注册或替换适配器。
    pub fn upsert_from_config(&mut self, config: ModelProviderConfig) -> Result<&mut Self> {
        let name = config.name.clone();
        let adapter = Self::adapter_from_config(config)?;
        self.upsert(name, adapter)
    }

    /// Remove one provider.
    /// 移除一个服务商。
    pub fn remove(&mut self, provider_name: &str) -> Option<Arc<dyn ProviderAdapter>> {
        self.providers.remove(provider_name)
    }

    /// Build an adapter from config.
    /// 根据配置构造适配器。
    pub fn adapter_from_config(config: ModelProviderConfig) -> Result<Arc<dyn ProviderAdapter>> {
        match config.kind.clone() {
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => {
                #[cfg(feature = "openai")]
                {
                    use super::openai::{OpenAiChatCompletionsAdapter, OpenAiResponsesAdapter};
                    return match config.openai_protocol.clone() {
                        OpenAiProtocol::Responses => {
                            Ok(Arc::new(OpenAiResponsesAdapter::new(config)?))
                        }
                        OpenAiProtocol::ChatCompletions => {
                            Ok(Arc::new(OpenAiChatCompletionsAdapter::new(config)?))
                        }
                    };
                }

                #[cfg(not(feature = "openai"))]
                {
                    let _ = config;
                    anyhow::bail!("openai feature is disabled")
                }
            }
            ProviderKind::Anthropic => {
                #[cfg(feature = "anthropic")]
                {
                    use super::anthropic::AnthropicMessagesAdapter;
                    Ok(Arc::new(AnthropicMessagesAdapter::new(config)?))
                }

                #[cfg(not(feature = "anthropic"))]
                {
                    let _ = config;
                    anyhow::bail!("anthropic feature is disabled")
                }
            }
        }
    }

    /// Call one registered provider.
    /// 调用一个已注册服务商。
    pub async fn complete(&self, provider: &str, req: ModelRequest) -> Result<ModelResponse> {
        let adapter = self
            .providers
            .get(provider)
            .ok_or_else(|| anyhow!("unknown model provider: {provider}"))?;
        adapter.complete(req).await
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
            .get(provider)
            .ok_or_else(|| anyhow!("unknown model provider: {provider}"))?;
        Ok(adapter.stream(req).await)
    }

    /// Whether a provider exists.
    /// 服务商是否存在。
    pub fn contains(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    /// Return provider names.
    /// 返回所有服务商名称。
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
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
