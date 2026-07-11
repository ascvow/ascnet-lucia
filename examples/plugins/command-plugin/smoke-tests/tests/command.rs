//! 官方 Command 插件的真实 WASM 端到端测试。

use agent_plugin_host::{
    ui::{UiPlacement, UiRenderRequest},
    wasm::WasmPluginHost,
    PluginHost, PluginServiceCall,
};
use command_protocol::{
    CommandSnapshot, PrepareExecuteRequest, PrepareExecuteResponse, SnapshotRequest, SurfaceAction,
    PREPARE_EXECUTE_SERVICE, PROTOCOL_VERSION, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE,
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
            action: SurfaceAction::CompactSession,
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
}
