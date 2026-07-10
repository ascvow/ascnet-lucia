//! Agent Runtime 插件的真实 WASM 端到端测试。

use agent_core::{
    Agent, AgentExtension, AgentOptions, ChatModel, ModelGateway, ModelRequest, ModelResponse,
    ProviderAdapter,
};
use agent_plugin_host::{wasm::WasmPluginHost, PluginHostServices};
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate, RuntimeLimits,
};
use agent_tool::ToolCall;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc};

/// 返回固定文本的离线模型，确保测试不依赖网络或真实服务商。
struct FixedModel;

#[async_trait]
impl ChatModel for FixedModel {
    /// 完成一次确定性的模型调用。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text("派生 Agent 已完成"))
    }
}

#[async_trait]
impl ProviderAdapter for FixedModel {
    /// 返回测试路由使用的稳定 provider 名称。
    fn name(&self) -> &'static str {
        "fixed"
    }
}

/// 调用真实 component 工具并返回成功内容。
async fn call_tool(host: &WasmPluginHost, name: &str, args: Value) -> Value {
    let result = AgentExtension::call_tool(host, ToolCall::new("smoke-call", name, args))
        .await
        .expect("插件工具路由不应失败")
        .expect("Agent Runtime 插件应处理自己的工具");
    assert!(!result.is_error, "插件工具返回错误：{}", result.content);
    result.content
}

/// 轮询真实 component 暴露的非阻塞结果接口。
async fn wait_result(host: &WasmPluginHost, target: &str) -> Value {
    for _ in 0..100 {
        let content = call_tool(host, "agent_runtime_result", json!({"target": target})).await;
        if content["completed"] == true {
            return content;
        }
        tokio::task::yield_now().await;
    }
    panic!("派生 Agent 未在轮询预算内结束");
}

/// 注入 Runtime、加载真实 component，并验证派生、结果与私有会话续跑链路。
#[tokio::test]
async fn component_spawns_and_continues_agent() {
    let mut gateway = ModelGateway::new();
    gateway
        .register("fixed", Arc::new(FixedModel))
        .expect("固定模型应注册成功");
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("fixed", "test-model"),
    );

    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("Runtime 应创建成功");
    let controller_profile = AgentProfileId::new("controller").expect("profile 应合法");
    runtime
        .register_profile(
            controller_profile.clone(),
            AgentTemplate::from_agent(&agent),
            AgentPermissions::default(),
        )
        .await
        .expect("controller profile 应注册成功");

    let services = PluginHostServices::new()
        .with_agent_runtime(
            Arc::new(runtime),
            controller_profile,
            HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
        )
        .expect("Host 应接受 Agent Runtime 服务");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = WasmPluginHost::load_from_manifest_with_services(manifest, services)
        .await
        .expect("Agent Runtime component 应加载成功");

    let tools = AgentExtension::list_tools(&host)
        .await
        .expect("插件工具应可读取");
    assert_eq!(tools.len(), 6);

    let spawned = call_tool(
        &host,
        "agent_runtime_spawn",
        json!({"input": "检查第一轮任务"}),
    )
    .await;
    let first_id = spawned["handle"]["id"]
        .as_str()
        .expect("spawn 应返回 Agent ID");
    let first_result = wait_result(&host, first_id).await;
    assert_eq!(
        first_result["outcome"]["result"]["final_text"],
        "派生 Agent 已完成"
    );

    let continued = call_tool(
        &host,
        "agent_runtime_continue",
        json!({"target": first_id, "input": "继续检查第二轮任务"}),
    )
    .await;
    assert_eq!(continued["handle"]["lineage"]["parent"], first_id);
    let second_id = continued["handle"]["id"]
        .as_str()
        .expect("continue 应返回新 Agent ID");
    let second_result = wait_result(&host, second_id).await;
    assert_eq!(second_result["outcome"]["status"], "succeeded");

    host.deactivate()
        .await
        .expect("插件卸载应撤销 Runtime 资源");
}
