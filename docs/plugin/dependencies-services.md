# 插件依赖与服务

Lucia 的插件组合分为两层：manifest 依赖保证插件存在、版本兼容和加载顺序；通用服务 API 负责运行时复用。它与 Fabric mod 的依赖关系相似，但服务载荷保持为插件自行定义的 JSON。

<div class="arch-flow">manifest dependencies
  -> Host 校验 SemVer 与循环
  -> provider 先 activate
  -> provider 注册服务
  -> dependent activate 并调用服务</div>

## 声明依赖

依赖 command 插件的 manifest：

```toml
[plugin]
id = "hello-command"
name = "Hello Command"
version = "1.0.0"
api_version = "0.6.0"
wasm = "target/wasm32-wasip2/release/hello_command.wasm"

[[dependencies]]
id = "command"
version = "^1.0"
optional = false
```

Host 保持原配置中无关插件的相对顺序，并保证 provider 先于 dependent 激活。组合宿主关闭时使用相反顺序，让 dependent 先完成清理。

## Command Provider 服务

官方 Command 插件在激活时注册 `command.register`、`command.unregister`、`command.snapshot`、`command.prepare-completion` 和 `command.surface.update` 五个 `1.0.0` 服务。Host 只保存服务描述、注入可信 `caller_id` 并路由 JSON；命令所有权、名称冲突、参数解析、补全、执行编排和 `/` 输入规则都由 Command 插件处理。前四个服务面向第三方插件与 SDK；`command.surface.update` 是宿主会话查询的应答入口，仅接受 manifest 配置的宿主调用方。

公开类型位于 `command-protocol`：

- `CommandSpec` 定义规范名称、别名、摘要、说明、用法、参数和可用状态；
- `ArgumentSpec` 定义必填、可选或可变位置参数，类型支持字符串、整数、布尔值、枚举和 Session；
- `CompletionSource` 支持随快照下发的静态候选、owner 回调候选和 TUI 数据源候选；
- `PrepareCompletionRequest` 与 `PrepareCompletionResponse` 让 Provider 返回 UTF-8 替换区间、本地候选或受控回调计划；
- `RegisterCommandRequest`、`UnregisterCommandRequest` 与 `CommandCallbackRequest` 是版本化服务载荷。

注册表按 Host 注入的插件 ID 记录 owner。插件只能替换或注销自己拥有的命令；执行计划始终携带注册时保存的 owner 与回调服务，不接受调用方覆盖目标。

## 使用类型化 SDK

下游插件通过 `command-sdk` 安装统一回调服务，再注册命令定义和本地处理器：

```rust
use agent_plugin::{PluginHostApi, ServiceCall};
use anyhow::Result;
use command_protocol::{
    ArgumentKind, ArgumentSpec, CommandInvocation, CommandSpec, CompletionItem,
    CommandCompletionRequest, CompletionSource,
};
use command_sdk::{CommandHandler, CommandRouter};
use serde_json::{json, Value};

struct HelloCommand;

impl CommandHandler for HelloCommand {
    fn execute(&mut self, invocation: CommandInvocation) -> Result<Value> {
        Ok(json!({ "name": invocation.arguments["name"][0] }))
    }

    fn complete(&mut self, _request: CommandCompletionRequest) -> Result<Vec<CompletionItem>> {
        Ok(vec![CompletionItem {
            label: "Lucia".into(),
            insert_text: "Lucia".into(),
            description: Some("默认称呼".into()),
        }])
    }
}

fn register_commands(router: &mut CommandRouter, host: &dyn PluginHostApi) -> Result<()> {
    router.install_callback_service(host)?;
    let spec = CommandSpec::new("hello", "发送问候", "向指定对象发送问候。")
        .with_argument(
            ArgumentSpec::required("name", "问候对象", ArgumentKind::String)
                .with_completion(CompletionSource::Callback),
        );
    router.register(host, spec, "hello-handler", HelloCommand)?;
    Ok(())
}

fn handle_command_service(router: &mut CommandRouter, call: ServiceCall) -> Result<Value> {
    router.handle_service(call)
}
```

插件应在 `AgentPlugin::activate` 中调用 `register_commands`，并在 `AgentPlugin::handle_service` 中把 `command.callback` 交给 `CommandRouter::handle_service`。动态候选最多返回 20 项；TUI 只在显式 Tab 时调用补全服务，并以 `caller_id=command` 执行 Provider 刚生成的可信回调计划。卸载单个命令时调用 `CommandRouter::unregister`，Provider 确认移除后 SDK 才释放本地处理器。

## 服务发现与宿主调用

Guest 使用 `list_services(Some("command"))` 检查可选能力。原生嵌入方使用 `PluginHost::services()` 获取可信目录，使用 `PluginServiceCall` 调用指定 owner。服务名只允许 ASCII 字母、数字、点、下划线和连字符，服务版本必须是完整 SemVer。

服务调用是同步 JSON RPC。Host 会阻止同步自调用和循环重入；耗时工作应由插件管理后台任务，再通过事件或状态返回进度。卸载插件会同时移除它的服务和调用端点。
