# Rust API 手册

Lucia 的精确 API 文档直接来自源码 Rustdoc。每个公开模块、结构体、枚举、字段、trait、trait 方法和函数都必须有文档；七个库 crate 均启用了 `#![deny(missing_docs)]`，缺少说明会让构建失败。

本页说明各 crate 的职责和入口。具体参数、返回值、错误、字段语义与源码位置以生成的 Rustdoc 为准。

## 生成 API 手册

生成面向使用者的公开 API：

```bash
bun run docs:rust
```

入口位于：

```text
target/doc/agent_core/index.html
```

需要查看私有模块、内部结构体和辅助函数时生成内部手册：

```bash
bun run docs:rust:private
```

内部手册适合维护者追踪实现，不代表稳定 API。测试模块和构建脚本不属于库接口，不纳入公开 API 承诺。

## agent-core

Agent 的通用运行机制。它不加载插件、不决定配置文件位置，也不持久化模型密钥。

| 源码模块           | 主要 API                                                                | 负责内容                                     |
| ------------------ | ----------------------------------------------------------------------- | -------------------------------------------- |
| `agent.rs`         | `Agent`、`AgentControl`、`AgentOptions`、`AgentModelConfig`、`AgentRun` | ReAct 生命周期、运行参数、实时控制与最终结果 |
| `config.rs`        | `AgentRootConfig`、`ModelConfig`、`AgentConfig`                         | TOML 解析及运行时配置构造                    |
| `context.rs`       | `ContextLoader`、`ContextLoadRequest`、`LoadedContext`                  | 模型请求前的上下文替换契约                   |
| `event.rs`         | `AgentEvent`、`AgentEventKind`、`EventSink`                             | 生命周期事件与 JSONL/内存 sink               |
| `extension.rs`     | `AgentExtension`、`ToolDecision`                                        | 宿主无关扩展、工具贡献和前后置钩子           |
| `model/adapter.rs` | `ChatModel`、`ProviderAdapter`                                          | 模型完成和流式响应的服务商边界               |
| `model/gateway.rs` | `ModelGateway`、`ModelProviderConfig`、`ProviderKind`                   | provider 注册、选择和请求路由                |
| `model/ir.rs`      | `ModelRequest`、`ModelResponse`、`ModelMessage`、`ContentBlock`         | OpenAI/Anthropic 共享的中间表示              |
| `model/stream.rs`  | `ModelEventStream`、`ModelStreamEvent`                                  | 文本、推理和工具调用的流式事件               |
| `session.rs`       | `Session`                                                               | 单次 Agent 上下文中的有序消息历史            |

Rustdoc 入口：`target/doc/agent_core/index.html`。

## agent-tool

模型可见工具与宿主原生工具注册表，不包含 Agent 循环或插件加载逻辑。

| API            | 用途                                     |
| -------------- | ---------------------------------------- |
| `ToolSpec`     | 工具名称、说明和输入 JSON Schema         |
| `ToolCall`     | 模型请求执行的一次 provider-neutral 调用 |
| `ToolResult`   | 与调用 ID 对应的成功值或结构化错误       |
| `Tool`         | 宿主原生异步工具 trait                   |
| `JsonTool`     | 把异步 JSON 函数包装为 `Tool`            |
| `ToolRegistry` | 注册、列举和按名称执行原生工具           |
| `builtins`     | Core 之外的通用内置工具实现              |

Rustdoc 入口：`target/doc/agent_tool/index.html`。

## agent-session

版本化会话记录、CAS 和本地存储。它不持有模型配置、Agent 调度或插件状态。

| 源码文件        | 主要 API                                                       | 负责内容                             |
| --------------- | -------------------------------------------------------------- | ------------------------------------ |
| `protocol.rs`   | `SessionId`、`SessionRecord`、`SessionSummary`、`SessionStore` | schema、revision、错误与异步存储契约 |
| `memory.rs`     | `MemorySessionStore`                                           | 测试和短生命周期进程的内存实现       |
| `file_store.rs` | `FileSessionStore`                                             | 原子文件写入、跨进程锁和摘要索引     |
| `diagnostic.rs` | `diagnose_file_session_store`、`SessionDiagnosticReport`       | 不创建锁或索引的只读记录诊断         |

Rustdoc 入口：`target/doc/agent_session/index.html`。

## agent-runtime

Agent 身份、权限收缩、派生、生命周期、事件订阅和资源上限。workflow、teammate 等业务协议不属于该 crate。

| 源码文件         | 主要 API                                                                      | 负责内容                                             |
| ---------------- | ----------------------------------------------------------------------------- | ---------------------------------------------------- |
| `identity.rs`    | `AgentId`、`AgentLineage`、`RuntimePrincipal`、`AgentProfileId`               | 可信身份和父子谱系                                   |
| `permissions.rs` | `AgentPermissions`、`ToolAccess`、`AgentTemplate`                             | 只能收缩的工具和运行参数权限                         |
| `protocol.rs`    | `AgentRuntimeApi`、`AgentRuntimeProvisioner`、`AgentSnapshot`、`AgentOutcome` | spawn、continue、steer、observe、events、cancel 契约 |
| `runtime.rs`     | `AgentRuntime`                                                                | 并发、拓扑限制、任务执行与事件回放                   |
| `error.rs`       | `AgentRuntimeError`、`RuntimeResult`                                          | 稳定错误分类                                         |

Rustdoc 入口：`target/doc/agent_runtime/index.html`。

## agent-plugin-host

WASM ABI、manifest、权限鉴权、贡献注册、服务目录、owner 路由和宿主无关 UI 协议。

| 源码模块      | 主要 API                                                   | 负责内容                              |
| ------------- | ---------------------------------------------------------- | ------------------------------------- |
| `lib.rs`      | `PluginHost`、`CompositePluginHost`、`PluginHostServices`  | 多插件组合和应用层宿主入口            |
| `manifest.rs` | `PluginManifest`、`PluginDependency`、`ProvidedCapability` | manifest 解析、依赖排序与独占能力选择 |
| `service.rs`  | `PluginService`、`PluginServiceCall`                       | 版本化插件服务注册和调用              |
| `ui.rs`       | `UiDeclaration`、`UiFrame`、`UiInput`                      | 终端无关的视图、样式和输入协议        |
| `wasm`        | `WasmPluginHost`、`WasmPluginLimits`、`load_wasm_plugins*` | component 加载、WIT 调用和资源限制    |

Rustdoc 入口：`target/doc/agent_plugin_host/index.html`。

## agent-plugin

WASM Guest SDK。插件作者实现 `AgentPlugin`，通过 `PluginHostApi` 使用已授权宿主能力，并用 `export_plugin!` 导出 component。

| API                                                  | 用途                                             |
| ---------------------------------------------------- | ------------------------------------------------ |
| `AgentPlugin`                                        | activate、工具、事件、服务、上下文和 UI 生命周期 |
| `PluginHostApi`                                      | 受控文件、子进程、服务目录和 Agent Runtime 调用  |
| `ActivationContext`                                  | Host 注入的可信插件 ID 与 manifest 元数据        |
| `ExtensionEvent`、`EventPresentation`                | 结构化事件及宿主无关展示提示                     |
| `ContextLoadRequest`、`LoadedContext`                | 上下文完整替换协议                               |
| `ServiceSpec`、`ServiceDescriptor`、`ServiceCall`    | 插件间版本化服务协议                             |
| `AgentSpawnRequest`、`AgentSnapshot`、`AgentOutcome` | Guest 可见的 Agent Runtime 控制面类型            |
| `export_plugin!`                                     | 生成 WIT Component Model 导出                    |

Rustdoc 入口：`target/doc/agent_plugin/index.html`。

## agent-plugin-manager

Registry 索引、SemVer 依赖求解、GitHub Release 获取、本地插件 bundle 安装、原子更新、
完整性锁和只读诊断。它不会构建源码或实例化 WASM 插件。

| API                                                                      | 用途                                |
| ------------------------------------------------------------------------ | ----------------------------------- |
| `PluginManager`                                                          | 管理插件根目录及安装状态            |
| `InstallOptions`、`GithubInstallOptions`                                 | 控制本地与 GitHub 安装行为          |
| `GithubPluginSource`、`GithubInstallResult`                              | 规范化 GitHub 来源和安装结果        |
| `RegistryRequest`、`RegistryInstallResult`                               | 解析 name@semver 并返回依赖安装结果 |
| `RegistrySearchResult`、`RegistryOutdatedPlugin`、`RegistryUpdateResult` | Registry 查询与更新结果             |
| `InstalledPlugin`、`PluginLock`                                          | 持久化 bundle 来源、摘要和状态      |
| `DoctorReport`、`DoctorIssue`                                            | 依赖、文件完整性和能力选择诊断      |

Rustdoc 入口：`target/doc/agent_plugin_manager/index.html`。

## 文档约束

新增或修改公开 API 时，Rustdoc 至少需要说明：

- 该项解决什么问题，以及它属于哪个模块边界。
- 非显然参数、字段和返回值的语义。
- 可能返回的错误及触发条件。
- 持久化、网络、进程、权限或事件等重要副作用。
- 兼容性要求、默认值和调用顺序约束。

私有复杂逻辑使用简短边界说明；不要给显然的局部变量或简单转发函数添加逐行复述式注释。
