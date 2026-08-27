//! Promotion 的不可变 Evolution Certificate。

use crate::{ArtifactStore, ArtifactStoreError, EvolutionScorecard};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, DatasetVersionId, EpisodeId, EvaluationReportId, EvolutionIssueId,
    EvolutionLifecycle, GateDecision, GenomeDiff, GenomeRevisionId, InheritanceVerification,
    MutationId, ReleaseId, RunId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// 当前 EvolutionCertificate 结构版本。
pub const EVOLUTION_CERTIFICATE_SCHEMA_VERSION: u32 = 1;

/// 当前 RollbackRecord 结构版本。
pub const ROLLBACK_RECORD_SCHEMA_VERSION: u32 = 1;

/// Release 被回滚的稳定原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackCategory {
    /// 安全、权限、泄漏或完整性门槛失败。
    Safety,
    /// 能力、稳定性或资源性能发生不可接受退化。
    Performance,
    /// 评测、Registry、依赖或运行基础设施故障。
    Infrastructure,
    /// 由授权人员基于外部业务条件执行的人工回滚。
    Manual,
}

/// 一次 Release 回滚的不可变审计记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackRecord {
    /// RollbackRecord 结构版本。
    pub schema_version: u32,
    /// 被回滚的 Release。
    pub release_record: ReleaseId,
    /// 稳定原因分类，用于历史回滚率拆分。
    pub category: RollbackCategory,
    /// 不含 Secret 或 Hidden 内容的人工可读原因。
    pub reason: String,
    /// 支撑回滚决定的不可变 CAS 制品。
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    /// 回滚记录生成时间，Unix 毫秒。
    pub created_at_ms: u64,
}

impl RollbackRecord {
    /// 校验 Release 绑定、原因和制品引用的基本结构。
    ///
    /// # Errors
    ///
    /// Schema 未知、Release 不匹配、原因为空或 Evidence 摘要重复时返回错误。
    fn validate(&self, expected_release: &ReleaseId) -> Result<(), CertificateError> {
        if self.schema_version != ROLLBACK_RECORD_SCHEMA_VERSION {
            return Err(CertificateError::UnsupportedRollbackSchemaVersion(
                self.schema_version,
            ));
        }
        if &self.release_record != expected_release {
            return Err(CertificateError::RollbackReleaseMismatch);
        }
        if self.reason.trim().is_empty() {
            return Err(CertificateError::InvalidRollbackReason);
        }
        require_unique(
            self.evidence.iter().map(|artifact| &artifact.digest),
            "rollback.evidence",
        )
    }
}

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
    /// 同一 Release 内从零开始递增的不可变状态修订号。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u32,
    /// 上一状态修订的 Certificate 摘要；初始 Promotion 为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_certificate_digest: Option<ArtifactDigest>,
    /// 回滚状态绑定的正式记录；非回滚状态为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_record: Option<RollbackRecord>,
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
}

impl EvolutionCertificate {
    /// 从成功 Promotion 的 Scorecard 与可信发布输入创建初始 Certificate。
    ///
    /// 初始证明固定为 [`EvolutionLifecycle::Promoted`]，继承证据随后通过
    /// [`Self::with_inheritance`] 形成新的不可变状态修订。
    ///
    /// # Errors
    ///
    /// Scorecard 不是成功 Promotion、Gate 未通过、输入修订或报告不匹配，或关键列表为空、
    /// 重复时返回 [`CertificateError`]。
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
            inheritance_verification: None,
            post_promotion_run_ids: Vec::new(),
            lifecycle: EvolutionLifecycle::Promoted,
            revision: 0,
            previous_certificate_digest: None,
            rollback_record: None,
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
            .chain(
                self.rollback_record
                    .iter()
                    .flat_map(|record| record.evidence.iter()),
            )
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

    /// 创建绑定正式继承制品的新状态修订，保留初始 Promotion Certificate。
    ///
    /// `verification` 必须证明重启、新 Session、旧 Session、Stable Ref 和 Genome 摘要均
    /// 符合当前 Child Genome；`artifact` 是该结构化验证结果在 CAS 中的引用。
    ///
    /// # Errors
    ///
    /// 当前状态不是 Promoted、验证不完整、Genome 不匹配、Run ID 为空或重复，或摘要无法
    /// 计算时返回错误。
    pub fn with_inheritance(
        &self,
        verification: &InheritanceVerification,
        artifact: ArtifactRef,
        post_promotion_run_ids: Vec<RunId>,
    ) -> Result<Self, CertificateError> {
        self.verify_digest()?;
        if self.lifecycle != EvolutionLifecycle::Promoted
            || verification.expected_genome != self.child_revision
            || !inheritance_verification_is_complete(verification)
        {
            return Err(CertificateError::InvalidInheritanceRevision);
        }
        require_unique_non_empty(&post_promotion_run_ids, "post_promotion_run_ids")?;
        let mut value = self.clone();
        value.lifecycle = EvolutionLifecycle::InheritanceVerified;
        value.inheritance_verification = Some(artifact);
        value.post_promotion_run_ids = post_promotion_run_ids;
        value.revision = self
            .revision
            .checked_add(1)
            .ok_or(CertificateError::RevisionOverflow)?;
        value.previous_certificate_digest = Some(self.certificate_digest.clone());
        value.rollback_record = None;
        value.certificate_digest = value.compute_digest()?;
        Ok(value)
    }

    /// 创建保留全部既有证明、并绑定正式 RollbackRecord 的不可变状态修订。
    ///
    /// # Errors
    ///
    /// 当前状态不是 Promoted 或 InheritanceVerified、RollbackRecord 无效，或摘要无法重新计算
    /// 时返回错误。
    pub fn rolled_back(&self, record: RollbackRecord) -> Result<Self, CertificateError> {
        self.verify_digest()?;
        if !matches!(
            self.lifecycle,
            EvolutionLifecycle::Promoted | EvolutionLifecycle::InheritanceVerified
        ) {
            return Err(CertificateError::InvalidRollbackTransition);
        }
        record.validate(&self.release_record)?;
        let mut value = self.clone();
        value.lifecycle = EvolutionLifecycle::RolledBack;
        value.revision = self
            .revision
            .checked_add(1)
            .ok_or(CertificateError::RevisionOverflow)?;
        value.previous_certificate_digest = Some(self.certificate_digest.clone());
        value.rollback_record = Some(record);
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
        if self.parent_revision == self.child_revision || self.gate_decision != GateDecision::Pass {
            return Err(CertificateError::InvalidPromotionState);
        }
        require_unique_non_empty(&self.source_episode_ids, "source_episode_ids")?;
        require_unique_non_empty(&self.repaired_task_case_ids, "repaired_task_case_ids")?;
        require_unique(
            self.candidate_artifacts
                .iter()
                .map(|artifact| &artifact.digest),
            "candidate_artifacts",
        )?;
        match self.lifecycle {
            EvolutionLifecycle::Promoted
                if self.revision == 0
                    && self.previous_certificate_digest.is_none()
                    && self.inheritance_verification.is_none()
                    && self.post_promotion_run_ids.is_empty()
                    && self.rollback_record.is_none() => {}
            EvolutionLifecycle::InheritanceVerified
                if ((self.revision == 0 && self.previous_certificate_digest.is_none())
                    || (self.revision > 0 && self.previous_certificate_digest.is_some()))
                    && self.inheritance_verification.is_some()
                    && self.rollback_record.is_none() =>
            {
                require_unique_non_empty(&self.post_promotion_run_ids, "post_promotion_run_ids")?;
            }
            EvolutionLifecycle::RolledBack
                if self.revision > 0
                    && self.previous_certificate_digest.is_some()
                    && self.rollback_record.is_some() =>
            {
                self.rollback_record
                    .as_ref()
                    .expect("已检查 RollbackRecord 存在")
                    .validate(&self.release_record)?;
            }
            // 兼容 schema v1 早期只更新 lifecycle、尚未携带正式 RollbackRecord 的归档快照。
            EvolutionLifecycle::RolledBack
                if self.revision == 0
                    && self.previous_certificate_digest.is_none()
                    && self.inheritance_verification.is_some()
                    && !self.post_promotion_run_ids.is_empty()
                    && self.rollback_record.is_none() =>
            {
                require_unique_non_empty(&self.post_promotion_run_ids, "post_promotion_run_ids")?;
            }
            _ => return Err(CertificateError::InvalidCertificateLifecycle),
        }
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
    /// 状态修订号。
    #[serde(skip_serializing_if = "is_zero")]
    revision: u32,
    /// 前一状态修订摘要。
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_certificate_digest: &'a Option<ArtifactDigest>,
    /// 正式回滚记录。
    #[serde(skip_serializing_if = "Option::is_none")]
    rollback_record: &'a Option<RollbackRecord>,
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
            revision: value.revision,
            previous_certificate_digest: &value.previous_certificate_digest,
            rollback_record: &value.rollback_record,
        }
    }
}

/// Certificate 创建或验证错误。
#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    /// 输入不是 Gate PASS 且带 Release 的成功 Promotion。
    #[error("只有 Gate PASS 且已发布的 Scorecard 才能生成 Promotion Certificate")]
    InvalidPromotionState,
    /// Certificate 生命周期与继承、回滚或修订字段不一致。
    #[error("EvolutionCertificate 生命周期字段组合无效")]
    InvalidCertificateLifecycle,
    /// 继承状态修订不是从 Promoted 生成，或验证内容不完整。
    #[error("EvolutionCertificate 继承状态修订无效")]
    InvalidInheritanceRevision,
    /// Rollback 状态修订不能从当前生命周期生成。
    #[error("EvolutionCertificate 回滚状态转换无效")]
    InvalidRollbackTransition,
    /// 状态修订号无法继续递增。
    #[error("EvolutionCertificate 状态修订号溢出")]
    RevisionOverflow,
    /// RollbackRecord Schema 未知。
    #[error("不支持的 RollbackRecord schema 版本 {0}")]
    UnsupportedRollbackSchemaVersion(u32),
    /// RollbackRecord 指向了其他 Release。
    #[error("RollbackRecord 与 EvolutionCertificate 的 Release 不匹配")]
    RollbackReleaseMismatch,
    /// RollbackRecord 原因为空。
    #[error("RollbackRecord 必须包含非空原因")]
    InvalidRollbackReason,
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
    if scorecard.gate.decision != GateDecision::Pass
        || !matches!(
            scorecard.lifecycle,
            EvolutionLifecycle::Promoted | EvolutionLifecycle::InheritanceVerified
        )
        || scorecard.release_record.is_none()
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
    require_unique(
        input
            .candidate_artifacts
            .iter()
            .map(|artifact| &artifact.digest),
        "candidate_artifacts",
    )?;
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

/// 校验允许为空的集合不包含重复值。
fn require_unique<'a, T: 'a + Ord>(
    values: impl IntoIterator<Item = &'a T>,
    name: &'static str,
) -> Result<(), CertificateError> {
    let mut unique = BTreeSet::new();
    if values.into_iter().any(|value| !unique.insert(value)) {
        return Err(CertificateError::InvalidList(name));
    }
    Ok(())
}

/// 判断原始继承协议是否完整证明全部运行边界。
fn inheritance_verification_is_complete(value: &InheritanceVerification) -> bool {
    value.verified
        && value.restart_cases_total > 0
        && value.restart_cases_passed == value.restart_cases_total
        && value.new_session_cases_total > 0
        && value.new_session_cases_passed == value.new_session_cases_total
        && value.old_session_parent_preserved == Some(true)
        && value.stable_reference_verified
        && value.genome_digest_verified
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

/// Serde 兼容辅助：零值状态修订不写入旧 Certificate 的规范 JSON。
const fn is_zero(value: &u32) -> bool {
    *value == 0
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
            revision: 0,
            previous_certificate_digest: None,
            rollback_record: None,
            certificate_digest: empty_digest(),
        };
        let mut signed = certificate;
        signed.certificate_digest = signed.compute_digest().expect("应计算摘要");
        let legacy_json = serde_json::to_value(&signed).expect("应序列化兼容 Certificate");
        assert!(legacy_json.get("revision").is_none());
        assert!(legacy_json.get("previous_certificate_digest").is_none());
        assert!(legacy_json.get("rollback_record").is_none());
        serde_json::from_value::<EvolutionCertificate>(legacy_json)
            .expect("旧 Certificate JSON 应保持可读")
            .verify_digest()
            .expect("旧 Certificate 摘要应保持有效");
        let rolled_back = signed
            .rolled_back(RollbackRecord {
                schema_version: ROLLBACK_RECORD_SCHEMA_VERSION,
                release_record: signed.release_record.clone(),
                category: RollbackCategory::Safety,
                reason: "发现安全回归".into(),
                evidence: Vec::new(),
                created_at_ms: 1,
            })
            .expect("应创建回滚视图");
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
            revision: 0,
            previous_certificate_digest: None,
            rollback_record: None,
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

    /// Archive 必须保留 Promotion、继承和回滚三个不可变修订，并只返回最终生命周期视图。
    #[tokio::test]
    async fn archive_preserves_certificate_lifecycle_revisions() {
        let root = std::env::temp_dir().join(format!(
            "lucia-certificate-chain-{}",
            Uuid::new_v4().simple()
        ));
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let scorecard = artifacts
            .put("application/json", br#"{"scorecard":true}"#)
            .await
            .expect("应写入 Scorecard");
        let inheritance_artifact = artifacts
            .put("application/json", br#"{"verified":true}"#)
            .await
            .expect("应写入继承证据");
        let rollback_evidence = artifacts
            .put("application/json", br#"{"safety_regression":true}"#)
            .await
            .expect("应写入回滚证据");
        let child = GenomeRevisionId::generate();
        let release = ReleaseId::generate();
        let mut promotion = EvolutionCertificate {
            schema_version: EVOLUTION_CERTIFICATE_SCHEMA_VERSION,
            parent_revision: GenomeRevisionId::generate(),
            child_revision: child.clone(),
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
            scorecard,
            gate_decision: GateDecision::Pass,
            release_record: release.clone(),
            inheritance_verification: None,
            post_promotion_run_ids: Vec::new(),
            lifecycle: EvolutionLifecycle::Promoted,
            revision: 0,
            previous_certificate_digest: None,
            rollback_record: None,
            certificate_digest: empty_digest(),
        };
        promotion.certificate_digest = promotion.compute_digest().expect("应签署 Promotion");
        let verification = InheritanceVerification {
            expected_genome: child,
            observed_genome_after_restart: Some(promotion.child_revision.clone()),
            restart_cases_passed: 1,
            restart_cases_total: 1,
            new_session_cases_passed: 1,
            new_session_cases_total: 1,
            old_session_parent_preserved: Some(true),
            stable_reference_verified: true,
            genome_digest_verified: true,
            verified: true,
        };
        let inherited = promotion
            .with_inheritance(&verification, inheritance_artifact, vec![RunId::generate()])
            .expect("应生成继承修订");
        let rolled_back = inherited
            .rolled_back(RollbackRecord {
                schema_version: ROLLBACK_RECORD_SCHEMA_VERSION,
                release_record: release.clone(),
                category: RollbackCategory::Safety,
                reason: "发现安全回归".into(),
                evidence: vec![rollback_evidence],
                created_at_ms: 3,
            })
            .expect("应生成回滚修订");
        let archive = FileEvolutionArchive::new(&root);
        archive
            .append_certificate(&promotion)
            .await
            .expect("应归档 Promotion");
        archive
            .append_certificate(&inherited)
            .await
            .expect("应归档继承修订");
        archive
            .append_certificate(&rolled_back)
            .await
            .expect("应归档回滚修订");
        let loaded = archive
            .certificate(&release)
            .await
            .expect("应读取链尾")
            .expect("Certificate 应存在");
        assert_eq!(loaded.lifecycle, EvolutionLifecycle::RolledBack);
        assert_eq!(loaded.revision, 2);
        loaded
            .verify(&artifacts)
            .await
            .expect("回滚 Evidence 也必须通过 CAS 验证");
        assert_eq!(
            archive
                .certificate_history(&release)
                .await
                .expect("应读取完整状态修订链")
                .len(),
            3
        );
        assert_eq!(
            archive.list_certificates().await.expect("应读取归档").len(),
            1
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
