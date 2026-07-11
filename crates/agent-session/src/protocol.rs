//! 会话标识、版本化记录与 CAS 存储协议。

use agent_core::Session;
use async_trait::async_trait;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

/// 当前支持的会话记录格式版本。
pub const CURRENT_SESSION_SCHEMA_VERSION: u32 = 1;

const MAX_SESSION_ID_LENGTH: usize = 128;

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

pub(crate) fn validate_record(record: &SessionRecord) -> Result<(), SessionStoreError> {
    validate_schema_version(&record.id, record.schema_version)
}

pub(crate) fn validate_schema_version(
    id: &SessionId,
    schema_version: u32,
) -> Result<(), SessionStoreError> {
    if schema_version != CURRENT_SESSION_SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchemaVersion {
            id: id.clone(),
            found: schema_version,
            supported: CURRENT_SESSION_SCHEMA_VERSION,
        });
    }
    Ok(())
}

pub(crate) fn prepare_saved_record(
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

pub(crate) fn verify_revision(
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

fn unix_time_ms() -> Result<u64, SessionStoreError> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}
