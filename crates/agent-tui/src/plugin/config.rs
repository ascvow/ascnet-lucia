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
use sha2::{Digest, Sha256};
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

/// 被 Host 直接消费的 HTTP 端点键；同样不出现在交给 Guest 的元数据里。
///
/// 与模型路由键分开列举：一个插件可能只需要 HTTP 而不需要模型完成，反之亦然。
const HTTP_KEYS: &[&str] = &[
    "http_base_url",
    "http_api_key",
    "http_api_key_env",
    "http_auth_header",
    "http_auth_prefix",
    "http_timeout_ms",
    "http_max_request_bytes",
    "http_max_response_bytes",
];

/// HTTP 端点未显式配置时的默认超时，单位毫秒。
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 60_000;

/// HTTP 请求正文的默认上限。
const DEFAULT_HTTP_MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// HTTP 响应正文的默认上限。
const DEFAULT_HTTP_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// 默认承载凭证的 header 名。
const DEFAULT_HTTP_AUTH_HEADER: &str = "authorization";

/// 默认凭证前缀；OpenAI 兼容端点使用 `Bearer `。
const DEFAULT_HTTP_AUTH_PREFIX: &str = "Bearer ";

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
    /// 插件专属 HTTP 端点；未配置 `http_base_url` 时为 `None`，该插件无 HTTP 能力。
    pub(crate) http: Option<PluginHttpSettings>,
    /// 除路由与 HTTP 字段外的全部配置项，作为激活元数据提供给 Guest。
    pub(crate) metadata: HashMap<String, String>,
}

/// 插件受限 HTTP 端点的解析结果。
///
/// `credential_headers` 里是明文凭证，只在 Host 侧流转：它交给
/// `PluginHostServices::with_plugin_http` 后由宿主在发请求时注入，**不进入激活元数据，
/// 因此不会到达 Guest**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginHttpSettings {
    /// 可信 base URL，插件只能在其之下访问相对路径。
    pub(crate) base_url: String,
    /// Host 注入的凭证 header。
    pub(crate) credential_headers: Vec<(String, String)>,
    /// 单次请求正文上限。
    pub(crate) max_request_bytes: usize,
    /// 单次响应正文上限。
    pub(crate) max_response_bytes: usize,
    /// 单次请求超时，单位毫秒。
    pub(crate) timeout_ms: u64,
}

impl PluginHttpSettings {
    /// 返回该端点的非敏感稳定标识。
    ///
    /// 取 `base_url` 的摘要而不是原文：插件需要它来判定嵌入空间是否变化（换端点必须
    /// 重建语义索引），但端点地址属于用户环境信息，没有下发原文的必要。凭证不参与摘要
    /// ——换密钥不改变向量空间，把它算进去会导致无谓的全量重建。
    pub(crate) fn endpoint_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"lucia-plugin-http-endpoint-v1\0");
        digest.update(self.base_url.as_bytes());
        format!("{:x}", digest.finalize())[..16].to_string()
    }
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
    let http = resolve_http(plugin_id, &merged)?;
    let metadata = merged
        .into_iter()
        .filter(|(key, _)| {
            !ROUTE_KEYS.contains(&key.as_str()) && !HTTP_KEYS.contains(&key.as_str())
        })
        .map(|(key, value)| (key, scalar_to_string(&value)))
        .collect();
    Ok(PluginSettings {
        route,
        http,
        metadata,
    })
}

/// 从合并后的配置解析受限 HTTP 端点；未声明 `http_base_url` 时返回 `None`。
///
/// 凭证与模型路由采用同一套取值规则（`http_api_key` 优先于 `http_api_key_env`），
/// 但**允许不配置凭证**：并非所有端点都需要认证，强制要求会挡住本地无鉴权的服务。
///
/// # Errors
///
/// 字段类型不符、`http_api_key_env` 指向的环境变量缺失，或体积与超时上限为零时返回错误。
fn resolve_http(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
) -> Result<Option<PluginHttpSettings>> {
    let Some(base_url) = optional_string(plugin_id, config, "http_base_url")? else {
        return Ok(None);
    };
    let api_key = match optional_string(plugin_id, config, "http_api_key")? {
        Some(api_key) => Some(api_key),
        None => match optional_string(plugin_id, config, "http_api_key_env")? {
            Some(env_name) => Some(std::env::var(&env_name).with_context(|| {
                format!("插件 `{plugin_id}` 的 http_api_key_env 指向的环境变量缺失：{env_name}")
            })?),
            None => None,
        },
    };
    let mut credential_headers = Vec::new();
    if let Some(api_key) = api_key {
        let header = optional_string(plugin_id, config, "http_auth_header")?
            .unwrap_or_else(|| DEFAULT_HTTP_AUTH_HEADER.to_string());
        let prefix = optional_raw_string(plugin_id, config, "http_auth_prefix")?
            .unwrap_or_else(|| DEFAULT_HTTP_AUTH_PREFIX.to_string());
        credential_headers.push((header, format!("{prefix}{api_key}")));
    }
    let timeout_ms = optional_u64(plugin_id, config, "http_timeout_ms")?
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_MS);
    let max_request_bytes = optional_usize(plugin_id, config, "http_max_request_bytes")?
        .unwrap_or(DEFAULT_HTTP_MAX_REQUEST_BYTES);
    let max_response_bytes = optional_usize(plugin_id, config, "http_max_response_bytes")?
        .unwrap_or(DEFAULT_HTTP_MAX_RESPONSE_BYTES);
    if timeout_ms == 0 || max_request_bytes == 0 || max_response_bytes == 0 {
        return Err(anyhow!(
            "插件 `{plugin_id}` 的 HTTP 超时与体积上限必须大于零"
        ));
    }
    Ok(Some(PluginHttpSettings {
        base_url,
        credential_headers,
        max_request_bytes,
        max_response_bytes,
        timeout_ms,
    }))
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

/// 读取可选字符串配置项，**保留显式配置的空串**。
///
/// 与 [`optional_string`] 的区别只在空串语义：对 `model` 这类键，空串等同于没配；
/// 但 `http_auth_prefix = ""` 表示「发送裸密钥」，是 `x-api-key` 这类认证的正确写法，
/// 把它归为缺失会让默认的 `Bearer ` 前缀被错误地加上去。
fn optional_raw_string(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(anyhow!(
            "插件 `{plugin_id}` 的配置项 {key} 必须是字符串，实际是 {}",
            other.type_str()
        )),
    }
}

/// 读取可选无符号 64 位整数配置项。
fn optional_u64(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<u64>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| anyhow!("插件 `{plugin_id}` 的配置项 {key} 超出取值范围：{value}")),
        Some(other) => Err(anyhow!(
            "插件 `{plugin_id}` 的配置项 {key} 必须是整数，实际是 {}",
            other.type_str()
        )),
    }
}

/// 读取可选长度配置项。
fn optional_usize(
    plugin_id: &str,
    config: &toml::map::Map<String, Value>,
    key: &str,
) -> Result<Option<usize>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => usize::try_from(*value)
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

    /// 未配置 `http_base_url` 的插件不应获得 HTTP 端点。
    #[test]
    fn settings_without_http_base_url_have_no_endpoint() {
        let settings = resolve_plugin_settings(
            "code-context",
            "plugin:code-context",
            table("max_entries = 20\n"),
            None,
            &defaults(),
        )
        .expect("解析应成功");

        assert!(settings.http.is_none());
    }

    /// HTTP 端点必须被解析，且凭证不得混进交给 Guest 的元数据。
    #[test]
    fn http_endpoint_is_parsed_and_credentials_stay_host_side() {
        let settings = resolve_plugin_settings(
            "code-context",
            "plugin:code-context",
            table(
                "http_base_url = \"https://api.openai.com/v1/\"\nhttp_api_key = \"sk-test\"\nembedding_mode = \"api\"\n",
            ),
            None,
            &defaults(),
        )
        .expect("解析应成功");

        let http = settings.http.expect("配置了 base_url 时应产生端点");
        assert_eq!(http.base_url, "https://api.openai.com/v1/");
        assert_eq!(
            http.credential_headers,
            vec![(
                "authorization".to_string(),
                "Bearer sk-test".to_string()
            )]
        );
        // 非 HTTP 键继续作为激活元数据下发。
        assert_eq!(settings.metadata["embedding_mode"], "api");
        // 凭证与端点配置绝不进入 Guest 可见的元数据。
        assert!(!settings.metadata.contains_key("http_api_key"));
        assert!(!settings.metadata.contains_key("http_base_url"));
    }

    /// 无凭证的端点必须被接受——本地服务未必需要认证。
    #[test]
    fn http_endpoint_without_credentials_is_accepted() {
        let settings = resolve_plugin_settings(
            "code-context",
            "plugin:code-context",
            table("http_base_url = \"http://localhost:11434/v1/\"\n"),
            None,
            &defaults(),
        )
        .expect("解析应成功");

        let http = settings.http.expect("应产生端点");
        assert!(http.credential_headers.is_empty());
    }

    /// 自定义凭证 header 与前缀必须生效。
    #[test]
    fn custom_auth_header_and_prefix_are_applied() {
        let settings = resolve_plugin_settings(
            "code-context",
            "plugin:code-context",
            table(
                "http_base_url = \"https://example.com/\"\nhttp_api_key = \"k\"\nhttp_auth_header = \"x-api-key\"\nhttp_auth_prefix = \"\"\n",
            ),
            None,
            &defaults(),
        )
        .expect("解析应成功");

        let http = settings.http.expect("应产生端点");
        assert_eq!(
            http.credential_headers,
            vec![("x-api-key".to_string(), "k".to_string())]
        );
    }

    /// 体积与超时上限为零必须被拒绝。
    #[test]
    fn zero_http_limits_are_rejected() {
        let error = resolve_plugin_settings(
            "code-context",
            "plugin:code-context",
            table("http_base_url = \"https://example.com/\"\nhttp_timeout_ms = 0\n"),
            None,
            &defaults(),
        )
        .expect_err("零超时应被拒绝");

        assert!(error.to_string().contains("必须大于零"), "实际：{error}");
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
