# 官方插件

官方插件与第三方插件使用同一套 WASM ABI、manifest 和 Host API，不拥有绕过权限或直接访问 Core 的内部接口。官方身份只表示该插件由 Lucia 仓库维护，并包含真实 component 路由测试。

插件版 TUI 与官方插件分开安装：

```bash
bun run install:tui
lucia
```

`install:tui` 安装 TUI、原生 Command、原生 Skill 和原生 Context；启动后输入 `/plugins`，官方插件与第三方插件使用相同的 Registry、本地目录或 GitHub Release 人工安装流程，只有用户显式安装且启用的插件才进入新 Session。开发目录也可以通过 `--plugin-manifest` 临时加载。

仓库开发环境执行 `bun run install:all` 时，会把官方清单中的 bundle 更新到 `$LUCIA_HOME/official-plugins`。清单通过 `replaces` 声明插件改名关系，同步时只删除被明确替代的旧官方 bundle，避免新旧插件同时提供同一独占能力。Loader 也会自动扫描该目录和 `$LUCIA_HOME/plugins`，配置中的 `disabled_plugins` 仍可按 ID 排除自动发现的插件。

## 原生 Context

Context 是 TUI 默认原生能力，不经过 Plugin Host，也不需要安装 bundle。它在约 120k token 时静默清理旧工具结果，在约 167k token 时额外调用一次应用固定路由的模型，把较旧 API 轮次替换为结构化摘要并保留近期完整轮次；`[1m]` 模型使用对应的百万上下文水位。原生 `/compact` 命令会立即调用同一模型摘要流程，成功持久化压缩结果后当场替换当前 Session，不需要再发送一条消息。

实现位于 `crates/agent-core/context`，默认插件版与 `--no-default-features` 纯 Core 版行为一致。

## MCP

`mcp` 扫描 JSON 配置、启动 stdio MCP Server、完成初始化与工具发现，并把 Server 工具动态注册给 Agent。它不包含 MasterGo、Figma 等服务的业务逻辑。

```bash
bun run build:plugin:mcp
bun run test:plugin:mcp
```

Manifest：`examples/plugins/mcp-plugin/plugin.toml`。

## 原生 Skill

Skill 是 TUI 默认原生能力，不经过 Plugin Host，也不需要安装 bundle。普通运行递归扫描 `$LUCIA_HOME/skills`、项目 `skills` 和项目 `.lucia/skills` 中的 `SKILL.md`，解析 YAML frontmatter 的 `name` 与 `description` 并注入轻量索引；模型只有在任务匹配时才通过原生 `skill_read` 读取完整正文。


实现位于 `crates/agent-core/skill`，默认插件版与 `--no-default-features` 纯 Core 版行为一致。

## 原生 Command

Command 是 TUI 默认原生状态机，负责斜杠命令补全、参数校验、执行编排和会话 Dialog，不经过 Plugin Host。插件构建还提供人工管理命令 `/plugins`；它不进入 Plugin Host。其余内置命令包括 `/help`、`/resume`、`/new`、`/sessions`、`/clear`、`/compact` 与 `/exit`；`/quit` 是 `/exit` 的别名。`/resume` 和 `/sessions` 只先加载当前项目的轻量摘要，完整 Session 仍只在用户确认后按 revision 校验并加载。

实现位于 `crates/agent-tui/src/native_command.rs`，纯 Core 构建提供除 `/plugins` 外的原生命令；`/compact` 在没有 Context Loader 时返回可见说明，不伪造压缩结果。

## Teammate 插件

Teammate 插件负责成员角色、稳定成员地址、有界邮箱、确认、dispatch 重试与消息注入规则，并通过 `teammate.mailbox@1.0.0` 向其他插件提供 owner 隔离的版本化服务。插件同时声明右侧团队摘要和全屏团队工作台，TUI 只负责通用视图渲染、焦点和导航。

Manifest：`examples/plugins/teammate-plugin/plugin.toml`。插件版 TUI 为它注入共享 Agent Runtime、controller profile 与受限 `worker` 派生配置；worker 模板继承同一个动态 Plugin Host，因此只读工具仍会经过权限插件与 Host 启动期门禁。

## Plan 插件

Plan 插件提供结构化计划的整体更新、只读查询、状态校验、输入框上方的紧凑进度架和完整计划子视图。计划最多只能有一个 `in_progress` 步骤，空计划表示显式清空；状态只在当前插件实例内保存。

## 权限插件

权限插件提供 Agent 工具策略与输入区审批 UI。它只读取 Host 绑定的最终权限 scope：纯内部 `Control` 直接放行，`Read`、`Write`、`Shell`、`Network` 与 `Unknown` 进入当前审批模式。Host 会按 component 已授的进程、文件、模型、Agent 和宿主动作能力抬高 manifest `tool_scopes` 下限，高权限插件不能用 `Control` 跳过审批。可信读写 scope 携带路径时只允许工作区内词法相对路径，`.env`、私钥、凭据目录和 `.git` 内容始终拒绝访问。审批卡片按 scope 展示脱敏摘要、原因、作用范围和风险，支持方向键加 `Enter` 及 `A/S/D/Esc` 快捷键；会话级放行只适用于携带可验证顶层路径的同目录只读调用。`/permission` 可切换“手动审批、帮我审批、全部放行”，模式按项目隔离。模型代审只接受无额外字段的最小 JSON：`allow` 不携带原因，`deny` 必须携带非空原因；待审批状态按 Core 绑定的 `(run_id, call_id)` 隔离并发 Agent。拒绝或异常只阻止当前调用，详细原因不写入 Session，一次性说明仅作为下一轮的工具结果让对应 Agent 调整方案继续，不会被提升为 Developer 指令。全部放行下 Shell 仍拥有当前系统用户的完整原生权限，界面会持续警示。

插件声明独占 `agent.tool-policy@1.0.0` 能力，因此 Host 会在其他插件完成工具 Rewrite 后执行最终检查。权限插件不申请文件、进程、HTTP、secret 或 Agent Runtime 能力；模型代审只申请受控 `model_completion`，命令输入清理只申请通用 `surface_actions`。Shell 审批只表示用户或审核模型接受该次完整宿主命令风险，不提供操作系统级隔离。

```bash
bun run build:plugin:plan
bun run test:plugin:plan
```

Manifest：`examples/plugins/plan-plugin/plugin.toml`。

## 记忆插件

记忆插件跨会话保留结论，并在每次模型请求前注入上下文。它只申请 `storage` 能力，读写自己的
私有持久存储，不触达用户文件、网络或子进程。

记忆分两个作用域：`project` 跟随当前项目，`user` 跨项目共享。工具默认写入 `project`——把项目
事实写进用户级会污染其他项目，因此提示明确要求只有确属跨项目的个人偏好才用 `user`。宿主未提供
项目身份时插件降级为只用 `user`，而不是把项目数据写进用户级。

三个工具：`memory_record` 记录、`memory_list` 查询完整清单、`memory_forget` 删除过时结论。
单条记忆有字数上限，同作用域内正文相同的记忆不重复记录，每个作用域有条数上限。

插件不注册常驻提示，使用时机全部写在工具描述里；只有记忆正文本身会被注入，没有记忆时不注入
任何内容。注入有硬预算：超出时优先保留最近的记忆并提示用 `memory_list` 查看全部，而不是让记忆
无声吃掉上下文。记忆由模型自己写入，属于不可信内容，因此注入时会声明它们只是背景事实、不是
指令，且与系统提示或用户当前请求冲突时以后者为准——否则一条被污染的记忆会在之后每一轮持续
生效。这一句无法由工具描述承担，是唯一保留的说明性文字。

`/memory` 打开记忆面板查看当前记忆。

```bash
bun run build:plugin:memory
bun run test:plugin:memory
```

Manifest：`examples/plugins/memory-plugin/plugin.toml`。

## 知识库插件

知识库插件把当前项目的历史对话蒸馏成带出处的条目，检索命中后可按条目回溯对话原文。
它与记忆插件互补：记忆保存**结论**，容量小、每轮注入；知识库保存**线索**，平时不进
上下文，需要时才检索。记忆回答「我该记住什么」，知识库回答「这件事我以前是怎么处理的」。

### 为什么是词法检索而不是向量

语料是用户自己的对话记录，查询词与正文共享同一批标识符、文件路径、报错串和命令名——
这正是精确词匹配最强、而稠密向量最弱的场合。向量本该提供的语义归一化，已经由索引期的
模型摘要完成，那一步做一次、可审查，比一个不透明的向量空间更适合这个规模的语料。

平台条件也不支持向量：宿主没有嵌入接口，插件单次调用只有 5000 万 fuel、64 MiB 线性
内存，在 WASM 里跑嵌入模型不可行；而为此新增宿主嵌入 API 会引入新的 provider 集成、
密钥管理和每次索引的调用成本。

检索实现为两段式：先用子串命中做召回并顺带统计精确的文档频率，再只对候选集算 BM25。
中文没有可用分词器，按相邻字符二元组切分；ASCII 按字母数字段切分并小写化，因此大小写
不同的标识符仍能召回。

### Agent 自主选择索引范围

每次 TUI 成功完成运行并持久化会话后，插件会收集尚未评估或修订号已经变化的最近 8 个
会话。每个候选只读取末尾 8 条消息，并把单个预览限制在 2400 字符内；受控模型只负责从
这些可信候选 ID 中选择值得知识库化的会话，可以全部跳过，也可以选择多个。成功评估过的
修订不会重复付费，内容变化后才重新判断。

选中的会话进入原有待办队列，并在该次 TUI 结束事件中立即蒸馏 1 个，保证下一轮即可检索；
`core.run.finish` 继续推进最多 2 个已有待办，`kb_status` 推进 1 个。`kb_index` 保留为用户
明确要求全量补建或重建时的入口，只登记待办并立即返回。

已索引且修订号未变的会话不会重复登记；会话有新内容时重新蒸馏并替换该会话的旧条目，
不会留下多份。来源会话被删除时对应条目一并移除并停止跟踪。单个会话失败只跳过该会话并
记录原因，不中断整批——一个损坏的历史会话不应当让整个知识库停止推进。

### 权限

插件申请 `sessions`、`storage` 与 `model_completion`。其中 `sessions` 会把它**全部工具**
的权限下限抬到 `unknown`，因此 `kb_search` 这类纯检索调用也会进入当前审批模式。这是刻意的：
会话内容是 Agent 历史上见过的一切的并集，涵盖文件正文、shell 输出与网络返回，按只读文件
对待会低估覆盖面。

自动评估和条目蒸馏都会阅读历史对话，而历史对话里可能含有注入文本。两类模型输出都按
严格结构解析，多余字段直接拒绝；自动评估返回的会话 ID 还必须属于本批可信候选，不能由
模型扩大读取范围。检索结果只作为工具返回值交给模型，不注入知识正文。知识库非空时仅注入
一行带条目数的能力提示，让主 Agent 自行判断何时使用 `kb_search` 与 `kb_read`；空库不注入。

`/kb` 打开知识库面板查看条目数与索引进度。

```bash
bun run build:plugin:knowledge
bun run test:plugin:knowledge
```

Manifest：`examples/plugins/knowledge-plugin/plugin.toml`。

## 统一验证

```bash
bun run build:plugin:official
bun run test:plugin:official
```

Context、MCP、Skill、Command、Teammate、Plan 和 Permission 都以独立插件 workspace 构建，不加入原生 Workspace；每个 bundle 都包含独立 `cdylib` component。它们的扫描、解析、协议和业务规则不会进入 Agent Core 或 Plugin Host。
