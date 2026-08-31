# Manifest 与权限

## 基础结构

```toml
[plugin]
id = "example"
name = "Example Plugin"
version = "0.1.0"
api_version = "0.7.0"
wasm = "target/wasm32-wasip2/release/example_plugin.wasm"
description = "示例插件"

[capabilities]
process_exec = false
model_completion = false
fs_read = ["config", "assets/schema.json"]
surface_action_allowlist = ["set_input"]

[capabilities.tool_scopes]
example_status = "control"
example_run = "shell"

[metadata]
config_dir = "config"
```

`wasm`、`fs_read` 和插件元数据中的相对路径都以 `plugin.toml` 所在目录为基准。

## 插件依赖

```toml
[[dependencies]]
id = "audit-provider"
version = "^1.2"
optional = false
```

`version` 使用 SemVer 约束，默认是 `*`。Host 在实例化 component 前检查重复 ID、缺失必选依赖、版本不匹配和循环依赖，并按稳定拓扑顺序激活。可选依赖缺失时继续加载；已经安装但版本不匹配仍会失败。

依赖同时构成跨插件服务的授权边界：Guest 只能发现和调用自身 manifest 已声明依赖所提供的服务。插件通过[通用服务 API](/plugin/dependencies-services)公开和复用能力，Host 只执行 owner、依赖和路由校验，不理解服务的业务协议。

Host 会把每项依赖规范化为 `service_dependencies` 授权范围，格式为 `插件ID@版本约束;optional=布尔值`，并按字节序写入完整授权 profile。新增或删除依赖、改变版本范围、在必选与可选之间切换，都会改变安装身份并要求用户重新审查；旧版本的启用授权不能沿用。

## 功能能力声明与冲突

插件可以声明自己提供的通用能力。最终工具策略使用独占能力声明：

```toml
[[provides]]
id = "agent.tool-policy"
version = "1.0.0"
mode = "exclusive"
```

Host 会先串联普通插件的工具 Rewrite，再把最终调用交给选中的策略 owner。策略可以允许、阻止、重写或请求用户审批，避免后续 Rewrite 绕过安全检查。

`multi` 允许多个 provider 同时存在；`exclusive` 在同一运行时只能选择一个。Host 在实例化任意 component 前校验能力 ID、SemVer、基数一致性和独占冲突，但不理解能力背后的 MCP 或工具策略业务。

当多个插件提供同一独占能力时，应用配置必须显式选择 owner：

```toml
[capability_selection]
"agent.tool-policy" = "permission"
```

未选择、选择未声明该能力的插件、为 `multi` 能力指定唯一 owner 都会阻止启动。Host 不使用加载顺序静默覆盖插件。

## ABI 固有权限与授权 profile

Manifest capability 只描述额外 Host 资源，不是插件的全部权限。任何启用的 v0.7 Guest 都能贡献
developer prompt 和模型工具，读取并阻止、重写工具调用或返回 `CancelRun`，观察模型可见工具结果与
Host 脱敏后的生命周期事件，向应用事件流发布可更新主事件列表、Footer 或插件视图导航的扩展事件，
提供插件 UI 和自有工具渲染，读写按插件与 Bundle 身份隔离的状态，并提供插件服务或按 manifest
依赖 ACL 调用其他服务。

`PluginAuthorizationProfile` 把这些 ABI 固有权限与 `ManifestCapabilitySnapshot` 并列保存。Manager
使用完整 profile 比较官方升级能否继承启用授权，TUI 从同一 profile 逐项渲染启用审查。固有权限
不会写入 `ManifestCapabilitySnapshot.requested`，因为该结构还用于核对真实 Component import；把
ABI 基线混入其中会把“启用即可使用”误报为“当前 Guest 实际导入”。

## 当前权限

| Capability | 状态 | 边界 |
| --- | --- | --- |
| 纯计算 | 默认允许 | 受 Wasmtime fuel 与线性内存上限控制 |
| `fs_read` | 可用 | 仅允许声明路径及其目录后代 |
| `process_exec` | 可用 | 无 shell，stdin/stdout 由 Host 管理 |
| `agent` | 可用 | 分操作授权、profile allowlist、可信身份与卸载撤销 |
| `model_completion` | 可用 | 应用固定 provider/model/stream，Host 禁用工具与推理并限制输出 |
| `surface_action_allowlist` | 可用 | 按动作授权 `set_input`、会话操作或退出，禁止一项权限覆盖全部动作 |
| `tool_scopes` | 可用 | Host 按公开工具名绑定 scope；未声明工具降级为 `unknown`，已授 component 能力会进一步抬高风险下限 |
| `fs_write` | 尚未开放 | manifest 申请会被拒绝 |
| `http` | 尚未开放 | manifest 申请会被拒绝 |
| `secrets` | 尚未开放 | manifest 申请会被拒绝 |

Host 不继承 WASI 环境变量、预打开目录或 stdio。需要 I/O 的插件必须使用 `PluginHostApi`。

## 模型完成权限

```toml
[capabilities]
model_completion = true
```

Manifest 只允许插件请求受限模型完成；应用还必须通过 `PluginHostServices::with_model_completion` 注入已注册的模型网关、可信 provider/model、流式模式和最大输出 token。Guest 请求只包含 system、provider-neutral messages 和更小的输出上限，未知字段会被拒绝。Host 强制工具列表为空、`tool_choice = none`、推理关闭，并执行 ExecutionPolicy 中的累计调用次数和输入字节预算；预算按单个插件激活隔离，同一激活内的调用共享原子账本。模型返回工具调用或空文本时调用失败。

## 宿主动作权限

```toml
[capabilities]
surface_action_allowlist = ["set_input", "query_sessions"]
```

Host 会解析每个 `ui.host.action` 并按动作种类复核 allowlist；授权 `set_input` 不会同时获得清空会话或退出应用。旧 manifest 的 `surface_actions = true` 仅为兼容保留，等价于全部动作，新插件不得使用。动作词汇表与应答类型定义在 `agent_plugin_host::ui`（Guest 侧为 `agent_plugin` 同名类型），线格式由 `wit/fixtures/ui-v1.json` 的双侧契约测试保证。

## Agent Runtime 权限

```toml
[capabilities.agent]
spawn = true
observe = true
cancel = true
profiles = ["worker", "reviewer"]
```

三个布尔字段分别控制派生与续跑、查询和取消。声明 `spawn = true` 时必须至少配置一个 profile；未声明 spawn 时不能配置 profiles。Manifest 只描述插件请求的上限，应用还必须通过 `PluginHostServices` 注册同名 profile 并向当前 principal 授权，二者任一缺失都会拒绝加载或调用。

每次插件激活使用唯一 principal。Host 注入 owner 和 controller，并在激活失败或卸载时撤销该 principal 的全部 Agent Runtime 资源。teammate 消息权限由 teammate 插件自己的 manifest、服务和配置定义。旧 ABI 插件不能申请 Agent Runtime 权限。

## 子进程安全

`process_exec = true` 是高权限能力。WASM guest 仍在沙箱内，但它启动的原生程序不受 WASM 文件和网络限制。

- 只向可信插件授予。
- Host 不经过 shell，`command` 与 `args` 分开传递。
- 子进程环境先清空，只保留 PATH、HOME、TMPDIR、LANG、LC_ALL，再叠加请求环境。
- 每个插件实例的进程上限取 Host 硬上限 8 与 `ExecutionPolicy.limits.max_processes` 的较小值。
- Host 限制启动请求、参数数量、环境变量总量、单次 stdin 写入和单行 stdout 大小，避免高权限入口接收无界载荷。
- `process_exec` 仍等价于授予插件当前操作系统用户的原生进程权限；上述结构限制不能替代插件来源校验和用户信任决策。
- stdout 单行限制为 4 MiB，读取超时上限为 120 秒。
- token 应放在被忽略的本地配置，不写入 manifest 或源码。

## ABI 兼容

当前 manifest 版本是 `0.7.0`，Host 只接受该版本并要求 WIT 导出完整存在。WIT 函数表面发生破坏性变化时必须升级 ABI；当前 Host 不保留旧 component 的探测加载分支。
