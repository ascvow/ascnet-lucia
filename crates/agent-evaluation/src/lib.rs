//! Lucia 的受信离线评测平面。
//!
//! 本 crate 是 Hidden Dataset、Fixture 和最终 Verifier 的行为所有者。普通 TUI、
//! `agent-evolution` 与 Candidate 不依赖本 crate；它们只能读取
//! `agent-evolution-protocol` 中不含任务正文和答案的结果制品。

pub mod archive;
pub mod audit;
pub mod dataset;
pub mod fixture;
pub mod gate;
pub mod health;
pub mod model;
pub mod protocol;
pub mod release;
pub mod report;
pub mod runner;
pub mod verifier;

pub use archive::{
    EvaluationArchiveError, EvaluationRequestBinding, EvaluationSeal, TrustedEvaluationArchive,
    VerifiedEvaluation, EVALUATION_REQUEST_BINDING_SCHEMA_VERSION, EVALUATION_SEAL_SCHEMA_VERSION,
    PREPARED_EVALUATION_SCHEMA_VERSION,
};
pub use audit::{
    AuditEvent, AuditRecord, AuditStoreError, AuditVerification, FileAuditLog,
    AUDIT_RECORD_SCHEMA_VERSION,
};
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
pub use gate::{
    compute_evaluation_metrics, evaluate_commit_gate, CommitGateOutcome, CommitPolicy,
    DatasetComparisonMetrics, EvaluationIntegrity, SafetyGateMetrics, TrustedEvaluationMetrics,
    COMMIT_POLICY_VERSION,
};
pub use health::{
    FileRuntimeHealthObservationStore, ReleaseHealthVerificationError, ReleaseHealthVerifier,
    RuntimeHealthStoreError, VerifiedRuntimeHealthObservation,
    MAX_RUNTIME_HEALTH_OBSERVATION_BYTES,
};
pub use model::{
    ModelExchange, ModelFixture, ModelFixtureInteraction, ModelMock, ModelRequestMatcher,
    RecordingModel, ReplayModel, MODEL_FIXTURE_SCHEMA_VERSION,
};
pub use protocol::{ProtocolDifference, ProtocolTrace, ProtocolTraceEntry, ProtocolTraceError};
pub use release::{ReleaseController, ReleaseError, ReleaseReceipt};
pub use report::{
    evaluation_report_digest, EvaluationReportBuilder, EvaluationReportIdentity,
    EvaluationReportMetadata, ReportBuildError, TrustedEvaluationReport,
};
pub use runner::{
    ComparativeEvaluation, ComparativeRunner, ComparativeRunnerConfig, EvaluationAssurances,
    EvaluationSubject, FixtureReplayReport, RecordedFixtureAttempt, RunnerError,
};
pub use verifier::{
    BuiltinVerifierV1, TrustedVerifier, VerificationResult, VerifierCheck, VerifierRegistry,
    VerifierRule, VERIFIER_RULE_SCHEMA_VERSION,
};
