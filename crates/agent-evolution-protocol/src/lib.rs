//! Lucia 自进化链路的数据处理协议。
//!
//! 本 crate 定义"哪些运行证据可以进入进化流程、以何种形态进入"，见 ADR-0001。
//! 它只包含协议与脱敏实现，**不包含** Hidden Dataset、Verifier 或 Commit Policy。
//!
//! 依赖方向：本 crate 不依赖 `agent-core`，`agent-core` 也不依赖本 crate。
//! Serve 平面不应因为引入进化能力而链接任何变异逻辑。

#![deny(missing_docs)]

pub mod context;
pub mod data_class;
pub mod episode;
pub mod evaluation;
pub mod failure;
pub mod genome;
pub mod ids;
pub mod ipc;
pub mod mutation;
pub mod redaction;
pub mod supervision;

pub use genome::{
    AgentGenome, GenomeDigestError, GenomeMetadata, GenomeRevision, GenomeRevisionError,
    InvalidGenome, ModelGenome, PluginGenome, PolicyRef, PromptArtifactRef, PromptGenome,
    PromptLayer, RuntimeIdentity, SkillRef, ToolProfileGenome, GENOME_SCHEMA_VERSION,
};

pub use context::{
    ContextEvaluationMetricsV1, ContextEvaluationObservationV1, ContextGateFailureV1,
    ContextPolicyCandidateV1, ContextPolicyEvaluationReportV1, ContextPolicyMutationProposalV1,
    ContextPolicyV1, InvalidContextEvaluation, InvalidContextMutation, InvalidContextPolicy,
    PlanSnapshotRetentionPolicyV1, PostSummaryValidationAlgorithmV1, RecallObservationV1,
    ToolResultRetentionPolicyV1, UserConstraintRetentionPolicyV1,
    CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION, CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION,
    CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION, CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION,
    CONTEXT_POLICY_SCHEMA_VERSION, MAX_CONTEXT_HYPOTHESIS_BYTES, MAX_CONTEXT_THRESHOLD_TOKENS,
    MAX_PINNED_ITEM_COUNT, MAX_RECENT_MESSAGE_COUNT, MAX_RECENT_TOOL_RESULT_COUNT,
    MAX_SUMMARY_TOKEN_BUDGET, MIN_CONTEXT_THRESHOLD_TOKENS, MIN_SUMMARY_TOKEN_BUDGET,
    MIN_SUMMARY_VALIDATION_COVERAGE_BPS,
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
    id_json_schema, ArtifactDigest, AuditRecordId, CandidateId, DatasetVersionId, EpisodeId,
    EvaluationReportId, EvaluationRunId, EventId, EvolutionCycleId, EvolutionIssueId,
    FailureRecordId, FeedbackId, GenomeDigest, GenomeRevisionId, IncidentId, InvalidEvolutionId,
    MutationId, OutcomeRevisionId, ReleaseId, RunId, SpanId, TaskCaseId,
};
pub use ipc::{
    EvaluationReceiptV1, EvaluationRequestV1, HealthCheckReceiptV1, HealthCheckRequestV1,
    InvalidEvaluatorIpc, PromotionRequestV1, ReleaseReceiptV1, RollbackRequestV1,
    RuntimeHealthObservationV1, EVALUATION_RECEIPT_SCHEMA_VERSION,
    EVALUATION_REQUEST_SCHEMA_VERSION, HEALTH_RECEIPT_SCHEMA_VERSION,
    RELEASE_RECEIPT_SCHEMA_VERSION,
};
pub use mutation::{
    EvolutionCycleRequestInput, EvolutionCycleRequestV1, EvolutionCycleSnapshotV1,
    EvolutionCycleStage, ExpectedEffect, InvalidEvolutionCycle, InvalidMutation, MutationCandidate,
    MutationPatch, MutationProposal, MutationRisk, EVOLUTION_CYCLE_SCHEMA_VERSION,
    MAX_CANDIDATES_PER_CYCLE, MIN_CANDIDATES_PER_CYCLE, MUTATION_CANDIDATE_SCHEMA_VERSION,
    MUTATION_PROPOSAL_SCHEMA_VERSION,
};
pub use redaction::{RedactionOutcome, RedactionRule, Redactor, REDACTION_RULES_VERSION};
pub use supervision::{
    default_component, default_recoverability, sorted_unique_event_ids, ComponentRef, DetectorRef,
    EventEnvelope, FeedbackEvent, FeedbackSignal, FeedbackSource, Incident, IncidentKind,
    IncidentStatus, InvalidSupervision, OutcomeResolution, OutcomeRevision, OutcomeSource,
    Recoverability, Severity, SUPERVISION_SCHEMA_VERSION,
};
