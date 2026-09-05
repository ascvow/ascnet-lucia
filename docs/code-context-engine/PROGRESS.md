# Code Context Engine Development Progress

本文件是 Code Context Engine 开发的 **Single Source of Truth**。任何新 Session、接手的 Agent
或人工开发者，只需阅读 `docs/code-context-engine/` 与本文件即可确定开发进度与下一步动作。

更新日期：2026-09-05

## Current Phase

Phase 15 — Hardening

Status: IN PROGRESS

当前加固重点是中文行为描述的语义排序区分度与真实 API 长任务可靠性。符号与 BM25 继续使用
原子块；相邻过短兄弟声明新增有界语义复合表示，并把父符号、import 上下文与正文送入嵌入
模板。API 嵌入现已增加主动限速、429/5xx 单批重试、429 可续状态与跨进程 sqlite-vec 缓存；
已完成本仓库真实模型的函数定位验收：八条业务描述的目标定义 Top-10 从 6/8 提升到 8/8，
Top-1 从 4/8 提升到 6/8；真实工具入口同样达到 8/8，并验证返回源码与行号一致。
详见 [2026-09-05 验收记录](./retrieval-acceptance-2026-09-05.md)。这不是跨仓库或完整 App 场景验收。

## Overall Status

- [x] Phase 0 — Architecture Revalidation
- [x] Phase 1 — Plugin Skeleton
- [x] Phase 2 — Repository Discovery
- [x] Phase 3 — Structural Parsing
- [x] Phase 4 — Structural Index
- [x] Phase 5 — Lexical & Symbol Retrieval
- [x] Phase 6 — Embedding Abstraction
- [x] Phase 7 — API Embedding Provider（本轮实现宿主 HTTP 能力，阻塞解除）
- [x] Phase 8 — Semantic Vector Index（WASM int4 快照；原生入口 sqlite-vec 持久化）
- [x] Phase 9 — Hybrid Retrieval
- [x] Phase 10 — Context Builder
- [x] Phase 11 — Incremental Indexing
- [ ] Phase 12 — Local Embedding Provider — **BLOCKED**（Host 无 embedding 能力）
- [x] Phase 13 — Agent Tool Integration
- [x] Phase 14 — Retrieval Benchmark
- [ ] Phase 15 — Hardening

**实际执行顺序**：

```
2 → 3 → 4 → 5 → 6 → 9 → 10 → 11 → 7 → 8 → [12 BLOCKED] → 13 → 14 → 15
```

Phase 7 / 8 原计划跳过，在 Phase 11 之后因宿主 HTTP 能力落地而补做。Phase 9 的融合层
从一开始就按「任意子集」设计，因此语义分支从缺席到接入都不需要改动融合代码——
新增的第三路通道是纯粹的加法。语义不可用时 `code_search` 进入 `Degraded`，
这正是架构第 13 问要求的降级形态。

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
Updated Date: 2026-09-04

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

---

### Phase 2 — Repository Discovery

Status: COMPLETE
Completed Date: 2026-09-03

Summary：实现仓库扫描器，从宿主注入的可信工作区稳定输出 `IndexableFile[]`（路径、语言、
内容哈希、字节数、行数）。含忽略规则、语言识别、敏感文件排除、错误隔离与 fuel 分片游标。

Implemented:
- **可分片扫描游标**：`RepositoryScan::advance(host, budget)` 消耗到预算上限即保存游标返回。
  预算含 `max_host_calls` 与 `max_bytes_read` 两个维度——只限调用次数挡不住一批大文件，
  只限字节数挡不住海量空文件。**这是全局机制，Phase 4 与 Phase 11 复用同一形态。**
- **`.gitignore` 语义**：注释、否定（`!`）、目录限定（结尾 `/`）、锚定（开头 `/` 或含内部 `/`）、
  `*`、`?`、字符类（含区间与否定）、`**` 跨段匹配、行尾空白与 `\!` 转义。
- **三层优先级**：硬排除（`.git`/`.hg`/`.svn`，不可覆盖）→ 内置默认忽略（可被 `!` 覆盖）→
  规则文件（`.gitignore` 后接 `.agentignore`，深层目录覆盖浅层）。
- **规则链 arena**：每个待遍历目录只持有一个 `u32` 链节点下标，而不是完整规则副本。深目录树下
  逐层复制规则会让游标体积随深度平方增长。
- **语言识别**：扩展名白名单（22 种语言 + 无扩展名文件名表），未识别扩展名直接跳过而不是
  当作文本读取。`Language::supports_structural_parsing()` 标注 V1 的 Tier 2 覆盖范围。
- **敏感文件排除**：`.env*`、`*.pem`、`*.key`、`*.p12` 等无条件排除；`credential*` / `secret*`
  前缀规则**只作用于非源码文件**（理由见下方 Important decisions）。敏感文件单独计数。
- **错误隔离**：目录列举失败、文件读取失败、超大文件、内嵌 NUL 的二进制内容全部降级为告警，
  不中断扫描。告警数量有上限，总数仍如实计数。
- **规模上限**：深度、目录数、文件数、单目录条目数、单文件字节数、告警数。

Key files:
- `examples/plugins/code-context-plugin/src/repository/mod.rs` — 扫描器与分片游标
- `.../src/repository/ignore.rs` — gitignore 语义规则引擎
- `.../src/repository/language.rs` — 语言识别与文件归类
- `.../src/repository/tests.rs` — 扫描器测试
- `.../src/testing.rs` — 新增 `RepoHost` 虚拟文件系统替身

Tests：新增 57 个（扫描器 26 + 忽略规则 20 + 语言识别 11），插件单测总数 55 → 112。

覆盖：常规文件、忽略文件、嵌套 `.gitignore`、`.agentignore` 覆盖、二进制、超大文件、
未识别语言、敏感文件、深度上限、规模上限、告警上限、读取失败隔离、列举失败隔离、
内嵌 NUL、空仓库、分片恢复一致性、预算约束、输出确定性。

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：112 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：无警告。
- `bun run test:plugin:code-context`：全链通过（单测 → WASM release 构建 → 7 个 smoke test）。

Build Result：`wasm32-wasip2` release 构建成功，7 个真实 Host smoke test 全通过，无 regression。

Important decisions:
1. **扩展名白名单而非黑名单**。`FileEntry` 没有 size/mtime，"先读了再判断"要为每个未知扩展名
   付出一次完整读取，还可能把几百 MB 的产物读进 64 MiB 线性内存。白名单把风险挡在读取之前。
2. **`credential*` / `secret*` 只作用于非源码文件**。需求要求过滤这两个前缀，但直接按前缀过滤
   会排除 `secrets.rs`、`credentials.ts` 这类**定义凭证处理逻辑的源码**——Agent 恰恰经常需要
   检索它们，且它们存的是代码不是凭证值。真正的风险集中在 `secrets.json`、`credentials.yaml`
   这类数据文件。`.env` / `*.pem` / `*.key` 则无条件排除，这些形态不存在"其实是源码"的情况。
3. **硬排除独立成层**。把 `.git/` 交给可被否定的规则去挡，等于允许一条用户规则让扫描器走进
   版本库内部。
4. **`.agentignore` 排在 `.gitignore` 之后**。用户用它表达的是"给检索引擎看什么"，理应比版本
   控制规则更靠后，因此可以用 `!` 重新包含被 git 忽略的目录。
5. **完成时按路径排序**。下游增量 diff 需要在两次扫描结果间做有序比较，保留广度优先的发现
   顺序会让目录结构的微小变化产生大量伪差异。
6. **符号链接环靠深度与数量上限防御**，不靠路径去重。软链目标的相对路径每次都不同，
   按路径去重挡不住它；宿主的 `canonicalize` + 前缀校验已挡住指向工作区外部的符号链接。

Known limitations:
- **子目录列举的运行时验证推迟到 Phase 4。** 扫描器尚未接入任何导出函数，因此只在 `RepoHost`
  替身上验证过。其依赖的宿主契约（`list_dir` 对子目录返回完整仓库相对路径、按路径排序）
  已通过直接阅读 `capability/mod.rs` 的 `strip_prefix(&resolved.display_root)` 确认，
  且 `list_dir(".")` 这条路径已由 Phase 1 的 smoke test 端到端验证。Phase 4 把扫描接入索引
  构建后，必须补一个真实 Host 上的子目录扫描 smoke test。
- 单文件体量上限只能在读取后生效（宿主不提供 size），因此超大文件仍会产生一次完整读取的
  内存峰值。`FileEntry` 增加 size/mtime 是架构中的 Recommended 级 Host 改动。
- 未实现文件 watcher；当前由 Phase 11 的 `core.tool.call` / `core.tool.result` 路径覆盖 Agent
  写入，编辑器等外部修改仍由 Phase 10 惰性一致性校验兜底。watcher 仍是 Optional 级 Host 能力。

Deferred items:
- 扫描结果的持久化（`hash_index` 已提供路径→哈希映射，落盘属于 Phase 4）。
- 增量 diff（added / modified / deleted）已在 Phase 11 完成。

Next: Phase 3 — Structural Parsing

---

### Phase 3 — Structural Parsing

Status: COMPLETE
Completed Date: 2026-09-03

Summary：把源文件转换为 `CodeChunk` 与 `Symbol`。**结构化解析一律使用现成的纯 Rust 解析器，
不自建语法分析**——这推翻了原计划的"手写 Tier 2 启发式解析器"，详见下方 Architecture Drift
与新增的 [adr-structural-parsing.md](./adr-structural-parsing.md)。

Implemented:
- 统一的 `Declaration` 归一化层：各家 AST 折算成同一结构后，限定名推导、chunk 装配与符号表
  构建对所有语言共用一套代码。**新增语言 = 新增一个适配器**，不是新增一套解析器。
- 四个语言适配器：
  | 语言 | 解析器 | 覆盖 |
  | --- | --- | --- |
  | Rust | `syn` 3 | fn / struct / enum / trait / impl / mod / const / static / type / macro_rules，含 trait 与 impl 内部条目 |
  | TS / JS（含 TSX / JSX） | `oxc_parser` 0.148 | function / class + 方法 / interface + 成员 / enum / type / namespace / 箭头函数与函数表达式赋值 |
  | Python | `rustpython-parser` 0.4 | def / async def / class + 方法、全大写模块级常量、docstring |
  | Go | `gosyn` 0.2 | func / 方法（按接收者限定）/ struct / interface / type / const / var |
- 限定名推导：Rust 用 `::`，其余用 `.`；Go 方法通过 `owner` 得到 `Repo.Get`——接收者参与限定名
  但不是词法父节点。
- 文档注释：Rust 从 `#[doc]` 属性、Python 从 docstring、Go 从 `docs` 注释节点提取。
- import 收集：Rust 的 `use` 树还原、TS/JS 的模块路径、Python 的 `import` / `from`、Go 的 import 块。
- **稳定 chunk 标识**：由仓库身份 + 文件路径 + 限定名 + 符号种类 + 同名序号决定，
  **不含行号**——行号会在文件任意位置插入一行后全部改变，让增量索引把整个文件判成新内容。
- Tier 3 文本分块：未覆盖语言、解析失败、无声明的文件。优先在空行处切分，行区间连续不重叠。

Key files:
- `examples/plugins/code-context-plugin/src/parser/mod.rs` — 归一化层、限定名、chunk 装配、文本兜底
- `.../src/parser/rust_lang.rs`、`typescript.rs`、`python.rs`、`golang.rs` — 四个 AST 适配器
- `.../src/parser/tests.rs` — 解析测试
- `docs/code-context-engine/adr-structural-parsing.md` — 选型 ADR

Tests：新增 34 个，插件单测总数 112 → 146。

覆盖：五种语言的声明种类与限定名、方法归属、嵌套模块 / namespace、行区间、文档注释、import、
TSX / JSX、语法错误降级、未覆盖语言降级、无声明文件降级、空文件、纯空白文件、
长文本切分连续性、chunk 身份跨行号平移稳定、内容变化时哈希变化、跨文件 ID 区分、
解析确定性、多字节字符行号。

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：146 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：无警告。
- `bun run test:plugin:code-context`：全链通过（单测 → WASM release 构建 → 7 个 smoke test）。

Build Result：`wasm32-wasip2` release 构建成功，无 regression。

**体积影响**：解析器全部链接进产物后，插件 wasm 从 592 KB 增至 **3.4 MB**（实测，用临时探针
强制链接后测量；当前 592 KB 是因为解析器尚未被任何导出路径引用而被死代码消除，
Phase 4 接入后会体现为 3.4 MB）。模块体积不占用 64 MiB 线性内存预算，可接受。

Important decisions:
1. **不自建语法分析。** 手写解析器每新增一种语言就要重写一套词法屏蔽 + 声明识别 + 边界推导，
   且必须自行处理字符串里的括号、注释里的关键字、Rust 生命周期与字符字面量歧义、模板字面量、
   JSX——这些在成熟解析器里早已解决。
2. **原"缺 C 工具链所以只能手写"的论证只对 Tree-sitter 成立。** 纯 Rust 解析器走 `rustc` 的
   `wasm32-wasip2` 后端，与 C 工具链无关。已实测验证四个解析器均可编译并正确工作。
3. **`oxc` 的容错特性被刻意保留**：语法错误时仍返回可用 AST，只在 `panicked` 时降级。
   编辑中的半成品文件依然应该可被检索，这正是 Coding Agent 场景所需。
4. **箭头函数赋值归类为函数**：`export const foo = () => {}` 是现代 TS/JS 最主流的函数定义
   形态，归类为常量会让"某某函数在哪"的查询大面积落空。
5. **Python 只收全大写模块级赋值为常量**：Python 没有 const，全大写是社区通行约定；
   收录每个模块级变量会让符号检索被噪声淹没。
6. **Rust impl 块以被实现的类型命名**，trait 名记入 `owner`：用户找的是"这个类型的方法在哪"。

Known limitations:
- **未覆盖语言走 Tier 3**：Vue、PHP、Java、C/C++、Ruby 等仍进入索引、仍可被词法与标识符检索
  命中，但没有符号级边界。后续按需增加适配器，**不因为缺适配器就把文件排除在索引外**。
- Vue 单文件组件的 `<script>` 块理论上可交给 `oxc` 解析，属后续优化。
- Go 的类型与常量声明没有结束偏移（`gosyn` 不提供），结束行退化为起始行；函数有精确结束位置。
- 解析器大版本升级会带来 API 变更（本次已遇 `oxc` 0.148 拆分 `TSModuleDeclaration`），
  适配层集中在四个文件，影响面可控。

Deferred items:
- 解析结果的持久化与索引构建属于 Phase 4。
- Phase 11 复用本阶段提供的稳定 chunk ID，按文件应用增量 diff。

Next: Phase 4 — Structural Index

---

### Phase 4 — Structural Index

Status: COMPLETE
Completed Date: 2026-09-03

Summary：把 Phase 2 的扫描器与 Phase 3 的解析器接成一条索引流水线，产出可查询的结构索引，
并按存储 ADR 分流持久化。`code_search` 现在真实驱动索引构建并如实上报索引规模。

Implemented:
- **`StructuralIndex`**：文件记录、chunk、符号的驻留存储，支持 upsert / remove / get / list、
  按路径取 chunk、规模统计与清空。`upsert_file` 幂等——重复索引同一文件不产生孤儿 chunk，
  这正是 Phase 11 增量重建依赖的性质。跨仓库写入被拒绝。
- **`IndexBuild` 流水线**：扫描 → 解析 → 入库，复用 Phase 2 已冻结的 `ScanBudget` 游标形态，
  **没有发明第二套分片机制**。
- **`FileSink` 回调**：扫描器在读取文件的同一时刻把正文交给解析器，随后即丢弃。
  避免了"扫描读一遍、解析再读一遍"的双倍宿主调用（有测试断言每个文件只读一次），
  也避免为后续解析而把整个仓库的正文驻留在内存里。
- **惰性构建**：由首次 `code_search` 触发，之后每次调用推进一个预算片。**不在 `activate` 里做**
  ——`activate` 与其他导出共用同一份 fuel 预算，在那里做全量索引必然 trap。
- **哈希表持久化**：`IndexStore::save_file_hashes` / `load_file_hashes` / `clear_file_hashes`，
  键 `index.file_hashes`，上限 5 万条。超限时**不写入**而不是截断——残缺的哈希表会让 Phase 11
  把未变更文件误判成已变更，比没有哈希表更糟。
- **索引规模进入工具返回值**：`unavailable_result_with_index` 让 `code_search` 的 details 带上
  files / chunks / symbols / structural_files / text_files / truncated。这既是用户可见的进度，
  也是 smoke test 验证扫描与解析真跑通的唯一可观测出口。
- **补齐 Phase 2 遗留的真实 Host 验证**：新增 smoke test 覆盖三层嵌套目录、多语言解析、
  `.gitignore` 与默认忽略规则。

Key files:
- `examples/plugins/code-context-plugin/src/index/mod.rs` — `StructuralIndex`、`FileRecord`、`IndexStats`
- `.../src/index/build.rs` — `IndexBuild` 流水线与 `BUILD_BUDGET`
- `.../src/index/tests.rs` — 索引与流水线测试
- `.../src/storage.rs` — 哈希表持久化
- `.../src/repository/mod.rs` — 新增 `FileSink` 与 `advance_with`
- `.../smoke-tests/tests/code_context.rs` — 真实 Host 嵌套目录索引测试

Tests：新增 23 个，插件单测总数 146 → 169；smoke test 7 → 8。

覆盖：索引增删改查、幂等 upsert、旧 chunk 清理、跨仓库拒绝、文本兜底统计、清空、
哈希表导出与变更判断、端到端构建、每文件只读一次、分片与一次跑完结果一致、
未完成构建的部分状态、读取失败隔离、解析失败文本兜底、空仓库、哈希表往返、
重启后重建产出同一批 chunk 身份、无项目作用域时拒绝写入、清理哈希表。

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：169 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：无警告。
- `bun run test:plugin:code-context`：全链通过（单测 → WASM release 构建 → 8 个 smoke test）。

Build Result：`wasm32-wasip2` release 构建成功，无 regression。

Important decisions:
1. **索引本体内存驻留，不落盘。** 宿主存储是 8 MiB JSON KV 且每次写入重写全量，装不下 chunk
   与符号表。这是存储 ADR 的既定结论，本阶段严格执行：落盘的只有 manifest 与路径→哈希表。
2. **"重启恢复"的含义是重建结果确定。** 因为索引不落盘，恢复不是把索引读回来，而是
   **重建产出与上一轮完全一致的 chunk 身份与哈希表**。测试按这个定义验证。
3. **`FileSink` 而不是让扫描器保留正文。** 保留正文会让内存占用与仓库总体积成正比；
   重新读一遍会让宿主调用翻倍。回调让两者都避免。
4. **构建完成后的状态是 `Degraded` 而不是 `Ready`。** 语义分支缺席（Host 缺 HTTP 与 embedding
   能力），按架构第 13 问的定义这就是 `Degraded`。**这不是错误，是如实反映能力边界。**
5. **哈希表本阶段只写不读。** 结构索引每次启动重建，"上一轮的哈希"对本轮构建没有用处；
   它的消费者是 Phase 11 的增量索引。刻意不预先把它读进上下文，避免留下无用字段。
6. **`BUILD_BUDGET` 取值保守**（32 次宿主调用 / 512 KiB）。构建比纯扫描多跑一遍语法解析，
   单位字节指令数高一个量级。**真实 fuel 消耗需在 Phase 14 benchmark 实测后再调**——
   宁可多跑几轮，也不要 trap。

Known limitations:
- **`BUILD_BUDGET` 未经实测。** 当前取值是保守估计，可能过于保守（索引大仓库需要很多次
  `code_search` 调用才能建成）。Phase 14 必须实测 fuel 消耗并校准。
- 大仓库的首次索引会跨多次 `code_search` 调用，期间工具返回 `indexing` 状态与已处理文件数。
  这是 fuel 模型的直接后果，不是缺陷；但用户体验上需要 Phase 13 给出更好的提示。
- 索引规模上限（5 万文件 / 40 万 chunk）触顶时标记 `truncated` 并如实上报，不静默丢弃。
- 检索尚未接入：`code_search` 仍返回明确的不可用状态 + 索引规模，不返回结果（Phase 5）。

Deferred items:
- 符号表与倒排索引的检索结构属于 Phase 5。
- Phase 11 已复用文件哈希与稳定 chunk ID 完成 added / modified / deleted / unchanged diff。

Next: Phase 5 — Lexical & Symbol Retrieval

---

### Phase 5 — Lexical & Symbol Retrieval

Status: COMPLETE
Completed Date: 2026-09-03

Summary：在零 embedding 的前提下建立两路检索能力——符号检索与 BM25 词法检索，
产出内部候选 `SearchCandidate`。**刻意不绑定到 Agent API**：候选还要经过 Phase 9 的融合与
Phase 10 的打包，现在绑定会让融合层无处安放，也会让模型先学到一套很快作废的输出形态。

Implemented:
- **代码专用切词**（`tokenize`）：驼峰 / 帕斯卡 / 蛇形 / 全大写蛇形 / 路径 / 限定名，
  首字母缩写边界（`HTTPServer` → http + server），数字后接大写（`sha256Digest`）。
  完整标识符与子词都入索引，使精确查询强于部分匹配。
- **中日韩支持**：单字 + 双字（bigram）词元。本仓库注释是中文，只切 ASCII 等于放弃全部
  文档注释检索；只有单字则"权限"与"限权"无法区分。不引入分词器——词典依赖与体积代价
  不成比例。
- **符号检索**：两份排序下标数组 + `partition_point` 二分。四档匹配（精确限定名 1.0 /
  精确短名 0.85 / 限定名后缀 0.75 / 短名前缀 0.4），档次刻意拉开，使融合层无论怎么加权
  都不会把前缀匹配排到精确匹配前面。同时处理"整条查询是标识符"与"自然语言里夹标识符"。
- **BM25 词法检索**：标准 BM25（k1=1.2, b=0.75）+ **字段加权**，通过重复计入词频实现：
  符号名 5 倍、限定名 3 倍、路径 / 签名 / 文档 2 倍、正文 1 倍。无需改公式即体现字段重要性。
- **索引正文但不驻留正文**：倒排索引保存 `term → (chunk, 词频)`，正文切词后即丢弃。
  这让 `execute_command` 这类只出现在函数体里的标识符可被检索，同时不违反存储 ADR。
- **`SearchCandidate`**：带通道内排名（从 1 开始）。融合用排名而非分数——BM25 分数无上界、
  符号分在 0..1，量纲不可比。**这正是选 RRF 而不是加权分数融合的直接原因。**
- 路径过滤作用于两路通道；结果顺序完全确定（分数相同时按 chunk ID 排序，避免 HashMap
  遍历顺序让同一查询在不同进程返回不同排名）。

Key files:
- `examples/plugins/code-context-plugin/src/retrieval/mod.rs` — `SearchCandidate`、`RetrievalIndex`
- `.../src/retrieval/tokenize.rs` — 代码与中日韩切词
- `.../src/retrieval/symbol.rs` — 符号索引与四档匹配
- `.../src/retrieval/lexical.rs` — BM25 倒排索引与字段加权
- `.../src/retrieval/tests.rs` — 六类查询验证
- `.../src/index/build.rs` — 构建时同步建立检索索引

Tests：新增 38 个（切词 14 + 检索 24），插件单测总数 169 → 207。

Definition of Done 的六类查询逐条验证：

| 查询类型 | 测试 | 结果 |
| --- | --- | --- |
| 精确符号 | `exact_qualified_symbol_ranks_first` | `PermissionManager::check` 命中且排首位 |
| 部分符号 | `partial_symbol_prefix_is_found` | `Permission` 命中 Manager 与 Rule |
| 标识符 | `snake_case_identifier_in_body_is_found` | `tool_call_id` 命中 |
| 文件路径词 | `file_path_term_is_searchable` | `session store` 命中 `src/session/store.rs` |
| 源码关键字 | `source_keyword_inside_body_is_found` | `execute_command` 命中函数体内调用 |
| 文档注释关键字 | `chinese_doc_comment_keyword_is_found` | 中文「审批」「会话状态」命中 |

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：207 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：无警告。
- `bun run test:plugin:code-context`：全链通过（单测 → WASM release 构建 → 8 个 smoke test）。

Build Result：`wasm32-wasip2` release 构建成功，无 regression。

Important decisions:
1. **候选不绑定 Agent API。** Phase 5 交付的是内部候选；`code_search` 仍返回状态而不是结果。
   发明一个会被 Phase 10 整体替换的临时结果格式，是纯浪费，且会污染模型学到的调用形态。
2. **字段加权用重复计入词频实现**，而不是维护多套倒排表。后者要为每个字段各存一份 posting，
   内存翻数倍，收益相同。
3. **中日韩用 bigram 而非分词器。** 本仓库注释全中文，不支持等于放弃文档注释检索。
4. **查询侧词频只计一次。** BM25 的查询词频对代码检索没有增益，反而会让重复词放大成噪声。
5. **符号检索要求 `finalize`。** 未排序时返回空而不是错序结果——错序结果比没结果更有害，
   有专门测试守护这条。

Known limitations:
- **BM25 参数未经调优。** k1 / b 与六个字段权重都是常用默认值，没有基于真实检索质量校准。
  Phase 14 的 benchmark 必须用 Recall@5 / Recall@10 / MRR 验证并调整。
- 倒排索引 posting 上限 300 万，触顶后停止收录并标记 `truncated`。大仓库的实际内存占用
  未实测，Phase 14 需要一并测量。
- 前缀匹配要求至少 3 字符，更短的前缀会匹配到成百上千符号并淹没精确匹配。
- 未实现拼写纠错与同义词扩展，属于 V1 明确不做的范围。
- `RetrievalIndex` 已提供按 chunk 与按文件删除接口，供 Phase 11 同步清理符号和词法 posting。

Next: Phase 6 — Embedding Abstraction

---

### Phase 6 — Embedding Abstraction

Status: COMPLETE
Completed Date: 2026-09-03

Summary：把语义层的边界补齐到可以接入真实 provider 的程度。Phase 1 已冻结
`EmbeddingProvider::space` / `embed`、`EmbeddingSpace` 与 `embedding_space_id`，
**本阶段是补齐而非重做**：围绕这两个必需方法加默认实现，让分批、维度校验与降级判断
对所有 provider 一致。

Implemented:
- **最小实现契约**：provider 只需实现 `space()` 与 `embed()`；`provider_id` / `model_id` /
  `dimension` 全部从嵌入空间派生，不重复保存状态。新增 provider 的成本是两个方法。
- **`embed_documents` / `embed_query` 分离**，并自动按 `max_batch_size` 分批。两者分开是
  因为提示模板不同，而模板版本参与嵌入空间标识——混用会让查询向量与文档向量不可比较。
  测试验证同一段文本作为查询与作为文档得到**不同**向量。
- **返回值校验**（`embed_batched`）：条数与维度不符即报错。向量与 chunk 是按下标对齐的，
  provider 少返回一条会让后面全部向量错位挂到别的 chunk 上——**这种错误不会让检索失败，
  只会让结果长期缓慢地不正确**，是最难发现的一类 bug。在抽象层一次挡住比让每个 provider
  各自记得检查可靠。
- **`health_check()`**：使降级判断不必等到第一次真正嵌入失败才发生。默认实现检查是否声明
  嵌入空间；真实 provider 应覆盖为轻量探测。
- **`select_provider(config)`**：provider 选择集中到一处，Phase 7 / 12 接入时只改这里。
  **刻意不提供"选不到就用 Mock"的回退。**
- **`EmbeddingSpace::is_compatible_with`**：比较完整标识而不是维度。
- **`MockEmbeddingProvider`**（`#[cfg(test)]`）：确定性伪向量，用于在真实 provider 缺席时
  验证接口契约本身。带 `cfg(test)` 因此不会编译进插件产物，也无法被 `select_provider` 选中。

Key files:
- `examples/plugins/code-context-plugin/src/embedding.rs` — provider 契约、分批校验、选择、mock
- `.../src/model.rs` — `EmbeddingSpace::is_compatible_with` 与兼容性测试

Tests：新增 19 个，插件单测总数 207 → 226。

覆盖：派生元数据、批量顺序与条数对应、确定性、查询与文档模板分离、自动分批、
批大小为 0、空输入、少返回向量被拒、错误维度被拒、失败降级、归一化单位向量、
trait 对象可用、伪向量分布合理、provider 选择遵循配置、未配置 provider 健康检查失败。

**嵌入空间兼容性**（DoD 明确要求）：
- `same_dimension_different_model_is_incompatible` — 两个 1024 维空间因模型不同判定为不兼容。
- `every_space_field_affects_compatibility` — 八个字段逐一变化都必须打破兼容性。
- `switching_model_invalidates_the_space_even_at_same_dimension` — 从 provider 侧验证同一条。
- `switching_embedding_space_rebuilds_semantics_only` — 换空间只触发语义重建，结构索引保留。

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：226 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：无警告。
- `bun run test:plugin:code-context`：全链通过（单测 → WASM release 构建 → 8 个 smoke test）。

Build Result：`wasm32-wasip2` release 构建成功，无 regression。

Important decisions:
1. **补齐而非重做冻结接口。** `space()` 与 `embed()` 保持原样，新增能力全部走默认方法。
   重新设计已冻结的契约会让 Phase 1 的 smoke test 与 manifest 兼容判断一并失效。
2. **维度校验放在抽象层。** 这是 Phase 7 DoD 明确要求的检查项之一；放在抽象层意味着
   Api 与 Local 两个 provider 都自动获得，不会有一个忘记做。
3. **Mock 严格限定在测试中。** 生产路径上"绝不返回伪造向量"的约束从骨架阶段延续至今。
4. **兼容性比较完整标识而不是维度。** 这是语义索引最容易犯且最难发现的错误。

Known limitations:
- **没有真实 provider 可测。** 本阶段验证的是接口契约，不是任何真实嵌入质量。
  Phase 7 / 12 被宿主能力阻塞，见 Blocked Phases。
- `health_check` 的默认实现只检查是否声明空间，不做真实探测——真实探测要等 provider 落地。
- 提示模板本身尚未定义（只有版本号参与空间标识）。模板内容属于 Phase 7 / 12 的 provider 实现。
- 语义管线的**下游**（向量索引、语义检索）属于 Phase 8，当前被阻塞，因此本阶段只能验证
  provider 边界，无法端到端验证"语义管线"。

Next: Phase 9 — Hybrid Retrieval（Phase 7 / 8 被阻塞，按既定顺序跳过）

---

### Phase 9 — Hybrid Retrieval

Status: COMPLETE
Completed Date: 2026-09-03

Summary：用 RRF 把多路候选融合成稳定排序的 `SearchResult[]`。融合层在**任意通道子集**上
成立，因此语义分支缺席不影响可用性——这是架构第 13 问要求的降级形态。

Implemented:
- **RRF 融合**：`score = Σ 1 / (k + rank)`，k = 60。用排名而非分数，因为各通道分数量纲
  不可比（BM25 无上界且随语料漂移、符号分固定 0..1、语义是余弦相似度），
  加权相加需要先归一化，而归一化依赖分布假设、换个仓库就失效。
- **任意通道子集**：空输入、单通道、三通道全部正常工作，有测试逐一覆盖。
  新增 `CandidateSource::Semantic` 变体并用**合成语义候选**验证三路融合无需改动代码——
  把"为语义分支预留"从声称变成事实。
- **集中配置的提升**（`FusionConfig`）：精确限定名 3.0、精确短名 1.5、路径 1.0、
  元数据重排 0.75，
  单位是 `rank_one_unit() = 1/(k+1)`，即"相当于在一路里排第一"。
  **没有散落的魔数**：每个值都以可读量级表达并写明依据。
- **精确符号不被挤下去**：RRF 天然偏好"多路都命中"，但用户查确切符号名时那个符号必须
  排第一。3.0 当量使单路精确匹配稳定压过双路各排第一（约 2.0 当量），有测试守护。
- **弱匹配不提升**：后缀与前缀匹配本就是弱信号，再提升会让"查 check 命中所有名字带
  check 的东西"淹没目标。
- **路径提升按命中比例缩放**，且严格弱于符号提升——否则"搜某个函数"会变成"搜某个目录"。
- **确定性轻量重排**：RRF 后按查询词对符号、签名、文档注释和路径的覆盖率调整接近候选；
  标识符查询优先符号，自然语言优先文档注释，不回读正文、不调用模型或网络。0.75 当量
  小于一路第一名分值，只调整接近候选，不覆盖原始融合形成的大幅领先。
- **内部诊断** `FusionDebug`：各通道排名、RRF 原始分、各项提升、重排命中词数、可读理由。
  分数严格等于各部分之和，有测试核对。**不对 Agent 暴露**（任务书 §9）。
- **确定性**：分数相同时按 chunk ID 升序，同一查询在任何进程得到同一顺序。
- **失效候选被跳过**：候选指向已不存在的 chunk 时跳过而不是 panic——增量索引期间
  检索索引与结构索引可能短暂不同步。

Key files:
- `examples/plugins/code-context-plugin/src/retrieval/fusion.rs` — RRF、提升、`SearchResult`
- `.../src/retrieval/fusion_tests.rs` — 融合测试
- `.../src/retrieval/mod.rs` — `RetrievalIndex::search` / `search_with` 入口

Tests：Phase 9 初始新增 24 个；2026-09-04 为轻量重排补充 2 个融合测试。

覆盖：跨通道一致性胜过单通道、通道内排名、任意子集（空/单路/三路）、确定性、
并列稳定排序、limit 约束、候选池不变量、精确符号压过多路命中、弱匹配无提升、
限定名提升强于短名、路径提升、元数据覆盖率重排、重排不覆盖大幅融合领先、
提升量级次序、rank-1 当量、诊断记录各路排名、
可读理由、分数可分解、端到端精确符号排首位、结果携带定位元数据、
自然语言查询、路径过滤贯穿融合、无命中、空索引、失效候选。

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：250 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：无警告。
- `bun run test:plugin:code-context`：全链通过（单测 → WASM release 构建 → 8 个 smoke test）。

Build Result：`wasm32-wasip2` release 构建成功，无 regression。

Important decisions:
1. **RRF 而非加权分数融合。** 决定性理由是它在任意通道子集上成立，无需重新配权重。
   语义分支目前缺席、将来可能因 provider 故障临时缺席，这条性质是刚需而非优雅。
2. **提升以 rank-1 当量表达。** `exact_symbol_boost = 3.0` 的含义是"相当于额外在三路里
   各排第一"，比 `0.049` 这样的绝对值可读得多，也让调参有量纲依据。
3. **k 保持 60。** 该取值让"两路中等"胜过"单路第一"，对代码检索方向正确。
   精确符号的优先级由提升项保证，而不是靠调小 k 破坏 RRF 的一致性偏好。
4. **`Semantic` 变体现在就加。** 不是为未来预留字段，而是为了让"任意子集"能被实际测试。

Known limitations:
- **提升值未经真实检索质量校准。** 3.0 / 1.5 / 1.0 是按量级推理选定并有次序测试守护，
  但没有基于 Recall/MRR 验证。**Phase 14 必须校准**，与 `BUILD_BUDGET` 和 BM25 参数
  一并纳入同一次测量。
- `SearchResult` 不含正文片段：正文由 Phase 10 按需重读并打包（存储 ADR 的既定分工）。
- 融合按线性查找聚合候选（候选量数百级）。候选池上限提高后需要改为哈希聚合。
- 尚未接入 `code_search`：工具返回值的最终形态属于 Phase 10 / 13。

Next: Phase 10 — Context Builder

---

### Phase 10 — Context Builder

Status: COMPLETE
Completed Date: 2026-09-03

Summary：把融合结果打包成 Agent 可用的上下文，消除父子重叠、尊重 token 预算、惰性校验
文件一致性。Phase 9 的融合输出不含正文（存储 ADR 的既定分工），本阶段按需回读、
按预算打包、就地处理文件变更。

Implemented:
- **父子抑制**：检测并抑制区间包含关系（impl 块完整包含其方法），避免同一段代码返回两次。
  输入按分数降序，保留的总是更高分的那条。
- **区间合并**：相邻或夹行数 ≤ `merge_gap_lines` 的区间合并为一段，默认 gap=0（只合并紧邻）。
  采用传递性合并：候选可能桥接多个已保留区间，反复扩展并集直到收敛。
  保留主符号（首个关联项）的元数据，只扩展正文范围。
- **多样性约束**：`max_per_file` 限制单文件贡献条目数（默认 4），防止大文件占满预算而让
  "涉及哪几个文件"这类信息丢失。
- **惰性一致性校验**（Architecture Q8）：索引是快照，仓库可能被外部编辑（编辑器保存、
  `git checkout`），而宿主不提供文件变更事件。**按文件比对内容哈希**，不一致时重新解析该文件
  并按限定名重新定位符号。代价被限制在命中集合（通常十几个文件）而不是整个仓库。
  **合并过的区间也能重定位**：文件哈希校验是按文件而不是按条目，合并后的区间沿用主符号的
  新位置——这是前期架构修正堵住的设计漏洞。
- **Token 估算**：区分 ASCII（÷4）与 CJK（1:1）密度，向上取整保守估算——宁可少放一条
  也不超预算（超预算的后果是调用方截断，截断点不可控）。Phase 14 必须校准。
- **预算打包**：预算不足时跳过大条目继续尝试小条目（按分数降序遍历），而不是直接中断。
- **超长片段截断**：超过 `max_snippet_lines` 的片段截断并明确标注 `// …（片段已截断）`，
  避免模型把半截函数当作完整实现。
- **不返回错误**：文件读不到或符号已消失的条目被丢弃并计入统计，其余照常返回。
  一个被删掉的文件不该让整次检索失败。

Key files:
- `examples/plugins/code-context-plugin/src/context/mod.rs` — 核心实现（~555 行）
- `.../src/context/tests.rs` — 完整测试套件（~593 行，24 个测试）
- `.../src/retrieval/tokenize.rs` — `is_cjk` 改为 `pub(crate)` 供 token 估算使用
- `.../src/lib.rs` — 导出 `build_context`、`estimate_tokens`、`ContextBundle`、`ContextConfig` 等

Tests：新增 24 个，插件单测总数 248 → 272。

覆盖：父子抑制、无重复行、相邻合并、相距远不合并、包含时保留高分、
多样性约束、max_results、token 预算、预算为 0、预算不足跳过大条目、超长截断、
未修改文件原样返回、文件被外部修改后重新定位、符号已删除时丢弃、文件已删除时丢弃、
同一文件只读一次、token 估算区分 ASCII 与 CJK、token 估算向上取整、
空输入返回空、结果按分数降序、确定性、条目带定位与片段。

Tests Executed / Result:
- `cargo test --offline -p code-context-plugin`：272 单元测试 + 1 文档测试，全通过。
- `cargo clippy --offline -p code-context-plugin --all-targets`：1 个 unused variable 警告（测试代码），
  已修正后无警告。

Build Result：`wasm32-wasip2` release 构建成功，无 regression。

Important decisions:
1. **按文件而不是按条目校验一致性。** 文件的内容哈希已经在结构索引里，比对一次就能判定
   整个文件是否变化。按条目校验则要为每条结果单独切片比对，且对**合并过的区间无从比对**
   ——那正是这套设计要堵住的漏洞：合并会让校验被静默跳过。
2. **重新定位按限定名查找，而不是按行号。** 文件变化后行号失效，但限定名（稳定 chunk ID）
   依然唯一标识符号。找不到时丢弃条目，而不是返回错误位置的代码——**返回错误位置比不返回更糟**。
3. **Merge gap 默认 0。** 只合并紧邻或重叠，不合并中间隔着空行的两个独立方法。取值过大会把
   独立符号并成一段，丢掉按符号归属结果的能力。
4. **Token 估算区分脚本密度。** ASCII 与 CJK 的 token 密度差一个量级，用同一套系数会让含
   中文注释的代码估算偏离数倍。Phase 14 必须用真实 tokenizer 校准系数。
5. **多样性约束默认 4 条/文件。** 没有它，一个大文件里的十几个函数会占满全部预算，
   而"这个功能涉及哪几个文件"恰恰是 Agent 最需要的信息。

Known limitations:
- **Token 估算无真实 tokenizer。** 经验系数（ASCII ÷4、CJK 1:1）加保守取整，Phase 14 必须
  用真实 tokenizer 校准并测量实际偏离。
- **合并后符号元数据退化。** 合并后的区间沿用主符号（首个关联项）的元数据，其他被并入的
  符号信息丢失。用户看到的是一段代码 + 第一个符号的限定名，但这段代码里可能含多个符号。
  这是可解释的降级：合并边界是多条结果的并集，文件变化后无从还原，退回单符号定位是合理的。
- **不含嵌入向量。** Phase 10 打包的是词法与符号两路融合结果，语义分支属于 Phase 8（被阻塞）。
- 尚未接入 `code_search` 工具：最终形态属于 Phase 13。

Next: Phase 11 — Incremental Indexing

---

### Phase 11 — Incremental Indexing

Status: COMPLETE
Completed Date: 2026-09-03

Summary：在不引入 watcher 或新 Host 能力的前提下，把 Agent 成功写入事件转换为有界脏路径集合，
并复用 Phase 2 扫描器的游标、预算与目录快照，以及 Phase 4 的幂等 upsert，实现文件级增量重建。

Implemented:
- **事件驱动脏集合**：订阅 `core.tool.call`、`core.tool.result` 与 `core.run.finish`；只为成功
  的写入类工具标记路径，失败、取消、只读工具和未解析事件不产生变更。调用参数与结果
  `content.path` 均支持安全的仓库相对路径，工作区内绝对路径会先转换，越界路径拒绝。
- **有界与幂等状态**：脏路径最多 4096 项，未完成写入最多 1024 项；调用 ID 按
  `run_id + call_id` 隔离，重复 call/result 投递不会重复扩大集合，运行结束清理悬挂调用。
- **文件级增量扫描**：目录仍完整遍历以发现新增、删除和不可索引路径，但正文只读取脏文件
  与当前结构索引中不存在的新文件；哈希、语言、大小和行数均未变化的文件跳过解析。
- **稳定 chunk diff**：`StructuralIndex::upsert_file_incremental` 原子计算并应用新增、更新、
  删除与未变化 chunk，容量或跨仓库校验失败时保留旧索引并记录待重试路径。
- **双路检索同步**：更新前从符号表和词法倒排移除旧 chunk，更新后只重建变化 chunk；词法
  文档删除会重映射 posting 下标并重算长度统计，避免 stale candidate。
- **安全删除与错误隔离**：只有完整目录快照才推断删除；列举失败、深度/规模截断或未保留告警
  会保留旧索引。单文件读取失败保留旧值并让路径进入下一轮重试。
- **分片进度**：增量构建沿用 `ScanBudget`，每次推进发布 `code-context.index.progress` 的
  结构化 Footer 事件；完成后持久化文件哈希与 manifest，新一轮事件不会被当前构建清掉。

Key files:
- `examples/plugins/code-context-plugin/src/incremental.rs` — 写入工具识别、路径提取与边界校验
- `.../src/index/build.rs` — Full / Incremental 构建模式、分片推进与删除策略
- `.../src/index/mod.rs` — 文件级 chunk diff 与原子 upsert
- `.../src/retrieval/{symbol,lexical,mod}.rs` — 双路索引删除与 posting 重映射
- `.../src/repository/mod.rs` — 目录快照集合、过滤读取与完整性判断
- `.../src/lib.rs`、`plugin.toml` — 生命周期接入、事件订阅与 Footer 进度

Tests：覆盖增量 added / modified / deleted / unchanged、事件幂等与失败隔离、路径安全、检索
清理、预算分片和 Footer 进度；插件单元测试总数以本次构建输出为准，全部通过。

Known limitations:
- 外部编辑器或 `git checkout` 没有事件时，仍依赖 Phase 10 命中结果的惰性一致性校验；统一
  `filesystem.watch()` 仍待 Host 的 Optional 能力。
- 结构索引本体仍驻留内存，进程重启后需要全量重建；持久化哈希表只作为元数据基线。
- `BUILD_BUDGET` 与增量统计的真实吞吐仍需 Phase 14 benchmark 校准。

Next: Phase 12 — Local Embedding Provider（BLOCKED）

---

### Phase 7 — API Embedding Provider

Status: COMPLETE
Completed Date: 2026-09-03

Summary：本阶段原被列为 BLOCKED（Host 未实现插件 HTTP 能力，且在加载期主动拒绝该声明）。
本轮**先实现了原 Blocker 记录里的推荐方案 1**——通用宿主受限 HTTP 能力——随后 Phase 7 落地。

Implemented（Host 侧，`agent-plugin-host` / `lucia`）:
- **`host-http-request` 能力**：`wit` import、SDK 类型与宏接线、manifest
  `[capabilities.http] allow_hosts`、audit 快照、linker 注册、capability 运行时。
- **三道闸门**：manifest allowlist（宿主注入的 base URL 必须落在其中，使声明的授权边界
  始终真实）、执行策略 `permits_network_access`（Profile 身份拥有最终否决权）、
  请求整形（方法白名单排除 `CONNECT`/`TRACE`、路径不得逃逸 base URL、凭证类 header
  不接受 Guest 覆盖、请求与响应体积上限、超时、**不跟随重定向**）。
- **预算**：`ResourceLimits` 新增 `max_plugin_http_requests` 与
  `max_plugin_http_request_bytes`，与模型完成分开计量——嵌入是高频小请求，
  与低频大请求共用账本会让任意一方的上限失去意义。
- **应用配置**（`[plugins.<id>]`）：通用 `http_base_url` / `http_api_key` /
  `http_api_key_env` / `http_auth_header` / `http_auth_prefix` / 超时与体积上限。
  凭证解析后直接交给 Host，**不进入激活元数据，因此不到达 Guest**。
- 宿主把 `base_url` 的摘要作为 `http_endpoint_id` 注入激活元数据，供插件判定端点是否更换。

Implemented（插件侧）:
- **`ApiEmbeddingProvider`**：只提交相对路径 `embeddings` 与请求体，看不到地址也拿不到密钥。
- **按 `index` 字段重排返回向量**：接口不保证返回顺序，而向量与 chunk 按下标对齐，
  顺序错了不报错，只会让检索结果长期缓慢地不正确。
- 非 2xx 回传状态码与端点说明；`health_check` 覆盖默认实现，真的打一次探测请求。
- 嵌入空间纳入端点摘要：换端点即失效，触发语义索引重建。

Tests：Host 侧新增 11 项，插件侧新增 11 项 provider 测试与 4 项配置测试。

Important decisions:
1. **凭证由 Host 注入，不进 Guest。** 沿用 `host-model-complete` 范式与
   `adr-api-embedding.md` 的结论，未采用被否决的 `process_exec + curl`。
2. **不跟随重定向。** 跟随会让 allowlist 内的主机把请求引到 allowlist 外，
   把前两道闸门一并绕过。
3. **配置键用通用 `http_` 前缀而非嵌入语义。** 宿主能力是通用 HTTP，应用层不应承载
   单个插件的业务概念。

**Known cost —— `code_search` 的生效 scope 从 `read` 变为 `unknown`**：
本插件同时持有 `workspace_read`（贡献 `read`）与 `http`（贡献 `network`），
`merge_tool_permission_scopes` 对两个不相等且都不是 `control` 的 scope 返回 `unknown`。
后果是 `code_search` 进入最保守的审批档位，且不再参与 `run.rs` 的只读并行批处理。
这是启用 API 语义检索的直接代价，已在 `plugin.toml` 与冒烟测试中如实记录。
不需要语义检索的部署删掉 `[capabilities.http]` 段即可回落到 `read`。

Deferred:
- 原 Blocker 记录的方案 3（拆独立 embedding provider 插件，让网络权限留在对方 manifest 上，
  使 `code_search` 保持 `read`）**仍然可行且是消除上述代价的唯一途径**，留待需要时实施。

Next: Phase 8 — Semantic Vector Index

---

### Phase 8 — Semantic Vector Index

Status: COMPLETE
Completed Date: 2026-09-03

Summary：本阶段原被列为 BLOCKED，理由是「无可用 embedding provider」与「现有存储装不下向量」。
前者随 Phase 7 解除；后者被重新界定：`adr-storage.md` 把向量列为「V1 不存在」，
**说的是持久化而不是存在**。结构索引、符号表与倒排索引同样装不下 8 MiB 存储，
它们的处理方式是内存驻留、进程启动后按需重建。向量走同一条路，语义检索因此在 V1 成立。

Implemented:
- **`SemanticIndex`**：chunk 向量的内存驻留存储，支持 upsert / `contains_current` /
  `retain_current` / `remove_file` / 按嵌入空间重置与兼容性判断。
- **线性精确扫描而非 ANN**：代码仓库 chunk 在数千到数万量级，此时线性扫描常数更小，
  且结果精确、行为确定——近似索引会让同一查询在索引重建后给出不同结果，
  破坏检索质量的可回归性。
- **写入时归一化**，检索用点积代替余弦。零向量与含 `NaN`/`Inf` 的向量直接拒绝：
  一个 `NaN` 会让它参与的全部比较返回 `false`，排序变成未定义且不报错。
- **维度不符时返回空**而不是给出无意义的排名。
- **接入 RRF**：`search_hybrid` 在查询向量缺席、语义索引未就绪或维度不符时自动退化为
  两路，结果与原 `search` 完全一致。Phase 9 的融合层无需任何改动就接住了第三路通道——
  「任意通道子集」的设计在这里兑现。
- **端到端链路**：结构索引就绪后建立语义索引（只嵌入尚无向量的 chunk，
  全量重嵌在 API 模式下等于重新付费）；检索时嵌入查询；语义索引未就绪时不发起请求；
  嵌入失败降级为两路并记录原因；索引状态据此区分 `Ready` 与 `Degraded`。
- 送去嵌入的是「路径 + 限定名 + 签名 + 文档注释」而非源码正文：正文不驻留内存，
  且模型对「这段代码做什么」最敏感的信号就在签名与文档注释里。

Tests：新增 14 项语义索引测试、5 项通道接线测试、5 项端到端测试。插件单测 301 → 324。

**WASM 向量持久化已实现（int4 量化）**：f32 向量以单条 scale factor 加四位有符号分量打包，
两个维度共用一个字节，base64 编码后存入 `host-store` 的 `index.semantic` 键。快照不再按固定
条数截断，而是按 7 MiB 实际 JSON 字节预算保留记录，给 8 MiB Host KV 中的 manifest 与
文件哈希表留出空间。进程重启时先按 `space_id` 校验嵌入空间，再按 chunk 内容哈希校验每条
向量；旧 int8 快照仍可读取，但因缺少内容哈希会安全地重新嵌入一次。删除、重命名和正文变化
产生的旧记录会在写回快照前清理。

原生 `search_repository` 已改用 sqlite-vec：同一仓库的 manifest 与文件哈希仍保存在
`~/.lucia/code-context/<仓库哈希>.json`，归一化 f32 向量保存在同名 `.sqlite3` 数据库并由
`vec0` 执行余弦 KNN。数据库重开、路径范围过滤、嵌入空间隔离、批量事务、增量删除和旧 JSON
int4 快照迁移均有离线回归；迁移成功后会删除 JSON 中的重复向量。真实 SiliconFlow 全仓库
二次复用仍需在有凭证环境中运行两次命令确认。

Next: Phase 13 — Agent Tool Integration

---

### Phase 13 — Agent Tool Integration

Status: COMPLETE
Completed Date: 2026-09-04

Summary：把混合检索 + RRF 融合 + 上下文打包接入 `code_search` 工具返回值。此前工具报告
"候选融合与上下文打包尚未接入"；现在返回带代码片段、符号元数据与定位信息的成功结果。

Implemented:
- **`tool::search_result`**：构造检索成功时的 `ToolResult`。`content`（发送给模型）包含
  结构化 JSON：`status`、`results[]`（每条含 `file`、`lines`、`language`、`snippet`、
  `symbol`、`qualified_symbol`、`kind`、`signature`，以及可选的 `truncated` / `relocated`）。
  `details`（仅供 UI）包含构建统计（候选数、返回数、抑制 / 合并 / 预算 / 多样性丢弃数、
  重定位数、读取文件数、估算 token 数）与索引规模。
- **`search()` 方法重写**：索引就绪后调用 `search_results`（混合检索 + 查询嵌入）→
  `context::build`（去重、父子抑制、区间合并、多样性约束、一致性校验、token 预算打包）→
  `tool::search_result`。**不再返回 unavailable_result**。
- **多路径范围支持**：底层检索只接受单前缀；单前缀直传、多前缀时不传 scope 后置过滤取并集。
- **降级信息对模型可见**：语义通道不可用时 `content.note` 附带降级原因。

Key files:
- `examples/plugins/code-context-plugin/src/tool.rs` — 新增 `search_result`
- `.../src/lib.rs` — `search()` 方法接入真实检索管线

Tests：新增 1 个 `search_returns_context_entries_when_index_is_ready`，修改 1 个
`search_on_empty_repo_returns_success_with_no_results`。插件单测 324 → 325。

覆盖：有内容仓库返回包含代码片段的成功结果（验证 file / lines / snippet 字段与构建统计），
空仓库返回成功但结果为空且状态为 degraded。

Tests Executed / Result:
- `cargo test -p code-context-plugin`：325 单元测试 + 1 文档测试 + 1 集成测试，全通过。

Important decisions:
1. **索引就绪后一律返回成功结果，包括结果为空时。** 此前返回错误是因为检索未实现——
   模型不应把"工具坏了"当作"仓库里没有相关代码"。现在检索已实现，空结果如实反映
   "没有匹配"，模型可据此调整查询策略或改用其他工具。
2. **`content` 与 `details` 严格分离。** 模型看到定位信息与代码片段，不看到融合诊断与统计——
   那些是开发者调参和 Phase 14 benchmark 的输入，暴露给模型只增加噪声。
3. **SymbolKind 用 serde 序列化。** `#[serde(rename_all = "snake_case")]` 保证稳定的机器可读名。

Known limitations:
- **path_scope 多前缀时走后置过滤。** 底层检索只支持单前缀，多前缀时先搜全局再过滤，
  可能在 limit 较小时错过一些结果。改为底层支持多前缀需改动 retrieval 层，留给 Phase 15。
- **未测试 token 预算是否准确。** `estimate_tokens` 的经验系数未经真实 tokenizer 校准，
  Phase 14 必须验证。

Next: Phase 14 — Retrieval Benchmark

---

### Phase 14 — Retrieval Benchmark

Status: COMPLETE
Completed Date: 2026-09-04

Summary：建立 fixture 与完整 Lucia 仓库两级检索基准，覆盖独立标识符、中文自然语言、
中英混合查询、路径相关查询、符号 / BM25 / 语义融合和 token 预算。调优后全部查询都能在
规定候选范围内找到目标文件。

Implemented:
- **查询侧中文降噪**：索引继续保留单字与双字词元；多字中文查询只让双字词元参与 BM25，
  避免“化”“引”等高频单字主导排序，独立单字查询保持可用。
- **按可用通道调整符号权重**：自然语言中的普通精确符号保持弱提示；有语义通道时全面压低
  符号噪声，缺少语义通道时让前缀 / 后缀符号恢复标准权重。这样“一个agent对话”优先返回
  TUI 对话框实现，“索引 manifest 持久化”同时能命中 `storage.rs`。
- **失败诊断**：真实仓库基准失败时打印 top-20 分数、各通道排名和目标文件原始候选排名，
  后续调参可以直接定位召回还是融合问题。
- **容量回归**：6500 条 1024 维 int4 向量及真实长度的 chunk ID、内容哈希和路径可完整装入
  7 MiB 语义快照预算，不触发截断。

Benchmark Result:
- fixture：Recall@1 `0.781`，Recall@5 `0.969`，MRR `0.906`。
- 完整仓库：344 个输入文件、343 个已索引文件、6539 个代码块、6266 个符号，16 个预算
  轮次完成；Recall@1 `0.600`、Recall@5 `0.933`、Recall@10 `0.933`、MRR `0.756`。
- 真实 SiliconFlow 查询未在本次会话重跑：当前进程没有 `SILICONFLOW_API_KEY`。离线端到端测试
  已覆盖三路融合与跨进程向量复用，不能把它替代为真实服务验收。

Tests Executed / Result:
- `cargo test -p code-context-plugin --lib`：338 项通过。
- `cargo test -p code-context-plugin --test retrieval_benchmark -- --nocapture`：9 项通过。
- `cargo test -p code-context-plugin --example search_repository --features test-support`：7 项通过。

Next: Phase 15 — Hardening

---

### Phase 15 — Hardening

Status: IN PROGRESS

Summary：32 条真实查询的消融表明，符号 + 词法 + 语义已把 Recall@10 提升到 `0.938`，但中文
行为描述的 Recall@1 在加入语义后仍为 `0.12`，加入 `Qwen/Qwen3-Reranker-4B` 后为 `0.38`。
候选池已经覆盖目标，当前瓶颈是大量三五行代码块在嵌入空间里区分度不足，而不是融合权重。

Implemented:
- **2026-09-05 函数定位加固**：自然语言的词法与语义候选各取至少 120 条，通常最多 200 条；
  输出条数不再压缩召回深度。启用模型重排时保留至少 40 个候选，正文按 1800 字符限额提交，
  同文件一次回读后释放全文。自然语言按模型相关度排序并保留测试意图约束；精确标识符仍
  跳过网络，混合查询仍使用条件重排与有界保护。下方 Phase 4/5 的自然语言保护规则是历史
  记录，以本条为准。工具协议、索引 schema 与嵌入模板均未变更。
- **语义专用复合块**：同一父节点、owner、测试属性与叶子类别下，相邻的短兄弟声明可合成
  复合块。单成员最多 6 行，间隔最多 2 个空白行，复合块最多 40 行和 1200 个字符；少于两个
  成员不生成。
- **保留精确检索粒度**：原子块继续进入符号表与 BM25；复合块只进入语义通道和模型重排。
  原子成员通过 `semantic_group_id` 记录归属，不再重复生成向量。
- **补充结构上下文**：嵌入文档现在包含路径、文件级文档、父符号、限定名、签名、块级文档、
  最多 16 条 / 600 字符的 import 上下文，以及最多 1200 字符的正文。
- **兼容性版本更新**：`STRUCTURAL_INDEX_VERSION` 从 1 提升到 2，触发结构索引全量重建；
  `document_template_version` 从 1 提升到 2，触发语义向量重建。
- **业务式验收面板**：在 32 条标准查询之外增加 8 类不含目标符号名的中文行为描述，覆盖
  实体定义、页面组件、鉴权过滤、状态下钻、选择器、工具函数、控制器与刷新接口。每条查询
  重复运行 3 次，记录完整 Top-10 chunk 身份序列、Top-1 一致率与全序一致率。
- **实现正文判据**：目标路径、目标符号与非空源码片段必须同时出现才算实现本体命中；另按
  目标定义的实际行区间计算覆盖率，并区分完整实现与被截断或缺失的部分实现。
- **延迟与能力边界**：报告检索加上下文打包的平均、P50、P95 延迟、平均返回条目、token 与
  行覆盖；调用链和影响面明确标为不支持，不把源码片段伪装成符号图谱能力。
- **评测污染隔离**：真实仓库报告排除 `retrieval_report.rs`、`retrieval_benchmark.rs`、本进度
  文件和生成报告，防止查询文本、目标符号与既有结果被同一个索引读回并造成指标虚高。
- **嵌入请求限速与重试**：相邻请求默认至少间隔 1500 ms；单批 429/5xx 默认重试 4 次，
  退避基数 5000 ms，支持整数秒 `Retry-After`，单次等待最多 30 秒。三项均可配置并允许设为 0。
- **429 专用可续状态**：持续限流不再累计普通瞬时故障的停滞轮次，也不会转成终态
  `Degraded`；成功前缀立即写入语义索引，端点恢复后只补剩余向量。
- **真实消融跨进程缓存**：`retrieval_report` 的真实 API 场景使用独立 sqlite-vec 数据库，
  与正式搜索缓存隔离。进程在 5760/6600 等部分进度中断后，重跑会复用已有向量继续建立。
- **真实测试凭证收口**：`siliconflow_e2e` 不再保存源码内密钥，只从宿主进程的
  `SILICONFLOW_API_KEY` 环境变量读取，凭证不进入插件元数据或持久化文件。
- **候选阶段诊断与 union 保底**：混合检索现在记录 symbol、lexical、semantic、去重 union、
  post-fusion 全序和 final Top-K 六个阶段的 chunk 身份。语义通道仍是对分路候选的纯加法，
  新增回归测试断言开启语义后原有符号与词法候选不会在 candidate generation 阶段消失；
  后续 Recall 下降可明确归因到召回器、union、融合排序或最终截断。Phase 1 验证：
  `cargo test -p code-context-plugin retrieval::tests --lib`，32 项通过。
- **确定性 Query Analyzer 与 SearchPlan**：查询按 `Identifier`、`QualifiedSymbol`、
  `NaturalLanguage`、`Mixed` 分类，并附加定义、实现、调用点、类型、行为、概念六类轻量目标。
  SearchPlan 为三路召回分配独立配额；精确标识符与限定名跳过查询嵌入和模型重排，自然语言
  与混合查询才启用语义候选。判断只使用字符形态和通用意图词，不调用 LLM，也不绑定路径
  或 benchmark 题目。Phase 2 验证：Query Analyzer 4 项、fusion 31 项、Fast Path 端到端
  1 项全部通过，`cargo check -p code-context-plugin` 通过。
- **ChunkRole 与角色感知融合**：不修改持久化 schema，直接从符号种类、文件扩展名与测试标记
  推导 `Definition`、`Implementation`、`CallSite`、`TypeDefinition`、`Documentation`、`Test`、
  `Config`、`Other`。融合层按 QueryGoal 提升定义、类型或实现角色；实现、行为与调用点查询会
  显式压低说明性文档，概念查询仍允许文档参与。最终同分决胜固定为分数、角色、路径、符号、
  chunk ID，避免集合遍历顺序造成漂移。当前没有 references graph，`CallSite` 只保留协议角色，
  不伪造调用链结果。Phase 3 验证：角色推导 2 项、fusion 32 项、Context 24 项、rerank 5 项
  全部通过。
- **强命中保护与有界模型增量**：远端相关度不再覆盖本地融合分，而是按本地/模型名次差施加
  最多 0.75 个 rank-1 当量的有界增量。最强限定名精确命中保持 Top-1，最强精确短名保持
  Top-3，强 lexical + implementation 候选保持 Top-5；同名重载只保护本地最强候选，避免多个
  候选争抢同一位置。每条结果记录模型原始相关度与实际增量，便于归因。Phase 4 验证：
  rerank 7 项全部通过，`cargo check -p code-context-plugin` 通过。
- **可解释置信度与 Conditional Rerank**：本地排序根据精确符号、多路一致、目标角色、Top-1
  分差和角色冲突计算高低置信度。只有自然语言或混合查询、候选多于一条且置信度低时才调用
  远端模型；精确查询、高置信度定义/实现和通道一致结果直接返回。执行诊断记录 QueryIntent、
  SearchPlan、候选阶段、置信度、重排是否尝试/应用及 Fast/Normal/Hard 路径。Phase 5 验证：
  Fast/Normal/Hard 路径端到端各 1 项、rerank 9 项与 sqlite-vec 重启语义检索 1 项通过；
  `cargo test -p code-context-plugin --lib` 共 386 项通过，`cargo check -p code-context-plugin`
  通过。
- **Symbol Resolution 与 Body Hydration**：Context Builder 在父子抑制和预算打包前，按文件内
  限定名或唯一短名把检索代码块解析为完整符号范围；文件变化时继续用重解析结果定位当前
  符号。`ContextEntry` 与工具输出同时保留 `retrieval_lines`、`hydrated_lines` 和实际返回
  `lines`，并携带 `role` 与明确截断标记。单条默认上限从 120 行提高到 240 行，超过上限的
  大符号仍保留完整目标范围并标记截断。Phase 6 验证：Context 25 项、插件库 387 项通过，
  真实仓库离线报告 6 项通过、3 项真实 API 报告按设计忽略。

Phase 6 Real Repository Result（2026-09-04）:
- 8 条业务式查询目标文件进入 Top-10 从 `7/8` 提升到 `8/8`，定义、实现本体与完整实现覆盖
  均为 `5/8`；`render_native_plugin_dialog` 的正文覆盖从约 `55%` 提升到 `100%`。
- 检索加上下文打包平均 `145.883 ms`、P50 `149.171 ms`、P95 `184.820 ms`，平均上下文
  估算 token 为 `4636`，未进入 2 秒级默认路径。
- `retrieval_benchmark` 仍有 2 条排序断言失败：合成语料的 `embedding_space_id` 查询未同时
  保留 storage 目标，真实仓库的“增量索引 脏路径”候选在 lexical 原始列表中但被融合挤出
  Top-20。两项属于 Phase 7 的 fusion/benchmark 问题，不属于正文回填，本阶段不改评测答案。

Scale Result（Lucia 当前仓库）:
- 原子代码块 6641 个，语义复合块 262 个，其中 970 个原子块由复合块代表。
- 实际语义文档 5933 个，比逐原子块嵌入减少 708 个，约 `10.7%`。

Offline Acceptance Result（2026-09-04，本机单次报告）:
- 排除评测答案后的语料为 341 个文件、6825 个代码块、6311 个符号。
- 32 条标准查询：Recall@1 `0.625`、Recall@5 `0.875`、Recall@10 `0.938`、MRR `0.710`，
  定义命中 `19/19`。
- 8 条业务式查询：目标文件进入 Top-10 `7/8`，定义命中 `4/8`，实现本体命中 `4/8`，
  完整实现覆盖 `3/8`；所有查询都返回了有效代码片段。
- 同一查询 3 轮运行的 Top-1 与完整 Top-10 顺序一致率均为 `8/8`。本机端到端延迟平均
  `140.680 ms`、P50 `144.568 ms`、P95 `175.503 ms`；延迟只代表本次机器与离线配置，
  不作为跨机器固定门槛。
- 八类结果证明当前主要差距仍在排序与实现区分：候选召回 `87.5%`，实现本体只命中 `50.0%`。

Verified:
- `cargo test --offline -p code-context-plugin --lib`：371 项通过。
- `cargo test -p code-context-plugin --test retrieval_benchmark -- --nocapture`：9 项通过。
- `cargo test -p code-context-plugin --features test-support --test retrieval_report -- --nocapture`：
  6 项离线报告通过，3 项真实 API 报告按设计忽略。
- `cargo test -p code-context-plugin --example search_repository --features test-support`：11 项通过。
- `cargo clippy -p code-context-plugin --all-targets`：退出成功；四个语言适配器仍有 4 条既有
  `field_reassign_with_default` 警告，本轮未做无关重构。
- `bun run build:plugin:code-context`：`wasm32-wasip2` release 构建通过。
- `cargo test -p code-context-plugin-smoke-tests`：9 项真实 WASM Host smoke 通过。
- `cargo test --offline -p code-context-plugin --test siliconflow_e2e`：1 项磁盘持久化测试通过，
  4 项真实网络测试按设计忽略；测试宿主已通过编译且不再包含源码凭证。
- 当前仓库目标查询基线：以纯词法、无缓存模式运行“app用户下单接口实现”，完成 344 个文件、
  6980 个代码块索引，Top-10 没有下单实现；Lucia 仓库不存在正样本，只用于确认工具链行为。
- 上述数字是结构规模与离线回归结果，**不是新策略的真实模型质量提升证明**。

Pending:
- 八条业务查询与 Agent 场景已在 2026-09-05 使用真实模型复测；完整 32 条逐通道消融仍待重跑，
  不把八条业务查询的改善外推到整个查询集。继续复用真实响应缓存，凭证只通过宿主环境传入。
- 最终查询 `search_code("app用户下单接口实现")` 需要在真实包含 App 下单业务的仓库上验收；
  Lucia 仓库不存在该业务实现，不能通过添加伪造源码制造正样本。当前工具正式名称仍为
  `code_search`，`search_code(...)` 只作为目标调用写法，是否增加兼容别名需单独确定协议需求。
- 继续验证超大仓库、fuel、内存与宿主存储配额边界。

## Blocked Phases

一个阶段因 Host 能力缺失无法实现。按开发任务书 §25，记录 Blocker 而**不绕开架构做 hack 实现**。

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

Phase 15 — Hardening。八类业务式函数定位已经真实模型与工具入口验收，继续扩充独立查询集
并完成全量逐通道消融；取得真实 App 仓库后验收“app 用户下单接口实现”。随后继续验证
超大仓库、fuel、内存和宿主存储配额边界。

## Known Issues

八条业务查询的 Top-1 已改善，但仍有两个目标未排第一；完整 32 条消融与跨仓库验证尚未完成。
源码重排增加自然语言查询的网络延迟，响应缓存命中时的耗时不能代表生产端点延迟。

Phase 2 遗留的真实 Host 验证已在 Phase 4 补齐：
`wasm_host_scans_nested_directories_and_builds_index` 覆盖三层嵌套目录、多语言解析与忽略规则。

## Architecture Drift

### 2026-09-04 · 原生搜索入口采用 sqlite-vec

**偏离内容。** 原存储 ADR 拒绝 SQLite，并把语义向量统一保存为 Host JSON int4 快照。该结论
对 WASM Guest 仍然成立；独立运行的原生 `search_repository` 不受 Guest 文件能力限制，现改用
bundled SQLite + sqlite-vec 保存 f32 向量并执行余弦 KNN。

**边界。** WASM 构建默认不启用 `sqlite-vector-store` feature，插件 manifest 没有增加
`fs_write`。SQLite 路径由原生入口固定派生为 `~/.lucia/code-context/<仓库哈希>.sqlite3`，不是
Guest 可控制的通用文件接口。旧 JSON int4 快照保留为 WASM 后端和原生入口的一次性迁移来源。

**验证。** sqlite-vec 重开、KNN、路径范围、嵌入空间变化清库、查询失败内存降级与旧快照迁移
测试通过；348 项插件库测试、11 项搜索命令测试和 8 项真实 WASM Host smoke test 通过；
`cargo clippy --all-targets --features test-support -D warnings` 无警告；默认 `wasm32-wasip2`
release 构建通过且正常依赖树不含 SQLite。真实 SiliconFlow 缓存迁移未运行：当前进程未设置
`SILICONFLOW_API_KEY`，现有旧 JSON 缓存保持未改动。

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

### 2026-09-03 · Phase 3 · 结构化解析选型修订（重大）

**偏离内容。** `architecture.md` 第 12 问与 `implementation-plan.md` Phase 3 规定
"Tier 2 纯 Rust 结构化解析，按 `LanguageAdapter` 自行实现"。实际实现改为
**一律使用现成的纯 Rust 解析器**（`syn` / `oxc_parser` / `rustpython-parser` / `gosyn`），
取消自建启发式解析层。

**原因。** 原论证——"本机无 wasi-sdk、`clang` 无 wasm 后端、构建 `--offline`"——
**只对 Tree-sitter 成立**（它的运行时与语法都是 C），却被外推到了所有第三方解析器。
纯 Rust 解析器走 `rustc` 的 `wasm32-wasip2` 后端，与 C 工具链无关。

**验证。** 依赖可获取、`cargo build --target wasm32-wasip2` 通过、四种语言实际解析与位置信息
正确、体积 592 KB → 3.4 MB。

**修订后的层级模型。**

```
Tier 1  现成解析器的完整 AST   —— Rust / TS / JS / Python / Go
Tier 2  （取消）              —— 不再自建启发式解析
Tier 3  文本分块              —— 未覆盖语言、解析失败、无声明的文件
```

**已同步文档。** 新增 [adr-structural-parsing.md](./adr-structural-parsing.md) 记录完整论证。
`adr-storage.md` 中"SQLite 无法编译"的论证不受影响——那确实是 C 库；本次只修正了把该论证
外推到解析器选型的部分。Tree-sitter 仍不使用，理由不变。

## Deferred Work

以下属于 V1 明确不做，非阻塞项，不得 scope creep：

- Full LSP（Go to Definition / Find References / Types / Call Hierarchy）。架构已为未来接入预留：
  `CodeChunk` 保留 `qualified_symbol`、`symbol_kind`、`parent_symbol` 与精确行区间，
  LSP 将来作为**独立插件**通过 `host-service-call` 提供。
- Code Graph、Git history 语义索引、跨仓库 / 团队 / 分布式索引、云端托管索引。
- LLM query rewrite。远端 reranker 已由原生搜索入口实现，默认模型为
  `Qwen/Qwen3-Reranker-4B`；WASM Guest 仍只通过受限 Host HTTP 能力调用模型端点。
- Tier 1 完整 AST（tree-sitter）：本机无 wasi-sdk，`clang` 注册目标中无 wasm 后端，构建流水线
  使用 `--offline`。V1 用 Tier 2 纯 Rust 解析 + Tier 3 文本兜底。作为后续独立 ADR。
- 文件 watcher：V1 用 `core.tool.result` + 惰性校验兜底。若将来实现，必须是宿主统一的仓库变更
  事件，不是 ACE 专属 watcher。
- Host 侧 Recommended 改动（blob 存储、HTTP、后台任务 handle、`FileEntry` 加 size/mtime、
  按插件资源上限可配置）：属于独立 Host 能力提案，不在本插件交付范围。

## Next Action

Phase 15 正在进行：已使用真实 `BAAI/bge-m3` + `Qwen/Qwen3-Reranker-4B` 完成本仓库函数定位
与工具位置验收。下一步扩大独立业务查询集、继续评估 Top-1 与远端延迟，并在真实 App 仓库
验收“app 用户下单接口实现”；不能用 Lucia 仓库替代不存在的下单业务正样本。
Phase 12 — Local Embedding Provider 仍受 Host embedding 能力缺失阻塞（跳过）。

---

已完成的 Phase 4 原始计划（保留供追溯）：

1. 把 Phase 2 的扫描器与 Phase 3 的解析器接到一条索引流水线上，复用 Phase 2 已冻结的
   `ScanBudget` 分片游标形态，**不发明第二套**。
2. 实现 chunk / symbol / 文件哈希的内存索引结构，支持 insert / update / delete / get / list
   与仓库隔离。
3. 按 [adr-storage.md](./adr-storage.md) 分流持久化：`IndexManifest` 与
   路径→内容哈希表落 Host JSON KV（`storage.rs` 已预留 `index.` 键前缀）；
   **chunk 正文、符号表与倒排索引一律内存驻留**，8 MiB JSON 装不下。
   不得因为方便就把索引本体塞进 KV。
4. 用 `content_hash` + Phase 3 的稳定 chunk ID 判断内容变化，为 Phase 11 增量索引铺路。
5. 冷启动路径：读 manifest → 读哈希表 → 按需重建内存索引（懒构建，不在 `activate` 里做）。
6. **补 Phase 2 遗留的真实 Host 验证**：扫描器的子目录遍历尚未在真实 Host 上跑过，
   本阶段接入后必须补一个真实 Host 的子目录扫描 smoke test。

Definition of Done：可以 index repository → restart plugin → restore structural index
并正确查询数据；扫描 + 解析全链在真实 Host 上验证通过。
