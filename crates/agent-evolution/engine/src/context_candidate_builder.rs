//! 从受限 Context Policy 提案构建并登记可信 Candidate Genome。

use crate::{
    context_policy::{ContextPolicyRepository, ContextPolicyRepositoryError},
    verify_allowed_genome_diff, FileArtifactStore, FileGenomeStore, GenomeDiffError, GenomeStore,
    GenomeStoreError,
};
use agent_evolution_protocol::{
    CandidateId, ContextPolicyCandidateV1, ContextPolicyMutationProposalV1, EvolutionCycleId,
    GenomeDigest, GenomeMetadata, GenomeRevision, GenomeRevisionError, GenomeRevisionId,
    InvalidContextMutation, MutationId, MutationSurface, CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION,
    NATIVE_CONTEXT_POLICY_ID,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};

/// 使用真实 Genome Store 与 Artifact CAS 构建 Context Policy Candidate 的可信边界。
///
/// Builder 只替换既有 `PolicyRef.config_digest`，不会改变策略 ID、插件 bundle、能力 owner
/// 或任何其他 Genome 表面。Parent 与 Candidate 策略都必须是 CAS 中可复读的规范 V1 制品。
#[derive(Debug)]
pub struct ContextCandidateBuilder<'a> {
    genomes: &'a FileGenomeStore,
    policies: ContextPolicyRepository<'a>,
}

impl<'a> ContextCandidateBuilder<'a> {
    /// 创建绑定真实文件 Store 的 Candidate Builder，不访问文件系统。
    pub fn new(genomes: &'a FileGenomeStore, artifacts: &'a FileArtifactStore) -> Self {
        Self {
            genomes,
            policies: ContextPolicyRepository::new(artifacts),
        }
    }

    /// 使用控制面当前时间构建唯一修改 Context Policy 参数的 Candidate。
    ///
    /// # Errors
    ///
    /// 系统时间不可用，或 [`Self::build_at`] 的任一可信边界校验失败时返回
    /// [`ContextCandidateBuildError`]。
    pub async fn build(
        &self,
        cycle_id: EvolutionCycleId,
        proposal: &ContextPolicyMutationProposalV1,
    ) -> Result<ContextPolicyCandidateV1, ContextCandidateBuildError> {
        let created_at_ms =
            u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
                .map_err(|_| ContextCandidateBuildError::TimestampOverflow)?;
        self.build_at(cycle_id, proposal, created_at_ms).await
    }

    /// 使用受信调用方给定的创建时间构建 Candidate，支持崩溃后的幂等重试。
    ///
    /// 方法先读取并复核 Parent Genome 与 Parent Policy CAS，再规范化候选策略写入 CAS，
    /// 计算完整 Genome Diff，最后只追加 Candidate Revision。相同 Cycle、Mutation 和内容会
    /// 派生相同 ID；重复调用只在已存在内容完全一致时成功。
    ///
    /// # Errors
    ///
    /// 提案无效、Parent 或策略错绑、真实能力 owner 不匹配、策略未变化、Diff 越界、稳定
    /// ID 无效，或任一 Store 操作失败时返回 [`ContextCandidateBuildError`]。
    pub async fn build_at(
        &self,
        cycle_id: EvolutionCycleId,
        proposal: &ContextPolicyMutationProposalV1,
        created_at_ms: u64,
    ) -> Result<ContextPolicyCandidateV1, ContextCandidateBuildError> {
        proposal
            .validate()
            .map_err(ContextCandidateBuildError::InvalidProposal)?;
        let parent = self
            .genomes
            .get(&proposal.parent_revision_id)
            .await?
            .ok_or_else(|| {
                ContextCandidateBuildError::ParentNotFound(proposal.parent_revision_id.clone())
            })?;
        if parent.digest != proposal.parent_genome_digest {
            return Err(ContextCandidateBuildError::ParentDigestMismatch {
                declared: proposal.parent_genome_digest.clone(),
                actual: parent.digest.clone(),
            });
        }

        let parent_ref = parent
            .genome
            .context_policy
            .as_ref()
            .ok_or(ContextCandidateBuildError::MissingParentPolicy)?;
        if parent_ref.config_digest != proposal.parent_policy_digest {
            return Err(ContextCandidateBuildError::ParentPolicyDigestMismatch {
                declared: proposal.parent_policy_digest.clone(),
                actual: parent_ref.config_digest.clone(),
            });
        }
        if parent_ref.id != NATIVE_CONTEXT_POLICY_ID {
            return Err(ContextCandidateBuildError::ContextPolicyOwnerMismatch {
                actual: parent_ref.id.clone(),
            });
        }

        let parent_policy = self.policies.get(&parent_ref.config_digest).await?;
        if parent_policy == proposal.candidate_policy {
            return Err(ContextCandidateBuildError::UnchangedPolicy(
                parent_ref.config_digest.clone(),
            ));
        }
        let expected_candidate_digest = self.policies.digest(&proposal.candidate_policy)?;
        if expected_candidate_digest == parent_ref.config_digest {
            return Err(ContextCandidateBuildError::UnchangedPolicy(
                parent_ref.config_digest.clone(),
            ));
        }
        let candidate_artifact = self.policies.put(&proposal.candidate_policy).await?;
        if candidate_artifact.digest != expected_candidate_digest {
            return Err(ContextCandidateBuildError::CandidatePolicyDigestMismatch {
                expected: expected_candidate_digest,
                actual: candidate_artifact.digest,
            });
        }

        let mut candidate_genome = parent.genome.clone();
        candidate_genome
            .context_policy
            .as_mut()
            .expect("Parent Policy 已在前置校验中确认存在")
            .config_digest = candidate_artifact.digest.clone();
        let mut candidate_revision = GenomeRevision::create(
            candidate_genome,
            GenomeMetadata {
                created_at: None,
                description: None,
                parent: Some(parent.revision_id.clone()),
                mutation: Some(proposal.mutation_id.clone()),
            },
        )?;
        candidate_revision.revision_id = deterministic_revision_id(
            &cycle_id,
            &proposal.mutation_id,
            &candidate_revision.digest,
        )?;

        let expected_surfaces = BTreeSet::from([MutationSurface::ContextPolicy]);
        let diff = verify_allowed_genome_diff(&parent, &candidate_revision, &expected_surfaces)?;
        if diff.changed_surfaces != expected_surfaces {
            return Err(ContextCandidateBuildError::UnexpectedDiff {
                changed_surfaces: diff.changed_surfaces,
            });
        }

        let candidate_id = deterministic_candidate_id(
            &cycle_id,
            &proposal.mutation_id,
            &candidate_revision.revision_id,
        )?;
        let candidate = ContextPolicyCandidateV1 {
            schema_version: CONTEXT_POLICY_CANDIDATE_SCHEMA_VERSION,
            candidate_id,
            cycle_id,
            mutation_id: proposal.mutation_id.clone(),
            parent_revision_id: parent.revision_id.clone(),
            candidate_revision_id: candidate_revision.revision_id.clone(),
            parent_genome_digest: parent.digest,
            candidate_genome_digest: candidate_revision.digest.clone(),
            parent_policy_digest: parent_ref.config_digest.clone(),
            candidate_policy_digest: candidate_artifact.digest,
            changed_surfaces: diff.changed_surfaces,
            created_at_ms,
        };
        candidate
            .validate()
            .map_err(ContextCandidateBuildError::InvalidCandidate)?;

        match self.genomes.append(&candidate_revision).await {
            Ok(()) => {}
            Err(GenomeStoreError::AlreadyExists(existing_id))
                if existing_id == candidate_revision.revision_id =>
            {
                let existing = self
                    .genomes
                    .get(&candidate_revision.revision_id)
                    .await?
                    .ok_or_else(|| {
                        ContextCandidateBuildError::MissingIdempotentRevision(
                            candidate_revision.revision_id.clone(),
                        )
                    })?;
                if existing != candidate_revision {
                    return Err(ContextCandidateBuildError::IdempotentRevisionConflict(
                        candidate_revision.revision_id,
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(candidate)
    }
}

/// 从稳定输入派生 Candidate Genome Revision ID。
fn deterministic_revision_id(
    cycle_id: &EvolutionCycleId,
    mutation_id: &MutationId,
    digest: &GenomeDigest,
) -> Result<GenomeRevisionId, ContextCandidateBuildError> {
    let body = deterministic_id_body(&[
        b"context-policy-genome-revision-v1",
        cycle_id.as_str().as_bytes(),
        mutation_id.as_str().as_bytes(),
        digest.as_str().as_bytes(),
    ]);
    GenomeRevisionId::new(format!("{}_{}", GenomeRevisionId::PREFIX, body))
        .map_err(|error| ContextCandidateBuildError::DeterministicId(error.to_string()))
}

/// 从稳定输入派生 Context Policy Candidate ID。
fn deterministic_candidate_id(
    cycle_id: &EvolutionCycleId,
    mutation_id: &MutationId,
    revision_id: &GenomeRevisionId,
) -> Result<CandidateId, ContextCandidateBuildError> {
    let body = deterministic_id_body(&[
        b"context-policy-candidate-v1",
        cycle_id.as_str().as_bytes(),
        mutation_id.as_str().as_bytes(),
        revision_id.as_str().as_bytes(),
    ]);
    CandidateId::new(format!("{}_{}", CandidateId::PREFIX, body))
        .map_err(|error| ContextCandidateBuildError::DeterministicId(error.to_string()))
}

/// 对带长度分隔的稳定字段计算 SHA-256，避免简单拼接歧义。
fn deterministic_id_body(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

/// Context Policy Candidate 的可信构建错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextCandidateBuildError {
    /// 提案未通过 Context 专用结构校验。
    #[error("Context Policy 提案无效：{0}")]
    InvalidProposal(InvalidContextMutation),
    /// Parent Revision 不存在。
    #[error("Parent Genome 修订不存在：{0}")]
    ParentNotFound(GenomeRevisionId),
    /// 提案声明的 Parent Genome 摘要与 Store 中真实值不一致。
    #[error("Parent Genome 摘要错绑：声明 {declared}，实际 {actual}")]
    ParentDigestMismatch {
        /// 提案声明摘要。
        declared: GenomeDigest,
        /// Store 中真实摘要。
        actual: GenomeDigest,
    },
    /// Parent 没有 Context Policy，不能把“新增策略”伪装成参数变异。
    #[error("Parent Genome 缺少 Context Policy 引用")]
    MissingParentPolicy,
    /// 提案声明的 Parent Policy 摘要与 Genome 中真实值不一致。
    #[error("Parent Context Policy 摘要错绑：声明 {declared}，实际 {actual}")]
    ParentPolicyDigestMismatch {
        /// 提案声明摘要。
        declared: agent_evolution_protocol::ArtifactDigest,
        /// Genome 中真实摘要。
        actual: agent_evolution_protocol::ArtifactDigest,
    },
    /// Context Policy ID 不是 Kernel 原生上下文能力的稳定 ID。
    #[error("Context Policy owner `{actual}` 不是原生 owner `{NATIVE_CONTEXT_POLICY_ID}`")]
    ContextPolicyOwnerMismatch {
        /// PolicyRef 中实际声明的 owner ID。
        actual: String,
    },
    /// Parent 与 Candidate 策略结构值或摘要相同。
    #[error("Candidate Context Policy 未发生变化：{0}")]
    UnchangedPolicy(agent_evolution_protocol::ArtifactDigest),
    /// CAS 返回的候选摘要与写入前可信计算值不一致。
    #[error("Candidate Context Policy 摘要不一致：期望 {expected}，实际 {actual}")]
    CandidatePolicyDigestMismatch {
        /// 写入前计算的摘要。
        expected: agent_evolution_protocol::ArtifactDigest,
        /// CAS 返回的摘要。
        actual: agent_evolution_protocol::ArtifactDigest,
    },
    /// Context Policy CAS 读写或规范性校验失败。
    #[error("Context Policy CAS 校验失败：{0}")]
    PolicyRepository(#[from] ContextPolicyRepositoryError),
    /// Candidate Genome 修订无法构造。
    #[error("Candidate Genome 修订无效：{0}")]
    GenomeRevision(#[from] GenomeRevisionError),
    /// 完整 Genome 差异校验失败。
    #[error("Candidate Genome 差异无效：{0}")]
    GenomeDiff(#[from] GenomeDiffError),
    /// 真实差异没有精确落在 Context Policy 表面。
    #[error("Candidate Genome 差异不是唯一 Context Policy：{changed_surfaces:?}")]
    UnexpectedDiff {
        /// Builder 计算出的实际差异表面。
        changed_surfaces: BTreeSet<MutationSurface>,
    },
    /// Candidate DTO 未通过协议校验。
    #[error("ContextPolicyCandidate 无效：{0}")]
    InvalidCandidate(InvalidContextMutation),
    /// 稳定身份派生结果违反强类型 ID 契约。
    #[error("构造 Context Policy Candidate 稳定身份失败：{0}")]
    DeterministicId(String),
    /// 幂等 Revision 已声明存在，但复读时缺失。
    #[error("幂等 Context Policy Candidate Revision 复读缺失：{0}")]
    MissingIdempotentRevision(GenomeRevisionId),
    /// 相同稳定 Revision ID 被不同内容占用。
    #[error("幂等 Context Policy Candidate Revision 内容冲突：{0}")]
    IdempotentRevisionConflict(GenomeRevisionId),
    /// 系统时钟早于 Unix Epoch。
    #[error("无法生成 Context Policy Candidate 时间戳：{0}")]
    Clock(#[from] SystemTimeError),
    /// 系统时间戳超过协议使用的 u64 毫秒范围。
    #[error("Context Policy Candidate 时间戳超过 u64 毫秒范围")]
    TimestampOverflow,
    /// Genome Store 读取或追加失败。
    #[error("访问 Genome Store 失败：{0}")]
    GenomeStore(#[from] GenomeStoreError),
}
