# Code Context Engine 实现状态

更新日期：2026-09-04

## Phase

Phase 15：Hardening。Phase 0–11、13–14 已完成；Phase 12 因 Host 无本地嵌入能力阻塞。

## Status

WASM 插件检索链路已完成，当前加固原生全仓库搜索、持久化与检索质量。详细阶段记录以
[`PROGRESS.md`](./PROGRESS.md) 为准。

## Implemented

- 仓库扫描、忽略规则、五类结构化解析、稳定 chunk 与增量索引
- 符号、BM25、语义三路检索，RRF 融合、上下文打包与模型重排
- WASM Guest 通过 Host Workspace KV 保存 manifest、文件哈希与 int4 语义快照
- 原生 `search_repository` 通过 sqlite-vec 保存 f32 向量并执行余弦 KNN
- `~/.lucia/code-context/<仓库哈希>.json` 与 `.sqlite3` 跨进程缓存
- 旧 JSON 向量自动迁移、嵌入空间隔离、增量删除和数据库重开恢复
- 默认 `BAAI/bge-m3` 嵌入与 `Qwen/Qwen3-Reranker-4B` 重排

## Deferred

- Host 原生 embedding capability 与 Local Embedding Provider
- Host blob 存储，以及正式 WASM 插件的大规模完整 f32 向量后端
- 文件 watcher、LSP、Code Graph、Git indexing

## Known Limitations

- sqlite-vec 只用于独立原生搜索入口，WASM Guest 不直接拥有文件写入能力。
- Host 没有 repository open/change/close 事件；外部编辑依赖惰性一致性校验。
- Guest 没有后台任务与取消 handle；长索引任务通过明确的分片推进协议完成。
- Host JSON KV 仍不适合原始 f32 向量；WASM 使用有 7 MiB 预算的 int4 快照。

## Verification

- `cargo test -p code-context-plugin --lib --features test-support`：348 项通过。
- `cargo test -p code-context-plugin --example search_repository --features test-support`：11 项通过。
- sqlite-vec 重开、KNN、路径范围、空间隔离、查询失败降级与旧快照迁移测试：通过。
- `cargo clippy -p code-context-plugin --all-targets --features test-support -- -D warnings`：通过。
- 默认 `wasm32-wasip2` release 构建：通过，正常依赖树不含 SQLite。
- `cargo test --offline -p code-context-plugin-smoke-tests`：8 项通过。
- 完整仓库基准：Recall@1 `0.600`、Recall@5/10 `0.933`、MRR `0.756`。
- 真实 SiliconFlow 跨进程复用未重跑：当前进程未设置 `SILICONFLOW_API_KEY`。
