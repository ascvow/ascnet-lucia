//! M8 真实官方插件 WASM Host smoke。

use agent_evaluation::{
    protocol_component_interface, run_plugin_host_smoke, PluginHostAuditBinding,
    PluginHostSmokeInput, TrustedHostCheckOutcome,
};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, CapabilityProfile, MutationId, PluginCapabilitySet,
};
use agent_plugin_host::audit::audit_plugin_component;
use agent_plugin_host::manifest::{resolve_plugin_capabilities, PluginManifest};
use agent_plugin_host::wasm::WasmPluginLimits;
use agent_plugin_manager::hash_plugin_bundle;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// 使用环境变量指定的真实官方 Component 执行 activation、声明读取、shutdown 与审计绑定。
///
/// 需要设置 `LUCIA_M8_SMOKE_MANIFEST`、`LUCIA_M8_SMOKE_COMPONENT` 和
/// `LUCIA_M8_SMOKE_FIXTURE_ROOT`。测试不会构建或改写 Component，适合由 M8 E2E 在官方
/// 插件已经完成 release component 构建后显式运行。
#[tokio::test]
#[ignore = "需要环境变量提供已构建的真实官方 WASM Component"]
async fn activates_real_official_plugin_and_binds_host_audit() {
    let manifest_path = required_path("LUCIA_M8_SMOKE_MANIFEST");
    let component_path = required_path("LUCIA_M8_SMOKE_COMPONENT");
    let fixture_root = required_path("LUCIA_M8_SMOKE_FIXTURE_ROOT");
    let manifest = PluginManifest::load(&manifest_path).expect("真实 manifest 应通过 Host 校验");
    let resolved = resolve_plugin_capabilities(std::slice::from_ref(&manifest), &HashMap::new())
        .expect("官方插件能力 owner 应可解析");
    let component_digest = file_digest(&component_path);
    let manifest_digest = file_digest(&manifest_path);
    let raw_audit = audit_plugin_component(&manifest, &component_path, &resolved, Vec::new())
        .expect("真实 Component 应可由 Host 审计器扫描");
    let scanner_revision = bytes_digest(b"m8-plugin-host-smoke-scanner-v1");
    let expected_interface = protocol_component_interface(
        manifest.plugin.id.clone(),
        component_digest.clone(),
        scanner_revision,
        &raw_audit,
    )
    .expect("真实接口快照应可绑定到 M8 协议");
    let expected_capabilities = expected_capabilities(&raw_audit);
    let bundle_root = manifest_path.parent().expect("manifest 应位于 Bundle 根内");
    let bundle_digest = ArtifactDigest::from_sha256_hex(
        hash_plugin_bundle(bundle_root).expect("真实 Bundle 应可计算确定性摘要"),
    )
    .expect("真实 Bundle 摘要应符合协议格式");
    let placeholder = TrustedHostCheckOutcome {
        report_digest: bytes_digest(b"m8-real-smoke-overwrite"),
        check_count: 1,
        failure_count: 0,
    };
    let binding = PluginHostAuditBinding {
        plugin_id: manifest.plugin.id.clone(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest,
        manifest_digest,
        bundle_digest,
        expected_interface,
        expected_capabilities,
        verifier_revision: bytes_digest(b"m8-plugin-host-smoke-verifier-v1"),
        host_smoke: placeholder.clone(),
        runtime_audit: placeholder,
        completed_at_ms: 1_800_000_000_000,
    };

    let output = run_plugin_host_smoke(PluginHostSmokeInput {
        manifest_path: &manifest_path,
        component_path: &component_path,
        fixture_root: &fixture_root,
        resolved_capabilities: &resolved,
        limits: WasmPluginLimits::default(),
        binding,
    })
    .await
    .expect("真实官方插件 Host smoke 应通过");

    println!(
        "plugin={} prompts={} tools={:?} ui={:?} renderers={:?} services={:?}",
        output.declarations.plugin_id,
        output.declarations.prompt_messages.len(),
        output
            .declarations
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        output
            .declarations
            .ui_declarations
            .iter()
            .map(|declaration| declaration.view_id.as_str())
            .collect::<Vec<_>>(),
        output
            .declarations
            .tool_renderers
            .iter()
            .map(|renderer| renderer.renderer_id.as_str())
            .collect::<Vec<_>>(),
        output
            .declarations
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>(),
    );

    assert_eq!(output.declarations.plugin_id, manifest.plugin.id);
    assert!(output.host_audit.host_smoke.passed);
    assert!(output.host_audit.manifest_audit.passed);
    assert!(output.host_audit.import_audit.passed);
    assert!(output.host_audit.interface_audit.passed);
    assert!(output.host_audit.owner_audit.passed);
    assert!(output.host_audit.runtime_audit.passed);
    assert_eq!(
        output.host_audit.host_smoke.report_digest,
        output.declaration_report_digest
    );
}

/// 从 Host 中立审计快照重建期望的 M8 能力 Profile。
fn expected_capabilities(
    evidence: &agent_plugin_host::audit::PluginAuditEvidence,
) -> CapabilityProfile {
    let requested = PluginCapabilitySet::new(
        evidence
            .manifest
            .requested
            .iter()
            .map(|capability| capability.capability_id.clone())
            .collect(),
    )
    .expect("真实请求能力应符合 M8 命名规则");
    let provided = PluginCapabilitySet::new(
        evidence
            .manifest
            .provided
            .iter()
            .map(|capability| capability.capability_id.clone())
            .collect(),
    )
    .expect("真实提供能力应符合 M8 命名规则");
    CapabilityProfile::new(requested, provided).expect("真实能力 Profile 应合法")
}

/// 读取必填环境变量并转换为路径。
fn required_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("必须设置环境变量 {name}"))
}

/// 读取真实文件并计算 SHA-256 摘要。
fn file_digest(path: &Path) -> ArtifactDigest {
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    bytes_digest(&bytes)
}

/// 计算测试绑定使用的合法 SHA-256 摘要。
fn bytes_digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes))).expect("测试摘要应合法")
}
