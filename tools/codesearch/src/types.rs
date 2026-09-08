//! 跨模块共享的基础类型与对外数据契约。
//!
//! CLI 与 MCP 共用这里的请求/响应结构，保证两条通路语义一致。
//! 所有 token 数都是估算值，字段 `estimated` 恒为 `true`，不要当成精确 tokenizer 结果。

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// 支持的语言。
///
/// 分三类：Tree-sitter 结构化解析、单文件组件（拆出内嵌块后委派子语言）、
/// 以及走兜底分块的标记与配置格式。配置格式仍然拥有独立取值，
/// 这样 `--language yaml` 之类的过滤才可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust（Tree-sitter 结构化解析）。
    Rust,
    /// TypeScript（Tree-sitter 结构化解析）。
    ///
    /// 显式重命名：`snake_case` 会得到 `type_script`，既与 [`Language::as_str`]
    /// 不一致，也无法被 [`Language::parse_filter`] 接受，Agent 把结果里的
    /// `language` 原样当作过滤值就会失败。
    #[serde(rename = "typescript")]
    TypeScript,
    /// TSX（Tree-sitter 结构化解析）。
    Tsx,
    /// JavaScript / JSX（Tree-sitter 结构化解析）。
    #[serde(rename = "javascript")]
    JavaScript,
    /// Python（Tree-sitter 结构化解析）。
    Python,
    /// Go（Tree-sitter 结构化解析）。
    Go,
    /// Java（Tree-sitter 结构化解析）。
    Java,
    /// C（Tree-sitter 结构化解析）。
    C,
    /// C++（Tree-sitter 结构化解析）。
    Cpp,
    /// C#（Tree-sitter 结构化解析）。
    #[serde(rename = "csharp")]
    CSharp,
    /// Ruby（Tree-sitter 结构化解析）。
    Ruby,
    /// PHP（Tree-sitter 结构化解析）。
    Php,
    /// Swift（Tree-sitter 结构化解析）。
    Swift,
    /// Kotlin（Tree-sitter 结构化解析）。
    Kotlin,
    /// Scala（Tree-sitter 结构化解析）。
    Scala,
    /// Lua（Tree-sitter 结构化解析）。
    Lua,
    /// Shell 脚本（Tree-sitter bash 语法）。
    Bash,
    /// CSS 及其预处理器方言（Tree-sitter 结构化解析）。
    Css,
    /// SQL（Tree-sitter 结构化解析）。
    Sql,
    /// Dart，包含 Flutter 类与方法。
    Dart,
    /// Elixir 模块、函数与宏。
    Elixir,
    /// Zig 函数与容器声明。
    Zig,
    /// Haskell 函数与类型声明。
    Haskell,
    /// HCL 与 Terraform 声明块。
    Hcl,
    /// GraphQL 类型、操作与片段。
    Graphql,
    /// Vue 单文件组件（拆出 script/style 后委派子语言）。
    Vue,
    /// Svelte 单文件组件（拆出 script/style 后委派子语言）。
    Svelte,
    /// HTML（脚本与样式结构化解析，其余标记保留原文）。
    Html,
    /// JSON（有界文本兜底）。
    Json,
    /// YAML（有界文本兜底）。
    Yaml,
    /// TOML（有界文本兜底）。
    Toml,
    /// Markdown（按标题分块）。
    Markdown,
    /// 其他纯文本（有界文本兜底）。
    Text,
}

impl Language {
    /// 全部取值。
    ///
    /// 一致性测试与 [`Language::parse_filter`] 的提示信息都由此生成，
    /// 新增变体时不会漏掉任何一处。
    pub const ALL: &'static [Language] = &[
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Python,
        Language::Go,
        Language::Java,
        Language::C,
        Language::Cpp,
        Language::CSharp,
        Language::Ruby,
        Language::Php,
        Language::Swift,
        Language::Kotlin,
        Language::Scala,
        Language::Lua,
        Language::Bash,
        Language::Css,
        Language::Sql,
        Language::Dart,
        Language::Elixir,
        Language::Zig,
        Language::Haskell,
        Language::Hcl,
        Language::Graphql,
        Language::Vue,
        Language::Svelte,
        Language::Html,
        Language::Json,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
        Language::Text,
    ];

    /// 稳定的字符串标识（写入数据库与 JSON 输出）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Python => "python",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Php => "php",
            Language::Swift => "swift",
            Language::Kotlin => "kotlin",
            Language::Scala => "scala",
            Language::Lua => "lua",
            Language::Bash => "bash",
            Language::Css => "css",
            Language::Sql => "sql",
            Language::Dart => "dart",
            Language::Elixir => "elixir",
            Language::Zig => "zig",
            Language::Haskell => "haskell",
            Language::Hcl => "hcl",
            Language::Graphql => "graphql",
            Language::Vue => "vue",
            Language::Svelte => "svelte",
            Language::Html => "html",
            Language::Json => "json",
            Language::Yaml => "yaml",
            Language::Toml => "toml",
            Language::Markdown => "markdown",
            Language::Text => "text",
        }
    }

    /// 由数据库或配置中的字符串解析语言。
    ///
    /// 除稳定标识外还接受常见别名与扩展名写法，方便直接把扩展名当过滤值使用。
    pub fn from_str_opt(value: &str) -> Option<Self> {
        let lowered = value.trim().to_ascii_lowercase();
        let language = match lowered.as_str() {
            "rust" | "rs" => Language::Rust,
            "typescript" | "ts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "javascript" | "js" | "jsx" => Language::JavaScript,
            "python" | "py" => Language::Python,
            "go" | "golang" => Language::Go,
            "java" => Language::Java,
            "c" => Language::C,
            "cpp" | "c++" | "cxx" | "cc" => Language::Cpp,
            "csharp" | "c#" | "cs" => Language::CSharp,
            "ruby" | "rb" => Language::Ruby,
            "php" => Language::Php,
            "swift" => Language::Swift,
            "kotlin" | "kt" => Language::Kotlin,
            "scala" => Language::Scala,
            "lua" => Language::Lua,
            "bash" | "sh" | "shell" | "zsh" => Language::Bash,
            "css" | "scss" | "less" | "sass" => Language::Css,
            "sql" => Language::Sql,
            "dart" => Language::Dart,
            "elixir" | "ex" | "exs" => Language::Elixir,
            "zig" => Language::Zig,
            "haskell" | "hs" => Language::Haskell,
            "hcl" | "tf" | "tfvars" | "terraform" => Language::Hcl,
            "graphql" | "gql" => Language::Graphql,
            "vue" => Language::Vue,
            "svelte" => Language::Svelte,
            "html" | "htm" => Language::Html,
            "json" => Language::Json,
            "yaml" | "yml" => Language::Yaml,
            "toml" => Language::Toml,
            "markdown" | "md" => Language::Markdown,
            "text" | "txt" | "plain" => Language::Text,
            _ => return None,
        };
        Some(language)
    }

    /// 解析用户提供的语言过滤值，非法值返回 [`Error::InvalidQuery`]。
    pub fn parse_filter(value: &str) -> Result<Self> {
        Self::from_str_opt(value).ok_or_else(|| {
            let available: Vec<&str> = Self::ALL.iter().map(|language| language.as_str()).collect();
            Error::InvalidQuery(format!(
                "不支持的语言 `{value}`；可用值: {}",
                available.join(", ")
            ))
        })
    }

    /// 是否由 Tree-sitter 做结构化解析（含单文件组件的委派解析）。
    pub fn is_structured(&self) -> bool {
        !matches!(
            self,
            Language::Json | Language::Yaml | Language::Toml | Language::Markdown | Language::Text
        )
    }
}

/// 代码块类别。用于展示、排序降权与上下文扩展决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// 自由函数。
    Function,
    /// 关联函数或方法。
    Method,
    /// struct / class 定义。
    Struct,
    /// enum 定义。
    Enum,
    /// trait / interface 定义。
    Trait,
    /// impl 块。
    Impl,
    /// 模块或命名空间。
    Module,
    /// 类型别名。
    TypeAlias,
    /// 常量或静态变量。
    Const,
    /// 具名声明式规则：CSS 选择器规则、SQL 索引等。
    Rule,
    /// 导入声明聚合块。
    Imports,
    /// 父节点的结构概览（只含签名，不重复子节点完整代码）。
    Overview,
    /// 超长定义按语法结构拆分出的片段。
    Part,
    /// Markdown 标题小节。
    Section,
    /// 纯文本兜底块。
    Text,
}

impl ChunkKind {
    /// 稳定的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            ChunkKind::Function => "function",
            ChunkKind::Method => "method",
            ChunkKind::Struct => "struct",
            ChunkKind::Enum => "enum",
            ChunkKind::Trait => "trait",
            ChunkKind::Impl => "impl",
            ChunkKind::Module => "module",
            ChunkKind::TypeAlias => "type_alias",
            ChunkKind::Const => "const",
            ChunkKind::Rule => "rule",
            ChunkKind::Imports => "imports",
            ChunkKind::Overview => "overview",
            ChunkKind::Part => "part",
            ChunkKind::Section => "section",
            ChunkKind::Text => "text",
        }
    }

    /// 由数据库字符串还原。
    pub fn from_str_opt(value: &str) -> Option<Self> {
        let kind = match value {
            "function" => ChunkKind::Function,
            "method" => ChunkKind::Method,
            "struct" => ChunkKind::Struct,
            "enum" => ChunkKind::Enum,
            "trait" => ChunkKind::Trait,
            "impl" => ChunkKind::Impl,
            "module" => ChunkKind::Module,
            "type_alias" => ChunkKind::TypeAlias,
            "const" => ChunkKind::Const,
            "rule" => ChunkKind::Rule,
            "imports" => ChunkKind::Imports,
            "overview" => ChunkKind::Overview,
            "part" => ChunkKind::Part,
            "section" => ChunkKind::Section,
            "text" => ChunkKind::Text,
            _ => return None,
        };
        Some(kind)
    }

    /// 全部取值，供一致性测试遍历。
    pub const ALL: &'static [ChunkKind] = &[
        ChunkKind::Function,
        ChunkKind::Method,
        ChunkKind::Struct,
        ChunkKind::Enum,
        ChunkKind::Trait,
        ChunkKind::Impl,
        ChunkKind::Module,
        ChunkKind::TypeAlias,
        ChunkKind::Const,
        ChunkKind::Rule,
        ChunkKind::Imports,
        ChunkKind::Overview,
        ChunkKind::Part,
        ChunkKind::Section,
        ChunkKind::Text,
    ];

    /// 是否代表一个可命名的定义（用于符号表与上下文扩展）。
    pub fn is_definition(&self) -> bool {
        matches!(
            self,
            ChunkKind::Function
                | ChunkKind::Method
                | ChunkKind::Struct
                | ChunkKind::Enum
                | ChunkKind::Trait
                | ChunkKind::Module
                | ChunkKind::TypeAlias
                | ChunkKind::Const
                | ChunkKind::Rule
        )
    }
}

/// 召回通路标识。出现在结果的 `matched_by` 中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchSource {
    /// 完整符号精确命中。
    Symbol,
    /// 路径命中。
    Path,
    /// 源码字面量子串命中。
    Literal,
    /// FTS5 全文命中。
    Fts,
    /// 向量近邻命中。
    Vector,
    /// 由上下文扩展补入（父作用域、被包含定义等）。
    Expanded,
}

impl MatchSource {
    /// 稳定的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchSource::Symbol => "symbol",
            MatchSource::Path => "path",
            MatchSource::Literal => "literal",
            MatchSource::Fts => "fts",
            MatchSource::Vector => "vector",
            MatchSource::Expanded => "expanded",
        }
    }
}

/// 作用域过滤条件。CLI 参数、MCP 入参与查询内联语法最终都归一到这里。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeFilter {
    /// 仅保留匹配这些 glob 的路径。
    pub include: Vec<String>,
    /// 排除匹配这些 glob 的路径。
    pub exclude: Vec<String>,
    /// 仅保留这些语言。
    pub languages: Vec<Language>,
}

impl ScopeFilter {
    /// 是否没有任何过滤条件。
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty() && self.languages.is_empty()
    }
}

/// 搜索请求。
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// 原始查询文本。
    pub query: String,
    /// 作用域过滤。
    pub scope: ScopeFilter,
    /// 期望返回的结果数（受配置上限约束）。
    pub max_results: Option<usize>,
    /// token 预算（估算，受配置上限约束）。
    pub token_budget: Option<usize>,
    /// 检索模式。
    pub mode: SearchMode,
}

/// 检索模式。第一版只有 `fast`；不提供行为相同的假模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// 兼容的默认入口：混合召回与上下文组装，可由部署配置显式启用重排。
    #[default]
    Fast,
}

impl SearchMode {
    /// 稳定的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Fast => "fast",
        }
    }
}

/// 单条搜索结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// 指向确定源码版本的引用，可直接交给 `read_code`。
    pub code_ref: String,
    /// `/` 分隔的相对路径。
    pub path: String,
    /// 语言。
    pub language: Language,
    /// 限定符号名，可能为空（例如纯文本块）。
    pub symbol: Option<String>,
    /// 代码块类别。
    pub kind: ChunkKind,
    /// 内容哈希（blake3 全长十六进制），与 `code_ref` 中的前缀一致。
    pub file_hash: String,
    /// 文件版本自增 ID。
    pub revision_id: i64,
    /// 起始行（从 1 开始，含）。
    pub start_line: u32,
    /// 结束行（从 1 开始，含）。
    pub end_line: u32,
    /// 命中来源，按稳定顺序排列。
    pub matched_by: Vec<MatchSource>,
    /// 融合分数，仅表示排序依据，不是正确概率。
    pub score: f64,
    /// 原始源码（未做拆词或规范化）。
    pub content: String,
    /// 内容是否因预算被截断。
    pub truncated: bool,
    /// 估算 token 数。
    pub estimated_tokens: usize,
}

/// 索引新鲜度与降级状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchStatus {
    /// 是否处于降级状态（向量通路不可用或语义索引未追上）。
    pub degraded: bool,
    /// 降级原因，面向人类阅读。
    pub reasons: Vec<String>,
    /// 本次查询是否真实使用了向量召回。
    pub vector_search_used: bool,
    /// 当前等待 Embedding 的代码块数量。
    pub pending_embeddings: i64,
    /// 标记为失败、不再重试的代码块数量。
    pub failed_embeddings: i64,
    /// 新鲜度语义，第一版恒为 `best_effort`。
    pub freshness: String,
}

/// 搜索响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    /// 目录修订号：每次提交目录状态变更递增。
    pub catalog_epoch: i64,
    /// 使用的检索模式。
    pub mode: SearchMode,
    /// 索引状态与降级信息。
    pub status: SearchStatus,
    /// 结果列表。
    pub results: Vec<SearchResult>,
    /// 结果内容合计估算 token 数。
    pub total_estimated_tokens: usize,
    /// token 预算（估算）。
    pub token_budget: usize,
    /// token 统计是否为估算值，恒为 `true`。
    pub estimated: bool,
    /// 是否有结果因预算被丢弃或截断。
    pub truncated: bool,
    /// 查询分析结果摘要，便于诊断排序原因。
    pub query_plan: QueryPlanInfo,
}

/// 查询分析摘要。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryPlanInfo {
    /// 识别出的查询类别。
    pub kinds: Vec<String>,
    /// 用于精确召回的完整符号。
    pub symbols: Vec<String>,
    /// 用于路径召回的片段。
    pub paths: Vec<String>,
    /// 用于字面量召回的片段。
    pub literals: Vec<String>,
    /// 拆词后用于 FTS 的词条。
    pub terms: Vec<String>,
    /// 识别出的意图提示。
    pub intents: Vec<String>,
    /// 生效的作用域过滤。
    pub scope: ScopeFilter,
}

/// `read_code` 的扩展方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadExpand {
    /// 只返回引用对应的范围。
    #[default]
    None,
    /// 扩展到包含该范围的函数或类型定义。
    ContainingSymbol,
    /// 返回引用对应版本的整个文件。
    File,
}

impl ReadExpand {
    /// 由字符串解析，非法值返回 [`Error::InvalidQuery`]。
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Ok(ReadExpand::None),
            "containing_symbol" | "symbol" => Ok(ReadExpand::ContainingSymbol),
            "file" => Ok(ReadExpand::File),
            other => Err(Error::InvalidQuery(format!(
                "不支持的 expand `{other}`；可用值: none, containing_symbol, file"
            ))),
        }
    }
}

/// 读取请求。
#[derive(Debug, Clone)]
pub struct ReadRequest {
    /// 由搜索结果返回的 `code_ref`。
    pub code_ref: String,
    /// 扩展方式。
    pub expand: ReadExpand,
    /// token 预算（估算）。
    pub token_budget: Option<usize>,
}

/// 读取响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResponse {
    /// 归一化后的引用（扩展后可能指向更大范围）。
    pub code_ref: String,
    /// 相对路径。
    pub path: String,
    /// 语言。
    pub language: Language,
    /// 限定符号名。
    pub symbol: Option<String>,
    /// 内容哈希。
    pub file_hash: String,
    /// 文件版本 ID。
    pub revision_id: i64,
    /// 起始行（含）。
    pub start_line: u32,
    /// 结束行（含）。
    pub end_line: u32,
    /// 该版本文件的总行数。
    pub file_line_count: u32,
    /// 原始源码。
    pub content: String,
    /// 是否被截断。
    pub truncated: bool,
    /// 被省略的行范围描述（截断时给出）。
    pub omitted: Option<String>,
    /// 估算 token 数。
    pub estimated_tokens: usize,
    /// token 统计是否为估算值，恒为 `true`。
    pub estimated: bool,
    /// 该引用对应的版本是否仍是当前磁盘采集版本。
    pub is_current: bool,
}

/// `status` 命令的输出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    /// workspace 身份。
    pub workspace_id: String,
    /// 规范化后的根目录。
    pub root: String,
    /// 索引数据目录。
    pub data_dir: String,
    /// 数据库 schema 版本。
    pub schema_version: i64,
    /// 目录修订号。
    pub catalog_epoch: i64,
    /// 处于活跃状态的文件数。
    pub active_files: i64,
    /// 因规则或读取问题被跳过的文件数。
    pub skipped_files: i64,
    /// 当前有效代码块数。
    pub current_chunks: i64,
    /// 已完成 Embedding 的当前代码块数。
    pub embeddings_ready: i64,
    /// 等待 Embedding 的当前代码块数。
    pub embeddings_pending: i64,
    /// 失败且不再重试的当前代码块数。
    pub embeddings_failed: i64,
    /// Embedding 缓存条目数。
    pub cache_entries: i64,
    /// 当前 Embedding 空间描述；未配置模型时为 `None`。
    pub embedding_space: Option<EmbeddingSpaceInfo>,
    /// 最近一次扫描信息。
    pub last_scan: Option<ScanInfo>,
    /// 向量内存已应用的更新日志序号。
    pub vector_applied_seq: i64,
    /// 已持久化检查点对应的序号。
    pub vector_durable_seq: i64,
    /// 更新日志中尚未应用的操作数。
    pub vector_ops_backlog: i64,
    /// 向量索引中的向量数量。
    pub vector_count: i64,
    /// 是否处于降级状态。
    pub degraded: bool,
    /// 降级或错误原因。
    pub diagnostics: Vec<String>,
}

/// Embedding 空间的可观察描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingSpaceInfo {
    /// 空间身份哈希。
    pub id: String,
    /// Provider 类型。
    pub provider: String,
    /// 模型名。
    pub model: String,
    /// 缓存命名空间（模型修订）。
    pub cache_namespace: String,
    /// 向量维度。
    pub dimensions: usize,
    /// 距离度量。
    pub metric: String,
    /// 归一化规则。
    pub normalize: String,
    /// 输入模板版本。
    pub template_version: u32,
}

/// 最近一次扫描信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanInfo {
    /// 完成时间（UNIX 秒）。
    pub finished_at: i64,
    /// 扫描到的候选文件数。
    pub scanned: i64,
    /// 内容变化并重新索引的文件数。
    pub changed: i64,
    /// 删除的文件数。
    pub removed: i64,
    /// 跳过的文件数。
    pub skipped: i64,
    /// 耗时毫秒。
    pub duration_ms: i64,
}

/// 一次索引操作的结果摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    /// 目录修订号。
    pub catalog_epoch: i64,
    /// 扫描到的候选文件数。
    pub scanned_files: i64,
    /// 新建或更新的文件数。
    pub changed_files: i64,
    /// 未变化因而跳过重解析的文件数。
    pub unchanged_files: i64,
    /// 被删除的文件数。
    pub removed_files: i64,
    /// 被跳过的文件及原因。
    pub skipped: Vec<crate::workspace::scan::SkippedFile>,
    /// 当前有效代码块数。
    pub current_chunks: i64,
    /// 目录索引耗时毫秒。
    pub catalog_duration_ms: i64,
    /// 本次是否因分块器升级而强制全量重新分块。
    ///
    /// 为真时本次耗时会明显高于普通增量索引，属于预期行为。
    #[serde(default)]
    pub rechunked: bool,
    /// 本次新生成的 Embedding 数量。
    pub embeddings_created: i64,
    /// 命中缓存复用的 Embedding 数量。
    pub embeddings_cached: i64,
    /// 仍在等待 Embedding 的代码块数。
    pub embeddings_pending: i64,
    /// 失败的代码块数。
    pub embeddings_failed: i64,
    /// 语义索引阶段是否被跳过（离线或未配置模型）。
    pub semantic_skipped: bool,
    /// 语义阶段说明。
    pub semantic_note: Option<String>,
    /// 是否处于降级状态。
    pub degraded: bool,
}

/// 估算文本的 token 数。
///
/// 没有精确 tokenizer，因此使用保守启发式：ASCII 约 4 字符 1 token，
/// CJK 字符按 1 token 计，其他非 ASCII 按 2 字符 1 token 计。
/// 返回值只用于预算控制与展示，任何对外字段都必须标记 `estimated = true`。
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    let estimate = ascii.div_ceil(4) + cjk + other.div_ceil(2);
    if text.is_empty() {
        0
    } else {
        estimate.max(1)
    }
}

/// 是否属于需要按字符切分的中日韩表意字符区间。
pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF      // 日文平假名与片假名
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0x20000..=0x2A6DF  // CJK 扩展 B
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_round_trips_through_str() {
        for language in Language::ALL {
            assert_eq!(Language::from_str_opt(language.as_str()), Some(*language));
        }
        assert_eq!(Language::from_str_opt("RS"), Some(Language::Rust));
        assert_eq!(Language::from_str_opt("golang"), Some(Language::Go));
        assert_eq!(Language::from_str_opt("scss"), Some(Language::Css));
        assert!(Language::parse_filter("cobol").is_err());
    }

    /// `ALL` 必须真的覆盖每一个变体。
    ///
    /// 一致性测试、`parse_filter` 的可用值提示都由 `ALL` 生成；漏登记一个变体
    /// 就会让它绕过全部校验，因此这里用稳定标识去重来反查数量。
    #[test]
    fn language_all_has_no_duplicates_and_covers_every_variant() {
        let mut seen: Vec<&str> = Language::ALL
            .iter()
            .map(|language| language.as_str())
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "ALL 中存在重复语言");

        // 穷举匹配：新增变体而忘记登记 ALL 时，这里不会编译通过。
        for language in Language::ALL {
            let covered = match language {
                Language::Rust
                | Language::TypeScript
                | Language::Tsx
                | Language::JavaScript
                | Language::Python
                | Language::Go
                | Language::Java
                | Language::C
                | Language::Cpp
                | Language::CSharp
                | Language::Ruby
                | Language::Php
                | Language::Swift
                | Language::Kotlin
                | Language::Scala
                | Language::Lua
                | Language::Bash
                | Language::Css
                | Language::Sql
                | Language::Dart
                | Language::Elixir
                | Language::Zig
                | Language::Haskell
                | Language::Hcl
                | Language::Graphql
                | Language::Vue
                | Language::Svelte
                | Language::Html
                | Language::Json
                | Language::Yaml
                | Language::Toml
                | Language::Markdown
                | Language::Text => true,
            };
            assert!(covered);
        }
    }

    /// JSON 表示、`as_str` 与过滤值解析必须是同一套标识。
    ///
    /// 三者一旦分叉，Agent 把搜索结果里的 `language` 直接当作过滤值就会报错。
    #[test]
    fn language_json_matches_as_str_and_is_accepted_as_a_filter() {
        for language in Language::ALL {
            let json = serde_json::to_value(language).expect("序列化");
            let text = json.as_str().expect("语言应序列化为字符串");
            assert_eq!(text, language.as_str(), "JSON 表示与 as_str 不一致");
            assert_eq!(
                Language::parse_filter(text).expect("JSON 值必须能当作过滤值"),
                *language
            );
        }
    }

    /// 其余对外枚举同样要求 JSON 表示与稳定标识一致。
    #[test]
    fn chunk_kind_and_match_source_json_match_as_str() {
        for kind in ChunkKind::ALL {
            let json = serde_json::to_value(kind).expect("序列化");
            assert_eq!(json.as_str(), Some(kind.as_str()));
        }
        for source in [
            MatchSource::Symbol,
            MatchSource::Path,
            MatchSource::Literal,
            MatchSource::Fts,
            MatchSource::Vector,
            MatchSource::Expanded,
        ] {
            let json = serde_json::to_value(source).expect("序列化");
            assert_eq!(json.as_str(), Some(source.as_str()));
        }
        let json = serde_json::to_value(SearchMode::Fast).expect("序列化");
        assert_eq!(json.as_str(), Some(SearchMode::Fast.as_str()));
    }

    #[test]
    fn chunk_kind_round_trips_through_str() {
        for kind in ChunkKind::ALL {
            assert_eq!(ChunkKind::from_str_opt(kind.as_str()), Some(*kind));
        }
        assert_eq!(ChunkKind::from_str_opt("nope"), None);
    }

    #[test]
    fn token_estimate_separates_ascii_and_cjk() {
        assert_eq!(estimate_tokens(""), 0);
        // 8 个 ASCII 字符约 2 token。
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        // 中文按字符计。
        assert_eq!(estimate_tokens("重试逻辑"), 4);
        // 混合内容相加。
        assert_eq!(estimate_tokens("fn 重试()"), estimate_tokens("fn ()") + 2);
    }

    #[test]
    fn read_expand_parsing_is_strict() {
        assert_eq!(ReadExpand::parse("none").expect("none"), ReadExpand::None);
        assert_eq!(
            ReadExpand::parse("containing_symbol").expect("symbol"),
            ReadExpand::ContainingSymbol
        );
        assert_eq!(ReadExpand::parse("file").expect("file"), ReadExpand::File);
        assert!(ReadExpand::parse("everything").is_err());
    }
}
