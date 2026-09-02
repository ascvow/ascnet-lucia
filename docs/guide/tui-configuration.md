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
# models.dev 中使用其他服务商 ID 的兼容网关可显式指定：
# models_dev_provider = "openrouter"
# context_window = 200000
openai_protocol = "responses"
```

`base_url` 是模型服务 URL，`model` 是请求使用的模型名称。Lucia 打包时下载并内置 `models.dev/api.json`，运行时优先按服务商和模型 ID 精确读取上下文窗口、输出 token 上限、思考等级及输入输出模态；服务商未命中但模型 ID 在快照中唯一时仍会使用该模型配置，不依赖网络。重复模型 ID 必须通过 `models_dev_provider` 指定服务商消歧。`context_window`、`agent.max_tokens` 和 `agent.reasoning` 可显式覆盖；其中输出上限和思考等级不得超出快照声明的能力。未显式配置 `agent.max_tokens` 时采用快照声明的输出上限，OpenAI Responses 兼容网关也会收到标准 `max_output_tokens` 字段，避免回落到过小的网关默认额度。图片、普通文件和 PDF 在输入框中分别显示为 `[IMAGE#N]`、`[FILE#N]`、`[PDF#N]`，并在加入时校验模型输入能力；普通粘贴文本折叠为 `[粘贴的文本]`，删除标签会同步删除绑定的原文。音频、视频能力会被识别，但 Lucia 当前模型协议尚未实现对应内容块，因此不会误发送。输入输出模态只用于能力校验，不在底栏显示，也不会把尚未实现的图片、音频或视频输出伪装成文本。OpenAI 与 Anthropic 自动映射官方服务商 ID；兼容网关可用 `models_dev_provider` 指定对应 ID。密钥默认使用 `api_key = "..."` 直接配置，也可以删除该字段并改用 `api_key_env = "OPENAI_API_KEY"` 指定环境变量名；两者同时存在时 `api_key` 优先。TUI 输入 `/reasoning` 后会在输入框上方打开命令面板风格的等级列表，可用方向键选择 models.dev 声明的等级并按 Enter 应用；`/effort` 与 `/effo` 是兼容别名。也可使用 `/reasoning high` 精确切换，或使用 `/reasoning next`、`Ctrl+R` 循环切换。切换成功后会原子写回本次启动实际使用的配置文件，只影响后续模型请求；写回失败时运行态保持原等级。OpenAI Responses 的非关闭等级仍会请求自动推理摘要，用于判断模型是否处于思考阶段；摘要文本不显示、不进入消息或 Session，思考期间的运行状态显示为 `Thinking...`。

`agent.cache_retention` 控制提示缓存保留策略，缺省为 `short`，普通 `run` 和 `run_session` 会自动使用，不需要缓存专用运行 API。`none` 不发送显式缓存控制；`long` 对 OpenAI 请求 `24h`，对 Anthropic 请求 `1h`。OpenAI 会根据稳定提示前缀自动生成缓存亲和键，Anthropic 会在 system、最后一个工具和最近用户内容上写入缓存断点。兼容网关需要覆盖路由字段时，可通过现有 `agent.provider_options` 显式配置。

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
review_model = "gpt-5.6-luna"
```

`sessions_dir` 是项目会话根目录。相对配置值与 `events_jsonl` 都以配置文件所在目录为基准；CLI 的 `--sessions-dir` 与 `--events-jsonl` 仍以当前工作目录为基准，并覆盖配置值。`review_model` 是 Host 固定提供给权限插件“帮我审批”模式的模型 ID；缺省时兼容沿用主模型，建议显式配置同服务商下可用的小模型。旧字段 `plugin_model` 仍可读取，但新配置统一使用 `review_model`。旧配置中的 `default_session` 和 `resume_latest` 仍可解析，但普通启动不再用它们自动恢复会话。

`model.extra_headers` 暂不支持，因为它可能同时携带 Secret 和未经审阅的行为。模型密钥缺失
时使用内置演示模型，并在界面中显示配置提示。

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
