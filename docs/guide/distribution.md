# 构建与打包

Lucia 提供三个打包形态：Core 库、纯 Core TUI 与插件版 TUI。常规 TUI 构建默认启用插件系统；纯 Core 形态通过 `--no-default-features` 排除 Plugin Host 和 Wasmtime。

## Core 库

```bash
bun run build:core
```

产物：

```text
target/core/release/libagent_tool.rlib
target/core/release/libagent_core.rlib
target/core/release/libagent_session.rlib
```

`agent-tool`、`agent-core` 与 `agent-session` 组成可独立复用的最小 Agent 核心：ReAct 循环、模型网关、工具注册表与会话存储，不含任何 TUI、Plugin Host 或 Wasmtime 依赖。外部项目通过 path 或 git 依赖直接嵌入（三个 crate 均已声明内部依赖版本号，可按 `agent-tool → agent-core → agent-session` 顺序发布到注册表）。嵌入示例见 `examples/basic-cli` 与 `docs/agent/`。

## 纯 Core TUI

```bash
bun run build:tui:core
```

产物：

```text
target/core-tui/release/lucia
```

该二进制不会编译或链接：

- `agent-plugin-host`
- Wasmtime 与 WASI runtime
- plugin manifest 加载
- 插件四向插槽、Dialog 和输入路由
- `--plugin-manifest` CLI 参数

配置文件中的插件列表不会由纯 Core TUI 加载。模型、ContextLoader、原生工具、Agent 事件和 JSONL sink 保持可用。

## 插件版 TUI

```bash
bun run build:tui:plugins
```

产物：

```text
target/plugin-tui/release/lucia
```

该版本会自动加载 `$LUCIA_HOME/official-plugins/*/plugin.toml`。配置中的 `[[plugins]]` 和 `--plugin-manifest` 用于补充其他插件；同 ID 的显式插件优先。

该版本包含 Plugin Host、WASM component 加载、依赖解析、插件服务和插件 TUI。

## 同时构建

```bash
bun run build:tui
```

该命令依次构建纯 Core TUI 与插件版 TUI，并分别写入上述目录。

## 管理命令

插件版 `lucia` 同时包含 `lucia plugin` 和全局 `lucia doctor`。插件管理器仍是独立 library
crate，负责下载、安装和完整性规则，但不再分发单独的 `agent-plugin` 可执行程序。纯 Core
版本保留 `lucia doctor`，并跳过插件运行时检查。

## 本地安装

构建全部官方与示例插件、安装插件版 TUI 并同步官方 bundle：

```bash
bun run install:all
```

只构建并安装插件版 TUI 与默认官方 bundle，同时确保新 zsh 会话可以找到 Cargo bin：

```bash
bun run install:tui
```

只安装纯 Core 版本：

```bash
bun run install:tui:core
```

默认安装会把官方 Context、MCP、Skill、Command、Teammate、Plan 与 Sandbox bundle 同步到 `$LUCIA_HOME/official-plugins`，不会删除其中的用户配置或 Skill 文件。运行时文件先写入同目录临时文件并原子替换，`plugin.toml` 最后发布，避免并发启动读取到半写 WASM。安装后直接运行 `lucia`，无需额外插件参数。两个版本的命令名均为 `lucia`，因此同一 Cargo bin 目录内后安装的版本会覆盖前一个。分发两个压缩包时应使用独立 `--target-dir` 构建，并在归档名称中区分 `lucia-core` 与 `lucia-plugins`。

## 验证边界

```bash
cargo tree \
  -p lucia \
  --no-default-features \
  -e normal
```

输出中不应出现 `agent-plugin-host`、`wasmtime` 或 `wasmtime-wasi`。
