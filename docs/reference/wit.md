# WIT API 0.6

Lucia 插件 ABI 的 world 是 `ascnet:lucia-plugin@0.6.0`。WIT 只固定函数名和 `string` 边界，业务结构通过 JSON 传递；这样可选字段可以按 serde 默认值演进，而不必为每个字段变化重写 Component 类型。

```wit
package ascnet:lucia-plugin@0.6.0;

world plugin {
  // Host imports
  import host-agent-upsert-tool: func(request-json: string) -> string;
  // ...其余 imports

  // Guest exports
  export activate: func(context-json: string) -> string;
  // ...其余 exports
}
```

完整 world 有 20 个 Host imports 和 12 个 Guest exports。当前 Host 要求所有 exports 存在；插件没有某项能力时由 SDK 默认实现返回空值，而不是删除 export。

## Host 响应信封

所有 Host imports 都返回同一种 JSON 字符串。

成功：

```json
{
  "schema_version": 1,
  "ok": true,
  "value": null
}
```

失败：

```json
{
  "schema_version": 1,
  "ok": false,
  "error": "面向插件开发者的错误文本"
}
```

- `schema_version`：Host 能力响应信封版本，当前为 `1`，与插件 ABI `0.6.0` 独立。
- `ok = true`：读取 `value`；无返回值的操作使用 JSON `null`。
- `ok = false`：读取 `error`，不得同时把缺失 `value` 当作成功空值。
- Guest SDK 的 `decode_host_response<T>` 会把失败信封转为 Rust `Err`，并把 `value` 反序列化为目标类型。
- 请求和进程数据有 Host 侧大小上限；超过限制、JSON 无法解析、权限不足或资源不存在都返回失败信封。

直接实现其他语言 Guest 时必须保留未知加法字段，不能要求信封只含上述三个键。

## Agent 贡献 imports

### `host-agent-upsert-tool`

注册或替换当前插件的动态工具。

请求：

```json
{
  "local_name": "search_project",
  "spec": {
    "name": "search_project",
    "description": "在当前项目中搜索文本。",
    "input_schema": {
      "type": "object",
      "properties": {
        "query": { "type": "string" }
      },
      "required": ["query"],
      "additionalProperties": false
    }
  }
}
```

- `local_name`：插件内部稳定键，用于替换同一贡献；不是最终公开名。
- `spec.name`：期望的模型可见名称，Host 会校验跨 provider 命名规则和冲突。
- `spec.description`：直接提供给模型。
- `spec.input_schema`：JSON Schema 对象。
- 成功 `value`：Host 分配的公开工具名字符串。删除工具和处理实际路由时应保存这个值。
- 常见错误：本地名/公开名非法、schema 非对象、公开名与其他 owner 冲突。

Rust SDK：`PluginHostApi::upsert_tool(local_name, spec) -> Result<String>`。

### `host-agent-remove-tool`

```json
{ "name": "公开工具名" }
```

`name` 必须是 upsert 返回的公开名。成功 `value` 为 `null`；目标不存在时保持幂等。插件不能删除其他 owner 的工具。

Rust SDK：`remove_tool(public_name) -> Result<()>`。

### `host-agent-upsert-prompt`

```json
{
  "id": "project-rules",
  "content": "修改文件前先读取项目规则。",
  "priority": 10
}
```

- `id`：插件内部稳定 ID。
- `content`：注入模型请求的 developer 提示正文。
- `priority`：整数，数值越小排列越靠前；缺省为 `0`。
- 成功 `value`：Host 分配的公开提示 ID 字符串。

Rust SDK：`upsert_prompt(prompt) -> Result<String>`。

### `host-agent-remove-prompt`

```json
{ "id": "project-rules" }
```

按插件内部 ID 幂等删除，成功 `value` 为 `null`。

Rust SDK：`remove_prompt(id) -> Result<()>`。

### `host-agent-emit-event`

```json
{
  "name": "index.ready",
  "data": { "files": 42 },
  "presentation": {
    "target": "main_event_list",
    "variant": "text",
    "tone": "success",
    "text": "索引已就绪"
  }
}
```

- `name`：非空的插件协议事件名。
- `data`：任意 JSON，缺省为 `null`。
- `presentation`：可选展示提示；没有 UI 的消费者可忽略。
- Host 会覆盖来源，注入 `source = { "type": "plugin", "id": <可信插件 ID> }`。Guest payload 中伪造的 source 不参与身份判断。
- 成功 `value`：`null`。

Rust SDK：`emit_event(event) -> Result<()>`；子视图导航使用 `navigate_view`，底层仍通过本 import 发布稳定事件。

## 实例状态 imports

状态只属于当前激活实例，卸载后消失，不是持久化 API。

### `host-state-get`

```json
{ "key": "cursor" }
```

成功 `value` 是此前保存的任意 JSON；不存在时为 `null`。key 必须满足 Host 的长度和字符约束。

Rust SDK：`get_state(key) -> Result<Option<Value>>`。

### `host-state-set`

```json
{
  "key": "cursor",
  "value": { "offset": 12 }
}
```

写入或替换任意 JSON，成功 `value` 为 `null`。该操作只改变当前 component 实例内存。

Rust SDK：`set_state(key, value) -> Result<()>`。

### `host-state-remove`

```json
{ "key": "cursor" }
```

成功 `value` 是被删除的旧 JSON；不存在时为 `null`。

Rust SDK：`remove_state(key) -> Result<Option<Value>>`。

## 插件服务 imports

### `host-service-upsert`

```json
{
  "name": "command.snapshot",
  "version": "1.0.0",
  "description": "返回命令注册表快照。"
}
```

- `name`：当前插件内稳定服务名。
- `version`：服务 JSON 契约的 SemVer，与插件 ABI 独立。
- `description`：可选开发者说明。
- Host 强制 owner 为当前插件 ID；请求不能指定 `plugin_id`。
- 成功 `value`：`null`。

Rust SDK：`upsert_service(service) -> Result<()>`。

### `host-service-remove`

```json
{ "name": "command.snapshot" }
```

幂等删除当前插件拥有的服务，成功 `value` 为 `null`。

Rust SDK：`remove_service(name) -> Result<()>`。

### `host-service-list`

查询全部：

```json
{ "plugin_id": null }
```

查询一个插件：

```json
{ "plugin_id": "command" }
```

成功 `value`：

```json
[
  {
    "plugin_id": "command",
    "name": "command.snapshot",
    "version": "1.0.0",
    "description": "返回命令注册表快照。"
  }
]
```

返回的 `plugin_id` 由 Host 服务目录注入。

Rust SDK：`list_services(plugin_id) -> Result<Vec<ServiceDescriptor>>`。

### `host-service-call`

```json
{
  "plugin_id": "command",
  "name": "command.snapshot",
  "payload": { "include_hidden": false }
}
```

- `plugin_id`：目标插件可信 ID。
- `name`：目标插件内服务名。
- `payload`：服务版本定义的任意 JSON，缺省为 `null`。
- 成功 `value`：服务自行定义的 JSON。
- Host 路由给目标 Guest 时会注入真实 `caller_id`，忽略 payload 中任何身份声明。
- 常见错误：目标/服务不存在、调用方不满足服务协议、目标 Guest 返回错误信封。

Rust SDK：`call_service(plugin_id, name, payload) -> Result<Value>`。

## 文件 imports

### `host-fs-read`

```json
{ "path": "skills/example/SKILL.md" }
```

路径相对插件目录解析，也可使用 manifest 明确允许的路径。Host canonicalize 请求路径和 allowlist，防止 `..`、符号链接等逃逸。成功 `value` 是 UTF-8 字符串；文件不存在、非 UTF-8、未声明 `fs_read` 或越界时失败。

Rust SDK：`read_file(path) -> Result<String>`。

### `host-fs-list`

```json
{ "path": "skills" }
```

成功 `value` 是按 path 排序的一层目录项：

```json
[
  { "path": "skills/example", "is_dir": true },
  { "path": "skills/index.json", "is_dir": false }
]
```

不递归。路径不是目录、越界或目录读取失败时返回失败信封。

Rust SDK：`list_dir(path) -> Result<Vec<FileEntry>>`。

## 进程 imports

所有进程操作要求 manifest `process_exec = true`。句柄只在当前激活实例内有效；Host 限制并发进程数、命令/参数/env 大小、单次写入和单行输出。

### `host-process-spawn`

```json
{
  "command": "example-server",
  "args": ["--stdio"],
  "env": { "LOG_LEVEL": "warn" },
  "cwd": ".",
  "inherit_stderr": false
}
```

- `command`：可执行文件名或绝对路径，不经过 shell。
- `args`：原样传递，不做字符串拆分。
- `env`：在 Host 保留的少量基础环境之上增加；Guest 不能继承完整宿主环境。
- `cwd`：可选，相对路径以插件目录为基准并接受 Host 路径校验。
- `inherit_stderr`：true 时接入 Lucia stderr，否则丢弃。
- 成功 `value`：`u64` 进程句柄。

Rust SDK：`spawn_process(spec) -> Result<u64>`。

### `host-process-write`

```json
{
  "handle": 1,
  "data": "{\"jsonrpc\":\"2.0\"}\n"
}
```

`data` 原样写入并 flush，不自动追加换行。未知句柄、超过单次写入上限或 stdin 关闭时失败。成功 `value` 为 `null`。

Rust SDK：`write_process(handle, data) -> Result<()>`。

### `host-process-read-line`

```json
{
  "handle": 1,
  "timeout_ms": 30000
}
```

`timeout_ms` 缺省时使用 Host 默认值，并被夹在安全范围内。成功 `value` 是不含换行符的一行字符串；stdout EOF 时为 `null`。超时、未知句柄、I/O 错误或单行超限返回失败信封。

Rust SDK：`read_process_line(handle, timeout_ms) -> Result<Option<String>>`。

### `host-process-kill`

```json
{ "handle": 1 }
```

终止并释放句柄，成功 `value` 为 `null`。未知句柄或终止失败时返回错误。Guest 应在 `deactivate` 主动清理；Host 丢弃 store 时也会 kill-on-drop。

Rust SDK：`kill_process(handle) -> Result<()>`。

## 模型完成 import

### `host-model-complete`

该 import 提供受 manifest 与应用配置共同约束的独立模型调用。请求拒绝未知字段：

```json
{
  "system": "生成结构化摘要",
  "messages": [
    {
      "role": "user",
      "content": [
        { "type": "text", "text": "待摘要的旧上下文" }
      ]
    }
  ],
  "max_tokens": 20000
}
```

成功 `value`：

```json
{
  "text": "模型生成的摘要",
  "usage": {
    "input_tokens": 1200,
    "output_tokens": 300,
    "total_tokens": 1500
  }
}
```

共同约束：

- manifest 必须声明 `model_completion = true`，应用必须注入已注册的模型网关和固定 provider/model。
- Guest 只能提交 system、provider-neutral messages 和期望输出上限，不能提交路由、工具、推理级别或 provider options。
- Host 强制空工具列表、`tool_choice = none` 和关闭推理，并把 `max_tokens` 收窄到应用上限。
- 空消息、超大请求、模型工具调用、空文本或底层模型错误都会返回失败信封。

Rust SDK：`complete_model(request) -> Result<ModelCompletionResponse>`。

## Agent Runtime import

### `host-agent-runtime-call`

`host-agent-runtime-call` 用一个 import 承载八种短控制面操作。顶层请求拒绝未知字段：

```json
{
  "operation": "status",
  "request": { "target": "Agent UUID" }
}
```

共同约束：

- `operation` 必须是下表值。
- `request` 缺省为 `null`，但需要空对象的操作应显式传 `{}`。
- 请求有独立字节上限。
- Host 先检查 manifest 的 spawn/observe/cancel 权限，再解析目标和调用 Runtime。
- target 只能是当前 controller 或其可管理后代。

| operation | request | 成功 `value` | Rust SDK |
| --- | --- | --- | --- |
| `identity` | `{}` | 透明 Agent ID 字符串 | `agent_identity()` |
| `spawn` | `{ "profile": "worker", "input": "任务" }` | AgentHandle | `spawn_agent(request)` |
| `continue` | `{ "target": "...", "input": "继续" }` | 新 AgentHandle | `continue_agent(request)` |
| `steer` | `{ "target": "...", "input": "先停止写文件" }` | `null` | `steer_agent(target, input)` |
| `status` | `{ "target": "..." }` | AgentSnapshot | `agent_status(target)` |
| `result` | `{ "target": "..." }` | AgentOutcome 或 `null` | `agent_result(target)` |
| `events` | `{ "target": "...", "limit": 128 }` | AgentEvent 数组 | `agent_events(target, limit)` |
| `cancel` | `{ "target": "..." }` | boolean | `cancel_agent(target)` |

AgentHandle 示例：

```json
{
  "id": "Agent UUID",
  "lineage": {
    "parent": "父 Agent UUID",
    "root": "controller UUID",
    "depth": 1
  }
}
```

`status` 返回示例：

```json
{
  "id": "Agent UUID",
  "lineage": {
    "parent": "父 Agent UUID",
    "root": "controller UUID",
    "depth": 1
  },
  "status": "running",
  "permissions": {
    "tools": {
      "mode": "allowlist",
      "tools": ["read_file", "search"]
    }
  }
}
```

全部工具权限编码为 `{ "mode": "all" }`。`status` 枚举是 `ready`、`queued`、`running`、`succeeded`、`failed` 或 `cancelled`。

`result` 未终态时为 `null`；三种终态分别编码为：

```json
{
  "status": "succeeded",
  "result": {
    "run_id": "run UUID",
    "final_text": "任务结果",
    "steps_used": 2,
    "usage": {
      "input_tokens": 100,
      "output_tokens": 20,
      "total_tokens": 120
    }
  }
}
```

```json
{ "status": "failed", "error": "错误文本" }
```

```json
{ "status": "cancelled" }
```

`spawn.profile` 必须同时在 manifest `capabilities.agent.profiles` 和应用 Host 注册表中；`input` 不能为空。Guest 不能提交 provider、model、tool allowlist 或资源上限。

`events.limit` 缺省为 `128`，Host 将其限制在 `1..=512`。第一次调用创建订阅并回放 Runtime 保留的近期事件，后续调用只非阻塞取出新增事件。WIT 不暴露阻塞式 `wait`，避免同步 component 调用长期占用 store。

## Guest exports

### `activate`

```wit
export activate: func(context-json: string) -> string;
```

输入：

```json
{
  "plugin_id": "echo",
  "metadata": { "channel": "stable" }
}
```

`plugin_id` 由 Host 从 manifest 注入；`metadata` 缺省为空对象。返回空字符串表示成功，非空字符串是加载错误。SDK JSON 解析失败也直接返回错误文本。

映射：`AgentPlugin::activate(host, context)`。

### `deactivate`

```wit
export deactivate: func() -> string;
```

返回空字符串表示成功，非空字符串表示清理错误。无论返回什么，调用方都不能假设 Guest 实例继续可用。

映射：`AgentPlugin::deactivate(host)`。

### `handle-service`

输入：

```json
{
  "caller_id": "可信调用方插件 ID",
  "name": "command.snapshot",
  "payload": {}
}
```

输出使用 Guest 服务信封，不带 Host `schema_version`：

```json
{ "ok": true, "value": { "commands": [] } }
```

或：

```json
{ "ok": false, "error": "未知服务" }
```

`caller_id` 由 Host 注入。JSON 解析失败与插件返回 `Err` 都变成 `ok = false`。

映射：`AgentPlugin::handle_service(host, call)`。

### `list-tools`

```wit
export list-tools: func() -> string;
```

输出是 ToolSpec JSON 数组，无工具时为 `[]`。它不是响应信封。

```json
[
  {
    "name": "echo",
    "description": "返回输入文本。",
    "input_schema": { "type": "object" }
  }
]
```

映射：`AgentPlugin::list_tools()`。

### `call-tool`

输入 ToolCall：

```json
{
  "id": "call-1",
  "name": "echo",
  "args": { "text": "hello" }
}
```

输出 ToolResult：

```json
{
  "call_id": "call-1",
  "name": "echo",
  "content": { "echo": "hello" },
  "is_error": false
}
```

`details` 是可选 UI JSON，不发送给模型。SDK 在输入无法解析时返回 `call_id = "invalid-call"`、`name = "invalid-tool"` 的错误 ToolResult；插件返回 Rust `Err` 时保留原 call ID/name 并转换为 `is_error = true`。

映射：`AgentPlugin::call_tool_with_host(host, call)`。

### `before-tool`

输入 ToolCall，输出使用 `type` 判别的 ToolDecision：

```json
{ "type": "allow" }
```

```json
{ "type": "block", "reason": "参数不允许" }
```

```json
{
  "type": "require_approval",
  "request_id": "approval-1",
  "reason": "需要执行命令",
  "poll_interval_ms": 100
}
```

其他变体是 `cancel_run` 和带完整 `call` 的 `rewrite`。输入解析失败时 SDK 返回 `block`，不会 panic 或执行未知调用。

映射：`AgentPlugin::before_tool(call)`。

### `after-tool`

```wit
export after-tool: func(result-json: string);
```

输入完整 ToolResult，无返回值。SDK 无法解析时忽略本次观察回调；它不会改变 Core 已持有的结果。

映射：`AgentPlugin::after_tool(result)`。

### `on-event`

```json
{
  "id": "event UUID",
  "run_id": "run UUID",
  "timestamp_ms": 0,
  "kind": "tool_finished",
  "step": 2,
  "payload": {}
}
```

`kind` 使用 snake_case，覆盖 run、turn、模型、用量、工具、steering、follow-up 与 step limit 事件。无返回值；SDK 无法解析时忽略该回调。

映射：`AgentPlugin::on_event(event)`。

### `load-context`

输入字段：`run_id`、`step`、`provider`、`model`、可选 `system` 和完整 `messages`。

透传：

```json
{ "context": null, "error": null }
```

替换：

```json
{
  "context": {
    "system": "压缩后的 system",
    "messages": []
  },
  "error": null
}
```

失败：

```json
{ "context": null, "error": "上下文压缩失败" }
```

`context` 是完整替换而非差量。输入解析或插件执行错误都写入 `error`，Host 不会发送本轮模型请求。

映射：`AgentPlugin::load_context(host, request)`。

### `describe-ui`

输出 UiDeclaration 数组，无 UI 时为 `[]`。每项包含 plugin/view ID、title、placement、size 和 focusable；Guest 的 `plugin_id` 应留空，Host 会注入可信值。

```json
[
  {
    "plugin_id": "",
    "view_id": "status",
    "title": "状态",
    "placement": "right",
    "size": { "width": 28, "height": null },
    "focusable": true
  }
]
```

映射：`AgentPlugin::describe_ui()`。

### `render-ui`

输入：

```json
{
  "plugin_id": "echo",
  "view_id": "status",
  "instance_id": null,
  "width": 28,
  "height": 10,
  "focused": false,
  "frame": 42
}
```

返回 UiFrame JSON 表示更新；返回空字符串表示本帧不更新。无效输入也返回空字符串。

```json
{
  "view_id": "status",
  "visible": true,
  "lines": [
    {
      "spans": [
        {
          "text": "Ready",
          "style": { "foreground": "green", "bold": true }
        }
      ]
    }
  ]
}
```

映射：`AgentPlugin::render_ui_with_host(host, request)`。

### `on-ui-input`

键盘事件：

```json
{
  "plugin_id": "echo",
  "view_id": "status",
  "instance_id": null,
  "event": {
    "type": "key",
    "code": "enter",
    "modifiers": []
  }
}
```

鼠标事件使用 `type = "mouse"`、`kind`、相对内容区的 `x`/`y`。无返回值；无效输入被 SDK 忽略。

映射：`AgentPlugin::on_ui_input_with_host(host, input)`。

## 错误转换总表

| 边界 | 输入解析失败 | 插件/Host 执行失败 |
| --- | --- | --- |
| Host import | `ok=false` Host 信封 | `ok=false` Host 信封 |
| `activate` / `deactivate` | 非空错误字符串 | 非空错误字符串 |
| `handle-service` | `ok=false` Guest 服务信封 | `ok=false` Guest 服务信封 |
| `call-tool` | 错误 ToolResult，使用 invalid call/name | 错误 ToolResult，保留原 call ID/name |
| `before-tool` | `block` 决策 | trait 本身不返回 Result |
| `load-context` | `error` 字段 | `error` 字段 |
| `render-ui` | 空字符串 | 无帧时空字符串 |
| `after-tool` / `on-event` / `on-ui-input` | 忽略回调 | 无返回通道 |

## 兼容规则

1. Host 当前只加载 manifest `api_version = "0.6.0"`，且要求 world 的全部 exports 存在。
2. 修改 WIT import/export、删除或改名函数、改变字符串返回含义时必须升级插件 ABI。
3. JSON 新字段必须可选并提供 serde 默认值；读取方必须忽略未知加法字段。
4. 删除、改名、收紧枚举或改变既有字段含义属于破坏性变化，需要升级对应协议。
5. Host 响应 `schema_version = 1`、插件 ABI、服务 SemVer、workspace crate 版本和 Plugin Manager lock schema 是独立版本。
6. 修改 `wit/plugin.wit` 时必须同步 Guest 内嵌 WIT、Host 绑定、UI fixture 和双边契约测试。

## 相关页面

- [Plugin SDK、Host 与 Manager API](/reference/rust-plugin)
- [插件开发](/development/plugin)
- [Manifest 与权限](/host/manifest-capabilities)
- [插件依赖与服务](/plugin/dependencies-services)
