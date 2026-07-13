# 二次开发

二次开发是把 Lucia 的 Core、工具、会话或 Runtime 嵌入自己的 Rust 应用，而不是通过 WASM 扩展一个现有 `lucia` 进程。应用负责配置来源、密钥、UI、持久化和部署；Core 不读取 `$HOME/.lucia`，也不会替应用保存配置或会话。

## 选择 crate

- `agent-core`：Agent、模型适配器、ReAct、上下文、事件和扩展契约。
- `agent-tool`：工具定义、调用结果、原生工具 trait 和注册表。
- `agent-session`：版本化 Session 记录、CAS 和文件/内存存储。
- `agent-runtime`：受限 Agent 派生、生命周期、权限收缩和资源限额。
- `agent-plugin-host`：应用需要加载 WASM 插件时使用。

只需要单 Agent + 原生工具时依赖 `agent-core` 和 `agent-tool`。不要让 Core 依赖 Plugin Host；应用层负责把两者组装在一起。

## 最短嵌入示例

```rust
use agent_core::AgentRootConfig;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AgentRootConfig::load("lucia.toml")?;
    let agent = config.build_agent()?;
    let run = agent.run("分析当前项目").await?;
    println!("{}", run.final_text);
    Ok(())
}
```

这个路径适合配置文件由应用维护的场景。`build_agent` 只构造进程内对象，不会保存 TOML，也不会创建 Session 文件。

## 配置函数

### `AgentRootConfig::load`

```rust
pub fn load(path: impl AsRef<Path>) -> Result<AgentRootConfig>
```

从指定 TOML 文件读取模型与 Agent 配置。

- `path`：文件路径。Core 原样交给文件系统，不应用 TUI 的 `LUCIA_CONFIG` 或 `$LUCIA_HOME` 规则。
- 返回值：成功时返回反序列化后的 `AgentRootConfig`，包含 `model` 和 `agent` 两部分。
- 错误：文件不可读、TOML 语法错误或字段类型不匹配时返回带路径上下文的错误。
- 副作用：只读文件，不连接模型、不读取 `api_key_env` 指向的变量。

### `AgentRootConfig::agent_model_config`

```rust
pub fn agent_model_config(&self) -> Result<AgentModelConfig>
```

把 TOML 配置转换为运行时模型配置。

- 参数：只使用 `self.model` 和与模型请求有关的 `self.agent` 字段。
- 返回值：包含 provider、model、tool choice、token、温度、推理等级和 provider options。
- 错误：`api_key` 与 `api_key_env` 都没有值，或 `api_key_env` 对应环境变量不存在时返回错误。
- 副作用：可能读取一个环境变量；不会修改原配置或持久化解析后的密钥。

### `AgentRootConfig::build_agent`

```rust
pub fn build_agent(&self) -> Result<Agent>
```

根据配置创建可运行 Agent，并应用 `max_steps` 和自定义 system prompt。

- 返回值：空工具、无扩展、无事件 sink 的 Agent；模型 adapter 已注册并被选中。
- 错误：凭据解析失败、当前编译未启用对应 provider feature、base URL 或 provider 配置无效时返回错误。
- 副作用：只构造内存对象，不发送网络请求。第一次 `run*` 才会调用模型。

## 编程式模型配置

不使用 TOML 时，直接构造 `ModelProviderConfig`：

```rust
use agent_core::{Agent, ModelProviderConfig};

let provider = ModelProviderConfig::openai(
    "primary",
    std::env::var("OPENAI_API_KEY")?,
);
let agent = Agent::from_provider_config(provider, "模型 ID")?;
```

### `ModelProviderConfig::new`

```rust
pub fn new(
    name: impl Into<String>,
    kind: ProviderKind,
    api_key: impl Into<String>,
) -> ModelProviderConfig
```

- `name`：应用内部的逻辑 provider 名，供 `AgentOptions.provider` 和路由函数引用，不发送给模型服务商。
- `kind`：`OpenAi`、`OpenAiCompatible` 或 `Anthropic`，决定 adapter 类型。
- `api_key`：已经解析出的密钥值。函数不会替你读取环境变量。
- 返回值：使用默认协议、无自定义 base URL 和额外 header 的配置。

便捷构造函数 `openai(name, api_key)` 预设 Responses，`openai_compatible(name, api_key, base_url)` 预设 Chat Completions，`anthropic(name, api_key)` 预设 Messages。

### Builder 函数

- `with_base_url(base_url)`：设置服务根地址，不要包含 `/responses` 或 `/chat/completions` 等 endpoint 路径。消费并返回配置，适合链式调用。
- `with_openai_protocol(protocol)`：选择 `Responses` 或 `ChatCompletions`；Anthropic 忽略该字段。
- `with_header(key, value)`：添加一个额外 HTTP header。重复 key 会以最后一次值覆盖；不要用它绕过 adapter 的认证字段管理。

### `Agent::from_provider_config`

```rust
pub fn from_provider_config(
    provider_config: ModelProviderConfig,
    model: impl Into<String>,
) -> Result<Agent>
```

- `provider_config`：完整 provider 配置，所有权移入 Agent 的网关。
- `model`：实际发送给 provider 的模型 ID。
- 返回值：使用默认 `AgentOptions` 的 Agent，provider 和 model 会自动对齐到传入值。
- 错误：provider feature 未启用或 adapter 无法构造时返回错误。

需要同时指定 system prompt、步数和模型请求选项时使用 `from_provider_config_with_options(provider_config, model, options)`。该函数会以 `provider_config.name` 和 `model` 覆盖 `options` 中同名路由字段，其他选项保留。

### `Agent::set_model_config`

```rust
pub fn set_model_config(
    &mut self,
    config: AgentModelConfig,
) -> Result<&mut Agent>
```

运行前或两次运行之间注册/替换 provider，并切换完整模型请求配置。

- `config.provider`：注册或替换同名 adapter。
- `config.model`：切换模型 ID。
- 其余字段：覆盖 tool choice、token、温度、推理等级和 provider options。
- 返回值：可继续链式配置的可变 Agent 引用。
- 错误与原子性：adapter 构造失败时返回错误，当前模型选择不会切换。

只增加 provider 而不切换当前模型时使用 `upsert_model_provider(provider_config)`；切换已注册 provider 和模型时使用 `set_model_selection(provider, model)`，未知 provider 会返回错误。

## `Agent::new`

```rust
pub fn new(gateway: ModelGateway, options: AgentOptions) -> Agent
```

这是完全手动组装入口。

- `gateway`：已经注册 provider adapter 的路由表。`options.provider` 必须与其中某个逻辑名称一致，否则运行时返回未知 provider。
- `options`：provider、model、system prompt、ReAct 步数、工具策略和请求参数。
- 返回值：没有原生工具、扩展、事件 sink 和自定义上下文加载器的 Agent。
- 副作用：只分配内存状态，不验证选中 provider 是否存在，也不发送请求。

`AgentOptions.max_steps = 0` 表示单条用户指令不设置总 ReAct 步数上限。服务端后台任务通常应使用正数上限；交互应用还应提供取消能力。

## 注册原生工具

### `ToolSpec::new`

```rust
pub fn new(
    name: impl Into<String>,
    description: impl Into<String>,
    input_schema: serde_json::Value,
) -> ToolSpec
```

- `name`：稳定公开工具名，只能包含 ASCII 字母、数字、下划线和连字符，最长 64 字符。
- `description`：直接发送给模型，应该说明何时调用和返回什么，不要写宿主敏感信息。
- `input_schema`：描述参数的 JSON Schema 对象。建议明确 `required` 和 `additionalProperties`。
- 返回值：尚未校验名称的工具定义；注册时 `ToolRegistry` 会调用名称校验。

无参数工具使用 `ToolSpec::empty_object_schema()`，它禁止额外字段。

### `JsonTool::new`

```rust
pub fn new<F, Fut>(spec: ToolSpec, handler: F) -> JsonTool
```

把异步 JSON 函数包装为原生 `Tool`。

- `spec`：模型可见工具定义。
- `handler`：接收已解析 JSON 参数并返回 `Result<Value>` 的异步闭包。闭包必须 `Send + Sync + 'static`，future 必须 `Send + 'static`。
- 返回值：可注册的 `JsonTool`。
- 错误语义：handler 返回的 `Err` 会成为工具执行错误；需要让模型看到并自行修正的业务失败，可返回包含错误字段的 JSON，或实现自定义 `Tool` 返回 `ToolResult::error`。

示例：

```rust
use agent_tool::{JsonTool, ToolRegistry, ToolSpec};
use serde_json::json;

let mut tools = ToolRegistry::new();
tools.register(JsonTool::new(
    ToolSpec::new(
        "status",
        "读取当前服务状态。",
        ToolSpec::empty_object_schema(),
    ),
    |_args| async { Ok(json!({ "status": "ready" })) },
))?;
```

### `ToolRegistry::register`

```rust
pub fn register<T>(&mut self, tool: T) -> Result<&mut ToolRegistry>
where
    T: Tool + 'static
```

- `tool`：原生工具实例，注册表取得所有权并放入 `Arc`。
- 返回值：注册表可变引用，可链式注册多个工具。
- 错误：名称不符合跨 provider 规则或已经存在同名工具。
- 副作用：修改当前注册表；不会自动修改已经复制出的其他注册表。

已有 `Arc<dyn Tool>` 使用 `register_arc`。需要为派生 Agent 收窄工具集合时使用 `subset(names)`；任一名称不存在或重复时返回错误。

### `Agent::with_tools` 与 `set_tools`

`with_tools(tools) -> Agent` 消费 Agent 并返回配置后的值，适合构造链；`set_tools(&mut self, tools) -> &mut Agent` 替换现有注册表，适合可变对象。两者都会整体替换，不是追加。运行前追加单个工具可使用 `agent.tools_mut().register(tool)?`。

## 运行函数

### `Agent::run`

```rust
pub async fn run(&self, input: impl Into<String>) -> Result<AgentRun>
```

创建新 `Session`，补入 Agent 的 system prompt，追加一条用户消息并执行 ReAct。

- `input`：本次用户文本，会转换为 `String` 并写入新 Session。
- 返回值：`AgentRun`，包含 `run_id`、最终可见文本、使用步数、累计 token、最终 Session 和 `cancelled` 标志。
- 错误：同一 Agent 已在运行，或上下文、模型、工具、扩展、事件 sink 任一环节失败。
- 副作用：发送模型请求、执行工具、发出事件并更新 `Agent::state()`；Core 不自动持久化返回的 Session。

### `Agent::prepare_session`

```rust
pub fn prepare_session(
    &self,
    session: Session,
    input: impl Into<String>,
) -> Session
```

只准备会话，不运行模型。空会话会补 system prompt，然后追加一次用户消息。应用可以先持久化返回值，再调用 `run_session`，从而在模型失败时保留用户输入。

多模态输入使用 `prepare_session_blocks(session, content)`。`content` 是 `Vec<ContentBlock>`，可含文本、图片或文件；空列表不会追加用户消息。

### `Agent::run_session`

```rust
pub async fn run_session(&self, session: Session) -> Result<AgentRun>
```

直接运行调用方构造的完整 Session，不追加用户消息，也不替换 system prompt。

- `session`：所有权移入运行；调用前必须保证消息和 tool call / result 关系合法。
- 返回值：包含运行后 Session 的 `AgentRun`。
- 取消：在模型流、工具之间或下一步前收敛，返回 `cancelled = true`；已完成内容和部分流式文本保留。
- 错误：与 `run` 相同；无论成功、取消还是失败，最近快照都会写入 Agent 状态。

### `Agent::run_continue`

```rust
pub async fn run_continue(
    &self,
    session: Session,
    input: impl Into<String>,
) -> Result<AgentRun>
```

等价于先 `prepare_session(session, input)` 再 `run_session`。适合调用方已经拥有上一次 `AgentRun.session`，但不需要在模型调用前单独落盘的场景。

同一个 `Agent` 不允许并发运行。需要并发派生和父子生命周期时使用 `agent-runtime`，不要绕过运行槽位共享内部状态。

## 运行控制与状态

### `Agent::control`

```rust
pub fn control(&self) -> AgentControl
```

返回可跨 Tokio task 持有的轻量控制句柄。它共享 steering、follow-up、取消标志和状态快照，不取得整个 Agent 所有权。

### `AgentControl::steer`

```rust
pub fn steer(&self, text: impl Into<String>)
```

把消息排入 steering 队列，在当前工具完成后尽快注入，并跳过本轮尚未执行的工具。`text` 是新增用户指令；函数无返回值，锁中毒会 panic，因此调用方不应在持锁回调内递归调用。

### `AgentControl::follow_up`

```rust
pub fn follow_up(&self, text: impl Into<String>)
```

把消息排到当前任务正常完成之后，开启后续 ReAct 预算。它不会中断当前工具，也不会在当前任务失败或取消后强制续跑。

### `AgentControl::cancel`

```rust
pub fn cancel(&self)
```

设置当前运行的取消请求。运行循环在下一个检查点优雅收尾；函数不等待取消完成。新一次运行开始时会清除未消费的旧取消标志。

### `state`

`Agent::state()` 和 `AgentControl::state()` 都返回 `AgentState` 克隆快照。Control 版本还会在读取时合并最新队列长度与取消标志。调用方可以查询 phase、run ID、step、Session、流式文本、thinking、工具调用、用量和错误，但不能通过快照修改 Core 状态。

## 事件与上下文

### `with_event_sink` / `set_event_sink`

参数是 `Arc<dyn EventSink>`。Builder 版本消费 Agent，setter 版本替换现有 sink。内置实现包括丢弃事件的 `NoopEventSink`、测试用 `InMemoryEventSink`、追加 JSONL 的 `JsonlEventSink` 和顺序转发的 `CompositeEventSink`。

事件 sink 失败会使当前运行返回错误。日志路径、轮转和敏感信息策略属于应用职责。

### `with_context_loader` / `set_context_loader`

参数是 `Arc<dyn ContextLoader>`。加载器在每次模型请求前接收 provider-neutral system 与消息，并返回本轮实际输入。适合裁剪、摘要和检索增强；必须保持工具消息配对。加载失败会在发送模型请求前终止本轮。

同步闭包形式可使用 `with_context_transform(Arc<ContextTransform>)`，Core 会包装为异步加载器。

## 会话持久化

Core 返回 provider-neutral `Session`，`agent-session` 用 `SessionRecord` 增加稳定 ID、schema、revision、时间和标题。

### `SessionRecord::new`

```rust
pub fn new(
    id: SessionId,
    session: Session,
) -> Result<SessionRecord, SessionStoreError>
```

- `id`：通过 `SessionId::new` 校验的稳定标识，只允许 ASCII 字母、数字、连字符和下划线，最长 128 字符。
- `session`：要持久化的完整 Core Session。
- 返回值：`revision = 0` 的未保存记录，并填充当前 schema 和时间。
- 错误：系统时间早于 UNIX epoch 时失败。

### `FileSessionStore::open`

```rust
pub async fn open(root: impl AsRef<Path>) -> Result<FileSessionStore, SessionStoreError>
```

创建或打开文件存储并固定规范化根目录。`root` 不能是符号链接，必须是安全目录；函数可能创建根目录和跨进程锁文件，摘要索引在后续列表或保存操作中维护。失败时返回路径安全或 I/O 错误。

### `SessionStore::save`

```rust
async fn save(
    &self,
    record: SessionRecord,
    expected_revision: Option<u64>,
) -> Result<SessionRecord, SessionStoreError>
```

使用比较并交换保存记录。

- `record`：提交的完整记录，其 `revision` 必须与期望条件一致。
- `expected_revision = None`：只在记录不存在时创建，传入记录 revision 必须为 `0`。
- `expected_revision = Some(n)`：只在存储中的 revision 恰好为 `n` 时更新，传入记录也必须携带 `n`。
- 返回值：revision 已递增、更新时间已刷新的新记录；后续保存必须使用这个返回值。
- 错误：revision 冲突、schema 不支持、路径不安全、序列化或原子写入失败。

不要在冲突后直接覆盖文件。应重新 `load(id)`，决定合并、提示用户或创建新的会话 ID。

### 读取与删除

- `load(id) -> Result<Option<SessionRecord>, _>`：记录不存在返回 `None`；损坏或版本不支持返回错误。
- `list_summaries() -> Result<Vec<SessionSummary>, _>`：只读取轻量摘要，适合会话选择器；真正恢复前仍须 `load` 完整记录。
- `delete(id, expected_revision) -> Result<(), _>`：只有 revision 完全匹配才删除，避免另一进程更新后被旧客户端误删。

## 何时使用 Agent Runtime

需要后台并发、父子身份、私有会话续跑、profile 权限收缩、token/时间上限或级联取消时，使用 `agent-runtime`。Runtime 负责通用生命周期；workflow、teammate 消息和多 Agent 编排规则仍应位于插件。

继续阅读 [Agent Runtime](/agent/agent-runtime)和[架构边界](/guide/architecture)。按函数查询时使用 [Core、工具、会话与 Runtime API](/reference/rust-core)。
