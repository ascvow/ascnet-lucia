# 官方插件

官方插件与第三方插件使用同一套 WASM ABI、manifest 和 Host API，不拥有绕过权限或直接访问 Core 的内部接口。官方身份只表示该插件由 Lucia 仓库维护，并包含真实 component 路由测试。

默认安装命令会构建插件版 TUI、同步官方 bundle，并注册 zsh PATH：

```bash
bun run install:tui
```

官方插件安装在 `$LUCIA_HOME/official-plugins`，启动 `lucia` 时自动加载，无需传入 `--plugin-manifest`。显式配置同 ID 插件时，显式版本优先。

## Context

`context` 提供官方上下文管理与压缩能力。它在约 120k token 时清理旧工具结果，在约 167k token 时把较旧 API 轮次替换为结构化摘要，并保留近期完整轮次；`[1m]` 模型使用对应的百万上下文水位。Command 插件内置的 `/compact` 会立即调用 `context.compact`，成功持久化压缩结果后当场替换当前 Session，不需要再发送一条消息。

```bash
bun run build:plugin:context
bun run test:plugin:context
```

Manifest：`examples/plugins/context-plugin/plugin.toml`。

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

## Command

`command` 提供斜杠命令注册表、输入预览、参数校验、候选补全和执行路由。内置命令包括 `/help`、`/resume`、`/new`、`/sessions`、`/clear`、`/compact` 与 `/exit`；`/quit` 是 `/exit` 的别名。`/resume` 和 `/sessions` 使用插件 Dialog 展示当前项目的轻量会话摘要，完整 Session 只由 TUI 在用户确认后加载。

```bash
bun run build:plugin:command
bun run test:plugin:command
```

Manifest：`examples/plugins/command-plugin/plugin.toml`。公开协议与开发 SDK 分别位于 `crates/command-protocol` 和 `crates/command-sdk`。

## 统一验证

```bash
bun run build:plugin:official
bun run test:plugin:official
```

Context、MCP、Skill 和 Command 都以独立插件 workspace 构建，不加入原生 Workspace；每个 bundle 都包含独立 `cdylib` component。它们的扫描、解析、协议和业务规则不会进入 Agent Core 或 Plugin Host。
