# TUI 配置与会话

`lucia` 的应用配置属于 TUI crate。Agent Core 只解析模型和 Agent 参数，不决定配置文件位置、会话目录或启动恢复策略。

## 初始化

安装后直接运行：

```bash
lucia
```

首次启动会自动创建默认配置。默认配置路径是 `$LUCIA_HOME/config.toml`，未设置 `LUCIA_HOME` 时使用 `$HOME/.lucia/config.toml`。插件版只加载插件管理器中已启用的插件，以及配置或命令行显式指定的 manifest。未检测到模型密钥时，TUI 会进入本地演示模式，并在主事件区显示配置提示。

模型服务完全由配置文件决定：

```toml
[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key = "sk-..."
context_window = 200000
openai_protocol = "responses"
```

`base_url` 是模型服务 URL，`model` 是请求使用的模型名称。可选的 `context_window` 只用于 TUI 状态栏显示当前上下文占比，不会改变模型请求。密钥默认使用 `api_key = "..."` 直接配置，也可以删除该字段并改用 `api_key_env = "OPENAI_API_KEY"` 指定环境变量名；两者同时存在时 `api_key` 优先。修改配置后重新运行 `lucia` 即可生效。

也可以显式初始化指定文件后退出：

```bash
lucia --config ./lucia.toml --init
```

初始化使用原子排他创建，不会覆盖已有文件。macOS/Linux 下配置文件权限为 `0600`；运行前需要填写密钥，并确认 `model.model` 是账号可用的模型 ID。

## 自动读取

配置文件选择顺序：

1. `--config <path>`；
2. `LUCIA_CONFIG` 环境变量；
3. `$LUCIA_HOME/config.toml`；
4. `$HOME/.lucia/config.toml`。

配置存在后，直接运行 `lucia` 即可，不再需要每次传入 `--config`。`--demo` 仍可在没有模型配置时使用离线脚本模型。

## TUI 字段

模型与 Agent 字段继续使用 Core 的 `[model]` 和 `[agent]`。TUI 设置位于独立段落：

```toml
[tui]
sessions_dir = "projects"
events_jsonl = "events.jsonl"

[genome]
root_dir = "evolution"
stable = "stable/general"
# revision_id = "grev_0123456789abcdef0123456789abcdef"

[evidence]
enabled = false
```

`sessions_dir` 是项目会话根目录。相对配置值与 `events_jsonl` 都以配置文件所在目录为基准；CLI 的 `--sessions-dir` 与 `--events-jsonl` 仍以当前工作目录为基准，并覆盖配置值。旧配置中的 `default_session` 和 `resume_latest` 仍可解析，但普通启动不再用它们自动恢复会话。

`genome.root_dir` 相对配置文件解析，未配置时使用 `$LUCIA_HOME/evolution`。新 Session 在
`genome.stable` 与 `genome.revision_id` 中选择一个；Stable 通过只读 Resolver 解析为精确
Revision。启动会重新校验 Revision 的行为摘要，不存在、引用不一致或被篡改时拒绝运行。
旧配置中的 `evidence.root_dir`、`evidence.genome_revision_id` 和 `evidence.genome_stable` 仍可
读取，但独立 `[genome]` 配置优先。

Session Genome 绑定不受 `evidence.enabled` 控制。`[model]` 中的 `api_key` 或 `api_key_env` 仍
负责提供 Secret；其余模型行为、Prompt CAS、原生工具集合、插件 bundle、独占能力 owner 与
执行策略始终由 Genome 固定。Evidence 默认关闭，此时不会创建 `episodes`、`outbox`、
`outcome-revisions` 或 `issue-observations`；启用后才在同一根目录追加生产证据。
Genome 必须按顺序引用至少一个包含完整系统提示的 UTF-8 Prompt CAS 制品；空 Prompt 不会
采用 `[agent].system_prompt` 或 Core 默认提示。
`model.extra_headers` 暂不支持，因为它可能同时携带 Secret 和未入 Genome 的行为。模型密钥
缺失时不会自动退回演示模型；显式 `--demo` 只接受声明 `scripted-demo` 路由的 Genome。
插件版在 Genome 固定插件全部 Ready 前不开始 Run，任一加载失败后继续保持阻断。
Genome 的包版本、Git 提交、dirty 状态、目标三元组和 TUI feature 必须与当前编译产物一致。
缺少可验证 Git commit 的源码归档构建不能建立具备运行资格的 Session Genome 绑定。

## 会话恢复

Lucia 启动时规范化当前目录，并据此生成稳定 `project-id`。同一个工作目录可以保存多个会话，不同工作目录不会混用列表；实际文件位于 `<sessions_dir>/<project-id>/sessions`。

普通启动总是创建只存在于内存中的空白 Draft，不加载上一次记录。用户发送第一条普通消息时，TUI 先用 revision/CAS 保存用户输入并生成短标题，再运行 Agent；没有发送消息就退出时不会创建会话文件。默认插件版与纯 Core 版都可直接在 TUI 中恢复：

```bash
lucia
```

```text
/resume
```

原生 `/resume` 打开当前项目的会话列表，用户选中后才按 revision 校验并加载完整 Session；`/sessions` 使用同一界面只读浏览。两者不依赖 Plugin Host。

新 Session 会在首次保存前绑定精确 Genome Revision。Stable 引用更新只影响后续新 Session，
已有绑定始终按原 Revision 精确恢复。未配置可解析 Registry 时，新 Draft 可以显示，但消息提交、
队列输入、Host Action 和 `/compact` 都会在保存或模型调用前失败关闭。已持久化且缺少绑定的旧
记录只允许加法标记为 `LegacyUnbound/NotEligible` 并只读恢复，不能启动 Run 或进入 Evidence。

CLI 仍提供显式恢复和只读列举：

```bash
# 恢复指定会话
lucia --session-id design-review

# 恢复最近更新的会话
lucia --resume-latest

# 不连接模型，仅列出会话
lucia --list-sessions
```

显式 `--session-id` 的优先级高于 `--resume-latest`。没有最近记录时，`--resume-latest` 与普通启动一样返回新的空白 Draft。

恢复后，Session 中的用户消息、助手文本和工具结果会重新显示在主事件列表中；system、developer 和 thinking 内容不会直接展示。底栏显示当前 session ID 与 revision，方便确认当前写入目标。

## 路径覆盖

完整优先级如下：

| 项目 | 第一优先 | 第二优先 | 默认值 |
| --- | --- | --- | --- |
| 配置文件 | `--config` | `LUCIA_CONFIG` | `$LUCIA_HOME/config.toml` |
| 项目会话根目录 | `--sessions-dir` | `tui.sessions_dir` | `$LUCIA_HOME/projects` |
| 启动会话 | `--session-id` | `--resume-latest` | 新的空白 Draft |
| 事件日志 | `--events-jsonl` | `tui.events_jsonl` | 不写入 |

配置支持保存 `model.api_key`，但包含明文密钥的文件不得提交到版本库。生产环境优先使用 `model.api_key_env`，并让 shell、密钥管理器或部署环境提供对应变量。
