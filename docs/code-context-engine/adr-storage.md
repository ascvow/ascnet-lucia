# ADR：索引存储后端

## 状态

已接受，2026-09-03；2026-09-04 增补原生搜索入口的 sqlite-vec 后端。

本 ADR 的文件能力约束针对 WASM Guest。独立原生程序不继承该限制，但不得借此扩大插件
manifest 权限或让 Guest 自行指定任意数据库路径。

## 背景

Code Context Engine 需要保存三类数据：索引 manifest（KB 级）、结构化 chunk 与倒排索引（MB 级）、嵌入向量（数十 MB 级）。立项前的候选方案包括 SQLite FTS5、LanceDB、USearch。审计后这些候选全部需要重新评估，因为它们的可行性完全取决于真实的 WASM 插件能力。

## 候选评估

### WASM Guest 内的 SQLite（含 FTS5）

否决。三条独立阻断：

1. **无法编译。** SQLite 是 C。本机无 wasi-sdk，`clang -print-targets` 的注册目标中没有 wasm 后端，构建流水线 `scripts/build-plugin.ts:20` 使用 `--offline`。
2. **无法写文件。** `manifest.rs:1153` 在加载期拒绝 `fs_write`。SQLite 需要真实文件句柄与锁。
3. **无 mmap、无文件锁、无线程。** WASI p2 不提供 mmap；`engine.rs:52` 未开启 threads。

SQLite WASM（官方 wasm 构建）同样不适用：它是给浏览器 JS 宿主的，需要 VFS 后端，而本插件既没有文件写入也没有 IndexedDB 等价物。

### 原生搜索入口中的 sqlite-vec

接受。`search_repository` 是单独运行的原生可执行示例，可以在宿主进程内使用 bundled SQLite
和 sqlite-vec，不改变 WASM Guest 的能力集合。数据库固定放入用户缓存目录，并按规范化仓库
路径摘要隔离；Guest 只接收已经由原生入口确定的数据库路径，不提供任意路径参数。

该后端保存归一化 f32 向量并使用 `vec0` 余弦 KNN。数据库同时记录 schema、嵌入空间标识、
维度、chunk ID、仓库相对路径与内容哈希；嵌入空间变化时清空旧记录，文件变化时增量删除或
替换。旧版 Host JSON int4 快照可一次性迁移，迁移成功后删除重复向量。

### LanceDB / USearch

否决。二者都是原生库（Rust + C++/SIMD），依赖 mmap 与文件写入。USearch 的 Rust binding 走 C++ FFI，同样撞上 C 工具链缺失。

### 纯 Rust / WASM 存储引擎（sled、redb 等）

否决。都需要文件写入与页级 I/O。当前 Guest 只有 `host-store-*`（JSON KV）这一条持久化通道。

### Host storage capability（现状）

`state.rs:33`：8 MiB / 8192 键上限，`capability/mod.rs:428` 每次写入克隆全量 map 并重写整个文件（`serde_json::to_vec_pretty` + 两次 fsync）。

**可承载：** manifest、配置游标、文件路径 → 内容哈希表（2000 文件约 160 KB）。
**不可承载：** chunk 全文、倒排索引、原始 f32 向量或无边界增长的完整向量集合。

先例：knowledge-plugin 把整个索引塞进一个 `entries` 键并硬性截断到 2000 条（`examples/plugins/knowledge-plugin/src/lib.rs:46`、`:178`）。这就是该存储层的实际承载量级。

### 自建紧凑索引 + 内存驻留

可行，且是 V1 唯一可行的方案。

## 决策

**V1：分层存储，按体量分流。**

| 数据 | 位置 | 理由 |
| --- | --- | --- |
| `IndexManifest` | `host-store` Workspace | KB 级，现有能力充分 |
| 路径 → 内容哈希表 | `host-store` Workspace | 百 KB 级，增量判断的最小充分状态 |
| Chunk / 符号表 / 倒排索引 | **内存驻留，进程启动后按需重建** | 8 MiB JSON 装不下 |
| WASM Guest 嵌入向量 | `host-store` Workspace 的 int4 快照 | 7 MiB 实际 JSON 预算；超限时明确截断并在下次补算 |
| 原生 `search_repository` 嵌入向量 | `~/.lucia/code-context/<仓库哈希>.sqlite3` | sqlite-vec 持久化 f32，并执行余弦 KNN |

索引结构在插件内自实现，格式对 Host 不透明：

- 符号表：`qualified_symbol` → chunk id 的排序数组 + 二分查找。
- 词法索引：term → posting list 的紧凑倒排，`u32` delta 编码。
- Chunk 内容：**不驻留全文**，只存 `(file_path, start_line, end_line, content_hash)`；返回结果时按需 `host-fs-read` 重读。这同时解决了内存压力与外部编辑的一致性校验。

**懒构建。** `activate` 只读 manifest 与哈希表，不做任何解析——它和其他导出共用同一份 5000 万 fuel 预算（`engine.rs:15`），全量索引必然 trap。首次 `code_search` 触发构建，按 fuel 预算分片，跨多次调用推进；未完成时返回已就绪部分并标注 `mode: partial`。

**V2：迁入 blob 存储。** 依赖[架构](./architecture.md#recommended索引-blob-存储)提出的 `host-store-blob-*` 能力。届时索引与向量以自定义紧凑二进制格式落盘，冷启动从"重建"变为"mmap 式顺序读取"。格式设计已按此预留：全部索引结构使用可直接从字节切片零拷贝读取的布局，不依赖 serde。

## 后果

正面：

- V1 零新增存储能力即可交付（只依赖唯一必需的工作区读取能力）。
- 不污染用户仓库——索引路径由 Host 按插件 ID 与作用域派生（`capability/mod.rs:682`），插件无法指定。
- `Workspace` 作用域天然实现多仓库隔离。
- 不驻留 chunk 全文，使 64 MiB 内存上限在中等仓库下可控。
- 原生搜索命令复用完整 f32 向量，不再受 Host JSON KV 容量限制，也不需要独立向量服务。

负面，且必须如实承认：

- **每次应用启动都要重建索引。** 哈希表能避免重复解析未变更文件吗？不能——判断"未变更"本身就需要读全文算哈希（`FileEntry` 没有 size/mtime，见[能力矩阵](./plugin-capability-matrix.md)）。因此冷启动成本与文件数成正比。这是 Recommended 级 Host 改动（`FileEntry` 加元数据）能立刻缓解的一项。
- 仓库规模有上限。超过约束时插件必须显式降级（缩小 `path_scope`、提示用户），不能静默截断到无用的规模——knowledge-plugin 的 `truncate` 是被逼出来的，不应作为范式沿用。
- sqlite-vec 仅属于原生示例；正式 WASM 插件仍不能直接打开数据库，不能把该实现当作新增的
  Guest 文件能力。

## 未决

blob 存储的配额默认值需要在 Host 提案阶段确定。建议默认 256 MiB 并允许应用策略收紧，与 `ExecutionPolicy` 的既有收紧语义（`policy.rs:202`）保持一致。
