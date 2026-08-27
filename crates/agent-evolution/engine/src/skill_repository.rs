//! Skill 制品 CAS 与只追加状态链仓库。
//!
//! Skill 正文始终写入通用 Artifact CAS；本模块只额外保存按 Skill 修订分区的状态链索引。
//! 索引中的每一项都引用一份完整、规范、不可变的 [`SkillArtifactV1`]，因此进程重启后
//! 仍可证明新状态只是在旧状态链尾部追加，而不是覆盖或删除既有历史。

use crate::{ArtifactStore, ArtifactStoreError, FileArtifactStore};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, InvalidSkillEvolution, SkillArtifactV1, SkillId,
    SkillStatusTransitionV1,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// SkillArtifact V1 规范 JSON 的媒体类型。
pub const SKILL_ARTIFACT_MEDIA_TYPE: &str = "application/vnd.ascnet.lucia.skill-artifact.v1+json";
/// 单个 Skill 制品允许的最大规范 JSON 字节数。
pub const MAX_SKILL_ARTIFACT_BYTES: usize = 128 * 1_024;
/// Skill 状态链索引结构版本。
pub const SKILL_STATUS_INDEX_SCHEMA_VERSION: u32 = 1;
/// 单条状态索引允许的最大 JSON 字节数。
pub const MAX_SKILL_STATUS_INDEX_BYTES: usize = 16 * 1_024;

/// 绑定真实 Artifact CAS 的 Skill 制品仓库。
#[derive(Debug, Clone, Copy)]
pub struct SkillArtifactRepository<'a> {
    artifacts: &'a FileArtifactStore,
}

impl<'a> SkillArtifactRepository<'a> {
    /// 创建借用现有 Artifact CAS 的 Skill 仓库，不产生文件系统副作用。
    pub fn new(artifacts: &'a FileArtifactStore) -> Self {
        Self { artifacts }
    }

    /// 规范化并幂等写入一份 Skill 制品。
    ///
    /// 相同制品总是得到相同摘要；写入前会校验全部 M7 协议不变量和固定字节上限。
    ///
    /// # Errors
    ///
    /// 制品无效、过大，或 Artifact CAS 写入失败时返回 [`SkillRepositoryError`]。
    pub async fn put(
        &self,
        artifact: &SkillArtifactV1,
    ) -> Result<ArtifactRef, SkillRepositoryError> {
        let bytes = canonical_artifact_bytes(artifact)?;
        self.artifacts
            .put(SKILL_ARTIFACT_MEDIA_TYPE, &bytes)
            .await
            .map_err(SkillRepositoryError::ArtifactStore)
    }

    /// 按摘要读取、解析并重新校验一份规范 Skill 制品。
    ///
    /// # Errors
    ///
    /// 制品不存在、CAS 完整性失败、字节过大、JSON 无效或不是规范字节时返回
    /// [`SkillRepositoryError`]。
    pub async fn get(
        &self,
        digest: &ArtifactDigest,
    ) -> Result<SkillArtifactV1, SkillRepositoryError> {
        let bytes = self
            .artifacts
            .get(digest)
            .await?
            .ok_or_else(|| SkillRepositoryError::ArtifactNotFound(digest.clone()))?;
        if bytes.len() > MAX_SKILL_ARTIFACT_BYTES {
            return Err(SkillRepositoryError::ArtifactTooLarge {
                size_bytes: bytes.len(),
                max_bytes: MAX_SKILL_ARTIFACT_BYTES,
            });
        }
        let artifact = SkillArtifactV1::from_json_slice(&bytes)?;
        if canonical_artifact_bytes(&artifact)? != bytes {
            return Err(SkillRepositoryError::NonCanonicalArtifact(digest.clone()));
        }
        if artifact.digest()? != *digest {
            return Err(SkillRepositoryError::ArtifactDigestMismatch {
                expected: digest.clone(),
                actual: artifact.digest()?,
            });
        }
        Ok(artifact)
    }

    /// 只计算合法 Skill 制品的规范摘要，不写入 CAS。
    ///
    /// # Errors
    ///
    /// 制品无效、过大或摘要构造失败时返回 [`SkillRepositoryError`]。
    pub fn digest(
        &self,
        artifact: &SkillArtifactV1,
    ) -> Result<ArtifactDigest, SkillRepositoryError> {
        let bytes = canonical_artifact_bytes(artifact)?;
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
            .map_err(|error| SkillRepositoryError::Digest(error.to_string()))
    }
}

/// 状态链中一条不可变索引记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStatusIndexEntryV1 {
    /// 状态索引结构版本。
    pub schema_version: u32,
    /// Skill 稳定 ID。
    pub skill_id: SkillId,
    /// Skill 内容修订号；每个修订拥有独立状态链。
    pub skill_revision: u32,
    /// 从 1 开始的状态链序号。
    pub sequence: u32,
    /// 携带该完整状态链前缀的不可变 SkillArtifact 摘要。
    pub artifact_digest: ArtifactDigest,
    /// 本次新增的状态记录。
    pub transition: SkillStatusTransitionV1,
}

impl SkillStatusIndexEntryV1 {
    /// 校验索引字段与所引用 Skill 制品的完整绑定。
    ///
    /// # Errors
    ///
    /// schema、Skill、修订、序号、摘要或链尾任一项不一致时返回
    /// [`SkillRepositoryError`]。
    pub fn validate_against(
        &self,
        artifact: &SkillArtifactV1,
        expected_digest: &ArtifactDigest,
    ) -> Result<(), SkillRepositoryError> {
        if self.schema_version != SKILL_STATUS_INDEX_SCHEMA_VERSION {
            return Err(SkillRepositoryError::UnsupportedStatusIndexSchema {
                found: self.schema_version,
                supported: SKILL_STATUS_INDEX_SCHEMA_VERSION,
            });
        }
        if self.skill_id != artifact.skill_id
            || self.skill_revision != artifact.revision
            || self.artifact_digest != *expected_digest
            || usize::try_from(self.sequence).ok() != Some(artifact.status_history.len())
            || artifact.status_history.last() != Some(&self.transition)
        {
            return Err(SkillRepositoryError::StatusIndexBindingMismatch);
        }
        Ok(())
    }
}

/// 基于不可变 JSON 索引与 Artifact CAS 的 Skill 状态链仓库。
///
/// 状态链按 `(skill_id, skill_revision)` 隔离。每次追加必须提交一份状态历史恰好多一项的
/// 新 SkillArtifact；重复提交同一链尾幂等成功，旧链尾或分叉提交失败关闭。
#[derive(Debug)]
pub struct FileSkillStatusStore<'a> {
    root: PathBuf,
    artifacts: SkillArtifactRepository<'a>,
}

impl<'a> FileSkillStatusStore<'a> {
    /// 创建延迟初始化的状态链仓库。
    pub fn new(root: impl Into<PathBuf>, artifacts: &'a FileArtifactStore) -> Self {
        Self {
            root: root.into(),
            artifacts: SkillArtifactRepository::new(artifacts),
        }
    }

    /// 返回状态索引根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 向指定 Skill 内容修订的状态链追加唯一后继。
    ///
    /// 方法先把完整 SkillArtifact 幂等写入 CAS，再比较现有链。首次追加只接受单项
    /// Quarantined 历史；后续提交必须保持全部非状态字段不变，并让 `status_history` 恰好
    /// 在既有链尾新增一项。相同链尾重试返回原引用，不制造重复索引。
    ///
    /// # Errors
    ///
    /// 制品或历史无效、状态历史被改写/跳跃、出现并发分叉、路径不安全，或 CAS/I/O 失败
    /// 时返回 [`SkillRepositoryError`]。
    pub async fn append(
        &self,
        artifact: &SkillArtifactV1,
    ) -> Result<ArtifactRef, SkillRepositoryError> {
        artifact.validate()?;
        let reference = self.artifacts.put(artifact).await?;
        let history = self.history(&artifact.skill_id, artifact.revision).await?;

        if let Some(latest) = history.last() {
            if latest.artifact_digest == reference.digest {
                return Ok(reference);
            }
            let previous = self.artifacts.get(&latest.artifact_digest).await?;
            validate_append_only(&previous, artifact)?;
        } else if artifact.status_history.len() != 1 {
            return Err(SkillRepositoryError::MissingStatusPrefix);
        }

        let sequence = u32::try_from(artifact.status_history.len())
            .map_err(|_| SkillRepositoryError::StatusSequenceOverflow)?;
        let entry = SkillStatusIndexEntryV1 {
            schema_version: SKILL_STATUS_INDEX_SCHEMA_VERSION,
            skill_id: artifact.skill_id.clone(),
            skill_revision: artifact.revision,
            sequence,
            artifact_digest: reference.digest.clone(),
            transition: artifact
                .status_history
                .last()
                .cloned()
                .ok_or(SkillRepositoryError::MissingStatusPrefix)?,
        };
        entry.validate_against(artifact, &reference.digest)?;
        self.commit_index(&entry).await?;
        Ok(reference)
    }

    /// 读取并完整复核指定 Skill 内容修订的状态链索引。
    ///
    /// 返回值按 `sequence` 升序排列。目录不存在时返回空列表；每一项都会从 CAS 复读
    /// SkillArtifact，并验证完整状态前缀和相邻追加关系。
    ///
    /// # Errors
    ///
    /// 目录或文件不安全、索引损坏、序号不连续、CAS 制品缺失/篡改，或历史不是只追加链时
    /// 返回 [`SkillRepositoryError`]。
    pub async fn history(
        &self,
        skill_id: &SkillId,
        skill_revision: u32,
    ) -> Result<Vec<SkillStatusIndexEntryV1>, SkillRepositoryError> {
        let directory = self.chain_dir(skill_id, skill_revision);
        if !validate_existing_directory(&directory).await? {
            return Ok(Vec::new());
        }
        let mut reader = fs::read_dir(&directory)
            .await
            .map_err(|source| io_error("遍历 Skill 状态目录", &directory, source))?;
        let mut paths = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|source| io_error("读取 Skill 状态目录项", &directory, source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut result = Vec::with_capacity(paths.len());
        let mut previous_artifact: Option<SkillArtifactV1> = None;
        for (index, path) in paths.into_iter().enumerate() {
            let expected_sequence = u32::try_from(index + 1)
                .map_err(|_| SkillRepositoryError::StatusSequenceOverflow)?;
            if path.file_name().and_then(|value| value.to_str())
                != Some(&format!("{expected_sequence:010}.json"))
            {
                return Err(SkillRepositoryError::BrokenStatusSequence {
                    skill_id: skill_id.clone(),
                    skill_revision,
                });
            }
            let bytes = read_safe_file(&path).await?;
            if bytes.len() > MAX_SKILL_STATUS_INDEX_BYTES {
                return Err(SkillRepositoryError::StatusIndexTooLarge {
                    path,
                    size_bytes: bytes.len(),
                    max_bytes: MAX_SKILL_STATUS_INDEX_BYTES,
                });
            }
            let entry: SkillStatusIndexEntryV1 =
                serde_json::from_slice(&bytes).map_err(|source| {
                    SkillRepositoryError::InvalidStatusIndex {
                        path: path.clone(),
                        source,
                    }
                })?;
            if entry.skill_id != *skill_id
                || entry.skill_revision != skill_revision
                || entry.sequence != expected_sequence
            {
                return Err(SkillRepositoryError::StatusIndexBindingMismatch);
            }
            let artifact = self.artifacts.get(&entry.artifact_digest).await?;
            entry.validate_against(&artifact, &entry.artifact_digest)?;
            if let Some(previous) = &previous_artifact {
                validate_append_only(previous, &artifact)?;
            } else if artifact.status_history.len() != 1 {
                return Err(SkillRepositoryError::MissingStatusPrefix);
            }
            previous_artifact = Some(artifact);
            result.push(entry);
        }
        Ok(result)
    }

    /// 返回指定 Skill 内容修订的最新完整制品；没有历史时为 `None`。
    ///
    /// # Errors
    ///
    /// 状态链或 CAS 制品无效时返回 [`SkillRepositoryError`]。
    pub async fn current(
        &self,
        skill_id: &SkillId,
        skill_revision: u32,
    ) -> Result<Option<SkillArtifactV1>, SkillRepositoryError> {
        let Some(entry) = self
            .history(skill_id, skill_revision)
            .await?
            .last()
            .cloned()
        else {
            return Ok(None);
        };
        self.artifacts.get(&entry.artifact_digest).await.map(Some)
    }

    fn chain_dir(&self, skill_id: &SkillId, skill_revision: u32) -> PathBuf {
        self.root
            .join(skill_id.as_str())
            .join(format!("{skill_revision:010}"))
    }

    async fn commit_index(
        &self,
        entry: &SkillStatusIndexEntryV1,
    ) -> Result<(), SkillRepositoryError> {
        let skill_dir = self.root.join(entry.skill_id.as_str());
        let chain_dir = self.chain_dir(&entry.skill_id, entry.skill_revision);
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&skill_dir).await?;
        ensure_safe_directory(&chain_dir).await?;
        let path = chain_dir.join(format!("{:010}.json", entry.sequence));
        let bytes = serde_json::to_vec(entry).map_err(SkillRepositoryError::Serialization)?;
        if bytes.len() > MAX_SKILL_STATUS_INDEX_BYTES {
            return Err(SkillRepositoryError::StatusIndexTooLarge {
                path,
                size_bytes: bytes.len(),
                max_bytes: MAX_SKILL_STATUS_INDEX_BYTES,
            });
        }
        let temporary = chain_dir.join(format!(".{}.tmp", Uuid::new_v4().simple()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|source| io_error("创建 Skill 状态临时文件", &temporary, source))?;
        file.write_all(&bytes)
            .await
            .map_err(|source| io_error("写入 Skill 状态临时文件", &temporary, source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("同步 Skill 状态临时文件", &temporary, source))?;
        drop(file);
        let linked = fs::hard_link(&temporary, &path).await;
        let _ = fs::remove_file(&temporary).await;
        match linked {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_safe_file(&path).await?;
                let decoded: SkillStatusIndexEntryV1 =
                    serde_json::from_slice(&existing).map_err(|source| {
                        SkillRepositoryError::InvalidStatusIndex {
                            path: path.clone(),
                            source,
                        }
                    })?;
                if decoded == *entry {
                    Ok(())
                } else {
                    Err(SkillRepositoryError::ConcurrentStatusUpdate {
                        skill_id: entry.skill_id.clone(),
                        skill_revision: entry.skill_revision,
                    })
                }
            }
            Err(source) => Err(io_error("提交 Skill 状态索引", &path, source)),
        }
    }
}

/// Skill 制品或状态链仓库错误。
#[derive(Debug, Error)]
pub enum SkillRepositoryError {
    /// SkillArtifact 违反 M7 协议。
    #[error("Skill 制品无效：{0}")]
    InvalidArtifact(#[from] InvalidSkillEvolution),
    /// SkillArtifact 超出固定字节上限。
    #[error("Skill 制品过大：{size_bytes} 字节，上限 {max_bytes} 字节")]
    ArtifactTooLarge {
        /// 实际字节数。
        size_bytes: usize,
        /// 最大字节数。
        max_bytes: usize,
    },
    /// 指定摘要不存在。
    #[error("Skill 制品不存在：{0}")]
    ArtifactNotFound(ArtifactDigest),
    /// CAS 字节虽可解析但不是规范 JSON。
    #[error("Skill 制品不是规范 JSON：{0}")]
    NonCanonicalArtifact(ArtifactDigest),
    /// 协议摘要与 CAS 摘要不一致。
    #[error("Skill 制品摘要不匹配：期望 {expected}，实际 {actual}")]
    ArtifactDigestMismatch {
        /// CAS 声明摘要。
        expected: ArtifactDigest,
        /// 规范制品实际摘要。
        actual: ArtifactDigest,
    },
    /// 摘要构造失败。
    #[error("Skill 制品摘要构造失败：{0}")]
    Digest(String),
    /// Artifact CAS 访问失败。
    #[error("访问 Skill Artifact CAS 失败：{0}")]
    ArtifactStore(#[from] ArtifactStoreError),
    /// 状态索引 schema 不受支持。
    #[error("不支持的 Skill 状态索引 schema {found}，当前支持 {supported}")]
    UnsupportedStatusIndexSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// 索引与制品绑定不一致。
    #[error("Skill 状态索引与制品 ID、修订、摘要、序号或链尾不一致")]
    StatusIndexBindingMismatch,
    /// 首次写入没有提交单项 Quarantined 前缀。
    #[error("Skill 状态链首次提交必须只包含初始 Quarantined 状态")]
    MissingStatusPrefix,
    /// 新制品修改了既有状态前缀或非状态内容。
    #[error("Skill 状态链只能追加一个状态，不能改写前缀或同修订内容")]
    NonAppendOnlyStatus,
    /// 状态序号溢出。
    #[error("Skill 状态链序号溢出")]
    StatusSequenceOverflow,
    /// 状态索引序号缺失或不连续。
    #[error("Skill `{skill_id}` 修订 {skill_revision} 的状态索引序号不连续")]
    BrokenStatusSequence {
        /// Skill ID。
        skill_id: SkillId,
        /// Skill 内容修订号。
        skill_revision: u32,
    },
    /// 并发写入产生不同链尾。
    #[error("Skill `{skill_id}` 修订 {skill_revision} 发生并发状态分叉")]
    ConcurrentStatusUpdate {
        /// Skill ID。
        skill_id: SkillId,
        /// Skill 内容修订号。
        skill_revision: u32,
    },
    /// 状态索引文件超过上限。
    #[error("Skill 状态索引过大：{path} 为 {size_bytes} 字节，上限 {max_bytes} 字节")]
    StatusIndexTooLarge {
        /// 文件路径。
        path: PathBuf,
        /// 实际字节数。
        size_bytes: usize,
        /// 最大字节数。
        max_bytes: usize,
    },
    /// 状态索引 JSON 损坏。
    #[error("Skill 状态索引损坏：{path}: {source}")]
    InvalidStatusIndex {
        /// 文件路径。
        path: PathBuf,
        /// JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 状态索引序列化失败。
    #[error("序列化 Skill 状态索引失败：{0}")]
    Serialization(serde_json::Error),
    /// 状态目录或文件不是安全的普通路径。
    #[error("Skill 状态存储路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 固定原因。
        reason: &'static str,
    },
    /// 文件系统访问失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        #[source]
        source: std::io::Error,
    },
}

fn canonical_artifact_bytes(artifact: &SkillArtifactV1) -> Result<Vec<u8>, SkillRepositoryError> {
    let bytes = artifact.canonical_bytes()?;
    if bytes.len() > MAX_SKILL_ARTIFACT_BYTES {
        return Err(SkillRepositoryError::ArtifactTooLarge {
            size_bytes: bytes.len(),
            max_bytes: MAX_SKILL_ARTIFACT_BYTES,
        });
    }
    Ok(bytes)
}

fn validate_append_only(
    previous: &SkillArtifactV1,
    next: &SkillArtifactV1,
) -> Result<(), SkillRepositoryError> {
    let prefix_is_unchanged = next.status_history.len() == previous.status_history.len() + 1
        && next.status_history.starts_with(&previous.status_history);
    let mut previous_without_status = previous.clone();
    previous_without_status.status_history.clear();
    let mut next_without_status = next.clone();
    next_without_status.status_history.clear();
    if !prefix_is_unchanged || previous_without_status != next_without_status {
        return Err(SkillRepositoryError::NonAppendOnlyStatus);
    }
    Ok(())
}

async fn ensure_safe_directory(path: &Path) -> Result<(), SkillRepositoryError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 Skill 状态目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Skill 状态目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SkillRepositoryError::UnsafePath {
            path: path.to_path_buf(),
            reason: "状态目录必须是非符号链接目录",
        });
    }
    Ok(())
}

async fn validate_existing_directory(path: &Path) -> Result<bool, SkillRepositoryError> {
    match fs::symlink_metadata(path).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("检查 Skill 状态目录", path, source)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(SkillRepositoryError::UnsafePath {
                path: path.to_path_buf(),
                reason: "状态目录必须是非符号链接目录",
            })
        }
        Ok(_) => Ok(true),
    }
}

async fn read_safe_file(path: &Path) -> Result<Vec<u8>, SkillRepositoryError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Skill 状态索引", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillRepositoryError::UnsafePath {
            path: path.to_path_buf(),
            reason: "状态索引必须是非符号链接普通文件",
        });
    }
    fs::read(path)
        .await
        .map_err(|source| io_error("读取 Skill 状态索引", path, source))
}

fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> SkillRepositoryError {
    SkillRepositoryError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        EpisodeId, EvaluationReportId, MutationId, SkillOperationV1, SkillStatusV1,
        SKILL_ARTIFACT_SCHEMA_VERSION,
    };

    fn roots() -> (PathBuf, PathBuf) {
        let nonce = Uuid::new_v4().simple();
        let base = std::env::temp_dir().join(format!("lucia-skill-repository-{nonce}"));
        (base.join("cas"), base.join("status"))
    }

    fn artifact(history: Vec<SkillStatusTransitionV1>) -> SkillArtifactV1 {
        SkillArtifactV1 {
            schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
            skill_id: SkillId::new("skill_repository").expect("测试 Skill ID 应合法"),
            revision: 1,
            operation: SkillOperationV1::Create,
            name: "证据归因".into(),
            description: "从可信 Episode 复核失败来源".into(),
            instructions: "只读取脱敏事件并保留来源引用。".into(),
            trigger_policy: Default::default(),
            required_capabilities: Default::default(),
            source_episode_ids: [EpisodeId::generate()].into_iter().collect(),
            mutation_id: MutationId::generate(),
            status_history: history,
        }
    }

    fn transition(
        status: SkillStatusV1,
        recorded_at_ms: u64,
        report: Option<EvaluationReportId>,
    ) -> SkillStatusTransitionV1 {
        SkillStatusTransitionV1 {
            status,
            recorded_at_ms,
            evaluation_report_id: report,
        }
    }

    #[tokio::test]
    async fn artifact_repository_round_trips_canonical_cas() {
        let (cas_root, status_root) = roots();
        let cas = FileArtifactStore::new(&cas_root);
        let repository = SkillArtifactRepository::new(&cas);
        let artifact = artifact(vec![transition(SkillStatusV1::Quarantined, 1, None)]);
        let first = repository.put(&artifact).await.expect("首次写入应成功");
        let second = repository.put(&artifact).await.expect("重复写入应幂等");
        assert_eq!(first, second);
        assert_eq!(
            repository.get(&first.digest).await.expect("应复读制品"),
            artifact
        );
        let _ = fs::remove_dir_all(cas_root.parent().expect("测试根目录应存在")).await;
        let _ = status_root;
    }

    #[tokio::test]
    async fn status_store_appends_exact_prefix_and_retries_idempotently() {
        let (cas_root, status_root) = roots();
        let cas = FileArtifactStore::new(&cas_root);
        let store = FileSkillStatusStore::new(&status_root, &cas);
        let first = artifact(vec![transition(SkillStatusV1::Quarantined, 1, None)]);
        store.append(&first).await.expect("应追加初始隔离状态");
        store.append(&first).await.expect("相同链尾重试应幂等");

        let report = EvaluationReportId::generate();
        let mut second = first.clone();
        second.status_history.push(transition(
            SkillStatusV1::Evaluated,
            2,
            Some(report.clone()),
        ));
        store.append(&second).await.expect("应追加评测状态");
        let mut third = second.clone();
        third
            .status_history
            .push(transition(SkillStatusV1::Active, 3, Some(report)));
        store.append(&third).await.expect("应追加启用状态");

        assert_eq!(
            store
                .history(&third.skill_id, third.revision)
                .await
                .expect("应读取完整状态链")
                .len(),
            3
        );
        assert_eq!(
            store
                .current(&third.skill_id, third.revision)
                .await
                .expect("应读取链尾"),
            Some(third)
        );
        let _ = fs::remove_dir_all(cas_root.parent().expect("测试根目录应存在")).await;
    }

    #[tokio::test]
    async fn status_store_rejects_rewritten_prefix_and_skipped_state() {
        let (cas_root, status_root) = roots();
        let cas = FileArtifactStore::new(&cas_root);
        let store = FileSkillStatusStore::new(&status_root, &cas);
        let first = artifact(vec![transition(SkillStatusV1::Quarantined, 1, None)]);
        store.append(&first).await.expect("应追加初始状态");

        let report = EvaluationReportId::generate();
        let mut skipped = first.clone();
        skipped.status_history = vec![
            transition(SkillStatusV1::Quarantined, 1, None),
            transition(SkillStatusV1::Evaluated, 2, Some(report.clone())),
            transition(SkillStatusV1::Active, 3, Some(report)),
        ];
        assert!(matches!(
            store.append(&skipped).await,
            Err(SkillRepositoryError::NonAppendOnlyStatus)
        ));

        let mut rewritten = first.clone();
        rewritten.name = "改写既有内容".into();
        rewritten.status_history.push(transition(
            SkillStatusV1::Evaluated,
            2,
            Some(EvaluationReportId::generate()),
        ));
        assert!(matches!(
            store.append(&rewritten).await,
            Err(SkillRepositoryError::NonAppendOnlyStatus)
        ));
        let _ = fs::remove_dir_all(cas_root.parent().expect("测试根目录应存在")).await;
    }
}
