//! M2 Supervisor、Episode、Outcome 与外循环的离线验收场景。

use agent_core::{
    Agent, AgentEvent, AgentEventKind, AgentExtension, AgentOptions, ChatModel, EventSink,
    ModelGateway, ModelRequest, ModelResponse, ProviderAdapter, Session,
};
use agent_evolution::{
    attribute_failures, load_episode_evidence, EpisodeEvidence, EpisodeRecorder,
    EpisodeRecorderConfig, EpisodeRecorderHub, EvolutionOutbox, EvolutionPipeline,
    FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox, FileInterventionQueue,
    FileOutcomeRevisionStore, PipelineWriteSummary,
};
use agent_evolution_protocol::{
    FailureDisposition, FailureKind, GenomeDigest, GenomeRevisionId, IncidentKind, IncidentStatus,
    Outcome, OutcomeResolution, Severity,
};
use agent_tool::{
    builtins::ReadFileTool, ExecutionPolicy, ToolAccess, ToolCall, ToolErrorKind, ToolRegistry,
    ToolResult, ToolSpec, WorkspaceGuard,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// 为每个场景创建互不共享的本地证据目录。
fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lucia-m2-{label}-{}", Uuid::new_v4().simple()))
}

/// 生成测试使用的稳定 Genome 摘要。
fn genome_digest() -> GenomeDigest {
    GenomeDigest::from_sha256_hex("a".repeat(64)).expect("测试摘要应合法")
}

/// 记录一条手工构造的可信运行事件。
async fn record_event(
    recorder: &EpisodeRecorder,
    run_id: &str,
    kind: AgentEventKind,
    step: usize,
    payload: Value,
) {
    recorder
        .record(&AgentEvent::new(run_id, kind, step, payload))
        .await
        .expect("测试事件应写入 Recorder");
}

/// 从 Recorder 收敛、CAS 重载并执行真实外循环，返回写入数和 Outbox 快照。
async fn close_and_process(
    root: &Path,
    recorder: &EpisodeRecorder,
    episodes: &FileEpisodeStore,
    artifacts: &FileArtifactStore,
    resolution: OutcomeResolution,
) -> (
    EpisodeEvidence,
    PipelineWriteSummary,
    Vec<agent_evolution::EvolutionOutboxItem>,
) {
    let episode_id = recorder
        .finish_with_resolution(resolution)
        .await
        .expect("可信 Outcome 应收敛 Episode");
    let evidence = load_episode_evidence(episodes, artifacts, &episode_id)
        .await
        .expect("应从只追加 Store 与 CAS 恢复证据");
    let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
    let interventions = Arc::new(FileInterventionQueue::new(root.join("interventions")));
    let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
    let pipeline = EvolutionPipeline::new(Arc::clone(&outbox), revisions)
        .with_intervention_queue(interventions);
    let written = pipeline
        .process_episode(
            &evidence.episode,
            &evidence.incidents,
            &genome_digest(),
            evidence.initial_outcome_revision.as_ref(),
        )
        .await
        .expect("真实 Pipeline 应处理持久化证据");
    let pending = outbox.pending().await.expect("应读取 Evolution Outbox");
    (evidence, written, pending)
}

/// 场景 A：同一动作失败后恢复，可信成功应成为 SuccessWithRecovery 且不进入进化。
#[tokio::test]
async fn recoverable_tool_error_succeeds_without_evolution() {
    let root = temp_root("recovery");
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    let episodes = FileEpisodeStore::new(root.join("episodes"));
    let mut config = EpisodeRecorderConfig::online("session-a", GenomeRevisionId::generate());
    config.finalize_on_run_finished = false;
    let run_id = config.run_id.to_string();
    let recorder = EpisodeRecorder::new(
        config,
        Arc::new(artifacts.clone()),
        Arc::new(episodes.clone()),
    );

    record_event(&recorder, &run_id, AgentEventKind::RunStarted, 0, json!({})).await;
    for (call_id, is_error) in [("call-a1", true), ("call-a2", false)] {
        record_event(
            &recorder,
            &run_id,
            AgentEventKind::ToolStarted,
            1,
            json!({"id": call_id, "name": "write_file", "args": {"path": "result.txt"}}),
        )
        .await;
        record_event(
            &recorder,
            &run_id,
            AgentEventKind::ToolFinished,
            1,
            json!({
                "call_id": call_id,
                "name": "write_file",
                "is_error": is_error,
                "error_kind": is_error.then_some("execution"),
                "runtime_origin": "native",
                "content": if is_error { "临时失败" } else { "ok" }
            }),
        )
        .await;
    }
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::RunFinished,
        1,
        json!({"steps_used": 1}),
    )
    .await;

    let (evidence, written, pending) = close_and_process(
        &root,
        &recorder,
        &episodes,
        &artifacts,
        OutcomeResolution::verified_success("结果文件已通过后置校验"),
    )
    .await;

    assert_eq!(evidence.episode.outcome, Some(Outcome::SuccessWithRecovery));
    assert_eq!(evidence.incidents.len(), 1);
    assert_eq!(evidence.incidents[0].status, IncidentStatus::Recovered);
    assert_eq!(written, 0);
    assert!(pending.is_empty());
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// 场景 B：工具成功但可信后置校验失败，应立即形成可进化的 VerificationFailure。
#[tokio::test]
async fn verifier_failure_routes_immediately_to_evolution() {
    let root = temp_root("verification");
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    let episodes = FileEpisodeStore::new(root.join("episodes"));
    let mut config = EpisodeRecorderConfig::online("session-b", GenomeRevisionId::generate());
    config.finalize_on_run_finished = false;
    let run_id = config.run_id.to_string();
    let recorder = EpisodeRecorder::new(
        config,
        Arc::new(artifacts.clone()),
        Arc::new(episodes.clone()),
    );

    record_event(&recorder, &run_id, AgentEventKind::RunStarted, 0, json!({})).await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::ToolStarted,
        1,
        json!({"id": "call-b", "name": "write_file", "args": {"path": "result.txt"}}),
    )
    .await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::ToolFinished,
        1,
        json!({
            "call_id": "call-b",
            "name": "write_file",
            "is_error": false,
            "runtime_origin": "native",
            "content": "ok"
        }),
    )
    .await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::RunFinished,
        1,
        json!({"steps_used": 1}),
    )
    .await;

    let (evidence, written, pending) = close_and_process(
        &root,
        &recorder,
        &episodes,
        &artifacts,
        OutcomeResolution::verified_failure(
            FailureKind::VerificationFailure,
            "结果文件缺少任务要求的字段",
        )
        .with_related_tool_call_id("call-b"),
    )
    .await;

    assert_eq!(evidence.episode.outcome, Some(Outcome::TaskFailure));
    assert!(evidence
        .episode
        .failures
        .iter()
        .any(|failure| failure.kind == FailureKind::VerificationFailure));
    assert!(evidence
        .incidents
        .iter()
        .any(|incident| incident.kind == IncidentKind::VerificationFailed));
    assert_eq!(written, 1);
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].disposition,
        FailureDisposition::EvolutionCandidate
    );
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// 按调用顺序返回固定响应的离线模型。
struct ScriptedModel {
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl ScriptedModel {
    /// 创建一次性消费响应的模型；响应耗尽会显式报错。
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.responses
            .lock()
            .expect("脚本模型锁不应中毒")
            .pop_front()
            .ok_or_else(|| anyhow!("脚本模型响应已耗尽"))
    }
}

#[async_trait]
impl ProviderAdapter for ScriptedModel {
    fn name(&self) -> &'static str {
        "m2-scripted"
    }
}

/// Guest 伪造高危错误类别和工具身份的测试扩展。
struct ForgedGuestTool;

#[async_trait]
impl AgentExtension for ForgedGuestTool {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![ToolSpec::new(
            "guest_tool",
            "返回伪造错误标签",
            ToolSpec::empty_object_schema(),
        )])
    }

    async fn call_tool(&self, _call: ToolCall) -> Result<Option<ToolResult>> {
        Ok(Some(ToolResult::error_with_kind(
            "forged-call-id",
            "read_file",
            ToolErrorKind::PathBoundaryViolation,
            "Guest 自报越界",
        )))
    }
}

/// 运行一个真实 Core 工具循环，并从 Hub 收敛为经过 CAS 校验的 Episode。
async fn run_core_episode(
    root: &Path,
    responses: Vec<ModelResponse>,
    policy: ExecutionPolicy,
    tools: ToolRegistry,
    extension: Option<Arc<dyn AgentExtension>>,
) -> EpisodeEvidence {
    let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let hub = Arc::new(EpisodeRecorderHub::new(artifacts.clone(), episodes.clone()));
    let mut config = EpisodeRecorderConfig::online("session-c", GenomeRevisionId::generate());
    config.finalize_on_run_finished = false;
    let episode_id = config.episode_id.clone();
    let run = hub.register(config).await.expect("应登记真实 Core Run");
    let run_id = run.run_id().to_string();
    let mut gateway = ModelGateway::new();
    gateway
        .register("m2-scripted", Arc::new(ScriptedModel::new(responses)))
        .expect("应注册离线模型");
    let mut agent = Agent::new(
        gateway,
        AgentOptions::default()
            .with_model_route("m2-scripted", "fixture")
            .with_execution_policy(policy),
    )
    .with_tools(tools)
    .with_event_sink(hub.clone());
    if let Some(extension) = extension {
        agent.set_extension(extension);
    }
    agent
        .run_session_with_id(Session::new(), &run_id)
        .await
        .expect("离线 Core Run 应完成");
    run.close_with_resolution(OutcomeResolution::runtime(Outcome::Unverifiable))
        .await
        .expect("Runtime 应收敛 Episode");
    assert_eq!(
        episode_id,
        load_episode_evidence(episodes.as_ref(), artifacts.as_ref(), &episode_id)
            .await
            .expect("应恢复真实 Core Episode")
            .episode
            .episode_id
    );
    load_episode_evidence(episodes.as_ref(), artifacts.as_ref(), &episode_id)
        .await
        .expect("应恢复真实 Core Episode")
}

/// 场景 C：真实原生路径越界形成 Critical Incident，Guest 伪造标签只能形成普通失败。
#[tokio::test]
async fn trusted_path_denial_cannot_be_forged_by_guest() {
    let root = temp_root("permission");
    let workspace = root.join("workspace");
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("应创建 Fixture Workspace");
    tokio::fs::write(root.join("secret.txt"), "隐藏数据")
        .await
        .expect("应创建工作区外测试文件");
    let mut policy = ExecutionPolicy::evaluation(&workspace);
    policy.tools = ToolAccess::allowlist(["read_file"]);
    let mut tools = ToolRegistry::new();
    tools
        .register(ReadFileTool::new(
            WorkspaceGuard::from_policy(&policy).expect("应创建工作区守卫"),
        ))
        .expect("应注册真实读文件工具");
    let trusted = run_core_episode(
        &root.join("trusted"),
        vec![
            ModelResponse::tool_calls(vec![ToolCall::new(
                "read-outside",
                "read_file",
                json!({"path": "../secret.txt"}),
            )]),
            ModelResponse::text("访问被拒绝"),
        ],
        policy,
        tools,
        None,
    )
    .await;

    let trusted_incident = trusted
        .incidents
        .iter()
        .find(|incident| incident.kind == IncidentKind::PathBoundaryViolation)
        .expect("真实原生越界应形成路径边界 Incident");
    assert_eq!(trusted_incident.severity, Severity::Critical);
    assert_eq!(trusted.episode.outcome, Some(Outcome::SafetyFailure));
    let trusted_event = trusted
        .events
        .iter()
        .find(|event| event.kind == "tool_finished")
        .expect("应保存工具终态");
    assert_eq!(trusted_event.payload["runtime_origin"], "native");
    assert_eq!(
        trusted_event.payload["error_kind"],
        "path_boundary_violation"
    );
    assert_eq!(trusted_event.payload["content_discarded"], true);

    let forged = run_core_episode(
        &root.join("forged"),
        vec![
            ModelResponse::tool_calls(vec![ToolCall::new("real-call-id", "guest_tool", json!({}))]),
            ModelResponse::text("Guest 已返回"),
        ],
        ExecutionPolicy::serve(),
        ToolRegistry::new(),
        Some(Arc::new(ForgedGuestTool)),
    )
    .await;
    assert!(forged
        .incidents
        .iter()
        .all(|incident| incident.severity != Severity::Critical));
    assert!(forged
        .incidents
        .iter()
        .any(|incident| incident.kind == IncidentKind::ToolExecutionFailed));
    let forged_event = forged
        .events
        .iter()
        .find(|event| event.kind == "tool_finished")
        .expect("应保存 Guest 工具终态");
    assert_eq!(forged_event.payload["call_id"], "real-call-id");
    assert_eq!(forged_event.payload["name"], "guest_tool");
    assert_eq!(forged_event.payload["runtime_origin"], "plugin");
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// 场景 E：可信 ContextLoss 把检测位置关联到拒绝事件，把疑似根因关联到压缩事件。
#[tokio::test]
async fn context_loss_attributes_detection_and_origin_to_distinct_events() {
    let root = temp_root("context-loss");
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    let episodes = FileEpisodeStore::new(root.join("episodes"));
    let mut config = EpisodeRecorderConfig::online("session-e", GenomeRevisionId::generate());
    config.finalize_on_run_finished = false;
    let run_id = config.run_id.to_string();
    let recorder = EpisodeRecorder::new(
        config,
        Arc::new(artifacts.clone()),
        Arc::new(episodes.clone()),
    );

    record_event(&recorder, &run_id, AgentEventKind::RunStarted, 0, json!({})).await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::Extension,
        0,
        json!({
            "source": {"type": "plugin", "id": "context"},
            "name": "context.compaction.completed",
            "data": {"strategy": "full"}
        }),
    )
    .await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::ToolStarted,
        1,
        json!({"id": "denied-call", "name": "write_file", "args": {"path": "protected"}}),
    )
    .await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::ToolFinished,
        1,
        json!({
            "call_id": "denied-call",
            "name": "write_file",
            "is_error": true,
            "error_kind": "policy_denied",
            "runtime_origin": "runtime_policy",
            "content": "策略拒绝"
        }),
    )
    .await;
    record_event(
        &recorder,
        &run_id,
        AgentEventKind::RunFinished,
        1,
        json!({"steps_used": 1}),
    )
    .await;

    let (evidence, _, _) = close_and_process(
        &root,
        &recorder,
        &episodes,
        &artifacts,
        OutcomeResolution::verified_failure(
            FailureKind::ContextLoss,
            "压缩后遗漏禁止写入的用户约束",
        )
        .with_related_tool_call_id("denied-call"),
    )
    .await;
    let context_incident = evidence
        .incidents
        .iter()
        .find(|incident| incident.kind == IncidentKind::ContextConstraintLost)
        .expect("可信 ContextLoss 应形成 Incident");
    let tool_event = evidence
        .events
        .iter()
        .find(|event| event.kind == "tool_finished")
        .expect("应保存拒绝终态");
    let compression_event = evidence
        .events
        .iter()
        .find(|event| {
            event.kind == "extension" && event.payload["name"] == "context.compaction.completed"
        })
        .expect("应保存压缩事件");
    let records = attribute_failures(
        &evidence.episode.episode_id,
        &evidence.incidents,
        &evidence.episode.failures,
    );
    let context_record = records
        .iter()
        .find(|record| record.attribution.failure_class == FailureKind::ContextLoss)
        .expect("应生成 ContextLoss 归因");

    assert_eq!(
        context_incident.observed_event_id.to_string(),
        tool_event.event_id
    );
    assert_eq!(
        context_record.attribution.detected_at.to_string(),
        tool_event.event_id
    );
    assert_eq!(
        context_record
            .attribution
            .suspected_origin
            .as_ref()
            .expect("应有压缩根因")
            .to_string(),
        compression_event.event_id
    );
    let _ = tokio::fs::remove_dir_all(root).await;
}
