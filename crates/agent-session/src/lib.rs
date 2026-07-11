//! Lucia Agent 会话的持久化协议与本地存储实现。
//!
//! 本 crate 只保存与服务商无关的 [`Session`]，不负责模型配置、密钥、插件状态或
//! Agent 调度。[`FileSessionStore`] 会对同一规范化根目录使用进程内异步锁和跨进程
//! 文件锁，保证所有协作进程中的 CAS 操作按顺序执行。

use agent_core::Session;
use async_trait::async_trait;
use serde::{
    de::{Error as _, IgnoredAny},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::{Mutex, OwnedMutexGuard, RwLock},
};
use uuid::Uuid;

/// 当前支持的会话记录格式版本。
pub const CURRENT_SESSION_SCHEMA_VERSION: u32 = 1;

const MAX_SESSION_ID_LENGTH: usize = 128;
const SESSION_FILE_EXTENSION: &str = "json";
const STORE_LOCK_FILE_NAME: &str = ".lucia-session.lock";
const SUMMARY_INDEX_FILE_NAME: &str = ".lucia-session-index";
const CURRENT_SUMMARY_INDEX_SCHEMA_VERSION: u32 = 1;

/// 经过路径安全校验的会话标识。
///
/// 标识只允许 1 到 128 个 ASCII 字母、数字、连字符或下划线，因此可以安全地
/// 作为文件名的一部分使用。反序列化同样会执行该校验。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// 生成基于 UUID v4 的随机会话标识。
    ///
    /// 返回值始终满足会话标识的路径安全规则，可直接用于创建 [`SessionRecord`]。
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// 创建会话标识。
    ///
    /// # Errors
    ///
    /// 当标识为空、过长或包含路径分隔符及其他非法字符时返回
    /// [`InvalidSessionId`]。
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSessionId> {
        let value = value.into();
        validate_session_id(&value)?;
        Ok(Self(value))
    }

    /// 返回会话标识的字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for SessionId {
    type Error = InvalidSessionId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for SessionId {
    type Error = InvalidSessionId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for SessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// 会话标识不满足文件名安全规则。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("非法会话标识 {value:?}：{reason}")]
pub struct InvalidSessionId {
    value: String,
    reason: &'static str,
}

impl InvalidSessionId {
    /// 返回校验失败的原始标识。
    pub fn value(&self) -> &str {
        &self.value
    }

    /// 返回固定的失败原因。
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

/// 带版本、修订号和通用元数据的会话持久化记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    /// 持久化格式版本，用于未来迁移。
    pub schema_version: u32,
    /// 会话的稳定标识。
    pub id: SessionId,
    /// 最近一次成功保存后的修订号；新记录在首次保存前为 0。
    pub revision: u64,
    /// 记录创建时间，单位为 UNIX epoch 毫秒。
    pub created_at_ms: u64,
    /// 最近一次成功保存时间，单位为 UNIX epoch 毫秒。
    pub updated_at_ms: u64,
    /// 可供界面展示的可选标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 与具体插件无关的扩展元数据。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// 与模型服务商无关的 Agent 会话。
    pub session: Session,
}

impl SessionRecord {
    /// 创建尚未保存的当前版本会话记录。
    ///
    /// # Errors
    ///
    /// 当系统时间早于 UNIX epoch 时返回 [`SessionStoreError::Clock`]。
    pub fn new(id: SessionId, session: Session) -> Result<Self, SessionStoreError> {
        let now = unix_time_ms()?;
        Ok(Self {
            schema_version: CURRENT_SESSION_SCHEMA_VERSION,
            id,
            revision: 0,
            created_at_ms: now,
            updated_at_ms: now,
            title: None,
            metadata: BTreeMap::new(),
            session,
        })
    }
}

/// 不包含完整消息正文的会话列表摘要。
///
/// 该类型用于会话选择器等只需展示元数据的场景。文件存储会把摘要同步保存到独立
/// 索引中，常规列表操作只读取该索引；兼容旧目录的首次读取会扫描一次会话文件以
/// 重建索引，但不会构造 [`Session`]、消息内容或工具调用结果。需要恢复会话时仍应
/// 通过 [`SessionStore::load`] 读取并校验完整记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    /// 会话的稳定标识。
    pub id: SessionId,
    /// 最近一次成功保存后的修订号。
    pub revision: u64,
    /// 记录创建时间，单位为 UNIX epoch 毫秒。
    pub created_at_ms: u64,
    /// 最近一次成功保存时间，单位为 UNIX epoch 毫秒。
    pub updated_at_ms: u64,
    /// 可供界面展示的可选标题。
    pub title: Option<String>,
    /// 会话内服务商无关消息的数量，包括工具结果消息。
    pub message_count: usize,
}

impl From<&SessionRecord> for SessionSummary {
    fn from(record: &SessionRecord) -> Self {
        Self {
            id: record.id.clone(),
            revision: record.revision,
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            title: record.title.clone(),
            message_count: record.session.messages().len(),
        }
    }
}

#[derive(Deserialize)]
struct StoredSessionSummary {
    schema_version: u32,
    id: SessionId,
    revision: u64,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    title: Option<String>,
    session: StoredSessionMessageCount,
}

#[derive(Deserialize)]
struct StoredSessionMessageCount {
    #[serde(default)]
    messages: Vec<IgnoredAny>,
}

impl StoredSessionSummary {
    fn into_summary(self) -> SessionSummary {
        SessionSummary {
            id: self.id,
            revision: self.revision,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            title: self.title,
            message_count: self.session.messages.len(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSessionSummaryIndex {
    schema_version: u32,
    summaries: Vec<SessionSummary>,
}

enum StoredSessionSummaryIndexState {
    Valid(Vec<SessionSummary>),
    Missing,
    Invalid,
}

/// 会话存储失败。
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// 会话标识未通过路径安全校验。
    #[error(transparent)]
    InvalidSessionId(#[from] InvalidSessionId),
    /// 记录格式版本不受当前实现支持。
    #[error("会话 {id} 的格式版本 {found} 不受支持，当前仅支持 {supported}")]
    UnsupportedSchemaVersion {
        /// 会话标识。
        id: SessionId,
        /// 读取到的格式版本。
        found: u32,
        /// 当前支持的格式版本。
        supported: u32,
    },
    /// 调用方提交的记录修订号与保存条件不一致。
    #[error("会话 {id} 的记录修订号 {record_revision} 与期望修订号 {expected_revision:?} 不一致")]
    RecordRevisionMismatch {
        /// 会话标识。
        id: SessionId,
        /// 记录自身携带的修订号。
        record_revision: u64,
        /// 保存调用要求的修订号，`None` 表示仅创建。
        expected_revision: Option<u64>,
    },
    /// 比较并交换条件未满足。
    #[error("会话 {id} 的修订号冲突：期望 {expected:?}，实际 {actual:?}")]
    RevisionConflict {
        /// 会话标识。
        id: SessionId,
        /// 调用方期望的修订号；`None` 表示期望记录不存在。
        expected: Option<u64>,
        /// 存储中实际存在的修订号；`None` 表示记录不存在。
        actual: Option<u64>,
    },
    /// 修订号已达到上限，无法继续递增。
    #[error("会话 {id} 的修订号已溢出")]
    RevisionOverflow {
        /// 会话标识。
        id: SessionId,
    },
    /// 文件内容不是有效的会话 JSON。
    #[error("无法解析会话文件 {}：{source}", path.display())]
    InvalidRecord {
        /// 会话文件路径。
        path: PathBuf,
        /// JSON 解析错误。
        #[source]
        source: serde_json::Error,
    },
    /// 文件名中的会话标识与记录内容不一致。
    #[error("会话文件 {} 的记录标识为 {record_id}，与文件标识 {file_id} 不一致", path.display())]
    RecordIdMismatch {
        /// 会话文件路径。
        path: PathBuf,
        /// 文件名表示的会话标识。
        file_id: SessionId,
        /// 文件内容表示的会话标识。
        record_id: SessionId,
    },
    /// 存储目录包含可能逃逸根目录的文件类型。
    #[error("不安全的会话存储路径 {}：{reason}", path.display())]
    UnsafePath {
        /// 被拒绝的路径。
        path: PathBuf,
        /// 拒绝原因。
        reason: &'static str,
    },
    /// 系统时间无法转换为 UNIX epoch 毫秒。
    #[error("无法读取系统时间：{0}")]
    Clock(#[from] std::time::SystemTimeError),
    /// 会话记录或摘要索引序列化失败。
    #[error("无法序列化会话存储数据：{0}")]
    Serialize(#[from] serde_json::Error),
    /// 文件系统操作失败。
    #[error("{operation}失败（{}）：{source}", path.display())]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 涉及的路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
}

/// 会话记录存储协议。
///
/// [`Self::save`] 使用 `expected_revision` 执行比较并交换：`None` 只允许创建
/// 不存在的记录，`Some(revision)` 只允许更新该修订号。成功保存会自动把修订号
/// 加一并返回规范化后的记录。
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// 按标识读取记录；记录不存在时返回 `None`。
    ///
    /// # Errors
    ///
    /// 当底层存储不可读、记录损坏或版本不受支持时返回错误。
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError>;

    /// 以比较并交换方式创建或更新记录，并返回新修订号的记录。
    ///
    /// `expected_revision` 为 `None` 时仅创建，为 `Some(revision)` 时仅更新完全
    /// 匹配的记录。调用方传入记录自身的 `revision` 必须与该条件一致。
    ///
    /// # Errors
    ///
    /// 当修订号冲突、记录版本非法或底层写入失败时返回错误。
    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError>;

    /// 仅在修订号匹配时删除记录。
    ///
    /// # Errors
    ///
    /// 当记录不存在、修订号冲突或底层删除失败时返回错误。
    async fn delete(&self, id: &SessionId, expected_revision: u64)
        -> Result<(), SessionStoreError>;

    /// 列出全部记录，结果按会话标识排序。
    ///
    /// # Errors
    ///
    /// 当底层存储不可读或任一记录损坏时返回错误。
    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError>;

    /// 列出全部轻量摘要，结果按会话标识排序。
    ///
    /// 默认实现从 [`Self::list`] 转换结果，以保持已有存储实现兼容。处理大型会话的
    /// 存储应覆盖该方法，避免为了列表展示加载完整消息正文。
    ///
    /// # Errors
    ///
    /// 当底层存储不可读或任一摘要所需字段损坏时返回错误。摘要读取不保证完整消息
    /// 正文可反序列化，恢复前仍须调用 [`Self::load`]。
    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        Ok(self
            .list()
            .await?
            .iter()
            .map(SessionSummary::from)
            .collect())
    }
}

/// 适用于测试和短生命周期进程的内存会话存储。
#[derive(Debug, Clone, Default)]
pub struct MemorySessionStore {
    records: Arc<RwLock<HashMap<SessionId, SessionRecord>>>,
}

impl MemorySessionStore {
    /// 创建空的内存会话存储。
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        Ok(self.records.read().await.get(id).cloned())
    }

    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        let mut records = self.records.write().await;
        let current = records.get(&record.id);
        let saved = prepare_saved_record(record, current, expected_revision)?;
        records.insert(saved.id.clone(), saved.clone());
        Ok(saved)
    }

    async fn delete(
        &self,
        id: &SessionId,
        expected_revision: u64,
    ) -> Result<(), SessionStoreError> {
        let mut records = self.records.write().await;
        verify_revision(id, records.get(id), Some(expected_revision))?;
        records.remove(id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        let mut records: Vec<_> = self.records.read().await.values().cloned().collect();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries: Vec<_> = self
            .records
            .read()
            .await
            .values()
            .map(SessionSummary::from)
            .collect();
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(summaries)
    }
}

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

    fn summary_index_path(&self) -> PathBuf {
        self.root.join(SUMMARY_INDEX_FILE_NAME)
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
            Ok(FileStoreOperationGuard {
                file,
                _operation_guard: operation_guard,
            })
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

    async fn load_summary_unlocked(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionSummary>, SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let path = self.record_path(id);
        let Some(_) = safe_regular_file_metadata(&path).await? else {
            return Ok(None);
        };
        let data = fs::read(&path)
            .await
            .map_err(|source| io_error("读取会话文件", &path, source))?;
        let stored: StoredSessionSummary =
            serde_json::from_slice(&data).map_err(|source| SessionStoreError::InvalidRecord {
                path: path.clone(),
                source,
            })?;
        validate_schema_version(&stored.id, stored.schema_version)?;
        if &stored.id != id {
            return Err(SessionStoreError::RecordIdMismatch {
                path,
                file_id: id.clone(),
                record_id: stored.id,
            });
        }
        Ok(Some(stored.into_summary()))
    }

    async fn read_summary_index_unlocked(
        &self,
    ) -> Result<StoredSessionSummaryIndexState, SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let path = self.summary_index_path();
        let Some(_) = safe_regular_file_metadata(&path).await? else {
            return Ok(StoredSessionSummaryIndexState::Missing);
        };
        let data = fs::read(&path)
            .await
            .map_err(|source| io_error("读取会话摘要索引", &path, source))?;
        let Ok(index) = serde_json::from_slice::<StoredSessionSummaryIndex>(&data) else {
            return Ok(StoredSessionSummaryIndexState::Invalid);
        };
        if index.schema_version != CURRENT_SUMMARY_INDEX_SCHEMA_VERSION
            || !index
                .summaries
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        {
            return Ok(StoredSessionSummaryIndexState::Invalid);
        }
        Ok(StoredSessionSummaryIndexState::Valid(index.summaries))
    }

    async fn rebuild_summary_index_unlocked(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let ids = self.list_ids_unlocked().await?;
        let mut summaries = Vec::with_capacity(ids.len());

        for id in ids {
            let path = self.record_path(&id);
            let summary =
                self.load_summary_unlocked(&id)
                    .await?
                    .ok_or(SessionStoreError::UnsafePath {
                        path,
                        reason: "重建摘要索引期间会话文件被移除",
                    })?;
            summaries.push(summary);
        }

        Ok(summaries)
    }

    async fn load_summary_index_for_update_unlocked(
        &self,
    ) -> Result<(Vec<SessionSummary>, bool), SessionStoreError> {
        match self.read_summary_index_unlocked().await? {
            StoredSessionSummaryIndexState::Valid(summaries) => Ok((summaries, true)),
            StoredSessionSummaryIndexState::Missing => {
                Ok((self.rebuild_summary_index_unlocked().await?, false))
            }
            StoredSessionSummaryIndexState::Invalid => {
                self.invalidate_summary_index_unlocked().await?;
                Ok((self.rebuild_summary_index_unlocked().await?, false))
            }
        }
    }

    async fn load_or_rebuild_summary_index_unlocked(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let (summaries, index_was_valid) = self.load_summary_index_for_update_unlocked().await?;
        if !index_was_valid {
            self.write_summary_index_unlocked(&summaries).await?;
        }
        Ok(summaries)
    }

    async fn invalidate_summary_index_unlocked(&self) -> Result<(), SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let path = self.summary_index_path();
        if safe_regular_file_metadata(&path).await?.is_none() {
            return Ok(());
        }
        fs::remove_file(&path)
            .await
            .map_err(|source| io_error("使会话摘要索引失效", &path, source))?;
        sync_directory(self.root()).await
    }

    async fn write_summary_index_unlocked(
        &self,
        summaries: &[SessionSummary],
    ) -> Result<(), SessionStoreError> {
        ensure_safe_root(self.root()).await?;
        let destination = self.summary_index_path();
        safe_regular_file_metadata(&destination).await?;
        let data = serde_json::to_vec_pretty(&StoredSessionSummaryIndex {
            schema_version: CURRENT_SUMMARY_INDEX_SCHEMA_VERSION,
            summaries: summaries.to_vec(),
        })?;
        let temporary = self.root.join(format!(
            "{}.{}.tmp",
            SUMMARY_INDEX_FILE_NAME,
            Uuid::new_v4().simple()
        ));

        let result = async {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|source| io_error("创建会话摘要索引临时文件", &temporary, source))?;
            file.write_all(&data)
                .await
                .map_err(|source| io_error("写入会话摘要索引临时文件", &temporary, source))?;
            file.sync_all()
                .await
                .map_err(|source| io_error("同步会话摘要索引临时文件", &temporary, source))?;
            drop(file);
            fs::rename(&temporary, &destination)
                .await
                .map_err(|source| io_error("原子替换会话摘要索引", &destination, source))?;
            sync_directory(self.root()).await
        }
        .await;

        if result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        result
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

#[derive(Debug)]
struct FileStoreOperationGuard {
    file: Arc<std::fs::File>,
    _operation_guard: OwnedMutexGuard<()>,
}

impl Drop for FileStoreOperationGuard {
    fn drop(&mut self) {
        // 解锁只是单次系统调用，不会像等待锁那样长时间阻塞异步运行时线程。
        let _ = self.file.unlock();
    }
}

fn validate_session_id(value: &str) -> Result<(), InvalidSessionId> {
    let invalid = |reason| InvalidSessionId {
        value: value.to_owned(),
        reason,
    };
    if value.is_empty() {
        return Err(invalid("标识不能为空"));
    }
    if value.len() > MAX_SESSION_ID_LENGTH {
        return Err(invalid("标识长度不能超过 128 个字符"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid("仅允许 ASCII 字母、数字、连字符和下划线"));
    }
    Ok(())
}

fn shared_operation_lock(root: &Path) -> Arc<Mutex<()>> {
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

async fn open_cross_process_lock_file(path: PathBuf) -> Result<std::fs::File, SessionStoreError> {
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

fn validate_record(record: &SessionRecord) -> Result<(), SessionStoreError> {
    validate_schema_version(&record.id, record.schema_version)
}

fn validate_schema_version(id: &SessionId, schema_version: u32) -> Result<(), SessionStoreError> {
    if schema_version != CURRENT_SESSION_SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchemaVersion {
            id: id.clone(),
            found: schema_version,
            supported: CURRENT_SESSION_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn prepare_saved_record(
    mut record: SessionRecord,
    current: Option<&SessionRecord>,
    expected_revision: Option<u64>,
) -> Result<SessionRecord, SessionStoreError> {
    validate_record(&record)?;
    let record_expected = expected_revision.unwrap_or(0);
    if record.revision != record_expected {
        return Err(SessionStoreError::RecordRevisionMismatch {
            id: record.id,
            record_revision: record.revision,
            expected_revision,
        });
    }
    verify_revision(&record.id, current, expected_revision)?;
    record.revision =
        record_expected
            .checked_add(1)
            .ok_or_else(|| SessionStoreError::RevisionOverflow {
                id: record.id.clone(),
            })?;
    record.updated_at_ms = unix_time_ms()?;
    Ok(record)
}

fn verify_revision(
    id: &SessionId,
    current: Option<&SessionRecord>,
    expected: Option<u64>,
) -> Result<(), SessionStoreError> {
    let actual = current.map(|record| record.revision);
    if actual != expected {
        return Err(SessionStoreError::RevisionConflict {
            id: id.clone(),
            expected,
            actual,
        });
    }
    Ok(())
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

fn unix_time_ms() -> Result<u64, SessionStoreError> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn io_error(
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

fn blocking_task_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: tokio::task::JoinError,
) -> SessionStoreError {
    io_error(operation, path, std::io::Error::other(source.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK_HELPER_ROOT_ENV: &str = "LUCIA_SESSION_LOCK_HELPER_ROOT";
    const LOCK_HELPER_READY_ENV: &str = "LUCIA_SESSION_LOCK_HELPER_READY";
    const LOCK_HELPER_RELEASE_ENV: &str = "LUCIA_SESSION_LOCK_HELPER_RELEASE";

    fn id(value: &str) -> SessionId {
        SessionId::new(value).expect("测试会话标识应该有效")
    }

    fn record(value: &str) -> SessionRecord {
        let mut session = Session::new();
        session.set_system("测试系统提示词");
        session.push_user("测试消息");
        SessionRecord::new(id(value), session).expect("应该可以创建测试记录")
    }

    fn test_directory() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-session-test-{}", Uuid::new_v4()))
    }

    async fn remove_test_directory(path: &Path) {
        let _ = fs::remove_dir_all(path).await;
    }

    #[test]
    fn file_store_cross_process_lock_holder_helper() {
        let Some(root) = std::env::var_os(LOCK_HELPER_ROOT_ENV) else {
            return;
        };
        let ready = PathBuf::from(
            std::env::var_os(LOCK_HELPER_READY_ENV).expect("锁测试必须提供就绪文件路径"),
        );
        let release = PathBuf::from(
            std::env::var_os(LOCK_HELPER_RELEASE_ENV).expect("锁测试必须提供释放文件路径"),
        );
        let lock_path = PathBuf::from(root).join(STORE_LOCK_FILE_NAME);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("子进程应该可以打开会话存储锁文件");
        file.lock().expect("子进程应该可以获取跨进程锁");
        std::fs::write(&ready, b"ready").expect("子进程应该可以发送就绪信号");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !release.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "等待父进程释放跨进程锁超时"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        file.unlock().expect("子进程应该可以释放跨进程锁");
    }

    #[test]
    fn generated_session_ids_are_valid_and_unique() {
        let first = SessionId::generate();
        let second = SessionId::generate();

        assert_ne!(first, second);
        assert!(Uuid::parse_str(first.as_str()).is_ok());
        assert_eq!(SessionId::new(first.to_string()).unwrap(), first);
    }

    #[test]
    fn session_id_rejects_path_escape_and_invalid_json() {
        for invalid in ["", "../escape", "nested/name", ".", "会话"] {
            assert!(SessionId::new(invalid).is_err(), "应拒绝 {invalid:?}");
        }
        let error = serde_json::from_str::<SessionId>(r#""../escape""#)
            .expect_err("反序列化不能绕过标识校验");
        assert!(error.to_string().contains("非法会话标识"));
    }

    #[tokio::test]
    async fn memory_store_supports_cas_lifecycle() {
        let store = MemorySessionStore::new();
        let created = store
            .save(record("session_a"), None)
            .await
            .expect("首次保存应该成功");
        assert_eq!(created.revision, 1);
        assert_eq!(
            store.load(&created.id).await.unwrap(),
            Some(created.clone())
        );

        let mut updated = created.clone();
        updated.title = Some("新标题".to_owned());
        let updated = store
            .save(updated, Some(created.revision))
            .await
            .expect("匹配修订号时应该更新成功");
        assert_eq!(updated.revision, 2);

        let error = store
            .save(created, Some(1))
            .await
            .expect_err("过期记录不能覆盖新记录");
        assert!(matches!(
            error,
            SessionStoreError::RevisionConflict {
                expected: Some(1),
                actual: Some(2),
                ..
            }
        ));

        store
            .delete(&updated.id, updated.revision)
            .await
            .expect("匹配修订号时应该删除成功");
        assert!(store.load(&updated.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_store_lists_records_by_id() {
        let store = MemorySessionStore::new();
        store.save(record("z"), None).await.unwrap();
        store.save(record("a"), None).await.unwrap();

        let ids: Vec<_> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect();
        assert_eq!(ids, ["a", "z"]);
    }

    #[tokio::test]
    async fn memory_store_lists_summaries_by_id() {
        let store = MemorySessionStore::new();
        let mut last = record("z");
        last.session.push_assistant_text("第二条消息");
        let last = store.save(last, None).await.unwrap();
        let mut first = record("a");
        first.title = Some("第一个会话".to_owned());
        let first = store.save(first, None).await.unwrap();

        let summaries = store.list_summaries().await.unwrap();

        assert_eq!(
            summaries,
            [SessionSummary::from(&first), SessionSummary::from(&last)]
        );
        assert_eq!(summaries[0].title.as_deref(), Some("第一个会话"));
        assert_eq!(summaries[1].message_count, 2);
    }

    #[tokio::test]
    async fn file_store_persists_records_across_reopen() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let saved = store.save(record("persisted"), None).await.unwrap();
        assert!(fs::try_exists(store.summary_index_path()).await.unwrap());
        drop(store);

        let reopened = FileSessionStore::open(&root).await.unwrap();
        assert_eq!(reopened.load(&saved.id).await.unwrap(), Some(saved));
        assert_eq!(reopened.list().await.unwrap().len(), 1);
        assert_eq!(
            reopened.list_summaries().await.unwrap(),
            [SessionSummary::from(
                &reopened.load(&id("persisted")).await.unwrap().unwrap()
            )]
        );

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn file_store_summary_index_tracks_updates_and_deletes_across_reopen() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let first = store.save(record("first"), None).await.unwrap();
        let second = store.save(record("second"), None).await.unwrap();
        drop(store);

        let reopened = FileSessionStore::open(&root).await.unwrap();
        let mut updated = first.clone();
        updated.title = Some("更新后的会话".to_owned());
        updated.session.push_assistant_text("新增回复");
        let updated = reopened.save(updated, Some(first.revision)).await.unwrap();
        assert_eq!(
            reopened.list_summaries().await.unwrap(),
            [
                SessionSummary::from(&updated),
                SessionSummary::from(&second)
            ]
        );

        reopened.delete(&second.id, second.revision).await.unwrap();
        drop(reopened);

        let reopened = FileSessionStore::open(&root).await.unwrap();
        assert_eq!(
            reopened.list_summaries().await.unwrap(),
            [SessionSummary::from(&updated)]
        );

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn file_store_rebuilds_missing_index_from_legacy_records() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let mut legacy = record("legacy");
        legacy.revision = 4;
        legacy.title = Some("旧会话".to_owned());
        legacy.session.push_assistant_text("旧回复");
        fs::write(
            store.record_path(&legacy.id),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .await
        .unwrap();
        assert!(!fs::try_exists(store.summary_index_path()).await.unwrap());

        assert_eq!(
            store.list_summaries().await.unwrap(),
            [SessionSummary::from(&legacy)]
        );
        assert!(fs::try_exists(store.summary_index_path()).await.unwrap());
        drop(store);

        let reopened = FileSessionStore::open(&root).await.unwrap();
        assert_eq!(
            reopened.list_summaries().await.unwrap(),
            [SessionSummary::from(&legacy)]
        );

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn file_store_rebuilds_corrupted_summary_index() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let saved = store.save(record("corrupted_index"), None).await.unwrap();
        fs::write(store.summary_index_path(), "不是有效索引".as_bytes())
            .await
            .unwrap();

        assert_eq!(
            store.list_summaries().await.unwrap(),
            [SessionSummary::from(&saved)]
        );
        let rebuilt: StoredSessionSummaryIndex =
            serde_json::from_slice(&fs::read(store.summary_index_path()).await.unwrap()).unwrap();
        assert_eq!(rebuilt.schema_version, CURRENT_SUMMARY_INDEX_SCHEMA_VERSION);
        assert_eq!(rebuilt.summaries, [SessionSummary::from(&saved)]);

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn file_store_summary_index_avoids_reading_session_records() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let saved = store.save(record("indexed"), None).await.unwrap();
        fs::write(store.record_path(&saved.id), "不是有效会话记录".as_bytes())
            .await
            .unwrap();

        assert_eq!(
            store.list_summaries().await.unwrap(),
            [SessionSummary::from(&saved)]
        );
        assert!(matches!(
            store.list().await,
            Err(SessionStoreError::InvalidRecord { .. })
        ));

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn file_store_summary_skips_full_message_deserialization() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let session_id = id("lightweight");
        let path = store.record_path(&session_id);
        let malformed_messages = serde_json::json!({
            "schema_version": CURRENT_SESSION_SCHEMA_VERSION,
            "id": session_id,
            "revision": 7,
            "created_at_ms": 11,
            "updated_at_ms": 22,
            "title": "轻量摘要",
            "session": {
                "messages": [null, { "不是": "有效模型消息" }]
            }
        });
        fs::write(&path, serde_json::to_vec(&malformed_messages).unwrap())
            .await
            .unwrap();

        let summaries = store.list_summaries().await.unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id("lightweight"));
        assert_eq!(summaries[0].revision, 7);
        assert_eq!(summaries[0].updated_at_ms, 22);
        assert_eq!(summaries[0].title.as_deref(), Some("轻量摘要"));
        assert_eq!(summaries[0].message_count, 2);
        assert!(matches!(
            store.list().await,
            Err(SessionStoreError::InvalidRecord { .. })
        ));

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn file_store_serializes_concurrent_cas_updates_across_instances() {
        let root = test_directory();
        let left = FileSessionStore::open(&root).await.unwrap();
        let right = FileSessionStore::open(&root).await.unwrap();
        let created = left.save(record("concurrent"), None).await.unwrap();
        let left_record = created.clone();
        let right_record = created.clone();

        let (left_result, right_result) = tokio::join!(
            left.save(left_record, Some(created.revision)),
            right.save(right_record, Some(created.revision))
        );
        let successes = usize::from(left_result.is_ok()) + usize::from(right_result.is_ok());
        assert_eq!(successes, 1);
        let error = left_result.err().or_else(|| right_result.err()).unwrap();
        assert!(matches!(error, SessionStoreError::RevisionConflict { .. }));

        remove_test_directory(&root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_store_serializes_all_operations_with_another_process() {
        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let loaded = store.save(record("locked_load"), None).await.unwrap();
        let deleted = store.save(record("locked_delete"), None).await.unwrap();
        let saved = record("locked_save");
        let ready = root.join("helper-ready");
        let release = root.join("helper-release");
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::file_store_cross_process_lock_holder_helper")
            .arg("--test-threads=1")
            .env(LOCK_HELPER_ROOT_ENV, &root)
            .env(LOCK_HELPER_READY_ENV, &ready)
            .env(LOCK_HELPER_RELEASE_ENV, &release)
            .kill_on_drop(true)
            .spawn()
            .expect("应该可以启动跨进程锁测试子进程");

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if fs::try_exists(&ready).await.unwrap() {
                    break;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "跨进程锁测试子进程在就绪前退出"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("等待跨进程锁测试子进程就绪超时");

        let barrier = Arc::new(tokio::sync::Barrier::new(6));
        let load_task = tokio::spawn({
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            let id = loaded.id.clone();
            async move {
                barrier.wait().await;
                store.load(&id).await
            }
        });
        let save_task = tokio::spawn({
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                store.save(saved, None).await
            }
        });
        let delete_task = tokio::spawn({
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            let id = deleted.id.clone();
            let revision = deleted.revision;
            async move {
                barrier.wait().await;
                store.delete(&id, revision).await
            }
        });
        let list_task = tokio::spawn({
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                store.list().await
            }
        });
        let summaries_task = tokio::spawn({
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                store.list_summaries().await
            }
        });
        barrier.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        assert!(!load_task.is_finished(), "load 必须等待跨进程锁");
        assert!(!save_task.is_finished(), "save 必须等待跨进程锁");
        assert!(!delete_task.is_finished(), "delete 必须等待跨进程锁");
        assert!(!list_task.is_finished(), "list 必须等待跨进程锁");
        assert!(
            !summaries_task.is_finished(),
            "list_summaries 必须等待跨进程锁"
        );

        fs::write(&release, b"release").await.unwrap();
        let (load_result, save_result, delete_result, list_result, summaries_result) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(load_task, save_task, delete_task, list_task, summaries_task)
            })
            .await
            .expect("跨进程锁释放后存储操作应该完成");

        assert_eq!(load_result.unwrap().unwrap(), Some(loaded));
        assert_eq!(save_result.unwrap().unwrap().revision, 1);
        delete_result.unwrap().unwrap();
        list_result.unwrap().unwrap();
        summaries_result.unwrap().unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("等待跨进程锁测试子进程退出超时")
            .unwrap();
        assert!(status.success());

        remove_test_directory(&root).await;
    }

    #[tokio::test]
    async fn store_rejects_unsupported_schema_and_revision_mismatch() {
        let store = MemorySessionStore::new();
        let mut unsupported = record("unsupported");
        unsupported.schema_version = CURRENT_SESSION_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.save(unsupported, None).await,
            Err(SessionStoreError::UnsupportedSchemaVersion { .. })
        ));

        let mut mismatched = record("mismatched");
        mismatched.revision = 3;
        assert!(matches!(
            store.save(mismatched, None).await,
            Err(SessionStoreError::RecordRevisionMismatch { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_store_rejects_symlinked_session_file() {
        use std::os::unix::fs::symlink;

        let root = test_directory();
        let store = FileSessionStore::open(&root).await.unwrap();
        let outside = root.with_extension("outside.json");
        fs::write(&outside, b"{}").await.unwrap();
        symlink(&outside, store.record_path(&id("linked"))).unwrap();

        assert!(matches!(
            store.load(&id("linked")).await,
            Err(SessionStoreError::UnsafePath { .. })
        ));

        remove_test_directory(&root).await;
        let _ = fs::remove_file(outside).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_store_rejects_symlinked_lock_file() {
        use std::os::unix::fs::symlink;

        let root = test_directory();
        fs::create_dir_all(&root).await.unwrap();
        let outside = root.with_extension("outside.lock");
        fs::write(&outside, b"").await.unwrap();
        symlink(&outside, root.join(STORE_LOCK_FILE_NAME)).unwrap();

        assert!(matches!(
            FileSessionStore::open(&root).await,
            Err(SessionStoreError::UnsafePath { .. })
        ));

        remove_test_directory(&root).await;
        let _ = fs::remove_file(outside).await;
    }
}
