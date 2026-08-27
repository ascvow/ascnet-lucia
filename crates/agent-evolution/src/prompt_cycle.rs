//! Prompt 自进化单轮编排。
//!
//! 本模块只消费脱敏 Evidence、外部 Prompt 制品和独立 Evaluator 回执。Hidden Dataset、
//! Verifier、Commit Policy、Promotion 与 Rollback 实现始终留在 `lucia-eval` 进程中。

use crate::{
    ArtifactStore, BoundedPromptMutator, CandidateBuildError, CandidateBuilder,
    CandidateSelectionError, CandidateSelector, CycleStoreError, EpisodeSelectionError,
    EpisodeSelector, EvaluatorClient, EvaluatorProcessError, EvolutionCycleStore, EvolutionOutbox,
    EvolutionPolicy, FileArtifactStore, FileEpisodeStore, FileEvolutionCycleStore,
    FileEvolutionOutbox, FileGenomeResolver, FileIssueObservationStore, GenomeResolver,
    GenomeResolverError, GenomeSelector, MutationEvidence, MutationProposalContext, OutboxError,
    PromptMutationError, PromptMutationGenerator,
};
use agent_evolution_protocol::{
    CandidateId, DatasetVersionId, EvaluationRequestV1, EvolutionCycleId, EvolutionCycleRequestV1,
    EvolutionCycleSnapshotV1, EvolutionCycleStage, GenomeRevision, HealthCheckRequestV1,
    MutationRisk, PromotionRequestV1, ReleaseId, RollbackRequestV1,
    EVALUATION_REQUEST_SCHEMA_VERSION, EVOLUTION_CYCLE_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::SystemTimeError};

/// 内置 Prompt Mutator 不可变配置制品的稳定正文。
const MUTATOR_REVISION_BYTES: &[u8] = br#"{"id":"bounded-task-strategy-v1","schema_version":1}"#;

/// 把脱敏证据、受限 Mutator、Candidate Builder 与独立 Evaluator 串联的 Cycle Runner。
pub struct PromptEvolutionCycle<G, E>
where
    G: PromptMutationGenerator,
    E: EvaluatorClient,
{
    evolution_root: PathBuf,
    cycle_store: FileEvolutionCycleStore,
    mutator: BoundedPromptMutator<G>,
    evaluator: E,
    dataset_version: DatasetVersionId,
}

impl<G, E> PromptEvolutionCycle<G, E>
where
    G: PromptMutationGenerator,
    E: EvaluatorClient,
{
    /// 使用 Evolution 数据根、独立 Evaluator 客户端和固定 Dataset 版本创建 Runner。
    ///
    /// `dataset_version` 只作为 Evolver 期望的受信版本前置条件；Dataset 路径和正文不会进入
    /// 本类型，实际版本仍由 `lucia-eval` 从受信配置加载并复核。
    pub fn new(
        evolution_root: impl Into<PathBuf>,
        generator: G,
        evaluator: E,
        dataset_version: DatasetVersionId,
    ) -> Self {
        let evolution_root = evolution_root.into();
        Self {
            cycle_store: FileEvolutionCycleStore::new(evolution_root.join("cycles")),
            evolution_root,
            mutator: BoundedPromptMutator::task_strategy_mvp(generator),
            evaluator,
            dataset_version,
        }
    }

    /// 返回只追加 Cycle Store，供 CLI 查询或验收归档。
    pub fn cycle_store(&self) -> &FileEvolutionCycleStore {
        &self.cycle_store
    }

    /// 执行一次完整 Prompt Evolution Cycle。
    ///
    /// 已持久化终态的相同 Cycle 会幂等返回，并再次补齐 Outbox 消费标记。非终态会从最后一
    /// 份完整快照继续；Candidate、Evaluate 与 Promotion 均使用稳定幂等身份，崩溃重启不会
    /// 重复提交不同正式制品。Promotion 成功后返回 `AwaitingHealth`，由 [`Self::verify_health`]
    /// 使用受信 Runtime 观察完成健康验证或自动回滚。
    ///
    /// # Errors
    ///
    /// 请求或 Stable 前置条件无效、Evidence 不匹配、Prompt/Candidate 构建失败、Evaluator
    /// 失败、Cycle Store 失败或 Outbox 无法在可消费终态后消费时返回 [`PromptCycleError`]。
    /// 确定性输入错误会追加 `Failed`，瞬时 Store/Evaluator 错误保留非终态供后续恢复。
    pub async fn run(
        &self,
        request: &EvolutionCycleRequestV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        request
            .validate()
            .map_err(|error| PromptCycleError::InvalidRequest(error.to_string()))?;
        self.validate_policy(request)?;
        let initial = if let Some(existing) = self.cycle_store.latest(&request.cycle_id).await? {
            if existing.request != *request {
                return Err(PromptCycleError::CycleRequestConflict);
            }
            if crate::is_terminal_cycle_stage(existing.stage) {
                if should_consume_outbox(existing.stage) {
                    self.consume_outbox(request).await?;
                }
                return Ok(existing);
            }
            if existing.stage == EvolutionCycleStage::AwaitingHealth {
                return Ok(existing);
            }
            existing
        } else {
            self.validate_stable(request).await?;
            let initial = EvolutionCycleSnapshotV1 {
                schema_version: EVOLUTION_CYCLE_SCHEMA_VERSION,
                request: request.clone(),
                cycle_id: request.cycle_id.clone(),
                issue_id: request.issue_id.clone(),
                parent_revision_id: request.parent_revision_id.clone(),
                stage: EvolutionCycleStage::Requested,
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
            self.cycle_store.append(&initial).await?;
            initial
        };

        match self.run_active(request, initial).await {
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

    /// 使用受信 Evaluator 的 Runtime 健康观察完成 Promotion 后验证。
    ///
    /// `lucia-evolve` 不接收健康结果正文；`lucia-eval` 按 Release ID 从受信配置根加载观察，
    /// 复核 Stable 后返回脱敏回执。健康失败会在同一 Cycle 内自动回滚 Parent 并归档两份
    /// Release Receipt。
    ///
    /// # Errors
    ///
    /// Cycle 不存在、尚未 Promotion、健康观察不可用、Evaluator/Release Controller 或 Store
    /// 失败时返回 [`PromptCycleError`]。瞬时失败保留可恢复阶段。
    pub async fn verify_health(
        &self,
        cycle_id: &EvolutionCycleId,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        let mut current = self
            .cycle_store
            .latest(cycle_id)
            .await?
            .ok_or_else(|| PromptCycleError::CycleNotFound(cycle_id.clone()))?;
        let request = current.request.clone();
        if crate::is_terminal_cycle_stage(current.stage) {
            if should_consume_outbox(current.stage) {
                self.consume_outbox(&request).await?;
            }
            return Ok(current);
        }
        if current.stage == EvolutionCycleStage::AwaitingHealth {
            current = self
                .advance(current, EvolutionCycleStage::VerifyingHealth, |_| {})
                .await?;
        } else if !matches!(
            current.stage,
            EvolutionCycleStage::VerifyingHealth | EvolutionCycleStage::RollingBack
        ) {
            return Err(PromptCycleError::HealthNotReady(current.stage));
        }
        let snapshot = self.run_active(&request, current).await?;
        if should_consume_outbox(snapshot.stage) {
            self.consume_outbox(&request).await?;
        }
        Ok(snapshot)
    }

    /// 完成已经建立首快照的活动 Cycle。
    async fn run_active(
        &self,
        request: &EvolutionCycleRequestV1,
        mut current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        loop {
            current = match current.stage {
                EvolutionCycleStage::Requested => {
                    self.advance(current, EvolutionCycleStage::SelectingEvidence, |_| {})
                        .await?
                }
                EvolutionCycleStage::SelectingEvidence => {
                    self.select_evidence(request).await?;
                    self.advance(current, EvolutionCycleStage::Diagnosing, |_| {})
                        .await?
                }
                EvolutionCycleStage::Diagnosing => {
                    self.load_parent_prompt(request).await?;
                    self.advance(current, EvolutionCycleStage::Mutating, |_| {})
                        .await?
                }
                EvolutionCycleStage::Mutating => self.resume_mutation(request, current).await?,
                EvolutionCycleStage::BuildingCandidates => {
                    self.resume_candidate_build(request, current).await?
                }
                EvolutionCycleStage::Evaluating => self.resume_evaluation(request, current).await?,
                EvolutionCycleStage::SelectingWinner => self.resume_selection(current).await?,
                EvolutionCycleStage::Promoting => self.resume_promotion(current).await?,
                EvolutionCycleStage::AwaitingHealth => return Ok(current),
                EvolutionCycleStage::VerifyingHealth => {
                    self.resume_health_verification(current).await?
                }
                EvolutionCycleStage::RollingBack => self.resume_rollback(current).await?,
                EvolutionCycleStage::Completed
                | EvolutionCycleStage::HealthVerified
                | EvolutionCycleStage::RolledBack
                | EvolutionCycleStage::Rejected
                | EvolutionCycleStage::Failed => return Ok(current),
            };
        }
    }

    /// 从 Mutating 阶段重新加载受信输入并归档固定数量 Proposal。
    async fn resume_mutation(
        &self,
        request: &EvolutionCycleRequestV1,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        let evidence = self.select_evidence(request).await?;
        let (parent, parent_prompt) = self.load_parent_prompt(request).await?;
        let artifacts = FileArtifactStore::new(self.evolution_root.join("artifacts"));
        let mutator_revision = artifacts
            .put("application/json", MUTATOR_REVISION_BYTES)
            .await?;
        let proposals = self
            .mutator
            .propose(
                &parent_prompt,
                &evidence,
                &MutationProposalContext {
                    parent_revision_id: parent.revision_id,
                    parent_genome_digest: parent.digest,
                    mutator_revision,
                    risk: MutationRisk::Low,
                },
                &artifacts,
            )
            .await?;
        if proposals.len() != request.candidate_count as usize {
            return Err(PromptCycleError::CandidateCountMismatch);
        }
        self.advance(
            current,
            EvolutionCycleStage::BuildingCandidates,
            |snapshot| snapshot.proposals = proposals,
        )
        .await
    }

    /// 从已归档 Proposal 前缀继续构建尚未提交快照的 Candidate。
    async fn resume_candidate_build(
        &self,
        request: &EvolutionCycleRequestV1,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        if current.proposals.len() != request.candidate_count as usize
            || current.candidates.len() > current.proposals.len()
            || current
                .candidates
                .iter()
                .zip(&current.proposals)
                .any(|(candidate, proposal)| candidate.mutation_id != proposal.mutation_id)
        {
            return Err(PromptCycleError::StateArtifactMismatch);
        }
        if current.candidates.len() == current.proposals.len() {
            return self
                .advance(current, EvolutionCycleStage::Evaluating, |_| {})
                .await;
        }

        let proposal = current.proposals[current.candidates.len()].clone();
        let artifacts = FileArtifactStore::new(self.evolution_root.join("artifacts"));
        let resolver = FileGenomeResolver::new(&self.evolution_root);
        let candidate = CandidateBuilder::new(resolver.store(), &artifacts)
            .build_at(request.cycle_id.clone(), &proposal, request.requested_at_ms)
            .await?;
        self.advance(
            current,
            EvolutionCycleStage::BuildingCandidates,
            |snapshot| snapshot.candidates.push(candidate),
        )
        .await
    }

    /// 从已归档 Receipt 前缀继续独立评测尚未提交的 Candidate。
    async fn resume_evaluation(
        &self,
        request: &EvolutionCycleRequestV1,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        if current.candidates.len() != request.candidate_count as usize
            || current.evaluation_receipts.len() > current.candidates.len()
            || current
                .evaluation_receipts
                .iter()
                .zip(&current.candidates)
                .any(|(receipt, candidate)| {
                    receipt.candidate_revision_id != candidate.candidate_revision_id
                        || receipt.request_id != evaluation_request_id(request, candidate)
                })
        {
            return Err(PromptCycleError::StateArtifactMismatch);
        }
        if current.evaluation_receipts.len() == current.candidates.len() {
            return self
                .advance(current, EvolutionCycleStage::SelectingWinner, |_| {})
                .await;
        }

        let candidate = &current.candidates[current.evaluation_receipts.len()];
        let receipt = self
            .evaluator
            .evaluate(&EvaluationRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                request_id: evaluation_request_id(request, candidate),
                parent_revision_id: request.parent_revision_id.clone(),
                candidate_revision_id: candidate.candidate_revision_id.clone(),
                lineage: request.lineage.clone(),
                expected_parent_generation: request.expected_parent_generation,
                expected_dataset_version: self.dataset_version.clone(),
            })
            .await?;
        self.advance(current, EvolutionCycleStage::Evaluating, |snapshot| {
            snapshot.evaluation_receipts.push(receipt);
        })
        .await
    }

    /// 从完整正式回执集合确定稳定胜者或 Reject 终态。
    async fn resume_selection(
        &self,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        let Some(selected) =
            CandidateSelector::select(&current.candidates, &current.evaluation_receipts)?
        else {
            return self
                .advance(current, EvolutionCycleStage::Rejected, |_| {})
                .await;
        };
        self.advance(current, EvolutionCycleStage::Promoting, |snapshot| {
            snapshot.winner = Some(selected.candidate_id);
        })
        .await
    }

    /// 使用确定性 Release ID 幂等完成 Promotion，并进入健康观察等待阶段。
    async fn resume_promotion(
        &self,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        let winner = current
            .winner
            .as_ref()
            .ok_or(PromptCycleError::StateArtifactMismatch)?;
        let candidate = candidate_by_id(&current, winner)?;
        let receipt = current
            .evaluation_receipts
            .iter()
            .find(|receipt| receipt.candidate_revision_id == candidate.candidate_revision_id)
            .ok_or(PromptCycleError::StateArtifactMismatch)?;
        let release_id = deterministic_release_id(
            "promotion-v1",
            &current.cycle_id,
            winner.as_str(),
            receipt.report_id.as_str(),
        )?;
        let release_receipt = self
            .evaluator
            .promote(&PromotionRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                report_id: receipt.report_id.clone(),
                release_id,
            })
            .await?;
        self.advance(current, EvolutionCycleStage::AwaitingHealth, |snapshot| {
            snapshot.release_receipt = Some(release_receipt)
        })
        .await
    }

    /// 请求受信 Evaluator 复核 Runtime 观察，并选择成功或回滚分支。
    async fn resume_health_verification(
        &self,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        let release = current
            .release_receipt
            .as_ref()
            .ok_or(PromptCycleError::StateArtifactMismatch)?;
        let health = self
            .evaluator
            .health(&HealthCheckRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                request_id: format!("{}-health", current.cycle_id),
                release_id: release.release_id.clone(),
                lineage: release.lineage.clone(),
                expected_revision_id: release.to.clone(),
                expected_generation: release.generation,
            })
            .await?;
        let stage = if health.verified {
            EvolutionCycleStage::HealthVerified
        } else {
            EvolutionCycleStage::RollingBack
        };
        self.advance(current, stage, |snapshot| {
            snapshot.health_receipt = Some(health);
        })
        .await
    }

    /// 使用确定性 Rollback Release ID 把失败 Promotion 原子切回 Parent。
    async fn resume_rollback(
        &self,
        current: EvolutionCycleSnapshotV1,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError> {
        let release = current
            .release_receipt
            .as_ref()
            .ok_or(PromptCycleError::StateArtifactMismatch)?;
        if current
            .health_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.verified)
        {
            return Err(PromptCycleError::StateArtifactMismatch);
        }
        let rollback_release_id = deterministic_release_id(
            "rollback-v1",
            &current.cycle_id,
            release.release_id.as_str(),
            release.report_id.as_str(),
        )?;
        let rollback = self
            .evaluator
            .rollback(&RollbackRequestV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: release.release_id.clone(),
                rollback_release_id,
            })
            .await?;
        self.advance(current, EvolutionCycleStage::RolledBack, |snapshot| {
            snapshot.rollback_receipt = Some(rollback);
        })
        .await
    }

    /// 验证请求使用固定不可反序列化 Policy。
    fn validate_policy(&self, request: &EvolutionCycleRequestV1) -> Result<(), PromptCycleError> {
        let policy = EvolutionPolicy::task_strategy_mvp();
        if request.evolution_policy_version != policy.version()
            || request.candidate_count as usize != policy.candidate_count()
        {
            return Err(PromptCycleError::PolicyMismatch);
        }
        Ok(())
    }

    /// 验证请求 Parent 仍是相同 lineage 的当前 Stable。
    async fn validate_stable(
        &self,
        request: &EvolutionCycleRequestV1,
    ) -> Result<(), PromptCycleError> {
        let resolver = FileGenomeResolver::new(&self.evolution_root);
        let stable = resolver.stable_reference(&request.lineage).await?;
        if stable.revision_id != request.parent_revision_id
            || stable.digest != request.parent_genome_digest
            || stable.generation != request.expected_parent_generation
        {
            return Err(PromptCycleError::StablePreconditionFailed);
        }
        Ok(())
    }

    /// 选择并严格绑定请求中的 Issue、Genome 与 Episode 集合。
    async fn select_evidence(
        &self,
        request: &EvolutionCycleRequestV1,
    ) -> Result<MutationEvidence, PromptCycleError> {
        let selector = EpisodeSelector::new(
            Arc::new(FileEvolutionOutbox::new(self.evolution_root.join("outbox"))),
            Arc::new(FileEpisodeStore::new(self.evolution_root.join("episodes"))),
            Arc::new(FileIssueObservationStore::new(
                self.evolution_root.join("issue-observations"),
            )),
        );
        let evidence = selector
            .select()
            .await?
            .into_iter()
            .find(|evidence| evidence.issue_id == request.issue_id)
            .ok_or(PromptCycleError::EvidenceNotFound)?;
        let actual = evidence
            .episodes
            .iter()
            .map(|episode| episode.episode_id.clone())
            .collect::<BTreeSet<_>>();
        let expected = request
            .source_episode_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if actual != expected
            || evidence.genome_digest != request.parent_genome_digest
            || evidence
                .episodes
                .iter()
                .any(|episode| episode.genome_revision_id != request.parent_revision_id)
        {
            return Err(PromptCycleError::EvidenceBindingMismatch);
        }
        Ok(evidence)
    }

    /// 从真实 Parent Revision 与 CAS 加载唯一 Task Strategy Prompt。
    async fn load_parent_prompt(
        &self,
        request: &EvolutionCycleRequestV1,
    ) -> Result<(GenomeRevision, String), PromptCycleError> {
        let resolver = FileGenomeResolver::new(&self.evolution_root);
        let parent = resolver
            .resolve(&GenomeSelector::Revision(
                request.parent_revision_id.clone(),
            ))
            .await?;
        if parent.digest != request.parent_genome_digest {
            return Err(PromptCycleError::StablePreconditionFailed);
        }
        let digest = parent
            .genome
            .prompt
            .task_strategy()
            .ok_or(PromptCycleError::MissingTaskStrategy)?;
        let bytes = FileArtifactStore::new(self.evolution_root.join("artifacts"))
            .get(digest)
            .await?
            .ok_or(PromptCycleError::MissingParentPrompt)?;
        let prompt = String::from_utf8(bytes).map_err(|_| PromptCycleError::ParentPromptNotUtf8)?;
        if prompt.trim().is_empty() {
            return Err(PromptCycleError::MissingParentPrompt);
        }
        Ok((parent, prompt))
    }

    /// 追加一份保留全部历史制品的新阶段快照。
    async fn advance<F>(
        &self,
        previous: EvolutionCycleSnapshotV1,
        stage: EvolutionCycleStage,
        mutate: F,
    ) -> Result<EvolutionCycleSnapshotV1, PromptCycleError>
    where
        F: FnOnce(&mut EvolutionCycleSnapshotV1),
    {
        let mut next = previous.clone();
        next.sequence = previous
            .sequence
            .checked_add(1)
            .ok_or(PromptCycleError::SequenceOverflow)?;
        next.previous_digest = Some(FileEvolutionCycleStore::snapshot_digest(&previous)?);
        next.stage = stage;
        next.created_at_ms = now_ms()?;
        next.failure_code = None;
        mutate(&mut next);
        self.cycle_store.append(&next).await?;
        Ok(next)
    }

    /// 在活动 Cycle 失败时尽力追加稳定错误码终态。
    async fn append_failed(
        &self,
        request: &EvolutionCycleRequestV1,
        code: &'static str,
    ) -> Result<(), PromptCycleError> {
        let Some(previous) = self.cycle_store.latest(&request.cycle_id).await? else {
            return Ok(());
        };
        if crate::is_terminal_cycle_stage(previous.stage) {
            return Ok(());
        }
        self.advance(previous, EvolutionCycleStage::Failed, |snapshot| {
            snapshot.failure_code = Some(code.to_string());
        })
        .await?;
        Ok(())
    }

    /// 只有终态已落盘后才按 Issue 与 Episode 双重绑定消费 Outbox。
    async fn consume_outbox(
        &self,
        request: &EvolutionCycleRequestV1,
    ) -> Result<(), PromptCycleError> {
        let store = FileEvolutionOutbox::new(self.evolution_root.join("outbox"));
        let source = request
            .source_episode_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        for item in store.pending().await? {
            if item.issue_id.as_ref() == Some(&request.issue_id)
                && source.contains(&item.episode_id)
            {
                store.mark_consumed(&item.outbox_id).await?;
            }
        }
        Ok(())
    }
}

/// Prompt Evolution Cycle 的可信边界错误。
#[derive(Debug, thiserror::Error)]
pub enum PromptCycleError {
    /// Cycle 请求结构无效。
    #[error("Prompt Evolution Cycle 请求无效：{0}")]
    InvalidRequest(String),
    /// 请求试图使用非固定 Policy 或候选数量。
    #[error("Prompt Evolution Cycle Policy 与内置策略不匹配")]
    PolicyMismatch,
    /// 相同 Cycle ID 已被另一份不同请求占用。
    #[error("Prompt Evolution Cycle ID 已绑定另一份请求")]
    CycleRequestConflict,
    /// 指定 Cycle 不存在。
    #[error("Prompt Evolution Cycle 不存在：{0}")]
    CycleNotFound(EvolutionCycleId),
    /// Cycle 尚未进入 Promotion 后健康验证阶段。
    #[error("Prompt Evolution Cycle 当前阶段不能验证健康：{0:?}")]
    HealthNotReady(EvolutionCycleStage),
    /// Stable Revision、摘要或代数已变化。
    #[error("Prompt Evolution Cycle Stable 前置条件失败")]
    StablePreconditionFailed,
    /// 请求 Issue 没有可变异脱敏证据。
    #[error("Prompt Evolution Cycle 未找到合格 Evidence")]
    EvidenceNotFound,
    /// Evidence 的 Issue、Episode、Revision 或摘要与请求不一致。
    #[error("Prompt Evolution Cycle Evidence 绑定不匹配")]
    EvidenceBindingMismatch,
    /// Parent Genome 缺少唯一 Task Strategy Prompt。
    #[error("Prompt Evolution Cycle Parent 缺少 Task Strategy Prompt")]
    MissingTaskStrategy,
    /// Parent Prompt CAS 制品缺失或为空。
    #[error("Prompt Evolution Cycle Parent Prompt 制品缺失")]
    MissingParentPrompt,
    /// Parent Prompt 不是 UTF-8。
    #[error("Prompt Evolution Cycle Parent Prompt 不是 UTF-8")]
    ParentPromptNotUtf8,
    /// Mutator 返回数量与受信请求不一致。
    #[error("Prompt Evolution Cycle Candidate 数量不匹配")]
    CandidateCountMismatch,
    /// 已归档阶段制品的数量、顺序或身份不满足恢复前置条件。
    #[error("Prompt Evolution Cycle 已归档制品与阶段不一致")]
    StateArtifactMismatch,
    /// 稳定幂等 Release ID 无法满足强类型协议。
    #[error("Prompt Evolution Cycle 构造稳定 Release ID 失败：{0}")]
    DeterministicReleaseId(String),
    /// 快照序号溢出。
    #[error("Prompt Evolution Cycle 快照序号溢出")]
    SequenceOverflow,
    /// Cycle Store 失败。
    #[error(transparent)]
    CycleStore(#[from] CycleStoreError),
    /// Genome 解析失败。
    #[error(transparent)]
    GenomeResolver(#[from] GenomeResolverError),
    /// Episode Selector 失败。
    #[error(transparent)]
    EpisodeSelection(#[from] EpisodeSelectionError),
    /// Prompt Mutator 失败。
    #[error(transparent)]
    PromptMutation(#[from] PromptMutationError),
    /// Candidate Builder 失败。
    #[error(transparent)]
    CandidateBuild(#[from] CandidateBuildError),
    /// Candidate Selector 失败。
    #[error(transparent)]
    CandidateSelection(#[from] CandidateSelectionError),
    /// Artifact CAS 失败。
    #[error(transparent)]
    ArtifactStore(#[from] crate::ArtifactStoreError),
    /// 独立 Evaluator 调用失败。
    #[error(transparent)]
    Evaluator(#[from] EvaluatorProcessError),
    /// Evolution Outbox 失败。
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    /// 系统时钟不可用。
    #[error("Prompt Evolution Cycle 系统时钟无效：{0}")]
    Clock(#[from] SystemTimeError),
    /// Unix 毫秒超过 `u64`。
    #[error("Prompt Evolution Cycle 系统时间溢出")]
    ClockOverflow,
}

impl PromptCycleError {
    /// 返回不含路径、用户正文和内部错误细节的稳定失败码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "cycle_request_invalid",
            Self::PolicyMismatch => "evolution_policy_mismatch",
            Self::CycleRequestConflict => "cycle_request_conflict",
            Self::CycleNotFound(_) => "cycle_not_found",
            Self::HealthNotReady(_) => "cycle_health_not_ready",
            Self::StablePreconditionFailed => "stable_precondition_failed",
            Self::EvidenceNotFound => "evidence_not_found",
            Self::EvidenceBindingMismatch => "evidence_binding_mismatch",
            Self::MissingTaskStrategy => "task_strategy_missing",
            Self::MissingParentPrompt => "parent_prompt_missing",
            Self::ParentPromptNotUtf8 => "parent_prompt_not_utf8",
            Self::CandidateCountMismatch => "candidate_count_mismatch",
            Self::StateArtifactMismatch => "cycle_state_artifact_mismatch",
            Self::DeterministicReleaseId(_) => "release_id_derivation_failed",
            Self::SequenceOverflow => "cycle_sequence_overflow",
            Self::CycleStore(_) => "cycle_store_failed",
            Self::GenomeResolver(_) => "genome_resolve_failed",
            Self::EpisodeSelection(_) => "evidence_selection_failed",
            Self::PromptMutation(_) => "prompt_mutation_failed",
            Self::CandidateBuild(_) => "candidate_build_failed",
            Self::CandidateSelection(_) => "candidate_selection_failed",
            Self::ArtifactStore(_) => "artifact_store_failed",
            Self::Evaluator(_) => "evaluator_failed",
            Self::Outbox(_) => "outbox_failed",
            Self::Clock(_) | Self::ClockOverflow => "clock_failed",
        }
    }

    /// 确定性协议/绑定错误关闭当前 Cycle；瞬时 I/O 和独立进程错误保留恢复点。
    fn should_close_cycle(&self) -> bool {
        matches!(
            self,
            Self::InvalidRequest(_)
                | Self::PolicyMismatch
                | Self::CycleRequestConflict
                | Self::StablePreconditionFailed
                | Self::EvidenceNotFound
                | Self::EvidenceBindingMismatch
                | Self::MissingTaskStrategy
                | Self::MissingParentPrompt
                | Self::ParentPromptNotUtf8
                | Self::CandidateCountMismatch
                | Self::StateArtifactMismatch
                | Self::DeterministicReleaseId(_)
                | Self::CandidateSelection(_)
        )
    }
}

/// 按 Candidate ID 查找已归档 Candidate，并拒绝缺失身份。
fn candidate_by_id<'a>(
    snapshot: &'a EvolutionCycleSnapshotV1,
    candidate_id: &CandidateId,
) -> Result<&'a agent_evolution_protocol::MutationCandidate, PromptCycleError> {
    snapshot
        .candidates
        .iter()
        .find(|candidate| &candidate.candidate_id == candidate_id)
        .ok_or(PromptCycleError::StateArtifactMismatch)
}

/// 从 Cycle 与受信控制面身份派生稳定 Release ID，供崩溃后的幂等重试使用。
fn deterministic_release_id(
    domain: &str,
    cycle_id: &EvolutionCycleId,
    first: &str,
    second: &str,
) -> Result<ReleaseId, PromptCycleError> {
    let mut hasher = Sha256::new();
    for value in [domain, cycle_id.as_str(), first, second] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    ReleaseId::new(format!("{}_{:x}", ReleaseId::PREFIX, hasher.finalize()))
        .map_err(|error| PromptCycleError::DeterministicReleaseId(error.to_string()))
}

/// 只有行为终态才消费 Evidence Outbox；基础设施失败保留证据供新 Cycle 重试。
fn should_consume_outbox(stage: EvolutionCycleStage) -> bool {
    matches!(
        stage,
        EvolutionCycleStage::Completed
            | EvolutionCycleStage::HealthVerified
            | EvolutionCycleStage::RolledBack
            | EvolutionCycleStage::Rejected
    )
}

/// 从 Cycle/Candidate 强类型 ID 生成不含用户内容的稳定评测幂等 ID。
fn evaluation_request_id(
    request: &EvolutionCycleRequestV1,
    candidate: &agent_evolution_protocol::MutationCandidate,
) -> String {
    format!("{}-{}", request.cycle_id, candidate.candidate_id)
}

/// 返回当前 Unix 毫秒。
fn now_ms() -> Result<u64, PromptCycleError> {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )
    .map_err(|_| PromptCycleError::ClockOverflow)
}
