# TUI 使用

本章面向直接在终端中使用 Lucia 的用户。插件版 TUI 只提供 Plugin Host；功能插件由用户显式安装、启用或通过 manifest 指定。

## 安装与启动

```bash
bun run install:tui
lucia plugin install context
lucia
```

`install:tui` 会构建插件版 `lucia` 并安装到 Cargo bin 目录，不会安装任何功能插件。`lucia plugin install` 管理的已启用插件会在启动时加载；未设置 `LUCIA_HOME` 时，应用目录是 `$HOME/.lucia`。

第一次启动会创建 `$LUCIA_HOME/config.toml`。配置不存在或模型密钥不可用时，TUI 使用本地演示模型；也可以显式运行：

```bash
lucia --demo
```

## 配置模型

推荐在配置中保存环境变量名，不保存密钥明文：

```toml
[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "替换为账号可用的模型 ID"
api_key_env = "OPENAI_API_KEY"
openai_protocol = "responses"
context_window = 200000

[agent]
max_steps = 0
max_tokens = 4096
stream = true

[tui]
sessions_dir = "projects"
# events_jsonl = "events.jsonl"
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
| 发送 | `Enter` | 提交当前输入；Agent 运行中提交的普通输入按 FIFO 排队 |
| 换行 | `Ctrl+J` | 在输入框中插入换行 |
| 中断或退出 | `Esc` | 运行中请求取消；有输入时清空；空闲且输入为空时退出 |
| 滚动历史 | `PgUp` / `PgDn` | 手动滚动消息区 |
| 输入历史 | `Ctrl+P` | 进入或退出历史输入回溯 |
| 复制回复 | `Ctrl+Y` | 复制最近一条助手回复 |
| 切换插件焦点 | `Tab` | 在主输入区与可聚焦插件视图间循环 |

终端支持键盘增强协议时，带修饰键的 `Enter` 会作为换行手势保留给编辑器。粘贴或拖入本地图片、文件路径时，TUI 会创建附件引用；单个附件上限为 10 MiB。

## 斜杠命令

输入 `/` 会显示官方 Command 插件提供的命令和用法。常用命令包括：

- `/resume`：打开当前项目的会话列表，选中后恢复完整会话。
- `/sessions`：只读浏览当前项目的会话摘要。
- `/compact`：请求 Context 插件立即压缩当前上下文。

命令名称从本地快照补全。光标位于命令参数时，显式按 `Tab` 才会请求动态候选。禁用 Command 插件后，斜杠命令和对应 Dialog 不可用，但已有会话文件不会被删除。

## 会话行为

普通 `lucia` 启动会创建内存中的空白 Draft，不自动恢复上一次会话。发送第一条消息时，TUI 先保存用户输入，再调用模型；这样模型请求失败时仍能保留原始输入。

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

底栏会在启动阶段显示插件加载结果。加载完成后只在仍在加载或存在失败时保留状态，避免长期占用对话区。

## 退出与故障恢复

空闲时按 `Esc` 退出。Lucia 会在正常退出、错误和 panic 展开时恢复 raw mode、备用屏幕和 bracketed paste 状态。

模型或工具问题需要完整事件时使用：

```bash
lucia --events-jsonl ./runs/events.jsonl
```

配置、会话或插件启动异常可先运行 `lucia doctor`。全部启动参数见 [CLI 使用](/usage/cli)。
