# Lucia 文档

Lucia 同时提供可直接使用的终端 Agent 和可嵌入、可扩展的 Rust 运行时。文档分为“使用”和“开发”两个板块：使用板块回答命令怎么运行，开发板块解释函数为什么存在、参数如何填写以及调用会产生什么结果。

## 使用

### TUI 使用

[TUI 使用](/usage/tui)从安装开始，说明模型配置、首次启动、会话恢复、输入方式、插件视图和常见退出流程。只想在终端中使用 Lucia，应先阅读这一章。

### CLI 使用

[CLI 使用](/usage/cli)集中说明 `lucia` 的全局参数、会话参数、事件日志参数和 `doctor` 管理命令，并给出参数覆盖配置文件时的优先级。

### 插件管理

[插件管理](/usage/plugin-management)说明如何从 Registry、GitHub Release 或本地 bundle 安装插件，以及如何搜索、更新、启用、禁用、删除和选择独占能力 owner。

### 其他

[其他使用方式](/usage/other)收录离线 CLI 示例、纯 Core 与插件版构建、全局诊断、事件排障、真实模型测试和本地文档运行方法。

## 开发

### 插件开发

[插件开发](/development/plugin)覆盖一个插件从 crate、`plugin.toml`、`AgentPlugin` 实现到 WASM 构建和 Host smoke test 的完整路径。生命周期函数和 Host API 均包含参数、返回值、错误和副作用说明。

### 二次开发

[二次开发](/development/custom)面向需要嵌入或改造 Lucia 的 Rust 应用，解释如何构造模型网关、创建 Agent、注册原生工具、运行或续跑会话、处理事件、控制任务和接入持久化。

## 深入资料

- [架构边界](/guide/architecture)：crate 职责与依赖方向。
- [Agent Runtime](/agent/agent-runtime)：派生 Agent、身份、生命周期和资源限额。
- [Plugin Host](/host/overview)：宿主加载、组合与 owner 路由接口。
- [Manifest 与权限](/host/manifest-capabilities)：插件能力声明和安全边界。
- [插件依赖与服务](/plugin/dependencies-services)：跨插件版本化服务。
- [WIT ABI 0.6](/reference/wit)：Host imports、Guest exports 与兼容策略。
- [Rust API 索引](/reference/rust-api)：按 crate 查询完整公开符号与生成 Rustdoc。

## 示例目录

| 目标 | 目录 |
| --- | --- |
| 最小 ReAct 与原生工具 | `examples/basic-cli` |
| 最小 WASM 工具插件 | `examples/plugins/echo-plugin` |
| stdio MCP | `examples/plugins/mcp-plugin` |
| Skill 按需加载 | `examples/plugins/skill-plugin` |
| 命令与 Dialog | `examples/plugins/command-plugin` |
| Agent 派生与续跑 | `examples/plugins/agent-runtime-plugin` |
| 多 Agent 协作 | `examples/plugins/teammate-plugin` |
| 工作流 | `examples/plugins/workflow-plugin` |
| 结构化计划 | `examples/plugins/plan-plugin` |
| 插件 TUI | `examples/plugins/ui-showcase-plugin` |

## 本地运行文档

```bash
bun install
bun run docs:dev
```

生产构建使用 `bun run docs:build`。
