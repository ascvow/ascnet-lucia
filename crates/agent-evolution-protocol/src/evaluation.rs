//! Evolution Evaluation 的版本化制品协议。
//!
//! 本模块只描述可信评测平面产生的数据，不负责聚合、判定、晋升或终端展示。
//! Hidden TaskCase 的正文、答案和 Verifier 实现均不得写入这些可展示结构。

use crate::{
    ArtifactRef, DatasetVersionId, EvaluationReportId, EvaluationRunId, GenomeRevisionId,
    ReleaseId, RunId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 当前支持的 EvaluationReport 结构版本。
pub const EVALUATION_REPORT_SCHEMA_VERSION: u32 = 1;

/// Evaluation 数据集的用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetKind {
    /// 复现并验证本轮已知问题的数据集。
    Repair,
    /// Mutator 与 Candidate 均不可见的泛化数据集。
    Hidden,
    /// 保存父版本既有能力与历史修复的数据集。
    Regression,
    /// 独立验证权限、泄漏和完整性的安全数据集。
    Safety,
}

/// Candidate 被允许修改的行为表面。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationSurface {
    /// Task Strategy Prompt 制品。
    TaskStrategyPrompt,
    /// 上下文压缩策略参数。
    ContextPolicy,
    /// 计划策略参数。
    PlanningPolicy,
    /// Skill 内容或选择配置。
    Skill,
    /// 插件 bundle 或配置。
    Plugin,
    /// 模型服务商、模型或采样参数。
    Model,
    /// 工具集合或访问范围。
    ToolProfile,
    /// 执行 Profile 与资源限制。
    ExecutionProfile,
    /// Runtime 或 Kernel 构建。
    Runtime,
    /// 身份、安全或工具契约 Prompt。
    ProtectedPrompt,
    /// 当前协议无法识别的行为表面；默认不可比较。
    Other(String),
}

/// Parent 到 Candidate 的可审计 Genome 差异摘要。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GenomeDiff {
    /// 实际发生变化的行为表面。
    #[serde(default)]
    pub changed_surfaces: BTreeSet<MutationSurface>,
    /// 由可信差异生成器产生的结构化摘要；不得包含 Secret 或 Hidden 内容。
    #[serde(default)]
    pub summary: Vec<String>,
    /// 差异制品的可选 CAS 引用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}

/// 一次 Parent/Candidate 运行的可比环境快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationEnvironment {
    /// 不可变 Kernel 或 Runtime 构建引用。
    pub kernel_ref: String,
    /// 模型逻辑服务商。
    pub model_provider: String,
    /// 模型标识。
    pub model: String,
    /// 模型参数的规范化内容摘要。
    pub model_parameters_digest: String,
    /// 工具 Profile 的规范化内容摘要。
    pub tool_profile_digest: String,
    /// Execution Profile 的规范化内容摘要。
    pub execution_profile_digest: String,
    /// 启用插件集合及配置的规范化内容摘要。
    pub plugin_set_digest: String,
    /// Capability Owner 映射的规范化内容摘要。
    pub capability_owner_digest: String,
    /// 资源预算的规范化内容摘要。
    pub resource_budget_digest: String,
    /// Verifier 实现版本。
    pub verifier_version: String,
    /// Evaluation Policy 版本。
    pub evaluation_policy_version: String,
    /// 环境 Fixture 的内容摘要。
    pub environment_fixture_digest: String,
    /// 每个 TaskCase 的目标重复次数。
    pub repeat_count: u32,
}

/// 单次 TaskCase 尝试的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAttemptStatus {
    /// 运行完成且可信 Verifier 通过。
    Passed,
    /// 运行完成但可信 Verifier 未通过。
    Failed,
    /// 评测平台自身故障；默认不进入有效 Repeat 分母。
    InfrastructureFailure,
    /// Candidate 超出资源预算；属于行为失败而非基础设施失败。
    BudgetFailure,
    /// Candidate 行为导致超时；属于行为失败。
    Timeout,
    /// 尝试制品损坏、缺少 Verifier 或无法可信分类。
    Invalid,
}

/// 单次尝试的资源使用量。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EvaluationUsage {
    /// 总 Token；Provider 未报告时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// 可信计费器报告的货币成本；未知价格时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// 墙钟延迟，单位毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// 工具调用次数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    /// 模型调用次数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_calls: Option<u64>,
    /// ReAct 步数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub react_steps: Option<u64>,
    /// 实际派生的子 Agent 数量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_agents: Option<u64>,
}

/// 单次尝试中由可信安全 Verifier 产生的计数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SafetyAttemptSummary {
    /// Critical 级安全失败数。
    #[serde(default)]
    pub critical_failures: u64,
    /// High 级安全失败数。
    #[serde(default)]
    pub high_failures: u64,
    /// Medium 级安全失败数。
    #[serde(default)]
    pub medium_failures: u64,
    /// 权限扩大次数。
    #[serde(default)]
    pub permission_expansions: u64,
    /// 访问 Hidden Dataset 的尝试次数。
    #[serde(default)]
    pub hidden_dataset_access_attempts: u64,
    /// 访问 Secret 的尝试次数。
    #[serde(default)]
    pub secret_access_attempts: u64,
    /// Candidate 或评测制品完整性失败数。
    #[serde(default)]
    pub artifact_integrity_failures: u64,
    /// 审计链完整性失败数。
    #[serde(default)]
    pub audit_integrity_failures: u64,
    /// 本次尝试覆盖的 Critical 安全 Case 数。
    #[serde(default)]
    pub critical_cases_total: u64,
    /// 本次尝试通过的 Critical 安全 Case 数。
    #[serde(default)]
    pub critical_cases_passed: u64,
}

/// 同一 TaskCase 的一次独立 Repeat 结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAttemptResult {
    /// TaskCase 稳定标识；不得包含题目正文或答案。
    pub task_case_id: String,
    /// 从 0 开始的 Repeat 序号。
    pub repeat_index: u32,
    /// 运行终态。
    pub status: TaskAttemptStatus,
    /// 可信 Verifier 的显式判定；缺失时该尝试不得计为成功。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_passed: Option<bool>,
    /// 资源使用量。
    #[serde(default)]
    pub usage: EvaluationUsage,
    /// 可信安全 Verifier 输出；旧报告或未执行时为 `None`，不得解释为零失败。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety: Option<SafetyAttemptSummary>,
    /// 运行对应的真实 Run ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
}

/// TaskCase 的非敏感元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCaseMetadata {
    /// TaskCase 稳定标识。
    pub task_case_id: String,
    /// 数据集中的任务族；能力热力图只能使用该字段分组。
    pub task_family: String,
    /// 所属数据集用途。
    pub dataset_kind: DatasetKind,
    /// 是否属于不可丢失的 Critical Regression Case。
    #[serde(default)]
    pub critical: bool,
    /// 是否由完全确定性的模型、Fixture 和 Verifier 构成。
    #[serde(default)]
    pub deterministic: bool,
    /// 父版本被视为通过该 Case 的门槛；缺失时由可信策略决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_threshold: Option<f64>,
}

/// 一个 TaskCase 的全部 Repeat 结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCaseResult {
    /// 不含题目正文与答案的元数据。
    pub metadata: TaskCaseMetadata,
    /// 按 `repeat_index` 唯一排列的真实运行结果。
    #[serde(default)]
    pub attempts: Vec<TaskAttemptResult>,
}

/// 一次 Genome 在完整 Evaluation Dataset 上的运行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationRun {
    /// Evaluation 运行标识。
    pub run_id: EvaluationRunId,
    /// 被评测的 Genome 修订。
    pub genome_revision: GenomeRevisionId,
    /// 可比环境快照。
    pub environment: EvaluationEnvironment,
    /// 四类数据集的版本；缺失某类时评分卡对应指标为 `N/A`。
    #[serde(default)]
    pub datasets: BTreeMap<DatasetKind, DatasetVersionId>,
    /// 真实 TaskCase 聚合输入；不含 Hidden 正文或答案。
    #[serde(default)]
    pub task_cases: Vec<TaskCaseResult>,
}

/// Commit Gate 的可信决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    /// 全部硬门槛通过。
    Pass,
    /// 等待人工批准，尚未晋升。
    RequireApproval,
    /// 至少一个硬门槛拒绝 Candidate。
    Reject,
    /// Gate 尚未运行或旧报告缺少可信结果。
    #[default]
    Unknown,
}

/// Candidate 的发布生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionLifecycle {
    /// 尚未完成评测的 Candidate。
    Candidate,
    /// 已生成可信 EvaluationReport。
    Evaluated,
    /// 行为通过且满足自动晋升资格。
    Eligible,
    /// Gate 要求人工批准。
    RequireApproval,
    /// 已更新 Stable 引用，但尚未完成继承验证。
    Promoted,
    /// Promotion 已通过重启、新 Session 与摘要验证。
    InheritanceVerified,
    /// 已从该发布回滚。
    RolledBack,
    /// Gate 已拒绝。
    Rejected,
    /// 因安全或完整性失败被隔离。
    Quarantined,
}

/// Promotion 后的继承验证结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritanceVerification {
    /// 预期由新进程和新 Session 使用的 Genome。
    pub expected_genome: GenomeRevisionId,
    /// 进程重启后实际加载的 Genome。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_genome_after_restart: Option<GenomeRevisionId>,
    /// 重启验证通过数。
    #[serde(default)]
    pub restart_cases_passed: u32,
    /// 重启验证总数。
    #[serde(default)]
    pub restart_cases_total: u32,
    /// 新 Session 验证通过数。
    #[serde(default)]
    pub new_session_cases_passed: u32,
    /// 新 Session 验证总数。
    #[serde(default)]
    pub new_session_cases_total: u32,
    /// 旧 Session 是否继续使用 Parent Genome。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_session_parent_preserved: Option<bool>,
    /// Stable Ref 是否指向 Candidate。
    #[serde(default)]
    pub stable_reference_verified: bool,
    /// Registry 中的 Genome 摘要是否匹配。
    #[serde(default)]
    pub genome_digest_verified: bool,
    /// 可信继承 Verifier 的最终结论。
    #[serde(default)]
    pub verified: bool,
}

/// 一份同时绑定 Parent 与 Candidate 的可信 Evaluation 制品。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationReport {
    /// 报告结构版本；未知版本必须拒绝计算。
    pub schema_version: u32,
    /// 报告标识。
    pub report_id: EvaluationReportId,
    /// Parent 评测结果。
    pub parent: EvaluationRun,
    /// Candidate 评测结果。
    pub candidate: EvaluationRun,
    /// Candidate 的 Genome 差异。
    pub genome_diff: GenomeDiff,
    /// 本轮可信策略允许的变异表面。
    #[serde(default)]
    pub allowed_mutation_surfaces: BTreeSet<MutationSurface>,
    /// Commit Gate 决策。
    #[serde(default)]
    pub gate_decision: GateDecision,
    /// 当前发布生命周期。
    pub lifecycle: EvolutionLifecycle,
    /// Promotion 发布记录；未发布时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_record: Option<ReleaseId>,
    /// Promotion 后的继承验证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inheritance: Option<InheritanceVerification>,
    /// Evaluation 制品哈希是否已由可信控制面验证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_integrity_verified: Option<bool>,
    /// 审计链是否已由可信控制面验证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_integrity_verified: Option<bool>,
    /// Hidden Dataset 隔离策略是否已被可信控制面验证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_dataset_isolated: Option<bool>,
    /// 报告生成时间，使用 Unix 毫秒便于稳定排序。
    pub generated_at_ms: u64,
}

impl EvaluationReport {
    /// 校验不依赖存储查询的结构不变量。
    ///
    /// # Errors
    ///
    /// Schema 版本未知、Parent/Candidate 修订相同、TaskCase 与 Attempt 标识不一致，
    /// 或 Repeat 序号重复时返回 [`InvalidEvaluationReport`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluationReport> {
        if self.schema_version != EVALUATION_REPORT_SCHEMA_VERSION {
            return Err(InvalidEvaluationReport::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: EVALUATION_REPORT_SCHEMA_VERSION,
            });
        }
        if self.parent.genome_revision == self.candidate.genome_revision {
            return Err(InvalidEvaluationReport::SameRevision);
        }
        validate_run(&self.parent)?;
        validate_run(&self.candidate)?;
        Ok(())
    }
}

/// EvaluationReport 结构校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEvaluationReport {
    /// 报告使用了当前实现无法解释的版本。
    #[error("不支持的 EvaluationReport schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchemaVersion {
        /// 报告声明的版本。
        found: u32,
        /// 当前实现支持的版本。
        supported: u32,
    },
    /// Parent 与 Candidate 指向同一修订。
    #[error("Parent 与 Candidate 不能是同一 Genome 修订")]
    SameRevision,
    /// 同一运行内 TaskCase 标识重复。
    #[error("EvaluationRun 中 TaskCase 标识重复：{0}")]
    DuplicateTaskCase(String),
    /// Attempt 携带了与所属 Case 不同的标识。
    #[error("TaskAttemptResult `{attempt}` 不属于 TaskCase `{case_id}`")]
    AttemptCaseMismatch {
        /// 所属 TaskCase 标识。
        case_id: String,
        /// Attempt 声明的 TaskCase 标识。
        attempt: String,
    },
    /// 同一 TaskCase 的 Repeat 序号重复。
    #[error("TaskCase `{case_id}` 的 repeat_index {repeat_index} 重复")]
    DuplicateRepeat {
        /// TaskCase 标识。
        case_id: String,
        /// 重复序号。
        repeat_index: u32,
    },
}

/// 校验单个 EvaluationRun 的 Case 与 Repeat 唯一性。
fn validate_run(run: &EvaluationRun) -> Result<(), InvalidEvaluationReport> {
    let mut case_ids = BTreeSet::new();
    for case in &run.task_cases {
        if !case_ids.insert(case.metadata.task_case_id.clone()) {
            return Err(InvalidEvaluationReport::DuplicateTaskCase(
                case.metadata.task_case_id.clone(),
            ));
        }
        let mut repeats = BTreeSet::new();
        for attempt in &case.attempts {
            if attempt.task_case_id != case.metadata.task_case_id {
                return Err(InvalidEvaluationReport::AttemptCaseMismatch {
                    case_id: case.metadata.task_case_id.clone(),
                    attempt: attempt.task_case_id.clone(),
                });
            }
            if !repeats.insert(attempt.repeat_index) {
                return Err(InvalidEvaluationReport::DuplicateRepeat {
                    case_id: case.metadata.task_case_id.clone(),
                    repeat_index: attempt.repeat_index,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小报告，供结构校验测试按需修改。
    fn report() -> EvaluationReport {
        let environment = EvaluationEnvironment {
            kernel_ref: "kernel-1".into(),
            model_provider: "fixture".into(),
            model: "deterministic".into(),
            model_parameters_digest: "model-params".into(),
            tool_profile_digest: "tools".into(),
            execution_profile_digest: "evaluation".into(),
            plugin_set_digest: "plugins".into(),
            capability_owner_digest: "owners".into(),
            resource_budget_digest: "budget".into(),
            verifier_version: "verifier-1".into(),
            evaluation_policy_version: "policy-1".into(),
            environment_fixture_digest: "fixture-1".into(),
            repeat_count: 1,
        };
        let case = TaskCaseResult {
            metadata: TaskCaseMetadata {
                task_case_id: "repair-1".into(),
                task_family: "修复".into(),
                dataset_kind: DatasetKind::Repair,
                critical: false,
                deterministic: true,
                pass_threshold: None,
            },
            attempts: vec![TaskAttemptResult {
                task_case_id: "repair-1".into(),
                repeat_index: 0,
                status: TaskAttemptStatus::Passed,
                verifier_passed: Some(true),
                usage: EvaluationUsage::default(),
                safety: Some(SafetyAttemptSummary::default()),
                run_id: None,
            }],
        };
        EvaluationReport {
            schema_version: EVALUATION_REPORT_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            parent: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: GenomeRevisionId::generate(),
                environment: environment.clone(),
                datasets: BTreeMap::new(),
                task_cases: vec![case.clone()],
            },
            candidate: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: GenomeRevisionId::generate(),
                environment,
                datasets: BTreeMap::new(),
                task_cases: vec![case],
            },
            genome_diff: GenomeDiff::default(),
            allowed_mutation_surfaces: BTreeSet::new(),
            gate_decision: GateDecision::Unknown,
            lifecycle: EvolutionLifecycle::Evaluated,
            release_record: None,
            inheritance: None,
            artifact_integrity_verified: Some(true),
            audit_integrity_verified: Some(true),
            hidden_dataset_isolated: Some(true),
            generated_at_ms: 0,
        }
    }

    #[test]
    fn validates_case_and_repeat_identity() {
        let mut value = report();
        value.validate().expect("合法报告应通过");
        value.candidate.task_cases[0].attempts[0].task_case_id = "other".into();
        assert!(matches!(
            value.validate(),
            Err(InvalidEvaluationReport::AttemptCaseMismatch { .. })
        ));
    }

    #[test]
    fn safety_absence_is_not_deserialized_as_pass() {
        let encoded = serde_json::to_value(report()).expect("报告应可序列化");
        let safety = encoded.pointer("/parent/task_cases/0/attempts/0/safety");
        assert!(safety.is_some(), "显式安全结果应被保留");
        let mut legacy = encoded;
        legacy["parent"]["task_cases"][0]["attempts"][0]
            .as_object_mut()
            .expect("Attempt 应是对象")
            .remove("safety");
        let decoded: EvaluationReport = serde_json::from_value(legacy).expect("旧字段应可读取");
        assert_eq!(decoded.parent.task_cases[0].attempts[0].safety, None);
    }
}
