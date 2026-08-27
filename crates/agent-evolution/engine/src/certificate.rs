//! Promotion 的不可变 Evolution Certificate。

use crate::{ArtifactStore, ArtifactStoreError, EvolutionScorecard, InheritanceMetrics};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, DatasetVersionId, EpisodeId, EvaluationReportId, EvolutionIssueId,
    EvolutionLifecycle, GateDecision, GenomeDiff, GenomeRevisionId, MutationId, ReleaseId, RunId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// 当前 EvolutionCertificate 结构版本。
pub const EVOLUTION_CERTIFICATE_SCHEMA_VERSION: u32 = 1;

/// 一次成功 Promotion 的不可变证明包。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionCertificate {
    /// Certificate 结构版本。
    pub schema_version: u32,
    /// Parent Genome 修订。
    pub parent_revision: GenomeRevisionId,
    /// 晋升后的 Child Genome 修订。
    pub child_revision: GenomeRevisionId,
    /// 触发本轮 Evolution 的源 Episode。
    pub source_episode_ids: Vec<EpisodeId>,
    /// 被解决的 EvolutionIssue。
    pub evolution_issue_id: EvolutionIssueId,
    /// 生成 Candidate 的 Mutation Proposal。
    pub mutation_id: MutationId,
    /// 可信控制面确认的允许差异。
    pub allowed_diff: GenomeDiff,
    /// Candidate 产生的不可变制品。
    pub candidate_artifacts: Vec<ArtifactRef>,
    /// Repair Dataset 版本。
    pub repair_dataset: DatasetVersionId,
    /// Regression Dataset 版本。
    pub regression_dataset: DatasetVersionId,
    /// Hidden Dataset 版本。
    pub hidden_dataset: DatasetVersionId,
    /// Safety Dataset 版本。
    pub safety_dataset: DatasetVersionId,
    /// 本次 Promotion 修复并进入后续 Regression 的 TaskCase ID。
    pub repaired_task_case_ids: Vec<String>,
    /// 源 EvaluationReport。
    pub evaluation_report: EvaluationReportId,
    /// 派生 Scorecard 的 CAS 引用。
    pub scorecard: ArtifactRef,
    /// 可信 Commit Gate 决策。
    pub gate_decision: GateDecision,
    /// Release 记录。
    pub release_record: ReleaseId,
    /// Promotion 后的继承验证 CAS 引用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inheritance_verification: Option<ArtifactRef>,
    /// 证明重启或新 Session 实际加载 Child Genome 的 Run ID。
    #[serde(default)]
    pub post_promotion_run_ids: Vec<RunId>,
    /// 当前生命周期；Rollback 不删除 Certificate，只把其归档视图更新为 RolledBack。
    pub lifecycle: EvolutionLifecycle,
    /// 不包含自身字段时的 SHA-256 摘要。
    pub certificate_digest: ArtifactDigest,
}

/// 创建 Certificate 所需的可信 Promotion 输入。
#[derive(Debug, Clone, PartialEq)]
pub struct EvolutionCertificateInput {
    /// Parent Genome 修订。
    pub parent_revision: GenomeRevisionId,
    /// Child Genome 修订。
    pub child_revision: GenomeRevisionId,
    /// 源 Episode。
    pub source_episode_ids: Vec<EpisodeId>,
    /// EvolutionIssue。
    pub evolution_issue_id: EvolutionIssueId,
    /// Mutation Proposal。
    pub mutation_id: MutationId,
    /// 已审核 Genome Diff。
    pub allowed_diff: GenomeDiff,
    /// Candidate 制品。
    pub candidate_artifacts: Vec<ArtifactRef>,
    /// Repair Dataset 版本。
    pub repair_dataset: DatasetVersionId,
    /// Regression Dataset 版本。
    pub regression_dataset: DatasetVersionId,
    /// Hidden Dataset 版本。
    pub hidden_dataset: DatasetVersionId,
    /// Safety Dataset 版本。
    pub safety_dataset: DatasetVersionId,
    /// 已修复 TaskCase ID。
    pub repaired_task_case_ids: Vec<String>,
    /// Scorecard CAS 引用。
    pub scorecard: ArtifactRef,
    /// Release 记录。
    pub release_record: ReleaseId,
    /// 继承验证 CAS 引用。
    pub inheritance_verification: ArtifactRef,
    /// Promotion 后 Run ID。
    pub post_promotion_run_ids: Vec<RunId>,
}

impl EvolutionCertificate {
    /// 从已达到 EVOLVED 的 Scorecard 与可信 Promotion 输入创建 Certificate。
    ///
    /// # Errors
    ///
    /// Scorecard 未达到 EVOLVED、Gate 未通过、继承未完成、输入修订或报告不匹配、
    /// 列表为空或重复时返回 [`CertificateError`]。
    pub fn create(
        input: EvolutionCertificateInput,
        scorecard: &EvolutionScorecard,
    ) -> Result<Self, CertificateError> {
        validate_creation(&input, scorecard)?;
        let mut certificate = Self {
            schema_version: EVOLUTION_CERTIFICATE_SCHEMA_VERSION,
            parent_revision: input.parent_revision,
            child_revision: input.child_revision,
            source_episode_ids: input.source_episode_ids,
            evolution_issue_id: input.evolution_issue_id,
            mutation_id: input.mutation_id,
            allowed_diff: input.allowed_diff,
            candidate_artifacts: input.candidate_artifacts,
            repair_dataset: input.repair_dataset,
            regression_dataset: input.regression_dataset,
            hidden_dataset: input.hidden_dataset,
            safety_dataset: input.safety_dataset,
            repaired_task_case_ids: input.repaired_task_case_ids,
            evaluation_report: scorecard.evaluation_report.clone(),
            scorecard: input.scorecard,
            gate_decision: scorecard.gate.decision,
            release_record: input.release_record,
            inheritance_verification: Some(input.inheritance_verification),
            post_promotion_run_ids: input.post_promotion_run_ids,
            lifecycle: EvolutionLifecycle::InheritanceVerified,
            certificate_digest: empty_digest(),
        };
        certificate.certificate_digest = certificate.compute_digest()?;
        Ok(certificate)
    }

    /// 计算忽略 `certificate_digest` 字段后的规范摘要。
    ///
    /// # Errors
    ///
    /// Certificate 无法序列化时返回 [`CertificateError::Serialization`]。
    pub fn compute_digest(&self) -> Result<ArtifactDigest, CertificateError> {
        let canonical = CertificateCanonical::from(self);
        let bytes = serde_json::to_vec(&canonical).map_err(CertificateError::Serialization)?;
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
            .map_err(|error| CertificateError::InvalidDigest(error.to_string()))
    }

    /// 校验结构、摘要以及所有引用 CAS 制品的内容摘要与长度。
    ///
    /// # Errors
    ///
    /// 结构无效、摘要不匹配、任一制品不存在、内容哈希或长度不匹配时返回错误。
    pub async fn verify(&self, artifacts: &dyn ArtifactStore) -> Result<(), CertificateError> {
        self.verify_digest()?;
        for reference in self
            .candidate_artifacts
            .iter()
            .chain(std::iter::once(&self.scorecard))
            .chain(self.inheritance_verification.iter())
        {
            verify_artifact(reference, artifacts).await?;
        }
        Ok(())
    }

    /// 校验结构与 Certificate 自身摘要，不读取外部 CAS。
    ///
    /// # Errors
    ///
    /// Schema、Promotion 状态、关键列表或摘要不合法时返回错误。
    pub fn verify_digest(&self) -> Result<(), CertificateError> {
        self.validate_structure()?;
        let actual = self.compute_digest()?;
        if actual != self.certificate_digest {
            return Err(CertificateError::DigestMismatch {
                declared: self.certificate_digest.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// 创建保留原证明、但生命周期更新为 RolledBack 的归档视图。
    ///
    /// # Errors
    ///
    /// 归档视图无法重新计算摘要时返回错误。
    pub fn rolled_back(&self) -> Result<Self, CertificateError> {
        let mut value = self.clone();
        value.lifecycle = EvolutionLifecycle::RolledBack;
        value.certificate_digest = value.compute_digest()?;
        Ok(value)
    }

    /// 校验不依赖 CAS 的 Certificate 结构。
    fn validate_structure(&self) -> Result<(), CertificateError> {
        if self.schema_version != EVOLUTION_CERTIFICATE_SCHEMA_VERSION {
            return Err(CertificateError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.parent_revision == self.child_revision
            || self.gate_decision != GateDecision::Pass
            || !matches!(
                self.lifecycle,
                EvolutionLifecycle::InheritanceVerified | EvolutionLifecycle::RolledBack
            )
            || self.inheritance_verification.is_none()
        {
            return Err(CertificateError::InvalidPromotionState);
        }
        require_unique_non_empty(&self.source_episode_ids, "source_episode_ids")?;
        require_unique_non_empty(&self.repaired_task_case_ids, "repaired_task_case_ids")?;
        require_unique_non_empty(&self.post_promotion_run_ids, "post_promotion_run_ids")?;
        Ok(())
    }
}

/// 不含摘要自身的规范序列化视图。
#[derive(Serialize)]
struct CertificateCanonical<'a> {
    /// 结构版本。
    schema_version: u32,
    /// Parent 修订。
    parent_revision: &'a GenomeRevisionId,
    /// Child 修订。
    child_revision: &'a GenomeRevisionId,
    /// 源 Episode。
    source_episode_ids: &'a [EpisodeId],
    /// EvolutionIssue。
    evolution_issue_id: &'a EvolutionIssueId,
    /// Mutation Proposal。
    mutation_id: &'a MutationId,
    /// 允许差异。
    allowed_diff: &'a GenomeDiff,
    /// Candidate 制品。
    candidate_artifacts: &'a [ArtifactRef],
    /// Repair Dataset。
    repair_dataset: &'a DatasetVersionId,
    /// Regression Dataset。
    regression_dataset: &'a DatasetVersionId,
    /// Hidden Dataset。
    hidden_dataset: &'a DatasetVersionId,
    /// Safety Dataset。
    safety_dataset: &'a DatasetVersionId,
    /// 修复 Case。
    repaired_task_case_ids: &'a [String],
    /// EvaluationReport。
    evaluation_report: &'a EvaluationReportId,
    /// Scorecard CAS。
    scorecard: &'a ArtifactRef,
    /// Gate 决策。
    gate_decision: GateDecision,
    /// Release 记录。
    release_record: &'a ReleaseId,
    /// 继承验证。
    inheritance_verification: &'a Option<ArtifactRef>,
    /// Promotion 后运行。
    post_promotion_run_ids: &'a [RunId],
    /// 生命周期。
    lifecycle: EvolutionLifecycle,
}

impl<'a> From<&'a EvolutionCertificate> for CertificateCanonical<'a> {
    fn from(value: &'a EvolutionCertificate) -> Self {
        Self {
            schema_version: value.schema_version,
            parent_revision: &value.parent_revision,
            child_revision: &value.child_revision,
            source_episode_ids: &value.source_episode_ids,
            evolution_issue_id: &value.evolution_issue_id,
            mutation_id: &value.mutation_id,
            allowed_diff: &value.allowed_diff,
            candidate_artifacts: &value.candidate_artifacts,
            repair_dataset: &value.repair_dataset,
            regression_dataset: &value.regression_dataset,
            hidden_dataset: &value.hidden_dataset,
            safety_dataset: &value.safety_dataset,
            repaired_task_case_ids: &value.repaired_task_case_ids,
            evaluation_report: &value.evaluation_report,
            scorecard: &value.scorecard,
            gate_decision: value.gate_decision,
            release_record: &value.release_record,
            inheritance_verification: &value.inheritance_verification,
            post_promotion_run_ids: &value.post_promotion_run_ids,
            lifecycle: value.lifecycle,
        }
    }
}

/// Certificate 创建或验证错误。
#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    /// 输入不是已经完成继承验证的 EVOLVED Promotion。
    #[error("只有 Gate PASS 且继承验证完成的 EVOLVED Scorecard 才能生成 Certificate")]
    InvalidPromotionState,
    /// 输入修订、Release 或 EvaluationReport 与 Scorecard 不匹配。
    #[error("Certificate 输入与 Scorecard 不匹配：{0}")]
    InputMismatch(&'static str),
    /// 关键列表为空或包含重复项。
    #[error("Certificate 字段 `{0}` 必须非空且不得重复")]
    InvalidList(&'static str),
    /// 结构版本未知。
    #[error("不支持的 EvolutionCertificate schema 版本 {0}")]
    UnsupportedSchemaVersion(u32),
    /// 规范 JSON 序列化失败。
    #[error("序列化 EvolutionCertificate 失败：{0}")]
    Serialization(serde_json::Error),
    /// 构造摘要类型失败。
    #[error("EvolutionCertificate 摘要无效：{0}")]
    InvalidDigest(String),
    /// 声明摘要与实际内容不一致。
    #[error("EvolutionCertificate 摘要不匹配：声明 {declared}，实际 {actual}")]
    DigestMismatch {
        /// Certificate 声明摘要。
        declared: ArtifactDigest,
        /// 重新计算摘要。
        actual: ArtifactDigest,
    },
    /// 引用 CAS 制品不存在。
    #[error("EvolutionCertificate 引用的制品不存在：{0}")]
    MissingArtifact(ArtifactDigest),
    /// 引用 CAS 制品长度与声明不一致。
    #[error("EvolutionCertificate 制品长度不匹配：{digest}，声明 {declared}，实际 {actual}")]
    ArtifactSizeMismatch {
        /// 制品摘要。
        digest: ArtifactDigest,
        /// 声明长度。
        declared: u64,
        /// 实际长度。
        actual: u64,
    },
    /// CAS 读取或摘要验证失败。
    #[error("读取 EvolutionCertificate 制品失败：{0}")]
    Artifact(#[from] ArtifactStoreError),
}

/// 创建前交叉验证 Scorecard 与 Promotion 输入。
fn validate_creation(
    input: &EvolutionCertificateInput,
    scorecard: &EvolutionScorecard,
) -> Result<(), CertificateError> {
    if scorecard.headline_verdict != crate::HeadlineVerdict::Evolved
        || scorecard.gate.decision != GateDecision::Pass
        || scorecard.lifecycle != EvolutionLifecycle::InheritanceVerified
        || !scorecard
            .inheritance
            .as_ref()
            .is_some_and(InheritanceMetrics::verified_complete)
    {
        return Err(CertificateError::InvalidPromotionState);
    }
    if input.parent_revision != scorecard.parent_revision {
        return Err(CertificateError::InputMismatch("parent_revision"));
    }
    if input.child_revision != scorecard.candidate_revision {
        return Err(CertificateError::InputMismatch("child_revision"));
    }
    if scorecard.release_record.as_ref() != Some(&input.release_record) {
        return Err(CertificateError::InputMismatch("release_record"));
    }
    require_unique_non_empty(&input.source_episode_ids, "source_episode_ids")?;
    require_unique_non_empty(&input.repaired_task_case_ids, "repaired_task_case_ids")?;
    require_unique_non_empty(&input.post_promotion_run_ids, "post_promotion_run_ids")?;
    Ok(())
}

/// 验证一个列表非空且没有重复。
fn require_unique_non_empty<T: Ord + Clone>(
    values: &[T],
    name: &'static str,
) -> Result<(), CertificateError> {
    if values.is_empty() || values.iter().cloned().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(CertificateError::InvalidList(name));
    }
    Ok(())
}

/// 读取并核对一个 ArtifactRef 的摘要与长度。
async fn verify_artifact(
    reference: &ArtifactRef,
    artifacts: &dyn ArtifactStore,
) -> Result<(), CertificateError> {
    let Some(bytes) = artifacts.get(&reference.digest).await? else {
        return Err(CertificateError::MissingArtifact(reference.digest.clone()));
    };
    if bytes.len() as u64 != reference.size_bytes {
        return Err(CertificateError::ArtifactSizeMismatch {
            digest: reference.digest.clone(),
            declared: reference.size_bytes,
            actual: bytes.len() as u64,
        });
    }
    Ok(())
}

/// 构造稍后必定被替换的合法零摘要。
fn empty_digest() -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex("0".repeat(64)).expect("固定零摘要必须合法")
}

impl InheritanceMetrics {
    /// 判断继承证据本身是否完整，不读取具体策略门槛。
    fn verified_complete(&self) -> bool {
        self.verified
            && self.restart.is_complete()
            && self.new_session.is_complete()
            && self.old_session_parent_preserved == Some(true)
            && self.stable_reference_verified
            && self.genome_digest_verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileArtifactStore, FileEvolutionArchive};
    use uuid::Uuid;

    #[test]
    fn certificate_digest_changes_after_rollback() {
        let certificate = EvolutionCertificate {
            schema_version: EVOLUTION_CERTIFICATE_SCHEMA_VERSION,
            parent_revision: GenomeRevisionId::generate(),
            child_revision: GenomeRevisionId::generate(),
            source_episode_ids: vec![EpisodeId::generate()],
            evolution_issue_id: EvolutionIssueId::generate(),
            mutation_id: MutationId::generate(),
            allowed_diff: GenomeDiff::default(),
            candidate_artifacts: Vec::new(),
            repair_dataset: DatasetVersionId::generate(),
            regression_dataset: DatasetVersionId::generate(),
            hidden_dataset: DatasetVersionId::generate(),
            safety_dataset: DatasetVersionId::generate(),
            repaired_task_case_ids: vec!["repair-1".into()],
            evaluation_report: EvaluationReportId::generate(),
            scorecard: ArtifactRef {
                digest: empty_digest(),
                media_type: "application/json".into(),
                size_bytes: 0,
            },
            gate_decision: GateDecision::Pass,
            release_record: ReleaseId::generate(),
            inheritance_verification: Some(ArtifactRef {
                digest: empty_digest(),
                media_type: "application/json".into(),
                size_bytes: 0,
            }),
            post_promotion_run_ids: vec![RunId::generate()],
            lifecycle: EvolutionLifecycle::InheritanceVerified,
            certificate_digest: empty_digest(),
        };
        let mut signed = certificate;
        signed.certificate_digest = signed.compute_digest().expect("应计算摘要");
        let rolled_back = signed.rolled_back().expect("应创建回滚视图");
        assert_ne!(signed.certificate_digest, rolled_back.certificate_digest);
        assert_eq!(rolled_back.lifecycle, EvolutionLifecycle::RolledBack);
    }

    #[tokio::test]
    async fn certificate_verify_checks_all_cas_references() {
        let root =
            std::env::temp_dir().join(format!("lucia-certificate-{}", Uuid::new_v4().simple()));
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let candidate = artifacts
            .put("application/json", br#"{"candidate":true}"#)
            .await
            .expect("应写入 Candidate 制品");
        let scorecard = artifacts
            .put("application/json", br#"{"schema_version":1}"#)
            .await
            .expect("应写入 Scorecard");
        let inheritance = artifacts
            .put("application/json", br#"{"verified":true}"#)
            .await
            .expect("应写入继承证据");
        let mut certificate = EvolutionCertificate {
            schema_version: EVOLUTION_CERTIFICATE_SCHEMA_VERSION,
            parent_revision: GenomeRevisionId::generate(),
            child_revision: GenomeRevisionId::generate(),
            source_episode_ids: vec![EpisodeId::generate()],
            evolution_issue_id: EvolutionIssueId::generate(),
            mutation_id: MutationId::generate(),
            allowed_diff: GenomeDiff::default(),
            candidate_artifacts: vec![candidate],
            repair_dataset: DatasetVersionId::generate(),
            regression_dataset: DatasetVersionId::generate(),
            hidden_dataset: DatasetVersionId::generate(),
            safety_dataset: DatasetVersionId::generate(),
            repaired_task_case_ids: vec!["repair-1".into()],
            evaluation_report: EvaluationReportId::generate(),
            scorecard,
            gate_decision: GateDecision::Pass,
            release_record: ReleaseId::generate(),
            inheritance_verification: Some(inheritance),
            post_promotion_run_ids: vec![RunId::generate()],
            lifecycle: EvolutionLifecycle::InheritanceVerified,
            certificate_digest: empty_digest(),
        };
        certificate.certificate_digest = certificate.compute_digest().expect("应计算摘要");
        certificate
            .verify(&artifacts)
            .await
            .expect("全部 CAS 引用应验证通过");

        let archive = FileEvolutionArchive::new(&root);
        archive
            .append_certificate(&certificate)
            .await
            .expect("Certificate 应归档");
        let loaded = archive
            .certificate(&certificate.release_record)
            .await
            .expect("归档应可读取")
            .expect("Certificate 应存在");
        assert_eq!(loaded.certificate_digest, certificate.certificate_digest);
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
