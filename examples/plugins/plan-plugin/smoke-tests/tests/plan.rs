//! Plan 插件的真实 WASM 端到端测试。
//!
//! 这里走真实的 Host → component 路由，重点验证会话隔离依赖的两条可信通道：
//! 工具调用携带的 `ToolCall.session_id` 与渲染请求携带的 `UiRenderRequest.session_id`。

use agent_core::{
    AgentEvent, AgentEventKind, AgentExtension, AgentRecordEvent, EventBus, RunFinishEvent,
};
use agent_plugin_host::{
    ui::{UiFrame, UiPlacement, UiRenderRequest},
    wasm::WasmPluginHost,
    LivePluginHost, PluginHost,
};
use agent_tool::ToolCall;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

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

/// 运行结束应通过真实事件总线收敛并回收计划，而不是把旧进度留在进度架上。
#[tokio::test]
async fn component_recycles_plan_on_run_finish() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = Arc::new(
        WasmPluginHost::load_from_manifest(manifest)
            .await
            .expect("Plan component 应加载成功"),
    );
    let live = LivePluginHost::new();
    live.publish(host.clone() as Arc<dyn PluginHost>)
        .expect("发布真实 Plan 宿主");
    live.finish_capability_planning().expect("标记能力规划完成");
    let bus = EventBus::new();
    live.attach_bus(bus.clone()).expect("挂载事件总线");

    // 打开运行快照后才能接收 core.run.finish，和 Core 的真实顺序一致。
    bus.dispatch(AgentRecordEvent {
        record: AgentEvent::new("run-a", AgentEventKind::RunStarted, 0, json!({})),
    })
    .await;
    call_tool(
        host.as_ref(),
        Some("session-a"),
        Some("run-a"),
        "update_plan",
        three_step_plan("运行 A 的第一步"),
    )
    .await;

    // 本轮更新过计划：in_progress 收敛为 completed，剩余步骤保留给后续轮次。
    let finished = bus
        .dispatch(RunFinishEvent {
            run_id: "run-a".into(),
            steps_used: 2,
            cancelled: false,
            usage: Default::default(),
            session_id: Some("session-a".into()),
        })
        .await;
    assert!(finished.errors.is_empty(), "{:?}", finished.errors);
    let converged = call_tool(
        host.as_ref(),
        Some("session-a"),
        None,
        "get_plan",
        json!({}),
    )
    .await;
    assert_eq!(converged["plan"][0]["status"], "completed");
    assert_eq!(converged["plan"][1]["status"], "pending");
    let (shelf, shelf_text) = render_text(host.as_ref(), Some("session-a"), SHELF_VIEW).await;
    assert!(shelf.visible, "{shelf_text}");

    // 下一轮没有更新计划：视为已废弃并回收，进度架不再显示遗留步骤。
    bus.dispatch(AgentRecordEvent {
        record: AgentEvent::new("run-b", AgentEventKind::RunStarted, 0, json!({})),
    })
    .await;
    let finished = bus
        .dispatch(RunFinishEvent {
            run_id: "run-b".into(),
            steps_used: 1,
            cancelled: false,
            usage: Default::default(),
            session_id: Some("session-a".into()),
        })
        .await;
    assert!(finished.errors.is_empty(), "{:?}", finished.errors);
    let cleared = call_tool(
        host.as_ref(),
        Some("session-a"),
        None,
        "get_plan",
        json!({}),
    )
    .await;
    assert_eq!(cleared["plan"], json!([]), "未维护的计划应被回收");
    let (shelf, shelf_text) = render_text(host.as_ref(), Some("session-a"), SHELF_VIEW).await;
    assert!(!shelf.visible, "回收后进度架应隐藏：{shelf_text}");

    // 取消的运行不参与收敛与回收：中断后计划仍保留，用户可继续原任务。
    bus.dispatch(AgentRecordEvent {
        record: AgentEvent::new("run-c", AgentEventKind::RunStarted, 0, json!({})),
    })
    .await;
    call_tool(
        host.as_ref(),
        Some("session-b"),
        Some("run-c"),
        "update_plan",
        three_step_plan("会话 B 的第一步"),
    )
    .await;
    bus.dispatch(RunFinishEvent {
        run_id: "run-c".into(),
        steps_used: 1,
        cancelled: true,
        usage: Default::default(),
        session_id: Some("session-b".into()),
    })
    .await;
    let kept = call_tool(
        host.as_ref(),
        Some("session-b"),
        None,
        "get_plan",
        json!({}),
    )
    .await;
    assert_eq!(
        kept["plan"][0]["status"], "in_progress",
        "取消的运行不能推进或回收计划"
    );
}
