# 其他使用方式

本章收录不属于日常 TUI、CLI 参数或插件管理的运行方式，包括离线示例、不同分发形态、诊断、事件排障和真实模型测试。

## 离线 CLI 示例

```bash
cargo run -p agent-basic-cli -- --demo "你好"
```

`agent-basic-cli` 使用确定性的脚本模型演示完整 ReAct 循环。模型先请求 `echo` 工具，再读取工具结果生成最终文本，不需要 API key 或网络。

加载 WASM Echo 插件：

```bash
bun run build:plugin:echo
cargo run -p agent-basic-cli -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml \
  "hello from wasm"
```

未传 manifest 时示例使用原生 Echo fallback；传入 manifest 后，同名工具由 WASM 插件提供。

## 构建形态

### 纯 Core TUI

```bash
bun run build:tui:core
```

产物位于 `target/core-tui/release/lucia`。该版本不编译 Plugin Host、Wasmtime 或插件 UI，也没有 `lucia plugin` 和 `--plugin-manifest`，但保留模型、原生工具、会话、ContextLoader 和事件日志。

### 插件版 TUI

```bash
bun run build:tui:plugins
```

产物位于 `target/plugin-tui/release/lucia`，包含 Plugin Host 和插件管理命令。构建不会自动安装到 Cargo bin，也不会同步官方 bundle。

### 安装

```bash
bun run install:tui
# 或只安装纯 Core 版本
bun run install:tui:core
```

两个版本的可执行文件都叫 `lucia`，后安装的版本会覆盖 Cargo bin 中的前一个版本。

## 全局诊断

```bash
lucia doctor
lucia doctor --json
lucia doctor --network
```

诊断只读检查配置、Session 路径、插件锁、manifest、依赖和运行时组合。默认不联网；`--network` 仅增加 GitHub API 读取检查，不下载插件。

## 事件排障

```bash
lucia --events-jsonl ./runs/events.jsonl
```

JSONL 记录 run、模型、工具和扩展事件。排查时优先按同一 `run_id` 关联事件，并检查：

1. `ModelRequest` 是否使用预期 provider 和 model。
2. 工具名称是否由预期的原生注册表或插件 owner 提供。
3. 工具失败是返回 `ToolResult::error`，还是整个调用返回 Rust 错误。
4. 运行是否以正常完成、取消或错误状态结束。

事件文件可能包含提示内容、工具参数和结果，不应提交到公开仓库。

## 真实模型测试

仓库的 live test 默认不会自动使用任意环境变量。按[真实模型测试](/guide/live-testing)准备显式配置后运行：

```bash
bun run test:live
```

插件场景需要先构建对应 WASM component。live test 涉及真实网络和费用，不应替代离线单元测试。

## 性能检查

```bash
bun run perf:plugin:host
bun run perf:plugin:wasm
```

Host 微基准用于比较组合、路由和 UI 开销；WASM 性能示例覆盖真实 component 边界。需要把阈值作为失败门禁时运行 `bun run perf:plugin:gate`。
