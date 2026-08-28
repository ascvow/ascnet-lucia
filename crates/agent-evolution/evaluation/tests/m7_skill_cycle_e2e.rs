//! M7 Skill 生产 Cycle 的真实 Exit Gate、Stable 发布与健康回滚证据。

use agent_core::{AgentEvent, AgentEventKind, EventSink};
use agent_evaluation::{SkillActivationAuthorizationV1, SkillExitGate, SkillExitGateOutcomeV1};
use agent_evolution::{
    collect_trusted_skill_evaluation_bindings, EpisodeRecorder, EpisodeRecorderConfig,
    EvolutionOutbox, EvolutionOutboxItem, FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox,
    FileGenomeResolver, FileSkillEvolutionCycleArchive, FileStableGenomePublisher, GenomeResolver,
    GenomeSelector, GenomeStore, MutationEpisodeEvidence, MutationEvidence,
    MutationFailureEvidence, SkillArtifactRepository, SkillContentDraftV1, SkillEvolutionCycle,
    SkillEvolutionCycleRequestV1, SkillEvolutionCycleResultV1, SkillEvolutionCycleStage,
    SkillEvolutionDispositionV1, SkillEvolutionOrchestrator, SkillEvolutionOrchestratorError,
    SkillGateCycleOutcomeV1, SkillGatePromotionV1, SkillHealthVerdictV1,
    SkillMutationDraftOperationV1, SkillMutationDraftV1, SkillMutationGenerationError,
    SkillMutationGenerator, SkillMutationRequestV1, StableGenomeRef, NATIVE_SKILL_READ_TOOL,
    NATIVE_SKILL_USAGE_SCHEMA_VERSION, SKILL_EVOLUTION_CANDIDATE_COUNT,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, DiagnosticStatus, EvaluationReportId, FailureDisposition,
    FailureKind, GenomeMetadata, GenomeRevision, ModelGenome, Outcome, OutcomeRevisionId,
    PluginGenome, PromptGenome, ReplayabilityGrade, RuntimeIdentity, SkillCandidateV1, SkillId,
    SkillStatusV1, SkillTriggerPolicyV1, SkillUsageEvidenceSourceV1, SkillUsageObservationV1,
    SkillUsageResultV1, ToolProfileGenome, UsageSummary, GENOME_SCHEMA_VERSION,
    SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tempfile::TempDir;

/// 返回固定测试摘要，避免依赖工作区外部制品。
fn digest(character: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 创建具备 Skill 只读能力且执行策略固定为 Serve 的 Parent Genome。
fn parent_genome() -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "m7-skill-cycle".into(),
                git_dirty: false,
                target_triple: "test-target".into(),
                features: BTreeSet::new(),
            },
            model: ModelGenome {
                provider: "fixture".into(),
                provider_kind: "fixture".into(),
                model: "deterministic".into(),
                base_url: None,
                protocol: None,
                max_tokens: Some(512),
                temperature: None,
                stream: false,
                provider_options_digest: None,
            },
            prompt: PromptGenome::default(),
            plugins: vec![PluginGenome {
                id: "skill".into(),
                version: "0.1.0".into(),
                api_version: "0.7.0".into(),
                bundle: digest('a'),
                manifest_digest: Some(digest('b')),
                config_digest: None,
                capability_profile_digest: Some(digest('c')),
                load_order: Some(0),
                hook_order: Vec::new(),
            }],
            capability_owners: BTreeMap::from([("episode.read_redacted".into(), "skill".into())]),
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

/// 创建一份只使用 Parent 既有能力的 Create 草案。
fn create_draft(skill_id: &str, suffix: &str) -> SkillMutationDraftV1 {
    SkillMutationDraftV1 {
        hypothesis: format!("候选 {suffix} 可修复稳定验证失败"),
        operation: SkillMutationDraftOperationV1::Create {
            skill: SkillContentDraftV1 {
                skill_id: SkillId::new(skill_id).expect("测试 Skill ID 应合法"),
                name: format!("可信复核 {suffix}"),
                description: format!("执行 {suffix} 的脱敏证据复核"),
                instructions: format!("只读取脱敏证据并完成 {suffix} 的确定性复核。"),
                trigger_policy: SkillTriggerPolicyV1::default(),
                required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            },
        },
    }
}

/// 返回固定三候选的离线生成器。
struct ScriptedSkillGenerator {
    drafts: Vec<SkillMutationDraftV1>,
}

#[async_trait]
impl SkillMutationGenerator for ScriptedSkillGenerator {
    /// 返回固定草案，并断言 Cycle 没有放宽候选数量或变异表面。
    async fn generate(
        &self,
        request: SkillMutationRequestV1<'_>,
    ) -> Result<Vec<SkillMutationDraftV1>, SkillMutationGenerationError> {
        assert_eq!(request.candidate_count, SKILL_EVOLUTION_CANDIDATE_COUNT);
        assert_eq!(
            request.mutation_surface,
            agent_evolution_protocol::MutationSurface::Skill
        );
        Ok(self.drafts.clone())
    }
}

/// 使用真实 Recorder 写入原生 `skill_read` 工具终态，并返回 Episode ID。
async fn record_evaluation_episode(
    artifacts: Arc<FileArtifactStore>,
    episodes: Arc<FileEpisodeStore>,
    candidate_revision: &GenomeRevision,
    candidate: &SkillCandidateV1,
) -> agent_evolution_protocol::EpisodeId {
    let config = EpisodeRecorderConfig::online(
        format!("m7-skill-cycle-{}", candidate.candidate_id),
        candidate_revision.revision_id.clone(),
    );
    let run_id = config.run_id.to_string();
    let recorder = EpisodeRecorder::new(config, artifacts, episodes);
    recorder
        .record(&AgentEvent::new(
            &run_id,
            AgentEventKind::RunStarted,
            0,
            json!({}),
        ))
        .await
        .expect("应记录评测运行开始");
    for (index, (skill_id, artifact_digest)) in
        candidate.candidate_artifact_digests.iter().enumerate()
    {
        let call_id = format!("evaluation-call-{index}");
        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::ToolFinished,
                index + 1,
                json!({
                    "call_id": call_id,
                    "name": NATIVE_SKILL_READ_TOOL,
                    "is_error": false,
                    "runtime_origin": "native",
                    "details": {
                        "skill_usage": {
                            "schema_version": NATIVE_SKILL_USAGE_SCHEMA_VERSION,
                            "skill_id": skill_id,
                            "artifact_digest": artifact_digest,
                            "genome_revision_id": candidate_revision.revision_id,
                            "genome_digest": candidate_revision.digest
                        }
                    },
                    "content": "评测正文必须由 Recorder 丢弃"
                }),
            ))
            .await
            .expect("应记录原生 Skill 工具终态");
    }
    recorder
        .record(&AgentEvent::new(
            &run_id,
            AgentEventKind::RunFinished,
            candidate.candidate_artifact_digests.len() + 1,
            json!({"steps_used": candidate.candidate_artifact_digests.len()}),
        ))
        .await
        .expect("应收敛评测 Episode");
    recorder.episode_id().await.expect("应产生评测 Episode")
}

/// 把真实 `SkillExitGate` 适配为生产 Cycle 所需的独立端口。
struct RealSkillGateOrchestrator {
    evolution_root: PathBuf,
    authorization: SkillActivationAuthorizationV1,
    health: SkillHealthVerdictV1,
    /// 记录 Gate 调用次数，供恢复测试确认已归档候选不被重复评测。
    evaluation_attempts: Option<Arc<AtomicUsize>>,
    /// 在指定 Gate 调用序号进入真实 Evaluator 前模拟进程中断。
    interrupt_on_attempt: Option<usize>,
}

impl RealSkillGateOrchestrator {
    /// 返回不泄露正文的激活授权证据 ID。
    fn authorization_evidence_id(authorization: &SkillActivationAuthorizationV1) -> String {
        match authorization {
            SkillActivationAuthorizationV1::LocalEvaluation => "local-evaluation".into(),
            SkillActivationAuthorizationV1::Approved { approval_id } => approval_id.clone(),
            SkillActivationAuthorizationV1::CanaryPassed { canary_report_id } => {
                canary_report_id.to_string()
            }
        }
    }
}

#[async_trait]
impl SkillEvolutionOrchestrator for RealSkillGateOrchestrator {
    /// 记录真实评测 Episode，构造可信使用绑定，再调用独立 `SkillExitGate` 完成 Q→E→A。
    async fn evaluate_and_promote(
        &self,
        candidate: &SkillCandidateV1,
        evaluated_at_ms: u64,
        activated_at_ms: u64,
    ) -> Result<SkillGateCycleOutcomeV1, SkillEvolutionOrchestratorError> {
        if let Some(attempts) = &self.evaluation_attempts {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.interrupt_on_attempt == Some(attempt) {
                return Err(SkillEvolutionOrchestratorError::new(
                    "skill_evaluator_interrupted",
                ));
            }
        }
        let genomes = FileGenomeResolver::new(&self.evolution_root);
        let candidate_revision = genomes
            .store()
            .get(&candidate.candidate_revision_id)
            .await
            .map_err(|_| SkillEvolutionOrchestratorError::new("candidate_store_read_failed"))?
            .ok_or_else(|| SkillEvolutionOrchestratorError::new("candidate_not_found"))?;
        let artifacts = Arc::new(FileArtifactStore::new(
            self.evolution_root.join("artifacts"),
        ));
        let episodes = Arc::new(FileEpisodeStore::new(
            self.evolution_root.join("skill-evaluation-episodes"),
        ));
        let episode_id = record_evaluation_episode(
            artifacts.clone(),
            episodes.clone(),
            &candidate_revision,
            candidate,
        )
        .await;
        let bindings = collect_trusted_skill_evaluation_bindings(
            episodes.as_ref(),
            artifacts.as_ref(),
            &episode_id,
            &candidate_revision,
        )
        .await
        .map_err(|_| SkillEvolutionOrchestratorError::new("skill_usage_binding_failed"))?;
        let observations = bindings
            .values()
            .cloned()
            .map(|binding| SkillUsageObservationV1 {
                schema_version: SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
                binding,
                outcome_revision_id: OutcomeRevisionId::generate(),
                evidence_source: SkillUsageEvidenceSourceV1::TrustedEpisodeOutcome,
                result: SkillUsageResultV1::VerifiedSuccess,
                verifier_passed: Some(true),
                safety_failures: 0,
                observed_at_ms: evaluated_at_ms.saturating_sub(1).max(1),
            })
            .collect::<Vec<_>>();
        let outcome = SkillExitGate::new(
            genomes.store(),
            artifacts.as_ref(),
            self.evolution_root.join("skill-status"),
        )
        .evaluate_and_promote(
            candidate,
            self.authorization.clone(),
            &observations,
            &bindings,
            EvaluationReportId::generate(),
            evaluated_at_ms,
            activated_at_ms,
        )
        .await
        .map_err(|_| SkillEvolutionOrchestratorError::new("skill_exit_gate_failed"))?;
        Ok(match outcome {
            SkillExitGateOutcomeV1::Rejected {
                gate,
                report_artifact,
            } => SkillGateCycleOutcomeV1::Rejected {
                candidate_id: gate.report.candidate_id.clone(),
                report_id: gate.report.report_id.clone(),
                report_artifact,
            },
            SkillExitGateOutcomeV1::Promoted(receipt) => {
                let production_permitted = receipt.activation_authorization.permits_production();
                let authorization_evidence_id =
                    Self::authorization_evidence_id(&receipt.activation_authorization);
                SkillGateCycleOutcomeV1::Promoted(Box::new(SkillGatePromotionV1 {
                    evaluated_candidate: receipt.evaluated_candidate,
                    report_id: receipt.gate.report.report_id,
                    report_artifact: receipt.report_artifact,
                    active_skill_artifacts: receipt.active_skill_artifacts,
                    active_genome: receipt.active_genome,
                    authorization_evidence_id,
                    production_permitted,
                }))
            }
        })
    }

    /// 返回固定的可信健康结论，供测试覆盖保留与自动回滚分支。
    async fn verify_health(
        &self,
        _promoted: &StableGenomeRef,
    ) -> Result<SkillHealthVerdictV1, SkillEvolutionOrchestratorError> {
        Ok(self.health.clone())
    }
}

/// 一轮隔离 Cycle 所需的 Parent、Stable 前置条件和脱敏证据。
struct CycleFixture {
    root: TempDir,
    parent: GenomeRevision,
    request: SkillEvolutionCycleRequestV1,
    evidence: MutationEvidence,
}

impl CycleFixture {
    /// 创建并发布第一代 Parent Stable。
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("应创建测试目录");
        let parent = parent_genome();
        let publisher = FileStableGenomePublisher::new(root.path());
        publisher
            .resolver()
            .store()
            .append(&parent)
            .await
            .expect("应登记 Parent Genome");
        publisher
            .publish("production", &parent, 1)
            .await
            .expect("应发布 Parent Stable");
        let request = SkillEvolutionCycleRequestV1 {
            cycle_id: agent_evolution_protocol::EvolutionCycleId::generate(),
            parent_revision_id: parent.revision_id.clone(),
            parent_genome_digest: parent.digest.clone(),
            lineage: "production".into(),
            expected_parent_generation: 1,
            mutation_generated_at_ms: 10,
            candidate_created_at_ms: 20,
            evaluated_at_ms: 30,
            activated_at_ms: 40,
        };
        let evidence = MutationEvidence {
            issue_id: agent_evolution_protocol::EvolutionIssueId::generate(),
            genome_digest: parent.digest.clone(),
            failure_kind: FailureKind::VerificationFailure,
            root_cause_hypothesis: "现有 Skill 集合缺少稳定复核能力".into(),
            expected_behavior: "候选 Skill 应通过独立验证并保留可信证据".into(),
            confidence: 1.0,
            status: DiagnosticStatus::EligibleForEvolution,
            episodes: vec![MutationEpisodeEvidence {
                outbox_id: "outbox-m7-skill-cycle".into(),
                episode_id: agent_evolution_protocol::EpisodeId::generate(),
                genome_revision_id: parent.revision_id.clone(),
                outcome: Outcome::TaskFailure,
                task_family: "skill-evolution".into(),
                tags: BTreeSet::new(),
                failure: MutationFailureEvidence {
                    kind: FailureKind::VerificationFailure,
                    confidence: 1.0,
                    rule_derived: true,
                    model_assisted: false,
                },
                usage: UsageSummary::default(),
                replayability: ReplayabilityGrade::FixtureReproducible,
            }],
        };
        Self {
            root,
            parent,
            request,
            evidence,
        }
    }

    /// 创建使用真实 Gate 适配器与固定三候选生成器的生产 Runner。
    fn cycle(
        &self,
        authorization: SkillActivationAuthorizationV1,
        health: SkillHealthVerdictV1,
    ) -> SkillEvolutionCycle<ScriptedSkillGenerator, RealSkillGateOrchestrator> {
        let generator = ScriptedSkillGenerator {
            drafts: vec![
                create_draft("skill_cycleaa1", "A"),
                create_draft("skill_cyclebb2", "B"),
                create_draft("skill_cyclecc3", "C"),
            ],
        };
        let orchestrator = RealSkillGateOrchestrator {
            evolution_root: self.root.path().to_path_buf(),
            authorization,
            health,
            evaluation_attempts: None,
            interrupt_on_attempt: None,
        };
        SkillEvolutionCycle::new(self.root.path(), generator, orchestrator)
    }

    /// 创建可计数并可在指定 Gate 调用处中断的生产 Runner。
    fn controlled_cycle(
        &self,
        authorization: SkillActivationAuthorizationV1,
        health: SkillHealthVerdictV1,
        evaluation_attempts: Arc<AtomicUsize>,
        interrupt_on_attempt: Option<usize>,
    ) -> SkillEvolutionCycle<ScriptedSkillGenerator, RealSkillGateOrchestrator> {
        let generator = ScriptedSkillGenerator {
            drafts: vec![
                create_draft("skill_cycleaa1", "A"),
                create_draft("skill_cyclebb2", "B"),
                create_draft("skill_cyclecc3", "C"),
            ],
        };
        let orchestrator = RealSkillGateOrchestrator {
            evolution_root: self.root.path().to_path_buf(),
            authorization,
            health,
            evaluation_attempts: Some(evaluation_attempts),
            interrupt_on_attempt,
        };
        SkillEvolutionCycle::new(self.root.path(), generator, orchestrator)
    }
}

/// 验证三份 Proposal、Candidate、Gate 结果和所有 Active Genome 均被保留。
async fn assert_full_archive(
    root: &Path,
    result: &SkillEvolutionCycleResultV1,
) -> Vec<GenomeRevision> {
    assert_eq!(
        result.archive.proposals.len(),
        SKILL_EVOLUTION_CANDIDATE_COUNT
    );
    assert_eq!(
        result.archive.candidates.len(),
        SKILL_EVOLUTION_CANDIDATE_COUNT
    );
    assert_eq!(
        result.archive.gate_outcomes.len(),
        SKILL_EVOLUTION_CANDIDATE_COUNT
    );
    assert!(result.archive_path.is_file());
    let resolver = FileGenomeResolver::new(root);
    let parent = resolver
        .store()
        .get(&result.archive.request.parent_revision_id)
        .await
        .expect("应复读 Parent Revision")
        .expect("Parent Revision 应保留");
    let frozen_plugin_environment = parent
        .genome
        .plugin_environment_snapshot()
        .digest()
        .expect("Parent 插件环境应可摘要");
    for candidate in &result.archive.candidates {
        let stored_candidate = resolver
            .store()
            .get(&candidate.candidate_revision_id)
            .await
            .expect("应复读 Candidate Revision")
            .expect("Candidate Revision 应保留");
        assert_eq!(
            stored_candidate
                .genome
                .plugin_environment_snapshot()
                .digest()
                .expect("Candidate 插件环境应可摘要"),
            frozen_plugin_environment
        );
    }
    let mut active_genomes = Vec::new();
    for outcome in &result.archive.gate_outcomes {
        let SkillGateCycleOutcomeV1::Promoted(receipt) = outcome else {
            panic!("三份真实 Gate 评测都应产生 Active Genome")
        };
        let stored = resolver
            .store()
            .get(&receipt.active_genome.revision_id)
            .await
            .expect("应复读 Active Genome")
            .expect("Active Genome 应保留");
        assert_eq!(stored, receipt.active_genome);
        assert_eq!(
            stored
                .genome
                .plugin_environment_snapshot()
                .digest()
                .expect("Active 插件环境应可摘要"),
            frozen_plugin_environment
        );
        active_genomes.push(stored);
    }
    active_genomes
}

/// 校验完整阶段历史的连续序号与前向 SHA-256 摘要链。
fn assert_snapshot_chain(history: &[agent_evolution::SkillEvolutionCycleSnapshotV1]) {
    assert!(!history.is_empty());
    for (index, snapshot) in history.iter().enumerate() {
        assert_eq!(snapshot.sequence, index as u64);
        let expected = index.checked_sub(1).map(|previous| {
            FileSkillEvolutionCycleArchive::snapshot_digest(&history[previous])
                .expect("阶段快照应可规范摘要")
        });
        assert_eq!(snapshot.previous_digest, expected);
    }
}

/// Approved Gate 应发布第一份合格 Active Skill Set，并保留全部三候选证据。
#[tokio::test]
async fn approved_cycle_promotes_and_resolves_active_skill_set() {
    let fixture = CycleFixture::new().await;
    let cycle = fixture.cycle(
        SkillActivationAuthorizationV1::approved("approval-m7-cycle").expect("测试批准 ID 应合法"),
        SkillHealthVerdictV1::Healthy {
            evidence_id: "health-m7-cycle".into(),
        },
    );
    let result = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("Approved Cycle 应完成 Stable 发布");
    assert_eq!(
        result.archive.disposition,
        SkillEvolutionDispositionV1::HealthVerified
    );
    let active_genomes = assert_full_archive(fixture.root.path(), &result).await;
    let winner = result.archive.winner.as_ref().expect("应选择首个合格候选");
    let winner_active = result
        .archive
        .gate_outcomes
        .iter()
        .find_map(|outcome| match outcome {
            SkillGateCycleOutcomeV1::Promoted(receipt)
                if &receipt.evaluated_candidate.candidate_id == winner =>
            {
                Some(&receipt.active_genome)
            }
            _ => None,
        })
        .expect("应找到 Winner Active Genome");
    let resolver = FileGenomeResolver::new(fixture.root.path());
    let stable = resolver
        .resolve(&GenomeSelector::Stable("production".into()))
        .await
        .expect("Stable Resolver 应加载 Winner Active Genome");
    assert_eq!(stable, *winner_active);
    assert_eq!(active_genomes.len(), SKILL_EVOLUTION_CANDIDATE_COUNT);
    let repository = SkillArtifactRepository::new(cycle.artifacts());
    for reference in &stable.genome.skills {
        let artifact = repository
            .get(&reference.content)
            .await
            .expect("Stable Skill CAS 应可复读");
        assert_eq!(
            artifact.status_history.last().map(|status| status.status),
            Some(SkillStatusV1::Active)
        );
    }
}

/// LocalEvaluation 即使真实 Gate Pass，也不得修改生产 Stable。
#[tokio::test]
async fn local_evaluation_never_updates_stable() {
    let fixture = CycleFixture::new().await;
    let cycle = fixture.cycle(
        SkillActivationAuthorizationV1::local_evaluation(),
        SkillHealthVerdictV1::Healthy {
            evidence_id: "unused-local-health".into(),
        },
    );
    let result = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("LocalEvaluation 应完成归档但不发布");
    assert_eq!(
        result.archive.disposition,
        SkillEvolutionDispositionV1::Rejected
    );
    assert!(result.archive.winner.is_none());
    assert!(result.archive.promotion.is_none());
    assert_full_archive(fixture.root.path(), &result).await;
    let stable = FileGenomeResolver::new(fixture.root.path())
        .resolve(&GenomeSelector::Stable("production".into()))
        .await
        .expect("Parent Stable 应保持可解析");
    assert_eq!(stable, fixture.parent);
}

/// 生产健康失败必须绑定原 Promotion Release 自动回滚 Parent。
#[tokio::test]
async fn unhealthy_cycle_rolls_back_to_parent() {
    let fixture = CycleFixture::new().await;
    let cycle = fixture.cycle(
        SkillActivationAuthorizationV1::canary_passed(EvaluationReportId::generate()),
        SkillHealthVerdictV1::Unhealthy {
            evidence_id: "health-m7-rollback".into(),
            reason_code: "verification_regression".into(),
        },
    );
    let result = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("不健康 Cycle 应自动回滚");
    assert_eq!(
        result.archive.disposition,
        SkillEvolutionDispositionV1::RolledBack
    );
    assert_full_archive(fixture.root.path(), &result).await;
    let promotion = result.archive.promotion.as_ref().expect("应保留 Promotion");
    let rollback = result.archive.rollback.as_ref().expect("应保留 Rollback");
    assert_eq!(rollback.revision_id, fixture.parent.revision_id);
    assert_eq!(rollback.generation, 3);
    assert_eq!(rollback.rollback_of, promotion.release_id);
    let final_stable = FileGenomeResolver::new(fixture.root.path())
        .stable_reference("production")
        .await
        .expect("回滚后的 Stable 引用应可读取");
    assert_eq!(final_stable, *rollback);
}

/// 逐 Candidate 评测中断后只能继续剩余项，已归档 Gate 不得重复执行。
#[tokio::test]
async fn interrupted_evaluation_resumes_only_remaining_candidates() {
    let fixture = CycleFixture::new().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let authorization =
        SkillActivationAuthorizationV1::approved("approval-m7-resume").expect("批准 ID 应合法");
    let health = SkillHealthVerdictV1::Healthy {
        evidence_id: "health-m7-resume".into(),
    };
    let interrupted = fixture.controlled_cycle(
        authorization.clone(),
        health.clone(),
        attempts.clone(),
        Some(2),
    );
    let error = interrupted
        .run_until_health(&fixture.request, &fixture.evidence)
        .await
        .expect_err("第二份 Candidate 评测应模拟中断");
    assert_eq!(error.code(), "skill_evaluator_failed");
    let partial = interrupted
        .cycle_archive()
        .latest(&fixture.request.cycle_id)
        .await
        .expect("应读取中断历史")
        .expect("中断前应已提交阶段");
    assert_eq!(partial.stage, SkillEvolutionCycleStage::Evaluating);
    assert_eq!(partial.gate_outcomes.len(), 1);
    let mut failed = partial.clone();
    failed.stage = SkillEvolutionCycleStage::Failed;
    failed.failure_code = Some("skill_evaluator_interrupted".into());
    failed.validate().expect("失败终态应允许保留完整制品前缀");
    failed.source_outbox_ids.clear();
    assert!(failed.validate().is_err(), "失败终态也必须拒绝残缺来源绑定");

    let recovered = fixture.controlled_cycle(authorization, health, attempts.clone(), None);
    let pending = recovered
        .run_until_health(&fixture.request, &fixture.evidence)
        .await
        .expect("恢复后应完成剩余 Candidate");
    assert_eq!(pending.stage, SkillEvolutionCycleStage::AwaitingHealth);
    assert_eq!(pending.gate_outcomes.len(), SKILL_EVOLUTION_CANDIDATE_COUNT);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    let history = recovered
        .cycle_archive()
        .history(&fixture.request.cycle_id)
        .await
        .expect("恢复历史应有效");
    assert_snapshot_chain(&history);
}

/// Stable 已切换但 AwaitingHealth 快照未提交时，恢复不得重复晋升或增加代数。
#[tokio::test]
async fn promotion_recovery_accepts_already_switched_stable() {
    let fixture = CycleFixture::new().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let cycle = fixture.controlled_cycle(
        SkillActivationAuthorizationV1::approved("approval-m7-promotion-recovery")
            .expect("批准 ID 应合法"),
        SkillHealthVerdictV1::Healthy {
            evidence_id: "health-m7-promotion-recovery".into(),
        },
        attempts.clone(),
        None,
    );
    let committed = cycle
        .run_until_health(&fixture.request, &fixture.evidence)
        .await
        .expect("应先完成 Promotion");
    assert_eq!(committed.stage, SkillEvolutionCycleStage::AwaitingHealth);
    let uncommitted_path = cycle
        .cycle_archive()
        .root()
        .join(fixture.request.cycle_id.as_str())
        .join(format!("{:020}.json", committed.sequence));
    fs::remove_file(&uncommitted_path).expect("应模拟 AwaitingHealth 快照未提交");

    let recovered = cycle
        .resume(&fixture.request)
        .await
        .expect("应识别已切换的 Stable");
    assert_eq!(recovered.stage, SkillEvolutionCycleStage::AwaitingHealth);
    assert_eq!(recovered.promotion, committed.promotion);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        SKILL_EVOLUTION_CANDIDATE_COUNT
    );
    let stable = FileGenomeResolver::new(fixture.root.path())
        .stable_reference("production")
        .await
        .expect("Promotion Stable 应可读取");
    assert_eq!(stable.generation, 2);
    assert_eq!(Some(&stable), recovered.promotion.as_ref());
    let history = cycle
        .cycle_archive()
        .history(&fixture.request.cycle_id)
        .await
        .expect("恢复后的历史应有效");
    assert_snapshot_chain(&history);
}

/// 最终 Archive 已提交但消费标记丢失时，重试必须补消费且不重跑 Gate。
#[tokio::test]
async fn terminal_archive_retry_completes_outbox_consumption() {
    let fixture = CycleFixture::new().await;
    let source = &fixture.evidence.episodes[0];
    let outbox_id = source.outbox_id.clone();
    let outbox = FileEvolutionOutbox::new(fixture.root.path().join("outbox"));
    outbox
        .append(&EvolutionOutboxItem {
            outbox_id: outbox_id.clone(),
            episode_id: source.episode_id.clone(),
            outcome: Outcome::TaskFailure,
            disposition: FailureDisposition::EvolutionCandidate,
            issue_id: Some(fixture.evidence.issue_id.clone()),
            issue_status: DiagnosticStatus::EligibleForEvolution,
            created_at_ms: 1,
            consumed: false,
        })
        .await
        .expect("应写入来源 Outbox");
    let attempts = Arc::new(AtomicUsize::new(0));
    let cycle = fixture.controlled_cycle(
        SkillActivationAuthorizationV1::approved("approval-m7-terminal-retry")
            .expect("批准 ID 应合法"),
        SkillHealthVerdictV1::Healthy {
            evidence_id: "health-m7-terminal-retry".into(),
        },
        attempts.clone(),
        None,
    );
    let first = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("首轮应完成健康终态");
    let archive_bytes = fs::read(&first.archive_path).expect("最终 Archive 应可读取");
    let consumed_marker = outbox.root().join(format!("outbox-{outbox_id}.consumed"));
    fs::remove_file(&consumed_marker).expect("应模拟 Archive 后消费标记未提交");
    assert_eq!(outbox.pending().await.expect("应复读 Outbox").len(), 1);

    let repeated = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("重试应补做 Outbox 消费");
    assert_eq!(repeated.archive, first.archive);
    assert_eq!(
        fs::read(&repeated.archive_path).expect("重试后 Archive 应可读取"),
        archive_bytes
    );
    assert!(outbox.pending().await.expect("应复读消费结果").is_empty());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        SKILL_EVOLUTION_CANDIDATE_COUNT
    );
}

/// 无阶段历史的旧版最终 Archive 必须按原字节复读，不生成伪快照或改变 Hash。
#[tokio::test]
async fn legacy_final_archive_remains_byte_and_hash_compatible() {
    let fixture = CycleFixture::new().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let cycle = fixture.controlled_cycle(
        SkillActivationAuthorizationV1::approved("approval-m7-legacy").expect("批准 ID 应合法"),
        SkillHealthVerdictV1::Healthy {
            evidence_id: "health-m7-legacy".into(),
        },
        attempts.clone(),
        None,
    );
    let first = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("应先生成旧版最终 Archive");
    let bytes_before = fs::read(&first.archive_path).expect("旧版 Archive 应可读取");
    assert_eq!(
        bytes_before,
        serde_json::to_vec_pretty(&first.archive).expect("旧版 Archive 应可稳定序列化")
    );
    let digest_before = Sha256::digest(&bytes_before);
    let cycle_root = cycle
        .cycle_archive()
        .root()
        .join(fixture.request.cycle_id.as_str());
    fs::remove_dir_all(&cycle_root).expect("应模拟仅有旧版最终 Archive 的历史数据");

    let repeated = cycle
        .run(&fixture.request, &fixture.evidence)
        .await
        .expect("旧版 Archive 应直接复读");
    let bytes_after = fs::read(&repeated.archive_path).expect("复读后 Archive 应仍存在");
    assert_eq!(bytes_after, bytes_before);
    assert_eq!(Sha256::digest(&bytes_after), digest_before);
    assert!(!cycle_root.exists());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        SKILL_EVOLUTION_CANDIDATE_COUNT
    );
}
