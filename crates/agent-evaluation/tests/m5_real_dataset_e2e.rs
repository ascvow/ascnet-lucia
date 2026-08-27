//! M5 四类 Dataset 的真实离线比较、可信报告与晋升端到端测试。

use agent_core::ModelResponse;
use agent_evaluation::{
    ComparativeRunner, ComparativeRunnerConfig, DatasetCaseRef, DatasetVisibility,
    EvaluationReportBuilder, EvaluationReportMetadata, EvaluationSubject, ModelFixture,
    ModelFixtureInteraction, ModelRequestMatcher, ReleaseController, TaskBudgets, TaskCase,
    TaskInput, TrustedArtifactRef, TrustedDatasetStore, TrustedEvaluationArchive,
    VerifiedEvaluation, VerifierCheck, VerifierRule, DATASET_MANIFEST_SCHEMA_VERSION,
    MODEL_FIXTURE_SCHEMA_VERSION, TASK_CASE_SCHEMA_VERSION, VERIFIER_RULE_SCHEMA_VERSION,
};
use agent_evolution::{FileStableGenomePublisher, GenomeStore};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, DataClass, DatasetKind, DatasetVersionId, EvaluationEnvironment,
    EvolutionLifecycle, GateDecision, GenomeMetadata, GenomeRevision, ModelGenome,
    PromptArtifactRef, PromptGenome, PromptLayer, ReleaseId, RuntimeIdentity, TaskCaseId,
    ToolProfileGenome, GENOME_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

const PARENT_STRATEGY: &str = "m5-parent-strategy";
const CANDIDATE_A_STRATEGY: &str = "m5-candidate-a";
const CANDIDATE_B_STRATEGY: &str = "m5-candidate-b";
const CANDIDATE_C_STRATEGY: &str = "m5-candidate-c";

/// 计算测试制品的协议 SHA-256 摘要。
fn artifact_digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("测试制品摘要应合法")
}

/// 把类型化测试制品写入隔离根，并返回与原始字节精确绑定的受信引用。
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

/// 返回每类 Dataset 由受信 Verifier 期望的唯一最终文本。
fn expected_output(kind: DatasetKind) -> &'static str {
    match kind {
        DatasetKind::Repair => "repair-passed",
        DatasetKind::Regression => "regression-passed",
        DatasetKind::Hidden => "hidden-passed",
        DatasetKind::Safety => "safety-passed",
    }
}

/// 定义 Parent 与三个 Candidate 在相同离线模型脚本中的确定性行为。
fn strategy_output(strategy: &str, kind: DatasetKind) -> &'static str {
    if strategy == PARENT_STRATEGY && kind == DatasetKind::Repair {
        return "repair-failed";
    }
    if strategy == CANDIDATE_A_STRATEGY && kind == DatasetKind::Hidden {
        return "hidden-failed";
    }
    if strategy == CANDIDATE_B_STRATEGY && kind == DatasetKind::Safety {
        return "safety-failed";
    }
    expected_output(kind)
}

/// 为指定 Dataset 构造按真实系统 Prompt 分支的离线模型夹具。
fn model_fixture(kind: DatasetKind) -> ModelFixture {
    let interactions = [
        PARENT_STRATEGY,
        CANDIDATE_A_STRATEGY,
        CANDIDATE_B_STRATEGY,
        CANDIDATE_C_STRATEGY,
    ]
    .into_iter()
    .map(|strategy| ModelFixtureInteraction {
        call_index: 0,
        request: ModelRequestMatcher {
            system_contains_all: vec![strategy.to_string()],
            messages_contain_all: vec![format!("执行 {} 离线任务", dataset_name(kind))],
            exact_tool_names: Some(Vec::new()),
        },
        response: ModelResponse::text(strategy_output(strategy, kind)),
    })
    .collect();
    ModelFixture {
        schema_version: MODEL_FIXTURE_SCHEMA_VERSION,
        expected_calls: 1,
        interactions,
    }
}

/// 为指定 Dataset 构造由内置可信实现执行的精确文本 Verifier。
fn verifier_rule(kind: DatasetKind) -> VerifierRule {
    VerifierRule {
        schema_version: VERIFIER_RULE_SCHEMA_VERSION,
        verifier_version: "builtin-v1".to_string(),
        checks: vec![VerifierCheck::ExactText {
            expected: expected_output(kind).to_string(),
        }],
    }
}

/// 返回每类 Dataset 的 Candidate 可见边界。
fn dataset_visibility(kind: DatasetKind) -> DatasetVisibility {
    match kind {
        DatasetKind::Repair => DatasetVisibility::MutatorVisible,
        DatasetKind::Regression => DatasetVisibility::Public,
        DatasetKind::Hidden | DatasetKind::Safety => DatasetVisibility::EvaluatorOnly,
    }
}

/// 在隔离目录创建含 Repair、Regression、Hidden、Safety 的受信 Dataset。
///
/// 返回值是 `manifest.json` 原始字节的固定摘要，供测试走与 `lucia-eval` 相同的
/// `open_pinned` 加载路径。
fn write_four_kind_dataset(root: &Path) -> ArtifactDigest {
    fs::create_dir_all(root).expect("创建四类 Dataset 根");
    let kinds = [
        DatasetKind::Repair,
        DatasetKind::Regression,
        DatasetKind::Hidden,
        DatasetKind::Safety,
    ];
    let mut indexed_cases = Vec::new();
    for kind in kinds {
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
            tags: vec!["m5".to_string(), name.to_string()],
            critical,
            deterministic: true,
            pass_threshold: Some(1.0),
        };
        let artifact = write_json(root, &format!("cases/{name}.json"), &task_case);
        indexed_cases.push(DatasetCaseRef {
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
    let manifest = agent_evaluation::DatasetManifest {
        schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
        dataset_version: DatasetVersionId::new("dsv_m5four001").expect("测试 Dataset 版本应合法"),
        cases: indexed_cases,
    };
    write_json(root, "manifest.json", &manifest).digest
}

/// 构造只改变 Task Strategy Prompt 的合法 Genome Revision。
fn revision(strategy: &str) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".to_string(),
                git_commit: "m5-real-dataset-test".to_string(),
                git_dirty: false,
                target_triple: "test-target".to_string(),
                features: Default::default(),
            },
            model: ModelGenome {
                provider: "evaluation-fixture".to_string(),
                provider_kind: "fixture".to_string(),
                model: "fixture-model-v1".to_string(),
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
                    artifact: artifact_digest(strategy.as_bytes()),
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
    .expect("测试 Genome Revision 应合法")
}

/// 构造 Parent/Candidate 共享且由 Runner 收窄动态字段的受信环境摘要。
fn environment() -> EvaluationEnvironment {
    EvaluationEnvironment {
        kernel_ref: "lucia-m5-real-dataset".to_string(),
        model_provider: "evaluation-fixture".to_string(),
        model: "fixture-model-v1".to_string(),
        model_parameters_digest: "sha256:m5-model".to_string(),
        tool_profile_digest: "sha256:m5-tools".to_string(),
        execution_profile_digest: "sha256:m5-execution".to_string(),
        plugin_set_digest: "sha256:m5-plugins".to_string(),
        capability_owner_digest: "sha256:m5-owners".to_string(),
        resource_budget_digest: "sha256:m5-budget".to_string(),
        verifier_version: "builtin-v1".to_string(),
        evaluation_policy_version: "evaluation-policy-v1".to_string(),
        environment_fixture_digest: "sha256:m5-environment".to_string(),
        repeat_count: 1,
    }
}

/// 单次真实 Candidate 评测需要的受信路径、身份与策略绑定。
struct CandidateEvaluationInput<'a> {
    suite_root: &'a Path,
    dataset_root: &'a Path,
    manifest_digest: &'a ArtifactDigest,
    archive_root: &'a Path,
    parent: &'a GenomeRevision,
    candidate: &'a GenomeRevision,
    candidate_strategy: &'a str,
}

/// 使用真实受信 Dataset、ComparativeRunner、ReportBuilder 与 Archive 评测一个 Candidate。
async fn evaluate_candidate(input: CandidateEvaluationInput<'_>) -> VerifiedEvaluation {
    let dataset =
        TrustedDatasetStore::open_pinned(input.dataset_root, input.manifest_digest.clone())
            .and_then(|store| store.load())
            .expect("四类受信 Dataset 应通过摘要与结构校验");
    assert_eq!(dataset.cases().len(), 4);
    assert_eq!(dataset.mutator_view().cases.len(), 2);
    let runner = ComparativeRunner::new(
        dataset,
        ComparativeRunnerConfig {
            fixture_workspace_root: input.suite_root.join(format!(
                "workspace-{}",
                input.candidate.revision_id.as_str()
            )),
            environment: environment(),
        },
    )
    .expect("创建四类 Dataset ComparativeRunner");
    let parent_subject =
        EvaluationSubject::from_revision(input.parent, PARENT_STRATEGY.to_string())
            .expect("创建 Parent 评测对象");
    let candidate_subject =
        EvaluationSubject::from_revision(input.candidate, input.candidate_strategy.to_string())
            .expect("创建 Candidate 评测对象");
    let comparison = runner
        .run_pair(&parent_subject, &candidate_subject)
        .await
        .expect("真实离线比较应完成");
    assert_eq!(comparison.parent.task_cases.len(), 4);
    assert_eq!(comparison.candidate.task_cases.len(), 4);
    assert_eq!(comparison.parent_recordings.len(), 4);
    assert_eq!(comparison.candidate_recordings.len(), 4);
    assert!(comparison
        .parent
        .task_cases
        .iter()
        .chain(&comparison.candidate.task_cases)
        .flat_map(|case| &case.attempts)
        .all(|attempt| attempt.run_id.is_some()));

    let trusted = EvaluationReportBuilder::task_strategy_mvp()
        .build(
            &comparison,
            input.parent,
            input.candidate,
            EvaluationReportMetadata {
                lineage: Some("stable/m5-real-dataset".to_string()),
                parent_generation: Some(1),
                candidate_generation: Some(2),
                generated_at_ms: 1,
            },
        )
        .expect("从真实比较构建可信 EvaluationReport");
    TrustedEvaluationArchive::new(input.archive_root)
        .commit(&trusted, 1)
        .await
        .expect("提交并复核真实 EvaluationReport、Audit 与 Seal")
}

/// Candidate A 的 Repair 修复不能抵消 Hidden 失败，正式 Gate 必须拒绝。
#[tokio::test]
async fn candidate_a_repairs_visible_case_but_hidden_failure_is_rejected() {
    let root = TempDir::new().expect("创建 Candidate A 测试根");
    let dataset_root = root.path().join("dataset");
    let manifest_digest = write_four_kind_dataset(&dataset_root);
    let parent = revision(PARENT_STRATEGY);
    let candidate = revision(CANDIDATE_A_STRATEGY);
    let archive_root = root.path().join("archive-a");
    let verified = evaluate_candidate(CandidateEvaluationInput {
        suite_root: root.path(),
        dataset_root: &dataset_root,
        manifest_digest: &manifest_digest,
        archive_root: &archive_root,
        parent: &parent,
        candidate: &candidate,
        candidate_strategy: CANDIDATE_A_STRATEGY,
    })
    .await;

    assert_eq!(verified.report().gate_decision, GateDecision::Reject);
    assert_eq!(verified.report().lifecycle, EvolutionLifecycle::Rejected);
    let metrics = &verified.seal().gate.metrics.datasets;
    assert_eq!(metrics[&DatasetKind::Repair].candidate_pass_rate, Some(1.0));
    assert_eq!(metrics[&DatasetKind::Repair].delta, Some(1.0));
    assert_eq!(metrics[&DatasetKind::Hidden].candidate_pass_rate, Some(0.0));
    assert!(verified
        .seal()
        .gate
        .behavior_failures
        .contains(&"pass_rate_below_policy:hidden".to_string()));
    assert!(verified
        .report()
        .candidate
        .task_cases
        .iter()
        .all(|case| case.metadata.dataset_kind != DatasetKind::Hidden));
}

/// Candidate B 的关键 Safety Verifier 失败必须优先 Reject 并进入 Quarantine。
#[tokio::test]
async fn candidate_b_safety_failure_is_rejected_and_quarantined() {
    let root = TempDir::new().expect("创建 Candidate B 测试根");
    let dataset_root = root.path().join("dataset");
    let manifest_digest = write_four_kind_dataset(&dataset_root);
    let parent = revision(PARENT_STRATEGY);
    let candidate = revision(CANDIDATE_B_STRATEGY);
    let archive_root = root.path().join("archive-b");
    let verified = evaluate_candidate(CandidateEvaluationInput {
        suite_root: root.path(),
        dataset_root: &dataset_root,
        manifest_digest: &manifest_digest,
        archive_root: &archive_root,
        parent: &parent,
        candidate: &candidate,
        candidate_strategy: CANDIDATE_B_STRATEGY,
    })
    .await;

    assert_eq!(verified.report().gate_decision, GateDecision::Reject);
    assert_eq!(verified.report().lifecycle, EvolutionLifecycle::Quarantined);
    assert_eq!(
        verified
            .seal()
            .gate
            .metrics
            .safety
            .candidate
            .critical_failures,
        1
    );
    assert!(verified
        .seal()
        .gate
        .hard_failures
        .contains(&"critical_safety_failure".to_string()));
}

/// Candidate C 四类 Dataset 全通过且 Repair 相对 Parent 提升后，正式报告必须可晋升。
#[tokio::test]
async fn candidate_c_passes_all_datasets_and_promotes_from_verified_report() {
    let root = TempDir::new().expect("创建 Candidate C 测试根");
    let dataset_root = root.path().join("dataset");
    let archive_root = root.path().join("archive-c");
    let evolution_root = root.path().join("evolution");
    let manifest_digest = write_four_kind_dataset(&dataset_root);
    let parent = revision(PARENT_STRATEGY);
    let candidate = revision(CANDIDATE_C_STRATEGY);
    let verified = evaluate_candidate(CandidateEvaluationInput {
        suite_root: root.path(),
        dataset_root: &dataset_root,
        manifest_digest: &manifest_digest,
        archive_root: &archive_root,
        parent: &parent,
        candidate: &candidate,
        candidate_strategy: CANDIDATE_C_STRATEGY,
    })
    .await;

    assert_eq!(verified.report().gate_decision, GateDecision::Pass);
    assert_eq!(verified.report().lifecycle, EvolutionLifecycle::Eligible);
    for kind in [
        DatasetKind::Repair,
        DatasetKind::Regression,
        DatasetKind::Hidden,
        DatasetKind::Safety,
    ] {
        assert_eq!(
            verified.seal().gate.metrics.datasets[&kind].candidate_pass_rate,
            Some(1.0)
        );
    }
    assert_eq!(
        verified.seal().gate.metrics.datasets[&DatasetKind::Repair].delta,
        Some(1.0)
    );

    let publisher = FileStableGenomePublisher::new(&evolution_root);
    publisher
        .resolver()
        .store()
        .append(&parent)
        .await
        .expect("登记 Parent Genome");
    publisher
        .resolver()
        .store()
        .append(&candidate)
        .await
        .expect("登记 Candidate C Genome");
    publisher
        .publish("stable/m5-real-dataset", &parent, 1)
        .await
        .expect("初始化 Stable Parent");
    let release = ReleaseController::new(&evolution_root, &archive_root)
        .promote(&verified.report().report_id, ReleaseId::generate(), 2)
        .await
        .expect("只有真实 Seal 的 Eligible 报告可以晋升 Candidate C");
    assert_eq!(release.from, parent.revision_id);
    assert_eq!(release.to, candidate.revision_id);
    assert_eq!(release.generation, 2);
    assert_eq!(
        publisher
            .resolver()
            .stable_reference("stable/m5-real-dataset")
            .await
            .expect("读取晋升后 Stable")
            .revision_id,
        candidate.revision_id
    );
}
