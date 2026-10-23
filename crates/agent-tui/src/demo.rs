//! 不联网的确定性演示模型，用于 `--demo` 与缺少密钥时的兜底运行。

use agent_core::{
    model::{
        ChatModel, ContentBlock, MessageRole, ModelGateway, ModelRequest, ModelResponse,
        ProviderAdapter,
    },
    AgentOptions,
};
use agent_tool::{ToolCall, ToolPermissionScope, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// 构建无需外部模型服务的确定性演示运行时。
pub(crate) fn build_demo_gateway() -> (ModelGateway, AgentOptions) {
    let gateway = ModelGateway::new();
    gateway
        .register("default", Arc::new(ScriptedReActModel))
        .expect("注册脚本模型");
    let options = AgentOptions {
        provider: "default".to_string(),
        model: "scripted-react-demo".to_string(),
        max_steps: 0,
        ..AgentOptions::default()
    };
    (gateway, options)
}

pub(crate) fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo",
        "回显输入文本。",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "要回显的文本" }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 确定性脚本模型，不联网即可演示 ReAct loop。
struct ScriptedReActModel;

#[async_trait]
impl ChatModel for ScriptedReActModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        if let Some(tool_text) = latest_tool_result_text(&req) {
            return Ok(ModelResponse::text(format!("工具返回: {tool_text}")));
        }
        let user_text = latest_user_text(&req).unwrap_or_default();
        if req.tools.iter().any(|t| t.name == "echo") {
            Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                "demo-call-1",
                "echo",
                json!({ "text": user_text }),
            )]))
        } else {
            Ok(ModelResponse::text(format!(
                "没有可用工具。用户说: {user_text}"
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
        .find(|m| matches!(&m.role, MessageRole::User))
        .map(|m| m.text_content())
}

fn latest_tool_result_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(&m.role, MessageRole::Tool))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { result } => Some(result.content_text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
}
