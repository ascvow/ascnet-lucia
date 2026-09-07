//! 引擎：CLI 与 MCP 共用的唯一核心。
//!
//! 两条对外通路都只调用本模块，检索逻辑不在 CLI 或 MCP 中重复实现。
//!
//! # 进程锁
//!
//! 同一 workspace 同时只允许一个写者进程修改目录状态与向量检查点。
//! [`AccessMode::Auto`] 在写锁被占用时自动降级为只读，并在结果中给出明确说明；
//! [`AccessMode::ReadWrite`] 则直接返回 [`Error::WorkspaceBusy`]。
//!
//! # 新鲜度
//!
//! 第一版只公开 `best_effort`：返回最近一次采集的状态、待嵌入数量与降级原因。
//! "索引中的当前版本" 不等于 "查询这一瞬间磁盘上的绝对最新版本"。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{Config, ProviderKind, DEFAULT_CONFIG_FILE};
use crate::context::{self, AssembleInput};
use crate::error::{Error, Result};
use crate::indexer::{self, lock_index, lock_store};
use crate::models::{self, EmbeddingSpace, Provider};
use crate::retrieval::{self, RetrievalInput};
use crate::store::Store;
use crate::types::{
    EmbeddingSpaceInfo, IndexReport, QueryPlanInfo, ReadRequest, ReadResponse, ScanInfo,
    SearchRequest, SearchResponse, SearchResult, SearchStatus, StatusReport,
};
use crate::vector::VectorIndex;
use crate::workspace::lock::WorkspaceLock;
use crate::workspace::Workspace;

/// 数据库文件名。
const DB_FILE_NAME: &str = "index.db";
/// 向量检查点子目录。
const VECTOR_DIR_NAME: &str = "vectors";
/// 新鲜度语义标识。
const FRESHNESS_BEST_EFFORT: &str = "best_effort";

/// 访问模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// 必须取得写锁，否则返回 [`Error::WorkspaceBusy`]。
    ReadWrite,
    /// 不取写锁，也不修改任何状态。
    ReadOnly,
    /// 优先写模式；写锁被占用时降级为只读并记录说明。
    Auto,
}

/// 引擎启动参数。
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// workspace 根目录。
    pub root: PathBuf,
    /// 配置文件路径；为空时使用 `<root>/codesearch.toml`。
    pub config_path: Option<PathBuf>,
    /// 索引数据目录；为空时使用 `<root>/.codesearch`。
    pub data_dir: Option<PathBuf>,
    /// 离线模式：禁止任何模型网络调用。
    pub offline: bool,
    /// 访问模式。
    pub mode: AccessMode,
}

impl EngineOptions {
    /// 以默认参数指向某个根目录。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            config_path: None,
            data_dir: None,
            offline: false,
            mode: AccessMode::Auto,
        }
    }
}

/// 引擎。
pub struct Engine {
    workspace: Workspace,
    config: Config,
    store: Mutex<Store>,
    vector: Mutex<Option<VectorIndex>>,
    provider: Option<Arc<Provider>>,
    space: Option<EmbeddingSpace>,
    /// 持有期间独占该 workspace 的写权限；`None` 表示只读会话。
    _lock: Option<WorkspaceLock>,
    writable: bool,
    offline: bool,
    notes: Vec<String>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("root", &self.workspace.root())
            .field("writable", &self.writable)
            .field("offline", &self.offline)
            .field("space", &self.space.as_ref().map(|s| &s.id))
            .finish()
    }
}

impl Engine {
    /// 打开引擎：解析配置、取锁、打开存储、连接 Provider 并加载向量索引。
    pub async fn open(options: EngineOptions) -> Result<Self> {
        let workspace = Workspace::open(&options.root, options.data_dir.as_deref())?;
        let config_path = options
            .config_path
            .clone()
            .unwrap_or_else(|| workspace.root().join(DEFAULT_CONFIG_FILE));
        let config = Config::load(&config_path)?;
        Self::open_with_config(options, workspace, config).await
    }

    /// 使用调用方提供的配置打开引擎（测试与嵌入式调用使用）。
    pub async fn open_with_config(
        options: EngineOptions,
        workspace: Workspace,
        config: Config,
    ) -> Result<Self> {
        let mut notes = Vec::new();

        // ---- 取锁并决定读写能力 ----
        let (lock, writable) = match options.mode {
            AccessMode::ReadOnly => (None, false),
            AccessMode::ReadWrite => {
                workspace.ensure_data_dir()?;
                (
                    Some(WorkspaceLock::try_exclusive(workspace.data_dir())?),
                    true,
                )
            }
            AccessMode::Auto => {
                workspace.ensure_data_dir()?;
                match WorkspaceLock::try_exclusive(workspace.data_dir()) {
                    Ok(lock) => (Some(lock), true),
                    Err(Error::WorkspaceBusy(_)) => {
                        notes.push(
                            "workspace 写锁被其他进程持有，本次以只读模式运行，不会刷新索引"
                                .to_string(),
                        );
                        (None, false)
                    }
                    Err(error) => return Err(error),
                }
            }
        };

        // ---- 打开存储 ----
        let db_path = workspace.data_dir().join(DB_FILE_NAME);
        if !writable && !db_path.exists() {
            return Err(Error::IndexMissing(format!(
                "{} 不存在；请先运行 `codesearch index --root {}`",
                db_path.display(),
                workspace.root().display()
            )));
        }
        let store = Store::open(&db_path, !writable)?;
        if writable {
            store.bind_workspace(workspace.id())?;
        }

        // ---- 连接 Embedding Provider ----
        let mut provider = None;
        let mut space = None;
        if options.offline {
            if config.embedding.provider != ProviderKind::None {
                notes.push("offline 模式已启用，不会请求 Embedding 服务".to_string());
            }
        } else {
            match models::build_provider(&config, false, None).await {
                Ok(Some(built)) => {
                    let built_space = built.space().clone();
                    space = Some(built_space);
                    provider = Some(Arc::new(built));
                }
                Ok(None) => {}
                Err(error) => {
                    // Provider 不可用不应阻止符号与全文检索。
                    notes.push(format!("Embedding Provider 不可用，已降级: {error}"));
                }
            }
        }

        // ---- 加载向量索引 ----
        let mut vector = None;
        if let Some(space) = &space {
            if writable {
                store.ensure_embedding_space(&space.to_row())?;
            }
            let vector_dir = workspace.data_dir().join(VECTOR_DIR_NAME);
            match VectorIndex::open(&vector_dir, &store, &space.id, space.dimensions) {
                Ok(index) => vector = Some(index),
                Err(error) => {
                    notes.push(format!("向量索引不可用，已降级为全文检索: {error}"));
                }
            }
        }

        let mut engine = Self {
            workspace,
            config,
            store: Mutex::new(store),
            vector: Mutex::new(vector),
            provider,
            space,
            _lock: lock,
            writable,
            offline: options.offline,
            notes,
        };

        // 模型空间变化时，把旧空间的向量标记为待重嵌并登记删除，避免混用。
        if engine.writable {
            if let Some(space) = engine.space.clone() {
                let mut guard = lock_store(&engine.store)?;
                let migrated = guard.migrate_to_space(&space.id)?;
                if migrated > 0 {
                    engine.notes.push(format!(
                        "Embedding 空间已变更，{migrated} 个代码块需要重新生成向量"
                    ));
                }
            }
        }
        Ok(engine)
    }

    /// workspace 根目录。
    pub fn root(&self) -> &Path {
        self.workspace.root()
    }

    /// 当前配置。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 当前会话是否可写。
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// 是否存在可用的向量通路。
    pub fn has_vector_path(&self) -> bool {
        self.provider.is_some()
            && self
                .vector
                .lock()
                .map(|guard| guard.is_some())
                .unwrap_or(false)
    }

    fn require_writable(&self, action: &str) -> Result<()> {
        if self.writable {
            return Ok(());
        }
        Err(Error::ReadOnly(format!(
            "{action} 需要写权限，但 workspace 写锁不可用"
        )))
    }

    /// 执行一次完整索引。
    ///
    /// `force` 为真时忽略大小/修改时间快速路径，对每个文件重新计算内容 Hash。
    /// 返回值分别给出目录索引与语义索引的完成情况，两者不会混为一谈。
    pub async fn index(&self, force: bool) -> Result<IndexReport> {
        self.require_writable("索引")?;
        let catalog = {
            let mut guard = lock_store(&self.store)?;
            let stats = indexer::refresh_catalog(&mut guard, &self.workspace, &self.config, force)?;
            let info = ScanInfo {
                finished_at: crate::store::now_secs(),
                scanned: stats.scanned,
                changed: stats.changed,
                removed: stats.removed,
                skipped: stats.skipped.len() as i64,
                duration_ms: stats.duration_ms,
            };
            guard.set_last_scan(&serde_json::to_string(&info)?)?;
            stats
        };

        let semantic = self.run_embedding_stage().await?;

        let (counters, epoch) = {
            let guard = lock_store(&self.store)?;
            (guard.counters()?, guard.catalog_epoch()?)
        };
        Ok(IndexReport {
            catalog_epoch: epoch,
            scanned_files: catalog.scanned,
            changed_files: catalog.changed,
            unchanged_files: catalog.unchanged,
            removed_files: catalog.removed,
            skipped: catalog.skipped,
            current_chunks: counters.current_chunks,
            catalog_duration_ms: catalog.duration_ms,
            embeddings_created: semantic.created,
            embeddings_cached: semantic.cached,
            embeddings_pending: counters.embeddings_pending,
            embeddings_failed: counters.embeddings_failed,
            semantic_skipped: self.provider.is_none(),
            semantic_note: self.semantic_note(),
            degraded: self.provider.is_none() || counters.embeddings_pending > 0,
        })
    }

    /// 说明语义阶段为何未执行或未完成。
    fn semantic_note(&self) -> Option<String> {
        if self.offline {
            return Some("offline 模式：未请求模型，语义索引未更新".to_string());
        }
        if self.config.embedding.provider == ProviderKind::None {
            return Some(
                "未配置 Embedding Provider：仅建立符号与全文索引，语义检索不可用".to_string(),
            );
        }
        if self.provider.is_none() {
            return Some("Embedding Provider 连接失败：语义索引未更新".to_string());
        }
        None
    }

    /// 运行 Embedding 阶段并发布向量检查点。
    async fn run_embedding_stage(&self) -> Result<indexer::EmbedStats> {
        let Some(provider) = self.provider.clone() else {
            return Ok(indexer::EmbedStats::default());
        };
        let mut vector_slot = {
            let mut guard = lock_index(&self.vector)?;
            guard.take()
        };
        let Some(index) = vector_slot.take() else {
            return Ok(indexer::EmbedStats::default());
        };

        // 独占取出索引，避免在 await 期间持有锁。
        let index_cell = Mutex::new(index);
        let result =
            indexer::embed_pending(&self.store, &index_cell, &provider, &self.config).await;

        let mut index = index_cell
            .into_inner()
            .map_err(|_| Error::Internal("向量索引锁已中毒".to_string()))?;
        // 无论本轮是否出错，都尝试发布检查点，把已应用的部分持久化。
        if let Err(error) = index.checkpoint() {
            tracing::warn!(error = %error, "发布向量检查点失败");
        }
        {
            let mut guard = lock_index(&self.vector)?;
            *guard = Some(index);
        }
        result
    }

    /// 确保指定路径已经重新采集并提交目录状态。
    ///
    /// 只保证**目录**状态是最新的；语义索引是否追上需要通过 [`Self::status`] 观察。
    /// 这不是 `wait_for_revision`，不会等待向量生成完成。
    pub async fn ensure_indexed(&self, paths: &[String]) -> Result<()> {
        self.require_writable("ensure_indexed")?;
        if paths.is_empty() {
            return Ok(());
        }
        {
            let mut guard = lock_store(&self.store)?;
            indexer::refresh_paths(&mut guard, &self.workspace, &self.config, paths)?;
        }
        self.run_embedding_stage().await?;
        Ok(())
    }

    /// 执行搜索。
    pub async fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        let query = request.query.trim();
        if query.is_empty() {
            return Err(Error::InvalidQuery("查询不能为空".to_string()));
        }
        let mut notes = self.notes.clone();

        // 写模式下先做一次增量扫描，让刚保存的文件能被搜到。
        if self.writable && self.config.index.refresh_on_search {
            let mut guard = lock_store(&self.store)?;
            if let Err(error) =
                indexer::refresh_catalog(&mut guard, &self.workspace, &self.config, false)
            {
                notes.push(format!("查询前增量扫描失败: {error}"));
            }
        }

        let plan = retrieval::analyze(query, &request.scope)?;

        // 查询向量在存储锁之外生成，不阻塞其他查询。
        let mut query_vector: Option<Vec<f32>> = None;
        if let Some(provider) = &self.provider {
            match provider.embed_query(&plan.text).await {
                Ok(mut vector) => match models::normalize_l2(&mut vector) {
                    Ok(()) => query_vector = Some(vector),
                    Err(error) => notes.push(format!("查询向量归一化失败: {error}")),
                },
                Err(error) => notes.push(format!("查询向量生成失败: {error}")),
            }
        }

        let max_results = request
            .max_results
            .unwrap_or(self.config.retrieval.max_results)
            .clamp(1, self.config.retrieval.max_results_cap);
        let token_budget = request
            .token_budget
            .unwrap_or(self.config.retrieval.token_budget)
            .clamp(1, self.config.retrieval.token_budget_cap);

        let (results, truncated, total_tokens, counters, epoch, vector_used, mut recall_notes) = {
            let store = lock_store(&self.store)?;
            let vector_guard = lock_index(&self.vector)?;
            let input = RetrievalInput {
                plan: &plan,
                query_vector: query_vector.as_deref(),
                space_id: self.space.as_ref().map(|space| space.id.as_str()),
            };
            let outcome = retrieval::retrieve(
                &store,
                vector_guard.as_ref(),
                &self.config.retrieval,
                &input,
            )?;
            let assembled = context::assemble(
                &store,
                &AssembleInput {
                    candidates: &outcome.candidates,
                    plan: &plan,
                    max_results,
                    token_budget,
                    config: &self.config.retrieval,
                },
            )?;
            (
                assembled.results,
                assembled.truncated,
                assembled.total_estimated_tokens,
                store.counters()?,
                store.catalog_epoch()?,
                outcome.vector_used,
                outcome.notes,
            )
        };
        notes.append(&mut recall_notes);

        if let Some(note) = self.semantic_note() {
            notes.push(note);
        }
        if counters.embeddings_pending > 0 {
            notes.push(format!(
                "还有 {} 个代码块等待语义索引，本次结果可能不完整",
                counters.embeddings_pending
            ));
        }
        if counters.embeddings_failed > 0 {
            notes.push(format!(
                "{} 个代码块的 Embedding 已失败，不再重试",
                counters.embeddings_failed
            ));
        }
        notes.dedup();

        let degraded = !vector_used || counters.embeddings_pending > 0;
        // degraded 必须带着原因出现。作用域筛掉全部候选时向量通路不会执行，
        // 前面几路都没有话可说，只报一个孤零零的 degraded=true 会让调用方
        // 以为工具坏了，而不是自己的过滤条件没匹配上任何文件。
        if degraded && notes.is_empty() {
            notes.push(if results.is_empty() {
                "作用域未匹配到任何代码块，向量召回未执行".to_string()
            } else {
                "本次未使用向量召回，仅符号与全文结果".to_string()
            });
        }
        Ok(SearchResponse {
            catalog_epoch: epoch,
            mode: request.mode,
            status: SearchStatus {
                degraded,
                reasons: notes,
                vector_search_used: vector_used,
                pending_embeddings: counters.embeddings_pending,
                failed_embeddings: counters.embeddings_failed,
                freshness: FRESHNESS_BEST_EFFORT.to_string(),
            },
            results,
            total_estimated_tokens: total_tokens,
            token_budget,
            estimated: true,
            truncated,
            query_plan: plan_info(&plan),
        })
    }

    /// 按引用读取源码。
    pub fn read(&self, request: &ReadRequest) -> Result<ReadResponse> {
        let store = lock_store(&self.store)?;
        context::read(&store, request, self.config.retrieval.token_budget)
    }

    /// 汇报索引状态。
    pub fn status(&self) -> Result<StatusReport> {
        let store = lock_store(&self.store)?;
        let counters = store.counters()?;
        let vector_guard = lock_index(&self.vector)?;
        let mut diagnostics = self.notes.clone();

        let (applied, durable, count, backlog) = match vector_guard.as_ref() {
            Some(index) => {
                let space_id = index.space_id().to_string();
                let backlog = store.vector_ops_backlog(&space_id, index.applied_seq())?;
                (
                    index.applied_seq(),
                    index.durable_seq(),
                    index.len() as i64,
                    backlog,
                )
            }
            None => (0, 0, 0, 0),
        };
        if let Some(note) = self.semantic_note() {
            diagnostics.push(note);
        }
        if applied != durable {
            diagnostics.push(format!(
                "向量内存已应用到 {applied}，但持久化检查点只到 {durable}；崩溃后将重放差额"
            ));
        }
        let last_scan = match store.last_scan()? {
            Some(json) => serde_json::from_str::<ScanInfo>(&json).ok(),
            None => None,
        };
        let degraded = self.provider.is_none()
            || vector_guard.is_none()
            || counters.embeddings_pending > 0
            || counters.embeddings_failed > 0;
        diagnostics.dedup();

        Ok(StatusReport {
            workspace_id: self.workspace.id().to_string(),
            root: self.workspace.root().display().to_string(),
            data_dir: self.workspace.data_dir().display().to_string(),
            schema_version: store.schema_version()?,
            catalog_epoch: store.catalog_epoch()?,
            active_files: counters.active_files,
            skipped_files: counters.skipped_files,
            current_chunks: counters.current_chunks,
            embeddings_ready: counters.embeddings_ready,
            embeddings_pending: counters.embeddings_pending,
            embeddings_failed: counters.embeddings_failed,
            cache_entries: counters.cache_entries,
            embedding_space: self.space.as_ref().map(|space| EmbeddingSpaceInfo {
                id: space.id.clone(),
                provider: space.provider.clone(),
                model: space.model.clone(),
                cache_namespace: space.cache_namespace.clone(),
                dimensions: space.dimensions,
                metric: space.metric.clone(),
                normalize: space.normalize.clone(),
                template_version: space.template_version,
            }),
            last_scan,
            vector_applied_seq: applied,
            vector_durable_seq: durable,
            vector_ops_backlog: backlog,
            vector_count: count,
            degraded,
            diagnostics,
        })
    }

    /// 启动文件监听并持续增量索引，直到收到取消信号。
    ///
    /// 先建立监听再做首次全量扫描，避免交接期间漏事件。
    pub async fn watch_loop(&self, cancel: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        self.require_writable("watch")?;
        let mut handle = indexer::watch::start(&self.workspace, &self.config)?;
        // 监听已生效后再做首次扫描。
        self.index(false).await?;

        let mut cancel = cancel;
        let verify_interval = self.config.index.verify_interval_secs;
        let mut verify_timer =
            tokio::time::interval(std::time::Duration::from_secs(if verify_interval == 0 {
                3600
            } else {
                verify_interval
            }));
        verify_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        verify_timer.tick().await;

        loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        break;
                    }
                }
                batch = handle.batches.recv() => {
                    let Some(batch) = batch else { break };
                    if let Err(error) = self.apply_watch_batch(batch).await {
                        tracing::error!(error = %error, "处理文件变更失败");
                    }
                }
                _ = verify_timer.tick(), if verify_interval > 0 => {
                    // 低频校验扫描：重新计算全部内容 Hash，纠正监听可能漏掉的变更。
                    if let Err(error) = self.index(true).await {
                        tracing::error!(error = %error, "校验扫描失败");
                    }
                }
            }
        }
        Ok(())
    }

    async fn apply_watch_batch(&self, batch: indexer::watch::WatchBatch) -> Result<()> {
        match batch {
            indexer::watch::WatchBatch::FullRescan => {
                tracing::info!("监听报告事件丢失或变更过多，执行整目录重扫");
                self.index(false).await?;
            }
            indexer::watch::WatchBatch::Paths(paths) => {
                tracing::debug!(count = paths.len(), "处理文件变更");
                {
                    let mut guard = lock_store(&self.store)?;
                    indexer::refresh_paths(&mut guard, &self.workspace, &self.config, &paths)?;
                }
                self.run_embedding_stage().await?;
            }
        }
        Ok(())
    }
}

/// 把内部查询计划转换为对外可诊断的摘要。
fn plan_info(plan: &retrieval::QueryPlan) -> QueryPlanInfo {
    QueryPlanInfo {
        kinds: plan.kinds.iter().map(|k| k.as_str().to_string()).collect(),
        symbols: plan.symbols.clone(),
        paths: plan.paths.clone(),
        literals: plan.literals.clone(),
        terms: plan.terms.clone(),
        intents: plan
            .intents
            .iter()
            .map(|i| i.as_str().to_string())
            .collect(),
        scope: plan.scope.clone(),
    }
}

/// 渲染搜索结果为人类可读文本。
///
/// 与 JSON 输出共享同一份数据，降级原因同样会显示，不会只写进日志。
pub fn render_results(response: &SearchResponse) -> String {
    let mut out = String::new();
    if response.results.is_empty() {
        out.push_str("没有找到匹配的代码。\n");
    }
    for (index, result) in response.results.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}:{}-{}",
            index + 1,
            result.path,
            result.start_line,
            result.end_line
        ));
        if let Some(symbol) = &result.symbol {
            out.push_str(&format!("  [{symbol}]"));
        }
        out.push_str(&format!(
            "\n   kind={} score={:.4} matched_by={} ref={}\n",
            result.kind.as_str(),
            result.score,
            result
                .matched_by
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join("+"),
            result.code_ref
        ));
        for line in result.content.lines() {
            out.push_str("   | ");
            out.push_str(line);
            out.push('\n');
        }
        if result.truncated {
            out.push_str("   | ...（已按 token 预算截断）\n");
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "catalog_epoch={} mode={} 结果={} 估算 token={}/{}（estimated）\n",
        response.catalog_epoch,
        response.mode.as_str(),
        response.results.len(),
        response.total_estimated_tokens,
        response.token_budget
    ));
    out.push_str(&format!(
        "状态: degraded={} vector_search_used={} pending={} failed={}\n",
        response.status.degraded,
        response.status.vector_search_used,
        response.status.pending_embeddings,
        response.status.failed_embeddings
    ));
    for reason in &response.status.reasons {
        out.push_str(&format!("  - {reason}\n"));
    }
    out
}

/// 渲染单条读取结果为人类可读文本。
pub fn render_read(response: &ReadResponse) -> String {
    let mut out = format!(
        "{}:{}-{}  ref={}  file_hash={}  current={}\n",
        response.path,
        response.start_line,
        response.end_line,
        response.code_ref,
        &response.file_hash[..response.file_hash.len().min(16)],
        response.is_current
    );
    out.push_str(&response.content);
    if !response.content.ends_with('\n') {
        out.push('\n');
    }
    if let Some(omitted) = &response.omitted {
        out.push_str(&format!("...（{omitted}）\n"));
    }
    out
}

/// 把搜索结果裁剪为对外 JSON 结构（保持字段稳定）。
pub fn results_to_json(results: &[SearchResult]) -> serde_json::Value {
    serde_json::json!(results)
}
