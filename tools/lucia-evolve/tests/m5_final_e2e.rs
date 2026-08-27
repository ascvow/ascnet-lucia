//! M5 真实失败到双进程评测、发布后 Serve、健康回滚与归档保留端到端测试。

use agent_core::{
    Agent, AgentOptions, ChatModel, ModelGateway, ModelRequest, ModelResponse, ProviderAdapter,
    Session,
};
use agent_evaluation::{
    DatasetCaseRef, DatasetManifest, DatasetVisibility, ModelFixture, ModelFixtureInteraction,
    ModelRequestMatcher, TaskBudgets, TaskCase, TaskInput, TrustedArtifactRef,
    TrustedEvaluationArchive, VerifierCheck, VerifierRule, DATASET_MANIFEST_SCHEMA_VERSION,
    MODEL_FIXTURE_SCHEMA_VERSION, TASK_CASE_SCHEMA_VERSION, VERIFIER_RULE_SCHEMA_VERSION,
};
use agent_evolution::{
    load_episode_evidence, ArtifactStore, EpisodeRecorderConfig, EpisodeRecorderHub, EpisodeStore,
    EvolutionCycleStore, EvolutionOutbox, EvolutionPipeline, FileArtifactStore, FileEpisodeStore,
    FileEvolutionCycleStore, FileEvolutionOutbox, FileGenomeResolver, FileGenomeStore,
    FileIssueObservationStore, FileOutcomeRevisionStore, FileRuntimeHealthObservationStore,
    FileStableGenomePublisher, GenomeResolver, GenomeSelector, GenomeStore, IssueObservationStore,
    RuntimeHealthRecorder, EVOLUTION_POLICY_VERSION, RUNTIME_HEALTH_DIRECTORY,
    TASK_STRATEGY_MVP_CANDIDATE_COUNT,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, DataClass, DatasetKind, DatasetVersionId,
    EvolutionCycleRequestInput, EvolutionCycleRequestV1, EvolutionCycleSnapshotV1,
    EvolutionCycleStage, EvolutionEligibility, FailureKind, GenomeMetadata, GenomeRevision,
    ModelGenome, Outcome, OutcomeResolution, PromptArtifactRef, PromptGenome, PromptLayer,
    ReplayabilityGrade, RuntimeIdentity, TaskCaseId, TaskDescriptor, ToolProfileGenome,
    GENOME_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tempfile::TempDir;

/// E2E 使用的固定 Stable lineage。
const TEST_LINEAGE: &str = "stable/m5-final";
/// Parent 的唯一 Task Strategy Prompt。
const PARENT_PROMPT: &str = "先完成任务，再报告可验证结果。";
/// Candidate A 的确定性 Prompt 标记。
const CANDIDATE_A_MARKER: &str = "执行前列出与任务结果有关的约束";
/// Candidate B 的确定性 Prompt 标记。
const CANDIDATE_B_MARKER: &str = "工具调用后必须检查真实结果";
/// Candidate C 的确定性 Prompt 标记。
const CANDIDATE_C_MARKER: &str = "给出最终结果前执行与任务契约对应的独立验收";
/// E2E 使用的固定 Dataset 版本。
const DATASET_VERSION: &str = "dsv_m5final001";

/// 返回固定文本，并可验证真实 Core Serve 请求绑定了 Promotion Prompt 的模型。
struct FixedModel {
    response: &'static str,
    expected_system: Option<String>,
    observed: Arc<AtomicBool>,
}

#[async_trait]
impl ChatModel for FixedModel {
    async fn complete(&self, request: ModelRequest) -> anyhow::Result<ModelResponse> {
        if let Some(expected) = &self.expected_system {
            let system = request.system.as_deref().unwrap_or_default();
            if system != expected {
                anyhow::bail!("真实 Serve Agent 未绑定 Promotion Prompt");
            }
        }
        self.observed.store(true, Ordering::Release);
        Ok(ModelResponse::text(self.response))
    }
}

#[async_trait]
impl ProviderAdapter for FixedModel {
    fn name(&self) -> &'static str {
        "m5-final-fixture"
    }
}

/// 计算测试制品原始字节的协议 SHA-256 摘要。
fn artifact_digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("测试制品摘要应合法")
}

/// 写入受摘要保护的 Dataset JSON 制品。
fn write_json<T: Serialize>(root: &Path, relative: &str, value: &T) -> TrustedArtifactRef {
    let bytes = serde_json::to_vec_pretty(value).expect("测试制品应可序列化");
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("测试制品应有父目录")).expect("创建测试制品目录");
    fs::write(&path, &bytes).expect("写入测试制品");
    TrustedArtifactRef {
        path: relative.to_string(),
        digest: artifact_digest(&bytes),
    }
}

/// 返回 Dataset 类型的稳定小写名称。
fn dataset_name(kind: DatasetKind) -> &'static str {
    match kind {
        DatasetKind::Repair => "repair",
        DatasetKind::Regression => "regression",
        DatasetKind::Hidden => "hidden",
        DatasetKind::Safety => "safety",
    }
}

/// 返回每类 Dataset 的可信 Verifier 期望文本。
fn expected_output(kind: DatasetKind) -> &'static str {
    match kind {
        DatasetKind::Repair => "repair-passed",
        DatasetKind::Regression => "regression-passed",
        DatasetKind::Hidden => "hidden-passed",
        DatasetKind::Safety => "safety-passed",
    }
}

/// 定义 Parent 和三个真实 Candidate 的离线评测行为。
fn strategy_output(marker: &str, kind: DatasetKind) -> &'static str {
    if marker == PARENT_PROMPT && kind == DatasetKind::Repair {
        return "repair-failed";
    }
    if marker == CANDIDATE_A_MARKER && kind == DatasetKind::Hidden {
        return "hidden-failed";
    }
    if marker == CANDIDATE_B_MARKER && kind == DatasetKind::Safety {
        return "safety-failed";
    }
    expected_output(kind)
}

/// 为一类 Dataset 构造覆盖 Parent 与三 Candidate 的真实模型 Fixture。
fn model_fixture(kind: DatasetKind) -> ModelFixture {
    let interactions = [
        CANDIDATE_A_MARKER,
        CANDIDATE_B_MARKER,
        CANDIDATE_C_MARKER,
        PARENT_PROMPT,
    ]
    .into_iter()
    .map(|marker| ModelFixtureInteraction {
        call_index: 0,
        request: ModelRequestMatcher {
            system_contains_all: vec![marker.to_string()],
            messages_contain_all: vec![format!("执行 {} 离线任务", dataset_name(kind))],
            exact_tool_names: Some(Vec::new()),
        },
        response: ModelResponse::text(strategy_output(marker, kind)),
    })
    .collect();
    ModelFixture {
        schema_version: MODEL_FIXTURE_SCHEMA_VERSION,
        expected_calls: 1,
        interactions,
    }
}

/// 构造内置精确文本 Verifier 规则。
fn verifier_rule(kind: DatasetKind) -> VerifierRule {
    VerifierRule {
        schema_version: VERIFIER_RULE_SCHEMA_VERSION,
        verifier_version: "builtin-v1".to_string(),
        checks: vec![VerifierCheck::ExactText {
            expected: expected_output(kind).to_string(),
        }],
    }
}

/// 返回各 Dataset 的候选可见性边界。
fn dataset_visibility(kind: DatasetKind) -> DatasetVisibility {
    match kind {
        DatasetKind::Repair => DatasetVisibility::MutatorVisible,
        DatasetKind::Regression => DatasetVisibility::Public,
        DatasetKind::Hidden | DatasetKind::Safety => DatasetVisibility::EvaluatorOnly,
    }
}

/// 创建 Repair、Regression、Hidden 与 Safety 四类受信 Dataset。
fn write_dataset(root: &Path) -> ArtifactDigest {
    fs::create_dir_all(root).expect("创建 Dataset 根");
    let mut cases = Vec::new();
    for kind in [
        DatasetKind::Repair,
        DatasetKind::Regression,
        DatasetKind::Hidden,
        DatasetKind::Safety,
    ] {
        let name = dataset_name(kind);
        let model = write_json(root, &format!("models/{name}.json"), &model_fixture(kind));
        let verifier = write_json(
            root,
            &format!("verifiers/{name}.json"),
            &verifier_rule(kind),
        );
        let id = TaskCaseId::new(format!("case_m5{name}01")).expect("测试 Case ID 应合法");
        let visibility = dataset_visibility(kind);
        let critical = kind == DatasetKind::Safety;
        let task_case = TaskCase {
            schema_version: TASK_CASE_SCHEMA_VERSION,
            id: id.clone(),
            version: 1,
            family: format!("m5.{name}"),
            kind,
            input: TaskInput::Text {
                text: format!("执行 {name} 离线任务"),
            },
            initial_environment: None,
            tool_fixture: None,
            model_mock: model,
            verifier,
            budgets: TaskBudgets {
                max_steps: 2,
                max_tokens: 64,
                wall_clock_ms: 2_000,
                max_tool_calls: 0,
            },
            repeats: 1,
            visibility,
            data_class: if matches!(kind, DatasetKind::Hidden | DatasetKind::Safety) {
                DataClass::Sensitive
            } else {
                DataClass::Internal
            },
            tags: vec!["m5-final".to_string(), name.to_string()],
            critical,
            deterministic: true,
            pass_threshold: Some(1.0),
        };
        let artifact = write_json(root, &format!("cases/{name}.json"), &task_case);
        cases.push(DatasetCaseRef {
            id,
            version: 1,
            family: format!("m5.{name}"),
            kind,
            visibility,
            critical,
            deterministic: true,
            artifact,
        });
    }
    let manifest = DatasetManifest {
        schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
        dataset_version: DatasetVersionId::new(DATASET_VERSION).expect("Dataset 版本应合法"),
        cases,
    };
    write_json(root, "manifest.json", &manifest).digest
}

/// 构造只含 Task Strategy Prompt 的可 Serve Parent Genome。
fn parent_revision(prompt: ArtifactDigest) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: env!("CARGO_PKG_VERSION").to_string(),
                git_commit: "m5-final-e2e".to_string(),
                git_dirty: false,
                target_triple: "test-target".to_string(),
                features: BTreeSet::new(),
            },
            model: ModelGenome {
                provider: "m5-final-fixture".to_string(),
                provider_kind: "fixture".to_string(),
                model: "fixture-model".to_string(),
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

/// 创建明确允许脱敏失败证据进入 Evolution 的 Episode 策略。
fn eligible_episode_config(revision: &GenomeRevision, session_id: &str) -> EpisodeRecorderConfig {
    let mut config = EpisodeRecorderConfig::online(session_id, revision.revision_id.clone());
    config.finalize_on_run_finished = false;
    config.replayability = ReplayabilityGrade::FixtureReproducible;
    config.task = TaskDescriptor {
        family: "m5-final".to_string(),
        input_ref: None,
        tags: BTreeSet::from(["verification".to_string()]),
    };
    config.data_policy =
        agent_evolution_protocol::EpisodeDataPolicy::for_class(DataClass::Internal);
    config.data_policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    config.data_policy.redaction_rules_version = Some("redaction-v1".to_string());
    config
}

/// 用真实 Core Agent 与 Recorder 产生可信 VerificationFailure Episode。
async fn record_failure_episode(
    evolution_root: &Path,
    revision: &GenomeRevision,
) -> agent_evolution::EpisodeEvidence {
    let artifacts = Arc::new(FileArtifactStore::new(evolution_root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(evolution_root.join("episodes")));
    let hub = Arc::new(EpisodeRecorderHub::new(artifacts.clone(), episodes.clone()));
    let config = eligible_episode_config(revision, "m5-final-source");
    let episode_id = config.episode_id.clone();
    let run = hub.register(config).await.expect("应登记真实失败 Run");
    let run_id = run.run_id().to_string();
    let mut gateway = ModelGateway::new();
    gateway
        .register(
            "m5-final-fixture",
            Arc::new(FixedModel {
                response: "未执行独立验收",
                expected_system: None,
                observed: Arc::new(AtomicBool::new(false)),
            }),
        )
        .expect("应注册失败 Fixture 模型");
    let mut options = AgentOptions::default().with_model_route("m5-final-fixture", "fixture-model");
    options.stream = false;
    Agent::new(gateway, options)
        .with_event_sink(hub.clone())
        .run_session_with_id(Session::new(), &run_id)
        .await
        .expect("真实失败前 Core Run 应完成");
    run.close_with_resolution(OutcomeResolution::verified_failure(
        FailureKind::VerificationFailure,
        "独立 Verifier 判定任务后置条件失败",
    ))
    .await
    .expect("可信 Verifier 应收敛失败 Episode");
    load_episode_evidence(episodes.as_ref(), artifacts.as_ref(), &episode_id)
        .await
        .expect("应恢复真实失败 Episode")
}

/// 从真实 Episode 运行 Evolution Pipeline 并返回唯一待消费 Outbox。
async fn route_failure_to_outbox(
    evolution_root: &Path,
    revision: &GenomeRevision,
    evidence: &agent_evolution::EpisodeEvidence,
) -> agent_evolution::EvolutionOutboxItem {
    let outbox = Arc::new(FileEvolutionOutbox::new(evolution_root.join("outbox")));
    let pipeline = EvolutionPipeline::new(
        outbox.clone(),
        Arc::new(FileOutcomeRevisionStore::new(
            evolution_root.join("outcome-revisions"),
        )),
    )
    .with_issue_observation_store(Arc::new(FileIssueObservationStore::new(
        evolution_root.join("issue-observations"),
    )));
    let written = pipeline
        .process_episode(
            &evidence.episode,
            &evidence.incidents,
            &revision.digest,
            evidence.initial_outcome_revision.as_ref(),
        )
        .await
        .expect("真实失败应进入 Evolution Pipeline");
    assert_eq!(written, 1);
    let pending = outbox.pending().await.expect("应读取真实 Outbox");
    assert_eq!(pending.len(), 1);
    let observations = FileIssueObservationStore::new(evolution_root.join("issue-observations"))
        .all()
        .await
        .expect("应读取真实 Issue 观察");
    assert_eq!(observations.len(), 1);
    assert_eq!(
        pending[0].issue_id.as_ref(),
        Some(&observations[0].issue_id)
    );
    pending.into_iter().next().expect("Outbox 应存在")
}

/// 定位与当前测试二进制共享 target profile 的真实 lucia-eval 可执行文件。
fn evaluator_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("LUCIA_M5_EVALUATOR_BIN") {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .expect("应定位测试二进制")
        .parent()
        .and_then(Path::parent)
        .expect("测试二进制应位于 target profile/deps")
        .join("lucia-eval")
}

/// 为 lucia-evolve 配置唯一受信双进程路径和 Dataset 绑定。
fn evolve_command(
    evaluator: &Path,
    evolution_root: &Path,
    dataset_root: &Path,
    workspace_root: &Path,
    archive_root: &Path,
    manifest_digest: &ArtifactDigest,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lucia-evolve"));
    command
        .env("LUCIA_EVOLVE_EVALUATOR_BIN", evaluator)
        .env("LUCIA_EVOLVE_EVOLUTION_ROOT", evolution_root)
        .env("LUCIA_EVOLVE_DATASET_VERSION", DATASET_VERSION)
        .env("LUCIA_EVAL_EVOLUTION_ROOT", evolution_root)
        .env("LUCIA_EVAL_DATASET_ROOT", dataset_root)
        .env("LUCIA_EVAL_WORKSPACE_ROOT", workspace_root)
        .env("LUCIA_EVAL_ARCHIVE_ROOT", archive_root)
        .env(
            "LUCIA_EVAL_DATASET_MANIFEST_DIGEST",
            manifest_digest.as_str(),
        )
        .env("LUCIA_EVAL_KERNEL_REF", "m5-final-kernel")
        .env(
            "LUCIA_EVAL_HEALTH_STORE_ROOT",
            evolution_root.join(RUNTIME_HEALTH_DIRECTORY),
        );
    command
}

/// 调用真实 lucia-evolve 子进程并解析唯一 JSON 回执。
fn invoke_evolve(
    mut command: Command,
    args: &[&str],
    stdin: Option<&impl Serialize>,
) -> EvolutionCycleSnapshotV1 {
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("应启动真实 lucia-evolve");
    if let Some(value) = stdin {
        child
            .stdin
            .take()
            .expect("Cycle 应打开 stdin")
            .write_all(&serde_json::to_vec(value).expect("请求应可序列化"))
            .expect("应写入 Cycle 请求");
    }
    let output = child.wait_with_output().expect("应等待真实 lucia-evolve");
    assert!(
        output.status.success(),
        "lucia-evolve 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("lucia-evolve 应返回 Cycle Snapshot JSON")
}

/// 使用 Promotion Prompt 运行真实 Core Serve Agent，并以可信失败终态形成 Episode。
async fn run_promoted_serve_failure(
    evolution_root: &Path,
    revision: &GenomeRevision,
    prompt: String,
) -> agent_evolution_protocol::Episode {
    let artifacts = Arc::new(FileArtifactStore::new(evolution_root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(evolution_root.join("episodes")));
    let hub = Arc::new(EpisodeRecorderHub::new(artifacts.clone(), episodes.clone()));
    let config = eligible_episode_config(revision, "m5-final-promoted-serve");
    let episode_id = config.episode_id.clone();
    let run = hub
        .register(config)
        .await
        .expect("应登记 Promotion Serve Run");
    let run_id = run.run_id().to_string();
    let observed = Arc::new(AtomicBool::new(false));
    let mut gateway = ModelGateway::new();
    gateway
        .register(
            "m5-final-fixture",
            Arc::new(FixedModel {
                response: "Promotion Serve 已执行",
                expected_system: Some(prompt.clone()),
                observed: observed.clone(),
            }),
        )
        .expect("应注册 Serve Fixture 模型");
    let mut options = AgentOptions::default().with_model_route("m5-final-fixture", "fixture-model");
    options.system_prompt = prompt;
    options.stream = false;
    let agent = Agent::new(gateway, options);
    let session = agent.prepare_session(Session::new(), "执行发布后健康任务");
    agent
        .with_event_sink(hub)
        .run_session_with_id(session, &run_id)
        .await
        .expect("真实 Promotion Serve 应完成");
    assert!(observed.load(Ordering::Acquire));
    run.close_with_resolution(OutcomeResolution::verified_failure(
        FailureKind::VerificationFailure,
        "发布后真实运行未通过健康后置条件",
    ))
    .await
    .expect("应收敛发布后失败 Episode");
    episodes
        .get(&episode_id)
        .await
        .expect("应读取发布后 Episode")
        .expect("发布后 Episode 应存在")
}

/// 真实闭环必须只晋升 Candidate C，并在后续失败健康观察后回滚且保留全部制品。
#[tokio::test]
#[ignore = "需要先构建真实 lucia-eval，再显式运行双进程验收"]
async fn real_failure_evolves_promotes_serves_rolls_back_and_preserves_archive() {
    let root = TempDir::new().expect("创建 M5 E2E 根");
    let evolution_root = root.path().join("evolution");
    let dataset_root = root.path().join("dataset");
    let workspace_root = root.path().join("workspace");
    let archive_root = root.path().join("archive");
    let evaluator = evaluator_binary();
    assert!(
        evaluator.is_absolute() && evaluator.is_file(),
        "请先构建真实 lucia-eval：{}",
        evaluator.display()
    );
    let manifest_digest = write_dataset(&dataset_root);
    fs::create_dir_all(&workspace_root).expect("创建 Evaluator Workspace");
    let artifacts = FileArtifactStore::new(evolution_root.join("artifacts"));
    let parent_prompt = artifacts
        .put("text/plain", PARENT_PROMPT.as_bytes())
        .await
        .expect("写入 Parent Prompt CAS");
    let parent = parent_revision(parent_prompt.digest);
    let genomes = FileGenomeStore::new(evolution_root.join("genomes"));
    genomes.append(&parent).await.expect("登记 Parent Genome");
    FileStableGenomePublisher::new(&evolution_root)
        .publish(TEST_LINEAGE, &parent, 1)
        .await
        .expect("发布初始 Stable Parent");

    let evidence = record_failure_episode(&evolution_root, &parent).await;
    assert_eq!(evidence.episode.outcome, Some(Outcome::TaskFailure));
    assert!(evidence
        .episode
        .failures
        .iter()
        .any(|failure| failure.kind == FailureKind::VerificationFailure));
    let outbox = route_failure_to_outbox(&evolution_root, &parent, &evidence).await;
    let request = EvolutionCycleRequestV1::create(EvolutionCycleRequestInput {
        issue_id: outbox.issue_id.clone().expect("Outbox 应绑定 Issue"),
        parent_revision_id: parent.revision_id.clone(),
        parent_genome_digest: parent.digest.clone(),
        lineage: TEST_LINEAGE.to_string(),
        expected_parent_generation: 1,
        source_episode_ids: vec![evidence.episode.episode_id.clone()],
        evolution_policy_version: EVOLUTION_POLICY_VERSION.to_string(),
        candidate_count: TASK_STRATEGY_MVP_CANDIDATE_COUNT as u32,
        requested_at_ms: evidence.episode.finished_at_ms,
    })
    .expect("真实 Cycle 请求应合法");
    let promoted = invoke_evolve(
        evolve_command(
            &evaluator,
            &evolution_root,
            &dataset_root,
            &workspace_root,
            &archive_root,
            &manifest_digest,
        ),
        &["cycle"],
        Some(&request),
    );
    assert_eq!(
        promoted.stage,
        EvolutionCycleStage::AwaitingHealth,
        "三候选评测未进入 Promotion：{promoted:#?}"
    );
    assert_eq!(promoted.candidates.len(), 3);
    assert_eq!(promoted.evaluation_receipts.len(), 3);
    let winner_id = promoted.winner.as_ref().expect("应选出 Candidate C");
    let winner = promoted
        .candidates
        .iter()
        .find(|candidate| &candidate.candidate_id == winner_id)
        .expect("Winner 应属于三 Candidate");
    let winner_prompt = String::from_utf8(
        artifacts
            .get(&winner.prompt.digest)
            .await
            .expect("读取 Winner Prompt CAS")
            .expect("Winner Prompt 应存在"),
    )
    .expect("Winner Prompt 应为 UTF-8");
    assert!(winner_prompt.contains(CANDIDATE_C_MARKER));
    let stable = FileGenomeResolver::new(&evolution_root)
        .stable_reference(TEST_LINEAGE)
        .await
        .expect("读取 Promotion Stable");
    assert_eq!(stable.revision_id, winner.candidate_revision_id);
    let promoted_revision = FileGenomeResolver::new(&evolution_root)
        .resolve(&GenomeSelector::Stable(TEST_LINEAGE.to_string()))
        .await
        .expect("重新解析 Promotion Genome");
    let health_recorder = RuntimeHealthRecorder::from_stable(&evolution_root, TEST_LINEAGE)
        .await
        .expect("装配可信健康记录器")
        .expect("Promotion Stable 应等待健康观察");
    let post_promotion_episode =
        run_promoted_serve_failure(&evolution_root, &promoted_revision, winner_prompt).await;
    let health_observation = health_recorder
        .record_first_episode(&post_promotion_episode)
        .await
        .expect("真实发布后 Episode 应生成健康观察");
    assert_eq!(health_observation.observation().checks_passed, 1);
    assert_eq!(health_observation.observation().checks_total, 2);
    let release_id = promoted
        .release_receipt
        .as_ref()
        .expect("Promotion Receipt 应存在")
        .release_id
        .clone();
    FileRuntimeHealthObservationStore::new(evolution_root.join(RUNTIME_HEALTH_DIRECTORY))
        .expect("健康 Store 根应合法")
        .load(&release_id)
        .await
        .expect("独立 Store 应复核健康观察");

    let rolled_back = invoke_evolve(
        evolve_command(
            &evaluator,
            &evolution_root,
            &dataset_root,
            &workspace_root,
            &archive_root,
            &manifest_digest,
        ),
        &["health", "--cycle-id", promoted.cycle_id.as_str()],
        None::<&EvolutionCycleRequestV1>,
    );
    assert_eq!(rolled_back.stage, EvolutionCycleStage::RolledBack);
    assert!(
        !rolled_back
            .health_receipt
            .as_ref()
            .expect("Health Receipt 应归档")
            .verified
    );
    assert_eq!(
        rolled_back
            .rollback_receipt
            .as_ref()
            .expect("Rollback Receipt 应归档")
            .rollback_of
            .as_ref(),
        Some(&release_id)
    );
    let rolled_back_stable = FileGenomeResolver::new(&evolution_root)
        .stable_reference(TEST_LINEAGE)
        .await
        .expect("读取回滚 Stable");
    assert_eq!(rolled_back_stable.revision_id, parent.revision_id);
    assert_eq!(rolled_back_stable.rollback_of.as_ref(), Some(&release_id));

    let archive = TrustedEvaluationArchive::new(&archive_root);
    for receipt in &rolled_back.evaluation_receipts {
        archive
            .get_verified(&receipt.report_id)
            .await
            .expect("Rollback 后三份 EvaluationReport 必须仍可复核");
    }
    for candidate in &rolled_back.candidates {
        genomes
            .get(&candidate.candidate_revision_id)
            .await
            .expect("读取 Candidate Genome")
            .expect("Rollback 后 Candidate Genome 必须保留");
        artifacts
            .get(&candidate.prompt.digest)
            .await
            .expect("读取 Candidate Prompt")
            .expect("Rollback 后 Candidate Prompt 必须保留");
    }
    let history = FileEvolutionCycleStore::new(evolution_root.join("cycles"))
        .history(&request.cycle_id)
        .await
        .expect("读取只追加 Cycle Archive");
    assert!(history
        .iter()
        .any(|snapshot| snapshot.stage == EvolutionCycleStage::AwaitingHealth));
    assert_eq!(history.last(), Some(&rolled_back));
}
