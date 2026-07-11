//! Teammate 插件的真实 WASM 端到端测试。

use agent_core::{
    Agent, AgentExtension, AgentOptions, ChatModel, ModelGateway, ModelRequest, ModelResponse,
    ProviderAdapter,
};
use agent_plugin_host::{
    ui::{UiInput, UiInputEvent, UiPlacement, UiRenderRequest, UI_NAVIGATION_EVENT},
    wasm::WasmPluginHost,
    PluginHost, PluginHostServices, PluginServiceCall,
};
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate, RuntimeLimits,
};
use agent_tool::ToolCall;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc};

/// 返回固定文本的离线模型，确保端到端测试不依赖网络。
struct FixedModel;

#[async_trait]
impl ChatModel for FixedModel {
    /// 完成一次确定性的模型调用。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text("teammate 已完成"))
    }
}

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
        .expect("Teammate 插件应处理自己的工具");
    assert!(!result.is_error, "插件工具返回错误：{}", result.content);
    result.content
}

/// 轮询成员当前执行句柄，直到返回终态结果。
async fn wait_result(host: &WasmPluginHost, member_id: &str) -> Value {
    for _ in 0..100 {
        let content = call_tool(host, "teammate_result", json!({"member_id": member_id})).await;
        if content["completed"] == true {
            return content;
        }
        tokio::task::yield_now().await;
    }
    panic!("teammate 未在轮询预算内结束");
}

/// 验证派生、可信发送、邮箱、dispatch 自动确认和续跑句柄更新链路。
#[tokio::test]
async fn component_runs_mailbox_dispatch_flow() {
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
        .expect("Teammate component 应加载成功");

    let tools = AgentExtension::list_tools(&host)
        .await
        .expect("插件工具应可读取");
    assert_eq!(tools.len(), 10);
    let declarations = PluginHost::ui_declarations(&host)
        .await
        .expect("团队 UI 声明应可读取");
    assert_eq!(declarations.len(), 2);
    assert_eq!(declarations[0].placement, UiPlacement::Right);
    assert_eq!(declarations[1].placement, UiPlacement::Subview);
    let dock = PluginHost::render_ui(
        &host,
        &UiRenderRequest {
            plugin_id: "teammate".into(),
            view_id: "teammate-team-dock".into(),
            instance_id: None,
            width: 30,
            height: 16,
            focused: true,
            frame: 1,
        },
    )
    .await
    .expect("团队摘要渲染不应失败")
    .expect("团队摘要应返回可见帧");
    assert!(dock.visible);
    PluginHost::on_ui_input(
        &host,
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-team-dock".into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("团队入口按键路由不应失败");
    let navigation_events = AgentExtension::drain_events(&host)
        .await
        .expect("团队导航事件应可读取");
    assert!(navigation_events.iter().any(|event| {
        event["name"] == UI_NAVIGATION_EVENT
            && event["data"]["action"]["push"]["view"]["view_id"]
                == "teammate-team-workspace"
    }));
    let services = PluginHost::services(&host)
        .await
        .expect("插件服务目录应可读取");
    assert!(services.iter().any(|service| {
        service.plugin_id == "teammate"
            && service.name == "teammate.mailbox"
            && service.version == "1.0.0"
    }));
    let isolated_list = PluginHost::call_service(
        &host,
        &PluginServiceCall {
            caller_id: "orchestrator".into(),
            plugin_id: "teammate".into(),
            name: "teammate.mailbox".into(),
            payload: json!({"operation": "list"}),
        },
    )
    .await
    .expect("版本化 service 调用不应失败")
    .expect("Teammate 插件应处理自己的 service");
    assert!(isolated_list["members"]
        .as_array()
        .expect("service 应返回成员数组")
        .is_empty());

    let spawned = call_tool(
        &host,
        "teammate_spawn",
        json!({"role": "reviewer", "input": "检查首轮实现"}),
    )
    .await;
    let member_id = spawned["member"]["id"]
        .as_str()
        .expect("spawn 应返回稳定成员地址");
    wait_result(&host, member_id).await;

    let sent = call_tool(
        &host,
        "teammate_send",
        json!({
            "recipient": member_id,
            "topic": "review.requested",
            "payload": {"path": "src/lib.rs"}
        }),
    )
    .await;
    assert_eq!(sent["message"]["sender"]["kind"], "controller");
    let message_id = sent["message"]["id"]
        .as_u64()
        .expect("send 应返回消息 ID");

    let inbox = call_tool(
        &host,
        "teammate_inbox",
        json!({"member_id": member_id}),
    )
    .await;
    assert_eq!(inbox["messages"].as_array().map(Vec::len), Some(1));

    let dispatched = call_tool(
        &host,
        "teammate_dispatch",
        json!({"member_id": member_id, "message_id": message_id}),
    )
    .await;
    assert_eq!(dispatched["acked"], true);
    assert_ne!(dispatched["handle"]["id"], member_id);
    let second_result = wait_result(&host, member_id).await;
    assert_eq!(second_result["outcome"]["status"], "succeeded");

    let empty_inbox = call_tool(
        &host,
        "teammate_inbox",
        json!({"member_id": member_id}),
    )
    .await;
    assert!(empty_inbox["messages"]
        .as_array()
        .expect("inbox 应返回数组")
        .is_empty());

    let removed = call_tool(
        &host,
        "teammate_remove",
        json!({"member_id": member_id}),
    )
    .await;
    assert_eq!(removed["removed"], true);
    let members = call_tool(&host, "teammate_list", json!({})).await;
    assert!(members["members"]
        .as_array()
        .expect("成员目录应返回数组")
        .is_empty());

    host.deactivate().await.expect("插件应正常卸载");
}
