# 构建与打包

Lucia TUI 提供两个编译形态。常规构建默认启用插件系统；纯 Core 形态通过 `--no-default-features` 排除 Plugin Host 和 Wasmtime。

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

## 插件管理器

插件安装与诊断 CLI 使用独立输出目录：

```bash
bun run build:plugin-manager
```

产物位于 `target/plugin-manager/release/agent-plugin`。该程序只管理本地 bundle、锁文件和运行时配置，不加载 WASM component。

## 本地安装

使用 Bun 管理安装脚本。默认安装插件版本、构建并同步官方插件，同时确保新 zsh 会话可以找到 Cargo bin：

```bash
bun run install:tui
```

只安装纯 Core 版本：

```bash
bun run install:tui:core
```

默认安装会把官方 Context、MCP、Skill 与 Command bundle 同步到 `$LUCIA_HOME/official-plugins`，不会删除其中的用户配置或 Skill 文件。运行时文件先写入同目录临时文件并原子替换，`plugin.toml` 最后发布，避免并发启动读取到半写 WASM。安装后直接运行 `lucia`，无需额外插件参数。两个版本的命令名均为 `lucia`，因此同一 Cargo bin 目录内后安装的版本会覆盖前一个。分发两个压缩包时应使用独立 `--target-dir` 构建，并在归档名称中区分 `lucia-core` 与 `lucia-plugins`。

## 验证边界

```bash
cargo tree \
  -p lucia \
  --no-default-features \
  -e normal
```

输出中不应出现 `agent-plugin-host`、`wasmtime` 或 `wasmtime-wasi`。
