# Rust API 手册

本手册面向把 Lucia 嵌入 Rust 应用、实现原生工具、接入会话存储、管理派生 Agent 或加载 WASM 插件的开发者。它按开发目标解释参数、返回值、错误和副作用，不要求先理解仓库目录。

## 按目标选择

| 目标 | 首选 API | 详细参考 |
| --- | --- | --- |
| 构造并运行单个 Agent | `Agent`、`AgentOptions`、`ModelProviderConfig` | [Core 与 Runtime API](/reference/rust-core#agent-core) |
| 注册宿主原生工具 | `ToolSpec`、`Tool`、`JsonTool`、`ToolRegistry` | [工具 API](/reference/rust-core#agent-tool) |
| 保存和恢复会话 | `SessionRecord`、`SessionStore`、`FileSessionStore` | [会话 API](/reference/rust-core#agent-session) |
| 派生和管理后台 Agent | `AgentRuntimeApi`、`AgentRuntimeProvisioner` | [Runtime API](/reference/rust-core#agent-runtime) |
| 编写 WASM Guest 插件 | `AgentPlugin`、`PluginHostApi`、`export_plugin!` | [Plugin SDK API](/reference/rust-plugin#agent-plugin) |
| 在应用中加载插件 | `PluginHost`、`CompositePluginHost`、`load_wasm_plugins*` | [Plugin Host API](/reference/rust-plugin#agent-plugin-host) |
| 管理插件安装状态 | `PluginManager`、Registry API | [Plugin Manager API](/reference/rust-plugin#agent-plugin-manager) |
| 直接核对 Component ABI | WIT imports、exports、JSON 信封 | [WIT API 0.6](/reference/wit) |

## Crate 边界

| Crate | 对外职责 | 不负责 |
| --- | --- | --- |
| `agent-core` | 模型网关、ReAct、上下文、事件和扩展契约 | 插件加载、配置文件位置、会话持久化 |
| `agent-tool` | 工具定义、调用结果和原生注册表 | Agent 循环、插件 owner 路由 |
| `agent-context` | 默认上下文压缩、模型摘要与 Context Policy 绑定 | Agent 循环、插件加载 |
| `agent-session` | 版本化记录、CAS、文件与内存存储 | 模型配置、Agent 调度、插件状态 |
| `agent-runtime` | 身份、派生、权限收缩、生命周期和资源上限 | workflow、teammate 等业务协议 |
| `agent-plugin` | Guest SDK、共享 JSON 类型、WIT 绑定和导出宏 | Host 实现、终端渲染 |
| `agent-plugin-host` | ABI、鉴权、贡献注册、服务和 owner 路由 | MCP、Skill、Command 等具体协议 |
| `agent-plugin-manager` | 安装、完整性锁、依赖求解、启停和诊断 | 构建插件源码、实例化 WASM |

## 通用约定

### 错误

大部分构造、I/O、模型和插件接口返回 `anyhow::Result<T>`；`agent-session` 与 `agent-runtime` 使用可分类的领域错误。业务上允许模型继续修正的工具失败应返回 `ToolResult::error`，基础设施或协议失败才返回 Rust `Err`。

### 所有权

- `with_*` 通常消费 `self` 并返回新值，适合构造链。
- `set_*` 通常接收 `&mut self` 并返回 `&mut Self`，适合运行前动态配置。
- `Arc<dyn Trait>` 表示组件可能被 Agent、Runtime 或异步任务共享。
- `run*` 接收并返回 Session 所有权；Core 不在内部持久化会话。

### 同步与异步

模型、工具、事件、存储、Runtime 和 Host 路由是异步接口。`AgentPlugin` 保持同步，因为 Guest 调用运行在 Component 边界内；需要异步 I/O 时通过同步 `PluginHostApi` 提交给 Host 的受控能力实现。

### JSON 与兼容性

模型中间表示、工具参数、插件服务和 WIT ABI 都使用 provider-neutral JSON。新增公共 JSON 字段应有 serde 默认值；删除、改名或改变含义需要升级相应协议版本。WIT 的稳定规则见 [WIT API 0.6](/reference/wit#兼容规则)。

## 典型调用链

### 嵌入 Agent

```text
ModelProviderConfig
  -> Agent::from_provider_config
  -> ToolRegistry / EventSink / ContextLoader
  -> Agent::run 或 Agent::run_session
  -> AgentRun.session 交给应用持久化
```

### 加载插件

```text
PluginManifest::load + validate
  -> load_wasm_plugins_with_selection_and_services
  -> CompositePluginHost
  -> Agent::with_extension
  -> 工具、事件、服务和 UI 按 owner 路由
```

### 派生 Agent

```text
Host 注册 profile
  -> AgentRuntimeProvisioner::grant_profile
  -> provision 得到 controller 与绑定 API
  -> AgentRuntimeApi::spawn / continue_agent
  -> status / result / subscribe / cancel
```

## 继续阅读

- [Core、工具、会话与 Runtime API](/reference/rust-core)
- [Plugin SDK、Host 与 Manager API](/reference/rust-plugin)
- [插件开发指南](/development/plugin)
- [二次开发指南](/development/custom)
