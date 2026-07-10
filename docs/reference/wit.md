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

`host-agent-runtime-call` 使用一个带 `operation` 的 JSON 请求承载 `identity`、`spawn`、`status`、`result`、`cancel`、`send` 和 `try_receive`。Rust Guest SDK 将其封装为 `agent_identity`、`spawn_agent`、`agent_status`、`agent_result`、`cancel_agent`、`send_agent_message` 和 `try_receive_agent_message`，插件不需要手工拼接 operation。

该 import 不提供阻塞式 wait 或 receive。派生任务立即返回 handle，插件随后轮询状态或结果，避免在同步 component 调用期间占用 WASM store 锁。

## Guest exports

| WIT 函数 | `AgentPlugin` 方法 | 可选兼容 |
| --- | --- | --- |
| `activate` | `activate` | 旧 component 可缺失 |
| `deactivate` | `deactivate` | 旧 component 可缺失 |
| `handle-service` | `handle_service` | 旧 component 可缺失 |
| `list-tools` | `list_tools` | 必需 |
| `call-tool` | `call_tool_with_host` | 必需 |
| `before-tool` | `before_tool` | 必需 |
| `after-tool` | `after_tool` | 必需 |
| `on-event` | `on_event` | 必需 |
| `load-context` | `load_context` | 旧 component 可缺失 |
| `describe-ui` | `describe_ui` | 旧 component 可缺失 |
| `render-ui` | `render_ui` | 声明 UI 时必需 |
| `on-ui-input` | `on_ui_input` | 声明 UI 时必需 |

Rust SDK 的 `export_plugin!` 会生成这些导出、保持单例插件状态，并把解析或执行错误转换为 `ToolResult::error`。

## 兼容策略

- Host 当前支持 manifest `0.6.0`。
- `0.1.0` 到 `0.5.0` 作为兼容版本继续加载，但不能申请 Agent Runtime 权限。
- 新增 guest export 应先由 Host 探测为可选，再在主版本中收紧。
- 新增 JSON 字段必须使用 serde 默认值保持向后兼容。
