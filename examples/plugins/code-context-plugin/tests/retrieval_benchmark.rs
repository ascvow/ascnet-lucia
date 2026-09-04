//! Phase 14 — Retrieval Benchmark。
//!
//! **Fixture 测试**使用精心设计的合成仓库验证稳定的基线检索质量。
//! **真实仓库测试**直接索引当前 Lucia 仓库中全部受支持的文本与源码文件，在完整仓库
//! 条件下衡量 Recall@K、MRR，并作为默认测试执行。
//!
//! 单独运行真实仓库测试：
//!
//! ```sh
//! cargo test -p code-context-plugin --test retrieval_benchmark real_repo -- --nocapture
//! ```

use code_context_plugin::{
    classify, estimate_tokens, CodeContextPlugin, IndexStatus, PathClass, SearchResult,
};

use agent_plugin::{ActivationContext, AgentPlugin, ToolCall};
use serde_json::json;

// ===========================================================================
// Fixture 仓库（合成，CI 使用）
// ===========================================================================

/// 构建 fixture 仓库的文件列表。每个文件约 30–60 行，覆盖不同领域。
fn fixture_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "src/config.rs",
            r#"//! 插件配置解析。
//!
//! 配置经宿主的激活元数据下发。

/// 语义嵌入模式。
#[derive(Debug, Clone, Copy)]
pub enum EmbeddingMode {
    /// 本机推理，源码不出本机。
    Local,
    /// 用户自配的 OpenAI 兼容嵌入端点。
    Api,
}

/// 插件配置。
pub struct PluginConfig {
    /// 引擎是否启用。
    pub enabled: bool,
    /// 语义嵌入模式。
    pub embedding_mode: EmbeddingMode,
    /// 嵌入模型名称。
    pub embedding_model: String,
    /// 期望的向量维度。
    pub embedding_dimension: u32,
}

impl PluginConfig {
    /// 从宿主激活元数据解析配置。
    pub fn from_metadata(metadata: &std::collections::HashMap<String, String>) -> Result<Self, String> {
        let enabled = metadata.get("enabled").map_or(true, |v| v == "true");
        Ok(Self {
            enabled,
            embedding_mode: EmbeddingMode::Api,
            embedding_model: "text-embedding-3-small".into(),
            embedding_dimension: 1536,
        })
    }
}
"#,
        ),
        (
            "src/embedding.rs",
            r#"//! 嵌入向量 Provider 接口。
//!
//! 定义 EmbeddingProvider trait 与 API/Local 两种实现的选择逻辑。

/// 嵌入空间标识。
pub struct EmbeddingSpace {
    /// 模型名称。
    pub model: String,
    /// 向量维度。
    pub dimension: u32,
}

/// 嵌入 provider trait。
pub trait EmbeddingProvider {
    /// 返回嵌入空间；未配置时返回 None。
    fn space(&self) -> Option<EmbeddingSpace>;

    /// 对文档批次计算嵌入向量。
    fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, String>;

    /// 对查询计算嵌入向量。
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, String>;

    /// 健康检查。
    fn health_check(&self) -> Result<(), String>;
}

/// API 嵌入 Provider。
///
/// 使用 OpenAI 兼容的嵌入端点。请求通过宿主受限 HTTP 能力发出，
/// 插件看不到地址与凭证。
pub struct ApiEmbeddingProvider {
    pub model: String,
    pub dimension: u32,
}

/// 根据配置选择嵌入 provider。
pub fn select_provider(mode: &str) -> Box<dyn EmbeddingProvider> {
    todo!("根据 mode 选择 Api 或 Local provider")
}
"#,
        ),
        (
            "src/error.rs",
            r#"//! 领域错误定义。

use std::fmt;

/// Code Context Engine 领域错误。
#[derive(Debug)]
pub enum EngineError {
    /// 宿主能力不可用。
    CapabilityUnavailable(String),
    /// 持久存储读写失败。
    StorageFailed(String),
    /// 索引 manifest 损坏。
    InvalidManifest(String),
    /// 索引尚未建立。
    NotIndexed,
    /// 嵌入能力不可用。
    EmbeddingUnavailable(String),
    /// 配置无效。
    InvalidConfiguration(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityUnavailable(d) => write!(f, "宿主能力不可用：{d}"),
            Self::StorageFailed(d) => write!(f, "存储失败：{d}"),
            Self::InvalidManifest(d) => write!(f, "manifest 无效：{d}"),
            Self::NotIndexed => write!(f, "索引尚未建立"),
            Self::EmbeddingUnavailable(d) => write!(f, "嵌入不可用：{d}"),
            Self::InvalidConfiguration(d) => write!(f, "配置无效：{d}"),
        }
    }
}
"#,
        ),
        (
            "src/index.rs",
            r#"//! 结构索引与索引构建。
//!
//! StructuralIndex 维护文件记录、代码块和符号表。IndexBuild 流水线
//! 把扫描器与解析器串联，复用 ScanBudget 分片游标。

/// 文件记录。
pub struct FileRecord {
    /// 仓库相对路径。
    pub path: String,
    /// 内容哈希，用于增量判断。
    pub content_hash: u64,
    /// 语言标识。
    pub language: String,
}

/// 索引统计。
pub struct IndexStats {
    /// 已索引的文件数。
    pub files: u32,
    /// 代码块数。
    pub chunks: u32,
    /// 符号数。
    pub symbols: u32,
}

/// 结构索引。
///
/// 内存驻留，不落盘：宿主 8 MiB JSON 存储装不下 chunk 与符号表。
/// 进程重启后按需重建，哈希表用于增量判断。
pub struct StructuralIndex {
    files: Vec<FileRecord>,
}

impl StructuralIndex {
    /// 幂等写入文件的代码块与符号。
    pub fn upsert_file(&mut self, path: &str, chunks: Vec<()>) {
        todo!()
    }

    /// 删除文件及其关联的代码块与符号。
    pub fn remove_file(&mut self, path: &str) {
        self.files.retain(|f| f.path != path);
    }

    /// 返回索引规模统计。
    pub fn stats(&self) -> IndexStats {
        IndexStats { files: self.files.len() as u32, chunks: 0, symbols: 0 }
    }
}

/// 索引构建流水线。
pub struct IndexBuildPipeline {
    /// 扫描预算。
    pub budget: ScanBudget,
}

/// 分片扫描预算。
pub struct ScanBudget {
    /// 最大宿主调用次数。
    pub max_host_calls: u32,
    /// 最大读取字节数。
    pub max_bytes_read: u64,
}
"#,
        ),
        (
            "src/parser.rs",
            r#"//! 代码解析与声明提取。
//!
//! 使用现成的纯 Rust 解析器（syn、oxc、rustpython-parser、gosyn）从源文件中
//! 提取结构化声明。归一化层把各家 AST 折算成统一的 Declaration 结构。

/// 一条归一化的声明。
pub struct Declaration {
    /// 符号短名。
    pub name: String,
    /// 限定名，例如 `PermissionManager::check`。
    pub qualified_name: String,
    /// 声明种类。
    pub kind: SymbolKind,
    /// 起始行。
    pub start_line: u32,
    /// 结束行。
    pub end_line: u32,
    /// 声明签名。
    pub signature: Option<String>,
    /// 文档注释。
    pub doc_comment: Option<String>,
}

/// 符号种类。
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Constant,
}

/// 解析结果。
pub struct ParseOutput {
    /// 提取到的声明列表。
    pub declarations: Vec<Declaration>,
    /// 文件级 import 列表。
    pub imports: Vec<String>,
}

/// 解析源文件。
///
/// 按语言选择适配器，解析失败时降级为文本分块。
pub fn parse_file(path: &str, language: &str, content: &str) -> ParseOutput {
    match language {
        "rust" => parse_rust(content),
        "typescript" | "javascript" => parse_typescript(content),
        "python" => parse_python(content),
        "go" => parse_go(content),
        _ => text_fallback(content),
    }
}

fn parse_rust(content: &str) -> ParseOutput { todo!() }
fn parse_typescript(content: &str) -> ParseOutput { todo!() }
fn parse_python(content: &str) -> ParseOutput { todo!() }
fn parse_go(content: &str) -> ParseOutput { todo!() }

/// 文本分块兜底：无声明的文件按空行切分为连续段落。
fn text_fallback(content: &str) -> ParseOutput {
    ParseOutput { declarations: vec![], imports: vec![] }
}
"#,
        ),
        (
            "src/retrieval.rs",
            r#"//! BM25 词法检索与符号检索。
//!
//! 两路检索产出内部候选 SearchCandidate，经 RRF 融合后交给上下文构建。

/// BM25 参数。
pub struct BM25Config {
    /// 词频饱和参数。
    pub k1: f32,
    /// 文档长度归一化参数。
    pub b: f32,
}

impl Default for BM25Config {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

/// BM25 倒排索引。
///
/// 索引正文但不驻留正文：倒排索引保存 term → (chunk, 词频)，
/// 正文切词后即丢弃。字段加权通过重复计入词频实现：
/// 符号名 5 倍、限定名 3 倍、路径/签名/文档 2 倍、正文 1 倍。
pub struct LexicalIndex {
    /// 倒排表。
    postings: Vec<(String, Vec<(u32, u32)>)>,
    /// 文档平均长度。
    avg_doc_len: f32,
}

impl LexicalIndex {
    /// 执行 BM25 检索。
    pub fn search(&self, query: &str, path_scope: Option<&str>, limit: usize) -> Vec<(&str, f32)> {
        todo!()
    }
}

/// 符号索引。
///
/// 两份排序下标数组 + partition_point 二分。四档匹配：
/// 精确限定名 1.0 / 精确短名 0.85 / 限定名后缀 0.75 / 短名前缀 0.4。
pub struct SymbolIndex {
    symbols: Vec<SymbolEntry>,
}

/// 符号表条目。
pub struct SymbolEntry {
    /// 符号短名。
    pub name: String,
    /// 限定名。
    pub qualified_name: String,
    /// 所属代码块 ID。
    pub chunk_id: String,
}

impl SymbolIndex {
    /// 按名称查询符号。
    pub fn search(&self, query: &str, path_scope: Option<&str>, limit: usize) -> Vec<(&str, f32)> {
        todo!()
    }
}

/// 代码专用切词：驼峰、蛇形、路径、限定名，中日韩 bigram。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if !word.is_empty() {
            tokens.push(word.to_lowercase());
        }
    }
    tokens
}
"#,
        ),
        (
            "src/fusion.rs",
            r#"//! RRF 候选融合。
//!
//! Reciprocal Rank Fusion 用排名而非分数融合多路候选。各通道的分数量纲不可比
//! （BM25 无上界、符号分 0..1、语义是余弦相似度），RRF 只用排名，在任意通道
//! 子集上都成立。

/// RRF 平滑常量。
const RRF_K: f32 = 60.0;

/// 融合配置。
pub struct FusionConfig {
    /// RRF 平滑常量。
    pub rrf_k: f32,
    /// 精确符号匹配提升（rank-1 当量）。
    pub exact_symbol_boost: f32,
    /// 短名精确匹配提升。
    pub name_match_boost: f32,
    /// 路径匹配提升。
    pub path_boost: f32,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            rrf_k: RRF_K,
            exact_symbol_boost: 3.0,
            name_match_boost: 1.5,
            path_boost: 1.0,
        }
    }
}

/// 一条融合后的检索结果。
pub struct SearchResult {
    /// 代码块 ID。
    pub chunk_id: String,
    /// 仓库相对路径。
    pub file_path: String,
    /// 符号短名。
    pub symbol_name: Option<String>,
    /// 融合分数。
    pub score: f32,
}

/// 融合多路候选。
///
/// 在任意通道子集上成立：空输入、单通道、三通道全部正常工作。
/// 新增通道是纯加法，不需要改动融合代码。
pub fn fuse(channels: &[Vec<()>], query: &str, limit: usize) -> Vec<SearchResult> {
    todo!()
}
"#,
        ),
        (
            "src/context.rs",
            r#"//! 上下文构建：把融合结果打包成 Agent 可用的上下文。
//!
//! 父子抑制 → 区间合并 → 多样性约束 → 正文回读与一致性校验 → token 预算打包。

/// 上下文配置。
pub struct ContextConfig {
    /// token 预算上限。
    pub token_budget: u32,
    /// 最多返回条目数。
    pub max_results: usize,
    /// 同一文件最多贡献的条目数（多样性约束）。
    pub max_per_file: usize,
    /// 单条片段最大行数。
    pub max_snippet_lines: u32,
}

/// 一条打包好的上下文条目。
pub struct ContextEntry {
    /// 仓库相对路径。
    pub file_path: String,
    /// 起始行。
    pub start_line: u32,
    /// 结束行。
    pub end_line: u32,
    /// 代码片段正文。
    pub snippet: String,
    /// 估算 token 数。
    pub estimated_tokens: u32,
}

/// 上下文构建统计。
pub struct ContextStats {
    /// 输入候选数。
    pub candidates: u32,
    /// 最终返回条目数。
    pub returned: u32,
    /// 被父子抑制丢弃的条目数。
    pub suppressed: u32,
    /// 被 token 预算丢弃的条目数。
    pub dropped_by_budget: u32,
}

/// 估算文本的 token 数。
///
/// ASCII 约每 4 字符一个 token，CJK 约每字一个 token。
/// 估算偏保守（向上取整），宁可少放一条也不超预算。
pub fn estimate_tokens(text: &str) -> u32 {
    let ascii = text.chars().filter(|c| !is_cjk(*c)).count();
    let cjk = text.chars().filter(|c| is_cjk(*c)).count();
    (ascii / 4 + cjk + 1) as u32
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}')
}

/// 把融合结果打包成上下文。
pub fn build_context(results: Vec<()>, config: &ContextConfig) -> Vec<ContextEntry> {
    todo!()
}
"#,
        ),
        (
            "src/storage.rs",
            r#"//! 索引元数据持久化存储。
//!
//! 按 adr-storage.md 分流：manifest 与哈希表落 Host JSON KV，
//! 索引本体（chunk、符号表、倒排）内存驻留。8 MiB JSON 装不下索引本体。

/// 索引存储。
pub struct IndexStore {
    /// 是否有可用的项目作用域存储。
    available: bool,
}

/// manifest 加载结果。
pub enum ManifestLoad {
    /// 成功加载。
    Loaded(IndexManifest),
    /// 存在但无法解析。
    Unreadable(String),
    /// 不存在。
    Missing,
}

/// 索引 manifest。
pub struct IndexManifest {
    /// schema 版本。
    pub schema_version: u32,
    /// 仓库身份。
    pub repository_identity: String,
    /// 嵌入空间标识。
    pub embedding_space_id: Option<String>,
    /// 创建时间。
    pub created_at: u64,
}

impl IndexStore {
    /// 保存 manifest。
    pub fn save_manifest(&self, manifest: &IndexManifest) -> Result<(), String> {
        if !self.available { return Err("无可用存储".into()); }
        todo!()
    }

    /// 加载 manifest。
    pub fn load_manifest(&self) -> ManifestLoad {
        todo!()
    }

    /// 保存文件哈希表，用于增量构建判断。
    pub fn save_file_hashes(&self, hashes: &std::collections::HashMap<String, u64>) -> Result<(), String> {
        if !self.available { return Err("无可用存储".into()); }
        todo!()
    }

    /// 加载文件哈希表。
    pub fn load_file_hashes(&self) -> std::collections::HashMap<String, u64> {
        todo!()
    }

    /// 保存语义向量快照（int8 量化 + base64 编码）。
    pub fn save_semantic(&self, snapshot: &SemanticSnapshot) -> Result<(), String> {
        if !self.available { return Err("无可用存储".into()); }
        todo!()
    }
}

/// 语义向量快照。
pub struct SemanticSnapshot {
    /// 嵌入空间标识。
    pub space_id: String,
    /// 量化后的向量数据。
    pub data: Vec<u8>,
}
"#,
        ),
        (
            "src/semantic.rs",
            r#"//! 语义向量索引。
//!
//! 内存驻留的 chunk 向量存储，支持按余弦相似度检索。
//! 线性精确扫描而非 ANN：代码仓库 chunk 在数千到数万量级，线性扫描常数更小
//! 且结果精确、行为确定。

/// 语义索引。
pub struct SemanticIndex {
    vectors: Vec<(String, Vec<f32>)>,
}

impl SemanticIndex {
    /// 创建空索引。
    pub fn new() -> Self {
        Self { vectors: Vec::new() }
    }

    /// 写入或更新一条向量。写入时归一化，检索用点积代替余弦。
    pub fn upsert(&mut self, chunk_id: &str, vector: &[f32]) {
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 || norm.is_nan() { return; }
        let normalized: Vec<f32> = vector.iter().map(|x| x / norm).collect();
        self.vectors.push((chunk_id.to_string(), normalized));
    }

    /// 按余弦相似度检索最近邻。
    pub fn search(&self, query_vector: &[f32], limit: usize) -> Vec<(String, f32)> {
        let mut scores: Vec<(String, f32)> = self.vectors.iter()
            .map(|(id, vec)| {
                let dot: f32 = vec.iter().zip(query_vector).map(|(a, b)| a * b).sum();
                (id.clone(), dot)
            })
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(limit);
        scores
    }

    /// 检查索引是否可用于检索。
    pub fn is_ready(&self) -> bool {
        !self.vectors.is_empty()
    }
}
"#,
        ),
        (
            "src/incremental.rs",
            r#"//! 增量索引：事件驱动的脏路径集合与文件级增量重建。
//!
//! Agent 的工具调用成功写入文件后，将路径加入有界脏集合。下一次 code_search
//! 检索时检查脏集合，对变更的文件执行增量重建。

/// 最大脏路径数。
const MAX_DIRTY_PATHS: usize = 4096;

/// 最大待处理写入数。
const MAX_PENDING_WRITES: usize = 1024;

/// 一条待确认的写入操作。
pub struct PendingWrite {
    /// 运行 ID。
    pub run_id: String,
    /// 工具名称。
    pub tool_name: String,
    /// 涉及的仓库相对路径。
    pub paths: Vec<String>,
}

/// 判断工具是否属于写入类。
pub fn is_write_tool(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "rename_file" | "create_file" | "patch_file")
}

/// 从工具调用参数中提取仓库相对路径。
pub fn extract_paths(tool_name: &str, args: &serde_json::Value, workspace_root: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        if let Some(relative) = strip_workspace_prefix(path, workspace_root) {
            paths.push(relative);
        }
    }
    paths
}

/// 安全地剥离工作区前缀。
fn strip_workspace_prefix(path: &str, workspace_root: &str) -> Option<String> {
    path.strip_prefix(workspace_root)
        .map(|s| s.trim_start_matches('/').to_string())
        .or_else(|| {
            if !path.starts_with('/') { Some(path.to_string()) } else { None }
        })
}
"#,
        ),
        (
            "src/tool.rs",
            r#"//! code_search 工具契约。
//!
//! 参数只暴露意图（query、max_results、token_budget、path_scope），
//! 不暴露检索策略参数（BM25 权重、RRF 常数、召回深度）——那些属于插件内部。

/// 工具名。
pub const TOOL_NAME: &str = "code_search";

/// 默认返回结果数。
const DEFAULT_MAX_RESULTS: u64 = 10;
/// 默认 token 预算。
const DEFAULT_TOKEN_BUDGET: u64 = 8000;
/// 查询字符串最大长度。
const MAX_QUERY_CHARS: usize = 1000;

/// 一次已校验的检索请求。
pub struct CodeSearchRequest {
    /// 自然语言或标识符查询。
    pub query: String,
    /// 期望返回的结果数上限。
    pub max_results: u64,
    /// token 预算。
    pub token_budget: u64,
    /// 路径范围。
    pub path_scope: Vec<String>,
}

impl CodeSearchRequest {
    /// 解析并校验模型提交的参数。
    ///
    /// 越界数值收敛到合法区间；空白 query 报错。
    pub fn parse(args: &serde_json::Value) -> Result<Self, String> {
        let query = args.get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        if query.is_empty() { return Err("query 不能为空".into()); }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err("query 超长".into());
        }
        Ok(Self {
            query: query.to_string(),
            max_results: DEFAULT_MAX_RESULTS,
            token_budget: DEFAULT_TOKEN_BUDGET,
            path_scope: Vec::new(),
        })
    }
}
"#,
        ),
        (
            "src/repository.rs",
            r#"//! 仓库扫描器。
//!
//! 从宿主注入的可信工作区根递归扫描，产出 IndexableFile 列表。
//! 含忽略规则（.gitignore + .agentignore）、语言识别、敏感文件排除与 fuel 分片游标。

/// 一个可索引的文件。
pub struct IndexableFile {
    /// 仓库相对路径。
    pub path: String,
    /// 语言标识。
    pub language: String,
    /// 内容哈希。
    pub content_hash: u64,
    /// 文件行数。
    pub line_count: u32,
}

/// 扫描进度。
#[derive(PartialEq)]
pub enum ScanProgress {
    /// 未完成，下次调用继续。
    Incomplete,
    /// 已完成。
    Complete,
}

/// 仓库扫描器。
pub struct RepositoryScan {
    /// 已扫描的文件。
    files: Vec<IndexableFile>,
    /// 是否完成。
    complete: bool,
}

impl RepositoryScan {
    /// 推进扫描。
    pub fn advance(&mut self, budget: &ScanBudget) -> ScanProgress {
        if self.complete { return ScanProgress::Complete; }
        self.complete = true;
        ScanProgress::Complete
    }
}

/// 仓库身份标识。不含绝对路径，使用工作区根的摘要。
pub struct RepositoryIdentity {
    digest: String,
}

/// 扫描预算。
pub struct ScanBudget {
    pub max_host_calls: u32,
    pub max_bytes_read: u64,
}

/// 内容哈希。
pub fn content_hash(content: &str) -> u64 {
    let mut hash: u64 = 0;
    for byte in content.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
    }
    hash
}
"#,
        ),
        (
            "src/ignore.rs",
            r#"//! gitignore 语义规则引擎。
//!
//! 实现 .gitignore 与 .agentignore 的完整规则语义：注释、否定（!）、
//! 目录限定（/）、锚定、通配符（*、?、**）、字符类。

/// 三层优先级：硬排除 → 内置默认 → 规则文件。
pub struct IgnoreStack {
    /// 规则链节点。
    chain: Vec<IgnoreRuleSet>,
}

/// 一组忽略规则。
pub struct IgnoreRuleSet {
    /// 规则列表。
    rules: Vec<IgnoreRule>,
    /// 基准目录。
    base_dir: String,
}

/// 一条忽略规则。
pub struct IgnoreRule {
    /// 模式。
    pattern: String,
    /// 是否为否定规则。
    negated: bool,
    /// 是否仅匹配目录。
    directory_only: bool,
}

impl IgnoreStack {
    /// 判断路径是否应被忽略。
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        for rules in self.chain.iter().rev() {
            for rule in rules.rules.iter().rev() {
                if rule.matches(path, is_dir) {
                    return !rule.negated;
                }
            }
        }
        false
    }
}

impl IgnoreRule {
    /// 判断规则是否匹配路径。
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.directory_only && !is_dir { return false; }
        glob_match(&self.pattern, path)
    }
}

/// glob 模式匹配。
fn glob_match(pattern: &str, text: &str) -> bool {
    todo!("完整 glob 匹配实现")
}
"#,
        ),
        (
            "src/language.rs",
            r#"//! 语言识别。
//!
//! 通过文件扩展名白名单识别编程语言。未识别的扩展名跳过而不是当文本读取。

/// 编程语言。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Java,
    Php,
    Vue,
    Html,
    Css,
    Markdown,
}

/// 文件分类结果。
pub enum PathClass {
    /// 可索引的源码文件。
    Source(Language),
    /// 应跳过的文件。
    Skip(SkipReason),
}

/// 跳过原因。
pub enum SkipReason {
    /// 未识别的扩展名。
    UnknownExtension,
    /// 二进制文件。
    Binary,
    /// 敏感文件。
    Sensitive,
}

/// 按扩展名分类文件。
pub fn classify(path: &str) -> PathClass {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "rs" => PathClass::Source(Language::Rust),
        "ts" | "tsx" => PathClass::Source(Language::TypeScript),
        "js" | "jsx" => PathClass::Source(Language::JavaScript),
        "py" => PathClass::Source(Language::Python),
        "go" => PathClass::Source(Language::Go),
        "java" => PathClass::Source(Language::Java),
        "php" => PathClass::Source(Language::Php),
        "vue" => PathClass::Source(Language::Vue),
        "env" | "pem" | "key" => PathClass::Skip(SkipReason::Sensitive),
        _ => PathClass::Skip(SkipReason::UnknownExtension),
    }
}

impl Language {
    /// 该语言是否支持结构化解析。
    pub fn supports_structural_parsing(self) -> bool {
        matches!(self, Self::Rust | Self::TypeScript | Self::JavaScript | Self::Python | Self::Go)
    }
}
"#,
        ),
    ]
}

// ===========================================================================
// 真实仓库文件收集
// ===========================================================================

/// 递归收集整个仓库中插件支持索引的文件，返回 `(仓库相对路径, 内容)`。
///
/// 测试夹具只负责避开版本库元数据和构建产物，文件类型与敏感路径仍复用生产代码的
/// [`classify`] 判定，确保真实仓库测试覆盖 Rust 之外的文档、配置和其他语言源码。
fn collect_repository_files(root: &std::path::Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    collect_recursive(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn collect_recursive(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, String)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir()
            && matches!(
                name_str.as_ref(),
                ".git" | ".hg" | ".svn" | "target" | "node_modules" | "dist" | "coverage"
            )
        {
            continue;
        }
        if path.is_dir() {
            collect_recursive(root, &path, out);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("子路径应以根为前缀")
                .to_string_lossy()
                .replace('\\', "/");
            if !matches!(classify(&relative), PathClass::Indexable(_)) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                out.push((relative, content));
            }
        }
    }
}

/// 从 `CARGO_MANIFEST_DIR` 推导仓库根路径（向上三级）。
fn repo_root() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("应能从 CARGO_MANIFEST_DIR 推导仓库根")
        .to_path_buf()
}

// ===========================================================================
// 查询集
// ===========================================================================

struct BenchmarkQuery {
    query: &'static str,
    /// fixture 版：精确路径匹配。
    expected_files: &'static [&'static str],
}

struct RealRepoQuery {
    query: &'static str,
    /// 真实仓库版：子串匹配（仓库结构可能变动，路径子串更稳定）。
    expected_path_patterns: &'static [&'static str],
}

/// Fixture 合成查询集。
fn fixture_queries() -> Vec<BenchmarkQuery> {
    vec![
        BenchmarkQuery {
            query: "EmbeddingProvider",
            expected_files: &["src/embedding.rs"],
        },
        BenchmarkQuery {
            query: "BM25 词法检索",
            expected_files: &["src/retrieval.rs"],
        },
        BenchmarkQuery {
            query: "RRF 融合",
            expected_files: &["src/fusion.rs"],
        },
        BenchmarkQuery {
            query: "持久化存储 manifest",
            expected_files: &["src/storage.rs"],
        },
        BenchmarkQuery {
            query: "code_search 参数校验",
            expected_files: &["src/tool.rs"],
        },
        BenchmarkQuery {
            query: "增量索引的脏路径",
            expected_files: &["src/incremental.rs"],
        },
        BenchmarkQuery {
            query: "parse_file",
            expected_files: &["src/parser.rs"],
        },
        BenchmarkQuery {
            query: "token 预算打包",
            expected_files: &["src/context.rs"],
        },
        BenchmarkQuery {
            query: "向量相似度检索",
            expected_files: &["src/semantic.rs"],
        },
        BenchmarkQuery {
            query: "StructuralIndex",
            expected_files: &["src/index.rs"],
        },
        BenchmarkQuery {
            query: "gitignore 忽略规则",
            expected_files: &["src/ignore.rs"],
        },
        BenchmarkQuery {
            query: "语言识别 扩展名",
            expected_files: &["src/language.rs"],
        },
        BenchmarkQuery {
            query: "仓库扫描 IndexableFile",
            expected_files: &["src/repository.rs"],
        },
        BenchmarkQuery {
            query: "嵌入空间 embedding_space_id",
            expected_files: &["src/storage.rs", "src/embedding.rs"],
        },
        BenchmarkQuery {
            query: "配置 embedding_mode",
            expected_files: &["src/config.rs"],
        },
        BenchmarkQuery {
            query: "错误类型 EngineError",
            expected_files: &["src/error.rs"],
        },
    ]
}

/// 真实仓库查询集：覆盖符号查询、自然语言查询、跨 crate 查询。
fn real_repo_queries() -> Vec<RealRepoQuery> {
    vec![
        RealRepoQuery {
            query: "ToolResult",
            expected_path_patterns: &["agent-core/tool/src/lib.rs"],
        },
        RealRepoQuery {
            query: "PluginHostApi",
            expected_path_patterns: &["agent-plugins/sdk/src/lib.rs"],
        },
        RealRepoQuery {
            query: "SemanticIndex",
            expected_path_patterns: &["code-context-plugin/src/semantic"],
        },
        RealRepoQuery {
            query: "StructuralIndex",
            expected_path_patterns: &["code-context-plugin/src/index/"],
        },
        RealRepoQuery {
            query: "CodeSearchRequest",
            expected_path_patterns: &["code-context-plugin/src/tool.rs"],
        },
        RealRepoQuery {
            query: "BM25 词法检索",
            expected_path_patterns: &["code-context-plugin/src/retrieval/"],
        },
        RealRepoQuery {
            query: "RRF 融合",
            expected_path_patterns: &["code-context-plugin/src/retrieval/fusion"],
        },
        RealRepoQuery {
            query: "parse_file 代码解析",
            expected_path_patterns: &["code-context-plugin/src/parser/"],
        },
        RealRepoQuery {
            query: "token 预算 上下文打包",
            expected_path_patterns: &["code-context-plugin/src/context/"],
        },
        RealRepoQuery {
            query: "gitignore 忽略规则",
            expected_path_patterns: &["code-context-plugin/src/repository/ignore"],
        },
        RealRepoQuery {
            query: "语言识别 扩展名",
            expected_path_patterns: &["code-context-plugin/src/repository/language"],
        },
        RealRepoQuery {
            query: "增量索引 脏路径",
            expected_path_patterns: &["code-context-plugin/src/incremental"],
        },
        RealRepoQuery {
            query: "嵌入向量 provider",
            expected_path_patterns: &["code-context-plugin/src/embedding"],
        },
        RealRepoQuery {
            query: "索引 manifest 持久化",
            expected_path_patterns: &["code-context-plugin/src/storage"],
        },
        RealRepoQuery {
            query: "HttpRequest 受限 HTTP",
            expected_path_patterns: &["agent-plugins/"],
        },
    ]
}

// ===========================================================================
// 辅助函数
// ===========================================================================

/// 构造激活上下文。
fn activation_context(workspace_root: &str) -> ActivationContext {
    ActivationContext {
        plugin_id: "code-context".into(),
        metadata: std::collections::HashMap::from([(
            "workspace_root".to_string(),
            workspace_root.to_string(),
        )]),
    }
}

/// 在 fixture 仓库上激活并推进索引至完成。
fn indexed_fixture_plugin(host: &code_context_plugin::testing::RepoHost) -> CodeContextPlugin {
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(host, activation_context("/repo/bench"))
        .expect("激活插件");
    for _ in 0..20 {
        let call = ToolCall::new("bench", "code_search", json!({"query": "x"}));
        let _ = plugin.call_tool_with_host(host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    assert_ne!(
        plugin.index_status(),
        IndexStatus::Indexing,
        "索引应在有限轮次内完成"
    );
    plugin
}

/// 在真实仓库上激活并推进索引至完成。
fn indexed_real_plugin(
    host: &code_context_plugin::testing::RepoHost,
    workspace_root: &str,
) -> CodeContextPlugin {
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(host, activation_context(workspace_root))
        .expect("激活插件");
    for round in 0..100 {
        let call = ToolCall::new("bench", "code_search", json!({"query": "x"}));
        let _ = plugin.call_tool_with_host(host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            eprintln!("[BUILD] 索引在第 {} 轮完成", round + 1);
            break;
        }
    }
    assert_ne!(
        plugin.index_status(),
        IndexStatus::Indexing,
        "索引应在有限轮次内完成"
    );
    plugin
}

/// Recall@K：期望文件在 top-K 中的命中率（精确路径匹配）。
fn recall_at_k(results: &[SearchResult], expected: &[&str], k: usize) -> f64 {
    if expected.is_empty() {
        return 1.0;
    }
    let top_k: Vec<&str> = results
        .iter()
        .take(k)
        .map(|r| r.file_path.as_str())
        .collect();
    let hits = expected
        .iter()
        .filter(|&&file| top_k.contains(&file))
        .count();
    hits as f64 / expected.len() as f64
}

/// Recall@K：路径子串匹配版。
fn recall_at_k_pattern(results: &[SearchResult], patterns: &[&str], k: usize) -> f64 {
    if patterns.is_empty() {
        return 1.0;
    }
    let top_k: Vec<&str> = results
        .iter()
        .take(k)
        .map(|r| r.file_path.as_str())
        .collect();
    let hit = patterns
        .iter()
        .any(|pattern| top_k.iter().any(|path| path.contains(pattern)));
    if hit {
        1.0
    } else {
        0.0
    }
}

/// MRR：期望文件首次出现位置的倒数（精确路径匹配）。
fn mrr(results: &[SearchResult], expected: &[&str]) -> f64 {
    if expected.is_empty() {
        return 1.0;
    }
    let mut best_rr = 0.0f64;
    for &file in expected {
        if let Some(rank) = results.iter().position(|r| r.file_path == file) {
            let rr = 1.0 / (rank as f64 + 1.0);
            if rr > best_rr {
                best_rr = rr;
            }
        }
    }
    best_rr
}

/// MRR：路径子串匹配版。
fn mrr_pattern(results: &[SearchResult], patterns: &[&str]) -> f64 {
    for (rank, result) in results.iter().enumerate() {
        if patterns
            .iter()
            .any(|pattern| result.file_path.contains(pattern))
        {
            return 1.0 / (rank as f64 + 1.0);
        }
    }
    0.0
}

// ===========================================================================
// Fixture 测试（CI，非 ignore）
// ===========================================================================

/// fixture 索引必须在合理轮次内完成，验证 BUILD_BUDGET 充足。
#[test]
fn index_completes_within_budget() {
    let host = code_context_plugin::testing::RepoHost::new(&fixture_files());
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, activation_context("/repo/bench"))
        .expect("激活插件");

    let mut rounds = 0u32;
    for _ in 0..20 {
        rounds += 1;
        let call = ToolCall::new("bench", "code_search", json!({"query": "x"}));
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    assert_ne!(plugin.index_status(), IndexStatus::Indexing);

    let stats = plugin.index_stats().expect("索引应已建成");
    eprintln!(
        "[BUILD_BUDGET] {} 个文件，{} 轮完成，{} 个 chunk，{} 个符号",
        stats.files, rounds, stats.chunks, stats.symbols
    );
    assert!(
        rounds <= 3,
        "BUILD_BUDGET 过低：{} 个文件用了 {} 轮才完成",
        stats.files,
        rounds
    );
}

/// 检索质量基准：对 fixture 仓库运行全部查询，验证 Recall@5 >= 0.80 且 MRR >= 0.65。
#[test]
fn retrieval_quality_meets_baseline() {
    let host = code_context_plugin::testing::RepoHost::new(&fixture_files());
    let mut plugin = indexed_fixture_plugin(&host);

    let all_queries = fixture_queries();
    let mut total_recall_1 = 0.0f64;
    let mut total_recall_5 = 0.0f64;
    let mut total_mrr = 0.0f64;
    let count = all_queries.len() as f64;

    for bq in &all_queries {
        let results = plugin.search_results(&host, bq.query, None, 10);
        let r1 = recall_at_k(&results, bq.expected_files, 1);
        let r5 = recall_at_k(&results, bq.expected_files, 5);
        let m = mrr(&results, bq.expected_files);
        total_recall_1 += r1;
        total_recall_5 += r5;
        total_mrr += m;

        eprintln!(
            "[QUERY] {:30} | R@1={:.2} R@5={:.2} MRR={:.2} | top: {}",
            bq.query,
            r1,
            r5,
            m,
            results
                .iter()
                .take(3)
                .map(|r| format!(
                    "{}:{}",
                    r.file_path,
                    r.symbol_name.as_deref().unwrap_or("-")
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let avg_recall_1 = total_recall_1 / count;
    let avg_recall_5 = total_recall_5 / count;
    let avg_mrr = total_mrr / count;

    eprintln!(
        "\n[SUMMARY] Recall@1={avg_recall_1:.3}  Recall@5={avg_recall_5:.3}  MRR={avg_mrr:.3}"
    );

    assert!(avg_mrr >= 0.65, "MRR {avg_mrr:.3} 低于基线 0.65");
    assert!(
        avg_recall_5 >= 0.80,
        "Recall@5 {avg_recall_5:.3} 低于基线 0.80"
    );
}

/// 每条查询的期望文件必须至少出现在 top-10 中。
#[test]
fn every_query_finds_its_target() {
    let host = code_context_plugin::testing::RepoHost::new(&fixture_files());
    let mut plugin = indexed_fixture_plugin(&host);

    let mut failures = Vec::new();
    for bq in &fixture_queries() {
        let results = plugin.search_results(&host, bq.query, None, 10);
        let found: Vec<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
        for &expected in bq.expected_files {
            if !found.contains(&expected) {
                failures.push(format!(
                    "查询 {:30} 期望 {} 出现在 top-10，实际: {:?}",
                    bq.query,
                    expected,
                    found.iter().take(5).collect::<Vec<_>>()
                ));
            }
        }
    }

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("[MISS] {f}");
        }
        panic!("{} 条查询未命中期望文件", failures.len());
    }
}

/// Token 估算验证：纯 ASCII、纯 CJK、混合文本的估算应在合理范围。
#[test]
fn token_estimation_is_reasonable() {
    let ascii = "pub fn check_permission(id: &str) -> bool { id.is_empty() }";
    let ascii_tokens = estimate_tokens(ascii);
    let ascii_chars = ascii.len() as u32;
    assert!(
        ascii_tokens >= ascii_chars / 6 && ascii_tokens <= ascii_chars / 2,
        "ASCII 估算 {} 偏离合理范围（{} 字符）",
        ascii_tokens,
        ascii_chars
    );

    let cjk = "检查工具调用是否需要审批";
    let cjk_tokens = estimate_tokens(cjk);
    let cjk_chars = cjk.chars().count() as u32;
    assert!(
        cjk_tokens >= cjk_chars / 2 && cjk_tokens <= cjk_chars * 2,
        "CJK 估算 {} 偏离合理范围（{} 字）",
        cjk_tokens,
        cjk_chars
    );

    let mixed = "/// 检查工具调用是否需要审批。\npub fn check_permission(id: &str) -> bool {}";
    let mixed_tokens = estimate_tokens(mixed);
    assert!(
        mixed_tokens > 0 && mixed_tokens < 200,
        "混合文本估算 {} 超出合理范围",
        mixed_tokens
    );

    eprintln!(
        "[TOKEN] ASCII: {} chars -> {} tokens | CJK: {} chars -> {} tokens | Mixed: {} tokens",
        ascii_chars, ascii_tokens, cjk_chars, cjk_tokens, mixed_tokens
    );
}

/// code_search 工具应返回带代码片段的成功结果。
#[test]
fn tool_returns_structured_results() {
    let host = code_context_plugin::testing::RepoHost::new(&fixture_files());
    let mut plugin = indexed_fixture_plugin(&host);

    let call = ToolCall::new(
        "bench",
        "code_search",
        json!({"query": "EmbeddingProvider"}),
    );
    let result = plugin
        .call_tool_with_host(&host, call)
        .expect("工具调用不应失败");

    assert!(!result.is_error, "索引就绪时应返回成功结果");
    let content = &result.content;
    let results = content["results"].as_array().expect("results 应是数组");
    assert!(!results.is_empty(), "EmbeddingProvider 应有结果");

    let first = &results[0];
    assert_eq!(first["file"], "src/embedding.rs");
    assert!(first["snippet"]
        .as_str()
        .unwrap()
        .contains("EmbeddingProvider"));
    assert!(first["lines"].is_array());

    let details = result.details.expect("应带构建统计");
    assert!(details["stats"]["returned"].as_u64().unwrap_or(0) > 0);

    eprintln!(
        "[TOOL] 返回 {} 条结果，估算 {} tokens",
        results.len(),
        details["stats"]["estimated_tokens"]
    );
}

// ===========================================================================
// 真实仓库测试
// ===========================================================================

/// 索引本仓库全部受支持文件，验证 BUILD_BUDGET 在完整仓库下依然充足。
#[test]
fn real_repo_index_within_budget() {
    let root = repo_root();
    let files = collect_repository_files(&root);
    assert!(
        files
            .iter()
            .any(|(path, _)| path.starts_with("crates/agent-core/")),
        "真实仓库夹具必须覆盖插件目录之外的核心 crate"
    );
    assert!(
        files.iter().any(|(path, _)| !path.ends_with(".rs")),
        "真实仓库夹具必须覆盖 Rust 之外的受支持文件"
    );
    eprintln!(
        "[REPOSITORY] 从 {} 读取 {} 个文件",
        root.display(),
        files.len()
    );

    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = code_context_plugin::testing::RepoHost::new(&refs);

    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, activation_context(&root.to_string_lossy()))
        .expect("激活插件");

    let mut rounds = 0u32;
    for _ in 0..100 {
        rounds += 1;
        let call = ToolCall::new("bench", "code_search", json!({"query": "x"}));
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    assert_ne!(plugin.index_status(), IndexStatus::Indexing);

    let stats = plugin.index_stats().expect("索引应已建成");
    eprintln!(
        "[BUILD_BUDGET] {} 个文件，{} 轮完成，{} 个 chunk，{} 个符号",
        stats.files, rounds, stats.chunks, stats.symbols
    );
    assert!(
        rounds <= 30,
        "BUILD_BUDGET 过低：{} 个文件用了 {} 轮才完成",
        stats.files,
        rounds
    );
}

/// 在本仓库全部受支持文件上运行检索质量评估。
#[test]
fn real_repo_retrieval_quality() {
    let root = repo_root();
    let files = collect_repository_files(&root);
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = code_context_plugin::testing::RepoHost::new(&refs);
    let mut plugin = indexed_real_plugin(&host, &root.to_string_lossy());

    let all_queries = real_repo_queries();
    let mut total_recall_1 = 0.0f64;
    let mut total_recall_5 = 0.0f64;
    let mut total_recall_10 = 0.0f64;
    let mut total_mrr = 0.0f64;
    let count = all_queries.len() as f64;

    for bq in &all_queries {
        let results = plugin.search_results(&host, bq.query, None, 20);
        let r1 = recall_at_k_pattern(&results, bq.expected_path_patterns, 1);
        let r5 = recall_at_k_pattern(&results, bq.expected_path_patterns, 5);
        let r10 = recall_at_k_pattern(&results, bq.expected_path_patterns, 10);
        let m = mrr_pattern(&results, bq.expected_path_patterns);
        total_recall_1 += r1;
        total_recall_5 += r5;
        total_recall_10 += r10;
        total_mrr += m;

        let status = if r5 >= 1.0 {
            "OK"
        } else if r10 >= 1.0 {
            "LATE"
        } else {
            "MISS"
        };
        eprintln!(
            "[{:4}] {:35} | R@1={:.0} R@5={:.0} R@10={:.0} MRR={:.2} | top3: {}",
            status,
            bq.query,
            r1,
            r5,
            r10,
            m,
            results
                .iter()
                .take(3)
                .map(|r| format!(
                    "{}:{}",
                    r.file_path,
                    r.symbol_name.as_deref().unwrap_or("-")
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let avg_recall_1 = total_recall_1 / count;
    let avg_recall_5 = total_recall_5 / count;
    let avg_recall_10 = total_recall_10 / count;
    let avg_mrr = total_mrr / count;

    eprintln!(
        "\n[SUMMARY] Recall@1={:.3}  Recall@5={:.3}  Recall@10={:.3}  MRR={:.3}  ({} queries)",
        avg_recall_1,
        avg_recall_5,
        avg_recall_10,
        avg_mrr,
        all_queries.len()
    );

    assert!(avg_mrr >= 0.50, "MRR {avg_mrr:.3} 低于基线 0.50");
    assert!(
        avg_recall_10 >= 0.75,
        "Recall@10 {avg_recall_10:.3} 低于基线 0.75"
    );
}

/// 真实仓库：每条查询的期望文件必须至少出现在 top-20 中。
#[test]
fn real_repo_every_query_finds_target() {
    let root = repo_root();
    let files = collect_repository_files(&root);
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = code_context_plugin::testing::RepoHost::new(&refs);
    let mut plugin = indexed_real_plugin(&host, &root.to_string_lossy());

    let mut failures = Vec::new();
    for bq in &real_repo_queries() {
        let results = plugin.search_results(&host, bq.query, None, 20);
        let found: Vec<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
        if !bq
            .expected_path_patterns
            .iter()
            .any(|pattern| found.iter().any(|path| path.contains(pattern)))
        {
            failures.push(format!(
                "查询 {:35} 期望匹配 {:?}，实际 top-5: {:?}",
                bq.query,
                bq.expected_path_patterns,
                found.iter().take(5).collect::<Vec<_>>()
            ));
        }
    }

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("[MISS] {f}");
        }
        panic!("{} 条查询未命中期望文件", failures.len());
    }
}

/// 真实仓库：code_search 工具返回结构化结果。
#[test]
fn real_repo_tool_returns_results() {
    let root = repo_root();
    let files = collect_repository_files(&root);
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = code_context_plugin::testing::RepoHost::new(&refs);
    let mut plugin = indexed_real_plugin(&host, &root.to_string_lossy());

    let call = ToolCall::new("bench", "code_search", json!({"query": "ToolResult"}));
    let result = plugin
        .call_tool_with_host(&host, call)
        .expect("工具调用不应失败");

    assert!(!result.is_error, "索引就绪时应返回成功结果");
    let content = &result.content;
    let results = content["results"].as_array().expect("results 应是数组");
    assert!(!results.is_empty(), "ToolResult 应有结果");

    let first = &results[0];
    assert!(first["file"].as_str().is_some(), "应有 file 字段");
    assert!(first["snippet"].as_str().is_some(), "应有 snippet 字段");
    assert!(first["lines"].is_array(), "应有 lines 字段");

    let details = result.details.expect("应带构建统计");
    eprintln!(
        "[TOOL] 返回 {} 条结果，估算 {} tokens",
        results.len(),
        details["stats"]["estimated_tokens"]
    );
}
