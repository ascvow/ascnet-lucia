//! 短生命周期与测试场景使用的内存会话存储。

use crate::{
    prepare_saved_record, verify_revision, SessionId, SessionRecord, SessionStore,
    SessionStoreError, SessionSummary,
};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

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
