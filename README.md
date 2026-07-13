# Lucia

Lucia 是一个用 Rust 实现的可嵌入 ReAct Agent 运行时，提供交互式 TUI、命令行管理能力和基于 WASM Component Model 的插件系统。

## 快速开始

环境要求：Rust stable、[Bun](https://bun.sh/)；开发 WASM 插件还需要 `wasm32-wasip2` target，仓库的 `rust-toolchain.toml` 已声明该目标。

先运行不需要 API key 的离线示例：

```bash
cargo run -p agent-basic-cli -- --demo "你好"
```

安装带官方插件的 TUI：

```bash
bun run install:tui
lucia --demo
```

首次运行会创建 `$HOME/.lucia/config.toml`。没有可用模型密钥时，`lucia` 会进入本地演示模式。

## 用户指南

- [TUI 使用](docs/usage/tui.md)：安装、启动、模型配置、会话、输入与插件界面。
- [CLI 使用](docs/usage/cli.md)：启动参数、会话参数、诊断命令及参数优先级。
- [插件管理](docs/usage/plugin-management.md)：搜索、安装、更新、启停、删除和独占能力选择。
- [其他使用方式](docs/usage/other.md)：离线示例、构建分发、诊断、事件日志和真实模型测试。

## 开发者指南

- [插件开发](docs/development/plugin.md)：插件工程、`AgentPlugin` 生命周期函数、Host API、manifest、构建与测试。
- [二次开发](docs/development/custom.md)：嵌入 Core、配置模型、注册工具、运行 Agent、控制任务与持久化会话。

开发章节不只是 API 名称索引。每个主要函数都说明用途、参数含义、返回值、错误条件和副作用；按模块查询时从 [Rust API 手册](docs/reference/rust-api.md)进入。

## 模块边界

```text
应用 / TUI
  -> agent-core          Agent、模型、ReAct、事件和扩展契约
  -> agent-tool          通用工具类型与原生工具注册表
  -> agent-session       版本化会话记录、CAS 和存储
  -> agent-runtime       Agent 身份、派生、生命周期和资源限额
  -> agent-plugin-host   WASM ABI、权限、贡献注册和 owner 路由
  -> agent-plugin        Guest SDK、共享协议类型和导出宏
```

具体插件协议属于独立插件，不进入 Core 或 Host。修改跨 crate 行为前先阅读[架构边界](docs/guide/architecture.md)；修改 WIT 或公共 JSON 类型前阅读 [WIT API](docs/reference/wit.md)。
