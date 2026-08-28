//! Task Strategy Prompt 的生成接口与确定性边界校验。

use crate::{
    episode_selection::mutation_evidence_is_behavior_only, ArtifactStore, ArtifactStoreError,
    EvolutionPolicy, MutationEvidence,
};
use agent_evolution_protocol::{
    ArtifactRef, ExpectedEffect, GenomeDigest, GenomeRevisionId, InvalidMutation, MutationId,
    MutationPatch, MutationProposal, MutationRisk, MutationSurface,
    MUTATION_PROPOSAL_SCHEMA_VERSION,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 生成器产生、尚未写入正式 Mutation Proposal 的 Prompt 草案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMutationDraft {
    /// 本候选要验证的行为假设；不得为空。
    pub hypothesis: String,
    /// 完整 Task Strategy Prompt 正文。
    pub prompt: String,
    /// 可由后续 Evaluation 验证的预期效果。
    pub expected_effects: Vec<ExpectedEffect>,
}

/// 把一组 Prompt 草案绑定为正式 [`MutationProposal`] 所需的受信上下文。
#[derive(Debug, Clone)]
pub struct MutationProposalContext {
    /// 本轮基于的 Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Parent Genome 的可信内容摘要，必须与脱敏证据绑定的摘要相同。
    pub parent_genome_digest: GenomeDigest,
    /// 生成器自身的不可变制品或配置修订。
    pub mutator_revision: ArtifactRef,
    /// 受信控制面给出的风险分类；Critical 会被 M5 Mutator 拒绝。
    pub risk: MutationRisk,
}

/// 传递给不受信 Prompt 生成器的最小请求视图。
#[derive(Debug, Clone)]
pub struct PromptMutationRequest<'a> {
    /// 当前 Parent 的 Task Strategy Prompt；不包含受保护 Prompt 层。
    pub parent_prompt: &'a str,
    /// 经 Selector 脱敏并完成资格校验的结构证据。
    pub evidence: &'a MutationEvidence,
    /// 固定 Policy 要求的候选数量。
    pub candidate_count: usize,
    /// 本轮唯一允许的变异表面。
    pub mutation_surface: MutationSurface,
}

/// 可替换的 Prompt 候选生成器。
///
/// 实现可以调用模型或读取离线脚本，但返回值始终被 [`BoundedPromptMutator`] 重新校验；
/// 生成器不能修改 Policy、证据资格或允许表面。
#[async_trait]
pub trait PromptMutationGenerator: Send + Sync {
    /// 根据 Parent Prompt 与脱敏证据生成候选草案。
    ///
    /// # Errors
    ///
    /// 模型、脚本或协议解析失败时返回 [`PromptMutationGenerationError`]。数量与内容边界
    /// 由外层 Mutator 独立强制。
    async fn generate(
        &self,
        request: PromptMutationRequest<'_>,
    ) -> Result<Vec<PromptMutationDraft>, PromptMutationGenerationError>;
}

/// Prompt 生成器自身的稳定错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Prompt 候选生成失败：{message}")]
pub struct PromptMutationGenerationError {
    message: String,
}

impl PromptMutationGenerationError {
    /// 从不含 Secret、用户正文和原始模型响应的稳定原因创建生成错误。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回供控制面记录的稳定错误原因。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 在生成器外强制固定 Policy 与候选内容边界的 Prompt Mutator。
pub struct BoundedPromptMutator<G>
where
    G: PromptMutationGenerator,
{
    generator: G,
    policy: EvolutionPolicy,
}

impl<G> BoundedPromptMutator<G>
where
    G: PromptMutationGenerator,
{
    /// 创建只允许 Task Strategy Prompt 且固定生成三个候选的 MVP Mutator。
    pub fn task_strategy_mvp(generator: G) -> Self {
        Self {
            generator,
            policy: EvolutionPolicy::task_strategy_mvp(),
        }
    }

    /// 返回 Mutator 使用的不可变内置 Policy。
    pub fn policy(&self) -> &EvolutionPolicy {
        &self.policy
    }

    /// 生成并规范化 Prompt 草案。
    ///
    /// 返回前会强制候选数量、UTF-8 字节上限、非空假设与预期效果、Prompt 唯一性，
    /// 并拒绝与 Parent 相同的候选。字符串首尾空白会被移除后再比较和返回。
    ///
    /// # Errors
    ///
    /// Parent Prompt 无效、生成器失败或任一候选越界时返回 [`PromptMutationError`]，且
    /// 不返回部分候选。
    pub async fn mutate(
        &self,
        parent_prompt: &str,
        evidence: &MutationEvidence,
    ) -> Result<Vec<PromptMutationDraft>, PromptMutationError> {
        if !mutation_evidence_is_behavior_only(evidence) {
            return Err(PromptMutationError::UnsupportedFailureKind(
                evidence.failure_kind,
            ));
        }
        let parent_prompt = parent_prompt.trim();
        if parent_prompt.is_empty() {
            return Err(PromptMutationError::EmptyParentPrompt);
        }
        if parent_prompt.len() > self.policy.max_prompt_bytes() {
            return Err(PromptMutationError::ParentPromptTooLong {
                actual: parent_prompt.len(),
                maximum: self.policy.max_prompt_bytes(),
            });
        }

        let drafts = self
            .generator
            .generate(PromptMutationRequest {
                parent_prompt,
                evidence,
                candidate_count: self.policy.candidate_count(),
                mutation_surface: MutationSurface::TaskStrategyPrompt,
            })
            .await?;
        if drafts.len() != self.policy.candidate_count() {
            return Err(PromptMutationError::InvalidCandidateCount {
                expected: self.policy.candidate_count(),
                actual: drafts.len(),
            });
        }

        let mut normalized = Vec::with_capacity(drafts.len());
        let mut prompt_indexes = BTreeMap::<String, usize>::new();
        for (index, draft) in drafts.into_iter().enumerate() {
            let hypothesis = draft.hypothesis.trim();
            if hypothesis.is_empty() {
                return Err(PromptMutationError::EmptyHypothesis { candidate: index });
            }
            if hypothesis.len() > self.policy.max_hypothesis_bytes() {
                return Err(PromptMutationError::HypothesisTooLong {
                    candidate: index,
                    actual: hypothesis.len(),
                    maximum: self.policy.max_hypothesis_bytes(),
                });
            }

            let prompt = draft.prompt.trim();
            if prompt.is_empty() {
                return Err(PromptMutationError::EmptyPrompt { candidate: index });
            }
            if prompt.len() > self.policy.max_prompt_bytes() {
                return Err(PromptMutationError::PromptTooLong {
                    candidate: index,
                    actual: prompt.len(),
                    maximum: self.policy.max_prompt_bytes(),
                });
            }
            if prompt == parent_prompt {
                return Err(PromptMutationError::UnchangedPrompt { candidate: index });
            }
            if let Some(first) = prompt_indexes.insert(prompt.to_string(), index) {
                return Err(PromptMutationError::DuplicatePrompt {
                    first_candidate: first,
                    duplicate_candidate: index,
                });
            }

            if draft.expected_effects.is_empty() {
                return Err(PromptMutationError::MissingExpectedEffects { candidate: index });
            }
            if draft.expected_effects.len() > self.policy.max_expected_effects() {
                return Err(PromptMutationError::TooManyExpectedEffects {
                    candidate: index,
                    actual: draft.expected_effects.len(),
                    maximum: self.policy.max_expected_effects(),
                });
            }
            let mut expected_effects = Vec::with_capacity(draft.expected_effects.len());
            for (effect_index, mut effect) in draft.expected_effects.into_iter().enumerate() {
                let task_family = effect.task_family.trim();
                let expected_behavior = effect.expected_behavior.trim();
                if task_family.is_empty() || expected_behavior.is_empty() {
                    return Err(PromptMutationError::EmptyExpectedEffect {
                        candidate: index,
                        effect: effect_index,
                    });
                }
                if expected_behavior.len() > self.policy.max_expected_effect_bytes() {
                    return Err(PromptMutationError::ExpectedEffectTooLong {
                        candidate: index,
                        effect: effect_index,
                        actual: expected_behavior.len(),
                        maximum: self.policy.max_expected_effect_bytes(),
                    });
                }
                effect.task_family = task_family.to_string();
                effect.expected_behavior = expected_behavior.to_string();
                effect
                    .validate()
                    .map_err(|source| PromptMutationError::InvalidExpectedEffect {
                        candidate: index,
                        effect: effect_index,
                        source,
                    })?;
                expected_effects.push(effect);
            }
            normalized.push(PromptMutationDraft {
                hypothesis: hypothesis.to_string(),
                prompt: prompt.to_string(),
                expected_effects,
            });
        }
        Ok(normalized)
    }

    /// 生成三个有界 Prompt、写入不可变 CAS，并构造正式 Mutation Proposal。
    ///
    /// Proposal 只保存 Prompt 制品引用，不跨进程携带正文。写 CAS 是唯一副作用；若后续
    /// 候选校验失败，已写入的内容寻址制品仍保持不可变且不会成为可用 Proposal。
    ///
    /// # Errors
    ///
    /// 脱敏证据与 Parent 摘要不一致、风险触及可信边界、草案越界、CAS 写入失败或协议
    /// 校验失败时返回 [`PromptMutationError`]。
    pub async fn propose<S>(
        &self,
        parent_prompt: &str,
        evidence: &MutationEvidence,
        context: &MutationProposalContext,
        artifacts: &S,
    ) -> Result<Vec<MutationProposal>, PromptMutationError>
    where
        S: ArtifactStore,
    {
        if context.parent_genome_digest != evidence.genome_digest {
            return Err(PromptMutationError::ParentGenomeDigestMismatch);
        }
        if context.risk == MutationRisk::Critical {
            return Err(PromptMutationError::CriticalRiskRejected);
        }
        if evidence.episodes.is_empty() {
            return Err(PromptMutationError::MissingMutationEvidence);
        }
        let mut unique_episodes = BTreeSet::new();
        for episode in &evidence.episodes {
            if !unique_episodes.insert(episode.episode_id.clone()) {
                return Err(PromptMutationError::DuplicateMutationEvidence {
                    episode_id: episode.episode_id.clone(),
                });
            }
        }

        let drafts = self.mutate(parent_prompt, evidence).await?;
        let evidence_episode_ids = evidence
            .episodes
            .iter()
            .map(|episode| episode.episode_id.clone())
            .collect::<Vec<_>>();
        let mut proposals = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let prompt = artifacts
                .put("text/plain; charset=utf-8", draft.prompt.as_bytes())
                .await?;
            let proposal = MutationProposal {
                schema_version: MUTATION_PROPOSAL_SCHEMA_VERSION,
                mutation_id: MutationId::generate(),
                issue_id: evidence.issue_id.clone(),
                parent_revision_id: context.parent_revision_id.clone(),
                parent_genome_digest: context.parent_genome_digest.clone(),
                surface: MutationSurface::TaskStrategyPrompt,
                evidence_episode_ids: evidence_episode_ids.clone(),
                hypothesis: draft.hypothesis,
                patch: MutationPatch::ReplaceTaskStrategyPrompt { prompt },
                expected_effects: draft.expected_effects,
                risk: context.risk,
                mutator_revision: context.mutator_revision.clone(),
            };
            proposal.validate()?;
            proposals.push(proposal);
        }
        Ok(proposals)
    }
}

/// Prompt Mutator 的生成或边界校验错误。
#[derive(Debug, thiserror::Error)]
pub enum PromptMutationError {
    /// Parent Task Strategy Prompt 为空。
    #[error("Parent Task Strategy Prompt 不能为空")]
    EmptyParentPrompt,
    /// Parent Prompt 超过固定字节上限。
    #[error("Parent Prompt 为 {actual} 字节，超过上限 {maximum}")]
    ParentPromptTooLong {
        /// 实际 UTF-8 字节数。
        actual: usize,
        /// Policy 允许的最大字节数。
        maximum: usize,
    },
    /// 生成器返回失败。
    #[error(transparent)]
    Generation(#[from] PromptMutationGenerationError),
    /// 脱敏证据与受信 Parent Genome 摘要不一致。
    #[error("MutationEvidence 与 Parent GenomeDigest 不一致")]
    ParentGenomeDigestMismatch,
    /// 失败属于插件实现、安全、Runtime 或环境边界，不能生成 Prompt Candidate。
    #[error("失败类别 {0:?} 不允许进入 Prompt 变异")]
    UnsupportedFailureKind(agent_evolution_protocol::FailureKind),
    /// 脱敏证据没有任何获准 Episode。
    #[error("MutationEvidence 必须至少包含一条获准 Episode")]
    MissingMutationEvidence,
    /// 脱敏证据重复引用同一 Episode。
    #[error("MutationEvidence 重复引用 Episode {episode_id}")]
    DuplicateMutationEvidence {
        /// 被重复引用的 Episode。
        episode_id: agent_evolution_protocol::EpisodeId,
    },
    /// Critical 风险触及安全或可信边界，普通 M5 流程必须拒绝。
    #[error("M5 Prompt Mutator 拒绝 Critical 风险提案")]
    CriticalRiskRejected,
    /// Prompt 正文写入 Artifact CAS 失败。
    #[error(transparent)]
    ArtifactStore(#[from] ArtifactStoreError),
    /// 生成出的正式 Proposal 违反共享协议。
    #[error(transparent)]
    InvalidProposal(#[from] InvalidMutation),
    /// 生成器没有返回固定数量的候选。
    #[error("Prompt 候选数量错误：期望 {expected}，实际 {actual}")]
    InvalidCandidateCount {
        /// Policy 固定候选数。
        expected: usize,
        /// 生成器实际返回数。
        actual: usize,
    },
    /// 候选假设为空。
    #[error("候选 {candidate} 的假设不能为空")]
    EmptyHypothesis {
        /// 从零开始的候选序号。
        candidate: usize,
    },
    /// 候选假设超过固定字节上限。
    #[error("候选 {candidate} 的假设为 {actual} 字节，超过上限 {maximum}")]
    HypothesisTooLong {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际 UTF-8 字节数。
        actual: usize,
        /// Policy 允许的最大字节数。
        maximum: usize,
    },
    /// 候选 Prompt 为空。
    #[error("候选 {candidate} 的 Prompt 不能为空")]
    EmptyPrompt {
        /// 从零开始的候选序号。
        candidate: usize,
    },
    /// 候选 Prompt 超过固定字节上限。
    #[error("候选 {candidate} 的 Prompt 为 {actual} 字节，超过上限 {maximum}")]
    PromptTooLong {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际 UTF-8 字节数。
        actual: usize,
        /// Policy 允许的最大字节数。
        maximum: usize,
    },
    /// 候选 Prompt 与 Parent 相同。
    #[error("候选 {candidate} 的 Prompt 与 Parent 相同")]
    UnchangedPrompt {
        /// 从零开始的候选序号。
        candidate: usize,
    },
    /// 两个候选 Prompt 在规范化后相同。
    #[error("候选 {duplicate_candidate} 与候选 {first_candidate} 的 Prompt 重复")]
    DuplicatePrompt {
        /// 第一次出现的候选序号。
        first_candidate: usize,
        /// 重复候选序号。
        duplicate_candidate: usize,
    },
    /// 候选没有声明任何预期效果。
    #[error("候选 {candidate} 必须至少声明一条预期效果")]
    MissingExpectedEffects {
        /// 从零开始的候选序号。
        candidate: usize,
    },
    /// 候选声明了过多预期效果。
    #[error("候选 {candidate} 声明了 {actual} 条预期效果，超过上限 {maximum}")]
    TooManyExpectedEffects {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际效果数量。
        actual: usize,
        /// Policy 允许的最大效果数量。
        maximum: usize,
    },
    /// 候选中的一条预期效果为空。
    #[error("候选 {candidate} 的预期效果 {effect} 不能为空")]
    EmptyExpectedEffect {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 从零开始的效果序号。
        effect: usize,
    },
    /// 候选中的一条预期效果超过字节上限。
    #[error("候选 {candidate} 的预期效果 {effect} 为 {actual} 字节，超过上限 {maximum}")]
    ExpectedEffectTooLong {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 从零开始的效果序号。
        effect: usize,
        /// 实际 UTF-8 字节数。
        actual: usize,
        /// Policy 允许的最大字节数。
        maximum: usize,
    },
    /// ExpectedEffect 违反共享协议的文本边界。
    #[error("候选 {candidate} 的预期效果 {effect} 不合法：{source}")]
    InvalidExpectedEffect {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 从零开始的效果序号。
        effect: usize,
        /// 共享协议返回的稳定原因。
        source: InvalidMutation,
    },
}
