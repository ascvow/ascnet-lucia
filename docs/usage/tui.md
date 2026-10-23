# TUI 使用

本章面向直接在终端中使用 Lucia 的用户。插件版 TUI 只提供 Plugin Host；功能插件由用户显式安装、启用或通过 manifest 指定。

TUI 与官方插件界面统一使用英文，不读取或注入系统语言设置。

## 安装与启动

```bash
bun run install:tui
lucia
```

`install:tui` 会构建插件版 `lucia` 并安装到 Cargo bin 目录，不会安装任何功能插件。启动时会加载 TUI `/plugins` 人工管理且已经启用的插件，并自动扫描 `$LUCIA_HOME/plugins` 与 `$LUCIA_HOME/official-plugins` 的直接子目录；未设置 `LUCIA_HOME` 时，应用目录是 `$HOME/.lucia`。

第一次启动会创建 `$LUCIA_HOME/config.json`。配置不存在或模型密钥不可用时，TUI 使用本地演示模型；也可以显式运行：

```bash
lucia --demo
```

## 配置模型

推荐在配置中保存环境变量名，不保存密钥明文：

```json
{
  "model": {
    "name": "default",
    "provider": "open-ai",
    "base_url": "https://api.openai.com/v1",
    "model": "替换为账号可用的模型 ID",
    "api_key_env": "OPENAI_API_KEY",
    "openai_protocol": "responses",
    "context_window": 200000
  },
  "agent": {
    "max_steps": 0,
    "max_tokens": 4096,
    "stream": true
  },
  "tui": {
    "sessions_dir": "projects"
  }
}
```

设置密钥后重新启动：

```bash
export OPENAI_API_KEY="你的密钥"
lucia
```

字段含义：

- `model.name`：进程内模型服务商的逻辑名称，Agent 通过它选择 provider。
- `model.provider`：适配器类型，支持 `open-ai`、`open-ai-compatible` 和 `anthropic`。
- `model.base_url`：服务根地址；官方 OpenAI 默认使用 `https://api.openai.com/v1`。
- `model.model`：实际发送给服务商的模型 ID，不是本地显示别名。
- `model.api_key_env`：保存密钥的环境变量名。也可使用 `api_key` 明文字段；两者同时存在时 `api_key` 优先。
- `model.openai_protocol`：OpenAI 类服务使用 `responses` 或 `chat-completions`。
- `model.context_window`：只用于底栏上下文占比展示，不改变请求上限。
- `agent.max_steps`：一条用户指令连续执行的 ReAct 步数；交互主会话中 `0` 表示不设置总步数上限。
- `agent.max_tokens`：单次模型响应的最大输出 token 数，是否支持由服务商决定。
- `agent.stream`：是否使用模型流式接口，默认 `true`；设为 `false` 时等待完整响应。
- `tui.sessions_dir`：按项目隔离的会话根目录；相对路径以配置文件目录为基准。
- `tui.events_jsonl`：可选事件日志；相对路径以配置文件目录为基准。

## 输入与快捷键

| 操作 | 按键 | 行为 |
| --- | --- | --- |
| 发送 | `Enter` | 提交当前输入；运行中的纯文本作为 steering 立即插话，带附件的输入排队到本轮结束后发送 |
| 换行 | `Ctrl+J` / `Shift+Enter` / `Alt+Enter` | 在输入框中插入换行 |
| 中断、返回或清空 | `Esc` | 运行中请求取消；菜单或子页面返回上一级；主输入区连续双按清空草稿，不会退出 Lucia |
| 中断或退出 | `Ctrl+C` | 运行中只中断本次运行；有草稿时清空草稿；空输入时提示并要求两秒内再按一次才退出 |
| 退出 | `Ctrl+D` | 空输入时按终端 EOF 约定直接退出；输入非空时删除光标右侧字符 |
| 行内移动 | `Ctrl+A` / `Ctrl+E` / `Home` / `End` | 移动到当前逻辑行的行首或行尾 |
| 行内删除 | `Ctrl+U` / `Ctrl+K` / `Ctrl+W` | 删除到行首、删除到行尾、删除左侧一个词 |
| 按词移动 | `Ctrl+←` / `Ctrl+→` | 按词左右移动；`Alt` 组合等价，兼容上报修饰键不同的终端 |
| 滚动历史 | `PgUp` / `PgDn` | 手动滚动消息区 |
| 跳转消息区 | `Ctrl+Home` / `Ctrl+End` | 跳到顶部或回到底部；手动翻阅时也可直接点击右下角提示回到底部 |
| 输入历史 | `↑` / `↓` | 空输入框回填最近提交，继续浏览更早或更新的输入；`Ctrl+P` / `Ctrl+N` 为兼容按键 |
| 复制选区 | 左键拖选后 `Ctrl+C` | 释放后底栏提示可复制；按 `Ctrl+C` 写入系统剪贴板并收起选区 |
| 复制回复 | `Ctrl+Y` | 复制最近一条助手回复 |
| 鼠标滚轮与选择 | 滚轮 / 左键拖选 | 滚轮浏览历史；拖选只保留高亮，不会在松开左键时覆盖系统剪贴板 |
| 滚动条 | 消息区右侧留白列上的竖条 | 内容超过一屏时显示；在轨道上按下或拖动即可定位到对应位置，拖到底部恢复自动跟随 |
| 工具详情 | 左键单击工具消息 | 工具结果默认紧凑显示；单击展开最多 40 行详情，再次单击收起 |
| 切换鼠标捕获 | `Ctrl+T` | 默认开启应用内滚轮、文本拖选和插件点击；关闭后恢复终端原生选择及右键菜单 |
| 切换插件焦点 | `Tab` | 在主输入区与可聚焦插件视图间循环 |

全部快捷键同时接受 `Control` 与 `Command`（终端上报为 `Super`）。macOS 上把
Control、Option 与 Command 互换以贴近 Windows 手感是常见做法，重映射后同一个物理键
到达终端时会变成另一个修饰键；两者语义不冲突，因此一并接受，不要求改回系统设置。
快捷键仍不生效时用 `/keys` 进入按键诊断，它会逐条回显终端实际上报的键名与修饰键，
并说明该组合是否被识别为命令修饰键。注意 `Option`（`Alt`）不是命令修饰键：它保留给
按词移动与换行手势。

`Ctrl+C` 与 `Ctrl+D` 都不会在一次按键内结束会话：前者需要在空输入上连按两次并有明确提示，后者只在输入为空时生效。退出待确认时输入框边框与标题会转为危险色。

终端支持键盘增强协议时，带修饰键的 `Enter` 会作为换行手势保留给编辑器。粘贴或拖入本地图片、文件路径时，TUI 会创建附件引用；单个附件上限为 10 MiB。

附件有两条加入方式：

- **粘贴或拖入**：终端送出的文本若是文件路径（绝对路径、`~/` 前缀、引号包裹，或相对当前项目目录），TUI 直接读取该文件。
- **`/attach <路径> [路径...]`**：手动添加，规则同上，一行可添加多个文件。斜杠命令不能携带既有附件，因此已有附件时需先移除。

从 macOS Finder 复制文件时，剪贴板的文本表示只有文件名、不含目录，因此只粘贴文件名无法定位文件。此时 TUI 会回查剪贴板的文件引用取得真实路径；仍无法定位时给出错误提示，而不是把文件名静默当成文本附件。Linux 与 Windows 的终端拖拽通常已给出完整路径，不走这条回查。

附件按 models.dev 声明的输入模态校验：模型不支持图片输入时会拒绝加入并在消息区说明原因。

## 斜杠命令

输入 `/` 会显示 TUI 原生提供的命令和用法。常用命令包括：

- `/resume`：打开当前项目的会话列表，选中后恢复完整会话。
- `/sessions`：只读浏览当前项目的会话摘要。
- `/plugins`：打开人工插件管理页；变更只影响新 Session，当前 Session 不热重载。
- `/compact`：请求原生 Context Loader 立即压缩当前上下文。
- `/attach`：把一个或多个本地文件加入待发送附件。

命令名称在本地同步补全，不依赖插件加载状态；默认插件版与纯 Core 版都提供斜杠命令和会话 Dialog。

## 会话行为

普通 `lucia` 启动会创建内存中的空白 Draft，不自动恢复上一次会话。用户发送第一条消息时才
按 revision/CAS 持久化该会话。

会话按启动目录隔离。实际路径为：

```text
<sessions_dir>/<project-id>/sessions
```

不同工作目录生成不同 `project-id`。恢复后，用户消息、助手文本和工具结果会重新显示；system、developer 和 thinking 内容不直接展示。

也可以在启动时显式恢复：

```bash
lucia --resume-latest
lucia --session-id design-review
```

## 插件界面

插件只能返回声明式 UI，不能直接操作终端。停靠视图通过 `Tab` 获得焦点；Dialog 显示时优先接收输入；进入插件子视图后按 `Esc` 返回上一层。

底栏会在启动阶段显示插件加载结果。加载完成后只在仍在加载或存在失败时保留加载状态；插件通过通用 `footer` 展示目标发布的当前模式会按可信插件来源常驻，例如权限模式不会进入对话消息或 Session。

## 退出与故障恢复

使用 `/exit`、`/quit`、空输入上的 `Ctrl+D`，或没有文本选区时连按两次 `Ctrl+C` 显式退出。Lucia 会在正常退出、错误和 panic 展开时恢复 raw mode、备用屏幕和 bracketed paste 状态。

模型或工具问题需要完整事件时使用：

```bash
lucia --events-jsonl ./runs/events.jsonl
```

配置、会话或插件启动异常可先运行 `lucia doctor`。全部启动参数见 [CLI 使用](/usage/cli)。
