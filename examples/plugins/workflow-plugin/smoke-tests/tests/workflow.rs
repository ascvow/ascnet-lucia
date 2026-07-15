//! 动态工作流插件的真实 WASM 端到端测试。

use agent_core::{
    Agent, AgentExtension, AgentOptions, ChatModel, ModelGateway, ModelRequest, ModelResponse,
    ProviderAdapter,
};
use agent_plugin_host::{
    ui::{UiInput, UiInputEvent, UiPlacement, UiRenderRequest, UI_NAVIGATION_EVENT},
    wasm::WasmPluginHost,
    PluginHost, PluginHostServices,
};
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate, RuntimeLimits,
};
use agent_tool::ToolCall;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

/// 记录用户输入并返回确定性文本的离线模型。
struct RecordingModel {
    inputs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ChatModel for RecordingModel {
    /// 根据输入是否包含依赖结果返回对应节点的固定结果。
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let input = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| block.text())
            .collect::<Vec<_>>()
            .join("\n");
        self.inputs
            .lock()
            .expect("输入记录锁不应中毒")
            .push(input.clone());
        let output = if input.contains("准备完成") {
            "复核完成"
        } else {
            "准备完成"
        };
        Ok(ModelResponse::text(output))
    }
}

#[async_trait]
impl ProviderAdapter for RecordingModel {
    /// 返回测试模型使用的稳定 provider 名称。
    fn name(&self) -> &'static str {
        "workflow-fixed"
    }
}

/// 调用真实 component 工具并返回成功内容。
async fn call_tool(host: &WasmPluginHost, name: &str, args: Value) -> Value {
    let result = AgentExtension::call_tool(host, ToolCall::new("workflow-smoke", name, args))
        .await
        .expect("插件工具路由不应失败")
        .expect("工作流插件应处理自己的工具");
    assert!(!result.is_error, "工作流插件返回错误：{}", result.content);
    result.content
}

/// 重复推进工作流，直到指定节点进入成功终态。
async fn tick_until_node_succeeds(
    host: &WasmPluginHost,
    workflow_id: &str,
    node_id: &str,
) -> Value {
    for _ in 0..100 {
        let content = call_tool(host, "workflow_tick", json!({"workflow_id": workflow_id})).await;
        if content["nodes"][node_id]["status"] == "succeeded" {
            return content;
        }
        tokio::task::yield_now().await;
    }
    panic!("节点 `{node_id}` 未在轮询预算内完成");
}

/// 重复推进已封存工作流，直到整体进入成功终态。
async fn tick_until_workflow_succeeds(host: &WasmPluginHost, workflow_id: &str) -> Value {
    for _ in 0..100 {
        let content = call_tool(host, "workflow_tick", json!({"workflow_id": workflow_id})).await;
        if content["status"] == "succeeded" {
            return content;
        }
        tokio::task::yield_now().await;
    }
    panic!("工作流未在轮询预算内完成");
}

/// 验证运行中追加节点、依赖调度、结果注入与封存收敛链路。
#[tokio::test]
async fn component_runs_dynamic_workflow() {
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = ModelGateway::new();
    gateway
        .register(
            "workflow-fixed",
            Arc::new(RecordingModel {
                inputs: Arc::clone(&inputs),
            }),
        )
        .expect("固定模型应注册成功");
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("workflow-fixed", "test-model"),
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
        .expect("动态工作流 component 应加载成功");

    let prompts = AgentExtension::prompt_messages(&host)
        .await
        .expect("工作流编排提示应可读取");
    assert!(prompts
        .iter()
        .any(|prompt| prompt.text_content().contains("workflow_create")));

    let tools = AgentExtension::list_tools(&host)
        .await
        .expect("插件工具应可读取");
    assert_eq!(tools.len(), 6);
    let declarations = PluginHost::ui_declarations(&host)
        .await
        .expect("工作流 UI 声明应可读取");
    assert_eq!(declarations.len(), 3);
    assert_eq!(declarations[0].placement, UiPlacement::ComposerShelf);
    assert_eq!(declarations[1].placement, UiPlacement::Subview);
    assert_eq!(declarations[2].placement, UiPlacement::Subview);

    let created = call_tool(
        &host,
        "workflow_create",
        json!({
            "name": "动态审查",
            "max_parallelism": 2,
            "nodes": [{"id": "prepare", "prompt": "准备材料"}]
        }),
    )
    .await;
    let workflow_id = created["id"].as_str().expect("创建工作流应返回 ID");
    let shelf = PluginHost::render_ui(
        &host,
        &UiRenderRequest {
            plugin_id: "workflow".into(),
            view_id: "workflow-shelf".into(),
            instance_id: None,
            width: 80,
            height: 3,
            focused: true,
            frame: 1,
        },
    )
    .await
    .expect("工作流摘要渲染不应失败")
    .expect("工作流摘要应返回帧");
    assert!(shelf.visible);
    assert!(shelf
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.text.contains("动态审查")));
    PluginHost::on_ui_input(
        &host,
        &UiInput {
            plugin_id: "workflow".into(),
            view_id: "workflow-shelf".into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("工作流入口按键路由不应失败");
    let navigation = AgentExtension::drain_events(&host)
        .await
        .expect("工作流导航事件应可读取");
    assert!(navigation.iter().any(|event| {
        event["name"] == UI_NAVIGATION_EVENT
            && event["data"]["action"]["view"]["view_id"] == "workflow-workspace"
            && event["data"]["action"]["view"]["instance_id"] == workflow_id
    }));
    let workspace = PluginHost::render_ui(
        &host,
        &UiRenderRequest {
            plugin_id: "workflow".into(),
            view_id: "workflow-workspace".into(),
            instance_id: Some(workflow_id.into()),
            width: 80,
            height: 24,
            focused: true,
            frame: 2,
        },
    )
    .await
    .expect("工作流工作台渲染不应失败")
    .expect("工作流工作台应返回帧");
    assert!(workspace
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.text.contains("prepare")));

    let prepared = tick_until_node_succeeds(&host, workflow_id, "prepare").await;
    assert_eq!(prepared["sealed"], false);
    assert_eq!(prepared["status"], "active");

    let expanded = call_tool(
        &host,
        "workflow_add_node",
        json!({
            "workflow_id": workflow_id,
            "node": {
                "id": "review",
                "prompt": "复核材料",
                "dependencies": ["prepare"]
            }
        }),
    )
    .await;
    assert_eq!(expanded["nodes"]["review"]["status"], "pending");

    call_tool(&host, "workflow_seal", json!({"workflow_id": workflow_id})).await;
    let completed = tick_until_workflow_succeeds(&host, workflow_id).await;
    assert_eq!(completed["nodes"]["review"]["output"], "复核完成");
    assert!(inputs
        .lock()
        .expect("输入记录锁不应中毒")
        .iter()
        .any(|input| input.contains("依赖节点结果") && input.contains("准备完成")));

    host.deactivate()
        .await
        .expect("插件卸载应撤销 Runtime 资源");
}
