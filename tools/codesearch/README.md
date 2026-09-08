# codesearch

供 Coding Agent 使用的**本地**代码语义搜索工具。输入自然语言问题、符号名、文件路径或错误消息，
返回当前工作目录中的相关代码，并附带准确的路径、行号与源码版本。

技术栈：Tree-sitter 结构化分块 + SQLite/FTS5 + USearch 向量索引，通过 CLI 与 MCP stdio 暴露。

- 单进程、无外部服务依赖（Embedding 服务除外，且可选）。
- 未配置 Embedding 服务时**诚实降级**为符号与全文检索，并在结果中说明原因。
- 源码只在显式配置了 Provider 后才会发往该服务；`--offline` 可彻底禁止模型网络调用。

---

## 1. 安装与原生编译依赖

本目录是**独立 workspace**，不属于 Lucia 主 workspace，因此不会给主仓库的 `clippy`/`test`
增加 C/C++ 构建前置。

需要：

- Rust 1.89 或更高（`rust-version = "1.89"`；本次开发使用 rustc 1.96.1）。
  这个下界来自 workspace 写锁：它用标准库的 `File::try_lock`（1.89 稳定），
  因此不必为文件锁引入第三方依赖。
- 一个支持 **C++17** 的 C/C++ 工具链。`usearch` 通过 `cc` 编译 C++ 源码，
  `rusqlite` 使用 `bundled` 特性从 C 源码编译 SQLite（含 FTS5），`tree-sitter` 语法同样需要 C 编译器。
  **不需要** cmake。
  - macOS：`xcode-select --install`
  - Debian/Ubuntu：`apt install build-essential`
  - Windows：MSVC Build Tools
- 首次索引若要使用语义检索，需要能访问所配置的 Embedding 服务；否则只用全文与符号检索。

构建：

```bash
cd tools/codesearch
cargo build --release
# 可选：安装到 ~/.cargo/bin
cargo install --path .
```

---

## 2. 快速开始

```bash
cd /path/to/your/project

# 1) 建立索引（未配置模型时只建符号与全文索引）
codesearch index --root .

# 2) 搜索
codesearch search --root . --query "请求失败后的重试逻辑在哪里"

# 3) 按结果给出的 code_ref 读取源码
codesearch read --root . --ref "cs1:22:00e385ee427902a1:19-59"

# 4) 查看索引状态与降级原因
codesearch status --root . --json
```

索引数据默认写入 `<root>/.codesearch/`（SQLite 数据库、USearch 快照与检查点）。
可用 `--data-dir` 改到别处，例如放在仓库外避免污染工作树。
该目录已在默认排除规则中，工具不会索引自己的数据。

---

## 3. 配置 Embedding 服务

复制 `codesearch.example.toml` 到工作目录根下命名为 `codesearch.toml`，按需修改。
所有字段都有默认值，可以只写要改的部分。**配置里只写环境变量名，绝不写真实凭证。**

### 硅基流动（SiliconFlow）免费向量模型

`BAAI/bge-m3` 在硅基流动属于免费模型（同名带 `Pro/` 前缀的是收费版），输出 1024 维，
中英文与代码混合场景表现可用。免费模型需要完成实名认证，且速率上限固定，不随付费用量等级提升。

```toml
[embedding]
provider = "openai_compatible"
base_url = "https://api.siliconflow.cn/v1"
model = "BAAI/bge-m3"
api_key_env = "SILICONFLOW_API_KEY"
dimensions = 1024
send_dimensions = false   # 该服务会拒绝请求体中的 dimensions
batch_size = 16
max_concurrency = 1
max_retries = 5
```

```bash
export SILICONFLOW_API_KEY=...   # 只放在环境变量里
codesearch index --root .
```

### 关于 `dimensions` 与 `send_dimensions`

这是两件不同的事，容易混淆：

- `dimensions`：**期望**维度。用于校验服务返回值、固定 Embedding 空间身份，并省掉一次探测请求。
- `send_dimensions`：是否把 `dimensions` **写进请求体**。默认 `false`。

只有支持按维度截断的服务（如 OpenAI `text-embedding-3-*`）才应开启 `send_dimensions`。
多数服务对未知参数直接返回 400 —— 实测硅基流动的 `BAAI/bge-m3` 会返回
`{"code":20015,"message":"The parameter is invalid."}`。

### OpenAI

```toml
[embedding]
provider = "openai_compatible"
base_url = "https://api.openai.com/v1"
model = "text-embedding-3-small"
api_key_env = "OPENAI_API_KEY"
dimensions = 1536
send_dimensions = true
```

### 本地推理服务

llama.cpp、Xinference、Ollama 等提供 OpenAI-compatible `/embeddings` 的服务都可以直接用，
不需要在进程内加载模型：

```toml
[embedding]
provider = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "bge-m3"
api_key_env = ""          # 留空表示不发送 Authorization 头
dimensions = 1024
```

### 无模型与 offline 模式

- `provider = "none"`（默认）：不配置模型。仍可用符号、路径、字面量与 FTS5 检索。
- `--offline`：即使配置了 Provider 也禁止任何模型网络调用。适合断网或不希望源码外发的场合。

两种情况下搜索都能工作，但响应中会带 `degraded = true` 与具体原因，例如
`未配置 Embedding Provider：仅建立符号与全文索引，语义检索不可用`。
降级信息同时出现在人读输出和 JSON 中，不会只写进日志。

### API Key 与源码外发

- Key 只从环境变量读取，不写入源码、配置、数据库和日志；`Debug` 输出中也已脱敏。
- 默认不向任何远程服务发送源码：必须显式配置 `provider` 与 `base_url` 才会发出请求。
- 默认排除规则会跳过 `.env`、私钥等敏感文件，并与普通噪声过滤**分开配置**。
  这不是安全边界，也不保证识别全部秘密信息。`.gitignore` 同理，只用于降噪。

---

## 4. CLI

所有子命令共享 `--root` / `--config` / `--data-dir` / `--offline`。

| 命令 | 作用 | 关键参数 |
| --- | --- | --- |
| `index` | 建立或更新索引 | `--force-rebuild`、`--watch`、`--json` |
| `search` | 搜索代码 | `--query`、`--include`、`--exclude`、`--language`、`--max-results`、`--token-budget`、`--json` |
| `read` | 按引用读取源码 | `--ref`、`--expand none\|containing_symbol\|file`、`--token-budget`、`--json` |
| `status` | 查看索引状态 | `--json` |
| `serve` | 启动 MCP 服务 | `--transport stdio`、`--watch true\|false` |

`--language` 接受 33 类格式：`rust`、`typescript`、`tsx`、`javascript`、`python`、`go`、
`java`、`c`、`cpp`、`csharp`、`ruby`、`php`、`swift`、`kotlin`、`scala`、`lua`、`bash`、
`css`、`sql`、`dart`、`elixir`、`zig`、`haskell`、`hcl`、`graphql`、`vue`、`svelte`、
`html`、`json`、`yaml`、`toml`、`markdown`、`text`。支持常见别名，如 `tf`、`gql`、`ex`。

查询里也可以内联作用域，与命令行参数叠加生效：

```bash
codesearch search --root . --query 'lang:rust path:src/network/** 退避重试'
codesearch search --root . --query '"connection reset by peer"'   # 引号内按字面量处理
codesearch search --root . --query '-path:**/tests/** RetryPolicy::execute'
```

`status --json` 会返回文件数、当前代码块数、已完成/等待/失败的 Embedding 数量、
当前 Embedding 空间、最近扫描状态，以及向量内存应用序号与持久化检查点序号。

---

## 5. MCP 配置

`serve` 使用 stdio 传输。**stdout 只输出 MCP 协议帧，所有日志走 stderr**，
日志级别由 `CODESEARCH_LOG` 控制（默认 `warn`）。

```json
{
  "mcpServers": {
    "codesearch": {
      "command": "codesearch",
      "args": ["serve", "--root", "/absolute/path/to/project", "--transport", "stdio"],
      "env": { "SILICONFLOW_API_KEY": "..." }
    }
  }
}
```

向 Agent 暴露以下工具：

- **`search_context`** — 入参 `query`（必填）、`include`/`exclude`/`language`、`max_results`、`token_budget`。
  返回 `catalog_epoch`、`status`、`results`、token 统计与 `estimated`/`truncated` 标志。
- **`search_code`** — 既有兼容入口，与 `search_context` 的参数、实现、返回值和错误完全一致。
- **`read_code`** — 入参 `code_ref`、`expand`、`token_budget`。返回该版本的原始代码与出处。

调用示例：`search_context({"query":"请求失败后在哪里等待并重试"})`。
先检查 `status.degraded` 与 `status.vector_search_used`，再按返回的 `code_ref` 读取源码确认。
未配置 Embedding 时没有纯语义能力；命中分数不表示答案一定正确。

workspace 范围由启动参数 `--root` 决定。**工具入参里没有任何根目录参数**，
Agent 无法通过参数访问其他本地目录；路径穿越与指向 workspace 外的符号链接都会被拒绝。

---

## 6. 增量更新与一致性语义

### 新鲜度

第一版只公开 `best_effort`。搜索结果附带最近采集状态、待嵌入数量与降级原因。

"索引中的当前版本"**不等于**"查询这一瞬间磁盘上的绝对最新版本"。
默认 `index.refresh_on_search = true` 会在每次查询前做一次增量扫描，让刚保存的文件立即可搜；
内部另有 `ensure_indexed(paths)` 用于保证指定路径已重新采集。
`wait_for_revision` 未实现，也未作为功能暴露。

### 变更处理

文件监听 + 去抖合并（默认 300ms）+ 内容 Hash + 启动扫描 + 可配置的低频校验扫描。
监听器只是加速器：它报告事件丢失时会触发重新扫描，队列超限时退化为整目录重扫。

一次修改的处理顺序：事务外读快照并解析分块 → 单个 SQLite 事务内提交新版本、
更新 FTS 与符号、使旧版本失效、记录旧向量删除、标记待嵌入 → 事务外查缓存并请求缺失向量 →
向量返回后**重新校验代码块是否仍是当前版本**（已失效则只保留可复用缓存，不重新激活旧代码）。

因此新代码会先通过符号与全文检索可见，语义索引追上之前返回 `pending`/`degraded` 状态。
旧向量即使短暂残留在 USearch 中，也会在查询时被当前版本过滤掉。

### SQLite 与 USearch 的一致性

两者没有共同事务，所以采用持久化有序的 `vector_ops` 日志 + 幂等重放 + 检查点：

- 发布检查点时先完整写入新 generation 的快照文件，再原子发布指向它的 manifest；
  旧检查点在新检查点发布成功前不会失效。
- 启动时加载匹配的有效检查点并重放其后的日志。
- 快照缺失或损坏时，**从 SQLite 中保存的向量重建，不重新请求模型**。

向量本身存在 SQLite 里，这是恢复能力的来源，也意味着磁盘占用会随代码块数量增长。
第一版不做自动历史 GC：不会提前清理恢复所需的日志、向量与源码快照。

### workspace 进程锁

同一个 workspace 同时只允许一个写进程。`serve` 会在整个会话期间持有独占锁。

- 同一进程内查询可以并发。
- 锁被占用时返回明确的 `workspace_busy` 错误。
- **实际影响**：`serve` 正在运行时，另开一个 `codesearch index` 会失败。
  这种情况下用只读方式查询（如 `search`）仍可工作，但不会刷新索引，
  并会在输出中提示"以只读模式运行"。
- 不同工作目录（含不同 git worktree）默认相互隔离。切换分支时索引跟随当前目录内容更新。

---

## 7. 检索行为

三路召回后用 RRF 融合，不直接相加 BM25 与余弦距离：

```
score(d) = Σ weight_i / (k + rank_i(d))
```

`k` 默认 60，各通道权重与候选预算均可配置。完整符号的精确命中有保留额度
（`exact_reserve`，默认 3），不会被语义近似结果淹没。

- **精确**：符号、路径、受作用域限制的字面量。
- **FTS5**：对 `path_terms` / `symbol_terms` / `doc_terms` / `code_terms` 分别建词条，
  对 camelCase、PascalCase、snake_case 与路径拆词，同时保留原始完整符号。
  用户查询始终通过参数化 SQL 传入，引号、括号、冒号等字符不会造成语法错误或注入。
- **向量**：查询向量与文档向量必须属于同一 Embedding 空间；命中后校验当前版本与作用域。

分数只表示排序依据，**不是校准过的正确概率**。

第一版只有 `fast` 模式（混合召回 + RRF + 基础上下文组装）。
没有实现额外重排，因此不提供行为相同、只是换名字的 `deep` 模式。

---

## 8. 已知限制

- **中文分词**：没有引入中文分词器。中文查询在 FTS 中按相邻字构成短语处理
  （例如「重试逻辑」→ `"重 试 逻 辑"`），并叠加语义召回。这在实测中可用，
  但不等于真正的中文分词，长句召回质量依赖 Embedding 模型。
- **token 数是估算值**：没有接入精确 tokenizer，所有 token 相关输出都带 `estimated = true`。
  分块参数（`target_tokens` 等）同为估算，另有保守的 `max_chars` 兜底。
  不宣称满足精确 token 上限。
- **关系扩展是启发式的**：只做父子包含、明确的导入信息与简单的测试关联，默认最多扩展一跳。
  Tree-sitter 提取的调用表达式**不等于**已解析的调用目标，按名称匹配得到的关系
  只标记为启发式，不冒充确定的引用或调用图。
- **语言支持**：28 类结构化入口覆盖 Rust、TS/TSX/JS、Python、Go、Java、C/C++、C#、
  Ruby、PHP、Swift、Kotlin、Scala、Lua、Bash、CSS、SQL、Dart、Elixir、Zig、Haskell、
  HCL、GraphQL、Vue、Svelte、HTML。Markdown 按标题分块；JSON/YAML/TOML/其他文本走有界文本分块。
  解析失败时有效文本仍保留，不会因未知语法丢失整个文件。
- **框架范围**：Flutter 类与方法走 Dart；React/Next.js 走 JSX/TSX；Vue/Nuxt/uni-app
  的 `.vue`/`.nvue`/`.uvue` 拆出脚本与样式；Vue `defineComponent` 与 Pinia `defineStore`
  提取静态选项对象方法；FastAPI/Spring 等装饰器、注解随声明保留。框架工厂名称是语法识别，
  不冒充跨文件导入解析。Terraform 块按种类与标签命名；GraphQL 只提取具名定义。
- **方言边界**：SCSS/Less/Sass 仍使用 CSS 语法，复杂预处理器结构可能只保留文本；
  Elixir 不展开宏，Haskell 不执行类型推导，HTML/Vue/Svelte 模板不生成绑定或调用图。
  这些都不是框架编译器或语言服务器。
- 未实现：完整跨语言调用图、LLM 全仓摘要、多仓库搜索、Web UI、向量库服务化、Reranker
  （只预留了接口，检索不依赖它）。

### 恢复方法

| 症状 | 处理 |
| --- | --- |
| USearch 快照损坏或缺失 | 直接重启。会从 SQLite 中的向量重建，不请求模型。 |
| 怀疑目录状态不一致 | `codesearch index --root . --force-rebuild`，对所有文件重算内容 Hash。 |
| 换了 Embedding 模型 | 改 `model` 或 `cache_namespace` 即可。Embedding 空间身份变化后旧向量不会被复用。 |
| 想彻底重来 | 删除 `<root>/.codesearch/` 后重新 `index`。 |
| `workspace_busy` | 关掉正在运行的 `serve` 或其他写进程。 |

---

## 9. 实际运行记录

以下是本机真实执行结果，非估计值。环境：macOS (darwin 25.6.0)、rustc 1.96.1、Apple clang 21.0.0。

### 检查

下列数字为初版历史记录，不代表当前测试数量。本次语言扩展与实测见
[语言扩展评测记录](reports/language-support-2026-09-08.md)。

```bash
cargo fmt --all -- --check                               # 通过
cargo clippy --all-features --all-targets -- -D warnings # 通过，无告警
cargo test --all-features                                # 200 passed; 0 failed
```

测试分布：单元测试 164、`end_to_end` 10、`http_provider` 18、`mcp_stdio` 6、
`real_model_smoke` 2（无凭证时跳过并打印说明）。

Embedding 测试分层：单元测试用确定性 Mock Provider；HTTP Provider 用本地测试服务器
验证真实协议、重试与错误处理；USearch 用真实库做集成测试；真实模型 smoke test 需显式提供凭证。

### 真实模型（硅基流动 `BAAI/bge-m3`）

从零索引本工具自身源码（含测试与 fixture），release 构建，
`batch_size=16`、`max_concurrency=1`：

```
57 个文件 → 875 个代码块，875 个向量全部生成成功，pending 0，failed 0，degraded=false
目录扫描 80 ms，整体 14.1s（绝大部分是等待 Embedding 服务）
无变更时再跑一次 index：目录扫描 1 ms，整体 0.01s，不重新请求模型
```

自然语言查询实测 Top-1（`--max-results 1`，排除了下节说明的 `eval/`）：

| 查询 | Top-1 结果 | 命中通路 |
| --- | --- | --- |
| API Key 是从哪里读取的 | `src/models/http.rs` `read_api_key` | symbol+fts+vector |
| 多路召回的结果是怎么合并排序的 | `src/retrieval/fusion.rs` `RecallList, Candidate` | fts+vector |
| 哪些文件会被跳过不索引 | `src/workspace/scan.rs` `SkippedFile, ScanOutput, …` | fts+vector |

注意第 2、3 行的 `symbol` 是一个**列表**：小型容器会整体保留为一个代码块，
并用块内全部符号标注，而不是把父节点和每个子节点重复索引成大量相同代码。

### 评测：FTS-only vs Hybrid

`eval/queries.json` 是 20 条人工标注查询（含自然语言、符号、路径与字面量）。
基线通过 `--offline` 获得：不请求模型即无法生成查询向量，只剩符号与 FTS 通道，
这正是"没有 Embedding 服务时"的真实表现。

```bash
export SILICONFLOW_API_KEY=...
cargo run --release --example eval -- --root . --queries eval/queries.json
```

评测会把 `eval/**` 排除在作用域外。就地索引本仓库时查询集文件本身也会被索引，
而它逐字包含每一条查询**以及**标注好的期望路径，于是稳定地成为自己那条查询的头部结果。
那是测量装置在污染自己，不是检索能力，所以必须排除。

本次实际结果：

| 指标 | FTS-only | Hybrid |
| --- | --- | --- |
| Recall@1 | 0.400 | **0.550** |
| Recall@3 | **0.800** | 0.750 |
| Recall@5 | 0.800 | 0.800 |
| MRR | 0.577 | **0.645** |

**这些数字只反映本次运行在这 20 条人工标注样本上的表现，不能外推为通用检索质量结论。**
查询集很小，主要用途是同一份代码上的回归对比。

结果不是单方面的，值得如实说明：混合检索把更多正确答案顶到了第 1 位
（Recall@1 0.400 → 0.550，MRR 0.577 → 0.645），但 **Recall@3 反而下降**
（0.800 → 0.750）。20 条里 8 条排名上升、3 条下降：

- "请求超时和重试是怎么配置的" `#1 → #7`
- "数据库表结构定义在哪" `#1 → #3`
- "怎么把驼峰和下划线命名拆成搜索词" `#3 → 未命中`

这三条 FTS 本来就靠关键词精确命中，语义近邻引入的相关块把它们挤了下去。
`exact_reserve` 只保护完整符号的精确命中，护不住这类普通关键词查询。
这是当前融合权重的真实代价，没有调参去掩盖它。

另有 2 条查询（"同一个目录被两个进程同时打开会怎样"、"文件保存后多久会被重新索引"）
两种配置都未命中标注路径。实际返回的是 README 中对应的说明小节——
对人来说是合理答案，但标注只认源码文件。这既是已知短板，也是小样本标注的局限。

---

## 10. 开发

```bash
cd tools/codesearch

cargo test --all-features                # 默认套件，不需要网络与凭证
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all

# 真实模型 smoke test（缺少凭证时自动跳过并打印说明）
export SILICONFLOW_API_KEY=...
cargo test --all-features --test real_model_smoke -- --nocapture

# 也可指向其他 OpenAI-compatible 服务
export CODESEARCH_SMOKE_BASE_URL=http://127.0.0.1:8080/v1
export CODESEARCH_SMOKE_MODEL=bge-m3
export CODESEARCH_SMOKE_API_KEY_ENV=MY_KEY
export CODESEARCH_SMOKE_DIMENSIONS=1024
```

模块划分：

```
src/
  lib.rs / main.rs
  engine.rs      统一核心引擎，CLI 与 MCP 共用，不复制检索逻辑
  config/        配置与校验
  workspace/     路径身份、扫描过滤、进程锁
  indexer/       索引流水线与文件监听
  parser/        Tree-sitter 适配层（Rust / TS / Elixir、通用语法表、HTML/SFC）
  chunker/       结构化分块
  store/         SQLite、FTS5、向量与缓存的权威记录
  models/        Embedding Provider（HTTP / Mock）与空间身份
  vector/        USearch、vector_ops 重放与检查点
  retrieval/     查询分析、三路召回、RRF 融合
  context/       上下文组装、去重与长度预算
  api/           CLI 与 MCP stdio
```

职责边界：源码文件是原始事实来源；SQLite 是已采集快照与索引状态的权威记录；
USearch 是可重建的派生索引；Context Builder 只选择和组织代码，不生成解释。
