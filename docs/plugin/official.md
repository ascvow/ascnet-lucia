# 官方插件

官方插件与第三方插件使用同一套 WASM ABI、manifest 和 Host API，不拥有绕过权限或直接访问 Core 的内部接口。官方身份只表示该插件由 Lucia 仓库维护，并包含真实 component 路由测试。

插件版 TUI 与官方插件分开安装：

```bash
bun run install:tui
lucia
```

`install:tui` 安装 TUI、原生 Command、原生 Skill 和原生 Context；启动后输入 `/plugins`，官方插件与第三方插件使用相同的 Registry、本地目录或 GitHub Release 人工安装流程，只有用户显式安装且启用的插件才进入新 Session。开发目录也可以通过 `--plugin-manifest` 临时加载。

仓库开发环境执行 `bun run install:all` 时，会把官方清单中的 bundle 更新到 `$LUCIA_HOME/official-plugins`。清单通过 `replaces` 声明插件改名关系，同步时只删除被明确替代的旧官方 bundle，避免新旧插件同时提供同一独占能力。`retired` 声明停止默认分发的 ID，安装时把对应旧官方 bundle 移入 `$LUCIA_HOME/disabled-official-plugins/<id>-<随机后缀>/<id>`，保留文件和用户设置但不再自动加载。Loader 只自动扫描 `$LUCIA_HOME/official-plugins` 和 `$LUCIA_HOME/plugins`，配置中的 `disabled_plugins` 仍可按 ID 排除自动发现的插件。手动安装和受管理插件不由上述迁移修改。

## 原生 Context

Context 是 TUI 默认原生能力，不经过 Plugin Host，也不需要安装 bundle。它在估算上下文达到当前模型最大窗口的 80% 时开始自动压缩：先静默清理旧工具结果，仍达到该水位时再调用当前模型，把较旧 API 轮次替换为结构化摘要并保留近期完整轮次。例如，200k 窗口对应 160k token，1m 窗口对应 800k token；未声明窗口时按 200k 兜底。切换模型会同步更新压缩器的路由与窗口，并清除旧压缩缓存。原生 `/compact` 命令不受自动水位限制，会立即调用同一模型摘要流程，成功持久化压缩结果后当场替换当前 Session，不需要再发送一条消息。摘要请求是独立调用：使用固定的压缩 system 提示、不携带工具、禁用推理，并固定关闭提示缓存保留——它的前缀在压缩完成后就不复存在，写入的缓存条目永远不会被读到，只会挤占主会话的缓存分区。会话路由标识仍随请求下发，使同一次运行内的摘要落在同一台机器上。

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

只有 `$LUCIA_HOME/skills` 由应用创建：`lucia --init` 和普通启动都会幂等创建该目录，使首次安装后就有可直接放入 `SKILL.md` 的入口。项目级 `skills/` 与 `.lucia/skills` 属于用户工作区，存在才扫描，不会自动生成。空目录不改变模型可见行为：不产生索引提示，也不注册 `skill_read`。


实现位于 `crates/agent-core/skill`，默认插件版与 `--no-default-features` 纯 Core 版行为一致。

## 原生 Command

Command 是 TUI 默认原生状态机，负责斜杠命令补全、参数校验、执行编排和会话 Dialog，不经过 Plugin Host。插件构建还提供人工管理命令 `/plugins`；它不进入 Plugin Host。其余内置命令包括 `/help`、`/resume`、`/new`、`/sessions`、`/clear`、`/compact` 与 `/exit`；`/quit` 是 `/exit` 的别名。`/resume` 和 `/sessions` 只先加载当前项目的轻量摘要，完整 Session 仍只在用户确认后按 revision 校验并加载。

实现位于 `crates/agent-tui/src/native_command.rs`，纯 Core 构建提供除 `/plugins` 外的原生命令；`/compact` 在没有 Context Loader 时返回可见说明，不伪造压缩结果。

## Teammate 插件

Teammate 插件负责成员角色、稳定成员地址、有界邮箱、确认、dispatch 重试与消息注入规则，并通过 `teammate.mailbox@1.0.0` 向其他插件提供 owner 隔离的版本化服务。插件声明输入框上方的成员架、全屏团队工作台和成员会话的输入路由；成员架与工作台由 TUI 通用视图渲染，成员会话则由 TUI **原生渲染**。成员架平时收起成一行摘要（成员数、运行数、未读数），`Tab` 聚焦后自动展开成员列表，`Esc` 返回输入后自动收起；展开态 `↑`/`↓` 选择、`Enter` 打开选中成员的会话、`w` 打开工作台、`Space` 折叠，点击成员行等价于选中并打开，点击摘要行等价于 `Space`。没有成员时整个架子隐藏。

`teammate_result` 返回的终态是 `{"status":"succeeded","result":{"final_text":…}}`（失败为 `status: failed` 加 `error`）。TUI 按工具结果的样式渲染它：摘要行取正文首行，卡片里预览前 8 行，单击展开看到更多，因此成员的长报告不会一次铺满时间线。

成员会话与主 Agent 共用消息布局缓存、Markdown、内置工具卡片、滚动条、输入框布局与纯文本编辑、状态/跳转提示以及流式揭示节拍。插件不再为成员会话逐帧生成文本帧；TUI 使用同一套滚轮、翻页和滑块拖拽算法，向上回看会暂停贴底跟随，拖到底部或点击跳转提示后恢复。工具卡片在点击松开时展开，拖动不误触，展开也不强制回到底部。输入区支持中文、多行、按词移动与删除，窗口高度紧张时优先保留输入框及光标；方向键用于编辑，翻页使用 `PgUp/PgDn`，`Ctrl+Home/End` 跳到时间线首尾。

会话分别持有消息、草稿、光标、滚动位置和揭示缓冲，子视图不会借用主会话的 Plan、附件、上下文用量或权限状态。隐藏但仍在导航栈中的会话继续归档事件，不逐帧播放；关闭视图则释放订阅。成员续跑时稳定实例 ID 不变、目标执行句柄更新，保留该实例草稿和历史，并拒绝旧句柄的迟到事件。

这些编辑与滚动交互由 TUI 本地处理，不向成员视图逐键派发 `UiInput`（通用输入准入事件仍保持原有语义）；用户提交时，宿主才把一条 `submit` 输入（`view_id` 为插件声明的成员会话视图）回送插件，由插件决定 steering 还是续跑，并在新句柄就绪后用同一实例 ID 重新打开视图。原生视图订阅的是插件 principal 名下的 Agent：目标不属于该插件时 Runtime 会拒绝，宿主只展示错误。公共导航与提交协议没有变化。成员会话暂不提供跨屏文本选择、附件提交，也不套用插件工具 renderer；worker 的原生读写和 Shell 工具由内置工具组件展示。

成员生命周期由插件自己收敛，不依赖模型主动清理：每轮运行结束时，状态已终态、所属 controller 运行已经结束且邮箱没有未读消息的成员会被回收，Runtime 句柄、目录项和邮箱一起释放；仍排队或运行中的成员保留，晚于 controller 结束的成员在后续运行结束时补收。终态结果移入容量 8 条的回收台账，`teammate_result` 仍可读取，`teammate_list` 同时返回 `retired_members`；`teammate_remove` 对已回收成员保持幂等。通过 `teammate.mailbox` 服务创建的成员没有 controller 运行身份，保留到调用方显式 `remove`。

成员失败时，插件在下一次状态刷新中读取一次幂等的终态结果，并把原因写入成员快照的可选 `error` 字段：`teammate_list`、`teammate_status`、团队工作台的成员详情和回收台账都能直接看到具体失败原因（例如单次输出上限、provider 报错或连接中断），不需要再额外调用 `teammate_result` 猜原因。

Manifest：`examples/plugins/teammate-plugin/plugin.toml`。插件版 TUI 为它注入共享 Agent Runtime、controller profile 与受限 `worker` 派生配置；worker 明确获得 `list_directory`、`search_files`、`read_file`、`write_file`、`edit_file`、`shell` 和已发现 Skill 的 `skill_read`，可以自行落盘和运行验证。父级权限仍取交集，文件工具仍受工作区边界约束；Shell 是完整原生进程信任而非沙箱。worker 模板继承同一个动态 Plugin Host，所有工具仍经过已启用的工具策略提供者（如手动安装的权限插件）与 Host 启动期门禁。worker 不覆盖步数，队员与主会话一样可以一直运行到任务完成；每次派活都是一次新的投胎，完成后插件会 `collect` 旧实例，队长可以一直派活、队员可以一直接活，上下文不足时由队员自己继承的 Context Loader 压缩。升级后重启 TUI 并新建成员，旧成员续跑不会凭空扩大其既有权限。

## Plan 插件

Plan 插件提供结构化计划的整体更新、只读查询、状态校验、输入框上方的紧凑进度架和完整计划子视图。计划最多只能有一个 `in_progress` 步骤，空计划表示显式清空；状态只在当前插件实例内保存。

计划只服务困难或较大的任务，默认不创建：跨模块且顺序有依赖的改动、能拆成 3 个以上可独立验证阶段的改动、需要多轮长时执行或持续汇报进度的任务，或用户明确要求维护计划时才使用；直接问答、代码阅读、单文件小改动、单条命令与单次测试、格式化与重命名、窄范围修复等一次连续执行即可完成的任务不开计划。准入规则同时写在 `update_plan` 的工具描述里并由插件强制执行：任何非空计划少于 3 个步骤都会被拒绝，并提示直接完成任务；该检查不作用于激活恢复，升级前保存的小计划仍可读取并完成。

计划按会话隔离：`update_plan`/`get_plan` 依据 `ToolCall.session_id` 归档，进度架依据 `UiRenderRequest.session_id` 只渲染当前会话的进度，因此新建或切换会话不会继承上一个会话的计划；插件最多保留 32 个会话的计划，宿主不提供会话路由键时退回单一共享作用域。旧版单计划状态不含会话身份，激活时直接丢弃。

进度架只在计划未完成时可见：空计划或全部完成的计划不占输入区高度。架子上的唯一动作是打开完整计划——聚焦后按 `Enter` 或直接点击，聚焦时汇总行会写出 `Enter 详情`，没有鼠标上报的终端也有明确入口。

计划还会在每轮运行结束时自动收敛和回收，不依赖模型记得收尾：`core.run.finish` 携带可信 `session_id`，本轮更新过的计划会把残留 `in_progress` 收敛为 `completed`；计划全部完成、或本轮完全没有维护过计划时直接回收，进度架随之消失。取消的运行不参与收敛或回收，中断后计划仍保留。工具描述同时要求每完成一个步骤就立即调用 `update_plan`。

## 权限插件（可选，不再默认分发）

`permission` 及旧版 `sandbox` 已从官方构建、安装与打包清单移除；`build:plugin:all` 和 `test:plugin:official` 也不再包含权限插件。源码及独立构建/测试入口保留，标准 workspace 检查仍单独构建它以运行兼容性测试，但不会生成官方发布包。

不启用该插件时，不提供 `/permission`、审批 UI 或插件的敏感路径策略；原生 `WorkspaceGuard`、Plugin Host manifest 能力鉴权与 Runtime 权限收缩仍生效。Shell 仍是当前系统用户的完整原生进程能力，并非沙箱。以下为手动安装启用后的行为。

权限插件提供 Agent 工具策略与输入区审批 UI。它只读取 Host 绑定的最终权限 scope：纯内部 `Control` 直接放行，`Read`、`Write`、`Shell`、`Network` 与 `Unknown` 进入当前审批模式。Host 会按 component 已授的进程、文件、模型、Agent 和宿主动作能力抬高 manifest `tool_scopes` 下限，高权限插件不能用 `Control` 跳过审批。可信读写 scope 携带路径时只允许工作区内词法相对路径，`.env`、私钥、凭据目录和 `.git` 内容始终拒绝访问。审批卡片按 scope 展示脱敏摘要、原因、作用范围和风险，支持方向键加 `Enter` 及 `A/S/D/Esc` 快捷键；会话级放行只适用于携带可验证顶层路径的同目录只读调用。`/permission` 可切换“手动审批、帮我审批、全部放行”，模式按项目隔离。模型代审只接受无额外字段的最小 JSON：`allow` 不携带原因，`deny` 必须携带非空原因；待审批状态按 Core 绑定的 `(run_id, call_id)` 隔离并发 Agent。拒绝或异常只阻止当前调用，详细原因不写入 Session，一次性说明仅作为下一轮的工具结果让对应 Agent 调整方案继续，不会被提升为 Developer 指令。全部放行下 Shell 仍拥有当前系统用户的完整原生权限，界面会持续警示。

插件声明独占 `agent.tool-policy@1.0.0` 能力，因此 Host 会在其他插件完成工具 Rewrite 后执行最终检查。权限插件不申请文件、进程、HTTP、secret 或 Agent Runtime 能力；模型代审只申请受控 `model_completion`，命令输入清理只申请通用 `surface_actions`。Shell 审批只表示用户或审核模型接受该次完整宿主命令风险，不提供操作系统级隔离。

```bash
bun run build:plugin:permission
bun run test:plugin:permission
```

Manifest：`examples/plugins/permission-plugin/plugin.toml`。

## 记忆插件

记忆插件跨会话保留结论，并在每次模型请求前注入上下文。它只申请 `storage` 能力，读写自己的
私有持久存储，不触达用户文件、网络或子进程。

记忆分两个作用域：`project` 跟随当前项目，`user` 跨项目共享。工具默认写入 `project`——把项目
事实写进用户级会污染其他项目，因此提示明确要求只有确属跨项目的个人偏好才用 `user`。宿主未提供
项目身份时插件降级为只用 `user`，而不是把项目数据写进用户级。

三个工具：`memory_record` 记录、`memory_list` 查询完整清单、`memory_forget` 删除过时结论。
单条记忆有字数上限，同作用域内正文相同的记忆不重复记录，每个作用域有条数上限。
时间线使用 `Record(Memory)`、`List(Memory)`、`Forget(Memory)`：显示记录/去重/删除摘要、短 ID、作用域及正文，不直接输出结果 JSON；历史卡片不重新读取当前记忆库。

插件不注册常驻提示，使用时机全部写在工具描述里；只有记忆正文本身会被注入，没有记忆时不注入
任何内容。注入有硬预算：超出时优先保留最近的记忆并提示用 `memory_list` 查看全部，而不是让记忆
无声吃掉上下文。记忆由模型自己写入，属于不可信内容，因此注入时会声明它们只是背景事实、不是
指令，且与系统提示或用户当前请求冲突时以后者为准——否则一条被污染的记忆会在之后每一轮持续
生效。这一句无法由工具描述承担，是唯一保留的说明性文字。

`/memory` 打开记忆面板查看当前记忆。

```bash
bun run build:plugin:memory
bun run test:plugin:memory
cargo test --offline -p memory-plugin-smoke-tests
```

Manifest：`examples/plugins/memory-plugin/plugin.toml`。

## 历史会话插件

历史会话插件不调用模型，也不生成新的语义摘要。它把当前项目的持久化会话按“用户消息到
下一条用户消息之前”切成片段，保存会话标题、用户请求、最后助手回复、工具名、有界原文证据
以及会话创建和更新时间。Agent 遇到“继续”“上次”“之前”或可能处理过的任务时，自行决定
是否检索；命中后可按片段消息区间回溯原文。

### 为什么是词法检索而不是向量

语料是用户自己的对话记录，查询词与正文共享同一批标识符、文件路径、报错串和命令名，
这正是精确词匹配最强的场合。插件同时索引标题、请求、最后回复、工具名和有界原文证据，
不依赖额外模型或不透明的向量空间。

平台条件也不支持向量：宿主没有嵌入接口，插件单次调用只有 5000 万 fuel、64 MiB 线性
内存，在 WASM 里跑嵌入模型不可行；而为此新增宿主嵌入 API 会引入新的 provider 集成、
密钥管理和每次索引的调用成本。

检索实现为两段式：先用子串命中做召回并顺带统计精确的文档频率，再只对候选集算 BM25。
中文没有可用分词器，按相邻字符二元组切分；ASCII 按字母数字段切分并小写化，因此大小写
不同的标识符仍能召回。

### 自动索引与范围

插件激活时同步全部未扫描或修订号变化的会话；每次 TUI 成功完成运行并持久化会话后再次
增量同步。会话正文按页读取，不能因宿主单页上限漏掉长会话。每个有正文或附件的用户轮次都
建立片段，包括尚未形成最终结论的工作；是否值得使用由主 Agent 在检索时决定，而不是由索引
阶段提前做脆弱的语义过滤。

修订号未变的会话不会重复读取；会话变化时整体替换该会话旧片段，来源会话被删除时对应片段
一并移除。单个会话读取失败会保留旧片段并等待下次重试，不中断其他会话。索引最多保留最近
2000 个片段，单片段用户请求、最后回复和原文证据分别限制为 600、900、1200 字符；原文证据
按流式首尾缓冲保存，不会先把长会话整份复制到插件内存。

已有上下文压缩仍由 Context Loader 负责，本插件只索引会话存储提供的消息投影，不复制压缩器。
旧 v1 模型蒸馏缓存升级时会从会话原文重建 v2 索引。

### 权限

插件只申请 `sessions` 与 `storage`，不申请 `model_completion`。其中 `sessions` 会把它**全部工具**
的权限下限抬到 `unknown`，因此 `history_search` 这类检索调用也会进入当前审批模式。这是刻意的：
会话内容是 Agent 历史上见过的一切的并集，涵盖文件正文、shell 输出与网络返回，按只读文件
对待会低估覆盖面。

历史原文可能包含过时结论或注入文本，因此检索结果只作为工具返回值交给主 Agent，不注入
历史正文。索引非空时仅注入一行带片段数的能力提示，让 Agent 自行判断何时使用
`history_search` 与 `history_read`；提示同时要求继续工作前核对当前代码和运行状态。工具返回
会话级 `created_at_ms`、`updated_at_ms`、UTC 时间和相对时间；消息本身没有独立时间戳。

时间线使用 `Search(History)`、`Read(History)`，保留片段标题、请求/结果、历史证据提示及回读分页位置，不把索引或会话标识的 JSON 展开成摘要。未知旧结果和失败诊断回退宿主原展示。

`/history` 打开历史会话面板查看片段数、会话数和最近更新时间；`/kb` 作为兼容入口保留。

```bash
bun run build:plugin:knowledge
bun run test:plugin:knowledge
cargo test --offline -p knowledge-plugin-smoke-tests
```

Manifest：`examples/plugins/knowledge-plugin/plugin.toml`。

## Eden 编辑器上下文插件

Eden 是一个 macOS 编辑器，它随 App 启动一个 Unix domain socket，回答「用户此刻在编辑器里看着
什么」。插件不把它做成模型工具，而是**上下文引用 provider**：

```
Eden ──socket──▶ eden-plugin ──context.ref──▶ 宿主 ──[REF#1]──▶ 下一条用户消息
```

用户在编辑器里选中一段代码，Lucia 的输入框就多出一个 `[REF#1]` 标签；发送时标签展开为带
`path` / `lines` / `language` 属性的代码块，随该条消息一起进入模型上下文。用户删掉标签就是
丢弃这段引用，同一段选区在选区变化前不会被自动补回。

### 为什么不是工具

选区是**用户此刻的状态**，不是模型需要决定去查的东西。做成工具等于要求模型先猜「我现在该不该
读选区」，而用户已经用鼠标表达过意图。做成引用则相反：用户选中什么，下一条消息就自动带上什么，
模型不需要为此多一轮往返，也不会在无关提问里凭空读一遍编辑器。

### 为什么由宿主轮询

Eden 协议 v1 只有「客户端请求、服务端应答」，没有事件推送；WASM 插件也没有后台线程。因此
「推送」落在插件与宿主的交界上：插件保留全部协议细节（握手、令牌、分帧、id 配对、超时、令牌
轮换），宿主只按固定节拍调用 `context.ref` 索要一份已经成形的快照。Eden 将来支持推送后，只需
让插件在收到事件时更新缓存，服务契约与宿主调用方式都不用改。

每个 provider 的快照都要通过宿主侧的契约校验（条数、ID 字符集、标题单行、正文上限与总量），
任何一条不合规就整份丢弃并给出一次可见错误——自动注入不接受「部分接受」这种模糊状态。

### 没有选区时

插件返回空快照，而不是退回光标行。引用是增益信息：Eden 没运行、没有打开编辑器或用户没有选中
任何文本时，Lucia 只是没有可带的内容，不会弹错误，也不影响正常对话。

### 权限

令牌文件与 socket 各需要一条 allowlist 声明，默认路径是 Eden 的约定位置；若用户通过
`EDEN_SOCKET` / `EDEN_TOKEN_FILE` 改到别处，需要在 `plugin.toml` 中同步声明，否则 Host 会拒绝
连接并说明原因。`context.ref` 的注册资格来自 manifest 的 `[[provides]] agent.context-ref`，
任意插件不能只靠在运行时宣布服务名就接入这条自动注入通道。

不需要 `process_exec`：早期做法是执行 `eden-cli` 再解析 stdout，那等于为了读一次选区就交出
执行任意命令的能力。

```bash
bun run build:plugin:eden
bun run test:plugin:eden
```

Manifest：`examples/plugins/eden-plugin/plugin.toml`。

## 统一验证

```bash
bun run build:plugin:official
bun run test:plugin:official
```

Context、MCP、Skill、Command、Teammate、Plan、Permission 和 Eden 都以独立插件 workspace 构建，不加入原生 Workspace；每个 bundle 都包含独立 `cdylib` component。它们的扫描、解析、协议和业务规则不会进入 Agent Core 或 Plugin Host。
