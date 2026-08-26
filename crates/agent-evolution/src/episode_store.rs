//! 只追加的本地 Episode Header 存储。

use agent_evolution_protocol::{Episode, EpisodeId, Outcome};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};

/// Episode 查询条件；所有已填写条件采用 AND 语义。
#[derive(Debug, Clone, Default)]
pub struct EpisodeQuery {
    /// 只返回指定终态。
    pub outcome: Option<Outcome>,
    /// 只返回指定会话。
    pub session_id: Option<String>,
}

/// 只追加 Episode 存储接口。
#[async_trait]
pub trait EpisodeStore: Send + Sync {
    /// 新增一条 Episode；同一 ID 已存在时必须拒绝，禁止覆盖证据。
    ///
    /// # Errors
    ///
    /// Episode 不合法、ID 已存在、路径不安全或 I/O 失败时返回错误。
    async fn append(&self, episode: &Episode) -> Result<(), EpisodeStoreError>;

    /// 按 ID 读取 Episode，不存在时返回 `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 记录损坏、路径不安全或 I/O 失败时返回错误。
    async fn get(&self, id: &EpisodeId) -> Result<Option<Episode>, EpisodeStoreError>;

    /// 查询 Episode，结果按开始时间和 ID 排序。
    ///
    /// # Errors
    ///
    /// 任一记录损坏、路径不安全或 I/O 失败时返回错误。
    async fn query(&self, query: &EpisodeQuery) -> Result<Vec<Episode>, EpisodeStoreError>;
}

/// 基于 JSON 文件的本地只追加 Episode Store。
#[derive(Debug, Clone)]
pub struct FileEpisodeStore {
    root: PathBuf,
}

impl FileEpisodeStore {
    /// 打开指定根目录；目录在首次追加时创建。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回单条 Episode 的固定文件路径。
    fn episode_path(&self, id: &EpisodeId) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
}

#[async_trait]
impl EpisodeStore for FileEpisodeStore {
    async fn append(&self, episode: &Episode) -> Result<(), EpisodeStoreError> {
        episode
            .validate()
            .map_err(|error| EpisodeStoreError::InvalidEpisode(error.to_string()))?;
        ensure_safe_root(&self.root).await?;
        let path = self.episode_path(&episode.episode_id);
        match fs::symlink_metadata(&path).await {
            Ok(_) => return Err(EpisodeStoreError::AlreadyExists(episode.episode_id.clone())),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("检查 Episode 文件", &path, source)),
        }

        let bytes = serde_json::to_vec_pretty(episode)
            .map_err(|source| EpisodeStoreError::Serialization { source })?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    EpisodeStoreError::AlreadyExists(episode.episode_id.clone())
                } else {
                    io_error("创建 Episode 文件", &path, source)
                }
            })?;
        file.write_all(&bytes)
            .await
            .map_err(|source| io_error("写入 Episode 文件", &path, source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("同步 Episode 文件", &path, source))
    }

    async fn get(&self, id: &EpisodeId) -> Result<Option<Episode>, EpisodeStoreError> {
        read_episode(&self.episode_path(id), Some(id)).await
    }

    async fn query(&self, query: &EpisodeQuery) -> Result<Vec<Episode>, EpisodeStoreError> {
        match fs::symlink_metadata(&self.root).await {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查 Episode 目录", &self.root, source)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(EpisodeStoreError::UnsafePath {
                    path: self.root.clone(),
                    reason: "Episode 根路径必须是非符号链接目录",
                })
            }
            Ok(_) => {}
        }

        let mut directory = fs::read_dir(&self.root)
            .await
            .map_err(|source| io_error("遍历 Episode 目录", &self.root, source))?;
        let mut episodes = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|source| io_error("读取 Episode 目录项", &self.root, source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let episode =
                read_episode(&path, None)
                    .await?
                    .ok_or_else(|| EpisodeStoreError::UnsafePath {
                        path: path.clone(),
                        reason: "遍历期间 Episode 文件被移除",
                    })?;
            if query
                .outcome
                .as_ref()
                .is_some_and(|outcome| episode.outcome.as_ref() != Some(outcome))
            {
                continue;
            }
            if query
                .session_id
                .as_ref()
                .is_some_and(|session_id| &episode.session_id != session_id)
            {
                continue;
            }
            episodes.push(episode);
        }
        episodes.sort_by(|left, right| {
            left.started_at_ms
                .cmp(&right.started_at_ms)
                .then_with(|| left.episode_id.cmp(&right.episode_id))
        });
        Ok(episodes)
    }
}

/// Episode Store 错误。
#[derive(Debug, thiserror::Error)]
pub enum EpisodeStoreError {
    /// Episode 结构不合法。
    #[error("Episode 不合法：{0}")]
    InvalidEpisode(String),
    /// 同一 ID 已存在，不能覆盖。
    #[error("Episode 已存在，禁止覆盖：{0}")]
    AlreadyExists(EpisodeId),
    /// 路径包含符号链接或不是预期类型。
    #[error("Episode 存储路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 稳定原因。
        reason: &'static str,
    },
    /// JSON 编码失败。
    #[error("序列化 Episode 失败：{source}")]
    Serialization {
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// JSON 记录损坏。
    #[error("Episode 记录损坏：{path}: {source}")]
    InvalidRecord {
        /// 损坏文件路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 文件名 ID 与记录内 ID 不一致。
    #[error("Episode 文件名与记录 ID 不一致：{path}")]
    IdMismatch {
        /// 错误文件路径。
        path: PathBuf,
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

/// 创建并校验 Episode 根目录。
async fn ensure_safe_root(root: &Path) -> Result<(), EpisodeStoreError> {
    fs::create_dir_all(root)
        .await
        .map_err(|source| io_error("创建 Episode 目录", root, source))?;
    let metadata = fs::symlink_metadata(root)
        .await
        .map_err(|source| io_error("检查 Episode 目录", root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EpisodeStoreError::UnsafePath {
            path: root.to_path_buf(),
            reason: "Episode 根路径必须是非符号链接目录",
        });
    }
    Ok(())
}

/// 读取并校验单条 Episode。
async fn read_episode(
    path: &Path,
    expected_id: Option<&EpisodeId>,
) -> Result<Option<Episode>, EpisodeStoreError> {
    match fs::symlink_metadata(path).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("检查 Episode 文件", path, source)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(EpisodeStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "Episode 必须是非符号链接普通文件",
            })
        }
        Ok(_) => {}
    }
    let bytes = fs::read(path)
        .await
        .map_err(|source| io_error("读取 Episode 文件", path, source))?;
    let episode: Episode =
        serde_json::from_slice(&bytes).map_err(|source| EpisodeStoreError::InvalidRecord {
            path: path.to_path_buf(),
            source,
        })?;
    episode
        .validate()
        .map_err(|error| EpisodeStoreError::InvalidEpisode(error.to_string()))?;
    if expected_id.is_some_and(|id| id != &episode.episode_id) {
        return Err(EpisodeStoreError::IdMismatch {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(episode))
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> EpisodeStoreError {
    EpisodeStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        ArtifactDigest, ArtifactRef, EpisodeDataPolicy, GenomeRevisionId, ReplayabilityGrade,
        RunId, TaskDescriptor, UsageSummary, EPISODE_SCHEMA_VERSION,
    };
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-episodes-{}", Uuid::new_v4().simple()))
    }

    fn episode(outcome: Outcome, started_at_ms: u64) -> Episode {
        Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: EpisodeId::generate(),
            run_id: RunId::generate(),
            session_id: "session-1".into(),
            genome_revision_id: GenomeRevisionId::generate(),
            task: TaskDescriptor::default(),
            event_stream_ref: ArtifactRef {
                digest: ArtifactDigest::from_sha256_hex("1".repeat(64)).expect("摘要应合法"),
                media_type: "application/x-ndjson".into(),
                size_bytes: 1,
            },
            supervision: None,
            environment_ref: None,
            outcome: Some(outcome),
            failures: Vec::new(),
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
            data_policy: EpisodeDataPolicy::default(),
            event_count: 1,
            started_at_ms,
            finished_at_ms: started_at_ms,
        }
    }

    #[tokio::test]
    async fn appends_queries_and_never_overwrites() {
        let root = temp_root();
        let store = FileEpisodeStore::new(&root);
        let success = episode(Outcome::Success, 2);
        let failure = episode(Outcome::TaskFailure, 1);
        store.append(&success).await.expect("应追加成功记录");
        store.append(&failure).await.expect("应追加失败记录");
        assert!(matches!(
            store.append(&success).await,
            Err(EpisodeStoreError::AlreadyExists(_))
        ));
        let failures = store
            .query(&EpisodeQuery {
                outcome: Some(Outcome::TaskFailure),
                session_id: None,
            })
            .await
            .expect("应查询");
        assert_eq!(failures, vec![failure]);
        let _ = fs::remove_dir_all(root).await;
    }
}
