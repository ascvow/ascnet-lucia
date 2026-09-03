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

/// Code Context Engine 配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeContextConfig {
    /// 引擎是否启用；关闭后不建立索引，`code_search` 明确报告已禁用。
    pub enabled: bool,
    /// 语义嵌入模式。
    ///
    /// 默认 `api`：它是最低硬件压力路径，且不要求用户具备本地推理条件。当前两种模式
    /// 的 provider 都尚未实现，该值只决定将来启用哪条分支与如何报告不可用原因。
    pub embedding_mode: EmbeddingMode,
}

impl Default for CodeContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            embedding_mode: EmbeddingMode::Api,
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
        Ok(config)
    }
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
