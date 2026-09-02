//! 把 TUI 的会话存储适配为插件宿主的只读会话来源。
//!
//! 适配器只暴露列举与读取：宿主拿到的端口没有 `save` 与 `delete`，因此插件路径
//! 不存在写入历史会话的可能，这一点由类型而不是纪律保证。

use agent_core::session::Session;
use agent_plugin_host::session::{PluginSessionDescriptor, PluginSessionSource};
use agent_session::{SessionId, SessionStore};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// 基于 [`SessionStore`] 的只读会话来源。
pub(crate) struct TuiPluginSessionSource {
    store: Arc<dyn SessionStore>,
}

impl TuiPluginSessionSource {
    /// 用当前项目的会话存储创建只读来源。
    pub(crate) fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl PluginSessionSource for TuiPluginSessionSource {
    async fn list_sessions(&self) -> Result<Vec<PluginSessionDescriptor>> {
        let summaries = self
            .store
            .list_summaries()
            .await
            .context("列出会话摘要失败")?;
        Ok(summaries
            .into_iter()
            .map(|summary| PluginSessionDescriptor {
                id: summary.id.as_str().to_string(),
                title: summary.title,
                created_at_ms: summary.created_at_ms,
                updated_at_ms: summary.updated_at_ms,
                revision: summary.revision,
                message_count: summary.message_count,
            })
            .collect())
    }

    async fn load_session(&self, id: &str) -> Result<Option<Session>> {
        // 非法标识按"不存在"处理：插件传入的 ID 不可信，不应当据此区分
        // 「格式非法」与「确实不存在」，那会把存储布局透给调用方。
        let Ok(session_id) = SessionId::new(id) else {
            return Ok(None);
        };
        let record = self
            .store
            .load(&session_id)
            .await
            .with_context(|| format!("读取会话 `{id}` 失败"))?;
        Ok(record.map(|record| record.session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::model::MessageRole;
    use agent_core::model::ModelMessage;
    use agent_session::{MemorySessionStore, SessionRecord};

    /// 构造带一条消息的会话存储。
    async fn store_with_session() -> (Arc<dyn SessionStore>, String) {
        let store = Arc::new(MemorySessionStore::default());
        let id = SessionId::generate();
        let session = Session::from_parts(
            None,
            vec![ModelMessage::text(MessageRole::User, "历史提问")],
        );
        let record = SessionRecord::new(id.clone(), session).expect("创建会话记录");
        store.save(record, None).await.expect("保存会话");
        (store as Arc<dyn SessionStore>, id.as_str().to_string())
    }

    /// 列举与读取都要能拿到真实内容。
    #[tokio::test]
    async fn lists_and_loads_persisted_sessions() {
        let (store, id) = store_with_session().await;
        let source = TuiPluginSessionSource::new(store);

        let listed = source.list_sessions().await.expect("列出会话");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].message_count, 1);

        let session = source.load_session(&id).await.expect("读取会话");
        assert!(session.is_some());
    }

    /// 非法或不存在的标识都返回 `None`，不区分两者也不泄漏存储布局。
    #[tokio::test]
    async fn unknown_or_invalid_id_reads_as_missing() {
        let (store, _) = store_with_session().await;
        let source = TuiPluginSessionSource::new(store);

        assert!(source
            .load_session("../../etc/passwd")
            .await
            .expect("非法标识不应报错")
            .is_none());
        assert!(source
            .load_session(SessionId::generate().as_str())
            .await
            .expect("未知标识不应报错")
            .is_none());
    }
}
