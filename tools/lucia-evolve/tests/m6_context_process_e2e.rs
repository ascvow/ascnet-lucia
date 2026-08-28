//! M6 Context Policy 独立双进程 Exit Gate。

use agent_evaluation::{
    ContextObservationFixtureV1, FileRuntimeHealthObservationStore, TrustedEvaluationArchive,
    CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION,
};
use agent_evolution::{
    ArtifactStore, BoundedContextMutator, ContextCandidateBuilder, ContextCycleStage,
    ContextEvolutionCycleRequestV1, ContextEvolutionCycleSnapshotV1, ContextPolicyRepository,
    EpisodeStore, EvolutionOutbox, EvolutionOutboxItem, FileArtifactStore, FileEpisodeStore,
    FileEvolutionOutbox, FileGenomeResolver, FileGenomeStore, FileStableGenomePublisher,
    GenomeResolver, GenomeSelector, GenomeStore, RUNTIME_HEALTH_DIRECTORY,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ContextEvaluationObservationV1, ContextGateFailureV1,
    ContextPolicyV1, DataClass, DatasetVersionId, DiagnosticStatus, Episode, EpisodeDataPolicy,
    EpisodeId, EvolutionCycleId, EvolutionEligibility, FailureClassification, FailureDisposition,
    FailureKind, GateDecision, GenomeMetadata, GenomeRevision, ModelGenome, Outcome, PolicyRef,
    RecallObservationV1, ReplayabilityGrade, RunId, RuntimeHealthObservationV1, RuntimeIdentity,
    TaskDescriptor, ToolProfileGenome, UsageSummary, CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
    EPISODE_SCHEMA_VERSION, EVALUATION_REQUEST_SCHEMA_VERSION, GENOME_SCHEMA_VERSION,
    M6_CONTEXT_GATE_VERSION, NATIVE_CONTEXT_POLICY_ID,
};
use agent_tool::{ExecutionPolicy, ToolAccess};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

const TEST_LINEAGE: &str = "stable/context-process";

/// 计算固定 Fixture 原始字节使用的 SHA-256 摘要。
fn artifact_digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 摘要应符合协议")
}

/// 构造包含原生 Context Policy 且允许生产晋升的 Parent Genome。
fn parent_revision(policy_digest: ArtifactDigest) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".to_string(),
                git_commit: "m6-context-process-e2e".to_string(),
                git_dirty: false,
                target_triple: "test-target".to_string(),
                features: BTreeSet::from(["plugins".to_string()]),
            },
            model: ModelGenome {
                provider: "fixture".to_string(),
                provider_kind: "fixture".to_string(),
                model: "deterministic".to_string(),
                base_url: None,
                protocol: None,
                max_tokens: Some(4_096),
                temperature: None,
                stream: false,
                provider_options_digest: None,
            },
            prompt: Default::default(),
            plugins: Vec::new(),
            capability_owners: Default::default(),
            tools: ToolProfileGenome {
                native_tools: BTreeSet::new(),
                access: ToolAccess::All,
            },
            context_policy: Some(PolicyRef {
                id: NATIVE_CONTEXT_POLICY_ID.to_string(),
                config_digest: policy_digest,
            }),
            planning_policy: None,
            skills: Vec::new(),
            execution: ExecutionPolicy::serve(),
        },
        GenomeMetadata::default(),
    )
    .expect("Parent Genome 应合法")
}

/// 构造明确允许 Context Mutator 使用的脱敏失败 Episode。
fn failure_episode(
    parent: &GenomeRevision,
    event_stream_ref: agent_evolution_protocol::ArtifactRef,
) -> Episode {
    let mut data_policy = EpisodeDataPolicy::for_class(DataClass::Internal);
    data_policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    data_policy.redaction_rules_version = Some("redaction-v1".to_string());
    Episode {
        schema_version: EPISODE_SCHEMA_VERSION,
        episode_id: EpisodeId::generate(),
        run_id: RunId::generate(),
        session_id: "m6-context-process-e2e".to_string(),
        genome_revision_id: parent.revision_id.clone(),
        task: TaskDescriptor {
            family: "context-retention".to_string(),
            input_ref: None,
            tags: BTreeSet::from(["context".to_string()]),
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
        data_policy,
        event_count: 0,
        started_at_ms: 1,
        finished_at_ms: 2,
    }
}

/// 构造分母固定为 100 的召回观察。
fn recall(recalled: u64) -> RecallObservationV1 {
    RecallObservationV1 {
        expected: 100,
        recalled,
    }
}

/// 从五项召回、token、成本和延迟构造固定 Context 观察。
#[allow(clippy::too_many_arguments)]
fn observation(
    facts: u64,
    constraints: u64,
    tools: u64,
    plan: u64,
    downstream: u64,
    tokens_after: u64,
    cost: u64,
    latency: u64,
) -> ContextEvaluationObservationV1 {
    ContextEvaluationObservationV1 {
        schema_version: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
        facts: recall(facts),
        constraints: recall(constraints),
        tool_states: recall(tools),
        plan_states: recall(plan),
        downstream_tasks: recall(downstream),
        tokens_before: 10_000,
        tokens_after,
        cost_microunits: cost,
        latency_ms: latency,
    }
}

/// 返回与当前测试 profile 对应的真实 `lucia-eval` 二进制。
fn evaluator_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("LUCIA_M6_EVALUATOR_BIN") {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .expect("应定位测试二进制")
        .parent()
        .and_then(Path::parent)
        .expect("测试二进制应位于 target profile/deps")
        .join("lucia-eval")
}

/// 为 `lucia-evolve` 固定独立 Evaluator、Context Fixture、Archive 与健康 Store。
fn evolve_command(
    evaluator: &Path,
    evolution_root: &Path,
    archive_root: &Path,
    fixture_root: &Path,
    fixture_version: &DatasetVersionId,
    fixture_digest: &ArtifactDigest,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lucia-evolve"));
    command
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env("LUCIA_EVOLVE_EVALUATOR_BIN", evaluator)
        .env("LUCIA_EVOLVE_EVOLUTION_ROOT", evolution_root)
        .env(
            "LUCIA_EVOLVE_CONTEXT_FIXTURE_VERSION",
            fixture_version.as_str(),
        )
        .env("LUCIA_EVAL_EVOLUTION_ROOT", evolution_root)
        .env("LUCIA_EVAL_ARCHIVE_ROOT", archive_root)
        .env("LUCIA_EVAL_CONTEXT_FIXTURE_ROOT", fixture_root)
        .env("LUCIA_EVAL_CONTEXT_FIXTURE_DIGEST", fixture_digest.as_str())
        .env(
            "LUCIA_EVAL_HEALTH_STORE_ROOT",
            evolution_root.join(RUNTIME_HEALTH_DIRECTORY),
        );
    command
}

/// 调用真实 `lucia-evolve` 子进程并解析唯一 JSON 回执。
fn invoke_evolve<T: DeserializeOwned>(
    mut command: Command,
    args: &[&str],
    stdin: Option<&impl Serialize>,
) -> T {
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
            .expect("命令应打开 stdin")
            .write_all(&serde_json::to_vec(value).expect("请求应可序列化"))
            .expect("应写入请求");
    }
    let output = child.wait_with_output().expect("应等待真实 lucia-evolve");
    assert!(
        output.status.success(),
        "lucia-evolve 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("lucia-evolve 应返回合法 JSON")
}

/// M6 必须通过真实双进程 Gate 晋升唯一候选，健康失败后回滚并消费来源 Outbox。
#[tokio::test]
#[ignore = "需要先构建真实 lucia-eval，再显式运行双进程验收"]
async fn context_policy_promotes_rolls_back_preserves_archive_and_consumes_outbox() {
    let root = TempDir::new().expect("创建 M6 双进程测试根");
    let evolution_root = root.path().join("evolution");
    let archive_root = root.path().join("archive");
    let fixture_root = root.path().join("context-fixture");
    fs::create_dir_all(&fixture_root).expect("创建 Context Fixture 根");
    let evaluator = evaluator_binary();
    assert!(
        evaluator.is_absolute() && evaluator.is_file(),
        "请先构建真实 lucia-eval：{}",
        evaluator.display()
    );

    let artifacts = FileArtifactStore::new(evolution_root.join("artifacts"));
    let parent_policy = ContextPolicyV1::default();
    let policy_artifact = ContextPolicyRepository::new(&artifacts)
        .put(&parent_policy)
        .await
        .expect("写入 Parent Context Policy CAS");
    let parent = parent_revision(policy_artifact.digest.clone());
    let genomes = FileGenomeStore::new(evolution_root.join("genomes"));
    genomes.append(&parent).await.expect("登记 Parent Genome");
    FileStableGenomePublisher::new(&evolution_root)
        .publish(TEST_LINEAGE, &parent, 1)
        .await
        .expect("发布初始 Stable Parent");
    let event_stream = artifacts
        .put("application/json", b"[]")
        .await
        .expect("写入 Episode 事件制品");
    let episode = failure_episode(&parent, event_stream);
    FileEpisodeStore::new(evolution_root.join("episodes"))
        .append(&episode)
        .await
        .expect("登记失败 Episode");
    let outbox = FileEvolutionOutbox::new(evolution_root.join("outbox"));
    outbox
        .append(&EvolutionOutboxItem {
            outbox_id: "m6-context-process-outbox".to_string(),
            episode_id: episode.episode_id.clone(),
            outcome: Outcome::TaskFailure,
            disposition: FailureDisposition::EvolutionCandidate,
            issue_id: None,
            issue_status: DiagnosticStatus::Clustered,
            created_at_ms: episode.finished_at_ms,
            consumed: false,
        })
        .await
        .expect("登记待消费 Outbox");
    let unrelated_episode_id = EpisodeId::generate();
    outbox
        .append(&EvolutionOutboxItem {
            outbox_id: "m6-context-unrelated-outbox".to_string(),
            episode_id: unrelated_episode_id.clone(),
            outcome: Outcome::TaskFailure,
            disposition: FailureDisposition::EvolutionCandidate,
            issue_id: None,
            issue_status: DiagnosticStatus::Clustered,
            created_at_ms: episode.finished_at_ms + 1,
            consumed: false,
        })
        .await
        .expect("登记无关待消费 Outbox");

    let fixture_version = DatasetVersionId::generate();
    let request = ContextEvolutionCycleRequestV1 {
        schema_version: agent_evolution::CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION,
        cycle_id: EvolutionCycleId::generate(),
        parent_revision_id: parent.revision_id.clone(),
        parent_genome_digest: parent.digest.clone(),
        lineage: TEST_LINEAGE.to_string(),
        expected_parent_generation: 1,
        evidence_episode_ids: BTreeSet::from([episode.episode_id.clone()]),
        expected_fixture_version: fixture_version.clone(),
        requested_at_ms: 100,
    };

    let proposals = BoundedContextMutator
        .propose(&request, &parent, &policy_artifact.digest, &parent_policy)
        .expect("生成固定三份 Context 提案");
    let builder = ContextCandidateBuilder::new(&genomes, &artifacts);
    let mut candidates = Vec::new();
    for (index, proposal) in proposals.iter().enumerate() {
        candidates.push(
            builder
                .build_at(request.cycle_id.clone(), proposal, 101 + index as u64)
                .await
                .expect("预计算固定 Candidate 身份"),
        );
    }
    let mut observations = BTreeMap::from([(
        parent.revision_id.clone(),
        observation(100, 100, 100, 100, 100, 7_000, 100, 900),
    )]);
    observations.insert(
        candidates[0].candidate_revision_id.clone(),
        observation(100, 100, 100, 100, 100, 6_000, 100, 800),
    );
    observations.insert(
        candidates[1].candidate_revision_id.clone(),
        observation(90, 100, 100, 100, 100, 6_000, 100, 800),
    );
    observations.insert(
        candidates[2].candidate_revision_id.clone(),
        observation(100, 100, 100, 100, 100, 8_000, 100, 800),
    );
    let fixture = ContextObservationFixtureV1 {
        schema_version: CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION,
        fixture_version: fixture_version.clone(),
        observations,
    };
    fixture.validate().expect("Context Fixture 应合法");
    let fixture_bytes = serde_json::to_vec_pretty(&fixture).expect("序列化 Context Fixture");
    fs::write(fixture_root.join("fixture.json"), &fixture_bytes).expect("写入 Context Fixture");
    let fixture_digest = artifact_digest(&fixture_bytes);

    let promoted: ContextEvolutionCycleSnapshotV1 = invoke_evolve(
        evolve_command(
            &evaluator,
            &evolution_root,
            &archive_root,
            &fixture_root,
            &fixture_version,
            &fixture_digest,
        ),
        &["context-cycle"],
        Some(&request),
    );
    assert_eq!(promoted.stage, ContextCycleStage::AwaitingHealth);
    assert_eq!(promoted.proposals.len(), 3);
    assert_eq!(promoted.candidates.len(), 3);
    assert_eq!(promoted.evaluation_receipts.len(), 3);
    assert_eq!(promoted.winner.as_ref(), Some(&candidates[0].candidate_id));
    assert_eq!(
        promoted
            .evaluation_receipts
            .iter()
            .filter(|receipt| receipt.context_report.decision == GateDecision::Pass)
            .count(),
        1
    );
    let receipt_for = |revision_id| {
        promoted
            .evaluation_receipts
            .iter()
            .find(|receipt| &receipt.context_report.candidate_revision_id == revision_id)
            .expect("每份 Candidate 都应有正式 Context Gate 回执")
    };
    assert_eq!(
        receipt_for(&candidates[0].candidate_revision_id)
            .context_report
            .decision,
        GateDecision::Pass
    );
    assert!(receipt_for(&candidates[1].candidate_revision_id)
        .context_report
        .failures
        .contains(&ContextGateFailureV1::FactRecall));
    assert!(receipt_for(&candidates[2].candidate_revision_id)
        .context_report
        .failures
        .contains(&ContextGateFailureV1::TokenReduction));
    assert_eq!(outbox.pending().await.expect("读取 Outbox").len(), 2);

    let parent_plugin_digest = parent
        .genome
        .plugin_environment_snapshot()
        .digest()
        .expect("计算 Parent PluginEnvironmentDigest");
    for candidate in &promoted.candidates {
        let revision = FileGenomeResolver::new(&evolution_root)
            .resolve(&GenomeSelector::Revision(
                candidate.candidate_revision_id.clone(),
            ))
            .await
            .expect("读取 Context Candidate Genome");
        assert_eq!(
            revision
                .genome
                .plugin_environment_snapshot()
                .digest()
                .expect("计算 Candidate PluginEnvironmentDigest"),
            parent_plugin_digest
        );
    }
    let promoted_stable = FileGenomeResolver::new(&evolution_root)
        .resolve(&GenomeSelector::Stable(TEST_LINEAGE.to_string()))
        .await
        .expect("读取 Promotion Stable");
    assert_eq!(
        promoted_stable.revision_id,
        candidates[0].candidate_revision_id
    );
    assert!(promoted_stable.genome.runtime.is_promotable());
    assert_eq!(
        promoted_stable
            .genome
            .plugin_environment_snapshot()
            .digest()
            .expect("计算 Promotion PluginEnvironmentDigest"),
        parent_plugin_digest
    );
    let release = promoted
        .release_receipt
        .as_ref()
        .expect("Promotion 回执应存在");
    assert_eq!(release.to, promoted_stable.revision_id);
    FileRuntimeHealthObservationStore::new(evolution_root.join(RUNTIME_HEALTH_DIRECTORY))
        .expect("创建健康观察 Store")
        .put(&RuntimeHealthObservationV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            release_id: release.release_id.clone(),
            observed_revision_id: release.to.clone(),
            checks_passed: 2,
            checks_total: 3,
            observed_at_ms: 2_100,
        })
        .await
        .expect("写入失败健康观察");

    let rolled_back: ContextEvolutionCycleSnapshotV1 = invoke_evolve(
        evolve_command(
            &evaluator,
            &evolution_root,
            &archive_root,
            &fixture_root,
            &fixture_version,
            &fixture_digest,
        ),
        &[
            "context-health",
            "--cycle-id",
            promoted.request.cycle_id.as_str(),
        ],
        None::<&ContextEvolutionCycleRequestV1>,
    );
    assert_eq!(rolled_back.stage, ContextCycleStage::RolledBack);
    assert_eq!(
        rolled_back
            .health_receipt
            .as_ref()
            .map(|receipt| receipt.verified),
        Some(false)
    );
    assert_eq!(
        rolled_back
            .rollback_receipt
            .as_ref()
            .and_then(|receipt| receipt.rollback_of.as_ref()),
        Some(&release.release_id)
    );
    let pending = outbox.pending().await.expect("复核 Outbox 精确消费");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].episode_id, unrelated_episode_id);
    let stable = FileGenomeResolver::new(&evolution_root)
        .resolve(&GenomeSelector::Stable(TEST_LINEAGE.to_string()))
        .await
        .expect("读取 Rollback Stable");
    assert_eq!(stable.revision_id, parent.revision_id);
    assert_eq!(
        stable
            .genome
            .plugin_environment_snapshot()
            .digest()
            .expect("计算 Rollback PluginEnvironmentDigest"),
        parent_plugin_digest
    );

    let archive = TrustedEvaluationArchive::new(&archive_root);
    for receipt in &rolled_back.evaluation_receipts {
        receipt
            .validate(M6_CONTEXT_GATE_VERSION)
            .expect("Context Gate 回执应可复核");
        archive
            .get_verified(&receipt.report_id)
            .await
            .expect("Rollback 后 EvaluationReport、Audit 与 Seal 必须保留");
    }
    let history: Vec<ContextEvolutionCycleSnapshotV1> = invoke_evolve(
        evolve_command(
            &evaluator,
            &evolution_root,
            &archive_root,
            &fixture_root,
            &fixture_version,
            &fixture_digest,
        ),
        &[
            "context-inspect",
            "--cycle-id",
            promoted.request.cycle_id.as_str(),
        ],
        None::<&ContextEvolutionCycleRequestV1>,
    );
    assert!(history
        .iter()
        .any(|snapshot| snapshot.stage == ContextCycleStage::AwaitingHealth));
    assert_eq!(history.last(), Some(&rolled_back));
}
