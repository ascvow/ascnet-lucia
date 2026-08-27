//! M6 Context Policy 的确定性指标计算与固定 Commit Gate。

use agent_evolution::{diff_genomes, GenomeDiffError};
use agent_evolution_protocol::{
    ContextEvaluationMetricsV1, ContextEvaluationObservationV1, ContextGateFailureV1,
    ContextPolicyEvaluationReportV1, GateDecision, GenomeRevision, InvalidContextEvaluation,
    MutationSurface, RecallObservationV1, CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION,
};
use std::collections::BTreeSet;

/// 固定 M6 Context Policy Gate 的版本。
///
/// 任一阈值或比较语义变化都必须更换该版本，不能继续接受旧报告。
pub const M6_CONTEXT_GATE_VERSION: &str = "context-policy-m6-v1";
/// Candidate 事实召回率最低为 95%。
pub const M6_MIN_FACT_RECALL_BPS: u16 = 9_500;
/// Candidate 用户约束召回率必须为 100%。
pub const M6_MIN_CONSTRAINT_RECALL_BPS: u16 = 10_000;
/// Candidate 工具状态召回率最低为 95%。
pub const M6_MIN_TOOL_STATE_RECALL_BPS: u16 = 9_500;
/// Candidate Plan 状态召回率必须为 100%。
pub const M6_MIN_PLAN_STATE_RECALL_BPS: u16 = 10_000;
/// Candidate 下游任务成功率最低为 95%。
pub const M6_MIN_DOWNSTREAM_TASK_SUCCESS_BPS: u16 = 9_500;
/// Candidate token 缩减率最低为 25%。
pub const M6_MIN_TOKEN_REDUCTION_BPS: u16 = 2_500;
/// Candidate 成本最多为 Parent 的 110%，单位为万分比。
pub const M6_MAX_COST_RATIO_BPS: u16 = 11_000;
/// Candidate 单次端到端延迟的固定绝对上限，单位毫秒。
///
/// 不使用严格 Parent 墙钟差值，避免共享 CI 调度抖动形成随机硬失败。
pub const M6_MAX_LATENCY_MS: u64 = 120_000;

/// 固定 M6 Gate 的完整阈值快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextGatePolicyV1 {
    /// Gate 语义版本。
    pub version: &'static str,
    /// 最低事实召回率，单位为万分比。
    pub min_fact_recall_bps: u16,
    /// 最低用户约束召回率，单位为万分比。
    pub min_constraint_recall_bps: u16,
    /// 最低工具状态召回率，单位为万分比。
    pub min_tool_state_recall_bps: u16,
    /// 最低 Plan 状态召回率，单位为万分比。
    pub min_plan_state_recall_bps: u16,
    /// 最低下游任务成功率，单位为万分比。
    pub min_downstream_task_success_bps: u16,
    /// 最低 token 缩减率，单位为万分比。
    pub min_token_reduction_bps: u16,
    /// 相对 Parent 的最大成本比例，单位为万分比。
    pub max_cost_ratio_bps: u16,
    /// Candidate 延迟绝对上限，单位毫秒。
    pub max_latency_ms: u64,
}

/// 当前唯一受支持的固定 M6 Gate 策略。
pub const M6_CONTEXT_GATE_POLICY: ContextGatePolicyV1 = ContextGatePolicyV1 {
    version: M6_CONTEXT_GATE_VERSION,
    min_fact_recall_bps: M6_MIN_FACT_RECALL_BPS,
    min_constraint_recall_bps: M6_MIN_CONSTRAINT_RECALL_BPS,
    min_tool_state_recall_bps: M6_MIN_TOOL_STATE_RECALL_BPS,
    min_plan_state_recall_bps: M6_MIN_PLAN_STATE_RECALL_BPS,
    min_downstream_task_success_bps: M6_MIN_DOWNSTREAM_TASK_SUCCESS_BPS,
    min_token_reduction_bps: M6_MIN_TOKEN_REDUCTION_BPS,
    max_cost_ratio_bps: M6_MAX_COST_RATIO_BPS,
    max_latency_ms: M6_MAX_LATENCY_MS,
};

/// 从可信原始观察值计算八项 Context Policy 指标。
///
/// 所有比例都用整数万分比和 `u128` 中间值计算，避免浮点 NaN、平台舍入差异或乘法溢出。
///
/// # Errors
///
/// 原始观察的 schema、召回计数或 token 关系无效时返回 [`ContextEvaluationError`]。
pub fn calculate_context_metrics(
    observation: &ContextEvaluationObservationV1,
) -> Result<ContextEvaluationMetricsV1, ContextEvaluationError> {
    observation.validate()?;
    let metrics = ContextEvaluationMetricsV1 {
        fact_recall_bps: ratio_bps(observation.facts),
        constraint_recall_bps: ratio_bps(observation.constraints),
        tool_state_recall_bps: ratio_bps(observation.tool_states),
        plan_state_recall_bps: ratio_bps(observation.plan_states),
        downstream_task_success_bps: ratio_bps(observation.downstream_tasks),
        token_reduction_bps: reduction_bps(observation.tokens_before, observation.tokens_after),
        cost_microunits: observation.cost_microunits,
        latency_ms: observation.latency_ms,
    };
    metrics.validate()?;
    Ok(metrics)
}

/// 对 Parent/Candidate 的真实 Revision 与可信观察运行固定 M6 Gate。
///
/// 六项质量/压缩比例必须同时达到绝对门槛且不低于 Parent；成本允许最多 10% 相对增长；
/// 延迟只使用固定绝对上限。真实 Genome Diff 不是精确的 `{ContextPolicy}` 时直接拒绝。
///
/// # Errors
///
/// Revision 无效、原始观察无效，或最终报告无法通过协议一致性校验时返回
/// [`ContextEvaluationError`]。合法但未达 Gate 的 Candidate 返回 `Ok(Reject)` 报告。
pub fn evaluate_context_policy_candidate(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
    parent_observation: &ContextEvaluationObservationV1,
    candidate_observation: &ContextEvaluationObservationV1,
) -> Result<ContextPolicyEvaluationReportV1, ContextEvaluationError> {
    let diff = diff_genomes(parent, candidate)?;
    let parent_metrics = calculate_context_metrics(parent_observation)?;
    let candidate_metrics = calculate_context_metrics(candidate_observation)?;
    let mut failures = BTreeSet::new();
    if diff.changed_surfaces != BTreeSet::from([MutationSurface::ContextPolicy]) {
        failures.insert(ContextGateFailureV1::GenomeDiff);
    }

    check_quality_metric(
        candidate_metrics.fact_recall_bps,
        parent_metrics.fact_recall_bps,
        M6_CONTEXT_GATE_POLICY.min_fact_recall_bps,
        ContextGateFailureV1::FactRecall,
        &mut failures,
    );
    check_quality_metric(
        candidate_metrics.constraint_recall_bps,
        parent_metrics.constraint_recall_bps,
        M6_CONTEXT_GATE_POLICY.min_constraint_recall_bps,
        ContextGateFailureV1::ConstraintRecall,
        &mut failures,
    );
    check_quality_metric(
        candidate_metrics.tool_state_recall_bps,
        parent_metrics.tool_state_recall_bps,
        M6_CONTEXT_GATE_POLICY.min_tool_state_recall_bps,
        ContextGateFailureV1::ToolStateRecall,
        &mut failures,
    );
    check_quality_metric(
        candidate_metrics.plan_state_recall_bps,
        parent_metrics.plan_state_recall_bps,
        M6_CONTEXT_GATE_POLICY.min_plan_state_recall_bps,
        ContextGateFailureV1::PlanStateRecall,
        &mut failures,
    );
    check_quality_metric(
        candidate_metrics.downstream_task_success_bps,
        parent_metrics.downstream_task_success_bps,
        M6_CONTEXT_GATE_POLICY.min_downstream_task_success_bps,
        ContextGateFailureV1::DownstreamTaskSuccess,
        &mut failures,
    );
    check_quality_metric(
        candidate_metrics.token_reduction_bps,
        parent_metrics.token_reduction_bps,
        M6_CONTEXT_GATE_POLICY.min_token_reduction_bps,
        ContextGateFailureV1::TokenReduction,
        &mut failures,
    );

    if exceeds_cost_ratio(
        parent_metrics.cost_microunits,
        candidate_metrics.cost_microunits,
        M6_CONTEXT_GATE_POLICY.max_cost_ratio_bps,
    ) {
        failures.insert(ContextGateFailureV1::Cost);
    }
    if candidate_metrics.latency_ms > M6_CONTEXT_GATE_POLICY.max_latency_ms {
        failures.insert(ContextGateFailureV1::Latency);
    }

    let decision = if failures.is_empty() {
        GateDecision::Pass
    } else {
        GateDecision::Reject
    };
    let report = ContextPolicyEvaluationReportV1 {
        schema_version: CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION,
        gate_version: M6_CONTEXT_GATE_POLICY.version.to_string(),
        parent_revision_id: parent.revision_id.clone(),
        candidate_revision_id: candidate.revision_id.clone(),
        parent_metrics,
        candidate_metrics,
        decision,
        failures,
    };
    report.validate(M6_CONTEXT_GATE_POLICY.version)?;
    Ok(report)
}

/// 把单项命中计数转换为向下取整的万分比。
fn ratio_bps(observation: RecallObservationV1) -> u16 {
    ((u128::from(observation.recalled) * 10_000) / u128::from(observation.expected)) as u16
}

/// 计算压缩前后 token 的缩减万分比。
fn reduction_bps(before: u64, after: u64) -> u16 {
    ((u128::from(before - after) * 10_000) / u128::from(before)) as u16
}

/// 检查一项比例指标的绝对门槛和 Parent 非回退条件。
fn check_quality_metric(
    candidate: u16,
    parent: u16,
    minimum: u16,
    failure: ContextGateFailureV1,
    failures: &mut BTreeSet<ContextGateFailureV1>,
) {
    if candidate < minimum || candidate < parent {
        failures.insert(failure);
    }
}

/// 判断 Candidate 成本是否超过 Parent 的固定相对上限。
///
/// Parent 成本为零时只允许 Candidate 仍为零，避免除零或用固定小额绕过相对门槛。
fn exceeds_cost_ratio(parent: u64, candidate: u64, max_ratio_bps: u16) -> bool {
    if parent == 0 {
        return candidate != 0;
    }
    u128::from(candidate) * 10_000 > u128::from(parent) * u128::from(max_ratio_bps)
}

/// Context Policy 指标计算或固定 Gate 错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextEvaluationError {
    /// 原始观察、指标或报告违反 Context 评测协议。
    #[error("Context Policy 评测协议无效：{0}")]
    InvalidProtocol(#[from] InvalidContextEvaluation),
    /// Parent/Candidate Revision 无效，无法产生可信完整 Diff。
    #[error("Context Policy Genome Diff 无效：{0}")]
    GenomeDiff(#[from] GenomeDiffError),
}
