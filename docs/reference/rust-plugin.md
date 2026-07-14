# Plugin SDK、Host 与 Manager API

本页覆盖 `agent-plugin`、`agent-plugin-host` 和 `agent-plugin-manager`。Guest SDK 面向插件作者；Host API 面向嵌入 Lucia 的应用；Manager API 面向安装和状态管理。三者的所有权不同，不应互相绕过。

## agent-plugin

### `AgentPlugin`

插件类型必须实现 `Default + Send + 'static`。`export_plugin!(Type)` 为每个 component 实例保持一个插件对象，并把 WIT JSON 转换为下列 Rust 方法。

| 方法 | 参数 | 返回 | 错误与副作用 |
| --- | --- | --- | --- |
| `activate(host, context)` | 受限 Host API；可信 plugin ID 与 manifest metadata | `Result<()>` | 错误阻止当前插件进入 Ready；适合注册动态贡献和启动长驻资源 |
| `deactivate(host)` | 当前实例 Host API | `Result<()>` | 应终止进程并移除动态贡献；错误会记录但实例仍会卸载 |
| `list_tools()` | 无 | `Vec<ToolSpec>` | 只返回静态工具，不应做 I/O |
| `call_tool(call)` | `ToolCall` | `Result<ToolResult>` | 只适合不需要 Host 能力的工具 |
| `call_tool_with_host(host, call)` | Host API 与 ToolCall | `Result<ToolResult>` | 默认转发到 `call_tool`；文件、进程、服务或 Runtime 工具覆盖此方法 |
| `before_tool(call)` | 任意候选工具调用 | `ToolDecision` | 可允许、阻止、取消、重写或请求审批 |
| `after_tool(result)` | 最终工具结果 | `()` | 只观察，不能修改已返回结果 |
| `on_event(event)` | Core 生命周期事件 | `()` | 适合指标或状态，不应假设 payload 固定 |
| `load_context(host, request)` | 本轮完整源上下文 | `Result<Option<LoadedContext>>` | `None` 透传；`Some` 完整替换；错误阻止模型请求 |
| `handle_service(host, call)` | Host 注入 caller 的服务调用 | `Result<Value>` | 仅处理已注册服务；payload 由服务版本定义 |
| `describe_ui()` | 无 | `Vec<UiDeclaration>` | 声明视图，不绘制内容 |
| `render_ui_with_host(host, request)` | Host API、尺寸、焦点、帧号 | `Option<UiFrame>` | `None` 表示不更新；不得返回 ANSI |
| `on_ui_input_with_host(host, input)` | Host API 与焦点视图输入 | `()` | 修改实例状态后由后续 render 返回新帧 |

不带 Host 的 `render_ui` 和 `on_ui_input` 是便捷入口；对应 `*_with_host` 默认转发给它们。插件应只实现一条路径，避免同一事件处理两次。

### 工具参数类型

`ToolCall` 的 `id` 必须原样带回 `ToolResult.call_id`；`name` 是 Host 路由后的公开名；`args` 是未强类型校验的 JSON。使用 `call.args_as::<Args>()` 解析，不要用字符串切割参数。

`ToolDecision`：

| 变体 | 字段 | Core 行为 |
| --- | --- | --- |
| `Allow` | 无 | 原样执行 |
| `Block` | `reason` | 生成模型可见拒绝结果，不执行工具 |
| `CancelRun` | `reason` | 优雅取消当前运行并保留 Session |
| `Rewrite` | 完整 `call` | 对最终调用重新执行策略检查后路由 |
| `RequireApproval` | `request_id`、`reason`、`poll_interval_ms` | 暂停并周期性重新询问策略 |

审批 `request_id` 必须稳定，`reason` 不应包含敏感参数。重写必须返回完整 ToolCall，不能只返回差量 args。

### 上下文类型

`ContextLoadRequest` 字段：

| 字段 | 含义 |
| --- | --- |
| `run_id` | 当前 Core 运行 ID |
| `step` | 从 0 开始的 ReAct 步数 |
| `provider` / `model` | 本轮实际模型路由 |
| `system` | 当前顶层 system 提示 |
| `messages` | 扩展提示与 Session 组成的 provider-neutral JSON 消息 |

`LoadedContext` 返回实际发送的 `system` 和完整 `messages`。它不是增量；压缩器必须自行保留需要的系统信息和工具调用配对。

### UI 类型

| 类型 | 主要字段 | 约束 |
| --- | --- | --- |
| `UiDeclaration` | `view_id`、title、placement、size、focusable | `plugin_id` 留空，由 Host 注入 |
| `UiRenderRequest` | plugin/view/instance ID、width、height、focused、frame | 尺寸是去除宿主边框后的内容区 |
| `UiFrame` | `view_id`、visible、lines | `None` 与 `visible = false` 含义不同 |
| `UiInput` | plugin/view/instance ID、event | 鼠标坐标相对内容区 |
| `UiNavigationRequest` | `request_id`、action | request ID 用于幂等去重 |

`UiNavigationAction::Push` 压入动态实例，`Replace` 只替换当前插件拥有的栈顶，`Pop` 返回父视图。插件通过 `host.navigate_view(request)` 请求导航，不直接操作应用导航栈。

### `PluginHostApi`

所有方法都绑定当前插件实例。Host 注入调用方身份并按 manifest 逐次鉴权。

#### Agent 贡献

| 方法 | 参数 | 返回/副作用 |
| --- | --- | --- |
| `upsert_tool(local_name, spec)` | 插件内稳定键与 ToolSpec | 返回模型可见公开名；后续工具快照变化 |
| `remove_tool(public_name)` | `upsert_tool` 返回的公开名 | 幂等删除当前插件工具 |
| `upsert_prompt(prompt)` | id、content、priority | 返回可信公开 ID；priority 越小越靠前 |
| `remove_prompt(id)` | 插件内部提示 ID | 幂等删除 |
| `emit_event(event)` | 名称、JSON data、可选展示提示 | Host 注入 `source.type=plugin` 与真实 ID |
| `navigate_view(request)` | 幂等导航请求 | 只能操作当前插件子视图 |

#### 实例状态与服务

| 方法 | 参数 | 返回/错误 |
| --- | --- | --- |
| `get_state(key)` | 实例内 key | 不存在返回 `None` |
| `set_state(key, value)` | key 与 JSON | 写入仅当前激活实例有效的内存 |
| `remove_state(key)` | key | 返回旧值，不存在返回 `None` |
| `upsert_service(service)` | name、SemVer、description | 注册或替换当前插件服务 |
| `remove_service(name)` | 当前插件内服务名 | 幂等删除 |
| `list_services(plugin_id)` | `None` 或目标插件 ID | 返回 Host 注入 owner 的目录 |
| `call_service(plugin_id, name, payload)` | 目标、服务名、协议 JSON | 返回服务 JSON；Host 注入真实 caller ID |

服务 owner 和 caller 不从 payload 读取。Guest 在 `handle_service` 收到的 `ServiceCall.caller_id` 才是可信身份。

#### 文件与进程

| 方法 | 参数 | 返回/错误 |
| --- | --- | --- |
| `read_file(path)` | manifest `fs_read` 范围内路径 | UTF-8 文本；越界、缺失或二进制失败 |
| `list_dir(path)` | 允许目录 | 稳定排序的一层 `FileEntry`，不递归 |
| `spawn_process(spec)` | command、args、env、cwd、stderr 策略 | 当前实例有效的 `u64` 句柄 |
| `write_process(handle, data)` | 句柄与原始文本 | 写入并 flush，不自动换行 |
| `read_process_line(handle, timeout_ms)` | 句柄与超时 | 一行、EOF `None`，超时返回错误 |
| `kill_process(handle)` | 句柄 | 终止并释放；未知句柄失败 |

`ProcessSpec.command` 不经过 shell；`args` 原样传递。Host 清空继承环境，只复制受控基础变量，再加入 spec.env。`process_exec` 是完整原生进程信任，不是 WASM 内的低权限能力。

#### Agent Runtime

| 方法 | 参数 | 返回/行为 |
| --- | --- | --- |
| `agent_identity()` | 无 | 当前 controller AgentId |
| `spawn_agent(request)` | 授权 profile 与首次输入 | 入队后返回 AgentHandle，不等待结束 |
| `continue_agent(request)` | 成功终态 target 与新增输入 | 返回新运行句柄，不暴露原 Session |
| `steer_agent(target, input)` | 后代 ID 与实时输入 | 注入排队或运行任务 |
| `agent_status(target)` | controller 或后代 ID | AgentSnapshot |
| `agent_result(target)` | controller 或后代 ID | 未终态返回 `None` |
| `agent_events(target, limit)` | 目标和单次上限 | 非阻塞读取回放与新增事件 |
| `cancel_agent(target)` | 后代 ID | 级联取消，返回是否产生新变化 |

Guest 不能提交模型、provider options、工具权限或 owner。`profile` 必须同时存在于 manifest 允许列表和应用注册表。

#### 受控模型完成

| 方法 | 参数 | 返回/行为 |
| --- | --- | --- |
| `complete_model(request)` | system、provider-neutral messages、可选 max_tokens | 返回模型文本和可选用量；Host 固定 provider/model/stream 并禁用工具与推理 |

该调用要求 manifest `model_completion = true` 和应用侧 `PluginHostServices::with_model_completion` 绑定。`ModelCompletionRequest` 拒绝未知字段，Guest 无法覆盖路由或 provider options。

### `export_plugin!`

宏必须在插件 crate 根调用一次。它负责：

1. 生成固定 WIT world 和 Component 导出。
2. 用 `Default` 延迟创建单例插件状态。
3. 把 Host imports 包装为 `PluginHostApi`。
4. 把 JSON 解析错误转换为对应 export 的稳定失败形式。

直接实现 WIT 时必须复现这些信封规则；详见 [WIT API 0.6](/reference/wit)。

## agent-plugin-host

### `PluginHost`

`PluginHost: AgentExtension`，所以 Host 可以直接挂到 `Agent::with_extension`。额外方法：

| 方法 | 参数 | 返回/语义 |
| --- | --- | --- |
| `id()` | 无 | 单插件稳定 ID；组合/空 Host 可为 None |
| `load_context(request)` | Core ContextLoadRequest | `None` 透传，Some 完整替换 |
| `ui_declarations()` | 无 | 当前 Host 视图声明 |
| `render_ui(request)` | 目标与尺寸 | 非 owner 返回 None |
| `on_ui_input(input)` | 焦点输入 | 非法路由返回错误 |
| `services()` | 无 | 当前 Host 公开服务 |
| `call_service(call)` | 带可信 caller/target 的调用 | 非 owner 返回 None |
| `shutdown()` | 无 | 执行 Guest deactivate 与资源清理 |

### `CompositePluginHost`

| API | 参数 | 行为 |
| --- | --- | --- |
| `new()` | 无 | 空组合 Host |
| `push(host)` | `Arc<dyn PluginHost>` | 按稳定加载顺序追加并失效路由缓存 |
| `set_capability_owner(capability, plugin)` | 能力 ID 与插件 ID | 记录独占能力 owner |
| `capability_owner(id)` | 能力 ID | 返回选中 owner |
| `hosts()` / `host_ids()` | 无 | 返回加载顺序快照 |
| `get(id)` | 插件 ID | 克隆目标 Host 引用 |
| `remove(id)` | 插件 ID | 移除但不自动 shutdown，调用方负责清理 |
| `clear()` | 无 | 返回全部 Host，不自动 shutdown |
| `tool_owner(name)` | 最近工具快照中的名称 | 返回 owner ID；快照未建立时可能为空 |

组合 Host 会缓存工具和 UI owner。修改子 Host 后必须通过公开管理 API 让缓存失效，不要在外部复制路由判断。

### Manifest API

`PluginManifest::load(path)` 读取 TOML 并立即 `validate()`。校验包括：身份字段非空、插件版本为 SemVer、`api_version` 等于当前支持值、WASM 路径非空、依赖不重复/不自依赖、能力声明合法以及未实现权限不被请求。

`CapabilitySection`：

| 字段 | 当前支持 |
| --- | --- |
| `agent` | 支持 spawn/observe/cancel 与 profile allowlist |
| `model_completion` | 支持，应用固定路由、工具策略和输出上限 |
| `process_exec` | 支持，属于完整原生进程信任 |
| `fs_read` | 支持，逐路径 canonicalize 后鉴权 |
| `http`、`secrets`、`fs_write` | 当前拒绝，不会静默忽略 |

`resolve_plugin_load_order(manifests)` 在实例化前检查必选依赖、SemVer 与循环。`resolve_plugin_capabilities(manifests, selections)` 解析 multi/exclusive owner；多个 exclusive provider 没有显式选择时失败。

`load_plugin_runtime_config(config_path)` 只读取应用 TOML 中的插件路径和 `capability_selection`。相对 manifest 路径以配置文件目录为基准。

### WASM 加载函数

| API | 额外参数 | 失败策略 |
| --- | --- | --- |
| `load_wasm_plugins(paths)` | 无 | 任一插件失败时 shutdown 已加载插件并整体返回错误 |
| `load_wasm_plugins_with_services(paths, services)` | Host 注入服务 | 同上 |
| `load_wasm_plugins_with_selection(paths, selection)` | 独占能力 owner 映射 | 规划或加载失败时整体失败 |
| `load_wasm_plugins_with_selection_and_services(...)` | owner + Host 服务 | 完整严格入口 |
| `load_wasm_plugins_resilient(paths)` | 无 | 保留无关成功插件，返回失败列表 |
| `load_wasm_plugins_resilient_with_selection(...)` | owner 映射 | 必选依赖失败向下游传播，可选依赖不阻塞 |
| `load_wasm_plugins_progressively_with_selection_and_services(...)` | LiveHost、回调 | 并发准备并按稳定计划发布 Ready/Failed 更新 |

严格加载适合服务启动必须全量成功的应用；容错加载适合 TUI；渐进加载适合需要首屏先出现、插件后台 Ready 的交互应用。

### `WasmPluginLimits`

| 字段 | 含义 |
| --- | --- |
| `fuel` | 单个 store 的总执行 fuel |
| `fuel_yield_interval` | 协作式异步 yield 的 fuel 间隔；None 关闭 |
| `max_memory_bytes` | 单个线性内存最大字节数 |

默认值适合常规插件。应用降低限制时应验证 activate、上下文压缩和 UI 渲染等最重路径；耗尽 fuel 或内存会作为插件调用错误传播。

## agent-plugin-manager

### 安装记录

`InstalledPlugin` 保存 ID、名称、插件版本、ABI 版本、启用状态、相对 manifest 路径、bundle SHA-256 和来源。`PluginLock` 保存 lock schema、插件列表和独占能力选择。它们描述已安装状态，不代表插件已经成功实例化。

`InstallOptions.enabled` 决定安装后是否进入运行时组合，默认 `true`。

### `PluginManager`

| API | 参数 | 返回/错误与副作用 |
| --- | --- | --- |
| `new(root)` | 管理根目录 | 只构造对象，不创建目录 |
| `root()` / `lock_path()` | 无 | 返回根目录或锁文件路径 |
| `install(bundle)` | 本地 bundle 根 | 校验、复制、hash、原子写锁；默认启用 |
| `install_with_options(bundle, options)` | bundle 与启用选项 | 失败时不修改现有锁和版本 |
| `list()` | 无 | 按 ID 返回锁记录；不做完整性校验 |
| `enable(id)` | 插件 ID | 校验依赖和能力后原子写锁 |
| `disable(id)` | 插件 ID | 被启用插件依赖时拒绝 |
| `select(capability, plugin)` | 独占能力和 owner | 启用目标并写 selection，失败不落盘 |
| `clear_selection(capability)` | 能力 ID | 返回原 owner；清除后产生冲突则拒绝 |
| `remove(id)` | 插件 ID | 先原子更新锁，再删除 bundle；被依赖时拒绝 |
| `doctor()` | 无 | 检查路径、manifest、WASM、hash、依赖和能力，返回报告 |
| `runtime_config()` | 无 | 先 doctor，再返回已启用 manifest 与 selection；不实例化插件 |

`list()` 适合 UI 列表，不能代替 `doctor()`。Host 启动前应使用 `runtime_config()`，避免加载被篡改或依赖无效的 bundle。

### GitHub API

`GithubPluginSource::parse(value)` 接受裸仓库名、`owner/repository` 或 `https://github.com/owner/repository`，拒绝其他 scheme、查询参数和额外路径。`repository_url()` 返回规范化展示 URL。

`GithubInstallOptions`：

| 字段 | 含义 |
| --- | --- |
| `enabled` | 安装后是否启用 |
| `tag` | 指定 Release；None 使用 latest |
| `asset` | 指定 ZIP；None 按稳定规则选择 |
| `expected_sha256` | Registry 提供的预期 hash，设置后必须匹配 |
| `max_download_bytes` | 下载上限，0 非法 |

`install_github(source, options)` 只下载预构建 Release bundle，不克隆、不编译、不执行插件。下载、checksum、解包、路径和大小全部校验后才进入原子安装；失败时锁文件不变。返回 `GithubInstallResult`，包含安装记录、实际 tag、asset 名和 checksum 是否验证。

### Registry API

`RegistryRequest::parse(input)` 解析 `name` 或 `name@semver`。版本要求使用 SemVer 语义；格式错误直接返回错误。

| API | 参数 | 返回/行为 |
| --- | --- | --- |
| `install_registry(request, enabled)` | RegistryRequest 与启用状态 | 求解依赖，先安装依赖再安装目标 |
| `search_registry(query)` | 名称/说明过滤词 | 当前 ABI 下可安装结果 |
| `outdated_registry()` | 无 | 已安装插件的兼容新版本 |
| `update_registry(name)` | 可选插件名 | Some 更新一个，None 更新全部兼容项 |

已安装依赖会作为固定约束参与求解；冲突不会静默替换。Registry schema、manifest ABI、插件版本和 lock schema 是四个独立版本维度。

### 诊断结果

`DoctorReport.checked_plugins` 是检查数量，`issues` 保存插件级或全局问题。`is_healthy()` 只在没有问题时返回 true。锁文件无法解析等全局错误直接作为 `Err` 返回，不进入 issues。

## 相关页面

- [插件开发](/development/plugin)
- [Manifest 与权限](/host/manifest-capabilities)
- [插件管理](/usage/plugin-management)
- [WIT API 0.6](/reference/wit)
