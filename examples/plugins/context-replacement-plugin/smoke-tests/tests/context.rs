//! 上下文替换插件的真实 WASM 端到端测试。

use anyhow::Result;
use agent_core::{
    Agent, AgentEventKind, AgentOptions, ChatModel, InMemoryEventSink, ModelGateway, ModelRequest,
    ModelResponse, ProviderAdapter,
};
use agent_plugin_host::wasm::load_wasm_plugins;
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

/// 加载真实 component，并验证摘要替换和事件发布完整链路。
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

    agent
        .run("这条完整历史不应直接发送给模型")
        .await
        .expect("Agent 应使用插件上下文完成运行");

    let requests = model.requests.lock().expect("模型请求锁不应中毒");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 1);
    assert!(requests[0].messages[0]
        .text_content()
        .contains("测试摘要：已将"));
    drop(requests);

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.replacement.completed"
            && event.payload["presentation"]["text"] == "上下文压缩"
    }));
}
