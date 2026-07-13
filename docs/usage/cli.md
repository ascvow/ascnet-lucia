# CLI 使用

`lucia` 的无子命令形式启动 TUI；`doctor` 和 `plugin` 是执行完即退出的管理命令。纯 Core 构建不包含 `plugin` 子命令和 `--plugin-manifest` 参数。

## 命令结构

```text
lucia [启动参数]
lucia doctor [--json] [--network]
lucia plugin <操作> [参数]
```

插件子命令的完整用法见[插件管理](/usage/plugin-management)。

## 启动参数

### `--init`

```bash
lucia --init
lucia --config ./lucia.toml --init
```

创建默认配置后退出。别名是 `--init-config`。初始化使用排他创建，不覆盖已有文件；macOS/Linux 下新文件权限为 `0600`。

### `--demo`

```bash
lucia --demo
```

强制使用内置脚本模型，不读取真实模型密钥，适合验证 TUI、会话和基础工具链。它仍会读取应用路径、会话参数和可用插件。

### `--config <path>`

```bash
lucia --config ./configs/lucia.toml
```

指定 TOML 配置文件。路径按当前工作目录解析，并覆盖 `LUCIA_CONFIG`、`LUCIA_HOME` 和默认应用目录。该参数是全局参数，也可用于 `doctor`。

### `--events-jsonl <path>`

```bash
lucia --events-jsonl ./runs/events.jsonl
```

把模型、工具、ReAct 和扩展生命周期事件按 JSONL 追加到文件。CLI 相对路径以当前工作目录为基准，并覆盖 `[tui].events_jsonl`。写入错误会作为运行错误报告，不会被静默忽略。

### `--sessions-dir <path>`

```bash
lucia --sessions-dir ./state/projects
```

覆盖项目会话根目录。CLI 相对路径以当前工作目录为基准；配置文件中的相对 `tui.sessions_dir` 则以配置文件目录为基准。

### `--session-id <id>`

```bash
lucia --session-id design-review
```

恢复并继续更新指定稳定会话。找不到会话、记录损坏或记录所属项目不匹配时返回错误。该参数优先于 `--resume-latest`。

### `--resume-latest`

```bash
lucia --resume-latest
```

恢复当前项目最近更新的会话。没有持久化记录时创建空白 Draft，不把“没有历史会话”视为错误。

### `--list-sessions`

```bash
lucia --list-sessions
```

按更新时间列出当前项目的会话摘要后退出，不连接模型服务。它适合在恢复前确认 session ID。

### `--plugin-manifest <path>`

```bash
lucia \
  --plugin-manifest ./plugins/context/plugin.toml \
  --plugin-manifest ./plugins/custom/plugin.toml
```

插件版 TUI 可重复传入该参数。manifest 按参数顺序加载并参与 UI 插槽顺序；同 ID 的显式 manifest 覆盖受管理插件和官方插件。加载失败会显示为插件启动错误，不会删除其他已加载插件。

## 诊断命令

```bash
lucia doctor
lucia doctor --json
lucia doctor --network
```

- 无参数：以文本报告检查应用目录、配置、模型字段、Session 路径、插件锁和运行时组合。
- `--json`：输出稳定 JSON 结构，便于脚本或 CI 消费。
- `--network`：额外只读检查 GitHub API 连通性。默认诊断不联网，也不会下载或安装插件。

诊断严格只读，不创建目录或配置，不修改插件锁，不打开会产生写入的 Session Store。

## 路径与优先级

| 项目 | 第一优先 | 第二优先 | 默认值 |
| --- | --- | --- | --- |
| 配置文件 | `--config` | `LUCIA_CONFIG` | `$LUCIA_HOME/config.toml` |
| 应用目录 | `LUCIA_HOME` | `$HOME/.lucia` | 当前目录的 `.lucia` |
| 会话根目录 | `--sessions-dir` | `tui.sessions_dir` | `$LUCIA_HOME/projects` |
| 启动会话 | `--session-id` | `--resume-latest` | 新的空白 Draft |
| 事件日志 | `--events-jsonl` | `tui.events_jsonl` | 不写入 |

配置文件中的 `api_key` 会覆盖 `api_key_env`。生产环境应优先使用 `api_key_env`，并由 shell、密钥管理器或部署环境提供值。

## 退出码与错误处理

参数冲突、配置解析失败、会话加载失败或管理命令失败时，`lucia` 返回非零退出码并把错误链写到标准错误。`--list-sessions`、`--init` 和管理命令成功后不会进入 TUI。
