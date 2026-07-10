# Lucia

`Lucia` 是一个 Rust workspace，用于实现最小 ReAct Agent 核心、模型协议转换层，以及 WASM Component Model 插件系统。

Author / 作者：`ascvow`

## Design boundary / 设计边界

core 只负责 ReAct 循环、会话状态、模型协议转换、事件和通用扩展点，不依赖插件 manifest、UI 协议、Wasmtime 或插件宿主 crate。

```text
agent-core
  ReAct loop / ReAct 循环
  Session / 会话
  ModelGateway / 模型协议转换层
  OpenAI Responses adapter / OpenAI Responses 适配器
  OpenAI Chat Completions adapter / OpenAI-compatible 适配器
  Anthropic Messages adapter / Anthropic Messages 适配器
  AgentExtension / 通用 Agent 扩展点

agent-tool
  ToolSpec / ToolCall / ToolResult
  Tool trait / ToolRegistry

agent-session
  Versioned session record / 版本化会话记录
  Memory and atomic file stores / 内存与原子文件存储

agent-runtime
  Agent derivation and lifecycle / Agent 派生与生命周期
  Permission narrowing and private session continuation / 权限收缩与私有会话续跑

agent-plugin
  Guest SDK / 插件侧 SDK
  AgentPlugin trait
  export_plugin! macro

agent-plugin-host
  Plugin manifest / 插件 manifest
  WASM host / Wasmtime 插件宿主
  Plugin UI protocol / 插件 UI 协议
  AgentExtension adapter / core 扩展适配器

agent-plugin-manager
  Local bundle install / 本地 bundle 安装
  Dependency and capability checks / 依赖与能力检查
  Integrity lock and diagnostics / 完整性锁与诊断
```

`agent-core` 只能定义 Agent 运行所需的通用消息、上下文、工具、事件和扩展契约，不得依赖 Plugin Host、WASM ABI、插件 manifest 或 UI。`agent-plugin-host` 只负责 ABI、生命周期、能力鉴权、贡献注册和 owner 路由，不得解析或实现 MCP、Skill、上下文压缩等具体扩展需求。具体协议和业务能力必须位于独立插件 crate，其端到端测试也归插件所有。

There is no independent HTTP proxy server in this project. Provider wire formats are converted in-process through `ModelGateway` and provider adapters.

本项目没有独立 HTTP proxy server。不同服务商的 wire format 通过进程内 `ModelGateway` 和 provider adapter 完成转换。

## Runtime model config / 运行时模型配置

`agent-core` does not persist model configuration. The caller owns API keys, selected provider, model id, and config storage. Core provides explicit runtime entry points: `Agent::from_model_config`, `Agent::set_model_config`, `Agent::set_model_provider_config`, `Agent::set_model_selection`, `Agent::set_model_route`, `Agent::set_provider_options`, `Agent::options_mut`, and `ModelGateway::upsert_from_config`.

`agent-core` 不持久化模型配置。API key、当前服务商、模型 ID 和配置存储都由调用方维护。core 提供明确的运行时入口：`Agent::from_model_config`、`Agent::set_model_config`、`Agent::set_model_provider_config`、`Agent::set_model_selection`、`Agent::set_model_route`、`Agent::set_provider_options`、`Agent::options_mut` 和 `ModelGateway::upsert_from_config`。

```rust
use agent_core::{Agent, AgentModelConfig, ModelProviderConfig};

let provider = ModelProviderConfig::openai(
    "default",
    std::env::var("OPENAI_API_KEY")?,
);

let mut model_config = AgentModelConfig::new(provider, "gpt-5.5");
model_config.max_tokens = Some(4096);

let mut agent = Agent::from_model_config(model_config)?;

// Later, the caller can replace provider config without any core-side persistence.
// 稍后，调用方可以替换服务商配置；core 不做任何配置持久化。
agent.set_model_config(AgentModelConfig::new(
    ModelProviderConfig::anthropic("default", std::env::var("ANTHROPIC_API_KEY")?),
    "claude-model-id",
))?;

// Or switch to an already-registered provider/model pair.
// 或切换到已经注册的 provider/model 组合。
agent.set_model_selection("default", "another-model-id")?;
```

## Event persistence and provider billing fields / 事件保存与服务商计费字段

Core emits structured events for the whole ReAct lifecycle. Persistence is caller-owned through `EventSink`; built-in sinks include `JsonlEventSink`, `InMemoryEventSink`, `CompositeEventSink`, and `NoopEventSink`.

core 会为整个 ReAct 生命周期产出结构化事件。事件持久化由调用方通过 `EventSink` 维护；内置 sink 包括 `JsonlEventSink`、`InMemoryEventSink`、`CompositeEventSink` 和 `NoopEventSink`。

```rust
use agent_core::JsonlEventSink;
use std::sync::Arc;

let agent = agent.with_event_sink(Arc::new(JsonlEventSink::new("runs/events.jsonl")));
let run = agent.run("hello").await?;
println!("run_id={}, total_tokens={:?}", run.run_id, run.usage.total_tokens);
```

Whenever a provider returns token usage or billing/cost fields, core emits a `billing_usage` event. Core does not keep a local price table and does not estimate cost. It forwards provider-returned cost fields into `provider_billing`.

只要服务商返回 token usage 或费用/计费字段，core 就会发出 `billing_usage` 事件。core 不维护本地价格表，也不估算费用；它会把服务商接口返回的费用字段转发到 `provider_billing`。

Example billing payload / 计费事件载荷示例：

```json
{
  "provider": "default",
  "model": "gpt-5.5",
  "usage": {
    "input_tokens": 1200,
    "output_tokens": 300,
    "total_tokens": 1500
  },
  "provider_billing": {
    "amount": 0.0123,
    "currency": "USD",
    "fields": {
      "usage.cost": 0.0123,
      "usage.currency": "USD"
    }
  }
}
```

## WASM plugin ABI / WASM 插件 ABI

The plugin contract is WIT-based and exported as a WASM Component:

插件契约基于 WIT，并以 WASM Component 形式导出：

```wit
package ascnet:lucia-plugin@0.6.0;

world plugin {
  import host-agent-upsert-tool: func(request-json: string) -> string;
  import host-agent-upsert-prompt: func(request-json: string) -> string;
  import host-fs-read: func(request-json: string) -> string;
  import host-process-spawn: func(request-json: string) -> string;
  import host-process-write: func(request-json: string) -> string;
  import host-process-read-line: func(request-json: string) -> string;
  import host-agent-runtime-call: func(request-json: string) -> string;
  export activate: func(context-json: string) -> string;
  export list-tools: func() -> string;
  export call-tool: func(call-json: string) -> string;
  export before-tool: func(call-json: string) -> string;
  export after-tool: func(result-json: string);
  export on-event: func(event-json: string);
  export load-context: func(request-json: string) -> string;
  export describe-ui: func() -> string;
  export render-ui: func(request-json: string) -> string;
  export on-ui-input: func(input-json: string);
}
```

The ABI uses JSON strings intentionally. This keeps the WIT boundary stable while Rust-side structs can evolve through serde-compatible fields.

ABI 故意使用 JSON 字符串。这样 WIT 边界保持稳定，同时 Rust 侧结构体可以通过 serde 兼容字段继续演进。

`agent-plugin-host` 提供端到端宿主实现：它会加载 `plugin.toml`，用 Wasmtime 编译 `.wasm` component，执行可选的 `activate`，并维护公开工具名到 owner 插件及本地工具 ID 的直接路由。旧插件仍可通过 `list-tools` 提供静态工具；新插件可以在运行时注册工具和 developer 提示。

### 插件 TUI

插件可以通过三个可选 UI 导出提供自己的终端界面。宿主探测不到 `describe-ui` 时，会把旧插件作为纯工具插件加载；探测到该导出后，`render-ui` 和 `on-ui-input` 必须同时存在。

- `describe-ui` 返回 `UiDeclaration` 数组。每个视图可以挂载到 `top`、`right`、`bottom`、`left` 四个插槽或 `dialog` 模态层。
- `render-ui` 接收宿主实际分配的尺寸和焦点状态，返回由行、文本片段和便携样式组成的 `UiFrame`。插件不能访问 Ratatui `Frame`、终端句柄或发送 ANSI 控制序列。
- `on-ui-input` 只接收当前焦点视图的宿主无关输入事件。`Tab` 在主输入区和可聚焦停靠视图之间切换；可见 `dialog` 始终优先接收输入。
- 多个插件占用同一插槽时按配置加载顺序堆叠，宿主始终为中心主界面保留最小空间；多个可见对话框只显示最后加载的一个。

主 TUI 通过 plugin-host 读取配置中的 `[[plugins]]` 条目。同一个组合宿主以 `AgentExtension` 挂到 core，并以 `PluginHost` 服务 UI 循环；core 不接触插件 UI 或加载细节。插件 UI 运行时错误只会显示在对应视图内，不会退出主 TUI。

完整能力展示见 [`examples/plugins/ui-showcase-plugin`](examples/plugins/ui-showcase-plugin/README.md)，该插件同时实现四向插槽、模态对话框、样式、键鼠输入、Agent 事件和工具驱动状态。

### 官方插件

官方 stdio MCP 插件位于 [`examples/plugins/mcp-plugin`](examples/plugins/mcp-plugin/README.md)。MCP 配置扫描、JSON-RPC 初始化、工具解析和 `tools/call` 都由该插件完成；Plugin Host 只提供受控文件读取、子进程 stdio、动态工具注册和 owner 路由，不理解 MCP 或 MasterGo 协议。

官方 Skill 插件位于 [`examples/plugins/skill-plugin`](examples/plugins/skill-plugin/README.md)。它扫描 `SKILL.md`，只把名称和描述注入 Agent，并通过 `skill_read` 按需加载完整指令；Core 和 Host 不解析 Skill 格式。

受限 Agent 派生与续跑示例见 [`examples/plugins/agent-runtime-plugin`](examples/plugins/agent-runtime-plugin/README.md)。Host 只提供 profile 授权、生命周期、状态、结果和取消；sub-agent、workflow、multi-agent 与 teammate 的角色、邮箱和消息协议由插件定义。

上下文完整替换链路见 [`examples/plugins/context-replacement-plugin`](examples/plugins/context-replacement-plugin/README.md)。该测试插件声明独占的 `agent.context-loader` 能力，模型只会收到插件返回的摘要；真实压缩策略仍由插件作者实现。

插件可以在 manifest 中使用 SemVer 声明必选或可选依赖。Host 在加载前解析依赖图，并提供版本化 JSON 服务的注册、发现与调用 API；command、Skill 等插件协议仍由插件自行定义。

## Documentation / 开发文档

文档站覆盖 Agent Core、Plugin Host、Guest API、WIT ABI、TUI 扩展和 WASM 插件开发。JavaScript 工具链统一使用 Bun：

```bash
bun install
bun run docs:dev
```

生产构建使用：

```bash
bun run docs:build
```

工程化指南：

- [插件管理](docs/guide/plugin-management.md)：安装、启用、依赖检查、独占能力选择和完整性诊断。
- [插件性能分析](docs/guide/performance.md)：纯 Core 编译边界、Host 微基准和真实 WASM p95 门禁。
- [真实模型分层测试](docs/guide/live-testing.md)：依次验证最小响应、ReAct、复杂工具链和插件调用。

## TUI 配置与会话

在仓库中安装一次纯 Core 版命令：

```bash
bun run install:tui
```

之后直接运行 `lucia`。首次启动会自动创建 `$HOME/.lucia/config.toml`；模型 URL、密钥和模型名称分别从 `model.base_url`、`model.api_key`/`model.api_key_env` 和 `model.model` 读取。未设置模型密钥时进入本地演示模式，并在主事件区提示配置方式。`LUCIA_HOME`、`LUCIA_CONFIG` 和 `--config` 可以覆盖默认位置。

```bash
lucia
lucia --resume-latest
lucia --session-id design-review
```

会话默认保存在 `$HOME/.lucia/sessions`。TUI 会自动恢复配置中的默认 Session，并把用户、助手和工具历史重新显示在主事件列表：

```bash
lucia --list-sessions
```

配置字段、路径优先级和 CAS 行为见 [TUI 配置与会话](docs/guide/tui-configuration.md)。

## Build / 构建

默认构建是不包含 Plugin Host、Wasmtime 和插件 UI 的纯 Core TUI：

```bash
bun run build:tui:core
```

需要插件系统时显式启用 `plugins` feature，并使用独立输出目录：

```bash
bun run build:tui:plugins
```

使用 `bun run build:tui` 可以依次构建两个版本。对应产物分别位于 `target/core-tui/release/lucia` 和 `target/plugin-tui/release/lucia`。完整说明见[构建与打包](docs/guide/distribution.md)。

```bash
cargo check
```

Build the sample plugin:

构建示例插件：

```bash
cd examples/plugins/echo-plugin
cargo build --release --target wasm32-wasip2
```

Run the demo model with the WASM plugin:

用 demo 模型加载 WASM 插件运行：

```bash
cd ../../..
cargo run -p agent-basic-cli -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml \
  --events-jsonl runs/events.jsonl \
  "hello from wasm"
```

Run the demo model without a WASM plugin. The CLI will use a native echo fallback, so the ReAct loop can be tested immediately:

不加载 WASM 插件运行 demo 模型。CLI 会使用原生 echo fallback，因此可以立即测试 ReAct loop：

```bash
cargo run -p agent-basic-cli -- --demo "hello"
```

## Real providers / 真实模型服务商

Edit the `model` field in the example config before using a real provider.

使用真实模型服务商前，请先把示例配置里的 `model` 字段改成你账号可用的模型 ID。

OpenAI Responses:

```bash
OPENAI_API_KEY=... cargo run -p agent-basic-cli -- \
  --config examples/config/openai-responses.toml \
  "Say hello"
```

OpenAI-compatible Chat Completions:

```bash
cargo run -p agent-basic-cli -- \
  --config examples/config/openai-compatible.toml \
  "Say hello"
```

Anthropic Messages:

```bash
ANTHROPIC_API_KEY=... cargo run -p agent-basic-cli -- \
  --config examples/config/anthropic.toml \
  "Say hello"
```

## Write a plugin / 编写插件

```rust
use agent_plugin::{export_plugin, AgentPlugin, Result, ToolCall, ToolResult, ToolSpec};
use serde_json::json;

#[derive(Default)]
struct MyPlugin;

impl AgentPlugin for MyPlugin {
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "echo",
            "Echo text. / 回显文本。",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false
            }),
        )]
    }

    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        Ok(ToolResult::success(call.id, call.name, call.args))
    }
}

export_plugin!(MyPlugin);
```

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
agent-plugin = { path = "../../crates/ascnet-lucia-plugin" }
serde_json = "1"
wit-bindgen = "0.59"
```

Build:

```bash
cargo build --release --target wasm32-wasip2
```

## Security model / 安全模型

插件默认没有文件、进程、HTTP、secret 或写入能力，并受 Wasmtime fuel 与默认 `64 MiB` 单线性内存上限约束。`fs_read` 只允许读取 manifest 声明的路径；`process_exec = true` 才能使用无 shell 的子进程 stdio API。HTTP、secret 和文件写入尚未实现，申请这些能力会在加载阶段失败。

进程能力具有较高权限，因为被启动的原生程序不受 WASM 沙箱约束。只应向可信插件授予该能力，并把 token 等凭据保存在被忽略的本地配置中。
