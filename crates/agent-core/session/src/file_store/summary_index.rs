//! 文件存储的轻量摘要索引与可恢复重建状态机。

use super::{
    ensure_safe_root, io_error, safe_regular_file_metadata, sync_directory, FileSessionStore,
};
use crate::{validate_schema_version, SessionId, SessionStoreError, SessionSummary};
use serde::{de::IgnoredAny, Deserialize, Serialize};
use std::path::PathBuf;
use tokio::{fs, fs::OpenOptions, io::AsyncWriteExt};
use uuid::Uuid;

const SUMMARY_INDEX_FILE_NAME: &str = ".lucia-session-index";
pub(super) const CURRENT_SUMMARY_INDEX_SCHEMA_VERSION: u32 = 1;

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
pub(super) struct StoredSessionSummaryIndex {
    pub(super) schema_version: u32,
    pub(super) summaries: Vec<SessionSummary>,
}

enum StoredSessionSummaryIndexState {
    Valid(Vec<SessionSummary>),
    Missing,
    Invalid,
}

impl FileSessionStore {
    pub(super) fn summary_index_path(&self) -> PathBuf {
        self.root.join(SUMMARY_INDEX_FILE_NAME)
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

    pub(super) async fn load_summary_index_for_update_unlocked(
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

    pub(super) async fn load_or_rebuild_summary_index_unlocked(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let (summaries, index_was_valid) = self.load_summary_index_for_update_unlocked().await?;
        if !index_was_valid {
            self.write_summary_index_unlocked(&summaries).await?;
        }
        Ok(summaries)
    }

    pub(super) async fn invalidate_summary_index_unlocked(&self) -> Result<(), SessionStoreError> {
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

    pub(super) async fn write_summary_index_unlocked(
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
}
