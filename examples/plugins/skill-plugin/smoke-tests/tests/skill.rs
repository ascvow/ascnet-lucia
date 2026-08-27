//! Lucia 官方 Skill 插件的真实 WASM 端到端测试。

use agent_core::AgentExtension;
use agent_evaluation::{
    bind_plugin_host_audit, protocol_component_interface, PluginHostAuditBinding,
    TrustedHostCheckOutcome,
};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, CapabilityProfile, MutationId, PluginCapabilitySet,
};
use agent_plugin_host::{
    audit::{audit_plugin_component, ComponentInterfaceItemKind},
    manifest::{resolve_plugin_capabilities, PluginManifest},
    wasm::WasmPluginHost,
    PluginHostServices,
};
use agent_tool::ToolCall;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path};

/// 计算真实测试字节的协议 SHA-256 摘要。
fn artifact_digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 摘要应合法")
}

/// 构造 Guest 可复核的版本化 Skill Set 注入值。
fn skill_set_json(
    skill_id: &str,
    name: &str,
    instructions: &str,
    status: &str,
    execution_profile: &str,
) -> (String, String) {
    let artifact_json = json!({
        "schema_version": 1,
        "skill_id": skill_id,
        "revision": 1,
        "operation": {"type": "create"},
        "name": name,
        "description": "来自 Genome CAS 的固定 Skill。",
        "instructions": instructions,
        "trigger_policy": {"mode": "manual"},
        "required_capabilities": [],
        "source_episode_ids": ["episode_00000000000000000000000000000000"],
        "mutation_id": "mutation_00000000000000000000000000000000",
        "status_history": [{"status": status, "recorded_at_ms": 1}]
    })
    .to_string();
    let digest = format!("{:x}", Sha256::digest(artifact_json.as_bytes()));
    let skill_set = json!({
        "schema_version": 1,
        "genome_revision_id": "grev_smoketest1",
        "genome_digest": "a".repeat(64),
        "execution_profile": execution_profile,
        "skills": [{
            "skill_id": skill_id,
            "artifact_digest": digest,
            "artifact_json": artifact_json
        }]
    })
    .to_string();
    (skill_set, digest)
}

/// 使用指定插件 ID 注入 Skill Set 并加载真实 component。
async fn load_with_skill_set(plugin_id: &str, skill_set: String) -> anyhow::Result<WasmPluginHost> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let services = PluginHostServices::new().with_activation_metadata(
        plugin_id,
        HashMap::from([("skill_set_json".into(), skill_set)]),
    )?;
    WasmPluginHost::load_from_manifest_with_services(manifest, services).await
}

/// 真实 Skill component 的 manifest、imports、exports 与能力 owner 必须形成同一审计证据。
#[tokio::test]
async fn component_exposes_complete_host_audit_evidence() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let manifest = PluginManifest::load(&manifest_path).expect("加载 Skill manifest");
    let resolved = resolve_plugin_capabilities(
        std::slice::from_ref(&manifest),
        &std::collections::HashMap::new(),
    )
    .expect("解析 Skill 能力 owner");
    let component_path = manifest_path
        .parent()
        .expect("Skill manifest 应有父目录")
        .join(&manifest.plugin.wasm);

    let component_bytes = std::fs::read(&component_path).expect("读取真实 Skill component");
    let manifest_bytes = std::fs::read(&manifest_path).expect("读取真实 Skill manifest");
    let host = WasmPluginHost::load_from_manifest(&manifest_path)
        .await
        .expect("真实 Skill component 应通过 Host 装载 smoke");
    let tools = host
        .list_tools()
        .await
        .expect("真实 Skill 工具路由应可读取");
    let evidence = audit_plugin_component(&manifest, &component_path, &resolved, Vec::new())
        .expect("真实 Skill component 审计应成功");
    assert_eq!(evidence.manifest.plugin_id, "skill");
    assert_eq!(evidence.manifest.provided[0].capability_id, "agent.skills");
    assert!(evidence
        .capability_import_checks
        .iter()
        .any(|check| check.capability_id == "fs_read" && check.satisfied));
    assert!(evidence
        .component
        .imports
        .iter()
        .any(|item| item.path == "host-fs-read"));
    assert!(evidence.component.exports.iter().any(|item| {
        item.path == "list-tools" && item.kind == ComponentInterfaceItemKind::ComponentFunction
    }));
    assert_eq!(
        evidence.resolved_capability_owners[0].owner_plugin_ids,
        ["skill"]
    );
    assert!(evidence.observed_host_service_calls.is_empty());

    let component_digest = artifact_digest(&component_bytes);
    let scanner_revision = artifact_digest(b"wasmtime-component-types-v1");
    let expected_interface = protocol_component_interface(
        "skill",
        component_digest.clone(),
        scanner_revision,
        &evidence,
    )
    .expect("真实 Skill 接口快照应进入 M8 协议");
    let expected_capabilities = CapabilityProfile::new(
        PluginCapabilitySet::new(vec!["fs_read".into()]).expect("Skill 请求能力应合法"),
        PluginCapabilitySet::new(vec!["agent.skills".into()]).expect("Skill 提供能力应合法"),
    )
    .expect("Skill 能力 Profile 应合法");
    let evidence_bytes = serde_json::to_vec(&evidence).expect("序列化 Host 审计证据");
    let host_audit = bind_plugin_host_audit(
        &evidence,
        PluginHostAuditBinding {
            plugin_id: "skill".into(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            component_digest,
            manifest_digest: artifact_digest(&manifest_bytes),
            bundle_digest: artifact_digest(&[manifest_bytes, component_bytes].concat()),
            expected_interface,
            expected_capabilities,
            verifier_revision: artifact_digest(b"m8-host-audit-adapter-v1"),
            host_smoke: TrustedHostCheckOutcome {
                report_digest: artifact_digest(
                    serde_json::to_string(&tools)
                        .expect("序列化真实工具路由摘要")
                        .as_bytes(),
                ),
                check_count: 2,
                failure_count: 0,
            },
            runtime_audit: TrustedHostCheckOutcome {
                report_digest: artifact_digest(&evidence_bytes),
                check_count: 1,
                failure_count: 0,
            },
            completed_at_ms: 1,
        },
    )
    .expect("真实 Skill Host 证据应转换为完整 M8 Gate 证据");
    assert!(host_audit.host_smoke.passed);
    assert!(host_audit.manifest_audit.passed);
    assert!(host_audit.import_audit.passed);
    assert!(host_audit.interface_audit.passed);
    assert!(host_audit.owner_audit.passed);
    assert!(host_audit.runtime_audit.passed);
}

/// 未注入 Genome Skill Set 时继续扫描目录并按需读取 `SKILL.md`。
#[tokio::test]
async fn component_preserves_local_scan_compatibility() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = WasmPluginHost::load_from_manifest(manifest)
        .await
        .expect("Skill component 应加载成功");

    let prompts = host.prompt_messages().await.expect("读取 Skill 提示贡献");
    assert_eq!(prompts.len(), 1);
    let prompt = prompts[0].text_content();
    assert!(prompt.contains("lucia-plugin-development"));
    assert!(prompt.contains("开发或修改 Lucia WASM 插件"));
    assert!(!prompt.contains("保持 Agent Core"));

    let result = host
        .call_tool(ToolCall::new(
            "local-skill-call",
            "skill_read",
            json!({"name": "lucia-plugin-development"}),
        ))
        .await
        .expect("Skill 工具路由不应失败")
        .expect("Skill 插件应处理读取工具");
    assert!(!result.is_error);
    assert!(result.content["content"]
        .as_str()
        .expect("Skill 内容应为字符串")
        .contains("保持 Agent Core"));
}

/// Genome 模式只提供注入的 Active Skill，按需读取返回精确 CAS 指令和可信绑定事件。
#[tokio::test]
async fn component_loads_only_active_genome_skill_and_emits_binding_event() {
    let instructions = "只执行 Genome 固定的精确指令，不读取插件目录。";
    let (skill_set, digest) = skill_set_json(
        "skill_genome01",
        "genome-only-skill",
        instructions,
        "active",
        "serve",
    );
    let host = load_with_skill_set("skill", skill_set)
        .await
        .expect("Active Genome Skill 应完成真实 component 激活");

    let prompts = host.prompt_messages().await.expect("读取 Skill 提示贡献");
    let prompt = prompts[0].text_content();
    assert!(prompt.contains("genome-only-skill"));
    assert!(!prompt.contains("lucia-plugin-development"));

    let result = host
        .call_tool(ToolCall::new(
            "genome-skill-call",
            "skill_read",
            json!({"name": "genome-only-skill"}),
        ))
        .await
        .expect("Genome Skill 工具路由不应失败")
        .expect("Genome Skill 插件应处理读取工具");
    assert!(!result.is_error);
    assert_eq!(result.content["content"], instructions);
    assert_eq!(result.content["skill_id"], "skill_genome01");
    assert_eq!(result.content["artifact_digest"], digest);

    let events = host.drain_events().await.expect("应读取插件结构化事件");
    let loaded = events
        .iter()
        .find(|event| event["name"] == "skill.loaded.v1")
        .expect("按需读取应发出 skill.loaded.v1");
    assert_eq!(loaded["data"]["schema_version"], 1);
    assert_eq!(loaded["data"]["skill_id"], "skill_genome01");
    assert_eq!(loaded["data"]["artifact_digest"], digest);
    assert_eq!(loaded["data"]["genome_revision_id"], "grev_smoketest1");
    assert_eq!(loaded["data"]["genome_digest"], "a".repeat(64));
    assert_eq!(loaded["data"]["call_id"], "genome-skill-call");
    assert!(loaded["data"].get("success").is_none());
    assert!(loaded["data"].get("result").is_none());
}

/// Host 元数据必须按真实 plugin_id 隔离，其他插件的 Skill Set 不能覆盖本插件扫描模式。
#[tokio::test]
async fn activation_metadata_is_isolated_by_plugin_id() {
    let (skill_set, _) = skill_set_json(
        "skill_isolated1",
        "must-not-leak",
        "不应泄露到 skill 插件。",
        "active",
        "serve",
    );
    let host = load_with_skill_set("other-plugin", skill_set)
        .await
        .expect("其他插件的激活元数据不应阻止 skill 插件加载");
    let prompts = host.prompt_messages().await.expect("读取隔离后的提示");
    let prompt = prompts[0].text_content();
    assert!(prompt.contains("lucia-plugin-development"));
    assert!(!prompt.contains("must-not-leak"));
}

/// 注入制品摘要被篡改时，Guest 必须在注册工具和提示前失败关闭。
#[tokio::test]
async fn component_rejects_tampered_artifact_digest() {
    let (skill_set, _) = skill_set_json(
        "skill_tampered1",
        "tampered-skill",
        "不应装配。",
        "active",
        "serve",
    );
    let mut value: Value = serde_json::from_str(&skill_set).expect("测试 Skill Set 应可解析");
    value["skills"][0]["artifact_digest"] = Value::String("0".repeat(64));
    let error = match load_with_skill_set("skill", value.to_string()).await {
        Ok(_) => panic!("摘要篡改必须阻止 component 激活"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("制品摘要不匹配"));
}

/// Quarantined、Deprecated 与 Deleted 终态都不能进入新的 Evidence 运行。
#[tokio::test]
async fn component_rejects_non_active_genome_skill_statuses() {
    for (index, status) in ["quarantined", "deprecated", "deleted"]
        .into_iter()
        .enumerate()
    {
        let skill_id = format!("skill_reject{index:02}");
        let (skill_set, _) = skill_set_json(
            &skill_id,
            &format!("rejected-{status}"),
            "不应装配。",
            status,
            "serve",
        );
        let error = match load_with_skill_set("skill", skill_set).await {
            Ok(_) => panic!("非 Active Skill 必须阻止 component 激活"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("不能进入 serve 运行"));
    }
}

/// Evaluation 平面可装载 Quarantined/Evaluated 候选并保持精确指令绑定。
#[tokio::test]
async fn evaluation_profile_loads_quarantined_and_evaluated_candidates() {
    for (index, status) in ["quarantined", "evaluated"].into_iter().enumerate() {
        let skill_id = format!("skill_evalset{index}");
        let instructions = format!("{status} 候选的固定评测指令。");
        let (skill_set, digest) = skill_set_json(
            &skill_id,
            &format!("candidate-{status}"),
            &instructions,
            status,
            "evaluation",
        );
        let host = load_with_skill_set("skill", skill_set)
            .await
            .expect("Evaluation 应允许装载隔离或已评测候选");
        let result = host
            .call_tool(ToolCall::new(
                format!("evaluation-call-{index}"),
                "skill_read",
                json!({"name": format!("candidate-{status}")}),
            ))
            .await
            .expect("Evaluation Skill 工具路由不应失败")
            .expect("Evaluation Skill 插件应处理读取工具");
        assert_eq!(result.content["content"], instructions);
        assert_eq!(result.content["artifact_digest"], digest);
    }
}
