# Lucia 文档

Lucia 是一个 Rust ReAct Agent 运行时。它既可以作为终端应用直接使用，也可以作为 Core 嵌入 Rust 程序，还可以通过 WASM 插件扩展工具、协议和界面。

这套文档按任务组织。先选择你要完成的事情，不需要从 API 参考开始阅读。

## 第一次使用

从[快速开始](/guide/quick-start)开始。它会依次带你完成：

1. 不使用 API key 运行离线 ReAct 示例。
2. 安装并启动交互式 TUI。
3. 配置 OpenAI Responses 或其他真实模型服务。
4. 确认会话和配置保存位置。

只想查命令时，直接打开[常用场景示例](/guide/examples)。

## 按目标阅读

### 使用终端 Agent

- [快速开始](/guide/quick-start)：从离线演示到第一次真实模型请求。
- [TUI 配置与会话](/guide/tui-configuration)：配置优先级、会话恢复、路径覆盖和事件文件。
- [常用场景示例](/guide/examples)：本地模型、指定会话、记录事件和手动加载插件。
- [插件管理](/guide/plugin-management)：安装、启用、依赖检查和完整性诊断。

### 嵌入 Rust 应用

- [Agent API](/agent/api)：构造 Agent、运行请求和控制执行。
- [工具与事件](/agent/tools-events)：注册原生工具并消费生命周期事件。
- [会话持久化](/agent/session-persistence)：使用 CAS 和文件存储保存会话。
- [上下文加载](/agent/context-loader)：裁剪或替换模型实际看到的消息。
- [Agent Runtime](/agent/agent-runtime)：派生受限 Agent 并管理生命周期。

### 开发 WASM 插件

- [创建 WASM 插件](/plugin/quick-start)：从 crate、实现、manifest 到构建和运行。
- [生命周期](/plugin/lifecycle)：理解 activate、工具调用、事件、UI 和 deactivate。
- [Guest Host API](/plugin/host-api)：文件、进程、服务和 Agent Runtime 能力。
- [Manifest 与权限](/host/manifest-capabilities)：声明权限、依赖和独占能力。
- [测试与调试](/plugin/testing)：component 编译、smoke test 和常见错误。

### 理解系统边界

- [架构边界](/guide/architecture)：各 crate 的职责和允许的依赖方向。
- [Plugin Host API](/host/overview)：宿主加载、组合与路由接口。
- [插件依赖与服务](/plugin/dependencies-services)：跨插件的版本化服务调用。
- [WIT ABI 0.6](/reference/wit)：Host imports、Guest exports 和兼容策略。

## 示例在哪里

| 目标 | 可运行示例 |
|---|---|
| 最小 ReAct 和原生工具 | `examples/basic-cli` |
| 最小 WASM 工具插件 | `examples/plugins/echo-plugin` |
| stdio MCP 接入 | `examples/plugins/mcp-plugin` |
| Skill 按需加载 | `examples/plugins/skill-plugin` |
| 上下文压缩 | `examples/plugins/context-plugin` |
| 命令与 Dialog | `examples/plugins/command-plugin` |
| Agent 派生与续跑 | `examples/plugins/agent-runtime-plugin` |
| 多 Agent 协作 | `examples/plugins/teammate-plugin` |
| 结构化计划 | `examples/plugins/plan-plugin` |
| 插件 TUI 能力 | `examples/plugins/ui-showcase-plugin` |

每个插件都保持为独立 crate，并在自己的目录中提供 README 和测试。官方插件的用途与统一构建命令见[官方插件](/plugin/official)。

## 一次请求经过哪些层

```text
用户输入
  -> 应用或 TUI
  -> Agent Core：组装上下文并请求模型
  -> 模型返回文本或 ToolCall
  -> 原生工具注册表，或 Plugin Host 的 owner 路由
  -> WASM 插件执行具体协议或业务能力
  -> ToolResult 返回 Core，继续 ReAct 循环
  -> 应用保存事件和会话
```

Core 只定义通用 Agent 机制；Plugin Host 只负责 ABI、权限、贡献注册和路由；MCP、Skill、Command 等规则属于对应插件。需要判断一个功能应该放在哪里时，阅读[架构边界](/guide/architecture)。

## 本地运行文档

```bash
bun install
bun run docs:dev
```

生产构建使用 `bun run docs:build`。
