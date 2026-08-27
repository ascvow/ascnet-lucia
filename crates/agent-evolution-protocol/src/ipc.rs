//! 独立 Evaluator 与 Evolver 之间的版本化 IPC 协议。
//!
//! 请求只允许声明强类型身份和并发前置条件。Dataset 路径、Hidden Verifier、Commit
//! Policy、Release Store 与权限上限均由受信 `lucia-eval` 进程配置，不能通过 IPC 覆盖。

use crate::{
    ArtifactDigest, ArtifactRef, AuditRecordId, CandidateId, ContextPolicyEvaluationReportV1,
    DatasetVersionId, EvaluationReportId, EvolutionLifecycle, GateDecision, GenomeDigest,
    GenomeRevision, GenomeRevisionId, ReleaseId, SkillCandidateV1, SkillId,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeMap;

/// 当前 Evaluator 请求协议版本；Evaluate、Promote、Health 与 Rollback 共用该严格版本。
pub const EVALUATION_REQUEST_SCHEMA_VERSION: u32 = 1;
/// 当前 Evaluation Receipt 协议版本。
pub const EVALUATION_RECEIPT_SCHEMA_VERSION: u32 = 1;
/// 当前 Context Evaluation Receipt 协议版本。
pub const CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION: u32 = 1;
/// 当前 Skill Evaluation 请求与回执协议版本。
pub const SKILL_EVALUATION_IPC_SCHEMA_VERSION: u32 = 1;
/// 当前 Skill Health 请求与回执协议版本。
pub const SKILL_HEALTH_IPC_SCHEMA_VERSION: u32 = 1;
/// Skill Candidate 规范 JSON 快照的固定 Artifact CAS 媒体类型。
pub const SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.skill-candidate.v1+json";
/// 当前 Health Receipt 协议版本。
pub const HEALTH_RECEIPT_SCHEMA_VERSION: u32 = 1;
/// 当前 Release Receipt 协议版本。
pub const RELEASE_RECEIPT_SCHEMA_VERSION: u32 = 1;

const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_LINEAGE_BYTES: usize = 128;
const MAX_POLICY_VERSION_BYTES: usize = 256;
const MAX_DIGEST_TEXT_BYTES: usize = 256;

/// Evolver 提交给独立 Evaluator 的最小比较请求。
///
/// 请求不含路径、Dataset 正文、Verifier、Policy 或 Prompt 正文；这些输入只能来自受信
/// Evaluator 配置和 Registry。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 调用方生成的稳定幂等标识；不得包含路径或用户内容。
    pub request_id: String,
    /// 当前 Stable Parent 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// 待评测 Candidate 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// 受信 Stable lineage。
    pub lineage: String,
    /// 调用方观察到的 Parent 代数，用作并发前置条件。
    pub expected_parent_generation: u64,
    /// 调用方期望使用的受信 Dataset 版本；实际 Dataset 根由 Evaluator 配置决定。
    pub expected_dataset_version: DatasetVersionId,
}

impl EvaluationRequestV1 {
    /// 校验不依赖受信 Store 的请求结构边界。
    ///
    /// # Errors
    ///
    /// Schema 未知、请求 ID 或 lineage 非法，或 Parent/Candidate 相同时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        validate_request_schema(self.schema_version)?;
        validate_request_id(&self.request_id)?;
        validate_lineage(&self.lineage)?;
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidEvaluatorIpc::SameRevision);
        }
        Ok(())
    }
}

/// Evolver 提交给独立 Evaluator 的最小 Context Policy 比较请求。
///
/// 请求只声明强类型修订、Stable 前置条件和预期 Fixture 版本。原始观察、Fixture 路径、
/// Context Gate 阈值和 Archive 路径只能来自受信 `lucia-eval` 配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEvaluationRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 调用方生成的稳定幂等标识。
    pub request_id: String,
    /// 当前 Stable Parent 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// 待评测 Candidate 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// 受信 Stable lineage。
    pub lineage: String,
    /// 调用方观察到的 Parent 代数。
    pub expected_parent_generation: u64,
    /// 调用方期望的受信 Context Fixture 版本。
    pub expected_fixture_version: DatasetVersionId,
}

impl ContextEvaluationRequestV1 {
    /// 校验请求版本、稳定名称和 Parent/Candidate 身份边界。
    ///
    /// # Errors
    ///
    /// Schema 未知、请求 ID 或 lineage 非法，或两个修订相同时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        validate_request_schema(self.schema_version)?;
        validate_request_id(&self.request_id)?;
        validate_lineage(&self.lineage)?;
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidEvaluatorIpc::SameRevision);
        }
        Ok(())
    }

    /// 转成现有可信 Archive 使用的通用请求绑定，不携带任何 Context Gate 控制字段。
    pub fn archive_request(&self) -> EvaluationRequestV1 {
        EvaluationRequestV1 {
            schema_version: self.schema_version,
            request_id: self.request_id.clone(),
            parent_revision_id: self.parent_revision_id.clone(),
            candidate_revision_id: self.candidate_revision_id.clone(),
            lineage: self.lineage.clone(),
            expected_parent_generation: self.expected_parent_generation,
            expected_dataset_version: self.expected_fixture_version.clone(),
        }
    }
}

/// Evolver 提交给独立 Skill Evaluator 的最小请求。
///
/// Candidate 正文只通过不可变 Artifact CAS 引用传递。使用观察、Episode/Run/Skill 绑定、
/// 激活授权、Gate Policy 和存储路径必须由 Evaluator 的受信 Registry 配置提供。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvaluationRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 调用方生成的稳定幂等标识。
    pub request_id: String,
    /// 待评测 Candidate 的强类型身份。
    pub candidate_id: CandidateId,
    /// Candidate Genome 修订，用于拒绝同一 Candidate ID 的修订错绑。
    pub candidate_revision_id: GenomeRevisionId,
    /// Candidate Genome 行为摘要，用于拒绝 Store 中的内容替换。
    pub candidate_genome_digest: GenomeDigest,
    /// Candidate 规范 JSON 在共享 Artifact CAS 中的不可变引用。
    pub candidate_artifact: ArtifactRef,
    /// Gate 写入 Evaluated 状态的可信时间。
    pub evaluated_at_ms: u64,
    /// Gate 写入 Active 状态的可信时间。
    pub activated_at_ms: u64,
}

impl SkillEvaluationRequestV1 {
    /// 校验版本、请求 ID、Candidate 媒体类型和 Q→E→A 时间顺序。
    ///
    /// # Errors
    ///
    /// 请求版本、短标识、Artifact 引用或时间顺序无效时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != SKILL_EVALUATION_IPC_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedSkillEvaluationSchema {
                found: self.schema_version,
                supported: SKILL_EVALUATION_IPC_SCHEMA_VERSION,
            });
        }
        validate_request_id(&self.request_id)?;
        validate_artifact_ref(&self.candidate_artifact)?;
        if self.candidate_artifact.media_type != SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE {
            return Err(InvalidEvaluatorIpc::InvalidSkillCandidateArtifact);
        }
        if self.evaluated_at_ms == 0 || self.activated_at_ms <= self.evaluated_at_ms {
            return Err(InvalidEvaluatorIpc::InvalidSkillEvaluationTime);
        }
        Ok(())
    }
}

/// 独立 Skill Evaluator 的脱敏 Gate 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillEvaluationOutcomeV1 {
    /// Gate 拒绝 Candidate，但正式报告仍保留在 Artifact CAS。
    Rejected {
        /// 正式评测报告。
        report_id: EvaluationReportId,
        /// 报告的不可变 CAS 引用。
        report_artifact: ArtifactRef,
    },
    /// Gate 完成 Q→E→A，并返回后续 Active Genome。
    Promoted {
        /// 已绑定正式报告的 Candidate 快照。
        evaluated_candidate: Box<SkillCandidateV1>,
        /// 正式评测报告。
        report_id: EvaluationReportId,
        /// 报告的不可变 CAS 引用。
        report_artifact: ArtifactRef,
        /// 每个变更 Skill 的 Active 制品引用。
        active_skill_artifacts: BTreeMap<SkillId, ArtifactRef>,
        /// 只替换 Active Skill Set 的后续 Serve Genome。
        active_genome: Box<GenomeRevision>,
        /// 不含正文的授权证据 ID。
        authorization_evidence_id: String,
        /// 受信授权是否允许生产 Stable 发布。
        production_permitted: bool,
    },
}

/// 独立 Skill Evaluator 返回的严格绑定回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEvaluationReceiptV1 {
    /// 回执 schema 版本。
    pub schema_version: u32,
    /// 对应请求的幂等 ID。
    pub request_id: String,
    /// 对应 Candidate。
    pub candidate_id: CandidateId,
    /// 独立 Gate 结果。
    pub result: SkillEvaluationOutcomeV1,
}

impl SkillEvaluationReceiptV1 {
    /// 校验回执版本、请求 ID、报告制品和 Promotion 主体的局部结构。
    ///
    /// # Errors
    ///
    /// 版本、标识、Artifact、Candidate、Genome 或授权字段无效时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != SKILL_EVALUATION_IPC_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedSkillEvaluationSchema {
                found: self.schema_version,
                supported: SKILL_EVALUATION_IPC_SCHEMA_VERSION,
            });
        }
        validate_request_id(&self.request_id)?;
        match &self.result {
            SkillEvaluationOutcomeV1::Rejected {
                report_artifact, ..
            } => validate_artifact_ref(report_artifact),
            SkillEvaluationOutcomeV1::Promoted {
                evaluated_candidate,
                report_artifact,
                active_skill_artifacts,
                active_genome,
                authorization_evidence_id,
                ..
            } => {
                validate_artifact_ref(report_artifact)?;
                evaluated_candidate
                    .validate()
                    .map_err(|error| InvalidEvaluatorIpc::InvalidSkillPayload(error.to_string()))?;
                if evaluated_candidate.candidate_id != self.candidate_id {
                    return Err(InvalidEvaluatorIpc::SkillReceiptCandidateMismatch);
                }
                active_genome
                    .validate()
                    .map_err(|error| InvalidEvaluatorIpc::InvalidSkillPayload(error.to_string()))?;
                if active_skill_artifacts.is_empty()
                    || active_skill_artifacts
                        .values()
                        .any(|artifact| validate_artifact_ref(artifact).is_err())
                    || !valid_control_id(authorization_evidence_id)
                {
                    return Err(InvalidEvaluatorIpc::InvalidSkillPayload(
                        "Skill Promotion 回执字段无效".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Evolver 请求独立控制面复核 Skill Promotion 健康状态的最小请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillHealthRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 调用方生成的稳定幂等标识。
    pub request_id: String,
    /// 被复核的 Promotion Release。
    pub release_id: ReleaseId,
    /// Stable lineage。
    pub lineage: String,
    /// 期望已发布的 Genome Revision。
    pub expected_revision_id: GenomeRevisionId,
    /// 期望的 Stable 代数。
    pub expected_generation: u64,
}

impl SkillHealthRequestV1 {
    /// 校验请求版本、短标识、lineage 和代数。
    ///
    /// # Errors
    ///
    /// 任一局部结构无效时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != SKILL_HEALTH_IPC_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedSkillHealthSchema {
                found: self.schema_version,
                supported: SKILL_HEALTH_IPC_SCHEMA_VERSION,
            });
        }
        validate_request_id(&self.request_id)?;
        validate_lineage(&self.lineage)?;
        if self.expected_generation == 0 {
            return Err(InvalidEvaluatorIpc::InvalidGeneration);
        }
        Ok(())
    }
}

/// Skill 健康复核的受信结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillHealthStatusV1 {
    /// Promotion 后运行健康。
    Healthy {
        /// 不含用户正文的健康证据 ID。
        evidence_id: String,
    },
    /// Promotion 后运行不健康。
    Unhealthy {
        /// 不含用户正文的健康证据 ID。
        evidence_id: String,
        /// 稳定失败码。
        reason_code: String,
    },
}

impl SkillHealthStatusV1 {
    /// 校验健康证据 ID 与可选失败码均为有限控制面标识。
    ///
    /// # Errors
    ///
    /// 任一字段为空、过长或包含路径字符时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        match self {
            Self::Healthy { evidence_id } if valid_control_id(evidence_id) => Ok(()),
            Self::Unhealthy {
                evidence_id,
                reason_code,
            } if valid_control_id(evidence_id) && valid_control_id(reason_code) => Ok(()),
            _ => Err(InvalidEvaluatorIpc::InvalidSkillHealthVerdict),
        }
    }
}

/// 独立 Skill 健康控制面返回的严格绑定回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillHealthReceiptV1 {
    /// 回执 schema 版本。
    pub schema_version: u32,
    /// 对应请求 ID。
    pub request_id: String,
    /// 被复核的 Promotion Release。
    pub release_id: ReleaseId,
    /// Stable lineage。
    pub lineage: String,
    /// 请求期望的 Revision。
    pub expected_revision_id: GenomeRevisionId,
    /// 控制面实际读取到的 Revision。
    pub observed_revision_id: GenomeRevisionId,
    /// 请求期望的代数。
    pub expected_generation: u64,
    /// 控制面实际读取到的代数。
    pub observed_generation: u64,
    /// 受信健康结论。
    pub result: SkillHealthStatusV1,
}

impl SkillHealthReceiptV1 {
    /// 校验回执版本、身份、代数与有限健康标识。
    ///
    /// # Errors
    ///
    /// 版本、请求绑定或健康标识无效时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != SKILL_HEALTH_IPC_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedSkillHealthSchema {
                found: self.schema_version,
                supported: SKILL_HEALTH_IPC_SCHEMA_VERSION,
            });
        }
        validate_request_id(&self.request_id)?;
        validate_lineage(&self.lineage)?;
        if self.expected_generation == 0 || self.observed_generation == 0 {
            return Err(InvalidEvaluatorIpc::InvalidGeneration);
        }
        self.result.validate()
    }
}

/// 使用正式 EvaluationReport 执行 Promotion 的受限请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 已完成 Seal 的正式 EvaluationReport。
    pub report_id: EvaluationReportId,
    /// 本次 Promotion 的幂等 Release ID。
    pub release_id: ReleaseId,
}

impl PromotionRequestV1 {
    /// 校验请求 schema；Report、Gate 与 Stable 前置条件由受信 Release Controller 复核。
    ///
    /// # Errors
    ///
    /// Schema 版本未知时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        validate_request_schema(self.schema_version)
    }
}

/// 把指定 Promotion 原子回滚到 Parent 的受限请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 被撤销的 Promotion Release。
    pub release_id: ReleaseId,
    /// 本次 Rollback 自身的幂等 Release ID。
    pub rollback_release_id: ReleaseId,
}

impl RollbackRequestV1 {
    /// 校验 schema 和两个 Release ID 的语义边界。
    ///
    /// # Errors
    ///
    /// Schema 未知或 Rollback 与原 Promotion 使用相同 Release ID 时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        validate_request_schema(self.schema_version)?;
        if self.release_id == self.rollback_release_id {
            return Err(InvalidEvaluatorIpc::SameRelease);
        }
        Ok(())
    }
}

/// 请求受信 Evaluator 验证 Promotion 后运行健康状态的最小输入。
///
/// 请求只声明 Release 绑定和预期 Stable 状态；真实运行观察由 `lucia-eval` 从受信健康
/// Store 加载，Evolver 不能通过 IPC 自报成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckRequestV1 {
    /// 请求 schema 版本。
    pub schema_version: u32,
    /// 调用方生成的稳定幂等标识；不得包含路径或用户内容。
    pub request_id: String,
    /// 正在验证的 Promotion Release。
    pub release_id: ReleaseId,
    /// Stable lineage。
    pub lineage: String,
    /// Promotion 后应由新运行使用的 Candidate Revision。
    pub expected_revision_id: GenomeRevisionId,
    /// Promotion 后预期的 Stable 单调代数。
    pub expected_generation: u64,
}

impl HealthCheckRequestV1 {
    /// 校验 schema、请求 ID 与 lineage；真实 Release 和 Stable 绑定由受信 Evaluator 复核。
    ///
    /// # Errors
    ///
    /// Schema 未知、请求 ID 或 lineage 非法时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        validate_request_schema(self.schema_version)?;
        validate_request_id(&self.request_id)?;
        validate_lineage(&self.lineage)
    }
}

/// 受信 Runtime 写入健康 Store 的版本化脱敏观察。
///
/// 观察只携带 Release、实际 Genome 和聚合检查计数，不含用户正文、模型输出或 ToolResult。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHealthObservationV1 {
    /// 观察 schema 版本，与 Evaluator 请求协议同步演进。
    pub schema_version: u32,
    /// 观察所属的 Promotion Release。
    pub release_id: ReleaseId,
    /// 新运行实际使用的 Genome Revision。
    pub observed_revision_id: GenomeRevisionId,
    /// 通过的受信健康检查数。
    pub checks_passed: u32,
    /// 执行的受信健康检查总数；必须大于零。
    pub checks_total: u32,
    /// 观察生成的 Unix 毫秒时间。
    pub observed_at_ms: u64,
}

impl RuntimeHealthObservationV1 {
    /// 校验 schema 与聚合计数边界。
    ///
    /// # Errors
    ///
    /// Schema 未知、检查总数为零或通过数超过总数时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        validate_request_schema(self.schema_version)?;
        validate_health_counts(self.checks_passed, self.checks_total)
    }
}

/// 独立 Evaluator 提交正式 Report Seal 后返回的脱敏回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReceiptV1 {
    /// 回执 schema 版本。
    pub schema_version: u32,
    /// 对应请求的幂等 ID。
    pub request_id: String,
    /// 正式 EvaluationReport ID。
    pub report_id: EvaluationReportId,
    /// Report 存储字节摘要。
    pub report_digest: ArtifactDigest,
    /// 提交 Report 的 Audit 记录。
    pub audit_record_id: AuditRecordId,
    /// 提交完成后的 Audit 链头摘要。
    pub audit_head_digest: ArtifactDigest,
    /// 被比较的 Parent 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// 被比较的 Candidate 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// Evaluator 固定的 Evaluation Policy 版本。
    pub evaluation_policy_version: String,
    /// Evaluator 固定的 Commit Policy 版本。
    pub commit_policy_version: String,
    /// 受信 Verifier Registry 集合摘要。
    pub verifier_set_digest: String,
    /// 最终 Gate 决策。
    pub gate_decision: GateDecision,
    /// Report 提交时的生命周期。
    pub lifecycle: EvolutionLifecycle,
}

impl EvaluationReceiptV1 {
    /// 校验回执的版本、请求绑定和稳定版本字段。
    ///
    /// # Errors
    ///
    /// Schema 未知、请求 ID 或版本字段非法，或 Parent/Candidate 相同时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != EVALUATION_RECEIPT_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedEvaluationReceiptSchema {
                found: self.schema_version,
                supported: EVALUATION_RECEIPT_SCHEMA_VERSION,
            });
        }
        validate_request_id(&self.request_id)?;
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidEvaluatorIpc::SameRevision);
        }
        validate_bounded_text(
            "evaluation_policy_version",
            &self.evaluation_policy_version,
            MAX_POLICY_VERSION_BYTES,
        )?;
        validate_bounded_text(
            "commit_policy_version",
            &self.commit_policy_version,
            MAX_POLICY_VERSION_BYTES,
        )?;
        validate_bounded_text(
            "verifier_set_digest",
            &self.verifier_set_digest,
            MAX_DIGEST_TEXT_BYTES,
        )
    }
}

/// 独立 Evaluator 提交 Context Gate Report 与正式 Report Seal 后返回的脱敏回执。
///
/// Context 报告包含八项聚合指标和稳定失败类别，不含原始观察、Fixture 正文或用户数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEvaluationReceiptV1 {
    /// 回执 schema 版本。
    pub schema_version: u32,
    /// 对应请求的幂等 ID。
    pub request_id: String,
    /// 正式通用 EvaluationReport ID，供 Release Controller 使用。
    pub report_id: EvaluationReportId,
    /// 正式通用 EvaluationReport 存储字节摘要。
    pub report_digest: ArtifactDigest,
    /// Context Gate 聚合报告的规范 JSON 摘要。
    pub context_report_digest: ArtifactDigest,
    /// 提交正式报告的 Audit 记录。
    pub audit_record_id: AuditRecordId,
    /// 提交完成后的 Audit 链头摘要。
    pub audit_head_digest: ArtifactDigest,
    /// 受信 Context Fixture 版本。
    pub fixture_version: DatasetVersionId,
    /// 固定 Context Gate 产生的八指标报告。
    pub context_report: ContextPolicyEvaluationReportV1,
    /// 正式报告提交时的生命周期。
    pub lifecycle: EvolutionLifecycle,
}

impl ContextEvaluationReceiptV1 {
    /// 校验回执版本、请求绑定、Context Report 摘要和 Gate 生命周期一致性。
    ///
    /// `expected_gate_version` 来自编译进受信 Evaluator/Evolver 的固定协议版本。
    ///
    /// # Errors
    ///
    /// Schema、请求 ID、Context Report、摘要或生命周期不一致时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self, expected_gate_version: &str) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION {
            return Err(
                InvalidEvaluatorIpc::UnsupportedContextEvaluationReceiptSchema {
                    found: self.schema_version,
                    supported: CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION,
                },
            );
        }
        validate_request_id(&self.request_id)?;
        self.context_report
            .validate(expected_gate_version)
            .map_err(|error| InvalidEvaluatorIpc::InvalidContextReport(error.to_string()))?;
        let bytes = serde_json::to_vec(&self.context_report)
            .map_err(|error| InvalidEvaluatorIpc::InvalidContextReport(error.to_string()))?;
        let actual = ArtifactDigest::from_sha256_hex(format!("{:x}", sha2::Sha256::digest(bytes)))
            .map_err(|error| InvalidEvaluatorIpc::InvalidContextReport(error.to_string()))?;
        if actual != self.context_report_digest {
            return Err(InvalidEvaluatorIpc::ContextReportDigestMismatch);
        }
        let expected_lifecycle = match self.context_report.decision {
            GateDecision::Pass => EvolutionLifecycle::Eligible,
            GateDecision::Reject => EvolutionLifecycle::Rejected,
            GateDecision::RequireApproval | GateDecision::Unknown => {
                return Err(InvalidEvaluatorIpc::InvalidContextReportDecision)
            }
        };
        if self.lifecycle != expected_lifecycle {
            return Err(InvalidEvaluatorIpc::InconsistentContextLifecycle);
        }
        Ok(())
    }
}

/// Promotion 后健康验证的版本化脱敏回执。
///
/// 回执绑定受信 Runtime 观察摘要和 Evaluator 重新读取的 Stable 状态；`verified` 只有在
/// Stable、Genome、代数与全部健康检查同时通过时才能为 `true`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckReceiptV1 {
    /// 回执 schema 版本。
    pub schema_version: u32,
    /// 对应请求的幂等 ID。
    pub request_id: String,
    /// 被验证的 Promotion Release。
    pub release_id: ReleaseId,
    /// Stable lineage。
    pub lineage: String,
    /// 请求期望的新 Genome Revision。
    pub expected_revision_id: GenomeRevisionId,
    /// Evaluator 从 Stable Store 观察到的 Revision。
    pub observed_revision_id: GenomeRevisionId,
    /// 请求期望的 Stable 代数。
    pub expected_generation: u64,
    /// Evaluator 从 Stable Store 观察到的代数。
    pub observed_generation: u64,
    /// 通过的受信健康检查数。
    pub checks_passed: u32,
    /// 执行的受信健康检查总数。
    pub checks_total: u32,
    /// Runtime 健康观察规范字节的摘要。
    pub observation_digest: ArtifactDigest,
    /// Stable 引用是否仍绑定当前 Release、预期 Revision 与代数。
    pub stable_reference_verified: bool,
    /// 可信健康验证的最终结论。
    pub verified: bool,
}

impl HealthCheckReceiptV1 {
    /// 校验回执结构和最终结论的一致性。
    ///
    /// # Errors
    ///
    /// Schema 未知、请求 ID 或 lineage 非法、健康计数越界，或 `verified` 与可复核字段不一致
    /// 时返回 [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != HEALTH_RECEIPT_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedHealthReceiptSchema {
                found: self.schema_version,
                supported: HEALTH_RECEIPT_SCHEMA_VERSION,
            });
        }
        validate_request_id(&self.request_id)?;
        validate_lineage(&self.lineage)?;
        validate_health_counts(self.checks_passed, self.checks_total)?;
        let derived = self.stable_reference_verified
            && self.observed_revision_id == self.expected_revision_id
            && self.observed_generation == self.expected_generation
            && self.checks_passed == self.checks_total;
        if self.verified != derived {
            return Err(InvalidEvaluatorIpc::InconsistentHealthVerdict);
        }
        Ok(())
    }
}

/// Promotion 或 Rollback 成功后的版本化脱敏回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReceiptV1 {
    /// 回执 schema 版本。
    pub schema_version: u32,
    /// 本次 Stable 切换的 Release ID。
    pub release_id: ReleaseId,
    /// Promotion 报告；Rollback 使用被撤销发布原先的报告。
    pub report_id: EvaluationReportId,
    /// Stable lineage。
    pub lineage: String,
    /// 切换前 Revision。
    pub from: GenomeRevisionId,
    /// 切换后 Revision。
    pub to: GenomeRevisionId,
    /// 切换后的单调代数。
    pub generation: u64,
    /// 记录最终控制面事件的 Audit ID。
    pub audit_record_id: AuditRecordId,
    /// Rollback 时为被撤销的 Promotion Release。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<ReleaseId>,
}

impl ReleaseReceiptV1 {
    /// 校验回执的版本、lineage、Revision 切换和 Rollback 绑定。
    ///
    /// # Errors
    ///
    /// Schema 未知、lineage 非法、切换前后 Revision 相同，或 Rollback 引用自身时返回
    /// [`InvalidEvaluatorIpc`]。
    pub fn validate(&self) -> Result<(), InvalidEvaluatorIpc> {
        if self.schema_version != RELEASE_RECEIPT_SCHEMA_VERSION {
            return Err(InvalidEvaluatorIpc::UnsupportedReleaseReceiptSchema {
                found: self.schema_version,
                supported: RELEASE_RECEIPT_SCHEMA_VERSION,
            });
        }
        validate_lineage(&self.lineage)?;
        if self.from == self.to {
            return Err(InvalidEvaluatorIpc::SameRevision);
        }
        if self.rollback_of.as_ref() == Some(&self.release_id) {
            return Err(InvalidEvaluatorIpc::SameRelease);
        }
        Ok(())
    }
}

/// Evaluator IPC 结构校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEvaluatorIpc {
    /// 请求 schema 不受支持。
    #[error("不支持的 Evaluator 请求 schema 版本 {found}，当前支持 {supported}")]
    UnsupportedRequestSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Evaluation Receipt schema 不受支持。
    #[error("不支持的 Evaluation Receipt schema 版本 {found}，当前支持 {supported}")]
    UnsupportedEvaluationReceiptSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Context Evaluation Receipt schema 不受支持。
    #[error("不支持的 Context Evaluation Receipt schema 版本 {found}，当前支持 {supported}")]
    UnsupportedContextEvaluationReceiptSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Skill Evaluation 请求或回执 schema 不受支持。
    #[error("不支持的 Skill Evaluation IPC schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSkillEvaluationSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Skill Health 请求或回执 schema 不受支持。
    #[error("不支持的 Skill Health IPC schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSkillHealthSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Health Receipt 使用了当前实现无法解释的版本。
    #[error("不支持的 Health Receipt schema 版本 {found}，当前支持 {supported}")]
    UnsupportedHealthReceiptSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// Release Receipt schema 不受支持。
    #[error("不支持的 Release Receipt schema 版本 {found}，当前支持 {supported}")]
    UnsupportedReleaseReceiptSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// 请求 ID 不符合稳定短标识规则。
    #[error("Evaluator request_id 不合法")]
    InvalidRequestId,
    /// Stable lineage 不符合安全名称规则。
    #[error("Evaluator lineage 不合法")]
    InvalidLineage,
    /// Parent 与 Candidate 或 Release 切换前后使用同一 Revision。
    #[error("Evaluator IPC 的两个 Revision 不能相同")]
    SameRevision,
    /// Rollback 与原 Promotion 使用同一 Release ID，或回执引用自身。
    #[error("Evaluator IPC 的 Promotion 与 Rollback Release ID 不能相同")]
    SameRelease,
    /// 健康检查总数为零或通过数超过总数。
    #[error("健康检查计数无效：passed={passed}，total={total}")]
    InvalidHealthCounts {
        /// 通过数。
        passed: u32,
        /// 总数。
        total: u32,
    },
    /// Health Receipt 的最终结论与可复核字段不一致。
    #[error("Health Receipt 的 verified 与 Stable 和检查结果不一致")]
    InconsistentHealthVerdict,
    /// Context Gate Report 的协议结构无效。
    #[error("Context Evaluation Receipt 中的 Gate Report 无效：{0}")]
    InvalidContextReport(String),
    /// Context Gate Report 摘要与规范 JSON 不一致。
    #[error("Context Evaluation Receipt 的 Gate Report 摘要不一致")]
    ContextReportDigestMismatch,
    /// Context 自动 Gate 出现了不支持的中间决策。
    #[error("Context Evaluation Receipt 只允许 Pass 或 Reject")]
    InvalidContextReportDecision,
    /// Context Gate 决策与正式报告生命周期不一致。
    #[error("Context Evaluation Receipt 的 Gate 决策与生命周期不一致")]
    InconsistentContextLifecycle,
    /// Skill Candidate 快照不是固定媒体类型或引用为空。
    #[error("Skill Candidate Artifact 引用无效")]
    InvalidSkillCandidateArtifact,
    /// Skill Evaluation 的 Evaluated/Active 时间顺序无效。
    #[error("Skill Evaluation 的 Q→E→A 时间顺序无效")]
    InvalidSkillEvaluationTime,
    /// Skill IPC 携带的协议主体无效。
    #[error("Skill IPC 主体无效：{0}")]
    InvalidSkillPayload(String),
    /// Skill Promotion 回执内部 Candidate 与外层身份不一致。
    #[error("Skill Evaluation Receipt 的 Candidate 身份错绑")]
    SkillReceiptCandidateMismatch,
    /// Stable 代数不能为零。
    #[error("Evaluator IPC 的 Stable 代数无效")]
    InvalidGeneration,
    /// Skill 健康结论缺少稳定证据 ID 或失败码。
    #[error("Skill Health 回执结论无效")]
    InvalidSkillHealthVerdict,
    /// 稳定版本或摘要字段为空或超过字节上限。
    #[error("Evaluator IPC 字段 `{field}` 必须是非空且不超过 {max_bytes} 字节的文本")]
    InvalidText {
        /// 字段名。
        field: &'static str,
        /// 最大字节数。
        max_bytes: usize,
    },
}

/// 校验全部 Evaluator 请求共用的 schema 版本。
fn validate_request_schema(schema_version: u32) -> Result<(), InvalidEvaluatorIpc> {
    if schema_version != EVALUATION_REQUEST_SCHEMA_VERSION {
        return Err(InvalidEvaluatorIpc::UnsupportedRequestSchema {
            found: schema_version,
            supported: EVALUATION_REQUEST_SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// 校验健康检查至少执行一项，且通过数不超过总数。
fn validate_health_counts(passed: u32, total: u32) -> Result<(), InvalidEvaluatorIpc> {
    if total == 0 || passed > total {
        return Err(InvalidEvaluatorIpc::InvalidHealthCounts { passed, total });
    }
    Ok(())
}

/// 校验请求幂等 ID，不接受路径分隔符或用户自由文本。
fn validate_request_id(value: &str) -> Result<(), InvalidEvaluatorIpc> {
    if value.is_empty()
        || value.len() > MAX_REQUEST_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(InvalidEvaluatorIpc::InvalidRequestId);
    }
    Ok(())
}

/// 校验 Stable lineage，拒绝路径逃逸和不稳定字符。
fn validate_lineage(value: &str) -> Result<(), InvalidEvaluatorIpc> {
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
        return Err(InvalidEvaluatorIpc::InvalidLineage);
    }
    Ok(())
}

/// 校验 Artifact 引用具有有限媒体类型和非零长度。
fn validate_artifact_ref(value: &ArtifactRef) -> Result<(), InvalidEvaluatorIpc> {
    if value.size_bytes == 0
        || value.media_type.trim().is_empty()
        || value.media_type.len() > MAX_POLICY_VERSION_BYTES
        || value.media_type.chars().any(char::is_control)
    {
        return Err(InvalidEvaluatorIpc::InvalidSkillCandidateArtifact);
    }
    Ok(())
}

/// 校验控制面证据短标识，禁止路径和用户正文。
fn valid_control_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// 校验不包含用户内容的有界稳定文本。
fn validate_bounded_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), InvalidEvaluatorIpc> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(InvalidEvaluatorIpc::InvalidText { field, max_bytes });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContextEvaluationMetricsV1, CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION,
        M6_CONTEXT_GATE_VERSION,
    };

    /// 生成固定 CAS 摘要。
    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要合法")
    }

    /// 构造合法 Evaluation Receipt。
    fn evaluation_receipt() -> EvaluationReceiptV1 {
        EvaluationReceiptV1 {
            schema_version: EVALUATION_RECEIPT_SCHEMA_VERSION,
            request_id: "cycle-001-candidate-01".to_string(),
            report_id: EvaluationReportId::generate(),
            report_digest: digest('a'),
            audit_record_id: AuditRecordId::generate(),
            audit_head_digest: digest('b'),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            evaluation_policy_version: "evaluation-policy-v1".to_string(),
            commit_policy_version: "commit-policy-v1".to_string(),
            verifier_set_digest: digest('c').to_string(),
            gate_decision: GateDecision::Pass,
            lifecycle: EvolutionLifecycle::Eligible,
        }
    }

    /// 构造通过固定 M6 Gate 的 Context 聚合报告。
    fn context_report() -> ContextPolicyEvaluationReportV1 {
        ContextPolicyEvaluationReportV1 {
            schema_version: CONTEXT_EVALUATION_REPORT_SCHEMA_VERSION,
            gate_version: M6_CONTEXT_GATE_VERSION.to_string(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            parent_metrics: ContextEvaluationMetricsV1::default(),
            candidate_metrics: ContextEvaluationMetricsV1::default(),
            decision: GateDecision::Pass,
            failures: Default::default(),
        }
    }

    /// 构造摘要和生命周期均与 Context Gate 一致的专用回执。
    fn context_evaluation_receipt() -> ContextEvaluationReceiptV1 {
        let context_report = context_report();
        let bytes = serde_json::to_vec(&context_report).expect("Context 报告应可序列化");
        let context_report_digest =
            ArtifactDigest::from_sha256_hex(format!("{:x}", sha2::Sha256::digest(bytes)))
                .expect("Context 报告摘要应合法");
        ContextEvaluationReceiptV1 {
            schema_version: CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION,
            request_id: "context-cycle-001-candidate-01".to_string(),
            report_id: EvaluationReportId::generate(),
            report_digest: digest('d'),
            context_report_digest,
            audit_record_id: AuditRecordId::generate(),
            audit_head_digest: digest('e'),
            fixture_version: DatasetVersionId::generate(),
            context_report,
            lifecycle: EvolutionLifecycle::Eligible,
        }
    }

    /// Skill 评测请求只允许强类型身份、Candidate CAS 和时间前置条件。
    #[test]
    fn skill_evaluation_request_is_strict_and_identity_bound() {
        let request = SkillEvaluationRequestV1 {
            schema_version: SKILL_EVALUATION_IPC_SCHEMA_VERSION,
            request_id: "skill-evaluate-candidate-01".into(),
            candidate_id: CandidateId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            candidate_genome_digest: GenomeDigest::from_sha256_hex("9".repeat(64))
                .expect("测试 Genome 摘要应合法"),
            candidate_artifact: ArtifactRef {
                digest: digest('8'),
                media_type: SKILL_CANDIDATE_SNAPSHOT_MEDIA_TYPE.into(),
                size_bytes: 128,
            },
            evaluated_at_ms: 10,
            activated_at_ms: 20,
        };
        request.validate().expect("Skill 评测请求应合法");
        let encoded = serde_json::to_value(&request).expect("请求应可序列化");
        assert_eq!(
            serde_json::from_value::<SkillEvaluationRequestV1>(encoded.clone())
                .expect("请求应可反序列化"),
            request
        );
        for field in [
            "observations",
            "trusted_usage_bindings",
            "authorization",
            "registry_root",
            "episode_store_root",
            "gate_policy",
        ] {
            let mut unknown = encoded.clone();
            unknown[field] = serde_json::json!("candidate-controlled");
            assert!(serde_json::from_value::<SkillEvaluationRequestV1>(unknown).is_err());
        }
        let mut invalid = request;
        invalid.candidate_artifact.media_type = "application/json".into();
        assert_eq!(
            invalid.validate(),
            Err(InvalidEvaluatorIpc::InvalidSkillCandidateArtifact)
        );
    }

    /// Skill 健康请求不得携带健康结论、Store 路径或回滚操作。
    #[test]
    fn skill_health_request_rejects_control_plane_fields() {
        let request = SkillHealthRequestV1 {
            schema_version: SKILL_HEALTH_IPC_SCHEMA_VERSION,
            request_id: "skill-health-release-01".into(),
            release_id: ReleaseId::generate(),
            lineage: "production".into(),
            expected_revision_id: GenomeRevisionId::generate(),
            expected_generation: 2,
        };
        request.validate().expect("Skill 健康请求应合法");
        let encoded = serde_json::to_value(request).expect("请求应可序列化");
        for field in ["result", "health_store_root", "stable_path", "rollback"] {
            let mut unknown = encoded.clone();
            unknown[field] = serde_json::json!("candidate-controlled");
            assert!(serde_json::from_value::<SkillHealthRequestV1>(unknown).is_err());
        }
    }

    /// Evaluate 请求与回执必须稳定往返并拒绝未知字段。
    #[test]
    fn evaluation_ipc_round_trips_strictly() {
        let request = EvaluationRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "cycle-001-candidate-01".to_string(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            lineage: "stable/general".to_string(),
            expected_parent_generation: 1,
            expected_dataset_version: DatasetVersionId::generate(),
        };
        request.validate().expect("Evaluate 请求应合法");
        let encoded = serde_json::to_value(&request).expect("请求应可序列化");
        assert_eq!(
            serde_json::from_value::<EvaluationRequestV1>(encoded.clone())
                .expect("请求应可反序列化"),
            request
        );
        let mut unknown = encoded;
        unknown["commit_policy"] = serde_json::json!("candidate-controlled");
        assert!(serde_json::from_value::<EvaluationRequestV1>(unknown).is_err());

        let receipt = evaluation_receipt();
        receipt.validate().expect("Evaluation Receipt 应合法");
        let mut unknown = serde_json::to_value(receipt).expect("回执应可序列化");
        unknown["hidden_results"] = serde_json::json!([]);
        assert!(serde_json::from_value::<EvaluationReceiptV1>(unknown).is_err());
    }

    /// Context Evaluate 请求必须稳定往返，并拒绝 Fixture、Gate 与 Archive 控制面字段。
    #[test]
    fn context_evaluation_request_rejects_control_plane_fields() {
        let request = ContextEvaluationRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "context-cycle-001-candidate-01".to_string(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            lineage: "stable/general".to_string(),
            expected_parent_generation: 1,
            expected_fixture_version: DatasetVersionId::generate(),
        };
        request.validate().expect("Context Evaluate 请求应合法");
        let encoded = serde_json::to_value(&request).expect("请求应可序列化");
        assert_eq!(
            serde_json::from_value::<ContextEvaluationRequestV1>(encoded.clone())
                .expect("请求应可反序列化"),
            request
        );

        for field in [
            "fixture_root",
            "observations",
            "gate_version",
            "gate_policy",
            "archive_root",
        ] {
            let mut unknown = encoded.clone();
            unknown[field] = serde_json::json!("candidate-controlled");
            assert!(serde_json::from_value::<ContextEvaluationRequestV1>(unknown).is_err());
        }
    }

    /// Context 回执必须拒绝额外私有字段、摘要篡改和 Gate 生命周期错绑。
    #[test]
    fn context_evaluation_receipt_is_strict_and_bound() {
        let receipt = context_evaluation_receipt();
        receipt
            .validate(M6_CONTEXT_GATE_VERSION)
            .expect("Context Evaluation Receipt 应合法");

        let mut unknown = serde_json::to_value(&receipt).expect("回执应可序列化");
        unknown["private_observations"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ContextEvaluationReceiptV1>(unknown).is_err());

        let mut tampered_digest = receipt.clone();
        tampered_digest.context_report_digest = digest('f');
        assert_eq!(
            tampered_digest.validate(M6_CONTEXT_GATE_VERSION),
            Err(InvalidEvaluatorIpc::ContextReportDigestMismatch)
        );

        let mut tampered_lifecycle = receipt;
        tampered_lifecycle.lifecycle = EvolutionLifecycle::Rejected;
        assert_eq!(
            tampered_lifecycle.validate(M6_CONTEXT_GATE_VERSION),
            Err(InvalidEvaluatorIpc::InconsistentContextLifecycle)
        );
    }

    /// Promotion、Rollback 请求必须拒绝未知版本、自回滚和额外字段。
    #[test]
    fn release_requests_are_fail_closed() {
        let promotion = PromotionRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            release_id: ReleaseId::generate(),
        };
        promotion.validate().expect("Promotion 请求应合法");
        let mut unknown = serde_json::to_value(promotion).expect("请求应可序列化");
        unknown["stable_path"] = serde_json::json!("/tmp/stable");
        assert!(serde_json::from_value::<PromotionRequestV1>(unknown).is_err());

        let release = ReleaseId::generate();
        let rollback = RollbackRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            release_id: release.clone(),
            rollback_release_id: release,
        };
        assert_eq!(rollback.validate(), Err(InvalidEvaluatorIpc::SameRelease));
    }

    /// Health 请求、Runtime 观察和回执必须严格绑定且不能自报不一致的成功结论。
    #[test]
    fn health_ipc_is_strict_and_fail_closed() {
        let release_id = ReleaseId::generate();
        let revision_id = GenomeRevisionId::generate();
        let request = HealthCheckRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "cycle-001-health".to_string(),
            release_id: release_id.clone(),
            lineage: "stable/general".to_string(),
            expected_revision_id: revision_id.clone(),
            expected_generation: 2,
        };
        request.validate().expect("Health 请求应合法");
        let mut unknown = serde_json::to_value(request).expect("请求应可序列化");
        unknown["health_store"] = serde_json::json!("/secret");
        assert!(serde_json::from_value::<HealthCheckRequestV1>(unknown).is_err());

        let observation = RuntimeHealthObservationV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            release_id: release_id.clone(),
            observed_revision_id: revision_id.clone(),
            checks_passed: 2,
            checks_total: 2,
            observed_at_ms: 10,
        };
        observation.validate().expect("Runtime 观察应合法");
        let mut invalid_observation = observation;
        invalid_observation.checks_total = 0;
        assert!(matches!(
            invalid_observation.validate(),
            Err(InvalidEvaluatorIpc::InvalidHealthCounts { .. })
        ));

        let mut receipt = HealthCheckReceiptV1 {
            schema_version: HEALTH_RECEIPT_SCHEMA_VERSION,
            request_id: "cycle-001-health".to_string(),
            release_id,
            lineage: "stable/general".to_string(),
            expected_revision_id: revision_id.clone(),
            observed_revision_id: revision_id,
            expected_generation: 2,
            observed_generation: 2,
            checks_passed: 2,
            checks_total: 2,
            observation_digest: ArtifactDigest::from_sha256_hex("a".repeat(64))
                .expect("摘要应合法"),
            stable_reference_verified: true,
            verified: true,
        };
        receipt.validate().expect("Health 回执应合法");
        receipt.checks_passed = 1;
        assert_eq!(
            receipt.validate(),
            Err(InvalidEvaluatorIpc::InconsistentHealthVerdict)
        );
    }

    /// Release Receipt 必须版本化、严格反序列化并证明 Revision 确实发生切换。
    #[test]
    fn release_receipt_is_versioned_and_strict() {
        let receipt = ReleaseReceiptV1 {
            schema_version: RELEASE_RECEIPT_SCHEMA_VERSION,
            release_id: ReleaseId::generate(),
            report_id: EvaluationReportId::generate(),
            lineage: "stable/general".to_string(),
            from: GenomeRevisionId::generate(),
            to: GenomeRevisionId::generate(),
            generation: 2,
            audit_record_id: AuditRecordId::generate(),
            rollback_of: None,
        };
        receipt.validate().expect("Release Receipt 应合法");
        let mut unknown = serde_json::to_value(receipt.clone()).expect("回执应可序列化");
        unknown["audit_path"] = serde_json::json!("/secret");
        assert!(serde_json::from_value::<ReleaseReceiptV1>(unknown).is_err());

        let mut invalid = receipt;
        invalid.schema_version = 2;
        assert!(matches!(
            invalid.validate(),
            Err(InvalidEvaluatorIpc::UnsupportedReleaseReceiptSchema { .. })
        ));
    }
}
