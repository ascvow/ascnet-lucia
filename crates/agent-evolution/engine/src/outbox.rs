//! 持久化 Evolution Outbox：主 Turn 与进化外循环之间的提交边界。
//!
//! 主 Turn 结束后，只有进化候选以只追加记录写入 Outbox；外层消费者按序拉取，
//! 处理后标记为已消费。旧版非候选记录只允许读取和显式迁移，不再接受新写入。

use crate::intervention_queue::{
    is_intervention_disposition, InterventionQueue, InterventionQueueError, InterventionQueueItemV1,
};
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
    /// 追加一条进化候选记录；ID 已存在时拒绝。
    ///
    /// # Errors
    ///
    /// 处置不是 `EvolutionCandidate`、ID 冲突或 I/O 失败时返回错误。
    async fn append(&self, item: &EvolutionOutboxItem) -> Result<(), OutboxError>;

    /// 返回全部未消费进化候选，按创建时间和 ID 稳定排序。
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

    /// 返回历史遗留的非进化处置记录，供显式迁移或人工审计。
    ///
    /// 新写入已禁止产生这类记录；该方法不会修改原始 JSON 或消费标记。
    ///
    /// # Errors
    ///
    /// 记录损坏、路径不安全或文件系统读取失败时返回错误。
    pub async fn legacy_pending(&self) -> Result<Vec<EvolutionOutboxItem>, OutboxError> {
        Ok(self
            .unconsumed_items()
            .await?
            .into_iter()
            .filter(|item| item.disposition != FailureDisposition::EvolutionCandidate)
            .collect())
    }

    /// 把历史人工处置记录幂等迁移到独立人工干预队列。
    ///
    /// 每条记录先写入人工队列，确认成功后才追加 `.consumed` 标记；任一步失败均可再次
    /// 调用。原 Outbox JSON 永不删除或改写。`Observe`、`Ignore` 与 `RetryInTurn` 没有人工
    /// 队列语义，会显式拒绝迁移。
    ///
    /// # Errors
    ///
    /// 历史处置不可迁移、人工队列写入失败、来源记录消失或 Outbox 操作失败时返回错误。
    pub async fn migrate_legacy_interventions<Q>(
        &self,
        queue: &Q,
    ) -> Result<usize, LegacyOutboxMigrationError>
    where
        Q: InterventionQueue + ?Sized,
    {
        let legacy_items = self
            .legacy_pending()
            .await
            .map_err(LegacyOutboxMigrationError::Outbox)?;
        let mut migrated = 0;
        for legacy in legacy_items {
            if !is_intervention_disposition(legacy.disposition) {
                return Err(LegacyOutboxMigrationError::UnsupportedDisposition {
                    outbox_id: legacy.outbox_id,
                    disposition: legacy.disposition,
                });
            }
            let request = InterventionQueueItemV1::create(
                legacy.episode_id,
                legacy.outcome,
                legacy.disposition,
                legacy.issue_id,
                legacy.issue_status,
                None,
                Some(legacy.outbox_id.clone()),
                legacy.created_at_ms,
            )
            .map_err(LegacyOutboxMigrationError::InterventionQueue)?;
            queue
                .append(&request)
                .await
                .map_err(LegacyOutboxMigrationError::InterventionQueue)?;
            if !self
                .mark_consumed(&legacy.outbox_id)
                .await
                .map_err(LegacyOutboxMigrationError::Outbox)?
            {
                return Err(LegacyOutboxMigrationError::SourceDisappeared(
                    legacy.outbox_id,
                ));
            }
            migrated += 1;
        }
        Ok(migrated)
    }

    /// 读取全部历史未消费记录，保留旧处置以支持迁移。
    async fn unconsumed_items(&self) -> Result<Vec<EvolutionOutboxItem>, OutboxError> {
        let metadata = match fs::symlink_metadata(&self.root).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查 Outbox 目录", &self.root, source)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(OutboxError::UnsafePath {
                path: self.root.clone(),
                reason: "Outbox 根路径必须是非符号链接目录",
            });
        }
        let mut entries = fs::read_dir(&self.root)
            .await
            .map_err(|source| io_error("遍历 Outbox 目录", &self.root, source))?;
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
                    });
                }
                Ok(_) => {}
            }
        }
        items.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.outbox_id.cmp(&right.outbox_id))
        });
        Ok(items)
    }
}

#[async_trait]
impl EvolutionOutbox for FileEvolutionOutbox {
    async fn append(&self, item: &EvolutionOutboxItem) -> Result<(), OutboxError> {
        validate_outbox_id(&item.outbox_id)?;
        if item.disposition != FailureDisposition::EvolutionCandidate {
            return Err(OutboxError::InvalidDisposition(item.disposition));
        }
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
        Ok(self
            .unconsumed_items()
            .await?
            .into_iter()
            .filter(|item| item.disposition == FailureDisposition::EvolutionCandidate)
            .collect())
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
    /// 非进化候选处置不能写入 Evolution Outbox。
    #[error("处置不能写入 Evolution Outbox：{0:?}")]
    InvalidDisposition(FailureDisposition),
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

/// 历史 Evolution Outbox 人工处置迁移错误。
#[derive(Debug, thiserror::Error)]
pub enum LegacyOutboxMigrationError {
    /// Outbox 读取或消费标记失败。
    #[error(transparent)]
    Outbox(OutboxError),
    /// 人工干预请求构造或写入失败。
    #[error(transparent)]
    InterventionQueue(InterventionQueueError),
    /// 旧记录属于 Turn 内或无任务语义，不能迁移到人工队列。
    #[error("旧 Outbox 记录 {outbox_id} 的处置不可迁移：{disposition:?}")]
    UnsupportedDisposition {
        /// 旧 Outbox ID。
        outbox_id: String,
        /// 不可迁移的处置。
        disposition: FailureDisposition,
    },
    /// 人工队列已提交后，来源 Outbox 记录意外消失。
    #[error("旧 Outbox 记录在迁移提交后消失：{0}")]
    SourceDisappeared(String),
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
    use crate::{FileInterventionQueue, InterventionQueue};
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

    /// 直接写入旧版 Fixture，绕过新接口的处置白名单。
    async fn write_legacy_fixture(outbox: &FileEvolutionOutbox, item: &EvolutionOutboxItem) {
        ensure_safe_root(outbox.root()).await.expect("应创建目录");
        let bytes = serde_json::to_vec_pretty(item).expect("旧记录应可序列化");
        fs::write(outbox.item_path(&item.outbox_id), bytes)
            .await
            .expect("应写入旧记录 Fixture");
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

    /// Evolution Outbox 只能接收进化候选，读取时必须隔离历史人工处置。
    #[tokio::test]
    async fn accepts_only_evolution_candidates_and_filters_legacy_records() {
        let root = temp_root();
        let outbox = FileEvolutionOutbox::new(&root);
        let mut legacy = item("legacy-manual", 1);
        legacy.disposition = FailureDisposition::ManualReview;
        assert!(matches!(
            outbox.append(&legacy).await,
            Err(OutboxError::InvalidDisposition(
                FailureDisposition::ManualReview
            ))
        ));

        write_legacy_fixture(&outbox, &legacy).await;
        outbox
            .append(&item("candidate", 2))
            .await
            .expect("候选应写入");

        let pending = outbox.pending().await.expect("应读取候选");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].outbox_id, "candidate");
        let legacy_pending = outbox.legacy_pending().await.expect("应读取旧记录");
        assert_eq!(legacy_pending, vec![legacy]);
        let _ = fs::remove_dir_all(root).await;
    }

    /// 五类历史人工处置必须可重试迁移，且只追加消费标记而不改写来源。
    #[tokio::test]
    async fn migrates_legacy_interventions_idempotently() {
        let root = temp_root();
        let outbox = FileEvolutionOutbox::new(root.join("outbox"));
        let queue = FileInterventionQueue::new(root.join("interventions"));
        let dispositions = [
            FailureDisposition::ManualReview,
            FailureDisposition::PlatformEngineering,
            FailureDisposition::PluginMaintenance,
            FailureDisposition::SecurityIncident,
            FailureDisposition::InfrastructureOperations,
        ];
        let mut originals = Vec::new();
        for (index, disposition) in dispositions.into_iter().enumerate() {
            let mut legacy = item(&format!("legacy-{index}"), index as u64);
            legacy.disposition = disposition;
            write_legacy_fixture(&outbox, &legacy).await;
            originals.push((
                outbox.item_path(&legacy.outbox_id),
                fs::read(outbox.item_path(&legacy.outbox_id))
                    .await
                    .expect("应读取迁移前记录"),
            ));
        }

        assert_eq!(
            outbox
                .migrate_legacy_interventions(&queue)
                .await
                .expect("迁移应成功"),
            5
        );
        assert_eq!(queue.pending().await.expect("应读取人工队列").len(), 5);
        assert!(outbox
            .legacy_pending()
            .await
            .expect("应复读旧记录")
            .is_empty());
        assert_eq!(
            outbox
                .migrate_legacy_interventions(&queue)
                .await
                .expect("重复迁移应幂等"),
            0
        );
        for (path, before) in originals {
            assert_eq!(fs::read(path).await.expect("应复读来源记录"), before);
        }
        let _ = fs::remove_dir_all(root).await;
    }

    /// Turn 内或无任务语义的旧处置必须保留原记录并显式拒绝迁移。
    #[tokio::test]
    async fn rejects_unmigratable_legacy_dispositions() {
        let root = temp_root();
        for disposition in [
            FailureDisposition::Observe,
            FailureDisposition::Ignore,
            FailureDisposition::RetryInTurn,
        ] {
            let suffix = format!("{disposition:?}").to_lowercase();
            let outbox = FileEvolutionOutbox::new(root.join(&suffix).join("outbox"));
            let queue = FileInterventionQueue::new(root.join(&suffix).join("interventions"));
            let mut legacy = item(&format!("legacy-{suffix}"), 1);
            legacy.disposition = disposition;
            write_legacy_fixture(&outbox, &legacy).await;

            assert!(matches!(
                outbox.migrate_legacy_interventions(&queue).await,
                Err(LegacyOutboxMigrationError::UnsupportedDisposition {
                    disposition: actual,
                    ..
                }) if actual == disposition
            ));
            assert_eq!(
                outbox.legacy_pending().await.expect("旧记录应保留").len(),
                1
            );
            assert!(queue.pending().await.expect("人工队列应为空").is_empty());
        }
        let _ = fs::remove_dir_all(root).await;
    }
}
