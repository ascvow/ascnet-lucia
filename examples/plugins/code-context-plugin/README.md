# Code Context Engine Plugin

Code Context Engine 是标准 Lucia WASM 插件，目标是为 Agent 提供稳定的 `code_search` 仓库检索工具。
当前已实现仓库扫描、结构化解析、增量索引、符号与 BM25 检索、sqlite-vec 语义检索、
RRF 融合、可选模型重排和上下文打包。

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
- `src/semantic.rs`：语义索引、WASM int4 快照与原生存储适配
- `src/sqlite_vector.rs`：仅原生入口启用的 sqlite-vec 数据库与余弦 KNN
- `src/rerank.rs`：扩大候选召回、源码组装、远端模型重排与失败降级
- `src/repository/`：全仓库扫描、忽略规则和语言识别
- `src/parser/`：Rust、TypeScript、JavaScript、Python 与 Go 结构化解析，以及短兄弟声明的语义复合
- `src/retrieval/`：符号、BM25、语义候选与 RRF 融合
- `src/context/`：正文回读、去重、范围合并与 token 预算打包
- `src/tool.rs`：`code_search` 外部 schema 和未索引返回协议

## Capabilities

- `workspace_read`：Host 注入当前项目可信根；首次检索时递归扫描并建立索引。
- `storage`：保存 manifest 与文件哈希表这类小元数据，由 Host 按插件和 Workspace
  作用域隔离。它是重写全量的 8 MiB JSON，只适合小体积数据。
- `data_dir`：Host 按插件与项目作用域派生一个隔离目录并 preopen 成 `/data`，**没有容量
  上限**。语义向量与代码索引放在这里：它们的体积由用户仓库规模决定，塞进 `storage`
  会在超限后被静默截断，表现为功能随仓库变大而悄悄退化。

插件仅在 API 嵌入模式下申请受限 HTTP 网络能力，不申请 secret、子进程、会话或本地推理能力。

## 配置

当前 Host 没有 manifest 配置 schema，配置通过激活元数据下发并由 Guest 严格校验：

- `enabled = true | false`，默认 `true`
- `embedding_mode = local | api`，默认 `api`
- `embedding_request_interval_ms = <0..60000>`，默认 `1500`；限制相邻嵌入请求的最小间隔
- `embedding_max_retries = <0..10>`，默认 `4`；控制单批 429/5xx 自动重试次数
- `embedding_retry_base_ms = <0..30000>`，默认 `5000`；控制指数退避基数，单次等待最多 30 秒
- `embedding_send_dimensions = true | false`，默认 `false`；仅端点支持维度截断时开启
- `rerank_model = <模型名>`，可选；配置后调用同一 HTTP 端点的 `/v1/rerank`

`api` 模式通过 Host 受限 HTTP 能力调用用户配置的兼容嵌入端点。鉴权、模型名或参数错误会让
检索降级为符号与 BM25 两路；429 与 5xx 会按配置限速并自动退避重试，已经写入的向量不会
丢失。持续 429 保持 `Indexing` 并在后续调用续建，不会因普通停滞阈值永久降级。
`local` 模式当前仍返回未配置状态。

## 直接搜索当前仓库

首次运行默认通过 SiliconFlow `BAAI/bge-m3` 为整个仓库生成语义向量，再执行符号、BM25 与
语义三路混合检索，并通过 `Qwen/Qwen3-Reranker-4B` 对扩大后的候选源码重排。API 密钥
只从环境变量读取，不写入命令参数、缓存或插件元数据：

```bash
export SILICONFLOW_API_KEY='<你的密钥>'
cargo run -p code-context-plugin --example search_repository --features test-support
```

命令先完成全仓库索引，再显示 `搜索>` 提示符。它是独立运行的原生入口，不把 SQLite 文件能力
授予 WASM Guest。每个仓库在 `~/.lucia/code-context/` 下生成两个由规范化仓库路径摘要隔离的文件：

- `<仓库哈希>.json`：manifest、文件哈希等小型元数据；
- `<仓库哈希>.sqlite3`：sqlite-vec f32 向量与代码块定位元数据。

同一仓库第二次运行会直接打开 SQLite，只重新计算新增或内容变化的代码块。若目录中已有旧版
JSON int4 向量，首次运行会自动迁移到 SQLite 并删除 JSON 中的重复向量，不会重新调用嵌入接口。
每批成功向量会立即事务写入 SQLite；遇到 429、进程中断或手动退出后，重新运行同一命令会从
已有向量继续，不要添加 `--rebuild-cache`。
可以连续输入自然语言或代码标识符，输入 `exit`、`quit` 或 `:q` 结束；也可以在命令末尾直接
提供一条查询，以单次搜索模式运行。

结构化解析会把同一父节点下相邻、单个不超过 6 行的短兄弟声明合成语义专用复合块，复合块
最多覆盖 40 行和 1200 个字符。原子块继续进入符号与 BM25 索引，复合块只进入语义通道；
被复合块覆盖的原子成员不再单独生成向量，避免大量三五行函数形成近似重复候选。

送往嵌入端点的文档包含路径、文件级文档、父符号、限定名、签名、块级文档注释、import
上下文和代码正文。正文最多 1200 个字符，import 最多 16 条且总计不超过 600 个字符。

限定搜索目录并控制结果数：

```bash
cargo run -p code-context-plugin --example search_repository --features test-support -- \
  --path-scope crates/agent-core/ --max-results 5 "工具调用结果"
```

命令会读取当前 Lucia 仓库中所有受支持的源码、文档和配置文件，完成索引后输出文件路径、
行号、符号名称和代码片段。使用 `--root <路径>` 可以搜索其他仓库；`--cache-dir <目录>`
可覆盖默认缓存位置，`--rebuild-cache` 会删除当前仓库的 JSON 元数据、SQLite 主文件及
WAL/SHM 辅助文件后重新生成全部向量，`--no-cache` 禁用跨进程缓存。
使用
`--embedding-base-url`、`--embedding-api-key-env`、`--embedding-model`、
`--embedding-dimension`、`--embedding-batch-size`、`--embedding-request-interval-ms`、
`--embedding-max-retries` 和 `--embedding-retry-base-ms` 可以连接并适配其他兼容端点；
`--rerank-model` 可以更换重排模型，`--no-rerank` 可以只保留本地融合排序。

融合排序会区分查询形态：`Agent`、`PermissionManager::check` 这类独立标识符保留精确符号
提升；“一个agent对话”这类自然语言会压低普通精确词的符号权重，语义通道可用时同时提高
语义权重。语义不可用时，前缀与后缀符号匹配恢复标准权重，避免纯词法降级丢失分散在不同
代码块里的有效信号。多字中文查询只用双字词元参与 BM25，单字查询仍然可用。

自然语言查询的词法与语义通道各取至少 120 个候选，通常最多 200 个；返回条数不会把这两路
召回深度压到个位数。配置 `rerank_model` 后，自然语言使用 40–48 个候选做源码重排，不再将
多路一致直接当成准确定位。每条候选最多附带 1800 个正文字符，同文件只读取一次且不缓存
整批文件全文；模型相关度主导自然语言排序，未明确查询测试时仍降低测试代码的排名。
独立标识符继续跳过模型请求，混合查询保留条件重排与精确符号保护。模型分数是排序依据，
不是已校准的命中概率；首次自然语言检索会增加一次远端重排延迟。

Vue 与 uni-app `.nvue` 使用 HTML 分词器定位顶层脚本，再以 Oxc 提取函数及原始行号。
支持普通脚本、`script setup`、JS/TS、renderjs、Options API 和 Hook 内命名函数；外链脚本
不读取，未知脚本语言和未被声明覆盖的模板/样式继续文本检索。API 参数里的匿名回调槽和
函数内普通临时变量不单独充当业务方法，纯数据字典不拆成方法树。结构索引版本为 4，
升级后旧结构索引需重建。全部语义模型仍通过配置的 API 使用，不运行本地模型推理。

同一仓库生命周期内最多缓存 32 条查询向量和 16 条已校验重排响应，不落盘。重排缓存键
包含完整请求（模型、问题、候选顺序及文档）和候选文件全文摘要；每次仍读取源码，任一文件
变化、读取失败或响应校验失败均不能复用。命中时重新应用当前候选分数并执行上下文打包，
而不是返回旧代码片段。查询向量最多约 1 MiB，重排响应最多 1 MiB；切换仓库后清空。
该优化加速重复查询，不保证首次查询延迟，也不替代索引增量刷新。

模型重排会把查询以及候选的路径、符号、注释和源码片段发送到用户配置的 API 端点。嵌入和
重排都可能发送仓库源码，不能将 API 模式理解为“只有查询离开本机”。接口
失败时搜索不会中断，而是保留原有 RRF 融合顺序并在结果提示中说明降级原因。

没有模型端点时可以显式追加 `--lexical-only`，只验证符号与 BM25 检索。该模式不会生成
向量或调用 rerank，也不能作为语义搜索验收结果。

## 构建与测试

插件把 SQLite（元数据 + `sqlite-vec` 向量索引）编进 WASM，因此构建需要 **wasi-sdk** 提供
clang 与 `wasm32-wasip2` sysroot。这是插件自己的依赖，Lucia 本体不因此增加任何构建前置。

从 [wasi-sdk releases](https://github.com/WebAssembly/wasi-sdk/releases) 下载对应平台的包，
解压到 `~/.wasi-sdk`（或任意位置并设置 `WASI_SDK_PATH`）；`scripts/build-plugin.ts` 会自动
发现它并注入交叉编译变量。

```bash
cargo test --offline -p code-context-plugin
cargo test --offline -p code-context-plugin --example search_repository --features test-support
cargo test --offline -p code-context-plugin --features test-support sqlite_vector
cargo test --offline -p code-context-plugin --features test-support --test retrieval_report -- --nocapture
bun run build:plugin:code-context
cargo test --offline -p code-context-plugin-smoke-tests
```

真实消融需要设置 `SILICONFLOW_API_KEY` 并单独运行 `retrieval_ablation_report`。语义向量保存在
`~/.lucia/code-context/retrieval-report/`，因此限流或进程中断后可跨进程续建；该目录与正式搜索
缓存隔离，不会删除或污染 `search_repository` 已有向量。

```bash
cargo test -p code-context-plugin --features test-support \
  --test retrieval_report retrieval_ablation_report -- --ignored --nocapture
```
