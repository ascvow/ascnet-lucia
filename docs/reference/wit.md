# WIT ABI 0.6

当前 package：

```wit
package ascnet:lucia-plugin@0.6.0;
```

ABI 使用 JSON 字符串承载 serde 类型。完整源文件位于 `wit/plugin.wit`，下面列出稳定函数面。

## Host imports

| WIT 函数 | Rust SDK |
| --- | --- |
| `host-agent-upsert-tool` | `upsert_tool` |
| `host-agent-remove-tool` | `remove_tool` |
| `host-agent-upsert-prompt` | `upsert_prompt` |
| `host-agent-remove-prompt` | `remove_prompt` |
| `host-agent-emit-event` | `emit_event` |
| `host-state-get` | `get_state` |
| `host-state-set` | `set_state` |
| `host-state-remove` | `remove_state` |
| `host-service-upsert` | `upsert_service` |
| `host-service-remove` | `remove_service` |
| `host-service-list` | `list_services` |
| `host-service-call` | `call_service` |
| `host-fs-read` | `read_file` |
| `host-fs-list` | `list_dir` |
| `host-process-spawn` | `spawn_process` |
| `host-process-write` | `write_process` |
| `host-process-read-line` | `read_process_line` |
| `host-process-kill` | `kill_process` |
| `host-agent-runtime-call` | Agent Runtime 的类型化短控制面方法 |

`host-agent-runtime-call` 使用一个带 `operation` 的 JSON 请求承载 `identity`、`spawn`、`continue`、`steer`、`status`、`result`、`events` 和 `cancel`。Rust Guest SDK 将其封装为 `agent_identity`、`spawn_agent`、`continue_agent`、`steer_agent`、`agent_status`、`agent_result`、`agent_events` 和 `cancel_agent`，插件不需要手工拼接 operation。

该 import 不提供阻塞式 wait。派生与续跑立即返回 handle，插件随后轮询状态、结果或事件，避免在同步 component 调用期间占用 WASM store 锁。`events` 首次调用会订阅目标并回放 Runtime 保留的最近事件，后续调用只非阻塞取出新增事件。teammate 邮箱不是 WIT Agent Runtime 的一部分。

## Guest exports

| WIT 函数 | `AgentPlugin` 方法 | 当前 ABI |
| --- | --- | --- |
| `activate` | `activate` | 必需 |
| `deactivate` | `deactivate` | 必需 |
| `handle-service` | `handle_service` | 必需 |
| `list-tools` | `list_tools` | 必需 |
| `call-tool` | `call_tool_with_host` | 必需 |
| `before-tool` | `before_tool` | 必需 |
| `after-tool` | `after_tool` | 必需 |
| `on-event` | `on_event` | 必需 |
| `load-context` | `load_context` | 必需 |
| `describe-ui` | `describe_ui` | 必需；无 UI 时返回空数组 |
| `render-ui` | `render_ui` | 声明 UI 时必需 |
| `on-ui-input` | `on_ui_input` | 声明 UI 时必需 |

Rust SDK 的 `export_plugin!` 会生成这些导出、保持单例插件状态，并把解析或执行错误转换为 `ToolResult::error`。

## 版本策略

- Host 仅支持 manifest `0.6.0`，所有 WIT export 必须完整存在。
- 修改 WIT import/export 或删除、改名 JSON 字段时必须升级 manifest ABI，旧 ABI component 不由当前 Host 兼容加载。
- 新增 JSON 字段必须使用 serde 默认值保持向后兼容。
- Host 能力响应包含 `schema_version = 1`；Guest 必须忽略未知加法字段，Host 必须对新增可选请求字段提供默认值。
- `agent-plugin` 的独立 ABI 契约测试负责验证旧最小请求、加法字段和响应信封兼容性。

| 版本 | 所有者 | 兼容含义 |
| --- | --- | --- |
| Workspace crate 版本 | Rust workspace | Rust 包发布版本，不代表插件 ABI |
| Manifest `api_version = 0.6.0` | Plugin Host | WIT world、完整导出和宿主能力契约 |
| `[[provides]].version` | 具体插件 | 插件间 JSON 服务的独立 SemVer |
| Lock `schema_version` | Plugin Manager | 本地插件锁文件格式，与运行时 ABI 独立 |
