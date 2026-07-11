//! TOML configuration helpers.
//! TOML 配置辅助工具。

use crate::{
    agent::{Agent, AgentModelConfig, AgentOptions},
    model::{ModelGateway, ModelProviderConfig, OpenAiProtocol, ProviderKind},
};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fs, path::Path};

/// Root configuration file.
/// 根配置文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRootConfig {
    pub model: ModelConfig,

    #[serde(default)]
    pub agent: AgentConfig,
}

impl AgentRootConfig {
    /// Load config from TOML.
    /// 从 TOML 加载配置。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse config: {}", path.display()))
    }

    /// Build a model gateway from the config.
    /// 根据配置构建模型网关。
    pub fn build_gateway(&self) -> Result<ModelGateway> {
        let mut gateway = ModelGateway::new();
        gateway.register_from_config(self.model.to_provider_config()?)?;
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
        let mut config =
            AgentModelConfig::new(self.model.to_provider_config()?, self.model.model.clone());
        config.max_tokens = self.agent.max_tokens.or(config.max_tokens);
        config.temperature = self.agent.temperature.or(config.temperature);
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
        let mut options = AgentOptions::default();
        options.provider = self.model.name.clone();
        options.model = self.model.model.clone();
        options.max_steps = self.agent.max_steps.unwrap_or(options.max_steps);
        if let Some(system_prompt) = &self.agent.system_prompt {
            options.system_prompt = system_prompt.clone();
        }
        options.max_tokens = self.agent.max_tokens.or(options.max_tokens);
        options.temperature = self.agent.temperature.or(options.temperature);
        options.provider_options = self
            .agent
            .provider_options
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));
        options
    }
}

/// Model provider config from TOML.
/// TOML 中的模型服务商配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Logical provider name.
    /// 逻辑服务商名称。
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
        })
    }
}

fn default_provider_name() -> String {
    "default".to_string()
}

/// Agent-level options from TOML.
/// TOML 中的 agent 级选项。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 单条用户指令允许连续执行的最大 ReAct 步数。
    pub max_steps: Option<usize>,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,

    /// Provider-specific options shallow-merged into the request.
    /// 浅合并到请求中的服务商专属选项。
    pub provider_options: Option<Value>,
}
