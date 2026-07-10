# 官方插件

官方插件与第三方插件使用同一套 WASM ABI、manifest 和 Host API，不拥有绕过权限或直接访问 Core 的内部接口。官方身份只表示该插件由 Lucia 仓库维护，并包含真实 component 路由测试。

使用前需要安装支持插件的 TUI：

```bash
bun run install:tui:plugins
```

## MCP

`mcp` 扫描 JSON 配置、启动 stdio MCP Server、完成初始化与工具发现，并把 Server 工具动态注册给 Agent。它不包含 MasterGo、Figma 等服务的业务逻辑。

```bash
bun run build:plugin:mcp
bun run test:plugin:mcp
```

Manifest：`examples/plugins/mcp-plugin/plugin.toml`。

## Skill

`skill` 递归扫描 `skills_dir` 中的 `SKILL.md`，解析 YAML frontmatter 的 `name` 和 `description`，并注入一份轻量索引。模型只有在任务匹配时才通过 `skill_read` 读取完整正文。

```bash
bun run build:plugin:skill
bun run test:plugin:skill
```

Manifest：`examples/plugins/skill-plugin/plugin.toml`。

## 统一验证

```bash
bun run build:plugin:official
bun run test:plugin:official
```

MCP 和 Skill 都是独立 `cdylib` crate，不加入原生 Workspace。它们的扫描、解析、协议和业务规则不会进入 Agent Core 或 Plugin Host。
