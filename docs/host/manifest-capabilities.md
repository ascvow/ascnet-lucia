# Manifest 与权限

## 基础结构

```toml
[plugin]
id = "example"
name = "Example Plugin"
version = "0.1.0"
api_version = "0.10.0"
wasm = "target/wasm32-wasip2/release/example_plugin.wasm"
description = "示例插件"

[capabilities]
process_exec = false
model_completion = false
workspace_read = false
fs_read = ["config", "assets/schema.json"]
surface_action_allowlist = ["set_input"]

[capabilities.tool_scopes]
example_status = "control"
example_run = "shell"

[metadata]
config_dir = "config"
```

`wasm`、`fs_read` 和插件元数据中的相对路径都以 `plugin.toml` 所在目录为基准；`~/` 前缀表示宿主 home 目录，是唯一支持的展开形式，`~user/` 与嵌在中间的 `~` 都按普通字符处理。展开与其他路径一样要经过 canonicalize 与 allowlist 校验。

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
| `workspace_read` | 可用 | Host 注入当前项目的可信规范化根，只允许读取根及其后代；工具权限下限为 `read` |
| `fs_read` | 可用 | 仅允许声明路径及其目录后代；支持 `~/` 展开 |
| `process_exec` | 可用 | 无 shell，stdin/stdout 由 Host 管理 |
| `agent` | 可用 | 分操作授权、profile allowlist、可信身份与卸载撤销 |
| `model_completion` | 可用 | 应用固定 provider/model/stream，Host 禁用工具与推理并限制输出 |
| `surface_action_allowlist` | 可用 | 按动作授权 `set_input`、会话操作或退出，禁止一项权限覆盖全部动作 |
| `storage` | 可用 | 仅本插件私有命名空间，按 `workspace`/`user` 作用域隔离；宿主未提供项目身份时 `workspace` 不可用 |
| `sessions` | 可用 | 只读当前项目历史会话；无写入路径，投影剔除私有推理与图片字节，并把工具权限下限抬到 `unknown` |
| `socket` | 可用 | 只连接 allowlist 中的 Unix domain socket；只搬字节，不解析上层协议；工具权限下限为 `network` |
| `env_read` | 可用 | 只注入 manifest 逐个列出的环境变量；默认不继承任何宿主环境；工具权限下限为 `read` |
| `tool_scopes` | 可用 | Host 按公开工具名绑定 scope；未声明工具降级为 `unknown`，已授 component 能力会进一步抬高风险下限 |
| `fs_write` | 尚未开放 | manifest 申请会被拒绝 |
| `http` | 可用 | 应用绑定端点和凭证、manifest 主机 allowlist、禁止重定向及有界请求/响应 |
| `secrets` | 尚未开放 | manifest 申请会被拒绝 |

Host 默认不继承 WASI 环境变量、不预打开目录、不接 stdio。唯一的环境变量入口是 manifest 逐个列出的 `env_read`；其余 I/O 必须使用 `PluginHostApi`。

## 当前工作区只读权限

```toml
[capabilities]
workspace_read = true
```

允许插件通过现有 `PluginHostApi::read_file` 与 `list_dir` 读取当前项目。实际根目录不写入 manifest，
而是由应用调用 `PluginHostServices::with_workspace_root` 注入；Host 会在注入时规范化并确认路径存在且
为目录。每次访问仍会重新规范化目标并校验根前缀，因此父目录穿越和指向项目外的符号链接都会被
拒绝。`list_dir` 返回相对当前项目根的路径。

未注入工作区根时能力调用失败，不会退化成插件安装目录。插件也不能仅凭激活元数据获得读取权：
manifest 声明与 Host 可信根必须同时存在。该能力复用 `host-fs-read` / `host-fs-list`，没有增加 WIT
函数，适用于 linter、依赖分析和代码上下文等所有仓库感知插件。

## 持久存储权限

```toml
[capabilities]
storage = true
```

允许插件读写自己的私有持久存储。插件只能访问自身命名空间，无法枚举或触达其他插件的数据，
也不能指定磁盘路径——路径由 Host 按插件 ID 与作用域派生。

两个作用域：`workspace` 跟随当前项目，`user` 跨项目共享。项目作用域由应用通过
`PluginHostServices::with_workspace_scope` 注入；未注入时 `workspace` 访问失败，而不是退化为
跨项目共享。

与实例状态的区别是**生命周期**而非权限范围：

| | 实例状态 (`state`) | 持久存储 (`store`) |
| --- | --- | --- |
| 插件代码变化 | 清空 | 保留 |
| 适用内容 | 游标、缓存等可重建的派生数据 | 记忆、索引等用户资产 |

实例状态把 Component 字节混入身份，因此升级插件即换命名空间——这对派生数据是正确的，但对
用户资产意味着一次重建就清空。持久存储因此不混入代码字节；替换 Bundle 冒名读取旧数据由
Plugin Manager 的完整性锁与授权摘要在安装层拦截，而不是靠存储层每次哈希字节。

存储受条目数与总字节配额约束，超限写入失败而不是静默丢弃。

## Unix domain socket 权限

```toml
[capabilities.socket]
allow_paths = ["~/Library/Application Support/Eden/agent.sock"]
```

允许插件通过 `host-socket-*` 连接 manifest 列出的本地 socket。`~/` 表示宿主 home 目录，
由 Host 展开；其余必须是绝对路径。声明空 allowlist 会在加载前被拒绝，避免把「申请了本地
通信但没有明确服务」误当成安全配置。

每次连接都会：

1. 拒绝相对路径、`..` 段与 `@` 抽象命名空间；
2. canonicalize 请求路径与 allowlist，再逐条精确比较——不做前缀或通配符匹配，否则同目录下
   任意本地服务都会被一并授权；
3. 要求目标文件类型确实是 socket，普通文件不能借这条能力变成文件读取；
4. 按 `ExecutionPolicy.permits_network_access` 复核，受限档位即使 manifest 声明也不放行。

结构限制：单个插件实例最多 4 条并发连接，单次读写 1 MiB，单次读取等待 2 秒，单条连接累计
读取 16 MiB；超过后必须重连。socket 与子进程共用 `ExecutionPolicy.limits.max_processes` 这一
原生句柄维度，把它设为 0 会同时关闭两者。Host 只搬运字节，不解析 NDJSON、JSON-RPC 或任何
具体协议。

allowlist 约束的是**路径**，不是对端身份：同一用户可以替换该路径下的 socket 文件，连接之后
真正证明对端身份的是协议自己的握手（例如令牌）。Host 不替插件判断对端是否可信。

该能力把插件全部工具的权限下限抬到 `network`：本地 socket 能访问的服务不亚于一条网络连接。

## 环境变量读取权限

```toml
[capabilities]
env_read = ["EDEN_SESSION", "EDEN_SOCKET", "EDEN_TOKEN_FILE"]
```

WASI 默认不继承任何宿主环境变量——宿主进程里通常就有模型 API 密钥，整体继承等于把凭证交给
插件。需要运行时身份的插件必须逐个列出变量名，Host 只把这些变量注入 WASI 环境表，其余仍然
不可见；变量名只接受大写字母、数字与下划线，并拒绝重复声明。

缺失的变量不会注入，也不会报错：插件应按「拿不到就用默认值」处理。该能力把工具权限下限抬到
`read`——它与 `fs_read` 同档，都是 manifest 逐项声明的只读数据。

## 只读会话权限

```toml
[capabilities]
sessions = true
```

允许插件列举并读取当前项目的历史会话。应用必须通过
`PluginHostServices::with_session_source` 注入只读来源；未注入时该能力失败，而不是
返回空列表——空列表会让插件把「宿主没接线」误读成「这个项目没有历史会话」。

注入的是只读端口而不是 `SessionStore`：后者带有 `save` 与 `delete`，把它交给插件宿主
就等于宿主持有了改写历史会话的能力，哪怕当前没有代码去调用。用类型而不是纪律来保证
插件路径不可能写会话。

**该能力会把插件全部公开工具的权限下限抬到 `unknown`。** 原因是它的覆盖面远大于名字：
会话内容是 Agent 历史上见过的一切的并集——文件正文、shell 输出、网络返回，横跨全部
过去的运行。持有它的插件因此能读到用户很久以前、为了另一件事才批准过的数据。按 `read`
处理会低估这个面，让插件凭一次授权拿到远超当次意图的东西。

投影在 Host 内完成，不下放给应用，避免不同宿主各自实现出宽窄不一的脱敏口径：

- 剔除模型私有推理块——推理可能携带只能回传给来源路由的签名，暴露给插件既无检索价值，
  又会让私有推理越过它的路由边界
- 图片只保留存在标记，不投影 base64 字节
- 单条正文与工具结果按字符上限截断并标注截断量
- 单页按消息数与字节数双重限制；达到字节预算提前收尾，但至少返回一条，保证调用方能推进

## 模型完成权限

默认 `ExecutionPolicy::serve()` 不限制插件激活期间的模型调用总次数或累计输入字节数，避免长会话中的安全审核耗尽额度。应用仍可显式配置 `max_plugin_model_completions`、`max_plugin_model_input_bytes` 收紧预算；单次输出上限、能力鉴权及 Evaluation 等其他策略的限额继续生效。

```toml
[capabilities]
model_completion = true
```

Manifest 只允许插件请求受限模型完成；应用还必须通过 `PluginHostServices::with_model_completion` 注入已注册的模型网关、可信 provider/model、流式模式、最大输出 token 和提示缓存保留策略，并可用 `with_plugin_model_completion` 为单个插件覆盖该路由。Guest 请求只包含 system、provider-neutral messages 和更小的输出上限，未知字段会被拒绝。Host 强制工具列表为空、`tool_choice = none`、推理关闭，按调用方插件 ID 生成缓存键，并执行 ExecutionPolicy 中的累计调用次数和输入字节预算；预算按单个插件激活隔离，同一激活内的调用共享原子账本。模型返回工具调用或空文本时调用失败。

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
collect = true
profiles = ["worker", "reviewer"]
```

四个布尔字段分别控制派生与续跑、查询、取消和回收。声明 `spawn = true` 时必须至少配置一个 profile；未声明 spawn 时不能配置 profiles。Manifest 只描述插件请求的上限，应用还必须通过 `PluginHostServices` 注册同名 profile 并向当前 principal 授权，二者任一缺失都会拒绝加载或调用。

`collect = true` 允许插件在目标进入终态后请 Runtime 释放该实例的私有会话、终态结果、事件历史与运行选项，并把它的累计名额归还 principal。回收不移除谱系锚点，因此后续派生仍能沿父指针追溯到 controller；回收后该实例不能再续跑，也读不到终态结果，调用方应先读取结果再回收。这使“每次派活都是一个新实例、完成后回收”的长期协作模式不消耗生命周期配额。

每次插件激活使用唯一 principal。Host 注入 owner 和 controller，并在激活失败或卸载时撤销该 principal 的全部 Agent Runtime 资源。teammate 消息权限由 teammate 插件自己的 manifest、服务和配置定义。旧 ABI 插件不能申请 Agent Runtime 权限。

## 子进程安全

`process_exec = true` 是高权限能力。WASM guest 仍在沙箱内，但它启动的原生程序不受 WASM 文件和网络限制。

- 只向可信插件授予。
- Host 不经过 shell，`command` 与 `args` 分开传递。
- 子进程环境先清空，只保留 PATH、HOME、TMPDIR、LANG、LC_ALL，再叠加请求环境。
- 每个插件实例的进程上限取 Host 硬上限 16 与 `ExecutionPolicy.limits.max_processes` 的较小值（默认 Serve 为 8）。
- Host 限制启动请求、参数数量、环境变量总量、单次 stdin 写入和单行 stdout 大小，避免高权限入口接收无界载荷。
- `process_exec` 仍等价于授予插件当前操作系统用户的原生进程权限；上述结构限制不能替代插件来源校验和用户信任决策。
- stdout 单行限制为 4 MiB，读取和写入超时上限为 120 秒。读取会在扩容前检查长度；
  超大行会回收句柄，超时保留已读取前缀供重试，避免将半条消息误作新消息。
- token 应放在被忽略的本地配置，不写入 manifest 或源码。

## 资源边界与授权复核

服务调用者身份在执行任何 Guest 代码前登记，初始化与 `list-tools` 阶段同样执行 manifest
依赖校验；Guest 请求不能冒充可信应用调用。加载失败或取消会撤销已登记的身份和服务。

授权快照包含 `socket.allow_paths`、`env_read`、`data_dir` 和 `agent.collect`。旧授权摘要不能
覆盖新增权限，权限变化后必须重新审查；这不改变 manifest ABI 或 Manager lock schema。

- `host-fs-read` 仅读取常规 UTF-8 文件，单次最多 8 MiB；实际读取也有上限，不仅检查文件长度。
- `host-fs-list` 最多返回 10,000 项，路径正文合计最多 8 MiB；超限明确报错，不返回伪完整列表。
- 文件正文读取和目录遍历在阻塞线程执行；这不是对慢文件系统的强制终止保证。
- 每插件活跃工具最多 1,024 项、提示最多 256 项、排队事件最多 1,024 项。
  单项最多 1 MiB，各类合计最多 8 MiB；删除或排空后归还预算，不设生命周期累计配额。
- Component UI 调用排队最多 250ms、执行最多 2 秒；工具/服务/事件/上下文的排队和执行各最多
  30 分钟，实例化、静态工具枚举与界面声明各最多 120 秒。排队超时不销毁健康实例，执行超时
  则销毁 Store、注销服务并启动 Runtime 撤销；必须重新加载才可再次调用。调用方取消等待不会
  取消独立清理。fuel 的协作 yield 始终启用；这不是对阻塞系统调用或外部副作用的硬抢占/回滚。
- 默认每个线性内存最多 64 MiB，单 Store 最多 4 个 memory、64 个 core instance、16 个 table，
  单 table 最多 100,000 元素；这些不是整个进程 RSS 的上限。
- 嵌套服务调用继承父导出的截止时间，不能利用独立任务延长预算；卸载钩子最多 60 秒，
  卸载的注销/资源撤销在独立任务中收尾，取消等待不会跳过清理。
- Component 读取和编译在阻塞线程执行，并发最多 3 个任务；许可由实际任务持有，取消等待不
  提前释放许可，也不宣称编译线程已被终止。
- UI 帧 JSON 最多 1 MiB、4,096 行、每行 1,024 个 span；工具帧还受请求的 `max_height` 约束。
- 插件私有数据目录拒绝最终目录和数据父目录的符号链接，Unix 权限收紧为 0700；不设数据总容量上限。

## ABI 兼容

当前 manifest 版本是 `0.10.0`，Host 只接受该版本并要求 WIT 导出完整存在。WIT 函数表面发生破坏性变化时必须升级 ABI；当前 Host 不保留旧 component 的探测加载分支。
