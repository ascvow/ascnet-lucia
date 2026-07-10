//! Lucia 官方 Skill 插件的真实 WASM 端到端测试。

use agent_core::AgentExtension;
use agent_plugin_host::wasm::WasmPluginHost;
use agent_tool::ToolCall;
use serde_json::json;
use std::path::Path;

/// 加载 Skill component，验证描述注入和完整指令按需读取链路。
#[tokio::test]
#[ignore = "需要先构建上级 skill-plugin 的 wasm32-wasip2 release component"]
async fn component_discovers_and_reads_skill() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = WasmPluginHost::load_from_manifest(manifest)
        .await
        .expect("Skill component 应加载成功");

    let prompts = host.prompt_messages().await.expect("读取 Skill 提示贡献");
    assert_eq!(prompts.len(), 1);
    let prompt = prompts[0].text_content();
    assert!(prompt.contains("lucia-plugin-development"));
    assert!(prompt.contains("开发或修改 Lucia WASM 插件"));
    assert!(!prompt.contains("保持 Agent Core"));

    let tools = host.list_tools().await.expect("读取 Skill 工具");
    let tool = tools
        .iter()
        .find(|tool| tool.name == "skill_read")
        .expect("Skill 读取工具应完成动态注册");
    assert!(tool.description.contains("完整指令"));

    let result = host
        .call_tool(ToolCall::new(
            "skill-call-1",
            "skill_read",
            json!({"name": "lucia-plugin-development"}),
        ))
        .await
        .expect("Skill 工具路由不应失败")
        .expect("Skill 插件应处理读取工具");
    assert!(!result.is_error);
    assert!(result.content["content"]
        .as_str()
        .expect("Skill 内容应为字符串")
        .contains("保持 Agent Core"));
}
