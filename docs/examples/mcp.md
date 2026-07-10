# 通用 MCP 插件

仓库中的 `examples/plugins/mcp-plugin` 是协议插件示例。Plugin Host 不理解 MCP；插件通过文件和进程 API 自己完成全部协议行为。

## 启动流程

<div class="arch-flow">activate
  -> list_dir / read_file
  -> spawn_process(command, args, env)
  -> initialize
  -> notifications/initialized
  -> tools/list
  -> upsert_tool(local id, public spec)
  -> emit_event("mcp.servers.connected")</div>

模型调用公开工具后：

```text
ToolCall(mcp__mastergo__get_node)
  -> Host owner route
  -> local id mastergo/get_node
  -> plugin tools/call
  -> stdio MCP Server
  -> ToolResult
```

## 配置格式

插件扫描 manifest metadata `config_dir` 指定目录内的 `.json` 文件。支持单 Server：

```json
{
  "command": "bunx",
  "args": [
    "@mastergo/magic-mcp",
    "--token=本地_TOKEN",
    "--url=https://mastergo.com",
    "--verbose"
  ],
  "env": {
    "NPM_CONFIG_REGISTRY": "https://registry.npmjs.org/"
  },
  "inheritStderr": true
}
```

也支持：

```json
{
  "mcpServers": {
    "mastergo": {
      "command": "bunx",
      "args": ["@mastergo/magic-mcp", "--token=本地_TOKEN"]
    }
  }
}
```

现有 stdio MCP 配置中的 `command`、`args`、`env` 和 `cwd` 会按原样传给通用进程 API。

## 为什么公开名和远端名分开

不同 Server 可能提供同名工具，而模型 provider 对名称长度和字符有限制。插件生成 provider-portable 的公开名，同时把 Server ID 与原始工具名保存在本地路由中。

Host 只保存 public name、owner、local id 三元组，不保存 MCP Server、JSON-RPC session 或分页 cursor。

## 使用真实原型

将 `config/mastergo.example.json` 另存为被忽略的 `config/mastergo.json`，填入本地 token，构建 component 后加载 `plugin.toml`。随后把原型地址或节点信息交给 Agent，模型会从动态工具定义中选择对应工具。
