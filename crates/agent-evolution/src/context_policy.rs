//! Context Policy 参数制品的规范化 CAS 读写。

use crate::{ArtifactStore, ArtifactStoreError, FileArtifactStore};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, ContextPolicyV1, InvalidContextPolicy,
};
use sha2::{Digest, Sha256};

/// Context Policy V1 规范 JSON 的媒体类型。
pub const CONTEXT_POLICY_MEDIA_TYPE: &str = "application/vnd.ascnet.lucia.context-policy.v1+json";
/// Context Policy 规范 JSON 允许的最大字节数。
pub const MAX_CONTEXT_POLICY_BYTES: usize = 64 * 1_024;

/// 绑定真实 Artifact CAS 的 Context Policy 参数仓库。
///
/// 该仓库不解释 Genome owner，也不装配插件；它只保证策略经过结构校验、使用规范 JSON，
/// 并在读取时由 Artifact Store 重新校验 SHA-256。
#[derive(Debug)]
pub struct ContextPolicyRepository<'a> {
    artifacts: &'a FileArtifactStore,
}

impl<'a> ContextPolicyRepository<'a> {
    /// 创建一个借用现有 Artifact CAS 的策略仓库，不产生文件系统副作用。
    pub fn new(artifacts: &'a FileArtifactStore) -> Self {
        Self { artifacts }
    }

    /// 规范化并写入一份 Context Policy，返回内容寻址引用。
    ///
    /// 相同结构值总是得到相同摘要；写入前会执行完整参数校验和字节上限检查。
    ///
    /// # Errors
    ///
    /// 策略无效、规范 JSON 超过上限，或 Artifact CAS 写入失败时返回
    /// [`ContextPolicyRepositoryError`]。
    pub async fn put(
        &self,
        policy: &ContextPolicyV1,
    ) -> Result<ArtifactRef, ContextPolicyRepositoryError> {
        let bytes = canonical_policy_bytes(policy)?;
        self.artifacts
            .put(CONTEXT_POLICY_MEDIA_TYPE, &bytes)
            .await
            .map_err(ContextPolicyRepositoryError::ArtifactStore)
    }

    /// 按摘要读取、解析并重新校验一份 Context Policy。
    ///
    /// # Errors
    ///
    /// 制品不存在、CAS 完整性校验失败、字节超过上限，或内容不是合法 Context Policy 时
    /// 返回 [`ContextPolicyRepositoryError`]。
    pub async fn get(
        &self,
        digest: &ArtifactDigest,
    ) -> Result<ContextPolicyV1, ContextPolicyRepositoryError> {
        let bytes = self
            .artifacts
            .get(digest)
            .await?
            .ok_or_else(|| ContextPolicyRepositoryError::NotFound(digest.clone()))?;
        if bytes.len() > MAX_CONTEXT_POLICY_BYTES {
            return Err(ContextPolicyRepositoryError::TooLarge {
                size_bytes: bytes.len(),
                max_bytes: MAX_CONTEXT_POLICY_BYTES,
            });
        }
        let policy = ContextPolicyV1::from_json_slice(&bytes)?;
        let canonical = canonical_policy_bytes(&policy)?;
        if canonical != bytes {
            return Err(ContextPolicyRepositoryError::NonCanonical(digest.clone()));
        }
        Ok(policy)
    }

    /// 只计算合法策略的规范制品摘要，不写入 CAS。
    ///
    /// # Errors
    ///
    /// 策略无效或规范 JSON 超过上限时返回 [`ContextPolicyRepositoryError`]。
    pub fn digest(
        &self,
        policy: &ContextPolicyV1,
    ) -> Result<ArtifactDigest, ContextPolicyRepositoryError> {
        let bytes = canonical_policy_bytes(policy)?;
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
            .map_err(|error| ContextPolicyRepositoryError::Digest(error.to_string()))
    }
}

/// 返回通过参数校验和字节上限校验的规范 JSON。
fn canonical_policy_bytes(
    policy: &ContextPolicyV1,
) -> Result<Vec<u8>, ContextPolicyRepositoryError> {
    let bytes = policy.canonical_bytes()?;
    if bytes.len() > MAX_CONTEXT_POLICY_BYTES {
        return Err(ContextPolicyRepositoryError::TooLarge {
            size_bytes: bytes.len(),
            max_bytes: MAX_CONTEXT_POLICY_BYTES,
        });
    }
    Ok(bytes)
}

/// Context Policy CAS 读写或完整性复核错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextPolicyRepositoryError {
    /// Context Policy 参数无效或无法规范序列化。
    #[error("Context Policy 参数无效：{0}")]
    InvalidPolicy(#[from] InvalidContextPolicy),
    /// 规范 JSON 超过固定字节上限。
    #[error("Context Policy 制品过大：{size_bytes} 字节，上限 {max_bytes} 字节")]
    TooLarge {
        /// 实际字节数。
        size_bytes: usize,
        /// 固定最大字节数。
        max_bytes: usize,
    },
    /// 指定摘要在 CAS 中不存在。
    #[error("Context Policy 制品不存在：{0}")]
    NotFound(ArtifactDigest),
    /// CAS 内容可解析但不是协议规定的规范 JSON 字节。
    #[error("Context Policy 制品不是规范 JSON：{0}")]
    NonCanonical(ArtifactDigest),
    /// 摘要构造失败。
    #[error("Context Policy 摘要构造失败：{0}")]
    Digest(String),
    /// Artifact CAS 访问或 SHA-256 复核失败。
    #[error("访问 Context Policy Artifact CAS 失败：{0}")]
    ArtifactStore(#[from] ArtifactStoreError),
}
