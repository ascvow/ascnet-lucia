# Code Context Engine Plugin

Code Context Engine 是标准 Lucia WASM 插件，目标是为 Agent 提供稳定的 `code_search` 仓库检索工具。
当前已实现仓库扫描、结构化解析、增量索引、符号与 BM25 检索、RRF 融合和上下文打包。

## 当前状态

- 插件 ID：`code-context`
- ABI：`0.10.0`
- Target：`wasm32-wasip2`
- 工具：`code_search(query, max_results?, token_budget?, path_scope?)`
- 当前工具行为：首次调用按预算分片建立索引，完成后返回文件、行号、符号和代码片段

## 架构

Guest 负责配置校验、仓库身份、索引状态、manifest 兼容性、持久化协议和工具契约。Host 只提供
通用能力，不包含 chunk、结构索引、Embedding 或检索业务逻辑。

当前模块：

- `src/lib.rs`：插件激活、仓库附着、动态工具注册和卸载
- `src/config.rs`：`enabled` 与 `embedding_mode` 配置校验
- `src/model.rs`：仓库身份、`IndexStatus`、`IndexManifest` 与稳定领域类型
- `src/storage.rs`：Host `store` 的 Workspace 作用域适配
- `src/embedding.rs`：嵌入 provider、批量请求与降级处理
- `src/repository/`：全仓库扫描、忽略规则和语言识别
- `src/parser/`：Rust、TypeScript、JavaScript、Python 与 Go 结构化解析
- `src/retrieval/`：符号、BM25、语义候选与 RRF 融合
- `src/context/`：正文回读、去重、范围合并与 token 预算打包
- `src/tool.rs`：`code_search` 外部 schema 和未索引返回协议

## Capabilities

- `workspace_read`：Host 注入当前项目可信根；首次检索时递归扫描并建立索引。
- `storage`：保存 `index.manifest`，由 Host 按插件和 Workspace 作用域隔离。

插件仅在 API 嵌入模式下申请受限 HTTP 网络能力，不申请 secret、子进程、会话、文件写入或
本地推理能力。

## 配置

当前 Host 没有 manifest 配置 schema，配置通过激活元数据下发并由 Guest 严格校验：

- `enabled = true | false`，默认 `true`
- `embedding_mode = local | api`，默认 `api`

`api` 模式通过 Host 受限 HTTP 能力调用用户配置的兼容嵌入端点；端点未配置或请求失败时，
检索自动降级为符号与 BM25 两路。`local` 模式当前仍返回未配置状态。

## 直接搜索当前仓库

从仓库根目录运行以下命令，查询可以使用自然语言或代码标识符：

```bash
cargo run -p code-context-plugin --example search_repository --features test-support -- "PluginHostApi"
```

限定搜索目录并控制结果数：

```bash
cargo run -p code-context-plugin --example search_repository --features test-support -- \
  --path-scope crates/agent-core/ --max-results 5 "工具调用结果"
```

命令会读取当前 Lucia 仓库中所有受支持的源码、文档和配置文件，完成索引后输出文件路径、
行号、符号名称和代码片段。使用 `--root <路径>` 可以搜索其他仓库。

## 构建与测试

```bash
cargo test --offline -p code-context-plugin
cargo test --offline -p code-context-plugin --example search_repository --features test-support
bun run build:plugin:code-context
cargo test --offline -p code-context-plugin-smoke-tests
```
