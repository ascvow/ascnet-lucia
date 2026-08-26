//! Turn 结束后的进化外循环编排。
//!
//! 该模块把监督证据转化为失败归因、聚合 Issue、更新 Outcome 修订并写入
//! Evolution Outbox。它不运行在 ReACT 主循环内，由应用层在 `EpisodeRecorder`
//! 收敛后显式调用。

use crate::{
    attribute_failures, EvolutionOutbox, EvolutionOutboxItem, IssueAggregator, OutcomeRevisionStore,
};
use agent_evolution_protocol::{
    FailureDisposition, GenomeDigest, OutcomeRevision, OutcomeRevisionId,
};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

/// 外循环编排器持有的共享状态。
pub struct EvolutionPipeline<O, R>
where
    O: EvolutionOutbox,
    R: OutcomeRevisionStore,
{
    outbox: Arc<O>,
    revisions: Arc<R>,
    aggregator: tokio::sync::Mutex<IssueAggregator>,
}

impl<O, R> EvolutionPipeline<O, R>
where
    O: EvolutionOutbox,
    R: OutcomeRevisionStore,
{
    /// 使用给定 Outbox 与修订存储创建编排器。
    pub fn new(outbox: Arc<O>, revisions: Arc<R>) -> Self {
        Self {
            outbox,
            revisions,
            aggregator: tokio::sync::Mutex::new(IssueAggregator::new()),
        }
    }

    /// 处理一条已收敛 Episode 的监督证据。
    ///
    /// 步骤：归因 → 逐条聚合 → 非 Observe/Ignore 处置写入 Outbox → 更新 Outcome 修订。
    /// 返回写入 Outbox 的记录数。
    ///
    /// `genome_digest` 由调用方从已解析的 Genome 提供，聚合器不反向依赖 Genome Registry。
    ///
    /// # Errors
    ///
    /// Outbox 或修订存储写入失败时返回错误，已写入的记录不会回滚。
    pub async fn process_episode(
        &self,
        episode: &agent_evolution_protocol::Episode,
        incidents: &[agent_evolution_protocol::Incident],
        genome_digest: &GenomeDigest,
        current_revision: Option<&OutcomeRevision>,
    ) -> Result<usize, PipelineError> {
        let records = attribute_failures(&episode.episode_id, incidents, &episode.failures);
        let mut written = 0;

        for record in &records {
            let (issue, disposition) = {
                let mut aggregator = self.aggregator.lock().await;
                aggregator.record(record, &episode.episode_id, genome_digest)
            };

            if matches!(
                disposition,
                FailureDisposition::Observe | FailureDisposition::Ignore
            ) {
                continue;
            }

            let item = EvolutionOutboxItem {
                outbox_id: format!("out_{}", Uuid::new_v4().simple()),
                episode_id: episode.episode_id.clone(),
                outcome: episode
                    .outcome
                    .clone()
                    .unwrap_or(agent_evolution_protocol::Outcome::Unverifiable),
                disposition,
                issue_id: Some(issue.issue_id.clone()),
                issue_status: issue.status,
                created_at_ms: episode.finished_at_ms,
                consumed: false,
            };
            self.outbox
                .append(&item)
                .await
                .map_err(PipelineError::Outbox)?;
            written += 1;
        }

        if let Some(current) = current_revision {
            if !records.is_empty() {
                let new_revision = OutcomeRevision {
                    revision_id: OutcomeRevisionId::generate(),
                    episode_id: episode.episode_id.clone(),
                    supersedes: Some(current.revision_id.clone()),
                    outcome: episode
                        .outcome
                        .clone()
                        .unwrap_or(agent_evolution_protocol::Outcome::Unverifiable),
                    source: agent_evolution_protocol::OutcomeSource::DeterministicRule,
                    reason: format!("聚合 {} 条失败归因后确认终态", records.len()),
                };
                self.revisions
                    .append(&new_revision)
                    .await
                    .map_err(PipelineError::OutcomeRevision)?;
            }
        }

        Ok(written)
    }
}

/// 外循环编排错误。
#[derive(Debug, Error)]
pub enum PipelineError {
    /// Outbox 写入失败。
    #[error(transparent)]
    Outbox(crate::OutboxError),
    /// Outcome 修订写入失败。
    #[error(transparent)]
    OutcomeRevision(crate::OutcomeRevisionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EpisodeStore, EvolutionOutbox, FileEpisodeStore, FileEvolutionOutbox,
        FileOutcomeRevisionStore, OutcomeRevisionStore,
    };
    use agent_evolution_protocol::{
        ArtifactDigest, ArtifactRef, ComponentRef, DetectorRef, Episode, EpisodeDataPolicy,
        EpisodeId, EventId, FailureClassification, FailureKind, GenomeRevisionId, Incident,
        IncidentId, IncidentKind, IncidentStatus, Outcome, OutcomeSource, Recoverability,
        ReplayabilityGrade, RunId, Severity, TaskDescriptor, UsageSummary, EPISODE_SCHEMA_VERSION,
    };
    use std::path::PathBuf;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-pipeline-{}", Uuid::new_v4().simple()))
    }

    fn digest() -> GenomeDigest {
        GenomeDigest::from_sha256_hex("a".repeat(64)).expect("摘要应合法")
    }

    fn episode() -> Episode {
        Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: EpisodeId::generate(),
            run_id: RunId::generate(),
            session_id: "session-1".into(),
            genome_revision_id: GenomeRevisionId::generate(),
            task: TaskDescriptor::default(),
            event_stream_ref: ArtifactRef {
                digest: ArtifactDigest::from_sha256_hex("1".repeat(64)).expect("摘要应合法"),
                media_type: "application/x-ndjson".into(),
                size_bytes: 1,
            },
            supervision: None,
            environment_ref: None,
            outcome: Some(Outcome::TaskFailure),
            failures: vec![FailureClassification {
                kind: FailureKind::ToolExecution,
                evidence_event_ids: vec![EventId::generate().to_string()],
                confidence: 1.0,
                rule_derived: true,
                model_assisted: false,
            }],
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
            data_policy: EpisodeDataPolicy::default(),
            event_count: 1,
            started_at_ms: 1,
            finished_at_ms: 2,
        }
    }

    fn incident(episode_id: &EpisodeId) -> Incident {
        let observed = EventId::generate();
        Incident {
            incident_id: IncidentId::generate(),
            episode_id: episode_id.clone(),
            observed_event_id: observed.clone(),
            kind: IncidentKind::ToolExecutionFailed,
            severity: Severity::Warning,
            recoverability: Recoverability::Recoverable,
            component: ComponentRef::Tool,
            detector: DetectorRef::ToolExecution,
            evidence: vec![observed],
            status: IncidentStatus::Observed,
        }
    }

    #[tokio::test]
    async fn pipeline_routes_repeated_failures_to_outbox() {
        let root = temp_root();
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions.clone());

        // 第一条失败：Observe，不进入 Outbox。
        let first = episode();
        episodes.append(&first).await.expect("应追加 Episode");
        let first_revision = OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: first.episode_id.clone(),
            supersedes: None,
            outcome: Outcome::Unverifiable,
            source: OutcomeSource::DeterministicRule,
            reason: "初始不可验证".into(),
        };
        revisions
            .append(&first_revision)
            .await
            .expect("应追加初始修订");
        let written = pipeline
            .process_episode(
                &first,
                &[incident(&first.episode_id)],
                &digest(),
                Some(&first_revision),
            )
            .await
            .expect("应处理");
        assert_eq!(written, 0);

        // 第二条同类失败：聚合为 Clustered，进入 EvolutionCandidate。
        let second = episode();
        episodes.append(&second).await.expect("应追加 Episode");
        let second_revision = OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: second.episode_id.clone(),
            supersedes: None,
            outcome: Outcome::TaskFailure,
            source: OutcomeSource::DeterministicRule,
            reason: "初始失败".into(),
        };
        revisions
            .append(&second_revision)
            .await
            .expect("应追加初始修订");
        let written = pipeline
            .process_episode(
                &second,
                &[incident(&second.episode_id)],
                &digest(),
                Some(&second_revision),
            )
            .await
            .expect("应处理");
        assert_eq!(written, 1);

        let pending = outbox.pending().await.expect("应读取 Outbox");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].disposition,
            FailureDisposition::EvolutionCandidate
        );
        assert_eq!(
            pending[0].issue_status,
            agent_evolution_protocol::DiagnosticStatus::Clustered
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn safety_failure_goes_to_outbox_immediately() {
        let root = temp_root();
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions.clone());

        let mut episode = episode();
        episode.outcome = Some(Outcome::SafetyFailure);
        episodes.append(&episode).await.expect("应追加 Episode");

        let mut incident = incident(&episode.episode_id);
        incident.kind = IncidentKind::PermissionDenied;
        incident.component = ComponentRef::Runtime;

        let written = pipeline
            .process_episode(&episode, &[incident], &digest(), None)
            .await
            .expect("应处理");
        assert_eq!(written, 1);

        let pending = outbox.pending().await.expect("应读取 Outbox");
        assert_eq!(pending[0].disposition, FailureDisposition::SecurityIncident);
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
