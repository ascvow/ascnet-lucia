# 插件能力矩阵

Code Context Engine 所需能力在当前插件系统（ABI `0.10.0`，wasmtime 46，`wasm32-wasip2`）中的实测状态。每一行的结论都指向仓库中的真实代码。

列含义：

- **Available**：该能力今天是否以任何形式可用
- **WASM Direct**：能否在 Guest 内直接完成，不经 Host 能力
- **Host Bridge**：能否通过现有 Host 能力完成
- **Missing**：是否需要新增 Host 能力或工具链

## 主矩阵

| Capability | Available | WASM Direct | Host Bridge | Missing | Notes |
| --- | --- | --- | --- | --- | --- |
| Repository read | 部分 | 否 | `host-fs-read` | **是** | `capability/mod.rs:609` 只返回 UTF-8；`:648` 按 manifest `fs_read` 前缀校验。**无法表达"当前工作区"**，`fs_read` 在安装时固化（见[缺口 G1](#g1-工作区读取)） |
| Repository write | 否 | 否 | 否 | 是 | `manifest.rs:1153` 加载期拒绝 `fs_write`。索引不写仓库，此项对 V1 非阻塞 |
| Recursive scanning | 是 | 否 | `host-fs-list` | 否 | `capability/mod.rs:618` 只列一层，返回 `{path, is_dir}`。递归 = N 次 JSON 往返，每次计入调用方 fuel |
| File metadata（size/mtime） | **否** | 否 | 否 | 是 | `capability/requests.rs:20` 的 `FileEntry` 只有 `path` 与 `is_dir`。增量索引无法做廉价 stat 判断（见[缺口 G4](#g4-变更检测)） |
| .gitignore access | 是 | 是 | `host-fs-read` | 否 | 作为普通 UTF-8 文件读取，匹配语义由插件实现。宿主不提供解析后的忽略规则 |
| Persistent storage | 是 | 否 | `host-store-*` | **是** | 语义正确（按插件按项目隔离、跨版本保留），**容量严重不足**：`state.rs:33` 上限 8 MiB / 8192 键（见[缺口 G2](#g2-索引持久化)） |
| Incremental storage write | **否** | 否 | 否 | 是 | `capability/mod.rs:428` 每次 `store_set` 克隆全量 map 并重写整个文件（`state.rs:179`，含两次 fsync）。写一个键 = O(全库) |
| SQLite | 否 | 否 | 否 | 是 | 需要 C 编译（工具链缺失，见[缺口 G6](#g6-c-工具链)）与文件写入/锁（`fs_write` 被拒）。`rusqlite` bundled 与 SQLite WASM 都不可行 |
| File watcher | 部分 | 否 | 事件总线 | 是 | 无 watcher 能力。**但 `core.tool.result` 覆盖 Agent 自身的全部写入**（`protocol/src/lib.rs:888`）。外部编辑无信号（见[缺口 G4](#g4-变更检测)） |
| HTTP requests | **否** | 否 | `process_exec` 间接 | 是 | `manifest.rs:1153` 加载期拒绝 `http`。WASI 最小化配置无 sockets（`wasm/mod.rs:505`）。唯一出网路径是子进程 |
| Threads | 否 | 否 | 否 | 是 | `engine.rs:52` 未调用 `wasm_threads`（默认关闭），linker 只加 wasi-p2 无 wasi-threads（`wasm/mod.rs:496`）。同插件全部导出调用串行（`wasm/mod.rs:277`） |
| WASM SIMD | 是 | 是 | — | 否 | wasmtime 46 默认开启且未被关闭。可用于向量点积与哈希 |
| Large memory | 否 | — | — | 是 | `engine.rs:18` 硬编码 64 MiB，`trap_on_grow_failure(true)`。全部 loader 使用 `WasmPluginLimits::default()`，应用层无提升路径 |
| memory64 | 否 | 否 | 否 | 是 | `engine.rs:52` 未开启，wasmtime 默认关闭。线性内存上限 4 GiB（实际 64 MiB） |
| mmap | 否 | 否 | 否 | 是 | WASI p2 无 mmap；Guest 只有线性内存 |
| Native libraries | 否 | 否 | `process_exec` 间接 | 是 | component 不支持动态链接原生库。原生代码只能作为子进程存在 |
| Model files（二进制读取） | **否** | 否 | 否 | 是 | `capability/mod.rs:613` 用 `read_to_string`，非 UTF-8 直接失败。**权重文件在 ABI 层面不可读**（见[缺口 G5](#g5-本地模型)） |
| Long-running tasks | 部分 | 是 | — | 是 | 单次导出调用预算 5000 万 fuel（`engine.rs:15`，每次调用重置）。超限即 trap。可跨多次调用分片，但会阻塞该插件其他导出 |
| Background work | **否** | 否 | 否 | 是 | WIT world 无 tick/timer；插件只在宿主调用导出时执行（`wit/plugin.wit:93-124`）。`api-capability-map.md:40` 列为 P0 未实现 |
| Cancellation | 否 | 否 | 否 | 是 | `wasm/mod.rs:277` 注释：Wasmtime 不允许丢弃已进入 component 的调用，调用方取消后 guest 仍跑到返回。只有 fuel 耗尽这一种终止方式 |
| Progress events | 是 | 否 | `host-agent-emit-event` | 否 | 可发布结构化扩展事件并携带 `EventPresentation`（`sdk/src/lib.rs:202`、`:274`），驱动 Footer 或事件列表 |
| Tool registration | 是 | — | `list-tools` / `host-agent-upsert-tool` | 否 | 静态与动态两条路径完备。Host 按 manifest `tool_scopes` 绑定权威 scope（`manifest.rs:1072`） |
| Configuration | 是 | — | 激活元数据 | 部分 | manifest `[metadata]` + 应用 `[plugins.<id>]` 配置合并后经 `activate` 下发（`wasm/mod.rs:618`）。无 schema、无类型校验；`api-capability-map.md:38` 列为 P0 |
| Secrets / credential storage | **否** | 否 | 否 | 是 | `manifest.rs:1153` 加载期拒绝 `secrets`。现有范式是密钥留在 Host、不进 Guest（`host-model-complete` 即此模式） |
| GPU access（Metal/CUDA/WebGPU） | 否 | 否 | `process_exec` 间接 | 是 | WASM 沙箱无任何 GPU 接口。只能通过子进程访问 |
| Model inference（受控） | 是 | 否 | `host-model-complete` | — | 已有能力，但是**文本补全**，不是 embedding。路由与密钥由 Host 固定（`services.rs:269`） |
| Plugin services | 是 | — | `host-service-*` | 否 | 版本化 JSON 服务 + manifest 依赖 ACL。可用于未来拆分 provider 插件 |
| Session read | 是 | 否 | `host-session-*` | 否 | 需 `sessions = true`。**会把全部工具 scope 抬到 `unknown`**，Code Context Engine 不应申请 |
| Sub-agent spawn | 是 | 否 | `host-agent-runtime-call` | 否 | 需 profile 授权。与本插件目标无关 |

## 六项专题判定

按需求文档 §48 要求逐项给出结论。

### Tree-sitter — `REQUIRES EXTENSION`（构建工具链）

阻塞点不在 Host，在构建环境：

```
$ ls /opt/wasi-sdk        → 不存在
$ clang -print-targets    → 注册目标中无 wasm/wasm32/wasm64
```

tree-sitter 运行时与全部官方 grammar 均为 C。`cc` crate 编译到 `wasm32-wasip2` 需要带 WASI sysroot 的 clang，本机 Apple clang 没有 wasm 后端。同时 `scripts/build-plugin.ts:20` 与 `scripts/check-offline.ts` 全部使用 `--offline`，新依赖需先联网 vendor。

**V1 判定：不引入 tree-sitter。** 用纯 Rust 结构化解析（Tier 2）覆盖目标语言，AST 精度（Tier 1）作为独立 ADR 在工具链问题解决后再评估。

### Lexical Index — `SUPPORTED`（计算），持久化受限

倒排索引、BM25、trigram 全是纯计算 + 内存，在 WASM 内完全可行，SIMD 可用。约束在两处：64 MiB 线性内存，以及 8 MiB 持久化上限。

**判定：内存中的词法检索 SUPPORTED；把它完整落盘 REQUIRES EXTENSION。** V1 采用"启动时重建 + 内存驻留"，落盘的只有轻量 manifest 与内容哈希。

### Vector Index — `REQUIRES EXTENSION`

算术量级判定：

| 规模 | 维度 | 量化 | 原始字节 | JSON 编码后 | 8 MiB 上限 |
| --- | --- | --- | --- | --- | --- |
| 20k chunk | 384 | int8 | 7.7 MB | ≈ 10.3 MB(base64) | 超限 |
| 20k chunk | 1024 | f32 | 82 MB | — | 超限，且超 64 MiB 内存 |
| 5k chunk | 256 | int8 | 1.3 MB | ≈ 1.7 MB | 勉强，但一个中等仓库远超 5k chunk |

叠加 `capability/mod.rs:428` 的全量重写语义：每增量更新一个 chunk 就要重新序列化并 fsync 整个向量库。

**判定：`REQUIRES EXTENSION`。** 在获得块级/流式存储能力前，语义索引不能持久化。

### API Embedding — `REQUIRES EXTENSION`

`http = true` 在 `manifest.rs:1153` 被加载期硬拒。今天技术上可行的唯一路径是 `process_exec` 启动 `curl`：

- 代价 1：把插件全部工具 scope 抬到 `Shell`（`manifest.rs:1100`），`code_search` 从此每次调用需审批。
- 代价 2：`docs/host/manifest-capabilities.md:197` 明确 `process_exec` 等价于授予操作系统用户的原生进程权限。
- 代价 3：API key 必须以命令行参数或环境变量交给子进程，即进入 Guest 可见范围——违反本仓库"密钥不进 Guest"的既有范式。

**判定：`REQUIRES EXTENSION`。** 正确形态是新增通用 `http.request()` 能力，`api-capability-map.md:39` 已把它列为 P0 并给出了推荐契约。

### Local Qwen Embedding — `NOT RECOMMENDED`（纯 WASM）

五条独立的阻断，任一条都足以否决：

1. **权重文件读不出来。** `capability/mod.rs:613` 用 `read_to_string`，非 UTF-8 直接失败。
2. **内存差两个数量级。** Qwen3-Embedding-0.6B 即使 int8 量化也在 600 MB 量级，上限 64 MiB。
3. **无线程。** 单线程推理。
4. **fuel 预算不匹配。** 单次调用 5000 万指令，一次 transformer 前向远超此数。
5. **无 GPU。**

**判定：`NOT RECOMMENDED` 作为纯 WASM 实现；未来形态应为 Host 通用 `embedding.embed()` 能力。** 详见 [ADR：本地嵌入](./adr-local-embedding.md)。

### File Watching — `SUPPORTED WITH HOST`（部分）

| 变更来源 | 现有信号 | 状态 |
| --- | --- | --- |
| Agent 自身写入/编辑 | 订阅 `core.tool.result` | 可用，无需新能力 |
| 用户在编辑器中编辑 | 无 | 缺失 |
| `git checkout` / 外部脚本 | 无 | 缺失 |

需求文档 §29 要求优先复用宿主已有事件而不是新建 watcher——`core.tool.result` 正是这个复用点，且它覆盖了 Coding Agent 场景下绝大多数的文件变动。

**判定：`SUPPORTED WITH HOST`。** 外部变更在 V1 用惰性校验兜底（检索命中的文件在返回前重读并比对哈希），不新建 watcher。

## 缺口清单

### G1 工作区读取

**阻塞 V1。** manifest `fs_read` 在安装时固化，无法表达"当前工作区根"。`crates/agent-tui/src/plugin/startup.rs:368` 的 `workspace_root` 激活元数据只硬编码给 `permission` 与 `teammate`，且拿到路径也无法通过 `resolve_read_path` 校验。

规避手段：项目本地安装 + `fs_read = ["../../.."]`。可用于开发，不是产品形态。

### G2 索引持久化

**阻塞持久索引。** 8 MiB / 8192 键 + 全量重写 + 双 fsync。

### G3 后台任务

**阻塞非阻塞式索引构建。** 插件只在导出被调用时执行，`activate` 内做全量索引会 fuel trap。

### G4 变更检测

`FileEntry` 无 size/mtime，外部编辑无事件。增量判断只能靠读全文算哈希。

### G5 本地模型

`read_file` 只返回 UTF-8 + 64 MiB 内存 + 无线程 + 无 GPU。

### G6 C 工具链

无 wasi-sdk，Apple clang 无 wasm 后端，构建流水线 `--offline`。

## 与既有路线图的对照

`docs/plugin/api-capability-map.md:35` 的"后续优先项"表已经独立地列出了本次审计发现的同一批缺口：

| 缺口 | 路线图条目 | 优先级 |
| --- | --- | --- |
| G2 索引持久化 | 持久插件 KV（namespace + revision/CAS） | P0 |
| G4 API 凭证 | 配置与 Secret 引用 | P0 |
| API Embedding | HTTP client | P0 |
| G3 后台任务 | 后台任务（start/status/result/cancel handle） | P0 |
| G1 工作区写入侧 | 文件写入（Host 分配的目录） | P1 |

**这不是巧合，而是一个强信号：Code Context Engine 需要的全部 Host 扩展都落在项目已经认可的通用能力清单内，没有一项是 ACE 专属 API。** 提案应沿用这些既有条目的推荐契约，而不是另立一套。
