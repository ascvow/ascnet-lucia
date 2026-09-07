//! 索引流水线：扫描 → 快照 → 解析 → 分块 → SQLite → Embedding → 向量。
//!
//! 一次文件修改严格按下面的顺序处理，保证 SQLite 与 USearch 在没有共同事务的情况下仍然一致：
//!
//! - **A. 事务外**：读取源码快照、计算内容 Hash、解析并分块；
//! - **B. 一个短写事务**：写入新版本与代码块、激活新版本、更新 FTS 与符号、
//!   失效旧版本、登记旧向量删除操作、把新代码块标记为待嵌入；
//! - **C. 事务外**：查询 Embedding 缓存并请求缺失向量；
//! - **D. 回写前重新校验**：代码块仍是当前版本才关联向量并登记 ADD，否则只保留可复用缓存；
//! - **E. Vector Writer**：顺序消费 `vector_ops` 并更新 USearch。
//!
//! 因此新代码先通过符号与全文检索可见，语义索引追上之前查询会返回 pending / degraded 状态。

pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::chunker::{chunk_file, ChunkContext};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::models::{normalize_l2, Provider};
use crate::parser;
use crate::store::{count_lines, PreparedFile, Store};
use crate::types::Language;
use crate::vector::VectorIndex;
use crate::workspace::scan::{
    detect_language, read_snapshot, scan_workspace, Classification, ScannedFile, SkipReason,
    SkippedFile,
};
use crate::workspace::Workspace;

/// 一次并行准备的批大小。批内并行解析，批间顺序提交，因此内存占用有界。
const PREPARE_BATCH: usize = 64;

/// 单次 Embedding 处理的最大代码块数，避免一次拉取过多待办。
const EMBED_PASS_LIMIT: usize = 512;

/// 目录索引阶段的统计。
#[derive(Debug, Clone, Default)]
pub struct CatalogStats {
    /// 扫描到的候选文件数。
    pub scanned: i64,
    /// 内容变化并重新索引的文件数。
    pub changed: i64,
    /// 内容未变化因而跳过重解析的文件数。
    pub unchanged: i64,
    /// 标记为删除的文件数。
    pub removed: i64,
    /// 被跳过的文件及原因。
    pub skipped: Vec<SkippedFile>,
    /// 耗时毫秒。
    pub duration_ms: i64,
}

/// Embedding 阶段的统计。
#[derive(Debug, Clone, Default)]
pub struct EmbedStats {
    /// 本次实际向模型请求并生成的向量数。
    pub created: i64,
    /// 命中缓存直接复用的向量数。
    pub cached: i64,
    /// 本次失败的代码块数。
    pub failed: i64,
    /// 仍然等待处理的代码块数。
    pub pending: i64,
}

/// 单个文件的准备结果（事务外完成）。
enum Prepared {
    /// 需要写入的新版本。
    Ready(Box<PreparedFile>),
    /// 内容未变化，只需刷新采集元数据。
    Unchanged { path: String, size: u64, mtime_ns: i64 },
    /// 应记录跳过原因。
    Skip {
        path: String,
        language: Language,
        reason: SkipReason,
        detail: String,
    },
}

/// 对一个已读取的源码快照做解析与分块，生成可直接提交的写入契约。
///
/// 全过程不访问数据库，也不访问网络，因此可以放在并行线程里执行。
pub fn prepare_file(
    config: &Config,
    rel_path: &str,
    language: Language,
    text: String,
    hash: String,
    size: u64,
    mtime_ns: i64,
) -> Result<PreparedFile> {
    // 解析失败不会让文件消失：`parse` 返回空结果时 `chunk_file` 退回文本兜底分块。
    let parsed = parser::parse(language, &text)?;
    let ctx = ChunkContext {
        rel_path,
        language,
        source: &text,
        config: &config.chunk,
        document_prefix: &config.embedding.document_prefix,
        max_input_chars: config.embedding.max_input_chars,
    };
    let chunks = chunk_file(&ctx, &parsed)?;
    Ok(PreparedFile {
        rel_path: rel_path.to_string(),
        language,
        file_hash: hash,
        size,
        mtime_ns,
        line_count: count_lines(&text),
        content: text,
        chunks,
        imports: parsed.imports,
    })
}

/// 对整个 workspace 做一次目录索引。
///
/// `verify` 为真时忽略大小/修改时间快速路径，对每个文件重新计算内容 Hash，
/// 用于低频校验扫描与 `--force-rebuild`。
pub fn refresh_catalog(
    store: &mut Store,
    workspace: &Workspace,
    config: &Config,
    verify: bool,
) -> Result<CatalogStats> {
    let started = std::time::Instant::now();
    let scan = scan_workspace(workspace, config)?;
    let catalog = store.catalog()?;
    let mut stats = CatalogStats {
        scanned: scan.files.len() as i64,
        skipped: scan.skipped.clone(),
        ..Default::default()
    };

    // 扫描阶段已确定跳过的文件也要留下可观察的记录。
    for skipped in &scan.skipped {
        store.mark_skipped(
            &skipped.path,
            detect_language(&skipped.path),
            skipped.reason.as_str(),
            &skipped.detail,
        )?;
    }

    // 快速路径：大小与修改时间都未变的活跃文件不再读取内容。
    let mut to_process: Vec<&ScannedFile> = Vec::new();
    for file in &scan.files {
        let unchanged_metadata = !verify
            && catalog.get(&file.rel_path).is_some_and(|entry| {
                entry.state == crate::store::FileState::Active
                    && entry.size == file.size as i64
                    && entry.mtime_ns == file.mtime_ns
            });
        if unchanged_metadata {
            stats.unchanged += 1;
        } else {
            to_process.push(file);
        }
    }

    let known_hashes: HashMap<&str, Option<&str>> = catalog
        .iter()
        .map(|(path, entry)| (path.as_str(), entry.file_hash.as_deref()))
        .collect();

    for batch in to_process.chunks(PREPARE_BATCH) {
        for prepared in prepare_batch(config, batch, &known_hashes)? {
            match prepared {
                Prepared::Ready(file) => {
                    store.commit_file(&file)?;
                    stats.changed += 1;
                }
                Prepared::Unchanged {
                    path,
                    size,
                    mtime_ns,
                } => {
                    // 内容一致但时间戳变了：刷新采集元数据，避免每次扫描重复读取。
                    store.touch_file(&path, size, mtime_ns)?;
                    stats.unchanged += 1;
                }
                Prepared::Skip {
                    path,
                    language,
                    reason,
                    detail,
                } => {
                    store.mark_skipped(&path, language, reason.as_str(), &detail)?;
                    stats.skipped.push(SkippedFile {
                        path,
                        reason,
                        detail,
                    });
                }
            }
        }
    }

    // 目录里还在、磁盘上已经没有的活跃文件视为删除。
    let present: std::collections::HashSet<&str> =
        scan.files.iter().map(|f| f.rel_path.as_str()).collect();
    let skipped_now: std::collections::HashSet<&str> =
        scan.skipped.iter().map(|f| f.path.as_str()).collect();
    for (path, entry) in &catalog {
        if entry.state != crate::store::FileState::Active {
            continue;
        }
        if present.contains(path.as_str()) || skipped_now.contains(path.as_str()) {
            continue;
        }
        if store.mark_deleted(path)? {
            stats.removed += 1;
        }
    }

    stats.duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok(stats)
}

/// 并行读取并准备一批文件。解析属于 CPU 任务，线程数由配置限定。
fn prepare_batch(
    config: &Config,
    batch: &[&ScannedFile],
    known_hashes: &HashMap<&str, Option<&str>>,
) -> Result<Vec<Prepared>> {
    let threads = config.parallelism().min(batch.len().max(1));
    let mut results: Vec<Option<Prepared>> = (0..batch.len()).map(|_| None).collect();

    if threads <= 1 {
        for (slot, file) in results.iter_mut().zip(batch.iter()) {
            *slot = Some(prepare_one(config, file, known_hashes));
        }
        return Ok(results.into_iter().flatten().collect());
    }

    // 把批切成 `threads` 段，每段一个作用域线程；不引入线程池依赖。
    let segment = batch.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (chunk_index, (files, slots)) in batch
            .chunks(segment)
            .zip(results.chunks_mut(segment))
            .enumerate()
        {
            let _ = chunk_index;
            handles.push(scope.spawn(move || {
                for (slot, file) in slots.iter_mut().zip(files.iter()) {
                    *slot = Some(prepare_one(config, file, known_hashes));
                }
            }));
        }
        for handle in handles {
            // 解析线程 panic 说明存在程序缺陷，向上传播而不是静默丢文件。
            if handle.join().is_err() {
                return Err(Error::Internal("解析线程异常终止".to_string()));
            }
        }
        Ok(())
    })?;

    Ok(results.into_iter().flatten().collect())
}

fn prepare_one(
    config: &Config,
    file: &ScannedFile,
    known_hashes: &HashMap<&str, Option<&str>>,
) -> Prepared {
    let language = detect_language(&file.rel_path);
    let snapshot = match read_snapshot(&file.abs_path, config.index.max_file_size_bytes) {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err((reason, detail))) => {
            return Prepared::Skip {
                path: file.rel_path.clone(),
                language,
                reason,
                detail,
            };
        }
        Err(error) => {
            return Prepared::Skip {
                path: file.rel_path.clone(),
                language,
                reason: SkipReason::ReadError,
                detail: error.to_string(),
            };
        }
    };
    // 内容 Hash 一致就不重新解析，也不重新嵌入。
    if known_hashes
        .get(file.rel_path.as_str())
        .and_then(|hash| *hash)
        == Some(snapshot.hash.as_str())
    {
        return Prepared::Unchanged {
            path: file.rel_path.clone(),
            size: snapshot.size,
            mtime_ns: snapshot.mtime_ns,
        };
    }
    match prepare_file(
        config,
        &file.rel_path,
        language,
        snapshot.text,
        snapshot.hash,
        snapshot.size,
        snapshot.mtime_ns,
    ) {
        Ok(prepared) => Prepared::Ready(Box::new(prepared)),
        Err(error) => Prepared::Skip {
            path: file.rel_path.clone(),
            language,
            reason: SkipReason::ReadError,
            detail: format!("解析失败: {error}"),
        },
    }
}

/// 重新采集指定的若干路径。
///
/// 提供给 `ensure_indexed(paths)`：返回前保证这些路径已重新读取并提交目录状态。
/// 不保证语义索引已经追上，语义状态由调用方通过 `status` 观察。
pub fn refresh_paths(
    store: &mut Store,
    workspace: &Workspace,
    config: &Config,
    paths: &[String],
) -> Result<CatalogStats> {
    let started = std::time::Instant::now();
    let filter = crate::workspace::scan::FileFilter::new(workspace, config)?;
    let catalog = store.catalog()?;
    let mut stats = CatalogStats::default();

    for rel_path in paths {
        // 所有路径都必须经过作用域校验，不允许 `..`、绝对路径或外部符号链接。
        let abs_path = workspace.resolve(rel_path)?;
        let language = detect_language(rel_path);
        stats.scanned += 1;

        if !abs_path.exists() {
            if store.mark_deleted(rel_path)? {
                stats.removed += 1;
            }
            continue;
        }
        match filter.classify(&abs_path) {
            Classification::Ignore => {
                // 命中噪声规则或未通过 include：从目录中移除，不作为可搜索内容。
                if store.mark_deleted(rel_path)? {
                    stats.removed += 1;
                }
                continue;
            }
            Classification::Skip(reason) => {
                store.mark_skipped(rel_path, language, reason.as_str(), "命中过滤规则")?;
                stats.skipped.push(SkippedFile {
                    path: rel_path.clone(),
                    reason,
                    detail: "命中过滤规则".to_string(),
                });
                continue;
            }
            Classification::Index => {}
        }

        let metadata = std::fs::symlink_metadata(&abs_path)
            .map_err(|e| Error::io(format!("读取 {} 元数据", abs_path.display()), e))?;
        let scanned = ScannedFile {
            rel_path: rel_path.clone(),
            abs_path,
            size: metadata.len(),
            mtime_ns: crate::workspace::scan::mtime_ns(&metadata),
        };
        let known: HashMap<&str, Option<&str>> = catalog
            .get(rel_path)
            .map(|entry| (rel_path.as_str(), entry.file_hash.as_deref()))
            .into_iter()
            .collect();
        match prepare_one(config, &scanned, &known) {
            Prepared::Ready(file) => {
                store.commit_file(&file)?;
                stats.changed += 1;
            }
            Prepared::Unchanged {
                path,
                size,
                mtime_ns,
            } => {
                store.touch_file(&path, size, mtime_ns)?;
                stats.unchanged += 1;
            }
            Prepared::Skip {
                path,
                language,
                reason,
                detail,
            } => {
                store.mark_skipped(&path, language, reason.as_str(), &detail)?;
                stats.skipped.push(SkippedFile {
                    path,
                    reason,
                    detail,
                });
            }
        }
    }
    stats.duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok(stats)
}

/// 为等待中的代码块生成 Embedding，并把新向量写入 USearch。
///
/// 缓存查询与模型请求都在事务之外完成。模型返回后逐条重新校验代码块是否仍是当前版本，
/// 因此请求乱序返回时晚到的旧结果不会覆盖新代码。
pub async fn embed_pending(
    store: &std::sync::Mutex<Store>,
    index: &std::sync::Mutex<VectorIndex>,
    provider: &Arc<Provider>,
    config: &Config,
) -> Result<EmbedStats> {
    let space = provider.space().clone();
    let mut stats = EmbedStats::default();
    let max_attempts = config.embedding.max_chunk_attempts;

    loop {
        // ---- 取一批待办，并先用缓存满足其中一部分 ----
        let pending = {
            let guard = lock_store(store)?;
            guard.pending_embeddings(EMBED_PASS_LIMIT, max_attempts)?
        };
        if pending.is_empty() {
            break;
        }

        let mut misses: Vec<(i64, String, String)> = Vec::new();
        {
            let mut guard = lock_store(store)?;
            for item in &pending {
                let cache_key = space.cache_key(&item.input);
                if guard.cached_vector(&space.id, &cache_key)?.is_some() {
                    if guard.attach_embedding(item.chunk_id, &space.id, &cache_key)? {
                        stats.cached += 1;
                    }
                } else {
                    misses.push((item.chunk_id, item.input.clone(), cache_key));
                }
            }
        }

        if !misses.is_empty() {
            let outcomes = request_embeddings(provider, config, &misses).await;
            let mut guard = lock_store(store)?;
            for outcome in outcomes {
                match outcome {
                    BatchOutcome::Ok { items } => {
                        for (chunk_id, cache_key, vector) in items {
                            guard.put_cached_vector(&space.id, &cache_key, &vector)?;
                            // 重新校验：已失效的代码块只保留可复用缓存，不重新激活。
                            if guard.attach_embedding(chunk_id, &space.id, &cache_key)? {
                                stats.created += 1;
                            }
                        }
                    }
                    BatchOutcome::Failed { chunk_ids, message } => {
                        for chunk_id in chunk_ids {
                            guard.record_embedding_failure(chunk_id, &message, max_attempts)?;
                            stats.failed += 1;
                        }
                    }
                }
            }
        }

        // ---- Vector Writer：顺序消费更新日志 ----
        {
            let guard = lock_store(store)?;
            let mut index_guard = lock_index(index)?;
            index_guard.replay(&guard)?;
        }

        if pending.len() < EMBED_PASS_LIMIT {
            break;
        }
    }

    let guard = lock_store(store)?;
    stats.pending = guard.counters()?.embeddings_pending;
    Ok(stats)
}

/// 一个批次的请求结果。
enum BatchOutcome {
    /// 成功：`(chunk_id, cache_key, 归一化后的向量)`。
    Ok {
        items: Vec<(i64, String, Vec<f32>)>,
    },
    /// 失败：该批全部代码块记一次失败。
    Failed {
        chunk_ids: Vec<i64>,
        message: String,
    },
}

/// 按配置的批大小与并发上限请求模型。
///
/// 并发由信号量限制；任何一批失败都不会中断其余批次。
async fn request_embeddings(
    provider: &Arc<Provider>,
    config: &Config,
    misses: &[(i64, String, String)],
) -> Vec<BatchOutcome> {
    let batch_size = config.embedding.batch_size.max(1);
    let permits = Arc::new(Semaphore::new(config.embedding.max_concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();

    for (batch_index, batch) in misses.chunks(batch_size).enumerate() {
        let provider = Arc::clone(provider);
        let permits = Arc::clone(&permits);
        let items: Vec<(i64, String, String)> = batch.to_vec();
        tasks.spawn(async move {
            // 信号量关闭只会发生在进程退出路径上，这里按失败处理而不是 panic。
            let Ok(_permit) = permits.acquire().await else {
                return (
                    batch_index,
                    Err(Error::Provider("Embedding 并发信号量已关闭".to_string())),
                );
            };
            let inputs: Vec<String> = items.iter().map(|(_, input, _)| input.clone()).collect();
            let result = provider.embed(&inputs).await;
            (batch_index, result)
        });
    }

    let mut collected: Vec<(usize, Result<Vec<Vec<f32>>>)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(value) => collected.push(value),
            Err(error) => {
                tracing::error!(error = %error, "Embedding 任务异常终止");
            }
        }
    }
    // 按批次序号还原顺序，保证写库顺序确定。
    collected.sort_by_key(|(index, _)| *index);

    let batches: Vec<&[(i64, String, String)]> = misses.chunks(batch_size).collect();
    let mut outcomes = Vec::with_capacity(collected.len());
    for (batch_index, result) in collected {
        let Some(batch) = batches.get(batch_index) else {
            continue;
        };
        let chunk_ids: Vec<i64> = batch.iter().map(|(id, _, _)| *id).collect();
        match result {
            Ok(vectors) => {
                let mut items = Vec::with_capacity(vectors.len());
                let mut failure: Option<String> = None;
                for (vector, (chunk_id, _, cache_key)) in vectors.into_iter().zip(batch.iter()) {
                    let mut vector = vector;
                    if let Err(error) = normalize_l2(&mut vector) {
                        failure = Some(error.to_string());
                        break;
                    }
                    items.push((*chunk_id, cache_key.clone(), vector));
                }
                match failure {
                    Some(message) => outcomes.push(BatchOutcome::Failed { chunk_ids, message }),
                    None => outcomes.push(BatchOutcome::Ok { items }),
                }
            }
            Err(error) => outcomes.push(BatchOutcome::Failed {
                chunk_ids,
                message: error.to_string(),
            }),
        }
    }
    outcomes
}

/// 获取存储锁；锁中毒说明此前有线程 panic，此时明确报错而不是继续使用可疑状态。
pub(crate) fn lock_store(store: &std::sync::Mutex<Store>) -> Result<std::sync::MutexGuard<'_, Store>> {
    store
        .lock()
        .map_err(|_| Error::Internal("存储锁已中毒".to_string()))
}

/// 获取向量索引锁。
///
/// 对 `Mutex<VectorIndex>` 与 `Mutex<Option<VectorIndex>>` 通用：
/// 引擎在没有可用 Embedding 空间时持有 `None`。
pub(crate) fn lock_index<T>(index: &std::sync::Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    index
        .lock()
        .map_err(|_| Error::Internal("向量索引锁已中毒".to_string()))
}
