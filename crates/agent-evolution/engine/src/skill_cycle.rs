//! Skill 自进化生产闭环编排。
//!
//! 本模块拥有 Mutator、Candidate Builder、Archive 与 Stable 发布顺序；独立 Evaluator
//! 通过 [`SkillEvolutionOrchestrator`] 端口返回 Skill Exit Gate 的 Q→E→A 可信结果，避免
//! `agent-evolution` 反向依赖 `agent-evaluation`。Hidden Dataset、Verifier 与激活授权仍留在
//! 独立 Evaluator，Evolver 只接受脱敏回执并在发布前强制检查生产授权。

use crate::{
    verify_allowed_genome_diff, ArtifactStore, ArtifactStoreError, BoundedSkillMutator,
    EvolutionOutbox, FileArtifactStore, FileEvolutionOutbox, FileGenomeResolver,
    FileStableGenomePublisher, GenomeDiffError, GenomePromotionError, GenomeResolver,
    GenomeResolverError, GenomeSelector, GenomeStore, GenomeStoreError, MutationEvidence,
    OutboxError, SkillArtifactRepository, SkillCandidateBuildError, SkillCandidateBuilder,
    SkillMutationError, SkillMutationGenerator, SkillRepositoryError, StableGenomeRef,
};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, CandidateId, EvaluationReportId, EvolutionCycleId,
    EvolutionIssueId, GenomeDigest, GenomeRevision, GenomeRevisionId, MutationSurface, ReleaseId,
    SkillCandidateV1, SkillId, SkillMutationProposalV1, SkillStatusV1,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// Skill Cycle 归档结构版本。
pub const SKILL_EVOLUTION_ARCHIVE_SCHEMA_VERSION: u32 = 1;
/// Skill Cycle 阶段快照结构版本。
pub const SKILL_EVOLUTION_CYCLE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
/// Skill Cycle 固定保留的候选数量。
pub const SKILL_EVOLUTION_CANDIDATE_COUNT: usize = 3;
/// 单份 Skill Cycle 阶段快照的固定字节上限。
const MAX_SKILL_CYCLE_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;

/// 一轮 Skill 生产 Cycle 的可信输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvolutionCycleRequestV1 {
    /// Cycle 稳定 ID。
    pub cycle_id: EvolutionCycleId,
    /// 启动时解析到的 Stable Parent Revision。
    pub parent_revision_id: GenomeRevisionId,
    /// Stable Parent 的行为摘要。
    pub parent_genome_digest: GenomeDigest,
    /// 要原子更新的 Stable lineage。
    pub lineage: String,
    /// 启动时观察到的 Stable 代数。
    pub expected_parent_generation: u64,
    /// Mutator 写入 Quarantined 状态的可信 Unix 毫秒。
    pub mutation_generated_at_ms: u64,
    /// Candidate 快照的可信 Unix 毫秒。
    pub candidate_created_at_ms: u64,
    /// Exit Gate 写入 Evaluated 状态的可信 Unix 毫秒。
    pub evaluated_at_ms: u64,
    /// Exit Gate 写入 Active 状态的可信 Unix 毫秒。
    pub activated_at_ms: u64,
}

impl SkillEvolutionCycleRequestV1 {
    /// 校验身份、代数与 Q→E→A 时间顺序。
    ///
    /// # Errors
    ///
    /// lineage 为空、代数为零，或时间戳不是严格递增的非零值时返回
    /// [`SkillEvolutionCycleError::InvalidRequest`]。
    pub fn validate(&self) -> Result<(), SkillEvolutionCycleError> {
        if self.lineage.trim().is_empty() || self.expected_parent_generation == 0 {
            return Err(SkillEvolutionCycleError::InvalidRequest);
        }
        let timestamps = [
            self.mutation_generated_at_ms,
            self.candidate_created_at_ms,
            self.evaluated_at_ms,
            self.activated_at_ms,
        ];
        if timestamps[0] == 0 || timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(SkillEvolutionCycleError::InvalidRequest);
        }
        Ok(())
    }
}

/// 独立 Skill Exit Gate 对单个 Candidate 的脱敏结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillGateCycleOutcomeV1 {
    /// Gate 拒绝 Candidate；正式报告仍必须位于 Artifact CAS。
    Rejected {
        /// 被拒 Candidate。
        candidate_id: CandidateId,
        /// 正式 EvaluationReport ID。
        report_id: EvaluationReportId,
        /// 正式报告 CAS 引用。
        report_artifact: ArtifactRef,
    },
    /// Gate 已完成 Q→E→A 并产生后续 Active Genome。
    Promoted(Box<SkillGatePromotionV1>),
}

impl SkillGateCycleOutcomeV1 {
    /// 返回该 Gate 结果绑定的 Candidate ID。
    pub fn candidate_id(&self) -> &CandidateId {
        match self {
            Self::Rejected { candidate_id, .. } => candidate_id,
            Self::Promoted(receipt) => &receipt.evaluated_candidate.candidate_id,
        }
    }

    /// 返回该 Gate 结果绑定的正式报告 ID。
    pub fn report_id(&self) -> &EvaluationReportId {
        match self {
            Self::Rejected { report_id, .. } => report_id,
            Self::Promoted(receipt) => &receipt.report_id,
        }
    }

    /// 返回正式报告的不可变 CAS 引用。
    pub fn report_artifact(&self) -> &ArtifactRef {
        match self {
            Self::Rejected {
                report_artifact, ..
            } => report_artifact,
            Self::Promoted(receipt) => &receipt.report_artifact,
        }
    }
}

/// Exit Gate 通过后的 Q→E→A 可信回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillGatePromotionV1 {
    /// 已绑定正式报告但不改写原 Candidate Revision 的快照。
    pub evaluated_candidate: SkillCandidateV1,
    /// 正式报告 ID。
    pub report_id: EvaluationReportId,
    /// 正式报告的不可变 CAS 引用。
    pub report_artifact: ArtifactRef,
    /// 每个变更 Skill 的 Active 制品引用。
    pub active_skill_artifacts: BTreeMap<SkillId, ArtifactRef>,
    /// 只替换 Active Skill Set 的后续 Serve Genome。
    pub active_genome: GenomeRevision,
    /// 独立控制面给出的非敏感授权证据 ID。
    pub authorization_evidence_id: String,
    /// 独立控制面是否确认授权允许生产发布。
    pub production_permitted: bool,
}

impl SkillGatePromotionV1 {
    /// 判断本回执是否具备生产 Stable 发布资格。
    ///
    /// 布尔结论与非空授权证据必须同时存在。适配 `agent-evaluation` 时，调用方必须从
    /// `SkillActivationAuthorizationV1::permits_production()` 填充该结论，不能使用 Gate Pass
    /// 或 Candidate 自报代替。
    pub fn permits_production(&self) -> bool {
        self.production_permitted && valid_control_id(&self.authorization_evidence_id)
    }
}

/// Promotion 后的受信健康结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillHealthVerdictV1 {
    /// 新 Stable Genome 已通过生产健康检查。
    Healthy {
        /// 不含用户正文的健康证据 ID。
        evidence_id: String,
    },
    /// 新 Stable Genome 未通过健康检查，Cycle 必须自动回滚 Parent。
    Unhealthy {
        /// 不含用户正文的健康证据 ID。
        evidence_id: String,
        /// 稳定、可聚合的失败码。
        reason_code: String,
    },
}

impl SkillHealthVerdictV1 {
    /// 校验健康回执只包含有限的非空标识。
    fn validate(&self) -> Result<(), SkillEvolutionCycleError> {
        match self {
            Self::Healthy { evidence_id } if valid_control_id(evidence_id) => Ok(()),
            Self::Unhealthy {
                evidence_id,
                reason_code,
            } if valid_control_id(evidence_id) && valid_control_id(reason_code) => Ok(()),
            _ => Err(SkillEvolutionCycleError::InvalidHealthVerdict),
        }
    }
}

/// Skill Cycle 调用独立 Evaluator 与健康控制面的最小生产端口。
///
/// 实现必须调用真实 Skill Exit Gate，不能在 Evolver 内复制 Hidden Dataset、Verifier、Commit
/// Policy 或激活授权规则。返回的所有 Store 绑定仍会由 [`SkillEvolutionCycle`] 重新验证。
#[async_trait]
pub trait SkillEvolutionOrchestrator: Send + Sync {
    /// 对单个 Quarantined Candidate 执行独立 Gate 与 Q→E→A。
    ///
    /// # Errors
    ///
    /// Evaluator 不可用、Gate 基础设施失败或回执无法产生时返回
    /// [`SkillEvolutionOrchestratorError`]。
    async fn evaluate_and_promote(
        &self,
        candidate: &SkillCandidateV1,
        evaluated_at_ms: u64,
        activated_at_ms: u64,
    ) -> Result<SkillGateCycleOutcomeV1, SkillEvolutionOrchestratorError>;

    /// 复核刚发布的 Stable Genome 的生产健康状态。
    ///
    /// # Errors
    ///
    /// 健康观察不可用、错绑或控制面失败时返回 [`SkillEvolutionOrchestratorError`]；明确的
    /// 不健康必须返回 [`SkillHealthVerdictV1::Unhealthy`]，由 Cycle 执行自动回滚。
    async fn verify_health(
        &self,
        promoted: &StableGenomeRef,
    ) -> Result<SkillHealthVerdictV1, SkillEvolutionOrchestratorError>;
}

/// Skill Cycle 最终状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionDispositionV1 {
    /// 所有 Candidate 均被 Gate 拒绝或缺少生产授权。
    Rejected,
    /// Production Stable 已发布并通过健康检查。
    HealthVerified,
    /// Production Stable 发布后健康失败，已原子回滚 Parent。
    RolledBack,
}

/// Skill Cycle 的可恢复阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionCycleStage {
    /// 已验证请求与 Stable Parent，并建立首份快照。
    Requested,
    /// 正在从脱敏失败证据生成固定三份提案。
    Mutating,
    /// 正在逐份构建并登记 Candidate。
    BuildingCandidates,
    /// 正在逐份调用独立 Skill Evaluator。
    Evaluating,
    /// 正在从完整 Gate 回执中选择生产 Candidate。
    SelectingWinner,
    /// 正在原子发布生产 Stable。
    Promoting,
    /// Promotion 已归档，等待显式健康验证。
    AwaitingHealth,
    /// 正在调用独立健康控制面。
    VerifyingHealth,
    /// Promotion 已通过健康验证。
    HealthVerified,
    /// 正在原子回滚 Parent。
    RollingBack,
    /// 不健康 Promotion 已回滚。
    RolledBack,
    /// 所有 Candidate 均不具备生产发布资格。
    Rejected,
    /// 确定性闭环错误已经归档。
    Failed,
}

impl SkillEvolutionCycleStage {
    /// 判断恢复当前阶段是否仍需要调用方提供原始脱敏 MutationEvidence。
    pub fn requires_mutation_evidence(self) -> bool {
        matches!(self, Self::Requested | Self::Mutating)
    }
}

/// 一轮 Skill Cycle 的只追加阶段快照。
///
/// 快照保存所有已经可信提交的 Proposal、Candidate、Gate、Stable 与健康回执。后续修订只能
/// 在这些向量和可选字段后追加，不能删除或改写先前证据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvolutionCycleSnapshotV1 {
    /// 阶段快照结构版本。
    pub schema_version: u32,
    /// 原始可信请求。
    pub request: SkillEvolutionCycleRequestV1,
    /// 启动时完整读取的 Stable Parent 引用。
    pub parent_stable: StableGenomeRef,
    /// 当前可恢复阶段。
    pub stage: SkillEvolutionCycleStage,
    /// Cycle 内从零开始的连续快照序号。
    pub sequence: u64,
    /// 前一份快照规范 JSON 的 SHA-256 摘要；首份为 `None`。
    pub previous_digest: Option<ArtifactDigest>,
    /// 进入 Mutator 的可信 Issue；Proposal 尚未生成时为 `None`。
    pub source_issue_id: Option<EvolutionIssueId>,
    /// 与本轮证据精确绑定的 Outbox ID。
    #[serde(default)]
    pub source_outbox_ids: BTreeSet<String>,
    /// Mutator 已生成的全部 Proposal。
    #[serde(default)]
    pub proposals: Vec<SkillMutationProposalV1>,
    /// Builder 已登记的 Candidate 前缀。
    #[serde(default)]
    pub candidates: Vec<SkillCandidateV1>,
    /// 独立 Evaluator 已提交的 Gate 回执前缀。
    #[serde(default)]
    pub gate_outcomes: Vec<SkillGateCycleOutcomeV1>,
    /// 获得生产授权的 Winner。
    pub winner: Option<CandidateId>,
    /// Promotion 后的 Stable 引用。
    pub promotion: Option<StableGenomeRef>,
    /// Promotion 后健康结论。
    pub health: Option<SkillHealthVerdictV1>,
    /// 不健康后的 Parent 回滚引用。
    pub rollback: Option<StableGenomeRef>,
    /// 仅在可信终态设置的旧 Archive 兼容终态。
    pub disposition: Option<SkillEvolutionDispositionV1>,
    /// 确定性失败终态的稳定错误码。
    pub failure_code: Option<String>,
    /// 受信控制面写入该快照的 Unix 毫秒。
    pub created_at_ms: u64,
}

impl SkillEvolutionCycleSnapshotV1 {
    /// 校验快照结构、请求、阶段制品前缀与终态字段组合。
    ///
    /// # Errors
    ///
    /// schema、Stable Parent、证据绑定、制品数量或阶段字段组合不一致时返回
    /// [`SkillEvolutionCycleError::InvalidArchive`]。
    pub fn validate(&self) -> Result<(), SkillEvolutionCycleError> {
        self.request.validate()?;
        self.parent_stable.validate()?;
        if let Some(promotion) = &self.promotion {
            promotion.validate()?;
        }
        if let Some(health) = &self.health {
            health.validate()?;
        }
        if let Some(rollback) = &self.rollback {
            rollback.validate()?;
        }
        if self.schema_version != SKILL_EVOLUTION_CYCLE_SNAPSHOT_SCHEMA_VERSION
            || self.created_at_ms == 0
            || self.parent_stable.lineage != self.request.lineage
            || self.parent_stable.revision_id != self.request.parent_revision_id
            || self.parent_stable.digest != self.request.parent_genome_digest
            || self.parent_stable.generation != self.request.expected_parent_generation
            || self.proposals.len() > SKILL_EVOLUTION_CANDIDATE_COUNT
            || self.candidates.len() > self.proposals.len()
            || self.gate_outcomes.len() > self.candidates.len()
            || self
                .candidates
                .iter()
                .zip(&self.proposals)
                .any(|(candidate, proposal)| {
                    candidate.cycle_id != self.request.cycle_id
                        || candidate.mutation_id != proposal.mutation_id
                })
            || self
                .gate_outcomes
                .iter()
                .zip(&self.candidates)
                .any(|(outcome, candidate)| outcome.candidate_id() != &candidate.candidate_id)
        {
            return Err(SkillEvolutionCycleError::InvalidArchive);
        }
        let has_no_source = self.source_issue_id.is_none()
            && self.source_outbox_ids.is_empty()
            && self.proposals.is_empty();
        let has_committed_source = self.source_issue_id.is_some()
            && !self.source_outbox_ids.is_empty()
            && self.source_outbox_ids.iter().all(|id| valid_control_id(id))
            && self.proposals.len() == SKILL_EVOLUTION_CANDIDATE_COUNT;
        if !has_no_source && !has_committed_source {
            return Err(SkillEvolutionCycleError::InvalidArchive);
        }
        if let Some(winner) = &self.winner {
            let eligible = self.gate_outcomes.iter().any(|outcome| {
                matches!(outcome, SkillGateCycleOutcomeV1::Promoted(receipt)
                    if receipt.permits_production()
                        && receipt.evaluated_candidate.candidate_id == *winner)
            });
            if !eligible {
                return Err(SkillEvolutionCycleError::InvalidArchive);
            }
        }
        if self.stage == SkillEvolutionCycleStage::Failed {
            if self.disposition.is_some()
                || self
                    .failure_code
                    .as_deref()
                    .is_none_or(|code| !valid_control_id(code))
                || self.rollback.is_some() && self.health.is_none()
                || self.health.is_some() && self.promotion.is_none()
                || self.promotion.is_some() && self.winner.is_none()
            {
                return Err(SkillEvolutionCycleError::InvalidArchive);
            }
            return Ok(());
        }
        let before_mutation_commit = matches!(
            self.stage,
            SkillEvolutionCycleStage::Requested | SkillEvolutionCycleStage::Mutating
        );
        if before_mutation_commit != has_no_source
            || (!before_mutation_commit && !has_committed_source)
        {
            return Err(SkillEvolutionCycleError::InvalidArchive);
        }
        if self.candidates.len() != SKILL_EVOLUTION_CANDIDATE_COUNT
            && matches!(
                self.stage,
                SkillEvolutionCycleStage::Evaluating
                    | SkillEvolutionCycleStage::SelectingWinner
                    | SkillEvolutionCycleStage::Promoting
                    | SkillEvolutionCycleStage::AwaitingHealth
                    | SkillEvolutionCycleStage::VerifyingHealth
                    | SkillEvolutionCycleStage::HealthVerified
                    | SkillEvolutionCycleStage::RollingBack
                    | SkillEvolutionCycleStage::RolledBack
                    | SkillEvolutionCycleStage::Rejected
            )
        {
            return Err(SkillEvolutionCycleError::InvalidArchive);
        }
        if self.gate_outcomes.len() != SKILL_EVOLUTION_CANDIDATE_COUNT
            && matches!(
                self.stage,
                SkillEvolutionCycleStage::SelectingWinner
                    | SkillEvolutionCycleStage::Promoting
                    | SkillEvolutionCycleStage::AwaitingHealth
                    | SkillEvolutionCycleStage::VerifyingHealth
                    | SkillEvolutionCycleStage::HealthVerified
                    | SkillEvolutionCycleStage::RollingBack
                    | SkillEvolutionCycleStage::RolledBack
                    | SkillEvolutionCycleStage::Rejected
            )
        {
            return Err(SkillEvolutionCycleError::InvalidArchive);
        }
        let terminal = match self.stage {
            SkillEvolutionCycleStage::Rejected => {
                self.disposition == Some(SkillEvolutionDispositionV1::Rejected)
                    && self.winner.is_none()
                    && self.promotion.is_none()
                    && self.health.is_none()
                    && self.rollback.is_none()
            }
            SkillEvolutionCycleStage::HealthVerified => {
                self.disposition == Some(SkillEvolutionDispositionV1::HealthVerified)
                    && self.winner.is_some()
                    && self.promotion.is_some()
                    && matches!(self.health, Some(SkillHealthVerdictV1::Healthy { .. }))
                    && self.rollback.is_none()
            }
            SkillEvolutionCycleStage::RolledBack => {
                self.disposition == Some(SkillEvolutionDispositionV1::RolledBack)
                    && self.winner.is_some()
                    && self.promotion.is_some()
                    && matches!(self.health, Some(SkillHealthVerdictV1::Unhealthy { .. }))
                    && self.rollback.is_some()
            }
            SkillEvolutionCycleStage::Failed => unreachable!("失败阶段已提前校验"),
            _ => self.disposition.is_none() && self.failure_code.is_none(),
        };
        if !terminal
            || (self.winner.is_some()
                != matches!(
                    self.stage,
                    SkillEvolutionCycleStage::Promoting
                        | SkillEvolutionCycleStage::AwaitingHealth
                        | SkillEvolutionCycleStage::VerifyingHealth
                        | SkillEvolutionCycleStage::HealthVerified
                        | SkillEvolutionCycleStage::RollingBack
                        | SkillEvolutionCycleStage::RolledBack
                ))
            || (self.promotion.is_some()
                != matches!(
                    self.stage,
                    SkillEvolutionCycleStage::AwaitingHealth
                        | SkillEvolutionCycleStage::VerifyingHealth
                        | SkillEvolutionCycleStage::HealthVerified
                        | SkillEvolutionCycleStage::RollingBack
                        | SkillEvolutionCycleStage::RolledBack
                ))
            || (self.health.is_some()
                != matches!(
                    self.stage,
                    SkillEvolutionCycleStage::HealthVerified
                        | SkillEvolutionCycleStage::RollingBack
                        | SkillEvolutionCycleStage::RolledBack
                ))
            || (self.rollback.is_some() != (self.stage == SkillEvolutionCycleStage::RolledBack))
        {
            return Err(SkillEvolutionCycleError::InvalidArchive);
        }
        Ok(())
    }
}

/// 文件系统上的只追加 Skill Cycle 阶段 Archive。
#[derive(Debug, Clone)]
pub struct FileSkillEvolutionCycleArchive {
    root: PathBuf,
}

impl FileSkillEvolutionCycleArchive {
    /// 创建延迟初始化的 Skill Cycle Archive。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回阶段 Archive 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 计算快照规范 JSON 的 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 快照无法序列化或摘要无法构造成强类型值时返回错误。
    pub fn snapshot_digest(
        snapshot: &SkillEvolutionCycleSnapshotV1,
    ) -> Result<ArtifactDigest, SkillEvolutionCycleError> {
        let bytes =
            serde_json::to_vec(snapshot).map_err(SkillEvolutionCycleError::ArchiveSerialization)?;
        skill_digest_bytes(&bytes)
    }

    /// 追加一份不可变阶段快照并复读验证。
    ///
    /// # Errors
    ///
    /// 快照、摘要链、阶段迁移、历史前缀、路径或 I/O 无效时返回错误。
    pub async fn append(
        &self,
        snapshot: &SkillEvolutionCycleSnapshotV1,
    ) -> Result<(), SkillEvolutionCycleError> {
        snapshot.validate()?;
        ensure_safe_archive_directory(&self.root).await?;
        let cycle_root = self.cycle_root(&snapshot.request.cycle_id);
        ensure_safe_archive_directory(&cycle_root).await?;
        let history = self.history(&snapshot.request.cycle_id).await?;
        validate_next_skill_snapshot(history.last(), snapshot)?;
        let path = cycle_root.join(format!("{:020}.json", snapshot.sequence));
        let bytes = serde_json::to_vec_pretty(snapshot)
            .map_err(SkillEvolutionCycleError::ArchiveSerialization)?;
        enforce_skill_snapshot_size(bytes.len() as u64)?;
        append_new_file(&path, &bytes).await?;
        let observed = read_skill_snapshot(&path).await?;
        if observed != *snapshot {
            return Err(SkillEvolutionCycleError::ArchiveConflict(path));
        }
        Ok(())
    }

    /// 读取并验证指定 Cycle 的完整快照历史。
    ///
    /// # Errors
    ///
    /// 路径、JSON、摘要链、阶段迁移或历史前缀无效时返回错误。
    pub async fn history(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Vec<SkillEvolutionCycleSnapshotV1>, SkillEvolutionCycleError> {
        let cycle_root = self.cycle_root(cycle_id);
        let metadata = match fs::symlink_metadata(&cycle_root).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(archive_io_error(
                    "检查 Skill Cycle 阶段目录",
                    &cycle_root,
                    source,
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SkillEvolutionCycleError::UnsafeArchivePath(cycle_root));
        }
        let mut directory = fs::read_dir(&cycle_root)
            .await
            .map_err(|source| archive_io_error("遍历 Skill Cycle 阶段目录", &cycle_root, source))?;
        let mut paths = Vec::new();
        while let Some(entry) = directory.next_entry().await.map_err(|source| {
            archive_io_error("读取 Skill Cycle 阶段目录项", &cycle_root, source)
        })? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                return Err(SkillEvolutionCycleError::UnsafeArchivePath(path));
            };
            if name.starts_with('.') {
                continue;
            }
            if name.len() != 25
                || !name.ends_with(".json")
                || !name[..20].bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(SkillEvolutionCycleError::UnsafeArchivePath(path));
            }
            paths.push(path);
        }
        paths.sort();
        let mut history = Vec::with_capacity(paths.len());
        for path in paths {
            let snapshot = read_skill_snapshot(&path).await?;
            if snapshot.request.cycle_id != *cycle_id
                || path
                    != self
                        .cycle_root(cycle_id)
                        .join(format!("{:020}.json", snapshot.sequence))
            {
                return Err(SkillEvolutionCycleError::ArchiveHistoryInvalid);
            }
            validate_next_skill_snapshot(history.last(), &snapshot)?;
            history.push(snapshot);
        }
        Ok(history)
    }

    /// 返回指定 Cycle 的最新快照；不存在时返回 `None`。
    ///
    /// # Errors
    ///
    /// 完整历史无法验证时返回错误。
    pub async fn latest(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<Option<SkillEvolutionCycleSnapshotV1>, SkillEvolutionCycleError> {
        Ok(self.history(cycle_id).await?.pop())
    }

    /// 返回单个 Cycle 的固定阶段归档目录。
    fn cycle_root(&self, cycle_id: &EvolutionCycleId) -> PathBuf {
        self.root.join(cycle_id.as_str())
    }
}

/// 一轮 Skill Cycle 的不可变全量归档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvolutionArchiveV1 {
    /// 归档结构版本。
    pub schema_version: u32,
    /// 原始可信请求。
    pub request: SkillEvolutionCycleRequestV1,
    /// Mutator 产生的全部三份正式 Proposal。
    pub proposals: Vec<SkillMutationProposalV1>,
    /// Builder 产生的全部三份 Quarantined Candidate。
    pub candidates: Vec<SkillCandidateV1>,
    /// 与 Candidate 同序的全部 Gate 结果，包括 Reject 与未选中的 Active Genome。
    pub gate_outcomes: Vec<SkillGateCycleOutcomeV1>,
    /// 实际获得 Stable 发布资格的 Candidate；无合格项时为 `None`。
    pub winner: Option<CandidateId>,
    /// Production Promotion 后的 Stable 引用。
    pub promotion: Option<StableGenomeRef>,
    /// Promotion 后健康结论；未发布时为 `None`。
    pub health: Option<SkillHealthVerdictV1>,
    /// 健康失败后的 Parent Stable 回滚引用。
    pub rollback: Option<StableGenomeRef>,
    /// Cycle 最终状态。
    pub disposition: SkillEvolutionDispositionV1,
}

/// Skill Cycle 完成结果与归档位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEvolutionCycleResultV1 {
    /// 完整不可变归档正文。
    pub archive: SkillEvolutionArchiveV1,
    /// Cycle 归档的固定文件路径。
    pub archive_path: PathBuf,
}

/// 串联 M7 Mutator、Builder、独立 Gate、Stable 与健康回滚的生产 Runner。
pub struct SkillEvolutionCycle<G, O>
where
    G: SkillMutationGenerator,
    O: SkillEvolutionOrchestrator,
{
    evolution_root: PathBuf,
    artifacts: FileArtifactStore,
    archive: FileSkillEvolutionCycleArchive,
    mutator: BoundedSkillMutator<G>,
    orchestrator: O,
}

impl<G, O> SkillEvolutionCycle<G, O>
where
    G: SkillMutationGenerator,
    O: SkillEvolutionOrchestrator,
{
    /// 使用 Evolution 根、受限生成器与独立 Gate 端口创建生产 Runner。
    ///
    /// 构造本身不访问文件系统；Genome 使用 `<root>/genomes`，Artifact 使用
    /// `<root>/artifacts`，最终归档使用 `<root>/skill-cycle-archive`。
    pub fn new(evolution_root: impl Into<PathBuf>, generator: G, orchestrator: O) -> Self {
        let evolution_root = evolution_root.into();
        Self {
            artifacts: FileArtifactStore::new(evolution_root.join("artifacts")),
            archive: FileSkillEvolutionCycleArchive::new(evolution_root.join("skill-cycles")),
            evolution_root,
            mutator: BoundedSkillMutator::m7(generator),
            orchestrator,
        }
    }

    /// 返回 Runner 使用的 Artifact CAS，供独立 Gate 适配器共享同一 Store。
    pub fn artifacts(&self) -> &FileArtifactStore {
        &self.artifacts
    }

    /// 返回只追加 Skill Cycle 阶段 Archive。
    pub fn cycle_archive(&self) -> &FileSkillEvolutionCycleArchive {
        &self.archive
    }

    /// 执行一次完整 Skill 生产闭环并归档全部 Candidate。
    ///
    /// 固定生成三份 Candidate，逐一调用独立 Skill Exit Gate；第一个同时 Gate Pass 且
    /// [`SkillGatePromotionV1::permits_production`] 的回执才可更新 Stable。发布后立即请求
    /// 健康验证，不健康时绑定原 Promotion Release 自动回滚 Parent。无论是否选中，三份
    /// Proposal、Candidate 与 Gate 结果都会写入只追加 Archive，CAS 与 Genome Revision
    /// 从不删除。
    ///
    /// # Errors
    ///
    /// Stable 前置条件、MutationEvidence、候选构建、Gate 回执、Active Skill Set、发布、
    /// 健康控制面或 Archive 写入失败时返回 [`SkillEvolutionCycleError`]。
    pub async fn run(
        &self,
        request: &SkillEvolutionCycleRequestV1,
        evidence: &MutationEvidence,
    ) -> Result<SkillEvolutionCycleResultV1, SkillEvolutionCycleError> {
        request.validate()?;
        if let Some(archive) =
            read_existing_archive(&self.evolution_root, &request.cycle_id).await?
        {
            if archive.request != *request {
                return Err(SkillEvolutionCycleError::CycleRequestConflict);
            }
            validate_archive(&archive)?;
            if let Some(snapshot) = self.archive.latest(&request.cycle_id).await? {
                if snapshot.request != *request || !is_consumable_skill_stage(snapshot.stage) {
                    return Err(SkillEvolutionCycleError::StateArtifactMismatch);
                }
                if archive_from_snapshot(&snapshot)? != archive {
                    return Err(SkillEvolutionCycleError::StateArtifactMismatch);
                }
                self.finalize_terminal(&snapshot).await?;
            }
            let archive_path = final_archive_path(&self.evolution_root, &request.cycle_id);
            return Ok(SkillEvolutionCycleResultV1 {
                archive,
                archive_path,
            });
        }
        let mut snapshot = self.run_until_health(request, evidence).await?;
        if snapshot.stage == SkillEvolutionCycleStage::AwaitingHealth {
            snapshot = self.verify_health(&request.cycle_id).await?;
        }
        let archive = archive_from_snapshot(&snapshot)?;
        let archive_path = append_archive(&self.evolution_root, &archive).await?;
        Ok(SkillEvolutionCycleResultV1 {
            archive,
            archive_path,
        })
    }

    /// 执行或恢复 Skill Cycle，直到等待显式健康观察或进入可信终态。
    ///
    /// # Errors
    ///
    /// 请求、证据、Stable、Candidate、Evaluator、阶段 Archive 或终态归档失败时返回错误。
    pub async fn run_until_health(
        &self,
        request: &SkillEvolutionCycleRequestV1,
        evidence: &MutationEvidence,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        request.validate()?;
        if read_existing_archive(&self.evolution_root, &request.cycle_id)
            .await?
            .is_some()
            && self.archive.latest(&request.cycle_id).await?.is_none()
        {
            return Err(SkillEvolutionCycleError::LegacyArchiveAlreadyComplete);
        }
        let initial = if let Some(existing) = self.archive.latest(&request.cycle_id).await? {
            if existing.request != *request {
                return Err(SkillEvolutionCycleError::CycleRequestConflict);
            }
            self.validate_recovery_evidence(&existing, evidence)?;
            if is_terminal_skill_stage(existing.stage)
                || existing.stage == SkillEvolutionCycleStage::AwaitingHealth
            {
                self.finalize_terminal(&existing).await?;
                return Ok(existing);
            }
            existing
        } else {
            if evidence.genome_digest != request.parent_genome_digest
                || evidence.episodes.is_empty()
            {
                return Err(SkillEvolutionCycleError::EvidenceBindingMismatch);
            }
            let publisher = FileStableGenomePublisher::new(&self.evolution_root);
            let parent_stable = self.validate_stable(request, publisher.resolver()).await?;
            let initial = SkillEvolutionCycleSnapshotV1 {
                schema_version: SKILL_EVOLUTION_CYCLE_SNAPSHOT_SCHEMA_VERSION,
                request: request.clone(),
                parent_stable,
                stage: SkillEvolutionCycleStage::Requested,
                sequence: 0,
                previous_digest: None,
                source_issue_id: None,
                source_outbox_ids: BTreeSet::new(),
                proposals: Vec::new(),
                candidates: Vec::new(),
                gate_outcomes: Vec::new(),
                winner: None,
                promotion: None,
                health: None,
                rollback: None,
                disposition: None,
                failure_code: None,
                created_at_ms: now_ms()?,
            };
            self.archive.append(&initial).await?;
            initial
        };
        match self.run_active(initial, Some(evidence)).await {
            Ok(snapshot) => {
                self.finalize_terminal(&snapshot).await?;
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

    /// 不重新读取 MutationEvidence，从已提交 Proposal 之后的阶段继续 Cycle。
    ///
    /// # Errors
    ///
    /// Cycle 不存在、请求不一致、仍停留在 Mutation 前阶段，或后续控制面失败时返回错误。
    pub async fn resume(
        &self,
        request: &SkillEvolutionCycleRequestV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        request.validate()?;
        let current = self
            .archive
            .latest(&request.cycle_id)
            .await?
            .ok_or_else(|| SkillEvolutionCycleError::CycleNotFound(request.cycle_id.clone()))?;
        if current.request != *request {
            return Err(SkillEvolutionCycleError::CycleRequestConflict);
        }
        if current.stage.requires_mutation_evidence() {
            return Err(SkillEvolutionCycleError::EvidenceRequired);
        }
        if is_terminal_skill_stage(current.stage)
            || current.stage == SkillEvolutionCycleStage::AwaitingHealth
        {
            self.finalize_terminal(&current).await?;
            return Ok(current);
        }
        let snapshot = self.run_active(current, None).await?;
        self.finalize_terminal(&snapshot).await?;
        Ok(snapshot)
    }

    /// 从已归档 Promotion 继续健康验证，并在失败时自动回滚 Parent。
    ///
    /// # Errors
    ///
    /// Cycle 不存在、阶段不允许、独立健康控制面、回滚或归档失败时返回错误。
    pub async fn verify_health(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        let current = self
            .archive
            .latest(cycle_id)
            .await?
            .ok_or_else(|| SkillEvolutionCycleError::CycleNotFound(cycle_id.clone()))?;
        if is_terminal_skill_stage(current.stage) {
            self.finalize_terminal(&current).await?;
            return Ok(current);
        }
        if !matches!(
            current.stage,
            SkillEvolutionCycleStage::AwaitingHealth
                | SkillEvolutionCycleStage::VerifyingHealth
                | SkillEvolutionCycleStage::RollingBack
        ) {
            return Err(SkillEvolutionCycleError::HealthNotReady(current.stage));
        }
        let current = if current.stage == SkillEvolutionCycleStage::AwaitingHealth {
            self.advance(current, SkillEvolutionCycleStage::VerifyingHealth, |_| {})
                .await?
        } else {
            current
        };
        let snapshot = self.run_active(current, None).await?;
        self.finalize_terminal(&snapshot).await?;
        Ok(snapshot)
    }

    /// 从最后一份完整快照继续固定 Skill 状态机。
    async fn run_active(
        &self,
        mut current: SkillEvolutionCycleSnapshotV1,
        evidence: Option<&MutationEvidence>,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        loop {
            current = match current.stage {
                SkillEvolutionCycleStage::Requested => {
                    self.advance(current, SkillEvolutionCycleStage::Mutating, |_| {})
                        .await?
                }
                SkillEvolutionCycleStage::Mutating => {
                    self.resume_mutation(
                        current,
                        evidence.ok_or(SkillEvolutionCycleError::EvidenceRequired)?,
                    )
                    .await?
                }
                SkillEvolutionCycleStage::BuildingCandidates => {
                    self.resume_candidate_build(current).await?
                }
                SkillEvolutionCycleStage::Evaluating => self.resume_evaluation(current).await?,
                SkillEvolutionCycleStage::SelectingWinner => self.resume_selection(current).await?,
                SkillEvolutionCycleStage::Promoting => self.resume_promotion(current).await?,
                SkillEvolutionCycleStage::AwaitingHealth => return Ok(current),
                SkillEvolutionCycleStage::VerifyingHealth => {
                    self.resume_health_verification(current).await?
                }
                SkillEvolutionCycleStage::RollingBack => self.resume_rollback(current).await?,
                SkillEvolutionCycleStage::HealthVerified
                | SkillEvolutionCycleStage::RolledBack
                | SkillEvolutionCycleStage::Rejected
                | SkillEvolutionCycleStage::Failed => return Ok(current),
            };
        }
    }

    /// 幂等重算三份 Skill Proposal，并归档证据与 Outbox 精确绑定。
    async fn resume_mutation(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
        evidence: &MutationEvidence,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        if evidence.genome_digest != current.request.parent_genome_digest
            || evidence.episodes.is_empty()
        {
            return Err(SkillEvolutionCycleError::EvidenceBindingMismatch);
        }
        let parent = FileGenomeResolver::new(&self.evolution_root)
            .resolve(&GenomeSelector::Revision(
                current.request.parent_revision_id.clone(),
            ))
            .await?;
        let proposals = self
            .mutator
            .propose(
                &parent,
                evidence,
                current.request.mutation_generated_at_ms,
                &self.artifacts,
            )
            .await?;
        if proposals.len() != SKILL_EVOLUTION_CANDIDATE_COUNT {
            return Err(SkillEvolutionCycleError::CandidateCountMismatch);
        }
        let source_outbox_ids = evidence
            .episodes
            .iter()
            .map(|episode| episode.outbox_id.clone())
            .collect();
        self.advance(
            current,
            SkillEvolutionCycleStage::BuildingCandidates,
            |snapshot| {
                snapshot.source_issue_id = Some(evidence.issue_id.clone());
                snapshot.source_outbox_ids = source_outbox_ids;
                snapshot.proposals = proposals;
            },
        )
        .await
    }

    /// 逐个构建 Candidate；CAS 已提交但快照未落盘时由 Builder 幂等复读。
    async fn resume_candidate_build(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        if current.proposals.len() != SKILL_EVOLUTION_CANDIDATE_COUNT {
            return Err(SkillEvolutionCycleError::StateArtifactMismatch);
        }
        if current.candidates.len() == current.proposals.len() {
            return self
                .advance(current, SkillEvolutionCycleStage::Evaluating, |_| {})
                .await;
        }
        let index = current.candidates.len();
        let resolver = FileGenomeResolver::new(&self.evolution_root);
        let candidate = SkillCandidateBuilder::new(resolver.store(), &self.artifacts)
            .build_at(
                current.request.cycle_id.clone(),
                &current.proposals[index],
                current.request.candidate_created_at_ms,
            )
            .await?;
        self.advance(
            current,
            SkillEvolutionCycleStage::BuildingCandidates,
            |snapshot| snapshot.candidates.push(candidate),
        )
        .await
    }

    /// 逐个调用独立 Skill Evaluator，并在每份 Gate 回执后落盘。
    async fn resume_evaluation(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        if current.candidates.len() != SKILL_EVOLUTION_CANDIDATE_COUNT {
            return Err(SkillEvolutionCycleError::StateArtifactMismatch);
        }
        if current.gate_outcomes.len() == current.candidates.len() {
            return self
                .advance(current, SkillEvolutionCycleStage::SelectingWinner, |_| {})
                .await;
        }
        let candidate = &current.candidates[current.gate_outcomes.len()];
        let outcome = self
            .orchestrator
            .evaluate_and_promote(
                candidate,
                current.request.evaluated_at_ms,
                current.request.activated_at_ms,
            )
            .await?;
        self.validate_gate_outcome(
            candidate,
            &outcome,
            &FileGenomeResolver::new(&self.evolution_root),
        )
        .await?;
        self.advance(current, SkillEvolutionCycleStage::Evaluating, |snapshot| {
            snapshot.gate_outcomes.push(outcome);
        })
        .await
    }

    /// 从完整 Gate 回执中选择首个具有生产授权的 Candidate。
    async fn resume_selection(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        if current.gate_outcomes.len() != SKILL_EVOLUTION_CANDIDATE_COUNT {
            return Err(SkillEvolutionCycleError::StateArtifactMismatch);
        }
        let winner = current
            .gate_outcomes
            .iter()
            .find_map(|outcome| match outcome {
                SkillGateCycleOutcomeV1::Promoted(receipt) if receipt.permits_production() => {
                    Some(receipt.evaluated_candidate.candidate_id.clone())
                }
                _ => None,
            });
        let Some(winner) = winner else {
            return self
                .advance(current, SkillEvolutionCycleStage::Rejected, |snapshot| {
                    snapshot.disposition = Some(SkillEvolutionDispositionV1::Rejected);
                })
                .await;
        };
        self.advance(current, SkillEvolutionCycleStage::Promoting, |snapshot| {
            snapshot.winner = Some(winner);
        })
        .await
    }

    /// 使用确定性 Release ID 发布 Winner，并识别“已发布、快照未落盘”的恢复窗口。
    async fn resume_promotion(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        let winner = winner_receipt(&current)?.clone();
        let candidate = current
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == winner.evaluated_candidate.candidate_id)
            .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
        self.validate_gate_outcome(
            candidate,
            &SkillGateCycleOutcomeV1::Promoted(Box::new(winner.clone())),
            &FileGenomeResolver::new(&self.evolution_root),
        )
        .await?;
        let generation = current
            .request
            .expected_parent_generation
            .checked_add(1)
            .ok_or(SkillEvolutionCycleError::GenerationOverflow)?;
        let release_id = deterministic_release_id(
            "skillpromotion",
            &current.request.cycle_id,
            &winner.report_id,
        )?;
        let expected =
            StableGenomeRef::new(&current.request.lineage, &winner.active_genome, generation)?
                .bind_release(
                    release_id.clone(),
                    winner.report_id.clone(),
                    current.parent_stable.revision_id.clone(),
                    None,
                );
        let publisher = FileStableGenomePublisher::new(&self.evolution_root);
        let observed = publisher
            .resolver()
            .stable_reference(&current.request.lineage)
            .await?;
        let promoted = if observed == expected {
            expected
        } else if observed == current.parent_stable {
            publisher
                .publish_bound(
                    &current.parent_stable,
                    &winner.active_genome,
                    generation,
                    release_id,
                    winner.report_id.clone(),
                    None,
                )
                .await?
        } else {
            return Err(SkillEvolutionCycleError::StablePreconditionFailed);
        };
        self.advance(
            current,
            SkillEvolutionCycleStage::AwaitingHealth,
            |snapshot| snapshot.promotion = Some(promoted),
        )
        .await
    }

    /// 调用独立健康控制面并归档健康终态或回滚前置阶段。
    async fn resume_health_verification(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        let promoted = current
            .promotion
            .as_ref()
            .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
        let health = self.orchestrator.verify_health(promoted).await?;
        health.validate()?;
        let stage = if matches!(health, SkillHealthVerdictV1::Healthy { .. }) {
            SkillEvolutionCycleStage::HealthVerified
        } else {
            SkillEvolutionCycleStage::RollingBack
        };
        self.advance(current, stage, |snapshot| {
            if stage == SkillEvolutionCycleStage::HealthVerified {
                snapshot.disposition = Some(SkillEvolutionDispositionV1::HealthVerified);
            }
            snapshot.health = Some(health);
        })
        .await
    }

    /// 原子回滚 Parent，并识别“已回滚、快照未落盘”的恢复窗口。
    async fn resume_rollback(
        &self,
        current: SkillEvolutionCycleSnapshotV1,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
        if !matches!(current.health, Some(SkillHealthVerdictV1::Unhealthy { .. })) {
            return Err(SkillEvolutionCycleError::StateArtifactMismatch);
        }
        let promoted = current
            .promotion
            .as_ref()
            .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
        let winner = winner_receipt(&current)?;
        let parent = FileGenomeResolver::new(&self.evolution_root)
            .resolve(&GenomeSelector::Revision(
                current.request.parent_revision_id.clone(),
            ))
            .await?;
        let generation = promoted
            .generation
            .checked_add(1)
            .ok_or(SkillEvolutionCycleError::GenerationOverflow)?;
        let rollback_release = deterministic_release_id(
            "skillrollback",
            &current.request.cycle_id,
            &winner.report_id,
        )?;
        let promotion_release = promoted
            .release_id
            .clone()
            .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
        let expected = StableGenomeRef::new(&current.request.lineage, &parent, generation)?
            .bind_release(
                rollback_release.clone(),
                winner.report_id.clone(),
                promoted.revision_id.clone(),
                Some(promotion_release.clone()),
            );
        let publisher = FileStableGenomePublisher::new(&self.evolution_root);
        let observed = publisher
            .resolver()
            .stable_reference(&current.request.lineage)
            .await?;
        let rollback = if observed == expected {
            expected
        } else if observed == *promoted {
            publisher
                .publish_bound(
                    promoted,
                    &parent,
                    generation,
                    rollback_release,
                    winner.report_id.clone(),
                    Some(promotion_release),
                )
                .await?
        } else {
            return Err(SkillEvolutionCycleError::StablePreconditionFailed);
        };
        self.advance(current, SkillEvolutionCycleStage::RolledBack, |snapshot| {
            snapshot.rollback = Some(rollback);
            snapshot.disposition = Some(SkillEvolutionDispositionV1::RolledBack);
        })
        .await
    }

    /// 追加保留全部既有制品的下一阶段快照。
    async fn advance<F>(
        &self,
        previous: SkillEvolutionCycleSnapshotV1,
        stage: SkillEvolutionCycleStage,
        mutate: F,
    ) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError>
    where
        F: FnOnce(&mut SkillEvolutionCycleSnapshotV1),
    {
        let mut next = previous.clone();
        next.sequence = previous
            .sequence
            .checked_add(1)
            .ok_or(SkillEvolutionCycleError::SequenceOverflow)?;
        next.previous_digest = Some(FileSkillEvolutionCycleArchive::snapshot_digest(&previous)?);
        next.stage = stage;
        next.created_at_ms = now_ms()?;
        next.failure_code = None;
        mutate(&mut next);
        self.archive.append(&next).await?;
        Ok(next)
    }

    /// 尽力追加一个保留此前全部证据的失败终态。
    async fn append_failed(
        &self,
        request: &SkillEvolutionCycleRequestV1,
        code: &'static str,
    ) -> Result<(), SkillEvolutionCycleError> {
        let Some(previous) = self.archive.latest(&request.cycle_id).await? else {
            return Ok(());
        };
        if is_terminal_skill_stage(previous.stage) {
            return Ok(());
        }
        self.advance(previous, SkillEvolutionCycleStage::Failed, |snapshot| {
            snapshot.failure_code = Some(code.to_string());
        })
        .await?;
        Ok(())
    }

    /// 先提交兼容最终 Archive，再按受信 Issue、Episode 和 Outbox 三重绑定消费来源。
    async fn finalize_terminal(
        &self,
        snapshot: &SkillEvolutionCycleSnapshotV1,
    ) -> Result<(), SkillEvolutionCycleError> {
        if !is_consumable_skill_stage(snapshot.stage) {
            return Ok(());
        }
        let archive = archive_from_snapshot(snapshot)?;
        append_archive(&self.evolution_root, &archive).await?;
        let issue_id = snapshot
            .source_issue_id
            .as_ref()
            .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
        let episode_ids = snapshot
            .proposals
            .iter()
            .flat_map(|proposal| proposal.evidence_episode_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        let outbox = FileEvolutionOutbox::new(self.evolution_root.join("outbox"));
        for item in outbox.pending().await? {
            if snapshot.source_outbox_ids.contains(&item.outbox_id)
                && item.issue_id.as_ref() == Some(issue_id)
                && episode_ids.contains(&item.episode_id)
            {
                outbox.mark_consumed(&item.outbox_id).await?;
            }
        }
        Ok(())
    }

    /// 校验恢复调用携带的证据仍与已归档来源完全一致。
    fn validate_recovery_evidence(
        &self,
        snapshot: &SkillEvolutionCycleSnapshotV1,
        evidence: &MutationEvidence,
    ) -> Result<(), SkillEvolutionCycleError> {
        if evidence.genome_digest != snapshot.request.parent_genome_digest {
            return Err(SkillEvolutionCycleError::EvidenceBindingMismatch);
        }
        if snapshot.source_issue_id.is_none() {
            return Ok(());
        }
        let outbox_ids = evidence
            .episodes
            .iter()
            .map(|episode| episode.outbox_id.clone())
            .collect::<BTreeSet<_>>();
        if snapshot.source_issue_id.as_ref() != Some(&evidence.issue_id)
            || snapshot.source_outbox_ids != outbox_ids
        {
            return Err(SkillEvolutionCycleError::EvidenceBindingMismatch);
        }
        Ok(())
    }

    /// 复核请求 Parent 正是当前 Stable 的同一修订、摘要和代数。
    async fn validate_stable(
        &self,
        request: &SkillEvolutionCycleRequestV1,
        resolver: &FileGenomeResolver,
    ) -> Result<StableGenomeRef, SkillEvolutionCycleError> {
        let stable = resolver.stable_reference(&request.lineage).await?;
        if stable.revision_id != request.parent_revision_id
            || stable.digest != request.parent_genome_digest
            || stable.generation != request.expected_parent_generation
        {
            return Err(SkillEvolutionCycleError::StablePreconditionFailed);
        }
        let parent = resolver
            .resolve(&GenomeSelector::Stable(request.lineage.clone()))
            .await?;
        if parent.revision_id != request.parent_revision_id
            || parent.digest != request.parent_genome_digest
        {
            return Err(SkillEvolutionCycleError::StablePreconditionFailed);
        }
        Ok(stable)
    }

    /// 复核 Gate 结果、报告 CAS、Active Genome 与 Active Skill Set 的真实 Store 绑定。
    async fn validate_gate_outcome(
        &self,
        candidate: &SkillCandidateV1,
        outcome: &SkillGateCycleOutcomeV1,
        resolver: &FileGenomeResolver,
    ) -> Result<(), SkillEvolutionCycleError> {
        if outcome.candidate_id() != &candidate.candidate_id {
            return Err(SkillEvolutionCycleError::GateCandidateMismatch);
        }
        let report_artifact = outcome.report_artifact();
        let report_bytes = self
            .artifacts
            .get(&report_artifact.digest)
            .await?
            .ok_or(SkillEvolutionCycleError::GateReportNotFound)?;
        if report_artifact.media_type.trim().is_empty()
            || report_artifact.media_type.chars().any(char::is_control)
            || u64::try_from(report_bytes.len()).ok() != Some(report_artifact.size_bytes)
        {
            return Err(SkillEvolutionCycleError::GateReportBindingMismatch);
        }
        let SkillGateCycleOutcomeV1::Promoted(receipt) = outcome else {
            return Ok(());
        };
        if !valid_control_id(&receipt.authorization_evidence_id) {
            return Err(SkillEvolutionCycleError::InvalidAuthorizationEvidence);
        }
        let mut expected_evaluated = candidate.clone();
        expected_evaluated.evaluation_report_id = Some(receipt.report_id.clone());
        if receipt.evaluated_candidate != expected_evaluated {
            return Err(SkillEvolutionCycleError::GateCandidateMismatch);
        }
        receipt
            .active_genome
            .validate()
            .map_err(|error| SkillEvolutionCycleError::InvalidActiveGenome(error.to_string()))?;
        let stored_active = resolver
            .store()
            .get(&receipt.active_genome.revision_id)
            .await?
            .ok_or_else(|| {
                SkillEvolutionCycleError::ActiveGenomeNotFound(
                    receipt.active_genome.revision_id.clone(),
                )
            })?;
        if stored_active != receipt.active_genome
            || receipt.active_genome.metadata.parent.as_ref()
                != Some(&candidate.candidate_revision_id)
        {
            return Err(SkillEvolutionCycleError::ActiveGenomeStoreMismatch);
        }
        let candidate_revision = resolver
            .store()
            .get(&candidate.candidate_revision_id)
            .await?
            .ok_or_else(|| {
                SkillEvolutionCycleError::ActiveGenomeNotFound(
                    candidate.candidate_revision_id.clone(),
                )
            })?;
        if receipt.active_genome.genome.execution != candidate_revision.genome.execution {
            return Err(SkillEvolutionCycleError::ActiveGenomeExecutionMismatch);
        }
        let expected_active_skill_ids = candidate
            .candidate_artifact_digests
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let actual_active_skill_ids = receipt
            .active_skill_artifacts
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if actual_active_skill_ids != expected_active_skill_ids
            || candidate_revision.genome.skills.len() != receipt.active_genome.genome.skills.len()
        {
            return Err(SkillEvolutionCycleError::ActiveSkillSetMismatch);
        }
        let expected_surfaces = BTreeSet::from([MutationSurface::Skill]);
        let diff = verify_allowed_genome_diff(
            &candidate_revision,
            &receipt.active_genome,
            &expected_surfaces,
        )?;
        if diff.changed_surfaces != expected_surfaces {
            return Err(SkillEvolutionCycleError::InvalidActiveGenomeDiff);
        }

        let repository = SkillArtifactRepository::new(&self.artifacts);
        for candidate_skill in &candidate_revision.genome.skills {
            let active_skill = receipt
                .active_genome
                .genome
                .skills
                .iter()
                .find(|item| item.id == candidate_skill.id)
                .ok_or(SkillEvolutionCycleError::ActiveSkillSetMismatch)?;
            let skill_id = SkillId::new(candidate_skill.id.clone())
                .map_err(|_| SkillEvolutionCycleError::ActiveSkillSetMismatch)?;
            let expected_digest = receipt
                .active_skill_artifacts
                .get(&skill_id)
                .map(|reference| &reference.digest)
                .unwrap_or(&candidate_skill.content);
            if &active_skill.content != expected_digest {
                return Err(SkillEvolutionCycleError::ActiveSkillSetMismatch);
            }
        }
        for (skill_id, reference) in &receipt.active_skill_artifacts {
            let genome_ref = receipt
                .active_genome
                .genome
                .skills
                .iter()
                .find(|item| item.id == skill_id.as_str())
                .ok_or_else(|| SkillEvolutionCycleError::ActiveSkillMissing(skill_id.clone()))?;
            if genome_ref.content != reference.digest {
                return Err(SkillEvolutionCycleError::ActiveSkillDigestMismatch(
                    skill_id.clone(),
                ));
            }
            let artifact = repository.get(&reference.digest).await?;
            if artifact.skill_id != *skill_id
                || artifact.status_history.last().map(|item| item.status)
                    != Some(SkillStatusV1::Active)
            {
                return Err(SkillEvolutionCycleError::ActiveSkillNotActive(
                    skill_id.clone(),
                ));
            }
        }
        Ok(())
    }
}

/// 独立 Orchestrator 端口的稳定错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Skill Evolution Orchestrator 调用失败：{code}")]
pub struct SkillEvolutionOrchestratorError {
    code: String,
}

impl SkillEvolutionOrchestratorError {
    /// 从不含路径、用户正文或模型响应的稳定错误码创建错误。
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }

    /// 返回可安全写入控制面日志的错误码。
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Skill 生产 Cycle 的绑定、Store 与控制面错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillEvolutionCycleError {
    /// 请求字段或时间顺序无效。
    #[error("Skill Evolution Cycle 请求无效")]
    InvalidRequest,
    /// 相同 Cycle ID 已绑定另一请求。
    #[error("Skill Evolution Cycle ID 已绑定另一请求")]
    CycleRequestConflict,
    /// 指定 Cycle 不存在。
    #[error("Skill Evolution Cycle 不存在：{0}")]
    CycleNotFound(EvolutionCycleId),
    /// 当前阶段不能执行健康验证。
    #[error("Skill Evolution Cycle 当前阶段不能验证健康：{0:?}")]
    HealthNotReady(SkillEvolutionCycleStage),
    /// 旧格式最终 Archive 已完成，不能伪造此前不存在的阶段历史。
    #[error("Skill Evolution Cycle 已存在旧格式最终 Archive")]
    LegacyArchiveAlreadyComplete,
    /// 恢复阶段仍需要原始脱敏 MutationEvidence。
    #[error("Skill Evolution Cycle 恢复缺少 MutationEvidence")]
    EvidenceRequired,
    /// MutationEvidence 与请求或已归档来源不一致。
    #[error("Skill Evolution Cycle MutationEvidence 绑定不匹配")]
    EvidenceBindingMismatch,
    /// 已归档阶段制品的数量、顺序或身份不一致。
    #[error("Skill Evolution Cycle 已归档制品与阶段不一致")]
    StateArtifactMismatch,
    /// 快照序号无法递增。
    #[error("Skill Evolution Cycle 快照序号溢出")]
    SequenceOverflow,
    /// 当前 Stable 与请求 Parent 的修订、摘要或代数不一致。
    #[error("Skill Evolution Cycle Stable 前置条件失败")]
    StablePreconditionFailed,
    /// 固定候选数量没有得到保持。
    #[error("Skill Evolution Cycle 必须完整保留三份 Candidate")]
    CandidateCountMismatch,
    /// Gate 回执绑定了其他 Candidate。
    #[error("Skill Exit Gate 回执与 Candidate 不匹配")]
    GateCandidateMismatch,
    /// Gate 正式报告 CAS 不存在。
    #[error("Skill Exit Gate 正式报告未写入 Artifact CAS")]
    GateReportNotFound,
    /// Gate 正式报告引用的媒体类型或长度与 CAS 不一致。
    #[error("Skill Exit Gate 正式报告引用与 Artifact CAS 不一致")]
    GateReportBindingMismatch,
    /// 激活授权证据 ID 为空、过长或包含控制字符。
    #[error("Skill Exit Gate 激活授权证据无效")]
    InvalidAuthorizationEvidence,
    /// Active Genome 自身无效。
    #[error("Active Skill Set Genome 无效：{0}")]
    InvalidActiveGenome(String),
    /// Active Genome 未登记。
    #[error("Active Skill Set Genome 未登记：{0}")]
    ActiveGenomeNotFound(GenomeRevisionId),
    /// Active Genome 与不可变 Registry 不一致。
    #[error("Active Skill Set Genome 与 Registry 或 Candidate 绑定不一致")]
    ActiveGenomeStoreMismatch,
    /// Active Genome 改变了 Candidate 的执行策略。
    #[error("Active Skill Set Genome 不得改变 Serve 执行策略")]
    ActiveGenomeExecutionMismatch,
    /// Active Genome 的 Skill 集合或替换范围与 Candidate 回执不一致。
    #[error("Active Skill Set Genome 的替换范围与 Candidate 回执不一致")]
    ActiveSkillSetMismatch,
    /// Candidate 到 Active Genome 的 Diff 不只包含 Skill。
    #[error("Candidate 到 Active Genome 的 Diff 必须精确为 Skill")]
    InvalidActiveGenomeDiff,
    /// Active Genome 缺少回执声明的 Skill。
    #[error("Active Genome 缺少 Skill `{0}`")]
    ActiveSkillMissing(SkillId),
    /// Active Genome 的 Skill 摘要与回执不一致。
    #[error("Active Genome 的 Skill `{0}` 摘要与回执不一致")]
    ActiveSkillDigestMismatch(SkillId),
    /// Active Skill 制品终态不是 Active。
    #[error("Skill `{0}` 的生产制品终态不是 Active")]
    ActiveSkillNotActive(SkillId),
    /// 健康回执包含空或过大的标识。
    #[error("Skill 生产健康回执无效")]
    InvalidHealthVerdict,
    /// Stable 代数无法递增。
    #[error("Skill Stable 代数溢出")]
    GenerationOverflow,
    /// 无法构造确定性 Release ID。
    #[error("Skill Release ID 构造失败：{0}")]
    DeterministicReleaseId(String),
    /// Archive 结构与三候选闭环不一致。
    #[error("Skill Evolution Archive 结构无效")]
    InvalidArchive,
    /// Archive JSON 编码失败。
    #[error("序列化 Skill Evolution Archive 失败：{0}")]
    ArchiveSerialization(serde_json::Error),
    /// 阶段 Archive JSON 无法解析。
    #[error("Skill Evolution Cycle 阶段快照损坏：{path}: {source}")]
    ArchiveDeserialization {
        /// 损坏记录路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 阶段 Archive 超出固定字节上限。
    #[error("Skill Evolution Cycle 阶段快照过大：{actual} 字节，上限 {maximum} 字节")]
    ArchiveTooLarge {
        /// 实际字节数。
        actual: u64,
        /// 固定上限。
        maximum: u64,
    },
    /// 阶段 Archive 的摘要链、迁移或历史前缀无效。
    #[error("Skill Evolution Cycle 阶段历史无效")]
    ArchiveHistoryInvalid,
    /// 同一 Cycle 已存在不同归档正文。
    #[error("Skill Evolution Archive 已存在不同正文：{0}")]
    ArchiveConflict(PathBuf),
    /// Archive 路径不安全。
    #[error("Skill Evolution Archive 路径不安全：{0}")]
    UnsafeArchivePath(PathBuf),
    /// Archive 文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    ArchiveIo {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// Skill Mutator 失败。
    #[error(transparent)]
    Mutation(#[from] SkillMutationError),
    /// Candidate Builder 失败。
    #[error(transparent)]
    CandidateBuild(#[from] SkillCandidateBuildError),
    /// 独立 Orchestrator 失败。
    #[error(transparent)]
    Orchestrator(#[from] SkillEvolutionOrchestratorError),
    /// Artifact CAS 失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// Skill Repository 失败。
    #[error(transparent)]
    SkillRepository(#[from] SkillRepositoryError),
    /// Genome Registry 失败。
    #[error(transparent)]
    GenomeStore(#[from] GenomeStoreError),
    /// Genome Resolver 失败。
    #[error(transparent)]
    GenomeResolver(#[from] GenomeResolverError),
    /// Genome Diff 失败。
    #[error(transparent)]
    GenomeDiff(#[from] GenomeDiffError),
    /// Stable 发布失败。
    #[error(transparent)]
    GenomePromotion(#[from] GenomePromotionError),
    /// Evolution Outbox 失败。
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    /// 系统时钟不可用。
    #[error("Skill Evolution Cycle 系统时间不可用：{0}")]
    Clock(#[from] SystemTimeError),
    /// Unix 毫秒超过 `u64`。
    #[error("Skill Evolution Cycle 系统时间溢出")]
    ClockOverflow,
}

impl SkillEvolutionCycleError {
    /// 返回不包含路径、候选正文或底层错误细节的稳定 CLI 错误码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "skill_cycle_request_invalid",
            Self::CycleRequestConflict => "skill_cycle_request_conflict",
            Self::CycleNotFound(_) => "skill_cycle_not_found",
            Self::HealthNotReady(_) => "skill_health_not_ready",
            Self::LegacyArchiveAlreadyComplete => "skill_cycle_legacy_complete",
            Self::EvidenceRequired | Self::EvidenceBindingMismatch => "skill_evidence_invalid",
            Self::StateArtifactMismatch => "skill_cycle_state_invalid",
            Self::SequenceOverflow | Self::Clock(_) | Self::ClockOverflow => {
                "skill_cycle_time_invalid"
            }
            Self::StablePreconditionFailed => "skill_stable_precondition_failed",
            Self::CandidateCountMismatch => "skill_candidate_count_mismatch",
            Self::GateCandidateMismatch
            | Self::GateReportNotFound
            | Self::GateReportBindingMismatch
            | Self::InvalidAuthorizationEvidence
            | Self::InvalidActiveGenome(_)
            | Self::ActiveGenomeNotFound(_)
            | Self::ActiveGenomeStoreMismatch
            | Self::ActiveGenomeExecutionMismatch
            | Self::ActiveSkillSetMismatch
            | Self::InvalidActiveGenomeDiff
            | Self::ActiveSkillMissing(_)
            | Self::ActiveSkillDigestMismatch(_)
            | Self::ActiveSkillNotActive(_) => "skill_gate_receipt_invalid",
            Self::InvalidHealthVerdict => "skill_health_receipt_invalid",
            Self::GenerationOverflow | Self::DeterministicReleaseId(_) => {
                "skill_release_identity_invalid"
            }
            Self::InvalidArchive
            | Self::ArchiveSerialization(_)
            | Self::ArchiveDeserialization { .. }
            | Self::ArchiveTooLarge { .. }
            | Self::ArchiveHistoryInvalid
            | Self::ArchiveConflict(_)
            | Self::UnsafeArchivePath(_)
            | Self::ArchiveIo { .. } => "skill_archive_failed",
            Self::Mutation(_) => "skill_mutation_failed",
            Self::CandidateBuild(_) => "skill_candidate_build_failed",
            Self::Orchestrator(_) => "skill_evaluator_failed",
            Self::Artifact(_) | Self::SkillRepository(_) => "skill_artifact_store_failed",
            Self::GenomeStore(_) | Self::GenomeResolver(_) | Self::GenomeDiff(_) => {
                "skill_genome_store_failed"
            }
            Self::GenomePromotion(_) => "skill_stable_publish_failed",
            Self::Outbox(_) => "skill_outbox_consume_failed",
        }
    }

    /// 判断错误是否适合追加确定性失败终态。
    fn should_close_cycle(&self) -> bool {
        matches!(
            self,
            Self::StablePreconditionFailed
                | Self::CandidateCountMismatch
                | Self::EvidenceBindingMismatch
                | Self::StateArtifactMismatch
        )
    }
}

/// 校验最终 Archive 保留三份提案、候选、Gate 结果及合法发布分支。
fn validate_archive(archive: &SkillEvolutionArchiveV1) -> Result<(), SkillEvolutionCycleError> {
    if archive.schema_version != SKILL_EVOLUTION_ARCHIVE_SCHEMA_VERSION
        || archive.proposals.len() != SKILL_EVOLUTION_CANDIDATE_COUNT
        || archive.candidates.len() != SKILL_EVOLUTION_CANDIDATE_COUNT
        || archive.gate_outcomes.len() != SKILL_EVOLUTION_CANDIDATE_COUNT
        || archive
            .candidates
            .iter()
            .zip(&archive.gate_outcomes)
            .any(|(candidate, outcome)| candidate.candidate_id != *outcome.candidate_id())
    {
        return Err(SkillEvolutionCycleError::InvalidArchive);
    }
    let valid_branch = match archive.disposition {
        SkillEvolutionDispositionV1::Rejected => {
            archive.winner.is_none()
                && archive.promotion.is_none()
                && archive.health.is_none()
                && archive.rollback.is_none()
        }
        SkillEvolutionDispositionV1::HealthVerified => {
            archive.winner.is_some()
                && archive.promotion.is_some()
                && matches!(archive.health, Some(SkillHealthVerdictV1::Healthy { .. }))
                && archive.rollback.is_none()
        }
        SkillEvolutionDispositionV1::RolledBack => {
            archive.winner.is_some()
                && archive.promotion.is_some()
                && matches!(archive.health, Some(SkillHealthVerdictV1::Unhealthy { .. }))
                && archive.rollback.is_some()
        }
    };
    if !valid_branch {
        return Err(SkillEvolutionCycleError::InvalidArchive);
    }
    Ok(())
}

/// 从可信终态快照构造字节兼容的旧版最终 Archive。
fn archive_from_snapshot(
    snapshot: &SkillEvolutionCycleSnapshotV1,
) -> Result<SkillEvolutionArchiveV1, SkillEvolutionCycleError> {
    snapshot.validate()?;
    let disposition = snapshot
        .disposition
        .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
    let archive = SkillEvolutionArchiveV1 {
        schema_version: SKILL_EVOLUTION_ARCHIVE_SCHEMA_VERSION,
        request: snapshot.request.clone(),
        proposals: snapshot.proposals.clone(),
        candidates: snapshot.candidates.clone(),
        gate_outcomes: snapshot.gate_outcomes.clone(),
        winner: snapshot.winner.clone(),
        promotion: snapshot.promotion.clone(),
        health: snapshot.health.clone(),
        rollback: snapshot.rollback.clone(),
        disposition,
    };
    validate_archive(&archive)?;
    Ok(archive)
}

/// 返回当前快照中获得生产授权的 Winner Gate 回执。
fn winner_receipt(
    snapshot: &SkillEvolutionCycleSnapshotV1,
) -> Result<&SkillGatePromotionV1, SkillEvolutionCycleError> {
    let winner = snapshot
        .winner
        .as_ref()
        .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)?;
    snapshot
        .gate_outcomes
        .iter()
        .find_map(|outcome| match outcome {
            SkillGateCycleOutcomeV1::Promoted(receipt)
                if receipt.permits_production()
                    && receipt.evaluated_candidate.candidate_id == *winner =>
            {
                Some(receipt.as_ref())
            }
            _ => None,
        })
        .ok_or(SkillEvolutionCycleError::StateArtifactMismatch)
}

/// 校验并连接相邻 Skill Cycle 快照。
fn validate_next_skill_snapshot(
    previous: Option<&SkillEvolutionCycleSnapshotV1>,
    next: &SkillEvolutionCycleSnapshotV1,
) -> Result<(), SkillEvolutionCycleError> {
    next.validate()?;
    match previous {
        None => {
            if next.sequence != 0
                || next.previous_digest.is_some()
                || next.stage != SkillEvolutionCycleStage::Requested
            {
                return Err(SkillEvolutionCycleError::ArchiveHistoryInvalid);
            }
        }
        Some(previous) => {
            if previous.request != next.request
                || previous.parent_stable != next.parent_stable
                || previous.sequence.checked_add(1) != Some(next.sequence)
                || next.previous_digest.as_ref()
                    != Some(&FileSkillEvolutionCycleArchive::snapshot_digest(previous)?)
                || !allowed_skill_transition(previous.stage, next.stage)
                || !next.proposals.starts_with(&previous.proposals)
                || !next.candidates.starts_with(&previous.candidates)
                || !next.gate_outcomes.starts_with(&previous.gate_outcomes)
                || (previous.source_issue_id.is_some()
                    && previous.source_issue_id != next.source_issue_id)
                || (!previous.source_outbox_ids.is_empty()
                    && previous.source_outbox_ids != next.source_outbox_ids)
                || (previous.winner.is_some() && previous.winner != next.winner)
                || (previous.promotion.is_some() && previous.promotion != next.promotion)
                || (previous.health.is_some() && previous.health != next.health)
                || (previous.rollback.is_some() && previous.rollback != next.rollback)
                || (previous.disposition.is_some() && previous.disposition != next.disposition)
            {
                return Err(SkillEvolutionCycleError::ArchiveHistoryInvalid);
            }
        }
    }
    Ok(())
}

/// 判断相邻阶段是否属于固定 Skill Cycle 状态机。
fn allowed_skill_transition(from: SkillEvolutionCycleStage, to: SkillEvolutionCycleStage) -> bool {
    if is_terminal_skill_stage(from) {
        return false;
    }
    to == SkillEvolutionCycleStage::Failed
        || matches!(
            (from, to),
            (
                SkillEvolutionCycleStage::Requested,
                SkillEvolutionCycleStage::Mutating
            ) | (
                SkillEvolutionCycleStage::Mutating,
                SkillEvolutionCycleStage::BuildingCandidates
            ) | (
                SkillEvolutionCycleStage::BuildingCandidates,
                SkillEvolutionCycleStage::BuildingCandidates
            ) | (
                SkillEvolutionCycleStage::BuildingCandidates,
                SkillEvolutionCycleStage::Evaluating
            ) | (
                SkillEvolutionCycleStage::Evaluating,
                SkillEvolutionCycleStage::Evaluating
            ) | (
                SkillEvolutionCycleStage::Evaluating,
                SkillEvolutionCycleStage::SelectingWinner
            ) | (
                SkillEvolutionCycleStage::SelectingWinner,
                SkillEvolutionCycleStage::Promoting
            ) | (
                SkillEvolutionCycleStage::SelectingWinner,
                SkillEvolutionCycleStage::Rejected
            ) | (
                SkillEvolutionCycleStage::Promoting,
                SkillEvolutionCycleStage::AwaitingHealth
            ) | (
                SkillEvolutionCycleStage::AwaitingHealth,
                SkillEvolutionCycleStage::VerifyingHealth
            ) | (
                SkillEvolutionCycleStage::VerifyingHealth,
                SkillEvolutionCycleStage::HealthVerified
            ) | (
                SkillEvolutionCycleStage::VerifyingHealth,
                SkillEvolutionCycleStage::RollingBack
            ) | (
                SkillEvolutionCycleStage::RollingBack,
                SkillEvolutionCycleStage::RolledBack
            )
        )
}

/// 判断阶段是否已经关闭，不得再追加后续状态。
fn is_terminal_skill_stage(stage: SkillEvolutionCycleStage) -> bool {
    matches!(
        stage,
        SkillEvolutionCycleStage::HealthVerified
            | SkillEvolutionCycleStage::RolledBack
            | SkillEvolutionCycleStage::Rejected
            | SkillEvolutionCycleStage::Failed
    )
}

/// 判断终态是否具备完整最终 Archive 并允许消费来源 Outbox。
fn is_consumable_skill_stage(stage: SkillEvolutionCycleStage) -> bool {
    matches!(
        stage,
        SkillEvolutionCycleStage::HealthVerified
            | SkillEvolutionCycleStage::RolledBack
            | SkillEvolutionCycleStage::Rejected
    )
}

/// 读取既有字节兼容最终 Archive；不存在时返回 `None`。
async fn read_existing_archive(
    evolution_root: &Path,
    cycle_id: &EvolutionCycleId,
) -> Result<Option<SkillEvolutionArchiveV1>, SkillEvolutionCycleError> {
    let path = final_archive_path(evolution_root, cycle_id);
    let metadata = match fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(archive_io_error("检查 Skill Cycle 最终归档", &path, source)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillEvolutionCycleError::UnsafeArchivePath(path));
    }
    let bytes = fs::read(&path)
        .await
        .map_err(|source| archive_io_error("读取 Skill Cycle 最终归档", &path, source))?;
    let archive = serde_json::from_slice(&bytes).map_err(|source| {
        SkillEvolutionCycleError::ArchiveDeserialization {
            path: path.clone(),
            source,
        }
    })?;
    Ok(Some(archive))
}

/// 返回旧版最终 Archive 的稳定路径。
fn final_archive_path(evolution_root: &Path, cycle_id: &EvolutionCycleId) -> PathBuf {
    evolution_root
        .join("skill-cycle-archive")
        .join(format!("{cycle_id}.json"))
}

/// 读取并校验一份阶段快照普通文件。
async fn read_skill_snapshot(
    path: &Path,
) -> Result<SkillEvolutionCycleSnapshotV1, SkillEvolutionCycleError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| archive_io_error("检查 Skill Cycle 阶段快照", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillEvolutionCycleError::UnsafeArchivePath(
            path.to_path_buf(),
        ));
    }
    enforce_skill_snapshot_size(metadata.len())?;
    let bytes = fs::read(path)
        .await
        .map_err(|source| archive_io_error("读取 Skill Cycle 阶段快照", path, source))?;
    enforce_skill_snapshot_size(bytes.len() as u64)?;
    let snapshot = serde_json::from_slice(&bytes).map_err(|source| {
        SkillEvolutionCycleError::ArchiveDeserialization {
            path: path.to_path_buf(),
            source,
        }
    })?;
    SkillEvolutionCycleSnapshotV1::validate(&snapshot)?;
    Ok(snapshot)
}

/// 以临时文件与硬链接提交一份不可变普通文件。
async fn append_new_file(path: &Path, bytes: &[u8]) -> Result<(), SkillEvolutionCycleError> {
    let root = path
        .parent()
        .ok_or_else(|| SkillEvolutionCycleError::UnsafeArchivePath(path.to_path_buf()))?;
    let temporary = root.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|source| archive_io_error("创建 Skill Cycle 临时快照", &temporary, source))?;
        file.write_all(bytes)
            .await
            .map_err(|source| archive_io_error("写入 Skill Cycle 临时快照", &temporary, source))?;
        file.sync_all()
            .await
            .map_err(|source| archive_io_error("同步 Skill Cycle 临时快照", &temporary, source))?;
        drop(file);
        fs::hard_link(&temporary, path)
            .await
            .map_err(|source| archive_io_error("提交 Skill Cycle 阶段快照", path, source))
    }
    .await;
    let _ = fs::remove_file(&temporary).await;
    result
}

/// 校验阶段快照字节数不超过固定上限。
fn enforce_skill_snapshot_size(actual: u64) -> Result<(), SkillEvolutionCycleError> {
    if actual > MAX_SKILL_CYCLE_SNAPSHOT_BYTES {
        return Err(SkillEvolutionCycleError::ArchiveTooLarge {
            actual,
            maximum: MAX_SKILL_CYCLE_SNAPSHOT_BYTES,
        });
    }
    Ok(())
}

/// 计算任意 Skill Cycle 制品的强类型 SHA-256 摘要。
fn skill_digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, SkillEvolutionCycleError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|_| SkillEvolutionCycleError::ArchiveHistoryInvalid)
}

/// 返回当前受信 Unix 毫秒。
fn now_ms() -> Result<u64, SkillEvolutionCycleError> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .map_err(|_| SkillEvolutionCycleError::ClockOverflow)
}

/// 校验可写入归档的控制面证据 ID，避免正文或无界数据进入 Cycle。
fn valid_control_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

/// 把完整 Cycle 以确定性文件名只追加到独立 Archive。
async fn append_archive(
    evolution_root: &Path,
    archive: &SkillEvolutionArchiveV1,
) -> Result<PathBuf, SkillEvolutionCycleError> {
    let root = evolution_root.join("skill-cycle-archive");
    ensure_safe_archive_directory(&root).await?;
    let path = root.join(format!("{}.json", archive.request.cycle_id));
    let bytes = serde_json::to_vec_pretty(archive)
        .map_err(SkillEvolutionCycleError::ArchiveSerialization)?;
    if let Ok(metadata) = fs::symlink_metadata(&path).await {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SkillEvolutionCycleError::UnsafeArchivePath(path));
        }
        let existing = fs::read(&path)
            .await
            .map_err(|source| archive_io_error("读取已有 Skill Cycle 归档", &path, source))?;
        if existing == bytes {
            return Ok(path);
        }
        return Err(SkillEvolutionCycleError::ArchiveConflict(path));
    }

    let temporary = root.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|source| archive_io_error("创建 Skill Cycle 临时归档", &temporary, source))?;
        file.write_all(&bytes)
            .await
            .map_err(|source| archive_io_error("写入 Skill Cycle 临时归档", &temporary, source))?;
        file.sync_all()
            .await
            .map_err(|source| archive_io_error("同步 Skill Cycle 临时归档", &temporary, source))?;
        drop(file);
        fs::hard_link(&temporary, &path)
            .await
            .map_err(|source| archive_io_error("提交 Skill Cycle 归档", &path, source))
    }
    .await;
    let _ = fs::remove_file(&temporary).await;
    result?;
    Ok(path)
}

/// 创建并校验 Archive 根目录，拒绝符号链接替换。
async fn ensure_safe_archive_directory(path: &Path) -> Result<(), SkillEvolutionCycleError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| archive_io_error("创建 Skill Cycle 归档目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| archive_io_error("检查 Skill Cycle 归档目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SkillEvolutionCycleError::UnsafeArchivePath(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

/// 生成与 Cycle、报告绑定且可幂等重算的 Release ID。
fn deterministic_release_id(
    namespace: &str,
    cycle_id: &EvolutionCycleId,
    report_id: &EvaluationReportId,
) -> Result<ReleaseId, SkillEvolutionCycleError> {
    let digest = Sha256::digest(format!("{namespace}:{cycle_id}:{report_id}").as_bytes());
    ReleaseId::new(format!("rel_{:x}", digest))
        .map_err(|error| SkillEvolutionCycleError::DeterministicReleaseId(error.to_string()))
}

/// 构造带路径上下文的 Archive I/O 错误。
fn archive_io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> SkillEvolutionCycleError {
    SkillEvolutionCycleError::ArchiveIo {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}
