//! 独立 Skill Evaluator 使用的受信证据与健康 Registry。

use crate::{SkillActivationAuthorizationV1, SkillExitGateError};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, CandidateId, EvaluationReportId, EventId, GenomeRevisionId,
    ReleaseId, SkillHealthStatusV1, SkillUsageObservationV1, TrustedSkillUsageBindingV1,
    SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::fs;

/// 受信 Skill Registry 的固定文件名。
pub const SKILL_EVALUATION_REGISTRY_FILE: &str = "registry-v1.json";
/// 受信 Skill Registry 的结构版本。
pub const SKILL_EVALUATION_REGISTRY_SCHEMA_VERSION: u32 = 1;
/// 单份受信 Skill Registry 允许的最大字节数。
pub const MAX_SKILL_EVALUATION_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;

/// 受信控制面授予的可序列化 Skill 激活授权。
///
/// 该结构只能从摘要固定的 Evaluator Registry 读取，不能从普通 IPC 请求反序列化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillRegistryAuthorizationV1 {
    /// 只允许隔离评测，不允许生产 Stable 发布。
    LocalEvaluation,
    /// 受信人工或外部策略批准。
    Approved {
        /// 可审计批准记录 ID。
        approval_id: String,
    },
    /// 受信 Canary Gate 已通过。
    CanaryPassed {
        /// Canary 报告 ID。
        canary_report_id: EvaluationReportId,
    },
}

impl SkillRegistryAuthorizationV1 {
    /// 转换为 Exit Gate 的不可反序列化授权类型。
    ///
    /// # Errors
    ///
    /// Approved 记录 ID 不合法时返回 [`SkillRegistryError`]。
    pub fn gate_authorization(&self) -> Result<SkillActivationAuthorizationV1, SkillRegistryError> {
        match self {
            Self::LocalEvaluation => Ok(SkillActivationAuthorizationV1::local_evaluation()),
            Self::Approved { approval_id } if valid_control_id(approval_id) => {
                SkillActivationAuthorizationV1::approved(approval_id.clone())
                    .map_err(SkillRegistryError::Authorization)
            }
            Self::CanaryPassed { canary_report_id } => Ok(
                SkillActivationAuthorizationV1::canary_passed(canary_report_id.clone()),
            ),
            Self::Approved { .. } => Err(SkillRegistryError::InvalidAuthorizationEvidence),
        }
    }

    /// 返回不含正文的授权证据 ID。
    pub fn evidence_id(&self) -> String {
        match self {
            Self::LocalEvaluation => "local-evaluation".into(),
            Self::Approved { approval_id } => approval_id.clone(),
            Self::CanaryPassed { canary_report_id } => canary_report_id.to_string(),
        }
    }
}

/// 单个 Candidate 的可信 Episode 绑定、使用观察和生产授权。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvaluationRegistryEntryV1 {
    /// Candidate 身份。
    pub candidate_id: CandidateId,
    /// Candidate Genome Revision。
    pub candidate_revision_id: GenomeRevisionId,
    /// 由受信控制面预分配的正式报告 ID。
    pub report_id: EvaluationReportId,
    /// Candidate 规范 JSON 的 Artifact CAS 引用。
    pub candidate_artifact: ArtifactRef,
    /// 从真实 Episode 重新提取后必须逐字一致的绑定。
    pub trusted_usage_bindings: BTreeMap<EventId, TrustedSkillUsageBindingV1>,
    /// 与真实绑定逐项复核的可信使用观察。
    pub observations: Vec<SkillUsageObservationV1>,
    /// Gate 之外的激活授权。
    pub authorization: SkillRegistryAuthorizationV1,
    /// Gate 记录 Evaluated 状态的受信时间。
    pub evaluated_at_ms: u64,
    /// Gate 记录 Active 状态的受信时间。
    pub activated_at_ms: u64,
}

/// Promotion 后由受信控制面追加的 Skill 健康记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillHealthRegistryEntryV1 {
    /// Promotion Release。
    pub release_id: ReleaseId,
    /// Stable lineage。
    pub lineage: String,
    /// Promotion Revision。
    pub revision_id: GenomeRevisionId,
    /// Promotion 代数。
    pub generation: u64,
    /// 受信健康结论。
    pub result: SkillHealthStatusV1,
}

/// 摘要固定、版本化且拒绝未知字段的 Skill 受信 Registry。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvaluationRegistryV1 {
    /// Registry 结构版本。
    pub schema_version: u32,
    /// 按 Candidate ID 严格升序的评测证据。
    pub evaluations: Vec<SkillEvaluationRegistryEntryV1>,
    /// 按 Release ID 严格升序的健康记录。
    pub health: Vec<SkillHealthRegistryEntryV1>,
}

impl SkillEvaluationRegistryV1 {
    /// 校验版本、排序、Candidate/Episode/Run/Skill 绑定和健康记录局部结构。
    ///
    /// # Errors
    ///
    /// Registry 版本、排序、Artifact、观察、授权或健康记录无效时返回
    /// [`SkillRegistryError`]。
    pub fn validate(&self) -> Result<(), SkillRegistryError> {
        if self.schema_version != SKILL_EVALUATION_REGISTRY_SCHEMA_VERSION {
            return Err(SkillRegistryError::UnsupportedSchema(self.schema_version));
        }
        if !self
            .evaluations
            .windows(2)
            .all(|pair| pair[0].candidate_id < pair[1].candidate_id)
            || !self
                .health
                .windows(2)
                .all(|pair| pair[0].release_id < pair[1].release_id)
        {
            return Err(SkillRegistryError::UnsortedEntries);
        }
        for entry in &self.evaluations {
            if entry.candidate_artifact.media_type != SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE
                || entry.candidate_artifact.size_bytes == 0
                || entry.observations.is_empty()
                || entry.trusted_usage_bindings.is_empty()
                || entry.evaluated_at_ms == 0
                || entry.activated_at_ms <= entry.evaluated_at_ms
            {
                return Err(SkillRegistryError::InvalidEvaluationEntry(
                    entry.candidate_id.clone(),
                ));
            }
            entry.authorization.gate_authorization()?;
            if !entry.observations.windows(2).all(|pair| {
                pair[0].binding.tool_event.event_id < pair[1].binding.tool_event.event_id
            }) || entry.trusted_usage_bindings.values().any(|binding| {
                binding.validate().is_err()
                    || binding.genome_revision_id != entry.candidate_revision_id
            }) {
                return Err(SkillRegistryError::InvalidEvaluationEntry(
                    entry.candidate_id.clone(),
                ));
            }
            for observation in &entry.observations {
                let binding = entry
                    .trusted_usage_bindings
                    .get(&observation.binding.tool_event.event_id)
                    .ok_or_else(|| {
                        SkillRegistryError::InvalidEvaluationEntry(entry.candidate_id.clone())
                    })?;
                observation.validate(binding).map_err(|_| {
                    SkillRegistryError::InvalidEvaluationEntry(entry.candidate_id.clone())
                })?;
                if binding.genome_revision_id != entry.candidate_revision_id {
                    return Err(SkillRegistryError::InvalidEvaluationEntry(
                        entry.candidate_id.clone(),
                    ));
                }
            }
        }
        for entry in &self.health {
            if !valid_lineage(&entry.lineage) || entry.generation == 0 {
                return Err(SkillRegistryError::InvalidHealthEntry(
                    entry.release_id.clone(),
                ));
            }
            entry
                .result
                .validate()
                .map_err(|_| SkillRegistryError::InvalidHealthEntry(entry.release_id.clone()))?;
        }
        Ok(())
    }

    /// 返回与 Candidate 身份和 CAS 引用同时匹配的唯一评测记录。
    pub fn evaluation(
        &self,
        candidate_id: &CandidateId,
        candidate_artifact: &ArtifactRef,
    ) -> Option<&SkillEvaluationRegistryEntryV1> {
        self.evaluations
            .binary_search_by(|entry| entry.candidate_id.cmp(candidate_id))
            .ok()
            .and_then(|index| self.evaluations.get(index))
            .filter(|entry| &entry.candidate_artifact == candidate_artifact)
    }

    /// 返回与 Release、lineage、Revision 和代数全部匹配的唯一健康记录。
    pub fn health(
        &self,
        release_id: &ReleaseId,
        lineage: &str,
        revision_id: &GenomeRevisionId,
        generation: u64,
    ) -> Option<&SkillHealthRegistryEntryV1> {
        self.health
            .binary_search_by(|entry| entry.release_id.cmp(release_id))
            .ok()
            .and_then(|index| self.health.get(index))
            .filter(|entry| {
                entry.lineage == lineage
                    && entry.revision_id == *revision_id
                    && entry.generation == generation
            })
    }
}

/// 已通过绝对路径、固定根、摘要和规范 JSON 复核的 Registry。
#[derive(Debug, Clone)]
pub struct TrustedSkillEvaluationRegistry {
    registry: SkillEvaluationRegistryV1,
    digest: ArtifactDigest,
}

impl TrustedSkillEvaluationRegistry {
    /// 从固定 Evolution 根下的 Registry 目录加载规范 JSON。
    ///
    /// # Errors
    ///
    /// 路径不是绝对路径、逃离 Evolution 根、含符号链接，文件过大、摘要不一致、JSON
    /// 非规范编码或 Registry 内容无效时返回 [`SkillRegistryError`]。
    pub async fn open_pinned(
        evolution_root: &Path,
        registry_root: &Path,
        expected_digest: ArtifactDigest,
    ) -> Result<Self, SkillRegistryError> {
        ensure_trusted_subdirectory(evolution_root, registry_root).await?;
        let path = registry_root.join(SKILL_EVALUATION_REGISTRY_FILE);
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(|source| registry_io_error("检查 Skill Registry", &path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SkillRegistryError::UnsafePath(path));
        }
        if metadata.len() > MAX_SKILL_EVALUATION_REGISTRY_BYTES {
            return Err(SkillRegistryError::RegistryTooLarge(metadata.len()));
        }
        let bytes = fs::read(&path)
            .await
            .map_err(|source| registry_io_error("读取 Skill Registry", &path, source))?;
        let actual = digest_bytes(&bytes)?;
        if actual != expected_digest {
            return Err(SkillRegistryError::DigestMismatch {
                expected: expected_digest,
                actual,
            });
        }
        let registry: SkillEvaluationRegistryV1 =
            serde_json::from_slice(&bytes).map_err(SkillRegistryError::InvalidJson)?;
        registry.validate()?;
        let canonical = serde_json::to_vec(&registry).map_err(SkillRegistryError::Serialize)?;
        if canonical != bytes {
            return Err(SkillRegistryError::NonCanonicalJson);
        }
        Ok(Self {
            registry,
            digest: actual,
        })
    }

    /// 返回已校验 Registry 正文。
    pub fn registry(&self) -> &SkillEvaluationRegistryV1 {
        &self.registry
    }

    /// 返回 Registry 规范 JSON 摘要。
    pub fn digest(&self) -> &ArtifactDigest {
        &self.digest
    }
}

/// Skill Registry 加载与绑定错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillRegistryError {
    /// 路径不是绝对路径、逃逸固定根或包含非预期文件类型。
    #[error("Skill Registry 路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// Registry 文件超过固定上限。
    #[error("Skill Registry 过大：{0} 字节")]
    RegistryTooLarge(u64),
    /// Registry 摘要与受信环境固定值不一致。
    #[error("Skill Registry 摘要不一致：期望 {expected}，实际 {actual}")]
    DigestMismatch {
        /// 受信配置摘要。
        expected: ArtifactDigest,
        /// 实际文件摘要。
        actual: ArtifactDigest,
    },
    /// Registry JSON 损坏或含未知字段。
    #[error("Skill Registry JSON 无效：{0}")]
    InvalidJson(serde_json::Error),
    /// Registry 无法编码为规范 JSON。
    #[error("Skill Registry 规范序列化失败：{0}")]
    Serialize(serde_json::Error),
    /// 文件正文不是唯一规范 JSON 编码。
    #[error("Skill Registry 必须使用规范 JSON 编码")]
    NonCanonicalJson,
    /// Registry schema 不受支持。
    #[error("不支持的 Skill Registry schema：{0}")]
    UnsupportedSchema(u32),
    /// Candidate 或 Release 条目未严格排序。
    #[error("Skill Registry 条目必须严格排序且唯一")]
    UnsortedEntries,
    /// Candidate 评测条目绑定无效。
    #[error("Skill Registry Candidate 条目无效：{0}")]
    InvalidEvaluationEntry(CandidateId),
    /// 健康条目绑定无效。
    #[error("Skill Registry 健康条目无效：{0}")]
    InvalidHealthEntry(ReleaseId),
    /// 授权记录无效。
    #[error("Skill Registry 授权无效：{0}")]
    Authorization(SkillExitGateError),
    /// 授权证据 ID 不是有界控制面标识。
    #[error("Skill Registry 授权证据 ID 无效")]
    InvalidAuthorizationEvidence,
    /// 摘要文本无法构造成强类型摘要。
    #[error("Skill Registry 摘要无效：{0}")]
    InvalidDigest(String),
    /// 文件系统操作失败。
    #[error("{operation}失败 `{path}`：{source}")]
    Io {
        /// 操作名。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始错误。
        #[source]
        source: std::io::Error,
    },
}

/// 校验授权证据只包含有界 ASCII 控制面标识。
fn valid_control_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// 校验健康记录 lineage 不含绝对路径、空段或路径穿越。
fn valid_lineage(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.starts_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// 校验 Registry 是固定 Evolution 根下的绝对非符号链接目录。
async fn ensure_trusted_subdirectory(
    evolution_root: &Path,
    registry_root: &Path,
) -> Result<(), SkillRegistryError> {
    if !evolution_root.is_absolute()
        || !registry_root.is_absolute()
        || registry_root.parent() != Some(evolution_root)
    {
        return Err(SkillRegistryError::UnsafePath(registry_root.to_path_buf()));
    }
    for path in [evolution_root, registry_root] {
        let metadata = fs::symlink_metadata(path)
            .await
            .map_err(|source| registry_io_error("检查 Skill Registry 目录", path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SkillRegistryError::UnsafePath(path.to_path_buf()));
        }
    }
    Ok(())
}

/// 计算强类型 SHA-256 摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, SkillRegistryError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| SkillRegistryError::InvalidDigest(error.to_string()))
}

/// 构造带路径上下文的 Registry I/O 错误。
fn registry_io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> SkillRegistryError {
    SkillRegistryError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}
