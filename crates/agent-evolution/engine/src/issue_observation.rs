//! Issue 聚合的只追加观察日志，保证跨进程 Episode 计数不丢失。

use crate::aggregation::fingerprint_for;
use agent_evolution_protocol::{
    EpisodeId, EvolutionIssueId, FailureFingerprint, FailureRecord, GenomeDigest,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use thiserror::Error;

/// Issue 观察日志当前 Schema 版本。
pub const ISSUE_OBSERVATION_SCHEMA_VERSION: u32 = 1;
/// 观察日志文件名前缀。
const OBSERVATION_PREFIX: &str = "issue-observation";

/// 一条失败记录进入 Issue 聚合器时持久化的不可变观察。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueObservation {
    /// 观察日志 Schema 版本。
    pub schema_version: u32,
    /// 首次聚合时分配并在重启后复用的 Issue ID。
    pub issue_id: EvolutionIssueId,
    /// 用于校验重建结果的稳定失败指纹。
    pub fingerprint: FailureFingerprint,
    /// 发生失败的 Episode；同一指纹按不同 Episode 计数。
    pub episode_id: EpisodeId,
    /// 确定性失败归因记录。
    pub record: FailureRecord,
}

impl IssueObservation {
    /// 从 Pipeline 已确认的聚合结果创建观察记录。
    pub fn new(
        issue_id: EvolutionIssueId,
        episode_id: EpisodeId,
        genome_digest: &GenomeDigest,
        record: FailureRecord,
    ) -> Self {
        Self {
            schema_version: ISSUE_OBSERVATION_SCHEMA_VERSION,
            issue_id,
            fingerprint: fingerprint_for(&record, genome_digest),
            episode_id,
            record,
        }
    }

    /// 返回由指纹和 Episode 决定的幂等观察键。
    pub fn observation_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.fingerprint.stable_key().as_bytes());
        hasher.update(b"\0");
        hasher.update(self.episode_id.as_str().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// 校验 Schema、Episode 绑定、归因和稳定指纹。
    ///
    /// # Errors
    ///
    /// Schema 不支持、记录绑定其他 Episode、归因不合法或指纹与记录不一致时返回错误。
    pub fn validate(&self) -> Result<(), IssueObservationError> {
        if self.schema_version != ISSUE_OBSERVATION_SCHEMA_VERSION {
            return Err(IssueObservationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.record.episode_id != self.episode_id {
            return Err(IssueObservationError::MixedEpisode);
        }
        self.record
            .attribution
            .validate()
            .map_err(|error| IssueObservationError::InvalidRecord(error.to_string()))?;
        if fingerprint_for(&self.record, &self.fingerprint.genome_digest) != self.fingerprint {
            return Err(IssueObservationError::FingerprintMismatch);
        }
        Ok(())
    }
}

/// Issue 观察日志存储接口。
#[async_trait]
pub trait IssueObservationStore: Send + Sync {
    /// 幂等追加一条观察；相同指纹与 Episode 已存在时不覆盖原记录。
    async fn append(&self, observation: &IssueObservation) -> Result<(), IssueObservationError>;

    /// 读取全部经过完整性校验的观察，顺序按幂等观察键稳定排列。
    async fn all(&self) -> Result<Vec<IssueObservation>, IssueObservationError>;
}

/// 基于不可变 JSON 文件的本地 Issue 观察日志。
#[derive(Debug, Clone)]
pub struct FileIssueObservationStore {
    root: PathBuf,
}

impl FileIssueObservationStore {
    /// 创建延迟初始化的观察日志；构造本身不触碰文件系统。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 根据幂等观察键生成单段安全文件名。
    fn path_for(&self, observation_id: &str) -> PathBuf {
        self.root
            .join(format!("{OBSERVATION_PREFIX}-{observation_id}.json"))
    }
}

#[async_trait]
impl IssueObservationStore for FileIssueObservationStore {
    async fn append(&self, observation: &IssueObservation) -> Result<(), IssueObservationError> {
        observation.validate()?;
        ensure_safe_root(&self.root).await?;
        let observation_id = observation.observation_id();
        let path = self.path_for(&observation_id);
        let bytes =
            serde_json::to_vec_pretty(observation).map_err(IssueObservationError::Serialization)?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&path).await {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                file.write_all(&bytes)
                    .await
                    .map_err(|source| io_error("写入 Issue 观察", &path, source))?;
                file.sync_all()
                    .await
                    .map_err(|source| io_error("同步 Issue 观察", &path, source))
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_observation(&path).await?;
                if existing.observation_id() == observation_id
                    && existing.issue_id == observation.issue_id
                {
                    Ok(())
                } else {
                    Err(IssueObservationError::ConflictingObservation(
                        observation_id,
                    ))
                }
            }
            Err(source) => Err(io_error("创建 Issue 观察", &path, source)),
        }
    }

    async fn all(&self) -> Result<Vec<IssueObservation>, IssueObservationError> {
        match tokio::fs::symlink_metadata(&self.root).await {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(IssueObservationError::UnsafeRoot(self.root.clone())),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查 Issue 观察目录", &self.root, source)),
        }
        let mut directory = tokio::fs::read_dir(&self.root)
            .await
            .map_err(|source| io_error("遍历 Issue 观察目录", &self.root, source))?;
        let mut paths = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|source| io_error("读取 Issue 观察目录项", &self.root, source))?
        {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                return Err(IssueObservationError::UnsafeRecord(path));
            };
            if name.starts_with(&format!("{OBSERVATION_PREFIX}-")) && name.ends_with(".json") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut observations = Vec::with_capacity(paths.len());
        let mut issue_ids = BTreeMap::<String, EvolutionIssueId>::new();
        for path in paths {
            let observation = read_observation(&path).await?;
            if self.path_for(&observation.observation_id()) != path {
                return Err(IssueObservationError::UnsafeRecord(path));
            }
            let key = observation.fingerprint.stable_key();
            if let Some(existing) = issue_ids.insert(key, observation.issue_id.clone()) {
                if existing != observation.issue_id {
                    return Err(IssueObservationError::ConflictingIssueId);
                }
            }
            observations.push(observation);
        }
        Ok(observations)
    }
}

/// 读取并校验单个不可变观察文件。
async fn read_observation(path: &Path) -> Result<IssueObservation, IssueObservationError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Issue 观察文件", path, source))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(IssueObservationError::UnsafeRecord(path.to_path_buf()));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| io_error("读取 Issue 观察文件", path, source))?;
    let observation: IssueObservation =
        serde_json::from_slice(&bytes).map_err(|source| IssueObservationError::InvalidJson {
            path: path.to_path_buf(),
            source,
        })?;
    observation.validate()?;
    Ok(observation)
}

/// 创建并验证观察日志根目录自身。
async fn ensure_safe_root(root: &Path) -> Result<(), IssueObservationError> {
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|source| io_error("创建 Issue 观察目录", root, source))?;
    let metadata = tokio::fs::symlink_metadata(root)
        .await
        .map_err(|source| io_error("检查 Issue 观察目录", root, source))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(IssueObservationError::UnsafeRoot(root.to_path_buf()));
    }
    Ok(())
}

/// 构造带操作和路径上下文的 I/O 错误。
fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> IssueObservationError {
    IssueObservationError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// Issue 观察日志访问错误。
#[derive(Debug, Error)]
pub enum IssueObservationError {
    /// Schema 版本不受支持。
    #[error("不支持的 Issue 观察 schema 版本：{0}")]
    UnsupportedSchemaVersion(u32),
    /// FailureRecord 绑定了其他 Episode。
    #[error("Issue 观察与 FailureRecord 的 Episode 不一致")]
    MixedEpisode,
    /// FailureRecord 归因不合法。
    #[error("Issue 观察中的 FailureRecord 不合法：{0}")]
    InvalidRecord(String),
    /// 声明指纹与记录重新计算结果不一致。
    #[error("Issue 观察的 FailureFingerprint 与记录不一致")]
    FingerprintMismatch,
    /// 相同指纹在日志中绑定了不同 Issue ID。
    #[error("同一 FailureFingerprint 绑定了冲突的 Issue ID")]
    ConflictingIssueId,
    /// 幂等键已存在但内容绑定冲突。
    #[error("Issue 观察幂等键冲突：{0}")]
    ConflictingObservation(String),
    /// JSON 序列化失败。
    #[error("序列化 Issue 观察失败：{0}")]
    Serialization(serde_json::Error),
    /// JSON 文件损坏。
    #[error("Issue 观察记录损坏：{path}: {source}")]
    InvalidJson {
        /// 损坏文件路径。
        path: PathBuf,
        /// JSON 解析错误。
        #[source]
        source: serde_json::Error,
    },
    /// 根路径不是安全的普通目录。
    #[error("Issue 观察根目录不安全：{0}")]
    UnsafeRoot(PathBuf),
    /// 记录不是安全普通文件，或文件名与幂等键不一致。
    #[error("Issue 观察文件不安全：{0}")]
    UnsafeRecord(PathBuf),
    /// 文件系统访问失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 失败操作。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        AttributionMethod, DiagnosticStatus, EventId, FailureAttribution, FailureKind,
        FailureRecordId,
    };
    use uuid::Uuid;

    /// 创建合法测试观察。
    fn observation() -> IssueObservation {
        let episode_id = EpisodeId::generate();
        let record = FailureRecord {
            record_id: FailureRecordId::generate(),
            episode_id: episode_id.clone(),
            attribution: FailureAttribution {
                detected_at: EventId::generate(),
                suspected_origin: None,
                propagation_path: Vec::new(),
                decisive_step: None,
                failure_class: FailureKind::ToolExecution,
                confidence: 0.9,
                evidence: Vec::new(),
                method: AttributionMethod::DeterministicRule,
            },
            status: DiagnosticStatus::Confirmed,
        };
        let digest = GenomeDigest::from_sha256_hex("a".repeat(64)).expect("摘要应合法");
        IssueObservation::new(EvolutionIssueId::generate(), episode_id, &digest, record)
    }

    /// 相同 Episode 与指纹的重试必须幂等，不得制造重复观察。
    #[tokio::test]
    async fn append_is_idempotent_for_same_episode_and_fingerprint() {
        let root = std::env::temp_dir().join(format!(
            "lucia-issue-observation-{}",
            Uuid::new_v4().simple()
        ));
        let store = FileIssueObservationStore::new(&root);
        let observation = observation();
        store.append(&observation).await.expect("首次追加应成功");
        store.append(&observation).await.expect("重复追加应幂等");

        assert_eq!(store.all().await.expect("应读取观察").len(), 1);
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
