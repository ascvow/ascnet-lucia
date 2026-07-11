# Manifest 与权限

## 基础结构

```toml
[plugin]
id = "example"
name = "Example Plugin"
version = "0.1.0"
api_version = "0.6.0"
wasm = "target/wasm32-wasip2/release/example_plugin.wasm"
description = "示例插件"

[capabilities]
process_exec = false
fs_read = ["config", "assets/schema.json"]

[metadata]
config_dir = "config"
```

`wasm`、`fs_read` 和插件元数据中的相对路径都以 `plugin.toml` 所在目录为基准。

## 插件依赖

```toml
[[dependencies]]
id = "command"
version = "^1.2"
optional = false
```

`version` 使用 SemVer 约束，默认是 `*`。Host 在实例化 component 前检查重复 ID、缺失必选依赖、版本不匹配和循环依赖，并按稳定拓扑顺序激活。可选依赖缺失时继续加载；已经安装但版本不匹配仍会失败。

依赖本身不提供跨插件调用。插件通过[通用服务 API](/plugin/dependencies-services)公开和复用能力，Host 不理解服务的业务协议。

## 功能能力声明与冲突

插件可以声明自己提供的通用能力：

```toml
[[provides]]
id = "agent.context-loader"
version = "1.0.0"
mode = "exclusive"
```

最终工具策略同样使用独占能力声明：

```toml
[[provides]]
id = "agent.tool-policy"
version = "1.0.0"
mode = "exclusive"
```

Host 会先串联普通插件的工具 Rewrite，再把最终调用交给选中的策略 owner。策略可以允许、阻止、重写或请求用户审批，避免后续 Rewrite 绕过安全检查。

`multi` 允许多个 provider 同时存在；`exclusive` 在同一运行时只能选择一个。Host 在实例化任意 component 前校验能力 ID、SemVer、基数一致性和独占冲突，但不理解能力背后的压缩、MCP 或 Skill 业务。

当多个插件提供同一独占能力时，应用配置必须显式选择 owner：

```toml
[capability_selection]
"agent.context-loader" = "context"
```

未选择、选择未声明该能力的插件、为 `multi` 能力指定唯一 owner 都会阻止启动。Host 不使用加载顺序静默覆盖插件。

## 当前权限

| Capability | 状态 | 边界 |
| --- | --- | --- |
| 纯计算 | 默认允许 | 受 Wasmtime fuel 与线性内存上限控制 |
| `fs_read` | 可用 | 仅允许声明路径及其目录后代 |
| `process_exec` | 可用 | 无 shell，stdin/stdout 由 Host 管理 |
| `agent` | 可用 | 分操作授权、profile allowlist、可信身份与卸载撤销 |
| `fs_write` | 尚未开放 | manifest 申请会被拒绝 |
| `http` | 尚未开放 | manifest 申请会被拒绝 |
| `secrets` | 尚未开放 | manifest 申请会被拒绝 |

Host 不继承 WASI 环境变量、预打开目录或 stdio。需要 I/O 的插件必须使用 `PluginHostApi`。

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
- 每个插件实例最多同时运行 16 个进程。
- Host 限制启动请求、参数数量、环境变量总量、单次 stdin 写入和单行 stdout 大小，避免高权限入口接收无界载荷。
- `process_exec` 仍等价于授予插件当前操作系统用户的原生进程权限；上述结构限制不能替代插件来源校验和用户信任决策。
- stdout 单行限制为 4 MiB，读取超时上限为 120 秒。
- token 应放在被忽略的本地配置，不写入 manifest 或源码。

## ABI 兼容

当前 manifest 版本是 `0.6.0`，Host 只接受该版本并要求 WIT 导出完整存在。WIT 函数表面发生破坏性变化时必须升级 ABI；当前 Host 不保留旧 component 的探测加载分支。
