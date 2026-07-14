//! 官方 Command 插件的真实 WASM 端到端测试。

use agent_plugin_host::{
    ui::{UiInput, UiInputEvent, UiPlacement, UiRenderRequest},
    wasm::{load_wasm_plugins, WasmPluginHost},
    PluginHost, PluginServiceCall,
};
use command_protocol::{
    CommandSnapshot, PrepareExecuteRequest, PrepareExecuteResponse, SessionListStatus,
    SnapshotRequest, SurfaceAction, SurfaceEffect, SurfaceEffectsResponse, SurfaceUpdateRequest,
    PREPARE_EXECUTE_SERVICE, PROTOCOL_VERSION, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE,
    SURFACE_POLL_EFFECTS_SERVICE, SURFACE_UPDATE_SERVICE,
};
use serde_json::Value;
use std::path::Path;

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

/// 加载真实 component，验证服务注册、内置命令计划和声明式 Dialog 渲染。
#[tokio::test]
async fn component_routes_builtin_commands_and_dialog() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = WasmPluginHost::load_from_manifest(manifest)
        .await
        .expect("Command component 应加载成功");

    let services = host.services().await.expect("读取 Command 服务目录");
    assert_eq!(services.len(), 7);
    assert!(services
        .iter()
        .all(|service| service.version == PROTOCOL_VERSION));

    let declarations = host.ui_declarations().await.expect("读取 Command UI 声明");
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

    let help_value = call_service(
        &host,
        PREPARE_EXECUTE_SERVICE,
        serde_json::to_value(PrepareExecuteRequest {
            input: "/help".into(),
            agent_idle: true,
        })
        .expect("序列化帮助命令请求"),
    )
    .await;
    let help: PrepareExecuteResponse =
        serde_json::from_value(help_value).expect("解析帮助命令响应");
    assert!(matches!(
        help,
        PrepareExecuteResponse::Output { content } if content.contains("/help")
    ));

    let compact_value = call_service(
        &host,
        PREPARE_EXECUTE_SERVICE,
        serde_json::to_value(PrepareExecuteRequest {
            input: "/compact".into(),
            agent_idle: true,
        })
        .expect("序列化压缩命令请求"),
    )
    .await;
    let compact: PrepareExecuteResponse =
        serde_json::from_value(compact_value).expect("解析压缩命令响应");
    assert_eq!(
        compact,
        PrepareExecuteResponse::SurfaceAction {
            action: SurfaceAction::ReloadSessionContext,
        }
    );

    let resume_value = call_service(
        &host,
        PREPARE_EXECUTE_SERVICE,
        serde_json::to_value(PrepareExecuteRequest {
            input: "/resume".into(),
            agent_idle: true,
        })
        .expect("序列化恢复命令请求"),
    )
    .await;
    let resume: PrepareExecuteResponse =
        serde_json::from_value(resume_value).expect("解析恢复命令响应");
    assert_eq!(
        resume,
        PrepareExecuteResponse::SurfaceOpened {
            view_id: SESSION_DIALOG_VIEW.into(),
        }
    );

    let frame = host
        .render_ui(&UiRenderRequest {
            plugin_id: "command".into(),
            view_id: SESSION_DIALOG_VIEW.into(),
            instance_id: None,
            width: 80,
            height: 24,
            focused: true,
            frame: 1,
        })
        .await
        .expect("Command Dialog 渲染不应失败")
        .expect("打开后的 Command Dialog 必须返回帧");
    assert!(frame.visible);
    assert_eq!(frame.view_id, SESSION_DIALOG_VIEW);

    let effects_value =
        call_service(&host, SURFACE_POLL_EFFECTS_SERVICE, serde_json::json!({})).await;
    let effects: SurfaceEffectsResponse =
        serde_json::from_value(effects_value).expect("解析 surface effect");
    let request_id = effects
        .effects
        .iter()
        .find_map(|effect| match effect {
            SurfaceEffect::QuerySessions { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("/resume 必须请求会话摘要");

    let update = call_service(
        &host,
        SURFACE_UPDATE_SERVICE,
        serde_json::to_value(SurfaceUpdateRequest {
            request_id,
            status: SessionListStatus::Empty,
        })
        .expect("序列化 surface 更新"),
    )
    .await;
    assert_eq!(update["accepted"], true);

    host.on_ui_input(&UiInput {
        plugin_id: "command".into(),
        view_id: SESSION_DIALOG_VIEW.into(),
        instance_id: None,
        event: UiInputEvent::Key {
            code: "escape".into(),
            modifiers: Vec::new(),
        },
    })
    .await
    .expect("真实 WIT 输入路由不应失败");
    let effects_value =
        call_service(&host, SURFACE_POLL_EFFECTS_SERVICE, serde_json::json!({})).await;
    let effects: SurfaceEffectsResponse =
        serde_json::from_value(effects_value).expect("解析关闭 effect");
    assert_eq!(effects.effects, vec![SurfaceEffect::CloseSurface]);

    let error = host
        .call_service(&PluginServiceCall {
            caller_id: "untrusted-plugin".into(),
            plugin_id: "command".into(),
            name: SURFACE_UPDATE_SERVICE.into(),
            payload: serde_json::to_value(SurfaceUpdateRequest {
                request_id,
                status: SessionListStatus::Empty,
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
    assert_eq!(host.ui_declarations().await.expect("读取 UI 声明").len(), 1);
    assert_eq!(host.services().await.expect("读取服务目录").len(), 7);

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
