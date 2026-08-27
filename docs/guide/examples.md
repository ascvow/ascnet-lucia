# 常用场景示例

以下命令默认从仓库根目录执行。涉及真实服务的示例只在环境变量中传递密钥，不要把密钥提交到配置文件。

## 不联网验证 ReAct

使用最小 CLI 和原生工具：

```bash
cargo run -p agent-basic-cli -- --demo "检查工具调用"
```

使用交互式 TUI：

```bash
cargo run -p lucia -- --demo
```

## 使用 OpenAI Responses

先确认 `examples/config/openai-responses.toml` 中的 `model` 是账号可用的模型 ID，再执行：

```bash
export OPENAI_API_KEY="你的密钥"
cargo run -p agent-basic-cli -- \
  --config examples/config/openai-responses.toml \
  "用三句话介绍 Lucia"
```

## 使用 Anthropic Messages

修改 `examples/config/anthropic.toml` 中的模型 ID：

```bash
export ANTHROPIC_API_KEY="你的密钥"
cargo run -p agent-basic-cli -- \
  --config examples/config/anthropic.toml \
  "解释当前任务"
```

## 使用本地 OpenAI-compatible 服务

`examples/config/openai-compatible.toml` 默认指向 `http://localhost:11434/v1`，适用于提供 OpenAI Chat Completions 兼容接口的本地服务：

```bash
cargo run -p agent-basic-cli -- \
  --config examples/config/openai-compatible.toml \
  "你好"
```

如果服务地址、模型名或鉴权方式不同，修改配置中的 `base_url`、`model` 和 `api_key`/`api_key_env`。

## 指定配置和数据目录

使用单独的 Lucia Home：

```bash
LUCIA_HOME="$PWD/.lucia-dev" lucia --init
LUCIA_HOME="$PWD/.lucia-dev" lucia --demo
```

只覆盖配置文件：

```bash
lucia --config ./lucia.toml
```

配置路径选择顺序是 `--config`、`LUCIA_CONFIG`、`$LUCIA_HOME/config.toml`。

## 恢复和查看会话

```bash
# 只列出当前项目的会话，不连接模型
lucia --list-sessions

# 恢复最近更新的会话
lucia --resume-latest

# 恢复指定会话
lucia --session-id design-review
```

普通启动以当前工作目录计算项目标识，因此应在原项目目录中执行恢复命令。

## 记录事件

临时指定 JSONL 文件：

```bash
lucia --events-jsonl ./runs/events.jsonl
```

也可以写入配置：

```toml
[tui]
events_jsonl = "events.jsonl"
```

相对路径以配置文件所在目录为基准。事件包含模型请求、工具调用和运行状态，可能带有用户输入或工具结果，分享前应先检查内容。

## 加载 Echo WASM 插件

构建插件并用最小 CLI 调用：

```bash
bun run build:plugin:echo
cargo run -p agent-basic-cli -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml \
  "hello from wasm"
```

在 TUI 中查看插件右侧面板：

```bash
cargo run -p lucia -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml
```

按 `Tab` 把焦点切换到插件面板。插件的 crate、manifest 和实现位于 `examples/plugins/echo-plugin`。

## 在 Rust 中从 TOML 构造 Agent

应用的 `Cargo.toml`：

```toml
[dependencies]
agent-core = { path = "../ascnet-lucia/crates/agent-core/kernel" }
anyhow = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

最小入口：

```rust
use agent_core::AgentRootConfig;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AgentRootConfig::load("lucia.toml")?;
    let agent = config.build_agent()?;
    let result = agent.run("列出当前任务").await?;
    println!("{}", result.final_text);
    Ok(())
}
```

`agent-core` 不决定配置文件位置，也不持久化密钥。应用需要自行管理配置和凭据。注册原生工具、挂载事件 sink 或接入 Plugin Host 时，继续阅读 [Agent API](/agent/api)和[工具与事件](/agent/tools-events)。
