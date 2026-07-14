//! 官方上下文管理插件的真实 WASM 端到端测试。

use agent_core::{
    model::ModelEventStream, Agent, AgentEventKind, AgentOptions, ChatModel, ContentBlock,
    ContextLoadRequest, ContextLoader, InMemoryEventSink, MessageRole, ModelGateway, ModelMessage,
    ModelRequest, ModelResponse, ProviderAdapter, Session, TokenUsage, ToolChoice,
};
use agent_plugin_host::{
    wasm::{load_wasm_plugins, load_wasm_plugins_with_services},
    PluginHostServices,
};
use agent_tool::ToolResult;
use anyhow::Result;
use async_trait::async_trait;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

/// 捕获 Agent 实际发送给模型的请求。
#[derive(Default)]
struct CapturingModel {
    requests: std::sync::Mutex<Vec<ModelRequest>>,
    complete_calls: AtomicUsize,
    stream_calls: AtomicUsize,
}

/// 在摘要请求中返回带根因错误的测试模型。
struct FailingSummaryModel;

impl CapturingModel {
    /// 根据请求类型构造摘要或主 Agent 的确定性响应。
    fn response_for(request: &ModelRequest) -> ModelResponse {
        let is_summary = request
            .system
            .as_deref()
            .is_some_and(|system| system.contains("负责压缩长对话"));
        let mut response = if is_summary {
            ModelResponse::text(
                "<analysis>不应进入主上下文</analysis><summary>模型生成摘要：已分析旧上下文并保留关键状态。</summary>",
            )
        } else {
            ModelResponse::text("上下文插件测试完成")
        };
        response.usage = Some(TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            total_tokens: Some(120),
        });
        response
    }

    /// 保存请求，供测试检查摘要与主 Agent 的调用顺序。
    fn record(&self, request: ModelRequest) {
        self.requests
            .lock()
            .expect("模型请求锁不应中毒")
            .push(request);
    }
}

#[async_trait]
impl ChatModel for CapturingModel {
    /// 保存请求并返回确定性文本，避免测试访问网络。
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        let response = Self::response_for(&request);
        self.record(request);
        Ok(response)
    }

    /// 保存流式请求，并返回只包含终态的测试事件流。
    async fn stream(&self, request: ModelRequest) -> ModelEventStream {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let response = Self::response_for(&request);
        self.record(request);
        let (sender, stream) = ModelEventStream::channel();
        sender.done(response);
        stream
    }
}

#[async_trait]
impl ProviderAdapter for CapturingModel {
    /// 返回测试路由使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "capturing"
    }
}

#[async_trait]
impl ChatModel for FailingSummaryModel {
    /// 模拟第三方模型网关拒绝摘要请求。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(anyhow::anyhow!("上游模型拒绝摘要请求"))
    }
}

#[async_trait]
impl ProviderAdapter for FailingSummaryModel {
    /// 返回失败模型使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "failing-summary"
    }
}

/// 加载真实 component，并验证结构化摘要、近期消息保留和事件发布完整链路。
#[tokio::test]
async fn component_replaces_agent_context_and_emits_event() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let model = Arc::new(CapturingModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let services = PluginHostServices::new()
        .with_model_completion(
            gateway.clone(),
            "capturing",
            "trusted-summary-model",
            20_000,
            false,
        )
        .expect("Host 应接受固定模型完成服务");
    let plugin_host = Arc::new(
        load_wasm_plugins_with_services(&[manifest], services)
            .await
            .expect("上下文替换 component 应加载成功"),
    );
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

    {
        let requests = model.requests.lock().expect("模型请求锁不应中毒");
        assert_eq!(requests.len(), 2);
        assert_eq!(model.complete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.stream_calls.load(Ordering::SeqCst), 1);
        let summary_request = &requests[0];
        assert_eq!(summary_request.model, "trusted-summary-model");
        assert_eq!(summary_request.tool_choice, ToolChoice::None);
        assert!(summary_request.tools.is_empty());
        assert_eq!(summary_request.max_tokens, Some(20_000));
        assert!(summary_request
            .messages
            .last()
            .is_some_and(|message| message.text_content().contains("尚未完成的任务")));

        let main_request = &requests[1];
        assert_eq!(main_request.model, "test-model");
        assert!(main_request.messages.len() < 6);
        assert!(main_request.messages[0]
            .text_content()
            .contains("模型生成摘要"));
        assert!(!main_request.messages[0]
            .text_content()
            .contains("不应进入主上下文"));
        assert!(main_request
            .messages
            .iter()
            .any(|message| message.text_content() == recent_request));
    }

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.compaction.completed"
            && event.payload["presentation"]["text"] == "上下文压缩"
    }));
}

/// 摘要模型失败时，WASM 双边信封必须把底层 provider 原因传回 Core。
#[tokio::test]
async fn component_preserves_summary_model_error_chain() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let mut gateway = ModelGateway::new();
    gateway
        .register("failing", Arc::new(FailingSummaryModel))
        .expect("失败模型应注册成功");
    let services = PluginHostServices::new()
        .with_model_completion(gateway, "failing", "failing-model", 20_000, false)
        .expect("Host 应接受失败模型服务");
    let plugin_host = load_wasm_plugins_with_services(&[manifest], services)
        .await
        .expect("上下文 component 应加载成功");
    let session = Session::from_parts(
        None,
        vec![
            ModelMessage::text(MessageRole::User, "分析旧上下文"),
            ModelMessage::text(MessageRole::Assistant, "开始分析"),
            ModelMessage::text(MessageRole::Tool, "x".repeat(520_000)),
            ModelMessage::text(MessageRole::Assistant, "继续处理"),
            ModelMessage::text(MessageRole::User, "保留当前请求"),
        ],
    );
    let request = ContextLoadRequest {
        run_id: "failing-summary-run".into(),
        step: 0,
        provider: "failing".into(),
        model: "failing-model".into(),
        system: None,
        messages: session.model_messages(),
    };

    let error = ContextLoader::load(&plugin_host, request)
        .await
        .expect_err("摘要模型失败必须终止上下文加载");
    let error = format!("{error:#}");
    assert!(error.contains("调用模型生成上下文摘要失败"));
    assert!(error.contains("上游模型拒绝摘要请求"));
}

/// 短会话低于压缩水位时插件返回透传，Agent 必须继续使用原始上下文而不是报错。
#[tokio::test]
async fn component_passes_through_short_context_without_error() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("上下文透传 component 应加载成功"),
    );
    let model = Arc::new(CapturingModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "test-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host.clone());

    let session = Session::from_parts(
        Some("保持准确".into()),
        vec![ModelMessage::text(MessageRole::User, "你好")],
    );
    agent
        .run_session(session)
        .await
        .expect("短会话应透传原始上下文完成运行");

    let requests = model.requests.lock().expect("模型请求锁不应中毒");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].system.as_deref(), Some("保持准确"));
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[0].messages[0].text_content(), "你好");
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
    let model = Arc::new(CapturingModel::default());
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
        messages.push(ModelMessage::assistant_tool_calls(vec![
            agent_tool::ToolCall::new(
                format!("call-{index}"),
                "read_file",
                serde_json::json!({ "path": format!("file-{index}.txt") }),
            ),
        ]));
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

    {
        let requests = model.requests.lock().expect("模型请求锁不应中毒");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { result }
                    if result.content.as_str() == Some("[旧工具结果内容已清理]"))
            })
        }));
    }

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.micro_compaction.completed"
            && event.payload["presentation"].is_null()
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
        messages.push(ModelMessage::assistant_tool_calls(vec![
            agent_tool::ToolCall::new(
                format!("cache-call-{index}"),
                "read_file",
                serde_json::json!({ "path": format!("cache-{index}.txt") }),
            ),
        ]));
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
