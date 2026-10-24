//! JSON configuration helpers.
//! JSON 配置辅助工具。

use crate::{
    agent::{Agent, AgentModelConfig, AgentOptions},
    model::{
        GatewayKind, ModelGateway, ModelProviderConfig, OpenAiProtocol, PromptCacheRetention,
        ProviderKind, ReasoningLevel,
    },
};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fs, path::Path};

/// Root configuration file.
/// 根配置文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRootConfig {
    /// 唯一模型服务商和模型选择（向后兼容字段）；配置了 `models` 时可省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,

    /// 多上游配置；每个元素按 `name` 注册为一个逻辑 provider，支持任意协议混配。
    ///
    /// 同一个 `name` 可以挂在多个条目上（一个服务商多个可选模型），这些条目必须共用
    /// 同一份连接配置，只有 `model` 允许不同。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelConfig>,

    /// 默认使用的上游；缺省取 `model` 或 `models` 的第一个元素。
    ///
    /// 先按上游名匹配，再按模型 ID 匹配；同名上游挂了多个模型时，只能用模型 ID 选中
    /// 其中一条。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// ReAct 循环参数；缺省字段沿用 [`AgentOptions`] 的默认值。
    #[serde(default)]
    pub agent: AgentConfig,
}

impl AgentRootConfig {
    /// Load config from JSON.
    /// 从 JSON 加载配置。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let config: Self = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse config: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// 返回全部上游配置，`model` 在前、`models` 在后。
    pub fn upstreams(&self) -> Vec<&ModelConfig> {
        let mut all = Vec::with_capacity(self.models.len() + 1);
        if let Some(model) = &self.model {
            all.push(model);
        }
        all.extend(self.models.iter());
        all
    }

    /// 返回默认上游配置。
    ///
    /// 解析顺序：`default_model` 按上游名匹配（同名取第一个条目）→ 按模型 ID 匹配
    /// （必须唯一）→ `model` 字段 → `models` 第一个。
    ///
    /// # Errors
    ///
    /// 没有任何上游、`default_model` 既不匹配上游名也不匹配模型 ID、或模型 ID 匹配到
    /// 多条条目时返回。
    pub fn default_upstream(&self) -> Result<&ModelConfig> {
        let upstreams = self.upstreams();
        if upstreams.is_empty() {
            anyhow::bail!("config requires a `model` object or at least one `models` entry");
        }
        if let Some(default) = &self.default_model {
            // 名字优先：同名上游挂着的其余模型只能用模型 ID 选中，名字匹配固定取第一条。
            if let Some(matched) = upstreams.iter().find(|config| &config.name == default) {
                return Ok(matched);
            }
            let mut by_model = upstreams.iter().filter(|config| &config.model == default);
            if let Some(first) = by_model.next() {
                if by_model.next().is_some() {
                    anyhow::bail!("default_model `{default}` 匹配到多个模型条目，请改用上游名指定");
                }
                return Ok(first);
            }
            anyhow::bail!(
                "default_model `{default}` does not match any configured upstream name or model id"
            );
        }
        Ok(upstreams[0])
    }

    /// 校验上游配置的整体一致性。
    ///
    /// 同名条目是合法的（一个服务商挂多个可选模型），但它们必须共用同一份连接配置：
    /// 若同名条目的地址或凭据不同，网关里只会留下其中一份，另一条的请求会静默发往
    /// 错误的端点或凭据。
    fn validate(&self) -> Result<()> {
        let upstreams = self.upstreams();
        for (index, config) in upstreams.iter().enumerate() {
            for other in &upstreams[index + 1..] {
                if config.name != other.name {
                    continue;
                }
                if let Some(field) = conflicting_connection_field(config, other) {
                    anyhow::bail!(
                        "model upstream `{}` 重复定义且 `{field}` 不一致：同名上游只能共用同一份连接配置，只有 `model` 可以不同",
                        config.name
                    );
                }
            }
        }
        if let Some(default_name) = &self.default_model {
            // 名字与模型 ID 都是 `default_model` 的合法写法，与 default_upstream 保持一致。
            if !upstreams.iter().any(|config| &config.name == default_name)
                && !upstreams.iter().any(|config| &config.model == default_name)
            {
                anyhow::bail!(
                    "default_model `{default_name}` does not match any configured upstream"
                );
            }
        }
        Ok(())
    }

    /// Build a model gateway from the config.
    ///
    /// 根据配置构建模型网关，注册全部上游；默认上游按 [`Self::default_upstream`] 解析。
    /// 同名上游共用同一份连接配置（见 [`Self::validate`]），因此每个名字只注册一次。
    pub fn build_gateway(&self) -> Result<ModelGateway> {
        let gateway = ModelGateway::new();
        let mut registered: Vec<&str> = Vec::new();
        for upstream in self.upstreams() {
            if registered.contains(&upstream.name.as_str()) {
                continue;
            }
            gateway.register_from_config(upstream.to_provider_config()?)?;
            registered.push(upstream.name.as_str());
        }
        Ok(gateway)
    }

    /// Convert config to caller-style runtime model config.
    /// 转换为调用方风格的运行时模型配置。
    ///
    /// The returned value is not persisted by core. It can be passed to
    /// Agent::from_model_config or Agent::set_model_config.
    /// core 不会持久化返回值。它可以传给 Agent::from_model_config 或
    /// Agent::set_model_config。
    pub fn agent_model_config(&self) -> Result<AgentModelConfig> {
        let default = self.default_upstream()?;
        let mut config =
            AgentModelConfig::new(default.to_provider_config()?, default.model.clone());
        config.max_tokens = self.agent.max_tokens.or(config.max_tokens);
        config.stream = self.agent.stream.unwrap_or(config.stream);
        config.temperature = self.agent.temperature.or(config.temperature);
        config.reasoning = self.agent.reasoning.unwrap_or(config.reasoning);
        config.cache_retention = self.agent.cache_retention.unwrap_or(config.cache_retention);
        config.provider_options = self
            .agent
            .provider_options
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));
        Ok(config)
    }

    /// Build an agent from this config. The caller still owns config persistence.
    /// 根据该配置构建 agent。配置持久化仍由调用方负责。
    pub fn build_agent(&self) -> Result<Agent> {
        let mut agent = Agent::from_model_config(self.agent_model_config()?)?;
        let options = agent.options_mut();
        options.max_steps = self.agent.max_steps.unwrap_or(options.max_steps);
        if let Some(system_prompt) = &self.agent.system_prompt {
            options.system_prompt = system_prompt.clone();
        }
        Ok(agent)
    }

    /// Convert config to agent options.
    /// 将配置转换为 agent 选项。
    pub fn agent_options(&self) -> AgentOptions {
        let default = self
            .default_upstream()
            .expect("default upstream 在 load/validate 后必然存在");
        let mut options = AgentOptions::default();
        options.provider = default.name.clone();
        options.model = default.model.clone();
        options.max_steps = self.agent.max_steps.unwrap_or(options.max_steps);
        if let Some(system_prompt) = &self.agent.system_prompt {
            options.system_prompt = system_prompt.clone();
        }
        options.max_tokens = self.agent.max_tokens.or(options.max_tokens);
        options.stream = self.agent.stream.unwrap_or(options.stream);
        options.temperature = self.agent.temperature.or(options.temperature);
        options.reasoning = self.agent.reasoning.unwrap_or(options.reasoning);
        options.cache_retention = self
            .agent
            .cache_retention
            .unwrap_or(options.cache_retention);
        options.provider_options = self
            .agent
            .provider_options
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));
        options
    }
}

/// Model provider config from JSON.
/// JSON 中的模型服务商配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Logical provider name.
    ///
    /// 逻辑服务商名称，同时是网关注册名与 `/model <名称>` 的切换键。同一个名字可以挂在
    /// 多个条目上（同一服务商的多个可选模型），这些条目必须共用同一份连接配置。
    #[serde(default = "default_provider_name")]
    pub name: String,

    /// Provider kind: open-ai, open-ai-compatible, anthropic.
    /// 服务商类型：open-ai、open-ai-compatible、anthropic。
    pub provider: ProviderKind,

    /// Model id sent to provider.
    /// 发送给服务商的模型 ID。
    pub model: String,

    /// API key literal. Prefer api_key_env for real projects.
    /// API key 明文。真实项目建议使用 api_key_env。
    pub api_key: Option<String>,

    /// Environment variable that contains the API key.
    /// 保存 API key 的环境变量。
    pub api_key_env: Option<String>,

    /// Optional base URL.
    /// 可选 base URL。
    pub base_url: Option<String>,

    /// OpenAI protocol: responses or chat-completions.
    /// OpenAI 协议：responses 或 chat-completions。
    #[serde(default)]
    pub openai_protocol: OpenAiProtocol,

    /// Extra HTTP headers.
    /// 额外 HTTP header。
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,

    /// 网关厂商：open-ai、open-router、anthropic、open-ai-compatible。
    ///
    /// 决定提示缓存键与会话亲和 header 的约定；缺省按通用 OpenAI 兼容网关处理。
    #[serde(default)]
    pub gateway: GatewayKind,

    /// AWS 区域；仅 Bedrock 协议使用，例如 `us-east-1`。
    #[serde(default)]
    pub aws_region: Option<String>,

    /// AWS 访问密钥 ID；仅 Bedrock 协议使用。
    #[serde(default)]
    pub aws_access_key_id: Option<String>,

    /// AWS 秘密访问密钥；仅 Bedrock 协议使用，建议通过环境变量注入后覆盖。
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,

    /// AWS 临时会话令牌；仅 Bedrock 协议使用，长期凭据留空。
    #[serde(default)]
    pub aws_session_token: Option<String>,
}

impl ModelConfig {
    /// Convert to runtime provider config.
    /// 转换为运行时服务商配置。
    pub fn to_provider_config(&self) -> Result<ModelProviderConfig> {
        let api_key = match (&self.api_key, &self.api_key_env) {
            (Some(api_key), _) => api_key.clone(),
            (None, Some(env_name)) => std::env::var(env_name)
                .with_context(|| format!("missing model api key env var `{env_name}`"))?,
            (None, None) => {
                return Err(anyhow!(
                    "model.api_key or model.api_key_env must be set for provider `{}`",
                    self.name
                ));
            }
        };

        Ok(ModelProviderConfig {
            name: self.name.clone(),
            kind: self.provider.clone(),
            api_key,
            base_url: self.base_url.clone(),
            openai_protocol: self.openai_protocol.clone(),
            extra_headers: self.extra_headers.clone(),
            gateway: self.gateway,
            aws_region: self.aws_region.clone(),
            aws_access_key_id: self.aws_access_key_id.clone(),
            aws_secret_access_key: self.aws_secret_access_key.clone(),
            aws_session_token: self.aws_session_token.clone(),
        })
    }
}

fn default_provider_name() -> String {
    "default".to_string()
}

/// 返回两个同名上游之间第一处不一致的连接字段名。
///
/// 同名上游必须指向同一份连接：只有 `model` 可以不同。这里显式逐列比较而不给
/// [`ModelConfig`] 派生 `PartialEq`，是为了在错误里指出到底是哪个字段冲突，
/// 而不是只说"两条配置不一样"。
fn conflicting_connection_field(left: &ModelConfig, right: &ModelConfig) -> Option<&'static str> {
    for (field, differs) in [
        ("provider", left.provider != right.provider),
        ("base_url", left.base_url != right.base_url),
        (
            "openai_protocol",
            left.openai_protocol != right.openai_protocol,
        ),
        ("gateway", left.gateway != right.gateway),
        ("extra_headers", left.extra_headers != right.extra_headers),
        ("api_key", left.api_key != right.api_key),
        ("api_key_env", left.api_key_env != right.api_key_env),
        ("aws_region", left.aws_region != right.aws_region),
        (
            "aws_access_key_id",
            left.aws_access_key_id != right.aws_access_key_id,
        ),
        (
            "aws_secret_access_key",
            left.aws_secret_access_key != right.aws_secret_access_key,
        ),
        (
            "aws_session_token",
            left.aws_session_token != right.aws_session_token,
        ),
    ] {
        if differs {
            return Some(field);
        }
    }
    None
}

/// Agent-level options from JSON.
/// JSON 中的 agent 级选项。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 单条用户指令允许连续执行的最大 ReAct 步数；`0` 表示不设置总步数上限。
    pub max_steps: Option<usize>,
    /// 覆盖 Core 默认系统提示；`None` 表示保留默认提示。
    pub system_prompt: Option<String>,
    /// 单次模型响应允许生成的最大 token 数；具体支持程度由服务商决定。
    pub max_tokens: Option<u32>,
    /// 是否使用模型流式接口；`None` 使用默认值 `true`。
    pub stream: Option<bool>,
    /// 模型采样温度；Core 原样传递，合法范围由服务商适配器校验。
    pub temperature: Option<f32>,
    /// 模型推理强度；`None` 保持默认关闭，由应用层能力元数据决定是否补全。
    pub reasoning: Option<ReasoningLevel>,

    /// 模型提示缓存保留策略；`None` 使用默认短期缓存。
    pub cache_retention: Option<PromptCacheRetention>,

    /// Provider-specific options shallow-merged into the request.
    /// 浅合并到请求中的服务商专属选项。
    pub provider_options: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析内联 JSON 配置，供多上游用例复用。
    fn parse(text: &str) -> Result<AgentRootConfig> {
        let config: AgentRootConfig = serde_json::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// 单 `model` 对象必须保持向后兼容，默认上游取该对象。
    #[test]
    fn single_model_section_stays_compatible() {
        let config = parse(
            r#"{
  "model": {
    "provider": "anthropic",
    "model": "claude-sonnet-4-5",
    "api_key": "test-anthropic-key"
  }
}"#,
        )
        .expect("单上游配置应可解析");
        assert_eq!(config.upstreams().len(), 1);
        assert_eq!(
            config.default_upstream().expect("应有默认上游").model,
            "claude-sonnet-4-5"
        );
    }

    /// 配置解析/默认选择不依赖任何协议适配器，最小 feature 同样覆盖。
    #[test]
    fn validates_models_without_provider_features() {
        let mut config = parse(r#"{"default_model":"b","models":[{"name":"shared","provider":"open-ai","model":"a","api_key":"k"},{"name":"shared","provider":"open-ai","model":"b","api_key":"k"},{"name":"other","provider":"google","model":"c","api_key":"k"}]}"#).unwrap();
        assert_eq!(config.default_upstream().unwrap().model, "b");
        assert_eq!(config.agent_options().provider, "shared");
        config.default_model = Some("other".into());
        config.validate().unwrap();
        assert_eq!(config.default_upstream().unwrap().model, "c");
        config.default_model = Some("missing".into());
        assert!(config.validate().is_err());
        config.default_model = None;
        config.models[1].api_key = Some("other-key".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("api_key"));
    }

    /// 多上游必须全部注册进网关，default_model 决定默认上游。
    #[test]
    #[cfg(all(feature = "openai", feature = "google"))]
    fn registers_all_upstreams_and_honors_default() {
        let config = parse(
            r#"{
  "default_model": "gemini",
  "models": [
    { "name": "openai", "provider": "open-ai", "model": "gpt-5", "api_key": "test-openai-key" },
    { "name": "gemini", "provider": "google", "model": "gemini-2.5-pro", "api_key": "test-gemini-key" }
  ]
}"#,
        )
        .expect("多上游配置应可解析");
        let gateway = config.build_gateway().expect("网关应可构建");
        assert!(gateway.contains("openai"));
        assert!(gateway.contains("gemini"));
        assert_eq!(
            config.default_upstream().expect("应有默认上游").name,
            "gemini"
        );
        assert_eq!(config.agent_options().provider, "gemini");
    }

    /// 缺省 default_model 时必须回退到第一个上游。
    #[test]
    fn falls_back_to_first_upstream() {
        let config = parse(
            r#"{
  "models": [
    { "name": "first", "provider": "open-ai", "model": "gpt-5", "api_key": "test-openai-key" },
    { "name": "second", "provider": "anthropic", "model": "claude", "api_key": "test-anthropic-key" }
  ]
}"#,
        )
        .expect("配置应可解析");
        assert_eq!(
            config.default_upstream().expect("应有默认上游").name,
            "first"
        );
    }

    /// 同名上游只能共用同一份连接：地址或协议冲突必须被拒绝，并指出冲突字段。
    #[test]
    fn rejects_conflicting_duplicated_names_and_unknown_default() {
        let duplicated = parse(
            r#"{
  "models": [
    { "name": "same", "provider": "open-ai", "model": "a", "api_key": "a" },
    { "name": "same", "provider": "anthropic", "model": "b", "api_key": "b" }
  ]
}"#,
        );
        let error = duplicated.expect_err("同名但协议不同的上游必须被拒绝");
        assert!(error.to_string().contains("provider"), "{error}");

        let unknown_default = parse(
            r#"{
  "default_model": "missing",
  "models": [
    { "name": "a", "provider": "open-ai", "model": "a", "api_key": "a" }
  ]
}"#,
        );
        assert!(unknown_default.is_err());
    }

    /// 一个服务商挂多个模型：同名条目只要求连接一致，网关按名注册一次，
    /// default_model 用模型 ID 选中其中一条。
    #[test]
    #[cfg(feature = "openai")]
    fn allows_one_provider_with_several_models() {
        let config = parse(
            r#"{
  "default_model": "gpt-5.6-luna",
  "models": [
    { "name": "ascnet", "provider": "open-ai", "base_url": "https://ai.example.test/v1", "model": "gpt-6.1-sol", "api_key": "k" },
    { "name": "ascnet", "provider": "open-ai", "base_url": "https://ai.example.test/v1", "model": "gpt-5.6-luna", "api_key": "k" }
  ]
}"#,
        )
        .expect("同名多模型配置应可解析");
        assert_eq!(config.upstreams().len(), 2);
        let gateway = config.build_gateway().expect("同名上游只应注册一次");
        assert!(gateway.contains("ascnet"));
        let default = config.default_upstream().expect("应有默认上游");
        assert_eq!(default.name, "ascnet");
        assert_eq!(default.model, "gpt-5.6-luna");
        // 只写名字时取第一条：同名上游的第一个模型条目就是它的名字代表。
        let by_name: AgentRootConfig = serde_json::from_str(
            r#"{
  "default_model": "ascnet",
  "models": [
    { "name": "ascnet", "provider": "open-ai", "model": "gpt-6.1-sol", "api_key": "k" },
    { "name": "ascnet", "provider": "open-ai", "model": "gpt-5.6-luna", "api_key": "k" }
  ]
}"#,
        )
        .expect("配置应可解析");
        by_name.validate().expect("同名同连接应通过校验");
        assert_eq!(
            by_name.default_upstream().expect("应有默认上游").model,
            "gpt-6.1-sol"
        );
    }

    /// 模型 ID 跨上游重名时不能靠模型 ID 选默认，必须报错而不是随便挑一条。
    #[test]
    fn rejects_ambiguous_default_model_id() {
        let error = parse(
            r#"{
  "default_model": "shared",
  "models": [
    { "name": "a", "provider": "open-ai", "model": "shared", "api_key": "a" },
    { "name": "b", "provider": "anthropic", "model": "shared", "api_key": "b" }
  ]
}"#,
        )
        .expect("配置本身应可解析")
        .default_upstream()
        .expect_err("模型 ID 重名时不能自动选择");
        assert!(error.to_string().contains("多个模型条目"), "{error}");
    }

    /// 没有任何上游时必须给出可诊断错误。
    #[test]
    fn rejects_empty_upstreams() {
        let config = AgentRootConfig {
            model: None,
            models: Vec::new(),
            default_model: None,
            agent: AgentConfig::default(),
        };
        assert!(config.default_upstream().is_err());
    }
}
