//! 原子会话记录文件与存储事务编排。

use crate::{
    file_lock::{open_cross_process_lock_file, shared_operation_lock, FileStoreOperationGuard},
    prepare_saved_record, validate_record, verify_revision, SessionId, SessionRecord, SessionStore,
    SessionStoreError, SessionSummary,
};
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::Mutex,
};
use uuid::Uuid;

mod summary_index;
#[cfg(test)]
use summary_index::{StoredSessionSummaryIndex, CURRENT_SUMMARY_INDEX_SCHEMA_VERSION};

const SESSION_FILE_EXTENSION: &str = "json";
const STORE_LOCK_FILE_NAME: &str = ".lucia-session.lock";

/// 使用独立 JSON 文件保存记录的原子文件会话存储。
///
/// 每次写入先创建同目录临时文件并同步内容，再原子替换目标文件。该实现拒绝会话
/// 文件和存储根目录上的符号链接，并通过按规范化根目录共享的异步锁与锁文件保证
/// 同一进程及多个 Lucia 进程的操作不会交错。文件锁属于协作式锁；绕过本类型直接
/// 修改 JSON 文件的外部程序不受保护。[`SessionStore::save`] 会在同一次锁定期间完成
/// 旧修订号读取、CAS 校验、原子替换及摘要索引更新，因此 `expected_revision` 对协作
/// 进程同样有效。索引缺失或损坏时会在首次摘要操作中从已有会话文件重建。
#[derive(Debug, Clone)]
pub struct FileSessionStore {
    root: Arc<PathBuf>,
    operation_lock: Arc<Mutex<()>>,
    cross_process_lock_file: Arc<std::fs::File>,
}

impl FileSessionStore {
    /// 创建或打开文件会话存储，并固定其规范化根目录。
    ///
    /// # Errors
    ///
    /// 当根路径是符号链接、不是目录或无法创建时返回错误。
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let root = root.as_ref().to_path_buf();
        ensure_safe_root(&root).await?;
        let root = fs::canonicalize(&root)
            .await
            .map_err(|source| io_error("规范化会话存储目录", &root, source))?;
        ensure_safe_root(&root).await?;
        let operation_lock = shared_operation_lock(&root);
        let cross_process_lock_file =
            open_cross_process_lock_file(root.join(STORE_LOCK_FILE_NAME)).await?;
        Ok(Self {
            root: Arc::new(root),
            operation_lock,
            cross_process_lock_file: Arc::new(cross_process_lock_file),
        })
    }

    /// 返回规范化后的存储根目录。
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn record_path(&self, id: &SessionId) -> PathBuf {
        self.root
            .join(format!("{}.{}", id.as_str(), SESSION_FILE_EXTENSION))
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(STORE_LOCK_FILE_NAME)
    }

    async fn acquire_operation_lock(&self) -> Result<FileStoreOperationGuard, SessionStoreError> {
        // owned guard 跟随阻塞锁请求，即使调用 future 被取消，也不会让同进程的下一项操作越过它。
        let operation_guard = Arc::clone(&self.operation_lock).lock_owned().await;
        let file = Arc::clone(&self.cross_process_lock_file);
        let path = self.lock_path();
        let join_error_path = path.clone();
        tokio::task::spawn_blocking(move || {
            file.lock()
                .map_err(|source| io_error("获取会话存储跨进程锁", &path, source))?;
            Ok(FileStoreOperationGuard::new(file, operation_guard))
        })
        .await
        .map_err(|source| blocking_task_error("等待会话存储跨进程锁", join_error_path, source))?
    }

    async fn list_ids_unlocked(&self) -> Result<Vec<SessionId>, SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let mut directory = fs::read_dir(self.root())
            .await
            .map_err(|source| io_error("读取会话存储目录", self.root(), source))?;
        let mut ids = Vec::new();

        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|source| io_error("遍历会话存储目录", self.root(), source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some(SESSION_FILE_EXTENSION) {
                continue;
            }
            let metadata = entry
                .file_type()
                .await
                .map_err(|source| io_error("读取会话目录项类型", &path, source))?;
            if metadata.is_symlink() || !metadata.is_file() {
                return Err(SessionStoreError::UnsafePath {
                    path,
                    reason: "会话目录项必须是非符号链接普通文件",
                });
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| SessionStoreError::UnsafePath {
                    path: path.clone(),
                    reason: "会话文件名必须是 UTF-8",
                })?;
            ids.push(SessionId::new(stem)?);
        }

        ids.sort();
        Ok(ids)
    }

    async fn load_unlocked(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionRecord>, SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let path = self.record_path(id);
        let Some(_) = safe_regular_file_metadata(&path).await? else {
            return Ok(None);
        };
        let data = fs::read(&path)
            .await
            .map_err(|source| io_error("读取会话文件", &path, source))?;
        let record: SessionRecord =
            serde_json::from_slice(&data).map_err(|source| SessionStoreError::InvalidRecord {
                path: path.clone(),
                source,
            })?;
        validate_record(&record)?;
        if &record.id != id {
            return Err(SessionStoreError::RecordIdMismatch {
                path,
                file_id: id.clone(),
                record_id: record.id,
            });
        }
        Ok(Some(record))
    }

    async fn write_unlocked(&self, record: &SessionRecord) -> Result<(), SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let destination = self.record_path(&record.id);
        safe_regular_file_metadata(&destination).await?;
        let data = serde_json::to_vec_pretty(record)?;
        let temporary = self
            .root
            .join(format!(".{}.{}.tmp", record.id, Uuid::new_v4().simple()));

        let result = async {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|source| io_error("创建会话临时文件", &temporary, source))?;
            file.write_all(&data)
                .await
                .map_err(|source| io_error("写入会话临时文件", &temporary, source))?;
            file.sync_all()
                .await
                .map_err(|source| io_error("同步会话临时文件", &temporary, source))?;
            drop(file);
            fs::rename(&temporary, &destination)
                .await
                .map_err(|source| io_error("原子替换会话文件", &destination, source))?;
            sync_directory(self.root()).await
        }
        .await;

        if result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        result
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        let _guard = self.acquire_operation_lock().await?;
        self.load_unlocked(id).await
    }

    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        let _guard = self.acquire_operation_lock().await?;
        let current = self.load_unlocked(&record.id).await?;
        let saved = prepare_saved_record(record, current.as_ref(), expected_revision)?;
        let (mut summaries, index_was_valid) =
            self.load_summary_index_for_update_unlocked().await?;
        if index_was_valid {
            self.invalidate_summary_index_unlocked().await?;
        }
        self.write_unlocked(&saved).await?;
        match summaries.binary_search_by(|summary| summary.id.cmp(&saved.id)) {
            Ok(position) => summaries[position] = SessionSummary::from(&saved),
            Err(position) => summaries.insert(position, SessionSummary::from(&saved)),
        }
        self.write_summary_index_unlocked(&summaries).await?;
        Ok(saved)
    }

    async fn delete(
        &self,
        id: &SessionId,
        expected_revision: u64,
    ) -> Result<(), SessionStoreError> {
        let _guard = self.acquire_operation_lock().await?;
        let current = self.load_unlocked(id).await?;
        verify_revision(id, current.as_ref(), Some(expected_revision))?;
        let (mut summaries, index_was_valid) =
            self.load_summary_index_for_update_unlocked().await?;
        if index_was_valid {
            self.invalidate_summary_index_unlocked().await?;
        }
        let path = self.record_path(id);
        fs::remove_file(&path)
            .await
            .map_err(|source| io_error("删除会话文件", &path, source))?;
        sync_directory(self.root()).await?;
        if let Ok(position) = summaries.binary_search_by(|summary| summary.id.cmp(id)) {
            summaries.remove(position);
        }
        self.write_summary_index_unlocked(&summaries).await
    }

    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        let _guard = self.acquire_operation_lock().await?;
        let ids = self.list_ids_unlocked().await?;
        let mut records = Vec::with_capacity(ids.len());

        for id in ids {
            let path = self.record_path(&id);
            let record = self
                .load_unlocked(&id)
                .await?
                .ok_or(SessionStoreError::UnsafePath {
                    path,
                    reason: "遍历期间会话文件被移除",
                })?;
            records.push(record);
        }

        Ok(records)
    }

    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let _guard = self.acquire_operation_lock().await?;
        self.load_or_rebuild_summary_index_unlocked().await
    }
}

async fn ensure_safe_root(root: &Path) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(root).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(SessionStoreError::UnsafePath {
                path: root.to_path_buf(),
                reason: "存储根路径必须是非符号链接目录",
            });
        }
        Ok(_) => return Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("检查会话存储目录", root, source)),
    }

    fs::create_dir_all(root)
        .await
        .map_err(|source| io_error("创建会话存储目录", root, source))?;
    let metadata = fs::symlink_metadata(root)
        .await
        .map_err(|source| io_error("检查新建会话存储目录", root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionStoreError::UnsafePath {
            path: root.to_path_buf(),
            reason: "存储根路径必须是非符号链接目录",
        });
    }
    Ok(())
}

async fn safe_regular_file_metadata(
    path: &Path,
) -> Result<Option<std::fs::Metadata>, SessionStoreError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(SessionStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "会话路径必须是非符号链接普通文件",
            })
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("检查会话文件", path, source)),
    }
}

async fn sync_directory(path: &Path) -> Result<(), SessionStoreError> {
    #[cfg(unix)]
    {
        let directory = fs::File::open(path)
            .await
            .map_err(|source| io_error("打开会话存储目录", path, source))?;
        directory
            .sync_all()
            .await
            .map_err(|source| io_error("同步会话存储目录", path, source))?;
    }
    Ok(())
}

pub(crate) fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> SessionStoreError {
    SessionStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

pub(crate) fn blocking_task_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: tokio::task::JoinError,
) -> SessionStoreError {
    io_error(operation, path, std::io::Error::other(source.to_string()))
}

#[cfg(test)]
mod tests;
