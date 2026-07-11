use agent_core::AgentRootConfig;
use std::path::PathBuf;

/// 返回仓库内 OpenAI Responses 示例配置路径。
fn openai_responses_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/config/openai-responses.toml")
}

/// 使用示例配置真实调用 OpenAI Responses，验证 core 对话路径可解析服务商响应。
///
/// 错误：配置读取、服务商注册、网络请求或响应解析失败时返回测试错误。
#[tokio::test]
#[ignore = "需要 OPENAI_API_KEY，且会发起真实 OpenAI Responses 请求"]
async fn openai_responses_config_runs_core_agent() -> Result<(), Box<dyn std::error::Error>> {
    let config = AgentRootConfig::load(openai_responses_config_path())?;
    let mut agent = config.build_agent()?;

    agent.options_mut().max_steps = 1;
    agent.options_mut().max_tokens = Some(32);

    let run = agent
        .run("请只回复 OK 两个字母，不要添加其他内容。")
        .await?;
    assert!(
        !run.final_text.trim().is_empty(),
        "OpenAI Responses 应返回非空文本"
    );

    Ok(())
}
