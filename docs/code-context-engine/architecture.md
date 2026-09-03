# Code Context Engine 架构

本文在[插件系统审计](./plugin-system-audit.md)与[能力矩阵](./plugin-capability-matrix.md)的结论上确定架构。核心问题不是"怎么把 ACE 做出来"，而是"在现有 WASM 插件系统中，ACE 各组件最合理应该运行在哪里"。

## 总判定

```
WASM + HOST CAPABILITIES
```

不是 Pure WASM：插件读取当前工作区仍必须经过 Host 的可信能力边界。也不是"需要大改 Host"——
**唯一必需的通用 Host 改动已经落地，不含任何 ACE 语义。**

```
Required Host Changes: 1 项（工作区只读能力，已实现）
Plugin business logic remains inside the WASM component.
```

分模式结论：

```
API Mode:   REQUIRES EXTENSION   （需要通用 http.request()；不接受 process_exec 变通）
Local Mode: NOT CURRENTLY SUPPORTED（需要通用 embedding.embed()；V1 不交付）
```

## 分层：什么跑在哪里

```
┌─────────────────────────────────────────────────────────┐
│  Code Context Engine Plugin (WASM, wasm32-wasip2)       │
│                                                          │
│  Repository Scanner      ── 目录遍历、ignore 规则          │
│  Structural Parser       ── 纯 Rust，按语言 adapter        │
│  Chunk Store             ── 内存驻留                       │
│  Symbol Index            ── 内存驻留                       │
│  Lexical Index (BM25)    ── 内存驻留                       │
│  Semantic Index          ── V2，依赖存储扩展                │
│  Hybrid Retriever (RRF)  ── 纯计算                        │
│  Context Builder         ── 纯计算                        │
│  Index Manifest          ── 落 host-store（KB 级）         │
│                                                          │
│  EmbeddingProvider (trait)                               │
│       ├── Api    ── V2，依赖 http 能力                     │
│       └── Local  ── V3，依赖 embedding 能力                │
└──────────┬───────────────────────────────────────────────┘
           │ 现有 ABI / Host 能力         │ 后续需新增
           ▼                             ▼
  host-fs-read / host-fs-list      workspace 只读能力（Required）
  host-store-*                     http.request()（API Mode）
  workspace_read                   embedding.embed()（Local Mode）
  host-agent-upsert-tool
  on-event(core.tool.result)
  host-agent-emit-event
```

**全部检索业务逻辑留在插件内。Host 只提供通用原语。** 这条边界不做妥协：Host 不会知道什么是 chunk、什么是 embedding space、什么是 RRF。

## 13 个必答问题

### 1. 哪些代码运行在 WASM Plugin？

扫描、解析、分块、符号索引、词法索引、混合检索、重排、上下文打包、增量 diff、索引 manifest 管理、`code_search` 工具实现、降级策略。**即 ACE 的全部业务逻辑。**

### 2. 哪些能力由 Host 提供？

只有四类通用原语：

| 能力 | 现状 | 用途 |
| --- | --- | --- |
| 受控只读文件（工作区范围） | **已实现** | 扫描与读取源码 |
| 私有持久存储 | 已有（容量待扩） | 索引 manifest 与内容哈希 |
| 工具注册 + 事件订阅 | 已有 | 注册 `code_search`、感知文件改动 |
| 结构化 HTTP 请求 | 需新增（V2） | API embedding |
| 受控嵌入推理 | 需新增（V3） | Local embedding |

### 3. 是否需要修改 Host？

**是，但只有一项是必需的。**

| 级别 | 改动 | 阻塞什么 |
| --- | --- | --- |
| **Required（已实现）** | 工作区只读能力 | V1 运行门禁 |
| Recommended | 索引 blob 存储 | 每次启动全量重建 |
| Recommended | `http.request()` | API Mode |
| Recommended | 后台任务 handle | 首次索引阻塞用户 |
| Recommended | `FileEntry` 增加 size/mtime | 增量检测退化为全文哈希 |
| Recommended | 按插件资源上限可配置 | 大仓库触碰 64 MiB / fuel 上限 |
| Optional | 文件变更事件 | 外部编辑感知 |
| Optional | `embedding.embed()` | Local Mode |

完整提案见[必需的 Host 改动](#必需的-host-改动)。

### 4. 如果需要，增加哪些 generic capability？

见下文。全部遵守一条规则：**新增的是通用 Plugin Capability，不是 ACE-specific API。** 好的形状是 `embedding.embed()`、`filesystem.watch()`、`http.request()`；坏的形状是 `ace_index_repository()`、`ace_search_code()`。

### 5. Local Embedding 怎么运行？

```
Local Embedding Architecture: C. WASM + proposed generic Host Capability
```

不是 A（纯 WASM）——五条独立阻断，见[能力矩阵](./plugin-capability-matrix.md#local-qwen-embedding--not-recommended纯-wasm)。不是 B——现有 Host 没有任何推理能力（`host-model-complete` 是文本补全，不是 embedding）。不是 D——目标形态是清晰且通用的，只是尚未实现。

**V1 与 V2 都不交付 Local Mode。产品必须明确显示 `Local Mode: NOT SUPPORTED`，而不是提供一个降级到子进程的"能跑"版本。** 理由见 [ADR：本地嵌入](./adr-local-embedding.md)。

### 6. API Embedding 怎么运行？

```
API Architecture: C. proposed Host HTTP capability
```

不是 A（WASI 网络）——`wasm/mod.rs:505` 的 WASI 配置刻意最小化，无 sockets。不是 B——`manifest.rs:1153` 在加载期拒绝 `http = true`。不是 D——这是 `api-capability-map.md:39` 已列为 P0 的能力，契约形状已有共识。

`process_exec` + `curl` 在技术上今天就能跑通，但被明确否决：它把 `code_search` 的权限 scope 抬到 `Shell`（每次调用需审批，产品目标崩塌），并且要求 API key 进入 Guest（违反本仓库既有的密钥范式）。理由见 [ADR：API 嵌入](./adr-api-embedding.md)。

### 7. Index 保存在哪里？

分三类，按体量分流：

| 数据 | 位置 | 体量 | 依据 |
| --- | --- | --- | --- |
| Index Manifest（schema 版本、仓库身份、embedding space、时间戳） | `host-store` Workspace 作用域 | < 1 KB | 现有能力足够 |
| 文件内容哈希表（路径 → hash） | 同上 | 2000 文件 ≈ 160 KB | 现有能力足够 |
| Chunk / 符号 / 倒排索引 | **V1 内存驻留，启动时重建**；V2 迁入 blob 存储 | MB 级 | 8 MiB JSON 上限装不下 |
| Embedding 向量 | V2 blob 存储 | 数十 MB | 现有存储完全不可行 |

**索引逻辑上属于插件私有数据，不写用户仓库。** 这符合 `docs/host/manifest-capabilities.md:108` 的既有约定：路径由 Host 按插件 ID 与作用域派生，插件不能指定磁盘路径。`Workspace` 作用域天然实现了按项目隔离。

宿主未注入项目作用域时（`capability/mod.rs:411`），插件应降级为不持久化，而不是把项目索引写进 `User` 作用域——那会让 A 项目的索引污染 B 项目。

### 8. File change 如何传递给插件？

**复用宿主已有事件，不新建 watcher。**

```
Agent 写文件 → core.tool.result → on-event → 标记 dirty → 下次 code_search 前增量重建
```

订阅 `core.tool.result`（`protocol/src/lib.rs:888`），从工具名与参数中识别写入类调用，把受影响路径加入脏集合。这覆盖了 Coding Agent 场景下绝大多数变更，且零新增能力。

外部编辑（编辑器、`git checkout`）没有信号。V1 用**惰性校验**兜底：检索命中的文件在构造返回结果前重读并比对内容哈希，不一致就即时重解析该文件并修正结果。代价是把校验开销限制在命中集合（通常十几个文件）而不是整个仓库。

`filesystem.watch()` 是 Optional 级改动；即使将来实现，也应是宿主统一的仓库变更事件，而不是 ACE 专属 watcher。

### 9. Plugin 如何注册 code_search？

使用现有动态工具注册：`list-tools` 在 `activate` 之前执行，那时尚未获得配置与工作区能力状态；
插件在 `activate` 完成能力探测和 manifest 恢复后调用 `host-agent-upsert-tool`。Host 返回的公开工具名
被保存在运行时状态中，并在 `deactivate` 时通过 `host-agent-remove-tool` 注销。Manifest 仍声明权威
scope，没有新增 Plugin abstraction。

```toml
[capabilities.tool_scopes]
code_search = "read"
```

Agent 可见的接口刻意保持最小：

```
code_search(query, max_results?, token_budget?, path_scope?)
```

**Agent 看不到 `vector_search`、`bm25_search`、`rrf_search` 或任何检索内部结构。** 混合策略、权重、降级都是插件内部决策。

`read` scope 的前提是插件不申请任何会抬高 scope floor 的能力（`manifest.rs:1092`）。这直接约束了架构：**申请 `process_exec` 会让 scope 变成 `Shell`，申请 `sessions` 会变成 `Unknown`，都会毁掉工具的可用性。** 这是"能力最小化"在本项目中的产品级理由，不只是安全理由。

### 10. 插件 disable/uninstall/update 后 index 怎么处理？

| 事件 | 索引处理 | 依据 |
| --- | --- | --- |
| `deactivate` | 注销动态工具、尽力 flush 未保存 metadata、释放仓库上下文 | `wasm/mod.rs:694`，且加载 future 被取消时不保证执行 `deactivate`（`capability/mod.rs:215` 只兜底杀进程） |
| 插件 disable | `store` 数据保留不动 | `manager.rs:524` 的 `disable` 不触碰存储 |
| 插件 update | 按 `schema_version` 决定迁移级别 | `store` 命名空间刻意不含 Component 字节（`state.rs:92`），升级后数据仍在 |
| 插件 uninstall | 由 Plugin Manager 负责；插件侧不做自清理 | 插件在卸载后没有执行机会 |
| 应用重启 | 读 manifest → 校验 schema/仓库身份 → 增量扫描 | — |

**`deactivate` 不能是保存点。** 因为它在异常路径上不保证被调用。所有需要跨会话保留的状态必须在产生时即写入 `store`。

三级迁移（对应需求 §32）：

```
compatible        → 直接使用
semantic rebuild  → 结构索引保留，仅重建语义索引
full rebuild      → 全部丢弃重建
```

由 `IndexManifest.schema_version` 与 `embedding_space_id` 共同决定。

### 11. 多 Repository 如何隔离？

**当前进程模型已经天然隔离，无需额外设计。**

工作区在进程启动时由 `std::env::current_dir()` 确定（`crates/agent-tui/src/session/mod.rs:49`），没有 repository open/switch 事件，切换项目等于重启进程。持久存储的 `Workspace` 作用域由 Host 用工作区身份派生命名空间（`capability/mod.rs:682`），插件无法指定也无法枚举其他作用域。

仓库身份稳定性：`session/mod.rs:114` 的 `workspace_project_id` 对 cwd 做摘要。**同一仓库通过不同路径（符号链接、不同挂载点）打开会得到不同身份，索引会重建。** 这是可接受的行为——重建代价有限，而错误复用会导致路径失效。插件另在 `IndexManifest` 中记录自己观察到的 `repository_identity` 做二次校验。

### 12. unsupported language 如何降级？

三层，任何一层失败都不能让整个索引失败：

```
Tier 1  完整 AST            —— V1 不提供（工具链缺失，见 ADR）
Tier 2  纯 Rust 结构化解析   —— 按 LanguageAdapter 提取符号、签名、doc、边界
Tier 3  文本分块            —— 按空行/缩进/固定窗口，文件级元数据
```

V1 的 `LanguageAdapter` 覆盖 Rust、TypeScript、JavaScript、Python、Go。未覆盖语言直接落 Tier 3，仍然参与词法与符号名检索。

**单文件解析失败必须被捕获并降级为 Tier 3，绝不向上传播。** 一个语法错误的文件不能让整个仓库无法检索。

### 13. semantic provider failure 如何降级？

```
Normal    符号 + 词法 + 语义 → RRF 融合
Degraded  符号 + 词法        → RRF 融合（语义分支缺席）
```

API 离线、密钥失效、本地模型不可用时，`code_search` **继续正常返回结果**，只在结果元数据中标注 `mode: degraded`。语义分支永远是可选增强项，不是必要条件。

这条约束反向决定了检索架构：融合层必须能在任意子集上工作，因此 RRF（只需要各路排名，不需要可比的分数）是正确选择，而不是加权分数融合。

## 必需的 Host 改动

### Required：工作区只读能力（已实现）

**问题。** manifest `fs_read` 在安装时固化（`capability/mod.rs:648`），无法表达"当前工作区根"。全局安装的插件读不到用户当前项目。这不是 ACE 独有的问题——任何仓库感知型插件（linter、依赖分析、约定检查）都会撞上它。

**实现。** 新增独立 capability，由 Host 用应用注入的可信工作区根解析，Guest 不能指定路径：

```toml
[capabilities]
workspace_read = true
```

语义：

- 授权范围是当前工作区根及其后代，**边界由 Host 计算**，Guest 提交的路径按工作区根解析并做前缀校验，与现有 `resolve_read_path` 同一套逻辑。
- 复用现有 `host-fs-read` / `host-fs-list`，不新增 WIT 函数。Host 在解析时把工作区根加入 allowlist。
- scope floor 记为 `Read`，与 `fs_read` 一致。
- 宿主未注入工作区身份时该能力失败，而不是退化为读插件目录——与 `storage` 的 `Workspace` 作用域保持同一口径（`capability/mod.rs:411`）。
- 授权审查文案应明确："该插件可读取当前项目的全部文件。"

**为什么不是给 `fs_read` 加 `${workspace_root}` 占位符。** 占位符会让 manifest 里出现需要展开的模板语法，用户在审查界面上看到的不再是最终授权范围；而独立布尔能力在审查时的语义是一句话就能说清的。

TUI 已把原先按插件 ID 白名单注入的 `workspace_root` 元数据推广到全部已解析插件配置，并通过
`PluginHostServices::with_workspace_root` 单独注入不可被 Guest 覆盖的可信能力根。

## Architecture Drift

2026-09-03 的骨架实现确认了三处与初始文档不同的事实，并已同步到本文：

1. `code_search` 使用现有动态注册，而非静态 `list-tools`。原因是静态工具收集发生在
   `activate` 之前，无法依据 `enabled` 配置与工作区能力状态决定是否暴露工具。
2. Host 当前没有 repository open/change/close 事件。骨架把进程启动映射为 repository attach，
   把 `deactivate` 映射为 detach；真实 change 事件与 watcher 仍属于后续能力。
3. Manifest 当前没有配置 schema 字段。`enabled` 与 `embedding_mode` 继续通过激活元数据下发，
   由 Guest 做严格类型和值校验；这不是新增的假想 manifest 格式。

Guest 侧也没有可持有的取消令牌或后台任务句柄。骨架阶段不启动内部任务，因此 shutdown 只需要
注销工具、flush metadata 与释放状态；后续长任务必须等通用后台任务能力或采用明确的 fuel 分片协议。

### Recommended：索引 blob 存储

**问题。** `state.rs:33` 的 8 MiB / 8192 键，加上 `capability/mod.rs:428` 的全量重写 + 双 fsync。`api-capability-map.md:37` 已把它列为 P0。

**提案。** 在 `store` 之上增加块级二进制接口，保持 JSON 用于信封、字节用于载荷：

```wit
/// 读取插件私有存储中的一个二进制块。
import host-store-blob-read: func(request-json: string) -> list<u8>;
/// 写入插件私有存储中的一个二进制块。
import host-store-blob-write: func(request-json: string, bytes: list<u8>) -> string;
```

约束：沿用现有作用域与命名空间派生；按插件配额（建议默认 256 MiB，可由应用策略收紧）；单块上限独立于总配额；写入仍原子。

**这项能力不含任何 ACE 语义**——记忆插件、知识插件、任何需要存缓存或模型产物的插件都直接受益。knowledge-plugin 当前 `MAX_ENTRIES = 2_000` 的截断（`examples/plugins/knowledge-plugin/src/lib.rs:46`）就是被同一个限制逼出来的。

### Recommended：HTTP 能力

沿用 `api-capability-map.md:39` 的推荐契约：Host 执行的结构化请求，域名 allowlist、响应上限、超时与重定向策略。manifest 形如：

```toml
[capabilities.http]
allow_hosts = ["api.openai.com"]
```

**关键设计点：凭证不进 Guest。** 复制 `host-model-complete` 的范式（`services.rs:269`）——插件提交请求体，Host 按应用配置注入 Authorization 头。这样 API key 只存在于 Host 侧，与本仓库既有做法一致。

### Recommended：后台任务 handle

沿用 `api-capability-map.md:40`：`start / status / result / cancel`，卸载时撤销。首次全量索引应该在后台跑，而不是阻塞用户的第一次 `code_search`，也不是塞进 `activate` 的 5000 万 fuel 预算里。

### Recommended：`FileEntry` 增加元数据

`capability/requests.rs:20` 加两个字段：

```rust
pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
    pub(crate) size_bytes: u64,        // 新增
    pub(crate) modified_unix_ms: u64,  // 新增
}
```

改动极小，收益极大：增量索引可以先用 (size, mtime) 排除绝大多数未变更文件，只对候选集读全文算哈希。当前实现下这个优化根本无法表达。

### Recommended：按插件资源上限可配置

全部 loader 硬编码 `WasmPluginLimits::default()`（`wasm/loader.rs:95`）。建议允许应用按插件 ID 覆盖 fuel 与 `max_memory_bytes`，上限仍由 Host 硬顶约束。大仓库的内存驻留索引会逼近 64 MiB。

### Optional：文件变更事件

统一的宿主仓库变更事件（`core.fs.change` 或等价物）。**若实现，必须是宿主级通用事件，不是 ACE watcher。** 在此之前用 `core.tool.result` + 惰性校验。

### Optional：`embedding.embed()`

Local Mode 的前置条件。契约形状：Guest 提交文本批次，Host 返回向量；模型、后端（Metal/CUDA/CPU）、权重路径、批大小全部由应用配置，Guest 不可指定。与 `host-model-complete` 同构。

**Host 不知道这些向量是给代码检索用的。** 记忆插件、知识插件、任何检索型插件共用同一能力。

## 面向未来的 LSP 边界

V1 不做 LSP 集成，但结构必须允许后续接入：

```
Context Engine → candidate symbols → LSP enrichment → definition / reference / type
```

因此 `CodeChunk` 保留 `qualified_symbol`、`symbol_kind`、`parent_symbol` 与精确的 `start_line`/`end_line`——这些正是 LSP 定位所需的输入。LSP 本身将来应作为**独立插件**通过版本化服务提供（`host-service-call`），Code Context Engine 声明可选依赖并在其缺席时正常工作。不把 LSP 塞进本插件。

## 数据模型

```
CodeChunk {
    id, repository_id,
    file_path, language,
    symbol_name, qualified_symbol, symbol_kind, parent_symbol,
    start_line, end_line,
    signature, doc_comment, content, imports,
    content_hash
}

EmbeddingSpace {
    provider_type, provider_id, model, model_revision,
    dimension, normalization,
    query_template_version, document_template_version
}
→ embedding_space_id（摘要）

IndexManifest {
    schema_version, plugin_version,
    repository_identity,
    structural_index_version,
    embedding_space_id,
    created_at, updated_at
}
```

**Embedding 与结构化 chunk 严格分离。** 用户从 API 切到 Local 时，只有 `embedding_space_id` 不兼容，重建语义索引即可，结构索引原样保留。

## 隐私

API Mode 会把**源码片段**发送到用户配置的 embedding provider。插件配置界面与文档必须明确写出这一点，不能表述为"只有查询会离开你的机器"。

Local Mode 的承诺是源码不出本机——因此它更不能用"子进程调 curl"这类实现绕开，那会让承诺失真。

## 性能预算

目标是普通开发机，不假定 32 GB 内存或独立 GPU。

- API Mode 是最低硬件压力路径，应作为默认语义选项。
- Local Mode 若需要更高配置，产品必须能检测并提示，而不是静默变慢。
- V1 的纯词法 + 符号路径在任何机器上都可用，且不依赖任何外部服务。
