//! M7 Skill 从隔离 Candidate 到 Active 后续运行的独立 Exit Gate 证据。

use agent_core::{AgentEvent, AgentEventKind, EventSink};
use agent_evaluation::{SkillActivationAuthorizationV1, SkillExitGate, SkillExitGateOutcomeV1};
use agent_evolution::{
    collect_trusted_skill_evaluation_bindings, EpisodeRecorder, EpisodeRecorderConfig,
    FileArtifactStore, FileEpisodeStore, FileGenomeStore, FileSkillStatusStore, GenomeStore,
    SkillArtifactRepository, SkillCandidateBuilder,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, EpisodeId, EvaluationReportId, EvolutionCycleId, GenomeMetadata,
    GenomeRevision, ModelGenome, MutationId, OutcomeRevisionId, PluginGenome, PromptGenome,
    RuntimeIdentity, SkillArtifactV1, SkillId, SkillMutationProposalV1, SkillOperationV1,
    SkillStatusTransitionV1, SkillStatusV1, SkillTriggerPolicyV1, SkillUsageEvidenceSourceV1,
    SkillUsageObservationV1, SkillUsageResultV1, ToolProfileGenome, GENOME_SCHEMA_VERSION,
    SKILL_ARTIFACT_SCHEMA_VERSION, SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
    SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

/// 生成固定测试摘要。
fn digest(character: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 创建包含受限证据能力 owner 的 Parent Genome；Skill 本身由 Kernel 原生装配。
fn parent_genome() -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "m7-exit-gate".into(),
                git_dirty: false,
                target_triple: "test-target".into(),
                features: BTreeSet::from(["plugins".into()]),
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
                id: "evidence".into(),
                version: "0.1.0".into(),
                api_version: "0.7.0".into(),
                bundle: digest('a'),
                manifest_digest: Some(digest('b')),
                config_digest: None,
                capability_profile_digest: Some(digest('c')),
                load_order: Some(0),
                hook_order: Vec::new(),
            }],
            capability_owners: BTreeMap::from([(
                "episode.read_redacted".into(),
                "evidence".into(),
            )]),
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

/// 创建只含一份 Quarantined Create 制品的正式 Proposal。
fn proposal(parent: &GenomeRevision, skill_id: SkillId) -> SkillMutationProposalV1 {
    let mutation_id = MutationId::generate();
    let episode_id = EpisodeId::generate();
    SkillMutationProposalV1 {
        schema_version: SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
        mutation_id: mutation_id.clone(),
        parent_revision_id: parent.revision_id.clone(),
        parent_genome_digest: parent.digest.clone(),
        evidence_episode_ids: BTreeSet::from([episode_id.clone()]),
        proposed_artifacts: vec![SkillArtifactV1 {
            schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
            skill_id,
            revision: 1,
            operation: SkillOperationV1::Create,
            name: "可信复核".into(),
            description: "根据脱敏证据执行固定复核".into(),
            instructions: "先读取脱敏证据，再输出可验证结论。".into(),
            trigger_policy: SkillTriggerPolicyV1::default(),
            required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            source_episode_ids: BTreeSet::from([episode_id]),
            mutation_id,
            status_history: vec![SkillStatusTransitionV1 {
                status: SkillStatusV1::Quarantined,
                recorded_at_ms: 1,
                evaluation_report_id: None,
            }],
        }],
        hypothesis: "增加独立可信复核 Skill 可减少验证失败".into(),
    }
}

/// 使用真实 Recorder 写入由 Core 注入来源的原生 Skill 工具终态。
async fn record_skill_episode(
    artifacts: Arc<FileArtifactStore>,
    episodes: Arc<FileEpisodeStore>,
    genome: &GenomeRevision,
    skill_id: &SkillId,
    skill_digest: &ArtifactDigest,
    call_id: &str,
) -> EpisodeId {
    let config = EpisodeRecorderConfig::online("m7-skill-exit-gate", genome.revision_id.clone());
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
        .expect("应记录运行开始");
    recorder
        .record(&AgentEvent::new(
            &run_id,
            AgentEventKind::ToolFinished,
            1,
            json!({
                "call_id": call_id,
                "name": "skill_read",
                "is_error": false,
                "runtime_origin": "native",
                "details": {
                    "skill_usage": {
                        "schema_version": 1,
                        "skill_id": skill_id,
                        "artifact_digest": skill_digest,
                        "genome_revision_id": genome.revision_id,
                        "genome_digest": genome.digest
                    }
                }
            }),
        ))
        .await
        .expect("应记录真实原生 Skill 工具终态");
    recorder
        .record(&AgentEvent::new(
            &run_id,
            AgentEventKind::RunFinished,
            1,
            json!({"steps_used": 1}),
        ))
        .await
        .expect("应收敛 Episode");
    recorder.episode_id().await.expect("应产生 Episode")
}

/// 返回测试使用的独立根目录。
fn roots() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("lucia-m7-exit-{}", EvaluationReportId::generate()));
    (
        root.join("genomes"),
        root.join("artifacts"),
        root.join("episodes"),
        root.join("skill-status"),
    )
}

/// M7 Exit Gate 必须保持原 Candidate 不变，完成 Q→E→A，只登记引用 Active 摘要的
/// 后续 Serve Genome，并由新 Episode 证明该新制品实际被使用。
#[tokio::test]
async fn promotes_quarantined_candidate_and_proves_new_serve_usage() {
    let (genome_root, artifact_root, episode_root, status_root) = roots();
    let cleanup_root = genome_root
        .parent()
        .expect("测试根目录应存在")
        .to_path_buf();
    let genomes = FileGenomeStore::new(&genome_root);
    let artifacts = Arc::new(FileArtifactStore::new(&artifact_root));
    let episodes = Arc::new(FileEpisodeStore::new(&episode_root));
    let parent = parent_genome();
    genomes.append(&parent).await.expect("应登记 Parent Genome");
    let skill_id = SkillId::new("skill_exitgate1").expect("Skill ID 应合法");
    let proposal = proposal(&parent, skill_id.clone());
    let candidate = SkillCandidateBuilder::new(&genomes, artifacts.as_ref())
        .build_at(EvolutionCycleId::generate(), &proposal, 2)
        .await
        .expect("应构建 Quarantined Candidate");
    let candidate_revision = genomes
        .get(&candidate.candidate_revision_id)
        .await
        .expect("应读取 Candidate")
        .expect("Candidate 应已登记");
    let quarantined_digest = candidate.candidate_artifact_digests[&skill_id].clone();

    let evaluation_episode = record_skill_episode(
        artifacts.clone(),
        episodes.clone(),
        &candidate_revision,
        &skill_id,
        &quarantined_digest,
        "evaluation-call",
    )
    .await;
    let evaluation_bindings = collect_trusted_skill_evaluation_bindings(
        episodes.as_ref(),
        artifacts.as_ref(),
        &evaluation_episode,
        &candidate_revision,
    )
    .await
    .expect("原 Quarantined Candidate Revision 应产生可信评测绑定");
    let evaluation_binding = evaluation_bindings
        .values()
        .next()
        .expect("应有一条评测绑定")
        .clone();
    assert_eq!(
        evaluation_binding.genome_revision_id,
        candidate_revision.revision_id
    );
    let observation = SkillUsageObservationV1 {
        schema_version: SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
        binding: evaluation_binding,
        outcome_revision_id: OutcomeRevisionId::generate(),
        evidence_source: SkillUsageEvidenceSourceV1::TrustedEpisodeOutcome,
        result: SkillUsageResultV1::VerifiedSuccess,
        verifier_passed: Some(true),
        safety_failures: 0,
        observed_at_ms: 3,
    };

    let report_id = EvaluationReportId::generate();
    let quarantined_artifact = SkillArtifactRepository::new(artifacts.as_ref())
        .get(&quarantined_digest)
        .await
        .expect("应复读隔离候选制品");
    let status_store = FileSkillStatusStore::new(&status_root, artifacts.as_ref());
    status_store
        .append(&quarantined_artifact)
        .await
        .expect("应模拟已提交 Quarantined 前缀");
    let mut partially_evaluated = quarantined_artifact;
    partially_evaluated
        .status_history
        .push(SkillStatusTransitionV1 {
            status: SkillStatusV1::Evaluated,
            recorded_at_ms: 4,
            evaluation_report_id: Some(report_id.clone()),
        });
    status_store
        .append(&partially_evaluated)
        .await
        .expect("应模拟崩溃前已提交 Evaluated 前缀");

    let gate = SkillExitGate::new(&genomes, artifacts.as_ref(), &status_root);
    let outcome = gate
        .evaluate_and_promote(
            &candidate,
            SkillActivationAuthorizationV1::local_evaluation(),
            std::slice::from_ref(&observation),
            &evaluation_bindings,
            report_id,
            4,
            5,
        )
        .await
        .expect("可信评测应完成 Promotion");
    let SkillExitGateOutcomeV1::Promoted(receipt) = outcome else {
        panic!("可信评测应通过 Gate")
    };
    let receipt = receipt.as_ref();
    let retried = gate
        .evaluate_and_promote(
            &candidate,
            SkillActivationAuthorizationV1::local_evaluation(),
            &[observation],
            &evaluation_bindings,
            receipt.gate.report.report_id.clone(),
            4,
            5,
        )
        .await
        .expect("相同 Gate 输入重试应幂等");
    let SkillExitGateOutcomeV1::Promoted(retried) = retried else {
        panic!("幂等重试仍应为 Promoted")
    };
    let retried = retried.as_ref();
    assert_eq!(retried.active_genome, receipt.active_genome);
    assert_eq!(
        retried.active_skill_artifacts,
        receipt.active_skill_artifacts
    );
    assert!(!receipt.activation_authorization.permits_production());

    let original_candidate = genomes
        .get(&candidate.candidate_revision_id)
        .await
        .expect("应复读原 Candidate")
        .expect("原 Candidate 应保留");
    assert_eq!(original_candidate, candidate_revision);
    assert_eq!(
        original_candidate.genome.skills[0].content,
        quarantined_digest
    );
    assert_ne!(
        receipt.active_genome.revision_id,
        candidate.candidate_revision_id
    );
    assert_eq!(
        receipt.active_genome.genome.execution.profile(),
        agent_tool::ExecutionProfile::Serve
    );
    let active_reference = receipt
        .active_skill_artifacts
        .get(&skill_id)
        .expect("应有 Active Skill 引用");
    assert_eq!(
        receipt.active_genome.genome.skills[0].content,
        active_reference.digest
    );
    assert_ne!(active_reference.digest, quarantined_digest);

    let history = FileSkillStatusStore::new(&status_root, artifacts.as_ref())
        .history(&skill_id, 1)
        .await
        .expect("应读取完整状态链");
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].transition.status, SkillStatusV1::Quarantined);
    assert_eq!(history[1].transition.status, SkillStatusV1::Evaluated);
    assert_eq!(history[2].transition.status, SkillStatusV1::Active);
    let active_artifact = SkillArtifactRepository::new(artifacts.as_ref())
        .get(&active_reference.digest)
        .await
        .expect("应读取 Active 制品");
    assert_eq!(
        active_artifact
            .status_history
            .last()
            .map(|status| status.status),
        Some(SkillStatusV1::Active)
    );

    let serve_episode = record_skill_episode(
        artifacts.clone(),
        episodes.clone(),
        &receipt.active_genome,
        &skill_id,
        &active_reference.digest,
        "serve-call",
    )
    .await;
    let proof = gate
        .verify_post_promotion_use(receipt, episodes.as_ref(), &serve_episode)
        .await
        .expect("新运行应证明 Active Skill 实际可用");
    assert_eq!(proof.active_revision_id, receipt.active_genome.revision_id);
    assert_eq!(proof.episode_id, serve_episode);
    assert_eq!(proof.bindings.len(), 1);

    let _ = tokio::fs::remove_dir_all(cleanup_root).await;
}

/// Reject 不得追加 Skill 状态索引或登记 Active Genome。
#[tokio::test]
async fn reject_archives_report_without_promoting_state() {
    let (genome_root, artifact_root, _episode_root, status_root) = roots();
    let cleanup_root = genome_root
        .parent()
        .expect("测试根目录应存在")
        .to_path_buf();
    let genomes = FileGenomeStore::new(&genome_root);
    let artifacts = FileArtifactStore::new(&artifact_root);
    let parent = parent_genome();
    genomes.append(&parent).await.expect("应登记 Parent Genome");
    let skill_id = SkillId::new("skill_rejected1").expect("Skill ID 应合法");
    let proposal = proposal(&parent, skill_id.clone());
    let candidate = SkillCandidateBuilder::new(&genomes, &artifacts)
        .build_at(EvolutionCycleId::generate(), &proposal, 2)
        .await
        .expect("应构建 Candidate");
    let gate = SkillExitGate::new(&genomes, &artifacts, &status_root);
    let outcome = gate
        .evaluate_and_promote(
            &candidate,
            SkillActivationAuthorizationV1::local_evaluation(),
            &[],
            &BTreeMap::new(),
            EvaluationReportId::generate(),
            4,
            5,
        )
        .await
        .expect("无可信使用应成为 Reject 而非基础设施错误");
    assert!(matches!(outcome, SkillExitGateOutcomeV1::Rejected { .. }));
    assert!(FileSkillStatusStore::new(&status_root, &artifacts)
        .history(&skill_id, 1)
        .await
        .expect("Reject 后状态目录查询应成功")
        .is_empty());
    let candidate_revision = genomes
        .get(&candidate.candidate_revision_id)
        .await
        .expect("应读取 Candidate")
        .expect("Candidate 应保留");
    assert_eq!(candidate_revision.genome.skills.len(), 1);
    assert!(!Path::new(&status_root).exists());
    let _ = tokio::fs::remove_dir_all(cleanup_root).await;
}
