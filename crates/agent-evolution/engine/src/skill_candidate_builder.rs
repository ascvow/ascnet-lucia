//! 从可信 Skill 变异提案构建并登记 Candidate Genome。

use crate::{
    skill_repository::{SkillArtifactRepository, SkillRepositoryError},
    verify_allowed_genome_diff, FileArtifactStore, FileGenomeStore, GenomeDiffError, GenomeStore,
    GenomeStoreError,
};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, EvolutionCycleId, GenomeDigest, GenomeMetadata, GenomeRevision,
    GenomeRevisionError, GenomeRevisionId, InvalidSkillEvolution, MutationId, MutationSurface,
    SkillArtifactV1, SkillCandidateV1, SkillGenomeRefV1, SkillId, SkillMutationProposalV1,
    SkillOperationV1, SkillRef, SkillStatusV1, SKILL_CANDIDATE_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};

/// 使用真实 Genome Store 与 Artifact CAS 构建 Skill Candidate 的可信边界。
///
/// Builder 会复读 Parent Genome 的每个 SkillArtifact、验证操作引用的精确 CAS 摘要，
/// 规范写入候选制品，并从完整 Genome 重新计算 Diff。模型或 Mutator 自报的能力、摘要与
/// 变化表面都不能替代这些复核。
#[derive(Debug)]
pub struct SkillCandidateBuilder<'a> {
    genomes: &'a FileGenomeStore,
    skills: SkillArtifactRepository<'a>,
}

impl<'a> SkillCandidateBuilder<'a> {
    /// 创建绑定真实文件 Store 的 Builder，不访问文件系统。
    pub fn new(genomes: &'a FileGenomeStore, artifacts: &'a FileArtifactStore) -> Self {
        Self {
            genomes,
            skills: SkillArtifactRepository::new(artifacts),
        }
    }

    /// 使用控制面当前时间构建只修改 Skill Set 的 Candidate。
    ///
    /// # Errors
    ///
    /// 系统时间不可用，或 [`Self::build_at`] 的任一可信边界失败时返回
    /// [`SkillCandidateBuildError`]。
    pub async fn build(
        &self,
        cycle_id: EvolutionCycleId,
        proposal: &SkillMutationProposalV1,
    ) -> Result<SkillCandidateV1, SkillCandidateBuildError> {
        let created_at_ms =
            u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
                .map_err(|_| SkillCandidateBuildError::TimestampOverflow)?;
        self.build_at(cycle_id, proposal, created_at_ms).await
    }

    /// 使用可信时间构建 Candidate，支持崩溃后的确定性幂等重试。
    ///
    /// `proposal` 必须是受信控制面收到的原始提案。方法依次复核 Proposal、Parent
    /// Revision、Parent Skill CAS、六类操作来源与 Parent 能力上限；随后写入候选制品、
    /// 构造排序后的 Genome Skill Set、计算完整 Diff，并只追加确定性 Genome Revision。
    ///
    /// # Errors
    ///
    /// 任一绑定不一致、来源制品缺失、能力扩大、Diff 不精确为 `{Skill}`、确定性 ID 无效，
    /// 或 Store 访问失败时返回 [`SkillCandidateBuildError`]。
    pub async fn build_at(
        &self,
        cycle_id: EvolutionCycleId,
        proposal: &SkillMutationProposalV1,
        created_at_ms: u64,
    ) -> Result<SkillCandidateV1, SkillCandidateBuildError> {
        proposal
            .validate()
            .map_err(SkillCandidateBuildError::InvalidProposal)?;
        if created_at_ms == 0 {
            return Err(SkillCandidateBuildError::InvalidTimestamp);
        }
        let parent = self
            .genomes
            .get(&proposal.parent_revision_id)
            .await?
            .ok_or_else(|| {
                SkillCandidateBuildError::ParentNotFound(proposal.parent_revision_id.clone())
            })?;
        if parent.digest != proposal.parent_genome_digest {
            return Err(SkillCandidateBuildError::ParentDigestMismatch {
                declared: proposal.parent_genome_digest.clone(),
                actual: parent.digest,
            });
        }

        let (parent_skill_set, parent_artifacts) = self.load_parent_skill_set(&parent).await?;
        let parent_capabilities = parent
            .genome
            .capability_owners
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        validate_proposal_against_parent(&parent_artifacts, proposal)?;
        let candidate_artifacts = self.persist_candidate_artifacts(proposal).await?;
        let candidate_skill_set = apply_proposal(
            &parent_skill_set,
            &candidate_artifacts,
            &parent_capabilities,
        )?;
        if candidate_skill_set == parent_skill_set {
            return Err(SkillCandidateBuildError::UnchangedSkillSet);
        }

        let candidate_capabilities = candidate_skill_set
            .iter()
            .flat_map(|skill| skill.required_capabilities.iter().cloned())
            .collect::<BTreeSet<_>>();
        if !candidate_capabilities.is_subset(&parent_capabilities) {
            return Err(SkillCandidateBuildError::CapabilityExpansion);
        }

        let mut candidate_genome = parent.genome.clone();
        candidate_genome.skills = candidate_skill_set
            .iter()
            .map(|skill| SkillRef {
                id: skill.skill_id.to_string(),
                content: skill.artifact_digest.clone(),
            })
            .collect();
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

        let expected_surfaces = BTreeSet::from([MutationSurface::Skill]);
        let diff = verify_allowed_genome_diff(&parent, &candidate_revision, &expected_surfaces)?;
        if diff.changed_surfaces != expected_surfaces {
            return Err(SkillCandidateBuildError::UnexpectedDiff {
                changed_surfaces: diff.changed_surfaces,
            });
        }
        let candidate_id = deterministic_candidate_id(
            &cycle_id,
            &proposal.mutation_id,
            &candidate_revision.revision_id,
        )?;
        let candidate = SkillCandidateV1 {
            schema_version: SKILL_CANDIDATE_SCHEMA_VERSION,
            candidate_id,
            cycle_id,
            mutation_id: proposal.mutation_id.clone(),
            parent_revision_id: parent.revision_id.clone(),
            candidate_revision_id: candidate_revision.revision_id.clone(),
            parent_genome_digest: parent.digest,
            candidate_genome_digest: candidate_revision.digest.clone(),
            parent_skill_set,
            candidate_skill_set,
            parent_capabilities,
            candidate_capabilities,
            candidate_artifact_digests: candidate_artifacts
                .iter()
                .map(|artifact| (artifact.artifact.skill_id.clone(), artifact.digest.clone()))
                .collect(),
            changed_surfaces: diff.changed_surfaces,
            evaluation_report_id: None,
            created_at_ms,
        };
        candidate
            .validate_bindings(proposal, None, &BTreeMap::new())
            .map_err(SkillCandidateBuildError::InvalidCandidate)?;

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
                        SkillCandidateBuildError::MissingIdempotentRevision(
                            candidate_revision.revision_id.clone(),
                        )
                    })?;
                if existing != candidate_revision {
                    return Err(SkillCandidateBuildError::IdempotentRevisionConflict(
                        candidate_revision.revision_id,
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(candidate)
    }

    async fn load_parent_skill_set(
        &self,
        parent: &GenomeRevision,
    ) -> Result<(Vec<SkillGenomeRefV1>, BTreeMap<SkillId, SkillArtifactV1>), SkillCandidateBuildError>
    {
        let mut result = Vec::with_capacity(parent.genome.skills.len());
        let mut artifacts = BTreeMap::new();
        for reference in &parent.genome.skills {
            let skill_id = SkillId::new(reference.id.clone()).map_err(|error| {
                SkillCandidateBuildError::InvalidParentSkillId {
                    value: reference.id.clone(),
                    reason: error.to_string(),
                }
            })?;
            let artifact = self.skills.get(&reference.content).await?;
            if artifact.skill_id != skill_id {
                return Err(SkillCandidateBuildError::ParentSkillBindingMismatch {
                    skill_id,
                    digest: reference.content.clone(),
                });
            }
            if artifact.status_history.last().map(|entry| entry.status)
                != Some(SkillStatusV1::Active)
            {
                return Err(SkillCandidateBuildError::ParentSkillNotActive(skill_id));
            }
            result.push(SkillGenomeRefV1 {
                skill_id: artifact.skill_id.clone(),
                artifact_digest: reference.content.clone(),
                required_capabilities: artifact.required_capabilities.clone(),
            });
            artifacts.insert(artifact.skill_id.clone(), artifact);
        }
        let normalized = SkillCandidateV1::normalize_skill_set(result)
            .map_err(SkillCandidateBuildError::InvalidCandidate)?;
        Ok((normalized, artifacts))
    }

    async fn persist_candidate_artifacts(
        &self,
        proposal: &SkillMutationProposalV1,
    ) -> Result<Vec<PersistedSkillArtifact>, SkillCandidateBuildError> {
        let mut result = Vec::with_capacity(proposal.proposed_artifacts.len());
        for artifact in &proposal.proposed_artifacts {
            validate_candidate_artifact_state(artifact)?;
            let expected = self.skills.digest(artifact)?;
            let stored = self.skills.put(artifact).await?;
            if stored.digest != expected {
                return Err(SkillCandidateBuildError::CandidateArtifactDigestMismatch {
                    skill_id: artifact.skill_id.clone(),
                    expected,
                    actual: stored.digest,
                });
            }
            result.push(PersistedSkillArtifact {
                artifact: artifact.clone(),
                digest: stored.digest,
            });
        }
        Ok(result)
    }
}

#[derive(Debug, Clone)]
struct PersistedSkillArtifact {
    artifact: SkillArtifactV1,
    digest: ArtifactDigest,
}

fn validate_proposal_against_parent(
    parent: &BTreeMap<SkillId, SkillArtifactV1>,
    proposal: &SkillMutationProposalV1,
) -> Result<(), SkillCandidateBuildError> {
    for artifact in &proposal.proposed_artifacts {
        match &artifact.operation {
            SkillOperationV1::Create => {
                require_new_first_revision(parent, artifact)?;
            }
            SkillOperationV1::Update { .. } => {
                let previous = parent.get(&artifact.skill_id).ok_or_else(|| {
                    SkillCandidateBuildError::SourceSkillNotFound(artifact.skill_id.clone())
                })?;
                require_next_revision(previous, artifact)?;
            }
            SkillOperationV1::Merge { source_artifacts } => {
                for (source_id, source_digest) in source_artifacts {
                    require_parent_artifact(parent, source_id, source_digest)?;
                }
                if let Some(previous) = parent.get(&artifact.skill_id) {
                    if !source_artifacts.contains_key(&artifact.skill_id) {
                        return Err(SkillCandidateBuildError::SkillAlreadyExists(
                            artifact.skill_id.clone(),
                        ));
                    }
                    require_next_revision(previous, artifact)?;
                } else {
                    require_new_first_revision(parent, artifact)?;
                }
            }
            SkillOperationV1::Split {
                source_skill_id,
                source_artifact_digest,
                ..
            } => {
                let previous =
                    require_parent_artifact(parent, source_skill_id, source_artifact_digest)?;
                if artifact.skill_id == *source_skill_id {
                    require_next_revision(previous, artifact)?;
                } else {
                    require_new_first_revision(parent, artifact)?;
                }
            }
            SkillOperationV1::Deprecate {
                previous_artifact_digest,
            }
            | SkillOperationV1::Delete {
                previous_artifact_digest,
                ..
            } => {
                let previous =
                    require_parent_artifact(parent, &artifact.skill_id, previous_artifact_digest)?;
                require_next_revision(previous, artifact)?;
                let content_unchanged = previous.name == artifact.name
                    && previous.description == artifact.description
                    && previous.instructions == artifact.instructions
                    && previous.trigger_policy == artifact.trigger_policy
                    && previous.required_capabilities == artifact.required_capabilities;
                let status_appended = artifact.status_history.len()
                    == previous.status_history.len() + 1
                    && artifact
                        .status_history
                        .starts_with(&previous.status_history);
                if !content_unchanged || !status_appended {
                    return Err(SkillCandidateBuildError::InvalidLifecycleAppend(
                        artifact.skill_id.clone(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn require_new_first_revision(
    parent: &BTreeMap<SkillId, SkillArtifactV1>,
    artifact: &SkillArtifactV1,
) -> Result<(), SkillCandidateBuildError> {
    if parent.contains_key(&artifact.skill_id) {
        return Err(SkillCandidateBuildError::SkillAlreadyExists(
            artifact.skill_id.clone(),
        ));
    }
    if artifact.revision != 1 {
        return Err(SkillCandidateBuildError::InvalidArtifactRevision {
            skill_id: artifact.skill_id.clone(),
            expected: 1,
            actual: artifact.revision,
        });
    }
    Ok(())
}

fn require_next_revision(
    previous: &SkillArtifactV1,
    artifact: &SkillArtifactV1,
) -> Result<(), SkillCandidateBuildError> {
    let expected = previous.revision.checked_add(1).ok_or_else(|| {
        SkillCandidateBuildError::ArtifactRevisionOverflow(previous.skill_id.clone())
    })?;
    if artifact.revision != expected {
        return Err(SkillCandidateBuildError::InvalidArtifactRevision {
            skill_id: artifact.skill_id.clone(),
            expected,
            actual: artifact.revision,
        });
    }
    Ok(())
}

fn require_parent_artifact<'a>(
    parent: &'a BTreeMap<SkillId, SkillArtifactV1>,
    skill_id: &SkillId,
    declared_digest: &ArtifactDigest,
) -> Result<&'a SkillArtifactV1, SkillCandidateBuildError> {
    let artifact = parent
        .get(skill_id)
        .ok_or_else(|| SkillCandidateBuildError::SourceSkillNotFound(skill_id.clone()))?;
    let actual_digest = artifact
        .digest()
        .map_err(SkillCandidateBuildError::InvalidCandidate)?;
    if actual_digest != *declared_digest {
        return Err(SkillCandidateBuildError::SourceArtifactDigestMismatch {
            skill_id: skill_id.clone(),
            declared: declared_digest.clone(),
            actual: actual_digest,
        });
    }
    Ok(artifact)
}

fn validate_candidate_artifact_state(
    artifact: &SkillArtifactV1,
) -> Result<(), SkillCandidateBuildError> {
    match &artifact.operation {
        SkillOperationV1::Deprecate { .. } => {
            if artifact.status_history.last().map(|entry| entry.status)
                != Some(SkillStatusV1::Deprecated)
            {
                return Err(SkillCandidateBuildError::InvalidCandidateArtifactState(
                    artifact.skill_id.clone(),
                ));
            }
        }
        SkillOperationV1::Delete { .. } => {
            if artifact.status_history.last().map(|entry| entry.status)
                != Some(SkillStatusV1::Deleted)
            {
                return Err(SkillCandidateBuildError::InvalidCandidateArtifactState(
                    artifact.skill_id.clone(),
                ));
            }
        }
        _ => {
            if artifact.status_history.len() != 1
                || artifact.status_history[0].status != SkillStatusV1::Quarantined
            {
                return Err(SkillCandidateBuildError::InvalidCandidateArtifactState(
                    artifact.skill_id.clone(),
                ));
            }
        }
    }
    Ok(())
}

fn apply_proposal(
    parent_skill_set: &[SkillGenomeRefV1],
    proposed: &[PersistedSkillArtifact],
    parent_capabilities: &BTreeSet<String>,
) -> Result<Vec<SkillGenomeRefV1>, SkillCandidateBuildError> {
    let mut skills = parent_skill_set
        .iter()
        .cloned()
        .map(|skill| (skill.skill_id.clone(), skill))
        .collect::<BTreeMap<_, _>>();
    if proposed.len() > 1 {
        apply_split(&mut skills, proposed, parent_capabilities)?;
    } else {
        let candidate = proposed
            .first()
            .ok_or(SkillCandidateBuildError::EmptyProposal)?;
        apply_single(&mut skills, candidate, parent_capabilities)?;
    }
    Ok(skills.into_values().collect())
}

fn apply_split(
    skills: &mut BTreeMap<SkillId, SkillGenomeRefV1>,
    proposed: &[PersistedSkillArtifact],
    parent_capabilities: &BTreeSet<String>,
) -> Result<(), SkillCandidateBuildError> {
    let expected_results = proposed
        .iter()
        .map(|candidate| candidate.artifact.skill_id.clone())
        .collect::<BTreeSet<_>>();
    let (source_skill_id, source_digest, declared_results) = match &proposed[0].artifact.operation {
        SkillOperationV1::Split {
            source_skill_id,
            source_artifact_digest,
            result_skill_ids,
        } => (source_skill_id, source_artifact_digest, result_skill_ids),
        _ => return Err(SkillCandidateBuildError::MixedOperationBatch),
    };
    if declared_results != &expected_results {
        return Err(SkillCandidateBuildError::SplitResultSetMismatch);
    }
    for candidate in proposed {
        match &candidate.artifact.operation {
            SkillOperationV1::Split {
                source_skill_id: current_source,
                source_artifact_digest: current_digest,
                result_skill_ids,
            } if current_source == source_skill_id
                && current_digest == source_digest
                && result_skill_ids == declared_results => {}
            _ => return Err(SkillCandidateBuildError::MixedOperationBatch),
        }
        ensure_capability_subset(&candidate.artifact, parent_capabilities)?;
    }
    require_source(skills, source_skill_id, source_digest)?;
    skills.remove(source_skill_id);
    for candidate in proposed {
        if skills.contains_key(&candidate.artifact.skill_id) {
            return Err(SkillCandidateBuildError::SkillAlreadyExists(
                candidate.artifact.skill_id.clone(),
            ));
        }
        insert_candidate(skills, candidate);
    }
    Ok(())
}

fn apply_single(
    skills: &mut BTreeMap<SkillId, SkillGenomeRefV1>,
    candidate: &PersistedSkillArtifact,
    parent_capabilities: &BTreeSet<String>,
) -> Result<(), SkillCandidateBuildError> {
    ensure_capability_subset(&candidate.artifact, parent_capabilities)?;
    match &candidate.artifact.operation {
        SkillOperationV1::Create => {
            if skills.contains_key(&candidate.artifact.skill_id) {
                return Err(SkillCandidateBuildError::SkillAlreadyExists(
                    candidate.artifact.skill_id.clone(),
                ));
            }
            insert_candidate(skills, candidate);
        }
        SkillOperationV1::Update {
            previous_artifact_digest,
        } => {
            require_source(
                skills,
                &candidate.artifact.skill_id,
                previous_artifact_digest,
            )?;
            insert_candidate(skills, candidate);
        }
        SkillOperationV1::Merge { source_artifacts } => {
            for (skill_id, digest) in source_artifacts {
                require_source(skills, skill_id, digest)?;
            }
            let target_was_source = source_artifacts.contains_key(&candidate.artifact.skill_id);
            if skills.contains_key(&candidate.artifact.skill_id) && !target_was_source {
                return Err(SkillCandidateBuildError::SkillAlreadyExists(
                    candidate.artifact.skill_id.clone(),
                ));
            }
            for skill_id in source_artifacts.keys() {
                skills.remove(skill_id);
            }
            insert_candidate(skills, candidate);
        }
        SkillOperationV1::Split { .. } => {
            return Err(SkillCandidateBuildError::IncompleteSplitBatch)
        }
        SkillOperationV1::Deprecate {
            previous_artifact_digest,
        }
        | SkillOperationV1::Delete {
            previous_artifact_digest,
            ..
        } => {
            require_source(
                skills,
                &candidate.artifact.skill_id,
                previous_artifact_digest,
            )?;
            skills.remove(&candidate.artifact.skill_id);
        }
    }
    Ok(())
}

fn require_source(
    skills: &BTreeMap<SkillId, SkillGenomeRefV1>,
    skill_id: &SkillId,
    digest: &ArtifactDigest,
) -> Result<(), SkillCandidateBuildError> {
    match skills.get(skill_id) {
        None => Err(SkillCandidateBuildError::SourceSkillNotFound(
            skill_id.clone(),
        )),
        Some(source) if source.artifact_digest != *digest => {
            Err(SkillCandidateBuildError::SourceArtifactDigestMismatch {
                skill_id: skill_id.clone(),
                declared: digest.clone(),
                actual: source.artifact_digest.clone(),
            })
        }
        Some(_) => Ok(()),
    }
}

fn ensure_capability_subset(
    artifact: &SkillArtifactV1,
    parent_capabilities: &BTreeSet<String>,
) -> Result<(), SkillCandidateBuildError> {
    if !artifact
        .required_capabilities
        .is_subset(parent_capabilities)
    {
        return Err(SkillCandidateBuildError::ArtifactCapabilityExpansion(
            artifact.skill_id.clone(),
        ));
    }
    Ok(())
}

fn insert_candidate(
    skills: &mut BTreeMap<SkillId, SkillGenomeRefV1>,
    candidate: &PersistedSkillArtifact,
) {
    skills.insert(
        candidate.artifact.skill_id.clone(),
        SkillGenomeRefV1 {
            skill_id: candidate.artifact.skill_id.clone(),
            artifact_digest: candidate.digest.clone(),
            required_capabilities: candidate.artifact.required_capabilities.clone(),
        },
    );
}

fn deterministic_revision_id(
    cycle_id: &EvolutionCycleId,
    mutation_id: &MutationId,
    digest: &GenomeDigest,
) -> Result<GenomeRevisionId, SkillCandidateBuildError> {
    let body = deterministic_id_body(&[
        b"skill-genome-revision-v1",
        cycle_id.as_str().as_bytes(),
        mutation_id.as_str().as_bytes(),
        digest.as_str().as_bytes(),
    ]);
    GenomeRevisionId::new(format!("{}_{}", GenomeRevisionId::PREFIX, body))
        .map_err(|error| SkillCandidateBuildError::DeterministicId(error.to_string()))
}

fn deterministic_candidate_id(
    cycle_id: &EvolutionCycleId,
    mutation_id: &MutationId,
    revision_id: &GenomeRevisionId,
) -> Result<CandidateId, SkillCandidateBuildError> {
    let body = deterministic_id_body(&[
        b"skill-candidate-v1",
        cycle_id.as_str().as_bytes(),
        mutation_id.as_str().as_bytes(),
        revision_id.as_str().as_bytes(),
    ]);
    CandidateId::new(format!("{}_{}", CandidateId::PREFIX, body))
        .map_err(|error| SkillCandidateBuildError::DeterministicId(error.to_string()))
}

fn deterministic_id_body(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

/// Skill Candidate 的可信构建错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillCandidateBuildError {
    /// Proposal 未通过 M7 协议校验。
    #[error("Skill MutationProposal 无效：{0}")]
    InvalidProposal(InvalidSkillEvolution),
    /// Parent Genome 不存在。
    #[error("Parent Genome 修订不存在：{0}")]
    ParentNotFound(GenomeRevisionId),
    /// Parent 摘要错绑。
    #[error("Parent Genome 摘要错绑：声明 {declared}，实际 {actual}")]
    ParentDigestMismatch {
        /// Proposal 声明摘要。
        declared: GenomeDigest,
        /// Store 中真实摘要。
        actual: GenomeDigest,
    },
    /// Parent Skill ID 不是合法强类型 ID。
    #[error("Parent Genome Skill ID `{value}` 无效：{reason}")]
    InvalidParentSkillId {
        /// 原始 ID。
        value: String,
        /// 校验原因。
        reason: String,
    },
    /// Parent SkillRef 与 CAS 制品中的 Skill ID 不一致。
    #[error("Parent Skill `{skill_id}` 与 CAS 制品 {digest} 绑定不一致")]
    ParentSkillBindingMismatch {
        /// Genome 声明的 Skill ID。
        skill_id: SkillId,
        /// Genome 声明的制品摘要。
        digest: ArtifactDigest,
    },
    /// Parent Genome 引用了尚未通过 Gate 的 Skill。
    #[error("Parent Genome Skill `{0}` 的状态不是 Active")]
    ParentSkillNotActive(SkillId),
    /// Candidate 制品初始状态不符合其操作语义。
    #[error("Candidate Skill `{0}` 的状态链不符合 Create/Update/Merge/Split 或删除语义")]
    InvalidCandidateArtifactState(SkillId),
    /// Proposal 没有制品。
    #[error("Skill Proposal 没有候选制品")]
    EmptyProposal,
    /// 多制品批次不是同一个 Split。
    #[error("Skill Proposal 的多制品批次必须全部属于同一个 Split")]
    MixedOperationBatch,
    /// Split 声明的结果集合与实际制品不一致。
    #[error("Skill Split 的 result_skill_ids 与 Proposal 实际制品集合不一致")]
    SplitResultSetMismatch,
    /// 单制品 Split 缺少其余结果。
    #[error("Skill Split 必须在同一 Proposal 中携带全部结果制品")]
    IncompleteSplitBatch,
    /// Create 或结果 Skill 已存在。
    #[error("Skill 已存在，不能再次创建：{0}")]
    SkillAlreadyExists(SkillId),
    /// 操作引用的来源 Skill 不存在。
    #[error("Skill 操作引用的来源不存在：{0}")]
    SourceSkillNotFound(SkillId),
    /// 操作引用的来源摘要与 Parent CAS 不一致。
    #[error("Skill `{skill_id}` 来源摘要错绑：声明 {declared}，实际 {actual}")]
    SourceArtifactDigestMismatch {
        /// 来源 Skill。
        skill_id: SkillId,
        /// 操作声明摘要。
        declared: ArtifactDigest,
        /// Parent Genome 实际摘要。
        actual: ArtifactDigest,
    },
    /// 新制品修订号不是来源修订的唯一后继。
    #[error("Skill `{skill_id}` 修订号错误：期望 {expected}，实际 {actual}")]
    InvalidArtifactRevision {
        /// 出错 Skill。
        skill_id: SkillId,
        /// 唯一合法后继修订号。
        expected: u32,
        /// 提案声明修订号。
        actual: u32,
    },
    /// Parent Skill 修订号已到上限。
    #[error("Skill `{0}` 修订号已溢出，不能继续更新")]
    ArtifactRevisionOverflow(SkillId),
    /// Deprecate/Delete 改写了正文或状态前缀。
    #[error("Skill `{0}` 的 Deprecate/Delete 必须只追加一个状态且保留既有正文")]
    InvalidLifecycleAppend(SkillId),
    /// 单个制品声明超出 Parent 的能力。
    #[error("Skill `{0}` 声明了 Parent 未提供的能力")]
    ArtifactCapabilityExpansion(SkillId),
    /// 聚合后的 Candidate 能力超出 Parent。
    #[error("Candidate Skill 能力集合不是 Parent 能力集合的子集")]
    CapabilityExpansion,
    /// Skill Set 没有变化。
    #[error("Skill Proposal 未改变 Genome Skill Set")]
    UnchangedSkillSet,
    /// CAS 返回摘要与可信预计算不一致。
    #[error("Candidate Skill `{skill_id}` 摘要不一致：期望 {expected}，实际 {actual}")]
    CandidateArtifactDigestMismatch {
        /// Candidate Skill。
        skill_id: SkillId,
        /// 写入前摘要。
        expected: ArtifactDigest,
        /// CAS 返回摘要。
        actual: ArtifactDigest,
    },
    /// Skill CAS 访问或规范性复核失败。
    #[error("Skill CAS 校验失败：{0}")]
    SkillRepository(#[from] SkillRepositoryError),
    /// Candidate Genome 无法构造。
    #[error("Candidate Genome 修订无效：{0}")]
    GenomeRevision(#[from] GenomeRevisionError),
    /// 完整 Genome Diff 失败。
    #[error("Candidate Genome 差异无效：{0}")]
    GenomeDiff(#[from] GenomeDiffError),
    /// 真实 Diff 不是唯一 Skill。
    #[error("Candidate Genome 差异不是唯一 Skill：{changed_surfaces:?}")]
    UnexpectedDiff {
        /// 可信 Diff 的实际表面。
        changed_surfaces: BTreeSet<MutationSurface>,
    },
    /// Candidate DTO 无效。
    #[error("SkillCandidate 无效：{0}")]
    InvalidCandidate(InvalidSkillEvolution),
    /// 确定性 ID 构造失败。
    #[error("构造 Skill Candidate 稳定身份失败：{0}")]
    DeterministicId(String),
    /// 幂等修订声明存在但复读缺失。
    #[error("幂等 Skill Candidate Revision 复读缺失：{0}")]
    MissingIdempotentRevision(GenomeRevisionId),
    /// 确定性 ID 被不同内容占用。
    #[error("幂等 Skill Candidate Revision 内容冲突：{0}")]
    IdempotentRevisionConflict(GenomeRevisionId),
    /// 创建时间为零。
    #[error("Skill Candidate 创建时间不能为零")]
    InvalidTimestamp,
    /// 系统时钟早于 Unix Epoch。
    #[error("无法生成 Skill Candidate 时间戳：{0}")]
    Clock(#[from] SystemTimeError),
    /// 时间戳超过 u64 毫秒范围。
    #[error("Skill Candidate 时间戳超过 u64 毫秒范围")]
    TimestampOverflow,
    /// Genome Store 访问失败。
    #[error("访问 Genome Store 失败：{0}")]
    GenomeStore(#[from] GenomeStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        AgentGenome, EpisodeId, ModelGenome, PluginGenome, PromptGenome, RuntimeIdentity,
        SkillStatusTransitionV1, SkillTriggerPolicyV1, ToolProfileGenome, GENOME_SCHEMA_VERSION,
        SKILL_ARTIFACT_SCHEMA_VERSION, SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn roots() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "lucia-skill-candidate-builder-{}",
            Uuid::new_v4().simple()
        ));
        (base.join("genomes"), base.join("artifacts"))
    }

    fn digest(character: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
    }

    fn parent_revision() -> GenomeRevision {
        GenomeRevision::create(
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
                    config_digest: None,
                }],
                capability_owners: BTreeMap::from([(
                    "episode.read_redacted".into(),
                    "agent.skill-loader".into(),
                )]),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("Parent Genome 应合法")
    }

    fn proposal(parent: &GenomeRevision, capability: &str) -> SkillMutationProposalV1 {
        let mutation_id = MutationId::generate();
        let episode_id = EpisodeId::generate();
        SkillMutationProposalV1 {
            schema_version: SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
            mutation_id: mutation_id.clone(),
            parent_revision_id: parent.revision_id.clone(),
            parent_genome_digest: parent.digest.clone(),
            evidence_episode_ids: BTreeSet::from([episode_id.clone()]),
            proposed_artifacts: vec![SkillArtifactV1 {
                schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
                skill_id: SkillId::new("skill_candidate").expect("测试 ID 应合法"),
                revision: 1,
                operation: SkillOperationV1::Create,
                name: "候选 Skill".into(),
                description: "验证可信 Candidate Builder".into(),
                instructions: "只使用 Parent 已有能力。".into(),
                trigger_policy: SkillTriggerPolicyV1::default(),
                required_capabilities: BTreeSet::from([capability.into()]),
                source_episode_ids: BTreeSet::from([episode_id]),
                mutation_id,
                status_history: vec![SkillStatusTransitionV1 {
                    status: SkillStatusV1::Quarantined,
                    recorded_at_ms: 1,
                    evaluation_report_id: None,
                }],
            }],
            hypothesis: "候选 Skill 改善可信证据归因".into(),
        }
    }

    #[tokio::test]
    async fn builds_only_skill_diff_and_retries_idempotently() {
        let (genome_root, artifact_root) = roots();
        let genomes = FileGenomeStore::new(&genome_root);
        let artifacts = FileArtifactStore::new(&artifact_root);
        let parent = parent_revision();
        genomes.append(&parent).await.expect("应登记 Parent");
        let proposal = proposal(&parent, "episode.read_redacted");
        let builder = SkillCandidateBuilder::new(&genomes, &artifacts);
        let cycle_id = EvolutionCycleId::generate();
        let first = builder
            .build_at(cycle_id.clone(), &proposal, 10)
            .await
            .expect("应构建 Skill Candidate");
        let second = builder
            .build_at(cycle_id, &proposal, 10)
            .await
            .expect("相同输入重试应幂等");
        assert_eq!(first, second);
        assert_eq!(
            first.changed_surfaces,
            BTreeSet::from([MutationSurface::Skill])
        );
        assert!(first
            .candidate_capabilities
            .is_subset(&first.parent_capabilities));
        let _ = tokio::fs::remove_dir_all(genome_root.parent().expect("测试根目录应存在")).await;
    }

    #[tokio::test]
    async fn rejects_capability_outside_parent() {
        let (genome_root, artifact_root) = roots();
        let genomes = FileGenomeStore::new(&genome_root);
        let artifacts = FileArtifactStore::new(&artifact_root);
        let parent = parent_revision();
        genomes.append(&parent).await.expect("应登记 Parent");
        let proposal = proposal(&parent, "process_exec");
        let error = SkillCandidateBuilder::new(&genomes, &artifacts)
            .build_at(EvolutionCycleId::generate(), &proposal, 10)
            .await
            .expect_err("越界能力必须被拒绝");
        assert!(matches!(
            error,
            SkillCandidateBuildError::ArtifactCapabilityExpansion(_)
        ));
        let _ = tokio::fs::remove_dir_all(genome_root.parent().expect("测试根目录应存在")).await;
    }
}
