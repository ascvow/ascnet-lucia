//! Lucia Agent 会话的持久化协议与本地存储实现。
//!
//! 本 crate 只保存与服务商无关的会话，不负责模型配置、密钥、插件状态或 Agent 调度。

mod file_lock;
mod file_store;
mod memory;
mod protocol;

pub use file_store::FileSessionStore;
pub use memory::MemorySessionStore;
pub(crate) use protocol::{
    prepare_saved_record, validate_record, validate_schema_version, verify_revision,
};
pub use protocol::{
    InvalidSessionId, SessionId, SessionRecord, SessionStore, SessionStoreError, SessionSummary,
    CURRENT_SESSION_SCHEMA_VERSION,
};
