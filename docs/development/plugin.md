# 插件开发

Lucia 插件是独立的 `wasm32-wasip2` crate。插件通过 `agent-plugin` Guest SDK 实现 `AgentPlugin`，由 `export_plugin!` 导出 WIT component；Host 负责验证 manifest、注入可信身份、收窄权限并把工具、服务、事件和 UI 路由到正确插件。

## 适合放进插件的功能

MCP、工作流、多 Agent 编排和特定 UI 属于插件规则。Command、Skill 与上下文压缩是 Kernel 默认原生能力；通用消息、ReAct、模型网关和事件契约属于 Core，具体原生实现分别由应用装配层和独立原生 crate 承担。ABI、权限和 owner 路由属于 Host，不得为复用具体协议扩大 Host 职责。

## 工程结构

```text
my-plugin/
  Cargo.toml
  plugin.toml
  src/lib.rs
  tests/
```

最小 `Cargo.toml`：

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
agent-plugin = { path = "../../../crates/agent-plugin" }
serde_json = "1"
wit-bindgen = "0.59"
```

`cdylib` 是 component 构建所需产物类型。插件应保持独立 crate；不要把具体插件加入原生默认成员来规避 WASM 导出目标冲突。

## Manifest

```toml
[plugin]
id = "my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = "0.7.0"
wasm = "target/wasm32-wasip2/release/my_plugin.wasm"
description = "提供示例工具的 Lucia 插件。"

[capabilities]
```

- `id`：稳定插件身份。Host 从 manifest 注入，模型和 Guest 不能覆盖。
- `version`：插件自身版本，与 Lucia 程序版本独立。
- `api_version`：WIT ABI 版本，不是插件版本。
- `wasm`：相对 manifest 的 component 路径。
- `capabilities`：文件、进程、服务、Agent Runtime 等授权声明。未声明即不授权。

完整字段和安全约束见 [Manifest 与权限](/host/manifest-capabilities)。

## 最小实现

```rust
use agent_plugin::{
    export_plugin, AgentPlugin, Result, ToolCall, ToolResult, ToolSpec,
};
use serde_json::json;

#[derive(Default)]
struct MyPlugin;

impl AgentPlugin for MyPlugin {
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "echo",
            "将文本原样返回给模型。",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要返回的文本。" }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        )]
    }

    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        let text = call
            .args
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({ "echo": text }),
        ))
    }
}

export_plugin!(MyPlugin);
```

`export_plugin!(MyPlugin)` 在 crate 根调用一次。类型必须实现 `Default + Send + 'static`；Host 为每个激活实例创建插件状态，插件不能依赖进程级全局状态保存业务数据。

## 生命周期函数

### `activate`

```rust
fn activate(
    &mut self,
    host: &dyn PluginHostApi,
    context: ActivationContext,
) -> Result<()>
```

作用：插件实例创建后的启动钩子。适合扫描配置、注册动态工具或提示、注册版本化服务和启动已授权长驻进程。

- `host`：当前插件的受限 Host API。每次调用都会按 manifest 权限和当前实例身份校验。
- `context.plugin_id`：Host 从 manifest 注入的可信插件 ID，插件应使用它记录状态，不应从模型输入推断身份。
- `context.metadata`：manifest 提供的自由格式只读元数据；缺失键不是错误。
- 返回值：`Ok(())` 表示插件可以进入 Ready；`Err` 会使本插件加载失败，其他独立插件仍可继续加载。
- 副作用：通过 `host` 注册的动态贡献归当前实例所有，卸载前应在 `deactivate` 清理长驻资源。

### `deactivate`

```rust
fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()>
```

作用：实例卸载前的清理钩子。应终止 `spawn_process` 创建的长驻进程，并移除不再需要的动态工具、提示或服务。

- `host`：仍绑定当前实例的 Host API，只能清理本插件拥有的资源。
- 返回值：清理成功返回 `Ok(())`；错误会被 Host 记录，但不能假设实例会继续存活。
- 注意：实例内存状态在卸载后不会保留。需要跨实例持久化的数据必须由插件自己的受控存储或服务管理。

## 工具函数

### `list_tools`

```rust
fn list_tools(&self) -> Vec<ToolSpec>
```

返回插件静态工具快照。`ToolSpec::new(name, description, input_schema)` 的参数分别是公开名称、给模型看的选择说明和 JSON Schema 输入约束。名称应使用跨服务商可接受的 ASCII 字母、数字、下划线或连字符，最长 64 字符。

该函数没有 `host` 参数，不适合做文件扫描或服务调用。需要在启动后动态发现工具时，在 `activate` 中调用 `host.upsert_tool`。

### `before_tool`

```rust
fn before_tool(&mut self, call: ToolCall) -> ToolDecisionStatus
```

在任意工具实际执行前参与策略判断，不只观察本插件拥有的工具。

- `call.id`：模型生成的调用 ID，重写时必须保持结果可关联。
- `call.name`：公开工具名。
- `call.args`：已经解析的 JSON 参数，但尚未按某个 Rust 结构验证。
- 返回值：`Ready { decision }` 提交 `Allow`、`Block`、`CancelRun` 或 `Rewrite` 最终决策；`Pending { retry_after_ms }` 请求 Host 稍后重新调用。`Pending` 不携带审批 ID、原因或 UI 结构，审批协议及其状态必须由提供该能力的插件自行维护。
- 安全要求：`reason` 面向用户和模型，不应包含 secret、完整环境变量或未脱敏命令参数。

### `call_tool`

```rust
fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult>
```

执行不需要 Host 能力的插件工具。

- `call`：模型请求的完整调用。推荐先检查 `call.name`，再用 `call.args_as::<Args>()` 做强类型反序列化。
- 返回值：业务成功使用 `ToolResult::success(call.id, call.name, content)`；可预期的业务失败使用 `ToolResult::error(...)`，这样模型能看到失败并继续 ReAct。
- 错误：返回 Rust `Err` 表示插件执行链本身失败，例如协议损坏或无法恢复的内部错误，可能终止当前 Agent 运行。
- 副作用：`&mut self` 允许更新实例计数或缓存；这些状态不跨卸载保留。

### `call_tool_with_host`

```rust
fn call_tool_with_host(
    &mut self,
    host: &dyn PluginHostApi,
    call: ToolCall,
) -> Result<ToolResult>
```

需要读取文件、调用服务、控制进程或派生 Agent 时覆盖这个函数。默认实现转发到 `call_tool`，因此两者不要重复实现同一工具分支。

- `host`：能力受 manifest 限制；权限不足返回 `Err`，不会自动扩大授权。
- `call`：含义与 `call_tool` 相同。
- 返回值和错误：与 `call_tool` 相同；Host API 的权限、路径和句柄错误会沿 `Result` 返回。

### `after_tool`

```rust
fn after_tool(&mut self, result: ToolResult)
```

观察最终工具结果，适合更新指标、审计摘要或插件内状态。`result.call_id` 和 `result.name` 标识调用，`result.content` 是模型可见载荷，`result.details` 只供 UI 使用。该函数没有返回值，不能修改已经产生的工具结果。

## 事件与上下文函数

### `on_event`

```rust
fn on_event(&mut self, event: AgentEvent)
```

接收 Core 生命周期事件。`event.run_id` 关联一次运行，`event.kind` 是稳定事件类型，`event.step` 是 ReAct 步数，`event.payload` 是对应类型的 JSON。该回调用于观察，不应假设所有事件都包含同一种 payload 结构。

## 服务函数

### `handle_service`

```rust
fn handle_service(
    &mut self,
    host: &dyn PluginHostApi,
    call: ServiceCall,
) -> Result<serde_json::Value>
```

处理其他插件通过 Host 发来的版本化服务调用。只有先用 `host.upsert_service` 注册的服务才会路由到这里。

- `call.caller_id`：Host 注入的可信调用方插件 ID；不要信任 payload 中自行声明的身份。
- `call.name`：当前插件内的服务名。
- `call.payload`：由服务协议定义的 JSON 请求，应按版本对应结构解析。
- 返回值：服务协议定义的 JSON 响应。
- 错误：未知服务、版本不兼容、调用方无权访问或 payload 无效时返回 `Err`。

## UI 函数

### `describe_ui`

```rust
fn describe_ui(&self) -> Vec<UiDeclaration>
```

声明静态视图类型。每项包含 `view_id`、标题、插槽、建议尺寸和是否可聚焦。`plugin_id` 应留空，由 Host 注入。该函数只声明能力，不绘制内容。

### `describe_tool_renderers`

```rust
fn describe_tool_renderers(&self) -> Vec<ToolRendererContribution>
```

声明当前插件自有工具在消息列表中的 renderer。`tool_name` 必须是该插件拥有的公开工具名，`renderer_id` 是插件内稳定标识，`plugin_id` 应留空。Host 负责校验 owner 和维护路由，TUI 不读取这些声明。

### `render_ui_with_host`

```rust
fn render_ui_with_host(
    &mut self,
    host: &dyn PluginHostApi,
    request: UiRenderRequest,
) -> Option<UiFrame>
```

按宿主分配的内容区渲染声明式文本帧。默认转发到不带 Host 的 `render_ui(request)`。

- `request.view_id`：要渲染的视图类型；动态子视图还带 `instance_id`。
- `request.width`、`request.height`：去除宿主边框后的可用尺寸，插件应据此裁剪内容。
- `request.focused`：当前视图是否接收输入。
- `request.frame`：单调递增帧号，可驱动轻量动画，不能当作持久时间戳。
- 返回 `Some(UiFrame)`：替换该视图最近一帧；返回 `None`：本次不更新。

`UiFrame` 只能包含可移植文本和样式，不得放 ANSI 序列或终端句柄。

### `render_tool_with_host`

```rust
fn render_tool_with_host(
    &mut self,
    host: &dyn PluginHostApi,
    request: ToolRenderRequest,
) -> Option<UiFrame>
```

渲染工具调用在主消息列表中的内容。请求包含完整 `ToolCall`、执行状态、完整结果或跳过原因，以及消息区可用尺寸。`plugin_id` 和 `renderer_id` 由 Host 根据可信贡献快照注入；插件返回的 `UiFrame.view_id` 必须等于 `renderer_id`。

返回 `Some(UiFrame)` 会替换该条消息的原生工具行；返回 `None` 保留原生展示。该 hook 只负责展示，工具执行、审批和状态机仍由各自的 Core 或插件协议负责。

### `on_ui_input_with_host`

```rust
fn on_ui_input_with_host(
    &mut self,
    host: &dyn PluginHostApi,
    input: UiInput,
)
```

处理路由给当前焦点视图的键盘或鼠标输入。默认转发到 `on_ui_input(input)`。

- `input.view_id`、`input.instance_id`：确定静态视图或动态实例。
- `input.event`：宿主无关的键盘或相对内容区鼠标事件。
- `host`：可用于更新服务状态、发布事件或请求 `navigate_view`。
- 副作用：函数无返回值；可见变化应保存到实例状态，并由后续 `render_ui*` 返回新帧。

## PluginHostApi 函数

Host API 的共同规则是：身份由 Host 注入、能力由 manifest 收窄、错误通过 `Result` 返回。插件不应捕获权限错误后改走未经授权的路径。

### Agent 贡献

- `upsert_tool(local_name, spec) -> Result<String>`：注册或替换动态工具。`local_name` 是插件内稳定键，`spec` 是模型可见定义；返回 Host 处理命名冲突后的公开工具名。会改变后续模型请求的工具快照。
- `remove_tool(public_name) -> Result<()>`：按公开名删除本插件工具；不存在时幂等。参数必须使用 `upsert_tool` 的返回值，而不是假设公开名等于本地名。
- `upsert_prompt(prompt) -> Result<String>`：注册 developer 提示贡献。`PromptContribution.id` 是插件内 ID，`content` 是正文，`priority` 越小越靠前；返回可信公开 ID。
- `remove_prompt(id) -> Result<()>`：按插件内部 ID 移除提示，不存在时幂等。
- `emit_event(event) -> Result<()>`：发布结构化扩展事件。`event.name` 是协议名，`data` 是 JSON，`presentation` 仅是可选 UI 展示提示。
- `navigate_view(request) -> Result<()>`：发布幂等子视图导航。`request_id` 用于去重，动作只能操作当前插件拥有的子视图。

### 实例状态

- `get_state(key) -> Result<Option<Value>>`：读取当前激活实例的内存值；没有该键返回 `None`。
- `set_state(key, value) -> Result<()>`：写入 JSON 值。该状态不跨实例卸载持久化。
- `remove_state(key) -> Result<Option<Value>>`：删除并返回旧值；不存在返回 `None`。

### 插件服务

- `upsert_service(service) -> Result<()>`：注册或替换本插件服务。`name` 是插件内稳定名，`version` 是服务契约 SemVer，`description` 只用于开发者发现。
- `remove_service(name) -> Result<()>`：删除本插件服务，不存在时幂等。
- `list_services(plugin_id) -> Result<Vec<ServiceDescriptor>>`：`None` 查询全部目录，`Some(id)` 只查询目标插件。返回的 `plugin_id` 来自 Host。
- `call_service(plugin_id, name, payload) -> Result<Value>`：调用目标插件服务。三个参数依次是可信目标插件 ID、目标内部服务名和协议 JSON；Host 会把真实调用方 ID 注入 `ServiceCall`。

### 文件与进程

- `read_file(path) -> Result<String>`：读取 `fs_read` 允许范围内的 UTF-8 文件。`path` 相对插件目录或符合 manifest 允许的路径；二进制或越界路径返回错误。
- `list_dir(path) -> Result<Vec<FileEntry>>`：列出允许目录的一层内容，不递归。返回相对路径和 `is_dir`。
- `spawn_process(spec) -> Result<u64>`：不经过 shell 启动长驻进程。`command` 是程序，`args` 原样传递，`env` 受限注入，`cwd` 相对插件目录；返回仅当前实例有效的句柄。
- `write_process(handle, data) -> Result<()>`：把 `data` 原样写入 stdin，不自动追加换行。
- `read_process_line(handle, timeout_ms) -> Result<Option<String>>`：在超时内读取一行 stdout；stdout 关闭返回 `None`，超时或句柄无效返回错误。
- `kill_process(handle) -> Result<()>`：终止进程并释放句柄。`deactivate` 应清理仍存活的句柄。

`process_exec` 等同于完整原生进程信任。插件不能通过 shell 拼接规避 `ProcessSpec` 的结构化参数边界。

### Agent Runtime

- `agent_identity() -> Result<AgentId>`：返回当前插件实例绑定的 controller Agent 身份。
- `spawn_agent(request) -> Result<AgentHandle>`：`request.profile` 引用应用与 manifest 共同授权的派生策略，`request.input` 是首次用户输入；只等待入队，不等待模型完成。
- `continue_agent(request) -> Result<AgentHandle>`：从当前 controller 可管理的成功终态会话继续运行；参数是目标 `AgentId` 和新增输入，不向 Guest 暴露原始 Session。
- `steer_agent(target, input) -> Result<()>`：向排队或运行中的后代 Agent 注入实时消息。
- `agent_status(target) -> Result<AgentSnapshot>`：查询当前 controller 或后代的状态、谱系和权限快照。
- `agent_result(target) -> Result<Option<AgentOutcome>>`：终态返回结果；未结束返回 `None`，不是错误。
- `agent_events(target, limit) -> Result<Vec<AgentEvent>>`：非阻塞读取历史回放和后续事件，`limit` 限制单次返回量。
- `cancel_agent(target) -> Result<bool>`：级联取消可管理的后代；返回值表示本次是否实际发起取消。

Runtime 的模型、provider options、工具范围和 owner 由服务端策略决定，Guest 不能在请求中伪造。

## 构建与测试

构建示例插件：

```bash
bun run build:plugin:echo
```

自定义插件可复用 `scripts/build-plugin.ts` 的 component 构建方式，或直接运行：

```bash
cargo build \
  --manifest-path examples/plugins/echo-plugin/Cargo.toml \
  --target wasm32-wasip2 \
  --release
```

验证至少分三层：

1. 插件 crate 原生单元测试，覆盖协议解析和业务分支。
2. `wasm32-wasip2` component 编译，验证导出和目标兼容性。
3. 真实 WASM Host smoke test，验证 manifest、权限、WIT、owner 路由和返回信封。

修改 WIT、Guest SDK 或 Host 绑定时必须同时运行 `agent-plugin` 与 `agent-plugin-host` 测试，并同步 `wit/plugin.wit`、Guest 内嵌 WIT、Host 绑定和契约 fixtures。详细命令见[测试与调试](/plugin/testing)。
