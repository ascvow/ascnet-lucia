//! M8 Host 中立审计证据到插件 Gate 协议的适配测试。

use agent_evaluation::{
    bind_plugin_host_audit, protocol_component_interface, PluginHostAuditBinding,
    TrustedHostCheckOutcome,
};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, CapabilityProfile, MutationId, PluginCapabilitySet,
};
use agent_plugin_host::{
    audit::{
        CapabilityImportCheck, ComponentInterfaceItemKind, ComponentInterfaceItemSnapshot,
        ComponentInterfaceSnapshot as HostComponentInterfaceSnapshot, HostServiceCallObservation,
        HostServiceCallResult, ManifestCapabilityRequest, ManifestCapabilitySnapshot,
        ManifestProvidedCapability, PluginAuditEvidence, ResolvedCapabilityOwnerSnapshot,
        COMPONENT_INTERFACE_SCANNER_REVISION, COMPONENT_ROOT_WORLD,
    },
    manifest::ProvidedCapabilityMode,
};

/// 构造固定测试摘要。
fn digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造已通过 Host manifest、Component、import 与 owner 扫描的中立证据。
fn host_evidence() -> PluginAuditEvidence {
    PluginAuditEvidence {
        manifest: ManifestCapabilitySnapshot {
            plugin_id: "example.plugin".into(),
            requested: vec![ManifestCapabilityRequest {
                capability_id: "fs_read".into(),
                scopes: vec!["/workspace".into()],
            }],
            provided: vec![ManifestProvidedCapability {
                capability_id: "example.service".into(),
                version: "1.0.0".into(),
                mode: ProvidedCapabilityMode::Exclusive,
            }],
        },
        component: HostComponentInterfaceSnapshot {
            scanner_revision: COMPONENT_INTERFACE_SCANNER_REVISION.into(),
            world: COMPONENT_ROOT_WORLD.into(),
            imports: vec![ComponentInterfaceItemSnapshot {
                path: "host-fs-read".into(),
                kind: ComponentInterfaceItemKind::ComponentFunction,
                implements: None,
            }],
            exports: vec![ComponentInterfaceItemSnapshot {
                path: "run".into(),
                kind: ComponentInterfaceItemKind::ComponentFunction,
                implements: None,
            }],
        },
        capability_import_checks: vec![CapabilityImportCheck {
            capability_id: "fs_read".into(),
            mapped_host_imports: vec!["host-fs-read".into()],
            reachable_imports: vec!["host-fs-read".into()],
            satisfied: true,
        }],
        resolved_capability_owners: vec![ResolvedCapabilityOwnerSnapshot {
            capability_id: "example.service".into(),
            mode: ProvidedCapabilityMode::Exclusive,
            owner_plugin_ids: vec!["example.plugin".into()],
        }],
        observed_host_service_calls: Vec::new(),
    }
}

/// 构造与 Host manifest 精确对应的协议能力 Profile。
fn capability_profile() -> CapabilityProfile {
    CapabilityProfile::new(
        PluginCapabilitySet::new(vec!["fs_read".into()]).expect("请求能力应合法"),
        PluginCapabilitySet::new(vec!["example.service".into()]).expect("提供能力应合法"),
    )
    .expect("能力 Profile 应合法")
}

/// 构造真实 Host 证据所需的 Candidate 身份与外部 smoke/runtime 结果。
fn binding(evidence: &PluginAuditEvidence) -> PluginHostAuditBinding {
    let component_digest = digest('a');
    let expected_interface = protocol_component_interface(
        "example.plugin",
        component_digest.clone(),
        digest('b'),
        evidence,
    )
    .expect("真实 Host 接口应能进入协议");
    PluginHostAuditBinding {
        plugin_id: "example.plugin".into(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest,
        manifest_digest: digest('c'),
        bundle_digest: digest('d'),
        expected_interface,
        expected_capabilities: capability_profile(),
        verifier_revision: digest('e'),
        host_smoke: TrustedHostCheckOutcome {
            report_digest: digest('f'),
            check_count: 2,
            failure_count: 0,
        },
        runtime_audit: TrustedHostCheckOutcome {
            report_digest: digest('1'),
            check_count: 3,
            failure_count: 0,
        },
        completed_at_ms: 100,
    }
}

/// 真实 Host 快照应直接生成全部六项通过证据，不再由测试手工伪造摘要集合。
#[test]
fn converts_real_host_snapshot_into_complete_gate_evidence() {
    let evidence = host_evidence();
    let audit = bind_plugin_host_audit(&evidence, binding(&evidence))
        .expect("完整 Host 证据应完成协议绑定");
    assert_eq!(audit.plugin_id, "example.plugin");
    assert!(audit.host_smoke.passed);
    assert!(audit.manifest_audit.passed);
    assert!(audit.import_audit.passed);
    assert!(audit.interface_audit.passed);
    assert!(audit.owner_audit.passed);
    assert!(audit.runtime_audit.passed);
    audit.validate().expect("转换结果必须满足协议");
}

/// 不可达 import、错误 owner/caller 和真实服务失败必须保留为独立 Gate 失败计数。
#[test]
fn preserves_host_import_owner_and_runtime_failures() {
    let mut evidence = host_evidence();
    evidence.capability_import_checks[0].satisfied = false;
    evidence.resolved_capability_owners[0].owner_plugin_ids = vec!["foreign.plugin".into()];
    evidence
        .observed_host_service_calls
        .push(HostServiceCallObservation {
            caller_id: "forged.plugin".into(),
            target_owner_id: "foreign.plugin".into(),
            service: "query".into(),
            method: Some("run".into()),
            result: HostServiceCallResult::Failed {
                error: "permission_denied".into(),
            },
        });
    let audit = bind_plugin_host_audit(&evidence, binding(&evidence))
        .expect("结构完整的失败证据仍应生成 Gate 输入");
    assert_eq!(audit.import_audit.failure_count, 1);
    assert_eq!(audit.owner_audit.failure_count, 2);
    assert_eq!(audit.runtime_audit.failure_count, 1);
    assert!(!audit.import_audit.passed);
    assert!(!audit.owner_audit.passed);
    assert!(!audit.runtime_audit.passed);
}
