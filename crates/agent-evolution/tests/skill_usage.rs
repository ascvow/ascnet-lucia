//! M7 Skill 使用事件到可信 Episode 绑定的端到端测试。

use agent_core::{AgentEvent, AgentEventKind, EventSink};
use agent_evolution::{
    collect_trusted_skill_evaluation_bindings, collect_trusted_skill_usage_bindings, ArtifactStore,
    EpisodeRecorder, EpisodeRecorderConfig, FileArtifactStore, FileEpisodeStore,
    SkillArtifactRepository, SkillUsageBindingError, SKILL_LOADED_EVENT_V1,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, EpisodeId, EvaluationReportId, GenomeMetadata, GenomeRevision,
    ModelGenome, MutationId, PluginGenome, PromptGenome, RuntimeIdentity, SkillArtifactV1, SkillId,
    SkillOperationV1, SkillRef, SkillStatusTransitionV1, SkillStatusV1, SkillTriggerPolicyV1,
    ToolProfileGenome, GENOME_SCHEMA_VERSION, SKILL_ARTIFACT_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};

/// 创建测试使用的固定 Artifact 摘要。
fn digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 创建 Quarantined 或完整 Active 状态链的 Skill 制品。
fn skill_artifact(skill_id: SkillId, active: bool) -> SkillArtifactV1 {
    let mut status_history = vec![SkillStatusTransitionV1 {
        status: SkillStatusV1::Quarantined,
        recorded_at_ms: 1,
        evaluation_report_id: None,
    }];
    if active {
        let report = EvaluationReportId::generate();
        status_history.push(SkillStatusTransitionV1 {
            status: SkillStatusV1::Evaluated,
            recorded_at_ms: 2,
            evaluation_report_id: Some(report.clone()),
        });
        status_history.push(SkillStatusTransitionV1 {
            status: SkillStatusV1::Active,
            recorded_at_ms: 3,
            evaluation_report_id: Some(report),
        });
    }
    SkillArtifactV1 {
        schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
        skill_id,
        revision: 1,
        operation: SkillOperationV1::Create,
        name: "可信审查".into(),
        description: "根据固定检查项审查代码".into(),
        instructions: "先读取差异，再逐项验证。".into(),
        trigger_policy: SkillTriggerPolicyV1::default(),
        required_capabilities: BTreeSet::new(),
        source_episode_ids: BTreeSet::from([EpisodeId::generate()]),
        mutation_id: MutationId::generate(),
        status_history,
    }
}

/// 创建仅固定一份 Skill 制品的有效 Genome 修订。
fn genome(skill_id: &SkillId, skill_digest: ArtifactDigest) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "m7-skill-usage".into(),
                git_dirty: false,
                target_triple: "aarch64-apple-darwin".into(),
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
                id: "skill".into(),
                version: "0.1.0".into(),
                api_version: "0.7.0".into(),
                bundle: digest('b'),
                config_digest: None,
            }],
            capability_owners: Default::default(),
            tools: ToolProfileGenome::default(),
            context_policy: None,
            planning_policy: None,
            skills: vec![SkillRef {
                id: skill_id.to_string(),
                content: skill_digest,
            }],
            execution: ExecutionPolicy::serve(),
        },
        GenomeMetadata::default(),
    )
    .expect("测试 Genome 应合法")
}

/// 用真实 Recorder 持久化一条由 Host 包装来源的 Skill 加载事件。
async fn record_skill_episode(
    artifacts: Arc<FileArtifactStore>,
    episodes: Arc<FileEpisodeStore>,
    genome: &GenomeRevision,
    source_plugin_id: &str,
    skill_id: &SkillId,
    skill_digest: &ArtifactDigest,
) -> EpisodeId {
    let config = EpisodeRecorderConfig::online("m7-skill-session", genome.revision_id.clone());
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
            AgentEventKind::Extension,
            1,
            json!({
                "source": { "type": "plugin", "id": source_plugin_id },
                "name": SKILL_LOADED_EVENT_V1,
                "data": {
                    "schema_version": 1,
                    "skill_id": skill_id,
                    "artifact_digest": skill_digest,
                    "call_id": "skill-call-1"
                }
            }),
        ))
        .await
        .expect("应记录 Skill 加载事件");
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

/// 真实 Recorder、CAS 与 Genome 应共同生成唯一可信绑定和 payload 制品。
#[tokio::test]
async fn binds_real_host_plugin_event_to_active_genome_skill() {
    let root = std::env::temp_dir().join(format!("lucia-m7-usage-{}", uuid::Uuid::new_v4()));
    let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let skill_id = SkillId::new("skill_trustedreview").expect("Skill ID 应合法");
    let artifact = skill_artifact(skill_id.clone(), true);
    let reference = SkillArtifactRepository::new(artifacts.as_ref())
        .put(&artifact)
        .await
        .expect("应写入 Active Skill CAS");
    let genome = genome(&skill_id, reference.digest.clone());
    let episode_id = record_skill_episode(
        artifacts.clone(),
        episodes.clone(),
        &genome,
        "skill",
        &skill_id,
        &reference.digest,
    )
    .await;

    let bindings = collect_trusted_skill_usage_bindings(
        episodes.as_ref(),
        artifacts.as_ref(),
        &episode_id,
        &genome,
        "skill",
    )
    .await
    .expect("真实 Skill 事件应产生可信绑定");

    assert_eq!(bindings.len(), 1);
    let binding = bindings.values().next().expect("应有一条绑定");
    assert_eq!(binding.skill_id, skill_id);
    assert_eq!(binding.skill_artifact_digest, reference.digest);
    let payload = artifacts
        .get(&binding.plugin_event.payload_digest)
        .await
        .expect("应读取 payload CAS")
        .expect("payload CAS 应存在");
    assert!(String::from_utf8(payload)
        .expect("payload 应是 UTF-8 JSON")
        .contains("skill.loaded.v1"));
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Guest 伪造其他插件来源时不得生成部分可信绑定。
#[tokio::test]
async fn rejects_forged_plugin_owner() {
    let root = std::env::temp_dir().join(format!("lucia-m7-owner-{}", uuid::Uuid::new_v4()));
    let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let skill_id = SkillId::new("skill_ownercheck").expect("Skill ID 应合法");
    let reference = SkillArtifactRepository::new(artifacts.as_ref())
        .put(&skill_artifact(skill_id.clone(), true))
        .await
        .expect("应写入 Active Skill CAS");
    let genome = genome(&skill_id, reference.digest.clone());
    let episode_id = record_skill_episode(
        artifacts.clone(),
        episodes.clone(),
        &genome,
        "forged-plugin",
        &skill_id,
        &reference.digest,
    )
    .await;

    let error = collect_trusted_skill_usage_bindings(
        episodes.as_ref(),
        artifacts.as_ref(),
        &episode_id,
        &genome,
        "skill",
    )
    .await
    .expect_err("伪造 owner 必须失败关闭");
    assert!(matches!(
        error,
        SkillUsageBindingError::UntrustedPluginSource
    ));
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Quarantined Skill 即使出现在 Genome 引用和真实事件中也不能进入评测绑定。
#[tokio::test]
async fn rejects_quarantined_skill_from_serve_usage() {
    let root = std::env::temp_dir().join(format!("lucia-m7-quarantine-{}", uuid::Uuid::new_v4()));
    let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let skill_id = SkillId::new("skill_quarantine").expect("Skill ID 应合法");
    let reference = SkillArtifactRepository::new(artifacts.as_ref())
        .put(&skill_artifact(skill_id.clone(), false))
        .await
        .expect("应写入 Quarantined Skill CAS");
    let genome = genome(&skill_id, reference.digest.clone());
    let episode_id = record_skill_episode(
        artifacts.clone(),
        episodes.clone(),
        &genome,
        "skill",
        &skill_id,
        &reference.digest,
    )
    .await;

    let error = collect_trusted_skill_usage_bindings(
        episodes.as_ref(),
        artifacts.as_ref(),
        &episode_id,
        &genome,
        "skill",
    )
    .await
    .expect_err("Quarantined Skill 不得被绑定为 Serve 使用");
    assert!(matches!(
        error,
        SkillUsageBindingError::SkillNotLoadable { stage: "serve", .. }
    ));
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Evaluation Binder 可绑定原 Candidate Revision 中的 Quarantined Skill，且不会接受
/// Guest 自报 stage 来改变普通 Serve 入口。
#[tokio::test]
async fn evaluation_binder_accepts_quarantined_candidate_on_original_revision() {
    let root = std::env::temp_dir().join(format!("lucia-m7-evaluation-{}", uuid::Uuid::new_v4()));
    let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let skill_id = SkillId::new("skill_evalcandidate").expect("Skill ID 应合法");
    let reference = SkillArtifactRepository::new(artifacts.as_ref())
        .put(&skill_artifact(skill_id.clone(), false))
        .await
        .expect("应写入 Quarantined Skill CAS");
    let candidate = genome(&skill_id, reference.digest.clone());
    let episode_id = record_skill_episode(
        artifacts.clone(),
        episodes.clone(),
        &candidate,
        "skill",
        &skill_id,
        &reference.digest,
    )
    .await;

    let bindings = collect_trusted_skill_evaluation_bindings(
        episodes.as_ref(),
        artifacts.as_ref(),
        &episode_id,
        &candidate,
        "skill",
    )
    .await
    .expect("Evaluation 应绑定原 Candidate Revision 的 Quarantined Skill");
    let binding = bindings.values().next().expect("应产生可信绑定");
    assert_eq!(binding.genome_revision_id, candidate.revision_id);
    assert_eq!(binding.skill_artifact_digest, reference.digest);

    let serve_error = collect_trusted_skill_usage_bindings(
        episodes.as_ref(),
        artifacts.as_ref(),
        &episode_id,
        &candidate,
        "skill",
    )
    .await
    .expect_err("相同事件不能通过普通 Serve Binder");
    assert!(matches!(
        serve_error,
        SkillUsageBindingError::SkillNotLoadable { stage: "serve", .. }
    ));
    let _ = tokio::fs::remove_dir_all(root).await;
}
