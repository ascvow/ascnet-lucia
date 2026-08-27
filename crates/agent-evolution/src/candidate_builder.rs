//! 从受限变异提案构建并登记可信 Candidate Genome。

use crate::{
    verify_allowed_genome_diff, ArtifactStore, ArtifactStoreError, FileArtifactStore,
    FileGenomeStore, GenomeDiffError, GenomeStore, GenomeStoreError,
};
use agent_evolution_protocol::{
    ArtifactDigest, EvolutionCycleId, GenomeDigest, GenomeMetadata, GenomeRevision,
    GenomeRevisionError, GenomeRevisionId, InvalidMutation, MutationCandidate, MutationProposal,
    MutationSurface, PromptLayer,
};
use std::{
    collections::BTreeSet,
    str::Utf8Error,
    time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};

/// M5 Task Strategy Prompt 允许的最大原始字节数。
pub const MAX_TASK_STRATEGY_PROMPT_BYTES: u64 = 64 * 1024;

/// 使用真实 Genome Store 与 Artifact CAS 构建 Candidate 的可信边界。
///
/// Builder 只执行 `TaskStrategy` Prompt 的单点替换。所有提案、Parent、CAS 内容和完整
/// Genome 差异均在追加新修订前校验，失败不会留下 Candidate 修订。
#[derive(Debug)]
pub struct CandidateBuilder<'a> {
    genome_store: &'a FileGenomeStore,
    artifact_store: &'a FileArtifactStore,
}

impl<'a> CandidateBuilder<'a> {
    /// 创建绑定真实文件 Store 的 Candidate Builder。
    ///
    /// 构造本身不访问文件系统；[`Self::build`] 才会读取 Parent、复核 CAS 并追加新修订。
    pub fn new(genome_store: &'a FileGenomeStore, artifact_store: &'a FileArtifactStore) -> Self {
        Self {
            genome_store,
            artifact_store,
        }
    }

    /// 从提案构建只修改唯一 Task Strategy Prompt 的 Candidate，并追加 Genome 修订。
    ///
    /// `cycle_id` 绑定 Candidate 所属进化周期；`proposal` 只能引用已存在 Parent 和真实
    /// Artifact CAS 中的 UTF-8 非空纯文本 Prompt。方法会重新计算完整 Genome 差异，且只
    /// 在 Candidate DTO 也通过协议校验后追加修订。
    ///
    /// # Errors
    ///
    /// 提案无效、Parent 不存在或错绑、Task Strategy 不唯一、Prompt 制品无效、真实差异
    /// 越界、系统时间不可用，或任一 Store 访问失败时返回 [`CandidateBuildError`]。所有
    /// 追加前错误均不会写入 Candidate Genome 修订。
    pub async fn build(
        &self,
        cycle_id: EvolutionCycleId,
        proposal: &MutationProposal,
    ) -> Result<MutationCandidate, CandidateBuildError> {
        proposal
            .validate()
            .map_err(CandidateBuildError::InvalidProposal)?;

        let parent = self
            .genome_store
            .get(&proposal.parent_revision_id)
            .await?
            .ok_or_else(|| {
                CandidateBuildError::ParentNotFound(proposal.parent_revision_id.clone())
            })?;
        if parent.digest != proposal.parent_genome_digest {
            return Err(CandidateBuildError::ParentDigestMismatch {
                declared: proposal.parent_genome_digest.clone(),
                actual: parent.digest.clone(),
            });
        }

        let task_strategy_index = unique_task_strategy_index(&parent)?;
        let prompt = proposal.patch.task_strategy_prompt();
        validate_prompt_media_type(&prompt.media_type)?;
        if prompt.size_bytes > MAX_TASK_STRATEGY_PROMPT_BYTES {
            return Err(CandidateBuildError::PromptTooLarge {
                size_bytes: prompt.size_bytes,
                max_bytes: MAX_TASK_STRATEGY_PROMPT_BYTES,
            });
        }

        let prompt_bytes = self
            .artifact_store
            .get(&prompt.digest)
            .await?
            .ok_or_else(|| CandidateBuildError::PromptArtifactNotFound(prompt.digest.clone()))?;
        let actual_size = prompt_bytes.len() as u64;
        if actual_size != prompt.size_bytes {
            return Err(CandidateBuildError::PromptSizeMismatch {
                declared: prompt.size_bytes,
                actual: actual_size,
            });
        }
        if actual_size > MAX_TASK_STRATEGY_PROMPT_BYTES {
            return Err(CandidateBuildError::PromptTooLarge {
                size_bytes: actual_size,
                max_bytes: MAX_TASK_STRATEGY_PROMPT_BYTES,
            });
        }
        let prompt_text = std::str::from_utf8(&prompt_bytes)?;
        if prompt_text.trim().is_empty() {
            return Err(CandidateBuildError::BlankPrompt);
        }

        let parent_prompt = &parent.genome.prompt.messages[task_strategy_index].artifact;
        if parent_prompt == &prompt.digest {
            return Err(CandidateBuildError::UnchangedPrompt(prompt.digest.clone()));
        }

        let mut candidate_genome = parent.genome.clone();
        candidate_genome.prompt.messages[task_strategy_index].artifact = prompt.digest.clone();
        let candidate_revision = GenomeRevision::create(
            candidate_genome,
            GenomeMetadata {
                created_at: None,
                description: None,
                parent: Some(parent.revision_id.clone()),
                mutation: Some(proposal.mutation_id.clone()),
            },
        )?;

        let expected_surfaces = BTreeSet::from([MutationSurface::TaskStrategyPrompt]);
        let diff = verify_allowed_genome_diff(&parent, &candidate_revision, &expected_surfaces)?;
        if diff.changed_surfaces != expected_surfaces {
            return Err(CandidateBuildError::UnexpectedDiff {
                changed_surfaces: diff.changed_surfaces,
            });
        }

        let created_at_ms =
            u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
                .map_err(|_| CandidateBuildError::TimestampOverflow)?;
        let candidate = MutationCandidate::create(
            cycle_id,
            proposal,
            candidate_revision.revision_id.clone(),
            candidate_revision.digest.clone(),
            diff.changed_surfaces,
            created_at_ms,
        )
        .map_err(CandidateBuildError::InvalidCandidate)?;

        self.genome_store.append(&candidate_revision).await?;
        Ok(candidate)
    }
}

/// 返回 Parent 中唯一 Task Strategy Prompt 的位置。
fn unique_task_strategy_index(parent: &GenomeRevision) -> Result<usize, CandidateBuildError> {
    let mut found = None;
    for (index, message) in parent.genome.prompt.messages.iter().enumerate() {
        if message.layer != PromptLayer::TaskStrategy {
            continue;
        }
        if found.replace(index).is_some() {
            return Err(CandidateBuildError::AmbiguousTaskStrategy);
        }
    }
    found.ok_or(CandidateBuildError::MissingTaskStrategy)
}

/// 校验 Candidate Prompt 使用受支持的有界纯文本媒体类型。
fn validate_prompt_media_type(media_type: &str) -> Result<(), CandidateBuildError> {
    let normalized = media_type.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "text/plain" | "text/plain; charset=utf-8"
    ) {
        return Ok(());
    }
    Err(CandidateBuildError::UnsupportedPromptMediaType(
        media_type.to_owned(),
    ))
}

/// Candidate Builder 在可信读取、全字段校验或最终追加阶段产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum CandidateBuildError {
    /// MutationProposal 未通过协议结构校验。
    #[error("MutationProposal 无效：{0}")]
    InvalidProposal(InvalidMutation),
    /// Proposal 引用的 Parent Genome 修订不存在。
    #[error("Parent Genome 修订不存在：{0}")]
    ParentNotFound(GenomeRevisionId),
    /// Proposal 声明的 Parent 摘要与 Store 中真实修订不一致。
    #[error("Parent Genome 摘要错绑：声明 {declared}，实际 {actual}")]
    ParentDigestMismatch {
        /// Proposal 声明的 Parent 摘要。
        declared: GenomeDigest,
        /// Store 中 Parent 的真实摘要。
        actual: GenomeDigest,
    },
    /// Parent 不包含 Task Strategy Prompt，无法执行受限替换。
    #[error("Parent Genome 缺少 Task Strategy Prompt")]
    MissingTaskStrategy,
    /// Parent 包含多条 Task Strategy Prompt，替换目标不唯一。
    #[error("Parent Genome 包含多条 Task Strategy Prompt")]
    AmbiguousTaskStrategy,
    /// Prompt 媒体类型不属于受支持的 UTF-8 纯文本类型。
    #[error("Candidate Prompt 媒体类型不受支持：{0}")]
    UnsupportedPromptMediaType(String),
    /// Prompt 声明或真实字节数超过安全上限。
    #[error("Candidate Prompt 过大：{size_bytes} 字节，上限 {max_bytes} 字节")]
    PromptTooLarge {
        /// 声明或实测的字节数。
        size_bytes: u64,
        /// Builder 允许的最大字节数。
        max_bytes: u64,
    },
    /// Prompt 引用在 Artifact CAS 中不存在。
    #[error("Candidate Prompt 制品不存在：{0}")]
    PromptArtifactNotFound(ArtifactDigest),
    /// Prompt 引用声明的长度与 CAS 中真实字节数不一致。
    #[error("Candidate Prompt 长度不匹配：声明 {declared}，实际 {actual}")]
    PromptSizeMismatch {
        /// Proposal 中声明的字节数。
        declared: u64,
        /// CAS 中读取到的真实字节数。
        actual: u64,
    },
    /// Prompt CAS 内容不是合法 UTF-8。
    #[error("Candidate Prompt 不是合法 UTF-8：{0}")]
    PromptNotUtf8(#[from] Utf8Error),
    /// Prompt 去除空白后没有有效内容。
    #[error("Candidate Prompt 不能为空白文本")]
    BlankPrompt,
    /// 新旧 Task Strategy Prompt 摘要相同，没有产生行为变化。
    #[error("Candidate Prompt 未发生变化：{0}")]
    UnchangedPrompt(ArtifactDigest),
    /// 新 Genome 修订无法构造。
    #[error("Candidate Genome 修订无效：{0}")]
    GenomeRevision(#[from] GenomeRevisionError),
    /// 完整 Genome 差异校验失败。
    #[error("Candidate Genome 差异无效：{0}")]
    GenomeDiff(#[from] GenomeDiffError),
    /// 完整差异没有精确落在唯一 Task Strategy Prompt 表面。
    #[error("Candidate Genome 差异不是唯一 Task Strategy Prompt：{changed_surfaces:?}")]
    UnexpectedDiff {
        /// Builder 可信计算出的实际变化表面。
        changed_surfaces: BTreeSet<MutationSurface>,
    },
    /// Candidate DTO 未通过协议校验。
    #[error("MutationCandidate 无效：{0}")]
    InvalidCandidate(InvalidMutation),
    /// 系统时钟早于 Unix Epoch，无法生成 Candidate 时间戳。
    #[error("无法生成 Candidate 时间戳：{0}")]
    Clock(#[from] SystemTimeError),
    /// 系统时间戳超过协议使用的 `u64` 毫秒范围。
    #[error("Candidate 时间戳超过 u64 毫秒范围")]
    TimestampOverflow,
    /// Artifact CAS 读取或完整性复核失败。
    #[error("读取 Candidate Prompt 制品失败：{0}")]
    ArtifactStore(#[from] ArtifactStoreError),
    /// Genome Store 读取 Parent 或追加 Candidate 失败。
    #[error("访问 Genome Store 失败：{0}")]
    GenomeStore(#[from] GenomeStoreError),
}
