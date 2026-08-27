//! Host 协议中立审计证据到 M8 插件进化协议的受信适配层。
//!
//! Host 只负责产生 manifest、真实 Component 类型图、owner 和服务调用观察；本模块由
//! Evaluation 平面拥有，负责加入 Mutation/Candidate/制品身份并构造六项版本化 Gate 证据。

use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, CapabilityProfile, ComponentInterfaceSnapshot,
    InvalidPluginEvolution, MutationId, PluginAuditCheck, PluginCapabilitySet,
    PluginHostAuditEvidence, COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
    PLUGIN_AUDIT_CHECK_SCHEMA_VERSION, PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
};
use agent_plugin_host::audit::{
    HostServiceCallResult, PluginAuditEvidence as HostPluginAuditEvidence,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// 真实 Host smoke 或运行时审计器产生的一项受信结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedHostCheckOutcome {
    /// 完整外部报告的 CAS 摘要。
    pub report_digest: ArtifactDigest,
    /// 实际检查数，必须非零。
    pub check_count: u32,
    /// 失败检查数，不得超过检查数。
    pub failure_count: u32,
}

/// 把 Host 中立证据绑定到一次 M8 Candidate 所需的受信上下文。
#[derive(Debug, Clone)]
pub struct PluginHostAuditBinding {
    /// 受信控制面指定的插件 ID。
    pub plugin_id: String,
    /// 当前 Mutation ID。
    pub mutation_id: MutationId,
    /// 当前 Candidate ID。
    pub candidate_id: CandidateId,
    /// 真实 Component 内容摘要。
    pub component_digest: ArtifactDigest,
    /// Bundle 内 manifest 内容摘要。
    pub manifest_digest: ArtifactDigest,
    /// 完整 Bundle 内容摘要。
    pub bundle_digest: ArtifactDigest,
    /// 构建平面期望的真实接口快照。
    pub expected_interface: ComponentInterfaceSnapshot,
    /// 构建平面期望的真实能力 Profile。
    pub expected_capabilities: CapabilityProfile,
    /// 本适配器及规则集的不可变摘要。
    pub verifier_revision: ArtifactDigest,
    /// 真实 WASM 装载与路由 smoke 结果。
    pub host_smoke: TrustedHostCheckOutcome,
    /// 资源上限、生命周期与副作用审计结果。
    pub runtime_audit: TrustedHostCheckOutcome,
    /// 全部检查完成的 Unix 毫秒时间。
    pub completed_at_ms: u64,
}

/// 从 Host 真实 Component 类型图生成协议接口快照。
///
/// `scanner_revision` 必须标识 Host 扫描器二进制与路径规则；返回值会进行协议边界校验。
///
/// # Errors
///
/// 插件身份、Component 摘要、world 或接口路径不符合 M8 协议时返回错误。
pub fn protocol_component_interface(
    plugin_id: impl Into<String>,
    component_digest: ArtifactDigest,
    scanner_revision: ArtifactDigest,
    evidence: &HostPluginAuditEvidence,
) -> Result<ComponentInterfaceSnapshot, PluginHostAuditBindingError> {
    let mut imports = evidence
        .component
        .imports
        .iter()
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    let mut exports = evidence
        .component
        .exports
        .iter()
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    imports.sort();
    imports.dedup();
    exports.sort();
    exports.dedup();
    let snapshot = ComponentInterfaceSnapshot {
        schema_version: COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
        plugin_id: plugin_id.into(),
        component_digest,
        world: evidence.component.world.clone(),
        imports,
        exports,
        scanner_revision,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

/// 把 Host 中立证据转换为完整 `PluginHostAuditEvidence`。
///
/// manifest、import、interface 和 owner 结论从真实 Host 快照重新推导；调用方只需提供 Host
/// 无法表达的真实装载 smoke、资源/生命周期审计结果及 Candidate 身份。实际 Host 服务调用
/// 失败会叠加到 runtime 失败计数，不能被外部结果隐藏。
///
/// # Errors
///
/// Host 身份错绑、期望接口/能力无效、计数溢出或生成的协议证据不自洽时返回错误。
pub fn bind_plugin_host_audit(
    evidence: &HostPluginAuditEvidence,
    binding: PluginHostAuditBinding,
) -> Result<PluginHostAuditEvidence, PluginHostAuditBindingError> {
    binding.expected_interface.validate()?;
    binding.expected_capabilities.validate()?;
    validate_outcome(&binding.host_smoke)?;
    validate_outcome(&binding.runtime_audit)?;
    if evidence.manifest.plugin_id != binding.plugin_id
        || binding.expected_interface.plugin_id != binding.plugin_id
        || binding.expected_interface.component_digest != binding.component_digest
    {
        return Err(PluginHostAuditBindingError::IdentityMismatch);
    }

    let derived_interface = protocol_component_interface(
        binding.plugin_id.clone(),
        binding.component_digest.clone(),
        binding.expected_interface.scanner_revision.clone(),
        evidence,
    )?;
    let derived_capabilities = capability_profile(evidence)?;
    let evidence_digest = canonical_digest("ascnet.lucia.host-audit-evidence.v1", evidence)?;

    let import_failures = evidence
        .capability_import_checks
        .iter()
        .filter(|check| !check.satisfied)
        .count();
    let interface_failures =
        usize::from(derived_interface.world != binding.expected_interface.world)
            + usize::from(derived_interface.imports != binding.expected_interface.imports)
            + usize::from(derived_interface.exports != binding.expected_interface.exports)
            + usize::from(derived_capabilities != binding.expected_capabilities);

    let owners = evidence
        .resolved_capability_owners
        .iter()
        .map(|owner| (owner.capability_id.as_str(), &owner.owner_plugin_ids))
        .collect::<BTreeMap<_, _>>();
    let missing_owner_count = evidence
        .manifest
        .provided
        .iter()
        .filter(|provided| {
            !owners
                .get(provided.capability_id.as_str())
                .is_some_and(|plugin_ids| plugin_ids.iter().any(|id| id == &binding.plugin_id))
        })
        .count();
    let forged_caller_count = evidence
        .observed_host_service_calls
        .iter()
        .filter(|call| call.caller_id != binding.plugin_id)
        .count();
    let owner_failures = missing_owner_count
        .checked_add(forged_caller_count)
        .ok_or(PluginHostAuditBindingError::CountOverflow)?;

    let runtime_call_failures = evidence
        .observed_host_service_calls
        .iter()
        .filter(|call| matches!(call.result, HostServiceCallResult::Failed { .. }))
        .count();
    let runtime_check_count = checked_u32(evidence.observed_host_service_calls.len())?
        .checked_add(binding.runtime_audit.check_count)
        .ok_or(PluginHostAuditBindingError::CountOverflow)?;
    let runtime_failure_count = checked_u32(runtime_call_failures)?
        .checked_add(binding.runtime_audit.failure_count)
        .ok_or(PluginHostAuditBindingError::CountOverflow)?;
    let runtime_report_digest = canonical_digest(
        "ascnet.lucia.host-runtime-audit.v1",
        &(
            &binding.runtime_audit.report_digest,
            &evidence.observed_host_service_calls,
        ),
    )?;

    let result = PluginHostAuditEvidence {
        schema_version: PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
        plugin_id: binding.plugin_id,
        mutation_id: binding.mutation_id,
        candidate_id: binding.candidate_id,
        component_digest: binding.component_digest,
        manifest_digest: binding.manifest_digest,
        interface_digest: binding.expected_interface.digest()?,
        capability_profile_digest: binding.expected_capabilities.digest()?,
        bundle_digest: binding.bundle_digest,
        host_smoke: external_check(
            &binding.host_smoke,
            binding.verifier_revision.clone(),
            binding.completed_at_ms,
        ),
        manifest_audit: derived_check(
            evidence_digest.clone(),
            binding.verifier_revision.clone(),
            binding.completed_at_ms,
            1,
            0,
        )?,
        import_audit: derived_check(
            evidence_digest.clone(),
            binding.verifier_revision.clone(),
            binding.completed_at_ms,
            evidence.capability_import_checks.len().max(1),
            import_failures,
        )?,
        interface_audit: derived_check(
            evidence_digest.clone(),
            binding.verifier_revision.clone(),
            binding.completed_at_ms,
            4,
            interface_failures,
        )?,
        owner_audit: derived_check(
            evidence_digest,
            binding.verifier_revision.clone(),
            binding.completed_at_ms,
            (evidence.manifest.provided.len() + evidence.observed_host_service_calls.len()).max(1),
            owner_failures,
        )?,
        runtime_audit: PluginAuditCheck {
            schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
            report_digest: runtime_report_digest,
            verifier_revision: binding.verifier_revision,
            passed: runtime_failure_count == 0,
            check_count: runtime_check_count,
            failure_count: runtime_failure_count,
            completed_at_ms: binding.completed_at_ms,
        },
    };
    result.validate()?;
    Ok(result)
}

fn capability_profile(
    evidence: &HostPluginAuditEvidence,
) -> Result<CapabilityProfile, PluginHostAuditBindingError> {
    let requested = PluginCapabilitySet::new(
        evidence
            .manifest
            .requested
            .iter()
            .map(|capability| capability.capability_id.clone())
            .collect(),
    )?;
    let provided = PluginCapabilitySet::new(
        evidence
            .manifest
            .provided
            .iter()
            .map(|capability| capability.capability_id.clone())
            .collect(),
    )?;
    Ok(CapabilityProfile::new(requested, provided)?)
}

fn external_check(
    outcome: &TrustedHostCheckOutcome,
    verifier_revision: ArtifactDigest,
    completed_at_ms: u64,
) -> PluginAuditCheck {
    PluginAuditCheck {
        schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
        report_digest: outcome.report_digest.clone(),
        verifier_revision,
        passed: outcome.failure_count == 0,
        check_count: outcome.check_count,
        failure_count: outcome.failure_count,
        completed_at_ms,
    }
}

fn derived_check(
    report_digest: ArtifactDigest,
    verifier_revision: ArtifactDigest,
    completed_at_ms: u64,
    check_count: usize,
    failure_count: usize,
) -> Result<PluginAuditCheck, PluginHostAuditBindingError> {
    Ok(PluginAuditCheck {
        schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
        report_digest,
        verifier_revision,
        passed: failure_count == 0,
        check_count: checked_u32(check_count)?,
        failure_count: checked_u32(failure_count)?,
        completed_at_ms,
    })
}

fn validate_outcome(outcome: &TrustedHostCheckOutcome) -> Result<(), PluginHostAuditBindingError> {
    if outcome.check_count == 0 || outcome.failure_count > outcome.check_count {
        return Err(PluginHostAuditBindingError::InvalidTrustedOutcome);
    }
    Ok(())
}

fn checked_u32(value: usize) -> Result<u32, PluginHostAuditBindingError> {
    u32::try_from(value).map_err(|_| PluginHostAuditBindingError::CountOverflow)
}

fn canonical_digest<T: Serialize>(
    domain: &'static str,
    value: &T,
) -> Result<ArtifactDigest, PluginHostAuditBindingError> {
    let bytes = serde_json::to_vec(&(domain, value))?;
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| PluginHostAuditBindingError::InvalidDigest(error.to_string()))
}

/// Host 证据适配到 M8 协议时的失败。
#[derive(Debug, thiserror::Error)]
pub enum PluginHostAuditBindingError {
    /// 协议对象不合法。
    #[error("Host 审计协议对象无效：{0}")]
    Protocol(#[from] InvalidPluginEvolution),
    /// Host manifest、Component 或受信绑定身份不一致。
    #[error("Host 审计身份与 Candidate 绑定不一致")]
    IdentityMismatch,
    /// 外部受信检查计数无效。
    #[error("Host 外部受信检查计数无效")]
    InvalidTrustedOutcome,
    /// Host 证据项目数不能安全转换为协议计数。
    #[error("Host 审计检查计数溢出")]
    CountOverflow,
    /// 规范摘要序列化失败。
    #[error("Host 审计证据无法规范序列化：{0}")]
    Json(#[from] serde_json::Error),
    /// SHA-256 摘要无法转换为协议强类型 ID。
    #[error("Host 审计摘要无效：{0}")]
    InvalidDigest(String),
}
