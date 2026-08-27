//! Lucia 官方 Skill 插件的真实 WASM 端到端测试。

use agent_core::AgentExtension;
use agent_plugin_host::{
    wasm::WasmPluginHost,
    PluginHostServices,
};
use agent_tool::ToolCall;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path};

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
