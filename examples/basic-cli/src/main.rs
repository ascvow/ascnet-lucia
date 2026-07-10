//! Tiny ascnet-lucia CLI.
//! ascnet-lucia 极简 CLI。

use agent_core::{
    config::LuciaConfig,
    model::{
        ChatModel, ContentBlock, FinishReason, MessageRole, ModelGateway, ModelRequest,
        ModelResponse, ProviderAdapter,
    },
    Agent, AgentOptions, JsonlEventSink,
};
use agent_plugin_host::{manifest::load_plugin_manifest_paths, wasm::load_wasm_plugins};
use agent_tool::{JsonTool, ToolCall, ToolRegistry, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clap::Parser;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

/// Run a minimal ReAct agent.
/// 运行一个最小 ReAct agent。
#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Use the built-in scripted model instead of a real provider.
    /// 使用内置脚本模型，而不是调用真实服务商。
    #[arg(long)]
    demo: bool,

    /// Path to ascnet-lucia TOML config for real OpenAI / Anthropic calls.
    /// 真实 OpenAI / Anthropic 调用所用的 ascnet-lucia TOML 配置路径。
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to plugin.toml. Can be repeated.
    /// plugin.toml 路径。可以重复传入。
    #[arg(long = "plugin-manifest")]
    plugin_manifests: Vec<PathBuf>,

    /// Optional JSONL file for agent events.
    /// 可选的 agent 事件 JSONL 输出文件。
    #[arg(long = "events-jsonl")]
    events_jsonl: Option<PathBuf>,

    /// User input.
    /// 用户输入。
    input: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let mut plugin_manifests = args.plugin_manifests.clone();
    let (gateway, options) = if args.demo {
        build_demo_gateway()
    } else if let Some(path) = &args.config {
        let config = LuciaConfig::load(path)?;
        plugin_manifests.extend(load_plugin_manifest_paths(path)?);
        (config.build_gateway()?, config.agent_options())
    } else {
        return Err(anyhow!(
            "pass --demo for the scripted model or --config <file> for a real provider"
        ));
    };

    let mut native_tools = ToolRegistry::new();
    if args.demo && plugin_manifests.is_empty() {
        // Native fallback keeps `--demo` runnable even before building the WASM plugin.
        // 原生 fallback 让 `--demo` 在尚未构建 WASM 插件时也能运行。
        native_tools.register(JsonTool::new(echo_spec(), |args| async move {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(json!({ "echo": text, "source": "native-fallback" }))
        }))?;
    }

    let plugin_host = Arc::new(load_wasm_plugins(&plugin_manifests).await?);
    let mut agent = Agent::new(gateway, options)
        .with_tools(native_tools)
        .with_extension(plugin_host);

    if let Some(path) = args.events_jsonl {
        agent = agent.with_event_sink(Arc::new(JsonlEventSink::new(path)));
    }

    let run = agent.run(args.input).await?;
    println!("{}", run.final_text);
    Ok(())
}

fn build_demo_gateway() -> (ModelGateway, AgentOptions) {
    let mut gateway = ModelGateway::new();
    gateway
        .register("default", Arc::new(ScriptedReActModel))
        .expect("register scripted model");

    let options = AgentOptions {
        provider: "default".to_string(),
        model: "scripted-react-demo".to_string(),
        ..AgentOptions::default()
    };

    (gateway, options)
}

fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo",
        "Echo the input text. / 回显输入文本。",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo. / 要回显的文本。" }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    )
}

/// A deterministic model used to demonstrate the ReAct loop without network calls.
/// 一个确定性的模型，用于在不联网的情况下演示 ReAct loop。
struct ScriptedReActModel;

#[async_trait]
impl ChatModel for ScriptedReActModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        if let Some(tool_text) = latest_tool_result_text(&req) {
            return Ok(ModelResponse {
                content: vec![ContentBlock::Text {
                    text: format!("工具返回 / Tool returned: {tool_text}"),
                }],
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: None,
                billing: None,
                raw_provider_response: None,
            });
        }

        let user_text = latest_user_text(&req).unwrap_or_default();
        let has_echo = req.tools.iter().any(|tool| tool.name == "echo");
        if has_echo {
            Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                "demo-call-1",
                "echo",
                json!({ "text": user_text }),
            )]))
        } else {
            Ok(ModelResponse::text(format!(
                "没有可用工具 / No tools available. User said: {user_text}"
            )))
        }
    }
}

#[async_trait]
impl ProviderAdapter for ScriptedReActModel {
    fn name(&self) -> &'static str {
        "scripted-react-demo"
    }
}

fn latest_user_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|message| matches!(&message.role, MessageRole::User))
        .map(|message| message.text_content())
}

fn latest_tool_result_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|message| matches!(&message.role, MessageRole::Tool))
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { result } => Some(result.content_text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
}
