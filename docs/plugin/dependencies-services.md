# 插件依赖与服务

Lucia 的插件组合分为两层：manifest 依赖保证插件存在、版本兼容和加载顺序；通用服务 API 负责运行时复用。它与 Fabric mod 的依赖关系相似，但服务载荷保持为插件自行定义的 JSON。

<div class="arch-flow">manifest dependencies
  -> Host 校验 SemVer 与循环
  -> provider 先 activate
  -> provider 注册服务
  -> dependent activate 并调用服务</div>

## 声明依赖

依赖另一个插件的 manifest：

```toml
[plugin]
id = "audit-consumer"
name = "Audit Consumer"
version = "1.0.0"
api_version = "0.7.0"
wasm = "target/wasm32-wasip2/release/audit_consumer.wasm"

[[dependencies]]
id = "audit-provider"
version = "^1.0"
optional = false
```

Host 保持原配置中无关插件的相对顺序，并保证 provider 先于 dependent 激活。组合宿主关闭时使用相反顺序，让 dependent 先完成清理。

## 服务发现与宿主调用

Guest 使用 `list_services(Some("audit-provider"))` 检查可选能力。原生嵌入方使用 `PluginHost::services()` 获取可信目录，使用 `PluginServiceCall` 调用指定 owner。服务名只允许 ASCII 字母、数字、点、下划线和连字符，服务版本必须是完整 SemVer。

服务调用是同步 JSON RPC。Host 会阻止同步自调用和循环重入；耗时工作应由插件管理后台任务，再通过事件或状态返回进度。卸载插件会同时移除它的服务和调用端点。
