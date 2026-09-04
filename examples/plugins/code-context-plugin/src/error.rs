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
    ///
    /// 表示**永久性**失败：模型名写错、密钥无效、参数不被接受。重试没有意义，
    /// 应当直接进入降级并把原因暴露给用户。
    EmbeddingUnavailable(String),
    /// 嵌入端点暂时不可用（限流、网关错误、网络抖动）。
    ///
    /// 与 [`Self::EmbeddingUnavailable`] 的区别是**能否恢复**。嵌入一个中等仓库需要
    /// 几百次请求，429 几乎必然发生；把它当成永久失败会让语义索引永远建不完，而且失败
    /// 是静默的——检索照常返回结果，只是少了语义那一路。调用方应当保留已完成的部分并
    /// 在后续轮次继续。
    EmbeddingTransient(String),
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
            Self::EmbeddingTransient(detail) => write!(formatter, "嵌入端点暂时不可用：{detail}"),
            Self::InvalidConfiguration(detail) => write!(formatter, "插件配置无效：{detail}"),
        }
    }
}

impl CodeContextError {
    /// 该错误是否值得重试。
    ///
    /// 只有明确判定为瞬时的失败才返回 `true`；无法确定时返回 `false`，避免对着一个
    /// 永久性配置错误无限重试。
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::EmbeddingTransient(_))
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
