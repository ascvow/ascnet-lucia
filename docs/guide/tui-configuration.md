# TUI 配置与会话

`lucia` 的应用配置属于 TUI crate。Agent Core 只解析模型和 Agent 参数，不决定配置文件位置、会话目录或启动恢复策略。

## 初始化

安装后直接运行：

```bash
lucia
```

首次启动会自动创建默认配置。默认配置路径是 `$LUCIA_HOME/config.json`，未设置 `LUCIA_HOME` 时使用 `$HOME/.lucia/config.json`。插件版只加载插件管理器中已启用的插件，以及配置或命令行显式指定的 manifest。未检测到模型密钥时，TUI 会进入本地演示模式，并在主事件区显示配置提示。

模型服务完全由配置文件决定：

```json
{
  "model": {
    "name": "default",
    "provider": "open-ai",
    "base_url": "https://api.openai.com/v1",
    "model": "gpt-5",
    "api_key": "sk-...",
    "openai_protocol": "responses"
  }
}
```

### 多上游配置与模型切换

除单 `model` 对象外，可以用 `models` 数组配置任意数量的上游，并混用协议；`default_model` 指定启动默认（缺省取 `model` 或 `models` 第一个元素）。`name` 在全部上游中必须唯一。

```json
{
  "default_model": "deepseek",
  "models": [
    {
      "name": "deepseek",
      "provider": "open-ai-compatible",
      "base_url": "https://api.deepseek.com/v1",
      "model": "deepseek-chat",
      "api_key_env": "DEEPSEEK_API_KEY"
    },
    {
      "name": "gemini",
      "provider": "google",
      "model": "gemini-2.5-pro",
      "api_key_env": "GEMINI_API_KEY"
    },
    {
      "name": "bedrock",
      "provider": "bedrock",
      "model": "anthropic.claude-sonnet-4-5-20250929-v1:0",
      "aws_region": "us-east-1",
      "aws_access_key_id": "AKIA...",
      "aws_secret_access_key": "..."
    }
  ]
}
```

`provider` 支持内置协议 `open-ai`（Responses）、`open-ai-compatible`（Chat Completions）、`anthropic`（Messages）、`google`（Generative AI `generateContent`）与 `bedrock`（Converse）；第三方可通过应用层注册自定义协议工厂扩展新的协议 ID。TUI 内输入 `/model` 打开上游选择面板，面板每行按 `模型 ID [上游名]` 显示，行首 `●` 标记当前上游，方括号里的上游名就是 `/model <名称>` 的切换键；也可以用 `/model <名称>` 直接切换。切换只在当前会话生效（会话级），重启后回到 `default_model`。网关注册表使用内部读写锁，运行期切换对派生 Agent 与插件模型路由同样生效。

### 模型协议映射

`base_url` 是模型服务 URL，`model` 是请求使用的模型名称。Lucia 打包时下载并内置 `models.dev/api.json`，运行时优先按服务商和模型 ID 精确读取上下文窗口、输出 token 上限、思考等级及输入输出模态；服务商未命中但模型 ID 在快照中唯一时仍会使用该模型配置，不依赖网络。重复模型 ID 必须通过 `models_dev_provider` 指定服务商消歧。`context_window`、`agent.max_tokens` 和 `agent.reasoning` 可显式覆盖；其中输出上限和思考等级不得超出快照声明的能力。未显式配置 `agent.max_tokens` 时采用快照声明的输出上限，OpenAI Responses 兼容网关也会收到标准 `max_output_tokens` 字段，避免回落到过小的网关默认额度。图片、普通文件和 PDF 在输入框中分别显示为 `[IMAGE#N]`、`[FILE#N]`、`[PDF#N]`，并在加入时校验模型输入能力；普通粘贴文本折叠为 `[粘贴的文本]`，删除标签会同步删除绑定的原文。音频、视频能力会被识别，但 Lucia 当前模型协议尚未实现对应内容块，因此不会误发送。输入输出模态只用于能力校验，不在底栏显示，也不会把尚未实现的图片、音频或视频输出伪装成文本。OpenAI 与 Anthropic 自动映射官方服务商 ID；兼容网关可用 `models_dev_provider` 指定对应 ID。密钥默认使用 `api_key = "..."` 直接配置，也可以删除该字段并改用 `api_key_env = "OPENAI_API_KEY"` 指定环境变量名；两者同时存在时 `api_key` 优先。TUI 输入 `/reasoning` 后会在输入框上方打开命令面板风格的等级列表，可用方向键选择 models.dev 声明的等级并按 Enter 应用；`/effort` 与 `/effo` 是兼容别名。也可使用 `/reasoning high` 精确切换，或使用 `/reasoning next`、`Ctrl+R` 循环切换。切换成功后会原子写回本次启动实际使用的配置文件，只影响后续模型请求；写回失败时运行态保持原等级。OpenAI Responses 的非关闭等级仍会请求自动推理摘要，用于判断模型是否处于思考阶段；摘要文本不显示、不进入消息或 Session，思考期间的运行状态显示为 `Thinking...`。

`agent.cache_retention` 控制提示缓存保留策略，缺省为 `short`，普通 `run` 和 `run_session` 会自动使用，不需要缓存专用运行 API。`none` 不发送显式缓存控制；`long` 对 OpenAI 请求 `24h`，对 Anthropic 请求 `1h`。OpenAI 以当前会话 ID 作为缓存亲和键，Anthropic 会在 system、最后一个工具和最近用户内容上写入缓存断点。路由方式由 `model.gateway` 声明的网关厂商决定：`open-ai` 用请求体里的 `prompt_cache_key`；`open-router` 发送 `x-session-id`；缺省的 `open-ai-compatible` 同时发送标准 `prompt_cache_key` 和 `session_id`、`x-client-request-id`、`x-session-affinity` 三个会话亲和 header；`anthropic` 不需要额外路由字段。自建代理和第三方中转保持缺省即可。上下文压缩摘要的前缀在压缩完成后就不再出现，因此固定关闭缓存保留，只保留会话路由标识。插件模型补全反过来是"固定 system + 短小可变正文"的高频调用，跟随同一份缓存配置，并按插件 ID 分组缓存键（`plugin:<id>`），与主会话的缓存分区互不驱逐。兼容网关需要覆盖路由字段时，可通过现有 `agent.provider_options` 显式配置。

也可以显式初始化指定文件后退出：

```bash
lucia --config ./lucia.json --init
```

初始化使用原子排他创建，不会覆盖已有文件。macOS/Linux 下配置文件权限为 `0600`；运行前需要填写密钥，并确认 `model.model` 是账号可用的模型 ID。

## 自动读取

配置文件选择顺序：

1. `--config <path>`；
2. `LUCIA_CONFIG` 环境变量；
3. `$LUCIA_HOME/config.json`；
4. `$HOME/.lucia/config.json`。

配置存在后，直接运行 `lucia` 即可，不再需要每次传入 `--config`。`--demo` 仍可在没有模型配置时使用离线脚本模型。

## TUI 字段

模型与 Agent 字段继续使用 Core 的 `model` 和 `agent` 对象。TUI 设置位于独立的 `tui` 对象：

```json
{
  "tui": {
    "sessions_dir": "projects",
    "events_jsonl": "events.jsonl",
    "review_model": "gpt-5.6-luna",
    "fullscreen": true
  }
}
```

`sessions_dir` 是项目会话根目录。相对配置值与 `events_jsonl` 都以配置文件所在目录为基准；CLI 的 `--sessions-dir` 与 `--events-jsonl` 仍以当前工作目录为基准，并覆盖配置值。`review_model` 是 Host 固定提供给权限插件“帮我审批”模式的模型 ID；缺省时兼容沿用主模型，建议显式配置同服务商下可用的小模型。旧字段 `plugin_model` 仍可读取，但新配置统一使用 `review_model`。旧配置中的 `default_session` 和 `resume_latest` 仍可解析，但普通启动不再用它们自动恢复会话。

`model.extra_headers` 暂不支持，因为它可能同时携带 Secret 和未经审阅的行为。模型密钥缺失
时使用内置演示模型，并在界面中显示配置提示。

## 全屏渲染

Lucia 默认使用全屏渲染：进入终端备用屏幕（类似 vim），界面固定在整屏内重绘，退出后恢复终端原有内容。每帧只排版与视口相交的消息，历史部分只保留换行后的视觉行数，滚出窗口的渲染结果立即释放，因此会话再长，单帧开销和内存占用都基本不变。消息超出视口时，右侧留白列会出现滚动条，滑块长度按可见比例计算；在轨道上按下或拖动可直接定位，拖到底部恢复自动跟随。空闲时主循环不再按 tick 重绘，只在状态真正变化或存在动画时刷新。

```bash
/tui                # 查看当前渲染模式
/tui fullscreen     # 开启全屏渲染（默认）
/tui default        # 关闭备用屏幕，直接在主屏绘制
```

切换立即生效，并写入配置文件的 `tui.fullscreen`；也可以启动前直接编辑该字段。主屏模式下退出后最后一帧留在终端里，适合想用终端原生滚动和搜索回看输出的场景；全屏模式则把对话留在备用屏幕内，由应用自己处理滚动和选择。

## 底栏用量

运行时底栏的 `◔` 是本轮上下文规模（未缓存输入 + 缓存读写），配置了 `context_window` 时显示占比，越过 80% 与 95% 会变色。对话结束后隐藏用量底栏，也不再向对话追加 token 统计。

运行时的 `↺` 是本次打开当前会话期间累计的提示缓存命中率，按「缓存读取 ÷（缓存读取 + 未缓存输入）」计算；缓存写入不计入分母。命中率低于 30% 会变为警告色。终端宽度不足 72 列时隐藏该段。

输入 `/context` 查看最近一次实际模型请求的上下文快照：多色分段条区分 System prompt、开发者指令、工具定义、用户消息、助手消息、工具调用、工具结果和推理文本；灰色表示估算剩余容量。分类来自压缩及权限筛选后的请求，使用文本估算，百分比标明内容占比；协议开销、附件和不透明推理载荷不计入分类。服务商实际输入与缓存用量单独显示。尚未发送的消息不在快照内，恢复会话后需等待新的模型请求建立快照；切换或重载会话时清空旧统计。方向键上下滚动，Enter 或 Esc 关闭，不调用模型。

## 文件编辑预览

`edit_file` 成功后直接显示工具返回的 diff：删除行使用红色，新增行使用绿色；主对话同时显示增删数量，并支持展开较长结果。Agent / teammate 成员视图也读取相同的 diff 字段并显示红绿预览。普通读取和完整写入的内容预览不伪装成修改差异。

## 项目约定

启动时会自动加载 `AGENTS.md` 并并入系统提示，不需要每次对话复述项目规矩。按优先级从低到高收集：

1. `$LUCIA_HOME/AGENTS.md`，对所有项目生效；
2. 从当前目录向上回溯（最多 16 层）沿途各级目录的 `AGENTS.md`，越靠近工作目录的越具体。

同一条约定冲突时以最后出现的为准，也就是最靠近工作目录的那份。单份上限 32 KiB、合计上限 64 KiB，超出部分按字符边界截断并标注省略，不会整份丢弃。内容进入 system 段，因此一并进入提示缓存前缀，不会每轮重复计费。加载了几份会在启动提示里说明。

## 插件配置

每个插件可以有自己的配置，键名就是插件 ID：

```json
{
  "plugins": {
    "permission": {
      "model": "gpt-5.6-luna",
      "base_url": "https://ai.phrolova.uno",
      "api_key": "sk-...",
      "gateway": "open-ai-compatible",
      "auto_approve_reads": true
    }
  }
}
```

上面的 `api_key` 也可以换成 `api_key_env`；其余键值由插件自己解释。

配置有两个来源，主配置的同名键覆盖插件目录里随 bundle 分发的 `config.toml`（与 `plugin.toml` 同级），因此插件作者可以给出默认值，你只覆盖需要改的项。

路由字段是 `provider`、`model`、`base_url`、`api_key`、`api_key_env`、`openai_protocol`、`gateway`、`stream`、`max_output_tokens`，取值与 `model` 对象一致；它们由宿主消费并注册成该插件专属的服务商（网关内命名为 `plugin:<插件 ID>`），密钥只留在宿主侧，不会进入插件的 WASM 实例。声明了 `model` 却没有 `api_key` 或 `api_key_env` 时启动即报错，不会拖到第一次调用模型才失败。`max_output_tokens` 仍受执行策略上限约束，插件配置只能调小。

其余键值原样作为激活元数据交给插件读取，值统一转成字符串。宿主注入的可信项（如工作区标识）优先级最高，插件无法用配置覆盖它们。没有配置专属路由的插件继续使用 `tui.review_model`。

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
| 配置文件 | `--config` | `LUCIA_CONFIG` | `$LUCIA_HOME/config.json` |
| 项目会话根目录 | `--sessions-dir` | `tui.sessions_dir` | `$LUCIA_HOME/projects` |
| 启动会话 | `--session-id` | `--resume-latest` | 新的空白 Draft |
| 事件日志 | `--events-jsonl` | `tui.events_jsonl` | 不写入 |

配置支持保存 `model.api_key`，但包含明文密钥的文件不得提交到版本库。生产环境优先使用 `model.api_key_env`，并让 shell、密钥管理器或部署环境提供对应变量。
