//! M7 Skill 生产入口与独立 Evaluator 的双进程端到端证据。

use agent_core::{AgentEvent, AgentEventKind, EventSink};
use agent_evaluation::{
    SkillEvaluationRegistryEntryV1, SkillEvaluationRegistryV1, SkillHealthRegistryEntryV1,
    SkillRegistryAuthorizationV1, SKILL_EVALUATION_REGISTRY_FILE,
    SKILL_EVALUATION_REGISTRY_SCHEMA_VERSION,
};
use agent_evolution::{
    collect_trusted_skill_evaluation_bindings, ArtifactStore, BoundedSkillMutator,
    DeterministicSkillMutationGenerator, EpisodeRecorder, EpisodeRecorderConfig, EpisodeStore,
    EvolutionOutbox, EvolutionOutboxItem, FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox,
    FileGenomeResolver, FileGenomeStore, FileIssueObservationStore, FileStableGenomePublisher,
    GenomeResolver, GenomeSelector, GenomeStore, IssueObservation, IssueObservationStore,
    SkillArtifactRepository, SkillCandidateBuilder, SkillEvolutionArchiveV1,
    SkillEvolutionCycleRequestV1, SkillEvolutionDispositionV1, SkillGateCycleOutcomeV1,
    NATIVE_SKILL_READ_TOOL, NATIVE_SKILL_USAGE_SCHEMA_VERSION,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ArtifactRef, AttributionMethod, DataClass, DiagnosticStatus,
    Episode, EpisodeDataPolicy, EpisodeId, EvaluationReportId, EventId, EvolutionEligibility,
    EvolutionIssueId, FailureAttribution, FailureClassification, FailureDisposition, FailureKind,
    FailureRecord, FailureRecordId, GenomeMetadata, GenomeRevision, GenomeRevisionId, ModelGenome,
    Outcome, OutcomeRevisionId, ReleaseId, ReplayabilityGrade, RunId, RuntimeIdentity,
    SkillCandidateV1, SkillHealthStatusV1, SkillStatusTransitionV1, SkillStatusV1,
    SkillUsageEvidenceSourceV1, SkillUsageObservationV1, SkillUsageResultV1, TaskDescriptor,
    ToolProfileGenome, UsageSummary, EPISODE_SCHEMA_VERSION, GENOME_SCHEMA_VERSION,
    SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE, SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};
use tempfile::TempDir;

const LINEAGE: &str = "production";
const MUTATION_AT_MS: u64 = 10;
const CANDIDATE_AT_MS: u64 = 20;
const EVALUATED_AT_MS: u64 = 30;
const ACTIVATED_AT_MS: u64 = 40;

/// 返回固定 SHA-256 制品摘要。
fn digest(character: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造只允许 Serve 且尚未安装 Skill 的初始 Parent Genome。
fn parent_revision() -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "m7-skill-process-e2e".into(),
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
            prompt: Default::default(),
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

/// 创建可进入脱敏 Mutator 的 Episode 数据策略。
fn eligible_policy() -> EpisodeDataPolicy {
    let mut policy = EpisodeDataPolicy::for_class(DataClass::Internal);
    policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    policy.redaction_rules_version = Some("redaction-v1".into());
    policy
}

/// 构造 Selector 可复读的真实失败 Episode。
fn source_episode(episode_id: EpisodeId, parent: &GenomeRevision) -> Episode {
    Episode {
        schema_version: EPISODE_SCHEMA_VERSION,
        episode_id,
        run_id: RunId::generate(),
        session_id: "m7-source-session".into(),
        genome_revision_id: parent.revision_id.clone(),
        task: TaskDescriptor {
            family: "skill-evolution".into(),
            input_ref: None,
            tags: BTreeSet::from(["m7".into()]),
        },
        event_stream_ref: ArtifactRef {
            digest: digest('1'),
            media_type: "application/json".into(),
            size_bytes: 32,
        },
        supervision: None,
        environment_ref: None,
        outcome: Some(Outcome::TaskFailure),
        failures: vec![FailureClassification {
            kind: FailureKind::VerificationFailure,
            evidence_event_ids: vec![EventId::generate().to_string()],
            confidence: 1.0,
            rule_derived: true,
            model_assisted: false,
        }],
        usage: UsageSummary::default(),
        replayability: ReplayabilityGrade::FixtureReproducible,
        data_policy: eligible_policy(),
        event_count: 1,
        started_at_ms: 1,
        finished_at_ms: 2,
    }
}

/// 构造可由 Issue Aggregator 稳定重建的失败观察。
fn failure_record(episode_id: EpisodeId) -> FailureRecord {
    FailureRecord {
        record_id: FailureRecordId::generate(),
        episode_id,
        attribution: FailureAttribution {
            detected_at: EventId::generate(),
            suspected_origin: None,
            propagation_path: Vec::new(),
            decisive_step: None,
            failure_class: FailureKind::VerificationFailure,
            confidence: 1.0,
            evidence: Vec::new(),
            method: AttributionMethod::DeterministicRule,
        },
        status: DiagnosticStatus::Observed,
    }
}

/// 向固定三个 Store 写入唯一可选的 MutationEvidence。
async fn prepare_source_evidence(root: &Path, parent: &GenomeRevision) {
    let issue_id = EvolutionIssueId::generate();
    let first_episode_id = EpisodeId::generate();
    let selected_episode_id = EpisodeId::generate();
    let observations = FileIssueObservationStore::new(root.join("issue-observations"));
    observations
        .append(&IssueObservation::new(
            issue_id.clone(),
            first_episode_id.clone(),
            &parent.digest,
            failure_record(first_episode_id),
        ))
        .await
        .expect("应追加第一条 Issue 观察");
    observations
        .append(&IssueObservation::new(
            issue_id.clone(),
            selected_episode_id.clone(),
            &parent.digest,
            failure_record(selected_episode_id.clone()),
        ))
        .await
        .expect("应追加第二条 Issue 观察");
    FileEpisodeStore::new(root.join("episodes"))
        .append(&source_episode(selected_episode_id.clone(), parent))
        .await
        .expect("应追加失败 Episode");
    FileEvolutionOutbox::new(root.join("outbox"))
        .append(&EvolutionOutboxItem {
            outbox_id: "m7-skill-process".into(),
            episode_id: selected_episode_id,
            outcome: Outcome::TaskFailure,
            disposition: FailureDisposition::EvolutionCandidate,
            issue_id: Some(issue_id),
            issue_status: DiagnosticStatus::EligibleForEvolution,
            created_at_ms: 2,
            consumed: false,
        })
        .await
        .expect("应追加 Evolution Outbox");
}

/// 用真实 Recorder 记录 Candidate 的原生 Skill 工具终态。
async fn record_evaluation_episode(
    artifacts: Arc<FileArtifactStore>,
    episodes: Arc<FileEpisodeStore>,
    candidate_revision: &GenomeRevision,
    candidate: &SkillCandidateV1,
) -> EpisodeId {
    let config = EpisodeRecorderConfig::online(
        format!("m7-process-{}", candidate.candidate_id),
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
        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::ToolFinished,
                index + 1,
                json!({
                    "call_id": format!("skill-evaluation-{index}"),
                    "name": NATIVE_SKILL_READ_TOOL,
                    "is_error": false,
                    "runtime_origin": "native",
                    "details": {
                        "skill_usage": {
                            "schema_version": NATIVE_SKILL_USAGE_SCHEMA_VERSION,
                            "skill_id": skill_id,
                            "artifact_digest": artifact_digest,
                            "genome_revision_id": candidate_revision.revision_id,
                            "genome_digest": candidate_revision.digest,
                        }
                    },
                    "content": "该正文必须由 Recorder 丢弃",
                }),
            ))
            .await
            .expect("应记录原生 Skill 使用");
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
    recorder.episode_id().await.expect("应生成 Episode")
}

/// 根据 Candidate 的真实绑定构造成功或失败观察。
fn observations(
    bindings: &BTreeMap<EventId, agent_evolution_protocol::TrustedSkillUsageBindingV1>,
    success: bool,
) -> Vec<SkillUsageObservationV1> {
    bindings
        .values()
        .cloned()
        .map(|binding| SkillUsageObservationV1 {
            schema_version: SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
            binding,
            outcome_revision_id: OutcomeRevisionId::generate(),
            evidence_source: SkillUsageEvidenceSourceV1::TrustedEpisodeOutcome,
            result: if success {
                SkillUsageResultV1::VerifiedSuccess
            } else {
                SkillUsageResultV1::VerifiedFailure
            },
            verifier_passed: Some(success),
            safety_failures: 0,
            observed_at_ms: EVALUATED_AT_MS - 1,
        })
        .collect()
}

/// 预计 Gate 为 Candidate 生成的 Active Genome，供健康 Registry 提前绑定。
async fn expected_active_revision(
    artifacts: &FileArtifactStore,
    genomes: &FileGenomeStore,
    candidate: &SkillCandidateV1,
    report_id: &EvaluationReportId,
) -> GenomeRevision {
    let repository = SkillArtifactRepository::new(artifacts);
    let mut active_refs = BTreeMap::new();
    for (skill_id, quarantined_digest) in &candidate.candidate_artifact_digests {
        let mut artifact = repository
            .get(quarantined_digest)
            .await
            .expect("应读取 Quarantined Skill");
        artifact.status_history.push(SkillStatusTransitionV1 {
            status: SkillStatusV1::Evaluated,
            recorded_at_ms: EVALUATED_AT_MS,
            evaluation_report_id: Some(report_id.clone()),
        });
        artifact.status_history.push(SkillStatusTransitionV1 {
            status: SkillStatusV1::Active,
            recorded_at_ms: ACTIVATED_AT_MS,
            evaluation_report_id: Some(report_id.clone()),
        });
        active_refs.insert(
            skill_id.clone(),
            repository.put(&artifact).await.expect("应预计 Active 制品"),
        );
    }
    let candidate_revision = genomes
        .get(&candidate.candidate_revision_id)
        .await
        .expect("应读取 Candidate Genome")
        .expect("Candidate Genome 应存在");
    let mut genome = candidate_revision.genome.clone();
    for reference in &mut genome.skills {
        let skill_id = agent_evolution_protocol::SkillId::new(reference.id.clone())
            .expect("Candidate Skill ID 应合法");
        if let Some(active) = active_refs.get(&skill_id) {
            reference.content = active.digest.clone();
        }
    }
    let mut revision = GenomeRevision::create(
        genome,
        GenomeMetadata {
            created_at: None,
            description: None,
            parent: Some(candidate_revision.revision_id),
            mutation: Some(candidate.mutation_id.clone()),
        },
    )
    .expect("Active Genome 应合法");
    let mut hasher = Sha256::new();
    for part in [
        b"skill-active-genome-v1".as_slice(),
        candidate.candidate_id.as_str().as_bytes(),
        report_id.as_str().as_bytes(),
        revision.digest.as_str().as_bytes(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    revision.revision_id = GenomeRevisionId::new(format!(
        "{}_{:x}",
        GenomeRevisionId::PREFIX,
        hasher.finalize()
    ))
    .expect("Active Revision ID 应合法");
    revision
}

/// 使用 Cycle 与报告身份派生与生产 Runner 一致的 Promotion Release ID。
fn promotion_release_id(
    cycle_id: &agent_evolution_protocol::EvolutionCycleId,
    report_id: &EvaluationReportId,
) -> ReleaseId {
    let value = format!("skillpromotion:{cycle_id}:{report_id}");
    ReleaseId::new(format!("rel_{:x}", Sha256::digest(value.as_bytes())))
        .expect("Promotion Release ID 应合法")
}

/// 将规范 Registry 写入固定目录并返回摘要。
fn write_registry(root: &Path, registry: &SkillEvaluationRegistryV1) -> ArtifactDigest {
    let registry_root = root.join("skill-registry");
    fs::create_dir_all(&registry_root).expect("应创建 Registry 目录");
    let bytes = serde_json::to_vec(registry).expect("Registry 应可规范序列化");
    fs::write(registry_root.join(SKILL_EVALUATION_REGISTRY_FILE), &bytes).expect("应写入 Registry");
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("Registry 摘要应合法")
}

/// 定位与当前测试共享 target profile 的真实 `lucia-eval`。
fn evaluator_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("LUCIA_M7_EVALUATOR_BIN") {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .expect("应定位测试二进制")
        .parent()
        .and_then(Path::parent)
        .expect("测试二进制应位于 target profile/deps")
        .join("lucia-eval")
}

/// 启动真实 `lucia-evolve skill-cycle` 并解析唯一归档回执。
fn invoke_cycle(
    root: &Path,
    evaluator: &Path,
    registry_digest: &ArtifactDigest,
    request: &SkillEvolutionCycleRequestV1,
) -> SkillEvolutionArchiveV1 {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lucia-evolve"))
        .arg("skill-cycle")
        .env("LUCIA_EVOLVE_EVALUATOR_BIN", evaluator)
        .env("LUCIA_EVOLVE_EVOLUTION_ROOT", root)
        .env("LUCIA_EVAL_EVOLUTION_ROOT", root)
        .env(
            "LUCIA_EVAL_SKILL_REGISTRY_ROOT",
            root.join("skill-registry"),
        )
        .env("LUCIA_EVAL_SKILL_REGISTRY_DIGEST", registry_digest.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("应启动真实 lucia-evolve");
    child
        .stdin
        .take()
        .expect("skill-cycle 应打开 stdin")
        .write_all(&serde_json::to_vec(request).expect("请求应可序列化"))
        .expect("应写入 Skill Cycle 请求");
    let output = child.wait_with_output().expect("应等待 lucia-evolve");
    assert!(
        output.status.success(),
        "lucia-evolve 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("应返回 Skill Evolution Archive")
}

/// 真实双进程必须完成 Reject、本地评测、生产晋升与不健康回滚。
#[tokio::test]
#[ignore = "需要先构建真实 lucia-eval，再显式运行双进程验收"]
async fn skill_cycle_uses_independent_gate_and_rolls_back_unhealthy_promotion() {
    let temporary = TempDir::new().expect("应创建 M7 E2E 目录");
    let root = temporary.path().join("evolution");
    fs::create_dir_all(&root).expect("应创建 Evolution 根");
    let evaluator = evaluator_binary();
    assert!(
        evaluator.is_absolute() && evaluator.is_file(),
        "请先构建真实 lucia-eval：{}",
        evaluator.display()
    );
    let parent = parent_revision();
    let publisher = FileStableGenomePublisher::new(&root);
    publisher
        .resolver()
        .store()
        .append(&parent)
        .await
        .expect("应登记 Parent Genome");
    publisher
        .publish(LINEAGE, &parent, 1)
        .await
        .expect("应发布初始 Parent Stable");
    prepare_source_evidence(&root, &parent).await;
    let request = SkillEvolutionCycleRequestV1 {
        cycle_id: agent_evolution_protocol::EvolutionCycleId::generate(),
        parent_revision_id: parent.revision_id.clone(),
        parent_genome_digest: parent.digest.clone(),
        lineage: LINEAGE.into(),
        expected_parent_generation: 1,
        mutation_generated_at_ms: MUTATION_AT_MS,
        candidate_created_at_ms: CANDIDATE_AT_MS,
        evaluated_at_ms: EVALUATED_AT_MS,
        activated_at_ms: ACTIVATED_AT_MS,
    };

    let selector = agent_evolution::EpisodeSelector::new(
        Arc::new(FileEvolutionOutbox::new(root.join("outbox"))),
        Arc::new(FileEpisodeStore::new(root.join("episodes"))),
        Arc::new(FileIssueObservationStore::new(
            root.join("issue-observations"),
        )),
    );
    let evidence = selector
        .select()
        .await
        .expect("应恢复 MutationEvidence")
        .pop()
        .expect("应存在唯一证据");
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    let genomes = FileGenomeStore::new(root.join("genomes"));
    let proposals = BoundedSkillMutator::m7(DeterministicSkillMutationGenerator)
        .propose(&parent, &evidence, MUTATION_AT_MS, &artifacts)
        .await
        .expect("应预构建三份 Proposal");
    let builder = SkillCandidateBuilder::new(&genomes, &artifacts);
    let mut candidates = Vec::new();
    for proposal in &proposals {
        candidates.push(
            builder
                .build_at(request.cycle_id.clone(), proposal, CANDIDATE_AT_MS)
                .await
                .expect("应预构建 Candidate"),
        );
    }
    assert_eq!(candidates.len(), 3);

    let evaluation_episodes = Arc::new(FileEpisodeStore::new(
        root.join("skill-evaluation-episodes"),
    ));
    let shared_artifacts = Arc::new(artifacts.clone());
    let report_ids = [
        EvaluationReportId::generate(),
        EvaluationReportId::generate(),
        EvaluationReportId::generate(),
    ];
    let authorizations = [
        SkillRegistryAuthorizationV1::Approved {
            approval_id: "approval-rejected-a".into(),
        },
        SkillRegistryAuthorizationV1::LocalEvaluation,
        SkillRegistryAuthorizationV1::Approved {
            approval_id: "approval-production-c".into(),
        },
    ];
    let mut evaluations = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let candidate_revision = genomes
            .get(&candidate.candidate_revision_id)
            .await
            .expect("应读取 Candidate Genome")
            .expect("Candidate Genome 应存在");
        let episode_id = record_evaluation_episode(
            shared_artifacts.clone(),
            evaluation_episodes.clone(),
            &candidate_revision,
            candidate,
        )
        .await;
        let bindings = collect_trusted_skill_evaluation_bindings(
            evaluation_episodes.as_ref(),
            shared_artifacts.as_ref(),
            &episode_id,
            &candidate_revision,
        )
        .await
        .expect("应从真实 Episode 恢复绑定");
        let candidate_bytes = serde_json::to_vec(candidate).expect("Candidate 应可规范序列化");
        let candidate_artifact = artifacts
            .put(SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE, &candidate_bytes)
            .await
            .expect("应写入 Candidate 快照");
        evaluations.push(SkillEvaluationRegistryEntryV1 {
            candidate_id: candidate.candidate_id.clone(),
            candidate_revision_id: candidate.candidate_revision_id.clone(),
            report_id: report_ids[index].clone(),
            candidate_artifact,
            trusted_usage_bindings: bindings.clone(),
            observations: observations(&bindings, index != 0),
            authorization: authorizations[index].clone(),
            evaluated_at_ms: EVALUATED_AT_MS,
            activated_at_ms: ACTIVATED_AT_MS,
        });
    }
    evaluations.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));
    let winner_active =
        expected_active_revision(&artifacts, &genomes, &candidates[2], &report_ids[2]).await;
    let promotion_release = promotion_release_id(&request.cycle_id, &report_ids[2]);
    let registry = SkillEvaluationRegistryV1 {
        schema_version: SKILL_EVALUATION_REGISTRY_SCHEMA_VERSION,
        evaluations,
        health: vec![SkillHealthRegistryEntryV1 {
            release_id: promotion_release.clone(),
            lineage: LINEAGE.into(),
            revision_id: winner_active.revision_id.clone(),
            generation: 2,
            result: SkillHealthStatusV1::Unhealthy {
                evidence_id: "health-m7-process".into(),
                reason_code: "verification_regression".into(),
            },
        }],
    };
    registry.validate().expect("Registry 应合法");
    let registry_digest = write_registry(&root, &registry);

    let archive = invoke_cycle(&root, &evaluator, &registry_digest, &request);
    assert_eq!(archive.disposition, SkillEvolutionDispositionV1::RolledBack);
    assert_eq!(archive.candidates, candidates);
    assert_eq!(archive.gate_outcomes.len(), 3);
    assert!(matches!(
        archive.gate_outcomes[0],
        SkillGateCycleOutcomeV1::Rejected { .. }
    ));
    let SkillGateCycleOutcomeV1::Promoted(local) = &archive.gate_outcomes[1] else {
        panic!("Candidate B 应通过 Gate")
    };
    assert!(!local.production_permitted);
    let SkillGateCycleOutcomeV1::Promoted(production) = &archive.gate_outcomes[2] else {
        panic!("Candidate C 应通过 Gate")
    };
    assert!(production.production_permitted);
    assert_eq!(production.active_genome, winner_active);
    assert_eq!(archive.winner.as_ref(), Some(&candidates[2].candidate_id));
    assert_eq!(
        archive
            .promotion
            .as_ref()
            .and_then(|stable| stable.release_id.as_ref()),
        Some(&promotion_release)
    );
    let rollback = archive.rollback.as_ref().expect("应保留 Rollback");
    assert_eq!(rollback.revision_id, parent.revision_id);
    assert_eq!(rollback.generation, 3);
    let final_stable = FileGenomeResolver::new(&root)
        .resolve(&GenomeSelector::Stable(LINEAGE.into()))
        .await
        .expect("应解析回滚后 Stable");
    assert_eq!(final_stable, parent);

    for outcome in &archive.gate_outcomes {
        assert!(artifacts
            .get(&outcome.report_artifact().digest)
            .await
            .expect("应读取报告 CAS")
            .is_some());
    }
    for candidate in &archive.candidates {
        assert!(genomes
            .get(&candidate.candidate_revision_id)
            .await
            .expect("应读取 Candidate Revision")
            .is_some());
    }
    assert!(root
        .join("skill-cycle-archive")
        .join(format!("{}.json", request.cycle_id))
        .is_file());
    assert!(root
        .join("skill-registry")
        .join(SKILL_EVALUATION_REGISTRY_FILE)
        .is_file());
    assert!(FileEvolutionOutbox::new(root.join("outbox"))
        .pending()
        .await
        .expect("Cycle 归档后应可读取 Outbox")
        .is_empty());
}
