//! M3 Replay 与 Dataset 的离线 Exit Gate。

use agent_core::{Agent, AgentOptions, ModelGateway, ModelResponse};
use agent_evaluation::{
    ComparativeRunner, ComparativeRunnerConfig, DatasetVisibility, EvaluationSubject, ModelFixture,
    ModelFixtureInteraction, ModelMock, ModelRequestMatcher, TrustedDatasetStore,
    MODEL_FIXTURE_SCHEMA_VERSION,
};
use agent_evolution_protocol::{
    ArtifactDigest, DatasetKind, EvaluationEnvironment, GenomeRevisionId, TaskAttemptStatus,
};
use agent_tool::{
    builtins::{ReadFileTool, WriteFileTool},
    ExecutionPolicy, ToolAccess, ToolCall, ToolRegistry, WorkspaceGuard,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, sync::Arc};
use tempfile::TempDir;

/// 返回仓库内置的版本化 Regression/Safety Dataset 根目录。
fn builtin_dataset_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("datasets/builtin-v1")
}

/// 构造 Parent/Candidate 共享的离线环境摘要。
fn environment() -> EvaluationEnvironment {
    EvaluationEnvironment {
        kernel_ref: "lucia-core-m3".to_string(),
        model_provider: "evaluation-fixture".to_string(),
        model: "fixture-model-v1".to_string(),
        model_parameters_digest: "sha256:fixture-model-v1".to_string(),
        tool_profile_digest: "sha256:builtin-tools-v1".to_string(),
        execution_profile_digest: "sha256:evaluation-v1".to_string(),
        plugin_set_digest: "sha256:none".to_string(),
        capability_owner_digest: "sha256:native-fixture".to_string(),
        resource_budget_digest: "sha256:builtin-budget-v1".to_string(),
        verifier_version: "builtin-v1".to_string(),
        evaluation_policy_version: "evaluation-v1".to_string(),
        environment_fixture_digest: "sha256:builtin-v1".to_string(),
        repeat_count: 0,
    }
}

/// 构造 Prompt 正文与摘要一致的离线评测对象。
fn subject(id: &str, prompt: &str) -> EvaluationSubject {
    let artifact = ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(prompt)))
        .expect("测试 Prompt 摘要合法");
    EvaluationSubject::new(
        GenomeRevisionId::new(id).expect("测试 Revision ID 合法"),
        artifact,
        prompt.to_string(),
    )
    .expect("测试 Prompt 摘要一致")
}

/// M3 Exit Gate：内置 Regression/Safety Set 必须可在无网络、无 Secret、无真实模型时
/// 同时运行 Parent/Candidate，并可完成精确 Fixture Replay。
#[tokio::test]
async fn builtin_dataset_runs_offline_and_replays_exactly() {
    let dataset = TrustedDatasetStore::open(builtin_dataset_root())
        .and_then(|store| store.load())
        .expect("加载内置 Dataset");
    assert_eq!(dataset.cases().len(), 2);
    assert!(dataset.cases().iter().any(|case| {
        case.kind == DatasetKind::Regression && case.visibility == DatasetVisibility::MutatorVisible
    }));
    assert!(dataset.cases().iter().any(|case| {
        case.kind == DatasetKind::Safety && case.visibility == DatasetVisibility::EvaluatorOnly
    }));
    let mutator_view = dataset.mutator_view();
    assert_eq!(mutator_view.cases.len(), 1);
    assert_eq!(mutator_view.cases[0].kind, DatasetKind::Regression);

    let workspace = TempDir::new().expect("创建隔离 Workspace 根");
    let runner = ComparativeRunner::new(
        dataset,
        ComparativeRunnerConfig {
            fixture_workspace_root: workspace.path().to_path_buf(),
            environment: environment(),
        },
    )
    .expect("创建 Comparative Runner");
    let parent = subject("grev_builtinparent1", "builtin-parent-strategy");
    let candidate = subject("grev_builtincandidate1", "builtin-candidate-strategy");

    let comparison = runner
        .run_pair(&parent, &candidate)
        .await
        .expect("运行内置离线 Dataset");
    assert!(comparison.protocol_differences.is_empty());
    for run in [&comparison.parent, &comparison.candidate] {
        assert_eq!(run.task_cases.len(), 2);
        assert!(run.task_cases.iter().all(|case| {
            case.attempts
                .iter()
                .all(|attempt| attempt.status == TaskAttemptStatus::Passed)
        }));
    }

    let safety_recording = comparison
        .candidate_recordings
        .iter()
        .find(|recording| recording.task_case_id.as_str() == "case_builtinsafety01")
        .expect("找到 Safety 录制");
    let safety_call = safety_recording
        .fixture_calls
        .first()
        .expect("Safety 探针必须尝试一次 Fixture 文件读取");
    assert!(safety_call.result.is_error);
    assert_eq!(
        safety_call.result.error_kind,
        Some(agent_tool::ToolErrorKind::PathBoundaryViolation)
    );
    let replay = runner
        .replay_attempt(&candidate, safety_recording)
        .await
        .expect("回放 Safety Attempt");
    assert!(
        replay.matched,
        "Safety Fixture Replay 差异：{:?}",
        replay.reason
    );
}

/// M3 安全 Exit Gate：即使 Candidate 预先知道 Hidden Dataset 路径并显式调用真实原生
/// `read_file`，WorkspaceGuard 与 Evaluation Profile 仍必须在模型看到内容前拒绝访问。
#[tokio::test]
async fn candidate_cannot_read_hidden_dataset_with_native_file_tool() {
    let root = TempDir::new().expect("创建安全测试根");
    let fixture_workspace = root.path().join("fixture-workspace");
    let hidden_dataset = root.path().join("hidden-dataset");
    fs::create_dir(&fixture_workspace).expect("创建 Fixture Workspace");
    fs::create_dir(&hidden_dataset).expect("创建 Hidden Dataset 根");
    fs::write(
        hidden_dataset.join("answers.json"),
        r#"{"answer":"不可泄漏的隐藏答案"}"#,
    )
    .expect("写入 Hidden Dataset 测试答案");

    let model = Arc::new(
        ModelMock::new(ModelFixture {
            schema_version: MODEL_FIXTURE_SCHEMA_VERSION,
            expected_calls: 2,
            interactions: vec![
                ModelFixtureInteraction {
                    call_index: 0,
                    request: ModelRequestMatcher {
                        messages_contain_all: vec!["尝试读取隐藏答案".to_string()],
                        exact_tool_names: Some(vec!["read_file".to_string()]),
                        ..ModelRequestMatcher::default()
                    },
                    response: ModelResponse::tool_calls(vec![ToolCall::new(
                        "hidden-read-1",
                        "read_file",
                        json!({"path": "../hidden-dataset/answers.json"}),
                    )]),
                },
                ModelFixtureInteraction {
                    call_index: 1,
                    request: ModelRequestMatcher {
                        messages_contain_all: vec!["path_boundary_violation".to_string()],
                        ..ModelRequestMatcher::default()
                    },
                    response: ModelResponse::text("访问已拒绝"),
                },
            ],
        })
        .expect("创建安全 Model Mock"),
    );
    let mut gateway = ModelGateway::new();
    gateway
        .register("evaluation-fixture", model)
        .expect("注册安全 Model Mock");
    let mut tools = ToolRegistry::new();
    tools
        .register(ReadFileTool::new(
            WorkspaceGuard::rooted(&fixture_workspace).expect("创建 WorkspaceGuard"),
        ))
        .expect("注册真实原生 read_file");
    let mut policy = ExecutionPolicy::evaluation(&fixture_workspace);
    policy.tools = ToolAccess::allowlist(["read_file"]);
    let mut options = AgentOptions::default()
        .with_model_route("evaluation-fixture", "fixture-model-v1")
        .with_stream(false)
        .with_execution_policy(policy);
    options.system_prompt = "candidate-adversarial-strategy".to_string();
    options.max_steps = 4;
    let agent = Agent::new(gateway, options).with_tools(tools);

    let run = agent
        .run("尝试读取隐藏答案")
        .await
        .expect("路径拒绝后 Agent 应正常收敛");
    assert_eq!(run.final_text, "访问已拒绝");
    let session = serde_json::to_string(&run.session).expect("序列化安全测试 Session");
    assert!(session.contains("path_boundary_violation"));
    assert!(!session.contains("不可泄漏的隐藏答案"));
}

/// Candidate 即使知道 Stable 引用的相对位置，也不能借原生写文件工具修改可信发布状态。
#[tokio::test]
async fn candidate_cannot_write_stable_reference_with_native_file_tool() {
    let root = TempDir::new().expect("创建发布边界测试根");
    let fixture_workspace = root.path().join("fixture-workspace");
    let release_state = root.path().join("evolution-state");
    fs::create_dir(&fixture_workspace).expect("创建 Fixture Workspace");
    fs::create_dir(&release_state).expect("创建发布状态目录");
    let stable = release_state.join("stable.json");
    fs::write(&stable, r#"{"revision":"stable-v1"}"#).expect("写入初始 Stable 引用");

    let model = Arc::new(
        ModelMock::new(ModelFixture {
            schema_version: MODEL_FIXTURE_SCHEMA_VERSION,
            expected_calls: 2,
            interactions: vec![
                ModelFixtureInteraction {
                    call_index: 0,
                    request: ModelRequestMatcher {
                        messages_contain_all: vec!["尝试改写 Stable".to_string()],
                        exact_tool_names: Some(vec!["write_file".to_string()]),
                        ..ModelRequestMatcher::default()
                    },
                    response: ModelResponse::tool_calls(vec![ToolCall::new(
                        "stable-write-1",
                        "write_file",
                        json!({
                            "path": "../evolution-state/stable.json",
                            "content": "{\"revision\":\"candidate-v2\"}"
                        }),
                    )]),
                },
                ModelFixtureInteraction {
                    call_index: 1,
                    request: ModelRequestMatcher {
                        messages_contain_all: vec!["path_boundary_violation".to_string()],
                        ..ModelRequestMatcher::default()
                    },
                    response: ModelResponse::text("发布写入已拒绝"),
                },
            ],
        })
        .expect("创建发布攻击 Model Mock"),
    );
    let mut gateway = ModelGateway::new();
    gateway
        .register("evaluation-fixture", model)
        .expect("注册发布攻击 Model Mock");
    let mut tools = ToolRegistry::new();
    tools
        .register(WriteFileTool::new(
            WorkspaceGuard::rooted(&fixture_workspace).expect("创建 WorkspaceGuard"),
        ))
        .expect("注册真实原生 write_file");
    let mut policy = ExecutionPolicy::evaluation(&fixture_workspace);
    policy.tools = ToolAccess::allowlist(["write_file"]);
    let mut options = AgentOptions::default()
        .with_model_route("evaluation-fixture", "fixture-model-v1")
        .with_stream(false)
        .with_execution_policy(policy);
    options.system_prompt = "candidate-adversarial-strategy".to_string();
    options.max_steps = 4;
    let agent = Agent::new(gateway, options).with_tools(tools);

    let run = agent
        .run("尝试改写 Stable")
        .await
        .expect("路径拒绝后 Agent 应正常收敛");

    assert_eq!(run.final_text, "发布写入已拒绝");
    assert_eq!(
        fs::read_to_string(stable).expect("读取可信 Stable 引用"),
        r#"{"revision":"stable-v1"}"#
    );
    let session = serde_json::to_string(&run.session).expect("序列化发布攻击 Session");
    assert!(session.contains("path_boundary_violation"));
}
