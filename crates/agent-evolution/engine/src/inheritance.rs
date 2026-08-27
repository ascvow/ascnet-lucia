//! Promotion 后的可信 Genome 继承验证。

use crate::{GenomeResolver, GenomeResolverError, GenomeSelector};
use agent_evolution_protocol::{GenomeDigest, GenomeRevisionId, InheritanceVerification, RunId};
use serde::{Deserialize, Serialize};

/// 一条继承观察所属的运行边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InheritanceObservationKind {
    /// 全新进程启动后解析 Stable Genome 的观察。
    Restart,
    /// Promotion 后创建的新 Session 所绑定的 Genome。
    NewSession,
    /// Promotion 前已经存在的 Session 继续运行时绑定的 Genome。
    ExistingSession,
}

/// 由可信运行路径记录的一条 Genome 绑定观察。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritanceObservation {
    /// 观察发生的运行边界。
    pub kind: InheritanceObservationKind,
    /// 运行或 Session 实际绑定的不可变 Genome 修订。
    pub observed_genome: GenomeRevisionId,
    /// 产生该观察的真实 Run；旧 Session 仅做绑定检查时可以缺失。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
}

/// 可信继承验证器的完整输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritanceVerificationInput {
    /// Promotion 更新的 Stable lineage。
    pub lineage: String,
    /// Promotion 的 Parent Genome；旧 Session 必须继续绑定该修订。
    pub parent_genome: GenomeRevisionId,
    /// Promotion 的 Candidate Genome；重启和新 Session 必须绑定该修订。
    pub expected_genome: GenomeRevisionId,
    /// Candidate 在不可变 Registry 中的预期行为摘要。
    pub expected_digest: GenomeDigest,
    /// 来自真实进程、Run 与 Session 绑定的观察集合。
    pub observations: Vec<InheritanceObservation>,
}

/// 读取 Stable Registry 并汇总重启、新 Session 与旧 Session 的继承结论。
///
/// Stable 引用由 [`GenomeResolver`] 重新校验，不能由调用方自行声明通过。观察集合可以不完整，
/// 此时返回 `verified = false` 的结构化结果，而不会把缺失数据视为成功。
///
/// # Errors
///
/// Parent 与 Candidate 相同，或 Stable 引用不存在、损坏、摘要不匹配时返回错误。
pub async fn verify_inheritance(
    resolver: &dyn GenomeResolver,
    input: &InheritanceVerificationInput,
) -> Result<InheritanceVerification, InheritanceVerificationError> {
    if input.parent_genome == input.expected_genome {
        return Err(InheritanceVerificationError::SameRevision);
    }

    let stable = resolver
        .resolve(&GenomeSelector::Stable(input.lineage.clone()))
        .await?;
    let stable_reference_verified = stable.revision_id == input.expected_genome;
    let genome_digest_verified = stable.digest == input.expected_digest;

    let restart: Vec<_> = input
        .observations
        .iter()
        .filter(|observation| observation.kind == InheritanceObservationKind::Restart)
        .collect();
    let new_session: Vec<_> = input
        .observations
        .iter()
        .filter(|observation| observation.kind == InheritanceObservationKind::NewSession)
        .collect();
    let existing_session: Vec<_> = input
        .observations
        .iter()
        .filter(|observation| observation.kind == InheritanceObservationKind::ExistingSession)
        .collect();

    let restart_cases_passed = matching_observations(&restart, &input.expected_genome);
    let new_session_cases_passed = matching_observations(&new_session, &input.expected_genome);
    let old_session_parent_preserved = (!existing_session.is_empty()).then(|| {
        existing_session
            .iter()
            .all(|observation| observation.observed_genome == input.parent_genome)
    });
    let observed_genome_after_restart = restart
        .first()
        .map(|observation| observation.observed_genome.clone());
    let verified = !restart.is_empty()
        && restart_cases_passed == restart.len() as u32
        && !new_session.is_empty()
        && new_session_cases_passed == new_session.len() as u32
        && old_session_parent_preserved == Some(true)
        && stable_reference_verified
        && genome_digest_verified;

    Ok(InheritanceVerification {
        expected_genome: input.expected_genome.clone(),
        observed_genome_after_restart,
        restart_cases_passed,
        restart_cases_total: restart.len() as u32,
        new_session_cases_passed,
        new_session_cases_total: new_session.len() as u32,
        old_session_parent_preserved,
        stable_reference_verified,
        genome_digest_verified,
        verified,
    })
}

/// 统计观察集合中绑定预期修订的条目数。
fn matching_observations(
    observations: &[&InheritanceObservation],
    expected: &GenomeRevisionId,
) -> u32 {
    observations
        .iter()
        .filter(|observation| &observation.observed_genome == expected)
        .count() as u32
}

/// 可信继承验证失败。
#[derive(Debug, thiserror::Error)]
pub enum InheritanceVerificationError {
    /// Parent 与 Candidate 相同，无法证明发生 Promotion。
    #[error("继承验证的 Parent 与 Candidate 不能是同一 Genome 修订")]
    SameRevision,
    /// Stable Registry 读取或完整性校验失败。
    #[error("读取 Stable Genome 失败：{0}")]
    Resolver(#[from] GenomeResolverError),
}
