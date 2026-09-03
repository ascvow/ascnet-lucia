//! 插件级配置：模型路由覆盖与自由格式设置项。
//!
//! 配置有两个来源，按优先级从低到高合并：
//! 1. 插件目录下与 `plugin.toml` 同级的 `config.toml`，随 bundle 分发，负责默认值；
//! 2. 主配置 `[plugins.<插件 ID>]` 段，属于用户自己的配置，覆盖前者。
//!
//! 解析全部发生在 Host 可信侧。模型路由字段由 Host 消费并注册成该插件专属的 provider，
//! 密钥不会进入 Guest；其余键值作为激活元数据交给插件自己读取。

use agent_core::model::{
    GatewayKind, ModelProviderConfig, OpenAiProtocol, PromptCacheRetention, ProviderKind,
};
use agent_core::{AgentModelConfig, ReasoningLevel};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use toml::Value;

/// 插件目录中配置文件的固定名称。
const PLUGIN_CONFIG_FILE: &str = "config.toml";

/// 被 Host 直接消费的模型路由键；这些键不会出现在交给 Guest 的元数据里。
const ROUTE_KEYS: &[&str] = &[
    "provider",
    "model",
    "base_url",
    "api_key",
    "api_key_env",
    "openai_protocol",
    "gateway",
    "stream",
    "max_output_tokens",
    "reasoning",
    "temperature",
    "cache_retention",
];

/// 插件未显式配置时沿用的应用默认值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PluginRouteDefaults {
    /// 应用配置的流式开关。
    pub(crate) stream: bool,
    /// 应用配置的提示缓存保留策略。
    pub(crate) cache_retention: PromptCacheRetention,
}

/// 单个插件合并后的有效配置。
#[derive(Debug, Default, Clone)]
pub(crate) struct PluginSettings {
    /// 插件专属模型路由；未配置 `model` 时为 `None`，该插件沿用应用默认路由。
    pub(crate) route: Option<AgentModelConfig>,
    /// 除路由字段外的全部配置项，作为激活元数据提供给 Guest。
    pub(crate) metadata: HashMap<String, String>,
}

/// 主配置中 `[plugins]` 段的原始内容，键为插件 ID。
#[derive(Debug, Default, Deserialize)]
struct PluginConfigEnvelope {
    #[serde(default)]
    plugins: HashMap<String, Value>,
}

/// 从主配置文件读取 `[plugins]` 段；缺失时返回空映射。
///
/// # Errors
///
/// 文件不可读或 TOML 不合法时返回错误；`[plugins.<id>]` 不是表时同样报错，避免把
/// 标量误当成插件配置静默丢弃。
pub(crate) fn load_main_plugin_configs(path: &Path) -> Result<HashMap<String, Value>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let envelope: PluginConfigEnvelope = toml::from_str(&text)
        .with_context(|| format!("解析配置文件的 [plugins] 段失败：{}", path.display()))?;
    for (plugin_id, value) in &envelope.plugins {
        if !value.is_table() {
            return Err(anyhow!(
                "配置项 [plugins.{plugin_id}] 必须是表，实际是 {}",
                value.type_str()
            ));
        }
    }
    Ok(envelope.plugins)
}

/// 读取插件目录中随 bundle 分发的 `config.toml`；文件不存在时返回空映射。
///
/// # Errors
///
/// 文件存在但不可读或不是合法 TOML 表时返回错误。
pub(crate) fn load_plugin_dir_config(plugin_dir: &Path) -> Result<toml::map::Map<String, Value>> {
    let path = plugin_dir.join(PLUGIN_CONFIG_FILE);
    if !path.is_file() {
        return Ok(toml::map::Map::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取插件配置失败：{}", path.display()))?;
    let value: Value =
        toml::from_str(&text).with_context(|| format!("解析插件配置失败：{}", path.display()))?;
    match value {
        Value::Table(table) => Ok(table),
        other => Err(anyhow!(
            "插件配置 {} 必须是表，实际是 {}",
            path.display(),
            other.type_str()
        )),
    }
}

/// 合并插件目录配置与主配置段，解析出有效设置。
///
/// 主配置的同名键覆盖插件目录的默认值。`api_key` 优先于 `api_key_env`；两者都缺失但
/// 配置了 `model` 时报错，避免插件在运行期才因缺少凭据失败。
///
/// # Errors
///
/// 路由字段类型不符、`api_key_env` 指向的环境变量缺失，或声明了模型却没有可用凭据时
/// 返回错误。
pub(crate) fn resolve_plugin_settings(
    plugin_id: &str,
    provider_name: &str,
    dir_config: toml::map::Map<String, Value>,
    main_config: Option<&Value>,
    defaults: &PluginRouteDefaults,
) -> Result<PluginSettings> {
    let mut merged = dir_config;
    if let Some(Value::Table(table)) = main_config {
        for (key, value) in table.clone() {
            merged.insert(key, value);
        }
    }

    let route = resolve_route(plugin_id, provider_name, &merged, defaults)?;
    let metadata = merged
        .into_iter()
        .filter(|(key, _)| !ROUTE_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key, scalar_to_string(&value)))
        .collect();
    Ok(PluginSettings { route, metadata })
}

/// 从合并后的配置解析模型路由；未声明 `model` 时返回 `None`。
///
/// `provider_name` 由调用方按插件 ID 生成，必须在网关内唯一。产出的是 Core 的
/// [`AgentModelConfig`]，因此推理级别、采样温度、缓存策略这些参数与主 Agent 完全同构，
/// 不需要在插件侧维护第二套字段。
fn resolve_route(
    plugin_id: &str,
    provider_name: &str,
    config: &toml::map::Map<String, Value>,
    defaults: &PluginRouteDefaults,
) -> Result<Option<AgentModelConfig>> {
    let Some(model) = optional_string(plugin_id, config, "model")? else {
        return Ok(None);
    };
    let api_key = match optional_string(plugin_id, config, "api_key")? {
        Some(api_key) => api_key,
        None => match optional_string(plugin_id, config, "api_key_env")? {
            Some(env_name) => std::env::var(&env_name).with_context(|| {
                format!("插件 `{plugin_id}` 的 api_key_env 指向的环境变量缺失：{env_name}")
            })?,
            None => {
                return Err(anyhow!(
                    "插件 `{plugin_id}` 配置了 model，必须同时提供 api_key 或 api_key_env"
                ));
            }
        },
    };

    let provider = ModelProviderConfig {
        name: provider_name.to_string(),
        kind: parse_enum(
            plugin_id,
            config,
            "provider",
            ProviderKind::OpenAiCompatible,
        )?,
        api_key,
        base_url: optional_string(plugin_id, config, "base_url")?,
        openai_protocol: parse_enum(
            plugin_id,
            config,
            "openai_protocol",
            OpenAiProtocol::default(),
        )?,
        extra_headers: HashMap::new(),
        gateway: parse_enum(plugin_id, config, "gateway", GatewayKind::default())?,
    };

    let mut route = AgentModelConfig::new(provider, model);
    route.stream = optional_bool(plugin_id, config, "stream")?.unwrap_or(defaults.stream);
    route.max_tokens = optional_u32(plugin_id, config, "max_output_tokens")?;
    route.reasoning = parse_enum(plugin_id, config, "reasoning", ReasoningLevel::Off)?;
    route.temperature = optional_f32(plugin_id, config, "temperature")?;
    route.cache_retention = parse_enum(
        plugin_id,
        config,
        "cache_retention",
        defaults.cache_retention,
    )?;
    Ok(Some(route))
}

/// 读取可选字符串配置项。
fn optional_string(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(anyhow!(
            "插件 `{plugin_id}` 的配置项 {key} 必须是字符串，实际是 {}",
            other.type_str()
        )),
    }
}

/// 读取可选布尔配置项。
fn optional_bool(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<bool>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::Boolean(value)) => Ok(Some(*value)),
        Some(other) => Err(anyhow!(
            "插件 `{plugin_id}` 的配置项 {key} 必须是布尔值，实际是 {}",
            other.type_str()
        )),
    }
}

/// 读取可选正整数配置项。
fn optional_u32(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<u32>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => u32::try_from(*value)
            .map(Some)
            .map_err(|_| anyhow!("插件 `{plugin_id}` 的配置项 {key} 超出取值范围：{value}")),
        Some(other) => Err(anyhow!(
            "插件 `{plugin_id}` 的配置项 {key} 必须是整数，实际是 {}",
            other.type_str()
        )),
    }
}

/// 读取可选浮点配置项；整数写法同样接受。
fn optional_f32(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<f32>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::Float(value)) => Ok(Some(*value as f32)),
        Some(Value::Integer(value)) => Ok(Some(*value as f32)),
        Some(other) => Err(anyhow!(
            "插件 `{plugin_id}` 的配置项 {key} 必须是数值，实际是 {}",
            other.type_str()
        )),
    }
}

/// 按 TOML 字符串值反序列化枚举配置项，缺失时返回默认值。
fn parse_enum<T: serde::de::DeserializeOwned>(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
    default: T,
) -> Result<T> {
    let Some(text) = optional_string(plugin_id, config, key)? else {
        return Ok(default);
    };
    Value::String(text.clone())
        .try_into()
        .with_context(|| format!("插件 `{plugin_id}` 的配置项 {key} 取值无效：{text}"))
}

/// 把标量配置值转换为元数据字符串；表和数组按 TOML 原样序列化。
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::Datetime(stamp) => stamp.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析 TOML 表字面量，供用例构造配置来源。
    fn table(text: &str) -> toml::map::Map<String, Value> {
        match toml::from_str::<Value>(text).expect("测试 TOML 应合法") {
            Value::Table(table) => table,
            other => panic!("测试配置必须是表，实际是 {}", other.type_str()),
        }
    }

    /// 构造测试用的应用默认值。
    fn defaults() -> PluginRouteDefaults {
        PluginRouteDefaults {
            stream: true,
            cache_retention: PromptCacheRetention::Short,
        }
    }

    /// 主配置的同名键覆盖插件目录默认值，其余键合并保留。
    #[test]
    fn main_config_overrides_plugin_directory_defaults() {
        let settings = resolve_plugin_settings(
            "permission",
            "plugin:permission",
            table("model = \"dir-model\"\napi_key = \"dir-key\"\nauto_approve = false\n"),
            Some(&Value::Table(table(
                "model = \"main-model\"\nthreshold = 3\n",
            ))),
            &defaults(),
        )
        .expect("合并配置应成功");

        let route = settings.route.expect("配置了 model 时应产生路由");
        assert_eq!(route.model, "main-model");
        // 主配置没有重新声明密钥，插件目录的默认值继续生效。
        assert_eq!(route.provider.api_key, "dir-key");
        assert_eq!(route.provider.name, "plugin:permission");
        assert_eq!(settings.metadata["auto_approve"], "false");
        assert_eq!(settings.metadata["threshold"], "3");
        // 路由字段不得混进交给 Guest 的元数据。
        assert!(!settings.metadata.contains_key("model"));
        assert!(!settings.metadata.contains_key("api_key"));
    }

    /// 未声明 model 的插件不产生专属路由，配置项仍作为元数据下发。
    #[test]
    fn settings_without_model_keep_metadata_only() {
        let settings = resolve_plugin_settings(
            "memory",
            "plugin:memory",
            table("max_entries = 20\n"),
            None,
            &defaults(),
        )
        .expect("应成功");

        assert!(settings.route.is_none());
        assert_eq!(settings.metadata["max_entries"], "20");
    }

    /// 声明了 model 却没有任何凭据来源时必须在加载期报错。
    #[test]
    fn model_without_credentials_is_rejected() {
        let error = resolve_plugin_settings(
            "permission",
            "plugin:permission",
            table("model = \"m\"\n"),
            None,
            &defaults(),
        )
        .expect_err("缺少凭据应报错");

        assert!(error.to_string().contains("api_key"), "{error}");
    }

    /// 路由枚举字段使用与主配置一致的取值，非法值在加载期暴露。
    #[test]
    fn route_enum_fields_are_parsed_and_validated() {
        let settings = resolve_plugin_settings(
            "permission",
            "plugin:permission",
            table(
                "model = \"m\"\napi_key = \"k\"\nprovider = \"open-ai\"\ngateway = \"open-router\"\nopenai_protocol = \"chat-completions\"\nmax_output_tokens = 256\nstream = false\nreasoning = \"high\"\ntemperature = 0.2\n",
            ),
            None,
            &defaults(),
        )
        .expect("合法枚举应解析成功");
        let route = settings.route.expect("应产生路由");

        assert_eq!(route.provider.kind, ProviderKind::OpenAi);
        assert_eq!(route.provider.gateway, GatewayKind::OpenRouter);
        assert_eq!(
            route.provider.openai_protocol,
            OpenAiProtocol::ChatCompletions
        );
        assert_eq!(route.max_tokens, Some(256));
        assert!(!route.stream);
        // 思考级别与采样温度同样来自插件配置，与主 Agent 使用同一套取值。
        assert_eq!(route.reasoning, ReasoningLevel::High);
        assert_eq!(route.temperature, Some(0.2));

        let error = resolve_plugin_settings(
            "permission",
            "plugin:permission",
            table("model = \"m\"\napi_key = \"k\"\ngateway = \"nope\"\n"),
            None,
            &defaults(),
        )
        .expect_err("非法枚举应报错");
        assert!(error.to_string().contains("gateway"), "{error}");
    }
}
