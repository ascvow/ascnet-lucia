# Code Context Engine Development Progress

本文件是 Code Context Engine 开发的 **Single Source of Truth**。任何新 Session、接手的 Agent
或人工开发者，只需阅读 `docs/code-context-engine/` 与本文件即可确定开发进度与下一步动作。

更新日期：2026-09-03

## Current Phase

Phase 2 — Repository Discovery

Status: IN_PROGRESS

## Overall Status

- [x] Phase 0 — Architecture Revalidation
- [x] Phase 1 — Plugin Skeleton
- [ ] Phase 2 — Repository Discovery
- [ ] Phase 3 — Structural Parsing
- [ ] Phase 4 — Structural Index
- [ ] Phase 5 — Lexical & Symbol Retrieval
- [ ] Phase 6 — Embedding Abstraction
- [ ] Phase 7 — API Embedding Provider — **BLOCKED**（Host 无 HTTP 能力）
- [ ] Phase 8 — Semantic Vector Index — **BLOCKED**（依赖 Phase 7 与 blob 存储）
- [ ] Phase 9 — Hybrid Retrieval
- [ ] Phase 10 — Context Builder
- [ ] Phase 11 — Incremental Indexing
- [ ] Phase 12 — Local Embedding Provider — **BLOCKED**（Host 无 embedding 能力）
- [ ] Phase 13 — Agent Tool Integration
- [ ] Phase 14 — Retrieval Benchmark
- [ ] Phase 15 — Hardening

**实际执行顺序**（跳过 BLOCKED，保持依赖正确）：

```
2 → 3 → 4 → 5 → 6 → [7 BLOCKED] → [8 BLOCKED] → 9 → 10 → 11 → [12 BLOCKED] → 13 → 14 → 15
```

Phase 9 的融合层从一开始就按「任意子集」设计，因此语义分支缺席不阻塞它。Phase 13 交付的
`code_search` 在语义分支缺席时进入 `Degraded`，这正是架构第 13 问要求的降级形态。

## Phase 编号对照

开发任务书使用 Phase 0–15；仓库既有的 `implementation-plan.md` 使用 Phase 1–12。两套编号
指向同一份架构，对照如下。**本文件的 Phase 编号一律指任务书编号。**

| 任务书 Phase | implementation-plan Phase | 说明 |
| --- | --- | --- |
| 0 Architecture Revalidation | 门禁 | 复核，无对应实现阶段 |
| 1 Plugin Skeleton | 1 插件骨架与索引生命周期 | 一一对应 |
| 2 Repository Discovery | 2 仓库扫描器 | 一一对应 |
| 3 Structural Parsing | 3 结构化解析与分块 | 一一对应 |
| 4 Structural Index | 3 的产物持久化部分 | plan 未单列；本文件按任务书单列 |
| 5 Lexical & Symbol Retrieval | 4 符号与词法检索 | 一一对应 |
| 6 Embedding Abstraction | 8 Embedding Provider 接入 | 骨架已完成大部分 |
| 7 API Embedding Provider | 9 API Provider | 均依赖 Host HTTP 能力 |
| 8 Semantic Vector Index | 10 语义向量索引 | 均依赖 blob 存储 |
| 9 Hybrid Retrieval | 5 混合检索（+ 11 语义分支） | V1 只有两路 |
| 10 Context Builder | 5 上下文打包 | 同阶段的后半段 |
| 11 Incremental Indexing | 6 增量索引 | 一一对应 |
| 12 Local Embedding Provider | 12 Local Provider | 均依赖 Host embedding 能力 |
| 13 Agent Tool Integration | 5 尾部工具实现 | 骨架已注册工具，此处接入真实结果 |
| 14 Retrieval Benchmark | 7 Benchmark | 一一对应 |
| 15 Hardening | 7 加固 | 一一对应 |

## Completed Phases

### Phase 0 — Architecture Revalidation

Status: COMPLETE
Completed Date: 2026-09-03

Summary：复核既有设计文档与仓库实际实现的一致性。全部已有文档已读取，Plugin / Host 边界、
存储决策、API 与 Local 嵌入路径的结论在当前代码上重新验证，**全部仍然成立**。

Documents read:
- `plugin-system-audit.md`、`plugin-capability-matrix.md`
- `architecture.md`、`implementation-plan.md`
- `adr-storage.md`、`adr-api-embedding.md`、`adr-local-embedding.md`
- `implementation-status.md`、`codex-phase-1-handoff.md`

Revalidation evidence（逐条对照当前代码，非引用文档）:

| 结论 | 验证方式 | 结果 |
| --- | --- | --- |
| Host 在加载期拒绝 `http = true` | `crates/agent-plugins/host/src/manifest.rs:1189` | 成立 |
| WIT 中无 HTTP / embedding import | `rg 'host-http\|host-embedding' wit/plugin.wit` 无命中 | 成立 |
| `host-fs-read` 只返回 UTF-8 | `capability/mod.rs:619` 仍为 `read_to_string` | 成立 |
| `FileEntry` 无 size / mtime | `sdk/src/lib.rs:390` 只有 `path`、`is_dir` | 成立 |
| `workspace_read` 能力已落地 | `plugin.toml` 声明生效，smoke test 实际加载通过 | 成立 |
| 存储只有 JSON KV | `IndexStore` 仅用 `store_*`，无 blob 接口 | 成立 |

Important decisions:
- 存储决策（分层：manifest + 哈希表落 KV，索引本体内存驻留）仍然有效，不更换后端。
- API 与 Local 嵌入路径的判定不变：二者都是「目标形态清晰但 Host 能力未实现」，
  **不使用 `process_exec` + curl / 子进程变通**。理由见两份 ADR，核心是 scope floor 会把
  `code_search` 抬到 `Shell`，产品目标崩塌。
- Phase 顺序合理，但任务书的线性顺序需按上表跳过三个 BLOCKED 阶段执行。

Known limitations:
- 任务书假定 Phase 7 / 8 / 12 可直接开发，实际被 Host 能力阻塞。已在下方 Blocked Phases 记录。

Next: Phase 2 — Repository Discovery

---

### Phase 1 — Plugin Skeleton

Status: COMPLETE
Completed Date: 2026-09-03

Summary：建立可真实加载运行的 Code Context WASM 插件，含生命周期、仓库身份、索引 manifest
持久化、配置、存储边界与工具契约。**没有任何伪造检索结果**——`code_search` 在索引就绪前
一律返回明确的不可用状态。

Implemented:
- 独立 Guest crate `code-context-plugin`（`crate-type = ["cdylib", "rlib"]`），已入根 workspace。
- `plugin.toml`：`api_version = "0.10.0"`、`capabilities = { workspace_read, storage }`、
  `tool_scopes.code_search = "read"`。未申请的能力逐项写明否决理由。
- 完整 `IndexStatus` 生命周期（8 态），未简化为布尔。
- `IndexManifest` 读写、schema 校验与三级迁移判定（compatible / semantic rebuild / full rebuild）。
- `RepositoryIdentity` 由宿主注入的可信工作区根派生，使用摘要不落用户绝对路径。
- `EmbeddingSpace` 与稳定 `embedding_space_id`（含长度前缀，避免变长字段拼接歧义）。
- `EmbeddingProvider` trait 与 `UnconfiguredEmbeddingProvider`（返回错误，不返回伪造向量）。
- 动态注册 `code_search(query, max_results?, token_budget?, path_scope?)`；卸载时用 Host 返回的
  公开名注销。
- 配置解析（`enabled`、`embedding_mode = local | api`）与严格校验。

Key files:
- `examples/plugins/code-context-plugin/src/lib.rs` — 插件生命周期与工具注册
- `.../src/model.rs` — 领域模型（不含任何存储后端细节）
- `.../src/storage.rs` — `IndexStore`、`ManifestLoad`
- `.../src/embedding.rs` — `EmbeddingProvider` 边界
- `.../src/tool.rs` — `code_search` schema 与不可用结果
- `.../src/config.rs`、`.../src/error.rs`、`.../src/testing.rs`
- `.../plugin.toml`、`.../smoke-tests/tests/code_context.rs`

Tests:
- 单元测试 55 个 + 1 个文档测试，`cargo test --offline -p code-context-plugin` 全通过。
- Smoke test 7 个（真实 WASM Host 加载）：工具注册、`not_indexed` 报告、空查询拒绝、
  manifest 跨重载持久化、工作区隔离、无工作区作用域降级、无 workspace_root 降级。

Build Result:`bun run test:plugin:code-context` 全链通过（单测 → WASM release 构建 → smoke test）。

Important decisions:
1. **动态工具注册而非静态 `list_tools`**：静态工具在 `activate` 之前收集，那时读不到配置与
   工作区能力状态，无法决定是否暴露工具。
2. **`deactivate` 不是保存点**：加载被取消时不保证调用，因此 manifest 在产生时即写入。
3. **manifest 兼容也暂报 `NotIndexed`**：结构索引产物尚未实现，报 `Ready` 会让 `code_search`
   去搜一个空索引。Phase 4 落地后改用 `compatibility.to_status()`（`lib.rs:139` 有标注）。
4. **宿主缺项目作用域时不降级到 User 作用域**：那会让 A 项目索引污染 B 项目，宁可不持久化。

Known limitations:
- `code_search` 只返回真实状态，不返回检索结果（符合骨架阶段定义，非伪造完成）。
- Host 无 repository open/change/close 事件；进程启动映射为 attach，`deactivate` 映射为 detach。
- Guest 无后台任务与取消 handle，骨架阶段不启动内部任务。
- Host JSON KV 只适合 manifest 与小型 metadata。

Next: Phase 2 — Repository Discovery

## Blocked Phases

三个阶段因 Host 能力缺失无法实现。按开发任务书 §25，记录 Blocker 而**不绕开架构做 hack 实现**。

### Phase 7 — API Embedding Provider

Status: BLOCKED

Blocker：Host 未实现插件 HTTP 能力，且在插件加载期主动拒绝该声明。

Evidence:
- `crates/agent-plugins/host/src/manifest.rs:1189` — `if self.http || self.secrets || !self.fs_write.is_empty()`
  直接返回错误「当前 Lucia 版本尚未实现插件 HTTP、secret 或文件写入能力」。声明 `http = true`
  的插件根本无法启动。
- `wit/plugin.wit` 中不存在任何 HTTP import。
- `crates/agent-plugins/host/src/wasm/mod.rs` 的 WASI ctx 刻意最小化，无 `inherit_network`、
  无 `socket_addr_check`，`wasi:sockets` 不可用。

Required capability：通用 `host-http-request`，manifest 形如 `[capabilities.http] allow_hosts = [...]`，
凭证由 Host 注入（复制 `host-model-complete` 范式，密钥不进 Guest）。scope floor 为 `Network`
——`manifest.rs` 已为 `http` 预留该映射，说明是「已规划未实现」而非「被否决」。

Possible solutions:
1. 实现通用 Host HTTP 能力。属于独立的 Host 能力提案，不在本插件交付范围。
2. `process_exec` + `curl`。**已否决**：会把 `code_search` 的 scope 从 `read` 抬到 `shell`，
   每次调用都要用户审批，与「减少 Agent 交互与 token 成本」的产品目标直接冲突；且 API key
   必须进入 Guest 线性内存，违反本仓库既有范式。详见 `adr-api-embedding.md`。
3. 把 embedding 拆成独立 provider 插件，通过 `host-service-call` 供本插件调用，让网络权限留在
   provider 插件的 manifest 上。仍然依赖方案 1 先落地。

Recommended solution：方案 1。在该 Host 能力落地前，产品显示 `API Mode: 需要宿主 HTTP 能力`，
不提供任何变通实现。

### Phase 8 — Semantic Vector Index

Status: BLOCKED

Blocker：双重依赖 —— 无可用 embedding provider（Phase 7 / 12 均被阻塞），且现有存储装不下向量。

Evidence:
- `crates/agent-plugins/host/src/wasm/state.rs` 存储上限 8 MiB / 8192 键；
  `capability/mod.rs` 每次写入克隆全量 map 并重写整个文件（`to_vec_pretty` + 两次 fsync）。
  数十 MB 的向量在这条通道上完全不可行。
- 无 blob 接口：`IndexStore` 只能用 `store_get/set/remove/list`（JSON KV）。

Required capability：`host-store-blob-read` / `host-store-blob-write`（块级二进制接口），
建议默认配额 256 MiB。该能力不含 ACE 语义，记忆插件与知识插件同样受益
——knowledge-plugin 的 `MAX_ENTRIES = 2_000` 截断就是被同一限制逼出来的。

Possible solutions:
1. 实现 blob 存储能力 + 解除 Phase 7 或 12 的阻塞。
2. 向量存进 JSON KV。**否决**：8 MiB 上限下只能存约数千条 768 维向量的截断版本，且每次写入
   重写全量，中等仓库即不可用。
3. 向量写入用户仓库。**否决**：违反「索引不写用户仓库」的既有约定。

Recommended solution：方案 1，且必须在 Phase 7 或 12 至少一者解除阻塞之后。

### Phase 12 — Local Embedding Provider

Status: BLOCKED

Blocker：Host 无本地推理能力，且 WASM 沙箱在 ABI 层面无法承载嵌入模型。

Evidence（五条独立阻断，任一条足以否决纯 WASM 方案）:
1. **权重读不出来**：`capability/mod.rs:619` 的 `host-fs-read` 是 `read_to_string`，只返回 UTF-8。
   GGUF / safetensors / ONNX 全是二进制，UTF-8 校验必然失败。**插件没有任何途径把权重读进
   线性内存**——这一条单独就终结了讨论。
2. **内存差两个数量级**：`wasm/engine.rs` 上限 64 MiB 且 `trap_on_grow_failure(true)`；
   0.6B 模型 int8 量化仍在 600 MB 量级。
3. **无线程**：wasmtime 46 默认关闭 threads，linker 只注入 `wasmtime_wasi::p2`，无 wasi-threads。
4. **fuel 预算不匹配**：每插件 5000 万 fuel 且每次导出调用前重置；一次 transformer 前向远超此量级。
   分片规避不成立——推理无法在导出调用之间保存并恢复中间状态。
5. **无 GPU**：沙箱内没有 Metal / CUDA / DirectML / WebGPU 的任何接口。

另：`host-model-complete` 是**文本补全**不是 embedding，其实现绑定 `ModelGateway` 返回文本，
没有任何返回向量的路径。

Required capability：通用 `host-embedding-embed`。Guest 提交文本批次与 `input_type`；
Host 固定 provider、model、后端、权重路径、批大小、维度与归一化策略。与 `host-model-complete` 同构。

Possible solutions:
1. 实现通用 Host embedding 能力。属于独立 Host 能力提案。
2. `process_exec` 拉起 llama.cpp / Python 服务。**已否决**：scope 抬到 `Shell`；等同授予操作系统
   级进程权限；stdio 单行 4 MiB 压死批量吞吐；且形态上就是需求文档明令禁止的 native daemon。
   更关键的是它会让「源码留在本机」的产品承诺由一个生命周期不受 Host 管理的子进程兑现，
   比不提供该模式更糟。
3. 纯 WASM 内推理。**否决**，见上述五条。

Recommended solution：方案 1。在此之前产品显示 `Local Mode: NOT SUPPORTED`，附说明
「当前插件运行时不提供本地推理能力」。**不提供任何变通实现。**

关于 PoC：`adr-local-embedding.md` 判定当前无法执行——PoC 第一步就是把权重读进插件，而这一步
在 ABI 层面走不通。PoC 应在 Host embedding 能力提案阶段进行，测量对象是 Host 侧的原生推理运行时。

## Active Work

Phase 2 — Repository Discovery。目标是稳定输出 `IndexableFile[]`（path、language、hash、metadata）。

## Known Issues

无。Phase 1 全部验证通过，未发现 regression。

## Architecture Drift

### 2026-09-03 · Phase 1 骨架阶段（已同步至 architecture.md）

1. `code_search` 使用动态注册而非静态 `list-tools`。原因：静态工具收集发生在 `activate` 之前，
   无法依据 `enabled` 配置与工作区能力状态决定是否暴露工具。
2. Host 没有 repository open/change/close 事件。骨架把进程启动映射为 attach，`deactivate`
   映射为 detach；真实 change 事件与 watcher 仍属后续能力。
3. Manifest 没有配置 schema 字段。`enabled` 与 `embedding_mode` 通过激活元数据下发，由 Guest
   严格校验，不是新增的假想 manifest 格式。
4. Guest 没有可持有的取消令牌或后台任务句柄。骨架不启动内部任务；后续长任务必须等通用后台
   任务能力，或采用明确的 fuel 分片协议。

### 2026-09-03 · Phase 0 复核

未发现新增 drift。三份 ADR 的判定在当前代码上逐条重新验证，全部成立（证据见 Phase 0 条目）。

## Deferred Work

以下属于 V1 明确不做，非阻塞项，不得 scope creep：

- Full LSP（Go to Definition / Find References / Types / Call Hierarchy）。架构已为未来接入预留：
  `CodeChunk` 保留 `qualified_symbol`、`symbol_kind`、`parent_symbol` 与精确行区间，
  LSP 将来作为**独立插件**通过 `host-service-call` 提供。
- Code Graph、Git history 语义索引、跨仓库 / 团队 / 分布式索引、云端托管索引。
- LLM query rewrite、LLM reranker。
- Tier 1 完整 AST（tree-sitter）：本机无 wasi-sdk，`clang` 注册目标中无 wasm 后端，构建流水线
  使用 `--offline`。V1 用 Tier 2 纯 Rust 解析 + Tier 3 文本兜底。作为后续独立 ADR。
- 文件 watcher：V1 用 `core.tool.result` + 惰性校验兜底。若将来实现，必须是宿主统一的仓库变更
  事件，不是 ACE 专属 watcher。
- Host 侧 Recommended 改动（blob 存储、HTTP、后台任务 handle、`FileEntry` 加 size/mtime、
  按插件资源上限可配置）：属于独立 Host 能力提案，不在本插件交付范围。

## Next Action

开始 Phase 2 — Repository Discovery：

1. 新建 `examples/plugins/code-context-plugin/src/repository/`（有真实实现时才创建，不预建空模块）。
2. 基于 `host-fs-list` 的有界递归遍历：深度上限、条目数上限、稳定顺序、路径去重、错误隔离。
3. `.gitignore` 解析与匹配（作为普通 UTF-8 文件读取），叠加内置默认忽略。
4. 语言检测（按路径与扩展名），未知语言进入文本兜底。
5. 内容哈希计算。注意 `FileEntry` 无 size/mtime，无法先用元数据排除未变更文件。
6. 敏感文件过滤（`.env`、`*.pem`、`*.key`、`credential*`、`secret*`）——API 模式会把源码发往
   外部 provider，该过滤是隐私前提而非可选项。
7. fuel 分片游标机制：单次调用消耗到阈值即保存游标返回，下次继续。**这是全局机制，后续所有
   重活都复用它**，接口需在本阶段冻结。
