//! M8 插件源码 Gate。
//!
//! 本模块由受信评测控制面拥有，只从完整协议输入重新推导结论。它不能产生 Stable 或自动
//! 提升决定；全证据通过最多进入 Canary，任何硬失败都进入人工审批。

use agent_evolution_protocol::{
    InvalidPluginEvolution, PluginEvaluationGateInput, PluginEvaluationReport,
    PluginSourceGateDecision, PLUGIN_EVALUATION_REPORT_SCHEMA_VERSION,
};

/// 从完整构建、Host 审计与独立评测证据产生插件源码 Gate 报告。
///
/// 决策类型只包含 `RequireApproval` 和 `Canary`。调用方不能传入失败集合或目标发布阶段，
/// Stable 发布必须由后续受信 Release Controller 独立处理。
///
/// # Errors
///
/// 输入证据结构无效、身份或摘要错绑，或最终报告无法通过协议复核时返回
/// [`PluginGateError`]。
pub fn evaluate_plugin_source(
    input: &PluginEvaluationGateInput,
) -> Result<PluginEvaluationReport, PluginGateError> {
    input.validate()?;
    let failures = input.canonical_failures()?;
    let decision = if failures.is_empty() {
        PluginSourceGateDecision::Canary
    } else {
        PluginSourceGateDecision::RequireApproval
    };
    let report = PluginEvaluationReport {
        schema_version: PLUGIN_EVALUATION_REPORT_SCHEMA_VERSION,
        report_id: input.report_id.clone(),
        plugin_id: input.proposal.plugin_id.clone(),
        mutation_id: input.proposal.mutation_id.clone(),
        candidate_id: input.proposal.candidate_id.clone(),
        gate_input_digest: input.digest()?,
        proposal_digest: input.proposal.digest()?,
        build_attestation_digest: input.build_attestation.digest()?,
        component_digest: input.build_attestation.component_digest.clone(),
        bundle_digest: input.bundle_digest.clone(),
        host_audit_digest: input.host_audit.digest()?,
        safety_evaluation_digest: input.safety_evaluation.digest()?,
        agent_evaluation_digest: input.agent_evaluation.digest()?,
        decision,
        failures,
        generated_at_ms: input.evaluated_at_ms,
    };
    report.validate_for_input(input)?;
    Ok(report)
}

/// 插件源码 Gate 构建失败。
#[derive(Debug, thiserror::Error)]
pub enum PluginGateError {
    /// 协议输入或派生报告违反 M8 结构、身份、摘要、能力或时间不变量。
    #[error("插件源码 Gate 协议无效：{0}")]
    InvalidProtocol(#[from] InvalidPluginEvolution),
}
