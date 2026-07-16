//! 动态任务列表工作流插件的真实 WASM 端到端测试。

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
    /// 根据输入是否包含依赖结果返回对应任务的固定结果。
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

/// 从任务列表快照中取出指定任务。
fn task_in<'a>(content: &'a Value, task_id: &str) -> &'a Value {
    content["tasks"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["id"] == task_id))
        .unwrap_or(&Value::Null)
}

/// 反复读取任务列表（每次读取都会自动同步与调度），直到任务进入期望状态。
async fn list_until_status(host: &WasmPluginHost, task_id: &str, expected: &str) -> Value {
    for _ in 0..200 {
        let content = call_tool(host, "task_list", json!({})).await;
        if task_in(&content, task_id)["status"] == expected {
            return content;
        }
        tokio::task::yield_now().await;
    }
    panic!("任务 `{task_id}` 未在轮询预算内进入 `{expected}`");
}

/// 验证自动派生、依赖链自动推进、结果注入、动态追加与取消删除链路。
#[tokio::test]
async fn component_runs_dynamic_task_list() {
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
        .expect("任务编排提示应可读取");
    assert!(prompts
        .iter()
        .any(|prompt| prompt.text_content().contains("task_create")));

    let tools = AgentExtension::list_tools(&host)
        .await
        .expect("插件工具应可读取");
    assert_eq!(tools.len(), 4);
    let declarations = PluginHost::ui_declarations(&host)
        .await
        .expect("任务 UI 声明应可读取");
    assert_eq!(declarations.len(), 3);
    assert_eq!(declarations[0].placement, UiPlacement::ComposerShelf);
    assert_eq!(declarations[1].placement, UiPlacement::Subview);
    assert_eq!(declarations[2].placement, UiPlacement::Subview);

    // 批量创建后就绪任务应立即自动派生，被依赖阻塞的任务保持等待。
    let created = call_tool(
        &host,
        "task_create",
        json!({
            "tasks": [
                {"id": "prepare", "prompt": "准备材料"},
                {"id": "review", "prompt": "复核材料", "depends_on": ["prepare"]}
            ]
        }),
    )
    .await;
    assert_ne!(task_in(&created, "prepare")["status"], "pending");
    assert_eq!(task_in(&created, "review")["status"], "pending");
    assert_eq!(task_in(&created, "review")["blocked_by"][0], "prepare");

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
    .expect("任务摘要渲染不应失败")
    .expect("任务摘要应返回帧");
    assert!(shelf.visible);
    assert!(shelf
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.text.contains("任务")));
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
    .expect("任务入口按键路由不应失败");
    let navigation = AgentExtension::drain_events(&host)
        .await
        .expect("任务导航事件应可读取");
    assert!(navigation.iter().any(|event| {
        event["name"] == UI_NAVIGATION_EVENT
            && event["data"]["action"]["view"]["view_id"] == "workflow-workspace"
            && event["data"]["action"]["view"]["instance_id"] == "tasks"
    }));
    let workspace = PluginHost::render_ui(
        &host,
        &UiRenderRequest {
            plugin_id: "workflow".into(),
            view_id: "workflow-workspace".into(),
            instance_id: Some("tasks".into()),
            width: 80,
            height: 24,
            focused: true,
            frame: 2,
        },
    )
    .await
    .expect("任务工作台渲染不应失败")
    .expect("任务工作台应返回帧");
    assert!(workspace
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.text.contains("prepare")));

    // 依赖链应在轮询同步中自动推进：prepare 完成后 review 自动派生并完成。
    let completed = list_until_status(&host, "review", "completed").await;
    assert_eq!(task_in(&completed, "prepare")["status"], "completed");
    let review = call_tool(&host, "task_get", json!({"id": "review"})).await;
    assert_eq!(review["output"], "复核完成");
    assert!(inputs
        .lock()
        .expect("输入记录锁不应中毒")
        .iter()
        .any(|input| input.contains("依赖任务结果") && input.contains("准备完成")));

    // 运行中随时追加新任务：依赖已完成时创建即自动派生。
    let expanded = call_tool(
        &host,
        "task_create",
        json!({
            "tasks": [{"id": "summary", "prompt": "总结结论", "depends_on": ["review"]}]
        }),
    )
    .await;
    assert_ne!(task_in(&expanded, "summary")["status"], "pending");

    // 取消并删除不再需要的任务。
    call_tool(
        &host,
        "task_create",
        json!({
            "tasks": [{"id": "obsolete", "prompt": "多余的收尾", "depends_on": ["summary"]}]
        }),
    )
    .await;
    let cancelled = call_tool(
        &host,
        "task_update",
        json!({"id": "obsolete", "status": "cancelled"}),
    )
    .await;
    assert_eq!(task_in(&cancelled, "obsolete")["status"], "cancelled");
    let deleted = call_tool(&host, "task_update", json!({"id": "obsolete", "delete": true})).await;
    assert!(task_in(&deleted, "obsolete").is_null());

    // 全部任务完成后，进度架应自动隐藏。
    list_until_status(&host, "summary", "completed").await;
    let settled_shelf = PluginHost::render_ui(
        &host,
        &UiRenderRequest {
            plugin_id: "workflow".into(),
            view_id: "workflow-shelf".into(),
            instance_id: None,
            width: 80,
            height: 3,
            focused: false,
            frame: 3,
        },
    )
    .await
    .expect("任务摘要渲染不应失败")
    .expect("任务摘要应返回帧");
    assert!(!settled_shelf.visible);

    host.deactivate()
        .await
        .expect("插件卸载应撤销 Runtime 资源");
}
