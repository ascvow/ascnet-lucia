# 官方插件

官方插件与第三方插件使用同一套 WASM ABI、manifest 和 Host API，不拥有绕过权限或直接访问 Core 的内部接口。官方身份只表示该插件由 Lucia 仓库维护，并包含真实 component 路由测试。

插件版 TUI 与官方插件分开安装：

```bash
bun run install:tui
lucia plugin search
lucia plugin install mcp
```

`install:tui` 安装 TUI、原生 Command、原生 Skill 和原生 Context；其他官方插件与第三方插件使用相同的 Registry、本地目录或 GitHub Release 安装流程，只有用户显式安装且启用的插件才进入运行时。开发目录也可以通过 `--plugin-manifest` 临时加载。

仓库开发环境执行 `bun run install:all` 时，会把官方清单中的 bundle 更新到 `$LUCIA_HOME/official-plugins`。清单通过 `replaces` 声明插件改名关系，同步时只删除被明确替代的旧官方 bundle，避免新旧插件同时提供同一独占能力。Loader 也会自动扫描该目录和 `$LUCIA_HOME/plugins`，配置中的 `disabled_plugins` 仍可按 ID 排除自动发现的插件。

## 原生 Context

Context 是 TUI 默认原生能力，不经过 Plugin Host，也不需要安装 bundle。它在约 120k token 时静默清理旧工具结果，在约 167k token 时额外调用一次应用固定路由的模型，把较旧 API 轮次替换为结构化摘要并保留近期完整轮次；`[1m]` 模型使用对应的百万上下文水位。原生 `/compact` 命令会立即调用同一模型摘要流程，成功持久化压缩结果后当场替换当前 Session，不需要再发送一条消息。

Evidence 运行从 Genome 固定的 Artifact CAS 装配 Context Policy，策略 owner 固定为 `native-context`。实现位于 `crates/agent-core/context`，默认插件版与 `--no-default-features` 纯 Core 版行为一致。

## MCP

`mcp` 扫描 JSON 配置、启动 stdio MCP Server、完成初始化与工具发现，并把 Server 工具动态注册给 Agent。它不包含 MasterGo、Figma 等服务的业务逻辑。

```bash
bun run build:plugin:mcp
bun run test:plugin:mcp
```

Manifest：`examples/plugins/mcp-plugin/plugin.toml`。

## 原生 Skill

Skill 是 TUI 默认原生能力，不经过 Plugin Host，也不需要安装 bundle。普通运行递归扫描 `$LUCIA_HOME/skills`、项目 `skills` 和项目 `.lucia/skills` 中的 `SKILL.md`，解析 YAML frontmatter 的 `name` 与 `description` 并注入轻量索引；模型只有在任务匹配时才通过原生 `skill_read` 读取完整正文。

Evidence 运行不扫描可变目录，而是从 Genome 固定的 Artifact CAS 装配 Skill。Serve 只接受 Active 制品，Evaluation 可接受 Quarantined、Evaluated 或 Active 候选；可信使用证据来自 Core 注入的原生 `tool_finished` 事件。

实现位于 `crates/agent-core/skill`，默认插件版与 `--no-default-features` 纯 Core 版行为一致。

## 原生 Command

Command 是 TUI 默认原生状态机，负责斜杠命令补全、参数校验、执行编排和会话 Dialog，不经过 Plugin Host。内置命令包括 `/help`、`/resume`、`/new`、`/sessions`、`/clear`、`/compact` 与 `/exit`；`/quit` 是 `/exit` 的别名。`/resume` 和 `/sessions` 只先加载当前项目的轻量摘要，完整 Session 仍只在用户确认后按 revision 校验并加载。

实现位于 `crates/agent-tui/src/native_command.rs`，纯 Core 构建同样提供这些命令；`/compact` 在没有 Context Loader 时返回可见说明，不伪造压缩结果。

## Teammate 插件

Teammate 插件负责成员角色、稳定成员地址、有界邮箱、确认、dispatch 重试与消息注入规则，并通过 `teammate.mailbox@1.0.0` 向其他插件提供 owner 隔离的版本化服务。插件同时声明右侧团队摘要和全屏团队工作台，TUI 只负责通用视图渲染、焦点和导航。

Manifest：`examples/plugins/teammate-plugin/plugin.toml`。插件版 TUI 为它注入共享 Agent Runtime、controller profile 与受限 `worker` 派生配置。

## Plan 插件

Plan 插件提供结构化计划的整体更新、只读查询、状态校验、输入框上方的紧凑进度架和完整计划子视图。计划最多只能有一个 `in_progress` 步骤，空计划表示显式清空；状态只在当前插件实例内保存。

## 权限插件

权限插件提供 Agent 工具策略与输入区审批 UI。普通读取只允许工作区内的词法相对路径，`.env`、私钥、凭据目录和 `.git` 内容始终拒绝访问；写文件、Shell 和未知插件工具需要审批。界面纵向提供允许一次、允许相似调用、全部放行和取消，支持方向键加 `Enter`、`Y/S/C` 快捷键，以及 `Cmd+A` 开启全部放行。取消会结束当前 Agent 运行并保留会话；相似 Shell 调用按命令族匹配，写文件按父目录匹配；全部放行仍不会绕过敏感路径和工作区边界。

插件声明独占 `agent.tool-policy@1.0.0` 能力，因此 Host 会在其他插件完成工具 Rewrite 后执行最终检查。权限插件本身不申请文件、进程、HTTP、secret 或 Agent Runtime 能力，激活阶段不会联网。Shell 审批只表示用户接受该次完整宿主命令风险，不提供操作系统级隔离。

```bash
bun run build:plugin:plan
bun run test:plugin:plan
```

Manifest：`examples/plugins/plan-plugin/plugin.toml`。

## 统一验证

```bash
bun run build:plugin:official
bun run test:plugin:official
```

Context、MCP、Skill、Command、Teammate、Plan 和 Permission 都以独立插件 workspace 构建，不加入原生 Workspace；每个 bundle 都包含独立 `cdylib` component。它们的扫描、解析、协议和业务规则不会进入 Agent Core 或 Plugin Host。
