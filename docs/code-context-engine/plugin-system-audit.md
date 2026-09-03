# 插件系统审计

本文记录 Code Context Engine 立项前对 ascnet-lucia 现有 WASM 插件系统的实测审计。所有结论都指向仓库中的真实代码位置，不使用通用 WASM/WASI 常识推断。

审计基线：`wit/plugin.wit` ABI `0.10.0`，wasmtime `46`，target `wasm32-wasip2`。

## 1. 运行时与进程模型

### Engine 配置

`crates/agent-plugins/host/src/wasm/engine.rs:52` 的 `create_wasm_engine` 只设置三项：

```rust
config.wasm_component_model(true);
config.consume_fuel(true);
config.cache(Some(cache));   // 仅在应用注入缓存目录时
```

没有调用 `wasm_threads`、`wasm_memory64`、`wasm_simd` 或 `wasm_relaxed_simd`，因此这些特性全部取 wasmtime 46 的默认值。查证 `wasmtime-46.0.1/src/config.rs:935` 与 `:1178`：`threads` 与 `memory64` 需要显式开启，默认关闭；`simd` 默认随 target 特性开启。Linker 只注入 `wasmtime_wasi::p2::add_to_linker_async`（`wasm/mod.rs:496`），没有 `wasi-threads`，因此即使开启 threads 提案，guest 也拿不到线程创建接口。

**结论：单线程、32 位线性内存、SIMD 可用、无共享内存、无 wasi-threads。**

### 每插件资源上限

`wasm/engine.rs:15`：

```rust
const DEFAULT_FUEL_PER_PLUGIN: u64 = 50_000_000;
const DEFAULT_FUEL_YIELD_INTERVAL: u64 = 250_000;
const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
```

内存上限通过 `StoreLimitsBuilder::memory_size` + `trap_on_grow_failure(true)` 施加（`wasm/mod.rs:510`），超限直接 trap 而不是返回分配失败。全部 loader 入口都使用 `WasmPluginLimits::default()`（`wasm/loader.rs:95`、`:580`、`:655`），应用层没有任何提高单插件上限的配置路径。

fuel 在**每一次导出调用前重置**为 50,000,000（`wasm/fuel.rs:12` 的 `refill_fuel`，被 `call_tool`、`on_event`、`render_ui`、`handle_service`、`activate`、`deactivate` 全部调用）。唯一例外是 `load-context`，它按输入字节额外授予 fuel（`fuel.rs:17`，512 fuel/字节，硬上限 500,000,000）。

`fuel_async_yield_interval(250_000)` 表示 guest 每消耗 25 万 fuel 就让出 tokio 执行器，因此长计算不会阻塞宿主事件循环，但**仍然消耗同一份 5000 万预算，用尽即 trap**。

**结论：单次导出调用的计算预算是 5000 万条 wasm 指令量级，内存 64 MiB。这是"每次调用"而不是"每次会话"的预算。**

### 实例并发模型

`wasm/mod.rs:215` 中 `WasmPluginHost.state: Arc<Mutex<WasmPluginState>>`，`wasm/mod.rs:277` 的 `complete_component_call` 在进入 component 前先取独占锁，并把调用交给独立 tokio 任务跑完。

**结论：同一插件的全部导出调用（工具、事件、UI 渲染、服务）严格串行。** 一次长时间的 `call_tool` 会阻塞该插件的 UI 渲染与事件投递，但不影响其他插件。函数注释明确说明 Wasmtime 不允许丢弃已进入 component 的异步调用，因此**没有调用级取消**：调用方放弃等待后，guest 仍会跑到导出返回。

### 长任务与后台工作

WIT world（`wit/plugin.wit:93-124`）导出只有 `activate`、`deactivate`、`handle-service`、`list-tools`、`call-tool`、`on-event`、`load-context`、`describe-ui`、`render-ui`、`on-ui-input`。

**没有 tick、timer、background job 或任何宿主主动周期性回调。** 插件只在宿主调用某个导出时才拥有执行权。可用的"伪心跳"只有三条：订阅 `core.turn.start` 一类事件、注册 UI 视图后由 TUI 渲染循环驱动 `render-ui`、以及模型主动调用工具。

`docs/plugin/api-capability-map.md:40` 已经把"后台任务（start/status/result/cancel handle）"列为 P0 未实现能力。

## 2. 能力与权限模型

### manifest 声明的能力全集

`crates/agent-plugins/host/src/manifest.rs:936` 的 `CapabilitySection`：`agent`、`model_completion`、`surface_actions`/`surface_action_allowlist`、`process_exec`、`http`、`secrets`、`storage`、`sessions`、`fs_read`、`fs_write`、`tool_scopes`。

`manifest.rs:1153` 在加载阶段硬拒绝三项：

```rust
if self.http || self.secrets || !self.fs_write.is_empty() {
    return Err(anyhow!("当前 Lucia 版本尚未实现插件 HTTP、secret 或文件写入能力"));
}
```

`docs/host/manifest-capabilities.md:96` 的权限表与之一致。**插件今天没有任何 HTTP、密钥存储或文件写入路径。**

### 工具 scope 抬升

`manifest.rs:1072` 的 `bind_tool_scope` 与 `:1092` 的 `component_capability_scope_floor` 决定了一个重要副作用：manifest 每多授予一类资源能力，该插件**全部公开工具**的权限下限就被抬高一档；多类资源合并为 `Unknown`。具体映射：`process_exec` → `Shell`，`fs_read` → `Read`，`sessions`/`secrets`/`model_completion`/`agent`/`surface_action` → `Unknown`。

对 Code Context Engine 的直接后果：只要同时声明 `fs_read` 和 `storage` 之外的任何一类资源，`code_search` 就会落到 `Unknown`，从而在权限系统里变成"每次调用都需要审批"的工具——这与"减少 Agent 交互成本"的产品目标直接冲突。**能力越界的代价是产品级的，不只是安全审查级的。**

### 权限审查与冻结

ADR-0002（`docs/development/adr/0002-frozen-plugin-environment.md`）规定每个 Session 启动时绑定一份插件环境快照（插件集合、版本、Bundle Digest、Manifest Digest、Capability Profile Digest、加载顺序、Capability Owner），人工写操作只影响后续 Session。`docs/host/manifest-capabilities.md:43` 进一步说明依赖与能力变化会改变安装身份并要求用户重新审查。

**后果：索引所需的能力集合必须在设计阶段一次定死。** 后续为了"补一个能力"而修改 manifest，等价于让每个用户重新走一次授权审查。

## 3. 文件系统能力

### 读取接口

`crates/agent-plugins/host/src/capability/mod.rs:609`：

```rust
pub(crate) fn read_file(&mut self, request_json: &str) -> Result<Value> {
    let path = self.resolve_read_path(&request.path)?;
    let content = std::fs::read_to_string(&path)?;   // UTF-8 only
    Ok(json!(content))
}
```

`:618` 的 `list_dir` 只列**一层**，返回结构是 `FileEntry { path, is_dir }`（`capability/requests.rs:20`）——**没有文件大小、没有 mtime、没有 inode**。

三条硬约束：

1. **只能读 UTF-8 文本。** 二进制文件（模型权重、已有索引文件、图片）在 ABI 层面无法读取，`read_to_string` 会直接失败。
2. **递归扫描必须由插件自己发起 N 次 `list_dir`**，每次都是一趟 WASM↔Host JSON 往返。
3. **没有 stat。** 增量索引无法用 mtime/size 做廉价的"未变更"判断，只能读全文再算内容哈希——代价与全量重建同阶。

请求/响应经 JSON 信封，单次请求体上限 8 MiB（`capability/mod.rs:69`）。文件内容作为 JSON 字符串返回，需要转义，大文件的编解码成本会计入调用方 fuel。

### 路径授权

`capability/mod.rs:648` 的 `resolve_read_path` 把请求路径与 manifest `fs_read` 每一项都用 `resolve_from(&self.plugin_dir, ...)` + `canonicalize()` 解析后比对前缀。`capability/requests.rs:234` 的 `resolve_from` 对绝对路径原样返回。

manifest 校验（`manifest.rs:590` 的 `validate`）**不检查 `fs_read` 条目的内容**：绝对路径、`..` 都被接受，最终由用户在启用审查界面上判断。

**关键缺口：没有任何方式表达"当前工作区根目录"。** `fs_read` 在安装时固化。全局安装的官方插件无法随项目切换读取范围。`crates/agent-tui/src/plugin/startup.rs:368` 确实注入了 `workspace_root` 激活元数据，但：

```rust
activation_metadata.entry("permission".to_string()).or_default().extend([...
    ("workspace_root".to_string(), workspace_root.clone()),
]);
activation_metadata.entry("teammate".to_string()).or_default()
    .insert("workspace_root".to_string(), workspace_root);
```

只硬编码给 `permission` 和 `teammate` 两个插件 ID，且即使拿到路径字符串，`fs_read` 仍会拒绝读取。**这是 V1 唯一的必需 Host 改动。**

唯一可用的规避手段是项目本地安装：把 manifest 放进仓库内（如 `.lucia/plugins/ace/plugin.toml`）并声明 `fs_read = ["../../.."]`。这可行但不是产品形态。

### .gitignore

无专门能力。插件可以把 `.gitignore` 当作普通 UTF-8 文件读取并自行实现匹配语义。宿主不提供解析后的忽略规则。

## 4. 存储能力

这是本次审计中约束最强的一项。

### 两套语义

`crates/agent-plugins/host/src/state.rs:1` 的模块文档区分：

- `state`（`host-state-*`）：命名空间混入 Component 字节摘要（`wasm/mod.rs:97` 的 `derive_plugin_state_identity`），**插件重新构建即清空**。
- `store`（`host-store-*`，需 `storage = true`）：命名空间只绑定插件 ID 与作用域，跨版本保留。作用域为 `User`（跨项目）或 `Workspace`（跟随项目，需应用注入 `workspace_scope`）。

索引属于用户资产，必须用 `store`。

### 容量上限

`state.rs:28`：

```rust
const MAX_PLUGIN_STATE_BYTES: usize = 1024 * 1024;      // state：1 MiB
const MAX_PLUGIN_STORE_BYTES:  usize = 8 * 1024 * 1024;  // store：8 MiB
const MAX_PLUGIN_STATE_ENTRIES: usize = 1024;
const MAX_PLUGIN_STORE_ENTRIES: usize = 8192;
```

**8 MiB 是单个作用域下全部键的编码总量**，且用的是 `serde_json::to_vec_pretty`（`state.rs:193`），带缩进，实际有效载荷更低。

### 写入成本

`capability/mod.rs:428` 的 `store_set`：

```rust
let mut values = slot.values.clone();   // 克隆全量 map
values.insert(request.key, request.value);
slot.store.save(&values)?;              // 序列化全量 + 落盘
slot.values = values;
```

`state.rs:179` 的 `save` 每次执行：全量 `to_vec_pretty` → 写临时文件 → `sync_all` → `rename` → 目录 `sync_all`。

**即写入一个键的成本是 O(整个存储)，并且包含两次 fsync。** 没有增量写、没有 CAS、没有 revision。`docs/plugin/api-capability-map.md:37` 把"持久插件 KV（namespace + revision/CAS）"列为 P0 未完成项，说明当前实现是过渡形态。

### 既有插件的实际用法

knowledge-plugin 是仓库中最接近的先例——它对历史会话建词法索引并持久化。它的做法（`examples/plugins/knowledge-plugin/src/lib.rs:31`、`:46`、`:178`）是：整个索引存进**一个键** `entries`，并硬性 `MAX_ENTRIES: usize = 2_000` 后 `truncate`。

**这就是当前存储层能承载的索引规模量级：数千条粗粒度条目，不是数万个代码 chunk，更不是向量。**

## 5. 网络能力

**不存在。** WIT world 中没有任何 HTTP 或 socket import。`manifest.rs:1153` 在加载期拒绝 `http = true`。WASI 配置（`wasm/mod.rs:505`）刻意最小化：

```rust
let mut wasi = WasiCtxBuilder::new();
// 刻意保持最小 WASI：不继承环境变量、不预打开目录、不继承 stdio。
wasi.arg(&manifest.plugin.id);
```

没有 `inherit_network`、没有 `socket_addr_check`，因此 WASI p2 的 `wasi:sockets` 也不可用。

唯一的出网路径是 `process_exec`：插件启动一个原生子进程（`capability/process.rs:55`），由该进程发起网络请求。这是 mcp-plugin 采用的模式（`examples/plugins/mcp-plugin/plugin.toml` 声明 `process_exec = true`）。

`docs/plugin/api-capability-map.md:39` 把 "HTTP client（Host 执行的结构化请求，域名 allowlist、响应上限、超时与重定向策略）"列为 P0。

## 6. 子进程能力

`capability/process.rs` 提供 `spawn_process` / `write_process` / `read_process_line` / `kill_process`。特征：

- 不经 shell，`command` 与 `args` 分开传递（`process.rs:74`）。
- `env_clear()` 后只保留 PATH、HOME、TMPDIR、LANG、LC_ALL，再叠加请求环境（`requests.rs:243`）。
- Unix 下建立独立进程组，Drop 时 `SIGKILL` 整组（`process.rs:32`、`:48`）。
- 单插件进程数上限 `min(8 或策略值, 16)`（`process.rs:59`、`capability/mod.rs:51`）。
- stdin 单次写入 4 MiB，stdout 单行 4 MiB，读超时上限 120 秒（`capability/mod.rs:53`）。
- 受 `ExecutionPolicy` 二次约束：只有 `Serve` profile 且 `allow_process` 才放行（`process.rs:182`）。`crates/agent-core/tool/src/policy.rs:342` 与 `:374` 显示 Evaluation/Mutation 平面把 `max_processes` 设为 0。

**`process_exec` 是当前唯一可以突破 WASM 沙箱的一等公民能力，但代价明确：`docs/host/manifest-capabilities.md:197` 写明它"等价于授予插件当前操作系统用户的原生进程权限"，并把插件全部工具 scope 抬到 `Shell`。**

**只支持逐行 stdio。** 无法传输二进制帧（嵌入向量必须编成文本），单行 4 MiB 是一个真实的批量上限。

## 7. 密钥

**不存在。** `secrets = true` 在加载期被拒（`manifest.rs:1153`）。

现状的替代路径是"密钥不进 Guest"：`crates/agent-tui/src/plugin/startup.rs:346` 的注释说明，插件配置在 Host 侧解析，模型路由由宿主消费并注册成专属 provider，其余键值作为激活元数据下发。`host-model-complete`（`wit/plugin.wit:87`）正是这个模式的成品——插件只提交 system 与 messages，provider、model、API key 全部留在 Host。

**这是本仓库已确立的密钥处理范式，任何新的凭证类能力都应复制它，而不是把 key 交给 Guest。**

## 8. 生命周期

`docs/plugin/lifecycle.md:5` 的调用顺序：

```
instantiate component -> list-tools -> activate(context) -> describe-ui
  -> prompt/list/call/event/ui hooks -> deactivate()
```

TUI 使用渐进加载（`wasm/loader.rs:684`），最多 3 个并发实例化，`activate` + `describe-ui` 完成即原子发布为 Ready。Agent 在 `RunStarted` 时冻结当时 Ready 的工具集。

关键事实：

- **`activate` 是唯一的初始化窗口，且它消耗一份 5000 万 fuel 预算。** 在这里做全量索引会直接 trap。
- `activate` 返回非空字符串即阻止插件加载（`wasm/mod.rs:630`），没有"部分可用"状态。
- `deactivate` 在应用调用 `shutdown` 时触发（`wasm/mod.rs:694`），随后撤销服务注册与 Agent Runtime principal。**但加载 future 被取消或进程异常退出时不保证执行**——`CapabilityState::drop`（`capability/mod.rs:215`）只兜底杀子进程，不做状态落盘。
- **没有 repository open/close/switch 事件。** 工作区在进程启动时确定（`crates/agent-tui/src/session/mod.rs:49` 读 `current_dir`），切项目等于重启进程。这实际上简化了索引隔离问题。

## 9. 事件总线

`crates/agent-plugins/protocol/src/lib.rs:888` 列出核心事件：`core.run.start`、`core.run.finish`、`core.turn.start`、`core.turn.finish`、`core.model.request`、`core.model.response`、`core.tool.call`、`core.tool.result`、`core.message.inject`、`core.agent.event`。TUI 另有 `tui.` 前缀事件（knowledge-plugin 订阅 `tui.run.finish`）。

订阅在 manifest `[[events]]` 中声明，未声明的事件不投递——订阅本身就是投递权限边界（`docs/plugin/lifecycle.md:51`）。优先级 `lowest`…`highest`，外加只读的 `monitor`。

**没有任何文件变更事件。** 但 `core.tool.result` 覆盖了 Agent 自己执行的每一次工具调用，包括写文件与编辑文件的原生工具。这是"Agent 改了哪些文件"的现成信号源，无需新建 watcher。外部编辑（用户在编辑器里改、`git checkout`）没有信号。

`on-event` 可以返回 `Pending{retry_after_ms}` 请求延后重投，宿主按 50–1000 ms 夹取，累计等待上限 600 秒（`wasm/mod.rs:222`、`:777`）。这是插件能获得的最接近"定时器"的原语，但它绑定在一次事件分发上，会挂住该次分发。

## 10. 工具注册

两条路径，二者都要经过 manifest `[capabilities.tool_scopes]`：

- 静态：导出 `list-tools` 返回 `ToolSpec` JSON 数组，在 `activate` 之前调用（`wasm/mod.rs:576`）。
- 动态：`host-agent-upsert-tool` / `host-agent-remove-tool`，返回公开工具名。

Host 在调用 component 前把公开工具名替换回注册时的本地 ID（`wasm/mod.rs:808`），因此插件内部命名与模型可见命名解耦。

`code_search` 走标准静态注册即可，不需要任何新机制。

## 11. 构建流水线

- `rust-toolchain.toml` 固定 `targets = ["wasm32-wasip2"]`。
- `scripts/build-plugin.ts:20` 执行 `cargo build --offline --release --target wasm32-wasip2`，产物从根 `target/` 回拷到插件本地 `target/`。
- `scripts/check-offline.ts` 定义的全部校验步骤都带 `--offline`。

实测本机环境：

```
$ ls /opt/wasi-sdk            → 不存在
$ clang -print-targets        → 注册目标中没有 wasm / wasm32 / wasm64
$ ls ~/.cargo/registry/src/*/ | grep tree-sitter   → 无
```

**后果：当前工具链无法把任何 C 代码编译到 `wasm32-wasip2`。** Apple clang 没有 wasm 后端，机器上也没有 wasi-sdk 提供 sysroot。tree-sitter 核心与全部官方 grammar 都是 C，`cc` crate 在此环境下必然失败。同时 `--offline` 意味着新依赖必须先联网 vendor 进本地 registry。

这不是 Host 限制，是构建工具链限制，但它同样是"今天做不到"的硬事实。

## 12. 测试基础设施

`docs/plugin/testing.md` 定义三层：插件纯逻辑单元测试、`cargo check --target wasm32-wasip2` 验证 component 编译、独立 smoke-test crate 做端到端。仓库中 mcp/teammate/agent-runtime/workflow/permission 五个插件都带 `smoke-tests` 子 crate，已在根 workspace members 中。

`docs/plugin/testing.md:39` 明确要求关键官方插件必须在构建 component 后运行独立 smoke test，`cargo check` 不能替代。

Code Context Engine 应直接复用这一形态，不需要新的测试机制。

## 13. 审计结论汇总

现有插件系统**已经具备**的、Code Context Engine 可以直接使用的机制：

- 工具注册与 owner 路由（`code_search` 的注册路径完备）
- 事件订阅（`core.tool.result` 提供 Agent 侧文件改动信号）
- 按插件按项目隔离的持久存储（语义正确，容量不足）
- manifest 能力声明 + 用户审查 + 安装身份绑定
- 插件服务与依赖 ACL（未来拆分 provider 插件的现成手段）
- 三层测试与离线构建流水线

**缺失且必须解决**的：

1. 无法读取当前工作区（`fs_read` 在安装时固化，无 workspace 占位符）——阻塞 V1。
2. 存储容量与写入模型无法承载代码索引——阻塞任何持久索引。
3. 无 HTTP——阻塞 API Embedding。
4. 无二进制文件读取、64 MiB 内存、无线程——阻塞 Local Embedding。
5. 无后台任务——阻塞不阻塞用户的全量索引构建。
6. 无文件系统 watcher——外部编辑无法感知，只能靠轮询或惰性校验。

逐项判定与建议见[能力矩阵](./plugin-capability-matrix.md)，架构决策见[架构](./architecture.md)。
