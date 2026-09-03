//! 嵌入 provider 边界。
//!
//! 这一层的存在理由是**隔离**，不是复用：检索核心只依赖 [`EmbeddingProvider`]，因此
//! Local 与 API 两条分支的可用性变化不会渗进索引与检索代码。
//!
//! 当前两个真实实现都不存在，原因写在各自的 ADR 中：
//!
//! - API：宿主在加载期拒绝 `http` 能力，见 `adr-api-embedding.md`。
//! - Local：`host-fs-read` 只返回 UTF-8，权重文件在 ABI 层面读不出来，见
//!   `adr-local-embedding.md`。
//!
//! 因此本阶段只提供 [`UnconfiguredEmbeddingProvider`]，它对每次调用都明确报告不可用。
//! **它不返回任何伪造向量**——一份看似成功的随机向量会让整条语义链路的错误推迟到
//! 检索结果里才暴露。

use crate::config::EmbeddingMode;
use crate::error::CodeContextError;
use crate::model::EmbeddingSpace;

/// 嵌入输入的用途。
///
/// 查询与文档通常使用不同的提示模板，因此两者必须在接口上区分，而不是靠调用方
/// 自行拼接——模板版本是 [`EmbeddingSpace`] 的一部分，混用会让向量不可比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingInputKind {
    /// 检索查询。
    Query,
    /// 待索引的代码块。
    Document,
}

/// 嵌入 provider。
pub trait EmbeddingProvider {
    /// 返回该 provider 当前的嵌入空间；未配置时返回 `None`。
    ///
    /// 语义索引的兼容性判断依赖它：返回 `None` 表示当前没有可用的语义能力，
    /// 已有向量应当保留而不是重建。
    fn space(&self) -> Option<EmbeddingSpace>;

    /// 批量计算嵌入向量。
    ///
    /// # Errors
    ///
    /// provider 不可用或远端失败时返回 [`CodeContextError::EmbeddingUnavailable`]。
    /// 调用方必须把该错误处理为「进入降级检索」，而不是让整次检索失败。
    fn embed(
        &self,
        kind: EmbeddingInputKind,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, CodeContextError>;
}

/// 尚未配置的嵌入 provider。
///
/// 命名刻意说明状态而不是伪装成实现：调用它永远失败，且失败原因指出缺的是哪一项
/// 宿主能力，使用户看到的是「还没有这个能力」而不是「检索坏了」。
#[derive(Debug, Clone, Copy)]
pub struct UnconfiguredEmbeddingProvider {
    /// 用户配置的目标模式，用于给出对应的不可用原因。
    mode: EmbeddingMode,
}

impl UnconfiguredEmbeddingProvider {
    /// 按目标模式创建未配置 provider。
    pub fn new(mode: EmbeddingMode) -> Self {
        Self { mode }
    }

    /// 返回面向用户的不可用原因。
    pub fn reason(self) -> String {
        match self.mode {
            EmbeddingMode::Api => {
                "API 嵌入需要宿主提供 HTTP 能力，当前宿主在加载期拒绝该声明".to_string()
            }
            EmbeddingMode::Local => {
                "本地嵌入需要宿主提供受控推理能力，当前插件运行时无法加载模型权重".to_string()
            }
        }
    }
}

impl EmbeddingProvider for UnconfiguredEmbeddingProvider {
    fn space(&self) -> Option<EmbeddingSpace> {
        None
    }

    fn embed(
        &self,
        _kind: EmbeddingInputKind,
        _inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, CodeContextError> {
        Err(CodeContextError::EmbeddingUnavailable(self.reason()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未配置 provider 不得声明任何嵌入空间，否则会污染语义索引兼容判断。
    #[test]
    fn unconfigured_provider_has_no_space() {
        let provider = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Api);
        assert!(provider.space().is_none());
    }

    /// 未配置 provider 必须失败，绝不能返回伪造向量。
    #[test]
    fn unconfigured_provider_never_returns_vectors() {
        for mode in [EmbeddingMode::Api, EmbeddingMode::Local] {
            let provider = UnconfiguredEmbeddingProvider::new(mode);
            let result = provider.embed(
                EmbeddingInputKind::Query,
                &["where are permissions checked".to_string()],
            );
            assert!(matches!(
                result,
                Err(CodeContextError::EmbeddingUnavailable(_))
            ));
        }
    }

    /// 两种模式必须给出各自缺失的宿主能力，而不是同一句泛化文案。
    #[test]
    fn unavailable_reason_names_the_missing_capability() {
        let api = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Api).reason();
        let local = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Local).reason();
        assert!(api.contains("HTTP"), "{api}");
        assert!(local.contains("推理"), "{local}");
        assert_ne!(api, local);
    }
}
