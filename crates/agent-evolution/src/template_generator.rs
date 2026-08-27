//! 无外部模型依赖的确定性 Prompt Mutation Generator。

use crate::{
    PromptMutationDraft, PromptMutationGenerationError, PromptMutationGenerator,
    PromptMutationRequest,
};
use agent_evolution_protocol::ExpectedEffect;
use async_trait::async_trait;

/// 使用固定修复模板生成三个不同 Task Strategy Prompt 的离线生成器。
///
/// 该生成器只读取 [`crate::MutationEvidence`] 的脱敏结构字段，适合作为无 API Key 环境的
/// M5 默认实现。所有输出仍会由 [`crate::BoundedPromptMutator`] 强制数量、大小和唯一性。
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicPromptMutationGenerator;

#[async_trait]
impl PromptMutationGenerator for DeterministicPromptMutationGenerator {
    async fn generate(
        &self,
        request: PromptMutationRequest<'_>,
    ) -> Result<Vec<PromptMutationDraft>, PromptMutationGenerationError> {
        let task_family = request
            .evidence
            .episodes
            .iter()
            .map(|episode| episode.task_family.trim())
            .find(|family| !family.is_empty())
            .unwrap_or("general")
            .to_string();
        let expected_behavior = request.evidence.expected_behavior.trim();
        if expected_behavior.is_empty() {
            return Err(PromptMutationGenerationError::new(
                "missing_expected_behavior",
            ));
        }
        let strategies = [
            (
                "在执行前显式提取约束并建立验证标准",
                format!(
                    "{}\n\n补充策略：执行前列出与任务结果有关的约束和可验证完成条件；每一步都应服务于这些条件。目标行为：{}。",
                    request.parent_prompt, expected_behavior
                ),
            ),
            (
                "在工具调用后核验真实状态并根据错误分类恢复",
                format!(
                    "{}\n\n补充策略：工具调用后必须检查真实结果、错误类别和副作用；只有可恢复错误允许有界重试。目标行为：{}。",
                    request.parent_prompt, expected_behavior
                ),
            ),
            (
                "在结束前执行独立验收并保留失败证据",
                format!(
                    "{}\n\n补充策略：给出最终结果前执行与任务契约对应的独立验收；无法验证时明确失败或不可验证，不得自报成功。目标行为：{}。",
                    request.parent_prompt, expected_behavior
                ),
            ),
        ];
        if request.candidate_count != strategies.len() {
            return Err(PromptMutationGenerationError::new(
                "unsupported_candidate_count",
            ));
        }
        Ok(strategies
            .into_iter()
            .map(|(hypothesis, prompt)| PromptMutationDraft {
                hypothesis: hypothesis.to_string(),
                prompt,
                expected_effects: vec![ExpectedEffect {
                    task_family: task_family.clone(),
                    expected_behavior: expected_behavior.to_string(),
                }],
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BoundedPromptMutator, MutationEpisodeEvidence, MutationEvidence, MutationFailureEvidence,
    };
    use agent_evolution_protocol::{
        DiagnosticStatus, EpisodeId, FailureKind, GenomeDigest, GenomeRevisionId, Outcome,
        ReplayabilityGrade, UsageSummary,
    };
    use std::collections::BTreeSet;

    /// 构造不含用户正文和原始结果的脱敏证据。
    fn evidence() -> MutationEvidence {
        MutationEvidence {
            issue_id: agent_evolution_protocol::EvolutionIssueId::generate(),
            genome_digest: GenomeDigest::from_sha256_hex("a".repeat(64)).expect("摘要应合法"),
            failure_kind: FailureKind::VerificationFailure,
            root_cause_hypothesis: "任务策略可能导致 VerificationFailure 类失败".to_string(),
            expected_behavior: "任务策略应执行并检查必要验证".to_string(),
            confidence: 1.0,
            status: DiagnosticStatus::EligibleForEvolution,
            episodes: vec![MutationEpisodeEvidence {
                outbox_id: "outbox-test".to_string(),
                episode_id: EpisodeId::generate(),
                genome_revision_id: GenomeRevisionId::generate(),
                outcome: Outcome::TaskFailure,
                task_family: "code-edit".to_string(),
                tags: BTreeSet::new(),
                failure: MutationFailureEvidence {
                    kind: FailureKind::VerificationFailure,
                    confidence: 1.0,
                    rule_derived: true,
                    model_assisted: false,
                },
                usage: UsageSummary::default(),
                replayability: ReplayabilityGrade::FixtureReproducible,
            }],
        }
    }

    /// 默认离线生成器必须产生三个唯一且通过全部边界的 Prompt。
    #[tokio::test]
    async fn generates_three_bounded_unique_prompts() {
        let mutator = BoundedPromptMutator::task_strategy_mvp(DeterministicPromptMutationGenerator);
        let drafts = mutator
            .mutate("先完成任务，再报告结果。", &evidence())
            .await
            .expect("应生成候选");
        assert_eq!(drafts.len(), 3);
        assert!(drafts.iter().all(|draft| draft.prompt.contains("目标行为")));
    }
}
