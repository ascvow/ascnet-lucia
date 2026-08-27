//! M5 Prompt 自进化的变异、Candidate 与 Cycle 协议。
//!
//! 本模块只保存强类型标识和不可变制品引用，不包含 Prompt 正文、Hidden Dataset、
//! Final Verifier 或 Commit Policy。变异实现与 Cycle 持久化属于 `agent-evolution`，
//! 最终评测仍属于独立受信 Evaluator。

use crate::{
    ipc::{EvaluationReceiptV1, HealthCheckReceiptV1, ReleaseReceiptV1},
    ArtifactDigest, ArtifactRef, CandidateId, EpisodeId, EvaluationReportId, EvolutionCycleId,
    EvolutionIssueId, GenomeDigest, GenomeRevisionId, MutationId, MutationSurface, ReleaseId,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 当前 MutationProposal 协议版本。
pub const MUTATION_PROPOSAL_SCHEMA_VERSION: u32 = 1;
/// 当前 MutationCandidate 协议版本。
pub const MUTATION_CANDIDATE_SCHEMA_VERSION: u32 = 1;
/// 当前 Evolution Cycle 请求与快照协议版本。
pub const EVOLUTION_CYCLE_SCHEMA_VERSION: u32 = 1;
/// Prompt 自进化单轮必须生成的最少 Candidate 数。
pub const MIN_CANDIDATES_PER_CYCLE: u32 = 3;
/// Prompt 自进化单轮允许生成的最多 Candidate 数，防止无界资源消耗。
pub const MAX_CANDIDATES_PER_CYCLE: u32 = 16;

const MAX_HYPOTHESIS_BYTES: usize = 4 * 1024;
const MAX_EXPECTED_EFFECT_BYTES: usize = 2 * 1024;
const MAX_TASK_FAMILY_BYTES: usize = 128;
const MAX_POLICY_VERSION_BYTES: usize = 128;
const MAX_LINEAGE_BYTES: usize = 128;
const MAX_FAILURE_CODE_BYTES: usize = 128;

/// 一条变异允许执行的结构化补丁。
///
/// M5 只允许替换 Task Strategy Prompt。Prompt 正文必须先写入 CAS，协议仅携带摘要、
/// 媒体类型和长度，避免跨进程请求泄漏用户内容或形成无界输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MutationPatch {
    /// 使用指定 CAS 制品替换唯一的 Task Strategy Prompt。
    ReplaceTaskStrategyPrompt {
        /// 新 Prompt 的不可变 CAS 引用；Candidate Builder 必须复核摘要、长度、媒体类型、
        /// UTF-8、非空和配置的字节上限。
        prompt: ArtifactRef,
    },
}

impl MutationPatch {
    /// 返回补丁引用的新 Task Strategy Prompt 制品。
    pub fn task_strategy_prompt(&self) -> &ArtifactRef {
        match self {
            Self::ReplaceTaskStrategyPrompt { prompt } => prompt,
        }
    }
}

/// Mutator 声明的一项预期行为效果。
///
/// 该声明只用于生成 Repair Case 和审计，不能替代独立 Evaluation 或 Gate 结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedEffect {
    /// 预期改善的稳定任务族，不得包含原始任务输入。
    pub task_family: String,
    /// 可由后续 Repair Dataset 验证的预期行为描述。
    pub expected_behavior: String,
}

impl ExpectedEffect {
    /// 校验任务族和预期行为均为有界非空文本。
    ///
    /// # Errors
    ///
    /// 字段为空或超过协议字节上限时返回 [`InvalidMutation`]。
    pub fn validate(&self) -> Result<(), InvalidMutation> {
        validate_text(
            "expected_effect.task_family",
            &self.task_family,
            MAX_TASK_FAMILY_BYTES,
        )?;
        validate_text(
            "expected_effect.expected_behavior",
            &self.expected_behavior,
            MAX_EXPECTED_EFFECT_BYTES,
        )
    }
}

/// Mutator 对变异风险的受限分类。
///
/// 风险只表达提案属性，不能放宽 Evaluation Profile、Commit Policy 或审批要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationRisk {
    /// 只修改低风险外部制品，且不扩大任何能力。
    Low,
    /// 可能影响多个任务族，需要更完整的 Regression 验证。
    Moderate,
    /// 行为影响较大，默认需要人工审批。
    High,
    /// 涉及安全、权限或可信边界，普通自进化流程必须拒绝。
    Critical,
}

/// 一份由受限 Mutator 生成、尚未经 Candidate Builder 信任的变异提案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationProposal {
    /// 提案结构版本；未知版本必须拒绝。
    pub schema_version: u32,
    /// 提案标识。
    pub mutation_id: MutationId,
    /// 触发本提案的稳定 Issue。
    pub issue_id: EvolutionIssueId,
    /// 提案基于的 Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Parent Genome 行为摘要，用于拒绝过期或错绑提案。
    pub parent_genome_digest: GenomeDigest,
    /// 声明修改的唯一行为表面。
    pub surface: MutationSurface,
    /// 支撑提案的脱敏 Episode 标识，正文和事件不进入本协议。
    pub evidence_episode_ids: Vec<EpisodeId>,
    /// 可审计的根因与修复假设。
    pub hypothesis: String,
    /// 结构化补丁；M5 只能替换 Task Strategy Prompt。
    pub patch: MutationPatch,
    /// Mutator 声明的可验证预期效果。
    pub expected_effects: Vec<ExpectedEffect>,
    /// 变异风险分类。
    pub risk: MutationRisk,
    /// 生成本提案的不可变 Mutator 制品或配置修订。
    pub mutator_revision: ArtifactRef,
}

impl MutationProposal {
    /// 校验 M5 提案的结构边界，不读取 CAS 或 Parent Genome。
    ///
    /// # Errors
    ///
    /// Schema 未知、证据缺失或重复、文本越界、预期效果无效、补丁制品为空，或声明了
    /// Task Strategy Prompt 以外的表面时返回 [`InvalidMutation`]。
    pub fn validate(&self) -> Result<(), InvalidMutation> {
        if self.schema_version != MUTATION_PROPOSAL_SCHEMA_VERSION {
            return Err(InvalidMutation::UnsupportedProposalSchema {
                found: self.schema_version,
                supported: MUTATION_PROPOSAL_SCHEMA_VERSION,
            });
        }
        if self.surface != MutationSurface::TaskStrategyPrompt {
            return Err(InvalidMutation::UnsupportedSurface(self.surface.clone()));
        }
        validate_unique_non_empty_episodes(&self.evidence_episode_ids)?;
        validate_text("hypothesis", &self.hypothesis, MAX_HYPOTHESIS_BYTES)?;
        if self.expected_effects.is_empty() {
            return Err(InvalidMutation::MissingExpectedEffects);
        }
        for effect in &self.expected_effects {
            effect.validate()?;
        }
        validate_artifact("patch.prompt", self.patch.task_strategy_prompt())?;
        validate_artifact("mutator_revision", &self.mutator_revision)
    }
}

/// Candidate Builder 完成可信全字段 Diff 后产生的不可变 Candidate 描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationCandidate {
    /// Candidate 结构版本；未知版本必须拒绝。
    pub schema_version: u32,
    /// Candidate 稳定标识。
    pub candidate_id: CandidateId,
    /// 所属 Evolution Cycle。
    pub cycle_id: EvolutionCycleId,
    /// 来源 Issue。
    pub issue_id: EvolutionIssueId,
    /// 来源 MutationProposal。
    pub mutation_id: MutationId,
    /// Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Parent Genome 行为摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Candidate Builder 登记的 Genome 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// Candidate Genome 行为摘要。
    pub candidate_genome_digest: GenomeDigest,
    /// Candidate 实际引用的 Task Strategy Prompt 制品。
    pub prompt: ArtifactRef,
    /// Candidate Builder 对 Parent/Candidate 执行可信全字段 Diff 后得到的变化表面。
    ///
    /// 该字段不能复制 Proposal 自报值；M5 中必须精确等于 `task_strategy_prompt`。
    pub changed_surfaces: BTreeSet<MutationSurface>,
    /// Candidate 构建完成的 Unix 毫秒时间。
    pub created_at_ms: u64,
}

impl MutationCandidate {
    /// 从已校验 Proposal 与可信 Builder Diff 创建 Candidate，并生成新 [`CandidateId`]。
    ///
    /// `changed_surfaces` 必须来自 Builder 对完整 Genome 的可信差异计算；本函数只复核 M5
    /// 允许集合，无法替代差异计算本身。
    ///
    /// # Errors
    ///
    /// Proposal 无效、Parent/Candidate 相同、摘要相同，或变化表面不是唯一的 Task Strategy
    /// Prompt 时返回 [`InvalidMutation`]。
    pub fn create(
        cycle_id: EvolutionCycleId,
        proposal: &MutationProposal,
        candidate_revision_id: GenomeRevisionId,
        candidate_genome_digest: GenomeDigest,
        changed_surfaces: BTreeSet<MutationSurface>,
        created_at_ms: u64,
    ) -> Result<Self, InvalidMutation> {
        proposal.validate()?;
        let candidate = Self {
            schema_version: MUTATION_CANDIDATE_SCHEMA_VERSION,
            candidate_id: CandidateId::generate(),
            cycle_id,
            issue_id: proposal.issue_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            parent_revision_id: proposal.parent_revision_id.clone(),
            parent_genome_digest: proposal.parent_genome_digest.clone(),
            candidate_revision_id,
            candidate_genome_digest,
            prompt: proposal.patch.task_strategy_prompt().clone(),
            changed_surfaces,
            created_at_ms,
        };
        candidate.validate()?;
        Ok(candidate)
    }

    /// 校验 Candidate 的结构和 M5 允许表面。
    ///
    /// # Errors
    ///
    /// Schema 未知、身份或摘要未发生变化、Prompt 制品无效，或可信 Diff 声明了其他表面时
    /// 返回 [`InvalidMutation`]。
    pub fn validate(&self) -> Result<(), InvalidMutation> {
        if self.schema_version != MUTATION_CANDIDATE_SCHEMA_VERSION {
            return Err(InvalidMutation::UnsupportedCandidateSchema {
                found: self.schema_version,
                supported: MUTATION_CANDIDATE_SCHEMA_VERSION,
            });
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidMutation::SameRevision);
        }
        if self.parent_genome_digest == self.candidate_genome_digest {
            return Err(InvalidMutation::SameGenomeDigest);
        }
        let expected = BTreeSet::from([MutationSurface::TaskStrategyPrompt]);
        if self.changed_surfaces != expected {
            return Err(InvalidMutation::InvalidCandidateSurfaces(
                self.changed_surfaces.clone(),
            ));
        }
        validate_artifact("candidate.prompt", &self.prompt)
    }
}

/// Evolution Cycle 的持久化阶段标签。
///
/// 本枚举只描述稳定协议值；合法迁移、幂等恢复和持久化由 `agent-evolution` 状态机负责。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionCycleStage {
    /// 已接受受信 Cycle 请求。
    Requested,
    /// 正在选择脱敏 Episode 证据。
    SelectingEvidence,
    /// 正在确定根因与可变表面。
    Diagnosing,
    /// 正在生成有界 MutationProposal。
    Mutating,
    /// Candidate Builder 正在构建并验证 Genome。
    BuildingCandidates,
    /// 已向独立 Evaluator 提交 Candidate。
    Evaluating,
    /// 正在从可信 Evaluation Receipt 中选择胜者。
    SelectingWinner,
    /// 已请求受信 Release Controller 晋升胜者。
    Promoting,
    /// Promotion 已提交，等待后续真实运行产生健康观察。
    AwaitingHealth,
    /// 正在请求受信 Evaluator 复核 Promotion 与运行健康观察。
    VerifyingHealth,
    /// 健康验证失败，正在请求受信 Release Controller 回滚 Parent。
    RollingBack,
    /// 旧版 M5 Cycle 已完成；新 Cycle 使用更精确的 HealthVerified 或 RolledBack 终态。
    Completed,
    /// Promotion 通过 Stable、后续运行和健康检查验证，Cycle 成功完成。
    HealthVerified,
    /// Promotion 健康验证失败并已原子回滚 Parent，Cycle 安全完成。
    RolledBack,
    /// 所有 Candidate 均被拒绝，Cycle 正常终止。
    Rejected,
    /// 选择、诊断、生成、构建、评测或发布基础设施失败，Cycle 以失败关闭。
    Failed,
}

/// 受信控制面启动一次 Prompt 自进化 Cycle 的最小请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvolutionCycleRequestInput {
    /// 已聚合并具备进化资格的 Issue。
    pub issue_id: EvolutionIssueId,
    /// 当前 Stable Parent 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Parent Genome 摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Stable lineage。
    pub lineage: String,
    /// 调用方观察到的 Parent 代数，用作并发前置条件。
    pub expected_parent_generation: u64,
    /// 只包含脱敏标识的来源 Episode；不得携带原始消息、事件或 ToolResult。
    pub source_episode_ids: Vec<EpisodeId>,
    /// 固定 Evolution Policy 版本；策略正文由受信控制面持有，不能随请求传入。
    pub evolution_policy_version: String,
    /// 本轮有界 Candidate 数量。
    pub candidate_count: u32,
    /// 请求创建的 Unix 毫秒时间。
    pub requested_at_ms: u64,
}

/// 受信控制面启动一次 Prompt 自进化 Cycle 的版本化请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvolutionCycleRequestV1 {
    /// Cycle 请求结构版本。
    pub schema_version: u32,
    /// Cycle 标识。
    pub cycle_id: EvolutionCycleId,
    /// 已聚合并具备进化资格的 Issue。
    pub issue_id: EvolutionIssueId,
    /// 当前 Stable Parent 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Parent Genome 摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Stable lineage。
    pub lineage: String,
    /// 调用方观察到的 Parent 代数，用作并发前置条件。
    pub expected_parent_generation: u64,
    /// 只包含脱敏标识的来源 Episode；不得携带原始消息、事件或 ToolResult。
    pub source_episode_ids: Vec<EpisodeId>,
    /// 固定 Evolution Policy 版本；策略正文由受信控制面持有，不能随请求传入。
    pub evolution_policy_version: String,
    /// 本轮有界 Candidate 数量。
    pub candidate_count: u32,
    /// 请求创建的 Unix 毫秒时间。
    pub requested_at_ms: u64,
}

impl EvolutionCycleRequestV1 {
    /// 创建绑定新 Cycle ID 的请求。
    ///
    /// # Errors
    ///
    /// Lineage、策略版本、证据集合或 Candidate 数量不符合协议边界时返回
    /// [`InvalidEvolutionCycle`]。
    pub fn create(input: EvolutionCycleRequestInput) -> Result<Self, InvalidEvolutionCycle> {
        let request = Self {
            schema_version: EVOLUTION_CYCLE_SCHEMA_VERSION,
            cycle_id: EvolutionCycleId::generate(),
            issue_id: input.issue_id,
            parent_revision_id: input.parent_revision_id,
            parent_genome_digest: input.parent_genome_digest,
            lineage: input.lineage,
            expected_parent_generation: input.expected_parent_generation,
            source_episode_ids: input.source_episode_ids,
            evolution_policy_version: input.evolution_policy_version,
            candidate_count: input.candidate_count,
            requested_at_ms: input.requested_at_ms,
        };
        request.validate()?;
        Ok(request)
    }

    /// 校验 Cycle 请求的无状态结构边界。
    ///
    /// # Errors
    ///
    /// Schema 未知、Lineage 或策略版本非法、证据缺失或重复、Candidate 数量越界时返回
    /// [`InvalidEvolutionCycle`]。
    pub fn validate(&self) -> Result<(), InvalidEvolutionCycle> {
        if self.schema_version != EVOLUTION_CYCLE_SCHEMA_VERSION {
            return Err(InvalidEvolutionCycle::UnsupportedSchema {
                found: self.schema_version,
                supported: EVOLUTION_CYCLE_SCHEMA_VERSION,
            });
        }
        validate_lineage(&self.lineage)?;
        validate_cycle_text(
            "evolution_policy_version",
            &self.evolution_policy_version,
            MAX_POLICY_VERSION_BYTES,
        )?;
        validate_cycle_episodes(&self.source_episode_ids)?;
        if !(MIN_CANDIDATES_PER_CYCLE..=MAX_CANDIDATES_PER_CYCLE).contains(&self.candidate_count) {
            return Err(InvalidEvolutionCycle::CandidateCountOutOfRange {
                found: self.candidate_count,
                min: MIN_CANDIDATES_PER_CYCLE,
                max: MAX_CANDIDATES_PER_CYCLE,
            });
        }
        Ok(())
    }
}

/// Evolution Cycle 的可归档不可变状态快照。
///
/// `sequence` 与 `previous_digest` 供实现层建立只追加哈希链；协议层不执行阶段迁移或 I/O。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvolutionCycleSnapshotV1 {
    /// 快照结构版本。
    pub schema_version: u32,
    /// 启动本 Cycle 的完整原始请求，用于归档重放和策略审计。
    pub request: EvolutionCycleRequestV1,
    /// Cycle 标识。
    pub cycle_id: EvolutionCycleId,
    /// 来源 Issue。
    pub issue_id: EvolutionIssueId,
    /// Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// 当前阶段。
    pub stage: EvolutionCycleStage,
    /// 从零开始单调递增的快照序号。
    pub sequence: u64,
    /// 前一快照规范字节摘要；首个快照为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_digest: Option<ArtifactDigest>,
    /// 本 Cycle 已接受的提案。
    #[serde(default)]
    pub proposals: Vec<MutationProposal>,
    /// Candidate Builder 已完成的 Candidate。
    #[serde(default)]
    pub candidates: Vec<MutationCandidate>,
    /// 独立 Evaluator 返回的脱敏回执。
    #[serde(default)]
    pub evaluation_receipts: Vec<EvaluationReceiptV1>,
    /// 可信选择器选出的 Candidate；尚未选择时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<CandidateId>,
    /// Promotion 的受信 Release 回执。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_receipt: Option<ReleaseReceiptV1>,
    /// Promotion 后的受信健康验证回执。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_receipt: Option<HealthCheckReceiptV1>,
    /// 健康失败后的受信 Rollback 回执。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_receipt: Option<ReleaseReceiptV1>,
    /// Fail-closed 终态使用的稳定错误码；不保存外部错误正文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    /// 本快照创建的 Unix 毫秒时间。
    pub created_at_ms: u64,
}

impl EvolutionCycleSnapshotV1 {
    /// 校验快照的版本、身份绑定与嵌套制品，不执行阶段迁移判断。
    ///
    /// # Errors
    ///
    /// Schema 或原始请求无效、提案或 Candidate 无效、嵌套身份不属于本 Cycle、Winner 不存在，
    /// 或失败码超出稳定文本边界时返回 [`InvalidEvolutionCycle`]。
    pub fn validate(&self) -> Result<(), InvalidEvolutionCycle> {
        if self.schema_version != EVOLUTION_CYCLE_SCHEMA_VERSION {
            return Err(InvalidEvolutionCycle::UnsupportedSchema {
                found: self.schema_version,
                supported: EVOLUTION_CYCLE_SCHEMA_VERSION,
            });
        }
        self.request.validate()?;
        if self.request.cycle_id != self.cycle_id
            || self.request.issue_id != self.issue_id
            || self.request.parent_revision_id != self.parent_revision_id
        {
            return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
        }
        for proposal in &self.proposals {
            proposal
                .validate()
                .map_err(|error| InvalidEvolutionCycle::InvalidMutation(error.to_string()))?;
            if proposal.issue_id != self.issue_id
                || proposal.parent_revision_id != self.parent_revision_id
            {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
        }
        for candidate in &self.candidates {
            candidate
                .validate()
                .map_err(|error| InvalidEvolutionCycle::InvalidMutation(error.to_string()))?;
            if candidate.cycle_id != self.cycle_id
                || candidate.issue_id != self.issue_id
                || candidate.parent_revision_id != self.parent_revision_id
            {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
        }
        for receipt in &self.evaluation_receipts {
            receipt
                .validate()
                .map_err(|error| InvalidEvolutionCycle::InvalidControlReceipt(error.to_string()))?;
            if receipt.parent_revision_id != self.parent_revision_id
                || !self.candidates.iter().any(|candidate| {
                    candidate.candidate_revision_id == receipt.candidate_revision_id
                })
            {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
        }
        if let Some(winner) = &self.winner {
            if !self
                .candidates
                .iter()
                .any(|candidate| &candidate.candidate_id == winner)
            {
                return Err(InvalidEvolutionCycle::UnknownWinner(winner.clone()));
            }
        }
        if let Some(release) = &self.release_receipt {
            release
                .validate()
                .map_err(|error| InvalidEvolutionCycle::InvalidControlReceipt(error.to_string()))?;
            if release.rollback_of.is_some()
                || release.lineage != self.request.lineage
                || release.from != self.parent_revision_id
            {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
            let winner_revision = self.winner.as_ref().and_then(|winner| {
                self.candidates
                    .iter()
                    .find(|candidate| &candidate.candidate_id == winner)
                    .map(|candidate| &candidate.candidate_revision_id)
            });
            if winner_revision != Some(&release.to) {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
        }
        if let Some(health) = &self.health_receipt {
            health
                .validate()
                .map_err(|error| InvalidEvolutionCycle::InvalidControlReceipt(error.to_string()))?;
            let Some(release) = &self.release_receipt else {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            };
            if health.release_id != release.release_id
                || health.lineage != release.lineage
                || health.expected_revision_id != release.to
                || health.expected_generation != release.generation
            {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
        }
        if let Some(rollback) = &self.rollback_receipt {
            rollback
                .validate()
                .map_err(|error| InvalidEvolutionCycle::InvalidControlReceipt(error.to_string()))?;
            let Some(release) = &self.release_receipt else {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            };
            if rollback.rollback_of.as_ref() != Some(&release.release_id)
                || rollback.report_id != release.report_id
                || rollback.lineage != release.lineage
                || rollback.from != release.to
                || rollback.to != self.parent_revision_id
            {
                return Err(InvalidEvolutionCycle::NestedIdentityMismatch);
            }
        }
        if let Some(code) = &self.failure_code {
            validate_failure_code(code)?;
        }
        self.validate_stage_artifacts()?;
        Ok(())
    }

    /// 校验阶段与已归档控制面制品的一致性。
    fn validate_stage_artifacts(&self) -> Result<(), InvalidEvolutionCycle> {
        let failed = self.stage == EvolutionCycleStage::Failed;
        if failed != self.failure_code.is_some() {
            return Err(InvalidEvolutionCycle::StageArtifactMismatch);
        }
        let requires_winner = matches!(
            self.stage,
            EvolutionCycleStage::Promoting
                | EvolutionCycleStage::AwaitingHealth
                | EvolutionCycleStage::VerifyingHealth
                | EvolutionCycleStage::RollingBack
                | EvolutionCycleStage::HealthVerified
                | EvolutionCycleStage::RolledBack
                | EvolutionCycleStage::Completed
        );
        if requires_winner && self.winner.is_none() {
            return Err(InvalidEvolutionCycle::StageArtifactMismatch);
        }
        let requires_release = matches!(
            self.stage,
            EvolutionCycleStage::AwaitingHealth
                | EvolutionCycleStage::VerifyingHealth
                | EvolutionCycleStage::RollingBack
                | EvolutionCycleStage::HealthVerified
                | EvolutionCycleStage::RolledBack
                | EvolutionCycleStage::Completed
        );
        if requires_release && self.release_receipt.is_none() {
            return Err(InvalidEvolutionCycle::StageArtifactMismatch);
        }
        if matches!(
            self.stage,
            EvolutionCycleStage::RollingBack | EvolutionCycleStage::RolledBack
        ) && self
            .health_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.verified)
        {
            return Err(InvalidEvolutionCycle::StageArtifactMismatch);
        }
        if self.stage == EvolutionCycleStage::HealthVerified
            && self
                .health_receipt
                .as_ref()
                .is_none_or(|receipt| !receipt.verified)
        {
            return Err(InvalidEvolutionCycle::StageArtifactMismatch);
        }
        if (self.stage == EvolutionCycleStage::RolledBack) != self.rollback_receipt.is_some() {
            return Err(InvalidEvolutionCycle::StageArtifactMismatch);
        }
        Ok(())
    }

    /// 返回当前快照引用的全部正式 EvaluationReport ID。
    pub fn evaluation_report_ids(&self) -> BTreeSet<EvaluationReportId> {
        self.evaluation_receipts
            .iter()
            .map(|receipt| receipt.report_id.clone())
            .collect()
    }

    /// 返回当前快照引用的 Release ID；尚未发布时为 `None`。
    pub fn release_id(&self) -> Option<&ReleaseId> {
        self.release_receipt
            .as_ref()
            .map(|receipt| &receipt.release_id)
    }

    /// 返回健康失败后 Rollback 的 Release ID；尚未回滚时为 `None`。
    pub fn rollback_release_id(&self) -> Option<&ReleaseId> {
        self.rollback_receipt
            .as_ref()
            .map(|receipt| &receipt.release_id)
    }
}

/// MutationProposal 或 MutationCandidate 的结构校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMutation {
    /// MutationProposal schema 不受支持。
    #[error("不支持的 MutationProposal schema 版本 {found}，当前支持 {supported}")]
    UnsupportedProposalSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// MutationCandidate schema 不受支持。
    #[error("不支持的 MutationCandidate schema 版本 {found}，当前支持 {supported}")]
    UnsupportedCandidateSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// M5 不允许修改该表面。
    #[error("M5 不允许修改行为表面：{0:?}")]
    UnsupportedSurface(MutationSurface),
    /// 提案没有支撑 Episode。
    #[error("MutationProposal 必须至少绑定一条脱敏 Episode")]
    MissingEvidence,
    /// 提案重复引用同一 Episode。
    #[error("MutationProposal 的 evidence_episode_ids 不能重复")]
    DuplicateEvidence,
    /// 提案没有可验证预期效果。
    #[error("MutationProposal 必须至少声明一项 ExpectedEffect")]
    MissingExpectedEffects,
    /// 文本字段为空或超过协议上限。
    #[error("Mutation 字段 `{field}` 必须是非空且不超过 {max_bytes} 字节的文本")]
    InvalidText {
        /// 字段名。
        field: &'static str,
        /// 最大字节数。
        max_bytes: usize,
    },
    /// ArtifactRef 缺少媒体类型或正文长度为零。
    #[error("Mutation 制品 `{field}` 必须声明非空媒体类型和非零字节长度")]
    InvalidArtifact {
        /// 字段名。
        field: &'static str,
    },
    /// Parent 与 Candidate 使用同一 Revision。
    #[error("MutationCandidate 的 Parent 与 Candidate Revision 不能相同")]
    SameRevision,
    /// Parent 与 Candidate 行为摘要相同。
    #[error("MutationCandidate 的 Parent 与 Candidate GenomeDigest 不能相同")]
    SameGenomeDigest,
    /// Candidate Diff 不是唯一 Task Strategy Prompt 表面。
    #[error("M5 Candidate 的可信 Diff 必须精确包含 TaskStrategyPrompt，实际为 {0:?}")]
    InvalidCandidateSurfaces(BTreeSet<MutationSurface>),
}

/// Evolution Cycle 请求或快照的结构校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEvolutionCycle {
    /// Cycle schema 不受支持。
    #[error("不支持的 Evolution Cycle schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Lineage 不符合安全稳定名称规则。
    #[error("Evolution Cycle lineage 不合法")]
    InvalidLineage,
    /// 请求文本为空或超过上限。
    #[error("Evolution Cycle 字段 `{field}` 必须是非空且不超过 {max_bytes} 字节的文本")]
    InvalidText {
        /// 字段名。
        field: &'static str,
        /// 最大字节数。
        max_bytes: usize,
    },
    /// 请求没有来源 Episode。
    #[error("Evolution Cycle 必须至少绑定一条来源 Episode")]
    MissingEpisodes,
    /// 请求重复引用来源 Episode。
    #[error("Evolution Cycle 的 source_episode_ids 不能重复")]
    DuplicateEpisodes,
    /// Candidate 数量超出协议边界。
    #[error("Evolution Cycle Candidate 数量 {found} 不在 {min} 到 {max} 范围内")]
    CandidateCountOutOfRange {
        /// 实际数量。
        found: u32,
        /// 最小数量。
        min: u32,
        /// 最大数量。
        max: u32,
    },
    /// 嵌套 Proposal 或 Candidate 不合法。
    #[error("Evolution Cycle 包含无效 Mutation：{0}")]
    InvalidMutation(String),
    /// 嵌套 Evaluator 或 Release 控制面回执不合法。
    #[error("Evolution Cycle 包含无效控制面回执：{0}")]
    InvalidControlReceipt(String),
    /// 嵌套对象不属于当前 Cycle、Issue 或 Parent。
    #[error("Evolution Cycle 嵌套对象的 Cycle、Issue 或 Parent 身份不匹配")]
    NestedIdentityMismatch,
    /// Winner 不是本 Cycle 已构建的 Candidate。
    #[error("Evolution Cycle Winner 不存在于 Candidate 集合：{0}")]
    UnknownWinner(CandidateId),
    /// Fail-closed 错误码不符合稳定边界。
    #[error("Evolution Cycle failure_code 不合法")]
    InvalidFailureCode,
    /// Cycle 阶段与 Winner、Release、Health、Rollback 或 Failure 制品不一致。
    #[error("Evolution Cycle 阶段与归档制品不一致")]
    StageArtifactMismatch,
}

/// 校验有界非空文本。
fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), InvalidMutation> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(InvalidMutation::InvalidText { field, max_bytes });
    }
    Ok(())
}

/// 校验提案引用的 Episode 集合非空且唯一。
fn validate_unique_non_empty_episodes(values: &[EpisodeId]) -> Result<(), InvalidMutation> {
    if values.is_empty() {
        return Err(InvalidMutation::MissingEvidence);
    }
    if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(InvalidMutation::DuplicateEvidence);
    }
    Ok(())
}

/// 校验协议层可证明的 ArtifactRef 边界；CAS 内容校验仍由 Builder 完成。
fn validate_artifact(field: &'static str, artifact: &ArtifactRef) -> Result<(), InvalidMutation> {
    if artifact.media_type.trim().is_empty() || artifact.size_bytes == 0 {
        return Err(InvalidMutation::InvalidArtifact { field });
    }
    Ok(())
}

/// 校验安全 lineage，拒绝绝对路径、空段和 `.`/`..` 逃逸语义。
fn validate_lineage(value: &str) -> Result<(), InvalidEvolutionCycle> {
    if value.is_empty()
        || value.len() > MAX_LINEAGE_BYTES
        || value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(InvalidEvolutionCycle::InvalidLineage);
    }
    Ok(())
}

/// 校验 Cycle 使用的稳定短文本。
fn validate_cycle_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), InvalidEvolutionCycle> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(InvalidEvolutionCycle::InvalidText { field, max_bytes });
    }
    Ok(())
}

/// 校验 Cycle 来源 Episode 非空且唯一。
fn validate_cycle_episodes(values: &[EpisodeId]) -> Result<(), InvalidEvolutionCycle> {
    if values.is_empty() {
        return Err(InvalidEvolutionCycle::MissingEpisodes);
    }
    if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(InvalidEvolutionCycle::DuplicateEpisodes);
    }
    Ok(())
}

/// 校验不含用户文本的稳定错误码。
fn validate_failure_code(value: &str) -> Result<(), InvalidEvolutionCycle> {
    if value.is_empty()
        || value.len() > MAX_FAILURE_CODE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(InvalidEvolutionCycle::InvalidFailureCode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造固定测试摘要。
    fn artifact_digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要合法")
    }

    /// 构造固定 Genome 摘要。
    fn genome_digest(seed: char) -> GenomeDigest {
        GenomeDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要合法")
    }

    /// 构造不含 Prompt 正文的 CAS 引用。
    fn artifact(seed: char, media_type: &str) -> ArtifactRef {
        ArtifactRef {
            digest: artifact_digest(seed),
            media_type: media_type.to_string(),
            size_bytes: 32,
        }
    }

    /// 构造一份合法 M5 Proposal。
    fn proposal() -> MutationProposal {
        MutationProposal {
            schema_version: MUTATION_PROPOSAL_SCHEMA_VERSION,
            mutation_id: MutationId::generate(),
            issue_id: EvolutionIssueId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: genome_digest('a'),
            surface: MutationSurface::TaskStrategyPrompt,
            evidence_episode_ids: vec![EpisodeId::generate()],
            hypothesis: "补充工具失败后的验证步骤".to_string(),
            patch: MutationPatch::ReplaceTaskStrategyPrompt {
                prompt: artifact('b', "text/plain"),
            },
            expected_effects: vec![ExpectedEffect {
                task_family: "code-edit".to_string(),
                expected_behavior: "工具失败后先验证再重试".to_string(),
            }],
            risk: MutationRisk::Low,
            mutator_revision: artifact('c', "application/json"),
        }
    }

    /// Proposal 必须稳定往返，并且 JSON 不包含 Prompt 正文。
    #[test]
    fn proposal_round_trips_without_prompt_body() {
        let value = proposal();
        value.validate().expect("Proposal 应合法");
        let json = serde_json::to_string(&value).expect("Proposal 应可序列化");
        assert!(json.contains("replace_task_strategy_prompt"));
        assert!(!json.contains("完整 Prompt 正文"));
        assert_eq!(
            serde_json::from_str::<MutationProposal>(&json).expect("Proposal 应可反序列化"),
            value
        );
    }

    /// Proposal 和 Patch 都必须拒绝未知字段，避免协议静默接受越界控制项。
    #[test]
    fn proposal_rejects_unknown_fields() {
        let mut value = serde_json::to_value(proposal()).expect("Proposal 应可转 JSON");
        value["commit_policy"] = serde_json::json!("candidate-controlled");
        assert!(serde_json::from_value::<MutationProposal>(value).is_err());

        let mut value = serde_json::to_value(proposal()).expect("Proposal 应可转 JSON");
        value["patch"]["prompt_body"] = serde_json::json!("禁止跨进程携带");
        assert!(serde_json::from_value::<MutationProposal>(value).is_err());
    }

    /// M5 Proposal 必须拒绝其他表面、重复证据和空预期效果。
    #[test]
    fn proposal_validation_is_fail_closed() {
        let mut value = proposal();
        value.surface = MutationSurface::Runtime;
        assert!(matches!(
            value.validate(),
            Err(InvalidMutation::UnsupportedSurface(
                MutationSurface::Runtime
            ))
        ));

        let mut value = proposal();
        value
            .evidence_episode_ids
            .push(value.evidence_episode_ids[0].clone());
        assert_eq!(value.validate(), Err(InvalidMutation::DuplicateEvidence));

        let mut value = proposal();
        value.expected_effects.clear();
        assert_eq!(
            value.validate(),
            Err(InvalidMutation::MissingExpectedEffects)
        );
    }

    /// Candidate 构造器必须生成 ID 并绑定 Proposal、Prompt 和可信 Diff。
    #[test]
    fn candidate_create_binds_trusted_builder_output() {
        let proposal = proposal();
        let candidate = MutationCandidate::create(
            EvolutionCycleId::generate(),
            &proposal,
            GenomeRevisionId::generate(),
            genome_digest('d'),
            BTreeSet::from([MutationSurface::TaskStrategyPrompt]),
            10,
        )
        .expect("Candidate 应可构造");
        assert_eq!(candidate.mutation_id, proposal.mutation_id);
        assert_eq!(candidate.prompt, *proposal.patch.task_strategy_prompt());
        candidate.validate().expect("Candidate 应合法");
    }

    /// Candidate 必须拒绝未授权表面和未变化的 Genome 摘要。
    #[test]
    fn candidate_rejects_untrusted_diff() {
        let proposal = proposal();
        assert!(matches!(
            MutationCandidate::create(
                EvolutionCycleId::generate(),
                &proposal,
                GenomeRevisionId::generate(),
                genome_digest('d'),
                BTreeSet::from([MutationSurface::Runtime]),
                10,
            ),
            Err(InvalidMutation::InvalidCandidateSurfaces(_))
        ));
        assert_eq!(
            MutationCandidate::create(
                EvolutionCycleId::generate(),
                &proposal,
                GenomeRevisionId::generate(),
                proposal.parent_genome_digest.clone(),
                BTreeSet::from([MutationSurface::TaskStrategyPrompt]),
                10,
            ),
            Err(InvalidMutation::SameGenomeDigest)
        );
    }

    /// CycleRequest 必须绑定至少三个 Candidate、来源证据和固定策略版本。
    #[test]
    fn cycle_request_enforces_bounded_inputs() {
        let episode = EpisodeId::generate();
        let request = EvolutionCycleRequestV1::create(EvolutionCycleRequestInput {
            issue_id: EvolutionIssueId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: genome_digest('a'),
            lineage: "stable/general".to_string(),
            expected_parent_generation: 1,
            source_episode_ids: vec![episode.clone()],
            evolution_policy_version: "evolution-policy-v1".to_string(),
            candidate_count: 3,
            requested_at_ms: 10,
        })
        .expect("CycleRequest 应合法");
        assert_eq!(request.candidate_count, MIN_CANDIDATES_PER_CYCLE);

        let mut invalid = request.clone();
        invalid.candidate_count = 2;
        assert!(matches!(
            invalid.validate(),
            Err(InvalidEvolutionCycle::CandidateCountOutOfRange { .. })
        ));
        invalid = request;
        invalid.source_episode_ids.push(episode);
        assert_eq!(
            invalid.validate(),
            Err(InvalidEvolutionCycle::DuplicateEpisodes)
        );
    }

    /// Cycle Snapshot 必须拒绝错绑 Candidate 和不存在的 Winner。
    #[test]
    fn cycle_snapshot_validates_nested_identity() {
        let proposal = proposal();
        let request = EvolutionCycleRequestV1::create(EvolutionCycleRequestInput {
            issue_id: proposal.issue_id.clone(),
            parent_revision_id: proposal.parent_revision_id.clone(),
            parent_genome_digest: proposal.parent_genome_digest.clone(),
            lineage: "stable/general".to_string(),
            expected_parent_generation: 1,
            source_episode_ids: proposal.evidence_episode_ids.clone(),
            evolution_policy_version: "evolution-policy-v1".to_string(),
            candidate_count: 3,
            requested_at_ms: 9,
        })
        .expect("CycleRequest 应合法");
        let cycle_id = request.cycle_id.clone();
        let candidate = MutationCandidate::create(
            cycle_id.clone(),
            &proposal,
            GenomeRevisionId::generate(),
            genome_digest('d'),
            BTreeSet::from([MutationSurface::TaskStrategyPrompt]),
            10,
        )
        .expect("Candidate 应合法");
        let mut snapshot = EvolutionCycleSnapshotV1 {
            schema_version: EVOLUTION_CYCLE_SCHEMA_VERSION,
            request,
            cycle_id,
            issue_id: proposal.issue_id.clone(),
            parent_revision_id: proposal.parent_revision_id.clone(),
            stage: EvolutionCycleStage::BuildingCandidates,
            sequence: 1,
            previous_digest: Some(artifact_digest('e')),
            proposals: vec![proposal],
            candidates: vec![candidate.clone()],
            evaluation_receipts: Vec::new(),
            winner: Some(candidate.candidate_id.clone()),
            release_receipt: None,
            health_receipt: None,
            rollback_receipt: None,
            failure_code: None,
            created_at_ms: 11,
        };
        snapshot.validate().expect("Snapshot 应合法");
        snapshot.request.issue_id = EvolutionIssueId::generate();
        assert_eq!(
            snapshot.validate(),
            Err(InvalidEvolutionCycle::NestedIdentityMismatch)
        );
        snapshot.request.issue_id = snapshot.issue_id.clone();
        snapshot.winner = Some(CandidateId::generate());
        assert!(matches!(
            snapshot.validate(),
            Err(InvalidEvolutionCycle::UnknownWinner(_))
        ));
    }

    /// Cycle DTO 必须拒绝未知控制字段。
    #[test]
    fn cycle_dto_rejects_unknown_fields() {
        let request = EvolutionCycleRequestV1::create(EvolutionCycleRequestInput {
            issue_id: EvolutionIssueId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: genome_digest('a'),
            lineage: "stable/general".to_string(),
            expected_parent_generation: 1,
            source_episode_ids: vec![EpisodeId::generate()],
            evolution_policy_version: "evolution-policy-v1".to_string(),
            candidate_count: 3,
            requested_at_ms: 10,
        })
        .expect("CycleRequest 应合法");
        let mut json = serde_json::to_value(request).expect("请求应可转 JSON");
        json["hidden_dataset_root"] = serde_json::json!("/secret");
        assert!(serde_json::from_value::<EvolutionCycleRequestV1>(json).is_err());
    }
}
