//! codesearch：供 Coding Agent 使用的本地代码语义搜索。
//!
//! 索引流水线：工作目录 → 扫描/监听/变更合并 → 源码快照与内容 Hash → 语言解析 →
//! 结构化分块 → SQLite → Embedding 缓存与生成 → 向量更新日志 → USearch → 持久化检查点。
//!
//! 查询流水线：查询分析与作用域校验 → 符号/路径/字面量 + FTS5 + USearch 三路召回 →
//! 当前版本校验 → RRF 合并去重 → 必要上下文扩展 → 长度预算 → 带版本和出处的代码结果。
//!
//! 职责边界：源码文件是原始事实来源；SQLite 是已采集快照与索引状态的权威记录；
//! USearch 是可重建的派生索引；[`context`] 只选择和组织代码，不生成解释。
//! CLI 与 MCP 共用 [`engine::Engine`]，不复制检索逻辑。

pub mod api;
pub mod chunker;
pub mod config;
pub mod context;
pub mod engine;
pub mod error;
pub mod indexer;
pub mod models;
pub mod parser;
pub mod retrieval;
pub mod store;
pub mod types;
pub mod vector;
pub mod workspace;

pub use engine::{AccessMode, Engine, EngineOptions};
pub use error::{Error, Result};
