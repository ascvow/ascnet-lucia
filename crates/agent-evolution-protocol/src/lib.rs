//! Lucia 自进化链路的数据处理协议。
//!
//! 本 crate 定义"哪些运行证据可以进入进化流程、以何种形态进入"，见 ADR-0001。
//! 它只包含协议与脱敏实现，**不包含** Hidden Dataset、Verifier 或 Commit Policy。
//!
//! 依赖方向：本 crate 不依赖 `agent-core`，`agent-core` 也不依赖本 crate。
//! Serve 平面不应因为引入进化能力而链接任何变异逻辑。

#![deny(missing_docs)]

pub mod data_class;
pub mod episode;
pub mod evaluation;
pub mod failure;
pub mod genome;
pub mod ids;
pub mod redaction;
pub mod supervision;

pub use genome::{
    AgentGenome, GenomeDigestError, GenomeMetadata, GenomeRevision, GenomeRevisionError,
    InvalidGenome, ModelGenome, PluginGenome, PolicyRef, PromptArtifactRef, PromptGenome,
    PromptLayer, RuntimeIdentity, SkillRef, ToolProfileGenome, GENOME_SCHEMA_VERSION,
};

pub use data_class::{
    DataClass, EpisodeDataPolicy, EpisodeFieldClass, EvolutionEligibility, RawToolResultPolicy,
    RetentionPolicy,
};
pub use episode::{
    ArtifactRef, Episode, EpisodeEvent, EpisodeSupervisionRefs, FailureClassification, FailureKind,
    InvalidEpisode, Outcome, ReplayabilityGrade, TaskDescriptor, UsageSummary,
    EPISODE_SCHEMA_VERSION,
};
pub use evaluation::{
    DatasetKind, EvaluationEnvironment, EvaluationReport, EvaluationRun, EvaluationUsage,
    EvolutionLifecycle, GateDecision, GenomeDiff, InheritanceVerification, InvalidEvaluationReport,
    MutationSurface, SafetyAttemptSummary, TaskAttemptResult, TaskAttemptStatus, TaskCaseMetadata,
    TaskCaseResult, EVALUATION_REPORT_SCHEMA_VERSION,
};
pub use failure::{
    default_disposition, AttributionMethod, DiagnosticStatus, EvolutionIssue, FailureAttribution,
    FailureDisposition, FailureFingerprint, FailureRecord, InvalidFailure,
};
pub use ids::{
    id_json_schema, ArtifactDigest, AuditRecordId, DatasetVersionId, EpisodeId, EvaluationReportId,
    EvaluationRunId, EventId, EvolutionIssueId, FailureRecordId, FeedbackId, GenomeDigest,
    GenomeRevisionId, IncidentId, InvalidEvolutionId, MutationId, OutcomeRevisionId, ReleaseId,
    RunId, SpanId, TaskCaseId,
};
pub use redaction::{RedactionOutcome, RedactionRule, Redactor, REDACTION_RULES_VERSION};
pub use supervision::{
    default_component, default_recoverability, sorted_unique_event_ids, ComponentRef, DetectorRef,
    EventEnvelope, FeedbackEvent, FeedbackSignal, FeedbackSource, Incident, IncidentKind,
    IncidentStatus, InvalidSupervision, OutcomeResolution, OutcomeRevision, OutcomeSource,
    Recoverability, Severity, SUPERVISION_SCHEMA_VERSION,
};
