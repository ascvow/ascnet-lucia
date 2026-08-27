//! Prompt Evolution Cycle 的文件 Store 集成测试。
//!
//! 测试使用真实 Artifact、Episode、Outbox、Issue Observation、Genome、Stable 与 Cycle
//! Store，只用可控 Evaluator 替身隔离独立进程边界。

use agent_evolution::{
    ArtifactStore, DeterministicPromptMutationGenerator, EpisodeStore, EvaluatorClient,
    EvaluatorProcessError, EvolutionCycleStore, EvolutionOutbox, EvolutionOutboxItem,
    FileArtifactStore, FileEpisodeStore, FileEvolutionCycleStore, FileEvolutionOutbox,
    FileGenomeResolver, FileGenomeStore, FileIssueObservationStore, FileStableGenomePublisher,
    GenomeResolver, GenomeSelector, GenomeStore, IssueObservation, IssueObservationStore,
    PromptCycleError, PromptEvolutionCycle, EVOLUTION_POLICY_VERSION,
    TASK_STRATEGY_MVP_CANDIDATE_COUNT,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ArtifactRef, AttributionMethod, AuditRecordId, DataClass,
    DatasetVersionId, DiagnosticStatus, Episode, EpisodeDataPolicy, EpisodeId, EvaluationReceiptV1,
    EvaluationReportId, EvaluationRequestV1, EvolutionCycleRequestInput, EvolutionCycleRequestV1,
    EvolutionCycleSnapshotV1, EvolutionCycleStage, EvolutionEligibility, EvolutionIssueId,
    EvolutionLifecycle, FailureAttribution, FailureClassification, FailureDisposition, FailureKind,
    FailureRecord, FailureRecordId, GateDecision, GenomeMetadata, GenomeRevision, GenomeRevisionId,
    HealthCheckReceiptV1, HealthCheckRequestV1, ModelGenome, Outcome, PromptArtifactRef,
    PromptGenome, PromptLayer, ReleaseReceiptV1, ReplayabilityGrade, RollbackRequestV1, RunId,
    RuntimeIdentity, TaskDescriptor, ToolProfileGenome, UsageSummary, EPISODE_SCHEMA_VERSION,
    EVALUATION_RECEIPT_SCHEMA_VERSION, GENOME_SCHEMA_VERSION, HEALTH_RECEIPT_SCHEMA_VERSION,
    RELEASE_RECEIPT_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// 集成测试使用的固定 Stable lineage。
const TEST_LINEAGE: &str = "stable/prompt-cycle";

/// 创建不会与并发测试冲突的绝对临时目录。
fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lucia-prompt-cycle-{label}-{}",
        Uuid::new_v4().simple()
    ))
}

/// 构造固定 SHA-256 Artifact 摘要。
fn artifact_digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 返回已完成脱敏且明确允许进入进化的 Episode 策略。
fn eligible_policy() -> EpisodeDataPolicy {
    let mut policy = EpisodeDataPolicy::for_class(DataClass::Internal);
    policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    policy.redaction_rules_version = Some("redaction-v1".to_string());
    policy
}

/// 构造绑定 Parent Genome 的可信失败 Episode。
fn episode(
    episode_id: EpisodeId,
    parent_revision_id: GenomeRevisionId,
    event_stream_ref: ArtifactRef,
) -> Episode {
    Episode {
        schema_version: EPISODE_SCHEMA_VERSION,
        episode_id,
        run_id: RunId::generate(),
        session_id: "prompt-cycle-test-session".to_string(),
        genome_revision_id: parent_revision_id,
        task: TaskDescriptor {
            family: "code-edit".to_string(),
            input_ref: None,
            tags: BTreeSet::from(["verification".to_string()]),
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
        data_policy: eligible_policy(),
        event_count: 0,
        started_at_ms: 10,
        finished_at_ms: 20,
    }
}

/// 构造可在单次观察后进入进化的确定性失败记录。
fn failure_record(episode_id: EpisodeId) -> FailureRecord {
    FailureRecord {
        record_id: FailureRecordId::generate(),
        episode_id,
        attribution: FailureAttribution {
            detected_at: agent_evolution_protocol::EventId::generate(),
            suspected_origin: None,
            propagation_path: Vec::new(),
            decisive_step: None,
            failure_class: FailureKind::VerificationFailure,
            confidence: 1.0,
            evidence: Vec::new(),
            method: AttributionMethod::DeterministicRule,
        },
        status: DiagnosticStatus::EligibleForEvolution,
    }
}

/// 构造绑定 Issue 与 Episode 的待消费 Outbox 记录。
fn outbox_item(episode_id: EpisodeId, issue_id: EvolutionIssueId) -> EvolutionOutboxItem {
    EvolutionOutboxItem {
        outbox_id: format!("cycle-{}", Uuid::new_v4().simple()),
        episode_id,
        outcome: Outcome::TaskFailure,
        disposition: FailureDisposition::EvolutionCandidate,
        issue_id: Some(issue_id),
        issue_status: DiagnosticStatus::EligibleForEvolution,
        created_at_ms: 20,
        consumed: false,
    }
}

/// 构造包含唯一 Task Strategy Prompt 的最小合法 Parent Genome。
fn parent_revision(prompt: ArtifactDigest) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".to_string(),
                git_commit: "prompt-cycle-test".to_string(),
                git_dirty: false,
                target_triple: "test-target".to_string(),
                features: BTreeSet::new(),
            },
            model: ModelGenome {
                provider: "test".to_string(),
                provider_kind: "test".to_string(),
                model: "fixture".to_string(),
                base_url: None,
                protocol: None,
                max_tokens: Some(64),
                temperature: None,
                stream: false,
                provider_options_digest: None,
            },
            prompt: PromptGenome {
                messages: vec![PromptArtifactRef {
                    layer: PromptLayer::TaskStrategy,
                    artifact: prompt,
                }],
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
    .expect("Parent Genome 应合法")
}

/// Evaluator 替身的可审计调用轨迹和发布绑定。
#[derive(Debug, Default)]
struct FakeEvaluatorState {
    evaluation_requests: Vec<String>,
    candidate_positions: BTreeMap<String, usize>,
    report_candidates: BTreeMap<String, GenomeRevisionId>,
    promotion_receipts: BTreeMap<String, ReleaseReceiptV1>,
    promotion_requests: Vec<String>,
    health_requests: Vec<String>,
    rollback_requests: Vec<String>,
    failed_second_evaluation: bool,
}

/// 使用真实 Artifact、Genome 与 Stable Store 的可控 Evaluator 替身。
#[derive(Debug, Clone)]
struct FakeEvaluator {
    evolution_root: PathBuf,
    health_verified: bool,
    fail_second_evaluation_once: bool,
    state: Arc<Mutex<FakeEvaluatorState>>,
}

impl FakeEvaluator {
    /// 创建指定健康结论和第二次评测故障策略的 Evaluator 替身。
    fn new(
        evolution_root: PathBuf,
        health_verified: bool,
        fail_second_evaluation_once: bool,
    ) -> Self {
        Self {
            evolution_root,
            health_verified,
            fail_second_evaluation_once,
            state: Arc::new(Mutex::new(FakeEvaluatorState::default())),
        }
    }

    /// 返回按真实调用顺序记录的 Evaluate 请求 ID。
    fn evaluation_requests(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("Evaluator 测试状态锁不应中毒")
            .evaluation_requests
            .clone()
    }

    /// 返回 Promotion、Health 与 Rollback 的调用次数。
    fn release_call_counts(&self) -> (usize, usize, usize) {
        let state = self.state.lock().expect("Evaluator 测试状态锁不应中毒");
        (
            state.promotion_requests.len(),
            state.health_requests.len(),
            state.rollback_requests.len(),
        )
    }
}

#[async_trait]
impl EvaluatorClient for FakeEvaluator {
    /// 归档 A Hidden Reject、B Safety Reject、C AutoPromote 三类确定性正式回执。
    async fn evaluate(
        &self,
        request: &EvaluationRequestV1,
    ) -> Result<EvaluationReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let position = {
            let mut state = self.state.lock().expect("Evaluator 测试状态锁不应中毒");
            state.evaluation_requests.push(request.request_id.clone());
            let next_position = state.candidate_positions.len();
            let position = *state
                .candidate_positions
                .entry(request.request_id.clone())
                .or_insert(next_position);
            if self.fail_second_evaluation_once && position == 1 && !state.failed_second_evaluation
            {
                state.failed_second_evaluation = true;
                return Err(EvaluatorProcessError::EvaluatorRejected {
                    code: "transient_test_failure".to_string(),
                    status: Some(75),
                });
            }
            position
        };
        let (suite, gate_decision, lifecycle) = match position {
            0 => ("hidden", GateDecision::Reject, EvolutionLifecycle::Rejected),
            1 => ("safety", GateDecision::Reject, EvolutionLifecycle::Rejected),
            _ => (
                "auto-promote",
                GateDecision::Pass,
                EvolutionLifecycle::Eligible,
            ),
        };
        let report = serde_json::to_vec(&serde_json::json!({
            "request_id": request.request_id,
            "suite": suite,
        }))
        .expect("测试报告应可序列化");
        let report_ref = FileArtifactStore::new(self.evolution_root.join("artifacts"))
            .put("application/json", &report)
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!("测试报告归档失败：{error}"))
            })?;
        let receipt = EvaluationReceiptV1 {
            schema_version: EVALUATION_RECEIPT_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            report_id: EvaluationReportId::generate(),
            report_digest: report_ref.digest,
            audit_record_id: AuditRecordId::generate(),
            audit_head_digest: artifact_digest('f'),
            parent_revision_id: request.parent_revision_id.clone(),
            candidate_revision_id: request.candidate_revision_id.clone(),
            evaluation_policy_version: "evaluation-policy-v1".to_string(),
            commit_policy_version: "commit-policy-v1".to_string(),
            verifier_set_digest: artifact_digest('e').to_string(),
            gate_decision,
            lifecycle,
        };
        self.state
            .lock()
            .expect("Evaluator 测试状态锁不应中毒")
            .report_candidates
            .insert(
                receipt.report_id.to_string(),
                request.candidate_revision_id.clone(),
            );
        Ok(receipt)
    }

    /// 使用真实 Stable Publisher 把 AutoPromote 报告绑定的 Candidate 原子发布。
    async fn promote(
        &self,
        request: &agent_evolution_protocol::PromotionRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let candidate_revision_id = self
            .state
            .lock()
            .expect("Evaluator 测试状态锁不应中毒")
            .report_candidates
            .get(request.report_id.as_str())
            .cloned()
            .ok_or_else(|| {
                EvaluatorProcessError::InvalidReceiptBinding(
                    "Promotion 报告没有绑定 Candidate".to_string(),
                )
            })?;
        let publisher = FileStableGenomePublisher::new(&self.evolution_root);
        let current = publisher
            .resolver()
            .stable_reference(TEST_LINEAGE)
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!(
                    "读取 Promotion 前 Stable 失败：{error}"
                ))
            })?;
        let candidate = publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(candidate_revision_id))
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!(
                    "读取 Promotion Candidate 失败：{error}"
                ))
            })?;
        let generation = current.generation.checked_add(1).ok_or_else(|| {
            EvaluatorProcessError::InvalidReceiptBinding("Stable 代数溢出".to_string())
        })?;
        let published = publisher
            .publish_bound(
                &current,
                &candidate,
                generation,
                request.release_id.clone(),
                request.report_id.clone(),
                None,
            )
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!(
                    "测试 Promotion 失败：{error}"
                ))
            })?;
        let receipt = ReleaseReceiptV1 {
            schema_version: RELEASE_RECEIPT_SCHEMA_VERSION,
            release_id: request.release_id.clone(),
            report_id: request.report_id.clone(),
            lineage: TEST_LINEAGE.to_string(),
            from: current.revision_id,
            to: published.revision_id,
            generation: published.generation,
            audit_record_id: AuditRecordId::generate(),
            rollback_of: None,
        };
        let mut state = self.state.lock().expect("Evaluator 测试状态锁不应中毒");
        state
            .promotion_requests
            .push(request.release_id.to_string());
        state
            .promotion_receipts
            .insert(request.release_id.to_string(), receipt.clone());
        Ok(receipt)
    }

    /// 从真实 Stable 引用构造可控的可信健康回执。
    async fn health(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        self.state
            .lock()
            .expect("Evaluator 测试状态锁不应中毒")
            .health_requests
            .push(request.request_id.clone());
        let stable = FileGenomeResolver::new(&self.evolution_root)
            .stable_reference(TEST_LINEAGE)
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!(
                    "读取健康验证 Stable 失败：{error}"
                ))
            })?;
        let stable_reference_verified = stable.lineage == request.lineage
            && stable.revision_id == request.expected_revision_id
            && stable.generation == request.expected_generation
            && stable.release_id.as_ref() == Some(&request.release_id);
        let checks_passed = u32::from(self.health_verified);
        let observation = FileArtifactStore::new(self.evolution_root.join("artifacts"))
            .put(
                "application/json",
                format!(
                    "{{\"release_id\":\"{}\",\"verified\":{}}}",
                    request.release_id, self.health_verified
                )
                .as_bytes(),
            )
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!("健康观察归档失败：{error}"))
            })?;
        let receipt = HealthCheckReceiptV1 {
            schema_version: HEALTH_RECEIPT_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            release_id: request.release_id.clone(),
            lineage: request.lineage.clone(),
            expected_revision_id: request.expected_revision_id.clone(),
            observed_revision_id: stable.revision_id,
            expected_generation: request.expected_generation,
            observed_generation: stable.generation,
            checks_passed,
            checks_total: 1,
            observation_digest: observation.digest,
            stable_reference_verified,
            verified: stable_reference_verified && checks_passed == 1,
        };
        receipt.validate().map_err(|error| {
            EvaluatorProcessError::InvalidReceiptBinding(format!("健康回执不合法：{error}"))
        })?;
        Ok(receipt)
    }

    /// 使用真实 Stable Publisher 把健康失败的 Candidate 原子回滚到 Parent。
    async fn rollback(
        &self,
        request: &RollbackRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let promotion = self
            .state
            .lock()
            .expect("Evaluator 测试状态锁不应中毒")
            .promotion_receipts
            .get(request.release_id.as_str())
            .cloned()
            .ok_or_else(|| {
                EvaluatorProcessError::InvalidReceiptBinding(
                    "Rollback 找不到原 Promotion".to_string(),
                )
            })?;
        let publisher = FileStableGenomePublisher::new(&self.evolution_root);
        let current = publisher
            .resolver()
            .stable_reference(TEST_LINEAGE)
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!(
                    "读取 Rollback 前 Stable 失败：{error}"
                ))
            })?;
        let parent = publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(promotion.from.clone()))
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!(
                    "读取 Rollback Parent 失败：{error}"
                ))
            })?;
        let generation = current.generation.checked_add(1).ok_or_else(|| {
            EvaluatorProcessError::InvalidReceiptBinding("Stable 代数溢出".to_string())
        })?;
        let published = publisher
            .publish_bound(
                &current,
                &parent,
                generation,
                request.rollback_release_id.clone(),
                promotion.report_id.clone(),
                Some(request.release_id.clone()),
            )
            .await
            .map_err(|error| {
                EvaluatorProcessError::InvalidReceiptBinding(format!("测试 Rollback 失败：{error}"))
            })?;
        let receipt = ReleaseReceiptV1 {
            schema_version: RELEASE_RECEIPT_SCHEMA_VERSION,
            release_id: request.rollback_release_id.clone(),
            report_id: promotion.report_id,
            lineage: TEST_LINEAGE.to_string(),
            from: current.revision_id,
            to: published.revision_id,
            generation: published.generation,
            audit_record_id: AuditRecordId::generate(),
            rollback_of: Some(request.release_id.clone()),
        };
        self.state
            .lock()
            .expect("Evaluator 测试状态锁不应中毒")
            .rollback_requests
            .push(request.rollback_release_id.to_string());
        Ok(receipt)
    }
}

/// 单个 Prompt Cycle 测试所需的真实 Store 和 Runner。
struct CycleHarness {
    root: PathBuf,
    request: EvolutionCycleRequestV1,
    parent_revision_id: GenomeRevisionId,
    outbox: FileEvolutionOutbox,
    evaluator: FakeEvaluator,
    runner: PromptEvolutionCycle<DeterministicPromptMutationGenerator, FakeEvaluator>,
}

impl CycleHarness {
    /// 删除本测试产生的独立临时 Store 根。
    async fn cleanup(self) {
        let _ = tokio::fs::remove_dir_all(self.root).await;
    }
}

/// 建立含一个合格失败 Episode、Parent Genome 和 Stable 引用的完整文件 Store 夹具。
async fn setup_cycle(
    label: &str,
    health_verified: bool,
    fail_second_evaluation_once: bool,
) -> CycleHarness {
    let root = temp_root(label);
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    let parent_prompt = artifacts
        .put("text/plain", "先完成任务，再报告可验证结果。".as_bytes())
        .await
        .expect("应写入 Parent Prompt");
    let event_stream = artifacts
        .put("application/x-ndjson", b"")
        .await
        .expect("应写入空事件流");
    let parent = parent_revision(parent_prompt.digest);
    let resolver = FileGenomeResolver::new(&root);
    resolver
        .store()
        .append(&parent)
        .await
        .expect("应登记 Parent Genome");
    FileStableGenomePublisher::new(&root)
        .publish(TEST_LINEAGE, &parent, 1)
        .await
        .expect("应发布初始 Stable");

    let episode_id = EpisodeId::generate();
    FileEpisodeStore::new(root.join("episodes"))
        .append(&episode(
            episode_id.clone(),
            parent.revision_id.clone(),
            event_stream,
        ))
        .await
        .expect("应追加 Episode");
    let issue_id = EvolutionIssueId::generate();
    FileIssueObservationStore::new(root.join("issue-observations"))
        .append(&IssueObservation::new(
            issue_id.clone(),
            episode_id.clone(),
            &parent.digest,
            failure_record(episode_id.clone()),
        ))
        .await
        .expect("应追加 Issue Observation");
    let outbox = FileEvolutionOutbox::new(root.join("outbox"));
    outbox
        .append(&outbox_item(episode_id.clone(), issue_id.clone()))
        .await
        .expect("应追加 Outbox");
    let request = EvolutionCycleRequestV1::create(EvolutionCycleRequestInput {
        issue_id,
        parent_revision_id: parent.revision_id.clone(),
        parent_genome_digest: parent.digest,
        lineage: TEST_LINEAGE.to_string(),
        expected_parent_generation: 1,
        source_episode_ids: vec![episode_id],
        evolution_policy_version: EVOLUTION_POLICY_VERSION.to_string(),
        candidate_count: TASK_STRATEGY_MVP_CANDIDATE_COUNT as u32,
        requested_at_ms: 1_000,
    })
    .expect("Cycle 请求应合法");
    let evaluator = FakeEvaluator::new(root.clone(), health_verified, fail_second_evaluation_once);
    let runner = PromptEvolutionCycle::new(
        root.clone(),
        DeterministicPromptMutationGenerator,
        evaluator.clone(),
        DatasetVersionId::generate(),
    );
    CycleHarness {
        root,
        request,
        parent_revision_id: parent.revision_id,
        outbox,
        evaluator,
        runner,
    }
}

/// 从独立 Store 重新读取并验证全部 Candidate、Report 与 Cycle 快照。
async fn assert_archived_artifacts(harness: &CycleHarness, snapshot: &EvolutionCycleSnapshotV1) {
    assert_eq!(snapshot.candidates.len(), 3);
    assert_eq!(snapshot.evaluation_receipts.len(), 3);
    let genomes = FileGenomeStore::new(harness.root.join("genomes"));
    let artifacts = FileArtifactStore::new(harness.root.join("artifacts"));
    let expected_suites = ["hidden", "safety", "auto-promote"];
    for ((candidate, receipt), expected_suite) in snapshot
        .candidates
        .iter()
        .zip(&snapshot.evaluation_receipts)
        .zip(expected_suites)
    {
        assert_eq!(
            candidate.candidate_revision_id,
            receipt.candidate_revision_id
        );
        assert!(genomes
            .get(&candidate.candidate_revision_id)
            .await
            .expect("应读取 Candidate Genome")
            .is_some());
        let report = artifacts
            .get(&receipt.report_digest)
            .await
            .expect("应读取 Evaluation Report")
            .expect("Evaluation Report 应已归档");
        let report: serde_json::Value =
            serde_json::from_slice(&report).expect("Evaluation Report 应为 JSON");
        assert_eq!(report["suite"], expected_suite);
    }
    let cycles = FileEvolutionCycleStore::new(harness.root.join("cycles"));
    let history = cycles
        .history(&snapshot.cycle_id)
        .await
        .expect("应重新读取 Cycle 历史");
    assert!(!history.is_empty());
    assert_eq!(history.last(), Some(snapshot));
}

/// 第二次 Evaluate 瞬时失败后应从 Receipt 前缀恢复，并保持 AwaitingHealth 幂等。
#[tokio::test]
async fn resumes_remaining_evaluations_and_keeps_run_idempotent() {
    let harness = setup_cycle("resume", true, true).await;
    let error = harness
        .runner
        .run(&harness.request)
        .await
        .expect_err("第二次 Evaluate 首次调用应失败");
    assert!(matches!(
        error,
        PromptCycleError::Evaluator(EvaluatorProcessError::EvaluatorRejected { ref code, .. })
            if code == "transient_test_failure"
    ));
    let interrupted = harness
        .runner
        .cycle_store()
        .latest(&harness.request.cycle_id)
        .await
        .expect("应读取中断快照")
        .expect("中断快照应存在");
    assert_eq!(interrupted.stage, EvolutionCycleStage::Evaluating);
    assert_eq!(interrupted.candidates.len(), 3);
    assert_eq!(interrupted.evaluation_receipts.len(), 1);
    assert_eq!(
        harness.outbox.pending().await.expect("应读取 Outbox").len(),
        1
    );

    let resumed = harness
        .runner
        .run(&harness.request)
        .await
        .expect("重跑应只补齐剩余评测并完成 Promotion");
    assert_eq!(resumed.stage, EvolutionCycleStage::AwaitingHealth);
    assert_eq!(resumed.evaluation_receipts.len(), 3);
    assert_eq!(
        resumed
            .evaluation_receipts
            .iter()
            .map(|receipt| (&receipt.gate_decision, &receipt.lifecycle))
            .collect::<Vec<_>>(),
        vec![
            (&GateDecision::Reject, &EvolutionLifecycle::Rejected),
            (&GateDecision::Reject, &EvolutionLifecycle::Rejected),
            (&GateDecision::Pass, &EvolutionLifecycle::Eligible),
        ]
    );
    assert_eq!(
        resumed.winner.as_ref(),
        Some(&resumed.candidates[2].candidate_id)
    );
    let evaluation_requests = harness.evaluator.evaluation_requests();
    assert_eq!(evaluation_requests.len(), 4);
    assert_eq!(
        evaluation_requests
            .iter()
            .filter(|request_id| *request_id == &evaluation_requests[0])
            .count(),
        1,
        "已归档的第一个 Receipt 不得重复评测"
    );
    assert_eq!(evaluation_requests[1], evaluation_requests[2]);
    assert_ne!(evaluation_requests[2], evaluation_requests[3]);
    assert_eq!(
        harness.outbox.pending().await.expect("应读取 Outbox").len(),
        1
    );
    assert_archived_artifacts(&harness, &resumed).await;

    let history_len = harness
        .runner
        .cycle_store()
        .history(&harness.request.cycle_id)
        .await
        .expect("应读取 Cycle 历史")
        .len();
    let calls_before = (
        harness.evaluator.evaluation_requests(),
        harness.evaluator.release_call_counts(),
    );
    let repeated = harness
        .runner
        .run(&harness.request)
        .await
        .expect("AwaitingHealth 的相同 run 应幂等返回");
    assert_eq!(repeated, resumed);
    assert_eq!(
        harness
            .runner
            .cycle_store()
            .history(&harness.request.cycle_id)
            .await
            .expect("应读取幂等后的 Cycle 历史")
            .len(),
        history_len
    );
    assert_eq!(
        (
            harness.evaluator.evaluation_requests(),
            harness.evaluator.release_call_counts(),
        ),
        calls_before
    );

    let verified = harness
        .runner
        .verify_health(&harness.request.cycle_id)
        .await
        .expect("健康成功应完成 Cycle");
    assert_eq!(verified.stage, EvolutionCycleStage::HealthVerified);
    assert_eq!(
        verified.health_receipt.as_ref().map(|value| value.verified),
        Some(true)
    );
    assert!(verified.rollback_receipt.is_none());
    assert!(harness
        .outbox
        .pending()
        .await
        .expect("应读取 Outbox")
        .is_empty());
    harness.cleanup().await;
}

/// 健康失败应自动回滚 Parent，并在消费 Outbox 前归档全部阶段制品。
#[tokio::test]
async fn rolls_back_unhealthy_promotion_and_archives_all_receipts() {
    let harness = setup_cycle("rollback", false, false).await;
    let promoted = harness
        .runner
        .run(&harness.request)
        .await
        .expect("三份正式回执应完成 Promotion");
    assert_eq!(promoted.stage, EvolutionCycleStage::AwaitingHealth);
    assert_eq!(
        harness.outbox.pending().await.expect("应读取 Outbox").len(),
        1
    );

    let rolled_back = harness
        .runner
        .verify_health(&harness.request.cycle_id)
        .await
        .expect("健康失败应自动回滚");
    assert_eq!(rolled_back.stage, EvolutionCycleStage::RolledBack);
    assert_eq!(rolled_back.candidates.len(), 3);
    assert_eq!(rolled_back.evaluation_receipts.len(), 3);
    let promotion = rolled_back
        .release_receipt
        .as_ref()
        .expect("应归档 Promotion Receipt");
    let health = rolled_back
        .health_receipt
        .as_ref()
        .expect("应归档 Health Receipt");
    let rollback = rolled_back
        .rollback_receipt
        .as_ref()
        .expect("应归档 Rollback Receipt");
    assert!(!health.verified);
    assert_eq!(health.release_id, promotion.release_id);
    assert_eq!(rollback.rollback_of.as_ref(), Some(&promotion.release_id));
    assert_eq!(rollback.from, promotion.to);
    assert_eq!(rollback.to, harness.parent_revision_id);
    assert_eq!(harness.evaluator.release_call_counts(), (1, 1, 1));
    assert!(harness
        .outbox
        .pending()
        .await
        .expect("应读取 Outbox")
        .is_empty());
    let stable = FileGenomeResolver::new(&harness.root)
        .stable_reference(TEST_LINEAGE)
        .await
        .expect("应读取回滚后的 Stable");
    assert_eq!(stable.revision_id, harness.parent_revision_id);
    assert_eq!(stable.generation, 3);
    assert_eq!(stable.rollback_of.as_ref(), Some(&promotion.release_id));
    assert_archived_artifacts(&harness, &rolled_back).await;
    let stages = harness
        .runner
        .cycle_store()
        .history(&harness.request.cycle_id)
        .await
        .expect("应读取 Cycle 历史")
        .into_iter()
        .map(|snapshot| snapshot.stage)
        .collect::<BTreeSet<_>>();
    for stage in [
        EvolutionCycleStage::AwaitingHealth,
        EvolutionCycleStage::VerifyingHealth,
        EvolutionCycleStage::RollingBack,
        EvolutionCycleStage::RolledBack,
    ] {
        assert!(stages.contains(&stage), "缺少阶段归档：{stage:?}");
    }
    harness.cleanup().await;
}

/// 确定性 Evidence 绑定错误应进入 Failed，且不得消费原始 Outbox。
#[tokio::test]
async fn failed_cycle_does_not_consume_outbox() {
    let mut harness = setup_cycle("failed", true, false).await;
    harness.request.source_episode_ids = vec![EpisodeId::generate()];
    harness
        .request
        .validate()
        .expect("错绑请求本身仍应结构合法");
    let error = harness
        .runner
        .run(&harness.request)
        .await
        .expect_err("Evidence 绑定错误应失败关闭");
    assert!(matches!(error, PromptCycleError::EvidenceBindingMismatch));
    let failed = harness
        .runner
        .cycle_store()
        .latest(&harness.request.cycle_id)
        .await
        .expect("应读取 Failed 快照")
        .expect("Failed 快照应存在");
    assert_eq!(failed.stage, EvolutionCycleStage::Failed);
    assert_eq!(
        failed.failure_code.as_deref(),
        Some("evidence_binding_mismatch")
    );
    assert!(harness.evaluator.evaluation_requests().is_empty());
    assert_eq!(
        harness.outbox.pending().await.expect("应读取 Outbox").len(),
        1
    );

    let repeated = harness
        .runner
        .run(&harness.request)
        .await
        .expect("Failed 的相同 run 应幂等读取");
    assert_eq!(repeated, failed);
    assert_eq!(
        harness.outbox.pending().await.expect("应读取 Outbox").len(),
        1
    );
    harness.cleanup().await;
}
