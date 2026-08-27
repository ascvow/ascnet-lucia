//! M7 Skill Commit Gate、生命周期 Promotion 与后续运行证明编排。
//!
//! 本模块只消费 Store 中的真实 Genome、独立 Gate 结果和 Recorder/Host 产生的可信绑定。
//! Gate 通过前不会写入 Evaluated/Active；Promotion 后会创建引用新 Active 摘要的后续
//! Genome Revision，原 Quarantined Candidate Revision 与 CAS 制品保持不可变。

use crate::{evaluate_skill_candidate, SkillGateError, TrustedSkillGateResultV1};
use agent_evolution::{
    collect_trusted_skill_usage_bindings, verify_allowed_genome_diff, ArtifactStore,
    ArtifactStoreError, FileArtifactStore, FileGenomeStore, FileSkillStatusStore, GenomeDiffError,
    GenomeStore, GenomeStoreError, SkillArtifactRepository, SkillRepositoryError,
    SkillUsageBindingError,
};
use agent_evolution_protocol::{
    ArtifactRef, EpisodeId, EvaluationReportId, EventId, GateDecision, GenomeMetadata,
    GenomeRevision, GenomeRevisionError, GenomeRevisionId, MutationSurface, SkillArtifactV1,
    SkillCandidateV1, SkillId, SkillOperationV1, SkillStatusTransitionV1, SkillStatusV1,
    SkillUsageObservationV1, TrustedSkillUsageBindingV1,
};
use agent_tool::ExecutionProfile;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// M7 Skill EvaluationReport 在 Artifact CAS 中的媒体类型。
pub const SKILL_EVALUATION_REPORT_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.skill-evaluation-report.v2+json";

/// 可信控制面授予的 Skill 激活授权。
///
/// 本类型不实现反序列化，不能由 Guest、模型输出或普通 EvaluationReport 伪造。临时本地
/// Gate 验证必须显式选择 `LocalEvaluation`；生产控制面只能使用人工批准或 Canary 通过
/// 模式，并把对应证据 ID 写入 Promotion 收据。Gate Pass 本身不构成激活授权。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillActivationAuthorizationV1 {
    /// 仅用于隔离临时 Store 的本地端到端验证，不得发布为生产 stable 引用。
    LocalEvaluation,
    /// 人工或外部策略控制面批准。
    Approved {
        /// 可审计的非空批准记录 ID。
        approval_id: String,
    },
    /// 受信 Canary Gate 已通过。
    CanaryPassed {
        /// Canary 评测或健康报告 ID。
        canary_report_id: EvaluationReportId,
    },
}

impl SkillActivationAuthorizationV1 {
    /// 创建仅允许隔离本地验证的显式授权。
    pub const fn local_evaluation() -> Self {
        Self::LocalEvaluation
    }

    /// 创建生产人工批准授权。
    ///
    /// # Errors
    ///
    /// `approval_id` 为空或只有空白时返回 [`SkillExitGateError::InvalidApprovalId`]。
    pub fn approved(approval_id: impl Into<String>) -> Result<Self, SkillExitGateError> {
        let approval_id = approval_id.into();
        if approval_id.trim().is_empty() {
            return Err(SkillExitGateError::InvalidApprovalId);
        }
        Ok(Self::Approved { approval_id })
    }

    /// 创建受信 Canary 通过授权。
    pub const fn canary_passed(canary_report_id: EvaluationReportId) -> Self {
        Self::CanaryPassed { canary_report_id }
    }

    /// 判断授权是否允许生产 stable 发布。
    pub const fn permits_production(&self) -> bool {
        matches!(self, Self::Approved { .. } | Self::CanaryPassed { .. })
    }
}

/// Skill Exit Gate 的一次可信输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillExitGateOutcomeV1 {
    /// Gate 拒绝 Candidate；报告已归档，但没有状态或 Genome Promotion 副作用。
    Rejected {
        /// 独立 Gate 的可信结果。
        gate: Box<TrustedSkillGateResultV1>,
        /// 报告在不可变 Artifact CAS 中的引用。
        report_artifact: ArtifactRef,
    },
    /// Gate 通过且状态链与后续 Active Genome 已提交。
    Promoted(Box<SkillPromotionReceiptV1>),
}

/// Gate 通过后提交的 Skill Promotion 收据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPromotionReceiptV1 {
    /// 独立 Gate 的可信 Pass 结果。
    pub gate: TrustedSkillGateResultV1,
    /// 绑定正式报告 ID 的 Candidate 快照；原 Candidate Revision 不会被改写。
    pub evaluated_candidate: SkillCandidateV1,
    /// 报告在不可变 Artifact CAS 中的引用。
    pub report_artifact: ArtifactRef,
    /// Gate 之外的显式激活授权；生产必须为 Approved 或 CanaryPassed。
    pub activation_authorization: SkillActivationAuthorizationV1,
    /// 每个候选 Skill 的 Active 状态制品引用。
    pub active_skill_artifacts: BTreeMap<SkillId, ArtifactRef>,
    /// 引用 Active 摘要的后续 Serve Genome Revision。
    pub active_genome: GenomeRevision,
}

/// Promotion 后由新运行和 Serve Binder 共同形成的最终证明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPostPromotionProofV1 {
    /// 实际运行的 Active Genome Revision。
    pub active_revision_id: GenomeRevisionId,
    /// 产生真实 Skill 事件的新 Episode。
    pub episode_id: EpisodeId,
    /// 从真实 Episode 提取的可信使用绑定。
    pub bindings: BTreeMap<EventId, TrustedSkillUsageBindingV1>,
}

/// 绑定真实 Store 的 M7 Skill Exit Gate 控制面。
pub struct SkillExitGate<'a> {
    genomes: &'a FileGenomeStore,
    artifacts: &'a FileArtifactStore,
    statuses: FileSkillStatusStore<'a>,
}

impl<'a> SkillExitGate<'a> {
    /// 创建一个使用独立 Skill 状态根目录的 Exit Gate。
    ///
    /// 构造本身不访问文件系统；状态目录只会在成功 Promotion 时按需创建。
    pub fn new(
        genomes: &'a FileGenomeStore,
        artifacts: &'a FileArtifactStore,
        status_root: impl AsRef<Path>,
    ) -> Self {
        Self {
            genomes,
            artifacts,
            statuses: FileSkillStatusStore::new(status_root.as_ref().to_path_buf(), artifacts),
        }
    }

    /// 从 Store 复读 Candidate，运行独立 Gate，并在 Pass 后提交 Q→E→A 与 Active Genome。
    ///
    /// `trusted_candidate` 仍引用 Builder 产生的 Quarantined Candidate Revision。评测绑定
    /// 必须也固定该 Revision；本方法不会接受为了 Evaluation 临时改写 revision/digest 的
    /// Genome。报告无论 Pass/Reject 都写入 CAS，Reject 不产生任何生命周期或 Genome 写入。
    ///
    /// Pass 路径逐项先登记初始 Quarantined 制品，再只追加 Evaluated 与 Active 状态制品；
    /// 最后从原 Candidate Genome 克隆并仅替换对应 Skill 摘要，登记确定性的后续 Revision。
    /// 全部写入支持崩溃后的幂等重试，已存在内容只有逐字一致时才视为成功。
    ///
    /// # Errors
    ///
    /// Parent/Candidate 不存在或错绑、时间不递增、Gate/报告失败、Candidate 含不能激活的
    /// Deprecate/Delete、状态链分叉、Active Genome 不是唯一 Skill Diff，或任一 Store
    /// 操作失败时返回 [`SkillExitGateError`]。
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate_and_promote(
        &self,
        trusted_candidate: &SkillCandidateV1,
        activation_authorization: SkillActivationAuthorizationV1,
        observations: &[SkillUsageObservationV1],
        trusted_usage_bindings: &BTreeMap<EventId, TrustedSkillUsageBindingV1>,
        report_id: EvaluationReportId,
        evaluated_at_ms: u64,
        activated_at_ms: u64,
    ) -> Result<SkillExitGateOutcomeV1, SkillExitGateError> {
        if evaluated_at_ms == 0 || activated_at_ms <= evaluated_at_ms {
            return Err(SkillExitGateError::InvalidPromotionTime {
                evaluated_at_ms,
                activated_at_ms,
            });
        }
        let parent = self
            .genomes
            .get(&trusted_candidate.parent_revision_id)
            .await?
            .ok_or_else(|| {
                SkillExitGateError::ParentNotFound(trusted_candidate.parent_revision_id.clone())
            })?;
        let candidate_revision = self
            .genomes
            .get(&trusted_candidate.candidate_revision_id)
            .await?
            .ok_or_else(|| {
                SkillExitGateError::CandidateNotFound(
                    trusted_candidate.candidate_revision_id.clone(),
                )
            })?;
        if parent.digest != trusted_candidate.parent_genome_digest
            || candidate_revision.digest != trusted_candidate.candidate_genome_digest
        {
            return Err(SkillExitGateError::CandidateStoreBindingMismatch);
        }
        if trusted_candidate.evaluation_report_id.is_some() {
            return Err(SkillExitGateError::CandidateAlreadyEvaluated);
        }

        let gate = evaluate_skill_candidate(
            &parent,
            &candidate_revision,
            trusted_candidate,
            observations,
            trusted_usage_bindings,
            report_id,
            evaluated_at_ms,
        )?;
        let report_bytes = serde_json::to_vec(&gate.report)
            .map_err(SkillExitGateError::SerializeEvaluationReport)?;
        let report_artifact = self
            .artifacts
            .put(SKILL_EVALUATION_REPORT_MEDIA_TYPE, &report_bytes)
            .await?;
        if gate.report.decision != GateDecision::Pass {
            return Ok(SkillExitGateOutcomeV1::Rejected {
                gate: Box::new(gate),
                report_artifact,
            });
        }

        let skill_repository = SkillArtifactRepository::new(self.artifacts);
        let mut active_skill_artifacts = BTreeMap::new();
        for (skill_id, quarantined_digest) in &trusted_candidate.candidate_artifact_digests {
            let quarantined = skill_repository.get(quarantined_digest).await?;
            validate_quarantined_candidate_artifact(
                skill_id,
                quarantined_digest,
                &quarantined,
                evaluated_at_ms,
            )?;
            self.ensure_status_prefix(&quarantined).await?;

            let mut evaluated = quarantined;
            evaluated.status_history.push(SkillStatusTransitionV1 {
                status: SkillStatusV1::Evaluated,
                recorded_at_ms: evaluated_at_ms,
                evaluation_report_id: Some(gate.report.report_id.clone()),
            });
            self.ensure_status_prefix(&evaluated).await?;

            let mut active = evaluated;
            active.status_history.push(SkillStatusTransitionV1 {
                status: SkillStatusV1::Active,
                recorded_at_ms: activated_at_ms,
                evaluation_report_id: Some(gate.report.report_id.clone()),
            });
            let active_ref = self.ensure_status_prefix(&active).await?;
            active_skill_artifacts.insert(skill_id.clone(), active_ref);
        }

        let active_genome = self
            .commit_active_genome(
                &candidate_revision,
                trusted_candidate,
                &gate.report.report_id,
                &active_skill_artifacts,
            )
            .await?;
        let mut evaluated_candidate = trusted_candidate.clone();
        evaluated_candidate.evaluation_report_id = Some(gate.report.report_id.clone());
        Ok(SkillExitGateOutcomeV1::Promoted(Box::new(
            SkillPromotionReceiptV1 {
                gate,
                evaluated_candidate,
                report_artifact,
                activation_authorization,
                active_skill_artifacts,
                active_genome,
            },
        )))
    }

    /// 在 Promotion 后复读 Active Genome，并从新 Episode 构造 Serve 使用证明。
    ///
    /// 证明要求新 Episode 固定 Promotion 产生的精确 Revision，并至少真实调用一份本次
    /// Promotion 的 Active Skill。只调用 Parent 中未变化的 Skill 不足以证明新制品可用。
    ///
    /// # Errors
    ///
    /// Active Revision 缺失/篡改、Episode 不是该 Revision、Serve Binder 拒绝事件、没有
    /// 真实 Skill 事件，或事件没有命中本次 Active 制品时返回 [`SkillExitGateError`]。
    pub async fn verify_post_promotion_use(
        &self,
        receipt: &SkillPromotionReceiptV1,
        episodes: &dyn agent_evolution::EpisodeStore,
        episode_id: &EpisodeId,
    ) -> Result<SkillPostPromotionProofV1, SkillExitGateError> {
        let active = self
            .genomes
            .get(&receipt.active_genome.revision_id)
            .await?
            .ok_or_else(|| {
                SkillExitGateError::ActiveGenomeNotFound(receipt.active_genome.revision_id.clone())
            })?;
        if active != receipt.active_genome {
            return Err(SkillExitGateError::ActiveGenomeStoreMismatch);
        }
        let bindings =
            collect_trusted_skill_usage_bindings(episodes, self.artifacts, episode_id, &active)
                .await?;
        if bindings.is_empty() {
            return Err(SkillExitGateError::MissingPostPromotionUsage);
        }
        let promoted_use = bindings.values().any(|binding| {
            receipt
                .active_skill_artifacts
                .get(&binding.skill_id)
                .is_some_and(|reference| reference.digest == binding.skill_artifact_digest)
        });
        if !promoted_use {
            return Err(SkillExitGateError::PostPromotionSkillMismatch);
        }
        Ok(SkillPostPromotionProofV1 {
            active_revision_id: active.revision_id,
            episode_id: episode_id.clone(),
            bindings,
        })
    }

    async fn commit_active_genome(
        &self,
        candidate_revision: &GenomeRevision,
        trusted_candidate: &SkillCandidateV1,
        report_id: &EvaluationReportId,
        active_skill_artifacts: &BTreeMap<SkillId, ArtifactRef>,
    ) -> Result<GenomeRevision, SkillExitGateError> {
        if candidate_revision.genome.execution.profile() != ExecutionProfile::Serve {
            return Err(SkillExitGateError::ActiveGenomeNotServe);
        }
        let mut active_genome = candidate_revision.genome.clone();
        let mut replaced = BTreeSet::new();
        for reference in &mut active_genome.skills {
            let skill_id = SkillId::new(reference.id.clone()).map_err(|error| {
                SkillExitGateError::InvalidCandidateSkillId {
                    value: reference.id.clone(),
                    reason: error.to_string(),
                }
            })?;
            if let Some(active) = active_skill_artifacts.get(&skill_id) {
                reference.content = active.digest.clone();
                replaced.insert(skill_id);
            }
        }
        let expected = active_skill_artifacts
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if replaced != expected {
            return Err(SkillExitGateError::CandidateSkillSetMismatch);
        }

        let mut active_revision = GenomeRevision::create(
            active_genome,
            GenomeMetadata {
                created_at: None,
                description: None,
                parent: Some(candidate_revision.revision_id.clone()),
                mutation: Some(trusted_candidate.mutation_id.clone()),
            },
        )?;
        active_revision.revision_id = deterministic_active_revision_id(
            &trusted_candidate.candidate_id,
            report_id,
            &active_revision.digest,
        )?;
        let diff = verify_allowed_genome_diff(
            candidate_revision,
            &active_revision,
            &BTreeSet::from([MutationSurface::Skill]),
        )?;
        if diff.changed_surfaces != BTreeSet::from([MutationSurface::Skill]) {
            return Err(SkillExitGateError::UnexpectedActiveGenomeDiff(
                diff.changed_surfaces,
            ));
        }
        match self.genomes.append(&active_revision).await {
            Ok(()) => {}
            Err(GenomeStoreError::AlreadyExists(existing_id))
                if existing_id == active_revision.revision_id =>
            {
                let existing = self
                    .genomes
                    .get(&active_revision.revision_id)
                    .await?
                    .ok_or_else(|| {
                        SkillExitGateError::ActiveGenomeNotFound(
                            active_revision.revision_id.clone(),
                        )
                    })?;
                if existing != active_revision {
                    return Err(SkillExitGateError::ActiveGenomeIdempotencyConflict(
                        active_revision.revision_id,
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(active_revision)
    }

    /// 确保目标状态前缀已经只追加提交，并让完整 Promotion 重试保持幂等。
    async fn ensure_status_prefix(
        &self,
        desired: &SkillArtifactV1,
    ) -> Result<ArtifactRef, SkillExitGateError> {
        let current = self
            .statuses
            .current(&desired.skill_id, desired.revision)
            .await?;
        let Some(current) = current else {
            return self.statuses.append(desired).await.map_err(Into::into);
        };
        let prefix_committed = current.status_history.len() >= desired.status_history.len()
            && current.status_history.starts_with(&desired.status_history)
            && artifact_content_without_status(&current)
                == artifact_content_without_status(desired);
        if prefix_committed {
            return SkillArtifactRepository::new(self.artifacts)
                .put(desired)
                .await
                .map_err(Into::into);
        }
        self.statuses.append(desired).await.map_err(Into::into)
    }
}

fn artifact_content_without_status(artifact: &SkillArtifactV1) -> SkillArtifactV1 {
    let mut artifact = artifact.clone();
    artifact.status_history.clear();
    artifact
}

fn validate_quarantined_candidate_artifact(
    expected_id: &SkillId,
    expected_digest: &agent_evolution_protocol::ArtifactDigest,
    artifact: &SkillArtifactV1,
    evaluated_at_ms: u64,
) -> Result<(), SkillExitGateError> {
    if &artifact.skill_id != expected_id || artifact.digest()? != *expected_digest {
        return Err(SkillExitGateError::CandidateArtifactBindingMismatch(
            expected_id.clone(),
        ));
    }
    if matches!(
        artifact.operation,
        SkillOperationV1::Deprecate { .. } | SkillOperationV1::Delete { .. }
    ) {
        return Err(SkillExitGateError::NonActivatingOperation(
            expected_id.clone(),
        ));
    }
    if artifact.status_history.len() != 1
        || artifact.status_history[0].status != SkillStatusV1::Quarantined
    {
        return Err(SkillExitGateError::CandidateArtifactNotQuarantined(
            expected_id.clone(),
        ));
    }
    if artifact.status_history[0].recorded_at_ms >= evaluated_at_ms {
        return Err(SkillExitGateError::NonMonotonicArtifactTime(
            expected_id.clone(),
        ));
    }
    Ok(())
}

fn deterministic_active_revision_id(
    candidate_id: &agent_evolution_protocol::CandidateId,
    report_id: &EvaluationReportId,
    digest: &agent_evolution_protocol::GenomeDigest,
) -> Result<GenomeRevisionId, SkillExitGateError> {
    let mut hasher = Sha256::new();
    for part in [
        b"skill-active-genome-v1".as_slice(),
        candidate_id.as_str().as_bytes(),
        report_id.as_str().as_bytes(),
        digest.as_str().as_bytes(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    GenomeRevisionId::new(format!(
        "{}_{}",
        GenomeRevisionId::PREFIX,
        format_args!("{:x}", hasher.finalize())
    ))
    .map_err(|error| SkillExitGateError::DeterministicRevisionId(error.to_string()))
}

/// M7 Skill Exit Gate 编排错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillExitGateError {
    /// 人工批准记录 ID 为空。
    #[error("Skill Activation 的 approval_id 不能为空")]
    InvalidApprovalId,
    /// 评测与激活时间无效。
    #[error("Skill Promotion 时间无效：evaluated={evaluated_at_ms}, activated={activated_at_ms}")]
    InvalidPromotionTime {
        /// Gate 报告与 Evaluated 状态时间。
        evaluated_at_ms: u64,
        /// Active 状态时间。
        activated_at_ms: u64,
    },
    /// Parent Revision 不存在。
    #[error("Skill Candidate 的 Parent Revision 不存在：{0}")]
    ParentNotFound(GenomeRevisionId),
    /// Candidate Revision 不存在。
    #[error("Skill Candidate Revision 不存在：{0}")]
    CandidateNotFound(GenomeRevisionId),
    /// Candidate DTO 与 Store 中 Genome 摘要不一致。
    #[error("Skill Candidate 与 Store 中 Parent/Candidate Genome 绑定不一致")]
    CandidateStoreBindingMismatch,
    /// Candidate 已绑定其他报告，不能重复评测。
    #[error("Skill Candidate 已绑定 EvaluationReport")]
    CandidateAlreadyEvaluated,
    /// Candidate Artifact 与 DTO 摘要或 Skill ID 不一致。
    #[error("Skill Candidate Artifact `{0}` 与可信 DTO 绑定不一致")]
    CandidateArtifactBindingMismatch(SkillId),
    /// Deprecate/Delete 不是可激活操作。
    #[error("Skill `{0}` 的 Deprecate/Delete 操作不能进入 Active Promotion")]
    NonActivatingOperation(SkillId),
    /// Candidate Artifact 不是单项 Quarantined 初始链。
    #[error("Skill Candidate Artifact `{0}` 不是初始 Quarantined 状态")]
    CandidateArtifactNotQuarantined(SkillId),
    /// Candidate Artifact 时间不早于评测时间。
    #[error("Skill Candidate Artifact `{0}` 的状态时间没有严格早于评测时间")]
    NonMonotonicArtifactTime(SkillId),
    /// Candidate 的执行 Profile 不是 Serve，不能登记后续稳定运行。
    #[error("Skill Active Genome 必须保持 Candidate 的 Serve Profile")]
    ActiveGenomeNotServe,
    /// Candidate Skill ID 不符合强类型协议。
    #[error("Candidate Genome Skill ID `{value}` 无效：{reason}")]
    InvalidCandidateSkillId {
        /// 原始 ID。
        value: String,
        /// 校验原因。
        reason: String,
    },
    /// Candidate Genome 没有引用全部待 Promotion 制品。
    #[error("Candidate Genome Skill Set 没有精确引用全部待激活制品")]
    CandidateSkillSetMismatch,
    /// Active Revision 确定性 ID 构造失败。
    #[error("构造 Active Genome 确定性 Revision ID 失败：{0}")]
    DeterministicRevisionId(String),
    /// Candidate→Active 变化不是唯一 Skill 表面。
    #[error("Candidate 到 Active Genome 的差异不是唯一 Skill：{0:?}")]
    UnexpectedActiveGenomeDiff(BTreeSet<MutationSurface>),
    /// Active Genome 不存在。
    #[error("Active Genome Revision 不存在：{0}")]
    ActiveGenomeNotFound(GenomeRevisionId),
    /// Active Genome Store 内容与 Promotion 收据不一致。
    #[error("Active Genome Store 内容与 Promotion 收据不一致")]
    ActiveGenomeStoreMismatch,
    /// 确定性 Active Revision ID 被不同内容占用。
    #[error("Active Genome Revision 幂等冲突：{0}")]
    ActiveGenomeIdempotencyConflict(GenomeRevisionId),
    /// 后续新运行没有真实 Skill 使用事件。
    #[error("Promotion 后的新运行没有可信 Skill 使用事件")]
    MissingPostPromotionUsage,
    /// 后续事件没有命中本次 Promotion 的 Active 制品。
    #[error("Promotion 后的新运行没有使用本次激活的 Skill 制品")]
    PostPromotionSkillMismatch,
    /// 独立 Skill Gate 失败。
    #[error(transparent)]
    Gate(#[from] SkillGateError),
    /// EvaluationReport 无法归档编码。
    #[error("序列化 Skill EvaluationReport 失败：{0}")]
    SerializeEvaluationReport(serde_json::Error),
    /// Artifact CAS 操作失败。
    #[error(transparent)]
    ArtifactStore(#[from] ArtifactStoreError),
    /// Skill Artifact 或状态链操作失败。
    #[error(transparent)]
    SkillRepository(#[from] SkillRepositoryError),
    /// Skill Artifact 局部协议复核失败。
    #[error("Skill Artifact 无效：{0}")]
    InvalidSkillArtifact(#[from] agent_evolution_protocol::InvalidSkillEvolution),
    /// Genome Revision 构造失败。
    #[error(transparent)]
    GenomeRevision(#[from] GenomeRevisionError),
    /// Genome Diff 复核失败。
    #[error(transparent)]
    GenomeDiff(#[from] GenomeDiffError),
    /// Genome Store 操作失败。
    #[error(transparent)]
    GenomeStore(#[from] GenomeStoreError),
    /// Serve Binder 无法证明后续使用。
    #[error(transparent)]
    SkillUsage(#[from] SkillUsageBindingError),
}
