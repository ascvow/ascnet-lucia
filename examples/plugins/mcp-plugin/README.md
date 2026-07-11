# Lucia 官方 MCP 插件

这是 Lucia 首批官方插件之一，不包含 MasterGo、Figma 或其他服务的业务逻辑。它在启动时扫描 MCP JSON 配置，启动其中声明的 stdio Server，执行 MCP 初始化和工具发现，然后通过 Lucia 宿主 API 动态注册工具。模型调用公开工具后，Plugin Host 根据 owner 路由回本插件，本插件再向原 Server 发送 `tools/call`。

## 配置

正式 manifest 默认扫描 `config` 目录中的所有 `.json` 文件。每个文件可以直接使用单 Server 结构：

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

也支持常见的多 Server 结构：

```json
{
  "mcpServers": {
    "mastergo": {
      "command": "bunx",
      "args": ["@mastergo/magic-mcp", "--token=本地_TOKEN", "--url=https://mastergo.com"]
    }
  }
}
```

`command` 和 `args` 会按原样传给宿主进程 API，因此已有的 stdio MCP 配置无需协议适配。公开工具名采用 `mcp__<server-id>__<tool-name>`；插件内部仍保存 Server 原始工具名。

默认安装后，将 `$LUCIA_HOME/official-plugins/mcp/config/mastergo.example.json` 另存为同目录下的 `mastergo.json` 并填写本地 token。没有有效配置时插件保持待配置状态，不会阻止 TUI 启动。`config/*.json` 已被忽略，不应提交 token。

## 构建和运行

```bash
bun run install:tui
lucia
```

默认安装会构建 MCP component 并同步官方 bundle；`lucia` 启动时自动加载，无需传入 `--plugin-manifest`。

Agent 启动后会直接看到 MCP Server 返回的工具定义。提供 MasterGo 原型地址或节点信息时，模型可以选择对应的 `mcp__mastergo__...` 工具访问原型。

## 路由测试

`plugin.test.toml` 使用仓库内的 Bun 假 MCP Server，不需要 token 或网络：

```bash
cargo test --manifest-path \
  examples/plugins/mcp-plugin/smoke-tests/Cargo.toml \
  component_discovers_and_calls_stdio_tool \
  -- --ignored --nocapture
```

该测试验证配置扫描、子进程启动、`initialize`、`tools/list`、动态工具注册以及 `tools/call` 回程路由。
