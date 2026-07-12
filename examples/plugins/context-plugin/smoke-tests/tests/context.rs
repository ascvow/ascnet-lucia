//! 官方上下文管理插件的真实 WASM 端到端测试。

use agent_core::{
    Agent, AgentEventKind, AgentOptions, ChatModel, ContentBlock, ContextLoadRequest,
    ContextLoader, InMemoryEventSink, MessageRole, ModelGateway, ModelMessage, ModelRequest,
    ModelResponse, ProviderAdapter, Session,
};
use agent_plugin_host::wasm::load_wasm_plugins;
use agent_tool::ToolResult;
use anyhow::Result;
use async_trait::async_trait;
use std::{path::Path, sync::Arc};

/// 捕获 Agent 实际发送给模型的请求。
struct CapturingModel {
    requests: std::sync::Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ChatModel for CapturingModel {
    /// 保存请求并返回确定性文本，避免测试访问网络。
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests
            .lock()
            .expect("模型请求锁不应中毒")
            .push(request);
        Ok(ModelResponse::text("上下文插件测试完成"))
    }
}

#[async_trait]
impl ProviderAdapter for CapturingModel {
    /// 返回测试路由使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "capturing"
    }
}

/// 加载真实 component，并验证结构化摘要、近期消息保留和事件发布完整链路。
#[tokio::test]
async fn component_replaces_agent_context_and_emits_event() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("上下文替换 component 应加载成功"),
    );
    let model = Arc::new(CapturingModel {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let events = Arc::new(InMemoryEventSink::new());
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "test-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host.clone())
    .with_event_sink(events.clone());

    let recent_request = "继续处理最新请求";
    let session = Session::from_parts(
        Some("保持准确".into()),
        vec![
            ModelMessage::text(MessageRole::User, "分析上下文压缩方式"),
            ModelMessage::text(MessageRole::Assistant, "已定位上下文管理入口"),
            ModelMessage::text(MessageRole::Tool, "x".repeat(520_000)),
            ModelMessage::text(MessageRole::Assistant, "已完成旧历史分析"),
            ModelMessage::text(MessageRole::User, recent_request),
            ModelMessage::text(MessageRole::Assistant, "正在继续处理"),
        ],
    );

    agent
        .run_session(session)
        .await
        .expect("Agent 应使用插件上下文完成运行");

    let requests = model.requests.lock().expect("模型请求锁不应中毒");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.len() < 6);
    assert!(requests[0].messages[0]
        .text_content()
        .contains("用户请求与意图"));
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.text_content() == recent_request));
    drop(requests);

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.compaction.completed"
            && event.payload["presentation"]["text"] == "上下文压缩"
    }));
}

/// 验证微压缩后的工具结果仍可通过真实 WASM 宿主进入模型请求。
#[tokio::test]
async fn component_micro_compacts_tool_results_without_breaking_model_request() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("上下文微压缩 component 应加载成功"),
    );
    let model = Arc::new(CapturingModel {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let events = Arc::new(InMemoryEventSink::new());
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "test-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host.clone())
    .with_event_sink(events.clone());

    let mut messages = vec![ModelMessage::text(MessageRole::User, "分析工具执行结果")];
    for index in 0..4 {
        messages.push(ModelMessage::assistant_tool_calls(vec![agent_tool::ToolCall::new(
            format!("call-{index}"),
            "read_file",
            serde_json::json!({ "path": format!("file-{index}.txt") }),
        )]));
        messages.push(ModelMessage::tool_result(ToolResult::success(
            format!("call-{index}"),
            "read_file",
            serde_json::json!("x".repeat(100_000)),
        )));
    }
    messages.push(ModelMessage::text(MessageRole::User, "继续处理当前任务"));

    agent
        .run_session(Session::from_parts(None, messages))
        .await
        .expect("微压缩后的上下文应能完成模型请求");

    let requests = model.requests.lock().expect("模型请求锁不应中毒");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { result }
                if result.content.as_str() == Some("[旧工具结果内容已清理]"))
        })
    }));
    drop(requests);

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.micro_compaction.completed"
            && event.payload["presentation"]["text"] == "上下文微压缩"
    }));
}

/// 验证微压缩后的同一运行追加消息不会因比较大 JSON 前缀耗尽 WASM fuel。
#[tokio::test]
async fn component_reuses_micro_compacted_context_within_fuel_budget() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = load_wasm_plugins(&[manifest])
        .await
        .expect("上下文缓存 component 应加载成功");
    let mut messages = vec![ModelMessage::text(MessageRole::User, "分析工具执行结果")];
    for index in 0..4 {
        messages.push(ModelMessage::assistant_tool_calls(vec![agent_tool::ToolCall::new(
            format!("cache-call-{index}"),
            "read_file",
            serde_json::json!({ "path": format!("cache-{index}.txt") }),
        )]));
        messages.push(ModelMessage::tool_result(ToolResult::success(
            format!("cache-call-{index}"),
            "read_file",
            serde_json::json!("x".repeat(100_000)),
        )));
    }

    let first = ContextLoadRequest {
        run_id: "cached-micro-run".into(),
        step: 0,
        provider: "capturing".into(),
        model: "test-model".into(),
        system: None,
        messages: messages.clone(),
    };
    ContextLoader::load(&plugin_host, first)
        .await
        .expect("首次微压缩不应耗尽 fuel");

    messages.push(ModelMessage::text(MessageRole::User, "继续处理新消息"));
    let second = ContextLoadRequest {
        run_id: "cached-micro-run".into(),
        step: 1,
        provider: "capturing".into(),
        model: "test-model".into(),
        system: None,
        messages,
    };
    let loaded = ContextLoader::load(&plugin_host, second)
        .await
        .expect("复用微压缩前缀不应耗尽 fuel");
    assert!(loaded
        .messages
        .iter()
        .any(|message| message.text_content() == "继续处理新消息"));
}
