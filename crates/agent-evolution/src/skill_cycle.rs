//! Skill 自进化生产闭环编排。
//!
//! 本模块拥有 Mutator、Candidate Builder、Archive 与 Stable 发布顺序；独立 Evaluator
//! 通过 [`SkillEvolutionOrchestrator`] 端口返回 Skill Exit Gate 的 Q→E→A 可信结果，避免
//! `agent-evolution` 反向依赖 `agent-evaluation`。Hidden Dataset、Verifier 与激活授权仍留在
//! 独立 Evaluator，Evolver 只接受脱敏回执并在发布前强制检查生产授权。

use crate::{
    verify_allowed_genome_diff, ArtifactStore, ArtifactStoreError, BoundedSkillMutator,
    FileArtifactStore, FileGenomeResolver, FileStableGenomePublisher, GenomeDiffError,
    GenomePromotionError, GenomeResolver, GenomeResolverError, GenomeSelector, GenomeStore,
    GenomeStoreError, MutationEvidence, SkillArtifactRepository, SkillCandidateBuildError,
    SkillCandidateBuilder, SkillMutationError, SkillMutationGenerator, SkillRepositoryError,
    StableGenomeRef,
};
use agent_evolution_protocol::{
    ArtifactRef, CandidateId, EvaluationReportId, EvolutionCycleId, GenomeDigest, GenomeRevision,
    GenomeRevisionId, MutationSurface, ReleaseId, SkillCandidateV1, SkillId,
    SkillMutationProposalV1, SkillStatusV1,
};
use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// Skill Cycle 归档结构版本。
pub const SKILL_EVOLUTION_ARCHIVE_SCHEMA_VERSION: u32 = 1;
/// Skill Cycle 固定保留的候选数量。
pub const SKILL_EVOLUTION_CANDIDATE_COUNT: usize = 3;

/// 一轮 Skill 生产 Cycle 的可信输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionDispositionV1 {
    /// 所有 Candidate 均被 Gate 拒绝或缺少生产授权。
    Rejected,
    /// Production Stable 已发布并通过健康检查。
    HealthVerified,
    /// Production Stable 发布后健康失败，已原子回滚 Parent。
    RolledBack,
}

/// 一轮 Skill Cycle 的不可变全量归档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
            evolution_root,
            mutator: BoundedSkillMutator::m7(generator),
            orchestrator,
        }
    }

    /// 返回 Runner 使用的 Artifact CAS，供独立 Gate 适配器共享同一 Store。
    pub fn artifacts(&self) -> &FileArtifactStore {
        &self.artifacts
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
        let publisher = FileStableGenomePublisher::new(&self.evolution_root);
        let expected_stable = self.validate_stable(request, publisher.resolver()).await?;
        let parent = publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(
                request.parent_revision_id.clone(),
            ))
            .await?;
        let proposals = self
            .mutator
            .propose(
                &parent,
                evidence,
                request.mutation_generated_at_ms,
                &self.artifacts,
            )
            .await?;
        if proposals.len() != SKILL_EVOLUTION_CANDIDATE_COUNT {
            return Err(SkillEvolutionCycleError::CandidateCountMismatch);
        }

        let builder = SkillCandidateBuilder::new(publisher.resolver().store(), &self.artifacts);
        let mut candidates = Vec::with_capacity(proposals.len());
        for proposal in &proposals {
            candidates.push(
                builder
                    .build_at(
                        request.cycle_id.clone(),
                        proposal,
                        request.candidate_created_at_ms,
                    )
                    .await?,
            );
        }

        let mut gate_outcomes = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            let outcome = self
                .orchestrator
                .evaluate_and_promote(candidate, request.evaluated_at_ms, request.activated_at_ms)
                .await?;
            self.validate_gate_outcome(candidate, &outcome, publisher.resolver())
                .await?;
            gate_outcomes.push(outcome);
        }

        let winner = gate_outcomes.iter().find_map(|outcome| match outcome {
            SkillGateCycleOutcomeV1::Promoted(receipt) if receipt.permits_production() => {
                Some(receipt)
            }
            _ => None,
        });
        let (winner_id, promotion, health, rollback, disposition) = if let Some(winner) = winner {
            let promotion_generation = request
                .expected_parent_generation
                .checked_add(1)
                .ok_or(SkillEvolutionCycleError::GenerationOverflow)?;
            let promotion_release =
                deterministic_release_id("skillpromotion", &request.cycle_id, &winner.report_id)?;
            let promoted = publisher
                .publish_bound(
                    &expected_stable,
                    &winner.active_genome,
                    promotion_generation,
                    promotion_release.clone(),
                    winner.report_id.clone(),
                    None,
                )
                .await?;
            let health = self.orchestrator.verify_health(&promoted).await?;
            health.validate()?;
            match &health {
                SkillHealthVerdictV1::Healthy { .. } => (
                    Some(winner.evaluated_candidate.candidate_id.clone()),
                    Some(promoted),
                    Some(health),
                    None,
                    SkillEvolutionDispositionV1::HealthVerified,
                ),
                SkillHealthVerdictV1::Unhealthy { .. } => {
                    let rollback_generation = promotion_generation
                        .checked_add(1)
                        .ok_or(SkillEvolutionCycleError::GenerationOverflow)?;
                    let rollback_release = deterministic_release_id(
                        "skillrollback",
                        &request.cycle_id,
                        &winner.report_id,
                    )?;
                    let rolled_back = publisher
                        .publish_bound(
                            &promoted,
                            &parent,
                            rollback_generation,
                            rollback_release,
                            winner.report_id.clone(),
                            Some(promotion_release),
                        )
                        .await?;
                    (
                        Some(winner.evaluated_candidate.candidate_id.clone()),
                        Some(promoted),
                        Some(health),
                        Some(rolled_back),
                        SkillEvolutionDispositionV1::RolledBack,
                    )
                }
            }
        } else {
            (
                None,
                None,
                None,
                None,
                SkillEvolutionDispositionV1::Rejected,
            )
        };

        let archive = SkillEvolutionArchiveV1 {
            schema_version: SKILL_EVOLUTION_ARCHIVE_SCHEMA_VERSION,
            request: request.clone(),
            proposals,
            candidates,
            gate_outcomes,
            winner: winner_id,
            promotion,
            health,
            rollback,
            disposition,
        };
        validate_archive(&archive)?;
        let archive_path = append_archive(&self.evolution_root, &archive).await?;
        Ok(SkillEvolutionCycleResultV1 {
            archive,
            archive_path,
        })
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
