# Lucia

Lucia 是一个完全开源、可以自己组装能力的终端 Agent。

它不是又一个把模型包进命令行的外壳。Lucia 更关心的是：当 Agent 可以读取代码、调用工具、启动进程，甚至替你连续完成一组任务时，你能否看见它如何工作、限制它能做什么，并替换其中任何一个不符合需要的部分。

## 为什么做 Lucia

近期，一些 AI 编程工具的数据采集方式和本地代码处理边界引发了争议。无论具体事件最终如何定性，有一个问题已经无法回避：一个深入终端、代码仓库和开发环境的工具，不应该只要求用户无条件信任。

Lucia 因此诞生。

我们希望 Agent 的运行方式可以检查，能力可以选择，权限可以收紧。你不需要接受一整套不可拆分的功能，也不必等待某家公司决定下一步开放什么。需要 MCP，就安装 MCP 插件；需要 Skill、工作流、多 Agent 协作或新的交互界面，也可以通过插件实现。用不到的能力，不必交给它权限。

整个项目采用 MIT 许可证公开源代码。从模型请求、Agent 循环到工具路由、会话存储、插件加载和权限检查，都可以被阅读、修改和重新构建。

## Lucia 能做什么

- 在终端中与真实模型交互，并按项目保存和恢复会话。
- 接入 OpenAI、Anthropic 和 OpenAI-compatible 模型服务。
- 通过 WASM 插件增加 MCP、Skill、命令、上下文压缩、计划、审批和多 Agent 协作等能力。
- 只使用轻量的 Agent Core，或按需组合会话、Runtime、Plugin Host 和 TUI。
- 把 Lucia 作为 Rust 库嵌入自己的应用，而不是只能使用现成终端界面。

Lucia 当前提供 Context、MCP、Skill、Command、Teammate、Plan 和 Sandbox 等官方插件。插件不是附属脚本，而是项目的主要扩展方式：Core 负责通用 Agent 机制，具体功能留在各自插件中。

## 先运行一次

只想确认项目能够工作，安装 Rust 后在仓库根目录执行：

```bash
cargo run -p agent-basic-cli -- --demo "你好"
```

这个示例使用确定性的内置模型，不需要 API key，也不会连接外部模型服务。它会走完一次模型请求、原生工具调用和结果回传流程。

## 环境要求

- Rust stable，具体工具链以仓库中的 `rust-toolchain.toml` 为准。
- `wasm32-wasip2`，仅在编译 WASM 插件时需要。
- Bun 是可选项，用于批量构建和同步官方插件，不是编译 Lucia Core 的必要条件。

缺少 WASM target 时可以手动安装：

```bash
rustup target add wasm32-wasip2
```

## 编译与安装

Lucia 有纯 Core 和插件版两种构建形态。两者生成的命令都叫 `lucia`，区别在于是否包含 Plugin Host、Wasmtime、插件管理和插件 UI。

### 方式一：只使用 Cargo 编译纯 Core

纯 Core 版本不包含 WASM 插件系统，适合只需要模型、原生工具、会话和事件能力的用户。

```bash
cargo build \
  -p lucia \
  --release \
  --no-default-features \
  --target-dir target/core-tui

./target/core-tui/release/lucia --demo
```

安装到 Cargo bin 目录：

```bash
cargo install \
  --path crates/agent-tui \
  --locked \
  --force \
  --no-default-features

lucia --demo
```

如果 shell 找不到 `lucia`，确认 `$HOME/.cargo/bin` 已加入 `PATH`。

### 方式二：只使用 Cargo 编译插件版

插件版包含 Plugin Host 和插件管理能力，但单独编译主程序不会自动构建或安装官方插件。

```bash
cargo build \
  -p lucia \
  --release \
  --features plugins \
  --target-dir target/plugin-tui

./target/plugin-tui/release/lucia --demo
```

安装插件版命令：

```bash
cargo install \
  --path crates/agent-tui \
  --locked \
  --force \
  --features plugins
```

不使用 Bun 也可以手动编译并加载单个插件。下面以 Echo 插件为例：

```bash
cargo build \
  -p echo-plugin \
  --release \
  --target wasm32-wasip2 \
  --target-dir examples/plugins/echo-plugin/target

./target/plugin-tui/release/lucia \
  --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml
```

### 方式三：一键安装插件版和官方插件

希望直接使用完整体验时，可以安装 Bun 后运行：

```bash
bun run install:tui
lucia --demo
```

这条命令会依次完成：

1. 编译官方 WASM 插件。
2. 通过 Cargo 编译并安装插件版 `lucia`。
3. 将官方插件同步到 `$LUCIA_HOME/official-plugins`，未设置 `LUCIA_HOME` 时使用 `$HOME/.lucia`。
4. 按需把 `$HOME/.cargo/bin` 写入 zsh 的 `PATH`。

只想构建而不安装时，可以分别运行：

```bash
bun run build:tui:core
bun run build:tui:plugins
bun run build:plugin:official
```

对应产物位于 `target/core-tui/release/lucia`、`target/plugin-tui/release/lucia` 和各插件目录的 `target/wasm32-wasip2/release`。

## 配置模型

首次启动会创建 `$HOME/.lucia/config.toml`。也可以先初始化配置后退出：

```bash
lucia --init
```

下面是一份最小配置：

```toml
[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "替换为账号可用的模型 ID"
api_key_env = "OPENAI_API_KEY"
openai_protocol = "responses"

[agent]
max_steps = 0
max_tokens = 4096

[tui]
sessions_dir = "projects"
```

`api_key_env` 保存的是环境变量名，不是密钥本身。设置环境变量后启动：

```bash
export OPENAI_API_KEY="你的密钥"
lucia
```

`provider` 还支持 `open-ai-compatible` 和 `anthropic`。本地模型服务、其他接口地址及完整配置项见 [TUI 配置与会话](docs/guide/tui-configuration.md)。

## 我们如何看待安全

开源不等于自动安全，WASM 也不是一句“沙箱”就能解决所有问题。Lucia 选择把边界写清楚，而不是给出无法兑现的承诺。

- 插件必须在 manifest 中声明文件读取、原生进程或 Agent Runtime 等能力，未声明的能力不会被授权。
- Host 掌握插件身份、owner 和资源上限，不信任模型或插件自行声明的真实值。
- `process_exec` 等于授予插件当前系统用户的原生进程权限。启用这类插件前，仍应检查来源和代码。
- 连接在线模型时，请求会发送到你配置的模型服务；获权插件也可能访问本地资源。请根据自己的数据要求选择服务商、插件和权限。

Lucia 想提供的不是“请相信我们”，而是让信任有可以检查的依据。

## 面向开发者

只想增加一种能力，通常应该从插件开始，而不是修改 Core。MCP、Skill、命令、上下文压缩、工作流、多 Agent 编排和特定 UI 都属于插件能力。

仓库的主要模块保持明确的职责边界：

- `agent-core`：模型网关、ReAct、上下文、事件和扩展契约。
- `agent-tool`：通用工具类型与原生工具注册表。
- `agent-session`：版本化会话记录、CAS 和存储。
- `agent-runtime`：Agent 身份、派生、生命周期、权限和资源限额。
- `agent-plugin-host`：WASM ABI、鉴权、贡献注册和 owner 路由。
- `agent-plugin`：Guest SDK、共享协议类型、WIT 绑定和导出宏。
- `agent-tui`：应用组装、配置、输入和终端渲染。

常用验证命令：

```bash
cargo test -p agent-core
cargo test -p lucia --no-default-features
cargo test -p lucia --features plugins
```

修改插件时还应编译 `wasm32-wasip2` 组件，并运行该插件的真实 Host 冒烟测试。各官方插件已经在 `package.json` 中提供对应的 `bun run test:plugin:*` 命令。

## 文档入口

- [快速开始](docs/guide/quick-start.md)
- [TUI 使用](docs/usage/tui.md)
- [CLI 使用](docs/usage/cli.md)
- [插件管理](docs/usage/plugin-management.md)
- [创建 WASM 插件](docs/plugin/quick-start.md)
- [插件开发手册](docs/development/plugin.md)
- [Manifest 与权限](docs/host/manifest-capabilities.md)
- [Rust API 手册](docs/reference/rust-api.md)
- [架构边界](docs/guide/architecture.md)

## 项目状态

Lucia 仍处于早期阶段，接口和插件协议还会继续演进。当前仓库已经包含离线示例、两种 TUI 构建形态、官方插件、插件端到端冒烟测试和分层开发文档，但仍需要更多真实环境验证、安全审查和不同使用方式的反馈。

欢迎阅读代码、提出问题、编写插件或提交改进。比起把 Lucia 变成另一个封闭的全能工具，我们更希望它成为一套任何人都能理解、修改并掌握边界的 Agent 基础设施。

## 许可证

[MIT](LICENSE)
