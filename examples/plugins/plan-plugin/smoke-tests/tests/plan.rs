//! Plan 插件的真实 WASM 端到端测试。
//!
//! 这里走真实的 Host → component 路由，重点验证会话隔离依赖的两条可信通道：
//! 工具调用携带的 `ToolCall.session_id` 与渲染请求携带的 `UiRenderRequest.session_id`。

use agent_core::AgentExtension;
use agent_plugin_host::{
    ui::{UiFrame, UiPlacement, UiRenderRequest},
    wasm::WasmPluginHost,
    PluginHost,
};
use agent_tool::ToolCall;
use serde_json::{json, Value};
use std::path::Path;

const SHELF_VIEW: &str = "plan-shelf";
const DETAIL_VIEW: &str = "plan-detail";

/// 调用真实 component 工具并返回成功内容。
///
/// `session` 与 `run` 对应 Core 在准入前注入的可信字段；测试显式传入才能模拟
/// 两个并发会话各自持有计划。
async fn call_tool(
    host: &WasmPluginHost,
    session: Option<&str>,
    run: Option<&str>,
    name: &str,
    args: Value,
) -> Value {
    let mut call = ToolCall::new(format!("plan-smoke-{name}"), name, args);
    call.session_id = session.map(str::to_string);
    call.run_id = run.map(str::to_string);
    let result = AgentExtension::call_tool(host, call)
        .await
        .expect("插件工具路由不应失败")
        .expect("Plan 插件应处理自己的工具");
    assert!(!result.is_error, "Plan 插件返回错误：{}", result.content);
    result.content
}

/// 构造指向指定视图与会话的渲染请求。
///
/// `plan-detail` 是动态子视图，Host 要求每个请求携带实例 ID。
fn request(session: Option<&str>, view_id: &str) -> UiRenderRequest {
    UiRenderRequest {
        session_id: session.map(str::to_string),
        plugin_id: "plan".into(),
        view_id: view_id.into(),
        instance_id: (view_id == DETAIL_VIEW).then(|| "current".to_string()),
        width: 60,
        height: 10,
        focused: false,
        frame: 1,
    }
}

/// 渲染一帧并拼接全部文本，供断言检查。
async fn render_text(
    host: &WasmPluginHost,
    session: Option<&str>,
    view_id: &str,
) -> (UiFrame, String) {
    let frame = PluginHost::render_ui(host, &request(session, view_id))
        .await
        .expect("计划视图渲染不应失败")
        .expect("Plan 插件应返回帧");
    let text = frame
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.text.as_str())
        .collect();
    (frame, text)
}

/// 构造一份三步未完成计划。
fn three_step_plan(first: &str) -> Value {
    json!({
        "plan": [
            {"step": first, "status": "in_progress"},
            {"step": "实现改动", "status": "pending"},
            {"step": "补回归测试", "status": "pending"},
        ]
    })
}

/// 新会话不能继承上一个会话的计划，切回原会话仍能看到自己的进度。
#[tokio::test]
async fn component_isolates_plans_between_sessions() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = WasmPluginHost::load_from_manifest(manifest)
        .await
        .expect("Plan component 应加载成功");

    let declarations = PluginHost::ui_declarations(&host)
        .await
        .expect("计划 UI 声明应可读取");
    assert_eq!(declarations.len(), 2);
    assert_eq!(declarations[0].placement, UiPlacement::ComposerShelf);
    assert_eq!(declarations[1].placement, UiPlacement::Subview);

    // 全新宿主没有任何会话计划：进度架必须隐藏，而不是复用上一份状态。
    let (empty, empty_text) = render_text(&host, Some("session-a"), SHELF_VIEW).await;
    assert!(!empty.visible, "空会话不应显示计划架：{empty_text}");

    let updated = call_tool(
        &host,
        Some("session-a"),
        Some("run-a"),
        "update_plan",
        three_step_plan("会话 A 的第一步"),
    )
    .await;
    assert_eq!(updated["plan"].as_array().map(Vec::len), Some(3));

    // 另一个会话既读不到计划，也不会渲染出进度架。
    let other = call_tool(&host, Some("session-b"), None, "get_plan", json!({})).await;
    assert_eq!(other["plan"], json!([]), "其他会话必须读到空计划");
    let (_, other_text) = render_text(&host, Some("session-b"), SHELF_VIEW).await;
    assert!(
        !other_text.contains("会话 A 的第一步"),
        "新会话不能显示上一个会话的计划：{other_text}"
    );

    // 本会话按自己的快照渲染。
    let (own, own_text) = render_text(&host, Some("session-a"), SHELF_VIEW).await;
    assert!(own.visible);
    assert!(own_text.contains("会话 A 的第一步"), "{own_text}");
    assert!(own_text.contains("Plan  0/3 completed"), "{own_text}");

    // 会话 B 建立自己的计划后，两个会话各自独立。
    call_tool(
        &host,
        Some("session-b"),
        Some("run-b"),
        "update_plan",
        three_step_plan("会话 B 的第一步"),
    )
    .await;
    let (_, a_text) = render_text(&host, Some("session-a"), SHELF_VIEW).await;
    let (_, b_text) = render_text(&host, Some("session-b"), SHELF_VIEW).await;
    assert!(a_text.contains("会话 A 的第一步"), "{a_text}");
    assert!(!a_text.contains("会话 B 的第一步"), "{a_text}");
    assert!(b_text.contains("会话 B 的第一步"), "{b_text}");
    assert!(!b_text.contains("会话 A 的第一步"), "{b_text}");

    // 不提供会话键的旧宿主退回统一作用域，行为与单会话环境一致。
    let (_, shared_text) = render_text(&host, None, SHELF_VIEW).await;
    assert!(!shared_text.contains("会话 A 的第一步"), "{shared_text}");
    call_tool(
        &host,
        None,
        None,
        "update_plan",
        three_step_plan("共享作用域的第一步"),
    )
    .await;
    let (_, shared_text) = render_text(&host, None, SHELF_VIEW).await;
    assert!(shared_text.contains("共享作用域的第一步"), "{shared_text}");
    let (_, a_text) = render_text(&host, Some("session-a"), SHELF_VIEW).await;
    assert!(!a_text.contains("共享作用域的第一步"), "{a_text}");

    // 没有计划的会话打开详情时必须明确说明，而不是把别的会话计划当成当前计划。
    let (_, detail_text) = render_text(&host, Some("session-c"), DETAIL_VIEW).await;
    assert_eq!(detail_text.trim(), "No plan in this session");

    // 清空当前会话计划后进度架收回。
    call_tool(
        &host,
        Some("session-a"),
        None,
        "update_plan",
        json!({"plan": []}),
    )
    .await;
    let (cleared, cleared_text) = render_text(&host, Some("session-a"), SHELF_VIEW).await;
    assert!(!cleared.visible, "清空后不应显示计划架：{cleared_text}");

    host.deactivate().await.expect("插件应正常卸载");
}
