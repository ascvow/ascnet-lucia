//! Promotion 后 Runtime 健康观察的安全文件 Store 与受信复核器。

use crate::{AuditEvent, AuditStoreError, TrustedEvaluationArchive};
use agent_evolution::{FileGenomeResolver, GenomeResolver, GenomeResolverError, GenomeSelector};
use agent_evolution_protocol::{
    ArtifactDigest, HealthCheckReceiptV1, HealthCheckRequestV1, InvalidEvaluatorIpc, ReleaseId,
    RuntimeHealthObservationV1, HEALTH_RECEIPT_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{fs, io::AsyncWriteExt};

/// 单份 Runtime 健康观察允许的最大字节数。
pub const MAX_RUNTIME_HEALTH_OBSERVATION_BYTES: u64 = 64 * 1024;
/// 当前进程内用于避免观察临时文件名冲突的单调序号。
static HEALTH_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 已校验 Runtime 健康观察及其规范 JSON 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRuntimeHealthObservation {
    observation: RuntimeHealthObservationV1,
    digest: ArtifactDigest,
}

impl VerifiedRuntimeHealthObservation {
    /// 返回已通过 schema、Release 和计数校验的观察。
    pub fn observation(&self) -> &RuntimeHealthObservationV1 {
        &self.observation
    }

    /// 返回观察规范 JSON 字节的 SHA-256。
    pub fn digest(&self) -> &ArtifactDigest {
        &self.digest
    }
}

/// 使用固定 Release 文件名读取 Runtime 健康观察的安全文件 Store。
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

/// 复核 Promotion Audit、Stable 引用和真实 Runtime 观察的受信健康验证器。
#[derive(Debug, Clone)]
pub struct ReleaseHealthVerifier {
    resolver: FileGenomeResolver,
    archive: TrustedEvaluationArchive,
    observations: FileRuntimeHealthObservationStore,
}

impl ReleaseHealthVerifier {
    /// 使用 Evolution Registry、Evaluation Archive 与受信观察 Store 创建验证器。
    pub fn new(
        evolution_root: impl Into<PathBuf>,
        archive_root: impl Into<PathBuf>,
        observations: FileRuntimeHealthObservationStore,
    ) -> Self {
        Self {
            resolver: FileGenomeResolver::new(evolution_root),
            archive: TrustedEvaluationArchive::new(archive_root),
            observations,
        }
    }

    /// 验证一次 Promotion 后的 Stable 与 Runtime 健康状态并生成脱敏回执。
    ///
    /// Promotion Audit 必须唯一且与请求的 Release、lineage、Candidate 和代数完全一致；缺失
    /// 或冲突表示请求/归档不可信并直接报错。Stable 已变化、Runtime 使用错误 Revision 或健康
    /// 检查未全部通过属于可回滚的健康失败，返回 `verified = false`。
    ///
    /// # Errors
    ///
    /// 请求、Audit、Stable Registry 或观察 Store 无法可信验证时返回
    /// [`ReleaseHealthVerificationError`]。
    pub async fn verify(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, ReleaseHealthVerificationError> {
        request
            .validate()
            .map_err(ReleaseHealthVerificationError::InvalidRequest)?;
        let records = self.archive.audit_log().records().await?;
        let promotions = records
            .iter()
            .filter_map(|record| match &record.event {
                AuditEvent::PromotionCommitted {
                    release_id,
                    report_id,
                    lineage,
                    candidate,
                    generation,
                    ..
                } if release_id == &request.release_id => {
                    Some((report_id, lineage.as_str(), candidate, *generation))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if promotions.is_empty() {
            return Err(ReleaseHealthVerificationError::PromotionAuditNotFound(
                request.release_id.clone(),
            ));
        }
        if promotions.len() != 1 {
            return Err(ReleaseHealthVerificationError::PromotionAuditConflict(
                request.release_id.clone(),
            ));
        }
        let (report_id, audit_lineage, audit_candidate, audit_generation) = promotions[0];
        if audit_lineage != request.lineage
            || audit_candidate != &request.expected_revision_id
            || audit_generation != request.expected_generation
        {
            return Err(ReleaseHealthVerificationError::PromotionAuditMismatch(
                request.release_id.clone(),
            ));
        }

        let stable = self.resolver.stable_reference(&request.lineage).await?;
        let stable_revision = self
            .resolver
            .resolve(&GenomeSelector::Stable(request.lineage.clone()))
            .await?;
        let observation = self.observations.load(&request.release_id).await?;
        let observed = observation.observation();
        let stable_reference_verified = stable.lineage == request.lineage
            && stable.revision_id == request.expected_revision_id
            && stable_revision.revision_id == request.expected_revision_id
            && stable.generation == request.expected_generation
            && stable.release_id.as_ref() == Some(&request.release_id)
            && stable.evaluation_report_id.as_ref() == Some(report_id)
            && stable.rollback_of.is_none();
        let verified = stable_reference_verified
            && observed.observed_revision_id == request.expected_revision_id
            && observed.checks_passed == observed.checks_total;
        let receipt = HealthCheckReceiptV1 {
            schema_version: HEALTH_RECEIPT_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            release_id: request.release_id.clone(),
            lineage: request.lineage.clone(),
            expected_revision_id: request.expected_revision_id.clone(),
            observed_revision_id: observed.observed_revision_id.clone(),
            expected_generation: request.expected_generation,
            observed_generation: stable.generation,
            checks_passed: observed.checks_passed,
            checks_total: observed.checks_total,
            observation_digest: observation.digest().clone(),
            stable_reference_verified,
            verified,
        };
        receipt
            .validate()
            .map_err(ReleaseHealthVerificationError::InvalidReceipt)?;
        Ok(receipt)
    }
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

/// Promotion 后健康验证错误。
#[derive(Debug, thiserror::Error)]
pub enum ReleaseHealthVerificationError {
    /// 共享 Health 请求无效。
    #[error("Health 请求无效：{0}")]
    InvalidRequest(InvalidEvaluatorIpc),
    /// 指定 Release 没有 Promotion Audit。
    #[error("Promotion Audit 不存在：{0}")]
    PromotionAuditNotFound(ReleaseId),
    /// 同一 Release 出现多个 Promotion Audit。
    #[error("Promotion Audit 冲突：{0}")]
    PromotionAuditConflict(ReleaseId),
    /// Promotion Audit 与请求声明不一致。
    #[error("Promotion Audit 与 Health 请求不一致：{0}")]
    PromotionAuditMismatch(ReleaseId),
    /// 构造的共享 Health 回执不一致。
    #[error("Health 回执无效：{0}")]
    InvalidReceipt(InvalidEvaluatorIpc),
    /// Audit 链无法可信验证。
    #[error(transparent)]
    Audit(#[from] AuditStoreError),
    /// Stable Registry 无法可信读取。
    #[error(transparent)]
    Resolver(#[from] GenomeResolverError),
    /// Runtime 观察无法可信读取。
    #[error(transparent)]
    Observation(#[from] RuntimeHealthStoreError),
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
    use crate::AuditEvent;
    use agent_evolution::{FileStableGenomePublisher, GenomeStore};
    use agent_evolution_protocol::{
        AgentGenome, EvaluationReportId, GenomeMetadata, GenomeRevision, ModelGenome, PromptGenome,
        ReleaseId, RuntimeIdentity, ToolProfileGenome, EVALUATION_REQUEST_SCHEMA_VERSION,
        GENOME_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::collections::{BTreeMap, BTreeSet};
    use tempfile::TempDir;

    /// 构造健康验证所需的最小合法 Genome Revision。
    fn revision(marker: &str) -> GenomeRevision {
        GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".to_string(),
                    git_commit: marker.to_string(),
                    git_dirty: false,
                    target_triple: "test-target".to_string(),
                    features: BTreeSet::new(),
                },
                model: ModelGenome {
                    provider: "fixture".to_string(),
                    provider_kind: "fixture".to_string(),
                    model: "fixture-model".to_string(),
                    base_url: None,
                    protocol: None,
                    max_tokens: Some(64),
                    temperature: None,
                    stream: false,
                    provider_options_digest: None,
                },
                prompt: PromptGenome {
                    messages: Vec::new(),
                },
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("测试 Genome 应合法")
    }

    /// 创建已绑定 Promotion Audit 和 Stable Candidate 的健康测试环境。
    async fn promoted_fixture(
        root: &Path,
    ) -> (
        PathBuf,
        PathBuf,
        FileRuntimeHealthObservationStore,
        HealthCheckRequestV1,
    ) {
        let evolution_root = root.join("evolution");
        let archive_root = root.join("archive");
        let health_root = root.join("health");
        let publisher = FileStableGenomePublisher::new(&evolution_root);
        let parent = revision("parent");
        let candidate = revision("candidate");
        publisher
            .resolver()
            .store()
            .append(&parent)
            .await
            .expect("登记 Parent");
        publisher
            .resolver()
            .store()
            .append(&candidate)
            .await
            .expect("登记 Candidate");
        let initial = publisher
            .publish("stable/test", &parent, 1)
            .await
            .expect("初始化 Stable");
        let release_id = ReleaseId::generate();
        let report_id = EvaluationReportId::generate();
        publisher
            .publish_bound(
                &initial,
                &candidate,
                2,
                release_id.clone(),
                report_id.clone(),
                None,
            )
            .await
            .expect("发布 Candidate");
        TrustedEvaluationArchive::new(&archive_root)
            .audit_log()
            .append(
                2,
                AuditEvent::PromotionCommitted {
                    release_id: release_id.clone(),
                    report_id,
                    lineage: "stable/test".to_string(),
                    parent: parent.revision_id,
                    candidate: candidate.revision_id.clone(),
                    generation: 2,
                },
            )
            .await
            .expect("记录 Promotion Audit");
        let observations =
            FileRuntimeHealthObservationStore::new(health_root).expect("健康根是绝对路径");
        let request = HealthCheckRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "health-request-001".to_string(),
            release_id,
            lineage: "stable/test".to_string(),
            expected_revision_id: candidate.revision_id,
            expected_generation: 2,
        };
        (evolution_root, archive_root, observations, request)
    }

    /// Store 必须拒绝相对根，并对相同观察提供不可覆盖的幂等写入。
    #[tokio::test]
    async fn store_requires_absolute_root_and_create_new_or_same() {
        assert!(matches!(
            FileRuntimeHealthObservationStore::new("relative/health"),
            Err(RuntimeHealthStoreError::RelativeRoot(_))
        ));
        let root = TempDir::new().expect("创建临时根");
        let store = FileRuntimeHealthObservationStore::new(root.path().join("health"))
            .expect("绝对根应合法");
        let observation = RuntimeHealthObservationV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            release_id: ReleaseId::generate(),
            observed_revision_id: agent_evolution_protocol::GenomeRevisionId::generate(),
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
    }

    /// Store 必须在读取正文前拒绝超大普通文件。
    #[tokio::test]
    async fn store_rejects_oversized_observation_before_parsing() {
        let root = TempDir::new().expect("创建临时根");
        let store = FileRuntimeHealthObservationStore::new(root.path().join("health"))
            .expect("绝对根应合法");
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
    }

    /// Unix 下根目录和观察文件符号链接都必须被拒绝。
    #[cfg(unix)]
    #[tokio::test]
    async fn store_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("创建临时根");
        let actual = root.path().join("actual");
        fs::create_dir_all(&actual).await.expect("创建真实目录");
        let linked_root = root.path().join("linked");
        symlink(&actual, &linked_root).expect("创建目录符号链接");
        let linked_store =
            FileRuntimeHealthObservationStore::new(linked_root).expect("绝对路径结构合法");
        let release_id = ReleaseId::generate();
        assert!(matches!(
            linked_store.load(&release_id).await,
            Err(RuntimeHealthStoreError::UnsafePath(_))
        ));

        let store = FileRuntimeHealthObservationStore::new(root.path().join("safe"))
            .expect("创建安全 Store");
        fs::create_dir_all(store.root())
            .await
            .expect("创建安全目录");
        let target = root.path().join("target.json");
        fs::write(&target, b"{}").await.expect("创建链接目标");
        symlink(&target, store.observation_path(&release_id)).expect("创建文件符号链接");
        assert!(matches!(
            store.load(&release_id).await,
            Err(RuntimeHealthStoreError::UnsafePath(_))
        ));
    }

    /// Promotion Audit、Stable、Runtime Revision 和检查计数全部匹配时才允许健康通过。
    #[tokio::test]
    async fn verifier_requires_promotion_stable_and_runtime_observation_binding() {
        let root = TempDir::new().expect("创建临时根");
        let (evolution_root, archive_root, observations, request) =
            promoted_fixture(root.path()).await;
        observations
            .put(&RuntimeHealthObservationV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: request.release_id.clone(),
                observed_revision_id: request.expected_revision_id.clone(),
                checks_passed: 3,
                checks_total: 3,
                observed_at_ms: 10,
            })
            .await
            .expect("写入真实观察");
        let receipt = ReleaseHealthVerifier::new(evolution_root, archive_root, observations)
            .verify(&request)
            .await
            .expect("健康验证应完成");

        assert!(receipt.stable_reference_verified);
        assert!(receipt.verified);
        assert_eq!(receipt.checks_passed, 3);
        assert_eq!(receipt.checks_total, 3);
        receipt.validate().expect("回执必须满足共享协议");
    }

    /// Runtime 使用错误 Revision 时必须返回可触发回滚的失败回执。
    #[tokio::test]
    async fn verifier_returns_failed_receipt_for_wrong_runtime_revision() {
        let root = TempDir::new().expect("创建临时根");
        let (evolution_root, archive_root, observations, request) =
            promoted_fixture(root.path()).await;
        let wrong_revision = agent_evolution_protocol::GenomeRevisionId::generate();
        observations
            .put(&RuntimeHealthObservationV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: request.release_id.clone(),
                observed_revision_id: wrong_revision.clone(),
                checks_passed: 2,
                checks_total: 2,
                observed_at_ms: 10,
            })
            .await
            .expect("写入错误 Revision 观察");
        let receipt = ReleaseHealthVerifier::new(evolution_root, archive_root, observations)
            .verify(&request)
            .await
            .expect("行为失败仍应产生回执");

        assert!(receipt.stable_reference_verified);
        assert!(!receipt.verified);
        assert_eq!(receipt.observed_revision_id, wrong_revision);
        receipt.validate().expect("失败回执必须满足共享协议");
    }

    /// 请求与 Promotion Audit 的 Candidate 不一致时必须失败关闭，不能返回健康结论。
    #[tokio::test]
    async fn verifier_rejects_request_not_bound_to_promotion_audit() {
        let root = TempDir::new().expect("创建临时根");
        let (evolution_root, archive_root, observations, mut request) =
            promoted_fixture(root.path()).await;
        request.expected_generation = 3;
        let error = ReleaseHealthVerifier::new(evolution_root, archive_root, observations)
            .verify(&request)
            .await
            .expect_err("错误代数不得产生回执");
        assert!(matches!(
            error,
            ReleaseHealthVerificationError::PromotionAuditMismatch(_)
        ));
    }
}
