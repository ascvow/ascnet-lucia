# Rust API 索引

本页提供稳定入口索引。具体字段、错误和示例见左侧对应章节及源码 Rustdoc。

## agent-core

| 模块 | 主要公开 API |
| --- | --- |
| `agent` | `Agent`、`AgentControl`、`AgentOptions`、`AgentModelConfig`、`AgentRun` |
| `context` | `ContextLoader`、`ContextLoadRequest`、`LoadedContext`、`ContextTransform` |
| `extension` | `AgentExtension`、`ToolDecision` |
| `event` | `AgentEvent`、`AgentEventKind`、`EventSink` 与内置 sink |
| `model` | `ModelGateway`、provider 配置、IR、stream、adapter trait |
| `session` | `Session` 与消息追加/读取 |
| `config` | `LuciaConfig`、`ModelConfig`、`AgentConfig` |

## agent-tool

| API | 用途 |
| --- | --- |
| `ToolSpec` | 发送给模型的工具定义 |
| `ToolCall` | provider-neutral 调用 |
| `ToolResult` | provider-neutral 返回值 |
| `Tool` | 原生工具 trait |
| `JsonTool` | JSON async 函数适配器 |
| `ToolRegistry` | 原生工具注册与调用 |

## agent-session

| API | 用途 |
| --- | --- |
| `SessionId` | 路径安全的稳定会话 ID 与 UUID 生成 |
| `SessionRecord` | schema、revision、时间、元数据和完整 Session 包络 |
| `SessionStore` | load/save/delete/list 异步存储契约 |
| `MemorySessionStore` | 测试和短生命周期进程存储 |
| `FileSessionStore` | 同进程 CAS 与原子文件持久化 |

## agent-runtime

| API | 用途 |
| --- | --- |
| `AgentTemplate`、`AgentDeriveConfig` | 从 Core Agent 派生独立实例 |
| `AgentPermissions`、`ToolAccess` | 只能收缩的工具权限 |
| `AgentRuntimeApi` | spawn/continue_agent/status/result/wait/cancel/send/receive |
| `AgentRuntimeProvisioner` | controller profile 的 grant/provision/revoke |
| `AgentId`、`AgentLineage`、`RuntimePrincipal` | 可信身份、父子谱系和 owner |
| `RuntimeLimits` | 深度、子节点、并发、邮箱与消息上限 |

## agent-plugin-host

| 模块 | 主要公开 API |
| --- | --- |
| 根模块 | `PluginHost`、`CompositePluginHost`、`NoopPluginHost` |
| 宿主服务 | `PluginHostServices`，用于注入 Agent Runtime 等可选通用服务 |
| `manifest` | `PluginManifest`、`PluginDependency`、`ProvidedCapability`、冲突解析与配置加载 |
| `service` | `PluginService`、`PluginServiceCall` |
| `wasm` | `WasmPluginHost`、`WasmPluginLimits`、`load_*_with_services` 与批量加载 |
| `ui` | 声明、frame、style、颜色、输入事件 |

## agent-plugin

| API | 用途 |
| --- | --- |
| `AgentPlugin` | Guest 生命周期和业务实现 |
| `PluginHostApi` | Guest 访问通用 Host 能力 |
| `ActivationContext` | 可信 ID 与 manifest metadata |
| `PromptContribution` | 动态 developer 提示 |
| `ExtensionEvent`、`EventPresentation` | 结构化事件与强类型展示提示 |
| `ContextLoadRequest`、`LoadedContext` | WASM 插件上下文完整替换协议 |
| `ServiceSpec`、`ServiceDescriptor`、`ServiceCall` | 插件间服务注册、发现与处理 |
| `ProcessSpec`、`FileEntry` | I/O 能力类型 |
| `AgentSpawnRequest`、`AgentContinueRequest`、`AgentHandle`、`AgentSnapshot`、`AgentOutcome` | Guest Agent Runtime 控制面类型 |
| `export_plugin!` | 生成 WIT component 导出 |

## 可见性原则

公开 API 面向嵌入方或插件作者。以下内容保持私有：

- Agent 的 ReAct 局部变量与内部队列锁。
- Wasmtime `Store`、`Instance` 和 typed function。
- 插件贡献注册表的锁和路由实现。
- provider wire-format 解析辅助函数。

需要新能力时优先增加稳定 trait 或数据结构，不暴露具体运行时对象。
