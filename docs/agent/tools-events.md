# 工具与事件

## 原生工具

原生工具实现 `agent_tool::Tool`，并注册到 `ToolRegistry`：

```rust
use anyhow::Result;
use agent_tool::{JsonTool, ToolRegistry, ToolSpec};
use serde_json::json;

let mut tools = ToolRegistry::new();
tools.register(JsonTool::new(
    ToolSpec::new("status", "读取状态", json!({"type": "object"})),
    |_args| async { Ok(json!({"status": "ready"})) },
))?;
agent.set_tools(tools);
```

`Agent::tool_specs()` 合并原生工具与 `AgentExtension::list_tools()`。公开名称重复会在模型请求前报错。

需要展示长时间运行工具的实时进度时，工具可以覆盖 `Tool::call_with_output()`，通过
`ToolOutputSink` 发布带调用 ID、stdout/stderr 来源和文本的 `ToolOutputDelta`。Core 会按
接收顺序写入 `AgentEventKind::ToolOutputDelta`，最终 `ToolResult` 的模型语义保持不变。
内置 `shell` 工具会持续读取两个输出流，每个流最多保留并展示 100 KiB。

## 扩展工具生命周期

`AgentExtension` 提供以下通用钩子：

| 方法 | 作用 |
| --- | --- |
| `prompt_messages` | 为本次模型请求贡献消息 |
| `list_tools` | 返回扩展工具快照 |
| `call_tool` | 执行扩展拥有的工具 |
| `before_tool` | 允许、阻止或重写调用 |
| `after_tool` | 观察最终工具结果 |
| `on_event` | 观察 Core 生命周期事件 |
| `drain_events` | 发布结构化扩展事件 |

Core 不知道扩展由原生对象还是 WASM component 实现。

## EventSink

内置 sink：

- `NoopEventSink`：丢弃事件。
- `InMemoryEventSink`：测试和嵌入式读取。
- `JsonlEventSink`：按行追加 JSON。
- `CompositeEventSink`：按顺序转发给多个 sink。

```rust
let mut sinks = CompositeEventSink::new();
sinks.push(Arc::new(JsonlEventSink::new("runs/events.jsonl")));
sinks.push(Arc::new(InMemoryEventSink::new()));
agent.set_event_sink(Arc::new(sinks));
```

`AgentEventKind::Extension` 的 payload 由扩展定义。Plugin Host 会为 WASM 插件事件注入可信 `source`。
