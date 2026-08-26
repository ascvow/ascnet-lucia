//! Lucia Evolution 证据平面的存储、记录与确定性协议回放。
//!
//! 本 crate 可以由应用层选择性装配。`agent-core` 不依赖本 crate，因此未启用
//! Evolution 时原有 Serve 路径不会改变，也不会链接候选生成或评测逻辑。

#![deny(missing_docs)]

mod aggregation;
mod artifact_store;
mod attribution;
mod episode_store;
mod feedback;
mod genome_store;
mod outbox;
mod outcome_revision;
mod pipeline;
mod recorder;
mod recorder_hub;
mod replay;
mod supervision;

pub use aggregation::IssueAggregator;
pub use artifact_store::{ArtifactStore, ArtifactStoreError, FileArtifactStore};
pub use attribution::attribute_failures;
pub use episode_store::{EpisodeQuery, EpisodeStore, EpisodeStoreError, FileEpisodeStore};
pub use feedback::{FeedbackError, FeedbackProcessor};
pub use genome_store::{FileGenomeStore, GenomeStore, GenomeStoreError};
pub use outbox::{EvolutionOutbox, EvolutionOutboxItem, FileEvolutionOutbox, OutboxError};
pub use outcome_revision::{FileOutcomeRevisionStore, OutcomeRevisionError, OutcomeRevisionStore};
pub use pipeline::{EvolutionPipeline, PipelineError};
pub use recorder::{EpisodeRecorder, EpisodeRecorderConfig, EpisodeRecorderError};
pub use recorder_hub::{EpisodeRecorderHub, EpisodeRecorderHubError, RegisteredEpisodeRun};
pub use replay::{ProtocolReplay, ProtocolReplayError, ReplayEventSink, ReplayReport};
pub use supervision::{RunSupervisor, SupervisionError, SupervisionReport};
