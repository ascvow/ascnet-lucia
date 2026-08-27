//! M6 八项 Context 指标与固定 Gate 的独立集成测试。

use agent_evaluation::{
    calculate_context_metrics, evaluate_context_policy_candidate, M6_CONTEXT_GATE_VERSION,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ContextEvaluationObservationV1, ContextGateFailureV1,
    ContextPolicyV1, GateDecision, GenomeMetadata, GenomeRevision, ModelGenome, MutationSurface,
    PolicyRef, PromptGenome, RecallObservationV1, RuntimeIdentity, ToolProfileGenome,
    CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION, GENOME_SCHEMA_VERSION, NATIVE_CONTEXT_POLICY_ID,
};
use agent_tool::{ExecutionPolicy, ToolAccess};
use std::collections::BTreeSet;

/// 构造确定性的 Artifact 摘要。
fn digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造仅在 Context Policy 摘要上不同的 Parent/Candidate Revision。
fn revisions() -> (GenomeRevision, GenomeRevision) {
    let parent = GenomeRevision::create(sample_genome(digest('a')), GenomeMetadata::default())
        .expect("Parent 应合法");
    let mut candidate_genome = parent.genome.clone();
    candidate_genome
        .context_policy
        .as_mut()
        .expect("应有 Context Policy")
        .config_digest = digest('b');
    let candidate = GenomeRevision::create(candidate_genome, GenomeMetadata::default())
        .expect("Candidate 应合法");
    (parent, candidate)
}

/// 构造有效的 Context Policy Genome。
fn sample_genome(policy_digest: ArtifactDigest) -> AgentGenome {
    AgentGenome {
        schema_version: GENOME_SCHEMA_VERSION,
        runtime: RuntimeIdentity {
            package_version: "0.1.0".into(),
            git_commit: "m6".into(),
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
            max_tokens: Some(4_096),
            temperature: None,
            stream: false,
            provider_options_digest: None,
        },
        prompt: PromptGenome::default(),
        plugins: Vec::new(),
        capability_owners: Default::default(),
        tools: ToolProfileGenome {
            native_tools: BTreeSet::new(),
            access: ToolAccess::All,
        },
        context_policy: Some(PolicyRef {
            id: NATIVE_CONTEXT_POLICY_ID.into(),
            config_digest: policy_digest,
        }),
        planning_policy: None,
        skills: Vec::new(),
        execution: ExecutionPolicy::serve(),
    }
}

/// 构造全部召回、下游成功且资源量有界的可信观察。
fn observation(tokens_after: u64, cost: u64, latency_ms: u64) -> ContextEvaluationObservationV1 {
    ContextEvaluationObservationV1 {
        schema_version: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
        facts: RecallObservationV1 {
            expected: 20,
            recalled: 20,
        },
        constraints: RecallObservationV1 {
            expected: 5,
            recalled: 5,
        },
        tool_states: RecallObservationV1 {
            expected: 10,
            recalled: 10,
        },
        plan_states: RecallObservationV1 {
            expected: 4,
            recalled: 4,
        },
        downstream_tasks: RecallObservationV1 {
            expected: 20,
            recalled: 20,
        },
        tokens_before: 100_000,
        tokens_after,
        cost_microunits: cost,
        latency_ms,
    }
}

/// 八项指标必须由整数公式确定性计算。
#[test]
fn calculates_all_eight_metrics_deterministically() {
    let mut input = observation(60_000, 321, 654);
    input.facts.recalled = 19;
    input.tool_states.recalled = 9;

    let metrics = calculate_context_metrics(&input).expect("观察应可计算");

    assert_eq!(metrics.fact_recall_bps, 9_500);
    assert_eq!(metrics.constraint_recall_bps, 10_000);
    assert_eq!(metrics.tool_state_recall_bps, 9_000);
    assert_eq!(metrics.plan_state_recall_bps, 10_000);
    assert_eq!(metrics.downstream_task_success_bps, 10_000);
    assert_eq!(metrics.token_reduction_bps, 4_000);
    assert_eq!(metrics.cost_microunits, 321);
    assert_eq!(metrics.latency_ms, 654);
}

/// 唯一 Context Policy Diff、无质量回退和受控资源量必须通过固定 M6 Gate。
#[test]
fn passes_fixed_m6_gate() {
    let (parent, candidate) = revisions();
    let report = evaluate_context_policy_candidate(
        &parent,
        &candidate,
        &observation(70_000, 100, 1_000),
        &observation(60_000, 110, 2_000),
    )
    .expect("合法对照评测应产生报告");

    assert_eq!(report.gate_version, M6_CONTEXT_GATE_VERSION);
    assert_eq!(report.decision, GateDecision::Pass);
    assert!(report.failures.is_empty());
}

/// 约束或 Plan 丢失、工具召回低于门槛、token 压缩回退与成本超限必须同时可审计。
#[test]
fn rejects_all_quality_and_resource_failures() {
    let (parent, candidate) = revisions();
    let mut candidate_observation = observation(80_000, 112, 120_001);
    candidate_observation.constraints.recalled = 4;
    candidate_observation.tool_states.recalled = 9;
    candidate_observation.plan_states.recalled = 3;
    candidate_observation.downstream_tasks.recalled = 18;

    let report = evaluate_context_policy_candidate(
        &parent,
        &candidate,
        &observation(70_000, 100, 1_000),
        &candidate_observation,
    )
    .expect("未达 Gate 仍应产生结构化 Reject 报告");

    assert_eq!(report.decision, GateDecision::Reject);
    assert!(report
        .failures
        .contains(&ContextGateFailureV1::ConstraintRecall));
    assert!(report
        .failures
        .contains(&ContextGateFailureV1::ToolStateRecall));
    assert!(report
        .failures
        .contains(&ContextGateFailureV1::PlanStateRecall));
    assert!(report
        .failures
        .contains(&ContextGateFailureV1::DownstreamTaskSuccess));
    assert!(report
        .failures
        .contains(&ContextGateFailureV1::TokenReduction));
    assert!(report.failures.contains(&ContextGateFailureV1::Cost));
    assert!(report.failures.contains(&ContextGateFailureV1::Latency));
}

/// 即使八项指标通过，任何非 Context Policy 行为变化也必须被固定 Gate 拒绝。
#[test]
fn rejects_non_context_genome_diff() {
    let (parent, mut candidate) = revisions();
    candidate.genome.model.model = "candidate-model".into();
    candidate = GenomeRevision::create(candidate.genome, GenomeMetadata::default())
        .expect("模型与 Context 双表面变化仍是合法 Revision");

    let report = evaluate_context_policy_candidate(
        &parent,
        &candidate,
        &observation(70_000, 100, 1_000),
        &observation(60_000, 100, 1_000),
    )
    .expect("越界 Diff 应得到 Reject 报告");

    assert_eq!(report.decision, GateDecision::Reject);
    assert_eq!(
        report.failures,
        BTreeSet::from([ContextGateFailureV1::GenomeDiff])
    );
    let changed = agent_evolution::diff_genomes(&parent, &candidate)
        .expect("应能检查测试 Diff")
        .changed_surfaces;
    assert_eq!(
        changed,
        BTreeSet::from([MutationSurface::Model, MutationSurface::ContextPolicy])
    );
}

/// 测试夹具中的默认策略必须仍可通过协议校验，防止评测与策略版本漂移。
#[test]
fn evaluation_fixture_uses_valid_context_policy_version() {
    ContextPolicyV1::default()
        .validate()
        .expect("M6 默认策略应与固定 Gate 同时保持可用");
}
