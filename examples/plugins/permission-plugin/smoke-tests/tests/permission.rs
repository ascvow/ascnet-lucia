//! 权限插件真实 WASM Host 路由测试。

use agent_core::{AgentExtension, ToolDecision};
use agent_plugin_host::{
    manifest::TOOL_POLICY_CAPABILITY,
    ui::{UiInput, UiInputEvent, UiPlacement, UiRenderRequest},
    wasm::{load_wasm_plugins, load_wasm_plugins_with_services},
    PluginHost, PluginHostServices,
};
use agent_tool::ToolCall;
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};

const APPROVAL_VIEW: &str = "permission-approval";
const MODE_VIEW: &str = "permission-mode";

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

    let secret = ToolCall::new("secret", "read_file", json!({"path": ".env"}));
    assert!(matches!(
        AgentExtension::before_tool(host.as_ref(), &secret)
            .await
            .expect("检查敏感读取"),
        ToolDecision::Block { .. }
    ));

    let write = ToolCall::new(
        "write",
        "write_file",
        json!({"path": "src/safe.rs", "content": ""}),
    );
    let pending_host = Arc::clone(&host);
    let pending_write = write.clone();
    let pending_decision = tokio::spawn(async move {
        AgentExtension::before_tool(pending_host.as_ref(), &pending_write).await
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

    assert_eq!(
        pending_decision
            .await
            .expect("审批任务不应 panic")
            .expect("读取审批结果"),
        ToolDecision::Allow
    );
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
    first.ui_declarations().await.expect("建立权限插件 UI 路由");
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
    first.shutdown().await.expect("卸载首次权限插件");
    drop(first);

    let services = PluginHostServices::new()
        .with_persistent_state_dir(state_root.clone())
        .expect("再次注入插件状态目录");
    let reopened = load_wasm_plugins_with_services(&[&manifest], services)
        .await
        .expect("重新加载权限插件 WASM");
    let write = ToolCall::new(
        "restored-write",
        "write_file",
        json!({"path": "src/restored.rs", "content": ""}),
    );
    let decision = tokio::time::timeout(
        Duration::from_secs(1),
        AgentExtension::before_tool(&reopened, &write),
    )
    .await
    .expect("恢复后的全部放行不应等待人工审批")
    .expect("读取恢复后的权限决策");
    assert_eq!(decision, ToolDecision::Allow);

    reopened.shutdown().await.expect("卸载重载权限插件");
    std::fs::remove_dir_all(state_root).expect("清理权限状态测试目录");
}
