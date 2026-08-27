//! M6 Context Policy 从三候选到健康回滚的完整离线 E2E。

use agent_evaluation::{
    evaluate_context_policy_candidate, ContextEvaluationReportBuilder,
    ContextEvaluationReportMetadata, FileRuntimeHealthObservationStore, ReleaseController,
    ReleaseHealthVerifier, TrustedEvaluationArchive,
};
use agent_evolution::{
    ArtifactStore, ContextCycleStage, ContextEvaluatorClient, ContextEvolutionCycle,
    ContextPolicyRepository, EpisodeStore, EvaluatorProcessError, FileArtifactStore,
    FileEpisodeStore, FileGenomeResolver, FileGenomeStore, FileStableGenomePublisher,
    GenomeResolver, GenomeSelector, GenomeStore, CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ArtifactRef, ContextEvaluationObservationV1,
    ContextEvaluationReceiptV1, ContextEvaluationRequestV1, ContextPolicyV1, DataClass,
    DatasetVersionId, Episode, EpisodeDataPolicy, EpisodeId, EvolutionEligibility,
    FailureClassification, FailureKind, GateDecision, GenomeMetadata, GenomeRevision,
    GenomeRevisionId, HealthCheckReceiptV1, HealthCheckRequestV1, ModelGenome, Outcome, PolicyRef,
    PromotionRequestV1, RecallObservationV1, ReleaseReceiptV1, ReplayabilityGrade,
    RollbackRequestV1, RunId, RuntimeHealthObservationV1, RuntimeIdentity, TaskDescriptor,
    ToolProfileGenome, UsageSummary, CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
    CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION, EPISODE_SCHEMA_VERSION, GENOME_SCHEMA_VERSION,
    M6_CONTEXT_GATE_VERSION, NATIVE_CONTEXT_POLICY_ID, RELEASE_RECEIPT_SCHEMA_VERSION,
};
use agent_tool::{ExecutionPolicy, ToolAccess};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

/// 使用真实 Gate、Archive、Release 与 Health Verifier 的进程边界测试替身。
#[derive(Clone)]
struct TrustedContextEvaluator {
    evolution_root: PathBuf,
    archive_root: PathBuf,
    health_root: PathBuf,
    fixture_version: DatasetVersionId,
    fixture_digest: ArtifactDigest,
    evaluated: Arc<Mutex<Vec<GenomeRevisionId>>>,
}

impl TrustedContextEvaluator {
    /// 创建固定 Fixture 身份的受信测试控制面。
    fn new(
        evolution_root: PathBuf,
        archive_root: PathBuf,
        health_root: PathBuf,
        fixture_version: DatasetVersionId,
    ) -> Self {
        Self {
            evolution_root,
            archive_root,
            health_root,
            fixture_version,
            fixture_digest: digest_bytes(b"m6-context-fixture-v1"),
            evaluated: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 返回 Candidate 的固定观察：首个候选通过，其余候选分别发生召回或 token 失败。
    fn candidate_observation(
        &self,
        candidate: &GenomeRevisionId,
    ) -> ContextEvaluationObservationV1 {
        let mut evaluated = self.evaluated.lock().expect("测试评测序列锁不应中毒");
        let position = evaluated
            .iter()
            .position(|revision| revision == candidate)
            .unwrap_or_else(|| {
                evaluated.push(candidate.clone());
                evaluated.len() - 1
            });
        match position {
            0 => observation(100, 100, 100, 100, 100, 6_000, 100, 800),
            1 => observation(90, 100, 100, 100, 100, 6_000, 100, 800),
            _ => observation(100, 100, 100, 100, 100, 8_000, 100, 800),
        }
    }
}

#[async_trait]
impl ContextEvaluatorClient for TrustedContextEvaluator {
    async fn evaluate_context(
        &self,
        request: &ContextEvaluationRequestV1,
    ) -> Result<ContextEvaluationReceiptV1, EvaluatorProcessError> {
        request.validate().map_err(process_error)?;
        let resolver = FileGenomeResolver::new(&self.evolution_root);
        let parent = resolver
            .resolve(&GenomeSelector::Revision(
                request.parent_revision_id.clone(),
            ))
            .await
            .map_err(process_error)?;
        let candidate = resolver
            .resolve(&GenomeSelector::Revision(
                request.candidate_revision_id.clone(),
            ))
            .await
            .map_err(process_error)?;
        let parent_observation = parent_observation();
        let candidate_observation = self.candidate_observation(&candidate.revision_id);
        let context_report = evaluate_context_policy_candidate(
            &parent,
            &candidate,
            &parent_observation,
            &candidate_observation,
        )
        .map_err(process_error)?;
        let archive = TrustedEvaluationArchive::new(&self.archive_root);
        let archive_request = request.archive_request();
        let binding = archive
            .bind_request(&archive_request, 1_000)
            .await
            .map_err(process_error)?;
        let trusted = ContextEvaluationReportBuilder
            .build_with_fixed_identity(
                &context_report,
                &parent_observation,
                &candidate_observation,
                &parent,
                &candidate,
                ContextEvaluationReportMetadata {
                    lineage: request.lineage.clone(),
                    parent_generation: request.expected_parent_generation,
                    fixture_version: self.fixture_version.clone(),
                    fixture_digest: self.fixture_digest.clone(),
                    generated_at_ms: binding.generated_at_ms,
                },
                agent_evaluation::EvaluationReportIdentity {
                    report_id: binding.report_id.clone(),
                    generated_at_ms: binding.generated_at_ms,
                },
            )
            .map_err(process_error)?;
        archive
            .prepare_for_request(&binding, &trusted)
            .await
            .map_err(process_error)?;
        let verified = archive
            .commit_prepared_for_request(&binding, binding.generated_at_ms)
            .await
            .map_err(process_error)?;
        let context_report_digest = digest_json(&context_report);
        let receipt = ContextEvaluationReceiptV1 {
            schema_version: CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            report_id: verified.report().report_id.clone(),
            report_digest: verified.seal().report_digest.clone(),
            context_report_digest,
            audit_record_id: verified.seal().audit_record_id.clone(),
            audit_head_digest: verified.seal().audit_record_digest.clone(),
            fixture_version: self.fixture_version.clone(),
            context_report,
            lifecycle: verified.report().lifecycle,
        };
        receipt
            .validate(M6_CONTEXT_GATE_VERSION)
            .map_err(process_error)?;
        Ok(receipt)
    }

    async fn promote_context(
        &self,
        request: &PromotionRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        let receipt = ReleaseController::new(&self.evolution_root, &self.archive_root)
            .promote(&request.report_id, request.release_id.clone(), 2_000)
            .await
            .map_err(process_error)?;
        Ok(release_receipt(receipt))
    }

    async fn health_context(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, EvaluatorProcessError> {
        let observations =
            FileRuntimeHealthObservationStore::new(&self.health_root).map_err(process_error)?;
        observations
            .put(&RuntimeHealthObservationV1 {
                schema_version: agent_evolution_protocol::EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: request.release_id.clone(),
                observed_revision_id: request.expected_revision_id.clone(),
                checks_passed: 2,
                checks_total: 3,
                observed_at_ms: 2_100,
            })
            .await
            .map_err(process_error)?;
        ReleaseHealthVerifier::new(&self.evolution_root, &self.archive_root, observations)
            .verify(request)
            .await
            .map_err(process_error)
    }

    async fn rollback_context(
        &self,
        request: &RollbackRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        let receipt = ReleaseController::new(&self.evolution_root, &self.archive_root)
            .rollback(
                &request.release_id,
                request.rollback_release_id.clone(),
                3_000,
            )
            .await
            .map_err(process_error)?;
        Ok(release_receipt(receipt))
    }
}

/// 把任意受信测试控制面错误折叠到共享进程错误类型。
fn process_error(error: impl std::fmt::Display) -> EvaluatorProcessError {
    EvaluatorProcessError::InvalidReceiptBinding(error.to_string())
}

/// 把内部 Release 回执映射为共享 IPC 回执。
fn release_receipt(receipt: agent_evaluation::ReleaseReceipt) -> ReleaseReceiptV1 {
    let value = ReleaseReceiptV1 {
        schema_version: RELEASE_RECEIPT_SCHEMA_VERSION,
        release_id: receipt.release_id,
        report_id: receipt.report_id,
        lineage: receipt.lineage,
        from: receipt.from,
        to: receipt.to,
        generation: receipt.generation,
        audit_record_id: receipt.audit_record_id,
        rollback_of: receipt.rollback_of,
    };
    value.validate().expect("受信 Release 回执应合法");
    value
}

/// 构造全部召回率为 100%、token 缩减 30% 的 Parent 观察。
fn parent_observation() -> ContextEvaluationObservationV1 {
    observation(100, 100, 100, 100, 100, 7_000, 100, 900)
}

/// 从百分制命中数和资源值构造固定观察。
#[allow(clippy::too_many_arguments)]
fn observation(
    facts: u64,
    constraints: u64,
    tools: u64,
    plan: u64,
    downstream: u64,
    tokens_after: u64,
    cost: u64,
    latency: u64,
) -> ContextEvaluationObservationV1 {
    ContextEvaluationObservationV1 {
        schema_version: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
        facts: recall(facts),
        constraints: recall(constraints),
        tool_states: recall(tools),
        plan_states: recall(plan),
        downstream_tasks: recall(downstream),
        tokens_before: 10_000,
        tokens_after,
        cost_microunits: cost,
        latency_ms: latency,
    }
}

/// 构造分母固定为 100 的召回观察。
fn recall(recalled: u64) -> RecallObservationV1 {
    RecallObservationV1 {
        expected: 100,
        recalled,
    }
}

/// 构造包含 Context Loader owner 的可发布 Parent Genome。
fn parent_revision(policy_digest: ArtifactDigest) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".to_string(),
                git_commit: "m6-context-e2e".to_string(),
                git_dirty: false,
                target_triple: "test-target".to_string(),
                features: BTreeSet::from(["plugins".to_string()]),
            },
            model: ModelGenome {
                provider: "fixture".to_string(),
                provider_kind: "fixture".to_string(),
                model: "deterministic".to_string(),
                base_url: None,
                protocol: None,
                max_tokens: Some(4_096),
                temperature: None,
                stream: false,
                provider_options_digest: None,
            },
            prompt: Default::default(),
            plugins: Vec::new(),
            capability_owners: Default::default(),
            tools: ToolProfileGenome {
                native_tools: BTreeSet::new(),
                access: ToolAccess::All,
            },
            context_policy: Some(PolicyRef {
                id: NATIVE_CONTEXT_POLICY_ID.to_string(),
                config_digest: policy_digest,
            }),
            planning_policy: None,
            skills: Vec::new(),
            execution: ExecutionPolicy::serve(),
        },
        GenomeMetadata::default(),
    )
    .expect("Parent Genome 应合法")
}

/// 构造明确允许进入变异的脱敏 Episode。
fn episode(parent: &GenomeRevision, event_stream_ref: ArtifactRef) -> Episode {
    let mut data_policy = EpisodeDataPolicy::for_class(DataClass::Internal);
    data_policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    data_policy.redaction_rules_version = Some("redaction-v1".to_string());
    Episode {
        schema_version: EPISODE_SCHEMA_VERSION,
        episode_id: EpisodeId::generate(),
        run_id: RunId::generate(),
        session_id: "m6-context-e2e".to_string(),
        genome_revision_id: parent.revision_id.clone(),
        task: TaskDescriptor {
            family: "context-retention".to_string(),
            input_ref: None,
            tags: BTreeSet::from(["context".to_string()]),
        },
        event_stream_ref,
        supervision: None,
        environment_ref: None,
        outcome: Some(Outcome::TaskFailure),
        failures: vec![FailureClassification {
            kind: FailureKind::VerificationFailure,
            evidence_event_ids: Vec::new(),
            confidence: 1.0,
            rule_derived: true,
            model_assisted: false,
        }],
        usage: UsageSummary::default(),
        replayability: ReplayabilityGrade::FixtureReproducible,
        data_policy,
        event_count: 0,
        started_at_ms: 1,
        finished_at_ms: 2,
    }
}

/// 计算任意字节的 Artifact 摘要。
fn digest_bytes(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes))).expect("测试摘要应合法")
}

/// 计算 serde 值规范 JSON 的 Artifact 摘要。
fn digest_json(value: &impl serde::Serialize) -> ArtifactDigest {
    digest_bytes(&serde_json::to_vec(value).expect("测试 JSON 应可序列化"))
}

/// 初始化真实 CAS、Genome、Stable 与 Episode Store。
async fn initialize(root: &Path) -> (GenomeRevision, EpisodeId) {
    let evolution_root = root.join("evolution");
    let artifacts = FileArtifactStore::new(evolution_root.join("artifacts"));
    let parent_policy = ContextPolicyV1::default();
    let policy_artifact = ContextPolicyRepository::new(&artifacts)
        .put(&parent_policy)
        .await
        .expect("Parent Policy 应写入 CAS");
    let parent = parent_revision(policy_artifact.digest);
    FileGenomeStore::new(evolution_root.join("genomes"))
        .append(&parent)
        .await
        .expect("Parent Revision 应登记");
    FileStableGenomePublisher::new(&evolution_root)
        .publish("stable/context", &parent, 1)
        .await
        .expect("Parent 应成为初始 Stable");
    let event_stream = artifacts
        .put("application/json", b"[]")
        .await
        .expect("Episode 事件制品应写入 CAS");
    let episode = episode(&parent, event_stream);
    let episode_id = episode.episode_id.clone();
    FileEpisodeStore::new(evolution_root.join("episodes"))
        .append(&episode)
        .await
        .expect("Episode 应登记");
    (parent, episode_id)
}

/// 三候选必须经过真实八指标 Gate、Archive、Stable 继承并在健康失败后回滚。
#[tokio::test]
async fn completes_context_cycle_and_rolls_back_unhealthy_policy() {
    let temp = TempDir::new().expect("应创建临时目录");
    let evolution_root = temp.path().join("evolution");
    let archive_root = temp.path().join("archive");
    let health_root = temp.path().join("health");
    let (parent, episode_id) = initialize(temp.path()).await;
    let fixture_version = DatasetVersionId::generate();
    let evaluator = TrustedContextEvaluator::new(
        evolution_root.clone(),
        archive_root.clone(),
        health_root,
        fixture_version.clone(),
    );
    let runner = ContextEvolutionCycle::new(&evolution_root, evaluator, fixture_version.clone());
    let request = agent_evolution::ContextEvolutionCycleRequestV1 {
        schema_version: CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION,
        cycle_id: agent_evolution_protocol::EvolutionCycleId::generate(),
        parent_revision_id: parent.revision_id.clone(),
        parent_genome_digest: parent.digest.clone(),
        lineage: "stable/context".to_string(),
        expected_parent_generation: 1,
        evidence_episode_ids: BTreeSet::from([episode_id]),
        expected_fixture_version: fixture_version,
        requested_at_ms: 100,
    };

    let promoted = runner
        .run(&request)
        .await
        .expect("Context Cycle 应完成 Promotion");
    assert_eq!(promoted.stage, ContextCycleStage::AwaitingHealth);
    assert_eq!(promoted.proposals.len(), 3);
    assert_eq!(promoted.candidates.len(), 3);
    assert_eq!(promoted.evaluation_receipts.len(), 3);
    assert_eq!(
        promoted
            .evaluation_receipts
            .iter()
            .filter(|receipt| receipt.context_report.decision == GateDecision::Pass)
            .count(),
        1
    );
    let rejected_pair = promoted
        .candidates
        .iter()
        .zip(&promoted.evaluation_receipts)
        .find(|(_, receipt)| receipt.context_report.decision == GateDecision::Reject)
        .expect("测试应包含被拒绝 Candidate");
    let mut forged_winner = promoted.clone();
    forged_winner.winner = Some(rejected_pair.0.candidate_id.clone());
    assert!(
        forged_winner.validate().is_err(),
        "Archive 必须拒绝把 Gate Reject Candidate 改写为 Winner"
    );
    let mut forged_release = promoted.clone();
    forged_release
        .release_receipt
        .as_mut()
        .expect("Promotion 回执应存在")
        .report_id = rejected_pair.1.report_id.clone();
    assert!(
        forged_release.validate().is_err(),
        "Archive 必须拒绝 Promotion 与 Winner Report 错绑"
    );
    for receipt in &promoted.evaluation_receipts {
        receipt
            .validate(M6_CONTEXT_GATE_VERSION)
            .expect("八指标 Context 回执应可复核");
        let metrics = &receipt.context_report.candidate_metrics;
        let _all_metrics = (
            metrics.fact_recall_bps,
            metrics.constraint_recall_bps,
            metrics.tool_state_recall_bps,
            metrics.plan_state_recall_bps,
            metrics.downstream_task_success_bps,
            metrics.token_reduction_bps,
            metrics.cost_microunits,
            metrics.latency_ms,
        );
        TrustedEvaluationArchive::new(&archive_root)
            .get_verified(&receipt.report_id)
            .await
            .expect("每个候选的正式报告与 Seal 都应保留");
    }

    let winner = promoted
        .winner
        .as_ref()
        .expect("应选择唯一 Gate Pass Candidate");
    let winner_candidate = promoted
        .candidates
        .iter()
        .find(|candidate| &candidate.candidate_id == winner)
        .expect("Winner 应属于候选集合");
    let resolver = FileGenomeResolver::new(&evolution_root);
    let stable_candidate = resolver
        .resolve(&GenomeSelector::Stable("stable/context".to_string()))
        .await
        .expect("新 Serve 应解析 promoted Genome");
    assert_eq!(
        stable_candidate.revision_id,
        winner_candidate.candidate_revision_id
    );
    let promoted_policy_digest = stable_candidate
        .genome
        .context_policy
        .as_ref()
        .expect("promoted Genome 应保留 Context Policy")
        .config_digest
        .clone();
    assert_eq!(
        promoted_policy_digest,
        winner_candidate.candidate_policy_digest
    );
    let promoted_policy =
        ContextPolicyRepository::new(&FileArtifactStore::new(evolution_root.join("artifacts")))
            .get(&promoted_policy_digest)
            .await
            .expect("Serve 应可从 promoted Genome 读取新参数");
    assert_ne!(promoted_policy, ContextPolicyV1::default());

    let rolled_back = runner
        .verify_health(&request.cycle_id)
        .await
        .expect("健康失败应自动回滚");
    assert_eq!(rolled_back.stage, ContextCycleStage::RolledBack);
    assert_eq!(
        rolled_back
            .health_receipt
            .as_ref()
            .map(|value| value.verified),
        Some(false)
    );
    assert!(rolled_back.rollback_receipt.is_some());
    assert_eq!(rolled_back.proposals.len(), 3);
    assert_eq!(rolled_back.candidates.len(), 3);
    assert_eq!(rolled_back.evaluation_receipts.len(), 3);
    let mut forged_health = rolled_back.clone();
    forged_health
        .health_receipt
        .as_mut()
        .expect("健康回执应存在")
        .request_id = "forged-context-health".to_string();
    assert!(
        forged_health.validate().is_err(),
        "Archive 必须拒绝 Health 回执与 Cycle 请求错绑"
    );
    let stable_parent = resolver
        .resolve(&GenomeSelector::Stable("stable/context".to_string()))
        .await
        .expect("Rollback 后 Stable 应可解析");
    assert_eq!(stable_parent.revision_id, parent.revision_id);
    let stable_ref = resolver
        .stable_reference("stable/context")
        .await
        .expect("Rollback Stable 引用应存在");
    assert_eq!(stable_ref.generation, 3);
    assert_eq!(
        runner
            .archive()
            .history(&request.cycle_id)
            .await
            .expect("完整只追加历史应可验证")
            .last(),
        Some(&rolled_back)
    );
    let mut terminal_extension = rolled_back.clone();
    terminal_extension.sequence += 1;
    terminal_extension.previous_digest = Some(
        agent_evolution::FileContextCycleArchive::snapshot_digest(&rolled_back)
            .expect("终态快照摘要应可计算"),
    );
    terminal_extension.stage = ContextCycleStage::Failed;
    terminal_extension.failure_code = Some("context_cycle_state_invalid".to_string());
    assert!(
        runner.archive().append(&terminal_extension).await.is_err(),
        "Archive 不得在终态后追加 Failed 快照"
    );
}
