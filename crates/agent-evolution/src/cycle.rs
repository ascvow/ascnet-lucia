//! Evolution Cycle 的只追加状态机与本地归档。
//!
//! Cycle 快照只保存脱敏协议对象和受信进程回执，不保存 Prompt 正文、Hidden Dataset、
//! Verifier 或 Commit Policy。每次迁移都绑定前序快照摘要，终态后禁止继续追加。

use agent_evolution_protocol::{
    ArtifactDigest, EvolutionCycleId, EvolutionCycleSnapshotV1, EvolutionCycleStage,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// Evolution Cycle 快照的只追加存储契约。
#[async_trait]
pub trait EvolutionCycleStore: Send + Sync {
    /// 追加并验证一份不可变 Cycle 快照。
    ///
    /// # Errors
    ///
    /// 快照结构无效、序号或摘要链不连续、阶段迁移非法、记录已存在或 I/O 失败时返回错误。
    async fn append(&self, snapshot: &EvolutionCycleSnapshotV1) -> Result<(), CycleStoreError>;

    /// 读取并验证指定 Cycle 的完整快照历史。
    ///
    /// # Errors
    ///
    /// 任一记录损坏、身份不匹配、摘要链断裂、阶段迁移非法或 I/O 失败时返回错误。
    async fn history(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Vec<EvolutionCycleSnapshotV1>, CycleStoreError>;

    /// 返回指定 Cycle 的最新快照；Cycle 不存在时返回 `None`。
    ///
    /// # Errors
    ///
    /// 历史记录无法通过完整性校验时返回错误。
    async fn latest(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Option<EvolutionCycleSnapshotV1>, CycleStoreError> {
        Ok(self.history(cycle_id).await?.pop())
    }
}

/// 基于不可变 JSON 文件的 Evolution Cycle Store。
#[derive(Debug, Clone)]
pub struct FileEvolutionCycleStore {
    root: PathBuf,
}

impl FileEvolutionCycleStore {
    /// 创建延迟初始化的 Cycle Store。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回 Cycle Store 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 计算一份快照的稳定 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 快照无法序列化时返回 [`CycleStoreError::Serialization`]。
    pub fn snapshot_digest(
        snapshot: &EvolutionCycleSnapshotV1,
    ) -> Result<ArtifactDigest, CycleStoreError> {
        let bytes = serde_json::to_vec(snapshot).map_err(CycleStoreError::Serialization)?;
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
            .map_err(|error| CycleStoreError::InvalidSnapshot(error.to_string()))
    }

    /// 返回单个 Cycle 的固定目录。
    fn cycle_root(&self, cycle_id: &EvolutionCycleId) -> PathBuf {
        self.root.join(cycle_id.as_str())
    }

    /// 返回指定序号快照的固定路径。
    fn snapshot_path(&self, cycle_id: &EvolutionCycleId, sequence: u64) -> PathBuf {
        self.cycle_root(cycle_id)
            .join(format!("{sequence:020}.json"))
    }
}

#[async_trait]
impl EvolutionCycleStore for FileEvolutionCycleStore {
    async fn append(&self, snapshot: &EvolutionCycleSnapshotV1) -> Result<(), CycleStoreError> {
        snapshot
            .validate()
            .map_err(|error| CycleStoreError::InvalidSnapshot(error.to_string()))?;
        ensure_safe_directory(&self.root).await?;
        let cycle_root = self.cycle_root(&snapshot.cycle_id);
        ensure_safe_directory(&cycle_root).await?;
        let history = self.history(&snapshot.cycle_id).await?;
        validate_next_snapshot(history.last(), snapshot)?;

        let path = self.snapshot_path(&snapshot.cycle_id, snapshot.sequence);
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(CycleStoreError::Serialization)?;
        let temporary = cycle_root.join(format!(".{}.tmp", Uuid::new_v4().simple()));
        let result = async {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|source| io_error("创建 Cycle 临时文件", &temporary, source))?;
            file.write_all(&bytes)
                .await
                .map_err(|source| io_error("写入 Cycle 临时文件", &temporary, source))?;
            file.sync_all()
                .await
                .map_err(|source| io_error("同步 Cycle 临时文件", &temporary, source))?;
            drop(file);
            fs::hard_link(&temporary, &path).await.map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    CycleStoreError::AlreadyExists {
                        cycle_id: snapshot.cycle_id.clone(),
                        sequence: snapshot.sequence,
                    }
                } else {
                    io_error("提交 Cycle 快照", &path, source)
                }
            })
        }
        .await;
        let _ = fs::remove_file(&temporary).await;
        result?;

        let observed = read_snapshot(&path).await?;
        if observed != *snapshot {
            return Err(CycleStoreError::CommitVerificationFailed(path));
        }
        Ok(())
    }

    async fn history(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Vec<EvolutionCycleSnapshotV1>, CycleStoreError> {
        let cycle_root = self.cycle_root(cycle_id);
        let metadata = match fs::symlink_metadata(&cycle_root).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查 Cycle 目录", &cycle_root, source)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CycleStoreError::UnsafePath(cycle_root));
        }

        let mut directory = fs::read_dir(&cycle_root)
            .await
            .map_err(|source| io_error("遍历 Cycle 目录", &cycle_root, source))?;
        let mut paths = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|source| io_error("读取 Cycle 目录项", &cycle_root, source))?
        {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                return Err(CycleStoreError::UnsafePath(path));
            };
            if name.starts_with('.') {
                continue;
            }
            if name.len() != 25
                || !name.ends_with(".json")
                || !name[..20].bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(CycleStoreError::UnsafePath(path));
            }
            paths.push(path);
        }
        paths.sort();

        let mut history = Vec::with_capacity(paths.len());
        for path in paths {
            let snapshot = read_snapshot(&path).await?;
            if snapshot.cycle_id != *cycle_id
                || self.snapshot_path(cycle_id, snapshot.sequence) != path
            {
                return Err(CycleStoreError::IdentityMismatch(path));
            }
            validate_next_snapshot(history.last(), &snapshot)?;
            history.push(snapshot);
        }
        Ok(history)
    }
}

/// 判断 Cycle 阶段是否已关闭。
pub fn is_terminal_cycle_stage(stage: EvolutionCycleStage) -> bool {
    matches!(
        stage,
        EvolutionCycleStage::Completed
            | EvolutionCycleStage::HealthVerified
            | EvolutionCycleStage::RolledBack
            | EvolutionCycleStage::Rejected
            | EvolutionCycleStage::Failed
    )
}

/// 校验相邻 Cycle 快照的序号、摘要、身份和阶段迁移。
fn validate_next_snapshot(
    previous: Option<&EvolutionCycleSnapshotV1>,
    next: &EvolutionCycleSnapshotV1,
) -> Result<(), CycleStoreError> {
    match previous {
        None => {
            if next.sequence != 0
                || next.previous_digest.is_some()
                || next.stage != EvolutionCycleStage::Requested
            {
                return Err(CycleStoreError::InvalidInitialSnapshot);
            }
        }
        Some(previous) => {
            if previous.cycle_id != next.cycle_id
                || previous.issue_id != next.issue_id
                || previous.parent_revision_id != next.parent_revision_id
                || previous.request != next.request
            {
                return Err(CycleStoreError::IdentityMismatch(PathBuf::new()));
            }
            let expected_sequence = previous
                .sequence
                .checked_add(1)
                .ok_or(CycleStoreError::SequenceOverflow)?;
            if next.sequence != expected_sequence {
                return Err(CycleStoreError::SequenceMismatch {
                    expected: expected_sequence,
                    actual: next.sequence,
                });
            }
            let expected_digest = FileEvolutionCycleStore::snapshot_digest(previous)?;
            if next.previous_digest.as_ref() != Some(&expected_digest) {
                return Err(CycleStoreError::PreviousDigestMismatch);
            }
            if !allowed_transition(previous.stage, next.stage) {
                return Err(CycleStoreError::InvalidTransition {
                    from: previous.stage,
                    to: next.stage,
                });
            }
            if !is_prefix(&previous.proposals, &next.proposals)
                || !is_prefix(&previous.candidates, &next.candidates)
                || !is_prefix(&previous.evaluation_receipts, &next.evaluation_receipts)
                || (previous.winner.is_some() && previous.winner != next.winner)
                || (previous.release_receipt.is_some()
                    && previous.release_receipt != next.release_receipt)
                || (previous.health_receipt.is_some()
                    && previous.health_receipt != next.health_receipt)
                || (previous.rollback_receipt.is_some()
                    && previous.rollback_receipt != next.rollback_receipt)
            {
                return Err(CycleStoreError::HistoryRewrite);
            }
        }
    }
    Ok(())
}

/// 判断前一快照中的只追加列表是否完整保留在后一快照前缀中。
fn is_prefix<T: PartialEq>(previous: &[T], next: &[T]) -> bool {
    next.starts_with(previous)
}

/// 固定 M5 Cycle 的合法阶段迁移；任何工作阶段都可以失败关闭。
fn allowed_transition(from: EvolutionCycleStage, to: EvolutionCycleStage) -> bool {
    if is_terminal_cycle_stage(from) {
        return false;
    }
    if to == EvolutionCycleStage::Failed {
        return true;
    }
    matches!(
        (from, to),
        (
            EvolutionCycleStage::Requested,
            EvolutionCycleStage::SelectingEvidence
        ) | (
            EvolutionCycleStage::SelectingEvidence,
            EvolutionCycleStage::Diagnosing
        ) | (
            EvolutionCycleStage::Diagnosing,
            EvolutionCycleStage::Mutating
        ) | (
            EvolutionCycleStage::Mutating,
            EvolutionCycleStage::BuildingCandidates
        ) | (
            EvolutionCycleStage::BuildingCandidates,
            EvolutionCycleStage::BuildingCandidates
        ) | (
            EvolutionCycleStage::BuildingCandidates,
            EvolutionCycleStage::Evaluating
        ) | (
            EvolutionCycleStage::Evaluating,
            EvolutionCycleStage::Evaluating
        ) | (
            EvolutionCycleStage::Evaluating,
            EvolutionCycleStage::SelectingWinner
        ) | (
            EvolutionCycleStage::SelectingWinner,
            EvolutionCycleStage::Promoting
        ) | (
            EvolutionCycleStage::SelectingWinner,
            EvolutionCycleStage::Rejected
        ) | (
            EvolutionCycleStage::Promoting,
            EvolutionCycleStage::AwaitingHealth
        ) | (
            EvolutionCycleStage::AwaitingHealth,
            EvolutionCycleStage::VerifyingHealth
        ) | (
            EvolutionCycleStage::VerifyingHealth,
            EvolutionCycleStage::HealthVerified
        ) | (
            EvolutionCycleStage::VerifyingHealth,
            EvolutionCycleStage::RollingBack
        ) | (
            EvolutionCycleStage::RollingBack,
            EvolutionCycleStage::RolledBack
        )
    )
}

/// Cycle Store 的完整性、状态机和 I/O 错误。
#[derive(Debug, thiserror::Error)]
pub enum CycleStoreError {
    /// 快照自身不满足协议不变量。
    #[error("Evolution Cycle 快照无效：{0}")]
    InvalidSnapshot(String),
    /// 首个快照必须是序号零的 Requested，且不携带前序摘要。
    #[error("Evolution Cycle 首个快照必须是 sequence=0 的 Requested")]
    InvalidInitialSnapshot,
    /// 相邻快照的序号不连续。
    #[error("Evolution Cycle 序号不连续：期望 {expected}，实际 {actual}")]
    SequenceMismatch {
        /// 期望序号。
        expected: u64,
        /// 实际序号。
        actual: u64,
    },
    /// 快照序号无法继续递增。
    #[error("Evolution Cycle 序号溢出")]
    SequenceOverflow,
    /// 前序摘要与真实前一快照不一致。
    #[error("Evolution Cycle 前序摘要不匹配")]
    PreviousDigestMismatch,
    /// Cycle、Issue、Parent 或文件名身份不一致。
    #[error("Evolution Cycle 快照身份不匹配：{0}")]
    IdentityMismatch(PathBuf),
    /// 阶段迁移不属于固定 M5 状态机。
    #[error("Evolution Cycle 阶段迁移非法：{from:?} -> {to:?}")]
    InvalidTransition {
        /// 前一阶段。
        from: EvolutionCycleStage,
        /// 后一阶段。
        to: EvolutionCycleStage,
    },
    /// 后一快照删除或改写了已归档制品。
    #[error("Evolution Cycle 后续快照不得删除或改写历史制品")]
    HistoryRewrite,
    /// 相同 Cycle 与序号的快照已经存在。
    #[error("Evolution Cycle 快照已存在：{cycle_id} sequence={sequence}")]
    AlreadyExists {
        /// Cycle 标识。
        cycle_id: EvolutionCycleId,
        /// 快照序号。
        sequence: u64,
    },
    /// 快照 JSON 编码失败。
    #[error("序列化 Evolution Cycle 快照失败：{0}")]
    Serialization(serde_json::Error),
    /// 快照 JSON 损坏。
    #[error("Evolution Cycle 快照损坏：{path}: {source}")]
    InvalidRecord {
        /// 损坏文件路径。
        path: PathBuf,
        /// JSON 错误。
        source: serde_json::Error,
    },
    /// Store 或记录路径包含符号链接或意外类型。
    #[error("Evolution Cycle 路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// 提交后重新读取的内容与请求不一致。
    #[error("Evolution Cycle 提交后验证失败：{0}")]
    CommitVerificationFailed(PathBuf),
    /// 文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始错误。
        source: std::io::Error,
    },
}

/// 创建并验证 Store 内部目录，拒绝符号链接替换。
async fn ensure_safe_directory(path: &Path) -> Result<(), CycleStoreError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 Cycle 目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Cycle 目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CycleStoreError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

/// 从普通文件读取并校验一份 Cycle 快照。
async fn read_snapshot(path: &Path) -> Result<EvolutionCycleSnapshotV1, CycleStoreError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Cycle 快照", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CycleStoreError::UnsafePath(path.to_path_buf()));
    }
    let bytes = fs::read(path)
        .await
        .map_err(|source| io_error("读取 Cycle 快照", path, source))?;
    let snapshot: EvolutionCycleSnapshotV1 =
        serde_json::from_slice(&bytes).map_err(|source| CycleStoreError::InvalidRecord {
            path: path.to_path_buf(),
            source,
        })?;
    snapshot
        .validate()
        .map_err(|error| CycleStoreError::InvalidSnapshot(error.to_string()))?;
    Ok(snapshot)
}

/// 构造带路径上下文的 Cycle I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> CycleStoreError {
    CycleStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        EpisodeId, EvolutionCycleRequestInput, EvolutionCycleRequestV1, EvolutionCycleSnapshotV1,
        GenomeDigest, EVOLUTION_CYCLE_SCHEMA_VERSION,
    };

    /// 创建互不冲突的测试目录。
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-cycle-{}", Uuid::new_v4().simple()))
    }

    /// 构造最小合法快照。
    fn snapshot(
        request: &EvolutionCycleRequestV1,
        stage: EvolutionCycleStage,
        sequence: u64,
        previous_digest: Option<ArtifactDigest>,
    ) -> EvolutionCycleSnapshotV1 {
        EvolutionCycleSnapshotV1 {
            schema_version: EVOLUTION_CYCLE_SCHEMA_VERSION,
            request: request.clone(),
            cycle_id: request.cycle_id.clone(),
            issue_id: request.issue_id.clone(),
            parent_revision_id: request.parent_revision_id.clone(),
            stage,
            sequence,
            previous_digest,
            proposals: Vec::new(),
            candidates: Vec::new(),
            evaluation_receipts: Vec::new(),
            winner: None,
            release_receipt: None,
            health_receipt: None,
            rollback_receipt: None,
            failure_code: None,
            created_at_ms: sequence,
        }
    }

    /// 构造绑定固定策略和脱敏 Episode 的合法 Cycle 请求。
    fn request() -> EvolutionCycleRequestV1 {
        EvolutionCycleRequestV1::create(EvolutionCycleRequestInput {
            issue_id: agent_evolution_protocol::EvolutionIssueId::generate(),
            parent_revision_id: agent_evolution_protocol::GenomeRevisionId::generate(),
            parent_genome_digest: GenomeDigest::from_sha256_hex("a".repeat(64))
                .expect("摘要应合法"),
            lineage: "stable/general".to_string(),
            expected_parent_generation: 1,
            source_episode_ids: vec![EpisodeId::generate()],
            evolution_policy_version: "evolution-policy-v1".to_string(),
            candidate_count: 3,
            requested_at_ms: 1,
        })
        .expect("Cycle 请求应合法")
    }

    /// Store 必须保留并验证完整状态迁移与摘要链。
    #[tokio::test]
    async fn appends_and_verifies_cycle_hash_chain() {
        let root = temp_root();
        let store = FileEvolutionCycleStore::new(&root);
        let request = request();
        let cycle_id = request.cycle_id.clone();
        let first = snapshot(&request, EvolutionCycleStage::Requested, 0, None);
        store.append(&first).await.expect("应追加首个快照");
        let second = snapshot(
            &request,
            EvolutionCycleStage::SelectingEvidence,
            1,
            Some(FileEvolutionCycleStore::snapshot_digest(&first).expect("应计算摘要")),
        );
        store.append(&second).await.expect("应追加下一快照");
        assert_eq!(store.history(&cycle_id).await.expect("应读取历史").len(), 2);
        assert_eq!(
            store.latest(&cycle_id).await.expect("应读取最新"),
            Some(second)
        );
        let _ = fs::remove_dir_all(root).await;
    }

    /// Store 必须拒绝跳步、错误摘要和终态后的追加。
    #[tokio::test]
    async fn rejects_invalid_transition_and_history_rewrite() {
        let root = temp_root();
        let store = FileEvolutionCycleStore::new(&root);
        let request = request();
        let first = snapshot(&request, EvolutionCycleStage::Requested, 0, None);
        store.append(&first).await.expect("应追加首个快照");
        let invalid = snapshot(
            &request,
            EvolutionCycleStage::Evaluating,
            1,
            Some(FileEvolutionCycleStore::snapshot_digest(&first).expect("应计算摘要")),
        );
        assert!(matches!(
            store.append(&invalid).await,
            Err(CycleStoreError::InvalidTransition { .. })
        ));
        let _ = fs::remove_dir_all(root).await;
    }
}
