//! Lucia 的受信离线评测平面。
//!
//! 本 crate 是 Hidden Dataset、Fixture 和最终 Verifier 的行为所有者。普通 TUI、
//! `agent-evolution` 与 Candidate 不依赖本 crate；它们只能读取
//! `agent-evolution-protocol` 中不含任务正文和答案的结果制品。

pub mod archive;
pub mod audit;
pub mod context_evaluation;
pub mod context_fixture;
pub mod dataset;
pub mod fixture;
pub mod gate;
pub mod health;
pub mod model;
mod plugin_deployment;
mod plugin_deployment_store;
mod plugin_gate;
mod plugin_host_audit;
mod plugin_host_smoke;
mod plugin_release;
mod plugin_runtime_evaluation;
mod plugin_signature;
pub mod protocol;
pub mod release;
pub mod report;
pub mod runner;
mod skill_exit_gate;
mod skill_gate;
mod skill_registry;
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
pub use context_evaluation::{
    calculate_context_metrics, evaluate_context_policy_candidate, ContextEvaluationError,
    ContextGatePolicyV1, M6_CONTEXT_GATE_POLICY, M6_CONTEXT_GATE_VERSION, M6_MAX_COST_RATIO_BPS,
    M6_MAX_LATENCY_MS, M6_MIN_CONSTRAINT_RECALL_BPS, M6_MIN_DOWNSTREAM_TASK_SUCCESS_BPS,
    M6_MIN_FACT_RECALL_BPS, M6_MIN_PLAN_STATE_RECALL_BPS, M6_MIN_TOKEN_REDUCTION_BPS,
    M6_MIN_TOOL_STATE_RECALL_BPS,
};
pub use context_fixture::{
    ContextFixtureError, ContextObservationFixtureV1, TrustedContextObservationFixture,
    CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION, MAX_CONTEXT_OBSERVATION_FIXTURE_BYTES,
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
pub use plugin_deployment::{
    PersistentPluginDeploymentController, PluginCanaryDeployment, PluginDeploymentController,
    PluginDeploymentError, PluginPromotionReceipt, PluginRollbackReceipt,
};
pub use plugin_deployment_store::{
    FilePluginDeploymentStore, PluginCanaryDeploymentBindingV1,
    PluginCanaryDeploymentPersistenceView, PluginCanaryDeploymentRecordV1, PluginDeploymentId,
    PluginDeploymentStateV1, PluginDeploymentStoreError,
    PLUGIN_CANARY_DEPLOYMENT_RECORD_SCHEMA_VERSION, PREVIOUS_PLUGIN_BUNDLE_MEDIA_TYPE,
};
pub use plugin_gate::{evaluate_plugin_source, PluginGateError};
pub use plugin_host_audit::{
    bind_plugin_host_audit, protocol_capability_profile, protocol_component_interface,
    ManifestComponentInspector, PluginHostAuditBinding, PluginHostAuditBindingError,
    TrustedHostCheckOutcome,
};
pub use plugin_host_smoke::{
    run_plugin_host_smoke, PluginHostDeclarationSnapshot, PluginHostSmokeError,
    PluginHostSmokeInput, PluginHostSmokeOutput,
};
pub use plugin_release::{
    FilePluginReleaseArchive, PluginCanaryAdmissionV1, PluginEvaluationArchiveRecordV1,
    PluginReleaseArchiveRecordV1, PluginReleaseController, PluginReleaseError,
    PluginRollbackRequestV1, PLUGIN_BUNDLE_MEDIA_TYPE, PLUGIN_COMPONENT_MEDIA_TYPE,
    PLUGIN_EVALUATION_ARCHIVE_SCHEMA_VERSION, PLUGIN_EVALUATION_REPORT_MEDIA_TYPE,
    PLUGIN_GATE_INPUT_MEDIA_TYPE, PLUGIN_RELEASE_ARCHIVE_SCHEMA_VERSION,
    PLUGIN_RELEASE_ENVELOPE_MEDIA_TYPE,
};
pub use plugin_runtime_evaluation::{
    PinnedPluginEvaluationDataset, PluginEvaluationHostFactory, PluginEvaluationHostRequest,
    PluginEvaluationSubject, PluginRuntimeActionKindV1, PluginRuntimeActionV1,
    PluginRuntimeCaseReceiptV1, PluginRuntimeCaseRefV1, PluginRuntimeCaseV1,
    PluginRuntimeDatasetManifestV1, PluginRuntimeEvaluationError, PluginRuntimeEvaluationOutput,
    PluginRuntimeEvaluationReportV1, PluginRuntimeEvaluator, PluginRuntimeFixtureRefV1,
    PluginRuntimeVerifierV1, WasmPluginEvaluationHostFactory, MAX_PLUGIN_EVALUATION_WASM_FUEL,
    MAX_PLUGIN_EVALUATION_WASM_MEMORY_BYTES, MAX_PLUGIN_EVALUATION_WASM_YIELD_INTERVAL,
    MAX_PLUGIN_RUNTIME_CASES, MAX_PLUGIN_RUNTIME_CASE_BYTES, MAX_PLUGIN_RUNTIME_CASE_TIMEOUT_MS,
    MAX_PLUGIN_RUNTIME_DATASET_BYTES, MAX_PLUGIN_RUNTIME_FIXTURES,
    MAX_PLUGIN_RUNTIME_MANIFEST_BYTES, PLUGIN_RUNTIME_CASE_SCHEMA_VERSION,
    PLUGIN_RUNTIME_DATASET_SCHEMA_VERSION, PLUGIN_RUNTIME_MANIFEST_FILE_NAME,
    PLUGIN_RUNTIME_REPORT_MEDIA_TYPE, PLUGIN_RUNTIME_REPORT_SCHEMA_VERSION,
};
pub use plugin_signature::{
    PluginSignatureError, TrustedPluginKeyring, TrustedPluginSigner, TrustedPluginVerifyingKey,
};
pub use protocol::{ProtocolDifference, ProtocolTrace, ProtocolTraceEntry, ProtocolTraceError};
pub use release::{ReleaseController, ReleaseError, ReleaseReceipt};
pub use report::{
    evaluation_report_digest, ContextEvaluationReportBuilder, ContextEvaluationReportMetadata,
    ContextReportBuildError, EvaluationReportBuilder, EvaluationReportIdentity,
    EvaluationReportMetadata, ReportBuildError, TrustedEvaluationReport,
};
pub use runner::{
    ComparativeEvaluation, ComparativeRunner, ComparativeRunnerConfig, EvaluationAssurances,
    EvaluationSubject, FixtureReplayReport, RecordedFixtureAttempt, RunnerError,
};
pub use skill_exit_gate::{
    SkillActivationAuthorizationV1, SkillExitGate, SkillExitGateError, SkillExitGateOutcomeV1,
    SkillPostPromotionProofV1, SkillPromotionReceiptV1, SKILL_EVALUATION_REPORT_MEDIA_TYPE,
};
pub use skill_gate::{
    evaluate_skill_candidate, evaluate_skill_candidate_with_policy, SkillCommitPolicyV1,
    SkillGateError, TrustedSkillGateResultV1, M7_SKILL_COMMIT_POLICY_VERSION,
};
pub use skill_registry::{
    SkillEvaluationRegistryEntryV1, SkillEvaluationRegistryV1, SkillHealthRegistryEntryV1,
    SkillRegistryAuthorizationV1, SkillRegistryError, TrustedSkillEvaluationRegistry,
    MAX_SKILL_EVALUATION_REGISTRY_BYTES, SKILL_EVALUATION_REGISTRY_FILE,
    SKILL_EVALUATION_REGISTRY_SCHEMA_VERSION,
};
pub use verifier::{
    BuiltinVerifierV1, TrustedVerifier, VerificationResult, VerifierCheck, VerifierRegistry,
    VerifierRule, VERIFIER_RULE_SCHEMA_VERSION,
};
