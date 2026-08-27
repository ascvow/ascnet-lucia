//! M7 Skill 的独立 Commit Policy 与可信 Gate。
//!
//! Gate 只接受真实 Genome Revision、可信 Builder 产出的 [`SkillCandidateV1`]，以及由
//! Recorder/Core 绑定到真实原生工具事件的使用观察。无论 Candidate 还是 Skill 自行声明
//! 成功，都不能绕过真实 Diff、能力子集与可信事件复核。

use agent_evolution::{diff_genomes, GenomeDiffError};
use agent_evolution_protocol::{
    EvaluationReportId, EventId, GateDecision, GenomeRevision, MutationSurface, SkillCandidateV1,
    SkillEvaluationReportV1, SkillGateFailureV1, SkillGenomeRefV1, SkillUsageObservationV1,
    SkillUsageResultV1, TrustedSkillUsageBindingV1, SKILL_EVALUATION_REPORT_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet};

/// 固定 M7 Skill Commit Policy 的语义版本。
pub const M7_SKILL_COMMIT_POLICY_VERSION: &str = "skill-commit-m7-v1";

/// M7 Skill Commit Gate 的不可变策略快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillCommitPolicyV1 {
    /// 策略语义版本；任何阈值或分类变化都必须更换。
    pub version: &'static str,
    /// Gate Pass 所需的最少可信原生 Skill 工具事件观察数。
    pub min_trusted_observations: usize,
    /// 是否把任一可信任务失败设为硬失败。
    pub reject_verified_failure: bool,
    /// 是否把误触发或漏触发设为硬失败。
    pub reject_trigger_regression: bool,
    /// 是否把任一可信安全失败设为硬失败。
    pub reject_safety_failure: bool,
}

impl SkillCommitPolicyV1 {
    /// 返回当前 M7 固定策略。
    pub const fn m7() -> Self {
        Self {
            version: M7_SKILL_COMMIT_POLICY_VERSION,
            min_trusted_observations: 1,
            reject_verified_failure: true,
            reject_trigger_regression: true,
            reject_safety_failure: true,
        }
    }
}

impl Default for SkillCommitPolicyV1 {
    fn default() -> Self {
        Self::m7()
    }
}

/// 独立 Skill Gate 的可信输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedSkillGateResultV1 {
    /// Gate 使用的固定策略版本。
    pub policy_version: String,
    /// 可归档并绑定 Candidate 的正式 Skill 评测报告。
    pub report: SkillEvaluationReportV1,
}

/// 使用固定 M7 Policy 评测 Skill Candidate。
///
/// # Errors
///
/// Parent/Candidate Revision 无效、报告身份时间无效，或最终可信报告无法通过协议校验时
/// 返回 [`SkillGateError`]。合法但存在自报成功、错绑观察、能力扩大或非 Skill Diff 的
/// Candidate 返回 `Ok(Reject)`，不会把不可信输入升级为基础设施错误。
pub fn evaluate_skill_candidate(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
    trusted_candidate: &SkillCandidateV1,
    observations: &[SkillUsageObservationV1],
    trusted_usage_bindings: &BTreeMap<EventId, TrustedSkillUsageBindingV1>,
    report_id: EvaluationReportId,
    generated_at_ms: u64,
) -> Result<TrustedSkillGateResultV1, SkillGateError> {
    evaluate_skill_candidate_with_policy(
        parent,
        candidate,
        trusted_candidate,
        observations,
        trusted_usage_bindings,
        report_id,
        generated_at_ms,
        SkillCommitPolicyV1::m7(),
    )
}

/// 使用显式不可变 Policy 运行独立 Skill Commit Gate。
///
/// 该入口用于版本化回放与测试。生产调用方应使用 [`evaluate_skill_candidate`] 固定到当前
/// M7 策略，不能接受模型或插件传入的 Policy。
///
/// # Errors
///
/// Policy 无效、Genome Diff 无法可信计算，或最终报告结构无效时返回
/// [`SkillGateError`]。
#[allow(clippy::too_many_arguments)]
pub fn evaluate_skill_candidate_with_policy(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
    trusted_candidate: &SkillCandidateV1,
    observations: &[SkillUsageObservationV1],
    trusted_usage_bindings: &BTreeMap<EventId, TrustedSkillUsageBindingV1>,
    report_id: EvaluationReportId,
    generated_at_ms: u64,
    policy: SkillCommitPolicyV1,
) -> Result<TrustedSkillGateResultV1, SkillGateError> {
    validate_policy(policy)?;
    if generated_at_ms == 0 {
        return Err(SkillGateError::InvalidGeneratedAt);
    }
    let diff = diff_genomes(parent, candidate)?;
    let mut failures = BTreeSet::new();
    if let Err(error) = trusted_candidate.validate() {
        match error {
            agent_evolution_protocol::InvalidSkillEvolution::CapabilityExpansion
            | agent_evolution_protocol::InvalidSkillEvolution::UnavailableSkillCapability {
                ..
            } => {
                failures.insert(SkillGateFailureV1::CapabilityExpansion);
            }
            agent_evolution_protocol::InvalidSkillEvolution::InvalidCandidateSurfaces(_) => {
                failures.insert(SkillGateFailureV1::GenomeDiff);
            }
            _ => {
                failures.insert(SkillGateFailureV1::Integrity);
            }
        }
    }

    let actual_parent_capabilities = parent
        .genome
        .capability_owners
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let actual_candidate_capabilities = candidate
        .genome
        .capability_owners
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if !actual_candidate_capabilities.is_subset(&actual_parent_capabilities)
        || !trusted_candidate
            .candidate_capabilities
            .is_subset(&actual_parent_capabilities)
    {
        failures.insert(SkillGateFailureV1::CapabilityExpansion);
    }
    if diff.changed_surfaces != BTreeSet::from([MutationSurface::Skill])
        || trusted_candidate.changed_surfaces != BTreeSet::from([MutationSurface::Skill])
    {
        failures.insert(SkillGateFailureV1::GenomeDiff);
    }
    if !candidate_binding_matches(
        parent,
        candidate,
        trusted_candidate,
        &actual_parent_capabilities,
    ) {
        failures.insert(SkillGateFailureV1::Integrity);
    }

    let actual_candidate_skills = candidate
        .genome
        .skills
        .iter()
        .map(|skill| (skill.id.as_str(), &skill.content))
        .collect::<BTreeMap<_, _>>();
    let mut trusted_observations = Vec::new();
    for observation in observations {
        let event_id = &observation.binding.tool_event.event_id;
        let valid_binding = trusted_usage_bindings
            .get(event_id)
            .is_some_and(|binding| observation.validate(binding).is_ok());
        let active_skill_matches = actual_candidate_skills
            .get(observation.binding.skill_id.as_str())
            .is_some_and(|digest| *digest == &observation.binding.skill_artifact_digest);
        let uses_candidate_genome = observation.binding.genome_revision_id == candidate.revision_id;
        if !valid_binding || !active_skill_matches || !uses_candidate_genome {
            failures.insert(SkillGateFailureV1::Integrity);
            continue;
        }
        match observation.result {
            SkillUsageResultV1::VerifiedFailure if policy.reject_verified_failure => {
                failures.insert(SkillGateFailureV1::VerifiedFailure);
            }
            SkillUsageResultV1::IncorrectTrigger | SkillUsageResultV1::MissedTrigger
                if policy.reject_trigger_regression =>
            {
                failures.insert(SkillGateFailureV1::TriggerRegression);
            }
            _ => {}
        }
        if policy.reject_safety_failure && observation.safety_failures != 0 {
            failures.insert(SkillGateFailureV1::SafetyFailure);
        }
        trusted_observations.push(observation.clone());
    }
    trusted_observations.sort_by(|left, right| {
        left.binding
            .tool_event
            .event_id
            .cmp(&right.binding.tool_event.event_id)
    });
    let before_dedup = trusted_observations.len();
    trusted_observations.dedup_by(|left, right| {
        left.binding.tool_event.event_id == right.binding.tool_event.event_id
    });
    if trusted_observations.len() != before_dedup {
        failures.insert(SkillGateFailureV1::Integrity);
    }
    if trusted_observations.len() < policy.min_trusted_observations {
        failures.insert(SkillGateFailureV1::NoTrustedUsage);
    }
    let observed_skill_ids = trusted_observations
        .iter()
        .map(|observation| observation.binding.skill_id.clone())
        .collect::<BTreeSet<_>>();
    if !trusted_candidate
        .candidate_artifact_digests
        .keys()
        .all(|skill_id| observed_skill_ids.contains(skill_id))
    {
        failures.insert(SkillGateFailureV1::NoTrustedUsage);
    }

    let decision = if failures.is_empty() {
        GateDecision::Pass
    } else {
        GateDecision::Reject
    };
    let report = SkillEvaluationReportV1 {
        schema_version: SKILL_EVALUATION_REPORT_SCHEMA_VERSION,
        report_id,
        mutation_id: trusted_candidate.mutation_id.clone(),
        candidate_id: trusted_candidate.candidate_id.clone(),
        parent_revision_id: parent.revision_id.clone(),
        candidate_revision_id: candidate.revision_id.clone(),
        parent_genome_digest: parent.digest.clone(),
        candidate_genome_digest: candidate.digest.clone(),
        evaluated_skill_ids: trusted_candidate
            .candidate_artifact_digests
            .keys()
            .cloned()
            .collect(),
        observations: trusted_observations,
        decision,
        failures,
        generated_at_ms,
    };
    report
        .validate(trusted_usage_bindings)
        .map_err(|error| SkillGateError::InvalidReport(error.to_string()))?;
    Ok(TrustedSkillGateResultV1 {
        policy_version: policy.version.to_string(),
        report,
    })
}

fn validate_policy(policy: SkillCommitPolicyV1) -> Result<(), SkillGateError> {
    if policy.version.trim().is_empty() || policy.min_trusted_observations == 0 {
        return Err(SkillGateError::InvalidPolicy);
    }
    Ok(())
}

fn candidate_binding_matches(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
    trusted: &SkillCandidateV1,
    actual_parent_capabilities: &BTreeSet<String>,
) -> bool {
    let claimed_required_capabilities = trusted
        .candidate_skill_set
        .iter()
        .flat_map(|skill| skill.required_capabilities.iter().cloned())
        .collect::<BTreeSet<_>>();
    trusted.parent_revision_id == parent.revision_id
        && trusted.candidate_revision_id == candidate.revision_id
        && trusted.parent_genome_digest == parent.digest
        && trusted.candidate_genome_digest == candidate.digest
        && trusted.parent_capabilities == *actual_parent_capabilities
        && trusted.candidate_capabilities == claimed_required_capabilities
        && genome_skill_set_matches(&parent.genome.skills, &trusted.parent_skill_set)
        && genome_skill_set_matches(&candidate.genome.skills, &trusted.candidate_skill_set)
}

fn genome_skill_set_matches(
    actual: &[agent_evolution_protocol::SkillRef],
    claimed: &[SkillGenomeRefV1],
) -> bool {
    actual.len() == claimed.len()
        && actual.iter().zip(claimed).all(|(actual, claimed)| {
            actual.id == claimed.skill_id.as_str() && actual.content == claimed.artifact_digest
        })
}

/// Skill Commit Gate 构建错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillGateError {
    /// Policy 版本或最低可信观察数无效。
    #[error("Skill Commit Policy 无效")]
    InvalidPolicy,
    /// 报告生成时间不能为零。
    #[error("Skill EvaluationReport 生成时间不能为零")]
    InvalidGeneratedAt,
    /// Parent/Candidate Revision 无法产生可信完整 Diff。
    #[error("Skill Genome Diff 无效：{0}")]
    GenomeDiff(#[from] GenomeDiffError),
    /// 最终报告违反 M7 协议。
    #[error("Skill EvaluationReport 无效：{0}")]
    InvalidReport(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        AgentGenome, ArtifactDigest, CandidateId, EvolutionCycleId, ModelGenome, MutationId,
        OutcomeRevisionId, PluginGenome, PromptGenome, RuntimeIdentity, SkillId, SkillRef,
        SkillUsageEvidenceSourceV1, ToolProfileGenome, TrustedSkillToolEventRefV1,
        GENOME_SCHEMA_VERSION, SKILL_CANDIDATE_SCHEMA_VERSION,
        SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;

    fn digest(character: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
    }

    fn genome(skills: Vec<SkillRef>) -> AgentGenome {
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
                bundle: digest('1'),
                config_digest: None,
            }],
            capability_owners: BTreeMap::from([(
                "episode.read_redacted".into(),
                "agent.skill-loader".into(),
            )]),
            tools: ToolProfileGenome::default(),
            context_policy: None,
            planning_policy: None,
            skills,
            execution: ExecutionPolicy::serve(),
        }
    }

    fn fixture() -> (
        GenomeRevision,
        GenomeRevision,
        SkillCandidateV1,
        TrustedSkillUsageBindingV1,
        SkillUsageObservationV1,
    ) {
        let skill_id = SkillId::new("skill_gatepass").expect("测试 ID 应合法");
        let artifact_digest = digest('2');
        let parent =
            GenomeRevision::create(genome(Vec::new()), Default::default()).expect("Parent 应合法");
        let candidate = GenomeRevision::create(
            genome(vec![SkillRef {
                id: skill_id.to_string(),
                content: artifact_digest.clone(),
            }]),
            Default::default(),
        )
        .expect("Candidate 应合法");
        let skill_ref = SkillGenomeRefV1 {
            skill_id: skill_id.clone(),
            artifact_digest: artifact_digest.clone(),
            required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
        };
        let trusted_candidate = SkillCandidateV1 {
            schema_version: SKILL_CANDIDATE_SCHEMA_VERSION,
            candidate_id: CandidateId::generate(),
            cycle_id: EvolutionCycleId::generate(),
            mutation_id: MutationId::generate(),
            parent_revision_id: parent.revision_id.clone(),
            candidate_revision_id: candidate.revision_id.clone(),
            parent_genome_digest: parent.digest.clone(),
            candidate_genome_digest: candidate.digest.clone(),
            parent_skill_set: Vec::new(),
            candidate_skill_set: vec![skill_ref],
            parent_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            candidate_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            candidate_artifact_digests: BTreeMap::from([(
                skill_id.clone(),
                artifact_digest.clone(),
            )]),
            changed_surfaces: BTreeSet::from([MutationSurface::Skill]),
            evaluation_report_id: None,
            created_at_ms: 1,
        };
        let binding = TrustedSkillUsageBindingV1 {
            episode_id: agent_evolution_protocol::EpisodeId::generate(),
            run_id: agent_evolution_protocol::RunId::generate(),
            genome_revision_id: candidate.revision_id.clone(),
            skill_id,
            skill_artifact_digest: artifact_digest,
            tool_event: TrustedSkillToolEventRefV1 {
                event_id: EventId::generate(),
                sequence: 3,
                runtime_origin: "native".into(),
                tool_name: "skill_read".into(),
                payload_digest: digest('3'),
            },
        };
        let observation = SkillUsageObservationV1 {
            schema_version: SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
            binding: binding.clone(),
            outcome_revision_id: OutcomeRevisionId::generate(),
            evidence_source: SkillUsageEvidenceSourceV1::TrustedEpisodeOutcome,
            result: SkillUsageResultV1::VerifiedSuccess,
            verifier_passed: Some(true),
            safety_failures: 0,
            observed_at_ms: 10,
        };
        (parent, candidate, trusted_candidate, binding, observation)
    }

    #[test]
    fn passes_only_with_real_skill_diff_and_trusted_usage() {
        let (parent, candidate, trusted_candidate, binding, observation) = fixture();
        let bindings = BTreeMap::from([(binding.tool_event.event_id.clone(), binding)]);
        let result = evaluate_skill_candidate(
            &parent,
            &candidate,
            &trusted_candidate,
            &[observation],
            &bindings,
            EvaluationReportId::generate(),
            20,
        )
        .expect("可信 Skill Candidate 应生成 Gate 结果");
        assert_eq!(result.policy_version, M7_SKILL_COMMIT_POLICY_VERSION);
        assert_eq!(result.report.decision, GateDecision::Pass);
        assert!(result.report.failures.is_empty());
    }

    #[test]
    fn rejects_skill_self_reported_success() {
        let (parent, candidate, trusted_candidate, binding, mut observation) = fixture();
        observation.evidence_source = SkillUsageEvidenceSourceV1::SkillSelfReported;
        let bindings = BTreeMap::from([(binding.tool_event.event_id.clone(), binding)]);
        let result = evaluate_skill_candidate(
            &parent,
            &candidate,
            &trusted_candidate,
            &[observation],
            &bindings,
            EvaluationReportId::generate(),
            20,
        )
        .expect("自报成功应成为 Reject 而不是构建错误");
        assert_eq!(result.report.decision, GateDecision::Reject);
        assert!(result
            .report
            .failures
            .contains(&SkillGateFailureV1::Integrity));
        assert!(result
            .report
            .failures
            .contains(&SkillGateFailureV1::NoTrustedUsage));
    }

    #[test]
    fn rejects_capability_expansion_and_non_skill_diff() {
        let (parent, mut candidate, mut trusted_candidate, binding, observation) = fixture();
        candidate
            .genome
            .capability_owners
            .insert("process_exec".into(), "agent.skill-loader".into());
        candidate = GenomeRevision::create(candidate.genome, Default::default())
            .expect("扩权 Candidate 结构仍合法");
        trusted_candidate.candidate_revision_id = candidate.revision_id.clone();
        trusted_candidate.candidate_genome_digest = candidate.digest.clone();
        trusted_candidate
            .candidate_capabilities
            .insert("process_exec".into());
        let bindings = BTreeMap::from([(binding.tool_event.event_id.clone(), binding)]);
        let result = evaluate_skill_candidate(
            &parent,
            &candidate,
            &trusted_candidate,
            &[observation],
            &bindings,
            EvaluationReportId::generate(),
            20,
        )
        .expect("能力扩大应成为 Reject");
        assert_eq!(result.report.decision, GateDecision::Reject);
        assert!(result
            .report
            .failures
            .contains(&SkillGateFailureV1::CapabilityExpansion));
        assert!(result
            .report
            .failures
            .contains(&SkillGateFailureV1::GenomeDiff));
    }
}
