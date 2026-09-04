//! Code Context Engine 插件。
//!
//! 为 Agent 提供 `code_search`：按自然语言或标识符定位仓库中的代码，替代多轮
//! `grep` 与读文件。设计依据见 `docs/code-context-engine/`。
//!
//! # 当前阶段
//!
//! 已完成仓库扫描、结构与检索索引、上下文构建、语义向量索引（含持久化），以及按写入
//! 事件触发的文件级增量重建。`code_search` 返回经 RRF 融合、去重、父子抑制与 token
//! 预算打包后的真实检索结果。
//!
//! # 能力边界
//!
//! 本插件只申请 `workspace_read` 与 `storage`。工作区读取由 Host 注入可信根并限制为只读，
//! 权限下限仍为 `read`。插件刻意不申请进程、网络与会话能力——那些会抬高工具权限，
//! 使 `code_search` 每次调用都需要额外审批。详见 `plugin.toml` 中逐项说明。
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
mod retrieval;
mod semantic;
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
    tokenize, CandidateSource, FusionConfig, FusionDebug, RetrievalIndex, RetrievalStats,
    SearchCandidate, SearchResult, SymbolMatch,
};
pub use semantic::SemanticIndex;
pub use storage::{IndexStore, ManifestLoad};
pub use tool::{CodeSearchRequest, TOOL_NAME};

/// 构造送去嵌入的文档文本。
///
/// 拼接限定名、签名与文档注释，而不是直接用源码正文：正文不驻留内存（见 adr-storage.md），
/// 且嵌入模型对"这段代码是干什么的"最敏感的信号恰恰在签名与文档注释里。缺失的字段直接
/// 跳过，不填占位符——占位符会成为所有同类 chunk 的共同噪声。
fn semantic_document_text(chunk: &CodeChunk) -> String {
    let mut parts = Vec::new();
    parts.push(chunk.file_path.clone());
    if let Some(qualified) = chunk.qualified_symbol.as_deref() {
        parts.push(qualified.to_string());
    }
    if let Some(signature) = chunk.signature.as_deref() {
        parts.push(signature.to_string());
    }
    if let Some(doc) = chunk.doc_comment.as_deref() {
        parts.push(doc.to_string());
    }
    parts.join("\n")
}

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
    /// 同样内存驻留：向量体量远超宿主存储上限，落盘要等 blob 能力（见 adr-storage.md）。
    /// 未配置嵌入 provider 时保持为空，检索自动退化为词法与符号两路。
    semantic: SemanticIndex,
    /// 已确认成功写入、等待下一次增量构建的仓库相对路径。
    dirty_paths: BTreeSet<String>,
    /// 尚未收到终态结果的写入调用，键为运行 ID 与调用 ID 的组合。
    pending_writes: BTreeMap<String, incremental::PendingWrite>,
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
    /// Host 返回的 `code_search` 公开名称；存在时表示工具已经注册给 Agent。
    registered_tool_name: Option<String>,
}

impl CodeContextPlugin {
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

        // 尝试从持久存储恢复语义向量。需要当前 provider 的嵌入空间来校验兼容性：
        // 快照的 space_id 与当前不一致时直接丢弃，避免混用不同模型的向量。
        let mut semantic = SemanticIndex::new();
        if let Some(space) = self.embedding_provider(host).space() {
            if let Some(snapshot) = store.load_semantic(host) {
                semantic.restore(&space, &snapshot);
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

        // 语义向量在结构索引就绪后建立。失败不影响已就绪的两路检索，只是把状态留在
        // Degraded——这与架构第 13 问对降级形态的定义一致，不是错误。
        let semantic_ready = self.build_semantic_index(host);
        self.index_status = if semantic_ready {
            IndexStatus::Ready
        } else {
            IndexStatus::Degraded
        };
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
        let mut results = self.search_results(host, &request.query, single_scope, limit);
        if request.path_scope.len() > 1 {
            results.retain(|result| {
                request
                    .path_scope
                    .iter()
                    .any(|scope| result.file_path.starts_with(scope.as_str()))
            });
        }

        let bundle = {
            let idx = self
                .repository
                .as_ref()
                .and_then(|ctx| ctx.index.as_ref());
            match idx {
                Some(index) => context::build(
                    host,
                    index,
                    results,
                    &context::ContextConfig {
                        token_budget: request.token_budget as u32,
                        max_results: limit,
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
        let degraded = self.degraded_reason.clone();
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
        let query_vector = self.embed_query(host, query);
        let Some(context) = self.repository.as_ref() else {
            return Vec::new();
        };
        let (Some(index), Some(retrieval)) = (context.index.as_ref(), context.retrieval.as_ref())
        else {
            return Vec::new();
        };
        retrieval.search_hybrid(
            index,
            &context.semantic,
            query,
            query_vector.as_deref(),
            path_scope,
            limit,
        )
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
        match provider.embed_query(query) {
            Ok(vector) => Some(vector),
            Err(error) => {
                self.degraded_reason = Some(format!("查询嵌入失败，已降级为两路检索：{error}"));
                None
            }
        }
    }

    /// 为已建成的结构索引计算语义向量。
    ///
    /// 返回语义索引是否可用于检索。嵌入不可用时返回 `false` 并记录降级原因，
    /// **不返回错误**：两路检索已经就绪，语义缺席只降低召回质量，不该让整次构建失败。
    ///
    /// 嵌入空间发生变化时先整体重置——旧向量与新查询向量不可比较，混用会让检索结果
    /// 长期缓慢地不正确。
    fn build_semantic_index(&mut self, host: &dyn PluginHostApi) -> bool {
        let provider = embedding::select_provider(&self.config, host);
        let Some(space) = provider.space() else {
            // 未配置嵌入 provider 是正常状态，不构成降级原因。
            return false;
        };
        let Some(context) = self.repository.as_mut() else {
            return false;
        };
        let Some(index) = context.index.as_ref() else {
            return false;
        };
        if !context.semantic.is_compatible_with(&space) {
            context.semantic.reset(space);
        } else if context.semantic.space().is_none() {
            context.semantic.reset(space);
        }

        // 只嵌入尚无向量的 chunk：全量重嵌在 API 模式下等于重新付费。
        let pending: Vec<(String, String, String)> = index
            .chunks()
            .filter(|chunk| !context.semantic.contains(&chunk.id))
            .map(|chunk| {
                (
                    chunk.id.clone(),
                    chunk.file_path.clone(),
                    semantic_document_text(chunk),
                )
            })
            .collect();
        if pending.is_empty() {
            return context.semantic.is_ready();
        }

        let inputs: Vec<String> = pending.iter().map(|(_, _, text)| text.clone()).collect();
        match provider.embed_documents(&inputs) {
            Ok(vectors) => {
                for ((chunk_id, file_path, _), vector) in pending.iter().zip(vectors.iter()) {
                    context.semantic.upsert(chunk_id, file_path, vector);
                }
                // 向量落盘：失败不影响本次检索，只让下次启动需要重新嵌入。
                if let Some(snapshot) = context.semantic.snapshot() {
                    if let Err(error) = context.store.save_semantic(host, &snapshot) {
                        self.degraded_reason =
                            Some(format!("语义向量未能持久化：{error}"));
                        return context.semantic.is_ready();
                    }
                }
                self.degraded_reason = None;
                context.semantic.is_ready()
            }
            Err(error) => {
                self.degraded_reason = Some(format!("语义索引不可用：{error}"));
                false
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

    /// 构造启用 API 嵌入的激活上下文。
    fn api_activation_context(workspace_root: &str) -> ActivationContext {
        let mut context = activation_context(workspace_root);
        context
            .metadata
            .insert("embedding_dimension".into(), "4".into());
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

    /// 嵌入端点失败时必须降级为两路检索，而不是让索引构建失败。
    #[test]
    fn embedding_failure_degrades_instead_of_failing_the_build() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE)
            .with_http_responses(vec![crate::testing::http_json(500, "upstream error")]);
        let plugin = indexed(&host, api_activation_context("/repo/a"));

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
        assert!(host.http_requests().is_empty(), "没有端点时不应发起嵌入请求");
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

    /// 语义索引未就绪时不得为查询发起嵌入请求——没有可比对的向量，纯属浪费配额。
    #[test]
    fn query_is_not_embedded_when_semantic_index_is_unready() {
        let host = crate::testing::RepoHost::new(SEMANTIC_FIXTURE);
        let mut plugin = indexed(&host, activation_context("/repo/a"));

        let results = plugin.search_results(&host, "permission check", None, 10);

        assert!(!results.is_empty(), "两路检索仍应有结果");
        assert!(host.http_requests().is_empty(), "不应为查询发起嵌入");
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
