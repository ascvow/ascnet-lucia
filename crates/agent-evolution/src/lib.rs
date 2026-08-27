//! Lucia Evolution 证据平面的存储、记录与确定性协议回放。
//!
//! 本 crate 可以由应用层选择性装配。`agent-core` 不依赖本 crate，因此未启用
//! Evolution 时原有 Serve 路径不会改变，也不会链接候选生成或评测逻辑。

#![deny(missing_docs)]

mod aggregation;
mod archive;
mod artifact_store;
mod attribution;
mod candidate_builder;
mod candidate_selection;
mod certificate;
mod context_candidate_builder;
mod context_policy;
mod cycle;
mod episode_evidence;
mod episode_selection;
mod episode_store;
mod evaluation_store;
mod evaluator_process;
mod evolution_policy;
mod feedback;
mod genome_diff;
mod genome_store;
mod history;
mod issue_observation;
mod metrics;
mod outbox;
mod outcome_revision;
mod pipeline;
mod plugin_builder_worker;
mod plugin_dependency_policy;
mod plugin_workspace;
mod prompt_cycle;
mod prompt_mutation;
mod recorder;
mod recorder_hub;
mod replay;
mod runtime_health;
mod scorecard;
mod skill_candidate_builder;
mod skill_cycle;
mod skill_mutation;
mod skill_repository;
mod skill_usage;
mod supervision;
mod template_generator;

pub use aggregation::IssueAggregator;
pub use archive::{ArchiveError, FileEvolutionArchive};
pub use artifact_store::{ArtifactStore, ArtifactStoreError, FileArtifactStore};
pub use attribution::attribute_failures;
pub use candidate_builder::{
    CandidateBuildError, CandidateBuilder, MAX_TASK_STRATEGY_PROMPT_BYTES,
};
pub use candidate_selection::{CandidateSelectionError, CandidateSelector, SelectedCandidate};
pub use certificate::{
    CertificateError, EvolutionCertificate, EvolutionCertificateInput,
    EVOLUTION_CERTIFICATE_SCHEMA_VERSION,
};
pub use context_candidate_builder::{
    ContextCandidateBuildError, ContextCandidateBuilder, CONTEXT_LOADER_CAPABILITY_ID,
};
pub use context_policy::{
    ContextPolicyRepository, ContextPolicyRepositoryError, CONTEXT_POLICY_MEDIA_TYPE,
    MAX_CONTEXT_POLICY_BYTES,
};
pub use cycle::{
    is_terminal_cycle_stage, CycleStoreError, EvolutionCycleStore, FileEvolutionCycleStore,
};
pub use episode_evidence::{load_episode_evidence, EpisodeEvidence, EpisodeEvidenceError};
pub use episode_selection::{
    EpisodeSelectionError, EpisodeSelector, MutationEpisodeEvidence, MutationEvidence,
    MutationFailureEvidence,
};
pub use episode_store::{EpisodeQuery, EpisodeStore, EpisodeStoreError, FileEpisodeStore};
pub use evaluation_store::{
    load_evaluation_report, EvaluationStoreError, FileEvaluationReportStore,
};
pub use evaluator_process::{EvaluatorClient, EvaluatorProcessError, LuciaEvalProcessClient};
pub use evolution_policy::{
    EvolutionPolicy, EVOLUTION_POLICY_VERSION, TASK_STRATEGY_MVP_CANDIDATE_COUNT,
};
pub use feedback::{FeedbackError, FeedbackProcessor};
pub use genome_diff::{diff_genomes, verify_allowed_genome_diff, GenomeDiffError};
pub use genome_store::{
    FileGenomeResolver, FileGenomeStore, FileStableGenomePublisher, GenomePromotionError,
    GenomeResolver, GenomeResolverError, GenomeSelector, GenomeStore, GenomeStoreError,
    StableGenomeRef, STABLE_GENOME_REF_SCHEMA_VERSION,
};
pub use history::{
    compute_history, CapabilityMapCell, CapabilityMapRow, EvolutionFunnel, EvolutionHistory,
    EvolutionVelocityPoint, FixSurvivalPoint, HiddenTrendPoint, HiddenTrendSegment, HistoryError,
    LineageNode, EVOLUTION_HISTORY_SCHEMA_VERSION,
};
pub use issue_observation::{
    FileIssueObservationStore, IssueObservation, IssueObservationError, IssueObservationStore,
    ISSUE_OBSERVATION_SCHEMA_VERSION,
};
pub use metrics::{
    aggregate_case, aggregate_dataset, compare_dataset, compare_resources, regression_retention,
    resource_averages, resource_delta, safety_metrics, stability, CapabilityScorePolicy,
    CapabilityScoreSummary, CaseMetric, DatasetComparison, DatasetMetrics, PercentagePointDelta,
    Rate, RegressionRetention, RelativeDelta, ResourceAverages, ResourceComparison, ResourceDelta,
    SafetyComparison, SafetyMetrics, StabilityMetrics,
};
pub use outbox::{EvolutionOutbox, EvolutionOutboxItem, FileEvolutionOutbox, OutboxError};
pub use outcome_revision::{FileOutcomeRevisionStore, OutcomeRevisionError, OutcomeRevisionStore};
pub use pipeline::{EvolutionPipeline, PipelineError};
pub use plugin_builder_worker::{
    BuildStepRecord, ComponentInspectionRequest, ComponentInspector, ComponentInspectorFailure,
    PluginBuildRequest, PluginBuildResult, PluginBuildStep, PluginBuilderWorker,
    PluginBuilderWorkerConfig, PluginBuilderWorkerError, ProcessOutput, ProcessRequest,
    ProcessRunner, ProcessRunnerFailure, ProcessStreamSummary, RealProcessRunner,
    TrustedComponentInspection, MAX_PLUGIN_BUILDER_ERROR_BYTES, MAX_PLUGIN_COMPONENT_BYTES,
    PLUGIN_BUILD_ENVIRONMENT_SCHEMA_VERSION, PLUGIN_BUILD_LOG_SCHEMA_VERSION,
};
pub use plugin_dependency_policy::{
    PluginDependencyPolicy, PluginDependencyPolicyError, ValidatedPluginDependencyPlan,
    MAX_PLUGIN_CARGO_LOCK_BYTES, MAX_PLUGIN_CARGO_MANIFEST_BYTES,
    PLUGIN_DEPENDENCY_PLAN_SCHEMA_VERSION,
};
pub use plugin_workspace::{
    PluginWorkspaceEntry, PluginWorkspaceError, PluginWorkspaceManifest,
    PluginWorkspaceMaterializer, PluginWorkspaceRequest, ValidatedPluginWorkspacePlan,
    MAX_PLUGIN_WORKSPACE_FILES, MAX_PLUGIN_WORKSPACE_FILE_BYTES, MAX_PLUGIN_WORKSPACE_PATCHES,
    MAX_PLUGIN_WORKSPACE_TOTAL_BYTES, PLUGIN_PATCH_PLAN_SCHEMA_VERSION,
};
pub use prompt_cycle::{PromptCycleError, PromptEvolutionCycle};
pub use prompt_mutation::{
    BoundedPromptMutator, MutationProposalContext, PromptMutationDraft, PromptMutationError,
    PromptMutationGenerationError, PromptMutationGenerator, PromptMutationRequest,
};
pub use recorder::{EpisodeRecorder, EpisodeRecorderConfig, EpisodeRecorderError};
pub use recorder_hub::{EpisodeRecorderHub, EpisodeRecorderHubError, RegisteredEpisodeRun};
pub use replay::{ProtocolReplay, ProtocolReplayError, ReplayEventSink, ReplayReport};
pub use runtime_health::{
    FileRuntimeHealthObservationStore, RuntimeHealthRecorder, RuntimeHealthRecorderError,
    RuntimeHealthStoreError, VerifiedRuntimeHealthObservation,
    MAX_RUNTIME_HEALTH_OBSERVATION_BYTES, RUNTIME_HEALTH_DIRECTORY,
};
pub use scorecard::{
    comparison_validity, compute_scorecard, headline_verdict, BehaviorAssessment,
    ComparisonValidity, ComparisonViolation, ComparisonViolationKind, ConfidenceInterval,
    DatasetMetricSummary, EvaluationConfidence, EvolutionScorecard, EvolutionVerdictPolicy,
    GateSummary, HeadlineVerdict, InheritanceMetrics, RegressionComparison, ResourceGatePolicy,
    ScorecardError, EVOLUTION_SCORECARD_SCHEMA_VERSION,
};
pub use skill_candidate_builder::{SkillCandidateBuildError, SkillCandidateBuilder};
pub use skill_cycle::{
    SkillEvolutionArchiveV1, SkillEvolutionCycle, SkillEvolutionCycleError,
    SkillEvolutionCycleRequestV1, SkillEvolutionCycleResultV1, SkillEvolutionDispositionV1,
    SkillEvolutionOrchestrator, SkillEvolutionOrchestratorError, SkillGateCycleOutcomeV1,
    SkillGatePromotionV1, SkillHealthVerdictV1, SKILL_EVOLUTION_ARCHIVE_SCHEMA_VERSION,
    SKILL_EVOLUTION_CANDIDATE_COUNT,
};
pub use skill_mutation::{
    BoundedSkillMutator, SkillContentDraftV1, SkillMutationDraftOperationV1, SkillMutationDraftV1,
    SkillMutationError, SkillMutationGenerationError, SkillMutationGenerator,
    SkillMutationParentView, SkillMutationRequestV1, M7_SKILL_CANDIDATE_COUNT,
    M7_SKILL_MUTATION_POLICY_VERSION, MAX_SKILL_DRAFT_ARTIFACTS, MAX_SKILL_DRAFT_BYTES,
    MAX_SKILL_DRAFT_HYPOTHESIS_BYTES,
};
pub use skill_repository::{
    FileSkillStatusStore, SkillArtifactRepository, SkillRepositoryError, SkillStatusIndexEntryV1,
};
pub use skill_usage::{
    collect_trusted_skill_evaluation_bindings, collect_trusted_skill_usage_bindings,
    SkillUsageBindingError, SKILL_LOADED_EVENT_SCHEMA_VERSION, SKILL_LOADED_EVENT_V1,
    SKILL_USAGE_EVENT_MEDIA_TYPE,
};
pub use supervision::{
    RunSupervisor, SupervisionError, SupervisionReport, OUTCOME_RESOLUTION_EVENT,
};
pub use template_generator::DeterministicPromptMutationGenerator;
