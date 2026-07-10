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

## Provider 注册服务

command 插件可以把命令注册和执行定义为自己的协议。Host 只保存服务描述并路由 JSON：

```rust
use agent_plugin::{PluginHostApi, ServiceCall, ServiceSpec};
use serde_json::{json, Value};

fn activate(host: &dyn PluginHostApi) -> anyhow::Result<()> {
    host.upsert_service(&ServiceSpec {
        name: "command.register".into(),
        version: "1.0.0".into(),
        description: Some("注册斜杠命令".into()),
    })?;
    host.upsert_service(&ServiceSpec {
        name: "command.execute".into(),
        version: "1.0.0".into(),
        description: Some("执行斜杠命令".into()),
    })
}

fn handle_service(call: ServiceCall) -> anyhow::Result<Value> {
    match call.name.as_str() {
        "command.register" => {
            // call.caller_id 是 Host 注入的可信插件 ID，可作为 handler owner。
            register_command(call.caller_id, call.payload)?;
            Ok(json!({"registered": true}))
        }
        "command.execute" => execute_command(call.payload),
        _ => anyhow::bail!("未知 command 服务：{}", call.name),
    }
}
```

实际插件在 `AgentPlugin::activate` 和 `AgentPlugin::handle_service` 中调用这些逻辑。`command.register` 的字段、命令冲突规则、补全和 `/` 输入处理都属于 command 插件，不进入 Host。

## Dependent 复用服务

下游插件先注册自己的回调服务，再向 command provider 注册命令：

```rust
host.upsert_service(&ServiceSpec {
    name: "hello-command.execute".into(),
    version: "1.0.0".into(),
    description: Some("处理 hello 命令".into()),
})?;

host.call_service(
    "command",
    "command.register",
    &json!({
        "name": "hello",
        "handler_service": "hello-command.execute"
    }),
)?;
```

command 插件执行 `/hello Lucia` 时，可根据注册信息回调 owner：

```rust
let result = host.call_service(
    &registration.plugin_id,
    &registration.handler_service,
    &json!({"args": ["Lucia"]}),
)?;
```

## 服务发现与宿主调用

Guest 使用 `list_services(Some("command"))` 检查可选能力。原生嵌入方使用 `PluginHost::services()` 获取可信目录，使用 `PluginServiceCall` 调用指定 owner。服务名只允许 ASCII 字母、数字、点、下划线和连字符，服务版本必须是完整 SemVer。

服务调用是同步 JSON RPC。Host 会阻止同步自调用和循环重入；耗时工作应由插件管理后台任务，再通过事件或状态返回进度。卸载插件会同时移除它的服务和调用端点。
