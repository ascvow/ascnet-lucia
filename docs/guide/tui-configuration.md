# TUI 配置与会话

`lucia` 的应用配置属于 TUI crate。Agent Core 只解析模型和 Agent 参数，不决定配置文件位置、会话目录或启动恢复策略。

## 初始化

默认配置路径是 `$LUCIA_HOME/config.toml`。未设置 `LUCIA_HOME` 时使用 `$HOME/.lucia/config.toml`：

```bash
bun run init:tui
```

也可以初始化指定文件：

```bash
cargo run -p lucia -- --config ./lucia.toml --init
```

初始化使用原子排他创建，不会覆盖已有文件。模板通过 `OPENAI_API_KEY` 环境变量读取密钥；运行前需要确认 `model.model` 是账号可用的模型 ID。

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
sessions_dir = "sessions"
default_session = "default"
resume_latest = false
events_jsonl = "events.jsonl"
```

`sessions_dir` 和 `events_jsonl` 的相对路径以配置文件所在目录为基准。CLI 的 `--sessions-dir` 与 `--events-jsonl` 仍以当前工作目录为基准，并覆盖配置值。

## 会话恢复

每次成功运行都会使用 revision/CAS 更新同一个 `SessionRecord`。首次保存会从第一条用户输入生成短标题。

```bash
# 恢复配置中的默认会话
lucia

# 恢复指定会话
lucia --session-id design-review

# 恢复最近更新的会话
lucia --resume-latest

# 不连接模型，仅列出会话
lucia --list-sessions
```

显式 `--session-id` 的优先级高于 `--resume-latest`。当最近会话不存在时，TUI 会创建配置中的 `default_session` 空记录。

恢复后，Session 中的用户消息、助手文本和工具结果会重新显示在主事件列表中；system、developer 和 thinking 内容不会直接展示。底栏显示当前 session ID 与 revision，方便确认当前写入目标。

## 路径覆盖

完整优先级如下：

| 项目 | 第一优先 | 第二优先 | 默认值 |
| --- | --- | --- | --- |
| 配置文件 | `--config` | `LUCIA_CONFIG` | `$LUCIA_HOME/config.toml` |
| 会话目录 | `--sessions-dir` | `tui.sessions_dir` | `$LUCIA_HOME/sessions` |
| 会话 ID | `--session-id` | `tui.default_session` | `default` |
| 事件日志 | `--events-jsonl` | `tui.events_jsonl` | 不写入 |

不要在配置中保存 API key。优先使用 `model.api_key_env`，并让 shell、密钥管理器或部署环境提供对应变量。
