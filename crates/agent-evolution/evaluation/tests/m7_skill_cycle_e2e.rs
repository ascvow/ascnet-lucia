//! M7 Skill 生产 Cycle 的真实 Exit Gate、Stable 发布与健康回滚证据。

use agent_core::{AgentEvent, AgentEventKind, EventSink};
use agent_evaluation::{SkillActivationAuthorizationV1, SkillExitGate, SkillExitGateOutcomeV1};
use agent_evolution::{
    collect_trusted_skill_evaluation_bindings, EpisodeRecorder, EpisodeRecorderConfig,
    FileArtifactStore, FileEpisodeStore, FileGenomeResolver, FileStableGenomePublisher,
    GenomeResolver, GenomeSelector, GenomeStore, MutationEpisodeEvidence, MutationEvidence,
    MutationFailureEvidence, SkillArtifactRepository, SkillContentDraftV1, SkillEvolutionCycle,
    SkillEvolutionCycleRequestV1, SkillEvolutionCycleResultV1, SkillEvolutionDispositionV1,
    SkillEvolutionOrchestrator, SkillEvolutionOrchestratorError, SkillGateCycleOutcomeV1,
    SkillGatePromotionV1, SkillHealthVerdictV1, SkillMutationDraftOperationV1,
    SkillMutationDraftV1, SkillMutationGenerationError, SkillMutationGenerator,
    SkillMutationRequestV1, StableGenomeRef, NATIVE_SKILL_READ_TOOL,
    NATIVE_SKILL_USAGE_SCHEMA_VERSION, SKILL_EVOLUTION_CANDIDATE_COUNT,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, DiagnosticStatus, EvaluationReportId, FailureKind, GenomeMetadata,
    GenomeRevision, ModelGenome, Outcome, OutcomeRevisionId, PluginGenome, PromptGenome,
    ReplayabilityGrade, RuntimeIdentity, SkillCandidateV1, SkillId, SkillStatusV1,
    SkillTriggerPolicyV1, SkillUsageEvidenceSourceV1, SkillUsageObservationV1, SkillUsageResultV1,
    ToolProfileGenome, UsageSummary, GENOME_SCHEMA_VERSION, SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
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
                config_digest: None,
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
    for candidate in &result.archive.candidates {
        assert!(resolver
            .store()
            .get(&candidate.candidate_revision_id)
            .await
            .expect("应复读 Candidate Revision")
            .is_some());
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
        active_genomes.push(stored);
    }
    active_genomes
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
