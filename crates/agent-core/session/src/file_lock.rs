//! 文件会话存储的进程内与跨进程协作锁。

use crate::{
    file_store::{blocking_task_error, io_error},
    SessionStoreError,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Debug)]
pub(crate) struct FileStoreOperationGuard {
    file: Arc<std::fs::File>,
    _operation_guard: OwnedMutexGuard<()>,
}

impl FileStoreOperationGuard {
    /// 组合已取得的进程锁与跨进程文件锁，任一 guard 销毁时统一释放。
    pub(crate) fn new(file: Arc<std::fs::File>, operation_guard: OwnedMutexGuard<()>) -> Self {
        Self {
            file,
            _operation_guard: operation_guard,
        }
    }
}

impl Drop for FileStoreOperationGuard {
    fn drop(&mut self) {
        // 解锁只是单次系统调用，不会像等待锁那样长时间阻塞异步运行时线程。
        let _ = self.file.unlock();
    }
}

pub(crate) fn shared_operation_lock(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    lock
}

pub(crate) async fn open_cross_process_lock_file(
    path: PathBuf,
) -> Result<std::fs::File, SessionStoreError> {
    let join_error_path = path.clone();
    tokio::task::spawn_blocking(move || open_cross_process_lock_file_blocking(&path))
        .await
        .map_err(|source| blocking_task_error("打开会话存储跨进程锁", join_error_path, source))?
}

fn open_cross_process_lock_file_blocking(path: &Path) -> Result<std::fs::File, SessionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(SessionStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "会话存储锁路径必须是非符号链接普通文件",
            });
        }
        Ok(_) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("检查会话存储锁文件", path, source)),
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| io_error("打开会话存储锁文件", path, source))?;
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|source| io_error("复查会话存储锁文件", path, source))?;
    let file_metadata = file
        .metadata()
        .map_err(|source| io_error("读取会话存储锁文件信息", path, source))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(SessionStoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "会话存储锁路径必须是非符号链接普通文件",
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(SessionStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "打开锁文件期间会话存储锁路径发生变化",
            });
        }
    }

    Ok(file)
}
