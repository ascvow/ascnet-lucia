//! M6 Context Policy 的版本化参数、候选与可信评测协议。
//!
//! 本模块只定义稳定数据契约，不实现上下文压缩、插件装配或 Commit Gate。策略正文以
//! 规范 JSON 写入 Artifact CAS，Genome 仅通过 `PolicyRef.config_digest` 引用它。

use crate::{
    ArtifactDigest, CandidateId, EpisodeId, EvolutionCycleId, GateDecision, GenomeDigest,
    GenomeRevisionId, MutationId, MutationSurface,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

/// 当前支持的 Context Policy 结构版本。
pub const CONTEXT_POLICY_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Context Policy 变异提案结构版本。
pub const CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Context Policy Candidate 结构版本。
pub const CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Context Policy 评测观察结构版本。
pub const CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Context Policy 评测报告结构版本。
pub const CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION: u32 = 1;

/// 自动微压缩阈值允许的最小 token 数。
pub const MIN_CONTEXT_THRESHOLD_TOKENS: u32 = 4_096;
/// 自动压缩阈值允许的最大 token 数。
pub const MAX_CONTEXT_THRESHOLD_TOKENS: u32 = 2_000_000;
/// 最近原文消息允许的最大条数。
pub const MAX_RECENT_MESSAGE_COUNT: u16 = 256;
/// 单类固定区允许保留的最大结构化条目数。
pub const MAX_PINNED_ITEM_COUNT: u16 = 256;
/// ToolResult 策略允许保留的最大近期成功结果数。
pub const MAX_RECENT_TOOL_RESULT_COUNT: u16 = 64;
/// 摘要输出预算允许的最小 token 数。
pub const MIN_SUMMARY_TOKEN_BUDGET: u32 = 256;
/// 摘要输出预算允许的最大 token 数。
pub const MAX_SUMMARY_TOKEN_BUDGET: u32 = 32_768;
/// 摘要结构化标记覆盖率的安全下限，单位为万分比。
pub const MIN_SUMMARY_VALIDATION_COVERAGE_BPS: u16 = 9_500;
/// 提案假设文本允许的最大 UTF-8 字节数。
pub const MAX_CONTEXT_HYPOTHESIS_BYTES: usize = 4 * 1_024;

/// 旧 ToolResult 的保留策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultRetentionPolicyV1 {
    /// 原样保留全部 ToolResult；适合短上下文或工具状态高度敏感的任务。
    PreserveAll,
    /// 原样保留全部错误结果及指定数量的近期成功结果，较早成功结果仅保留调用关联与状态。
    PreserveErrorsAndRecent {
        /// 按消息顺序从新到旧保留的成功 ToolResult 数量。
        recent_successful_results: u16,
    },
}

impl Default for ToolResultRetentionPolicyV1 {
    fn default() -> Self {
        Self::PreserveErrorsAndRecent {
            recent_successful_results: 3,
        }
    }
}

/// 结构化用户约束固定区策略。
///
/// 两个变体都要求约束由上游显式标注并携带稳定 ID；禁止从自然语言启发式猜测约束。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserConstraintRetentionPolicyV1 {
    /// 固定区逐字保留结构化约束，摘要正文无需重复。
    PinnedStructured {
        /// 固定区最多保留的约束数；超过上限时运行期必须失败关闭。
        max_items: u16,
    },
    /// 固定区逐字保留结构化约束，同时要求摘要结构化结果确认这些约束 ID。
    PinnedStructuredAndSummary {
        /// 固定区最多保留的约束数；超过上限时运行期必须失败关闭。
        max_items: u16,
    },
}

impl Default for UserConstraintRetentionPolicyV1 {
    fn default() -> Self {
        Self::PinnedStructuredAndSummary { max_items: 64 }
    }
}

/// 版本化 Plan snapshot 的保留策略。
///
/// 两个变体都要求使用 plan-plugin 提供的结构化只读快照，不允许从提示文本反推计划状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSnapshotRetentionPolicyV1 {
    /// 在固定区保留最新完整快照及其 schema 版本和修订号。
    LatestSnapshot {
        /// 单个快照允许的最大计划项数；超过上限时运行期必须失败关闭。
        max_items: u16,
    },
    /// 保留最新完整快照，并要求摘要结构化结果确认相同修订号。
    LatestSnapshotAndSummary {
        /// 单个快照允许的最大计划项数；超过上限时运行期必须失败关闭。
        max_items: u16,
    },
}

impl Default for PlanSnapshotRetentionPolicyV1 {
    fn default() -> Self {
        Self::LatestSnapshotAndSummary { max_items: 100 }
    }
}

/// 摘要后的确定性验证算法。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostSummaryValidationAlgorithmV1 {
    /// 验证结构化摘要信封中的稳定标记集合。
    ///
    /// 实现必须拒绝空摘要；对约束 ID、ToolResult 调用 ID 和事实 ID 分别去重后精确比较
    /// UTF-8 字节，并要求 Plan 修订号完全相等。覆盖率按“命中的必需标记数 / 必需标记总数”
    /// 计算，不能使用语义相似度或模型自评分；缺少结构化信封时失败关闭。
    StructuredMarkerCoverageV1 {
        /// 必需标记的最低覆盖率，单位为万分比；安全下限由协议固定为 95%。
        min_coverage_bps: u16,
    },
}

impl Default for PostSummaryValidationAlgorithmV1 {
    fn default() -> Self {
        Self::StructuredMarkerCoverageV1 {
            min_coverage_bps: 10_000,
        }
    }
}

/// M6 可进化的 Context Policy 参数。
///
/// `#[serde(default)]` 允许同一 V1 schema 后续增加带默认值的可选字段；改变已有字段含义、
/// 删除字段或收紧枚举必须升级 [`CONTEXT_POLICY_SCHEMA_VERSION`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextPolicyV1 {
    /// 策略结构版本。
    pub schema_version: u32,
    /// 触发旧 ToolResult 清理的估算 token 水位。
    pub micro_compact_threshold_tokens: u32,
    /// 触发模型摘要的估算 token 水位；必须严格大于微压缩水位。
    pub full_compact_threshold_tokens: u32,
    /// 完整压缩后逐条原样保留的最近消息数，而不是 token 目标或 API 轮次数。
    pub recent_message_count: u16,
    /// ToolResult 的原文、错误和调用关联保留规则。
    pub tool_result_retention: ToolResultRetentionPolicyV1,
    /// 显式结构化用户约束的固定区规则。
    pub user_constraints: UserConstraintRetentionPolicyV1,
    /// plan-plugin 结构化只读快照的固定区规则。
    pub plan_snapshot: PlanSnapshotRetentionPolicyV1,
    /// 单次摘要模型调用允许的最大输出 token 数。
    pub summary_token_budget: u32,
    /// 摘要返回后、替换真实上下文前必须执行的确定性验证算法。
    pub post_summary_validation: PostSummaryValidationAlgorithmV1,
}

impl Default for ContextPolicyV1 {
    fn default() -> Self {
        Self {
            schema_version: CONTEXT_POLICY_SCHEMA_VERSION,
            micro_compact_threshold_tokens: 120_000,
            full_compact_threshold_tokens: 167_000,
            recent_message_count: 8,
            tool_result_retention: ToolResultRetentionPolicyV1::default(),
            user_constraints: UserConstraintRetentionPolicyV1::default(),
            plan_snapshot: PlanSnapshotRetentionPolicyV1::default(),
            summary_token_budget: 20_000,
            post_summary_validation: PostSummaryValidationAlgorithmV1::default(),
        }
    }
}

impl ContextPolicyV1 {
    /// 校验 Context Policy 的版本、范围关系和不可关闭的安全下限。
    ///
    /// # Errors
    ///
    /// schema 不受支持、阈值顺序错误、任一数量越界、摘要预算无法装入完整压缩水位，
    /// 或摘要验证覆盖率低于安全下限时返回 [`InvalidContextPolicy`]。
    pub fn validate(&self) -> Result<(), InvalidContextPolicy> {
        if self.schema_version != CONTEXT_POLICY_SCHEMA_VERSION {
            return Err(InvalidContextPolicy::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CONTEXT_POLICY_SCHEMA_VERSION,
            });
        }
        validate_range(
            "micro_compact_threshold_tokens",
            self.micro_compact_threshold_tokens,
            MIN_CONTEXT_THRESHOLD_TOKENS,
            MAX_CONTEXT_THRESHOLD_TOKENS,
        )?;
        validate_range(
            "full_compact_threshold_tokens",
            self.full_compact_threshold_tokens,
            MIN_CONTEXT_THRESHOLD_TOKENS,
            MAX_CONTEXT_THRESHOLD_TOKENS,
        )?;
        if self.micro_compact_threshold_tokens >= self.full_compact_threshold_tokens {
            return Err(InvalidContextPolicy::InvalidThresholdOrder {
                micro: self.micro_compact_threshold_tokens,
                full: self.full_compact_threshold_tokens,
            });
        }
        validate_count(
            "recent_message_count",
            self.recent_message_count,
            MAX_RECENT_MESSAGE_COUNT,
        )?;
        if let ToolResultRetentionPolicyV1::PreserveErrorsAndRecent {
            recent_successful_results,
        } = self.tool_result_retention
        {
            validate_count(
                "tool_result_retention.recent_successful_results",
                recent_successful_results,
                MAX_RECENT_TOOL_RESULT_COUNT,
            )?;
        }
        validate_count(
            "user_constraints.max_items",
            constraint_limit(&self.user_constraints),
            MAX_PINNED_ITEM_COUNT,
        )?;
        validate_count(
            "plan_snapshot.max_items",
            plan_limit(&self.plan_snapshot),
            MAX_PINNED_ITEM_COUNT,
        )?;
        validate_range(
            "summary_token_budget",
            self.summary_token_budget,
            MIN_SUMMARY_TOKEN_BUDGET,
            MAX_SUMMARY_TOKEN_BUDGET,
        )?;
        if self.summary_token_budget >= self.full_compact_threshold_tokens {
            return Err(InvalidContextPolicy::SummaryBudgetExhaustsThreshold {
                budget: self.summary_token_budget,
                full_threshold: self.full_compact_threshold_tokens,
            });
        }
        let PostSummaryValidationAlgorithmV1::StructuredMarkerCoverageV1 { min_coverage_bps } =
            self.post_summary_validation;
        if !(MIN_SUMMARY_VALIDATION_COVERAGE_BPS..=10_000).contains(&min_coverage_bps) {
            return Err(InvalidContextPolicy::InvalidValidationCoverage {
                actual: min_coverage_bps,
                minimum: MIN_SUMMARY_VALIDATION_COVERAGE_BPS,
            });
        }
        Ok(())
    }

    /// 返回通过校验的规范 JSON 字节，用于写入 Artifact CAS。
    ///
    /// # Errors
    ///
    /// 策略不合法或 JSON 序列化失败时返回 [`InvalidContextPolicy`]。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, InvalidContextPolicy> {
        self.validate()?;
        serde_json::to_vec(self)
            .map_err(|error| InvalidContextPolicy::Serialization(error.to_string()))
    }

    /// 从 JSON 字节解析并校验一份 Context Policy。
    ///
    /// # Errors
    ///
    /// JSON 无法解析或策略不满足边界时返回 [`InvalidContextPolicy`]。
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, InvalidContextPolicy> {
        let policy: Self = serde_json::from_slice(bytes)
            .map_err(|error| InvalidContextPolicy::Serialization(error.to_string()))?;
        policy.validate()?;
        Ok(policy)
    }
}

/// Context Policy 的受限变异提案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicyMutationProposalV1 {
    /// 提案结构版本。
    pub schema_version: u32,
    /// 本次变异的稳定标识。
    pub mutation_id: MutationId,
    /// 提案绑定的 Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// 提案生成时观察到的 Parent Genome 摘要。
    pub parent_genome_digest: GenomeDigest,
    /// 提案生成时观察到的 Parent Context Policy CAS 摘要。
    pub parent_policy_digest: ArtifactDigest,
    /// 候选策略结构值；可信 Builder 负责规范化并写入 CAS。
    pub candidate_policy: ContextPolicyV1,
    /// 支撑本次变异的脱敏 Episode ID，禁止携带 Episode 正文或 Hidden 数据。
    #[serde(default)]
    pub evidence_episode_ids: BTreeSet<EpisodeId>,
    /// 不含 Secret、原始 ToolResult 或 Hidden 内容的有界变异假设。
    pub hypothesis: String,
}

impl ContextPolicyMutationProposalV1 {
    /// 校验提案版本、策略、证据和有界文本。
    ///
    /// # Errors
    ///
    /// schema 不受支持、候选策略无效、缺少证据或假设为空/过长时返回
    /// [`InvalidContextMutation`]。
    pub fn validate(&self) -> Result<(), InvalidContextMutation> {
        if self.schema_version != CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION {
            return Err(InvalidContextMutation::UnsupportedProposalSchema {
                found: self.schema_version,
                supported: CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION,
            });
        }
        self.candidate_policy
            .validate()
            .map_err(InvalidContextMutation::InvalidPolicy)?;
        if self.evidence_episode_ids.is_empty() {
            return Err(InvalidContextMutation::MissingEvidence);
        }
        let hypothesis = self.hypothesis.trim();
        if hypothesis.is_empty() || hypothesis.len() > MAX_CONTEXT_HYPOTHESIS_BYTES {
            return Err(InvalidContextMutation::InvalidHypothesis {
                max_bytes: MAX_CONTEXT_HYPOTHESIS_BYTES,
            });
        }
        Ok(())
    }
}

/// 可信 Builder 产生的 Context Policy Candidate 回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicyCandidateV1 {
    /// Candidate 结构版本。
    pub schema_version: u32,
    /// Candidate 稳定标识。
    pub candidate_id: CandidateId,
    /// Candidate 所属的进化周期。
    pub cycle_id: EvolutionCycleId,
    /// 产生 Candidate 的变异标识。
    pub mutation_id: MutationId,
    /// Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Candidate Genome 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// Parent Genome 行为摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Candidate Genome 行为摘要。
    pub candidate_genome_digest: GenomeDigest,
    /// Parent Context Policy CAS 摘要。
    pub parent_policy_digest: ArtifactDigest,
    /// Candidate Context Policy CAS 摘要。
    pub candidate_policy_digest: ArtifactDigest,
    /// 可信完整 Genome Diff 计算出的实际变化表面。
    #[serde(default)]
    pub changed_surfaces: BTreeSet<MutationSurface>,
    /// 可信控制面记录的创建时间，Unix 毫秒。
    pub created_at_ms: u64,
}

impl ContextPolicyCandidateV1 {
    /// 校验 Candidate 的绑定关系和唯一 Context Policy 差异。
    ///
    /// # Errors
    ///
    /// schema 不受支持、Parent/Candidate 未发生变化，或差异表面不是精确的
    /// `{ContextPolicy}` 时返回 [`InvalidContextMutation`]。
    pub fn validate(&self) -> Result<(), InvalidContextMutation> {
        if self.schema_version != CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION {
            return Err(InvalidContextMutation::UnsupportedCandidateSchema {
                found: self.schema_version,
                supported: CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION,
            });
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidContextMutation::SameRevision);
        }
        if self.parent_genome_digest == self.candidate_genome_digest {
            return Err(InvalidContextMutation::SameGenomeDigest);
        }
        if self.parent_policy_digest == self.candidate_policy_digest {
            return Err(InvalidContextMutation::SamePolicyDigest);
        }
        let expected = BTreeSet::from([MutationSurface::ContextPolicy]);
        if self.changed_surfaces != expected {
            return Err(InvalidContextMutation::InvalidCandidateSurfaces(
                self.changed_surfaces.clone(),
            ));
        }
        Ok(())
    }
}

/// 一项可验证目标的命中计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecallObservationV1 {
    /// Verifier 提供的必需目标总数，必须大于零。
    pub expected: u64,
    /// Candidate 输出中由 Verifier 确认命中的目标数，不得大于 `expected`。
    pub recalled: u64,
}

/// 可信 Runner 与 Verifier 产生的 Context Policy 原始观察值。
///
/// Candidate 与被测插件均不得直接写入最终指标；Gate 所有者根据这些计数确定性计算八项指标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEvaluationObservationV1 {
    /// 观察结构版本。
    pub schema_version: u32,
    /// 事实召回计数。
    pub facts: RecallObservationV1,
    /// 用户约束召回计数。
    pub constraints: RecallObservationV1,
    /// ToolResult 与工具调用状态召回计数。
    pub tool_states: RecallObservationV1,
    /// Plan snapshot 状态召回计数。
    pub plan_states: RecallObservationV1,
    /// 下游任务成功计数。
    pub downstream_tasks: RecallObservationV1,
    /// 压缩前同一上下文的估算或 Provider 报告 token 数，必须大于零。
    pub tokens_before: u64,
    /// 压缩后实际发送给模型的 token 数，不得大于 `tokens_before`。
    pub tokens_after: u64,
    /// 可信计费器计算的总成本，单位为最小货币单位的百万分之一。
    pub cost_microunits: u64,
    /// Runner 观测的端到端墙钟延迟，单位毫秒。
    pub latency_ms: u64,
}

impl ContextEvaluationObservationV1 {
    /// 校验观察计数和 token 单调关系。
    ///
    /// # Errors
    ///
    /// schema 不受支持、任一目标总数为零、命中数超出总数，或压缩后 token 增加时返回
    /// [`InvalidContextEvaluation`]。
    pub fn validate(&self) -> Result<(), InvalidContextEvaluation> {
        if self.schema_version != CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION {
            return Err(InvalidContextEvaluation::UnsupportedObservationSchema {
                found: self.schema_version,
                supported: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
            });
        }
        for (field, observation) in [
            ("facts", self.facts),
            ("constraints", self.constraints),
            ("tool_states", self.tool_states),
            ("plan_states", self.plan_states),
            ("downstream_tasks", self.downstream_tasks),
        ] {
            if observation.expected == 0 || observation.recalled > observation.expected {
                return Err(InvalidContextEvaluation::InvalidRecallCount {
                    field,
                    expected: observation.expected,
                    recalled: observation.recalled,
                });
            }
        }
        if self.tokens_before == 0 || self.tokens_after > self.tokens_before {
            return Err(InvalidContextEvaluation::InvalidTokenCounts {
                before: self.tokens_before,
                after: self.tokens_after,
            });
        }
        Ok(())
    }
}

/// M6 要求的八项 Context Policy 指标。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextEvaluationMetricsV1 {
    /// 事实召回率，单位为万分比。
    pub fact_recall_bps: u16,
    /// 用户约束召回率，单位为万分比。
    pub constraint_recall_bps: u16,
    /// 工具状态召回率，单位为万分比。
    pub tool_state_recall_bps: u16,
    /// Plan 状态召回率，单位为万分比。
    pub plan_state_recall_bps: u16,
    /// 下游任务成功率，单位为万分比。
    pub downstream_task_success_bps: u16,
    /// 相对压缩前上下文减少的 token 比例，单位为万分比。
    pub token_reduction_bps: u16,
    /// 可信计费器计算的总成本，单位为最小货币单位的百万分之一。
    pub cost_microunits: u64,
    /// Runner 观测的端到端墙钟延迟，单位毫秒。
    pub latency_ms: u64,
}

impl ContextEvaluationMetricsV1 {
    /// 校验六项比例指标均位于 0% 到 100% 之间。
    ///
    /// # Errors
    ///
    /// 任一万分比大于 10000 时返回 [`InvalidContextEvaluation`]。
    pub fn validate(&self) -> Result<(), InvalidContextEvaluation> {
        for (field, value) in [
            ("fact_recall_bps", self.fact_recall_bps),
            ("constraint_recall_bps", self.constraint_recall_bps),
            ("tool_state_recall_bps", self.tool_state_recall_bps),
            ("plan_state_recall_bps", self.plan_state_recall_bps),
            (
                "downstream_task_success_bps",
                self.downstream_task_success_bps,
            ),
            ("token_reduction_bps", self.token_reduction_bps),
        ] {
            if value > 10_000 {
                return Err(InvalidContextEvaluation::InvalidMetricBps { field, value });
            }
        }
        Ok(())
    }
}

/// 固定 M6 Gate 可报告的失败类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextGateFailureV1 {
    /// 事实召回未达到门槛或相对 Parent 回退。
    FactRecall,
    /// 用户约束未被完整保留或相对 Parent 回退。
    ConstraintRecall,
    /// 工具状态召回未达到门槛或相对 Parent 回退。
    ToolStateRecall,
    /// Plan 状态未被完整保留或相对 Parent 回退。
    PlanStateRecall,
    /// 下游任务成功率未达到门槛或相对 Parent 回退。
    DownstreamTaskSuccess,
    /// token 缩减率未达到门槛或相对 Parent 回退。
    TokenReduction,
    /// 成本超过固定的 Parent 相对上限。
    Cost,
    /// 延迟超过固定绝对上限。
    Latency,
    /// Parent/Candidate 的真实 Genome 差异不是唯一 Context Policy。
    GenomeDiff,
}

/// 固定 M6 Gate 产生的 Context Policy 对照评测报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicyEvaluationReportV1 {
    /// 报告结构版本。
    pub schema_version: u32,
    /// 固定 Gate 策略版本；阈值变化时必须更换此值。
    pub gate_version: String,
    /// Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Candidate Genome 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// Parent 的八项可信指标。
    pub parent_metrics: ContextEvaluationMetricsV1,
    /// Candidate 的八项可信指标。
    pub candidate_metrics: ContextEvaluationMetricsV1,
    /// Gate 决策；M6 自动 Gate 只产生 `Pass` 或 `Reject`。
    pub decision: GateDecision,
    /// 硬失败集合；为空当且仅当决策为 `Pass`。
    #[serde(default)]
    pub failures: BTreeSet<ContextGateFailureV1>,
}

impl ContextPolicyEvaluationReportV1 {
    /// 校验评测报告的版本、修订绑定、指标和决策一致性。
    ///
    /// `expected_gate_version` 由可信 Gate 实现传入，阻止旧阈值报告被当前发布流程接受。
    ///
    /// # Errors
    ///
    /// 报告版本错误、Gate 版本错绑、修订相同、指标越界，或决策与失败集合不一致时返回
    /// [`InvalidContextEvaluation`]。
    pub fn validate(&self, expected_gate_version: &str) -> Result<(), InvalidContextEvaluation> {
        if self.schema_version != CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION {
            return Err(InvalidContextEvaluation::UnsupportedReportSchema {
                found: self.schema_version,
                supported: CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION,
            });
        }
        if self.gate_version != expected_gate_version {
            return Err(InvalidContextEvaluation::GateVersionMismatch {
                expected: expected_gate_version.to_string(),
                actual: self.gate_version.clone(),
            });
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidContextEvaluation::SameRevision);
        }
        self.parent_metrics.validate()?;
        self.candidate_metrics.validate()?;
        let consistent = matches!(self.decision, GateDecision::Pass) && self.failures.is_empty()
            || matches!(self.decision, GateDecision::Reject) && !self.failures.is_empty();
        if !consistent {
            return Err(InvalidContextEvaluation::InconsistentDecision);
        }
        Ok(())
    }
}

/// Context Policy 参数校验错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidContextPolicy {
    /// schema 版本不受支持。
    #[error("不支持的 ContextPolicy schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchemaVersion {
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// u32 参数不在闭区间内。
    #[error("ContextPolicy 参数 `{field}`={actual} 超出范围 {min}..={max}")]
    InvalidRange {
        /// 参数名。
        field: &'static str,
        /// 实际值。
        actual: u32,
        /// 最小值。
        min: u32,
        /// 最大值。
        max: u32,
    },
    /// 数量参数为零或超过上限。
    #[error("ContextPolicy 参数 `{field}`={actual} 必须位于 1..={max}")]
    InvalidCount {
        /// 参数名。
        field: &'static str,
        /// 实际值。
        actual: u16,
        /// 最大值。
        max: u16,
    },
    /// 微压缩阈值没有严格小于完整压缩阈值。
    #[error("微压缩阈值 {micro} 必须严格小于完整压缩阈值 {full}")]
    InvalidThresholdOrder {
        /// 微压缩阈值。
        micro: u32,
        /// 完整压缩阈值。
        full: u32,
    },
    /// 摘要预算达到或超过完整压缩阈值。
    #[error("摘要 token 预算 {budget} 必须小于完整压缩阈值 {full_threshold}")]
    SummaryBudgetExhaustsThreshold {
        /// 摘要 token 预算。
        budget: u32,
        /// 完整压缩阈值。
        full_threshold: u32,
    },
    /// 摘要验证覆盖率低于安全下限或超过 100%。
    #[error("摘要验证覆盖率 {actual}bps 必须位于 {minimum}..=10000")]
    InvalidValidationCoverage {
        /// 实际覆盖率。
        actual: u16,
        /// 协议安全下限。
        minimum: u16,
    },
    /// JSON 编解码失败。
    #[error("ContextPolicy JSON 处理失败：{0}")]
    Serialization(String),
}

/// Context Policy 提案或 Candidate 的结构错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidContextMutation {
    /// 提案 schema 版本不受支持。
    #[error("不支持的 ContextPolicyProposal schema 版本 {found}，当前支持 {supported}")]
    UnsupportedProposalSchema {
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// Candidate schema 版本不受支持。
    #[error("不支持的 ContextPolicyCandidate schema 版本 {found}，当前支持 {supported}")]
    UnsupportedCandidateSchema {
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// 候选策略无效。
    #[error("Candidate ContextPolicy 无效：{0}")]
    InvalidPolicy(InvalidContextPolicy),
    /// 提案没有脱敏 Episode 证据。
    #[error("ContextPolicy 提案必须至少绑定一条脱敏 Episode")]
    MissingEvidence,
    /// 提案假设为空或过长。
    #[error("ContextPolicy 提案 hypothesis 必须是非空且不超过 {max_bytes} 字节的文本")]
    InvalidHypothesis {
        /// 最大 UTF-8 字节数。
        max_bytes: usize,
    },
    /// Parent 与 Candidate Revision 相同。
    #[error("ContextPolicy Candidate 的 Parent 与 Candidate Revision 不能相同")]
    SameRevision,
    /// Parent 与 Candidate Genome 摘要相同。
    #[error("ContextPolicy Candidate 的 Parent 与 Candidate GenomeDigest 不能相同")]
    SameGenomeDigest,
    /// Parent 与 Candidate 策略摘要相同。
    #[error("ContextPolicy Candidate 的 Parent 与 Candidate PolicyDigest 不能相同")]
    SamePolicyDigest,
    /// Candidate 变化表面不是唯一 Context Policy。
    #[error("M6 Candidate 的可信 Diff 必须精确包含 ContextPolicy，实际为 {0:?}")]
    InvalidCandidateSurfaces(BTreeSet<MutationSurface>),
}

/// Context Policy 观察、指标或报告错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidContextEvaluation {
    /// 观察 schema 版本不受支持。
    #[error("不支持的 ContextEvaluationObservation schema 版本 {found}，当前支持 {supported}")]
    UnsupportedObservationSchema {
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// 报告 schema 版本不受支持。
    #[error("不支持的 ContextEvaluationReport schema 版本 {found}，当前支持 {supported}")]
    UnsupportedReportSchema {
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// 召回计数无有效分母或命中数超过总数。
    #[error("Context 指标 `{field}` 计数无效：recalled={recalled}, expected={expected}")]
    InvalidRecallCount {
        /// 指标字段。
        field: &'static str,
        /// 必需目标总数。
        expected: u64,
        /// 命中目标数。
        recalled: u64,
    },
    /// 压缩 token 计数无效。
    #[error("Context token 计数无效：before={before}, after={after}")]
    InvalidTokenCounts {
        /// 压缩前 token 数。
        before: u64,
        /// 压缩后 token 数。
        after: u64,
    },
    /// 万分比指标超过 100%。
    #[error("Context 指标 `{field}`={value}bps 超过 10000")]
    InvalidMetricBps {
        /// 指标字段。
        field: &'static str,
        /// 实际万分比。
        value: u16,
    },
    /// 报告 Gate 版本与当前可信实现不一致。
    #[error("Context Gate 版本错绑：期望 `{expected}`，实际 `{actual}`")]
    GateVersionMismatch {
        /// 当前可信 Gate 版本。
        expected: String,
        /// 报告声明版本。
        actual: String,
    },
    /// Parent 与 Candidate Revision 相同。
    #[error("Context 评测的 Parent 与 Candidate Revision 不能相同")]
    SameRevision,
    /// 决策与失败集合不一致。
    #[error("Context Gate 决策与失败集合不一致")]
    InconsistentDecision,
}

/// 校验一个 u32 参数是否位于闭区间内。
fn validate_range(
    field: &'static str,
    actual: u32,
    min: u32,
    max: u32,
) -> Result<(), InvalidContextPolicy> {
    if !(min..=max).contains(&actual) {
        return Err(InvalidContextPolicy::InvalidRange {
            field,
            actual,
            min,
            max,
        });
    }
    Ok(())
}

/// 校验一个 u16 数量是否位于 1 到上限之间。
fn validate_count(field: &'static str, actual: u16, max: u16) -> Result<(), InvalidContextPolicy> {
    if actual == 0 || actual > max {
        return Err(InvalidContextPolicy::InvalidCount { field, actual, max });
    }
    Ok(())
}

/// 返回用户约束策略声明的固定区上限。
fn constraint_limit(policy: &UserConstraintRetentionPolicyV1) -> u16 {
    match policy {
        UserConstraintRetentionPolicyV1::PinnedStructured { max_items }
        | UserConstraintRetentionPolicyV1::PinnedStructuredAndSummary { max_items } => *max_items,
    }
}

/// 返回 Plan snapshot 策略声明的条目上限。
fn plan_limit(policy: &PlanSnapshotRetentionPolicyV1) -> u16 {
    match policy {
        PlanSnapshotRetentionPolicyV1::LatestSnapshot { max_items }
        | PlanSnapshotRetentionPolicyV1::LatestSnapshotAndSummary { max_items } => *max_items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试使用的确定性摘要。
    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
    }

    /// 默认策略必须完整覆盖 M6 的七组参数并通过边界校验。
    #[test]
    fn default_policy_covers_all_m6_parameters() {
        let policy = ContextPolicyV1::default();

        policy.validate().expect("默认策略应合法");
        assert!(policy.micro_compact_threshold_tokens < policy.full_compact_threshold_tokens);
        assert!(policy.recent_message_count > 0);
        assert!(matches!(
            policy.tool_result_retention,
            ToolResultRetentionPolicyV1::PreserveErrorsAndRecent { .. }
        ));
        assert!(matches!(
            policy.user_constraints,
            UserConstraintRetentionPolicyV1::PinnedStructuredAndSummary { .. }
        ));
        assert!(matches!(
            policy.plan_snapshot,
            PlanSnapshotRetentionPolicyV1::LatestSnapshotAndSummary { .. }
        ));
        assert!(policy.summary_token_budget > 0);
        assert!(matches!(
            policy.post_summary_validation,
            PostSummaryValidationAlgorithmV1::StructuredMarkerCoverageV1 { .. }
        ));
    }

    /// V1 JSON 缺少后续加法字段时使用安全默认值，未知加法字段不会破坏旧读取器。
    #[test]
    fn v1_json_is_additive_compatible() {
        let policy = ContextPolicyV1::from_json_slice(
            br#"{"schema_version":1,"future_optional_field":{"enabled":true}}"#,
        )
        .expect("旧读取器应忽略未知加法字段并补齐默认值");

        assert_eq!(policy, ContextPolicyV1::default());
        assert_eq!(
            ContextPolicyV1::from_json_slice(&policy.canonical_bytes().expect("应规范序列化"))
                .expect("规范字节应可复读"),
            policy
        );
    }

    /// 阈值顺序、消息数和验证安全下限必须失败关闭。
    #[test]
    fn policy_rejects_out_of_bounds_values() {
        let mut policy = ContextPolicyV1 {
            micro_compact_threshold_tokens: 167_000,
            full_compact_threshold_tokens: 167_000,
            ..ContextPolicyV1::default()
        };
        assert!(matches!(
            policy.validate(),
            Err(InvalidContextPolicy::InvalidThresholdOrder { .. })
        ));

        policy = ContextPolicyV1 {
            recent_message_count: 0,
            ..ContextPolicyV1::default()
        };
        assert!(matches!(
            policy.validate(),
            Err(InvalidContextPolicy::InvalidCount {
                field: "recent_message_count",
                ..
            })
        ));

        policy = ContextPolicyV1 {
            post_summary_validation: PostSummaryValidationAlgorithmV1::StructuredMarkerCoverageV1 {
                min_coverage_bps: 9_499,
            },
            ..ContextPolicyV1::default()
        };
        assert!(matches!(
            policy.validate(),
            Err(InvalidContextPolicy::InvalidValidationCoverage { .. })
        ));
    }

    /// 提案必须绑定脱敏证据，Candidate 必须只有 Context Policy 表面变化。
    #[test]
    fn mutation_contract_is_fail_closed() {
        let proposal = ContextPolicyMutationProposalV1 {
            schema_version: CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION,
            mutation_id: MutationId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: GenomeDigest::from_sha256_hex("a".repeat(64))
                .expect("Genome 摘要应合法"),
            parent_policy_digest: digest('b'),
            candidate_policy: ContextPolicyV1::default(),
            evidence_episode_ids: BTreeSet::new(),
            hypothesis: "提升长上下文约束召回".into(),
        };
        assert_eq!(
            proposal.validate(),
            Err(InvalidContextMutation::MissingEvidence)
        );

        let candidate = ContextPolicyCandidateV1 {
            schema_version: CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION,
            candidate_id: CandidateId::generate(),
            cycle_id: EvolutionCycleId::generate(),
            mutation_id: MutationId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: GenomeDigest::from_sha256_hex("c".repeat(64))
                .expect("Genome 摘要应合法"),
            candidate_genome_digest: GenomeDigest::from_sha256_hex("d".repeat(64))
                .expect("Genome 摘要应合法"),
            parent_policy_digest: digest('e'),
            candidate_policy_digest: digest('f'),
            changed_surfaces: BTreeSet::from([MutationSurface::Runtime]),
            created_at_ms: 1,
        };
        assert!(matches!(
            candidate.validate(),
            Err(InvalidContextMutation::InvalidCandidateSurfaces(_))
        ));
    }

    /// 原始观察必须拒绝零分母、超额召回和负 token reduction。
    #[test]
    fn observation_rejects_impossible_metrics() {
        let observation = ContextEvaluationObservationV1 {
            schema_version: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
            facts: RecallObservationV1 {
                expected: 1,
                recalled: 2,
            },
            constraints: RecallObservationV1 {
                expected: 1,
                recalled: 1,
            },
            tool_states: RecallObservationV1 {
                expected: 1,
                recalled: 1,
            },
            plan_states: RecallObservationV1 {
                expected: 1,
                recalled: 1,
            },
            downstream_tasks: RecallObservationV1 {
                expected: 1,
                recalled: 1,
            },
            tokens_before: 100,
            tokens_after: 101,
            cost_microunits: 1,
            latency_ms: 1,
        };

        assert!(matches!(
            observation.validate(),
            Err(InvalidContextEvaluation::InvalidRecallCount { field: "facts", .. })
        ));
    }
}
