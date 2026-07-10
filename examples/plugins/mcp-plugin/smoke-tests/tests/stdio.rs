//! 通用 MCP 插件的 stdio 端到端测试。

use agent_core::AgentExtension;
use agent_plugin_host::wasm::WasmPluginHost;
use agent_tool::ToolCall;
use serde_json::json;
use std::path::Path;

/// 加载 MCP component，并通过假 stdio Server 验证发现和执行链路。
#[tokio::test]
#[ignore = "需要先构建上级 mcp-plugin 的 wasm32-wasip2 release component"]
async fn component_discovers_and_calls_stdio_tool() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.test.toml");
    let host = WasmPluginHost::load_from_manifest(manifest)
        .await
        .expect("MCP component 应加载成功");
    let tools = host.list_tools().await.expect("读取动态工具应成功");
    let tool = tools
        .iter()
        .find(|tool| tool.name == "mcp__mock__get_design_node")
        .expect("假 MCP 工具应完成动态注册");
    assert_eq!(tool.description, "读取测试原型中的设计节点。");

    let result = host
        .call_tool(ToolCall::new(
            "call-1",
            "mcp__mock__get_design_node",
            json!({"nodeId": "node-42"}),
        ))
        .await
        .expect("工具路由不应失败")
        .expect("MCP 插件应处理自己的工具");
    assert!(!result.is_error);
    let text = result
        .content
        .pointer("/content/0/text")
        .and_then(serde_json::Value::as_str)
        .expect("假 MCP 应返回文本内容");
    assert!(text.contains("node-42"));

    let prompts = host.prompt_messages().await.expect("读取提示贡献应成功");
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].text_content().contains("mock"));
}
