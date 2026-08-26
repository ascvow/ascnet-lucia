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

[evidence]
enabled = false
# root_dir = "evolution"
# genome_revision_id = "grev_0123456789abcdef0123456789abcdef"
# genome_stable = "stable/general"
```

`sessions_dir` 是项目会话根目录。相对配置值与 `events_jsonl` 都以配置文件所在目录为基准；CLI 的 `--sessions-dir` 与 `--events-jsonl` 仍以当前工作目录为基准，并覆盖配置值。旧配置中的 `default_session` 和 `resume_latest` 仍可解析，但普通启动不再用它们自动恢复会话。

`evidence.enabled` 默认关闭。启用时必须在 `genome_revision_id` 与 `genome_stable` 中二选一；
前者固定精确修订，后者通过只读 Stable 引用为新 Session 解析精确修订。启动会重新校验
Revision 的行为摘要，不存在、引用不一致或被篡改时拒绝运行。
`root_dir` 的配置值相对配置文件解析，未配置时使用 `$LUCIA_HOME/evolution`。Episode、CAS
制品和 Genome 修订分别保存在该根目录的 `episodes`、`artifacts` 和 `genomes` 子目录。

Evidence 启用后，`[model]` 中的 `api_key` 或 `api_key_env` 仍负责提供 Secret；其余模型行为、
Prompt CAS、原生工具集合、插件 bundle、独占能力 owner 与执行策略由 Genome 固定。
Genome 必须按顺序引用至少一个包含完整系统提示的 UTF-8 Prompt CAS 制品；空 Prompt 不会
采用 `[agent].system_prompt` 或 Core 默认提示。
`model.extra_headers` 暂不支持，因为它可能同时携带 Secret 和未入 Genome 的行为。模型密钥
缺失时不会自动退回演示模型；显式 `--demo` 只接受声明 `scripted-demo` 路由的 Genome。
插件版在固定插件全部 Ready 前不开始 Evidence Run，任一加载失败后继续保持阻断。
Genome 的包版本、Git 提交、dirty 状态、目标三元组和 TUI feature 必须与当前编译产物一致。
缺少可验证 Git commit 的源码归档构建可以运行普通 Serve，但不能启动 Evidence。

## 会话恢复

Lucia 启动时规范化当前目录，并据此生成稳定 `project-id`。同一个工作目录可以保存多个会话，不同工作目录不会混用列表；实际文件位于 `<sessions_dir>/<project-id>/sessions`。

普通启动总是创建只存在于内存中的空白 Draft，不加载上一次记录。用户发送第一条普通消息时，TUI 先用 revision/CAS 保存用户输入并生成短标题，再运行 Agent；没有发送消息就退出时不会创建会话文件。插件版可直接在 TUI 中恢复：

```bash
lucia
```

```text
/resume
```

`/resume` 由官方 Command 插件打开当前项目的会话列表，用户选中后才加载完整 Session。`/sessions` 使用同一界面只读浏览；移除 Command 插件后，这两个界面和其他斜杠命令不可用，但会话文件不受影响。

Evidence 模式会在首次保存前把 Session 绑定到精确 Genome Revision。已有记录缺少绑定或绑定
不同 Revision 时拒绝恢复；Stable 引用更新只影响后续新 Session，不会让旧 Session 静默升级。

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
