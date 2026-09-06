//! Code Context Engine 插件。
//!
//! 为 Agent 提供 `code_search`：按自然语言或标识符定位仓库中的代码，替代多轮
//! `grep` 与读文件。设计依据见 `docs/code-context-engine/`。
//!
//! # 当前阶段
//!
//! 已完成仓库扫描、结构与检索索引、上下文构建、语义向量索引（含持久化），以及按写入
//! 事件触发的文件级增量重建。`code_search` 返回经 RRF 融合、可选模型重排、去重、父子
//! 抑制与 token 预算打包后的真实检索结果。
//!
//! # 能力边界
//!
//! 本插件申请 `workspace_read`、`storage` 与受限 `http`。工作区读取由 Host 注入可信根并
//! 限制为只读；HTTP 仅访问 manifest 白名单内、由应用注入地址与凭证的模型端点。插件不
//! 申请进程与会话能力。HTTP 会把工具权限下限提升为最保守档位，详见 `plugin.toml`。
//!
//! # 并发与取消
//!
//! 宿主对同一插件的全部导出调用串行执行（单 Store + 独占锁），因此插件内部不需要任何
//! 锁或并发原语。宿主也不提供调用级取消：已进入 component 的调用一定会跑到返回。
//! 长任务的唯一可行形态是**按 fuel 预算分片**——单次调用消耗到阈值就保存游标返回，
//! 下次调用继续。该游标协议由 Phase 2 的扫描器提供，增量构建沿用同一套机制。

mod config;
mod context;
mod embedding;
mod error;
mod incremental;
mod index;
mod model;
mod parser;
mod repository;
mod rerank;
mod retrieval;
mod semantic;
#[cfg(feature = "sqlite-vector-store")]
mod sqlite_vector;
mod storage;
mod tool;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

use agent_plugin::{
    core_events, export_plugin, ActivationContext, AgentPlugin, EventOutcome, EventPresentation,
    EventPresentationTone, ExtensionEvent, PluginEvent, PluginHostApi, Result, RunFinishEventData,
    ToolCall, ToolCallEventData, ToolResult, ToolResultEventData, ToolSpec,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub use config::{CodeContextConfig, EmbeddingMode};
pub use context::{
    build as build_context, estimate_tokens, ContextBundle, ContextConfig, ContextEntry,
    ContextStats,
};
pub use embedding::{
    select_provider, EmbeddingInputKind, EmbeddingProvider, UnconfiguredEmbeddingProvider,
    DEFAULT_MAX_BATCH_SIZE,
};
pub use error::CodeContextError;
pub use index::{
    FileChangeKind, FileRecord, IncrementalStats, IndexBuild, IndexChange, IndexStats,
    StructuralIndex, BUILD_BUDGET,
};
pub use model::{
    CodeChunk, EmbeddingSpace, IndexManifest, IndexStatus, ManifestCompatibility,
    RepositoryIdentity, Symbol, SymbolKind, INDEX_SCHEMA_VERSION, STRUCTURAL_INDEX_VERSION,
};
pub use parser::{parse_file, Declaration, ParseOutput, ParseTier, ParsedFile};
pub use repository::ignore::{IgnoreRuleSet, IgnoreStack, AGENT_IGNORE_FILE, GIT_IGNORE_FILE};
pub use repository::language::{classify, Language, PathClass, SkipReason};
pub use repository::{
    content_hash, hash_index, IndexableFile, RepositoryScan, ScanBudget, ScanLimits, ScanProgress,
    ScanStats, ScanWarning, ScanWarningKind,
};
pub use retrieval::{
    tokenize, AnalyzedQuery, CandidateSource, CandidateStages, ChunkRole, ConfidenceLevel,
    FusionConfig, FusionDebug, PendingDirective, QueryGoal, QueryIntent, RetrievalConfidence,
    RetrievalIndex,
    RetrievalRun, RetrievalStats, SearchCandidate, SearchPath, SearchPlan, SearchResult,
    SymbolMatch,
};
pub use semantic::SemanticIndex;
pub use storage::{IndexStore, ManifestLoad};
pub use tool::{CodeSearchRequest, TOOL_NAME};

/// 单个 chunk 参与嵌入的最大正文字符数。
///
/// 嵌入模型的输入窗口有限，且一段代码的语义在前若干行里已经基本确定。取 1200 字符约合
/// 30–40 行代码，既覆盖绝大多数函数的完整实现，又不会让一个超长函数挤占整批的配额。
const MAX_SEMANTIC_BODY_CHARS: usize = 1_200;

/// 注入嵌入文本的文件级文档最大字符数。
///
/// 比块级文档更短：它在同一文件的每个块里重复出现，过长会把各块之间真正的差异冲淡，
/// 让整个文件的向量挤成一团。300 字符足以容纳一段说明职责的开头。
const MAX_FILE_DOC_CHARS: usize = 300;

/// 注入嵌入文本的 import 最大条数。
const MAX_SEMANTIC_IMPORTS: usize = 16;

/// 注入嵌入文本的 import 总字符上限。
const MAX_SEMANTIC_IMPORT_CHARS: usize = 600;

/// 构造送去嵌入的文档文本。
///
/// 拼接路径、**文件级文档**、父符号、限定名、签名、块级文档注释、import 与**代码正文**。
///
/// 正文是最关键的一项：没有它，一个没写文档注释的函数就只剩下路径和签名可供嵌入，向量
/// 几乎只编码了"它叫什么、在哪个文件"；而没有结构化解析器的语言（Java、Vue、PHP 等）
/// 连符号和签名都没有，向量退化成一条路径。
///
/// 文件级文档解决的是另一个问题：自然语言查询表达的常常是**文件的职责**而不是某个函数
/// 的细节。"代码块是怎么切分出来的"对应的是 `parser/mod.rs` 顶部那句"把源文件转换为
/// CodeChunk 与 Symbol"，但这段文字不属于文件里任何一个声明，按声明切块时会整段丢失，
/// 于是文件里每个函数都缺少"我属于干什么的模块"这一层语境。
///
/// `body` 为 `None` 表示正文回读失败，此时退回元数据——比完全跳过这个 chunk 更好。
/// 缺失字段直接跳过，不填占位符：占位符会成为所有同类 chunk 的共同噪声。
fn semantic_document_text(chunk: &CodeChunk, file_doc: Option<&str>, body: Option<&str>) -> String {
    let mut parts = Vec::new();
    parts.push(chunk.file_path.clone());
    if let Some(doc) = file_doc.map(str::trim).filter(|doc| !doc.is_empty()) {
        parts.push(truncate_chars(doc, MAX_FILE_DOC_CHARS));
    }
    if let Some(parent) = chunk.parent_symbol.as_deref() {
        parts.push(format!("父符号：{parent}"));
    }
    if let Some(qualified) = chunk.qualified_symbol.as_deref() {
        parts.push(qualified.to_string());
    }
    if let Some(signature) = chunk.signature.as_deref() {
        parts.push(signature.to_string());
    }
    if let Some(doc) = chunk.doc_comment.as_deref() {
        parts.push(doc.to_string());
    }
    if !chunk.imports.is_empty() {
        let imports = chunk
            .imports
            .iter()
            .take(MAX_SEMANTIC_IMPORTS)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!(
            "依赖：\n{}",
            truncate_chars(&imports, MAX_SEMANTIC_IMPORT_CHARS)
        ));
    }
    if let Some(body) = body.map(str::trim).filter(|body| !body.is_empty()) {
        parts.push(truncate_chars(body, MAX_SEMANTIC_BODY_CHARS));
    }
    parts.join("\n")
}

/// 按字符边界截断文本，避免在多字节字符中间切开。
fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((offset, _)) => text[..offset].to_string(),
        None => text.to_string(),
    }
}

/// 取闭区间行范围的正文；范围失效时返回 `None`。
fn slice_body(content: &str, start_line: u32, end_line: u32) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start = start_line.saturating_sub(1) as usize;
    let end = (end_line as usize).min(lines.len());
    (start < end).then(|| lines[start..end].join("\n"))
}

/// 判断一个结构 chunk 是否应拥有独立语义向量。
///
/// 语义复合块参与；被复合块覆盖的原子成员不参与；其余普通块保持原有行为。这样符号与
/// BM25 的精确粒度不变，同时减少过短代码块对嵌入请求和向量候选的占用。
fn participates_in_semantic_index(chunk: &CodeChunk) -> bool {
    chunk.semantic_only || chunk.semantic_group_id.is_none()
}

/// 单次调用最多提交的嵌入批次数。
///
/// 嵌入一个中等仓库需要几百次请求，全部塞进一次工具调用会让首次检索长时间无响应，
/// 也更容易触发端点的速率限制。分摊到多次调用后，每轮的停顿都是有界的，而限流恰好
/// 得到天然退避——两次工具调用之间的间隔就是退避窗口。
const MAX_EMBEDDING_BATCHES_PER_ROUND: usize = 24;

/// 连续多少轮毫无进展后放弃语义索引。
///
/// 普通 5xx 或网络瞬时错误值得重试，但不能无限重试：端点长时间不可用时，状态必须落到
/// 一个终态，否则界面会永远停在"正在建立索引"。HTTP 429 不使用该计数，因为它只表示
/// 当前配额窗口未恢复，不能据此永久放弃已经完成大半的语义索引。
const MAX_SEMANTIC_STALLED_ROUNDS: u32 = 3;

/// 一次语义索引构建的结果。
///
/// 区分"没配"、"没建完"和"建好了"三种状态，而不是一个布尔值：只有第一种是正常状态，
/// 第二种必须让调用方在下一轮继续，第三种才允许把状态置为 Ready。此前用布尔值把后两种
/// 混为一谈，结果是一次限流就让语义索引永久停在空表上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticBuild {
    /// 未配置嵌入 provider；这是正常状态，不构成降级。
    NotConfigured,
    /// 仍有代码块没有向量，但失败是瞬时的，需要在后续调用中继续。
    Incomplete,
    /// 嵌入永久不可用（模型名错误、密钥失效、参数不被接受），重试没有意义。
    Unavailable,
    /// 全部代码块都已有当前向量。
    Ready,
}

impl SemanticBuild {
    /// 把构建结果折算成对外的索引状态。
    ///
    /// `Incomplete` 映射到 `Indexing` 而不是 `Degraded`：后者是终态，会让调用方停止推进，
    /// 于是被限流打断的语义索引永远补不齐。未配置嵌入与永久不可用都落到 `Degraded`——
    /// 那正是架构定义的降级形态：符号与词法两路可用，语义缺席。
    fn into_status(self) -> IndexStatus {
        match self {
            Self::Ready => IndexStatus::Ready,
            Self::Incomplete => IndexStatus::Indexing,
            Self::NotConfigured | Self::Unavailable => IndexStatus::Degraded,
        }
    }
}

/// 宿主 preopen 的插件私有数据目录在 Guest 中的挂载点。
///
/// 由 manifest 的 `data_dir` 能力授予，按插件与项目作用域隔离，**没有容量上限**——
/// 索引与向量的体积由用户仓库规模决定，宿主只保证除本插件与宿主外无人可访问。
#[cfg(feature = "sqlite-vector-store")]
const PLUGIN_DATA_DIR: &str = "/data";

/// 宿主注入工作区根的激活元数据键。
const WORKSPACE_ROOT_KEY: &str = "workspace_root";

/// 插件发布状态变化使用的扩展事件名。
const STATUS_EVENT: &str = "code-context.status";

/// 索引分片推进时发布的底栏事件名。
const INDEX_PROGRESS_EVENT: &str = "code-context.index.progress";

/// 组合运行与调用身份，避免不同并发运行复用调用 ID 时互相覆盖。
fn pending_write_key(run_id: &str, call_id: &str) -> String {
    format!("{run_id}\u{0}{call_id}")
}

/// 已附着仓库的运行时上下文。
///
/// 宿主当前没有仓库打开/切换事件：工作区在进程启动时确定，换项目等于重启进程
/// （见 `architecture.md` 第 11 问）。附着与解除因此各只发生一次，但仍然建模为显式
/// 生命周期——将来宿主若引入仓库事件，接入点已经在这里。
#[derive(Debug)]
struct RepositoryContext {
    /// 当前仓库的稳定身份。
    identity: RepositoryIdentity,
    /// Host 注入的可信工作区根，仅用于把事件中的绝对路径转换为相对路径；不落盘、不发布。
    workspace_root: String,
    /// 索引元数据存储。
    store: IndexStore,
    /// 当前仓库的索引 manifest。
    manifest: IndexManifest,
    /// manifest 是否有尚未落盘的改动。
    dirty: bool,
    /// 进行中的索引构建；完成后转入 `index`。
    build: Option<IndexBuild>,
    /// 已建成的结构索引。
    ///
    /// 内存驻留，不落盘：宿主存储装不下 chunk 与符号表，理由见
    /// [ADR：索引存储后端](../../docs/code-context-engine/adr-storage.md)。
    index: Option<StructuralIndex>,
    /// 已建成的检索索引（符号表 + 词法倒排）。
    retrieval: Option<RetrievalIndex>,
    /// 语义向量索引。
    ///
    /// 运行时使用归一化 f32；原生搜索示例由 sqlite-vec 持久化并执行 KNN，WASM 插件继续
    /// 使用 int4 宿主快照。未配置嵌入 provider 时保持为空，检索自动退化为词法与符号两路。
    semantic: SemanticIndex,
    /// 已确认成功写入、等待下一次增量构建的仓库相对路径。
    dirty_paths: BTreeSet<String>,
    /// 尚未收到终态结果的写入调用，键为运行 ID 与调用 ID 的组合。
    pending_writes: BTreeMap<String, incremental::PendingWrite>,
    /// 仓库生命周期内的查询向量缓存：空间 ID、查询摘要、向量；最多 32 条，不落盘。
    query_vectors: Vec<(String, String, Vec<f32>)>,
    /// 同一仓库生命周期内，按完整候选源码和请求摘要复用已校验的模型响应。
    rerank_responses: rerank::ResponseCache,
}

/// Code Context Engine 插件。
#[derive(Default)]
pub struct CodeContextPlugin {
    /// 解析后的插件配置。
    config: CodeContextConfig,
    /// 当前附着的仓库；未附着时为 `None`。
    repository: Option<RepositoryContext>,
    /// 索引生命周期状态。
    index_status: IndexStatus,
    /// 引擎不可用时面向用户的原因。
    unavailable_reason: Option<String>,
    /// 引擎可用但能力受限时的原因，例如哈希表未能落盘。
    ///
    /// 与 `unavailable_reason` 区分：后者会让 `code_search` 拒绝服务，这个不会。
    degraded_reason: Option<String>,
    /// 最近一次检索中模型重排失败的降级原因；下一次检索开始时清空。
    last_rerank_degraded_reason: Option<String>,
    /// Host 返回的 `code_search` 公开名称；存在时表示工具已经注册给 Agent。
    registered_tool_name: Option<String>,
    /// 连续没有新向量产出的语义构建轮次；有进展时清零。
    semantic_stalled_rounds: u32,
    /// 原生测试入口使用的 sqlite-vec 数据库路径；WASM 构建不包含该字段。
    #[cfg(feature = "sqlite-vector-store")]
    sqlite_vector_path: Option<std::path::PathBuf>,
}

/// 一次代码检索的结构化诊断。
///
/// 该信息供 benchmark 与工具 `details` 使用，不进入模型可见代码片段。它记录查询分析、
/// 实际计划、候选阶段、置信度和模型重排决策，使延迟与 Recall 变化都可归因。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchDiagnostics {
    /// 确定性的查询分析结果。
    pub analyzed_query: AnalyzedQuery,
    /// 实际执行的检索计划。
    pub plan: SearchPlan,
    /// 各召回与排序阶段的候选身份。
    pub stages: CandidateStages,
    /// 模型重排前的本地置信度。
    pub confidence: RetrievalConfidence,
    /// 实际采用的 Fast、Normal 或 Hard 路径。
    pub path: SearchPath,
    /// 是否向远端模型发起了重排请求。
    pub rerank_activated: bool,
    /// 模型响应是否成功应用到最终排序。
    pub rerank_applied: bool,
    /// 重排被启用、跳过或降级的可读原因。
    pub rerank_reason: String,
}

/// 一次带完整诊断的代码检索结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchExecution {
    /// 最终返回候选。
    pub results: Vec<SearchResult>,
    /// 本次检索的结构化诊断。
    pub diagnostics: SearchDiagnostics,
}

/// 构造索引上下文缺席时的空检索结果，同时保留查询分析与计划诊断。
fn empty_search_execution(analyzed_query: AnalyzedQuery, plan: SearchPlan) -> SearchExecution {
    let confidence = rerank::assess_confidence(&analyzed_query, &[]);
    let path = if matches!(
        analyzed_query.intent,
        QueryIntent::Identifier | QueryIntent::QualifiedSymbol
    ) {
        SearchPath::Fast
    } else {
        SearchPath::Normal
    };
    SearchExecution {
        results: Vec::new(),
        diagnostics: SearchDiagnostics {
            analyzed_query,
            plan,
            stages: CandidateStages::default(),
            confidence,
            path,
            rerank_activated: false,
            rerank_applied: false,
            rerank_reason: "索引上下文不可用，未执行模型重排".to_string(),
        },
    }
}

impl CodeContextPlugin {
    /// 指定本地 sqlite-vec 数据库文件。
    ///
    /// 可执行搜索示例用它把向量落到 Lucia 用户目录。未显式指定时，WASM Guest 会自动落到
    /// 宿主 preopen 的私有数据目录，见 [`Self::default_vector_database`]。
    #[cfg(feature = "sqlite-vector-store")]
    pub fn with_sqlite_vector_database(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.sqlite_vector_path = Some(path.into());
        self
    }

    /// 返回宿主授予的私有数据目录中的向量库路径；目录不存在时为 `None`。
    ///
    /// `/data` 由宿主在 manifest 声明 `data_dir` 时 preopen。目录缺席说明宿主未授予该能力
    /// （旧版宿主、或用户关闭了它），此时调用方退回 int4 快照——功能不中断，只是大仓库会
    /// 因快照容量上限丢失一部分向量。
    #[cfg(feature = "sqlite-vector-store")]
    fn default_vector_database() -> Option<std::path::PathBuf> {
        let root = std::path::Path::new(PLUGIN_DATA_DIR);
        root.is_dir().then(|| root.join("vectors.sqlite3"))
    }

    /// 返回最近一次降级原因；正常时为 `None`。
    ///
    /// 语义通道失效通常是静默的——检索照样返回结果，只是少了一路召回。把原因暴露出来，
    /// 调用方与评测才能区分"语义没帮上忙"和"语义压根没跑起来"。
    pub fn degraded_reason(&self) -> Option<&str> {
        self.degraded_reason.as_deref()
    }

    /// 返回语义索引中的向量条数；未建立时为 0。
    pub fn semantic_len(&self) -> usize {
        self.repository
            .as_ref()
            .map_or(0, |context| context.semantic.len())
    }

    /// 返回当前索引状态。
    pub fn index_status(&self) -> IndexStatus {
        self.index_status
    }

    /// 返回当前仓库身份；未附着仓库时为 `None`。
    pub fn repository_identity(&self) -> Option<&RepositoryIdentity> {
        self.repository.as_ref().map(|context| &context.identity)
    }

    /// 返回引擎不可用的原因。
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }

    /// 返回当前生效的嵌入 provider。
    ///
    /// 选择逻辑集中在 [`embedding::select_provider`]：Api 模式在宿主注入 HTTP 端点时
    /// 返回真实 provider，否则连同 Local 模式一起返回未配置 provider。返回类型保持
    /// trait 对象，借用 `host` 是因为真实 provider 需要宿主能力入口发请求。
    pub fn embedding_provider<'host>(
        &self,
        host: &'host dyn PluginHostApi,
    ) -> Box<dyn EmbeddingProvider + 'host> {
        embedding::select_provider(&self.config, host)
    }

    /// 把当前仓库附着到引擎。
    ///
    /// 完整流程：解析仓库身份 → 探测插件存储命名空间 → 读取索引 manifest →
    /// 判断兼容性 → 设置索引状态。**不执行任何索引构建。**
    ///
    /// # Errors
    ///
    /// 工作区根缺失或存储写入失败时返回错误。调用方负责决定这是否应当阻止插件加载。
    fn attach_repository(
        &mut self,
        host: &dyn PluginHostApi,
        workspace_root: &str,
    ) -> std::result::Result<(), CodeContextError> {
        let identity = RepositoryIdentity::from_workspace_root(workspace_root)?;
        let store = IndexStore::probe(host);

        let (mut manifest, mut status, mut dirty) = match store.load_manifest(host) {
            ManifestLoad::Loaded(existing) => {
                let expected_space = self
                    .embedding_provider(host)
                    .space()
                    .map(|space| space.id());
                let compatibility = existing.compatibility(&identity, expected_space.as_deref());
                match compatibility {
                    // 结构索引内存驻留、不落盘，因此 manifest 兼容也不代表现在有索引可用：
                    // 进程刚起来时索引一定是空的，必须先重建。报告 Ready 会让 code_search
                    // 去搜一个空索引，把"还没建好"误报成"仓库里没有相关代码"。
                    // 这不是待办，而是存储 ADR 的既定结果。
                    ManifestCompatibility::Compatible => {
                        (*existing, IndexStatus::NotIndexed, false)
                    }
                    ManifestCompatibility::FullRebuildRequired => (
                        IndexManifest::new(identity.clone()),
                        IndexStatus::NeedsFullRebuild,
                        true,
                    ),
                    ManifestCompatibility::SemanticRebuildRequired => {
                        (*existing, IndexStatus::NeedsSemanticRebuild, false)
                    }
                }
            }
            // 损坏或版本过高的 manifest 不是错误路径：丢弃它并进入全量重建，
            // 让引擎能自愈，而不是让插件加载失败或让宿主收到异常。
            ManifestLoad::Unreadable(_) => (
                IndexManifest::new(identity.clone()),
                IndexStatus::NeedsFullRebuild,
                true,
            ),
            ManifestLoad::Missing => (
                IndexManifest::new(identity.clone()),
                IndexStatus::NotIndexed,
                true,
            ),
        };

        if dirty && store.is_available() {
            manifest.touch();
            store.save_manifest(host, &manifest)?;
            dirty = false;
        }
        // 宿主未提供项目作用域时索引无法持久化，但引擎仍可在内存中工作；
        // 这里保留 dirty 标记，使状态如实反映"这份 manifest 没有落盘"。
        if !store.is_available() {
            status = IndexStatus::NotIndexed;
        }

        // 尝试从持久存储恢复语义向量。原生搜索示例优先打开 sqlite-vec；数据库为空时
        // 自动迁移旧 int4 JSON 快照，避免切换存储后重新调用嵌入 API。
        let mut semantic = SemanticIndex::new();
        if let Some(space) = self.embedding_provider(host).space() {
            #[cfg(feature = "sqlite-vector-store")]
            // 显式指定的路径优先（可执行示例）；否则落到宿主 preopen 的私有数据目录。
            if let Some(path) = self
                .sqlite_vector_path
                .clone()
                .or_else(Self::default_vector_database)
            {
                semantic = SemanticIndex::with_sqlite(space.clone(), &path)
                    .map_err(CodeContextError::Storage)?;
            }
            if semantic.is_empty() {
                if let Some(snapshot) = store.load_semantic(host) {
                    semantic.restore(&space, &snapshot);
                }
            }
            #[cfg(feature = "sqlite-vector-store")]
            if semantic.sqlite_path().is_some() {
                if let Some(error) = semantic.take_backend_error() {
                    return Err(CodeContextError::Storage(error));
                }
                store.clear_semantic(host)?;
            }
        }

        self.repository = Some(RepositoryContext {
            identity,
            workspace_root: workspace_root.to_string(),
            store,
            manifest,
            dirty,
            build: None,
            index: None,
            retrieval: None,
            semantic,
            dirty_paths: BTreeSet::new(),
            pending_writes: BTreeMap::new(),
            query_vectors: Vec::new(),
            rerank_responses: rerank::ResponseCache::default(),
        });
        self.index_status = status;
        Ok(())
    }

    /// 解除仓库附着并尽力落盘未保存的元数据。
    ///
    /// **这不是保存点。** 宿主在加载被取消或进程异常退出时不保证调用 `deactivate`，
    /// 因此 manifest 在产生时就已经写入；这里只处理内存中尚未落盘的剩余改动。
    fn detach_repository(
        &mut self,
        host: &dyn PluginHostApi,
    ) -> std::result::Result<(), CodeContextError> {
        let Some(mut context) = self.repository.take() else {
            return Ok(());
        };
        self.index_status = IndexStatus::NotIndexed;
        if context.dirty && context.store.is_available() {
            context.manifest.touch();
            context.store.save_manifest(host, &context.manifest)?;
        }
        Ok(())
    }

    /// 注册 `code_search`。
    ///
    /// 采用动态注册而不是静态 `list_tools`：静态工具在 `activate` 之前就被收集，
    /// 那时还读不到配置，无法根据 `enabled` 与仓库可用性决定是否暴露工具。
    fn register_tool(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let public_name = host.upsert_tool(TOOL_NAME, &tool::spec())?;
        self.registered_tool_name = Some(public_name);
        Ok(())
    }

    /// 注销当前激活实例动态注册的工具。
    ///
    /// Host 可能重写公开工具名，因此必须使用 [`PluginHostApi::upsert_tool`] 的返回值，
    /// 不能假定它永远等于插件内部名称。
    fn unregister_tool(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let Some(public_name) = self.registered_tool_name.take() else {
            return Ok(());
        };
        host.remove_tool(&public_name)
    }

    /// 发布一条状态事件，让用户在启动摘要中看到引擎当前处境。
    fn emit_status(&self, host: &dyn PluginHostApi) -> Result<()> {
        let (text, tone) = match (&self.unavailable_reason, self.index_status) {
            (Some(reason), _) => (
                format!("Code Context: 不可用（{reason}）"),
                EventPresentationTone::Warning,
            ),
            (None, status) => (
                format!("Code Context: {}", status.as_str()),
                EventPresentationTone::Info,
            ),
        };
        host.emit_event(&ExtensionEvent {
            name: STATUS_EVENT.into(),
            data: json!({
                "status": self.index_status.as_str(),
                "tool_registered": self.registered_tool_name.is_some(),
                "embedding_mode": self.config.embedding_mode.as_str(),
                "reason": self.unavailable_reason,
                // 仓库身份是摘要而不是路径，可以安全地出现在事件与日志里，
                // 同时让宿主侧测试与诊断能确认引擎附着到了哪个仓库。
                "repository_identity": self
                    .repository_identity()
                    .map(RepositoryIdentity::as_str),
            }),
            presentation: Some(EventPresentation::footer(text, tone)),
        })
    }

    /// 推进索引构建。
    ///
    /// 构建是**惰性**的：首次调用时创建，之后每次调用推进一个预算片。放在这里而不是
    /// `activate` 里，是因为 `activate` 与其他导出共用同一份 fuel 预算，在那里做全量索引
    /// 必然 trap（见 [ADR：索引存储后端](../../docs/code-context-engine/adr-storage.md)）。
    ///
    /// 构建完成后把文件哈希表落盘，供下次启动做增量判断；哈希表写入失败不影响索引可用性，
    /// 只会让下次启动退化为全量重建。
    fn advance_index(&mut self, host: &dyn PluginHostApi) {
        let Some(context) = self.repository.as_mut() else {
            return;
        };
        if context.build.is_none() {
            // 把本轮脏集合从上下文中移出。构建期间新收到的事件会进入新的集合，完成时
            // 只合并构建自身的失败路径，因此不会误清理构建开始后再次变脏的同一路径。
            let requested_dirty = std::mem::take(&mut context.dirty_paths);
            // 结构索引与检索索引必须成对接管。只存在一侧时先恢复已有值，再启动全量构建，
            // 避免 `take()` 把另一侧静默丢掉。
            let existing_index = context.index.take();
            let existing_retrieval = context.retrieval.take();
            match (existing_index, existing_retrieval) {
                (Some(index), Some(retrieval)) if !requested_dirty.is_empty() => {
                    context.build = Some(IndexBuild::new_incremental(
                        index,
                        retrieval,
                        requested_dirty,
                        ScanLimits::default(),
                    ));
                }
                (Some(index), Some(retrieval)) => {
                    context.index = Some(index);
                    context.retrieval = Some(retrieval);
                    context.dirty_paths.extend(requested_dirty);
                    // 结构索引已就绪，但语义索引可能还差一截（限流中断、分轮未完）。
                    // 在这里继续推进，否则被中断的语义索引永远补不齐。
                    if self.index_status == IndexStatus::Indexing {
                        self.index_status = self.build_semantic_index(host).into_status();
                    }
                    return;
                }
                (index, retrieval) => {
                    context.index = index;
                    context.retrieval = retrieval;
                    // 没有完整的既有索引时只能全量构建；把已积累的脏路径交给构建，便于在
                    // 完成后只保留确实失败的路径。
                    context.build = Some(IndexBuild::new_full_with_dirty(
                        context.identity.clone(),
                        ScanLimits::default(),
                        requested_dirty,
                    ));
                }
            }
            self.index_status = IndexStatus::Indexing;
        }

        let (progress, is_incremental, scan_stats, incremental_stats) = {
            let build = context.build.as_mut().expect("已创建索引构建");
            let progress = build.advance(host, BUILD_BUDGET);
            (
                progress,
                build.is_incremental(),
                build.scan_stats(),
                build.incremental_stats(),
            )
        };
        if progress == ScanProgress::Incomplete {
            self.index_status = IndexStatus::Indexing;
            Self::emit_index_progress(
                host,
                is_incremental,
                progress,
                scan_stats,
                incremental_stats,
                context.dirty_paths.len(),
            );
            return;
        }

        let build = context.build.take().expect("完成的构建必须存在");
        let remaining_dirty = build.remaining_dirty_paths();
        let (index, retrieval) = build.into_parts();
        context.dirty_paths.extend(remaining_dirty);

        // 哈希表只是增量判断的优化输入，写不进去不影响本次检索能力。
        match context.store.save_file_hashes(host, &index.file_hashes()) {
            Ok(()) => {
                self.degraded_reason = None;
            }
            Err(error) => {
                self.degraded_reason = Some(format!("文件哈希表未能落盘：{error}"));
            }
        }
        context.manifest.touch();
        context.dirty = context
            .store
            .save_manifest(host, &context.manifest)
            .is_err();
        context.index = Some(index);
        context.retrieval = Some(retrieval);
        // 在交出 `context` 的可变借用前取走事件所需的计数。
        let remaining_dirty_count = context.dirty_paths.len();

        // 语义向量在结构索引就绪后建立。永久错误进入 Degraded；限流或仍有进展的瞬时错误
        // 保持 Indexing 供后续调用续建。两种状态都不破坏已就绪的符号与词法索引。
        let semantic = self.build_semantic_index(host);
        self.index_status = semantic.into_status();
        Self::emit_index_progress(
            host,
            is_incremental,
            ScanProgress::Complete,
            scan_stats,
            incremental_stats,
            remaining_dirty_count,
        );
    }

    /// 发布一次索引推进快照；事件失败只记录在 Host 侧，不得阻塞索引本身。
    fn emit_index_progress(
        host: &dyn PluginHostApi,
        is_incremental: bool,
        progress: ScanProgress,
        scan_stats: ScanStats,
        incremental_stats: Option<IncrementalStats>,
        pending_dirty: usize,
    ) {
        let complete = progress == ScanProgress::Complete;
        let mode = if is_incremental {
            "incremental"
        } else {
            "full"
        };
        let stats = incremental_stats.unwrap_or_default();
        let discovered_files = if is_incremental {
            stats.discovered_files
        } else {
            scan_stats.files_indexed
        };
        let processed_files = if is_incremental {
            stats.processed_files
        } else {
            scan_stats.files_indexed
        };
        let text = if complete {
            if is_incremental {
                format!(
                    "Code Context：增量更新完成（新增 {}、修改 {}、删除 {}）",
                    stats.added_files, stats.modified_files, stats.deleted_files
                )
            } else {
                format!(
                    "Code Context：索引构建完成（{} 个文件）",
                    scan_stats.files_indexed
                )
            }
        } else if is_incremental {
            format!(
                "Code Context：正在增量更新（已处理 {} 个文件）",
                stats.processed_files
            )
        } else {
            format!(
                "Code Context：正在构建索引（已处理 {} 个文件）",
                scan_stats.files_indexed
            )
        };
        let _ = host.emit_event(&ExtensionEvent {
            name: INDEX_PROGRESS_EVENT.into(),
            data: json!({
                "status": if complete { IndexStatus::Degraded.as_str() } else { IndexStatus::Indexing.as_str() },
                "mode": mode,
                "complete": complete,
                "pending_dirty_paths": pending_dirty,
                "scan": scan_stats,
                "incremental": incremental_stats,
                "discovered_files": discovered_files,
                "processed_files": processed_files,
                "unchanged_files": stats.unchanged_files,
                "added_files": stats.added_files,
                "modified_files": stats.modified_files,
                "deleted_files": stats.deleted_files,
                "failed_files": stats.failed_files,
                "changed_chunks": stats.changed_chunks,
                "removed_chunks": stats.removed_chunks,
            }),
            presentation: Some(EventPresentation::footer(
                text,
                if complete {
                    EventPresentationTone::Info
                } else {
                    EventPresentationTone::Muted
                },
            )),
        });
    }

    /// 在有界集合中加入已确认成功写入的路径，避免恶意或异常事件耗尽插件内存。
    fn mark_dirty_paths(&mut self, paths: impl IntoIterator<Item = String>) {
        let Some(context) = self.repository.as_mut() else {
            return;
        };
        for path in paths {
            context.dirty_paths.insert(path);
            while context.dirty_paths.len() > incremental::MAX_DIRTY_PATHS {
                // 保留字典序较小的前缀，保证达到上限时结果仍然确定。
                let Some(last) = context.dirty_paths.iter().next_back().cloned() else {
                    break;
                };
                context.dirty_paths.remove(&last);
            }
        }
    }

    /// 记录一次可能修改文件的工具调用，等待成功终态后再标记脏路径。
    fn observe_tool_call(&mut self, event: &PluginEvent) {
        let Ok(data) = event.parse::<ToolCallEventData>() else {
            return;
        };
        if data.run_id.trim().is_empty() || data.call.id.trim().is_empty() {
            return;
        }
        let Some(context) = self.repository.as_mut() else {
            return;
        };
        let key = pending_write_key(&data.run_id, &data.call.id);
        if event.is_cancelled() {
            // 取消的重复投递也要清理先前登记的调用，避免悬挂记录一直占用容量。
            context.pending_writes.remove(&key);
            return;
        }
        if !incremental::is_write_tool(&data.call.name) {
            // 同一调用若被重写成只读工具，旧的写入登记不能继续影响成功结果。
            context.pending_writes.remove(&key);
            return;
        }
        let paths = incremental::extract_paths_in_workspace(
            &data.call.name,
            &data.call.args,
            &context.workspace_root,
        );
        if let Some(existing) = context.pending_writes.get_mut(&key) {
            // 同一调用可能因总线重投收到多次 call 事件；合并而不是重复分配记录。
            existing.paths.extend(paths);
            return;
        }
        if context.pending_writes.len() >= incremental::MAX_PENDING_WRITES {
            // 丢弃字典序最大的旧项，给最新调用留出确认机会；结果事件仍会走 content.path
            // 兜底，不会因为这里的内存保护而永久漏记一个成功写入。
            if let Some(largest) = context.pending_writes.keys().next_back().cloned() {
                context.pending_writes.remove(&largest);
            }
        }
        context.pending_writes.insert(
            key,
            incremental::PendingWrite {
                run_id: data.run_id,
                tool_name: data.call.name,
                paths,
            },
        );
    }

    /// 在工具成功终态到达后提交脏路径；失败、取消或非写入工具一律忽略。
    fn observe_tool_result(&mut self, event: &PluginEvent) {
        let Ok(data) = event.parse::<ToolResultEventData>() else {
            return;
        };
        if data.run_id.trim().is_empty() || data.result.call_id.trim().is_empty() {
            return;
        }
        if event.is_cancelled() || data.result.is_error {
            if let Some(context) = self.repository.as_mut() {
                let key = pending_write_key(&data.run_id, &data.result.call_id);
                context.pending_writes.remove(&key);
            }
            return;
        }
        let paths = {
            let Some(context) = self.repository.as_mut() else {
                return;
            };
            let key = pending_write_key(&data.run_id, &data.result.call_id);
            let pending = context.pending_writes.remove(&key);
            let tool_name = pending
                .as_ref()
                .map(|write| write.tool_name.clone())
                .unwrap_or_else(|| data.result.name.clone());
            if !incremental::is_write_tool(&tool_name) {
                return;
            }
            let mut paths = pending.map(|write| write.paths).unwrap_or_default();
            paths.extend(incremental::extract_result_paths_in_workspace(
                &tool_name,
                &data.result.content,
                &context.workspace_root,
            ));
            paths
        };
        self.mark_dirty_paths(paths);
    }

    /// 清理运行结束后没有收到终态结果的待处理写入，避免取消运行泄漏瞬时状态。
    fn finish_run(&mut self, event: &PluginEvent) {
        let Ok(data) = event.parse::<RunFinishEventData>() else {
            return;
        };
        if data.run_id.trim().is_empty() {
            return;
        }
        if let Some(context) = self.repository.as_mut() {
            context
                .pending_writes
                .retain(|_, pending| pending.run_id != data.run_id);
        }
    }

    /// 执行一次 `code_search` 调用。
    ///
    /// 完整流程：参数校验 → 索引推进 → 混合检索（符号 + 词法 + 语义）→ RRF 融合 →
    /// 上下文打包（去重、父子抑制、token 预算）→ 工具结果。语义通道不可用时自动
    /// 降级为两路检索，不返回错误。
    fn search(&mut self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        let request = match CodeSearchRequest::parse(&call.args) {
            Ok(request) => request,
            Err(error) => return Ok(ToolResult::error(call.id, call.name, error.to_string())),
        };

        if !self.config.enabled {
            return Ok(tool::unavailable_result(
                &call,
                self.index_status,
                "Code Context Engine 已在配置中禁用",
            ));
        }
        if let Some(reason) = self.unavailable_reason.clone() {
            return Ok(tool::unavailable_result(&call, self.index_status, &reason));
        }

        self.advance_index(host);

        if self.index_status == IndexStatus::Indexing {
            let stats = self.building_stats();
            return Ok(tool::unavailable_result_with_index(
                &call,
                self.index_status,
                &format!(
                    "代码索引正在构建（已处理 {} 个文件）；索引按 fuel 预算分片推进，请稍后重试",
                    stats.files
                ),
                Some(stats),
            ));
        }

        // 底层检索只支持单路径前缀；多前缀时不传 scope、由后置过滤取并集。
        let limit = request.max_results as usize;
        let single_scope = if request.path_scope.len() == 1 {
            Some(request.path_scope[0].as_str())
        } else {
            None
        };
        // 模型重排启用时把更多已排序候选交给上下文层，使父子抑制、路径并集和单文件
        // 多样性过滤之后仍有足够条目补满用户请求的数量。
        let context_candidate_limit = if self.config.rerank_model.is_some() {
            rerank::candidate_limit(limit)
        } else {
            limit
        };
        // 上下文层先合并同文件的重叠区间，再执行最终文件配额；在这里提前筛选会让
        // 多个窗口占满名额，将已经召回和重排的完整函数丢掉。
        let mut results = self
            .search_execution_with_file_quota(
                host,
                &request.query,
                single_scope,
                context_candidate_limit,
                false,
            )
            .results;
        if request.path_scope.len() > 1 {
            results.retain(|result| {
                request
                    .path_scope
                    .iter()
                    .any(|scope| result.file_path.starts_with(scope.as_str()))
            });
        }

        let bundle = {
            let idx = self.repository.as_ref().and_then(|ctx| ctx.index.as_ref());
            match idx {
                Some(index) => context::build(
                    host,
                    index,
                    results,
                    &context::ContextConfig {
                        token_budget: request.token_budget as u32,
                        max_results: limit,
                        prefer_method_locations: {
                            let query = AnalyzedQuery::analyze(&request.query);
                            query.intent == QueryIntent::NaturalLanguage
                                && !matches!(
                                    query.goal,
                                    QueryGoal::TypeLookup
                                        | QueryGoal::Definition
                                        | QueryGoal::CallSite
                                )
                        },
                        ..context::ContextConfig::default()
                    },
                ),
                None => context::ContextBundle {
                    entries: Vec::new(),
                    stats: context::ContextStats::default(),
                },
            }
        };

        let stats = self.index_stats();
        let degraded = match (
            self.degraded_reason.as_deref(),
            self.last_rerank_degraded_reason.as_deref(),
        ) {
            (Some(index), Some(rerank)) => Some(format!("{index}；{rerank}")),
            (Some(index), None) => Some(index.to_string()),
            (None, Some(rerank)) => Some(rerank.to_string()),
            (None, None) => None,
        };
        Ok(tool::search_result(
            &call,
            &bundle,
            self.index_status,
            degraded.as_deref(),
            stats,
        ))
    }

    /// 执行含语义通道的混合检索。
    ///
    /// 查询嵌入失败时降级为两路检索而不是返回错误：用户要的是结果，不是一条
    /// "嵌入端点挂了"的报错。降级原因记录在 `degraded_reason` 里供工具返回值携带。
    pub fn search_results(
        &mut self,
        host: &dyn PluginHostApi,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        self.search_execution(host, query, path_scope, limit)
            .results
    }

    /// 执行查询意图驱动的检索，并返回排序结果与完整阶段诊断。
    ///
    /// 与 [`Self::search_results`] 的结果一致；额外诊断供 benchmark、稳定性和延迟分档统计。
    /// 查询嵌入与模型重排仅在 SearchPlan 和置信度共同允许时发生。
    pub fn search_execution(
        &mut self,
        host: &dyn PluginHostApi,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> SearchExecution {
        self.search_execution_with_file_quota(host, query, path_scope, limit, true)
    }

    /// 共用检索流程，并按结果消费者决定是否执行最终文件配额。
    ///
    /// 直接返回检索列表时保留原配额；工具路径传入 `false`，交给上下文层在水合、
    /// 去重和合并后执行配额。工具保留已有有界候选池供合并后补位，不扩大模型候选或调用次数。
    fn search_execution_with_file_quota(
        &mut self,
        host: &dyn PluginHostApi,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
        apply_file_quota: bool,
    ) -> SearchExecution {
        self.last_rerank_degraded_reason = None;
        let rerank_model = self.config.rerank_model.clone();
        let mut analyzed = AnalyzedQuery::analyze(query);
        // "搜索主播开播代码"里的"搜索"到底是指令还是主题，只有语料能回答：本仓库从未出现
        // 过"搜索主播"这个组合，就说明它是指令。裁决必须在生成查询向量之前完成，否则被
        // 污染的文本会直接进入嵌入请求。
        if let Some(retrieval) = self
            .repository
            .as_ref()
            .and_then(|context| context.retrieval.as_ref())
        {
            analyzed.resolve_leading_directive(|term| retrieval.document_frequency(term));
        }
        let analyzed = analyzed;
        let semantic_available = self
            .repository
            .as_ref()
            .is_some_and(|context| context.semantic.is_ready());
        let plan =
            SearchPlan::for_query(&analyzed, limit, semantic_available, rerank_model.is_some());
        let query_vector = plan
            .enable_semantic
            .then(|| self.embed_query(host, &analyzed.retrieval_text))
            .flatten();
        let Some(context) = self.repository.as_ref() else {
            return empty_search_execution(analyzed, plan);
        };
        let (Some(index), Some(retrieval)) = (context.index.as_ref(), context.retrieval.as_ref())
        else {
            return empty_search_execution(analyzed, plan);
        };
        let mut run = retrieval.search_planned(
            index,
            &context.semantic,
            // 召回使用剥离指令词后的文本；意图与目标仍由原查询决定。
            &analyzed.retrieval_text,
            query_vector.as_deref(),
            path_scope,
            &plan,
        );
        #[cfg(feature = "sqlite-vector-store")]
        if let Some(error) = context.semantic.take_backend_error() {
            self.degraded_reason =
                Some(format!("sqlite-vec 查询失败，已退回内存语义检索：{error}"));
        }
        let confidence = rerank::assess_confidence(&analyzed, &run.results);
        let mut rerank_activated = plan.allow_rerank
            && rerank_model.is_some()
            && rerank::should_rerank(&analyzed, &run.results, &confidence);
        let mut rerank_applied = false;
        let mut rerank_reason = if !plan.allow_rerank {
            "查询计划禁用模型重排".to_string()
        } else if rerank_model.is_none() {
            "未配置模型重排".to_string()
        } else if rerank_activated && analyzed.intent == QueryIntent::NaturalLanguage {
            "自然语言缺少精确符号证据，使用源码相关度重排".to_string()
        } else if confidence.level == ConfidenceLevel::High {
            "本地检索置信度高，直接返回".to_string()
        } else if run.results.len() <= 1 {
            "候选不足两条，无需模型重排".to_string()
        } else if rerank_activated {
            "低置信度候选存在分歧，启用模型重排".to_string()
        } else {
            "混合查询已有多路证据或分差足够，保留本地排序".to_string()
        };
        if rerank_activated {
            let model = rerank_model.as_deref().expect("已确认配置了重排模型");
            let cache = &mut self
                .repository
                .as_mut()
                .expect("索引已就绪")
                .rerank_responses;
            // 重排与召回必须看同一份查询文本：指令词对交叉编码器同样是噪声，两处不一致
            // 还会让模型给出的名次无法与本地名次对齐。
            match rerank::rerank_results(
                host,
                model,
                &analyzed.retrieval_text,
                &run.results,
                cache,
            ) {
                Ok((reranked, cached)) => {
                    run.results = reranked;
                    rerank_applied = true;
                    if cached {
                        rerank_activated = false;
                        rerank_reason = "源码和请求未变化，复用已校验模型重排响应".to_string();
                    }
                }
                Err(reason) => {
                    self.last_rerank_degraded_reason =
                        Some(format!("模型重排不可用，已保留本地融合排序：{reason}"));
                    rerank_reason = format!("模型重排失败，保留本地排序：{reason}");
                }
            }
        }
        if apply_file_quota {
            run.results = retrieval::fusion::select_with_file_quota(
                std::mem::take(&mut run.results),
                limit,
                retrieval::fusion::FusionConfig::default().max_per_file,
            );
        }
        // 工具路径保留现有有界候选池：前排可能合并为同一段或超出预算，后续候选需能补位。
        // 最终数量和文件名额由上下文层控制，不另发模型请求。
        run.stages.final_results = run
            .results
            .iter()
            .map(|result| result.chunk_id.clone())
            .collect();
        let path = if matches!(
            analyzed.intent,
            QueryIntent::Identifier | QueryIntent::QualifiedSymbol
        ) || (!plan.enable_semantic && confidence.level == ConfidenceLevel::High)
        {
            SearchPath::Fast
        } else if rerank_activated {
            SearchPath::Hard
        } else {
            SearchPath::Normal
        };
        SearchExecution {
            results: run.results,
            diagnostics: SearchDiagnostics {
                analyzed_query: analyzed,
                plan,
                stages: run.stages,
                confidence,
                path,
                rerank_activated,
                rerank_applied,
                rerank_reason,
            },
        }
    }

    /// 计算查询向量；嵌入不可用或语义索引未就绪时返回 `None`。
    ///
    /// 语义索引未就绪时**不发起嵌入请求**：没有可比对的库内向量，这次调用纯属浪费
    /// 配额与延迟。
    fn embed_query(&mut self, host: &dyn PluginHostApi, query: &str) -> Option<Vec<f32>> {
        if !self
            .repository
            .as_ref()
            .is_some_and(|context| context.semantic.is_ready())
        {
            return None;
        }
        let provider = embedding::select_provider(&self.config, host);
        let space_id = provider.space()?.id();
        let query_hash = content_hash(query);
        if let Some(vector) = self.repository.as_ref().and_then(|context| {
            context
                .query_vectors
                .iter()
                .find(|(space, hash, _)| space == &space_id && hash == &query_hash)
        }) {
            return Some(vector.2.clone());
        }
        match provider.embed_query(query) {
            Ok(vector) => {
                // 不保留失败或异常超大输入；按 FIFO 淘汰，缓存向量总量最多约 1 MiB。
                if vector.len() <= 8192 {
                    if let Some(context) = self.repository.as_mut() {
                        if context.query_vectors.len() == 32 {
                            context.query_vectors.remove(0);
                        }
                        context
                            .query_vectors
                            .push((space_id, query_hash, vector.clone()));
                    }
                }
                Some(vector)
            }
            Err(error) => {
                self.degraded_reason = Some(format!("查询嵌入失败，已降级为两路检索：{error}"));
                None
            }
        }
    }

    /// 为已建成的结构索引计算语义向量。
    ///
    /// 返回本轮语义构建状态：未配置、仍可续建、已经就绪或永久不可用。
    ///
    /// 嵌入失败不会向上传播为结构索引错误：符号与词法两路已经就绪，语义缺席只降低召回
    /// 质量。429 与普通瞬时故障还需分开，使配额窗口未恢复时不会被停滞阈值永久降级。
    ///
    /// 嵌入空间发生变化时先整体重置——旧向量与新查询向量不可比较，混用会让检索结果
    /// 长期缓慢地不正确。
    fn build_semantic_index(&mut self, host: &dyn PluginHostApi) -> SemanticBuild {
        let provider = embedding::select_provider(&self.config, host);
        let Some(space) = provider.space() else {
            // 未配置嵌入 provider 是正常状态，不构成降级原因。
            return SemanticBuild::NotConfigured;
        };
        let Some(context) = self.repository.as_mut() else {
            return SemanticBuild::NotConfigured;
        };
        let Some(index) = context.index.as_ref() else {
            return SemanticBuild::NotConfigured;
        };
        if context.semantic.space().is_none() || !context.semantic.is_compatible_with(&space) {
            context.semantic.reset(space);
        }
        #[cfg(feature = "sqlite-vector-store")]
        if let Some(error) = context.semantic.take_backend_error() {
            self.degraded_reason = Some(format!("sqlite-vec 索引重置失败：{error}"));
            return SemanticBuild::Incomplete;
        }

        let semantic_chunks: Vec<&CodeChunk> = index
            .chunks()
            .filter(|chunk| participates_in_semantic_index(chunk))
            .collect();
        let semantic_total = semantic_chunks.len();
        let current_chunks: Vec<(&str, &str)> = semantic_chunks
            .iter()
            .map(|chunk| (chunk.id.as_str(), chunk.content_hash.as_str()))
            .collect();
        context.semantic.retain_current(&current_chunks);
        #[cfg(feature = "sqlite-vector-store")]
        if let Some(error) = context.semantic.take_backend_error() {
            self.degraded_reason = Some(format!("sqlite-vec 过期向量清理失败：{error}"));
            return SemanticBuild::Incomplete;
        }

        // 只嵌入尚无当前向量的 chunk：内容哈希可防止正文变化后误复用同 ID 的旧向量。
        //
        // 正文按文件缓存回读：待嵌入的 chunk 通常成片来自同几个文件，逐条读会把宿主调用
        // 次数放大到 chunk 数量级。读取失败的文件退化为只用元数据嵌入，不中断整批。
        let mut sources: BTreeMap<String, Option<String>> = BTreeMap::new();
        let pending: Vec<(String, String, String, String)> = semantic_chunks
            .iter()
            .copied()
            .filter(|chunk| {
                !context
                    .semantic
                    .contains_current(&chunk.id, &chunk.content_hash)
            })
            .map(|chunk| {
                let body = sources
                    .entry(chunk.file_path.clone())
                    .or_insert_with(|| host.read_file(&chunk.file_path).ok())
                    .as_deref()
                    .and_then(|content| slice_body(content, chunk.start_line, chunk.end_line));
                (
                    chunk.id.clone(),
                    chunk.file_path.clone(),
                    chunk.content_hash.clone(),
                    semantic_document_text(
                        chunk,
                        index
                            .file(&chunk.file_path)
                            .and_then(|file| file.doc.as_deref()),
                        body.as_deref(),
                    ),
                )
            })
            .collect();
        if pending.is_empty() {
            self.degraded_reason = None;
            return SemanticBuild::Ready;
        }

        let inputs: Vec<String> = pending.iter().map(|(_, _, _, text)| text.clone()).collect();
        // 可续嵌入：限流或端点抖动只中断本轮，已完成的向量照常写入，剩余部分留给下一次
        // `code_search` 继续。全有或全无会让中等以上规模的仓库永远建不完索引。
        let before = context.semantic.len();
        let outcome = provider.embed_documents_resumable(&inputs, MAX_EMBEDDING_BATCHES_PER_ROUND);
        let stopped_by = outcome.stopped_by;
        {
            let vectors = outcome.vectors;
            {
                let batch: Vec<(&str, &str, Option<&str>, &[f32])> = pending
                    .iter()
                    .zip(vectors.iter())
                    .map(|((chunk_id, file_path, content_hash, _), vector)| {
                        (
                            chunk_id.as_str(),
                            file_path.as_str(),
                            Some(content_hash.as_str()),
                            vector.as_slice(),
                        )
                    })
                    .collect();
                context.semantic.upsert_batch(&batch);
                #[cfg(feature = "sqlite-vector-store")]
                if let Some(error) = context.semantic.take_backend_error() {
                    self.degraded_reason = Some(format!("sqlite-vec 向量写入失败：{error}"));
                    return SemanticBuild::Incomplete;
                }
                // WASM 插件继续通过宿主 KV 保存 int4 快照；sqlite-vec 模式的 snapshot
                // 返回 None，避免同一批向量重复写进 JSON。
                if let Some(snapshot) = context.semantic.snapshot() {
                    if let Err(error) = context.store.save_semantic(host, &snapshot) {
                        self.degraded_reason = Some(format!("语义向量未能持久化：{error}"));
                        return SemanticBuild::Incomplete;
                    }
                }
            }
        }
        match stopped_by {
            None => {
                self.degraded_reason = None;
                self.semantic_stalled_rounds = 0;
                SemanticBuild::Ready
            }
            Some(error) if error.is_rate_limited() => {
                // 限流只代表当前配额窗口尚未恢复。即使连续多轮没有新增向量，也必须保留
                // 已完成部分并继续等待；转成 Unavailable 会让 5760/6600 这类索引永久残缺。
                self.semantic_stalled_rounds = 0;
                self.degraded_reason = Some(format!(
                    "语义索引等待限流恢复（已完成 {} / {} 个语义块）：{error}",
                    context.semantic.len(),
                    semantic_total
                ));
                SemanticBuild::Incomplete
            }
            Some(error) if error.is_transient() && context.semantic.len() > before => {
                // 有新向量产出说明端点仍在工作，只是没做完；重置停滞计数继续推进。
                self.semantic_stalled_rounds = 0;
                self.degraded_reason = Some(format!(
                    "语义索引仍在建立（已完成 {} / {} 个语义块）：{error}",
                    context.semantic.len(),
                    semantic_total
                ));
                SemanticBuild::Incomplete
            }
            Some(error) if error.is_transient() => {
                // 本轮一条新向量都没产出。重试若干轮仍无进展就落到终态，避免界面永远
                // 停在"正在建立索引"。
                self.semantic_stalled_rounds = self.semantic_stalled_rounds.saturating_add(1);
                if self.semantic_stalled_rounds > MAX_SEMANTIC_STALLED_ROUNDS {
                    self.degraded_reason = Some(format!("语义索引不可用：{error}"));
                    return SemanticBuild::Unavailable;
                }
                self.degraded_reason = Some(format!(
                    "语义索引仍在建立（已完成 {} / {} 个语义块）：{error}",
                    context.semantic.len(),
                    semantic_total
                ));
                SemanticBuild::Incomplete
            }
            Some(error) => {
                self.degraded_reason = Some(format!("语义索引不可用：{error}"));
                SemanticBuild::Unavailable
            }
        }
    }

    /// 在已建成的索引上执行两路检索，返回未融合的分通道候选。
    ///
    /// 这是 Phase 9 融合层的输入。返回的是**内部候选**，不是 Agent 可见结果。
    /// 索引尚未建成时返回空。
    pub fn search_candidates(
        &self,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<Vec<SearchCandidate>> {
        self.repository
            .as_ref()
            .and_then(|context| context.retrieval.as_ref())
            .map(|retrieval| retrieval.search_all(query, path_scope, limit))
            .unwrap_or_default()
    }

    /// 返回检索索引规模；尚未建成时为 `None`。
    pub fn retrieval_stats(&self) -> Option<RetrievalStats> {
        self.repository
            .as_ref()
            .and_then(|context| context.retrieval.as_ref())
            .map(RetrievalIndex::stats)
    }

    /// 返回构建进行中的索引规模。
    ///
    /// 构建未完成时索引还在 `build` 里，`index` 仍为 `None`，因此进度必须从构建中读取，
    /// 否则用户看到的永远是 0 个文件。
    fn building_stats(&self) -> IndexStats {
        self.repository
            .as_ref()
            .and_then(|context| context.build.as_ref())
            .map(|build| build.index().stats())
            .unwrap_or_default()
    }

    /// 返回已建成的结构索引；尚未建成时为 `None`。
    ///
    /// 供评测与诊断读取代码块分布。检索本身不经由该入口，因此它不构成新的检索路径。
    pub fn structural_index(&self) -> Option<&StructuralIndex> {
        self.repository
            .as_ref()
            .and_then(|context| context.index.as_ref())
    }

    /// 返回当前索引的规模统计；索引尚未建成时为 `None`。
    pub fn index_stats(&self) -> Option<IndexStats> {
        self.repository
            .as_ref()
            .and_then(|context| context.index.as_ref())
            .map(StructuralIndex::stats)
    }

    /// 返回当前结构索引；尚未建成时为 `None`。
    pub fn index(&self) -> Option<&StructuralIndex> {
        self.repository
            .as_ref()
            .and_then(|context| context.index.as_ref())
    }
}

impl AgentPlugin for CodeContextPlugin {
    /// 解析配置、附着仓库并注册工具。
    ///
    /// 缺少工作区身份或存储不是加载失败：引擎会进入明确的不可用状态并把原因发布出去，
    /// 而不是阻止整个应用启动。只有配置本身非法才拒绝加载——那是用户能立刻修好的问题。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.config = CodeContextConfig::from_metadata(&context.metadata)?;
        self.unavailable_reason = None;

        if !self.config.enabled {
            self.index_status = IndexStatus::NotIndexed;
            self.unavailable_reason = Some("已在配置中禁用".into());
            return self.emit_status(host);
        }

        self.index_status = IndexStatus::Initializing;
        let workspace_root = context
            .metadata
            .get(WORKSPACE_ROOT_KEY)
            .map(String::as_str)
            .unwrap_or_default();
        if workspace_root.trim().is_empty() {
            self.index_status = IndexStatus::Error;
            self.unavailable_reason = Some(CodeContextError::RepositoryUnavailable.to_string());
            return self.emit_status(host);
        }
        // 只列出仓库根一次来验证 Host 已提供 manifest 声明的可信读取边界；真正的递归扫描
        // 延迟到首次 code_search，并按预算分片执行。
        if let Err(error) = host.list_dir(".") {
            self.index_status = IndexStatus::Error;
            self.unavailable_reason = Some(
                CodeContextError::CapabilityUnavailable(format!(
                    "无法访问当前工作区根目录：{error}"
                ))
                .to_string(),
            );
            return self.emit_status(host);
        }
        if let Err(error) = self.attach_repository(host, workspace_root) {
            self.index_status = IndexStatus::Error;
            self.unavailable_reason = Some(error.to_string());
            return self.emit_status(host);
        }

        self.register_tool(host)?;
        self.emit_status(host)
    }

    /// 注销动态工具、落盘未保存的元数据并释放仓库上下文。
    ///
    /// 没有子进程、后台任务或 Agent 资源需要释放：本插件不申请这些能力。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        // 两项清理必须都执行：工具注销失败不能阻止 manifest flush，存储失败也不能让
        // 动态工具继续暴露。聚合错误后交给 Host 记录诊断。
        let unregister_result = self.unregister_tool(host);
        let detach_result = self.detach_repository(host);
        self.unavailable_reason = None;
        match (unregister_result, detach_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(tool_error), Ok(())) => Err(tool_error),
            (Ok(()), Err(storage_error)) => Err(storage_error.into()),
            (Err(tool_error), Err(storage_error)) => Err(anyhow::anyhow!(
                "Code Context 卸载清理失败：工具注销：{tool_error:#}；仓库解除：{storage_error}"
            )),
        }
    }

    /// 不使用静态工具注册。
    ///
    /// `list_tools` 在 `activate` 之前被调用，那时配置与仓库可用性都还未知。
    /// 本插件在 `activate` 中动态注册，见 [`CodeContextPlugin::register_tool`]。
    fn list_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// 观察工具生命周期，把成功的写入转换为有界脏路径集合。
    ///
    /// 该监听器不改写也不取消任何事件：写入必须等 Host 发布成功终态后才会生效，失败或
    /// 被取消的调用只清理待处理记录。运行结束事件负责回收没有终态的调用。
    fn on_event(&mut self, _host: &dyn PluginHostApi, event: &mut PluginEvent) -> EventOutcome {
        if self.repository.is_none() || !self.config.enabled {
            return EventOutcome::ready();
        }
        match event.name() {
            core_events::TOOL_CALL => self.observe_tool_call(event),
            core_events::TOOL_RESULT => self.observe_tool_result(event),
            core_events::RUN_FINISH => self.finish_run(event),
            _ => {}
        }
        EventOutcome::ready()
    }

    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        if call.name != TOOL_NAME {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                "Code Context Engine 未实现该工具",
            ));
        }
        self.search(host, call)
    }
}

export_plugin!(CodeContextPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{activation_context, TestHost};
    use agent_plugin::PluginEventEnvelope;
    use serde_json::Value;

    /// 激活一个附着到指定工作区的插件。
    fn activated(host: &TestHost, workspace_root: &str) -> CodeContextPlugin {
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(host, activation_context(workspace_root))
            .expect("激活插件");
        plugin
    }

    /// 构造一次 code_search 调用。
    fn search_call(args: Value) -> ToolCall {
        ToolCall::new("call-1", TOOL_NAME, args)
    }

    /// 构造一条供事件监听测试使用的插件事件。
    fn plugin_event(name: &str, data: Value) -> PluginEvent {
        PluginEvent::new(PluginEventEnvelope {
            event: name.to_string(),
            cancelled: false,
            canceller: None,
            subscriber: "code-context".to_string(),
            data,
        })
    }

    /// 构造工具调用生命周期事件。
    fn tool_call_event(run_id: &str, call_id: &str, name: &str, args: Value) -> PluginEvent {
        plugin_event(
            core_events::TOOL_CALL,
            serde_json::to_value(ToolCallEventData {
                run_id: run_id.to_string(),
                step: 0,
                call: ToolCall::new(call_id, name, args),
                block_reason: None,
                agent_guidance: None,
                cancel_run: false,
            })
            .expect("序列化工具调用事件"),
        )
    }

    /// 构造工具结果生命周期事件。
    fn tool_result_event(
        run_id: &str,
        call_id: &str,
        name: &str,
        content: Value,
        is_error: bool,
    ) -> PluginEvent {
        plugin_event(
            core_events::TOOL_RESULT,
            serde_json::to_value(ToolResultEventData {
                run_id: run_id.to_string(),
                step: 0,
                result: if is_error {
                    ToolResult::error(call_id, name, "写入失败")
                } else {
                    ToolResult::success(call_id, name, content)
                },
                origin: "test".to_string(),
            })
            .expect("序列化工具结果事件"),
        )
    }

    /// 激活后必须建立仓库身份、注册工具并进入未索引状态。
    /// 嵌入文档必须带上代码正文。
    ///
    /// 没有正文时，一个不写文档注释的函数就只剩路径和签名可嵌入，向量几乎只编码了
    /// "它叫什么、在哪个文件"；而没有结构化解析器的语言连符号和签名都没有，向量会退化
    /// 成一条路径。这正是"用自然语言描述某个行为却检索不到实现"的首要原因。
    #[test]
    fn semantic_document_includes_source_body() {
        let chunk = CodeChunk {
            id: "chunk".into(),
            repository_id: RepositoryIdentity::from_workspace_root("/repo").expect("仓库身份"),
            file_path: "src/session.rs".into(),
            language: "rust".into(),
            symbol_name: Some("save".into()),
            qualified_symbol: Some("SessionStore::save".into()),
            symbol_kind: Some(model::SymbolKind::Function),
            parent_symbol: Some("SessionStore".into()),
            start_line: 1,
            end_line: 3,
            signature: Some("pub fn save(&mut self)".into()),
            doc_comment: None,
            is_test: false,
            semantic_only: false,
            semantic_group_id: None,
            imports: Vec::new(),
            content_hash: "hash".into(),
        };

        let without = semantic_document_text(&chunk, None, None);
        let with = semantic_document_text(
            &chunk,
            None,
            Some("self.flush_to_disk();\nself.mark_clean();"),
        );

        assert!(!without.contains("flush_to_disk"));
        assert!(
            with.contains("flush_to_disk") && with.contains("mark_clean"),
            "嵌入文档必须包含正文：{with}"
        );
        assert!(with.contains("src/session.rs") && with.contains("SessionStore::save"));
        assert!(with.contains("父符号：SessionStore"));

        // 文件级文档必须进入嵌入文本：自然语言查询表达的常常是文件职责而非函数细节。
        let with_file_doc =
            semantic_document_text(&chunk, Some("会话持久化：把会话状态写入磁盘。"), None);
        assert!(
            with_file_doc.contains("会话持久化"),
            "嵌入文档必须包含文件级文档：{with_file_doc}"
        );
    }

    /// 被语义复合块覆盖的原子块不得重复生成向量。
    #[test]
    fn semantic_group_members_do_not_get_individual_vectors() {
        let mut member = CodeChunk {
            id: "member".into(),
            repository_id: RepositoryIdentity::from_workspace_root("/repo").expect("仓库身份"),
            file_path: "src/service.rs".into(),
            language: "rust".into(),
            symbol_name: Some("first".into()),
            qualified_symbol: Some("Service::first".into()),
            symbol_kind: Some(model::SymbolKind::Function),
            parent_symbol: Some("Service".into()),
            start_line: 1,
            end_line: 3,
            signature: Some("fn first(&self)".into()),
            doc_comment: None,
            is_test: false,
            semantic_only: false,
            semantic_group_id: Some("group".into()),
            imports: vec!["crate::domain::Order".into()],
            content_hash: "member-hash".into(),
        };
        assert!(!participates_in_semantic_index(&member));

        member.id = "group".into();
        member.semantic_only = true;
        member.semantic_group_id = None;
        assert!(participates_in_semantic_index(&member));

        let document = semantic_document_text(&member, None, Some("self.persist_order();"));
        assert!(document.contains("依赖：\ncrate::domain::Order"));
    }

    /// 超长正文必须按字符边界截断，不能在多字节字符中间切开。
    #[test]
    fn semantic_document_body_is_truncated_on_char_boundary() {
        let body = "中文注释".repeat(1_000);
        let truncated = truncate_chars(&body, MAX_SEMANTIC_BODY_CHARS);

        assert_eq!(truncated.chars().count(), MAX_SEMANTIC_BODY_CHARS);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn activation_attaches_repository_and_registers_tool() {
        let host = TestHost::new();
        let plugin = activated(&host, "/repo/a");

        assert!(plugin.repository_identity().is_some());
        assert_eq!(plugin.index_status(), IndexStatus::NotIndexed);
        assert!(plugin.unavailable_reason().is_none());
        assert_eq!(host.registered_tools(), vec![TOOL_NAME.to_string()]);
        assert_eq!(host.emitted_events(), vec![STATUS_EVENT.to_string()]);
    }

    /// 首次附着必须写入 manifest，使重启后能识别同一仓库。
    #[test]
    fn activation_persists_manifest() {
        let host = TestHost::new();
        let plugin = activated(&host, "/repo/a");

        let store = IndexStore::probe(&host);
        match store.load_manifest(&host) {
            ManifestLoad::Loaded(manifest) => {
                assert_eq!(
                    Some(&manifest.repository_identity),
                    plugin.repository_identity()
                );
            }
            other => panic!("激活后应存在 manifest，实际为 {other:?}"),
        }
    }

    /// 重启插件后必须恢复同一份 manifest，而不是重新创建。
    #[test]
    fn manifest_survives_plugin_restart() {
        let host = TestHost::new();
        let mut first = activated(&host, "/repo/a");
        let created_at = match IndexStore::probe(&host).load_manifest(&host) {
            ManifestLoad::Loaded(manifest) => manifest.created_at,
            other => panic!("首次激活应写入 manifest，实际为 {other:?}"),
        };
        first.deactivate(&host).expect("卸载插件");

        let restored_host = TestHost::restored_from(host.snapshot());
        let second = activated(&restored_host, "/repo/a");

        match IndexStore::probe(&restored_host).load_manifest(&restored_host) {
            ManifestLoad::Loaded(manifest) => {
                assert_eq!(manifest.created_at, created_at, "重启不得重建 manifest");
                assert_eq!(
                    Some(&manifest.repository_identity),
                    second.repository_identity()
                );
            }
            other => panic!("重启后应恢复 manifest，实际为 {other:?}"),
        }
    }

    /// 不同仓库必须得到互相隔离的身份与 manifest。
    #[test]
    fn repositories_stay_isolated() {
        let host_a = TestHost::new();
        let host_b = TestHost::new();
        let plugin_a = activated(&host_a, "/repo/a");
        let plugin_b = activated(&host_b, "/repo/b");

        assert_ne!(
            plugin_a.repository_identity(),
            plugin_b.repository_identity()
        );

        // 同一份存储被另一个仓库附着时，必须判定为需要全量重建而不是复用。
        let cross_host = TestHost::restored_from(host_a.snapshot());
        let cross = activated(&cross_host, "/repo/b");
        assert_eq!(cross.index_status(), IndexStatus::NeedsFullRebuild);
    }

    /// 损坏的 manifest 必须进入全量重建，且不能让激活失败。
    #[test]
    fn corrupted_manifest_enters_full_rebuild() {
        let host = TestHost::new();
        host.seed_workspace("index.manifest", json!({"schema_version": "损坏"}));

        let plugin = activated(&host, "/repo/a");
        assert_eq!(plugin.index_status(), IndexStatus::NeedsFullRebuild);
        assert!(plugin.unavailable_reason().is_none());
    }

    /// 未来 schema 版本必须进入全量重建，而不是让插件加载失败。
    #[test]
    fn future_schema_version_enters_full_rebuild() {
        let host = TestHost::new();
        let mut manifest =
            IndexManifest::new(RepositoryIdentity::from_workspace_root("/repo/a").expect("身份"));
        manifest.schema_version = 999;
        host.seed_workspace(
            "index.manifest",
            serde_json::to_value(&manifest).expect("序列化"),
        );

        let plugin = activated(&host, "/repo/a");
        assert_eq!(plugin.index_status(), IndexStatus::NeedsFullRebuild);
    }

    /// 宿主未注入工作区身份时必须优雅降级，而不是阻止插件加载。
    #[test]
    fn missing_workspace_root_degrades_gracefully() {
        let host = TestHost::new();
        let mut plugin = CodeContextPlugin::default();
        let mut context = activation_context("/repo/a");
        context.metadata.remove(WORKSPACE_ROOT_KEY);

        plugin
            .activate(&host, context)
            .expect("缺少工作区身份不应阻止加载");

        assert_eq!(plugin.index_status(), IndexStatus::Error);
        assert!(plugin.unavailable_reason().is_some());
        assert!(host.registered_tools().is_empty(), "不可用时不应注册工具");
    }

    /// Manifest 声明了工作区读取能力，但 Host 未提供实现时必须优雅降级。
    #[test]
    fn missing_workspace_read_capability_degrades_gracefully() {
        let host = TestHost::without_workspace_read();
        let mut plugin = CodeContextPlugin::default();

        plugin
            .activate(&host, activation_context("/repo/a"))
            .expect("缺少工作区读取能力不应导致插件 panic");

        assert_eq!(plugin.index_status(), IndexStatus::Error);
        assert!(plugin
            .unavailable_reason()
            .is_some_and(|reason| reason.contains("工作区")));
        assert!(host.registered_tools().is_empty());
    }

    /// 宿主未提供项目作用域存储时仍应加载，并如实报告未索引。
    #[test]
    fn missing_workspace_storage_degrades_gracefully() {
        let host = TestHost::without_workspace_scope();
        let plugin = activated(&host, "/repo/a");

        assert_eq!(plugin.index_status(), IndexStatus::NotIndexed);
        assert!(host.user_keys().is_empty(), "不得把项目索引写进用户级存储");
    }

    /// 配置禁用时不得注册工具。
    #[test]
    fn disabled_plugin_registers_no_tool() {
        let host = TestHost::new();
        let mut plugin = CodeContextPlugin::default();
        let mut context = activation_context("/repo/a");
        context.metadata.insert("enabled".into(), "false".into());

        plugin.activate(&host, context).expect("禁用配置应正常加载");

        assert!(host.registered_tools().is_empty());
        assert!(plugin.unavailable_reason().is_some());
    }

    /// 非法配置必须阻止加载，让用户立刻看到问题。
    #[test]
    fn invalid_configuration_blocks_activation() {
        let host = TestHost::new();
        let mut plugin = CodeContextPlugin::default();
        let mut context = activation_context("/repo/a");
        context
            .metadata
            .insert("embedding_mode".into(), "cloud".into());

        assert!(plugin.activate(&host, context).is_err());
    }

    /// 空仓库检索返回成功但结果为空，状态为 degraded（无语义通道）。
    #[test]
    fn search_on_empty_repo_returns_success_with_no_results() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let result = plugin
            .call_tool_with_host(&host, search_call(json!({"query": "权限在哪里检查"})))
            .expect("工具调用不应失败");

        assert!(!result.is_error, "索引就绪时应返回成功结果");
        assert_eq!(result.content["status"], IndexStatus::Degraded.as_str());
        assert_eq!(result.content["results"], json!([]));
        let details = result.details.expect("应带结构化细节");
        assert_eq!(details["status"], IndexStatus::Degraded.as_str());
    }

    /// 索引就绪后 code_search 必须返回包含代码片段的成功结果。
    #[test]
    fn search_returns_context_entries_when_index_is_ready() {
        let host = crate::testing::RepoHost::new(&[
            ("src/a.rs", "pub fn alpha() {}\n"),
            ("src/b.rs", "pub struct Beta;\n"),
        ]);
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, activation_context("/repo/a"))
            .expect("激活插件");

        let result = plugin
            .call_tool_with_host(&host, search_call(json!({"query": "alpha"})))
            .expect("工具调用不应失败");

        assert!(!result.is_error, "索引就绪时检索应返回成功结果");
        assert_eq!(result.content["status"], IndexStatus::Degraded.as_str());
        let results = result.content["results"]
            .as_array()
            .expect("results 应是数组");
        assert!(!results.is_empty(), "应返回匹配的代码片段");
        let first = &results[0];
        assert!(first["file"].is_string(), "结果应包含文件路径");
        assert!(first["lines"].is_array(), "结果应包含行区间");
        assert!(
            first["retrieval_lines"].is_array(),
            "结果应包含原始检索区间"
        );
        assert!(
            first["hydrated_lines"].is_array(),
            "结果应包含完整正文目标区间"
        );
        assert!(first["role"].is_string(), "结果应包含代码角色");
        assert!(first["snippet"].is_string(), "结果应包含代码片段");

        let details = result.details.expect("应带构建统计");
        assert!(details["stats"]["returned"].as_u64().unwrap_or(0) > 0);
    }

    /// 首次 code_search 必须触发索引构建，而不是让索引永远停在未建立状态。
    #[test]
    fn search_drives_index_build() {
        let host = crate::testing::RepoHost::new(&[
            ("src/a.rs", "pub fn alpha() {}\n"),
            ("src/b.rs", "pub struct Beta;\n"),
        ]);
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, activation_context("/repo/a"))
            .expect("激活插件");

        assert!(plugin.index().is_none(), "activate 不得构建索引");

        plugin
            .call_tool_with_host(&host, search_call(json!({"query": "alpha"})))
            .expect("工具调用不应失败");

        let stats = plugin.index_stats().expect("检索调用后索引应已建成");
        assert_eq!(stats.files, 2);
        assert!(stats.symbols >= 2);
        assert_eq!(plugin.index_status(), IndexStatus::Degraded);
    }

    /// 索引建成后必须把文件哈希表落盘，供下次启动做增量判断。
    #[test]
    fn completed_build_persists_file_hashes() {
        let host = crate::testing::RepoHost::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, activation_context("/repo/a"))
            .expect("激活插件");
        plugin
            .call_tool_with_host(&host, search_call(json!({"query": "alpha"})))
            .expect("工具调用不应失败");

        let persisted = IndexStore::probe(&host).load_file_hashes(&host);
        assert_eq!(persisted.len(), 1);
        assert!(persisted.contains_key("src/a.rs"));
    }

    /// 参数错误必须与索引缺失区分开。
    #[test]
    fn search_rejects_blank_query() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let result = plugin
            .call_tool_with_host(&host, search_call(json!({"query": "  "})))
            .expect("工具调用不应失败");

        assert!(result.is_error);
        assert!(result.details.is_none(), "参数错误不应带索引状态细节");
    }

    /// 未知工具名必须安全返回错误。
    #[test]
    fn unknown_tool_is_rejected() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let result = plugin
            .call_tool_with_host(&host, ToolCall::new("call-1", "other_tool", json!({})))
            .expect("未知工具不应 panic");
        assert!(result.is_error);
    }

    /// 只有成功的写入结果才会把调用参数中的路径加入脏集合。
    #[test]
    fn successful_write_result_marks_dirty_paths() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");
        let mut call = tool_call_event(
            "run-1",
            "write-1",
            "write_file",
            json!({"path": "src/a.rs", "content": "fn a() {}"}),
        );
        assert_eq!(plugin.on_event(&host, &mut call), EventOutcome::ready());
        let mut result = tool_result_event(
            "run-1",
            "write-1",
            "write_file",
            json!({"path": "/repo/a/src/a.rs", "bytes_written": 9}),
            false,
        );
        assert_eq!(plugin.on_event(&host, &mut result), EventOutcome::ready());

        let context = plugin.repository.as_ref().expect("仓库上下文");
        assert_eq!(
            context.dirty_paths,
            BTreeSet::from(["src/a.rs".to_string()])
        );
        assert!(context.pending_writes.is_empty());
    }

    /// 失败、取消和只读工具结果都不得触发增量重建。
    #[test]
    fn failed_cancelled_and_read_results_do_not_mark_dirty() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let mut failed_call =
            tool_call_event("run-1", "write-1", "edit_file", json!({"path": "src/a.rs"}));
        plugin.on_event(&host, &mut failed_call);
        let mut failed = tool_result_event(
            "run-1",
            "write-1",
            "edit_file",
            json!({"path": "src/a.rs"}),
            true,
        );
        plugin.on_event(&host, &mut failed);

        let mut read_result = tool_result_event(
            "run-1",
            "read-1",
            "read_file",
            json!({"path": "src/read.rs"}),
            false,
        );
        plugin.on_event(&host, &mut read_result);

        let context = plugin.repository.as_ref().expect("仓库上下文");
        assert!(context.dirty_paths.is_empty());
        assert!(context.pending_writes.is_empty());
    }

    /// 重复投递同一调用与结果必须保持幂等，并且运行结束会清理悬挂调用。
    #[test]
    fn duplicate_events_are_idempotent_and_finish_cleans_pending() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");
        let mut first_call = tool_call_event(
            "run-1",
            "write-1",
            "rename_file",
            json!({"old_path": "src/old.rs", "new_path": "src/new.rs"}),
        );
        plugin.on_event(&host, &mut first_call);
        let mut duplicate_call = tool_call_event(
            "run-1",
            "write-1",
            "rename_file",
            json!({"old_path": "src/old.rs", "new_path": "src/new.rs"}),
        );
        plugin.on_event(&host, &mut duplicate_call);
        let mut result = tool_result_event(
            "run-1",
            "write-1",
            "rename_file",
            json!({"path": "src/new.rs"}),
            false,
        );
        plugin.on_event(&host, &mut result);
        let mut duplicate_result = tool_result_event(
            "run-1",
            "write-1",
            "rename_file",
            json!({"path": "src/new.rs"}),
            false,
        );
        plugin.on_event(&host, &mut duplicate_result);

        let context = plugin.repository.as_ref().expect("仓库上下文");
        assert_eq!(
            context.dirty_paths,
            BTreeSet::from(["src/new.rs".to_string(), "src/old.rs".to_string()])
        );

        let mut cancellable_call = tool_call_event(
            "run-cancel",
            "write-cancel",
            "write_file",
            json!({"path": "src/cancelled.rs"}),
        );
        plugin.on_event(&host, &mut cancellable_call);
        let mut cancelled_duplicate = tool_call_event(
            "run-cancel",
            "write-cancel",
            "write_file",
            json!({"path": "src/cancelled.rs"}),
        );
        cancelled_duplicate.cancel();
        plugin.on_event(&host, &mut cancelled_duplicate);
        assert!(plugin
            .repository
            .as_ref()
            .expect("仓库上下文")
            .pending_writes
            .is_empty());

        let mut pending_call = tool_call_event(
            "run-2",
            "write-2",
            "write_file",
            json!({"path": "src/pending.rs"}),
        );
        plugin.on_event(&host, &mut pending_call);
        let mut finish = plugin_event(
            core_events::RUN_FINISH,
            json!({"run_id": "run-2", "steps_used": 1, "cancelled": true}),
        );
        plugin.on_event(&host, &mut finish);
        assert!(plugin
            .repository
            .as_ref()
            .expect("仓库上下文")
            .pending_writes
            .is_empty());
    }

    /// 没有 call 事件时，成功写入结果中的相对或工作区内绝对路径仍可作为兜底。
    #[test]
    fn successful_result_uses_path_fallback_without_call_event() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");
        let mut result = tool_result_event(
            "run-1",
            "write-1",
            "write_file",
            json!({"path": "/repo/a/src/fallback.rs"}),
            false,
        );
        plugin.on_event(&host, &mut result);
        assert_eq!(
            plugin.repository.as_ref().expect("仓库上下文").dirty_paths,
            BTreeSet::from(["src/fallback.rs".to_string()])
        );
    }

    /// 工作区之外的绝对路径必须被拒绝，即使它出现在成功结果中。
    #[test]
    fn outside_workspace_result_is_rejected() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");
        let mut result = tool_result_event(
            "run-1",
            "write-1",
            "write_file",
            json!({"path": "/repo/other/src/a.rs"}),
            false,
        );
        plugin.on_event(&host, &mut result);
        assert!(plugin
            .repository
            .as_ref()
            .expect("仓库上下文")
            .dirty_paths
            .is_empty());
    }

    /// 已建成索引收到成功写入事件后，下一次搜索应走增量构建并保留未变更文件。
    #[test]
    fn search_triggers_incremental_build_after_successful_write() {
        let initial = crate::testing::RepoHost::new(&[
            ("src/keep.rs", "pub fn keep() {}\n"),
            ("src/change.rs", "pub fn old_name() {}\n"),
        ]);
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&initial, activation_context("/repo/a"))
            .expect("激活插件");
        plugin
            .call_tool_with_host(&initial, search_call(json!({"query": "old_name"})))
            .expect("首次搜索应推进全量构建");
        assert!(plugin
            .index()
            .expect("全量索引")
            .file("src/change.rs")
            .is_some());

        let mut call = tool_call_event(
            "run-1",
            "write-1",
            "edit_file",
            json!({"path": "src/change.rs"}),
        );
        plugin.on_event(&initial, &mut call);
        let mut result = tool_result_event(
            "run-1",
            "write-1",
            "edit_file",
            json!({"path": "src/change.rs"}),
            false,
        );
        plugin.on_event(&initial, &mut result);

        let changed = crate::testing::RepoHost::new(&[
            ("src/keep.rs", "pub fn keep() {}\n"),
            ("src/change.rs", "pub fn new_name() {}\n"),
        ]);
        plugin
            .call_tool_with_host(&changed, search_call(json!({"query": "new_name"})))
            .expect("增量构建应推进完成");

        let index = plugin.index().expect("增量索引");
        assert!(index.file("src/keep.rs").is_some());
        assert!(index.symbols().any(|symbol| symbol.name == "new_name"));
        assert!(!index.symbols().any(|symbol| symbol.name == "old_name"));
        assert!(plugin
            .repository
            .as_ref()
            .expect("仓库上下文")
            .dirty_paths
            .is_empty());
    }

    /// 索引分片必须发布带 Footer 展示的结构化进度，并在完成时发布终态快照。
    #[test]
    fn index_progress_event_contains_footer_and_terminal_payload() {
        let files: Vec<(&'static str, &'static str)> = (0..40)
            .map(|index| {
                let path: &'static str = Box::leak(format!("src/file-{index}.rs").into_boxed_str());
                (path, "pub fn indexed() {}\n")
            })
            .collect();
        let host = crate::testing::RepoHost::new(&files);
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, activation_context("/repo/a"))
            .expect("激活插件");
        plugin
            .call_tool_with_host(&host, search_call(json!({"query": "indexed"})))
            .expect("首次搜索");

        let first_progress = host
            .emitted_event_payloads()
            .into_iter()
            .find(|event| event.name == INDEX_PROGRESS_EVENT)
            .expect("应发布索引进度事件");
        assert_eq!(first_progress.data["mode"], "full");
        assert_eq!(first_progress.data["complete"], false);
        assert_eq!(
            first_progress
                .presentation
                .as_ref()
                .map(|presentation| &presentation.target),
            Some(&agent_plugin::EventPresentationTarget::Footer)
        );

        for _ in 0..8 {
            if plugin.index().is_some() {
                break;
            }
            plugin
                .call_tool_with_host(&host, search_call(json!({"query": "indexed"})))
                .expect("继续构建");
        }
        assert!(plugin.index().is_some(), "分片构建应在有限轮次内完成");
        let terminal = host
            .emitted_event_payloads()
            .into_iter()
            .rev()
            .find(|event| event.name == INDEX_PROGRESS_EVENT)
            .expect("应发布终态进度事件");
        assert_eq!(terminal.data["complete"], true);
        assert_eq!(
            terminal
                .presentation
                .as_ref()
                .map(|presentation| &presentation.target),
            Some(&agent_plugin::EventPresentationTarget::Footer)
        );
        let terminal_count = host
            .emitted_event_payloads()
            .into_iter()
            .filter(|event| event.name == INDEX_PROGRESS_EVENT && event.data["complete"] == true)
            .count();
        assert_eq!(terminal_count, 1, "一次构建只能发布一个终态事件");
    }

    /// 宿主未注入 HTTP 端点时绝不能返回任何伪造向量。
    #[test]
    fn embedding_provider_is_unavailable_without_endpoint() {
        let host = TestHost::new();
        let plugin = activated(&host, "/repo/a");

        let provider = plugin.embedding_provider(&host);
        assert!(provider.space().is_none());
        assert!(provider
            .embed(EmbeddingInputKind::Query, &["x".to_string()])
            .is_err());
    }

    // -----------------------------------------------------------------------
    // 端到端语义检索
    // -----------------------------------------------------------------------

    /// 语义链路测试用的最小仓库。
    const SEMANTIC_FIXTURE: &[(&str, &str)] = &[(
        "src/permission.rs",
        "/// 检查工具调用是否需要审批。\npub fn check_permission(id: &str) -> bool {\n    id.is_empty()\n}\n",
    )];

    /// 持续限流恢复测试使用的多批语义仓库。
    const SEMANTIC_RATE_LIMIT_FIXTURE: &[(&str, &str)] = &[
        (
            "src/permission.rs",
            "/// 检查工具调用是否需要审批。\npub fn check_permission(id: &str) -> bool {\n    id.is_empty()\n}\n",
        ),
        (
            "src/session.rs",
            "/// 恢复指定会话。\npub fn resume_session(id: &str) -> bool {\n    !id.is_empty()\n}\n",
        ),
        (
            "src/storage.rs",
            "/// 保存索引快照。\npub fn save_snapshot(id: &str) -> bool {\n    !id.is_empty()\n}\n",
        ),
    ];

    /// 构造启用 API 嵌入的激活上下文。
    fn api_activation_context(workspace_root: &str) -> ActivationContext {
        let mut context = activation_context(workspace_root);
        context
            .metadata
            .insert("embedding_dimension".into(), "4".into());
        // 离线测试验证状态机，不实际等待限速窗口。
        context
            .metadata
            .insert("embedding_request_interval_ms".into(), "0".into());
        context
            .metadata
            .insert("embedding_max_retries".into(), "0".into());
        context
            .metadata
            .insert("embedding_retry_base_ms".into(), "0".into());
        // 该键正常由宿主注入，测试里直接给出。
        context
            .metadata
            .insert("http_endpoint_id".into(), "test-endpoint".into());
        context
    }

    /// 在带仓库内容的宿主上激活并推进索引至完成。
    fn indexed(host: &crate::testing::RepoHost, context: ActivationContext) -> CodeContextPlugin {
        let mut plugin = CodeContextPlugin::default();
        plugin.activate(host, context).expect("激活插件");
        // 构建按 fuel 预算分片；小仓库通常一轮完成，这里仍循环以免依赖分片粒度。
        for _ in 0..8 {
            plugin.advance_index(host);
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
        }
        plugin
    }

    /// 嵌入可用时必须建立语义索引并报告 Ready。
    #[test]
    fn semantic_index_is_built_when_embedding_is_available() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_echo_embeddings(4);
        let plugin = indexed(&host, api_activation_context("/repo/a"));

        assert_eq!(
            plugin.index_status(),
            IndexStatus::Ready,
            "语义索引建成后应报告 Ready 而不是 Degraded"
        );
        assert!(!host.http_requests().is_empty(), "必须真的调用嵌入端点");
    }

    /// 重复查询省去请求，缓存有界且不能跨嵌入空间或仓库生命周期复用。
    #[test]
    fn query_vectors_are_bounded_and_scoped_to_space_and_repository() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_echo_embeddings(4);
        let mut plugin = indexed(&host, api_activation_context("/repo/a"));
        let initial = host.http_requests().len();
        let first = plugin.embed_query(&host, "库存查询").expect("查询向量");
        assert_eq!(plugin.embed_query(&host, "库存查询"), Some(first));
        assert_eq!(host.http_requests().len(), initial + 1);
        for n in 0..33 {
            plugin
                .embed_query(&host, &format!("查询 {n}"))
                .expect("查询向量");
        }
        assert_eq!(plugin.repository.as_ref().unwrap().query_vectors.len(), 32);
        let count = host.http_requests().len();
        plugin
            .embed_query(&host, "库存查询")
            .expect("被淘汰查询重新嵌入");
        assert_eq!(host.http_requests().len(), count + 1);
        plugin.repository.as_mut().unwrap().query_vectors[31].0 = "old-space".into();
        plugin
            .embed_query(&host, "库存查询")
            .expect("旧空间不可复用");
        assert_eq!(host.http_requests().len(), count + 2);
        plugin
            .activate(&host, api_activation_context("/repo/b"))
            .expect("切换仓库");
        assert!(plugin.repository.as_ref().unwrap().query_vectors.is_empty());
    }

    /// 瞬时请求失败不得进入缓存，恢复后必须重新生成查询向量。
    #[test]
    fn failed_query_embeddings_are_not_cached() {
        let good = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_echo_embeddings(4);
        let mut plugin = indexed(&good, api_activation_context("/repo/a"));
        let failing = crate::testing::RepoHost::new(SEMANTIC_FIXTURE);
        assert!(plugin.embed_query(&failing, "未缓存查询").is_none());
        assert!(plugin.embed_query(&failing, "未缓存查询").is_none());
        assert!(plugin.repository.as_ref().unwrap().query_vectors.is_empty());
        assert!(plugin.embed_query(&good, "未缓存查询").is_some());
    }

    /// 磁盘快照必须让下一进程跳过未变化代码块，并让内容变化的代码块重新嵌入。
    #[test]
    fn persisted_semantic_index_reuses_only_current_chunks() {
        let cache_path = std::env::temp_dir().join(format!(
            "lucia-code-context-semantic-{}.json",
            std::process::id()
        ));
        match std::fs::remove_file(&cache_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("清理旧测试缓存失败：{error}"),
        }
        let first_host = crate::testing::RepoHost::persisted(SEMANTIC_FIXTURE, &cache_path)
            .expect("创建首次磁盘宿主")
            .with_echo_embeddings(4);
        let first = indexed(&first_host, api_activation_context("/repo/a"));
        assert_eq!(first.index_status(), IndexStatus::Ready);
        assert_eq!(first_host.http_requests().len(), 1, "首次必须生成文档向量");
        drop(first);
        drop(first_host);

        let reopened = crate::testing::RepoHost::persisted(SEMANTIC_FIXTURE, &cache_path)
            .expect("重新打开磁盘宿主")
            .with_echo_embeddings(4);
        let reused = indexed(&reopened, api_activation_context("/repo/a"));
        assert_eq!(reused.index_status(), IndexStatus::Ready);
        assert!(
            reopened.http_requests().is_empty(),
            "未变化代码块应直接复用磁盘向量"
        );
        drop(reused);
        drop(reopened);

        let changed_fixture = &[(
            "src/permission.rs",
            "/// 检查工具调用是否需要人工审批。\npub fn check_permission(id: &str) -> bool {\n    !id.is_empty()\n}\n",
        )];
        let changed_host = crate::testing::RepoHost::persisted(changed_fixture, &cache_path)
            .expect("打开变更后的磁盘宿主")
            .with_echo_embeddings(4);
        let changed = indexed(&changed_host, api_activation_context("/repo/a"));
        assert_eq!(changed.index_status(), IndexStatus::Ready);
        assert_eq!(
            changed_host.http_requests().len(),
            1,
            "正文变化后必须重新生成向量"
        );

        std::fs::remove_file(&cache_path).expect("清理语义缓存测试文件");
    }

    /// sqlite-vec 必须迁移旧 JSON 向量，并在独立插件实例中继续复用与执行语义检索。
    #[cfg(feature = "sqlite-vector-store")]
    #[test]
    fn sqlite_vector_database_migrates_snapshot_and_survives_reopen() {
        let base = std::env::temp_dir().join(format!(
            "lucia-code-context-sqlite-migration-{}",
            std::process::id()
        ));
        let metadata_path = base.with_extension("json");
        let vector_path = base.with_extension("sqlite3");
        for path in [
            metadata_path.clone(),
            vector_path.clone(),
            vector_path.with_file_name(format!(
                "{}-wal",
                vector_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )),
            vector_path.with_file_name(format!(
                "{}-shm",
                vector_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )),
        ] {
            let _ = std::fs::remove_file(path);
        }

        let legacy_host = crate::testing::RepoHost::persisted(SEMANTIC_FIXTURE, &metadata_path)
            .expect("创建旧快照宿主")
            .with_echo_embeddings(4);
        let legacy = indexed(&legacy_host, api_activation_context("/repo/a"));
        assert_eq!(legacy_host.http_requests().len(), 1, "首次应生成文档向量");
        drop(legacy);
        drop(legacy_host);

        let migrated_host = crate::testing::RepoHost::persisted(SEMANTIC_FIXTURE, &metadata_path)
            .expect("打开待迁移快照")
            .with_echo_embeddings(4);
        let mut migrated = CodeContextPlugin::default().with_sqlite_vector_database(&vector_path);
        migrated
            .activate(&migrated_host, api_activation_context("/repo/a"))
            .expect("激活 sqlite-vec 插件");
        for _ in 0..8 {
            migrated.advance_index(&migrated_host);
            if migrated.index_status() != IndexStatus::Indexing {
                break;
            }
        }
        assert_eq!(migrated.index_status(), IndexStatus::Ready);
        assert!(vector_path.is_file(), "迁移后必须生成 sqlite3 数据库");
        assert!(
            migrated_host.http_requests().is_empty(),
            "迁移旧快照时不得重新生成文档向量"
        );
        assert!(
            migrated_host
                .store_get(agent_plugin::PluginStoreScope::Workspace, "index.semantic")
                .expect("读取旧快照键")
                .is_none(),
            "迁移成功后必须删除重复 JSON 向量"
        );
        drop(migrated);
        drop(migrated_host);

        let reopened_host = crate::testing::RepoHost::persisted(SEMANTIC_FIXTURE, &metadata_path)
            .expect("重新打开元数据")
            .with_echo_embeddings(4);
        let mut reopened = CodeContextPlugin::default().with_sqlite_vector_database(&vector_path);
        reopened
            .activate(&reopened_host, api_activation_context("/repo/a"))
            .expect("重新打开 sqlite-vec 插件");
        for _ in 0..8 {
            reopened.advance_index(&reopened_host);
            if reopened.index_status() != IndexStatus::Indexing {
                break;
            }
        }
        assert_eq!(reopened.index_status(), IndexStatus::Ready);
        assert!(
            reopened_host.http_requests().is_empty(),
            "重开时应复用文档向量"
        );
        let execution = reopened.search_execution(&reopened_host, "permission check", None, 10);
        assert!(
            !execution.results.is_empty(),
            "sqlite-vec KNN 应参与混合检索"
        );
        assert!(execution.diagnostics.plan.enable_semantic);
        assert!(
            !execution.diagnostics.stages.semantic.is_empty(),
            "重开后的 sqlite-vec 必须产生语义候选"
        );
        assert_eq!(
            reopened_host.http_requests().len(),
            1,
            "搜索只应生成一次查询向量"
        );
        drop(reopened);
        drop(reopened_host);

        for path in [
            metadata_path,
            vector_path.clone(),
            vector_path.with_file_name(format!(
                "{}-wal",
                vector_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )),
            vector_path.with_file_name(format!(
                "{}-shm",
                vector_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }

    /// 持续限流必须保留已有向量与可续状态，并在端点恢复后把语义索引补齐。
    ///
    /// 这是语义检索能否在真实仓库上建成的关键：嵌入几千个代码块需要几百次请求，429 几乎
    /// 必然发生。旧实现是全有或全无——一次限流丢弃整轮结果、状态落到终态 Degraded，
    /// 于是语义索引永远停在空表上，而检索照常返回结果，失败完全不可见。
    #[test]
    fn rate_limited_embedding_preserves_vectors_and_resumes() {
        let mut responses = vec![crate::testing::http_json(
            200,
            r#"{"data":[{"index":0,"embedding":[1,0,0,0]}]}"#,
        )];
        responses.extend(
            (0..MAX_SEMANTIC_STALLED_ROUNDS + 2)
                .map(|_| crate::testing::http_json(429, r#"{"message":"TPM limit reached"}"#)),
        );
        let host = crate::testing::RepoHost::new(SEMANTIC_RATE_LIMIT_FIXTURE)
            .with_http_responses(responses)
            .with_echo_embeddings(4);
        let mut plugin = CodeContextPlugin::default();
        let mut context = api_activation_context("/repo/a");
        context
            .metadata
            .insert("embedding_batch_size".into(), "1".into());
        plugin.activate(&host, context).expect("激活插件");

        // 首批成功、第二批限流，先证明成功前缀已经写入而不是整轮丢弃。
        let mut hit_rate_limit = false;
        for _ in 0..8 {
            plugin.advance_index(&host);
            if plugin
                .degraded_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("等待限流恢复"))
            {
                hit_rate_limit = true;
                assert_eq!(
                    plugin.index_status(),
                    IndexStatus::Indexing,
                    "限流应保持可续状态，而不是终态降级"
                );
                assert_eq!(plugin.semantic_len(), 1, "限流前的成功向量必须保留");
                break;
            }
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
        }
        assert!(hit_rate_limit, "测试应至少触发一次限流续建路径");

        // 连续超过普通瞬时故障的停滞阈值，429 仍不得转成终态 Degraded。
        for _ in 0..=MAX_SEMANTIC_STALLED_ROUNDS {
            plugin.advance_index(&host);
            assert_eq!(
                plugin.index_status(),
                IndexStatus::Indexing,
                "持续 429 不得经过停滞计数永久降级"
            );
            assert_eq!(plugin.semantic_len(), 1, "重试期间不得丢失已有向量");
        }

        // 预置 429 耗尽后，测试宿主恢复为正常嵌入响应。
        for _ in 0..8 {
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
            plugin.advance_index(&host);
        }
        assert_eq!(
            plugin.index_status(),
            IndexStatus::Ready,
            "端点恢复后必须把语义索引补齐，实际降级原因：{:?}",
            plugin.degraded_reason
        );
        assert!(plugin.semantic_len() > 1, "恢复后必须补齐剩余向量");
    }

    /// 嵌入端点持续失败时必须降级为两路检索，而不是让索引构建失败或永远停在建立中。
    ///
    /// 5xx 属于瞬时错误，会先重试若干轮；连续 [`MAX_SEMANTIC_STALLED_ROUNDS`] 轮没有
    /// 任何新向量产出后才落到终态降级。
    #[test]
    fn embedding_failure_degrades_instead_of_failing_the_build() {
        let responses: Vec<_> = (0..16)
            .map(|_| crate::testing::http_json(500, "upstream error"))
            .collect();
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_http_responses(responses);
        let mut plugin = indexed(&host, api_activation_context("/repo/a"));
        for _ in 0..=MAX_SEMANTIC_STALLED_ROUNDS {
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
            plugin.advance_index(&host);
        }

        assert_eq!(plugin.index_status(), IndexStatus::Degraded);
        assert!(
            plugin
                .degraded_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("语义索引不可用")),
            "必须说明降级原因，实际：{:?}",
            plugin.degraded_reason
        );
        // 两路检索仍然可用。
        assert!(!plugin.search_candidates("permission", None, 10).is_empty());
    }

    /// 未配置嵌入端点时必须走两路检索，且不发起任何 HTTP 调用。
    #[test]
    fn without_endpoint_no_http_call_is_made() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE);
        let plugin = indexed(&host, activation_context("/repo/a"));

        assert_eq!(plugin.index_status(), IndexStatus::Degraded);
        assert!(
            host.http_requests().is_empty(),
            "没有端点时不应发起嵌入请求"
        );
        assert!(!plugin.search_candidates("permission", None, 10).is_empty());
    }

    /// 语义索引就绪后，检索必须真的嵌入查询并返回融合结果。
    #[test]
    fn search_embeds_query_and_returns_fused_results() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_echo_embeddings(4);
        let mut plugin = indexed(&host, api_activation_context("/repo/a"));
        assert_eq!(plugin.index_status(), IndexStatus::Ready);

        let results = plugin.search_results(&host, "permission check", None, 10);

        assert!(!results.is_empty(), "混合检索应返回结果");
        assert_eq!(
            host.http_requests().len(),
            2,
            "建索引一次、查询一次，共两次端点调用"
        );
    }

    /// 精确标识符必须走 Fast Path，不得产生查询嵌入或模型重排请求。
    #[test]
    fn exact_identifier_skips_semantic_and_rerank_requests() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_echo_embeddings(4);
        let mut context = api_activation_context("/repo/a");
        context
            .metadata
            .insert("rerank_model".into(), "test-reranker".into());
        let mut plugin = indexed(&host, context);
        let requests_after_build = host.http_requests().len();

        let execution = plugin.search_execution(&host, "check_permission", None, 10);

        assert_eq!(
            execution.results[0].symbol_name.as_deref(),
            Some("check_permission")
        );
        assert_eq!(execution.diagnostics.path, SearchPath::Fast);
        assert!(!execution.diagnostics.rerank_activated);
        assert_eq!(
            host.http_requests().len(),
            requests_after_build,
            "精确标识符不得调用 embedding 或 rerank 端点"
        );
    }

    /// 混合查询已有精确符号与目标实现时，应走 Normal Path 并跳过模型重排。
    #[test]
    fn high_confidence_mixed_query_skips_rerank() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE).with_echo_embeddings(4);
        let mut context = api_activation_context("/repo/a");
        context
            .metadata
            .insert("rerank_model".into(), "test-reranker".into());
        let mut plugin = indexed(&host, context);

        let execution = plugin.search_execution(&host, "check_permission 怎么实现", None, 10);

        assert_eq!(execution.diagnostics.path, SearchPath::Normal);
        assert_eq!(
            execution.diagnostics.confidence.level,
            ConfidenceLevel::High
        );
        assert!(!execution.diagnostics.rerank_activated);
        assert!(host
            .http_requests()
            .iter()
            .all(|request| request.path == "embeddings"));
    }

    /// 语义索引未就绪时不得为查询发起嵌入请求——没有可比对的向量，纯属浪费配额。
    #[test]
    fn query_is_not_embedded_when_semantic_index_is_unready() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE);
        let mut plugin = indexed(&host, activation_context("/repo/a"));

        let results = plugin.search_results(&host, "permission check", None, 10);

        assert!(!results.is_empty(), "两路检索仍应有结果");
        assert!(host.http_requests().is_empty(), "不应为查询发起嵌入");
    }

    /// 模型重排失败时必须完整保留本地融合结果，并记录本次查询的降级原因。
    #[test]
    fn rerank_failure_keeps_local_results() {
        let fixture = &[(
            "src/permission.rs",
            "/// 检查权限。\npub fn check_permission() {}\n\n/// 描述权限。\npub fn describe_permission() {}\n",
        )];
        let host = crate::testing::RepoHost::new(fixture)
            .with_http_responses(vec![crate::testing::http_json(503, "rerank unavailable")]);
        let mut context = activation_context("/repo/a");
        context
            .metadata
            .insert("embedding_mode".into(), "local".into());
        context
            .metadata
            .insert("rerank_model".into(), "test-reranker".into());
        let mut plugin = indexed(&host, context);
        plugin.config.rerank_model = None;
        let expected = plugin.search_results(&host, "permission behavior", None, 10);
        plugin.config.rerank_model = Some("test-reranker".into());

        let actual = plugin.search_execution(&host, "permission behavior", None, 10);

        assert_eq!(actual.results, expected, "失败降级不得改变原有融合顺序");
        assert_eq!(actual.diagnostics.path, SearchPath::Hard);
        assert!(actual.diagnostics.rerank_activated);
        assert!(!actual.diagnostics.rerank_applied);
        assert!(plugin
            .last_rerank_degraded_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("HTTP 503")));
        let requests = host.http_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "rerank");
    }

    /// 卸载后必须释放仓库上下文并回到未索引状态。
    #[test]
    fn deactivate_releases_repository() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        plugin.deactivate(&host).expect("卸载插件");

        assert!(plugin.repository_identity().is_none());
        assert_eq!(plugin.index_status(), IndexStatus::NotIndexed);
        assert!(host.registered_tools().is_empty());
    }
}
