//! Knowledge 工具的真实 WASM 组件路由测试；只使用隔离的只读会话替身。

use agent_core::{AgentExtension, MessageRole, ModelMessage, Session};
use agent_plugin_host::{
    session::{PluginSessionDescriptor, PluginSessionSource},
    ui::{ToolRenderContext, ToolRenderState, UiFrame},
    wasm::WasmPluginHost,
    LivePluginHost, PluginHost, PluginHostServices,
};
use agent_tool::{ToolCall, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// 可切换为不可用，以验证工具历史渲染不再次回读会话。
struct TestSessions {
    available: AtomicBool,
    session: Session,
}

#[async_trait]
impl PluginSessionSource for TestSessions {
    async fn list_sessions(&self) -> anyhow::Result<Vec<PluginSessionDescriptor>> {
        Ok(vec![PluginSessionDescriptor {
            id: "history-1".into(),
            title: Some("页面样式验收".into()),
            message_count: self.session.messages().len(),
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_000,
            revision: 1,
        }])
    }
    async fn load_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        Ok((self.available.load(Ordering::SeqCst) && id == "history-1")
            .then(|| self.session.clone()))
    }
}

/// 工具调用后通过 Live Host 可信 owner 路由获得帧。
async fn call_and_render(
    host: &WasmPluginHost,
    live: &LivePluginHost,
    name: &str,
    args: Value,
) -> (ToolCall, ToolResult, UiFrame) {
    let call = ToolCall::new("history-smoke", name, args);
    let result = host.call_tool(call.clone()).await.unwrap().unwrap();
    assert!(!result.is_error, "{}", result.content);
    let frame = live
        .render_tool_message(&ToolRenderContext {
            call: call.clone(),
            state: ToolRenderState::Finished {
                result: result.clone(),
            },
            width: 24,
            max_height: 256,
            frame: 1,
        })
        .await
        .unwrap()
        .expect("History 必须返回简洁组件");
    (call, result, frame)
}

fn frame_text(frame: &UiFrame) -> String {
    frame
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect()
}

/// 分页、证据提示、原文以及来源删除后的历史组件均经真实 component 验证。
#[tokio::test]
async fn history_components_preserve_evidence_and_restored_transcript() {
    let mut messages = vec![ModelMessage::text(MessageRole::User, "页面样式问题")];
    messages.extend(
        (1..=60).map(|index| ModelMessage::text(MessageRole::Assistant, format!("过程 {index}"))),
    );
    messages.push(ModelMessage::text(
        MessageRole::Assistant,
        "中文 👩‍💻 完整答复\n第二行",
    ));
    let sessions = Arc::new(TestSessions {
        available: AtomicBool::new(true),
        session: Session::from_parts(None, messages),
    });
    let state = tempfile::tempdir().unwrap();
    let services = PluginHostServices::new()
        .with_persistent_state_dir(state.path())
        .unwrap()
        .with_workspace_scope("history-ui-smoke")
        .unwrap()
        .with_session_source(sessions.clone());
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
    assert_eq!(live.tool_renderers().await.unwrap().len(), 2);

    let (_, search, frame) = call_and_render(
        host.as_ref(),
        &live,
        "history_search",
        json!({"query":"页面","future":true}),
    )
    .await;
    assert!(frame_text(&frame).starts_with("● Search(History)"));
    assert!(frame_text(&frame).contains("1 条历史线索"));
    assert!(frame_text(&frame).contains("只是历史证据"));
    let entry = search.content["results"][0]["entry_id"].as_str().unwrap();
    let (_, _, frame) = call_and_render(
        host.as_ref(),
        &live,
        "history_read",
        json!({"entry_id":entry}),
    )
    .await;
    assert!(frame_text(&frame).contains("从消息 60 继续读取"));
    let (call, result, frame) = call_and_render(
        host.as_ref(),
        &live,
        "history_read",
        json!({"entry_id":entry,"offset":60}),
    )
    .await;
    assert!(frame_text(&frame).starts_with("● Read(History)"));
    assert!(frame_text(&frame).contains("👩‍💻") && frame_text(&frame).contains("第二行"));
    sessions.available.store(false, Ordering::SeqCst);
    let restored = live
        .render_tool_message(&ToolRenderContext {
            call,
            state: ToolRenderState::Finished { result },
            width: 12,
            max_height: 256,
            frame: 2,
        })
        .await
        .unwrap()
        .unwrap();
    assert!(frame_text(&restored).contains("第二行"));
    let failure = ToolCall::new("missing", "history_read", json!({"entry_id":entry}));
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
