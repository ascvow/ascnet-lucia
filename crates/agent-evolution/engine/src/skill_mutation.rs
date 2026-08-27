//! M7 Skill 候选生成接口与可信边界收窄。
//!
//! 生成器只描述希望执行的操作和候选正文；Parent Revision、来源摘要、修订号、能力上限、
//! Episode 绑定、Mutation ID 与生命周期状态均由本模块从可信 Store 和控制面输入重建。

use crate::{
    skill_repository::{SkillArtifactRepository, SkillRepositoryError},
    FileArtifactStore, MutationEvidence,
};
use agent_evolution_protocol::{
    EpisodeId, GenomeRevision, InvalidSkillEvolution, MutationId, MutationSurface, SkillArtifactV1,
    SkillDeletionModeV1, SkillId, SkillMutationProposalV1, SkillOperationV1,
    SkillStatusTransitionV1, SkillStatusV1, SkillTriggerPolicyV1, MIN_CANDIDATES_PER_CYCLE,
    SKILL_ARTIFACT_SCHEMA_VERSION, SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// M7 固定 Skill Mutator 策略版本。
pub const M7_SKILL_MUTATION_POLICY_VERSION: &str = "skill-mutation-m7-v1";
/// 每轮必须生成的 Skill Candidate 数量。
pub const M7_SKILL_CANDIDATE_COUNT: usize = MIN_CANDIDATES_PER_CYCLE as usize;
/// 单个 Split 最多产生的 Skill 制品数量。
pub const MAX_SKILL_DRAFT_ARTIFACTS: usize = 16;
/// 单个候选草案编码后的最大字节数。
pub const MAX_SKILL_DRAFT_BYTES: usize = 1024 * 1024;
/// 单个候选假设的最大 UTF-8 字节数。
pub const MAX_SKILL_DRAFT_HYPOTHESIS_BYTES: usize = 4 * 1024;

/// 生成器可读取的 Parent Skill 只读视图。
///
/// 视图不包含 Parent Genome 身份、可信状态、CAS 摘要或能力 owner；这些值只能由外层
/// Mutator 从真实 Revision 与 Artifact CAS 恢复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillMutationParentView {
    /// Skill 的稳定 ID。
    pub skill_id: SkillId,
    /// 当前 Active 修订的名称。
    pub name: String,
    /// 当前 Active 修订的用途说明。
    pub description: String,
    /// 当前 Active 修订的完整指令。
    pub instructions: String,
    /// 当前 Active 修订的触发规则。
    pub trigger_policy: SkillTriggerPolicyV1,
    /// 当前修订实际请求的能力；最终上限仍由可信 Parent Genome 决定。
    pub required_capabilities: BTreeSet<String>,
}

/// 生成器产生的一份 Skill 正文草案。
///
/// `skill_id` 与能力只是候选请求。外层 Mutator 会重新验证 ID、能力子集、来源与操作语义，
/// 并自行填写修订号、CAS 摘要、状态链和可信证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillContentDraftV1 {
    /// 候选 Skill 的稳定 ID。
    pub skill_id: SkillId,
    /// 面向选择器的名称。
    pub name: String,
    /// 不含 Secret 或原始 ToolResult 的用途说明。
    pub description: String,
    /// 完整候选指令。
    pub instructions: String,
    /// 候选触发规则。
    pub trigger_policy: SkillTriggerPolicyV1,
    /// 候选需要的能力；必须是 Parent Genome 能力集合的子集。
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
}

/// 生成器可请求的六类 Skill 操作。
///
/// 该类型有意不接受来源摘要、修订号或生命周期状态，避免生成器把自报值伪装成可信事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillMutationDraftOperationV1 {
    /// 创建一个新 Skill。
    Create {
        /// 新 Skill 正文。
        skill: SkillContentDraftV1,
    },
    /// 更新一个现有 Skill；`skill.skill_id` 即目标 ID。
    Update {
        /// 更新后的完整正文。
        skill: SkillContentDraftV1,
    },
    /// 合并至少两个现有 Skill。
    Merge {
        /// 来源 Skill ID；可信摘要由 Mutator 复读 Parent CAS 后填写。
        source_skill_ids: BTreeSet<SkillId>,
        /// 合并后的完整正文，ID 可以是来源之一或全新 ID。
        skill: SkillContentDraftV1,
    },
    /// 把一个现有 Skill 拆分为至少两个结果 Skill。
    Split {
        /// 唯一来源 Skill ID。
        source_skill_id: SkillId,
        /// 全部结果正文；若保留来源 ID，该项会成为来源的下一修订。
        skills: Vec<SkillContentDraftV1>,
    },
    /// 停止在新运行中装配一个现有 Skill。
    Deprecate {
        /// 要弃用的 Active Skill ID。
        skill_id: SkillId,
    },
    /// 对一个现有 Skill 追加逻辑删除墓碑。
    Delete {
        /// 要逻辑删除的 Active Skill ID。
        skill_id: SkillId,
    },
}

/// 生成器产生、尚未绑定可信 Parent 和 Episode 的 Skill 候选草案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMutationDraftV1 {
    /// 本候选要验证的行为假设。
    pub hypothesis: String,
    /// 生成器请求的有界 Skill 操作。
    pub operation: SkillMutationDraftOperationV1,
}

/// 传递给不受信 Skill 生成器的最小请求视图。
#[derive(Debug, Clone)]
pub struct SkillMutationRequestV1<'a> {
    /// 经 Selector 脱敏并完成资格校验的结构证据。
    pub evidence: &'a MutationEvidence,
    /// 当前 Parent 中可变异的 Active Skill 只读视图。
    pub parent_skills: &'a [SkillMutationParentView],
    /// 固定策略要求的候选数量。
    pub candidate_count: usize,
    /// 本轮唯一允许的变异表面，固定为 [`MutationSurface::Skill`]。
    pub mutation_surface: MutationSurface,
}

/// 可替换的 Skill 候选生成器。
///
/// 实现可以调用模型或离线脚本，但不能决定可信 Parent、能力 owner、状态、修订或证据绑定；
/// [`BoundedSkillMutator`] 会忽略所有不在草案协议内的事实并从真实 Store 重建。
#[async_trait]
pub trait SkillMutationGenerator: Send + Sync {
    /// 根据脱敏 Episode 与 Parent Skill 视图生成固定数量的候选。
    ///
    /// # Errors
    ///
    /// 模型、脚本或草案解析失败时返回 [`SkillMutationGenerationError`]。边界校验失败由
    /// 外层 Mutator 另行返回，不允许生成器放宽。
    async fn generate(
        &self,
        request: SkillMutationRequestV1<'_>,
    ) -> Result<Vec<SkillMutationDraftV1>, SkillMutationGenerationError>;
}

/// Skill 生成器自身的稳定错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Skill 候选生成失败：{message}")]
pub struct SkillMutationGenerationError {
    message: String,
}

/// 无外部模型依赖的 M7 确定性 Skill 候选生成器。
///
/// Parent 已有 Skill 时，它仅对按 ID 排序的首个 Skill 生成三份受限
/// Update 草案；Parent 为空时则生成三份固定 ID 的 Create 草案。草案只使用
/// Selector 提供的脱敏预期行为，所有身份、能力上限和证据绑定仍由
/// [`BoundedSkillMutator`] 从真实 Store 重建。
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicSkillMutationGenerator;

#[async_trait]
impl SkillMutationGenerator for DeterministicSkillMutationGenerator {
    async fn generate(
        &self,
        request: SkillMutationRequestV1<'_>,
    ) -> Result<Vec<SkillMutationDraftV1>, SkillMutationGenerationError> {
        if request.candidate_count != M7_SKILL_CANDIDATE_COUNT
            || request.mutation_surface != MutationSurface::Skill
        {
            return Err(SkillMutationGenerationError::new(
                "unsupported_skill_mutation_request",
            ));
        }
        let expected_behavior = request.evidence.expected_behavior.trim();
        if expected_behavior.is_empty() {
            return Err(SkillMutationGenerationError::new(
                "missing_expected_behavior",
            ));
        }
        let strategies = [
            ("补充执行前的可验证边界", "执行前先检查输入、权限和完成条件"),
            (
                "补充执行后的真实状态复核",
                "执行后核对真实状态、错误分类和外部副作用",
            ),
            (
                "补充结束前的独立验收",
                "结束前按任务契约独立验收并保留失败证据",
            ),
        ];
        let drafts = if let Some(parent) = request.parent_skills.first() {
            strategies
                .into_iter()
                .map(|(hypothesis, instruction)| SkillMutationDraftV1 {
                    hypothesis: hypothesis.to_string(),
                    operation: SkillMutationDraftOperationV1::Update {
                        skill: SkillContentDraftV1 {
                            skill_id: parent.skill_id.clone(),
                            name: parent.name.clone(),
                            description: parent.description.clone(),
                            instructions: format!(
                                "{}\n\n进化策略：{}。目标行为：{}。",
                                parent.instructions, instruction, expected_behavior
                            ),
                            trigger_policy: parent.trigger_policy.clone(),
                            required_capabilities: parent.required_capabilities.clone(),
                        },
                    },
                })
                .collect()
        } else {
            let ids = ["skill_m7create01", "skill_m7create02", "skill_m7create03"];
            strategies
                .into_iter()
                .zip(ids)
                .map(|((hypothesis, instruction), id)| SkillMutationDraftV1 {
                    hypothesis: hypothesis.to_string(),
                    operation: SkillMutationDraftOperationV1::Create {
                        skill: SkillContentDraftV1 {
                            skill_id: SkillId::new(id).expect("固定 Skill ID 必须合法"),
                            name: hypothesis.to_string(),
                            description: format!("为受信进化证据补充策略：{expected_behavior}。"),
                            instructions: format!(
                                "{}。目标行为：{}。",
                                instruction, expected_behavior
                            ),
                            trigger_policy: SkillTriggerPolicyV1::default(),
                            required_capabilities: BTreeSet::new(),
                        },
                    },
                })
                .collect()
        };
        Ok(drafts)
    }
}

impl SkillMutationGenerationError {
    /// 从不含 Secret、用户正文或原始模型响应的稳定原因创建错误。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回可写入控制面日志的稳定错误原因。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 在生成器外强制 M7 数量、大小、证据、能力与六类操作边界的 Skill Mutator。
pub struct BoundedSkillMutator<G>
where
    G: SkillMutationGenerator,
{
    generator: G,
}

impl<G> BoundedSkillMutator<G>
where
    G: SkillMutationGenerator,
{
    /// 创建使用固定 M7 Policy 的 Skill Mutator。
    pub fn m7(generator: G) -> Self {
        Self { generator }
    }

    /// 返回不可由生成器覆盖的固定策略版本。
    pub const fn policy_version(&self) -> &'static str {
        M7_SKILL_MUTATION_POLICY_VERSION
    }

    /// 生成并绑定正式 Skill Mutation Proposal。
    ///
    /// 方法先复核 Parent Revision、Active Skill CAS 与脱敏 Episode，再请求固定三个草案；
    /// 所有文本会规范化，重复或过大的候选会导致整批失败。最终制品的 Parent 摘要、来源
    /// 摘要、修订号、能力上限、Mutation ID、Episode 集合和状态链均由本方法可信生成。
    ///
    /// 该方法只读取 Store，不写 CAS；写入候选制品与 Genome Revision 属于 Candidate
    /// Builder 的职责。
    ///
    /// # Errors
    ///
    /// Parent/Episode 错绑、Skill CAS 无效、生成数量错误、候选重复/过大、操作来源不存在、
    /// 能力扩大、状态时间无效或最终协议校验失败时返回 [`SkillMutationError`]。
    pub async fn propose(
        &self,
        parent: &GenomeRevision,
        evidence: &MutationEvidence,
        generated_at_ms: u64,
        artifacts: &FileArtifactStore,
    ) -> Result<Vec<SkillMutationProposalV1>, SkillMutationError> {
        parent
            .validate()
            .map_err(|error| SkillMutationError::InvalidParent(error.to_string()))?;
        if generated_at_ms == 0 {
            return Err(SkillMutationError::InvalidGeneratedAt);
        }
        validate_evidence(parent, evidence)?;

        let repository = SkillArtifactRepository::new(artifacts);
        let parent_artifacts = load_parent_artifacts(parent, &repository).await?;
        let parent_views = parent_artifacts
            .values()
            .map(|artifact| SkillMutationParentView {
                skill_id: artifact.skill_id.clone(),
                name: artifact.name.clone(),
                description: artifact.description.clone(),
                instructions: artifact.instructions.clone(),
                trigger_policy: artifact.trigger_policy.clone(),
                required_capabilities: artifact.required_capabilities.clone(),
            })
            .collect::<Vec<_>>();
        let drafts = self
            .generator
            .generate(SkillMutationRequestV1 {
                evidence,
                parent_skills: &parent_views,
                candidate_count: M7_SKILL_CANDIDATE_COUNT,
                mutation_surface: MutationSurface::Skill,
            })
            .await?;
        if drafts.len() != M7_SKILL_CANDIDATE_COUNT {
            return Err(SkillMutationError::InvalidCandidateCount {
                expected: M7_SKILL_CANDIDATE_COUNT,
                actual: drafts.len(),
            });
        }

        let parent_capabilities = parent
            .genome
            .capability_owners
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let evidence_episode_ids = evidence
            .episodes
            .iter()
            .map(|episode| episode.episode_id.clone())
            .collect::<BTreeSet<_>>();
        let mut fingerprints = BTreeMap::<Vec<u8>, usize>::new();
        let mut proposals = Vec::with_capacity(drafts.len());
        for (candidate, draft) in drafts.into_iter().enumerate() {
            let draft = normalize_draft(draft);
            validate_draft_shape(candidate, &draft)?;
            let fingerprint =
                serde_json::to_vec(&draft).map_err(SkillMutationError::SerializeDraft)?;
            if fingerprint.len() > MAX_SKILL_DRAFT_BYTES {
                return Err(SkillMutationError::DraftTooLarge {
                    candidate,
                    actual: fingerprint.len(),
                    maximum: MAX_SKILL_DRAFT_BYTES,
                });
            }
            if let Some(first_candidate) = fingerprints.insert(fingerprint.clone(), candidate) {
                return Err(SkillMutationError::DuplicateCandidate {
                    first_candidate,
                    duplicate_candidate: candidate,
                });
            }

            let mutation_id = deterministic_mutation_id(
                parent,
                evidence,
                generated_at_ms,
                candidate,
                &fingerprint,
            )?;
            let proposed_artifacts = materialize_operation(
                candidate,
                draft.operation,
                &parent_artifacts,
                &parent_capabilities,
                &evidence_episode_ids,
                &mutation_id,
                generated_at_ms,
            )?;
            let proposal = SkillMutationProposalV1 {
                schema_version: SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
                mutation_id,
                parent_revision_id: parent.revision_id.clone(),
                parent_genome_digest: parent.digest.clone(),
                evidence_episode_ids: evidence_episode_ids.clone(),
                proposed_artifacts,
                hypothesis: draft.hypothesis,
            };
            proposal
                .validate()
                .map_err(|source| SkillMutationError::InvalidProposal { candidate, source })?;
            proposals.push(proposal);
        }
        Ok(proposals)
    }
}

/// 从完整可信输入和规范草案派生幂等 Mutation ID。
fn deterministic_mutation_id(
    parent: &GenomeRevision,
    evidence: &MutationEvidence,
    generated_at_ms: u64,
    candidate: usize,
    draft_fingerprint: &[u8],
) -> Result<MutationId, SkillMutationError> {
    let mut hasher = Sha256::new();
    for part in [
        M7_SKILL_MUTATION_POLICY_VERSION.as_bytes(),
        parent.revision_id.as_str().as_bytes(),
        parent.digest.as_str().as_bytes(),
        evidence.issue_id.as_str().as_bytes(),
        &generated_at_ms.to_be_bytes(),
        &(candidate as u64).to_be_bytes(),
        draft_fingerprint,
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    for episode in &evidence.episodes {
        let part = episode.episode_id.as_str().as_bytes();
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    MutationId::new(format!("{}_{:x}", MutationId::PREFIX, hasher.finalize()))
        .map_err(|error| SkillMutationError::DeterministicMutationId(error.to_string()))
}

fn validate_evidence(
    parent: &GenomeRevision,
    evidence: &MutationEvidence,
) -> Result<(), SkillMutationError> {
    if evidence.genome_digest != parent.digest {
        return Err(SkillMutationError::ParentGenomeDigestMismatch);
    }
    if evidence.episodes.is_empty() {
        return Err(SkillMutationError::MissingMutationEvidence);
    }
    let mut episode_ids = BTreeSet::new();
    for episode in &evidence.episodes {
        if episode.genome_revision_id != parent.revision_id {
            return Err(SkillMutationError::EpisodeParentMismatch {
                episode_id: episode.episode_id.clone(),
            });
        }
        if !episode_ids.insert(episode.episode_id.clone()) {
            return Err(SkillMutationError::DuplicateMutationEvidence {
                episode_id: episode.episode_id.clone(),
            });
        }
    }
    Ok(())
}

async fn load_parent_artifacts(
    parent: &GenomeRevision,
    repository: &SkillArtifactRepository<'_>,
) -> Result<BTreeMap<SkillId, SkillArtifactV1>, SkillMutationError> {
    let mut result = BTreeMap::new();
    for reference in &parent.genome.skills {
        let skill_id = SkillId::new(reference.id.clone()).map_err(|error| {
            SkillMutationError::InvalidParentSkillId {
                value: reference.id.clone(),
                reason: error.to_string(),
            }
        })?;
        let artifact = repository.get(&reference.content).await?;
        if artifact.skill_id != skill_id || artifact.digest()? != reference.content {
            return Err(SkillMutationError::ParentSkillBindingMismatch { skill_id });
        }
        if artifact.status_history.last().map(|entry| entry.status) != Some(SkillStatusV1::Active) {
            return Err(SkillMutationError::ParentSkillNotActive(skill_id));
        }
        if result.insert(skill_id.clone(), artifact).is_some() {
            return Err(SkillMutationError::DuplicateParentSkill(skill_id));
        }
    }
    Ok(result)
}

fn normalize_draft(mut draft: SkillMutationDraftV1) -> SkillMutationDraftV1 {
    draft.hypothesis = draft.hypothesis.trim().to_string();
    match &mut draft.operation {
        SkillMutationDraftOperationV1::Create { skill }
        | SkillMutationDraftOperationV1::Update { skill }
        | SkillMutationDraftOperationV1::Merge { skill, .. } => normalize_content(skill),
        SkillMutationDraftOperationV1::Split { skills, .. } => {
            for skill in skills.iter_mut() {
                normalize_content(skill);
            }
            skills.sort_by(|left, right| left.skill_id.cmp(&right.skill_id));
        }
        SkillMutationDraftOperationV1::Deprecate { .. }
        | SkillMutationDraftOperationV1::Delete { .. } => {}
    }
    draft
}

fn normalize_content(skill: &mut SkillContentDraftV1) {
    skill.name = skill.name.trim().to_string();
    skill.description = skill.description.trim().to_string();
    skill.instructions = skill.instructions.trim().to_string();
    skill.required_capabilities = skill
        .required_capabilities
        .iter()
        .map(|value| value.trim().to_string())
        .collect();
    skill.trigger_policy.explicit_triggers = skill
        .trigger_policy
        .explicit_triggers
        .iter()
        .map(|value| value.trim().to_string())
        .collect();
    skill.trigger_policy.semantic_intents = skill
        .trigger_policy
        .semantic_intents
        .iter()
        .map(|value| value.trim().to_string())
        .collect();
}

fn validate_draft_shape(
    candidate: usize,
    draft: &SkillMutationDraftV1,
) -> Result<(), SkillMutationError> {
    if draft.hypothesis.is_empty() {
        return Err(SkillMutationError::EmptyHypothesis { candidate });
    }
    if draft.hypothesis.len() > MAX_SKILL_DRAFT_HYPOTHESIS_BYTES {
        return Err(SkillMutationError::HypothesisTooLong {
            candidate,
            actual: draft.hypothesis.len(),
            maximum: MAX_SKILL_DRAFT_HYPOTHESIS_BYTES,
        });
    }
    if let SkillMutationDraftOperationV1::Split { skills, .. } = &draft.operation {
        if !(2..=MAX_SKILL_DRAFT_ARTIFACTS).contains(&skills.len()) {
            return Err(SkillMutationError::InvalidSplitResultCount {
                candidate,
                actual: skills.len(),
                minimum: 2,
                maximum: MAX_SKILL_DRAFT_ARTIFACTS,
            });
        }
        let unique = skills
            .iter()
            .map(|skill| skill.skill_id.clone())
            .collect::<BTreeSet<_>>();
        if unique.len() != skills.len() {
            return Err(SkillMutationError::DuplicateSplitResult { candidate });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn materialize_operation(
    candidate: usize,
    operation: SkillMutationDraftOperationV1,
    parent: &BTreeMap<SkillId, SkillArtifactV1>,
    parent_capabilities: &BTreeSet<String>,
    evidence_episode_ids: &BTreeSet<EpisodeId>,
    mutation_id: &MutationId,
    generated_at_ms: u64,
) -> Result<Vec<SkillArtifactV1>, SkillMutationError> {
    let mut artifacts = match operation {
        SkillMutationDraftOperationV1::Create { skill } => {
            if parent.contains_key(&skill.skill_id) {
                return Err(SkillMutationError::SkillAlreadyExists {
                    candidate,
                    skill_id: skill.skill_id,
                });
            }
            vec![new_artifact(
                skill,
                1,
                SkillOperationV1::Create,
                evidence_episode_ids,
                mutation_id,
                generated_at_ms,
                parent_capabilities,
                candidate,
            )?]
        }
        SkillMutationDraftOperationV1::Update { skill } => {
            let previous = require_parent(candidate, parent, &skill.skill_id)?;
            vec![new_artifact(
                skill,
                next_revision(candidate, previous)?,
                SkillOperationV1::Update {
                    previous_artifact_digest: previous.digest()?,
                },
                evidence_episode_ids,
                mutation_id,
                generated_at_ms,
                parent_capabilities,
                candidate,
            )?]
        }
        SkillMutationDraftOperationV1::Merge {
            source_skill_ids,
            skill,
        } => {
            if source_skill_ids.len() < 2 || source_skill_ids.len() > MAX_SKILL_DRAFT_ARTIFACTS {
                return Err(SkillMutationError::InvalidMergeSourceCount {
                    candidate,
                    actual: source_skill_ids.len(),
                    minimum: 2,
                    maximum: MAX_SKILL_DRAFT_ARTIFACTS,
                });
            }
            let mut source_artifacts = BTreeMap::new();
            for source_id in source_skill_ids {
                let source = require_parent(candidate, parent, &source_id)?;
                source_artifacts.insert(source_id, source.digest()?);
            }
            let revision = match parent.get(&skill.skill_id) {
                Some(previous) if source_artifacts.contains_key(&skill.skill_id) => {
                    next_revision(candidate, previous)?
                }
                Some(_) => {
                    return Err(SkillMutationError::SkillAlreadyExists {
                        candidate,
                        skill_id: skill.skill_id,
                    })
                }
                None => 1,
            };
            vec![new_artifact(
                skill,
                revision,
                SkillOperationV1::Merge { source_artifacts },
                evidence_episode_ids,
                mutation_id,
                generated_at_ms,
                parent_capabilities,
                candidate,
            )?]
        }
        SkillMutationDraftOperationV1::Split {
            source_skill_id,
            skills,
        } => {
            let source = require_parent(candidate, parent, &source_skill_id)?;
            let source_artifact_digest = source.digest()?;
            let result_skill_ids = skills
                .iter()
                .map(|skill| skill.skill_id.clone())
                .collect::<BTreeSet<_>>();
            let operation = SkillOperationV1::Split {
                source_skill_id: source_skill_id.clone(),
                source_artifact_digest,
                result_skill_ids,
            };
            let mut results = Vec::with_capacity(skills.len());
            for skill in skills {
                let revision = if skill.skill_id == source_skill_id {
                    next_revision(candidate, source)?
                } else if parent.contains_key(&skill.skill_id) {
                    return Err(SkillMutationError::SkillAlreadyExists {
                        candidate,
                        skill_id: skill.skill_id,
                    });
                } else {
                    1
                };
                results.push(new_artifact(
                    skill,
                    revision,
                    operation.clone(),
                    evidence_episode_ids,
                    mutation_id,
                    generated_at_ms,
                    parent_capabilities,
                    candidate,
                )?);
            }
            results
        }
        SkillMutationDraftOperationV1::Deprecate { skill_id } => vec![lifecycle_artifact(
            candidate,
            parent,
            skill_id,
            evidence_episode_ids,
            mutation_id,
            generated_at_ms,
            false,
        )?],
        SkillMutationDraftOperationV1::Delete { skill_id } => vec![lifecycle_artifact(
            candidate,
            parent,
            skill_id,
            evidence_episode_ids,
            mutation_id,
            generated_at_ms,
            true,
        )?],
    };
    artifacts.sort_by(|left, right| {
        (left.skill_id.as_str(), left.revision).cmp(&(right.skill_id.as_str(), right.revision))
    });
    Ok(artifacts)
}

#[allow(clippy::too_many_arguments)]
fn new_artifact(
    skill: SkillContentDraftV1,
    revision: u32,
    operation: SkillOperationV1,
    evidence_episode_ids: &BTreeSet<EpisodeId>,
    mutation_id: &MutationId,
    generated_at_ms: u64,
    parent_capabilities: &BTreeSet<String>,
    candidate: usize,
) -> Result<SkillArtifactV1, SkillMutationError> {
    if !skill.required_capabilities.is_subset(parent_capabilities) {
        return Err(SkillMutationError::CapabilityExpansion {
            candidate,
            skill_id: skill.skill_id,
        });
    }
    let artifact = SkillArtifactV1 {
        schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
        skill_id: skill.skill_id,
        revision,
        operation,
        name: skill.name,
        description: skill.description,
        instructions: skill.instructions,
        trigger_policy: skill.trigger_policy,
        required_capabilities: skill.required_capabilities,
        source_episode_ids: evidence_episode_ids.clone(),
        mutation_id: mutation_id.clone(),
        status_history: vec![SkillStatusTransitionV1 {
            status: SkillStatusV1::Quarantined,
            recorded_at_ms: generated_at_ms,
            evaluation_report_id: None,
        }],
    };
    artifact
        .validate()
        .map_err(|source| SkillMutationError::InvalidArtifact { candidate, source })?;
    Ok(artifact)
}

#[allow(clippy::too_many_arguments)]
fn lifecycle_artifact(
    candidate: usize,
    parent: &BTreeMap<SkillId, SkillArtifactV1>,
    skill_id: SkillId,
    evidence_episode_ids: &BTreeSet<EpisodeId>,
    mutation_id: &MutationId,
    generated_at_ms: u64,
    delete: bool,
) -> Result<SkillArtifactV1, SkillMutationError> {
    let previous = require_parent(candidate, parent, &skill_id)?;
    if previous
        .status_history
        .last()
        .is_some_and(|status| status.recorded_at_ms >= generated_at_ms)
    {
        return Err(SkillMutationError::NonMonotonicStatusTime {
            candidate,
            skill_id,
        });
    }
    let previous_digest = previous.digest()?;
    let mut artifact = previous.clone();
    artifact.revision = next_revision(candidate, previous)?;
    artifact.operation = if delete {
        SkillOperationV1::Delete {
            previous_artifact_digest: previous_digest,
            deletion_mode: SkillDeletionModeV1::LogicalTombstone,
        }
    } else {
        SkillOperationV1::Deprecate {
            previous_artifact_digest: previous_digest,
        }
    };
    artifact.source_episode_ids = evidence_episode_ids.clone();
    artifact.mutation_id = mutation_id.clone();
    artifact.status_history.push(SkillStatusTransitionV1 {
        status: if delete {
            SkillStatusV1::Deleted
        } else {
            SkillStatusV1::Deprecated
        },
        recorded_at_ms: generated_at_ms,
        evaluation_report_id: None,
    });
    artifact
        .validate()
        .map_err(|source| SkillMutationError::InvalidArtifact { candidate, source })?;
    Ok(artifact)
}

fn require_parent<'a>(
    candidate: usize,
    parent: &'a BTreeMap<SkillId, SkillArtifactV1>,
    skill_id: &SkillId,
) -> Result<&'a SkillArtifactV1, SkillMutationError> {
    parent
        .get(skill_id)
        .ok_or_else(|| SkillMutationError::SourceSkillNotFound {
            candidate,
            skill_id: skill_id.clone(),
        })
}

fn next_revision(candidate: usize, previous: &SkillArtifactV1) -> Result<u32, SkillMutationError> {
    previous
        .revision
        .checked_add(1)
        .ok_or_else(|| SkillMutationError::RevisionOverflow {
            candidate,
            skill_id: previous.skill_id.clone(),
        })
}

/// Skill Mutator 的生成、可信绑定或边界校验错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillMutationError {
    /// Parent Genome Revision 自身无效。
    #[error("Parent Genome Revision 无效：{0}")]
    InvalidParent(String),
    /// 可信生成时间不能为零。
    #[error("Skill 候选生成时间不能为零")]
    InvalidGeneratedAt,
    /// 脱敏证据与 Parent Genome 摘要不一致。
    #[error("MutationEvidence 与 Parent GenomeDigest 不一致")]
    ParentGenomeDigestMismatch,
    /// 脱敏证据没有任何获准 Episode。
    #[error("Skill MutationEvidence 必须至少包含一条获准 Episode")]
    MissingMutationEvidence,
    /// Episode 没有绑定精确 Parent Revision。
    #[error("Episode {episode_id} 没有绑定 Skill Mutator 的 Parent Revision")]
    EpisodeParentMismatch {
        /// 错绑 Episode。
        episode_id: EpisodeId,
    },
    /// 同一 Episode 在证据中出现多次。
    #[error("Skill MutationEvidence 重复引用 Episode {episode_id}")]
    DuplicateMutationEvidence {
        /// 重复 Episode。
        episode_id: EpisodeId,
    },
    /// Parent Skill ID 不符合强类型协议。
    #[error("Parent Genome Skill ID `{value}` 无效：{reason}")]
    InvalidParentSkillId {
        /// 原始 ID。
        value: String,
        /// 校验原因。
        reason: String,
    },
    /// Parent SkillRef 与真实 CAS 制品绑定不一致。
    #[error("Parent Skill `{skill_id}` 与 CAS 制品绑定不一致")]
    ParentSkillBindingMismatch {
        /// 错绑 Skill。
        skill_id: SkillId,
    },
    /// Parent Skill 尚未通过 Commit Gate。
    #[error("Parent Skill `{0}` 的状态不是 Active")]
    ParentSkillNotActive(SkillId),
    /// Parent Genome 重复声明 Skill ID。
    #[error("Parent Genome 重复声明 Skill `{0}`")]
    DuplicateParentSkill(SkillId),
    /// 生成器失败。
    #[error(transparent)]
    Generation(#[from] SkillMutationGenerationError),
    /// 生成器没有返回固定数量的候选。
    #[error("Skill 候选数量错误：期望 {expected}，实际 {actual}")]
    InvalidCandidateCount {
        /// 固定数量。
        expected: usize,
        /// 实际数量。
        actual: usize,
    },
    /// 候选假设为空。
    #[error("Skill 候选 {candidate} 的假设不能为空")]
    EmptyHypothesis {
        /// 从零开始的候选序号。
        candidate: usize,
    },
    /// 候选假设过长。
    #[error("Skill 候选 {candidate} 的假设为 {actual} 字节，超过上限 {maximum}")]
    HypothesisTooLong {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际字节数。
        actual: usize,
        /// 固定上限。
        maximum: usize,
    },
    /// Split 结果数量越界。
    #[error("Skill 候选 {candidate} 的 Split 结果数量为 {actual}，要求 {minimum}..={maximum}")]
    InvalidSplitResultCount {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际数量。
        actual: usize,
        /// 最少数量。
        minimum: usize,
        /// 最大数量。
        maximum: usize,
    },
    /// Split 重复产生同一 Skill ID。
    #[error("Skill 候选 {candidate} 的 Split 结果包含重复 Skill ID")]
    DuplicateSplitResult {
        /// 从零开始的候选序号。
        candidate: usize,
    },
    /// Merge 来源数量越界。
    #[error("Skill 候选 {candidate} 的 Merge 来源数量为 {actual}，要求 {minimum}..={maximum}")]
    InvalidMergeSourceCount {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际数量。
        actual: usize,
        /// 最少数量。
        minimum: usize,
        /// 最大数量。
        maximum: usize,
    },
    /// 候选草案过大。
    #[error("Skill 候选 {candidate} 为 {actual} 字节，超过上限 {maximum}")]
    DraftTooLarge {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 实际字节数。
        actual: usize,
        /// 固定上限。
        maximum: usize,
    },
    /// 两个规范化候选完全相同。
    #[error("Skill 候选 {duplicate_candidate} 与候选 {first_candidate} 重复")]
    DuplicateCandidate {
        /// 首次出现的候选序号。
        first_candidate: usize,
        /// 重复候选序号。
        duplicate_candidate: usize,
    },
    /// Create、Merge 或 Split 试图覆盖非来源 Skill。
    #[error("Skill 候选 {candidate} 试图创建已存在的 Skill `{skill_id}`")]
    SkillAlreadyExists {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 冲突 Skill。
        skill_id: SkillId,
    },
    /// 操作引用的来源不在 Parent 中。
    #[error("Skill 候选 {candidate} 引用不存在的来源 Skill `{skill_id}`")]
    SourceSkillNotFound {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 缺失 Skill。
        skill_id: SkillId,
    },
    /// 候选请求了 Parent 不具备的能力。
    #[error("Skill 候选 {candidate} 的 `{skill_id}` 请求了 Parent 未提供的能力")]
    CapabilityExpansion {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 扩权 Skill。
        skill_id: SkillId,
    },
    /// Parent Skill 修订号溢出。
    #[error("Skill 候选 {candidate} 的 `{skill_id}` 修订号已溢出")]
    RevisionOverflow {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 溢出 Skill。
        skill_id: SkillId,
    },
    /// Deprecate/Delete 时间没有严格晚于既有状态链。
    #[error("Skill 候选 {candidate} 的 `{skill_id}` 生命周期时间没有严格递增")]
    NonMonotonicStatusTime {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 出错 Skill。
        skill_id: SkillId,
    },
    /// 单个可信重建制品违反协议。
    #[error("Skill 候选 {candidate} 的制品无效：{source}")]
    InvalidArtifact {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 共享协议错误。
        source: InvalidSkillEvolution,
    },
    /// 正式 Proposal 违反共享协议。
    #[error("Skill 候选 {candidate} 的 Proposal 无效：{source}")]
    InvalidProposal {
        /// 从零开始的候选序号。
        candidate: usize,
        /// 共享协议错误。
        source: InvalidSkillEvolution,
    },
    /// 草案无法稳定序列化。
    #[error("序列化 Skill 候选草案失败：{0}")]
    SerializeDraft(serde_json::Error),
    /// 无法从受信证据与规范草案派生稳定 Mutation ID。
    #[error("构造确定性 Skill Mutation ID 失败：{0}")]
    DeterministicMutationId(String),
    /// Skill CAS 读取或校验失败。
    #[error(transparent)]
    SkillRepository(#[from] SkillRepositoryError),
    /// Skill Artifact 局部协议复核失败。
    #[error("Skill Artifact 无效：{0}")]
    InvalidSkillArtifact(#[from] InvalidSkillEvolution),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        AgentGenome, ArtifactDigest, DiagnosticStatus, FailureKind, GenomeMetadata, ModelGenome,
        Outcome, PluginGenome, PromptGenome, ReplayabilityGrade, RuntimeIdentity, SkillRef,
        ToolProfileGenome, UsageSummary, GENOME_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::{path::PathBuf, sync::Arc};
    use uuid::Uuid;

    use crate::{MutationEpisodeEvidence, MutationFailureEvidence};

    /// 返回脚本化草案的离线生成器。
    #[derive(Clone)]
    struct ScriptedGenerator {
        drafts: Arc<Vec<SkillMutationDraftV1>>,
    }

    #[async_trait]
    impl SkillMutationGenerator for ScriptedGenerator {
        /// 返回固定测试草案，证明外层边界不依赖模型行为。
        async fn generate(
            &self,
            _request: SkillMutationRequestV1<'_>,
        ) -> Result<Vec<SkillMutationDraftV1>, SkillMutationGenerationError> {
            Ok(self.drafts.as_ref().clone())
        }
    }

    fn digest(character: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-skill-mutator-{}", Uuid::new_v4().simple()))
    }

    fn active_artifact(skill_id: &str) -> SkillArtifactV1 {
        let report = agent_evolution_protocol::EvaluationReportId::generate();
        SkillArtifactV1 {
            schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
            skill_id: SkillId::new(skill_id).expect("测试 Skill ID 应合法"),
            revision: 1,
            operation: SkillOperationV1::Create,
            name: format!("{skill_id}-name"),
            description: "现有 Active Skill".into(),
            instructions: "执行现有受控指令。".into(),
            trigger_policy: SkillTriggerPolicyV1::default(),
            required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            source_episode_ids: BTreeSet::from([EpisodeId::generate()]),
            mutation_id: MutationId::generate(),
            status_history: vec![
                SkillStatusTransitionV1 {
                    status: SkillStatusV1::Quarantined,
                    recorded_at_ms: 1,
                    evaluation_report_id: None,
                },
                SkillStatusTransitionV1 {
                    status: SkillStatusV1::Evaluated,
                    recorded_at_ms: 2,
                    evaluation_report_id: Some(report.clone()),
                },
                SkillStatusTransitionV1 {
                    status: SkillStatusV1::Active,
                    recorded_at_ms: 3,
                    evaluation_report_id: Some(report),
                },
            ],
        }
    }

    async fn fixture() -> (PathBuf, FileArtifactStore, GenomeRevision) {
        let root = root();
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let repository = SkillArtifactRepository::new(&artifacts);
        let first = active_artifact("skill_parenta1");
        let second = active_artifact("skill_parentb2");
        let first_ref = repository.put(&first).await.expect("应写入 Parent Skill");
        let second_ref = repository.put(&second).await.expect("应写入 Parent Skill");
        let revision = GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".into(),
                    git_commit: "test".into(),
                    git_dirty: false,
                    target_triple: "test-target".into(),
                    features: BTreeSet::new(),
                },
                model: ModelGenome {
                    provider: "fixture".into(),
                    provider_kind: "fixture".into(),
                    model: "deterministic".into(),
                    base_url: None,
                    protocol: None,
                    max_tokens: None,
                    temperature: None,
                    stream: false,
                    provider_options_digest: None,
                },
                prompt: PromptGenome::default(),
                plugins: vec![PluginGenome {
                    id: "agent.skill-loader".into(),
                    version: "1.0.0".into(),
                    api_version: "1".into(),
                    bundle: digest('a'),
                    manifest_digest: Some(digest('b')),
                    config_digest: None,
                    capability_profile_digest: Some(digest('c')),
                    load_order: Some(0),
                    hook_order: Vec::new(),
                }],
                capability_owners: BTreeMap::from([(
                    "episode.read_redacted".into(),
                    "agent.skill-loader".into(),
                )]),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: vec![
                    SkillRef {
                        id: first.skill_id.to_string(),
                        content: first_ref.digest,
                    },
                    SkillRef {
                        id: second.skill_id.to_string(),
                        content: second_ref.digest,
                    },
                ],
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("Parent Genome 应合法");
        (root, artifacts, revision)
    }

    fn evidence(parent: &GenomeRevision) -> MutationEvidence {
        MutationEvidence {
            issue_id: agent_evolution_protocol::EvolutionIssueId::generate(),
            genome_digest: parent.digest.clone(),
            failure_kind: FailureKind::VerificationFailure,
            root_cause_hypothesis: "现有 Skill 未覆盖稳定失败".into(),
            expected_behavior: "候选 Skill 应通过独立验证".into(),
            confidence: 1.0,
            status: DiagnosticStatus::EligibleForEvolution,
            episodes: vec![MutationEpisodeEvidence {
                outbox_id: "outbox-skill".into(),
                episode_id: EpisodeId::generate(),
                genome_revision_id: parent.revision_id.clone(),
                outcome: Outcome::TaskFailure,
                task_family: "skill-evolution".into(),
                tags: BTreeSet::new(),
                failure: MutationFailureEvidence {
                    kind: FailureKind::VerificationFailure,
                    confidence: 1.0,
                    rule_derived: true,
                    model_assisted: false,
                },
                usage: UsageSummary::default(),
                replayability: ReplayabilityGrade::FixtureReproducible,
            }],
        }
    }

    fn content(skill_id: &str, suffix: &str) -> SkillContentDraftV1 {
        SkillContentDraftV1 {
            skill_id: SkillId::new(skill_id).expect("测试 Skill ID 应合法"),
            name: format!("候选 {suffix}"),
            description: format!("验证 {suffix} 操作"),
            instructions: format!("只执行 {suffix} 的受控步骤。"),
            trigger_policy: SkillTriggerPolicyV1::default(),
            required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
        }
    }

    fn generator(drafts: Vec<SkillMutationDraftV1>) -> ScriptedGenerator {
        ScriptedGenerator {
            drafts: Arc::new(drafts),
        }
    }

    /// Create、Deprecate、Delete 的可信字段必须由 Mutator 重建。
    #[tokio::test]
    async fn materializes_create_and_lifecycle_operations_from_trusted_parent() {
        let (root, artifacts, parent) = fixture().await;
        let drafts = vec![
            SkillMutationDraftV1 {
                hypothesis: "创建缺失能力".into(),
                operation: SkillMutationDraftOperationV1::Create {
                    skill: content("skill_created1", "create"),
                },
            },
            SkillMutationDraftV1 {
                hypothesis: "弃用误触发能力".into(),
                operation: SkillMutationDraftOperationV1::Deprecate {
                    skill_id: SkillId::new("skill_parenta1").expect("测试 ID 应合法"),
                },
            },
            SkillMutationDraftV1 {
                hypothesis: "逻辑删除危险能力".into(),
                operation: SkillMutationDraftOperationV1::Delete {
                    skill_id: SkillId::new("skill_parentb2").expect("测试 ID 应合法"),
                },
            },
        ];
        let evidence = evidence(&parent);
        let proposals = BoundedSkillMutator::m7(generator(drafts.clone()))
            .propose(&parent, &evidence, 10, &artifacts)
            .await
            .expect("三类操作应可信物化");
        let retried = BoundedSkillMutator::m7(generator(drafts))
            .propose(&parent, &evidence, 10, &artifacts)
            .await
            .expect("相同可信输入应可幂等重算");

        assert_eq!(proposals.len(), M7_SKILL_CANDIDATE_COUNT);
        assert_eq!(proposals, retried);
        assert_eq!(proposals[0].parent_revision_id, parent.revision_id);
        assert_eq!(proposals[0].parent_genome_digest, parent.digest);
        assert!(matches!(
            proposals[0].proposed_artifacts[0].operation,
            SkillOperationV1::Create
        ));
        assert!(matches!(
            proposals[1].proposed_artifacts[0].operation,
            SkillOperationV1::Deprecate { .. }
        ));
        assert_eq!(
            proposals[1].proposed_artifacts[0]
                .status_history
                .last()
                .map(|status| status.status),
            Some(SkillStatusV1::Deprecated)
        );
        assert!(matches!(
            proposals[2].proposed_artifacts[0].operation,
            SkillOperationV1::Delete {
                deletion_mode: SkillDeletionModeV1::LogicalTombstone,
                ..
            }
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Update、Merge、Split 的来源摘要和修订号必须来自 Parent CAS。
    #[tokio::test]
    async fn materializes_update_merge_and_split_with_exact_sources() {
        let (root, artifacts, parent) = fixture().await;
        let first = SkillId::new("skill_parenta1").expect("测试 ID 应合法");
        let second = SkillId::new("skill_parentb2").expect("测试 ID 应合法");
        let drafts = vec![
            SkillMutationDraftV1 {
                hypothesis: "更新现有指令".into(),
                operation: SkillMutationDraftOperationV1::Update {
                    skill: content(first.as_str(), "update"),
                },
            },
            SkillMutationDraftV1 {
                hypothesis: "合并重复能力".into(),
                operation: SkillMutationDraftOperationV1::Merge {
                    source_skill_ids: BTreeSet::from([first.clone(), second]),
                    skill: content("skill_merged01", "merge"),
                },
            },
            SkillMutationDraftV1 {
                hypothesis: "拆分混合职责".into(),
                operation: SkillMutationDraftOperationV1::Split {
                    source_skill_id: first.clone(),
                    skills: vec![
                        content("skill_splita01", "split-a"),
                        content("skill_splitb02", "split-b"),
                    ],
                },
            },
        ];
        let proposals = BoundedSkillMutator::m7(generator(drafts))
            .propose(&parent, &evidence(&parent), 10, &artifacts)
            .await
            .expect("三类内容操作应可信物化");

        assert!(matches!(
            proposals[0].proposed_artifacts[0].operation,
            SkillOperationV1::Update { .. }
        ));
        assert_eq!(proposals[0].proposed_artifacts[0].revision, 2);
        assert!(matches!(
            proposals[1].proposed_artifacts[0].operation,
            SkillOperationV1::Merge { .. }
        ));
        assert_eq!(proposals[2].proposed_artifacts.len(), 2);
        assert!(proposals[2]
            .proposed_artifacts
            .iter()
            .all(|artifact| matches!(artifact.operation, SkillOperationV1::Split { .. })));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 固定数量、唯一性与 Parent 能力子集由外层 Mutator 强制。
    #[tokio::test]
    async fn rejects_duplicate_and_capability_expanding_drafts() {
        let (root, artifacts, parent) = fixture().await;
        let duplicate = SkillMutationDraftV1 {
            hypothesis: "重复候选".into(),
            operation: SkillMutationDraftOperationV1::Create {
                skill: content("skill_duplicate", "duplicate"),
            },
        };
        let error = BoundedSkillMutator::m7(generator(vec![
            duplicate.clone(),
            duplicate.clone(),
            duplicate,
        ]))
        .propose(&parent, &evidence(&parent), 10, &artifacts)
        .await
        .expect_err("重复候选必须被拒绝");
        assert!(matches!(
            error,
            SkillMutationError::DuplicateCandidate { .. }
        ));

        let mut expanded = content("skill_expanded1", "expanded");
        expanded.required_capabilities.insert("process_exec".into());
        let drafts = vec![
            SkillMutationDraftV1 {
                hypothesis: "越界能力".into(),
                operation: SkillMutationDraftOperationV1::Create { skill: expanded },
            },
            SkillMutationDraftV1 {
                hypothesis: "第二候选".into(),
                operation: SkillMutationDraftOperationV1::Create {
                    skill: content("skill_second02", "second"),
                },
            },
            SkillMutationDraftV1 {
                hypothesis: "第三候选".into(),
                operation: SkillMutationDraftOperationV1::Create {
                    skill: content("skill_third003", "third"),
                },
            },
        ];
        let error = BoundedSkillMutator::m7(generator(drafts))
            .propose(&parent, &evidence(&parent), 10, &artifacts)
            .await
            .expect_err("能力扩大必须被拒绝");
        assert!(matches!(
            error,
            SkillMutationError::CapabilityExpansion { .. }
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
