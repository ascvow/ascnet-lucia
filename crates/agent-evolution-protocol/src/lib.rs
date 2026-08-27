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
mod plugin_evolution;
pub mod redaction;
mod skill_evolution;
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
pub use plugin_evolution::{
    CapabilityExpansionRequest, CapabilityProfile, ComponentInterfaceSnapshot,
    InvalidPluginEvolution, PluginApprovalDecision, PluginApprovalRecord, PluginAuditCheck,
    PluginBuildAttestation, PluginCanaryRecord, PluginCanaryState, PluginCapabilitySet,
    PluginEvaluationEvidence, PluginEvaluationGateInput, PluginEvaluationKind,
    PluginEvaluationReport, PluginFilePatch, PluginHostAuditEvidence, PluginMutationKind,
    PluginMutationProposal, PluginReleaseEnvelope, PluginReleaseStage, PluginSourceArtifact,
    PluginSourceFile, PluginSourceGateDecision, PluginSourceGateFailure, PreapprovedPluginProfile,
    SignatureAlgorithm, SignatureEnvelope, SignaturePurpose,
    CAPABILITY_EXPANSION_REQUEST_SCHEMA_VERSION, COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
    PLUGIN_APPROVAL_RECORD_SCHEMA_VERSION, PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
    PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION, PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
    PLUGIN_CAPABILITY_PROFILE_SCHEMA_VERSION, PLUGIN_CAPABILITY_SET_SCHEMA_VERSION,
    PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION, PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
    PLUGIN_EVALUATION_REPORT_SCHEMA_VERSION, PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
    PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION, PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
    PLUGIN_SOURCE_ARTIFACT_SCHEMA_VERSION, SIGNATURE_ENVELOPE_SCHEMA_VERSION,
};
pub use redaction::{RedactionOutcome, RedactionRule, Redactor, REDACTION_RULES_VERSION};
pub use skill_evolution::{
    InvalidSkillEvolution, InvalidSkillId, SkillArtifactV1, SkillCandidateV1, SkillDeletionModeV1,
    SkillEvaluationReportV1, SkillGateFailureV1, SkillGenomeRefV1, SkillId,
    SkillMutationProposalV1, SkillOperationV1, SkillStatusTransitionV1, SkillStatusV1,
    SkillTriggerModeV1, SkillTriggerPolicyV1, SkillUsageEvidenceSourceV1, SkillUsageObservationV1,
    SkillUsageResultV1, TrustedPluginEventRefV1, TrustedSkillUsageBindingV1,
    SKILL_ARTIFACT_SCHEMA_VERSION, SKILL_CANDIDATE_SCHEMA_VERSION,
    SKILL_EVALUATION_REPORT_SCHEMA_VERSION, SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
    SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
};
pub use supervision::{
    default_component, default_recoverability, sorted_unique_event_ids, ComponentRef, DetectorRef,
    EventEnvelope, FeedbackEvent, FeedbackSignal, FeedbackSource, Incident, IncidentKind,
    IncidentStatus, InvalidSupervision, OutcomeResolution, OutcomeRevision, OutcomeSource,
    Recoverability, Severity, SUPERVISION_SCHEMA_VERSION,
};
