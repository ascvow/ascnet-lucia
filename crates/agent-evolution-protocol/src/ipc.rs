//! 独立 Evaluator 与 Evolver 之间的版本化 IPC 协议。
//!
//! 请求只允许声明强类型身份和并发前置条件。Dataset 路径、Hidden Verifier、Commit
//! Policy、Release Store 与权限上限均由受信 `lucia-eval` 进程配置，不能通过 IPC 覆盖。

use crate::{
    ArtifactDigest, AuditRecordId, DatasetVersionId, EvaluationReportId, EvolutionLifecycle,
    GateDecision, GenomeRevisionId, ReleaseId,
};
use serde::{Deserialize, Serialize};

/// 当前 Evaluator 请求协议版本；Evaluate、Promote 与 Rollback 共用该严格版本。
pub const EVALUATION_REQUEST_SCHEMA_VERSION: u32 = 1;
/// 当前 Evaluation Receipt 协议版本。
pub const EVALUATION_RECEIPT_SCHEMA_VERSION: u32 = 1;
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
