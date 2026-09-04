//! 插件配置。
//!
//! 配置经宿主的激活元数据下发：应用把 `[plugins.code-context]` 解析后与 manifest
//! `[metadata]` 合并，再传给 `activate`。插件不自行读取配置文件，也拿不到任何密钥——
//! 凭证由宿主侧持有，这是本仓库既有的处理范式。

use crate::error::CodeContextError;
use std::collections::HashMap;

/// 语义嵌入模式。
///
/// 产品只提供本地与 API 两种来源，不实现官方托管服务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingMode {
    /// 本机推理，源码不出本机。
    Local,
    /// 用户自配的 OpenAI 兼容嵌入端点。
    Api,
}

impl EmbeddingMode {
    /// 解析配置值。
    ///
    /// # Errors
    ///
    /// 值不是 `local` 或 `api` 时返回错误，而不是回退到默认值——静默纠正拼写错误会让
    /// 用户以为配置生效了。
    fn parse(value: &str) -> Result<Self, CodeContextError> {
        match value.trim() {
            "local" => Ok(Self::Local),
            "api" => Ok(Self::Api),
            other => Err(CodeContextError::InvalidConfiguration(format!(
                "未知 embedding_mode `{other}`，可用值为 `local` 或 `api`"
            ))),
        }
    }

    /// 返回稳定的机器可读名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Api => "api",
        }
    }
}

/// API 嵌入默认使用的模型。
///
/// 选 OpenAI 的 `text-embedding-3-small`：绝大多数 OpenAI 兼容端点都实现了它，
/// 且维度与价格都适合代码检索这种高频场景。
const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// API 嵌入默认向量维度。
const DEFAULT_EMBEDDING_DIMENSION: u32 = 1536;

/// 单批最多提交的嵌入条数。
const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 64;

/// API 嵌入请求之间的默认最小间隔。
const DEFAULT_EMBEDDING_REQUEST_INTERVAL_MS: u64 = 1_500;

/// 单批 API 嵌入请求默认允许的重试次数。
const DEFAULT_EMBEDDING_MAX_RETRIES: u32 = 4;

/// API 嵌入瞬时错误的默认指数退避基数。
const DEFAULT_EMBEDDING_RETRY_BASE_MS: u64 = 5_000;

/// 请求间隔的配置上限，避免误配置让一次工具调用长时间无响应。
const MAX_EMBEDDING_REQUEST_INTERVAL_MS: u64 = 60_000;

/// 单批重试次数的配置上限。
const MAX_EMBEDDING_MAX_RETRIES: u32 = 10;

/// 指数退避基数的配置上限；实际单次等待还会在 provider 内限制为 30 秒。
const MAX_EMBEDDING_RETRY_BASE_MS: u64 = 30_000;

/// Code Context Engine 配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeContextConfig {
    /// 引擎是否启用；关闭后不建立索引，`code_search` 明确报告已禁用。
    pub enabled: bool,
    /// 语义嵌入模式。
    ///
    /// 默认 `api`：它是最低硬件压力路径，且不要求用户具备本地推理条件。
    pub embedding_mode: EmbeddingMode,
    /// 嵌入模型名称，随请求发给端点。
    pub embedding_model: String,
    /// 期望的向量维度。
    ///
    /// 参与嵌入空间标识，因此改动它会触发语义索引重建。端点实际返回的维度与之不符时
    /// 嵌入会失败而不是静默接受——维度不符意味着向量不可比，静默接受会让检索长期缓慢
    /// 地不正确。
    pub embedding_dimension: u32,
    /// 单批提交的最大条数。
    pub embedding_batch_size: usize,
    /// 相邻 API 请求之间的最小间隔，单位毫秒；设为 0 可关闭主动限速。
    ///
    /// 该限制同时覆盖正常批次和重试请求，避免全仓库向量化连续提交几十批后触发 TPM
    /// 限流。它不改变嵌入空间，因此调整后可以继续复用已有向量。
    pub embedding_request_interval_ms: u64,
    /// 单批请求遇到 HTTP 429 或 5xx 后允许的重试次数；设为 0 表示立即交回续建流程。
    pub embedding_max_retries: u32,
    /// HTTP 429 或 5xx 的指数退避基数，单位毫秒；设为 0 只用于关闭等待。
    ///
    /// 第 n 次重试按该值乘以 `2^n`，单次等待最多 30 秒；响应携带整数形式的
    /// `Retry-After` 时优先采用响应值。
    pub embedding_retry_base_ms: u64,
    /// 是否在嵌入请求中发送 `dimensions` 参数。
    ///
    /// **默认关闭。** 该参数只对支持维度截断的模型有意义（OpenAI 的
    /// `text-embedding-3-*` 系列）；`BAAI/bge-m3` 这类固定维度模型会直接返回
    /// HTTP 400 `The parameter is invalid`。
    ///
    /// 默认开启曾让整条语义链路静默失效：首次嵌入请求被拒 → 语义索引放弃建立 →
    /// 检索退化为符号与词法两路。而这一路恰恰是中文自然语言查询唯一能依赖的通道，
    /// 于是"用中文描述一个行为"几乎必然检索不到——但工具仍然正常返回结果，
    /// 失败完全不可见。默认值必须是"能用"的那一个。
    ///
    /// 维度校验仍在返回时执行，因此关闭此项不影响安全性。
    pub embedding_send_dimensions: bool,
    /// 可选的远端候选重排模型。
    ///
    /// 配置后，插件先扩大混合检索候选池，再把查询与候选源码提交给兼容 `/v1/rerank`
    /// 的端点；未配置时保持纯本地融合排序。
    pub rerank_model: Option<String>,
    /// 宿主注入的 HTTP 端点摘要；未配置 HTTP 时为 `None`。
    ///
    /// 由宿主从 `base_url` 计算后作为可信元数据下发，插件看不到地址与凭证。它参与嵌入
    /// 空间标识：换端点意味着向量不可比，必须重建语义索引。
    pub http_endpoint_id: Option<String>,
}

impl Default for CodeContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            embedding_mode: EmbeddingMode::Api,
            embedding_model: DEFAULT_EMBEDDING_MODEL.to_string(),
            embedding_dimension: DEFAULT_EMBEDDING_DIMENSION,
            embedding_batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
            embedding_request_interval_ms: DEFAULT_EMBEDDING_REQUEST_INTERVAL_MS,
            embedding_max_retries: DEFAULT_EMBEDDING_MAX_RETRIES,
            embedding_retry_base_ms: DEFAULT_EMBEDDING_RETRY_BASE_MS,
            embedding_send_dimensions: false,
            rerank_model: None,
            http_endpoint_id: None,
        }
    }
}

impl CodeContextConfig {
    /// 从宿主激活元数据解析配置。
    ///
    /// 未声明的键使用默认值；已声明但取值非法时返回错误，由激活流程拒绝加载。
    ///
    /// # Errors
    ///
    /// `enabled` 不是布尔字面量，或 `embedding_mode` 取值未知时返回
    /// [`CodeContextError::InvalidConfiguration`]。
    pub fn from_metadata(metadata: &HashMap<String, String>) -> Result<Self, CodeContextError> {
        let mut config = Self::default();
        if let Some(value) = metadata.get("enabled") {
            config.enabled = match value.trim() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(CodeContextError::InvalidConfiguration(format!(
                        "enabled 必须是 `true` 或 `false`，实际为 `{other}`"
                    )))
                }
            };
        }
        if let Some(value) = metadata.get("embedding_mode") {
            config.embedding_mode = EmbeddingMode::parse(value)?;
        }
        if let Some(value) = metadata.get("embedding_model") {
            let model = value.trim();
            if model.is_empty() {
                return Err(CodeContextError::InvalidConfiguration(
                    "embedding_model 不能为空".into(),
                ));
            }
            config.embedding_model = model.to_string();
        }
        if let Some(value) = metadata.get("embedding_dimension") {
            config.embedding_dimension = parse_positive_u32(value, "embedding_dimension")?;
        }
        if let Some(value) = metadata.get("embedding_batch_size") {
            config.embedding_batch_size =
                parse_positive_u32(value, "embedding_batch_size")? as usize;
        }
        if let Some(value) = metadata.get("embedding_request_interval_ms") {
            config.embedding_request_interval_ms = parse_bounded_u64(
                value,
                "embedding_request_interval_ms",
                MAX_EMBEDDING_REQUEST_INTERVAL_MS,
            )?;
        }
        if let Some(value) = metadata.get("embedding_max_retries") {
            config.embedding_max_retries =
                parse_bounded_u32(value, "embedding_max_retries", MAX_EMBEDDING_MAX_RETRIES)?;
        }
        if let Some(value) = metadata.get("embedding_retry_base_ms") {
            config.embedding_retry_base_ms = parse_bounded_u64(
                value,
                "embedding_retry_base_ms",
                MAX_EMBEDDING_RETRY_BASE_MS,
            )?;
        }
        if let Some(value) = metadata.get("embedding_send_dimensions") {
            config.embedding_send_dimensions = match value.trim() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(CodeContextError::InvalidConfiguration(format!(
                        "embedding_send_dimensions 必须是 `true` 或 `false`，实际为 `{other}`"
                    )))
                }
            };
        }
        if let Some(value) = metadata.get("rerank_model") {
            let model = value.trim();
            if model.is_empty() {
                return Err(CodeContextError::InvalidConfiguration(
                    "rerank_model 不能为空".into(),
                ));
            }
            config.rerank_model = Some(model.to_string());
        }
        // 该键由宿主注入，不接受插件配置伪造：宿主写入发生在用户配置合并之后。
        config.http_endpoint_id = metadata
            .get("http_endpoint_id")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Ok(config)
    }
}

/// 解析必须为正数的整数配置项。
///
/// # Errors
///
/// 值不是整数或为零时返回 [`CodeContextError::InvalidConfiguration`]。
fn parse_positive_u32(value: &str, key: &str) -> Result<u32, CodeContextError> {
    let parsed: u32 = value.trim().parse().map_err(|_| {
        CodeContextError::InvalidConfiguration(format!("{key} 必须是正整数，实际为 `{value}`"))
    })?;
    if parsed == 0 {
        return Err(CodeContextError::InvalidConfiguration(format!(
            "{key} 必须大于零"
        )));
    }
    Ok(parsed)
}

/// 解析允许为零但不能超过上限的 `u64` 配置项。
///
/// # Errors
///
/// 值不是整数或超过 `maximum` 时返回 [`CodeContextError::InvalidConfiguration`]。
fn parse_bounded_u64(value: &str, key: &str, maximum: u64) -> Result<u64, CodeContextError> {
    let parsed: u64 = value.trim().parse().map_err(|_| {
        CodeContextError::InvalidConfiguration(format!(
            "{key} 必须是 0 到 {maximum} 之间的整数，实际为 `{value}`"
        ))
    })?;
    if parsed > maximum {
        return Err(CodeContextError::InvalidConfiguration(format!(
            "{key} 不能超过 {maximum}，实际为 {parsed}"
        )));
    }
    Ok(parsed)
}

/// 解析允许为零但不能超过上限的 `u32` 配置项。
///
/// # Errors
///
/// 值不是整数或超过 `maximum` 时返回 [`CodeContextError::InvalidConfiguration`]。
fn parse_bounded_u32(value: &str, key: &str, maximum: u32) -> Result<u32, CodeContextError> {
    let parsed: u32 = value.trim().parse().map_err(|_| {
        CodeContextError::InvalidConfiguration(format!(
            "{key} 必须是 0 到 {maximum} 之间的整数，实际为 `{value}`"
        ))
    })?;
    if parsed > maximum {
        return Err(CodeContextError::InvalidConfiguration(format!(
            "{key} 不能超过 {maximum}，实际为 {parsed}"
        )));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造配置元数据。
    fn metadata(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// 空配置必须得到可用的默认值。
    #[test]
    fn empty_metadata_yields_defaults() {
        let config = CodeContextConfig::from_metadata(&HashMap::new()).expect("默认配置应可解析");
        assert_eq!(config, CodeContextConfig::default());
        assert!(config.enabled);
        assert_eq!(config.embedding_mode, EmbeddingMode::Api);
    }

    /// 显式配置必须覆盖默认值。
    #[test]
    fn explicit_values_override_defaults() {
        let config = CodeContextConfig::from_metadata(&metadata(&[
            ("enabled", "false"),
            ("embedding_mode", "local"),
            ("embedding_request_interval_ms", "2500"),
            ("embedding_max_retries", "6"),
            ("embedding_retry_base_ms", "7500"),
            ("rerank_model", "Qwen/Qwen3-Reranker-4B"),
        ]))
        .expect("显式配置应可解析");
        assert!(!config.enabled);
        assert_eq!(config.embedding_mode, EmbeddingMode::Local);
        assert_eq!(config.embedding_request_interval_ms, 2_500);
        assert_eq!(config.embedding_max_retries, 6);
        assert_eq!(config.embedding_retry_base_ms, 7_500);
        assert_eq!(
            config.rerank_model.as_deref(),
            Some("Qwen/Qwen3-Reranker-4B")
        );
    }

    /// 宿主注入的其他元数据不得干扰配置解析。
    #[test]
    fn unrelated_metadata_is_ignored() {
        let config = CodeContextConfig::from_metadata(&metadata(&[
            ("workspace_root", "/repo/a"),
            ("workspace_scope", "scope-a"),
        ]))
        .expect("无关元数据不应影响配置");
        assert_eq!(config, CodeContextConfig::default());
    }

    /// 非法布尔值必须报错，不能静默回退。
    #[test]
    fn invalid_enabled_is_rejected() {
        assert!(matches!(
            CodeContextConfig::from_metadata(&metadata(&[("enabled", "yes")])),
            Err(CodeContextError::InvalidConfiguration(_))
        ));
    }

    /// 未知嵌入模式必须报错。
    #[test]
    fn unknown_embedding_mode_is_rejected() {
        assert!(matches!(
            CodeContextConfig::from_metadata(&metadata(&[("embedding_mode", "cloud")])),
            Err(CodeContextError::InvalidConfiguration(_))
        ));
    }

    /// 限速与重试配置允许显式设为零，供无等待环境关闭对应机制。
    #[test]
    fn retry_timing_can_be_disabled() {
        let config = CodeContextConfig::from_metadata(&metadata(&[
            ("embedding_request_interval_ms", "0"),
            ("embedding_max_retries", "0"),
            ("embedding_retry_base_ms", "0"),
        ]))
        .expect("零值应关闭限速与重试等待");

        assert_eq!(config.embedding_request_interval_ms, 0);
        assert_eq!(config.embedding_max_retries, 0);
        assert_eq!(config.embedding_retry_base_ms, 0);
    }

    /// 限速与重试配置必须拒绝超过边界或无法解析的值。
    #[test]
    fn invalid_retry_configuration_is_rejected() {
        for (key, value) in [
            ("embedding_request_interval_ms", "60001"),
            ("embedding_max_retries", "11"),
            ("embedding_retry_base_ms", "30001"),
            ("embedding_retry_base_ms", "not-a-number"),
        ] {
            assert!(
                matches!(
                    CodeContextConfig::from_metadata(&metadata(&[(key, value)])),
                    Err(CodeContextError::InvalidConfiguration(_))
                ),
                "{key}={value} 应被拒绝"
            );
        }
    }
}
