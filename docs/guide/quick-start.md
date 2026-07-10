# 五分钟接入

## 环境

- Rust toolchain 以仓库的 `rust-toolchain.toml` 为准。
- JavaScript、VitePress 和辅助脚本统一使用 Bun。
- WASM 插件需要 `wasm32-wasip2` target。

```bash
rustup target add wasm32-wasip2
cargo check --workspace
```

## 创建 Agent

当前仓库内嵌时使用 path dependency：

```toml
[dependencies]
anyhow = "1"
agent-core = { path = "../ascnet-lucia/crates/ascnet-lucia-core" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

通过运行时模型配置创建 Agent：

```rust
use anyhow::Result;
use agent_core::{
    Agent, AgentModelConfig, ModelProviderConfig, ProviderKind,
};

#[tokio::main]
async fn main() -> Result<()> {
    let provider = ModelProviderConfig::new(
        "openai",
        ProviderKind::OpenAi,
        std::env::var("OPENAI_API_KEY")?,
    );
    let agent = Agent::from_model_config(AgentModelConfig::new(provider, "gpt-5"))?;
    let run = agent.run("列出当前任务").await?;
    println!("{}", run.final_text);
    Ok(())
}
```

API key、provider 选择和配置文件始终由调用方持有，Core 不持久化凭据。

## 从 TOML 创建

```rust
use anyhow::Result;
use agent_core::LuciaConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let config = LuciaConfig::load("examples/config/openai-responses.toml")?;
    let agent = config.build_agent()?;
    let run = agent.run("你好").await?;
    println!("{}", run.final_text);
    Ok(())
}
```

## 加载 WASM 插件

Plugin Host 在应用层组装，再作为通用 `AgentExtension` 挂到 Agent：

```rust
use agent_core::Agent;
use agent_plugin_host::wasm::load_wasm_plugins;
use std::sync::Arc;

let host = Arc::new(load_wasm_plugins(&["plugins/example/plugin.toml"]).await?);
let agent = Agent::new(gateway, options).with_extension(host);
```

下一步可以阅读 [Agent API](/agent/api) 或 [创建 WASM 插件](/plugin/quick-start)。
