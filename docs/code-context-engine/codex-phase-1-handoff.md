# Codex Phase 1 Handoff

更新日期：2026-09-03

## Plugin Path

- Guest：`examples/plugins/code-context-plugin`
- Manifest：`examples/plugins/code-context-plugin/plugin.toml`
- Host smoke tests：`examples/plugins/code-context-plugin/smoke-tests/tests/code_context.rs`
- 设计文档：`docs/code-context-engine/`

## Build Commands

```bash
cargo test --offline -p code-context-plugin
bun run build:plugin:code-context
cargo test --offline -p code-context-plugin-smoke-tests
```

Host 边界变更后的回归命令：

```bash
cargo test --offline -p agent-plugin-host
cargo test --offline -p agent-plugin-manager
cargo test --offline -p lucia --features plugins
```

## Important Module Paths

- `src/lib.rs`：`CodeContextPlugin`、activate/detach/shutdown、工具注册
- `src/config.rs`：`CodeContextConfig`、`EmbeddingMode`
- `src/error.rs`：`CodeContextError`
- `src/model.rs`：仓库身份、状态、manifest、chunk 与 symbol 领域类型
- `src/storage.rs`：`IndexStore`、`ManifestLoad`
- `src/embedding.rs`：`EmbeddingProvider`
- `src/tool.rs`：`CodeSearchRequest`、`code_search` schema 与不可用结果

## Public Interfaces

- `RepositoryIdentity::from_workspace_root`
- `IndexStatus` 与 `IndexStatus::can_serve_search`
- `IndexManifest::{new, validate, compatibility, touch}`
- `ManifestCompatibility::{Compatible, SemanticRebuildRequired, FullRebuildRequired}`
- `CodeChunk`、`Symbol`、`SymbolKind`
- `EmbeddingProvider::{space, embed}`
- `CodeSearchRequest::parse`

不要把存储后端 ID、SQLite row ID、向量库 ID 或 parser 内部状态加入领域类型。

## Storage API

`IndexStore` 只使用 Host `store` 的 `PluginStoreScope::Workspace`：

- 键：`index.manifest`
- `load_manifest`：区分 Missing / Loaded / Unreadable
- `save_manifest`：写前校验
- `clear_manifest`：为未来全量重建准备

Host 当前限制为 JSON KV、小容量、全量重写。下一阶段只允许增加受控的文件哈希等小型 metadata，
不得把 chunk 正文、倒排索引或向量放入该存储。

## Repository Lifecycle

1. Host 启动插件并在激活元数据中注入规范工作区路径，同时通过 `with_workspace_root` 注入可信能力根。
2. `activate` 解析配置，进入 `Initializing`，调用 `list_dir(".")` 进行最小能力探测。
3. 插件派生仓库身份，探测 Workspace storage，恢复并校验 `IndexManifest`，设置 `IndexStatus`。
4. 能力与配置可用时动态注册 `code_search`，随后发布 `code-context.status`。
5. `deactivate` 注销动态工具、flush 未提交 metadata、解除仓库附着并回到 `NotIndexed`。

Host 当前没有 repository open/change/close 事件，也没有 Guest 后台任务或取消 handle。不得在
`activate` 中执行全量扫描或解析。

## Relevant Host Capabilities

- `workspace_read = true`：复用 `host-fs-read` / `host-fs-list`，Host 强制当前项目根边界。
- `storage = true`：Workspace 作用域按项目隔离并跨插件版本保留。
- `tool_scopes.code_search = "read"`：Host 权威绑定最终工具 scope。
- 动态工具 API：`upsert_tool` 返回公开名；卸载必须使用该返回值调用 `remove_tool`。

## Codex MAY MODIFY

- `examples/plugins/code-context-plugin/src/repository/`
- `examples/plugins/code-context-plugin/src/parser/`
- `examples/plugins/code-context-plugin/src/index/structural/`
- 为上述模块增加的 `examples/plugins/code-context-plugin/src/*.rs` 模块入口
- `examples/plugins/code-context-plugin/tests/structural_*`
- 与 Structural Index 直接对应的插件单元测试和 fixture

目录当前尚未创建。只在有真实实现时创建，不要预建空模块。

## Codex MUST NOT MODIFY WITHOUT REVIEW

- `crates/agent-plugins/**` Host、ABI、capability 与 manifest 解析
- `crates/agent-tui/src/plugin/**`
- `wit/plugin.wit` 及 Guest 内嵌 WIT
- `examples/plugins/code-context-plugin/plugin.toml`
- `EmbeddingProvider` 接口与 `EmbeddingSpace` 语义
- `IndexStore` backend abstraction 与 Host storage 作用域
- `CodeContextPlugin` lifecycle、动态工具注册和 `code_search` 外部 schema
- `INDEX_SCHEMA_VERSION`、`STRUCTURAL_INDEX_VERSION`，除非有明确格式迁移评审

## Structural Index Phase Requirements

下一阶段以 `implementation-plan.md` 的 Phase 2 与 Phase 3 为准：

- Repository Scanner：基于 `list_dir` 的有界递归、稳定顺序、路径去重和错误隔离
- Ignore Rules：`.gitignore` 与内置忽略目录，不实现文件 watcher
- Language Detection：按路径与扩展名识别，未知语言进入文本兜底
- Structural Parsing：纯 Rust Tier 2 adapter，不引入 Tree-sitter 或 C 工具链
- AST-aware Chunking：以语言结构为边界，解析失败降级为 Tier 3 文本分块
- Symbol Extraction：生成现有 `Symbol` 与 `CodeChunk`，不嵌入存储 backend 字段
- Tests：覆盖越界、循环或深目录、忽略规则、未知语言、语法错误、超大文件和确定性输出

下一阶段不得实现 Embedding、向量搜索、BM25、RRF、Context Packing、Local/API provider、LSP、
Code Graph、Git indexing、Reranker 或完整增量索引。
