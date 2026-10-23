use agent_core::AgentRootConfig;
use std::path::PathBuf;

/// 返回仓库内 OpenAI Responses 示例配置路径。
fn openai_responses_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/config/openai-responses.json")
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

/// 使用显式指定的配置验证默认短期缓存在真实 Agent 续聊中的命中。
///
/// `LUCIA_CACHE_TEST_CONFIG` 指向含模型凭据的 JSON；只发送合成文本，不加载插件或工作区。
/// 最多发送四次付费请求，每次限制输出为 256 token，并等待服务端缓存建立。
/// 配置、网络、响应失败或服务端始终未报告缓存读取时返回错误；日志仅输出用量。
#[tokio::test]
#[ignore = "需要 LUCIA_CACHE_TEST_CONFIG，且最多发起四次真实付费请求"]
async fn compatible_responses_short_cache_hits_across_agent_turns(
) -> Result<(), Box<dyn std::error::Error>> {
    use agent_core::model::{GatewayKind, PromptCacheRetention};
    use std::time::Duration;

    let config = AgentRootConfig::load(std::env::var("LUCIA_CACHE_TEST_CONFIG")?)?;
    let mut agent = config.build_agent()?;
    let options = agent.options_mut();
    options.max_steps = 1;
    options.max_tokens = Some(256);
    options.stream = true;
    options.cache_retention = PromptCacheRetention::Short;
    options.provider_options = serde_json::json!({});
    options.system_prompt = (0..240)
        .map(|index| format!("Record {index}: This is synthetic cache verification data. Preserve the common prefix and reply only OK.\n"))
        .collect();
    assert_eq!(
        config.default_upstream()?.gateway,
        GatewayKind::OpenAiCompatible
    );
    agent.set_session_id(Some(uuid::Uuid::new_v4().to_string()));

    let mut session = None;
    for turn in 1..=4 {
        if turn > 1 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        let run = tokio::time::timeout(Duration::from_secs(90), async {
            match session.take() {
                Some(session) => agent.run_continue(session, "Reply only OK.").await,
                None => agent.run("Reply only OK.").await,
            }
        })
        .await??;
        println!("turn={turn} usage={:?}", run.usage);
        assert!(!run.final_text.trim().is_empty(), "模型应返回可见文本");
        if turn > 1 && run.usage.cache_read_tokens.unwrap_or(0) > 0 {
            return Ok(());
        }
        session = Some(run.session);
    }
    Err("四轮请求均未报告缓存读取；需检查网关转发、上游缓存支持与 usage 统计".into())
}
