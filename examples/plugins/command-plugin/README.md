# Command Plugin

Lucia 官方斜杠命令 Provider。插件维护命令注册表、参数协议、执行计划和会话选择 Dialog；TUI 只负责缓存快照、调用服务、提供当前项目的会话摘要，并执行受控的 surface effect。

## 包结构

- `command-protocol`：无 Host 依赖的稳定 JSON 协议，包括 `CommandSpec`、参数、补全、执行计划和会话界面数据。
- `command-sdk`：第三方插件使用的类型化客户端、注册与注销封装，以及统一的 `command.callback` 路由器。
- `command-plugin`：官方 WASM Provider，内置 `/help`、`/resume`、`/new`、`/sessions`、`/clear`、`/compact`、`/exit` 和 `/quit`。`/compact` 生成受控会话动作，由原生 TUI 立即调用 Context 插件并持久化压缩结果。

## 服务

- `command.register`：注册或替换调用方拥有的命令。
- `command.unregister`：注销调用方拥有的命令。
- `command.snapshot`：返回带 `generation` 的只读命令快照。
- `command.prepare-completion`：在显式 Tab 或节流请求中识别当前参数，并返回本地候选、可信 owner 回调计划或宿主数据源请求。
- `command.prepare-execute`：解析输入、校验参数和 `agent_idle`，并返回 callback、surface action、插件界面或文本输出。
- `command.surface.update`：TUI 注入当前 `cwd` 的异步 `SessionSummary` 查询结果。
- `command.surface.poll-effects`：TUI 原子取出查询、恢复和关闭界面动作。

`plugin.toml` 将 surface 调用方限制为 `lucia-tui`。原生 TUI 调用这两个 surface 服务时必须使用相同的可信 `caller_id`。补全回调计划中的 `owner_plugin_id`、`service` 和 `handler_id` 全部来自 Provider 注册表，TUI 不接受输入或候选响应提供的 owner；调用第三方回调时使用 `caller_id=command`，SDK 会拒绝其他调用方。

Provider 在生成快照、补全或执行计划前会核对 Host 服务目录。某个 owner 的回调服务已经卸载或激活失败时，对应命令会被清理并推进 `generation`，不会在预览中长期留下不可执行条目。

## 参数候选

`CommandSpec.arguments[].completion` 支持 `static`、`callback` 和 `surface`。`Choice` 参数即使没有显式 completion 也会由 Provider 本地生成候选，`Session` 参数默认映射到 `sessions` 宿主数据源。`command.prepare-completion` 返回 `CompletionContext`，其中包含当前参数、已解码前缀和 UTF-8 字节替换范围，TUI 无需重复解析命令行。

该服务只生成计划，不会同步调用第三方插件或宿主数据源。TUI 应继续使用 `command.snapshot` 缓存完成逐键命令预览，只在用户按 Tab 或实现了节流时显式调用 `command.prepare-completion`。第三方插件可通过 `CommandClient::prepare_completion` 使用同一类型化协议，动态候选最终由 `CommandRouter` 路由 `CommandCallbackRequest::Complete`。

## 第三方插件注册

第三方插件依赖 `command-protocol` 和 `command-sdk`，在 `activate` 中安装回调服务并注册命令：

```rust
use agent_plugin::{PluginHostApi, ServiceCall};
use anyhow::Result;
use command_protocol::{ArgumentKind, ArgumentSpec, CommandInvocation, CommandSpec};
use command_sdk::{CommandHandler, CommandRouter};
use serde_json::{json, Value};

struct HelloHandler;

impl CommandHandler for HelloHandler {
    fn execute(&mut self, invocation: CommandInvocation) -> Result<Value> {
        Ok(json!({"name": invocation.arguments["name"][0]}))
    }
}

fn activate(router: &mut CommandRouter, host: &dyn PluginHostApi) -> Result<()> {
    router.install_callback_service(host)?;
    let spec = CommandSpec::new("hello", "发送问候", "向指定用户发送一条问候。")
        .with_argument(ArgumentSpec::required(
            "name",
            "接收问候的用户名称",
            ArgumentKind::String,
        ));
    router.register(host, spec, "hello-handler", HelloHandler)?;
    Ok(())
}

fn handle_service(router: &mut CommandRouter, call: ServiceCall) -> Result<Value> {
    router.handle_service(call)
}
```

依赖方 manifest 需要声明 `command` 插件依赖，保证 Provider 先激活：

```toml
[[dependencies]]
id = "command"
version = "^1.0"
optional = false
```

## `/resume` 数据流

1. TUI 调用 `command.prepare-execute`，输入为 `/resume`。
2. 插件打开 `command-session-dialog`，返回 `surface_opened`，并排队一个 `query_sessions` effect。
3. TUI 轮询 effect，只读取当前 `cwd` 的轻量会话摘要，再通过 `command.surface.update` 回传结果。
4. 插件通过 `render_ui` 展示搜索、加载、空、错误、分页和选中状态，通过 `on_ui_input` 处理全部交互。
5. 用户按 Enter 后，插件返回带 `session_id` 和 `revision` 的 `resume_session` effect；完整会话只在 TUI 校验后读取。

连续搜索请求会在插件内合并，查询响应必须匹配最新 `request_id`。输入热路径使用 `command.snapshot` 的本地缓存，不需要逐字符跨 WASM。列表选择超过可见高度时由插件滚动窗口，鼠标行号也会按当前窗口起点转换为绝对会话索引。

## 验证

```bash
cargo test --offline --manifest-path examples/plugins/command-plugin/Cargo.toml --workspace
cargo clippy --offline --manifest-path examples/plugins/command-plugin/Cargo.toml --workspace --all-targets -- -D warnings
cargo check --offline --manifest-path examples/plugins/command-plugin/Cargo.toml --target wasm32-wasip2 -p command-plugin
cargo test --offline --manifest-path examples/plugins/command-plugin/smoke-tests/Cargo.toml
```
