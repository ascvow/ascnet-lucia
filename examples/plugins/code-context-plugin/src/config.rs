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
    /// 是否在嵌入请求中发送 `dimensions` 参数。
    ///
    /// 部分 OpenAI 兼容端点（如 SiliconFlow 的 `BAAI/bge-m3`）不接受该参数，设为 `false`
    /// 可避免 400 错误。维度校验仍在返回时执行，因此关闭此项不影响安全性。
    pub embedding_send_dimensions: bool,
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
            embedding_send_dimensions: true,
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
        ]))
        .expect("显式配置应可解析");
        assert!(!config.enabled);
        assert_eq!(config.embedding_mode, EmbeddingMode::Local);
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
}
