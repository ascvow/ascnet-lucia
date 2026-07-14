# Lucia

Lucia 是一个完全开源、可以自己组装能力的终端 Agent。

它不是又一个把模型包进命令行的外壳。Lucia 更关心一件事：当 Agent 能读取代码、调用工具、启动进程，甚至替你完成一连串操作时，你是否还知道它正在做什么，是否能够限制它，以及是否有权替换其中任何一个部分。

## 为什么做 Lucia

近期，围绕部分 AI 编程工具的数据采集边界，出现了不少争议和担忧，其中也包括对未披露行为甚至后门的质疑。这些说法需要证据验证，但它们提出的问题无法回避：一个深入终端和代码仓库的工具，不应该只要求用户无条件信任。

Lucia 因此诞生。

我们希望 Agent 的运行方式是可以检查的，能力是可以选择的，权限是可以收紧的。你不需要接受一整套不可拆分的功能，也不必等待某家公司决定下一步开放什么。需要 MCP，就安装 MCP 插件；需要 Skill、工作流、多 Agent 协作或新的交互界面，也可以通过插件实现。用不到的部分，不必交给它权限。

整个项目采用 MIT 许可证公开源代码。模型请求、Agent 循环、工具路由、会话存储、插件加载和权限检查都可以被阅读、修改和重新构建。

## Lucia 是什么

Lucia 由 Rust 编写，核心是一个可嵌入的 ReAct Agent 运行时，同时提供可直接使用的终端界面和基于 WASM Component Model 的插件系统。

- **对用户**：它是一个可以连接真实模型、保存项目会话、调用工具并按需安装插件的终端 Agent。
- **对开发者**：它是一组职责分明的 Rust crate，可以只嵌入 Agent Core，也可以接入会话、Runtime 和 Plugin Host。
- **对插件作者**：具体能力运行在独立 WASM 插件中，通过 manifest 声明所需权限，由 Host 负责身份、鉴权和路由。

Lucia 目前提供 Context、MCP、Skill、Command、Teammate、Plan 和 Sandbox 等官方插件。插件不是附属脚本，而是项目的主要扩展方式：Core 负责通用 Agent 机制，具体功能留在各自插件中。

## 我们如何看待安全

开源不等于自动安全，WASM 也不是一句“沙箱”就能解决所有问题。Lucia 选择把边界写清楚，而不是给出无法兑现的承诺。

- 插件必须在 manifest 中声明文件读取、原生进程或 Agent Runtime 等能力，未声明的能力不会被授权。
- Host 掌握插件身份、owner 和资源上限，不信任模型或插件自行声明的真实值。
- `process_exec` 等于授予插件当前系统用户的原生进程权限。启用这类插件前，仍应检查来源和代码。
- 连接在线模型时，请求必然会发送到你配置的模型服务；获权插件也可能访问本地资源。请根据自己的数据要求选择服务商、插件和权限。

Lucia 想提供的不是“请相信我们”，而是让信任有可以检查的依据。

## 快速开始

需要 Rust stable 和 [Bun](https://bun.sh/)。开发 WASM 插件时还需要 `wasm32-wasip2` target，仓库已经在 `rust-toolchain.toml` 中声明。

先运行不需要 API key 的离线示例：

```bash
cargo run -p agent-basic-cli -- --demo "你好"
```

安装带官方插件的 TUI：

```bash
bun run install:tui
lucia --demo
```

首次运行会创建 `$HOME/.lucia/config.toml`。配置自己的模型服务后，直接运行 `lucia` 即可进入真实会话。

更完整的说明见：

- [TUI 使用](docs/usage/tui.md)
- [模型配置与快速开始](docs/guide/quick-start.md)
- [插件管理](docs/usage/plugin-management.md)
- [常见问题诊断](docs/guide/doctor.md)

## 从哪里开始开发

只想增加一种能力，通常应该从插件开始，而不是修改 Core。MCP、Skill、命令、上下文压缩、工作流、多 Agent 编排和特定 UI 都属于插件能力。

- [创建 WASM 插件](docs/plugin/quick-start.md)
- [插件开发手册](docs/development/plugin.md)
- [Manifest 与权限](docs/host/manifest-capabilities.md)
- [Rust API 手册](docs/reference/rust-api.md)
- [架构边界](docs/guide/architecture.md)

需要把 Agent 嵌入自己的 Rust 应用时，可以只使用 `agent-core`，再按需要组合 `agent-tool`、`agent-session`、`agent-runtime` 和 `agent-plugin-host`。Lucia 不要求所有使用者接受同一种完整形态。

## 项目状态

Lucia 仍处于早期阶段，接口和插件协议还会继续演进。当前仓库包含离线示例、两种 TUI 构建形态、官方插件、插件端到端 smoke test 和分层开发文档，但它还需要更多真实环境验证、安全审查和不同使用方式的反馈。

欢迎阅读代码、提出问题、编写插件或提交改进。比起把 Lucia 变成另一个封闭的全能工具，我们更希望它成为一套任何人都能理解、修改并掌握边界的 Agent 基础设施。
