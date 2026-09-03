# Code Context Engine Plugin

Code Context Engine 是标准 Lucia WASM 插件，目标是为 Agent 提供稳定的 `code_search` 仓库检索工具。
当前仅完成插件骨架、生命周期和索引元数据持久化，尚未实现任何代码索引或检索算法。

## 当前状态

- 插件 ID：`code-context`
- ABI：`0.10.0`
- Target：`wasm32-wasip2`
- 工具：`code_search(query, max_results?, token_budget?, path_scope?)`
- 当前工具行为：返回结构化 `not_indexed` 错误与空结果，不伪造搜索命中

## 架构

Guest 负责配置校验、仓库身份、索引状态、manifest 兼容性、持久化协议和工具契约。Host 只提供
通用能力，不包含 chunk、结构索引、Embedding 或检索业务逻辑。

当前模块：

- `src/lib.rs`：插件激活、仓库附着、动态工具注册和卸载
- `src/config.rs`：`enabled` 与 `embedding_mode` 配置校验
- `src/model.rs`：仓库身份、`IndexStatus`、`IndexManifest` 与稳定领域类型
- `src/storage.rs`：Host `store` 的 Workspace 作用域适配
- `src/embedding.rs`：稳定 provider 接口与明确失败的未配置实现
- `src/tool.rs`：`code_search` 外部 schema 和未索引返回协议

## Capabilities

- `workspace_read`：Host 注入当前项目可信根。激活时只列出根目录一次用于能力探测；本阶段不扫描源码。
- `storage`：保存 `index.manifest`，由 Host 按插件和 Workspace 作用域隔离。

插件不申请网络、secret、子进程、会话、文件写入或本地推理能力。

## 配置

当前 Host 没有 manifest 配置 schema，配置通过激活元数据下发并由 Guest 严格校验：

- `enabled = true | false`，默认 `true`
- `embedding_mode = local | api`，默认 `api`

两种 Embedding 模式都只建立配置边界。API 请求、本地模型加载和向量生成均未实现；调用
`EmbeddingProvider` 会返回 `EmbeddingUnavailable`，不会生成伪造向量。

## 构建与测试

```bash
cargo test --offline -p code-context-plugin
bun run build:plugin:code-context
cargo test --offline -p code-context-plugin-smoke-tests
```

## 后续阶段

下一阶段只实现 Repository Scanner、忽略规则、语言检测、纯 Rust 结构化解析、AST-aware 风格分块、
符号提取及其测试。Tree-sitter、Embedding、向量索引、混合检索、本地模型和网络请求继续推迟。
