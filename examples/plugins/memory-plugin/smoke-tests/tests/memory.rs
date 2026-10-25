//! 真实 Memory WASM 的工具调用与 UI owner 路由验证；不连接模型或用户存储。

use agent_core::AgentExtension;
use agent_plugin_host::{
    ui::{ToolRenderContext, ToolRenderState, UiFrame},
    wasm::WasmPluginHost,
    LivePluginHost, PluginHost, PluginHostServices,
};
use agent_tool::{ToolCall, ToolResult};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

/// 通过真实工具路由执行，再让 Live Host 按贡献 owner 选择 renderer。
async fn call_and_render(
    host: &WasmPluginHost,
    live: &LivePluginHost,
    name: &str,
    args: Value,
) -> (ToolCall, ToolResult, UiFrame) {
    let call = ToolCall::new("memory-smoke", name, args);
    let result = host.call_tool(call.clone()).await.unwrap().unwrap();
    assert!(!result.is_error, "{}", result.content);
    let frame = live
        .render_tool_message(&ToolRenderContext {
            call: call.clone(),
            state: ToolRenderState::Finished {
                result: result.clone(),
            },
            width: 40,
            max_height: 256,
            frame: 1,
        })
        .await
        .unwrap()
        .expect("Memory 必须返回简洁组件");
    assert!(!frame.lines[0]
        .spans
        .iter()
        .any(|span| span.text.contains(name)));
    (call, result, frame)
}

/// 文本断言只读帧，不借助插件内部状态。
fn frame_text(frame: &UiFrame) -> String {
    frame
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect()
}

/// 记录、去重、列表、删除与删除后的历史帧都走真实 WASM ABI。
#[tokio::test]
async fn memory_components_survive_record_list_forget_flow() {
    let state = tempfile::tempdir().unwrap();
    let services = PluginHostServices::new()
        .with_persistent_state_dir(state.path())
        .unwrap()
        .with_workspace_scope("memory-ui-smoke")
        .unwrap();
    let host = Arc::new(
        WasmPluginHost::load_from_manifest_with_services(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml"),
            services,
        )
        .await
        .unwrap(),
    );
    let live = LivePluginHost::new();
    live.publish(host.clone() as Arc<dyn PluginHost>).unwrap();
    live.ui_declarations().await.unwrap();
    live.finish_capability_planning().unwrap();
    assert_eq!(live.tool_renderers().await.unwrap().len(), 3);

    let text = "中文 👩‍💻 记忆正文\n第二行 </memory>&";
    let (call, record, frame) = call_and_render(
        host.as_ref(),
        &live,
        "memory_record",
        json!({"text":text,"scope":"project","future":true}),
    )
    .await;
    assert!(frame_text(&frame).contains("● Record(Memory)"));
    assert!(frame_text(&frame).contains(text.split('\n').next().unwrap()));
    assert!(frame_text(&frame).contains("第二行"));
    let prompts = host.prompt_messages().await.unwrap();
    let prompt = prompts[0].text_content();
    assert!(prompt.starts_with("<memories>"));
    assert!(prompt.ends_with("</memories>"));
    assert!(prompt.contains("第二行 &lt;/memory&gt;&amp;"));
    assert_eq!(prompt.matches("</memory>").count(), 1);
    assert!(host
        .list_tools()
        .await
        .unwrap()
        .iter()
        .all(|tool| tool.description.starts_with("<tool_usage>")));
    let id = record.content["id"].as_str().unwrap().to_string();
    let (_, repeated, frame) = call_and_render(
        host.as_ref(),
        &live,
        "memory_record",
        json!({"text":text,"scope":"project"}),
    )
    .await;
    assert_eq!(repeated.content["status"], "already_recorded");
    assert!(frame_text(&frame).contains("未重复记录"));
    let (_, _, frame) = call_and_render(host.as_ref(), &live, "memory_list", json!({})).await;
    assert!(
        frame_text(&frame).contains("● List(Memory)") && frame_text(&frame).contains("1 条记忆")
    );
    let (_, _, frame) =
        call_and_render(host.as_ref(), &live, "memory_forget", json!({"id":id})).await;
    assert!(frame_text(&frame).contains("● Forget(Memory)"));
    let (_, _, frame) = call_and_render(host.as_ref(), &live, "memory_list", json!({})).await;
    assert!(frame_text(&frame).contains("0 条记忆"));

    let restored = live
        .render_tool_message(&ToolRenderContext {
            call,
            state: ToolRenderState::Finished { result: record },
            width: 16,
            max_height: 256,
            frame: 2,
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        frame_text(&restored).contains("第二行"),
        "历史不能依赖已删除记忆"
    );
    let failure = ToolCall::new("missing", "memory_forget", json!({"id":id}));
    let result = host.call_tool(failure.clone()).await.unwrap().unwrap();
    assert!(result.is_error);
    assert!(live
        .render_tool_message(&ToolRenderContext {
            call: failure,
            state: ToolRenderState::Finished { result },
            width: 40,
            max_height: 256,
            frame: 3
        })
        .await
        .unwrap()
        .is_none());
}
