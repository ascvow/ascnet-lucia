# 工具 owner 路由

## 注册表模型

每个插件拥有独立贡献注册表：

```text
公开工具名                 owner       插件本地 ID
mcp__design__get_node      plugin-7    design/get_node
```

公开名称发送给模型；本地 ID 只在调用 owner component 前写回 `ToolCall.name`。插件返回结果后，Host 恢复公开名称。

## 路由建立

`CompositePluginHost::list_tools()`：

1. 获取每个子宿主的当前工具快照。
2. 校验 provider-portable 工具名。
3. 检测跨插件重名。
4. 原子替换公开名称到 owner 的路由表。

`call_tool()` 随后通过 HashMap 直接定位 owner，不逐个询问插件。

## 动态更新

插件可以在生命周期内 `upsert_tool` 或 `remove_tool`。Agent 在每个 ReAct step 开始时重新读取工具快照，所以模型调用的工具总有对应 owner 路由。

同一步中不要在模型收到工具定义后立即删除该工具。若确实发生，Host 返回未处理，Core 会把未知工具错误交给模型。

## 重名策略

Host 不自动改名。命名空间属于插件协议实现，例如 MCP 插件可以生成：

```text
mcp__<server-id>__<remote-tool-name>
```

自动覆盖会使模型看到的工具和实际 owner 不确定，因此重复公开名称会直接阻止模型请求。
