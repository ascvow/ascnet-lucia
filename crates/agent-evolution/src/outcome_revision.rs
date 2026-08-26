//! Episode 终态的只追加修订存储。
//!
//! 原 Episode Header 中的 `outcome` 字段永不修改；延迟反馈与后续判定通过
//! [`OutcomeRevisionStore::append`] 追加修订，`supersedes` 指向前一条，
//! 形成完整历史链。

use agent_evolution_protocol::{EpisodeId, OutcomeRevision, OutcomeRevisionId};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// 只追加 Outcome 修订存储接口。
#[async_trait]
pub trait OutcomeRevisionStore: Send + Sync {
    /// 追加一条修订；`supersedes` 为 `None` 时必须是该 Episode 的首条修订。
    ///
    /// # Errors
    ///
    /// 修订不合法、ID 已存在、前序修订缺失或 I/O 失败时返回错误。
    async fn append(&self, revision: &OutcomeRevision) -> Result<(), OutcomeRevisionError>;

    /// 返回指定 Episode 的全部修订，按追加顺序排列。
    ///
    /// # Errors
    ///
    /// 记录损坏或 I/O 失败时返回错误。
    async fn history(
        &self,
        episode_id: &EpisodeId,
    ) -> Result<Vec<OutcomeRevision>, OutcomeRevisionError>;

    /// 返回指定 Episode 的最新修订；无修订时为 `None`。
    ///
    /// # Errors
    ///
    /// 记录损坏或 I/O 失败时返回错误。
    async fn current(
        &self,
        episode_id: &EpisodeId,
    ) -> Result<Option<OutcomeRevision>, OutcomeRevisionError> {
        Ok(self.history(episode_id).await?.into_iter().last())
    }
}

/// 基于 JSON 文件的本地 Outcome 修订存储。
///
/// 每个 Episode 一个目录，目录内按单调序号 `00000000000000000001.json` 保存。
/// 固定序号文件名同时承担跨进程 CAS：两个写入者竞争同一后继序号时只有一个能提交。
#[derive(Debug, Clone)]
pub struct FileOutcomeRevisionStore {
    root: PathBuf,
}

impl FileOutcomeRevisionStore {
    /// 打开指定根目录；目录在首次追加时创建。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn episode_dir(&self, episode_id: &EpisodeId) -> PathBuf {
        self.root.join(episode_id.as_str())
    }
}

#[async_trait]
impl OutcomeRevisionStore for FileOutcomeRevisionStore {
    async fn append(&self, revision: &OutcomeRevision) -> Result<(), OutcomeRevisionError> {
        revision
            .validate()
            .map_err(|error| OutcomeRevisionError::InvalidRevision(error.to_string()))?;
        let dir = self.episode_dir(&revision.episode_id);
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&dir).await?;

        let history = self.history(&revision.episode_id).await?;
        if history
            .iter()
            .any(|existing| existing.revision_id == revision.revision_id)
        {
            return Err(OutcomeRevisionError::AlreadyExists(
                revision.revision_id.clone(),
            ));
        }
        match (&revision.supersedes, history.last()) {
            (None, None) => {}
            (Some(expected), Some(latest)) if expected == &latest.revision_id => {}
            (None, Some(_)) => {
                return Err(OutcomeRevisionError::MissingSupersedes(
                    revision.episode_id.clone(),
                ))
            }
            (Some(expected), _) => {
                return Err(OutcomeRevisionError::StaleSupersedes {
                    episode_id: revision.episode_id.clone(),
                    expected: expected.clone(),
                })
            }
        }

        let sequence = history.len() as u64 + 1;
        let path = dir.join(format!("{sequence:020}.json"));
        let bytes = serde_json::to_vec_pretty(revision)
            .map_err(|source| OutcomeRevisionError::Serialization { source })?;
        let temporary = dir.join(format!(".{}.tmp", Uuid::new_v4().simple()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|source| io_error("创建 Outcome 修订临时文件", &temporary, source))?;
        file.write_all(&bytes)
            .await
            .map_err(|source| io_error("写入 Outcome 修订临时文件", &temporary, source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("同步 Outcome 修订临时文件", &temporary, source))?;
        drop(file);
        let linked = fs::hard_link(&temporary, &path).await;
        let _ = fs::remove_file(&temporary).await;
        match linked {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Err(
                OutcomeRevisionError::ConcurrentUpdate(revision.episode_id.clone()),
            ),
            Err(source) => Err(io_error("提交 Outcome 修订文件", &path, source)),
        }
    }

    async fn history(
        &self,
        episode_id: &EpisodeId,
    ) -> Result<Vec<OutcomeRevision>, OutcomeRevisionError> {
        let dir = self.episode_dir(episode_id);
        match fs::symlink_metadata(&dir).await {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查 Outcome 修订目录", &dir, source)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(OutcomeRevisionError::UnsafePath {
                    path: dir,
                    reason: "Outcome 修订目录必须是非符号链接目录",
                })
            }
            Ok(_) => {}
        }
        let mut entries = match fs::read_dir(&dir).await {
            Err(source) => return Err(io_error("遍历 Outcome 修订目录", &dir, source)),
            Ok(entries) => entries,
        };

        let mut records = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| io_error("读取 Outcome 修订目录项", &dir, source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(|source| io_error("检查 Outcome 修订文件", &path, source))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(OutcomeRevisionError::UnsafePath {
                    path,
                    reason: "Outcome 修订必须是非符号链接普通文件",
                });
            }
            let file_name = path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| OutcomeRevisionError::UnsafePath {
                    path: path.clone(),
                    reason: "Outcome 修订文件名必须是 UTF-8",
                })?
                .to_string();
            let bytes = fs::read(&path)
                .await
                .map_err(|source| io_error("读取 Outcome 修订文件", &path, source))?;
            let revision: OutcomeRevision = serde_json::from_slice(&bytes).map_err(|source| {
                OutcomeRevisionError::InvalidRecord {
                    path: path.clone(),
                    source,
                }
            })?;
            revision
                .validate()
                .map_err(|error| OutcomeRevisionError::InvalidRevision(error.to_string()))?;
            if revision.episode_id != *episode_id {
                return Err(OutcomeRevisionError::IdMismatch { path });
            }
            records.push((file_name, revision));
        }
        // revision_id 是随机标识，不能代表追加顺序；文件名前缀才是持久化序号。
        records.sort_by(|left, right| left.0.cmp(&right.0));
        let revisions = records
            .into_iter()
            .map(|(_, revision)| revision)
            .collect::<Vec<_>>();
        for (index, revision) in revisions.iter().enumerate() {
            let expected = index
                .checked_sub(1)
                .and_then(|previous| revisions.get(previous))
                .map(|previous| &previous.revision_id);
            if revision.supersedes.as_ref() != expected {
                return Err(OutcomeRevisionError::BrokenHistory {
                    episode_id: episode_id.clone(),
                    revision_id: revision.revision_id.clone(),
                });
            }
        }
        Ok(revisions)
    }
}

/// Outcome 修订存储错误。
#[derive(Debug, thiserror::Error)]
pub enum OutcomeRevisionError {
    /// 修订结构不合法。
    #[error("Outcome 修订不合法：{0}")]
    InvalidRevision(String),
    /// 同一 ID 已存在。
    #[error("Outcome 修订已存在：{0}")]
    AlreadyExists(OutcomeRevisionId),
    /// 另一写入者已提交同一前序的后继修订。
    #[error("Episode {0} 的 Outcome 修订发生并发冲突，请重新读取最新修订")]
    ConcurrentUpdate(EpisodeId),
    /// 该 Episode 已有历史，新修订必须指向前一条。
    #[error("Episode {0} 已有 Outcome 修订历史，新修订必须填写 supersedes")]
    MissingSupersedes(EpisodeId),
    /// 声称的前序修订与实际最新修订不一致。
    #[error("Episode {episode_id} 的 supersedes 过期：{expected}")]
    StaleSupersedes {
        /// Episode ID。
        episode_id: EpisodeId,
        /// 调用方声称的前序修订。
        expected: OutcomeRevisionId,
    },
    /// 文件名 ID 与记录内 Episode 不一致。
    #[error("Outcome 修订记录与目录不一致：{path}")]
    IdMismatch {
        /// 错误文件路径。
        path: PathBuf,
    },
    /// 历史链的 supersedes 与文件追加顺序不一致。
    #[error("Episode {episode_id} 的 Outcome 修订链断裂于 {revision_id}")]
    BrokenHistory {
        /// 所属 Episode。
        episode_id: EpisodeId,
        /// 首条无法接续前序的修订。
        revision_id: OutcomeRevisionId,
    },
    /// 路径包含非 UTF-8 文件名或不符合存储约束。
    #[error("Outcome 修订路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 错误路径。
        path: PathBuf,
        /// 稳定原因。
        reason: &'static str,
    },
    /// JSON 编码失败。
    #[error("序列化 Outcome 修订失败：{source}")]
    Serialization {
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// JSON 记录损坏。
    #[error("Outcome 修订记录损坏：{path}: {source}")]
    InvalidRecord {
        /// 损坏文件路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
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

fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> OutcomeRevisionError {
    OutcomeRevisionError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

/// 创建并验证存储目录自身，拒绝符号链接替换。
async fn ensure_safe_directory(path: &Path) -> Result<(), OutcomeRevisionError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 Outcome 修订目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Outcome 修订目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(OutcomeRevisionError::UnsafePath {
            path: path.to_path_buf(),
            reason: "Outcome 修订目录必须是非符号链接目录",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{Outcome, OutcomeSource};
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-outcomes-{}", Uuid::new_v4().simple()))
    }

    fn revision(
        episode_id: &EpisodeId,
        supersedes: Option<OutcomeRevisionId>,
        outcome: Outcome,
    ) -> OutcomeRevision {
        OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: episode_id.clone(),
            supersedes,
            outcome,
            source: OutcomeSource::DeterministicRule,
            reason: "测试修订".into(),
        }
    }

    #[tokio::test]
    async fn appends_revisions_and_keeps_history() {
        let root = temp_root();
        let store = FileOutcomeRevisionStore::new(&root);
        let episode_id = EpisodeId::generate();

        let first = revision(&episode_id, None, Outcome::Unverifiable);
        store.append(&first).await.expect("应追加首条");

        let second = revision(
            &episode_id,
            Some(first.revision_id.clone()),
            Outcome::TaskFailure,
        );
        store.append(&second).await.expect("应追加修订");

        let history = store.history(&episode_id).await.expect("应读取历史");
        assert_eq!(history.len(), 2);
        let current = store.current(&episode_id).await.expect("应读取当前");
        assert_eq!(current, Some(second.clone()));

        // 跳过前序的修订必须被拒绝。
        let stale = revision(
            &episode_id,
            Some(first.revision_id.clone()),
            Outcome::Success,
        );
        assert!(matches!(
            store.append(&stale).await,
            Err(OutcomeRevisionError::StaleSupersedes { .. })
        ));

        // 随机 Revision ID 不能在新的序号位置重复使用。
        let duplicate_id = OutcomeRevision {
            revision_id: second.revision_id.clone(),
            episode_id: episode_id.clone(),
            supersedes: Some(second.revision_id.clone()),
            outcome: Outcome::Cancelled,
            source: OutcomeSource::DeterministicRule,
            reason: "重复标识测试".into(),
        };
        assert!(matches!(
            store.append(&duplicate_id).await,
            Err(OutcomeRevisionError::AlreadyExists(_))
        ));
        let _ = fs::remove_dir_all(root).await;
    }

    /// 验证两个并发写入者不能同时提交同一前序的后继修订。
    #[tokio::test]
    async fn concurrent_successors_allow_only_one_commit() {
        let root = temp_root();
        let store = FileOutcomeRevisionStore::new(&root);
        let episode_id = EpisodeId::generate();
        let first = revision(&episode_id, None, Outcome::Unverifiable);
        store.append(&first).await.expect("应追加首条");

        let left = revision(
            &episode_id,
            Some(first.revision_id.clone()),
            Outcome::TaskFailure,
        );
        let right = revision(
            &episode_id,
            Some(first.revision_id.clone()),
            Outcome::Cancelled,
        );
        let (left_result, right_result) = tokio::join!(store.append(&left), store.append(&right));
        assert_eq!(
            usize::from(left_result.is_ok()) + usize::from(right_result.is_ok()),
            1
        );
        let rejected = if left_result.is_err() {
            left_result.expect_err("左侧应被拒绝")
        } else {
            right_result.expect_err("右侧应被拒绝")
        };
        assert!(matches!(
            rejected,
            OutcomeRevisionError::ConcurrentUpdate(_)
                | OutcomeRevisionError::StaleSupersedes { .. }
        ));
        assert_eq!(
            store.history(&episode_id).await.expect("应读取历史").len(),
            2
        );
        let _ = fs::remove_dir_all(root).await;
    }
}
