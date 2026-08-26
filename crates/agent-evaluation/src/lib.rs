//! Lucia 的受信离线评测平面。
//!
//! 本 crate 是 Hidden Dataset、Fixture 和最终 Verifier 的行为所有者。普通 TUI、
//! `agent-evolution` 与 Candidate 不依赖本 crate；它们只能读取
//! `agent-evolution-protocol` 中不含任务正文和答案的结果制品。

pub mod dataset;
pub mod fixture;
pub mod model;
pub mod protocol;
pub mod runner;
pub mod verifier;

pub use dataset::{
    DatasetCaseRef, DatasetError, DatasetManifest, DatasetVisibility, MutatorDatasetView,
    TaskBudgets, TaskCase, TaskInput, TrustedArtifactRef, TrustedDataset, TrustedDatasetStore,
    VisibleTaskCase, DATASET_MANIFEST_SCHEMA_VERSION, TASK_CASE_SCHEMA_VERSION,
};
pub use fixture::{
    EnvironmentFile, EnvironmentFixture, FixtureCallRecord, FixtureError, ToolFixture,
    ToolFixtureInteraction, ToolFixtureRuntime, ToolResultTemplate,
    ENVIRONMENT_FIXTURE_SCHEMA_VERSION, TOOL_FIXTURE_SCHEMA_VERSION,
};
pub use model::{
    ModelExchange, ModelFixture, ModelFixtureInteraction, ModelMock, ModelRequestMatcher,
    RecordingModel, ReplayModel, MODEL_FIXTURE_SCHEMA_VERSION,
};
pub use protocol::{ProtocolDifference, ProtocolTrace, ProtocolTraceEntry, ProtocolTraceError};
pub use runner::{
    ComparativeEvaluation, ComparativeRunner, ComparativeRunnerConfig, EvaluationSubject,
    FixtureReplayReport, RecordedFixtureAttempt, RunnerError,
};
pub use verifier::{VerificationResult, VerifierCheck, VerifierRule, VERIFIER_RULE_SCHEMA_VERSION};
