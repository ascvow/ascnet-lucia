//! Host 协议中立审计证据到 M8 插件进化协议的受信适配层。
//!
//! Host 只负责产生 manifest、真实 Component 类型图、owner 和服务调用观察；本模块由
//! Evaluation 平面拥有，负责加入 Mutation/Candidate/制品身份并构造六项版本化 Gate 证据。

use agent_evolution::{
    ComponentInspectionRequest, ComponentInspector, ComponentInspectorFailure,
    TrustedComponentInspection,
};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, CapabilityProfile, ComponentInterfaceSnapshot,
    InvalidPluginEvolution, MutationId, PluginAuditCheck, PluginCapabilitySet,
    PluginHostAuditEvidence, COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
    PLUGIN_AUDIT_CHECK_SCHEMA_VERSION, PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
};
use agent_plugin_host::audit::{
    scan_component_interfaces, snapshot_manifest_capabilities,
    ComponentInterfaceSnapshot as HostComponentInterfaceSnapshot, HostServiceCallResult,
    PluginAuditEvidence as HostPluginAuditEvidence,
};
use agent_plugin_host::manifest::PluginManifest;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};

/// 使用真实 manifest 与 Wasmtime Component 类型扫描实现构建 Worker 的生产 Inspector。
#[derive(Debug, Clone)]
pub struct ManifestComponentInspector {
    manifest_path: PathBuf,
    scanner_revision: ArtifactDigest,
}

impl ManifestComponentInspector {
    /// 固定受信 manifest 路径和扫描器修订摘要。
    ///
    /// 路径会在每次扫描时重新规范化并拒绝符号链接，避免构造后被替换；扫描器修订必须由
    /// 受信控制面绑定当前二进制与规则配置。
    pub fn new(manifest_path: impl Into<PathBuf>, scanner_revision: ArtifactDigest) -> Self {
        Self {
            manifest_path: manifest_path.into(),
            scanner_revision,
        }
    }
}

impl ComponentInspector for ManifestComponentInspector {
    /// 复核 Component 字节身份，扫描真实类型图，并从已校验 manifest 重建能力 Profile。
    ///
    /// # Errors
    ///
    /// manifest/Component 路径不安全、身份或摘要不匹配、WASM 不是合法 Component，或扫描
    /// 结果无法转换为 M8 协议时返回脱敏失败。
    fn inspect(
        &mut self,
        request: &ComponentInspectionRequest,
    ) -> Result<TrustedComponentInspection, ComponentInspectorFailure> {
        inspect_manifest_component(self, request)
            .map_err(|error| ComponentInspectorFailure::new(error.to_string()))
    }
}

/// 执行真实生产 Inspector 的可保留错误链主路径。
fn inspect_manifest_component(
    inspector: &ManifestComponentInspector,
    request: &ComponentInspectionRequest,
) -> anyhow::Result<TrustedComponentInspection> {
    let manifest_path = canonical_regular_file(&inspector.manifest_path, "插件 manifest")?;
    let component_path = canonical_regular_file(&request.component_path, "插件 Component")?;
    let component_bytes = fs::read(&component_path)?;
    if component_bytes.len() as u64 != request.component_size_bytes
        || digest_bytes(&component_bytes)? != request.component_digest
    {
        anyhow::bail!("插件 Component 字节身份与构建请求不一致");
    }

    let manifest = PluginManifest::load(&manifest_path)?;
    if manifest.plugin.id != request.plugin_id {
        anyhow::bail!("插件 manifest 身份与构建请求不一致");
    }
    let declared_component = safe_manifest_component_name(&manifest.plugin.wasm)?;
    if component_path.file_name() != Some(declared_component.as_os_str()) {
        anyhow::bail!("插件 manifest Component 文件名与真实构建产物不一致");
    }

    let host_interface = scan_component_interfaces(&component_path)?;
    let interface = protocol_component_interface_from_snapshot(
        request.plugin_id.clone(),
        request.component_digest.clone(),
        inspector.scanner_revision.clone(),
        &host_interface,
    )?;
    let capabilities = protocol_capability_profile(&manifest)?;
    Ok(TrustedComponentInspection {
        interface,
        capabilities,
    })
}

/// 从已校验 manifest 重建 M8 能力 Profile，不接受 Candidate 单独提交的能力结论。
///
/// # Errors
///
/// manifest 声明或协议能力 ID 不合法时返回错误。
pub fn protocol_capability_profile(
    manifest: &PluginManifest,
) -> Result<CapabilityProfile, PluginHostAuditBindingError> {
    let snapshot = snapshot_manifest_capabilities(manifest)
        .map_err(|_| PluginHostAuditBindingError::InvalidManifest)?;
    let requested = PluginCapabilitySet::new(
        snapshot
            .requested
            .into_iter()
            .map(|capability| capability.capability_id)
            .collect(),
    )?;
    let provided = PluginCapabilitySet::new(
        snapshot
            .provided
            .into_iter()
            .map(|capability| capability.capability_id)
            .collect(),
    )?;
    Ok(CapabilityProfile::new(requested, provided)?)
}

/// 规范化一个必须已存在的普通文件，并拒绝最终路径符号链接。
fn canonical_regular_file(path: &PathBuf, label: &str) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("{label} 必须是绝对路径");
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("{label} 必须是非符号链接普通文件");
    }
    Ok(fs::canonicalize(path)?)
}

/// 返回 manifest 声明的安全相对 Component 文件名。
fn safe_manifest_component_name(value: &str) -> anyhow::Result<std::ffi::OsString> {
    let path = std::path::Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        anyhow::bail!("插件 manifest Component 路径不安全");
    }
    path.file_name()
        .map(std::ffi::OsStr::to_os_string)
        .ok_or_else(|| anyhow::anyhow!("插件 manifest Component 路径缺少文件名"))
}

/// 计算真实 Component 字节的强类型 SHA-256 摘要。
fn digest_bytes(bytes: &[u8]) -> anyhow::Result<ArtifactDigest> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

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
    protocol_component_interface_from_snapshot(
        plugin_id,
        component_digest,
        scanner_revision,
        &evidence.component,
    )
}

/// 从 Host Component 类型图生成协议接口快照。
fn protocol_component_interface_from_snapshot(
    plugin_id: impl Into<String>,
    component_digest: ArtifactDigest,
    scanner_revision: ArtifactDigest,
    component: &HostComponentInterfaceSnapshot,
) -> Result<ComponentInterfaceSnapshot, PluginHostAuditBindingError> {
    let mut imports = component
        .imports
        .iter()
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    let mut exports = component
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
        world: component.world.clone(),
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
    /// manifest 无法通过 Host 正式校验或快照化。
    #[error("插件 manifest 无法生成受信能力快照")]
    InvalidManifest,
}
