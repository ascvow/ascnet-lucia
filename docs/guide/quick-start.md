# 快速开始

这条路径先使用内置模型跑通流程，再安装 TUI，最后接入真实模型。第一次验证不需要 API key 或网络请求。

## 环境

- Rust stable；版本与组件以仓库的 `rust-toolchain.toml` 为准。
- Bun；文档、构建和安装脚本统一通过 Bun 运行。
- `wasm32-wasip2`；工具链文件已经声明该 target，缺失时可手动安装。

```bash
rustup target add wasm32-wasip2
cargo check --workspace
```

## 1. 运行离线示例

在仓库根目录执行：

```bash
cargo run -p agent-basic-cli -- --demo "你好"
```

这个命令使用确定性的内置模型，不会连接外部服务。模型会调用原生 `echo` 工具，随后输出工具结果，因此可以同时验证模型循环、工具注册和结果回传。

## 2. 安装并启动 TUI

构建插件版 TUI、安装 `lucia` 命令，并显式选择需要的插件：

```bash
bun run install:tui
lucia plugin install context
lucia --demo
```

`--demo` 强制使用内置模型。退出后直接运行 `lucia` 即可按配置连接真实服务。

首次运行时，Lucia 会创建 `$HOME/.lucia/config.toml`。也可以只初始化配置并退出：

```bash
lucia --init
```

初始化不会覆盖已有文件。未设置模型密钥时，普通 `lucia` 启动也会进入本地演示模式，并在界面中显示配置提示。

## 3. 配置真实模型

编辑 `$HOME/.lucia/config.toml`：

```toml
[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "替换为账号可用的模型 ID"
api_key_env = "OPENAI_API_KEY"
openai_protocol = "responses"

[agent]
# 0 表示交互主会话不设置总 ReAct 步数上限。
max_steps = 0
max_tokens = 4096
# 默认 true；设为 false 时等待完整模型响应。
stream = true

[tui]
sessions_dir = "projects"
```

`api_key_env` 保存的是环境变量名。设置密钥后启动：

```bash
export OPENAI_API_KEY="你的密钥"
lucia
```

要使用 Anthropic 或本地 OpenAI-compatible 服务，参考[常用场景示例](/guide/examples)。配置路径优先级和全部 TUI 字段见 [TUI 配置与会话](/guide/tui-configuration)。

## 4. 使用会话

普通启动先进入空白 Draft，发送第一条消息后才写入当前项目的会话目录。TUI 中输入 `/resume` 可以选择历史会话；CLI 也提供显式操作：

```bash
lucia --list-sessions
lucia --resume-latest
lucia --session-id design-review
```

Lucia 按启动目录隔离项目会话。在不同目录运行时，看到的会话列表可能不同。

## 5. 选择下一步

- 想直接使用：阅读[常用场景示例](/guide/examples)和 [TUI 配置与会话](/guide/tui-configuration)。
- 想嵌入 Rust：阅读 [Agent API](/agent/api)和[工具与事件](/agent/tools-events)。
- 想扩展能力：阅读[创建 WASM 插件](/plugin/quick-start)和[测试与调试](/plugin/testing)。
- 想理解仓库：阅读[架构边界](/guide/architecture)。
