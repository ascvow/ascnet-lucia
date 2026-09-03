# Code Context Engine 实现状态

更新日期：2026-09-03

## Phase

Phase 1：WASM Plugin 初始化与基础架构 Skeleton。

## Status

实现已完成，最终构建与集成验证进行中。

## Implemented

- `examples/plugins/code-context-plugin` 独立 Guest crate、manifest 与 smoke-test crate
- `wasm32-wasip2` / ABI `0.10.0` 构建配置
- 通用 Host `workspace_read` capability、可信工作区根注入、权限审查和路径越界防护
- `storage` Workspace 作用域中的 `IndexManifest` 读写、校验、兼容性判断和重启恢复
- `NotIndexed`、`Initializing`、`Indexing`、`Ready`、`Degraded`、`NeedsSemanticRebuild`、
  `NeedsFullRebuild`、`Error` 状态模型
- `enabled`、`embedding_mode = local | api` 配置解析与校验
- `EmbeddingProvider` 稳定接口和不会返回伪造向量的 `UnconfiguredEmbeddingProvider`
- 动态注册 `code_search(query, max_results?, token_budget?, path_scope?)`
- 卸载时按 Host 返回的公开工具名注销工具、flush metadata 并解除仓库附着
- 单元测试与真实 WASM Host smoke 测试

## Deferred

- Repository Scanner、ignore rules、语言检测
- Tree-sitter 与完整 AST parser
- 结构化分块、符号索引、词法索引和完整 indexing
- Embedding API、本地模型、向量索引和持久化 blob
- Hybrid Retrieval、RRF、Context Packing、Reranker
- 文件 watcher、LSP、Code Graph、Git indexing

## Known Limitations

- `code_search` 当前只返回结构化 `not_indexed` 或其他真实状态，不返回检索结果。
- Host 没有 repository open/change/close 事件；启动映射为 attach，`deactivate` 映射为 detach。
- Host 没有 manifest 配置 schema；配置通过激活元数据下发并由 Guest 校验。
- Guest 没有后台任务与取消 handle；骨架阶段不启动内部任务。
- Host JSON KV 只适合 manifest 和小型 metadata，不适合 chunk、倒排索引或向量。

## Verification

- `cargo test --offline -p code-context-plugin`：通过，55 个单元测试与 1 个文档测试。
- `cargo test --offline -p agent-plugin-host`：通过，132 个单元测试与 2 个契约测试。
- `cargo test --offline -p agent-plugin-manager`：通过，53 个单元测试。
- WASM build、smoke test、TUI feature 测试与全仓检查：待本阶段最终验证后更新。
