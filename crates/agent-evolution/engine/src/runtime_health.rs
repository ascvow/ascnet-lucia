//! Promotion 后 Runtime 健康观察的安全存储与可信 Episode 适配。

use crate::{FileGenomeResolver, GenomeResolver, GenomeResolverError, GenomeSelector};
use agent_evolution_protocol::{
    ArtifactDigest, Episode, InvalidEvaluatorIpc, Outcome, ReleaseId, RuntimeHealthObservationV1,
    EVALUATION_REQUEST_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{fs, io::AsyncWriteExt};

/// Evolution 根目录下由可信 Runtime 写入健康观察的固定子目录。
pub const RUNTIME_HEALTH_DIRECTORY: &str = "runtime-health";
/// 单份 Runtime 健康观察允许的最大字节数。
pub const MAX_RUNTIME_HEALTH_OBSERVATION_BYTES: u64 = 64 * 1024;
/// 一次发布后运行需要通过的固定健康检查数量。
const RUNTIME_HEALTH_CHECK_COUNT: u32 = 2;
/// 当前进程内用于避免观察临时文件名冲突的单调序号。
static HEALTH_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 已校验 Runtime 健康观察及其规范 JSON 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRuntimeHealthObservation {
    observation: RuntimeHealthObservationV1,
    digest: ArtifactDigest,
}

impl VerifiedRuntimeHealthObservation {
    /// 返回已通过 schema、计数和 Release 绑定校验的观察。
    pub fn observation(&self) -> &RuntimeHealthObservationV1 {
        &self.observation
    }

    /// 返回观察规范 JSON 字节的 SHA-256。
    pub fn digest(&self) -> &ArtifactDigest {
        &self.digest
    }
}

/// 使用固定 Release 文件名读写 Runtime 健康观察的安全文件 Store。
///
/// 根路径必须由受信控制面提供绝对路径。观察文件名只由 Release ID 的 SHA-256 派生，IPC
/// 请求无法注入路径；根目录和目标文件都拒绝符号链接。
#[derive(Debug, Clone)]
pub struct FileRuntimeHealthObservationStore {
    root: PathBuf,
}

impl FileRuntimeHealthObservationStore {
    /// 创建文件 Store，但不触碰文件系统。
    ///
    /// # Errors
    ///
    /// `root` 不是绝对路径时返回 [`RuntimeHealthStoreError::RelativeRoot`]。
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, RuntimeHealthStoreError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(RuntimeHealthStoreError::RelativeRoot(root));
        }
        Ok(Self { root })
    }

    /// 返回受信配置提供的观察根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 以 create-new-or-same 语义写入一份脱敏 Runtime 观察。
    ///
    /// 该入口供受信 Runtime 记录聚合健康结果；相同 Release 已存在不同观察时拒绝覆盖。
    ///
    /// # Errors
    ///
    /// 观察无效、路径不安全、文件过大、同 Release 内容冲突或 I/O 失败时返回错误。
    pub async fn put(
        &self,
        observation: &RuntimeHealthObservationV1,
    ) -> Result<VerifiedRuntimeHealthObservation, RuntimeHealthStoreError> {
        observation
            .validate()
            .map_err(RuntimeHealthStoreError::InvalidObservation)?;
        ensure_safe_absolute_directory(&self.root).await?;
        let path = self.observation_path(&observation.release_id);
        let bytes =
            serde_json::to_vec_pretty(observation).map_err(RuntimeHealthStoreError::Serialize)?;
        enforce_size(bytes.len() as u64)?;
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            std::process::id(),
            HEALTH_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let commit = async {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|source| io_error("创建 Runtime 健康观察临时文件", &temporary, source))?;
            file.write_all(&bytes)
                .await
                .map_err(|source| io_error("写入 Runtime 健康观察临时文件", &temporary, source))?;
            file.sync_all()
                .await
                .map_err(|source| io_error("同步 Runtime 健康观察临时文件", &temporary, source))?;
            drop(file);
            fs::hard_link(&temporary, &path)
                .await
                .map_err(|source| io_error("提交 Runtime 健康观察", &path, source))
        }
        .await;
        let _ = fs::remove_file(&temporary).await;
        match commit {
            Ok(()) => {}
            Err(RuntimeHealthStoreError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let existing = self.load(&observation.release_id).await?;
                if existing.observation() != observation {
                    return Err(RuntimeHealthStoreError::ObservationConflict(
                        observation.release_id.clone(),
                    ));
                }
                return Ok(existing);
            }
            Err(error) => return Err(error),
        }
        self.load(&observation.release_id).await
    }

    /// 加载并验证指定 Promotion Release 的健康观察。
    ///
    /// # Errors
    ///
    /// 根路径或观察文件不安全、文件缺失或过大、JSON/schema/Release/计数不合法时返回错误。
    pub async fn load(
        &self,
        release_id: &ReleaseId,
    ) -> Result<VerifiedRuntimeHealthObservation, RuntimeHealthStoreError> {
        ensure_safe_absolute_directory(&self.root).await?;
        let path = self.observation_path(release_id);
        let metadata = fs::symlink_metadata(&path).await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                RuntimeHealthStoreError::ObservationNotFound(release_id.clone())
            } else {
                io_error("检查 Runtime 健康观察", &path, source)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(RuntimeHealthStoreError::UnsafePath(path));
        }
        enforce_size(metadata.len())?;
        let bytes = fs::read(&path)
            .await
            .map_err(|source| io_error("读取 Runtime 健康观察", &path, source))?;
        enforce_size(bytes.len() as u64)?;
        let observation: RuntimeHealthObservationV1 = serde_json::from_slice(&bytes)
            .map_err(|source| RuntimeHealthStoreError::InvalidJson { path, source })?;
        observation
            .validate()
            .map_err(RuntimeHealthStoreError::InvalidObservation)?;
        if observation.release_id != *release_id {
            return Err(RuntimeHealthStoreError::ReleaseMismatch {
                expected: release_id.clone(),
                actual: observation.release_id,
            });
        }
        let canonical =
            serde_json::to_vec(&observation).map_err(RuntimeHealthStoreError::Serialize)?;
        let digest = digest_bytes(&canonical)?;
        Ok(VerifiedRuntimeHealthObservation {
            observation,
            digest,
        })
    }

    /// 使用 Release ID 摘要构造固定普通文件路径，避免原始标识进入路径结构。
    fn observation_path(&self, release_id: &ReleaseId) -> PathBuf {
        let name = format!("{:x}.json", Sha256::digest(release_id.as_str().as_bytes()));
        self.root.join(name)
    }
}

/// 把 Promotion 后第一个真实 Episode 转换为不可覆盖健康观察的可信 Runtime 适配器。
///
/// 适配器只从受信 Stable 引用获取 Release 和 Revision，不接受调用方另行声明。当前 M5
/// 策略以第一个完整 Episode 作为固定健康窗口：Genome 绑定和 Episode 持久化始终计为一项
/// 通过；正常完成、带恢复完成或缺少任务 Verifier 的完整运行计为第二项通过，其他终态触发
/// 健康失败。后续 Episode 只复用首份观察，避免不同运行覆盖已经用于发布判定的证据。
#[derive(Debug, Clone)]
pub struct RuntimeHealthRecorder {
    release_id: ReleaseId,
    revision_id: agent_evolution_protocol::GenomeRevisionId,
    observations: FileRuntimeHealthObservationStore,
}

impl RuntimeHealthRecorder {
    /// 返回装配时固定的 Promotion Revision，供 Runtime 启动阶段核对行为绑定。
    pub fn revision_id(&self) -> &agent_evolution_protocol::GenomeRevisionId {
        &self.revision_id
    }

    /// 从可信 Evolution Registry 装配当前 Promotion 的健康记录器。
    ///
    /// 人工初始化 Stable 或已经完成 Rollback 的 Stable 返回 `Ok(None)`，因为它们没有待验证
    /// Promotion。目标 Revision 会通过 Resolver 与 Stable 摘要重新校验。
    ///
    /// # Errors
    ///
    /// Evolution 根不是绝对路径，Stable/Revision 缺失或损坏，或健康 Store 无法装配时返回
    /// [`RuntimeHealthRecorderError`]。
    pub async fn from_stable(
        evolution_root: impl Into<PathBuf>,
        lineage: &str,
    ) -> Result<Option<Self>, RuntimeHealthRecorderError> {
        let evolution_root = evolution_root.into();
        let observations =
            FileRuntimeHealthObservationStore::new(evolution_root.join(RUNTIME_HEALTH_DIRECTORY))?;
        let resolver = FileGenomeResolver::new(&evolution_root);
        let stable = resolver.stable_reference(lineage).await?;
        let revision = resolver
            .resolve(&GenomeSelector::Stable(lineage.to_string()))
            .await?;
        if revision.revision_id != stable.revision_id || revision.digest != stable.digest {
            return Err(RuntimeHealthRecorderError::StableRevisionMismatch);
        }
        if stable.rollback_of.is_some() {
            return Ok(None);
        }
        let Some(release_id) = stable.release_id else {
            return Ok(None);
        };
        Ok(Some(Self {
            release_id,
            revision_id: revision.revision_id,
            observations,
        }))
    }

    /// 记录本次 Promotion 后第一个完整 Episode，或幂等返回已经固定的首份观察。
    ///
    /// `Episode` 必须来自已收敛 Store，且绑定装配时解析的精确 Stable Revision。调用方不能
    /// 提供 Release ID、检查计数或观察时间。
    ///
    /// # Errors
    ///
    /// Episode 无效、没有终态、Revision 错绑，或观察 Store 不可用时返回
    /// [`RuntimeHealthRecorderError`]。
    pub async fn record_first_episode(
        &self,
        episode: &Episode,
    ) -> Result<VerifiedRuntimeHealthObservation, RuntimeHealthRecorderError> {
        episode
            .validate()
            .map_err(|error| RuntimeHealthRecorderError::InvalidEpisode(error.to_string()))?;
        if episode.genome_revision_id != self.revision_id {
            return Err(RuntimeHealthRecorderError::EpisodeRevisionMismatch {
                expected: self.revision_id.clone(),
                actual: episode.genome_revision_id.clone(),
            });
        }
        if let Some(existing) = self.load_existing().await? {
            return Ok(existing);
        }
        let outcome = episode
            .outcome
            .as_ref()
            .ok_or(RuntimeHealthRecorderError::MissingEpisodeOutcome)?;
        let checks_passed = if runtime_outcome_is_healthy(outcome) {
            RUNTIME_HEALTH_CHECK_COUNT
        } else {
            RUNTIME_HEALTH_CHECK_COUNT - 1
        };
        let observation = RuntimeHealthObservationV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            release_id: self.release_id.clone(),
            observed_revision_id: self.revision_id.clone(),
            checks_passed,
            checks_total: RUNTIME_HEALTH_CHECK_COUNT,
            observed_at_ms: episode.finished_at_ms,
        };
        match self.observations.put(&observation).await {
            Ok(verified) => Ok(verified),
            Err(RuntimeHealthStoreError::ObservationConflict(_)) => self
                .load_existing()
                .await?
                .ok_or(RuntimeHealthRecorderError::ObservationRace),
            Err(error) => Err(error.into()),
        }
    }

    /// 返回已存在且仍绑定当前 Stable Revision 的首份观察。
    async fn load_existing(
        &self,
    ) -> Result<Option<VerifiedRuntimeHealthObservation>, RuntimeHealthRecorderError> {
        match self.observations.load(&self.release_id).await {
            Ok(existing) => {
                if existing.observation().observed_revision_id != self.revision_id {
                    return Err(RuntimeHealthRecorderError::ExistingObservationRevisionMismatch);
                }
                Ok(Some(existing))
            }
            Err(RuntimeHealthStoreError::ObservationNotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

/// 判断一个已完整收敛的 Runtime 终态是否通过运行健康检查。
fn runtime_outcome_is_healthy(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Success | Outcome::SuccessWithRecovery | Outcome::Unverifiable
    )
}

/// Runtime 健康观察文件 Store 错误。
#[derive(Debug, thiserror::Error)]
pub enum RuntimeHealthStoreError {
    /// Store 根不是绝对路径。
    #[error("Runtime 健康观察根必须是绝对路径：{0}")]
    RelativeRoot(PathBuf),
    /// 根目录或观察文件是符号链接或不是预期类型。
    #[error("Runtime 健康观察路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// 指定 Release 尚无健康观察。
    #[error("Runtime 健康观察不存在：{0}")]
    ObservationNotFound(ReleaseId),
    /// 同一 Release 已存在不同观察。
    #[error("Runtime 健康观察已存在且内容不同：{0}")]
    ObservationConflict(ReleaseId),
    /// 文件正文绑定了另一 Release。
    #[error("Runtime 健康观察 Release 不匹配：期望 {expected}，实际 {actual}")]
    ReleaseMismatch {
        /// 请求的 Promotion Release。
        expected: ReleaseId,
        /// 文件正文中的 Release。
        actual: ReleaseId,
    },
    /// 观察文件超过固定读取上限。
    #[error("Runtime 健康观察过大：{actual} 字节，上限 {maximum} 字节")]
    ObservationTooLarge {
        /// 实际文件或序列化字节数。
        actual: u64,
        /// 固定读取上限。
        maximum: u64,
    },
    /// 共享观察协议无效。
    #[error("Runtime 健康观察无效：{0}")]
    InvalidObservation(InvalidEvaluatorIpc),
    /// 观察 JSON 损坏。
    #[error("Runtime 健康观察 JSON 损坏 `{path}`：{source}")]
    InvalidJson {
        /// 损坏文件路径。
        path: PathBuf,
        /// JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 观察 JSON 无法序列化。
    #[error("序列化 Runtime 健康观察失败：{0}")]
    Serialize(serde_json::Error),
    /// SHA-256 文本无法构造成协议摘要。
    #[error("构造 Runtime 健康观察摘要失败：{0}")]
    InvalidDigest(String),
    /// 文件系统操作失败。
    #[error("{operation}失败 `{path}`：{source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
}

/// 从 Stable 绑定和真实 Episode 生成健康观察时的错误。
#[derive(Debug, thiserror::Error)]
pub enum RuntimeHealthRecorderError {
    /// Stable Registry 或目标 Revision 无法可信读取。
    #[error(transparent)]
    Resolver(#[from] GenomeResolverError),
    /// 健康观察 Store 无法可信读写。
    #[error(transparent)]
    Store(#[from] RuntimeHealthStoreError),
    /// Stable 指针与重新解析的不可变 Revision 不一致。
    #[error("Stable 指针与实际 Genome Revision 不一致")]
    StableRevisionMismatch,
    /// 已收敛 Episode 违反共享协议。
    #[error("Runtime 健康 Episode 无效：{0}")]
    InvalidEpisode(String),
    /// Episode 尚未形成可信终态。
    #[error("Runtime 健康 Episode 缺少终态")]
    MissingEpisodeOutcome,
    /// Episode 没有使用装配时固定的 Stable Revision。
    #[error("Runtime 健康 Episode Revision 不匹配：期望 {expected}，实际 {actual}")]
    EpisodeRevisionMismatch {
        /// 装配时固定的 Stable Revision。
        expected: agent_evolution_protocol::GenomeRevisionId,
        /// Episode 实际绑定的 Revision。
        actual: agent_evolution_protocol::GenomeRevisionId,
    },
    /// 已存在观察与装配时固定的 Stable Revision 不一致。
    #[error("已有 Runtime 健康观察与当前 Stable Revision 不一致")]
    ExistingObservationRevisionMismatch,
    /// 并发写入发生冲突后无法恢复首份观察。
    #[error("Runtime 健康观察并发提交后无法恢复")]
    ObservationRace,
}

/// 创建并验证绝对、非符号链接普通目录。
async fn ensure_safe_absolute_directory(path: &Path) -> Result<(), RuntimeHealthStoreError> {
    if !path.is_absolute() {
        return Err(RuntimeHealthStoreError::RelativeRoot(path.to_path_buf()));
    }
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 Runtime 健康观察目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Runtime 健康观察目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RuntimeHealthStoreError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

/// 强制执行观察文件大小上限。
fn enforce_size(actual: u64) -> Result<(), RuntimeHealthStoreError> {
    if actual > MAX_RUNTIME_HEALTH_OBSERVATION_BYTES {
        return Err(RuntimeHealthStoreError::ObservationTooLarge {
            actual,
            maximum: MAX_RUNTIME_HEALTH_OBSERVATION_BYTES,
        });
    }
    Ok(())
}

/// 计算协议格式的 SHA-256 摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, RuntimeHealthStoreError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| RuntimeHealthStoreError::InvalidDigest(error.to_string()))
}

/// 构造带路径上下文的观察 Store I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> RuntimeHealthStoreError {
    RuntimeHealthStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        EpisodeDataPolicy, EpisodeId, GenomeRevisionId, ReplayabilityGrade, RunId, TaskDescriptor,
        UsageSummary, EPISODE_SCHEMA_VERSION,
    };
    use uuid::Uuid;

    /// 创建不会与并发测试冲突的绝对临时目录。
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-runtime-health-{}", Uuid::new_v4().simple()))
    }

    /// 构造绑定指定 Revision 和终态的最小完整 Episode。
    fn episode(revision_id: GenomeRevisionId, outcome: Outcome) -> Episode {
        Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: EpisodeId::generate(),
            run_id: RunId::generate(),
            session_id: "runtime-health-test".into(),
            genome_revision_id: revision_id,
            task: TaskDescriptor::default(),
            event_stream_ref: agent_evolution_protocol::ArtifactRef {
                digest: ArtifactDigest::from_sha256_hex("a".repeat(64)).expect("摘要应合法"),
                media_type: "application/json".into(),
                size_bytes: 2,
            },
            supervision: None,
            environment_ref: None,
            outcome: Some(outcome),
            failures: Vec::new(),
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
            data_policy: EpisodeDataPolicy::default(),
            event_count: 1,
            started_at_ms: 1,
            finished_at_ms: 2,
        }
    }

    /// Store 必须拒绝相对根，并对相同观察提供不可覆盖的幂等写入。
    #[tokio::test]
    async fn store_requires_absolute_root_and_create_new_or_same() {
        assert!(matches!(
            FileRuntimeHealthObservationStore::new("relative/health"),
            Err(RuntimeHealthStoreError::RelativeRoot(_))
        ));
        let root = temp_root();
        let store =
            FileRuntimeHealthObservationStore::new(root.join("health")).expect("绝对根应合法");
        let observation = RuntimeHealthObservationV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            release_id: ReleaseId::generate(),
            observed_revision_id: GenomeRevisionId::generate(),
            checks_passed: 2,
            checks_total: 2,
            observed_at_ms: 10,
        };
        let first = store.put(&observation).await.expect("首次写入应成功");
        let retry = store.put(&observation).await.expect("相同观察应幂等");
        assert_eq!(first, retry);
        let mut conflict = observation;
        conflict.observed_at_ms = 11;
        assert!(matches!(
            store.put(&conflict).await,
            Err(RuntimeHealthStoreError::ObservationConflict(_))
        ));
        let _ = fs::remove_dir_all(root).await;
    }

    /// Store 必须在读取正文前拒绝超大普通文件。
    #[tokio::test]
    async fn store_rejects_oversized_observation_before_parsing() {
        let root = temp_root();
        let store =
            FileRuntimeHealthObservationStore::new(root.join("health")).expect("绝对根应合法");
        fs::create_dir_all(store.root())
            .await
            .expect("创建健康目录");
        let release_id = ReleaseId::generate();
        let path = store.observation_path(&release_id);
        fs::write(
            &path,
            vec![b'x'; MAX_RUNTIME_HEALTH_OBSERVATION_BYTES as usize + 1],
        )
        .await
        .expect("写入越界测试文件");
        assert!(matches!(
            store.load(&release_id).await,
            Err(RuntimeHealthStoreError::ObservationTooLarge { .. })
        ));
        let _ = fs::remove_dir_all(root).await;
    }

    /// Unix 下根目录和观察文件符号链接都必须被拒绝。
    #[cfg(unix)]
    #[tokio::test]
    async fn store_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let actual = root.join("actual");
        fs::create_dir_all(&actual).await.expect("创建真实目录");
        let linked_root = root.join("linked");
        symlink(&actual, &linked_root).expect("创建目录符号链接");
        let linked_store =
            FileRuntimeHealthObservationStore::new(linked_root).expect("绝对路径结构合法");
        let release_id = ReleaseId::generate();
        assert!(matches!(
            linked_store.load(&release_id).await,
            Err(RuntimeHealthStoreError::UnsafePath(_))
        ));

        let store =
            FileRuntimeHealthObservationStore::new(root.join("safe")).expect("创建安全 Store");
        fs::create_dir_all(store.root())
            .await
            .expect("创建安全目录");
        let target = root.join("target.json");
        fs::write(&target, b"{}").await.expect("创建链接目标");
        symlink(&target, store.observation_path(&release_id)).expect("创建文件符号链接");
        assert!(matches!(
            store.load(&release_id).await,
            Err(RuntimeHealthStoreError::UnsafePath(_))
        ));
        let _ = fs::remove_dir_all(root).await;
    }

    /// 健康映射必须让完整正常运行通过，并让任务失败触发回滚分支。
    #[test]
    fn runtime_outcome_mapping_is_conservative() {
        assert!(runtime_outcome_is_healthy(&Outcome::Unverifiable));
        assert!(runtime_outcome_is_healthy(&Outcome::Success));
        assert!(!runtime_outcome_is_healthy(&Outcome::TaskFailure));
        assert!(!runtime_outcome_is_healthy(&Outcome::SafetyFailure));
        assert!(episode(GenomeRevisionId::generate(), Outcome::TaskFailure)
            .validate()
            .is_ok());
    }
}
