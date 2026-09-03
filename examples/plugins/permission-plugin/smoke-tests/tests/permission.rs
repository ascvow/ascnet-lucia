//! 权限插件真实 WASM Host 路由测试。

use agent_core::model::{ModelProviderConfig, ProviderKind};
use agent_core::{
    AgentEvent, AgentEventKind, AgentExtension, AgentModelConfig, AgentRecordEvent, ChatModel,
    EventBus, ModelGateway, ModelRequest, ModelResponse, ProviderAdapter, ToolCallEvent,
};
use agent_plugin_host::{
    manifest::TOOL_POLICY_CAPABILITY,
    ui::{UiInput, UiInputEvent, UiPlacement, UiRenderRequest},
    wasm::{load_wasm_plugins, load_wasm_plugins_with_services},
    CompositePluginHost, LivePluginHost, PluginHost, PluginHostServices,
};
use agent_tool::{ToolCall, ToolPermissionScope};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::Notify;

const APPROVAL_VIEW: &str = "permission-approval";
const MODE_VIEW: &str = "permission-mode";

/// 由通知器控制返回时机的审批模型，用于复现异步 Host 导入期间取消 Guest 调用。
struct BlockingReviewModel {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ChatModel for BlockingReviewModel {
    /// 通知测试模型已经进入，并等待测试显式释放后返回允许结论。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ModelResponse::text(r#"{"decision":"allow"}"#))
    }
}

#[async_trait]
impl ProviderAdapter for BlockingReviewModel {
    /// 返回回归测试使用的稳定 provider 名称。
    fn name(&self) -> &'static str {
        "blocking-permission-review"
    }
}

/// 构造模拟 Core/Host 已完成可信 scope 绑定的真实 ABI 调用。
fn scoped_call(
    id: &str,
    name: &str,
    args: Value,
    scope: ToolPermissionScope,
    run_id: &str,
) -> ToolCall {
    let mut call = ToolCall::new(id, name, args);
    call.bind_permission_scope(scope);
    call.bind_run_id(run_id);
    call
}

/// 把真实加载的插件装配成挂载总线、已开启一次运行快照的动态宿主。
///
/// 工具准入现在走事件总线，因此冒烟测试必须复现 Core 的真实路径：先由记录事件开启
/// 运行快照，再分发 `core.tool.call`。
async fn live_bus_with_run(composite: &CompositePluginHost, run_id: &str) -> EventBus {
    let live = LivePluginHost::new();
    let bus = EventBus::new();
    live.set_capability_owner(TOOL_POLICY_CAPABILITY, "permission")
        .expect("记录工具策略 owner");
    for host in composite.hosts() {
        live.publish(Arc::clone(host)).expect("发布真实插件宿主");
    }
    live.finish_capability_planning().expect("标记能力规划完成");
    live.attach_bus(bus.clone()).expect("挂载事件总线");
    bus.dispatch(AgentRecordEvent {
        record: AgentEvent::new(run_id, AgentEventKind::RunStarted, 0, json!({})),
    })
    .await;
    // Live Host 需要在进程内长期存活，测试结束前不释放。
    std::mem::forget(live);
    bus
}

/// 真实 component 必须阻止敏感读取，并在用户确认后放行一次写入。
#[tokio::test]
async fn wasm_host_routes_permission_approval() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("加载权限插件 WASM"),
    );
    assert_eq!(
        host.capability_owner(TOOL_POLICY_CAPABILITY),
        Some("permission")
    );
    let declarations = host.ui_declarations().await.expect("建立权限插件 UI 路由");
    assert_eq!(declarations[0].placement, UiPlacement::Input);

    let bus = live_bus_with_run(host.as_ref(), "smoke-run").await;

    let update_plan = scoped_call(
        "update-plan",
        "update_plan",
        json!({"plan": []}),
        ToolPermissionScope::Control,
        "smoke-run",
    );
    assert!(
        !bus.dispatch(ToolCallEvent::new("smoke-run", 0, update_plan))
            .await
            .is_cancelled(),
        "control scope 工具应放行"
    );

    let secret = scoped_call(
        "secret",
        "read_file",
        json!({"path": ".env"}),
        ToolPermissionScope::Read,
        "smoke-run",
    );
    let denied = bus
        .dispatch(ToolCallEvent::new("smoke-run", 0, secret))
        .await;
    assert!(denied.is_cancelled(), "敏感读取必须被拒绝");
    assert_eq!(denied.canceller(), Some("lucia.plugin-host"));

    // 本用例验证真实 UI 人工审批路由；模型代审的拒绝与异常语义由插件和 Core 分层测试覆盖。
    host.on_ui_input(&UiInput {
        plugin_id: "permission".into(),
        view_id: MODE_VIEW.into(),
        instance_id: None,
        event: UiInputEvent::MainInput {
            text: "/permission manual".into(),
            cursor: 18,
        },
    })
    .await
    .expect("输入人工审批模式");
    host.on_ui_input(&UiInput {
        plugin_id: "permission".into(),
        view_id: MODE_VIEW.into(),
        instance_id: None,
        event: UiInputEvent::Key {
            code: "enter".into(),
            modifiers: Vec::new(),
        },
    })
    .await
    .expect("切换人工审批模式");

    let write = scoped_call(
        "write",
        "write_file",
        json!({"path": "src/safe.rs", "content": ""}),
        ToolPermissionScope::Write,
        "smoke-run",
    );
    let pending_bus = bus.clone();
    let pending_decision = tokio::spawn(async move {
        pending_bus
            .dispatch(ToolCallEvent::new("smoke-run", 0, write))
            .await
    });

    let frame = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(frame) = host
                .render_ui(&UiRenderRequest {
                    plugin_id: "permission".into(),
                    view_id: APPROVAL_VIEW.into(),
                    instance_id: None,
                    width: 68,
                    height: 6,
                    focused: true,
                    frame: 1,
                })
                .await
                .expect("渲染审批 UI")
                .filter(|frame| frame.visible)
            {
                break frame;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("权限插件应在超时前展示审批界面");
    assert!(frame.visible);

    host.on_ui_input(&UiInput {
        plugin_id: "permission".into(),
        view_id: APPROVAL_VIEW.into(),
        instance_id: None,
        event: UiInputEvent::Key {
            code: "enter".into(),
            modifiers: Vec::new(),
        },
    })
    .await
    .expect("批准工具调用");

    assert!(
        !pending_decision
            .await
            .expect("审批任务不应 panic")
            .is_cancelled(),
        "用户批准后工具应放行"
    );
}

/// 外层事件 future 被取消后，已进入 Component 的调用必须自行完成，后续 UI 输入仍可进入实例。
#[tokio::test]
async fn wasm_host_keeps_component_enterable_after_cancelled_model_import() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut gateway = ModelGateway::new();
    gateway
        .register(
            "blocking-permission-review",
            Arc::new(BlockingReviewModel {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
        )
        .expect("注册阻塞审批模型");
    let services = PluginHostServices::new()
        .with_model_completion(
            gateway,
            AgentModelConfig::new(
                ModelProviderConfig::new(
                    "blocking-permission-review",
                    ProviderKind::OpenAiCompatible,
                    "smoke-key",
                ),
                "review-model",
            ),
            128,
        )
        .expect("注入阻塞审批模型");
    let host = Arc::new(
        load_wasm_plugins_with_services(&[manifest], services)
            .await
            .expect("加载权限插件 WASM"),
    );
    host.ui_declarations().await.expect("建立权限插件 UI 路由");
    let bus = live_bus_with_run(host.as_ref(), "cancelled-review-run").await;
    let call = scoped_call(
        "cancelled-review-call",
        "write_file",
        json!({"path": "src/cancelled-review.rs", "content": ""}),
        ToolPermissionScope::Write,
        "cancelled-review-run",
    );
    let dispatch = tokio::spawn(async move {
        bus.dispatch(ToolCallEvent::new("cancelled-review-run", 0, call))
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("审批模型应进入异步 Host 导入");
    dispatch.abort();
    assert!(
        dispatch
            .await
            .expect_err("事件分发任务应被取消")
            .is_cancelled(),
        "事件分发任务必须以取消状态结束"
    );
    release.notify_waiters();

    tokio::time::timeout(
        Duration::from_secs(1),
        host.on_ui_input(&UiInput {
            plugin_id: "permission".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        }),
    )
    .await
    .expect("取消后的 UI 输入不应永久等待")
    .expect("取消后的 UI 输入仍应进入同一 Component 实例");
}

/// 权限模式必须通过真实 Guest ABI 写入 Host，并在重新加载 component 后恢复。
#[tokio::test]
async fn wasm_host_restores_persisted_permission_mode() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let state_root = std::env::temp_dir().join(format!(
        "lucia-permission-smoke-state-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos()
    ));
    let services = PluginHostServices::new()
        .with_persistent_state_dir(state_root.clone())
        .expect("注入插件状态目录");
    let first = load_wasm_plugins_with_services(&[&manifest], services)
        .await
        .expect("首次加载权限插件 WASM");
    let activation_events = AgentExtension::drain_events(&first)
        .await
        .expect("读取默认权限模式底栏事件");
    assert!(activation_events.iter().any(|event| {
        event["source"]["id"] == "permission"
            && event["presentation"]["target"] == "footer"
            && event["presentation"]["text"] == "Permission: review"
    }));
    first.ui_declarations().await.expect("建立权限插件 UI 路由");
    first
        .on_ui_input(&UiInput {
            plugin_id: "permission".into(),
            view_id: MODE_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::MainInput {
                text: "/permission".into(),
                cursor: 11,
            },
        })
        .await
        .expect("打开默认权限模式面板");
    let default_frame = first
        .render_ui(&UiRenderRequest {
            plugin_id: "permission".into(),
            view_id: MODE_VIEW.into(),
            instance_id: None,
            width: 68,
            height: 4,
            focused: true,
            frame: 1,
        })
        .await
        .expect("渲染默认权限模式")
        .expect("默认权限模式应返回界面");
    let default_text = default_frame
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(
        default_text.contains("帮我审批 · 受限小模型审核  当前"),
        "{default_text}"
    );
    first
        .on_ui_input(&UiInput {
            plugin_id: "permission".into(),
            view_id: MODE_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::MainInput {
                text: "/permission allow-all".into(),
                cursor: 21,
            },
        })
        .await
        .expect("输入权限模式命令");
    first
        .on_ui_input(&UiInput {
            plugin_id: "permission".into(),
            view_id: MODE_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        })
        .await
        .expect("提交权限模式命令");
    let switched_events = AgentExtension::drain_events(&first)
        .await
        .expect("读取切换后的权限模式事件");
    assert!(switched_events.iter().any(|event| {
        event["source"]["id"] == "permission"
            && event["presentation"]["target"] == "footer"
            && event["presentation"]["text"] == "Permission: allow all"
    }));
    first.shutdown().await.expect("卸载首次权限插件");
    drop(first);

    let services = PluginHostServices::new()
        .with_persistent_state_dir(state_root.clone())
        .expect("再次注入插件状态目录");
    let reopened = load_wasm_plugins_with_services(&[&manifest], services)
        .await
        .expect("重新加载权限插件 WASM");
    let bus = live_bus_with_run(&reopened, "restored-run").await;
    let write = scoped_call(
        "restored-write",
        "write_file",
        json!({"path": "src/restored.rs", "content": ""}),
        ToolPermissionScope::Write,
        "restored-run",
    );
    let dispatched = tokio::time::timeout(
        Duration::from_secs(1),
        bus.dispatch(ToolCallEvent::new("restored-run", 0, write)),
    )
    .await
    .expect("恢复后的全部放行不应等待人工审批");
    assert!(!dispatched.is_cancelled(), "恢复的全部放行模式应直接放行");

    reopened.shutdown().await.expect("卸载重载权限插件");
    std::fs::remove_dir_all(state_root).expect("清理权限状态测试目录");
}
