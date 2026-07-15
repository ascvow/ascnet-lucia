//! Sandbox 插件真实 WASM Host 路由测试。

use agent_core::{AgentExtension, ToolDecision};
use agent_plugin_host::{
    manifest::TOOL_POLICY_CAPABILITY,
    ui::{UiInput, UiInputEvent, UiPlacement, UiRenderRequest},
    wasm::load_wasm_plugins,
    PluginHost,
};
use agent_tool::ToolCall;
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};

const APPROVAL_VIEW: &str = "sandbox-approval";

/// 真实 component 必须阻止敏感读取，并在用户确认后放行一次写入。
#[tokio::test]
async fn wasm_host_routes_sandbox_approval() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("加载 Sandbox WASM 插件"),
    );
    assert_eq!(
        host.capability_owner(TOOL_POLICY_CAPABILITY),
        Some("sandbox")
    );
    let declarations = host.ui_declarations().await.expect("建立 Sandbox UI 路由");
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
                    plugin_id: "sandbox".into(),
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
    .expect("Sandbox 应在超时前展示审批界面");
    assert!(frame.visible);

    host.on_ui_input(&UiInput {
        plugin_id: "sandbox".into(),
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
