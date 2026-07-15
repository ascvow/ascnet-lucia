# Core、工具、会话与 Runtime API

本页覆盖 `agent-core`、`agent-tool`、`agent-session` 和 `agent-runtime` 的主要公开契约。示例侧重调用顺序；表格中的“错误/副作用”用于判断失败是否会改变现有状态。

## agent-core

### `AgentOptions`

`AgentOptions` 决定后续每次 ReAct 运行的通用行为。

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `provider` | `String` | `ModelGateway` 中注册的逻辑名称，不是服务商类型 |
| `model` | `String` | 发送给 adapter 的实际模型 ID |
| `max_steps` | `usize` | 单条用户指令连续执行的 ReAct 步数；`0` 表示不设总上限 |
| `system_prompt` | `String` | 新 Session 缺少 system 时写入的默认提示 |
| `tool_choice` | `ToolChoice` | 模型是否以及如何选择工具 |
| `max_tokens` | `Option<u32>` | 单次响应上限；`None` 表示不发送该字段 |
| `stream` | `bool` | 是否使用模型流式接口；默认 `true`，设为 `false` 时等待完整响应 |
| `temperature` | `Option<f32>` | 采样温度；范围由 adapter 或服务商校验 |
| `reasoning` | `ReasoningLevel` | 推理等级；不支持的 adapter 可忽略或报错 |
| `provider_options` | `Value` | 浅合并到服务商请求的专属 JSON 对象 |

`with_provider(provider)`、`with_model(model)`、`with_model_route(provider, model)` 和 `with_stream(stream)` 消费并返回 `AgentOptions`。这些函数只写字段，不验证 provider 是否已经注册。

### `AgentModelConfig::new`

```rust
pub fn new(
    provider: ModelProviderConfig,
    model: impl Into<String>,
) -> AgentModelConfig
```

- `provider`：包含逻辑名称、服务商类型、密钥、base URL 和协议。
- `model`：实际模型 ID。
- 返回：`tool_choice = Auto`、token/温度为空、推理关闭的运行时配置。
- 副作用：无；配置不持久化，也不建立网络连接。

### Agent 构造函数

| API | 参数 | 返回 | 错误/副作用 |
| --- | --- | --- | --- |
| `Agent::new(gateway, options)` | 已注册的网关与完整运行选项 | 空工具、空扩展、空事件 sink 的 Agent | 不验证 `options.provider`；首次运行才发现未知 provider |
| `from_model_config(config)` | 完整运行时模型配置 | 已注册并选中 provider 的 Agent | adapter 无法构造时返回错误；不发送请求 |
| `from_provider_config(config, model)` | provider 配置和模型 ID | 使用默认 AgentOptions 的 Agent | provider feature 未启用或配置无效时失败 |
| `from_provider_config_with_options(config, model, options)` | provider、模型和自定义选项 | 保留自定义选项的 Agent | provider/model 以显式参数为准，不采用 options 中旧值 |

优先使用 `from_provider_config`；只有调用方已经维护多个 adapter 时才直接使用 `Agent::new`。

### 模型切换函数

| API | 行为 | 失败后的状态 |
| --- | --- | --- |
| `set_model_config(config)` | 注册或替换 adapter，并切换模型及请求选项 | adapter 构造失败时不切换当前选择 |
| `set_model_provider_config(config)` | 注册或替换 adapter，并选择其逻辑 provider | 失败时保留当前 provider |
| `upsert_model_provider(config)` | 只注册或替换 adapter | 不改变当前 provider/model |
| `set_model_selection(provider, model)` | 切换到已注册 provider 和模型 | 未注册 provider 返回错误，当前选择不变 |
| `set_model(model)` | 只改模型 ID | 不验证该 ID 是否为服务商支持 |
| `set_provider_options(value)` | 整体替换服务商专属 JSON | 不做深合并 |

`set_model_route` 是 `set_model_selection` 的同义入口。`gateway()` 只读访问注册表，`gateway_mut()` 允许高级调用方直接增删 adapter。

### Agent 组件函数

| Builder | Setter/访问器 | 参数与语义 |
| --- | --- | --- |
| `with_tools(tools)` | `set_tools`、`tools`、`tools_mut` | 整体替换原生工具注册表；追加单个工具使用 `tools_mut().register(...)` |
| `with_extension(extension)` | `set_extension`、`extension` | 挂载一个 `Arc<dyn AgentExtension>`；Plugin Host 也通过此边界进入 Core |
| `with_event_sink(events)` | `set_event_sink`、`event_sink` | 挂载事件消费者；sink 错误会使运行失败 |
| `with_context_loader(loader)` | `set_context_loader`、`context_loader` | 每次模型请求前替换上下文；加载失败时请求不会发送 |
| `with_context_transform(transform)` | `set_context_transform` | 把同步消息变换包装成 ContextLoader |

`reset_context_loader()` 恢复透传实现。`set_options(options)` 整体替换运行选项，调用方必须确保 provider 名仍与网关一致。

### 运行函数

| API | 输入处理 | 返回 | 失败条件 |
| --- | --- | --- | --- |
| `run(input)` | 新建 Session，补 system，追加 user 文本 | `AgentRun` | 同一 Agent 已运行，或上下文、模型、工具、扩展、sink 失败 |
| `prepare_session(session, input)` | 只补 system 和 user，不运行 | 新 Session 值 | 无异步副作用，适合先保存用户输入 |
| `prepare_session_blocks(session, content)` | 追加文本、图片或文件块 | 新 Session 值 | 空 `content` 不追加用户消息 |
| `run_continue(session, input)` | 在已有 Session 追加 user 后运行 | `AgentRun` | 与 `run_session` 相同 |
| `run_session(session)` | 原样运行，不追加或替换任何消息 | `AgentRun` | 非法消息关系最终可能被 adapter 拒绝 |

`AgentRun` 字段：

| 字段 | 含义 |
| --- | --- |
| `run_id` | 本次运行所有事件共享的稳定 ID |
| `final_text` | 最终面向用户的助手文本 |
| `steps_used` | 实际模型步数 |
| `usage` | 多次模型调用累计 token 用量 |
| `session` | 包含工具调用与结果的最终 provider-neutral Session |
| `cancelled` | 是否因取消请求优雅收尾 |

同一个 `Agent` 不允许并发调用 `run*`。取消不是 Rust 错误：检查点会保留已完成轮次和部分流式文本，并返回 `cancelled = true`。

### `AgentControl`

`Agent::control()` 返回共享控制句柄，适合交给输入线程或 Tokio task。

| API | 参数 | 生效时机 | 返回 |
| --- | --- | --- | --- |
| `steer(text)` | 新用户指令 | 当前工具结束后尽快注入，并跳过本轮剩余工具 | `()` |
| `follow_up(text)` | 后续用户指令 | 当前任务正常完成后开启新预算 | `()` |
| `cancel()` | 无 | 模型流事件、工具之间或下一步开始前 | `()`，不等待完成 |
| `pending_steering()` | 无 | 立即读取队列长度 | `usize` |
| `pending_follow_ups()` | 无 | 立即读取队列长度 | `usize` |
| `clear_steering()` / `clear_follow_ups()` | 无 | 立即清空尚未注入的消息 | `()` |
| `state()` | 无 | 合并当前队列和取消标志后生成快照 | `AgentState` 克隆 |

`Agent::steer`、`follow_up` 和 `cancel` 是转发到内部控制句柄的便捷方法。

### `ModelProviderConfig`

| 字段 | 说明 |
| --- | --- |
| `name` | 进程内逻辑名称，必须满足 provider 命名规则 |
| `kind` | `OpenAi`、`OpenAiCompatible` 或 `Anthropic` |
| `api_key` | 已解析出的密钥值；结构本身不读取环境变量 |
| `base_url` | 服务根地址，不含具体 endpoint 路径 |
| `openai_protocol` | `Responses` 或 `ChatCompletions`；Anthropic 忽略 |
| `extra_headers` | 每次请求附加的 header |

构造函数：

- `new(name, kind, api_key)`：通用入口，使用默认协议和默认 base URL。
- `openai(name, api_key)`：选择 OpenAI Responses。
- `openai_compatible(name, api_key, base_url)`：选择兼容服务与 Chat Completions。
- `anthropic(name, api_key)`：选择 Anthropic Messages。
- `with_base_url`、`with_openai_protocol`、`with_header`：消费并返回配置；重复 header key 以后值覆盖。

### `ModelGateway`

| API | 参数 | 返回/错误 |
| --- | --- | --- |
| `new()` | 无 | 空网关 |
| `from_config(config)` | 一个 provider 配置 | 已注册网关；adapter 构造失败返回错误 |
| `register(name, adapter)` | 逻辑名、共享 adapter | 重名或名称非法时失败 |
| `upsert(name, adapter)` | 逻辑名、共享 adapter | 同名时替换，名称非法时失败 |
| `register_from_config(config)` | provider 配置 | 构造并注册；不允许重名 |
| `upsert_from_config(config)` | provider 配置 | 构造并替换同名 adapter |
| `remove(name)` | 逻辑名 | 返回被移除的 adapter，未知名称返回 `None` |
| `complete(provider, request)` | 逻辑名和完整请求 | 单次 `ModelResponse`；未知 provider 或请求失败返回错误 |
| `stream(provider, request)` | 逻辑名和完整请求 | `ModelEventStream`；后续模型错误通过流传递 |
| `provider_names()` | 无 | 当前名称快照，不代表注册顺序 |

### `ContextLoader`

```rust
#[async_trait]
pub trait ContextLoader: Send + Sync {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext>;
}
```

`ContextLoadRequest` 包含 `run_id`、step、provider、model、system 和本轮源消息。返回的 `LoadedContext` 是完整替换结果，不是增量。`LoadedContext::passthrough(request)` 原样保留 system 与消息；`TransformContextLoader::new(transform)` 只变换消息列表。

加载器必须保持 assistant tool call 与 tool result 配对。返回错误会阻止本轮模型请求。

### `EventSink`

`EventSink` 接收每条 `AgentEvent`。事件包含唯一 ID、run ID、毫秒时间戳、稳定 `AgentEventKind`、step 和 JSON payload。

| 实现 | 用途与副作用 |
| --- | --- |
| `NoopEventSink` | 丢弃事件 |
| `InMemoryEventSink` | 保存到内存，适合测试和嵌入式查询 |
| `JsonlEventSink` | 按行追加文件，I/O 失败会传播到 Agent |
| `CompositeEventSink` | 按注册顺序转发；任一 sink 失败使本次发布失败 |

### `AgentExtension`

`AgentExtension` 是 Core 唯一的通用扩展边界。主要方法：

| 方法 | 输入 | 返回/行为 |
| --- | --- | --- |
| `prompt_messages()` | 无 | 本轮 developer/system 贡献消息 |
| `list_tools()` | 无 | 扩展工具快照 |
| `call_tool(call)` | `ToolCall` | 扩展拥有工具的结果 |
| `before_tool(call)` | 候选调用 | `Allow`、`Block`、`Rewrite` 或 `CancelRun` 最终决策 |
| `after_tool(result)` | 最终工具结果 | 只观察，不修改结果 |
| `on_event(event)` | Core 生命周期事件 | 观察或积累扩展状态 |
| `drain_events()` | 无 | 返回扩展待发布事件，Core 包装为 Extension 事件 |

## agent-tool

### 数据类型

| 类型 | 关键字段 | 约束 |
| --- | --- | --- |
| `ToolSpec` | `name`、`description`、`input_schema` | 名称最长 64 字符，只允许 ASCII 字母、数字、`_`、`-` |
| `ToolCall` | `id`、`name`、`args` | `id` 必须原样带回 ToolResult |
| `ToolResult` | `call_id`、`name`、`content`、`is_error`、`details` | `details` 只给 UI，不发送给模型 |

`ToolCall::args_as<T>()` 把 JSON 参数反序列化为强类型，字段不匹配时返回包含工具名的错误。`args_json_string()` 用于协议适配；序列化异常时回退为 `{}`。

`ToolResult::success(call_id, name, content)` 构造业务成功；`error(call_id, name, message)` 构造模型可见失败；`with_details(details)` 附加 UI 细节；`content_text()` 把字符串原样返回，其他 JSON 序列化为文本。

### `Tool` 与 `JsonTool`

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, call: ToolCall) -> Result<ToolResult>;
}
```

`spec()` 可以被多次调用，应保持稳定且无副作用。`call()` 接收完整调用，基础设施错误返回 `Err`；业务错误返回 `ToolResult::error`。

`JsonTool::new(spec, handler)` 把 `Fn(Value) -> Future<Output = Result<Value>>` 包装为 Tool。它适合只返回 JSON 成功值的简单工具；需要自定义 `is_error` 或 `details` 时直接实现 `Tool`。

### `ToolRegistry`

| API | 参数 | 返回/错误 |
| --- | --- | --- |
| `register(tool)` | 具体 Tool | 校验名称后注册；重名失败 |
| `register_arc(tool)` | `Arc<dyn Tool>` | 共享实例注册；重名失败 |
| `get(name)` | 工具名 | 克隆共享引用，未知名称返回 `None` |
| `subset(names)` | 名称迭代器 | 共享原工具但创建独立映射；未知或重复名称失败 |
| `specs()` | 无 | 按名称稳定排序的定义快照 |
| `call(call)` | 完整 ToolCall | 路由并执行；未知工具或实现错误返回 `Err` |
| `contains` / `len` / `is_empty` | 查询参数 | 只读，不触发工具实现 |

## agent-session

### `SessionId`

- `generate()`：创建随机 UUID 字符串标识。
- `new(value)`：校验外部标识；只允许 ASCII 字母、数字、连字符、下划线，最长 128 字符。
- `as_str()`：返回借用，不分配新字符串。

无效标识返回 `InvalidSessionId`，可通过 `value()` 和 `reason()` 分别读取原值与稳定原因。

### `SessionRecord`

| 字段 | 含义 |
| --- | --- |
| `schema_version` | 持久化格式版本，当前为 1 |
| `id` | 稳定 SessionId |
| `revision` | 最近成功保存后的 CAS 修订号；新记录为 0 |
| `created_at_ms` / `updated_at_ms` | UNIX 毫秒时间 |
| `title` | 可选展示标题 |
| `metadata` | 与插件无关的扩展 JSON 映射 |
| `session` | 完整 provider-neutral Session |

`SessionRecord::new(id, session)` 创建未保存记录；只在系统时间无法转为 UNIX epoch 时失败。

### `SessionStore`

| 方法 | 参数 | 返回与并发语义 |
| --- | --- | --- |
| `load(id)` | SessionId 引用 | 不存在返回 `Ok(None)`；损坏或版本不支持返回错误 |
| `save(record, None)` | revision 0 的新记录 | 仅在 ID 不存在时创建，返回 revision 1 |
| `save(record, Some(n))` | record.revision 也必须为 n | 仅在当前 revision 等于 n 时更新，返回 n+1 |
| `delete(id, revision)` | ID 与期望 revision | 只删除完全匹配记录 |
| `list()` | 无 | 加载完整记录并按 ID 排序 |
| `list_summaries()` | 无 | 返回轻量摘要；恢复正文仍需 `load` |

`FileSessionStore::open(root)` 会创建或打开安全根目录、规范化路径并建立跨进程锁。根路径是符号链接或非目录时失败。`MemorySessionStore::new()` 适合测试或不需要进程重启恢复的场景。

主要错误包括：无效 ID、schema 不支持、record revision 与条件不匹配、CAS 冲突、revision 溢出、记录损坏、文件名与记录 ID 不一致、不安全路径及 I/O 错误。发生冲突时应重新 `load`，不要绕过存储直接覆盖文件。

## agent-runtime

### 核心类型

| 类型 | 含义 |
| --- | --- |
| `RuntimePrincipal` | Host 注入的可信调用主体 |
| `AgentId` | Runtime 生成的稳定 Agent 身份 |
| `AgentLineage` | parent、root 和 depth |
| `AgentProfileId` | Host 注册并授权的派生策略名称 |
| `AgentPermissions` | 派生后实际生效的工具与运行权限 |
| `AgentHandle` | 身份与谱系的可序列化句柄 |
| `AgentSnapshot` | 当前状态、谱系和权限快照 |
| `AgentOutcome` | 成功、失败或取消终态 |

身份和 principal 必须由 Runtime/Host 创建。调用方只能保存和回传，不应从模型输入构造可信 owner。

### `AgentRuntimeApi`

| 方法 | 参数 | 返回/行为 |
| --- | --- | --- |
| `principal()` | 无 | 当前绑定的可信 principal |
| `identity()` | 无 | 当前 controller 身份 |
| `spawn(request)` | 输入和 Host 已收窄的派生配置 | 入队后立即返回 AgentHandle |
| `continue_agent(target, input)` | 成功终态目标与新增输入 | 复用私有会话和有效权限，返回新句柄 |
| `steer(target, input)` | 自身或后代目标 | 向排队/运行任务注入消息；不支持时返回 InteractionUnavailable |
| `status(target)` | 自身或后代 ID | 当前 AgentSnapshot |
| `result(target)` | 自身或后代 ID | 未终态返回 `None` |
| `wait(target)` | 自身或后代 ID | 异步等待并返回终态；不适合直接暴露为同步 WIT import |
| `cancel(target)` | 自身或后代 ID | 级联取消；首次产生变化返回 true |
| `subscribe(target)` | 自身或后代 ID | 先回放近期事件，再投递实时事件 |

访问非自身/后代目标会返回权限错误。事件通道不限量缓冲，长期订阅方必须及时消费。

### `AgentRuntimeProvisioner`

| 方法 | 参数 | 返回/副作用 |
| --- | --- | --- |
| `grant_profile(principal, profile)` | 可信主体与已注册 profile | 授予后续 provision 权限 |
| `provision(principal, profile)` | 已授权组合 | 创建独立 controller 和身份绑定 API |
| `revoke_profile_grant(principal, profile)` | 主体与策略 | 只撤销后续 provision，返回是否存在授权 |
| `revoke(principal)` | 主体 | 取消并清理其全部 controller，返回清理数量 |

`ProvisionedAgentRuntime` 包含 `controller: AgentHandle` 和 `api: Arc<dyn AgentRuntimeApi>`。Host 应把后者注入受限组件，不暴露模板、模型凭据或全局授权表。

### 错误边界

`AgentRuntimeError` 区分无效身份/profile、未授权、目标不可见、并发或拓扑上限、任务不存在、终态冲突、交互不可用及底层运行失败。调用方应按错误类型处理，不要通过字符串判断权限或重试策略。

## 相关页面

- [二次开发](/development/custom)
- [Agent Runtime 设计](/agent/agent-runtime)
- [Plugin SDK、Host 与 Manager API](/reference/rust-plugin)
