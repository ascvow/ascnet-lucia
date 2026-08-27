//! Promotion 后 Runtime 健康观察的安全文件 Store 与受信复核器。

use crate::{AuditEvent, AuditStoreError, TrustedEvaluationArchive};
use agent_evolution::{FileGenomeResolver, GenomeResolver, GenomeResolverError, GenomeSelector};
pub use agent_evolution::{
    FileRuntimeHealthObservationStore, RuntimeHealthStoreError, VerifiedRuntimeHealthObservation,
    MAX_RUNTIME_HEALTH_OBSERVATION_BYTES,
};
use agent_evolution_protocol::{
    HealthCheckReceiptV1, HealthCheckRequestV1, InvalidEvaluatorIpc, ReleaseId,
    HEALTH_RECEIPT_SCHEMA_VERSION,
};
use std::path::PathBuf;

/// 复核 Promotion Audit、Stable 引用和真实 Runtime 观察的受信健康验证器。
#[derive(Debug, Clone)]
pub struct ReleaseHealthVerifier {
    resolver: FileGenomeResolver,
    archive: TrustedEvaluationArchive,
    observations: FileRuntimeHealthObservationStore,
}

impl ReleaseHealthVerifier {
    /// 使用 Evolution Registry、Evaluation Archive 与受信观察 Store 创建验证器。
    pub fn new(
        evolution_root: impl Into<PathBuf>,
        archive_root: impl Into<PathBuf>,
        observations: FileRuntimeHealthObservationStore,
    ) -> Self {
        Self {
            resolver: FileGenomeResolver::new(evolution_root),
            archive: TrustedEvaluationArchive::new(archive_root),
            observations,
        }
    }

    /// 验证一次 Promotion 后的 Stable 与 Runtime 健康状态并生成脱敏回执。
    ///
    /// Promotion Audit 必须唯一且与请求的 Release、lineage、Candidate 和代数完全一致；缺失
    /// 或冲突表示请求/归档不可信并直接报错。Stable 已变化、Runtime 使用错误 Revision 或健康
    /// 检查未全部通过属于可回滚的健康失败，返回 `verified = false`。
    ///
    /// # Errors
    ///
    /// 请求、Audit、Stable Registry 或观察 Store 无法可信验证时返回
    /// [`ReleaseHealthVerificationError`]。
    pub async fn verify(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, ReleaseHealthVerificationError> {
        request
            .validate()
            .map_err(ReleaseHealthVerificationError::InvalidRequest)?;
        let records = self.archive.audit_log().records().await?;
        let promotions = records
            .iter()
            .filter_map(|record| match &record.event {
                AuditEvent::PromotionCommitted {
                    release_id,
                    report_id,
                    lineage,
                    candidate,
                    generation,
                    ..
                } if release_id == &request.release_id => {
                    Some((report_id, lineage.as_str(), candidate, *generation))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if promotions.is_empty() {
            return Err(ReleaseHealthVerificationError::PromotionAuditNotFound(
                request.release_id.clone(),
            ));
        }
        if promotions.len() != 1 {
            return Err(ReleaseHealthVerificationError::PromotionAuditConflict(
                request.release_id.clone(),
            ));
        }
        let (report_id, audit_lineage, audit_candidate, audit_generation) = promotions[0];
        if audit_lineage != request.lineage
            || audit_candidate != &request.expected_revision_id
            || audit_generation != request.expected_generation
        {
            return Err(ReleaseHealthVerificationError::PromotionAuditMismatch(
                request.release_id.clone(),
            ));
        }

        let stable = self.resolver.stable_reference(&request.lineage).await?;
        let stable_revision = self
            .resolver
            .resolve(&GenomeSelector::Stable(request.lineage.clone()))
            .await?;
        let observation = self.observations.load(&request.release_id).await?;
        let observed = observation.observation();
        let stable_reference_verified = stable.lineage == request.lineage
            && stable.revision_id == request.expected_revision_id
            && stable_revision.revision_id == request.expected_revision_id
            && stable.generation == request.expected_generation
            && stable.release_id.as_ref() == Some(&request.release_id)
            && stable.evaluation_report_id.as_ref() == Some(report_id)
            && stable.rollback_of.is_none();
        let verified = stable_reference_verified
            && observed.observed_revision_id == request.expected_revision_id
            && observed.checks_passed == observed.checks_total;
        let receipt = HealthCheckReceiptV1 {
            schema_version: HEALTH_RECEIPT_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            release_id: request.release_id.clone(),
            lineage: request.lineage.clone(),
            expected_revision_id: request.expected_revision_id.clone(),
            observed_revision_id: observed.observed_revision_id.clone(),
            expected_generation: request.expected_generation,
            observed_generation: stable.generation,
            checks_passed: observed.checks_passed,
            checks_total: observed.checks_total,
            observation_digest: observation.digest().clone(),
            stable_reference_verified,
            verified,
        };
        receipt
            .validate()
            .map_err(ReleaseHealthVerificationError::InvalidReceipt)?;
        Ok(receipt)
    }
}

/// Promotion 后健康验证错误。
#[derive(Debug, thiserror::Error)]
pub enum ReleaseHealthVerificationError {
    /// 共享 Health 请求无效。
    #[error("Health 请求无效：{0}")]
    InvalidRequest(InvalidEvaluatorIpc),
    /// 指定 Release 没有 Promotion Audit。
    #[error("Promotion Audit 不存在：{0}")]
    PromotionAuditNotFound(ReleaseId),
    /// 同一 Release 出现多个 Promotion Audit。
    #[error("Promotion Audit 冲突：{0}")]
    PromotionAuditConflict(ReleaseId),
    /// Promotion Audit 与请求声明不一致。
    #[error("Promotion Audit 与 Health 请求不一致：{0}")]
    PromotionAuditMismatch(ReleaseId),
    /// 构造的共享 Health 回执不一致。
    #[error("Health 回执无效：{0}")]
    InvalidReceipt(InvalidEvaluatorIpc),
    /// Audit 链无法可信验证。
    #[error(transparent)]
    Audit(#[from] AuditStoreError),
    /// Stable Registry 无法可信读取。
    #[error(transparent)]
    Resolver(#[from] GenomeResolverError),
    /// Runtime 观察无法可信读取。
    #[error(transparent)]
    Observation(#[from] RuntimeHealthStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuditEvent;
    use agent_evolution::{FileStableGenomePublisher, GenomeStore};
    use agent_evolution_protocol::{
        AgentGenome, EvaluationReportId, GenomeMetadata, GenomeRevision, ModelGenome, PromptGenome,
        ReleaseId, RuntimeHealthObservationV1, RuntimeIdentity, ToolProfileGenome,
        EVALUATION_REQUEST_SCHEMA_VERSION, GENOME_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    };
    use tempfile::TempDir;

    /// 构造健康验证所需的最小合法 Genome Revision。
    fn revision(marker: &str) -> GenomeRevision {
        GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".to_string(),
                    git_commit: marker.to_string(),
                    git_dirty: false,
                    target_triple: "test-target".to_string(),
                    features: BTreeSet::new(),
                },
                model: ModelGenome {
                    provider: "fixture".to_string(),
                    provider_kind: "fixture".to_string(),
                    model: "fixture-model".to_string(),
                    base_url: None,
                    protocol: None,
                    max_tokens: Some(64),
                    temperature: None,
                    stream: false,
                    provider_options_digest: None,
                },
                prompt: PromptGenome {
                    messages: Vec::new(),
                },
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("测试 Genome 应合法")
    }

    /// 创建已绑定 Promotion Audit 和 Stable Candidate 的健康测试环境。
    async fn promoted_fixture(
        root: &Path,
    ) -> (
        PathBuf,
        PathBuf,
        FileRuntimeHealthObservationStore,
        HealthCheckRequestV1,
    ) {
        let evolution_root = root.join("evolution");
        let archive_root = root.join("archive");
        let health_root = root.join("health");
        let publisher = FileStableGenomePublisher::new(&evolution_root);
        let parent = revision("parent");
        let candidate = revision("candidate");
        publisher
            .resolver()
            .store()
            .append(&parent)
            .await
            .expect("登记 Parent");
        publisher
            .resolver()
            .store()
            .append(&candidate)
            .await
            .expect("登记 Candidate");
        let initial = publisher
            .publish("stable/test", &parent, 1)
            .await
            .expect("初始化 Stable");
        let release_id = ReleaseId::generate();
        let report_id = EvaluationReportId::generate();
        publisher
            .publish_bound(
                &initial,
                &candidate,
                2,
                release_id.clone(),
                report_id.clone(),
                None,
            )
            .await
            .expect("发布 Candidate");
        TrustedEvaluationArchive::new(&archive_root)
            .audit_log()
            .append(
                2,
                AuditEvent::PromotionCommitted {
                    release_id: release_id.clone(),
                    report_id,
                    lineage: "stable/test".to_string(),
                    parent: parent.revision_id,
                    candidate: candidate.revision_id.clone(),
                    generation: 2,
                },
            )
            .await
            .expect("记录 Promotion Audit");
        let observations =
            FileRuntimeHealthObservationStore::new(health_root).expect("健康根是绝对路径");
        let request = HealthCheckRequestV1 {
            schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
            request_id: "health-request-001".to_string(),
            release_id,
            lineage: "stable/test".to_string(),
            expected_revision_id: candidate.revision_id,
            expected_generation: 2,
        };
        (evolution_root, archive_root, observations, request)
    }

    /// Promotion Audit、Stable、Runtime Revision 和检查计数全部匹配时才允许健康通过。
    #[tokio::test]
    async fn verifier_requires_promotion_stable_and_runtime_observation_binding() {
        let root = TempDir::new().expect("创建临时根");
        let (evolution_root, archive_root, observations, request) =
            promoted_fixture(root.path()).await;
        observations
            .put(&RuntimeHealthObservationV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: request.release_id.clone(),
                observed_revision_id: request.expected_revision_id.clone(),
                checks_passed: 3,
                checks_total: 3,
                observed_at_ms: 10,
            })
            .await
            .expect("写入真实观察");
        let receipt = ReleaseHealthVerifier::new(evolution_root, archive_root, observations)
            .verify(&request)
            .await
            .expect("健康验证应完成");

        assert!(receipt.stable_reference_verified);
        assert!(receipt.verified);
        assert_eq!(receipt.checks_passed, 3);
        assert_eq!(receipt.checks_total, 3);
        receipt.validate().expect("回执必须满足共享协议");
    }

    /// Runtime 使用错误 Revision 时必须返回可触发回滚的失败回执。
    #[tokio::test]
    async fn verifier_returns_failed_receipt_for_wrong_runtime_revision() {
        let root = TempDir::new().expect("创建临时根");
        let (evolution_root, archive_root, observations, request) =
            promoted_fixture(root.path()).await;
        let wrong_revision = agent_evolution_protocol::GenomeRevisionId::generate();
        observations
            .put(&RuntimeHealthObservationV1 {
                schema_version: EVALUATION_REQUEST_SCHEMA_VERSION,
                release_id: request.release_id.clone(),
                observed_revision_id: wrong_revision.clone(),
                checks_passed: 2,
                checks_total: 2,
                observed_at_ms: 10,
            })
            .await
            .expect("写入错误 Revision 观察");
        let receipt = ReleaseHealthVerifier::new(evolution_root, archive_root, observations)
            .verify(&request)
            .await
            .expect("行为失败仍应产生回执");

        assert!(receipt.stable_reference_verified);
        assert!(!receipt.verified);
        assert_eq!(receipt.observed_revision_id, wrong_revision);
        receipt.validate().expect("失败回执必须满足共享协议");
    }

    /// 请求与 Promotion Audit 的 Candidate 不一致时必须失败关闭，不能返回健康结论。
    #[tokio::test]
    async fn verifier_rejects_request_not_bound_to_promotion_audit() {
        let root = TempDir::new().expect("创建临时根");
        let (evolution_root, archive_root, observations, mut request) =
            promoted_fixture(root.path()).await;
        request.expected_generation = 3;
        let error = ReleaseHealthVerifier::new(evolution_root, archive_root, observations)
            .verify(&request)
            .await
            .expect_err("错误代数不得产生回执");
        assert!(matches!(
            error,
            ReleaseHealthVerificationError::PromotionAuditMismatch(_)
        ));
    }
}
