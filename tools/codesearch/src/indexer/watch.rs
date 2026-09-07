//! 文件监听与去抖合并。
//!
//! 监听只是**加速器**，不是唯一事实来源：
//!
//! - 启动时先建立监听再做首次扫描，避免扫描与监听交接期间漏掉事件；
//! - 同一文件的连续修改在去抖窗口内合并成一次处理；
//! - 待处理路径队列有上限，超过后退化为整目录重扫，不会为每个保存事件无限创建任务；
//! - 监听器报告事件丢失（`need_rescan`）时直接触发重新扫描。

use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::workspace::scan::{Classification, FileFilter};
use crate::workspace::Workspace;

/// 一批合并后的变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchBatch {
    /// 需要重新采集的相对路径（已通过作用域与噪声过滤）。
    Paths(Vec<String>),
    /// 事件丢失或变更过多，需要整目录重新扫描。
    FullRescan,
}

/// 监听句柄。丢弃即停止监听。
pub struct WatchHandle {
    /// 保活：`RecommendedWatcher` 被丢弃后底层监听立即停止。
    _watcher: RecommendedWatcher,
    /// 合并后的变更批次。
    pub batches: tokio::sync::mpsc::Receiver<WatchBatch>,
}

impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHandle").finish_non_exhaustive()
    }
}

/// 启动对 workspace 根目录的递归监听。
///
/// 返回后监听已经生效，调用方可以安全地执行首次全量扫描。
pub fn start(workspace: &Workspace, config: &Config) -> Result<WatchHandle> {
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        // 接收端已关闭说明引擎正在退出，丢弃事件即可。
        let _ = raw_tx.send(event);
    })
    .map_err(|e| Error::Internal(format!("创建文件监听失败: {e}")))?;
    watcher
        .watch(workspace.root(), RecursiveMode::Recursive)
        .map_err(|e| Error::Internal(format!("监听 {} 失败: {e}", workspace.root().display())))?;

    // 通道容量有界：消费不及时时发送端阻塞，形成自然背压而不是堆积任务。
    let (batch_tx, batches) = tokio::sync::mpsc::channel::<WatchBatch>(64);
    let filter = FileFilter::new(workspace, config)?;
    let workspace = workspace.clone();
    let debounce = Duration::from_millis(config.watch.debounce_ms);
    let max_pending = config.watch.max_pending_paths.max(1);

    std::thread::Builder::new()
        .name("codesearch-watch".to_string())
        .spawn(move || {
            debounce_loop(raw_rx, batch_tx, &workspace, &filter, debounce, max_pending);
        })
        .map_err(|e| Error::io("启动监听线程", e))?;

    Ok(WatchHandle {
        _watcher: watcher,
        batches,
    })
}

/// 去抖主循环：阻塞等待首个事件，然后在静默窗口内持续合并。
fn debounce_loop(
    raw_rx: mpsc::Receiver<notify::Result<notify::Event>>,
    batch_tx: tokio::sync::mpsc::Sender<WatchBatch>,
    workspace: &Workspace,
    filter: &FileFilter,
    debounce: Duration,
    max_pending: usize,
) {
    loop {
        let mut pending: HashSet<String> = HashSet::new();
        let mut rescan = false;

        // 阻塞等待第一个事件；发送端关闭时结束线程。
        match raw_rx.recv() {
            Ok(event) => absorb(event, workspace, filter, &mut pending, &mut rescan),
            Err(_) => return,
        }

        // 静默窗口内继续合并，连续保存因此只处理一次。
        loop {
            if pending.len() > max_pending {
                rescan = true;
                pending.clear();
            }
            match raw_rx.recv_timeout(debounce) {
                Ok(event) => absorb(event, workspace, filter, &mut pending, &mut rescan),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = emit(&batch_tx, pending, rescan);
                    return;
                }
            }
        }

        if emit(&batch_tx, pending, rescan).is_err() {
            return;
        }
    }
}

/// 把一次原始事件并入待处理集合。
fn absorb(
    event: notify::Result<notify::Event>,
    workspace: &Workspace,
    filter: &FileFilter,
    pending: &mut HashSet<String>,
    rescan: &mut bool,
) {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            // 监听器自身报错时保守地要求重扫，不假装什么都没发生。
            tracing::warn!(error = %error, "文件监听报错，将触发重新扫描");
            *rescan = true;
            return;
        }
    };
    if event.need_rescan() {
        *rescan = true;
        return;
    }
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return;
    }
    for path in event.paths {
        // 目录事件本身不处理：其中的文件会各自产生事件。
        if path.is_dir() {
            continue;
        }
        // 噪声目录、自身数据目录与未命中 include 的路径不产生任务。
        if matches!(filter.classify(&path), Classification::Ignore) {
            continue;
        }
        if let Some(rel) = workspace.relativize(&path) {
            pending.insert(rel);
        }
    }
}

/// 发送一批合并结果。返回 `Err` 表示消费端已关闭。
fn emit(
    batch_tx: &tokio::sync::mpsc::Sender<WatchBatch>,
    pending: HashSet<String>,
    rescan: bool,
) -> std::result::Result<(), ()> {
    if rescan {
        return batch_tx
            .blocking_send(WatchBatch::FullRescan)
            .map_err(|_| ());
    }
    if pending.is_empty() {
        return Ok(());
    }
    let mut paths: Vec<String> = pending.into_iter().collect();
    paths.sort();
    batch_tx
        .blocking_send(WatchBatch::Paths(paths))
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn merges_rapid_saves_into_one_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Workspace::open(dir.path(), None).expect("workspace");
        let mut config = Config::default();
        config.watch.debounce_ms = 200;
        let mut handle = start(&workspace, &config).expect("watch");

        let target = dir.path().join("a.rs");
        // 连续多次写入应被合并为一个批次。
        for index in 0..5 {
            std::fs::write(&target, format!("fn a() {{ let x = {index}; }}\n")).expect("write");
            std::thread::sleep(Duration::from_millis(20));
        }

        let batch = tokio::time::timeout(Duration::from_secs(10), handle.batches.recv())
            .await
            .expect("等待监听批次超时")
            .expect("批次通道已关闭");
        match batch {
            WatchBatch::Paths(paths) => assert!(paths.contains(&"a.rs".to_string())),
            WatchBatch::FullRescan => {}
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ignores_paths_excluded_by_filters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Workspace::open(dir.path(), None).expect("workspace");
        let config = Config::default();
        let mut handle = start(&workspace, &config).expect("watch");

        // 噪声目录与自身数据目录的写入不应产生批次。
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).expect("mkdir");
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "1\n").expect("write");
        std::fs::create_dir_all(dir.path().join(".codesearch")).expect("mkdir");
        std::fs::write(dir.path().join(".codesearch/index.db"), "x").expect("write");
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(dir.path().join("real.rs"), "fn real() {}\n").expect("write");

        let batch = tokio::time::timeout(Duration::from_secs(10), handle.batches.recv())
            .await
            .expect("等待监听批次超时")
            .expect("批次通道已关闭");
        if let WatchBatch::Paths(paths) = batch {
            assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
            assert!(!paths.iter().any(|p| p.starts_with(".codesearch")));
        }
    }
}
