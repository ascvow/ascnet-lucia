# Lucia

Lucia 是一个用 Rust 实现的可嵌入 ReAct Agent 运行时，同时提供交互式 TUI 和基于 WASM Component Model 的插件系统。

你可以用它完成三类工作：

- 直接运行 `lucia`，获得带会话、工具和官方插件的终端 Agent。
- 在 Rust 应用中使用 `agent-core`，自行控制模型、工具、事件和会话。
- 编写独立 WASM 插件，在不修改 Core 的情况下接入 MCP、Skill、命令、工作流或自定义界面。

## 先运行起来

环境要求：Rust stable、[Bun](https://bun.sh/)；WASM 插件使用 `wasm32-wasip2` target。仓库的 `rust-toolchain.toml` 已声明所需 target。

先用内置脚本模型验证完整 ReAct 和工具调用流程，不需要 API key：

```bash
cargo run -p agent-basic-cli -- --demo "你好"
```

预期输出包含原生 `echo` 工具返回的输入内容。然后安装完整 TUI 和官方插件：

```bash
bun run install:tui
lucia --demo
```

首次直接运行 `lucia` 时会创建 `$HOME/.lucia/config.toml`。没有配置模型密钥时，TUI 自动使用本地演示模型。

更完整的首次运行说明见[快速开始](docs/guide/quick-start.md)。

## 配置真实模型

推荐让配置文件只保存环境变量名，不保存密钥。编辑 `$HOME/.lucia/config.toml`：

```toml
[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "替换为账号可用的模型 ID"
api_key_env = "OPENAI_API_KEY"
openai_protocol = "responses"

[agent]
max_steps = 8
max_tokens = 4096
```

设置密钥并启动：

```bash
export OPENAI_API_KEY="你的密钥"
lucia
```

Anthropic、OpenAI-compatible 服务和自定义配置路径见 [TUI 配置与会话](docs/guide/tui-configuration.md)。

## 常用例子

### 恢复会话

普通启动会创建空白会话。可以在 TUI 输入 `/resume`，也可以从命令行恢复：

```bash
lucia --list-sessions
lucia --resume-latest
lucia --session-id design-review
```

### 记录 Agent 事件

```bash
lucia --events-jsonl ./runs/events.jsonl
```

JSONL 文件会记录模型、工具和 ReAct 生命周期事件，适合定位请求与路由问题。

### 加载一个 WASM 插件

```bash
bun run build:plugin:echo
cargo run -p agent-basic-cli -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml \
  "hello from wasm"
```

这个例子会把 `echo` 工具交给 WASM 插件处理。完整插件教程见[创建 WASM 插件](docs/plugin/quick-start.md)。

更多可直接复用的命令和代码见[常用场景示例](docs/guide/examples.md)。

## 在 Rust 中嵌入

仓库内可以通过 TOML 配置构造 Agent：

```rust
use agent_core::AgentRootConfig;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AgentRootConfig::load("examples/config/openai-responses.toml")?;
    let agent = config.build_agent()?;
    let run = agent.run("列出当前任务").await?;
    println!("{}", run.final_text);
    Ok(())
}
```

Core 不保存 API key、服务商选择或配置文件；这些状态由调用方持有。构造方式、工具注册和事件接口见 [Agent API](docs/agent/api.md)。

## 项目边界

```text
应用 / TUI
  -> agent-core          ReAct、模型、上下文、工具调用、事件
  -> agent-session       版本化会话记录与存储
  -> agent-runtime       Agent 身份、派生、生命周期与资源限制
  -> agent-plugin-host   WASM ABI、权限、贡献注册与 owner 路由
       -> 独立插件        MCP、Skill、Command、Context、Plan 等具体协议
```

`agent-core` 不依赖 Plugin Host、WASM ABI、manifest 或插件 UI。Plugin Host 不实现 MCP、Skill 等具体协议。独立插件不加入原生 workspace，避免宿主 target 与 component 导出目标冲突。

插件 ABI 使用 JSON 字符串维持 WIT 边界稳定，规范定义位于 [`wit/plugin.wit`](wit/plugin.wit)。插件默认没有文件、进程、HTTP、secret 或写入能力；权限与子进程风险见 [Manifest 与权限](docs/host/manifest-capabilities.md)。

## 文档导航

- [文档首页](docs/index.md)：按使用目标选择阅读路径。
- [快速开始](docs/guide/quick-start.md)：离线演示、TUI 安装和真实模型配置。
- [常用场景示例](docs/guide/examples.md)：会话、事件、本地模型、插件和 Rust 嵌入。
- [架构边界](docs/guide/architecture.md)：crate 职责与依赖方向。
- [官方插件](docs/plugin/official.md)：Context、MCP、Skill、Command、Teammate 和 Plan。
- [Rust API 索引](docs/reference/rust-api.md) 与 [WIT ABI](docs/reference/wit.md)：接口参考。

本地启动文档站：

```bash
bun install
bun run docs:dev
```

生成包含每个公开模块、类型、字段、trait 和函数的 Rustdoc API 手册：

```bash
bun run docs:rust
```

需要同时查看私有实现时使用 `bun run docs:rust:private`。详细入口见 [Rust API 手册](docs/reference/rust-api.md)。

## 开发与验证

```bash
cargo check --workspace
bun run build:all
bun run docs:build
```

`build:all` 构建插件版 TUI 以及仓库中的官方、示例插件。纯 Core 与插件版的独立构建方式见[构建与打包](docs/guide/distribution.md)。

许可证：MIT。作者：`ascvow`。
