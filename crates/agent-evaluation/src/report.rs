//! Comparative Runner 到正式 EvaluationReport 的受信构建路径。

use crate::{
    evaluate_commit_gate, CommitGateOutcome, CommitPolicy, ComparativeEvaluation,
    EvaluationIntegrity,
};
use agent_evolution::{diff_genomes, GenomeDiffError};
use agent_evolution_protocol::{
    ArtifactDigest, DatasetKind, EvaluationReport, EvaluationReportId, GenomeRevision,
    GenomeRevisionError, InvalidEvaluationReport, EVALUATION_REPORT_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};

/// EvaluationReport 的 Lineage 与时间元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationReportMetadata {
    /// Evolution Lineage；未进入版本链的独立比较可以为 `None`。
    pub lineage: Option<String>,
    /// Parent 在 Lineage 中的代数。
    pub parent_generation: Option<u64>,
    /// Candidate 在 Lineage 中的下一代代数。
    pub candidate_generation: Option<u64>,
    /// 由受信进程生成的 Unix 毫秒时间。
    pub generated_at_ms: u64,
}

/// 由受信请求绑定层预先分配的正式报告身份。
///
/// `report_id` 与 `generated_at_ms` 必须在同一 `request_id` 的全部重试中保持不变，避免
/// 部分提交恢复时生成另一个报告身份。普通调用方应继续使用
/// [`EvaluationReportBuilder::build`]，只有持久化请求绑定的控制面才能构造本类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationReportIdentity {
    /// 固定的正式报告标识。
    pub report_id: EvaluationReportId,
    /// 固定的报告生成时间，使用 Unix 毫秒。
    pub generated_at_ms: u64,
}

/// 正式报告及其不含 Hidden 正文的 Gate 诊断。
#[derive(Debug, Clone, PartialEq)]
pub struct TrustedEvaluationReport {
    /// 可进入不可变 Store 的正式协议报告。
    report: EvaluationReport,
    /// 可信 Commit Gate 的详细原因和指标；完整 Fixture 录制不在此结构中。
    gate: CommitGateOutcome,
    /// 生成本报告的内置 Commit Policy 版本。
    commit_policy_version: String,
    /// Evaluator 私有完整录制制品的 SHA-256；正文不得进入公开 Report 或 Receipt。
    private_artifact_digest: ArtifactDigest,
    /// 只允许可信 Archive 写入私有 CAS 的完整录制字节。
    private_artifact: Vec<u8>,
}

impl TrustedEvaluationReport {
    /// 返回不含 Hidden 逐 Case 内容的正式报告。
    pub fn report(&self) -> &EvaluationReport {
        &self.report
    }

    /// 返回由可信 Builder 计算的 Gate 详情和聚合指标。
    pub fn gate(&self) -> &CommitGateOutcome {
        &self.gate
    }

    /// 返回 Gate 使用的内置 Commit Policy 版本。
    pub fn commit_policy_version(&self) -> &str {
        &self.commit_policy_version
    }

    /// 返回完整录制私有制品摘要，不暴露正文。
    pub fn private_artifact_digest(&self) -> &ArtifactDigest {
        &self.private_artifact_digest
    }

    /// 返回完整录制字节，仅供本 crate 的可信 Archive 持久化。
    pub(crate) fn private_artifact_bytes(&self) -> &[u8] {
        &self.private_artifact
    }

    /// 从 Prepared Journal 绑定的公开报告、Gate 与私有录制制品恢复可信报告。
    ///
    /// 本入口只供 Archive 在进程重启后恢复 Runner 已完成的结果；它会重新校验协议报告、
    /// Gate 决策、Policy 版本与私有录制摘要，避免把损坏的 Prepared 制品继续提交为 Seal。
    ///
    /// # Errors
    ///
    /// 报告结构、Gate 绑定、Policy 版本或私有录制摘要不合法时返回
    /// [`ReportBuildError`]。
    pub(crate) fn restore_prepared(
        report: EvaluationReport,
        gate: CommitGateOutcome,
        commit_policy_version: String,
        private_artifact_digest: ArtifactDigest,
        private_artifact: Vec<u8>,
    ) -> Result<Self, ReportBuildError> {
        report.validate()?;
        if report.gate_decision != gate.decision || report.lifecycle != gate.lifecycle {
            return Err(ReportBuildError::PreparedGateMismatch);
        }
        if commit_policy_version.is_empty() {
            return Err(ReportBuildError::MissingPreparedCommitPolicyVersion);
        }
        if digest_bytes(&private_artifact)? != private_artifact_digest {
            return Err(ReportBuildError::PreparedPrivateArtifactDigestMismatch);
        }
        Ok(Self {
            report,
            gate,
            commit_policy_version,
            private_artifact_digest,
            private_artifact,
        })
    }
}

/// 固定使用只读 Commit Policy 的正式报告构建器。
#[derive(Debug, Clone)]
pub struct EvaluationReportBuilder {
    policy: CommitPolicy,
}

impl EvaluationReportBuilder {
    /// 创建只允许 Task Strategy Prompt 变异的内置构建器。
    pub fn task_strategy_mvp() -> Self {
        Self {
            policy: CommitPolicy::task_strategy_mvp(),
        }
    }

    /// 返回构建器持有的不可变 Commit Policy。
    pub fn policy(&self) -> &CommitPolicy {
        &self.policy
    }

    /// 从真实比较结果构造并校验一份正式 EvaluationReport。
    ///
    /// 本方法会重新验证双方 Revision 摘要，并调用受信全字段 Comparator 生成真实差异；
    /// 调用方不能提交 Candidate 自报的 Diff 或 Gate 结果。
    /// 完整模型/工具录制只保留在 `ComparativeEvaluation` 私有侧，不会复制进报告。
    ///
    /// # Errors
    ///
    /// Genome 摘要无效、Runner 结果与 Revision 不绑定、Registry/隔离保证缺失，或最终报告
    /// 结构不合法时返回错误。
    pub fn build(
        &self,
        comparison: &ComparativeEvaluation,
        parent: &GenomeRevision,
        candidate: &GenomeRevision,
        metadata: EvaluationReportMetadata,
    ) -> Result<TrustedEvaluationReport, ReportBuildError> {
        let identity = EvaluationReportIdentity {
            report_id: EvaluationReportId::generate(),
            generated_at_ms: metadata.generated_at_ms,
        };
        self.build_with_fixed_identity(comparison, parent, candidate, metadata, identity)
    }

    /// 使用请求绑定层持久化的固定身份构造正式报告。
    ///
    /// 除报告 ID 与生成时间由 `identity` 提供外，校验、可信差异、Gate、Hidden 数据裁剪和
    /// 私有录制绑定均与 [`Self::build`] 完全一致。本入口不会信任 Candidate 自报身份，调用方
    /// 必须先通过只追加请求 Store 固定 `identity`。
    ///
    /// # Errors
    ///
    /// Genome、Runner 绑定、可信保证、差异、Gate 或最终报告不合法时返回
    /// [`ReportBuildError`]。
    pub fn build_with_fixed_identity(
        &self,
        comparison: &ComparativeEvaluation,
        parent: &GenomeRevision,
        candidate: &GenomeRevision,
        mut metadata: EvaluationReportMetadata,
        identity: EvaluationReportIdentity,
    ) -> Result<TrustedEvaluationReport, ReportBuildError> {
        metadata.generated_at_ms = identity.generated_at_ms;
        parent
            .validate()
            .map_err(|source| ReportBuildError::InvalidParent { source })?;
        candidate
            .validate()
            .map_err(|source| ReportBuildError::InvalidCandidate { source })?;
        if comparison.parent.genome_revision != parent.revision_id
            || comparison.candidate.genome_revision != candidate.revision_id
        {
            return Err(ReportBuildError::SubjectMismatch);
        }
        if parent.genome.prompt.task_strategy() != Some(&comparison.parent_strategy_artifact)
            || candidate.genome.prompt.task_strategy()
                != Some(&comparison.candidate_strategy_artifact)
        {
            return Err(ReportBuildError::StrategyArtifactMismatch);
        }
        if !comparison.assurances.verifier_registry_enforced {
            return Err(ReportBuildError::MissingAssurance("verifier_registry"));
        }
        let genome_diff = diff_genomes(parent, candidate)?;

        let integrity = EvaluationIntegrity {
            artifact_integrity_verified: comparison.assurances.dataset_artifact_integrity_verified,
            hidden_dataset_isolated: comparison.assurances.hidden_dataset_isolated,
            // 报告尚未提交时不存在可绑定自身摘要的审计记录，不能预先声明审计成功。
            audit_integrity_verified: None,
        };
        let gate = evaluate_commit_gate(comparison, &genome_diff, integrity, &self.policy);
        let private_artifact = serialize_private_recordings(comparison)?;
        let private_artifact_digest = digest_bytes(&private_artifact)?;
        let mut parent_run = comparison.parent.clone();
        let mut candidate_run = comparison.candidate.clone();
        // Hidden 只允许以 Gate 聚合指标留在受信 Seal；正式报告不得包含逐 Case ID 或结果。
        parent_run
            .task_cases
            .retain(|case| case.metadata.dataset_kind != DatasetKind::Hidden);
        candidate_run
            .task_cases
            .retain(|case| case.metadata.dataset_kind != DatasetKind::Hidden);
        let report = EvaluationReport {
            schema_version: EVALUATION_REPORT_SCHEMA_VERSION,
            report_id: identity.report_id,
            lineage: metadata.lineage,
            parent_generation: metadata.parent_generation,
            candidate_generation: metadata.candidate_generation,
            parent: parent_run,
            candidate: candidate_run,
            genome_diff,
            allowed_mutation_surfaces: self.policy.allowed_surfaces().clone(),
            gate_decision: gate.decision,
            lifecycle: gate.lifecycle,
            release_record: None,
            inheritance: None,
            artifact_integrity_verified: Some(integrity.artifact_integrity_verified),
            audit_integrity_verified: integrity.audit_integrity_verified,
            hidden_dataset_isolated: Some(integrity.hidden_dataset_isolated),
            generated_at_ms: metadata.generated_at_ms,
        };
        report.validate()?;
        Ok(TrustedEvaluationReport {
            report,
            gate,
            commit_policy_version: self.policy.version().to_string(),
            private_artifact_digest,
            private_artifact,
        })
    }
}

/// 计算正式 EvaluationReport 存储 JSON 的稳定 SHA-256 摘要。
///
/// 摘要覆盖与不可变 Store 相同的 pretty JSON 字节，可用于 Audit、Seal 与 Release Controller
/// 的三方绑定；报告无法序列化时返回错误。
///
/// # Errors
///
/// JSON 序列化或强类型摘要构造失败时返回 [`ReportBuildError`]。
pub fn evaluation_report_digest(
    report: &EvaluationReport,
) -> Result<ArtifactDigest, ReportBuildError> {
    let bytes = serde_json::to_vec_pretty(report).map_err(ReportBuildError::Serialize)?;
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| ReportBuildError::InvalidDigest(error.to_string()))
}

/// 序列化 Evaluator 私有完整录制。
///
/// 返回值可能包含 Hidden 输入、模型交换和工具参数，只能写入 Evaluator 私有 CAS，不得写入
/// stdout、普通 Evidence、Mutation 输入或正式 EvaluationReport。
///
/// # Errors
///
/// 录制无法序列化时返回 [`ReportBuildError`]。
pub(crate) fn serialize_private_recordings(
    comparison: &ComparativeEvaluation,
) -> Result<Vec<u8>, ReportBuildError> {
    #[derive(serde::Serialize)]
    struct PrivateRecordings<'a> {
        parent: &'a [crate::RecordedFixtureAttempt],
        candidate: &'a [crate::RecordedFixtureAttempt],
    }

    serde_json::to_vec(&PrivateRecordings {
        parent: &comparison.parent_recordings,
        candidate: &comparison.candidate_recordings,
    })
    .map_err(ReportBuildError::Serialize)
}

/// 计算任意正式评测制品字节的协议摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, ReportBuildError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| ReportBuildError::InvalidDigest(error.to_string()))
}

impl Default for EvaluationReportBuilder {
    fn default() -> Self {
        Self::task_strategy_mvp()
    }
}

/// 正式 EvaluationReport 构建错误。
#[derive(Debug, thiserror::Error)]
pub enum ReportBuildError {
    /// Parent Genome 摘要或结构无效。
    #[error("Parent Genome 无效：{source}")]
    InvalidParent {
        /// 原始 Genome 校验错误。
        #[source]
        source: GenomeRevisionError,
    },
    /// Candidate Genome 摘要或结构无效。
    #[error("Candidate Genome 无效：{source}")]
    InvalidCandidate {
        /// 原始 Genome 校验错误。
        #[source]
        source: GenomeRevisionError,
    },
    /// Runner 返回的身份与输入 Genome 不一致。
    #[error("Comparative Runner 结果与 Parent/Candidate Genome 身份不一致")]
    SubjectMismatch,
    /// Runner 实际使用的 Prompt 摘要与 Genome 中的制品引用不一致。
    #[error("Comparative Runner 的 Task Strategy Prompt 与 Genome 制品引用不一致")]
    StrategyArtifactMismatch,
    /// Runner 未提供正式报告要求的可信保证。
    #[error("Comparative Runner 缺少可信保证：{0}")]
    MissingAssurance(&'static str),
    /// 最终报告违反协议结构不变量。
    #[error(transparent)]
    InvalidReport(#[from] InvalidEvaluationReport),
    /// Parent/Candidate 的可信全字段差异无法生成。
    #[error(transparent)]
    GenomeDiff(#[from] GenomeDiffError),
    /// 正式报告无法按不可变 Store 格式序列化。
    #[error("序列化正式 EvaluationReport 失败：{0}")]
    Serialize(serde_json::Error),
    /// SHA-256 文本无法构造成协议摘要。
    #[error("构造正式 EvaluationReport 摘要失败：{0}")]
    InvalidDigest(String),
    /// Prepared Gate 的决策或生命周期与正式报告不一致。
    #[error("Prepared Evaluation Gate 与正式报告不一致")]
    PreparedGateMismatch,
    /// Prepared 制品缺少生成报告时使用的 Commit Policy 版本。
    #[error("Prepared Evaluation 缺少 Commit Policy 版本")]
    MissingPreparedCommitPolicyVersion,
    /// Prepared 私有录制正文与声明的摘要不一致。
    #[error("Prepared Evaluation 私有录制摘要不一致")]
    PreparedPrivateArtifactDigestMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ComparativeEvaluation, EvaluationAssurances, ReleaseController, TrustedEvaluationArchive,
    };
    use agent_evolution::{
        FileGenomeResolver, FileStableGenomePublisher, GenomeResolver, GenomeSelector, GenomeStore,
    };
    use agent_evolution_protocol::{
        AgentGenome, DatasetVersionId, EvaluationEnvironment, EvaluationRequestV1, EvaluationRun,
        EvaluationRunId, EvaluationUsage, GateDecision, GenomeMetadata, GenomeRevisionId,
        ModelGenome, PromptArtifactRef, PromptGenome, PromptLayer, RunId, RuntimeIdentity,
        SafetyAttemptSummary, TaskAttemptResult, TaskAttemptStatus, TaskCaseMetadata,
        TaskCaseResult, ToolProfileGenome, EVALUATION_REQUEST_SCHEMA_VERSION,
        GENOME_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::collections::{BTreeMap, BTreeSet};
    use tempfile::TempDir;
    use tokio::fs;

    /// 计算测试 Prompt 的协议摘要。
    fn strategy_digest(prompt: &str) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(prompt.as_bytes())))
            .expect("测试 Prompt 摘要合法")
    }

    /// 构造仅 Task Strategy Prompt 不同的合法 Genome Revision。
    fn revision(prompt: &str) -> GenomeRevision {
        GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".to_string(),
                    git_commit: "test".to_string(),
                    git_dirty: false,
                    target_triple: "test-target".to_string(),
                    features: BTreeSet::new(),
                },
                model: ModelGenome {
                    provider: "fixture".to_string(),
                    provider_kind: "fixture".to_string(),
                    model: "fixture-model-v1".to_string(),
                    base_url: None,
                    protocol: None,
                    max_tokens: Some(64),
                    temperature: None,
                    stream: false,
                    provider_options_digest: None,
                },
                prompt: PromptGenome {
                    messages: vec![PromptArtifactRef {
                        layer: PromptLayer::TaskStrategy,
                        artifact: strategy_digest(prompt),
                    }],
                },
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("测试 Genome 合法")
    }

    /// 构造双方完全一致的可信 Evaluation 环境。
    fn environment() -> EvaluationEnvironment {
        EvaluationEnvironment {
            kernel_ref: "kernel-v1".to_string(),
            model_provider: "evaluation-fixture".to_string(),
            model: "fixture-model-v1".to_string(),
            model_parameters_digest: "model".to_string(),
            tool_profile_digest: "tools".to_string(),
            execution_profile_digest: "execution".to_string(),
            plugin_set_digest: "plugins".to_string(),
            capability_owner_digest: "owners".to_string(),
            resource_budget_digest: "budget".to_string(),
            verifier_version: "verifier-set:test".to_string(),
            evaluation_policy_version: "evaluation-policy-v1".to_string(),
            environment_fixture_digest: "environment".to_string(),
            repeat_count: 1,
        }
    }

    /// 构造单个 Dataset 的确定性 Case 结果。
    fn task_case(kind: DatasetKind, passed: bool) -> TaskCaseResult {
        let name = match kind {
            DatasetKind::Repair => "repair",
            DatasetKind::Hidden => "hidden-secret-case",
            DatasetKind::Regression => "regression",
            DatasetKind::Safety => "safety",
        };
        TaskCaseResult {
            metadata: TaskCaseMetadata {
                task_case_id: format!("case_{name}"),
                task_family: "test".to_string(),
                dataset_kind: kind,
                critical: kind == DatasetKind::Safety,
                deterministic: true,
                pass_threshold: Some(1.0),
            },
            attempts: vec![TaskAttemptResult {
                task_case_id: format!("case_{name}"),
                repeat_index: 0,
                status: if passed {
                    TaskAttemptStatus::Passed
                } else {
                    TaskAttemptStatus::Failed
                },
                verifier_passed: Some(passed),
                usage: EvaluationUsage::default(),
                safety: (kind == DatasetKind::Safety).then(|| SafetyAttemptSummary {
                    critical_failures: u64::from(!passed),
                    critical_cases_total: 1,
                    critical_cases_passed: u64::from(passed),
                    ..SafetyAttemptSummary::default()
                }),
                run_id: Some(RunId::generate()),
            }],
        }
    }

    /// 构造具备四类 Dataset、Repair 正向增益的可信比较。
    fn comparison(parent: &GenomeRevision, candidate: &GenomeRevision) -> ComparativeEvaluation {
        let datasets = [
            (DatasetKind::Repair, "dsv_repair000"),
            (DatasetKind::Hidden, "dsv_hidden000"),
            (DatasetKind::Regression, "dsv_regression000"),
            (DatasetKind::Safety, "dsv_safety000"),
        ]
        .into_iter()
        .map(|(kind, id)| {
            (
                kind,
                DatasetVersionId::new(id).expect("测试 Dataset ID 合法"),
            )
        })
        .collect::<BTreeMap<_, _>>();
        ComparativeEvaluation {
            parent: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: parent.revision_id.clone(),
                environment: environment(),
                datasets: datasets.clone(),
                task_cases: vec![
                    task_case(DatasetKind::Repair, false),
                    task_case(DatasetKind::Hidden, true),
                    task_case(DatasetKind::Regression, true),
                    task_case(DatasetKind::Safety, true),
                ],
            },
            candidate: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: candidate.revision_id.clone(),
                environment: environment(),
                datasets,
                task_cases: vec![
                    task_case(DatasetKind::Repair, true),
                    task_case(DatasetKind::Hidden, true),
                    task_case(DatasetKind::Regression, true),
                    task_case(DatasetKind::Safety, true),
                ],
            },
            protocol_differences: Vec::new(),
            parent_strategy_artifact: parent
                .genome
                .prompt
                .task_strategy()
                .expect("Parent 有策略")
                .clone(),
            candidate_strategy_artifact: candidate
                .genome
                .prompt
                .task_strategy()
                .expect("Candidate 有策略")
                .clone(),
            parent_recordings: Vec::new(),
            candidate_recordings: Vec::new(),
            assurances: EvaluationAssurances {
                dataset_artifact_integrity_verified: true,
                hidden_dataset_isolated: true,
                verifier_registry_enforced: true,
            },
        }
    }

    /// 构造固定 Lineage 元数据。
    fn metadata() -> EvaluationReportMetadata {
        EvaluationReportMetadata {
            lineage: Some("stable/test".to_string()),
            parent_generation: Some(1),
            candidate_generation: Some(2),
            generated_at_ms: 1,
        }
    }

    /// Builder 必须保留 Evaluation Policy、移除 Hidden 逐 Case，并且不能预先伪造 Audit。
    #[test]
    fn builds_sanitized_report_without_forged_audit() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let trusted = EvaluationReportBuilder::default()
            .build(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
            )
            .expect("构建正式报告");

        assert_eq!(trusted.report().gate_decision, GateDecision::Pass);
        assert_eq!(
            trusted
                .report()
                .parent
                .environment
                .evaluation_policy_version,
            "evaluation-policy-v1"
        );
        assert_eq!(trusted.report().audit_integrity_verified, None);
        assert!(trusted
            .report()
            .parent
            .task_cases
            .iter()
            .all(|case| case.metadata.dataset_kind != DatasetKind::Hidden));
        let json = serde_json::to_string(trusted.report()).expect("序列化正式报告");
        assert!(!json.contains("hidden-secret-case"));
    }

    /// 请求恢复入口必须使用持久化身份覆盖调用方元数据中的瞬时时间。
    #[test]
    fn fixed_identity_is_preserved_across_rebuilds() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let comparison = comparison(&parent, &candidate);
        let identity = EvaluationReportIdentity {
            report_id: EvaluationReportId::generate(),
            generated_at_ms: 42,
        };
        let builder = EvaluationReportBuilder::default();
        let first = builder
            .build_with_fixed_identity(
                &comparison,
                &parent,
                &candidate,
                metadata(),
                identity.clone(),
            )
            .expect("首次固定身份构建应成功");
        let mut later_metadata = metadata();
        later_metadata.generated_at_ms = 9_999;
        let rebuilt = builder
            .build_with_fixed_identity(
                &comparison,
                &parent,
                &candidate,
                later_metadata,
                identity.clone(),
            )
            .expect("重建应复用固定身份");

        assert_eq!(first.report().report_id, identity.report_id);
        assert_eq!(rebuilt.report().report_id, identity.report_id);
        assert_eq!(first.report().generated_at_ms, 42);
        assert_eq!(rebuilt.report().generated_at_ms, 42);
        assert_eq!(first.report(), rebuilt.report());
    }

    /// Runner 实际 Prompt 摘要与 Genome 不一致时必须失败关闭。
    #[test]
    fn rejects_strategy_artifact_mismatch() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let mut value = comparison(&parent, &candidate);
        value.candidate_strategy_artifact = strategy_digest("伪造 Prompt");

        assert!(matches!(
            EvaluationReportBuilder::default().build(&value, &parent, &candidate, metadata()),
            Err(ReportBuildError::StrategyArtifactMismatch)
        ));
    }

    /// 高能力分不能抵消可信 Safety 硬失败。
    #[test]
    fn safety_failure_overrides_capability_success() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let mut value = comparison(&parent, &candidate);
        let safety = value
            .candidate
            .task_cases
            .iter_mut()
            .find(|case| case.metadata.dataset_kind == DatasetKind::Safety)
            .expect("存在 Safety Case");
        safety.attempts[0].safety = Some(SafetyAttemptSummary {
            critical_failures: 1,
            critical_cases_total: 1,
            ..SafetyAttemptSummary::default()
        });
        let trusted = EvaluationReportBuilder::default()
            .build(&value, &parent, &candidate, metadata())
            .expect("构建拒绝报告");

        assert_eq!(trusted.report().gate_decision, GateDecision::Reject);
        assert!(trusted
            .gate()
            .hard_failures
            .contains(&"critical_safety_failure".to_string()));
    }

    /// 非 Prompt 行为变化必须由真实 Genome Diff 识别并硬拒绝。
    #[test]
    fn rejects_genome_diff_outside_allowed_surface() {
        let parent = revision("parent");
        let mut candidate = revision("candidate");
        candidate.genome.model.model = "other-model".to_string();
        candidate = GenomeRevision::create(candidate.genome, GenomeMetadata::default())
            .expect("重建越界 Candidate");
        let trusted = EvaluationReportBuilder::default()
            .build(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
            )
            .expect("越界差异应进入 Gate 而不是伪装成构建失败");

        assert_eq!(trusted.report().gate_decision, GateDecision::Reject);
        assert!(trusted
            .gate()
            .hard_failures
            .contains(&"unauthorized_mutation_surface".to_string()));
    }

    /// Report、聚合 Gate、私有录制、Audit 与 Seal 必须形成可重放的可信提交闭包。
    #[tokio::test]
    async fn archive_commits_and_verifies_sealed_report() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let trusted = EvaluationReportBuilder::default()
            .build(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
            )
            .expect("构建正式报告");
        let root = TempDir::new().expect("创建 Evaluation Archive");
        let archive = TrustedEvaluationArchive::new(root.path());
        let verified = archive.commit(&trusted, 1).await.expect("提交可信评测");

        assert_eq!(verified.report().report_id, trusted.report().report_id);
        assert!(verified
            .seal()
            .gate
            .metrics
            .datasets
            .contains_key(&DatasetKind::Hidden));
        assert_eq!(
            archive
                .audit_log()
                .verify()
                .await
                .expect("验证 Audit")
                .record_count(),
            1
        );

        let seal_path = root
            .path()
            .join("seals")
            .join(format!("{}.json", trusted.report().report_id));
        let mut seal: serde_json::Value =
            serde_json::from_slice(&fs::read(&seal_path).await.expect("读取 Seal"))
                .expect("解析 Seal");
        seal["commit_policy_version"] = serde_json::json!("forged-policy");
        fs::write(
            &seal_path,
            serde_json::to_vec_pretty(&seal).expect("序列化篡改 Seal"),
        )
        .await
        .expect("写入篡改 Seal");
        assert!(archive
            .get_verified(&trusted.report().report_id)
            .await
            .is_err());
    }

    /// request_id 重试必须优先复用固定身份和已完成 Seal，冲突请求不得覆盖绑定。
    #[tokio::test]
    async fn archive_binds_request_identity_and_returns_sealed_retry() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let root = TempDir::new().expect("创建 Evaluation Archive");
        let archive = TrustedEvaluationArchive::new(root.path());
        let request = EvaluationRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "evaluation-retry-001".to_string(),
            parent_revision_id: parent.revision_id.clone(),
            candidate_revision_id: candidate.revision_id.clone(),
            lineage: "stable/test".to_string(),
            expected_parent_generation: 1,
            expected_dataset_version: DatasetVersionId::new("dsv_repair000")
                .expect("测试 Dataset ID 合法"),
        };
        let binding = archive
            .bind_request(&request, 42)
            .await
            .expect("首次请求应固定身份");
        assert!(matches!(
            archive.get_verified_for_request(&binding).await,
            Err(crate::EvaluationArchiveError::SealNotFound(_))
        ));
        let trusted = EvaluationReportBuilder::default()
            .build_with_fixed_identity(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
                EvaluationReportIdentity {
                    report_id: binding.report_id.clone(),
                    generated_at_ms: binding.generated_at_ms,
                },
            )
            .expect("固定身份报告应构建成功");
        archive
            .commit(&trusted, binding.generated_at_ms)
            .await
            .expect("固定身份报告应完成 Seal");

        let retry = archive
            .bind_request(&request, 9_999)
            .await
            .expect("相同请求应复用身份");
        assert_eq!(retry, binding);
        let verified = archive
            .get_verified_for_request(&retry)
            .await
            .expect("重试应直接读取已 Seal 报告");
        assert_eq!(verified.report().report_id, binding.report_id);
        assert_eq!(verified.report().generated_at_ms, 42);

        let mut conflict = request;
        conflict.candidate_revision_id = GenomeRevisionId::generate();
        assert!(matches!(
            archive.bind_request(&conflict, 100).await,
            Err(crate::EvaluationArchiveError::RequestBindingConflict(_))
        ));
    }

    /// Report 已写但 Seal 尚未提交时，Prepared Journal 必须无需重建报告即可补齐提交。
    #[tokio::test]
    async fn archive_recovers_partial_report_commit_with_fixed_identity() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let root = TempDir::new().expect("创建 Evaluation Archive");
        let archive = TrustedEvaluationArchive::new(root.path());
        let request = EvaluationRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "evaluation-partial-001".to_string(),
            parent_revision_id: parent.revision_id.clone(),
            candidate_revision_id: candidate.revision_id.clone(),
            lineage: "stable/test".to_string(),
            expected_parent_generation: 1,
            expected_dataset_version: DatasetVersionId::new("dsv_repair000")
                .expect("测试 Dataset ID 合法"),
        };
        let binding = archive
            .bind_request(&request, 42)
            .await
            .expect("请求应固定身份");
        let trusted = EvaluationReportBuilder::default()
            .build_with_fixed_identity(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
                EvaluationReportIdentity {
                    report_id: binding.report_id.clone(),
                    generated_at_ms: binding.generated_at_ms,
                },
            )
            .expect("固定身份报告应构建成功");
        archive
            .prepare_for_request(&binding, &trusted)
            .await
            .expect("应先提交 Prepared Journal");
        let orphan_root = root.path().join("reports").join("reports");
        fs::create_dir_all(&orphan_root)
            .await
            .expect("创建孤立 Report 目录");
        fs::write(
            orphan_root.join(format!("{}.json", binding.report_id)),
            serde_json::to_vec_pretty(trusted.report()).expect("序列化孤立 Report"),
        )
        .await
        .expect("模拟已写 Report");
        drop(trusted);

        let verified = archive
            .commit_prepared_for_request(&binding, binding.generated_at_ms)
            .await
            .expect("Prepared 恢复应补齐 Audit 与 Seal");
        assert_eq!(verified.report().report_id, binding.report_id);
        archive
            .get_verified_for_request(&binding)
            .await
            .expect("补齐后请求应可直接恢复");
    }

    /// Audit 已写或 Seal 已提交时，Prepared Journal 重试不得追加重复 Audit 或重跑 Builder。
    #[tokio::test]
    async fn archive_recovers_prepared_after_audit_and_seal_commit() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let root = TempDir::new().expect("创建 Evaluation Archive");
        let archive = TrustedEvaluationArchive::new(root.path());
        let request = EvaluationRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "evaluation-prepared-audit-001".to_string(),
            parent_revision_id: parent.revision_id.clone(),
            candidate_revision_id: candidate.revision_id.clone(),
            lineage: "stable/test".to_string(),
            expected_parent_generation: 1,
            expected_dataset_version: DatasetVersionId::new("dsv_repair000")
                .expect("测试 Dataset ID 合法"),
        };
        let binding = archive
            .bind_request(&request, 42)
            .await
            .expect("请求应固定身份");
        let trusted = EvaluationReportBuilder::default()
            .build_with_fixed_identity(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
                EvaluationReportIdentity {
                    report_id: binding.report_id.clone(),
                    generated_at_ms: binding.generated_at_ms,
                },
            )
            .expect("固定身份报告应构建成功");
        archive
            .prepare_for_request(&binding, &trusted)
            .await
            .expect("应提交 Prepared Journal 和私有录制");
        let report = trusted.report().clone();
        let report_digest = evaluation_report_digest(&report).expect("计算 Report 摘要");
        let orphan_root = root.path().join("reports").join("reports");
        fs::create_dir_all(&orphan_root)
            .await
            .expect("创建孤立 Report 目录");
        fs::write(
            orphan_root.join(format!("{}.json", binding.report_id)),
            serde_json::to_vec_pretty(&report).expect("序列化孤立 Report"),
        )
        .await
        .expect("模拟已写 Report");
        archive
            .audit_log()
            .append(
                binding.generated_at_ms,
                crate::AuditEvent::EvaluationReportCommitted {
                    report_id: report.report_id.clone(),
                    parent: report.parent.genome_revision.clone(),
                    candidate: report.candidate.genome_revision.clone(),
                    decision: report.gate_decision,
                    report_digest,
                },
            )
            .await
            .expect("模拟已写 Audit");
        drop(trusted);

        archive
            .commit_prepared_for_request(&binding, binding.generated_at_ms)
            .await
            .expect("Audit 提交点后应从 Prepared 补齐 Seal");
        archive
            .commit_prepared_for_request(&binding, binding.generated_at_ms)
            .await
            .expect("Seal 提交点后应幂等恢复");
        assert_eq!(
            archive
                .audit_log()
                .verify()
                .await
                .expect("验证 Audit")
                .record_count(),
            1
        );
    }

    /// Promotion 必须绑定可信 Report；Rollback 必须原子恢复 Parent 并保持代数单调。
    #[tokio::test]
    async fn release_controller_promotes_and_rolls_back_atomically() {
        let parent = revision("parent");
        let candidate = revision("candidate");
        let root = TempDir::new().expect("创建 Release 测试根");
        let evolution_root = root.path().join("evolution");
        let archive_root = root.path().join("evaluation");
        let publisher = FileStableGenomePublisher::new(&evolution_root);
        publisher
            .resolver()
            .store()
            .append(&parent)
            .await
            .expect("登记 Parent");
        publisher
            .resolver()
            .store()
            .append(&candidate)
            .await
            .expect("登记 Candidate");
        publisher
            .publish("stable/test", &parent, 1)
            .await
            .expect("初始化 Stable Parent");

        let trusted = EvaluationReportBuilder::default()
            .build(
                &comparison(&parent, &candidate),
                &parent,
                &candidate,
                metadata(),
            )
            .expect("构建可晋升报告");
        TrustedEvaluationArchive::new(&archive_root)
            .commit(&trusted, 1)
            .await
            .expect("提交正式报告");
        let controller = ReleaseController::new(&evolution_root, &archive_root);
        let promotion_id = agent_evolution_protocol::ReleaseId::generate();
        let promoted = controller
            .promote(&trusted.report().report_id, promotion_id.clone(), 2)
            .await
            .expect("晋升 Candidate");
        assert_eq!(promoted.from, parent.revision_id);
        assert_eq!(promoted.to, candidate.revision_id);
        assert_eq!(promoted.generation, 2);
        let resolver = FileGenomeResolver::new(&evolution_root);
        assert_eq!(
            resolver
                .resolve(&GenomeSelector::Stable("stable/test".to_string()))
                .await
                .expect("解析晋升后 Stable")
                .revision_id,
            candidate.revision_id
        );

        // 相同 Release 重试只能补齐或复用 Audit，不能重复递增代数。
        controller
            .promote(&trusted.report().report_id, promotion_id.clone(), 3)
            .await
            .expect("幂等重试 Promotion");
        let rollback_id = agent_evolution_protocol::ReleaseId::generate();
        let rolled_back = controller
            .rollback(&promotion_id, rollback_id.clone(), 4)
            .await
            .expect("回滚 Parent");
        assert_eq!(rolled_back.from, candidate.revision_id);
        assert_eq!(rolled_back.to, parent.revision_id);
        assert_eq!(rolled_back.generation, 3);
        assert_eq!(rolled_back.rollback_of, Some(promotion_id.clone()));
        controller
            .rollback(&promotion_id, rollback_id, 5)
            .await
            .expect("幂等重试 Rollback");
        let stable = resolver
            .stable_reference("stable/test")
            .await
            .expect("读取回滚后 Stable");
        assert_eq!(stable.revision_id, parent.revision_id);
        assert_eq!(stable.generation, 3);
        assert_eq!(stable.rollback_of, Some(promotion_id));
        assert_eq!(
            TrustedEvaluationArchive::new(&archive_root)
                .audit_log()
                .verify()
                .await
                .expect("验证 Release Audit")
                .record_count(),
            3
        );
    }
}
