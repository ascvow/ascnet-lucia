//! Lucia 自进化链路的数据处理协议。
//!
//! 本 crate 定义"哪些运行证据可以进入进化流程、以何种形态进入"，见 ADR-0001。
//! 它只包含协议与脱敏实现，**不包含** Hidden Dataset、Verifier 或 Commit Policy。
//!
//! 依赖方向：本 crate 不依赖 `agent-core`，`agent-core` 也不依赖本 crate。
//! Serve 平面不应因为引入进化能力而链接任何变异逻辑。

#![deny(missing_docs)]

pub mod data_class;
pub mod redaction;

pub use data_class::{
    DataClass, EpisodeDataPolicy, EpisodeFieldClass, EvolutionEligibility, RawToolResultPolicy,
    RetentionPolicy,
};
pub use redaction::{RedactionOutcome, RedactionRule, Redactor, REDACTION_RULES_VERSION};
