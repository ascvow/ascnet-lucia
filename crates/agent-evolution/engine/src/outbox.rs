//! 持久化 Evolution Outbox：主 Turn 与进化外循环之间的提交边界。
//!
//! 主 Turn 结束后，处置结果以只追加记录写入 Outbox；外层消费者按序拉取，
//! 处理后标记为已消费。消费失败可以重试，记录不会丢失。

use agent_evolution_protocol::{
    DiagnosticStatus, EpisodeId, EvolutionIssueId, FailureDisposition, Outcome,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};

/// Outbox 记录标识前缀。
const OUTBOX_PREFIX: &str = "outbox";

/// 一条待消费的进化外循环任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionOutboxItem {
    /// Outbox 记录标识。
    pub outbox_id: String,
    /// 来源 Episode。
    pub episode_id: EpisodeId,
    /// 运行终态。
    pub outcome: Outcome,
    /// 建议处置。
    pub disposition: FailureDisposition,
    /// 聚合后的 Issue；未聚合时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<EvolutionIssueId>,
    /// Issue 当前诊断状态。
    pub issue_status: DiagnosticStatus,
    /// Unix 毫秒时间戳。
    pub created_at_ms: u64,
    /// 是否已被外层消费者处理。
    pub consumed: bool,
}

/// 只追加 Outbox 存储接口。
#[async_trait]
pub trait EvolutionOutbox: Send + Sync {
    /// 追加一条记录；ID 已存在时拒绝。
    ///
    /// # Errors
    ///
    /// ID 冲突或 I/O 失败时返回错误。
    async fn append(&self, item: &EvolutionOutboxItem) -> Result<(), OutboxError>;

    /// 返回全部未消费记录，按创建时间排序。
    ///
    /// # Errors
    ///
    /// 记录损坏或 I/O 失败时返回错误。
    async fn pending(&self) -> Result<Vec<EvolutionOutboxItem>, OutboxError>;

    /// 把指定记录标记为已消费；不存在时返回 `Ok(false)`。
    ///
    /// # Errors
    ///
    /// 记录损坏或 I/O 失败时返回错误。
    async fn mark_consumed(&self, outbox_id: &str) -> Result<bool, OutboxError>;
}

/// 基于 JSON 文件的本地 Outbox。
///
/// 每条记录一个不可变文件；消费状态通过独立 `.consumed` 标记文件表达，原始记录永不
/// 重写，因此崩溃恢复和审计不会依赖一次可能撕裂的原地覆盖。
#[derive(Debug, Clone)]
pub struct FileEvolutionOutbox {
    root: PathBuf,
}

impl FileEvolutionOutbox {
    /// 打开指定根目录；目录在首次追加时创建。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn item_path(&self, outbox_id: &str) -> PathBuf {
        self.root
            .join(format!("{}-{}.json", OUTBOX_PREFIX, outbox_id))
    }

    fn consumed_path(&self, outbox_id: &str) -> PathBuf {
        self.root
            .join(format!("{}-{}.consumed", OUTBOX_PREFIX, outbox_id))
    }
}

#[async_trait]
impl EvolutionOutbox for FileEvolutionOutbox {
    async fn append(&self, item: &EvolutionOutboxItem) -> Result<(), OutboxError> {
        validate_outbox_id(&item.outbox_id)?;
        if item.consumed {
            return Err(OutboxError::InvalidState(
                "新 Outbox 记录不能预先标记为已消费",
            ));
        }
        ensure_safe_root(&self.root).await?;
        let path = self.item_path(&item.outbox_id);
        let bytes = serde_json::to_vec_pretty(item)
            .map_err(|source| OutboxError::Serialization { source })?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    OutboxError::AlreadyExists(item.outbox_id.clone())
                } else {
                    io_error("创建 Outbox 文件", &path, source)
                }
            })?;
        file.write_all(&bytes)
            .await
            .map_err(|source| io_error("写入 Outbox 文件", &path, source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("同步 Outbox 文件", &path, source))
    }

    async fn pending(&self) -> Result<Vec<EvolutionOutboxItem>, OutboxError> {
        match fs::symlink_metadata(&self.root).await {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查 Outbox 目录", &self.root, source)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(OutboxError::UnsafePath {
                    path: self.root.clone(),
                    reason: "Outbox 根路径必须是非符号链接目录",
                })
            }
            Ok(_) => {}
        }
        let mut entries = match fs::read_dir(&self.root).await {
            Err(source) => return Err(io_error("遍历 Outbox 目录", &self.root, source)),
            Ok(entries) => entries,
        };
        let mut items = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| io_error("读取 Outbox 目录项", &self.root, source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(|source| io_error("检查 Outbox 文件", &path, source))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(OutboxError::UnsafePath {
                    path,
                    reason: "Outbox 记录必须是非符号链接普通文件",
                });
            }
            let bytes = fs::read(&path)
                .await
                .map_err(|source| io_error("读取 Outbox 文件", &path, source))?;
            let item: EvolutionOutboxItem =
                serde_json::from_slice(&bytes).map_err(|source| OutboxError::InvalidRecord {
                    path: path.clone(),
                    source,
                })?;
            validate_outbox_id(&item.outbox_id)?;
            if self.item_path(&item.outbox_id) != path {
                return Err(OutboxError::UnsafePath {
                    path,
                    reason: "Outbox 文件名与记录 ID 不一致",
                });
            }
            if item.consumed {
                continue;
            }
            let marker = self.consumed_path(&item.outbox_id);
            match fs::symlink_metadata(&marker).await {
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => items.push(item),
                Err(source) => return Err(io_error("检查 Outbox 消费标记", &marker, source)),
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(OutboxError::UnsafePath {
                        path: marker,
                        reason: "Outbox 消费标记必须是非符号链接普通文件",
                    })
                }
                Ok(_) => {}
            }
        }
        items.sort_by_key(|left| left.created_at_ms);
        Ok(items)
    }

    async fn mark_consumed(&self, outbox_id: &str) -> Result<bool, OutboxError> {
        validate_outbox_id(outbox_id)?;
        ensure_safe_root(&self.root).await?;
        let path = self.item_path(outbox_id);
        match fs::symlink_metadata(&path).await {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(io_error("检查 Outbox 文件", &path, source)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(OutboxError::UnsafePath {
                    path,
                    reason: "Outbox 记录必须是非符号链接普通文件",
                })
            }
            Ok(_) => {}
        }
        let marker = self.consumed_path(outbox_id);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .await
        {
            Ok(file) => file
                .sync_all()
                .await
                .map_err(|source| io_error("同步 Outbox 消费标记", &marker, source))?,
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(io_error("创建 Outbox 消费标记", &marker, source)),
        }
        Ok(true)
    }
}

/// Outbox 存储错误。
#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    /// Outbox ID 不符合安全文件名约束。
    #[error("Outbox ID 无效：{0}")]
    InvalidId(String),
    /// Outbox 记录初始状态不合法。
    #[error("Outbox 状态无效：{0}")]
    InvalidState(&'static str),
    /// ID 已存在。
    #[error("Outbox 记录已存在：{0}")]
    AlreadyExists(String),
    /// JSON 编码失败。
    #[error("序列化 Outbox 记录失败：{source}")]
    Serialization {
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// JSON 记录损坏。
    #[error("Outbox 记录损坏：{path}: {source}")]
    InvalidRecord {
        /// 损坏文件路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 路径不是预期的非符号链接普通文件。
    #[error("Outbox 路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 错误路径。
        path: PathBuf,
        /// 稳定原因。
        reason: &'static str,
    },
    /// 文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始错误。
        #[source]
        source: std::io::Error,
    },
}

/// 校验 Outbox ID 可安全用作单段文件名。
fn validate_outbox_id(value: &str) -> Result<(), OutboxError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(OutboxError::InvalidId(value.chars().take(64).collect()));
    }
    Ok(())
}

/// 创建并验证 Outbox 根目录自身。
async fn ensure_safe_root(root: &Path) -> Result<(), OutboxError> {
    fs::create_dir_all(root)
        .await
        .map_err(|source| io_error("创建 Outbox 目录", root, source))?;
    let metadata = fs::symlink_metadata(root)
        .await
        .map_err(|source| io_error("检查 Outbox 目录", root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(OutboxError::UnsafePath {
            path: root.to_path_buf(),
            reason: "Outbox 根路径必须是非符号链接目录",
        });
    }
    Ok(())
}

fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> OutboxError {
    OutboxError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-outbox-{}", Uuid::new_v4().simple()))
    }

    fn item(id: &str, created_at_ms: u64) -> EvolutionOutboxItem {
        EvolutionOutboxItem {
            outbox_id: id.to_string(),
            episode_id: EpisodeId::generate(),
            outcome: Outcome::TaskFailure,
            disposition: FailureDisposition::EvolutionCandidate,
            issue_id: None,
            issue_status: DiagnosticStatus::Clustered,
            created_at_ms,
            consumed: false,
        }
    }

    #[tokio::test]
    async fn pending_items_are_ordered_and_consumable() {
        let root = temp_root();
        let outbox = FileEvolutionOutbox::new(&root);
        outbox.append(&item("b", 2)).await.expect("应追加");
        outbox.append(&item("a", 1)).await.expect("应追加");

        let pending = outbox.pending().await.expect("应读取待消费");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].outbox_id, "a");
        assert_eq!(pending[1].outbox_id, "b");

        assert!(outbox.mark_consumed("a").await.expect("应标记消费"));
        let pending = outbox.pending().await.expect("应重新读取");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].outbox_id, "b");
        let _ = fs::remove_dir_all(root).await;
    }

    /// 验证 Outbox 拒绝路径逃逸 ID，并通过独立标记消费而不改写原始证据。
    #[tokio::test]
    async fn rejects_unsafe_id_and_preserves_consumed_record() {
        let root = temp_root();
        let outbox = FileEvolutionOutbox::new(&root);
        assert!(matches!(
            outbox.append(&item("../escape", 1)).await,
            Err(OutboxError::InvalidId(_))
        ));

        let record = item("safe-id", 2);
        outbox.append(&record).await.expect("应追加安全记录");
        let path = outbox.item_path(&record.outbox_id);
        let original = fs::read(&path).await.expect("应读取原始记录");
        assert!(outbox
            .mark_consumed(&record.outbox_id)
            .await
            .expect("应标记消费"));
        let after = fs::read(&path).await.expect("消费后仍应读取原始记录");
        assert_eq!(after, original);
        assert!(fs::try_exists(outbox.consumed_path(&record.outbox_id))
            .await
            .expect("应检查消费标记"));
        assert!(outbox.pending().await.expect("应读取待消费").is_empty());
        let _ = fs::remove_dir_all(root).await;
    }
}
