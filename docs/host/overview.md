# Plugin Host API

Plugin Host 是通用插件内核：加载 component、执行生命周期、检查权限、保存贡献并把公开工具路由到 owner。它不解析 MCP、Skill 或任何业务格式。

## 加载

```rust
use agent_plugin_host::wasm::{
    load_wasm_plugins, WasmPluginHost, WasmPluginLimits,
};

let plugin = WasmPluginHost::load_from_manifest("plugins/demo/plugin.toml").await?;

let limits = WasmPluginLimits {
    fuel: 80_000_000,
    fuel_yield_interval: Some(250_000),
    max_memory_bytes: 64 * 1024 * 1024,
};
let plugin = WasmPluginHost::load_from_manifest_with_limits(
    "plugins/demo/plugin.toml",
    limits,
).await?;
```

`WasmPluginHost` 公开可信 `id()`、`manifest()` 和 `deactivate()`。默认限制为每次调用 `50_000_000` fuel，并将单个线性内存限制为 `64 MiB`；应用可通过 `WasmPluginLimits` 调整。

## 组合宿主

```rust
let mut host = CompositePluginHost::new();
host.push(Arc::new(first));
host.push(Arc::new(second));

let ids = host.host_ids();
let plugin = host.get("first");
let owner = host.tool_owner("public_tool")?;
let removed = host.remove("first");
host.shutdown().await?;
```

| API | 行为 |
| --- | --- |
| `hosts()` | 返回加载顺序中的共享子宿主 |
| `host_ids()` | 返回具有稳定 ID 的子宿主 |
| `get(id)` | 克隆指定子宿主的 `Arc` |
| `remove(id)` | 移除并返回宿主，不自动 shutdown |
| `clear()` | 清空并返回全部宿主 |
| `tool_owner(name)` | 查询最近一次工具快照中的 owner |
| `services()` | 返回全部插件公开的服务目录 |
| `call_service(call)` | 按目标插件 ID 路由服务调用 |
| `shutdown()` | 按加载顺序的反向请求全部宿主清理 |

移除和清空返回 `Arc<dyn PluginHost>`，调用方可以决定并发关闭、错误处理和超时策略。

## PluginHost trait

`PluginHost` 继承 Core 的 `AgentExtension`，并增加：

- 可选稳定 `id`
- `ui_declarations`
- `render_ui`
- `on_ui_input`
- `services`
- `call_service`
- `shutdown`

因此同一个组合宿主既能挂到 Agent，也能交给 TUI 循环。
