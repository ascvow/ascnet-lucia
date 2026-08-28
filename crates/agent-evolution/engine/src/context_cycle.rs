//! M6 Context Policy 的有界变异、只追加归档和发布闭环。
//!
//! Evolver 只构造候选并消费独立 Evaluator 回执。八指标 Gate、正式 Archive、Promotion、
//! Runtime 健康判断和 Rollback 的权威实现均留在 `lucia-eval`。

use crate::{
    ArtifactStore, ArtifactStoreError, ContextCandidateBuildError, ContextCandidateBuilder,
    ContextEvaluatorClient, EpisodeStore, EpisodeStoreError, EvaluatorProcessError,
    EvolutionOutbox, FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox, FileGenomeResolver,
    GenomeResolver, GenomeResolverError, GenomeSelector, OutboxError,
};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, ContextEvaluationReceiptV1, ContextEvaluationRequestV1,
    ContextPolicyCandidateV1, ContextPolicyMutationProposalV1, ContextPolicyV1, DatasetVersionId,
    EpisodeId, EvolutionCycleId, EvolutionLifecycle, GateDecision, GenomeDigest, GenomeRevision,
    GenomeRevisionId, HealthCheckReceiptV1, HealthCheckRequestV1, MutationId, PromotionRequestV1,
    ReleaseId, ReleaseReceiptV1, RollbackRequestV1, CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION,
    EVALUATION_REQUEST_SCHEMA_VERSION, MAX_CONTEXT_THRESHOLD_TOKENS, MIN_CONTEXT_THRESHOLD_TOKENS,
    MIN_SUMMARY_TOKEN_BUDGET,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// Context Cycle 请求和快照的当前 schema 版本。
pub const CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION: u32 = 1;
/// M6 每轮固定构造的 Context Policy Candidate 数量。
pub const CONTEXT_EVOLUTION_CANDIDATE_COUNT: usize = 3;
/// 单份 Context Cycle 快照允许的最大字节数。
pub const MAX_CONTEXT_CYCLE_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// 启动一次 Context Policy Evolution Cycle 的最小请求。
///
/// 请求不包含 Context Gate 阈值、原始观察、Fixture 路径、Archive 路径或发布权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEvolutionCycleRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// Cycle 稳定身份。
    pub cycle_id: EvolutionCycleId,
    /// 当前 Stable Parent Revision。
    pub parent_revision_id: GenomeRevisionId,
    /// 请求方观察到的 Parent Genome 行为摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Stable lineage。
    pub lineage: String,
    /// Parent 当前单调代数。
    pub expected_parent_generation: u64,
    /// 支撑变异的脱敏 Episode ID。
    pub evidence_episode_ids: BTreeSet<EpisodeId>,
    /// 期望由独立 Evaluator 使用的受信 Context Fixture 版本。
    pub expected_fixture_version: DatasetVersionId,
    /// 请求生成时间，使用 Unix 毫秒；同时作为 Candidate 幂等时间基准。
    pub requested_at_ms: u64,
}

impl ContextEvolutionCycleRequestV1 {
    /// 校验请求版本、Stable 名称和证据集合边界。
    ///
    /// # Errors
    ///
    /// Schema、lineage 或证据集合无效时返回 [`ContextCycleError::InvalidRequest`]。
    pub fn validate(&self) -> Result<(), ContextCycleError> {
        if self.schema_version != CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION {
            return Err(ContextCycleError::InvalidRequest(
                "不支持的 Context Cycle schema 版本".to_string(),
            ));
        }
        validate_lineage(&self.lineage)?;
        if self.evidence_episode_ids.is_empty() || self.evidence_episode_ids.len() > 256 {
            return Err(ContextCycleError::InvalidRequest(
                "Context Cycle 必须绑定 1..=256 条脱敏 Episode".to_string(),
            ));
        }
        Ok(())
    }
}

/// Context Cycle 的固定阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCycleStage {
    /// 请求已归档。
    Requested,
    /// 正在验证证据并生成三个有界提案。
    Mutating,
    /// 正在通过可信 Builder 登记 Candidate。
    BuildingCandidates,
    /// 正在调用独立 Context Evaluator。
    Evaluating,
    /// 正在选择唯一晋升目标。
    SelectingWinner,
    /// 正在请求受信 Release Controller 晋升。
    Promoting,
    /// Candidate 已成为 Stable，等待 Runtime 健康观察。
    AwaitingHealth,
    /// 正在请求受信健康验证。
    VerifyingHealth,
    /// 新 Context Policy 已通过健康验证。
    HealthVerified,
    /// 健康失败，正在请求回滚。
    RollingBack,
    /// Stable 已回滚到 Parent。
    RolledBack,
    /// 全部候选均被 Context Gate 拒绝。
    Rejected,
    /// 确定性失败已归档。
    Failed,
}

/// Context Cycle 的不可变完整快照。
///
/// 每份后续快照都保留此前全部 Proposal、Candidate、八指标回执和 Release/Health 回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEvolutionCycleSnapshotV1 {
    /// 快照 schema 版本。
    pub schema_version: u32,
    /// 原始请求。
    pub request: ContextEvolutionCycleRequestV1,
    /// 当前阶段。
    pub stage: ContextCycleStage,
    /// 从零开始的单调快照序号。
    pub sequence: u64,
    /// 前一快照规范 JSON 的摘要。
    pub previous_digest: Option<ArtifactDigest>,
    /// 全部有界 Context Policy 提案。
    pub proposals: Vec<ContextPolicyMutationProposalV1>,
    /// 全部可信 Builder Candidate 回执。
    pub candidates: Vec<ContextPolicyCandidateV1>,
    /// 全部独立 Context Gate 与正式 Archive 回执。
    pub evaluation_receipts: Vec<ContextEvaluationReceiptV1>,
    /// 被选择的 Candidate；无合格候选时为空。
    pub winner: Option<CandidateId>,
    /// Promotion 成功回执。
    pub release_receipt: Option<ReleaseReceiptV1>,
    /// Runtime 健康验证回执。
    pub health_receipt: Option<HealthCheckReceiptV1>,
    /// 健康失败后的 Rollback 回执。
    pub rollback_receipt: Option<ReleaseReceiptV1>,
    /// 失败终态的稳定错误码。
    pub failure_code: Option<String>,
    /// 本快照生成时间，使用 Unix 毫秒。
    pub created_at_ms: u64,
}

impl ContextEvolutionCycleSnapshotV1 {
    /// 校验快照的请求、数量和全部候选/回执身份绑定。
    ///
    /// # Errors
    ///
    /// 快照或任一归档制品结构、数量、顺序、修订绑定不一致时返回
    /// [`ContextCycleArchiveError::InvalidSnapshot`]。
    pub fn validate(&self) -> Result<(), ContextCycleArchiveError> {
        if self.schema_version != CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "快照 schema 版本不受支持".to_string(),
            ));
        }
        self.request
            .validate()
            .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
        if self.proposals.len() > CONTEXT_EVOLUTION_CANDIDATE_COUNT
            || self.candidates.len() > self.proposals.len()
            || self.evaluation_receipts.len() > self.candidates.len()
        {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 制品数量或前缀关系无效".to_string(),
            ));
        }
        for proposal in &self.proposals {
            proposal
                .validate()
                .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            if proposal.parent_revision_id != self.request.parent_revision_id
                || proposal.parent_genome_digest != self.request.parent_genome_digest
                || proposal.evidence_episode_ids != self.request.evidence_episode_ids
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Proposal 与 Cycle 请求错绑".to_string(),
                ));
            }
        }
        for (candidate, proposal) in self.candidates.iter().zip(&self.proposals) {
            candidate
                .validate()
                .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            if candidate.cycle_id != self.request.cycle_id
                || candidate.mutation_id != proposal.mutation_id
                || candidate.parent_revision_id != self.request.parent_revision_id
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Candidate 与 Proposal 错绑".to_string(),
                ));
            }
        }
        for (receipt, candidate) in self.evaluation_receipts.iter().zip(&self.candidates) {
            receipt
                .validate(agent_evolution_protocol::M6_CONTEXT_GATE_VERSION)
                .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            if receipt.request_id != evaluation_request_id(&self.request, candidate)
                || receipt.context_report.parent_revision_id != self.request.parent_revision_id
                || receipt.context_report.candidate_revision_id != candidate.candidate_revision_id
                || receipt.fixture_version != self.request.expected_fixture_version
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Evaluation Receipt 与 Candidate 错绑".to_string(),
                ));
            }
        }
        if let Some(winner) = &self.winner {
            let Some(candidate) = self
                .candidates
                .iter()
                .find(|candidate| &candidate.candidate_id == winner)
            else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Cycle Winner 不属于候选集合".to_string(),
                ));
            };
            let Some(receipt) = self.evaluation_receipts.iter().find(|receipt| {
                receipt.context_report.candidate_revision_id == candidate.candidate_revision_id
            }) else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Cycle Winner 缺少正式评测回执".to_string(),
                ));
            };
            if receipt.context_report.decision != GateDecision::Pass
                || receipt.lifecycle != EvolutionLifecycle::Eligible
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Cycle Winner 未通过固定 Gate".to_string(),
                ));
            }
        }
        if let Some(release) = &self.release_receipt {
            release
                .validate()
                .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            let Some(winner) = &self.winner else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Promotion 缺少 Winner".to_string(),
                ));
            };
            let Some(candidate) = self
                .candidates
                .iter()
                .find(|candidate| &candidate.candidate_id == winner)
            else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Promotion Winner 不属于候选集合".to_string(),
                ));
            };
            let Some(evaluation) = self.evaluation_receipts.iter().find(|receipt| {
                receipt.context_report.candidate_revision_id == candidate.candidate_revision_id
            }) else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Promotion 缺少 Winner 评测回执".to_string(),
                ));
            };
            let expected_release_id = deterministic_release_id(
                b"context-promotion-v1",
                &self.request.cycle_id,
                winner.as_str(),
                evaluation.report_id.as_str(),
            )
            .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            if release.release_id != expected_release_id
                || release.report_id != evaluation.report_id
                || release.lineage != self.request.lineage
                || release.from != self.request.parent_revision_id
                || release.to != candidate.candidate_revision_id
                || Some(release.generation)
                    != self.request.expected_parent_generation.checked_add(1)
                || release.rollback_of.is_some()
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Promotion 回执与 Cycle/Winner 错绑".to_string(),
                ));
            }
        }
        if let Some(health) = &self.health_receipt {
            health
                .validate()
                .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            let Some(release) = &self.release_receipt else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Health 回执缺少 Promotion".to_string(),
                ));
            };
            if health.request_id != format!("{}-context-health", self.request.cycle_id)
                || health.release_id != release.release_id
                || health.lineage != release.lineage
                || health.expected_revision_id != release.to
                || health.expected_generation != release.generation
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Health 回执与 Promotion 错绑".to_string(),
                ));
            }
        }
        if let Some(rollback) = &self.rollback_receipt {
            rollback
                .validate()
                .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            let Some(release) = &self.release_receipt else {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Rollback 回执缺少 Promotion".to_string(),
                ));
            };
            let expected_rollback_id = deterministic_release_id(
                b"context-rollback-v1",
                &self.request.cycle_id,
                release.release_id.as_str(),
                release.report_id.as_str(),
            )
            .map_err(|error| ContextCycleArchiveError::InvalidSnapshot(error.to_string()))?;
            if rollback.release_id != expected_rollback_id
                || rollback.rollback_of.as_ref() != Some(&release.release_id)
                || rollback.report_id != release.report_id
                || rollback.lineage != release.lineage
                || rollback.from != release.to
                || rollback.to != self.request.parent_revision_id
                || Some(rollback.generation) != release.generation.checked_add(1)
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Rollback 回执与 Promotion/Parent 错绑".to_string(),
                ));
            }
        }
        if let Some(code) = &self.failure_code {
            if code.is_empty()
                || code.len() > 128
                || !code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return Err(ContextCycleArchiveError::InvalidSnapshot(
                    "Context Cycle 失败码不符合稳定文本边界".to_string(),
                ));
            }
        }
        self.validate_stage_artifacts()?;
        Ok(())
    }

    /// 校验阶段与已归档提案、候选和控制面回执的一致性。
    fn validate_stage_artifacts(&self) -> Result<(), ContextCycleArchiveError> {
        let failed = self.stage == ContextCycleStage::Failed;
        if failed != self.failure_code.is_some() {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段与失败码不一致".to_string(),
            ));
        }
        if failed {
            return Ok(());
        }

        if matches!(
            self.stage,
            ContextCycleStage::Requested | ContextCycleStage::Mutating
        ) && (!self.proposals.is_empty()
            || !self.candidates.is_empty()
            || !self.evaluation_receipts.is_empty())
        {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 初始阶段不得预置评测制品".to_string(),
            ));
        }
        if self.stage == ContextCycleStage::BuildingCandidates
            && !self.evaluation_receipts.is_empty()
        {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Candidate 构建阶段不得预置评测回执".to_string(),
            ));
        }

        let requires_proposals = matches!(
            self.stage,
            ContextCycleStage::BuildingCandidates
                | ContextCycleStage::Evaluating
                | ContextCycleStage::SelectingWinner
                | ContextCycleStage::Promoting
                | ContextCycleStage::AwaitingHealth
                | ContextCycleStage::VerifyingHealth
                | ContextCycleStage::HealthVerified
                | ContextCycleStage::RollingBack
                | ContextCycleStage::RolledBack
                | ContextCycleStage::Rejected
        );
        if requires_proposals && self.proposals.len() != CONTEXT_EVOLUTION_CANDIDATE_COUNT {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段缺少固定三个 Proposal".to_string(),
            ));
        }
        let requires_candidates = matches!(
            self.stage,
            ContextCycleStage::Evaluating
                | ContextCycleStage::SelectingWinner
                | ContextCycleStage::Promoting
                | ContextCycleStage::AwaitingHealth
                | ContextCycleStage::VerifyingHealth
                | ContextCycleStage::HealthVerified
                | ContextCycleStage::RollingBack
                | ContextCycleStage::RolledBack
                | ContextCycleStage::Rejected
        );
        if requires_candidates && self.candidates.len() != CONTEXT_EVOLUTION_CANDIDATE_COUNT {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段缺少固定三个 Candidate".to_string(),
            ));
        }
        let requires_evaluations = matches!(
            self.stage,
            ContextCycleStage::SelectingWinner
                | ContextCycleStage::Promoting
                | ContextCycleStage::AwaitingHealth
                | ContextCycleStage::VerifyingHealth
                | ContextCycleStage::HealthVerified
                | ContextCycleStage::RollingBack
                | ContextCycleStage::RolledBack
                | ContextCycleStage::Rejected
        );
        if requires_evaluations
            && self.evaluation_receipts.len() != CONTEXT_EVOLUTION_CANDIDATE_COUNT
        {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段缺少固定三个 Evaluation Receipt".to_string(),
            ));
        }
        let requires_winner = matches!(
            self.stage,
            ContextCycleStage::Promoting
                | ContextCycleStage::AwaitingHealth
                | ContextCycleStage::VerifyingHealth
                | ContextCycleStage::HealthVerified
                | ContextCycleStage::RollingBack
                | ContextCycleStage::RolledBack
        );
        if requires_winner != self.winner.is_some() {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段与 Winner 不一致".to_string(),
            ));
        }
        let requires_release = matches!(
            self.stage,
            ContextCycleStage::AwaitingHealth
                | ContextCycleStage::VerifyingHealth
                | ContextCycleStage::HealthVerified
                | ContextCycleStage::RollingBack
                | ContextCycleStage::RolledBack
        );
        if requires_release != self.release_receipt.is_some() {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段与 Promotion 回执不一致".to_string(),
            ));
        }
        if self.stage == ContextCycleStage::HealthVerified
            && self
                .health_receipt
                .as_ref()
                .is_none_or(|receipt| !receipt.verified)
        {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context HealthVerified 缺少成功健康回执".to_string(),
            ));
        }
        if matches!(
            self.stage,
            ContextCycleStage::RollingBack | ContextCycleStage::RolledBack
        ) && self
            .health_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.verified)
        {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Rollback 阶段缺少失败健康回执".to_string(),
            ));
        }
        let requires_health_receipt = matches!(
            self.stage,
            ContextCycleStage::HealthVerified
                | ContextCycleStage::RollingBack
                | ContextCycleStage::RolledBack
        );
        if requires_health_receipt != self.health_receipt.is_some() {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段与 Health 回执不一致".to_string(),
            ));
        }
        if (self.stage == ContextCycleStage::RolledBack) != self.rollback_receipt.is_some() {
            return Err(ContextCycleArchiveError::InvalidSnapshot(
                "Context Cycle 阶段与 Rollback 回执不一致".to_string(),
            ));
        }
        Ok(())
    }
}

/// 只追加 Context Cycle 快照 Archive。
#[derive(Debug, Clone)]
pub struct FileContextCycleArchive {
    root: PathBuf,
}

impl FileContextCycleArchive {
    /// 创建延迟初始化的 Context Cycle Archive。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回 Archive 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 计算快照规范 JSON 的 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 快照无法序列化时返回 [`ContextCycleArchiveError`]。
    pub fn snapshot_digest(
        snapshot: &ContextEvolutionCycleSnapshotV1,
    ) -> Result<ArtifactDigest, ContextCycleArchiveError> {
        let bytes = serde_json::to_vec(snapshot).map_err(ContextCycleArchiveError::Serialize)?;
        digest_bytes(&bytes)
    }

    /// 追加一份不可变快照并重新读取验证。
    ///
    /// # Errors
    ///
    /// 快照、摘要链、阶段迁移、历史前缀、路径或 I/O 无效时返回
    /// [`ContextCycleArchiveError`]。
    pub async fn append(
        &self,
        snapshot: &ContextEvolutionCycleSnapshotV1,
    ) -> Result<(), ContextCycleArchiveError> {
        snapshot.validate()?;
        ensure_safe_directory(&self.root).await?;
        let cycle_root = self.cycle_root(&snapshot.request.cycle_id);
        ensure_safe_directory(&cycle_root).await?;
        let history = self.history(&snapshot.request.cycle_id).await?;
        validate_next_snapshot(history.last(), snapshot)?;
        let path = cycle_root.join(format!("{:020}.json", snapshot.sequence));
        let bytes =
            serde_json::to_vec_pretty(snapshot).map_err(ContextCycleArchiveError::Serialize)?;
        enforce_snapshot_size(bytes.len() as u64)?;
        let temporary = cycle_root.join(format!(".{}.tmp", Uuid::new_v4().simple()));
        let result = async {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|source| {
                    archive_io_error("创建 Context Cycle 临时文件", &temporary, source)
                })?;
            file.write_all(&bytes).await.map_err(|source| {
                archive_io_error("写入 Context Cycle 临时文件", &temporary, source)
            })?;
            file.sync_all().await.map_err(|source| {
                archive_io_error("同步 Context Cycle 临时文件", &temporary, source)
            })?;
            drop(file);
            fs::hard_link(&temporary, &path)
                .await
                .map_err(|source| archive_io_error("提交 Context Cycle 快照", &path, source))
        }
        .await;
        let _ = fs::remove_file(&temporary).await;
        result?;
        let observed = read_snapshot(&path).await?;
        if observed != *snapshot {
            return Err(ContextCycleArchiveError::CommitVerificationFailed(path));
        }
        Ok(())
    }

    /// 读取并完整验证指定 Cycle 的全部快照。
    ///
    /// # Errors
    ///
    /// 路径、记录、摘要链、阶段迁移或历史前缀无效时返回
    /// [`ContextCycleArchiveError`]。
    pub async fn history(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Vec<ContextEvolutionCycleSnapshotV1>, ContextCycleArchiveError> {
        let cycle_root = self.cycle_root(cycle_id);
        let metadata = match fs::symlink_metadata(&cycle_root).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(archive_io_error(
                    "检查 Context Cycle 目录",
                    &cycle_root,
                    source,
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ContextCycleArchiveError::UnsafePath(cycle_root));
        }
        let mut directory = fs::read_dir(&cycle_root)
            .await
            .map_err(|source| archive_io_error("遍历 Context Cycle 目录", &cycle_root, source))?;
        let mut paths = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|source| archive_io_error("读取 Context Cycle 目录项", &cycle_root, source))?
        {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                return Err(ContextCycleArchiveError::UnsafePath(path));
            };
            if name.starts_with('.') {
                continue;
            }
            if name.len() != 25
                || !name.ends_with(".json")
                || !name[..20].bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(ContextCycleArchiveError::UnsafePath(path));
            }
            paths.push(path);
        }
        paths.sort();
        let mut history = Vec::with_capacity(paths.len());
        for path in paths {
            let snapshot = read_snapshot(&path).await?;
            if snapshot.request.cycle_id != *cycle_id
                || path
                    != self
                        .cycle_root(cycle_id)
                        .join(format!("{:020}.json", snapshot.sequence))
            {
                return Err(ContextCycleArchiveError::IdentityMismatch(path));
            }
            validate_next_snapshot(history.last(), &snapshot)?;
            history.push(snapshot);
        }
        Ok(history)
    }

    /// 返回指定 Cycle 最新快照；不存在时返回 `None`。
    ///
    /// # Errors
    ///
    /// 完整历史无法验证时返回 [`ContextCycleArchiveError`]。
    pub async fn latest(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Option<ContextEvolutionCycleSnapshotV1>, ContextCycleArchiveError> {
        Ok(self.history(cycle_id).await?.pop())
    }

    /// 返回单个 Cycle 的固定归档目录。
    fn cycle_root(&self, cycle_id: &EvolutionCycleId) -> PathBuf {
        self.root.join(cycle_id.as_str())
    }
}

/// 不读取用户正文的固定 Context Policy Mutator。
#[derive(Debug, Clone, Copy, Default)]
pub struct BoundedContextMutator;

impl BoundedContextMutator {
    /// 从 Parent 策略和脱敏 Episode ID 生成三个不同且合法的候选提案。
    ///
    /// 三个表面分别调整近期消息保留、摘要预算和完整压缩水位。所有值都在协议边界内，且
    /// Post-summary 验证算法、约束固定区和 Plan snapshot 安全规则不会被弱化。
    ///
    /// # Errors
    ///
    /// Parent 策略、生成策略或确定性 Mutation ID 无效时返回 [`ContextCycleError`]。
    pub fn propose(
        &self,
        request: &ContextEvolutionCycleRequestV1,
        parent: &GenomeRevision,
        parent_policy_digest: &ArtifactDigest,
        parent_policy: &ContextPolicyV1,
    ) -> Result<Vec<ContextPolicyMutationProposalV1>, ContextCycleError> {
        request.validate()?;
        parent_policy
            .validate()
            .map_err(|error| ContextCycleError::InvalidParentPolicy(error.to_string()))?;
        if parent.revision_id != request.parent_revision_id
            || parent.digest != request.parent_genome_digest
        {
            return Err(ContextCycleError::StablePreconditionFailed);
        }
        let mut policies = Vec::with_capacity(CONTEXT_EVOLUTION_CANDIDATE_COUNT);

        let mut recent = parent_policy.clone();
        recent.recent_message_count = alternate_u16(recent.recent_message_count);
        policies.push((
            recent,
            "调整近期原文消息窗口，验证事实召回与 token 缩减的平衡",
        ));

        let mut budget = parent_policy.clone();
        budget.summary_token_budget = if budget.summary_token_budget > MIN_SUMMARY_TOKEN_BUDGET {
            budget.summary_token_budget - 1
        } else {
            budget.summary_token_budget + 1
        };
        policies.push((budget, "调整摘要输出预算，验证成本下降且结构化召回不回退"));

        let mut threshold = parent_policy.clone();
        if threshold.full_compact_threshold_tokens < MAX_CONTEXT_THRESHOLD_TOKENS {
            threshold.full_compact_threshold_tokens += 1;
        } else if threshold.micro_compact_threshold_tokens > MIN_CONTEXT_THRESHOLD_TOKENS {
            threshold.micro_compact_threshold_tokens -= 1;
        } else {
            threshold.micro_compact_threshold_tokens += 1;
        }
        policies.push((
            threshold,
            "调整完整压缩水位，验证下游成功率、延迟和 token 缩减",
        ));

        let mut proposals = Vec::with_capacity(CONTEXT_EVOLUTION_CANDIDATE_COUNT);
        for (index, (candidate_policy, hypothesis)) in policies.into_iter().enumerate() {
            candidate_policy
                .validate()
                .map_err(|error| ContextCycleError::InvalidMutation(error.to_string()))?;
            let proposal = ContextPolicyMutationProposalV1 {
                schema_version: CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION,
                mutation_id: deterministic_mutation_id(&request.cycle_id, index)?,
                parent_revision_id: parent.revision_id.clone(),
                parent_genome_digest: parent.digest.clone(),
                parent_policy_digest: parent_policy_digest.clone(),
                candidate_policy,
                evidence_episode_ids: request.evidence_episode_ids.clone(),
                hypothesis: hypothesis.to_string(),
            };
            proposal
                .validate()
                .map_err(|error| ContextCycleError::InvalidMutation(error.to_string()))?;
            proposals.push(proposal);
        }
        if proposals
            .iter()
            .map(|proposal| proposal.candidate_policy.canonical_bytes())
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(|error| ContextCycleError::InvalidMutation(error.to_string()))?
            .len()
            != CONTEXT_EVOLUTION_CANDIDATE_COUNT
        {
            return Err(ContextCycleError::InvalidMutation(
                "Context Mutator 生成了重复策略".to_string(),
            ));
        }
        Ok(proposals)
    }
}

/// 串联 Context Mutator、Candidate Builder、独立 Evaluator 与 Release Controller 的 Runner。
pub struct ContextEvolutionCycle<E>
where
    E: ContextEvaluatorClient,
{
    evolution_root: PathBuf,
    archive: FileContextCycleArchive,
    evaluator: E,
    fixture_version: DatasetVersionId,
}

impl<E> ContextEvolutionCycle<E>
where
    E: ContextEvaluatorClient,
{
    /// 使用 Evolution 根、独立 Evaluator 和固定 Fixture 版本创建 Runner。
    pub fn new(
        evolution_root: impl Into<PathBuf>,
        evaluator: E,
        fixture_version: DatasetVersionId,
    ) -> Self {
        let evolution_root = evolution_root.into();
        Self {
            archive: FileContextCycleArchive::new(evolution_root.join("context-cycles")),
            evolution_root,
            evaluator,
            fixture_version,
        }
    }

    /// 返回只追加 Context Cycle Archive。
    pub fn archive(&self) -> &FileContextCycleArchive {
        &self.archive
    }

    /// 执行或幂等恢复一次完整 Context Cycle，直到等待健康观察或进入终态。
    ///
    /// # Errors
    ///
    /// 请求、Stable/Episode 绑定、策略 CAS、Candidate Builder、独立 Evaluator、Archive 或
    /// 终态 Outbox 消费失败时返回 [`ContextCycleError`]。
    pub async fn run(
        &self,
        request: &ContextEvolutionCycleRequestV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        request.validate()?;
        if request.expected_fixture_version != self.fixture_version {
            return Err(ContextCycleError::FixtureVersionMismatch);
        }
        let initial = if let Some(existing) = self.archive.latest(&request.cycle_id).await? {
            if existing.request != *request {
                return Err(ContextCycleError::CycleRequestConflict);
            }
            if is_terminal(existing.stage) {
                if should_consume_outbox(existing.stage) {
                    self.consume_outbox(request).await?;
                }
                return Ok(existing);
            }
            if existing.stage == ContextCycleStage::AwaitingHealth {
                return Ok(existing);
            }
            existing
        } else {
            self.validate_inputs(request).await?;
            let initial = ContextEvolutionCycleSnapshotV1 {
                schema_version: CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION,
                request: request.clone(),
                stage: ContextCycleStage::Requested,
                sequence: 0,
                previous_digest: None,
                proposals: Vec::new(),
                candidates: Vec::new(),
                evaluation_receipts: Vec::new(),
                winner: None,
                release_receipt: None,
                health_receipt: None,
                rollback_receipt: None,
                failure_code: None,
                created_at_ms: now_ms()?,
            };
            self.archive.append(&initial).await?;
            initial
        };
        match self.run_active(initial).await {
            Ok(snapshot) => {
                if should_consume_outbox(snapshot.stage) {
                    self.consume_outbox(request).await?;
                }
                Ok(snapshot)
            }
            Err(error) => {
                if error.should_close_cycle() {
                    let _ = self.append_failed(request, error.code()).await;
                }
                Err(error)
            }
        }
    }

    /// 使用受信 Runtime 观察完成健康验证，并在失败时自动回滚。
    ///
    /// # Errors
    ///
    /// Cycle 不存在、阶段不允许、健康观察/发布控制面、Archive 或终态 Outbox 消费失败时
    /// 返回 [`ContextCycleError`]。
    pub async fn verify_health(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        let current = self
            .archive
            .latest(cycle_id)
            .await?
            .ok_or_else(|| ContextCycleError::CycleNotFound(cycle_id.clone()))?;
        if is_terminal(current.stage) {
            if should_consume_outbox(current.stage) {
                self.consume_outbox(&current.request).await?;
            }
            return Ok(current);
        }
        if current.stage != ContextCycleStage::AwaitingHealth
            && current.stage != ContextCycleStage::VerifyingHealth
            && current.stage != ContextCycleStage::RollingBack
        {
            return Err(ContextCycleError::HealthNotReady(current.stage));
        }
        let current = if current.stage == ContextCycleStage::AwaitingHealth {
            self.advance(current, ContextCycleStage::VerifyingHealth, |_| {})
                .await?
        } else {
            current
        };
        let snapshot = self.run_active(current).await?;
        if should_consume_outbox(snapshot.stage) {
            self.consume_outbox(&snapshot.request).await?;
        }
        Ok(snapshot)
    }

    /// 从最后一份完整快照继续固定状态机。
    async fn run_active(
        &self,
        mut current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        loop {
            current = match current.stage {
                ContextCycleStage::Requested => {
                    self.advance(current, ContextCycleStage::Mutating, |_| {})
                        .await?
                }
                ContextCycleStage::Mutating => self.resume_mutation(current).await?,
                ContextCycleStage::BuildingCandidates => {
                    self.resume_candidate_build(current).await?
                }
                ContextCycleStage::Evaluating => self.resume_evaluation(current).await?,
                ContextCycleStage::SelectingWinner => self.resume_selection(current).await?,
                ContextCycleStage::Promoting => self.resume_promotion(current).await?,
                ContextCycleStage::AwaitingHealth => return Ok(current),
                ContextCycleStage::VerifyingHealth => {
                    self.resume_health_verification(current).await?
                }
                ContextCycleStage::RollingBack => self.resume_rollback(current).await?,
                ContextCycleStage::HealthVerified
                | ContextCycleStage::RolledBack
                | ContextCycleStage::Rejected
                | ContextCycleStage::Failed => return Ok(current),
            };
        }
    }

    /// 重新加载受信 Parent 策略并生成固定三个提案。
    async fn resume_mutation(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        let (parent, policy_digest, policy) = self.load_parent_policy(&current.request).await?;
        let proposals =
            BoundedContextMutator.propose(&current.request, &parent, &policy_digest, &policy)?;
        self.advance(current, ContextCycleStage::BuildingCandidates, |snapshot| {
            snapshot.proposals = proposals;
        })
        .await
    }

    /// 逐个构建并归档 Candidate，支持 Candidate 已登记但快照尚未提交的恢复。
    async fn resume_candidate_build(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        if current.proposals.len() != CONTEXT_EVOLUTION_CANDIDATE_COUNT {
            return Err(ContextCycleError::StateArtifactMismatch);
        }
        if current.candidates.len() == current.proposals.len() {
            return self
                .advance(current, ContextCycleStage::Evaluating, |_| {})
                .await;
        }
        let index = current.candidates.len();
        let genomes = FileGenomeResolver::new(&self.evolution_root)
            .store()
            .clone();
        let artifacts = FileArtifactStore::new(self.evolution_root.join("artifacts"));
        let created_at_ms = current
            .request
            .requested_at_ms
            .checked_add(index as u64 + 1)
            .ok_or(ContextCycleError::TimestampOverflow)?;
        let candidate = ContextCandidateBuilder::new(&genomes, &artifacts)
            .build_at(
                current.request.cycle_id.clone(),
                &current.proposals[index],
                created_at_ms,
            )
            .await?;
        self.advance(current, ContextCycleStage::BuildingCandidates, |snapshot| {
            snapshot.candidates.push(candidate);
        })
        .await
    }

    /// 逐个调用独立 Context Evaluator，并保留每份八指标报告。
    async fn resume_evaluation(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        if current.candidates.len() != CONTEXT_EVOLUTION_CANDIDATE_COUNT {
            return Err(ContextCycleError::StateArtifactMismatch);
        }
        if current.evaluation_receipts.len() == current.candidates.len() {
            return self
                .advance(current, ContextCycleStage::SelectingWinner, |_| {})
                .await;
        }
        let candidate = &current.candidates[current.evaluation_receipts.len()];
        let receipt = self
            .evaluator
            .evaluate_context(&ContextEvaluationRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                request_id: evaluation_request_id(&current.request, candidate),
                parent_revision_id: current.request.parent_revision_id.clone(),
                candidate_revision_id: candidate.candidate_revision_id.clone(),
                lineage: current.request.lineage.clone(),
                expected_parent_generation: current.request.expected_parent_generation,
                expected_fixture_version: self.fixture_version.clone(),
            })
            .await?;
        self.advance(current, ContextCycleStage::Evaluating, |snapshot| {
            snapshot.evaluation_receipts.push(receipt);
        })
        .await
    }

    /// 只从完整正式回执集合选择一个 Pass + Eligible Candidate。
    async fn resume_selection(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        if current.evaluation_receipts.len() != current.candidates.len() {
            return Err(ContextCycleError::StateArtifactMismatch);
        }
        let winner = current
            .candidates
            .iter()
            .zip(&current.evaluation_receipts)
            .filter(|(_, receipt)| {
                receipt.context_report.decision == GateDecision::Pass
                    && receipt.lifecycle == EvolutionLifecycle::Eligible
            })
            .map(|(candidate, _)| candidate.candidate_id.clone())
            .min();
        let Some(winner) = winner else {
            return self
                .advance(current, ContextCycleStage::Rejected, |_| {})
                .await;
        };
        self.advance(current, ContextCycleStage::Promoting, |snapshot| {
            snapshot.winner = Some(winner);
        })
        .await
    }

    /// 使用确定性 Release ID 请求受信 Release Controller 晋升。
    async fn resume_promotion(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        let winner = current
            .winner
            .as_ref()
            .ok_or(ContextCycleError::StateArtifactMismatch)?;
        let candidate = current
            .candidates
            .iter()
            .find(|candidate| &candidate.candidate_id == winner)
            .ok_or(ContextCycleError::StateArtifactMismatch)?;
        let receipt = current
            .evaluation_receipts
            .iter()
            .find(|receipt| {
                receipt.context_report.candidate_revision_id == candidate.candidate_revision_id
            })
            .ok_or(ContextCycleError::StateArtifactMismatch)?;
        let release_id = deterministic_release_id(
            b"context-promotion-v1",
            &current.request.cycle_id,
            winner.as_str(),
            receipt.report_id.as_str(),
        )?;
        let release = self
            .evaluator
            .promote_context(&PromotionRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                report_id: receipt.report_id.clone(),
                release_id,
            })
            .await?;
        self.advance(current, ContextCycleStage::AwaitingHealth, |snapshot| {
            snapshot.release_receipt = Some(release);
        })
        .await
    }

    /// 复核 Runtime 观察，并进入健康终态或 Rollback 分支。
    async fn resume_health_verification(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        let release = current
            .release_receipt
            .as_ref()
            .ok_or(ContextCycleError::StateArtifactMismatch)?;
        let health = self
            .evaluator
            .health_context(&HealthCheckRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                request_id: format!("{}-context-health", current.request.cycle_id),
                release_id: release.release_id.clone(),
                lineage: release.lineage.clone(),
                expected_revision_id: release.to.clone(),
                expected_generation: release.generation,
            })
            .await?;
        let next = if health.verified {
            ContextCycleStage::HealthVerified
        } else {
            ContextCycleStage::RollingBack
        };
        self.advance(current, next, |snapshot| {
            snapshot.health_receipt = Some(health);
        })
        .await
    }

    /// 使用确定性 Rollback Release ID 请求原子回滚 Parent。
    async fn resume_rollback(
        &self,
        current: ContextEvolutionCycleSnapshotV1,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError> {
        let release = current
            .release_receipt
            .as_ref()
            .ok_or(ContextCycleError::StateArtifactMismatch)?;
        if current
            .health_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.verified)
        {
            return Err(ContextCycleError::StateArtifactMismatch);
        }
        let rollback_release_id = deterministic_release_id(
            b"context-rollback-v1",
            &current.request.cycle_id,
            release.release_id.as_str(),
            release.report_id.as_str(),
        )?;
        let rollback = self
            .evaluator
            .rollback_context(&RollbackRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: release.release_id.clone(),
                rollback_release_id,
            })
            .await?;
        self.advance(current, ContextCycleStage::RolledBack, |snapshot| {
            snapshot.rollback_receipt = Some(rollback);
        })
        .await
    }

    /// 验证 Stable、Parent Policy CAS 和全部脱敏 Episode 绑定。
    async fn validate_inputs(
        &self,
        request: &ContextEvolutionCycleRequestV1,
    ) -> Result<(), ContextCycleError> {
        self.load_parent_policy(request).await?;
        let episodes = FileEpisodeStore::new(self.evolution_root.join("episodes"));
        for episode_id in &request.evidence_episode_ids {
            let episode = episodes
                .get(episode_id)
                .await?
                .ok_or_else(|| ContextCycleError::EvidenceNotFound(episode_id.clone()))?;
            if episode.genome_revision_id != request.parent_revision_id
                || !episode.data_policy.permits_mutation_input()
            {
                return Err(ContextCycleError::EvidenceBindingMismatch(
                    episode_id.clone(),
                ));
            }
        }
        Ok(())
    }

    /// 加载并重新绑定当前 Stable Parent 与 Context Policy CAS。
    async fn load_parent_policy(
        &self,
        request: &ContextEvolutionCycleRequestV1,
    ) -> Result<(GenomeRevision, ArtifactDigest, ContextPolicyV1), ContextCycleError> {
        let resolver = FileGenomeResolver::new(&self.evolution_root);
        let stable = resolver.stable_reference(&request.lineage).await?;
        if stable.revision_id != request.parent_revision_id
            || stable.digest != request.parent_genome_digest
            || stable.generation != request.expected_parent_generation
        {
            return Err(ContextCycleError::StablePreconditionFailed);
        }
        let parent = resolver
            .resolve(&GenomeSelector::Revision(
                request.parent_revision_id.clone(),
            ))
            .await?;
        let policy_digest = parent
            .genome
            .context_policy
            .as_ref()
            .ok_or(ContextCycleError::MissingParentPolicy)?
            .config_digest
            .clone();
        let bytes = FileArtifactStore::new(self.evolution_root.join("artifacts"))
            .get(&policy_digest)
            .await?
            .ok_or_else(|| ContextCycleError::MissingPolicyArtifact(policy_digest.clone()))?;
        let policy = ContextPolicyV1::from_json_slice(&bytes)
            .map_err(|error| ContextCycleError::InvalidParentPolicy(error.to_string()))?;
        if policy
            .canonical_bytes()
            .map_err(|error| ContextCycleError::InvalidParentPolicy(error.to_string()))?
            != bytes
        {
            return Err(ContextCycleError::InvalidParentPolicy(
                "Parent Context Policy 不是规范 JSON".to_string(),
            ));
        }
        Ok((parent, policy_digest, policy))
    }

    /// 追加保留完整历史制品的下一阶段快照。
    async fn advance<F>(
        &self,
        previous: ContextEvolutionCycleSnapshotV1,
        stage: ContextCycleStage,
        mutate: F,
    ) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleError>
    where
        F: FnOnce(&mut ContextEvolutionCycleSnapshotV1),
    {
        let mut next = previous.clone();
        next.sequence = previous
            .sequence
            .checked_add(1)
            .ok_or(ContextCycleError::SequenceOverflow)?;
        next.previous_digest = Some(FileContextCycleArchive::snapshot_digest(&previous)?);
        next.stage = stage;
        next.created_at_ms = now_ms()?;
        next.failure_code = None;
        mutate(&mut next);
        self.archive.append(&next).await?;
        Ok(next)
    }

    /// 尽力追加一个保留此前全部制品的失败终态。
    async fn append_failed(
        &self,
        request: &ContextEvolutionCycleRequestV1,
        code: &'static str,
    ) -> Result<(), ContextCycleError> {
        let Some(previous) = self.archive.latest(&request.cycle_id).await? else {
            return Ok(());
        };
        if is_terminal(previous.stage) {
            return Ok(());
        }
        self.advance(previous, ContextCycleStage::Failed, |snapshot| {
            snapshot.failure_code = Some(code.to_string());
        })
        .await?;
        Ok(())
    }

    /// 仅在闭环终态消费与请求证据精确绑定的 Evolution Outbox 记录。
    ///
    /// 消费标记独立于原始只追加记录；重复恢复终态时会幂等跳过已消费项。等待健康观察期间
    /// 不调用本方法，确保 Promotion 尚未验证或回滚时证据仍可恢复。
    async fn consume_outbox(
        &self,
        request: &ContextEvolutionCycleRequestV1,
    ) -> Result<(), ContextCycleError> {
        let store = FileEvolutionOutbox::new(self.evolution_root.join("outbox"));
        for item in store.pending().await? {
            if request.evidence_episode_ids.contains(&item.episode_id) {
                store.mark_consumed(&item.outbox_id).await?;
            }
        }
        Ok(())
    }
}

/// Context Cycle 执行错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextCycleError {
    /// 请求结构或安全名称无效。
    #[error("Context Cycle 请求无效：{0}")]
    InvalidRequest(String),
    /// Runner 固定 Fixture 版本与请求不匹配。
    #[error("Context Cycle Fixture 版本不匹配")]
    FixtureVersionMismatch,
    /// 相同 Cycle ID 已绑定另一请求。
    #[error("Context Cycle ID 已绑定另一请求")]
    CycleRequestConflict,
    /// Cycle 不存在。
    #[error("Context Cycle 不存在：{0}")]
    CycleNotFound(EvolutionCycleId),
    /// 当前阶段不能执行健康验证。
    #[error("Context Cycle 当前阶段不能验证健康：{0:?}")]
    HealthNotReady(ContextCycleStage),
    /// Stable Revision、摘要或代数已变化。
    #[error("Context Cycle Stable 前置条件失败")]
    StablePreconditionFailed,
    /// Parent 缺少 Context Policy。
    #[error("Context Cycle Parent 缺少 Context Policy")]
    MissingParentPolicy,
    /// Parent Context Policy CAS 制品不存在。
    #[error("Context Cycle Parent Policy 制品不存在：{0}")]
    MissingPolicyArtifact(ArtifactDigest),
    /// Parent Context Policy 无效。
    #[error("Context Cycle Parent Policy 无效：{0}")]
    InvalidParentPolicy(String),
    /// 有界 Mutator 生成无效提案。
    #[error("Context Mutator 生成无效提案：{0}")]
    InvalidMutation(String),
    /// 证据 Episode 不存在。
    #[error("Context Cycle 证据 Episode 不存在：{0}")]
    EvidenceNotFound(EpisodeId),
    /// Episode 不属于 Parent 或不允许进入变异。
    #[error("Context Cycle 证据 Episode 与 Parent/数据策略不匹配：{0}")]
    EvidenceBindingMismatch(EpisodeId),
    /// 恢复快照中的制品前缀或身份不一致。
    #[error("Context Cycle 归档制品状态不一致")]
    StateArtifactMismatch,
    /// 快照序号溢出。
    #[error("Context Cycle 快照序号溢出")]
    SequenceOverflow,
    /// Candidate 幂等时间溢出。
    #[error("Context Cycle Candidate 时间溢出")]
    TimestampOverflow,
    /// 系统时间不可用。
    #[error("Context Cycle 系统时间不可用：{0}")]
    Clock(#[from] SystemTimeError),
    /// Artifact CAS 失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// Episode Store 失败。
    #[error(transparent)]
    Episode(#[from] EpisodeStoreError),
    /// Genome Resolver 失败。
    #[error(transparent)]
    Resolver(#[from] GenomeResolverError),
    /// Context Candidate Builder 失败。
    #[error(transparent)]
    Candidate(#[from] ContextCandidateBuildError),
    /// 独立 Evaluator 进程失败。
    #[error(transparent)]
    Evaluator(#[from] EvaluatorProcessError),
    /// Context Cycle Archive 失败。
    #[error(transparent)]
    Archive(#[from] ContextCycleArchiveError),
    /// Evolution Outbox 读取或消费失败。
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    /// 确定性 ID 无效。
    #[error("Context Cycle 确定性 ID 无效：{0}")]
    DeterministicId(String),
}

impl ContextCycleError {
    /// 返回适合 CLI 跨进程消费的稳定错误码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "context_request_invalid",
            Self::FixtureVersionMismatch => "context_fixture_version_mismatch",
            Self::CycleRequestConflict => "context_cycle_request_conflict",
            Self::CycleNotFound(_) => "context_cycle_not_found",
            Self::HealthNotReady(_) => "context_health_not_ready",
            Self::StablePreconditionFailed => "context_stable_precondition_failed",
            Self::MissingParentPolicy
            | Self::MissingPolicyArtifact(_)
            | Self::InvalidParentPolicy(_) => "context_parent_policy_invalid",
            Self::InvalidMutation(_) => "context_mutation_invalid",
            Self::EvidenceNotFound(_) | Self::EvidenceBindingMismatch(_) => {
                "context_evidence_invalid"
            }
            Self::StateArtifactMismatch => "context_cycle_state_invalid",
            Self::SequenceOverflow | Self::TimestampOverflow | Self::Clock(_) => {
                "context_cycle_time_invalid"
            }
            Self::Artifact(_)
            | Self::Episode(_)
            | Self::Resolver(_)
            | Self::Candidate(_)
            | Self::Archive(_)
            | Self::Outbox(_)
            | Self::DeterministicId(_) => "context_cycle_store_failed",
            Self::Evaluator(_) => "context_evaluator_failed",
        }
    }

    /// 判断错误是否是可归档的确定性闭环失败。
    fn should_close_cycle(&self) -> bool {
        matches!(
            self,
            Self::FixtureVersionMismatch
                | Self::StablePreconditionFailed
                | Self::MissingParentPolicy
                | Self::MissingPolicyArtifact(_)
                | Self::InvalidParentPolicy(_)
                | Self::InvalidMutation(_)
                | Self::EvidenceNotFound(_)
                | Self::EvidenceBindingMismatch(_)
                | Self::StateArtifactMismatch
        )
    }
}

/// Context Cycle Archive 的完整性、状态机与 I/O 错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextCycleArchiveError {
    /// 快照自身不满足协议不变量。
    #[error("Context Cycle 快照无效：{0}")]
    InvalidSnapshot(String),
    /// 首快照不是 sequence=0 的 Requested。
    #[error("Context Cycle 首快照必须是 sequence=0 的 Requested")]
    InvalidInitialSnapshot,
    /// 相邻快照序号不连续。
    #[error("Context Cycle 快照序号不连续")]
    SequenceMismatch,
    /// 前序摘要不匹配。
    #[error("Context Cycle 前序摘要不匹配")]
    PreviousDigestMismatch,
    /// Cycle 请求或文件身份不匹配。
    #[error("Context Cycle 快照身份不匹配：{0}")]
    IdentityMismatch(PathBuf),
    /// 阶段迁移不属于固定状态机。
    #[error("Context Cycle 阶段迁移非法：{from:?} -> {to:?}")]
    InvalidTransition {
        /// 前一阶段。
        from: ContextCycleStage,
        /// 后一阶段。
        to: ContextCycleStage,
    },
    /// 后一快照删除或改写了已归档制品。
    #[error("Context Cycle 后续快照不得删除或改写历史制品")]
    HistoryRewrite,
    /// 快照 JSON 序列化失败。
    #[error("序列化 Context Cycle 快照失败：{0}")]
    Serialize(serde_json::Error),
    /// 快照 JSON 损坏。
    #[error("Context Cycle 快照损坏 `{path}`：{source}")]
    InvalidRecord {
        /// 记录路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        source: serde_json::Error,
    },
    /// 快照超过固定上限。
    #[error("Context Cycle 快照过大：{actual} 字节，上限 {maximum} 字节")]
    TooLarge {
        /// 实际字节数。
        actual: u64,
        /// 固定上限。
        maximum: u64,
    },
    /// Archive 路径包含符号链接或意外类型。
    #[error("Context Cycle Archive 路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// 提交后重新读取的内容与请求不一致。
    #[error("Context Cycle 快照提交后验证失败：{0}")]
    CommitVerificationFailed(PathBuf),
    /// SHA-256 文本无法构造协议摘要。
    #[error("构造 Context Cycle 摘要失败：{0}")]
    InvalidDigest(String),
    /// 文件系统操作失败。
    #[error("{operation}失败 `{path}`：{source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        source: std::io::Error,
    },
}

/// 校验并连接相邻快照。
fn validate_next_snapshot(
    previous: Option<&ContextEvolutionCycleSnapshotV1>,
    next: &ContextEvolutionCycleSnapshotV1,
) -> Result<(), ContextCycleArchiveError> {
    match previous {
        None => {
            if next.sequence != 0
                || next.previous_digest.is_some()
                || next.stage != ContextCycleStage::Requested
            {
                return Err(ContextCycleArchiveError::InvalidInitialSnapshot);
            }
        }
        Some(previous) => {
            if previous.request != next.request {
                return Err(ContextCycleArchiveError::IdentityMismatch(PathBuf::new()));
            }
            if previous.sequence.checked_add(1) != Some(next.sequence) {
                return Err(ContextCycleArchiveError::SequenceMismatch);
            }
            if next.previous_digest.as_ref()
                != Some(&FileContextCycleArchive::snapshot_digest(previous)?)
            {
                return Err(ContextCycleArchiveError::PreviousDigestMismatch);
            }
            if !allowed_transition(previous.stage, next.stage) {
                return Err(ContextCycleArchiveError::InvalidTransition {
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
                return Err(ContextCycleArchiveError::HistoryRewrite);
            }
        }
    }
    Ok(())
}

/// 判断相邻阶段是否属于固定 Context Cycle 状态机。
fn allowed_transition(from: ContextCycleStage, to: ContextCycleStage) -> bool {
    if is_terminal(from) {
        return false;
    }
    to == ContextCycleStage::Failed
        || matches!(
            (from, to),
            (ContextCycleStage::Requested, ContextCycleStage::Mutating)
                | (
                    ContextCycleStage::Mutating,
                    ContextCycleStage::BuildingCandidates
                )
                | (
                    ContextCycleStage::BuildingCandidates,
                    ContextCycleStage::BuildingCandidates
                )
                | (
                    ContextCycleStage::BuildingCandidates,
                    ContextCycleStage::Evaluating
                )
                | (ContextCycleStage::Evaluating, ContextCycleStage::Evaluating)
                | (
                    ContextCycleStage::Evaluating,
                    ContextCycleStage::SelectingWinner
                )
                | (
                    ContextCycleStage::SelectingWinner,
                    ContextCycleStage::Promoting
                )
                | (
                    ContextCycleStage::SelectingWinner,
                    ContextCycleStage::Rejected
                )
                | (
                    ContextCycleStage::Promoting,
                    ContextCycleStage::AwaitingHealth
                )
                | (
                    ContextCycleStage::AwaitingHealth,
                    ContextCycleStage::VerifyingHealth
                )
                | (
                    ContextCycleStage::VerifyingHealth,
                    ContextCycleStage::HealthVerified
                )
                | (
                    ContextCycleStage::VerifyingHealth,
                    ContextCycleStage::RollingBack
                )
                | (
                    ContextCycleStage::RollingBack,
                    ContextCycleStage::RolledBack
                )
        )
}

/// 判断阶段是否已经关闭。
fn is_terminal(stage: ContextCycleStage) -> bool {
    matches!(
        stage,
        ContextCycleStage::HealthVerified
            | ContextCycleStage::RolledBack
            | ContextCycleStage::Rejected
            | ContextCycleStage::Failed
    )
}

/// 只有已完成可信健康结论或无需发布的拒绝终态才消费 Evolution Outbox。
fn should_consume_outbox(stage: ContextCycleStage) -> bool {
    matches!(
        stage,
        ContextCycleStage::HealthVerified
            | ContextCycleStage::RolledBack
            | ContextCycleStage::Rejected
    )
}

/// 判断旧向量是否是新向量的完整前缀。
fn is_prefix<T: PartialEq>(previous: &[T], next: &[T]) -> bool {
    next.starts_with(previous)
}

/// 返回一定不同且仍位于正整数范围内的 u16 值。
fn alternate_u16(value: u16) -> u16 {
    if value > 1 {
        value - 1
    } else {
        value + 1
    }
}

/// 从 Cycle 和候选序号派生稳定 Mutation ID。
fn deterministic_mutation_id(
    cycle_id: &EvolutionCycleId,
    index: usize,
) -> Result<MutationId, ContextCycleError> {
    let digest = Sha256::digest(
        [
            b"context-mutation-v1".as_slice(),
            cycle_id.as_str().as_bytes(),
            &index.to_be_bytes(),
        ]
        .concat(),
    );
    MutationId::new(format!("{}_{}", MutationId::PREFIX, hex_digest(&digest)))
        .map_err(|error| ContextCycleError::DeterministicId(error.to_string()))
}

/// 从 Cycle、目标和正式报告派生稳定 Release ID。
fn deterministic_release_id(
    domain: &[u8],
    cycle_id: &EvolutionCycleId,
    target: &str,
    report: &str,
) -> Result<ReleaseId, ContextCycleError> {
    let digest = Sha256::digest(
        [
            domain,
            cycle_id.as_str().as_bytes(),
            target.as_bytes(),
            report.as_bytes(),
        ]
        .concat(),
    );
    ReleaseId::new(format!("{}_{}", ReleaseId::PREFIX, hex_digest(&digest)))
        .map_err(|error| ContextCycleError::DeterministicId(error.to_string()))
}

/// 把 SHA-256 字节编码为固定小写十六进制。
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 构造单个 Candidate 的稳定 Context Evaluate 请求 ID。
fn evaluation_request_id(
    request: &ContextEvolutionCycleRequestV1,
    candidate: &ContextPolicyCandidateV1,
) -> String {
    format!("{}-context-{}", request.cycle_id, candidate.candidate_id)
}

/// 校验 Stable lineage 的有限安全名称。
fn validate_lineage(lineage: &str) -> Result<(), ContextCycleError> {
    if lineage.is_empty()
        || lineage.len() > 128
        || lineage.starts_with('/')
        || lineage.ends_with('/')
        || lineage
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || !lineage
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(ContextCycleError::InvalidRequest(
            "Context Cycle lineage 不符合安全名称规则".to_string(),
        ));
    }
    Ok(())
}

/// 返回当前 Unix 毫秒时间。
fn now_ms() -> Result<u64, ContextCycleError> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .map_err(|_| ContextCycleError::TimestampOverflow)
}

/// 创建并验证 Archive 普通目录。
async fn ensure_safe_directory(path: &Path) -> Result<(), ContextCycleArchiveError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| archive_io_error("创建 Context Cycle 目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| archive_io_error("检查 Context Cycle 目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ContextCycleArchiveError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

/// 读取并校验单份普通快照文件。
async fn read_snapshot(
    path: &Path,
) -> Result<ContextEvolutionCycleSnapshotV1, ContextCycleArchiveError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| archive_io_error("检查 Context Cycle 快照", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ContextCycleArchiveError::UnsafePath(path.to_path_buf()));
    }
    enforce_snapshot_size(metadata.len())?;
    let bytes = fs::read(path)
        .await
        .map_err(|source| archive_io_error("读取 Context Cycle 快照", path, source))?;
    enforce_snapshot_size(bytes.len() as u64)?;
    let snapshot: ContextEvolutionCycleSnapshotV1 =
        serde_json::from_slice(&bytes).map_err(|source| {
            ContextCycleArchiveError::InvalidRecord {
                path: path.to_path_buf(),
                source,
            }
        })?;
    snapshot.validate()?;
    Ok(snapshot)
}

/// 校验单份快照不超过固定上限。
fn enforce_snapshot_size(actual: u64) -> Result<(), ContextCycleArchiveError> {
    if actual > MAX_CONTEXT_CYCLE_SNAPSHOT_BYTES {
        return Err(ContextCycleArchiveError::TooLarge {
            actual,
            maximum: MAX_CONTEXT_CYCLE_SNAPSHOT_BYTES,
        });
    }
    Ok(())
}

/// 计算任意 Context Cycle 制品摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, ContextCycleArchiveError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| ContextCycleArchiveError::InvalidDigest(error.to_string()))
}

/// 构造保留路径上下文的 Archive I/O 错误。
fn archive_io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> ContextCycleArchiveError {
    ContextCycleArchiveError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}
