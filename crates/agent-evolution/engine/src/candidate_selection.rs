//! 从独立 Evaluator 回执中确定性选择可晋升 Candidate。

use agent_evolution_protocol::{
    CandidateId, EvaluationReceiptV1, EvolutionLifecycle, GateDecision, MutationCandidate,
};
use std::collections::{BTreeMap, BTreeSet};

/// 已通过独立评测且可提交给 Release Controller 的胜者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedCandidate {
    /// Candidate 稳定标识。
    pub candidate_id: CandidateId,
    /// 已验证与 Candidate、Parent 绑定的正式 Evaluator 回执。
    pub receipt: EvaluationReceiptV1,
}

/// 只读取正式 Evaluator 回执的确定性 Candidate Selector。
#[derive(Debug, Clone, Copy, Default)]
pub struct CandidateSelector;

impl CandidateSelector {
    /// 从完整 Candidate/Receipt 集合中选择一个 `Pass + Eligible` 胜者。
    ///
    /// 每个 Candidate 必须恰好有一份身份匹配的回执；Reject、Unknown、RequireApproval 和非
    /// Eligible 生命周期都不会成为自动晋升胜者。若多个 Candidate 合格，按 Candidate ID
    /// 稳定升序选择，避免调用顺序影响结果。Gate 分数、自报成功状态或 Candidate 字段均不
    /// 参与判断。
    ///
    /// # Errors
    ///
    /// Candidate 无效、集合为空或跨 Parent、回执无效、错绑、重复、未知或缺失时返回
    /// [`CandidateSelectionError`]。
    pub fn select(
        candidates: &[MutationCandidate],
        receipts: &[EvaluationReceiptV1],
    ) -> Result<Option<SelectedCandidate>, CandidateSelectionError> {
        if candidates.is_empty() {
            return Err(CandidateSelectionError::MissingCandidates);
        }
        let parent = &candidates[0].parent_revision_id;
        let mut by_revision = BTreeMap::new();
        let mut candidate_ids = BTreeSet::new();
        for candidate in candidates {
            candidate
                .validate()
                .map_err(|error| CandidateSelectionError::InvalidCandidate(error.to_string()))?;
            if &candidate.parent_revision_id != parent {
                return Err(CandidateSelectionError::MixedParents);
            }
            if !candidate_ids.insert(candidate.candidate_id.clone())
                || by_revision
                    .insert(
                        candidate.candidate_revision_id.clone(),
                        candidate.candidate_id.clone(),
                    )
                    .is_some()
            {
                return Err(CandidateSelectionError::DuplicateCandidate);
            }
        }

        let mut receipt_by_candidate = BTreeMap::<CandidateId, EvaluationReceiptV1>::new();
        for receipt in receipts {
            receipt
                .validate()
                .map_err(|error| CandidateSelectionError::InvalidReceipt(error.to_string()))?;
            if &receipt.parent_revision_id != parent {
                return Err(CandidateSelectionError::ReceiptParentMismatch);
            }
            let Some(candidate_id) = by_revision.get(&receipt.candidate_revision_id) else {
                return Err(CandidateSelectionError::UnknownCandidateReceipt);
            };
            if receipt_by_candidate
                .insert(candidate_id.clone(), receipt.clone())
                .is_some()
            {
                return Err(CandidateSelectionError::DuplicateReceipt(
                    candidate_id.clone(),
                ));
            }
        }

        if receipt_by_candidate.len() != candidates.len() {
            let missing = candidate_ids
                .iter()
                .find(|candidate_id| !receipt_by_candidate.contains_key(*candidate_id))
                .cloned()
                .expect("数量不一致时必须存在缺失 Candidate");
            return Err(CandidateSelectionError::MissingReceipt(missing));
        }

        Ok(receipt_by_candidate
            .into_iter()
            .filter(|(_, receipt)| {
                receipt.gate_decision == GateDecision::Pass
                    && receipt.lifecycle == EvolutionLifecycle::Eligible
            })
            .map(|(candidate_id, receipt)| SelectedCandidate {
                candidate_id,
                receipt,
            })
            .next())
    }
}

/// Candidate Selection 的结构与身份绑定错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CandidateSelectionError {
    /// 没有可评测 Candidate。
    #[error("Candidate Selection 缺少 Candidate")]
    MissingCandidates,
    /// Candidate DTO 未通过协议校验。
    #[error("Candidate Selection 收到无效 Candidate：{0}")]
    InvalidCandidate(String),
    /// 一轮选择混入不同 Parent 的 Candidate。
    #[error("Candidate Selection 不能混用多个 Parent")]
    MixedParents,
    /// Candidate ID 或 Revision 重复。
    #[error("Candidate Selection 的 Candidate 身份重复")]
    DuplicateCandidate,
    /// Evaluator 回执结构无效。
    #[error("Candidate Selection 收到无效 Evaluator 回执：{0}")]
    InvalidReceipt(String),
    /// 回执的 Parent 与 Candidate 集合不一致。
    #[error("Evaluator 回执的 Parent 与 Candidate Selection 不匹配")]
    ReceiptParentMismatch,
    /// 回执引用本轮未构建的 Candidate Revision。
    #[error("Evaluator 回执引用未知 Candidate")]
    UnknownCandidateReceipt,
    /// 同一 Candidate 出现多份回执。
    #[error("Candidate 收到重复 Evaluator 回执：{0}")]
    DuplicateReceipt(CandidateId),
    /// Candidate 尚未取得正式回执，不能把不完整评测解释为 Reject。
    #[error("Candidate 缺少 Evaluator 回执：{0}")]
    MissingReceipt(CandidateId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        ArtifactDigest, ArtifactRef, AuditRecordId, EvaluationReportId, EvolutionCycleId,
        EvolutionIssueId, GenomeDigest, GenomeRevisionId, MutationCandidate, MutationId,
        MutationSurface, EVALUATION_RECEIPT_SCHEMA_VERSION, MUTATION_CANDIDATE_SCHEMA_VERSION,
    };

    /// 构造固定摘要。
    fn artifact_digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("摘要应合法")
    }

    /// 构造固定 Genome 摘要。
    fn genome_digest(seed: char) -> GenomeDigest {
        GenomeDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("摘要应合法")
    }

    /// 构造最小合法 Candidate。
    fn candidate(parent: &GenomeRevisionId, seed: char) -> MutationCandidate {
        MutationCandidate {
            schema_version: MUTATION_CANDIDATE_SCHEMA_VERSION,
            candidate_id: CandidateId::generate(),
            cycle_id: EvolutionCycleId::generate(),
            issue_id: EvolutionIssueId::generate(),
            mutation_id: MutationId::generate(),
            parent_revision_id: parent.clone(),
            parent_genome_digest: genome_digest('a'),
            candidate_revision_id: GenomeRevisionId::generate(),
            candidate_genome_digest: genome_digest(seed),
            prompt: ArtifactRef {
                digest: artifact_digest(seed),
                media_type: "text/plain".to_string(),
                size_bytes: 1,
            },
            changed_surfaces: BTreeSet::from([MutationSurface::TaskStrategyPrompt]),
            created_at_ms: 1,
        }
    }

    /// 构造与 Candidate 绑定的 Evaluator 回执。
    fn receipt(
        candidate: &MutationCandidate,
        decision: GateDecision,
        lifecycle: EvolutionLifecycle,
    ) -> EvaluationReceiptV1 {
        EvaluationReceiptV1 {
            schema_version: EVALUATION_RECEIPT_SCHEMA_VERSION,
            request_id: format!("eval-{}", candidate.candidate_id),
            report_id: EvaluationReportId::generate(),
            report_digest: artifact_digest('d'),
            audit_record_id: AuditRecordId::generate(),
            audit_head_digest: artifact_digest('e'),
            parent_revision_id: candidate.parent_revision_id.clone(),
            candidate_revision_id: candidate.candidate_revision_id.clone(),
            evaluation_policy_version: "evaluation-policy-v1".to_string(),
            commit_policy_version: "commit-policy-v1".to_string(),
            verifier_set_digest: "verifier-set-v1".to_string(),
            gate_decision: decision,
            lifecycle,
        }
    }

    /// 只有 Pass + Eligible 可以胜出，Reject 不能被候选自报状态抵消。
    #[test]
    fn selects_only_pass_and_eligible_receipt() {
        let parent = GenomeRevisionId::generate();
        let rejected = candidate(&parent, 'b');
        let eligible = candidate(&parent, 'c');
        let receipts = vec![
            receipt(
                &rejected,
                GateDecision::Reject,
                EvolutionLifecycle::Rejected,
            ),
            receipt(&eligible, GateDecision::Pass, EvolutionLifecycle::Eligible),
        ];
        let selected = CandidateSelector::select(&[rejected, eligible.clone()], &receipts)
            .expect("选择应成功")
            .expect("应有胜者");
        assert_eq!(selected.candidate_id, eligible.candidate_id);
        assert_eq!(selected.receipt.gate_decision, GateDecision::Pass);
    }

    /// 缺失回执属于不完整评测，不能静默当作无胜者。
    #[test]
    fn rejects_missing_or_unknown_receipt() {
        let parent = GenomeRevisionId::generate();
        let first = candidate(&parent, 'b');
        let second = candidate(&parent, 'c');
        assert!(matches!(
            CandidateSelector::select(
                &[first.clone(), second],
                &[receipt(
                    &first,
                    GateDecision::Reject,
                    EvolutionLifecycle::Rejected,
                )]
            ),
            Err(CandidateSelectionError::MissingReceipt(_))
        ));
    }
}
