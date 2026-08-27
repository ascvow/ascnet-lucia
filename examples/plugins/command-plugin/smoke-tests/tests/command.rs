//! 官方 Command 插件的真实 WASM 端到端测试。

use agent_plugin_host::{
    audit::{HostServiceCallResult, InMemoryHostServiceCallObserver, JsonValueKind},
    ui::{
        UiHostAction, UiHostActionRequest, UiInput, UiInputEvent, UiPlacement, UiRenderRequest,
        UiSessionListStatus, UiSessionsReply, UI_HOST_ACTION_EVENT,
    },
    wasm::{load_wasm_plugins, WasmPluginHost},
    AgentExtension, PluginHost, PluginHostServices, PluginServiceCall,
};
use command_protocol::{
    CommandSnapshot, SnapshotRequest, PROTOCOL_VERSION, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE,
    SURFACE_UPDATE_SERVICE,
};
use serde_json::Value;
use std::{path::Path, sync::Arc};

/// 补全弹层的视图 ID，与插件声明保持一致。
const POPUP_VIEW: &str = "command-popup";

/// 真实 Command component 服务调用必须经过共享 Host 路由并产生脱敏运行期审计记录。
#[tokio::test]
async fn component_service_call_is_observed_at_host_boundary() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let observer = Arc::new(InMemoryHostServiceCallObserver::new());
    let host = WasmPluginHost::load_from_manifest_with_services(
        manifest,
        PluginHostServices::new().with_service_call_observer(observer.clone()),
    )
    .await
    .expect("Command component 应加载成功");

    let value = call_service(
        &host,
        SNAPSHOT_SERVICE,
        serde_json::to_value(SnapshotRequest {}).expect("序列化快照请求"),
    )
    .await;
    assert!(value.is_object());

    let observations = observer.snapshot();
    let observation = observations
        .iter()
        .find(|observation| observation.service == SNAPSHOT_SERVICE)
        .expect("真实服务调用必须产生 Host 审计记录");
    assert_eq!(observation.caller_id, "lucia-tui");
    assert_eq!(observation.target_owner_id, "command");
    assert_eq!(observation.method, None);
    assert_eq!(
        observation.result,
        HostServiceCallResult::Succeeded {
            value_kind: JsonValueKind::Object
        }
    );
}

/// 通过真实 Host 服务路由调用 Command component。
async fn call_service(host: &WasmPluginHost, name: &str, payload: Value) -> Value {
    host.call_service(&PluginServiceCall {
        caller_id: "lucia-tui".into(),
        plugin_id: "command".into(),
        name: name.into(),
        payload,
    })
    .await
    .expect("Command 服务路由不应失败")
    .expect("Command component 应拥有目标服务")
}

/// 向指定视图发送一次输入事件。
async fn send_input(host: &WasmPluginHost, view_id: &str, event: UiInputEvent) {
    host.on_ui_input(&UiInput {
        plugin_id: "command".into(),
        view_id: view_id.into(),
        instance_id: None,
        event,
    })
    .await
    .expect("真实 WIT 输入路由不应失败");
}

/// 取出组件排队的全部宿主动作请求。
async fn drain_host_actions(host: &WasmPluginHost) -> Vec<UiHostAction> {
    AgentExtension::drain_events(host)
        .await
        .expect("读取插件事件不应失败")
        .into_iter()
        .filter(|event| event.get("name").and_then(Value::as_str) == Some(UI_HOST_ACTION_EVENT))
        .map(|event| {
            serde_json::from_value::<UiHostActionRequest>(
                event.get("data").cloned().unwrap_or(Value::Null),
            )
            .expect("宿主动作事件应可解析")
            .action
        })
        .collect()
}

/// 加载真实 component，验证服务注册、触发弹层与会话对话框的完整交互。
#[tokio::test]
async fn component_drives_commands_through_host_actions() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = WasmPluginHost::load_from_manifest(manifest)
        .await
        .expect("Command component 应加载成功");

    let services = host.services().await.expect("读取 Command 服务目录");
    assert_eq!(services.len(), 5);
    assert!(services
        .iter()
        .all(|service| service.version == PROTOCOL_VERSION));

    let declarations = host.ui_declarations().await.expect("读取 Command UI 声明");
    let popup = declarations
        .iter()
        .find(|declaration| declaration.view_id == POPUP_VIEW)
        .expect("Command 必须声明补全弹层");
    assert_eq!(popup.placement, UiPlacement::InputPanel);
    assert_eq!(popup.input_triggers, vec!["/".to_string()]);
    let dialog = declarations
        .iter()
        .find(|declaration| declaration.view_id == SESSION_DIALOG_VIEW)
        .expect("Command 必须声明 Session Dialog");
    assert_eq!(dialog.placement, UiPlacement::Dialog);

    let snapshot_value = call_service(
        &host,
        SNAPSHOT_SERVICE,
        serde_json::to_value(SnapshotRequest {}).expect("序列化快照请求"),
    )
    .await;
    let snapshot: CommandSnapshot = serde_json::from_value(snapshot_value).expect("解析命令快照");
    for name in ["help", "compact", "resume"] {
        assert!(snapshot.commands.iter().any(|command| command.name == name));
    }

    // 键入 `/res` 后弹层应可见并展示命令用法。
    send_input(
        &host,
        POPUP_VIEW,
        UiInputEvent::MainInput {
            text: "/res".into(),
            cursor: 4,
        },
    )
    .await;
    let frame = host
        .render_ui(&UiRenderRequest {
            plugin_id: "command".into(),
            view_id: POPUP_VIEW.into(),
            instance_id: None,
            width: 80,
            height: 8,
            focused: false,
            frame: 1,
        })
        .await
        .expect("弹层渲染不应失败")
        .expect("触发激活后弹层必须返回帧");
    assert!(frame.visible);
    let text = frame
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(text.contains("/resume"), "{text}");

    // `/compact` 提交后应产生清空输入与重载上下文两个宿主动作。
    send_input(
        &host,
        POPUP_VIEW,
        UiInputEvent::MainInput {
            text: "/compact".into(),
            cursor: 8,
        },
    )
    .await;
    send_input(
        &host,
        POPUP_VIEW,
        UiInputEvent::Key {
            code: "enter".into(),
            modifiers: Vec::new(),
        },
    )
    .await;
    let actions = drain_host_actions(&host).await;
    assert!(actions
        .iter()
        .any(|action| matches!(action, UiHostAction::SetInput { text, .. } if text.is_empty())));
    assert!(actions.iter().any(|action| matches!(
        action,
        UiHostAction::ReloadContext { label: Some(label) } if label == "/compact"
    )));

    // `/resume` 打开对话框并请求会话摘要。
    send_input(
        &host,
        POPUP_VIEW,
        UiInputEvent::MainInput {
            text: "/resume".into(),
            cursor: 7,
        },
    )
    .await;
    send_input(
        &host,
        POPUP_VIEW,
        UiInputEvent::Key {
            code: "enter".into(),
            modifiers: Vec::new(),
        },
    )
    .await;
    let actions = drain_host_actions(&host).await;
    let query_id = actions
        .iter()
        .find_map(|action| match action {
            UiHostAction::QuerySessions {
                query_id,
                reply_service,
                ..
            } => {
                assert_eq!(reply_service, SURFACE_UPDATE_SERVICE);
                Some(*query_id)
            }
            _ => None,
        })
        .expect("/resume 必须请求会话摘要");

    let frame = host
        .render_ui(&UiRenderRequest {
            plugin_id: "command".into(),
            view_id: SESSION_DIALOG_VIEW.into(),
            instance_id: None,
            width: 80,
            height: 24,
            focused: true,
            frame: 2,
        })
        .await
        .expect("Command Dialog 渲染不应失败")
        .expect("打开后的 Command Dialog 必须返回帧");
    assert!(frame.visible);
    assert_eq!(frame.view_id, SESSION_DIALOG_VIEW);

    let update = call_service(
        &host,
        SURFACE_UPDATE_SERVICE,
        serde_json::to_value(UiSessionsReply {
            query_id,
            status: UiSessionListStatus::Empty,
        })
        .expect("序列化会话应答"),
    )
    .await;
    assert_eq!(update["accepted"], true);

    // Esc 关闭对话框，关闭状态由帧可见性表达。
    send_input(
        &host,
        SESSION_DIALOG_VIEW,
        UiInputEvent::Key {
            code: "escape".into(),
            modifiers: Vec::new(),
        },
    )
    .await;
    let frame = host
        .render_ui(&UiRenderRequest {
            plugin_id: "command".into(),
            view_id: SESSION_DIALOG_VIEW.into(),
            instance_id: None,
            width: 80,
            height: 24,
            focused: false,
            frame: 3,
        })
        .await
        .expect("关闭后的渲染不应失败")
        .expect("关闭后的 Dialog 仍返回帧");
    assert!(!frame.visible);

    let error = host
        .call_service(&PluginServiceCall {
            caller_id: "untrusted-plugin".into(),
            plugin_id: "command".into(),
            name: SURFACE_UPDATE_SERVICE.into(),
            payload: serde_json::to_value(UiSessionsReply {
                query_id,
                status: UiSessionListStatus::Empty,
            })
            .expect("序列化未授权更新"),
        })
        .await
        .expect_err("Host 注入的非授权调用方必须被 Command component 拒绝");
    assert!(error.to_string().contains("无权访问 Command surface"));
}

/// 验证组合宿主卸载 Command 后会同步清除服务、回调与 UI owner 路由。
#[tokio::test]
async fn component_unload_clears_all_host_routes() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let mut host = load_wasm_plugins(&[manifest])
        .await
        .expect("Command component 应加载成功");
    assert_eq!(host.ui_declarations().await.expect("读取 UI 声明").len(), 2);
    assert_eq!(host.services().await.expect("读取服务目录").len(), 5);

    let removed = host.clear();
    assert!(host
        .ui_declarations()
        .await
        .expect("刷新空 UI 路由")
        .is_empty());
    assert!(host.services().await.expect("刷新空服务目录").is_empty());
    assert!(host
        .call_service(&PluginServiceCall {
            caller_id: "lucia-tui".into(),
            plugin_id: "command".into(),
            name: SNAPSHOT_SERVICE.into(),
            payload: serde_json::json!({}),
        })
        .await
        .expect("空宿主服务调用不应失败")
        .is_none());
    assert!(host
        .render_ui(&UiRenderRequest {
            plugin_id: "command".into(),
            view_id: SESSION_DIALOG_VIEW.into(),
            instance_id: None,
            width: 80,
            height: 24,
            focused: true,
            frame: 2,
        })
        .await
        .expect("空宿主 UI 路由不应失败")
        .is_none());

    for plugin in removed {
        plugin.shutdown().await.expect("卸载钩子必须完成清理");
    }
}
