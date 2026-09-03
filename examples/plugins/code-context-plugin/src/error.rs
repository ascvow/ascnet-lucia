//! Code Context Engine 的领域错误。
//!
//! 这些错误描述的是**引擎自身的状态**，与宿主能力被拒、存储失败等外部原因区分开。
//! 全部通过 `anyhow` 汇入 SDK 的 `Result`，因此它们既能携带稳定的判别信息，
//! 又不需要在插件里另立一套错误传播机制。

use std::fmt;

/// Code Context Engine 领域错误。
///
/// 刻意保持小集合：只覆盖当前阶段真实会发生的失败。新增变体应当伴随真实触发路径，
/// 而不是为将来可能出现的情况预留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeContextError {
    /// 宿主未提供本插件运行所需的能力（如项目作用域存储未注入）。
    CapabilityUnavailable(String),
    /// 持久存储读写失败。
    Storage(String),
    /// 索引 manifest 结构损坏或字段非法。
    InvalidManifest(String),
    /// 索引 manifest 的 schema 版本超出本插件支持范围。
    UnsupportedManifestVersion {
        /// 磁盘上读到的版本。
        found: u32,
        /// 当前插件支持的版本。
        expected: u32,
    },
    /// 宿主未提供可信的工作区身份，无法确定当前仓库。
    RepositoryUnavailable,
    /// 索引尚未建立，无法回答检索请求。
    NotIndexed,
    /// 嵌入能力不可用（未配置 provider，或宿主缺少对应能力）。
    EmbeddingUnavailable(String),
    /// 插件配置非法。
    InvalidConfiguration(String),
}

impl fmt::Display for CodeContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityUnavailable(detail) => {
                write!(formatter, "宿主能力不可用：{detail}")
            }
            Self::Storage(detail) => write!(formatter, "索引存储失败：{detail}"),
            Self::InvalidManifest(detail) => write!(formatter, "索引 manifest 无效：{detail}"),
            Self::UnsupportedManifestVersion { found, expected } => write!(
                formatter,
                "不支持的索引 manifest 版本 `{found}`，当前插件支持 `{expected}`"
            ),
            Self::RepositoryUnavailable => {
                write!(formatter, "宿主未提供可信的工作区身份，无法确定当前仓库")
            }
            Self::NotIndexed => write!(formatter, "当前仓库尚未建立代码索引"),
            Self::EmbeddingUnavailable(detail) => write!(formatter, "嵌入能力不可用：{detail}"),
            Self::InvalidConfiguration(detail) => write!(formatter, "插件配置无效：{detail}"),
        }
    }
}

impl std::error::Error for CodeContextError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本不兼容错误必须同时给出实际值与期望值，否则用户无法判断该升级还是该重建。
    #[test]
    fn unsupported_version_reports_both_versions() {
        let error = CodeContextError::UnsupportedManifestVersion {
            found: 7,
            expected: 1,
        };
        let text = error.to_string();
        assert!(text.contains('7'), "{text}");
        assert!(text.contains('1'), "{text}");
    }

    /// 领域错误必须能作为标准错误参与 anyhow 错误链。
    #[test]
    fn domain_error_is_a_standard_error() {
        let error = anyhow::Error::new(CodeContextError::NotIndexed);
        assert!(error.downcast_ref::<CodeContextError>().is_some());
    }
}
