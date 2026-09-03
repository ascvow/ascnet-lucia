# 实施计划

本计划建立在[架构](./architecture.md)的判定之上。阶段顺序按 Plugin Audit 的结论重排过：原计划把 EmbeddingProvider 与语义索引放在中段，现在推迟到 Host 能力落地之后，前段全部是零新增能力就能交付的部分。

## 门禁：已完成的 Host 改动

**唯一必需项：工作区只读能力，已于 2026-09-03 完成。**

```toml
[capabilities]
workspace_read = true
```

实际实现范围：

1. `crates/agent-plugins/host/src/manifest.rs` — `CapabilitySection` 增加 `workspace_read: bool`；`validate_supported` 不再拒绝；`component_capability_scope_floor` 映射为 `Read`。
2. `crates/agent-plugins/host/src/capability/mod.rs` — `CapabilityHostContext` 增加可信 `workspace_root`；`resolve_read_path` 在 `workspace_read` 开启时把工作区根加入 allowlist，沿用现有 canonicalize + 前缀校验。
3. `crates/agent-tui/src/plugin/startup.rs` — 注入 `workspace_root` 到 `PluginHostServices`；同时把 `:368` 硬编码给 `permission`/`teammate` 的激活元数据推广为对全部插件注入。

配套：Host 单元测试（无能力时拒绝、越界路径拒绝、宿主未注入工作区时失败而非退化）、启用审查文案。

该改动保持通用能力语义，没有把 Code Context Engine 业务逻辑移入 Host。

## 阶段划分

### Phase 1 — 插件骨架与索引生命周期

**状态：完成。** 真实实现与验证结果见 [implementation-status.md](./implementation-status.md)。

- crate `code-context-plugin`，`crate-type = ["cdylib", "rlib"]`，加入根 workspace members。
- `plugin.toml`：`api_version = "0.10.0"`，`capabilities = { workspace_read = true, storage = true }`，`tool_scopes.code_search = "read"`。
- `activate` 只列出仓库根一次用于能力探测，然后读取 `IndexManifest`；**不扫描文件、不做任何解析**（fuel 预算约束，见 [ADR：存储](./adr-storage.md)）。
- `IndexManifest` 的读写、schema 版本校验、三级迁移判定（compatible / semantic rebuild / full rebuild）。
- `smoke-tests` 子 crate，验证真实 Host 加载 + 工具注册 + 存储往返。

产出门禁：`cargo check --target wasm32-wasip2` 通过，smoke test 通过。

### Phase 2 — 仓库扫描器

- 基于 `host-fs-list` 的递归遍历，深度与条目数上限。
- `.gitignore` 解析与匹配（作为普通 UTF-8 文件读取），叠加内置默认忽略（`target/`、`node_modules/`、`.git/`、二进制扩展名）。
- 文件筛选：扩展名白名单、大小上限（读之前无法知道大小，因此靠读取失败与长度截断兜底）。
- 内容哈希计算与与上一轮的 diff（added / modified / deleted / unchanged）。
- **fuel 分片机制**：扫描按批次推进，单次导出调用内消耗到阈值即保存游标返回，下次调用继续。这是全局机制，后续所有重活都复用它。

### Phase 3 — 结构化解析与分块

- `LanguageAdapter` trait，Tier 2 纯 Rust 实现：Rust、TypeScript、JavaScript、Python、Go。
- Tier 3 文本兜底：未覆盖语言、解析失败、超大文件。
- `CodeChunk` 构造：符号名、限定名、kind、父符号、行区间、签名、doc、imports、内容哈希。
- **单文件解析失败必须捕获并降级为 Tier 3，绝不向上传播。**
- 不引入 tree-sitter（工具链缺失，见[审计](./plugin-system-audit.md#11-构建流水线)）。Tier 1 作为后续独立 ADR。

### Phase 4 — 符号与词法检索

- 符号表：限定名排序数组 + 二分 + 前缀/模糊匹配。
- 倒排索引：标识符切分（camelCase / snake_case 拆词）、BM25、posting list delta 编码。
- **不驻留 chunk 全文**，只存 `(file_path, start_line, end_line, content_hash)`，返回结果时按需重读。

### Phase 5 — 混合检索与上下文打包

- RRF 融合。V1 只有符号 + 词法两路，但融合层从一开始就按"任意子集"设计，为语义分支预留。
- `token_budget` 约束下的结果打包：按 chunk 取原文、去重、合并相邻区间。
- **惰性一致性校验**：命中文件在返回前重读并比对哈希，不一致即时重解析并修正。
- `code_search(query, max_results?, token_budget?, path_scope?)` 工具实现。Agent 不可见任何检索内部结构。

### Phase 6 — 增量索引

- 订阅 `core.tool.result`，识别写入类工具调用，提取受影响路径入脏集合。
- 脏集合驱动的按文件重解析：old/new chunk diff，只更新变化部分。
- 进度通过 `host-agent-emit-event` + `EventPresentation` 反馈到 Footer。

**Phase 1–6 构成 V1，零新增 Host 能力（除门禁项）。** 交付一个可用的词法 + 符号代码检索引擎。

### Phase 7 — Benchmark 与加固

- fixture repository + 基准查询集：
  - where are shell permissions checked?
  - how are approvals persisted?
  - what executes an approved tool?
  - where is session state stored?
  - which component matches permission rules?
- 指标：Recall@5、Recall@10、MRR。
- 对照组：Lexical / Semantic / Hybrid（V1 只有 Lexical，Semantic 与 Hybrid 在 V2 补齐对照）。
- 加固：超大仓库行为、fuel 边界、内存边界、解析异常、存储配额耗尽。

### Phase 8 — Embedding Provider 接入（V2 起）

- Phase 1 已冻结 `EmbeddingProvider`、`UnconfiguredEmbeddingProvider`、`EmbeddingSpace` 与
  `embedding_space_id` 边界；本阶段接入 Api / Local 的真实实现。
- 语义缺席时的 `Degraded` 路径打通并测试。

**该阶段可以在 Host HTTP 能力落地前完成**——它只是抽象与降级路径，不含实际网络调用。

### Phase 9 — API Provider（依赖 Host HTTP 能力）

见 [ADR：API 嵌入](./adr-api-embedding.md)。

### Phase 10 — 语义向量索引（依赖 Host blob 存储）

见 [ADR：存储](./adr-storage.md)。此时同步把 Phase 4 的索引结构迁入 blob 落盘，冷启动从重建变为读取。

### Phase 11 — 混合检索接入语义分支

RRF 加入第三路。Phase 5 的融合层无需重构。

### Phase 12 — Local Provider（依赖 Host embedding 能力）

见 [ADR：本地嵌入](./adr-local-embedding.md)。在该 Host 能力提案通过前不启动。

## 任务边界

需求文档 §44 要求明确划分。两个 Agent 不得同时修改同一模块。

### Claude Code owns

| 范围 | 理由 |
| --- | --- |
| `crates/agent-plugins/**` 的全部改动 | 信任边界、权限模型、ABI |
| `crates/agent-tui/src/plugin/**` | 宿主装配与能力注入 |
| `wit/plugin.wit` | ABI 契约 |
| `docs/code-context-engine/**` | 架构与 ADR |
| 插件的 `plugin.toml`（能力声明） | 权限边界 |
| 插件的 `provider/` 与 `EmbeddingProvider` 边界定义 | 跨 Host 能力的契约面 |
| `smoke-tests/` 的 Host 集成部分 | 端到端权限验证 |
| 全部 Host 能力提案与最终 Review | — |

### Codex owns

| 范围 | 理由 |
| --- | --- |
| `examples/plugins/code-context-plugin/src/scanner/` | 纯插件内逻辑 |
| `.../src/parser/`（LanguageAdapter 实现） | 纯计算，边界清晰 |
| `.../src/index/`（符号表、倒排、chunk store） | 纯数据结构 |
| `.../src/retrieval/`（BM25、RRF、打包） | 纯算法 |
| `.../src/tests/` 单元测试 | — |
| Benchmark fixture 与评测脚本 | — |

**Phase 1 已冻结的交界面**：`CodeChunk`、`Symbol`、`IndexManifest`、`EmbeddingSpace`、
`EmbeddingProvider`。`LanguageAdapter` 与 fuel 分片游标尚未冻结，下一阶段必须根据实际扫描与
解析需求设计，不得猜测接口。

**Codex 在架构冻结前不得开始**：SQLite / LanceDB / Qwen runtime / tree-sitter 架构 / file watcher 的任何实现——这些选型全部已在 ADR 中被否决或推迟，提前实现是纯浪费。

## 验证门禁

每个阶段必须通过 `docs/plugin/testing.md` 的三层：

1. 插件纯逻辑单元测试（`cargo test`）。
2. component 编译（`cargo check --offline --target wasm32-wasip2`）。
3. 独立 smoke test（真实 Host 加载、能力鉴权、工具路由）。

`docs/plugin/testing.md:39` 明确：关键官方插件必须在构建 component 后运行 smoke test，`cargo check` 不能替代。Code Context Engine 按官方插件标准对待。

全量离线校验：`bun run check`。

## V1 明确不做

Full LSP、Call Graph、Git history 索引、跨仓库语义索引、云端托管索引、团队共享索引、LLM reranker、LLM query rewriting、分布式索引、远程代码索引。
