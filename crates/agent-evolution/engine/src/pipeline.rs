//! Turn 结束后的进化外循环编排。
//!
//! 该模块把监督证据转化为失败归因、聚合 Issue、更新 Outcome 修订，并把进化候选与
//! 人工干预请求写入各自队列。它不运行在 ReACT 主循环内，由应用层在
//! `EpisodeRecorder` 收敛后显式调用。

use crate::{
    attribute_failures, EvolutionOutbox, EvolutionOutboxItem, InterventionQueue,
    InterventionQueueItemV1, IssueAggregator, IssueObservation, IssueObservationStore,
    OutcomeRevisionStore,
};
use agent_evolution_protocol::{
    EvolutionIssueId, FailureDisposition, GenomeDigest, OutcomeRevision,
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
    observations: Option<Arc<dyn IssueObservationStore>>,
    interventions: Option<Arc<dyn InterventionQueue>>,
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
            observations: None,
            interventions: None,
        }
    }

    /// 启用只追加 Issue 观察日志，使同类失败可跨进程重启聚合。
    pub fn with_issue_observation_store<S>(mut self, store: Arc<S>) -> Self
    where
        S: IssueObservationStore + 'static,
    {
        self.observations = Some(store);
        self
    }

    /// 配置独立人工干预队列。
    ///
    /// 未配置时遇到人工处置会失败关闭，绝不回落写入 Evolution Outbox。
    pub fn with_intervention_queue<Q>(mut self, queue: Arc<Q>) -> Self
    where
        Q: InterventionQueue + 'static,
    {
        self.interventions = Some(queue);
        self
    }

    /// 处理一条已收敛 Episode 的监督证据。
    ///
    /// 步骤：归因 → 逐条聚合 → 按处置分流到 Evolution Outbox 或人工干预队列。
    /// 返回两个队列各自的成功写入数；`Observe` 与 `Ignore` 不创建任务。
    ///
    /// `genome_digest` 由调用方从已解析的 Genome 提供，聚合器不反向依赖 Genome Registry。
    ///
    /// # Errors
    ///
    /// 队列或修订存储写入失败、人工队列缺失，或 Episode 结束后仍出现
    /// `RetryInTurn` 时返回错误；已写入的记录不会回滚。
    pub async fn process_episode(
        &self,
        episode: &agent_evolution_protocol::Episode,
        incidents: &[agent_evolution_protocol::Incident],
        genome_digest: &GenomeDigest,
        current_revision: Option<&OutcomeRevision>,
    ) -> Result<PipelineWriteSummary, PipelineError> {
        for incident in incidents {
            incident
                .validate()
                .map_err(|error| PipelineError::InvalidIncident(error.to_string()))?;
            if incident.episode_id != episode.episode_id {
                return Err(PipelineError::MixedEpisodeIncident {
                    expected: episode.episode_id.clone(),
                    actual: incident.episode_id.clone(),
                });
            }
        }
        if let Some(current) = current_revision {
            current
                .validate()
                .map_err(|error| PipelineError::InvalidOutcomeRevision(error.to_string()))?;
            if current.episode_id != episode.episode_id {
                return Err(PipelineError::MixedEpisodeRevision);
            }
            match self
                .revisions
                .current(&episode.episode_id)
                .await
                .map_err(PipelineError::OutcomeRevision)?
            {
                None => self
                    .revisions
                    .append(current)
                    .await
                    .map_err(PipelineError::OutcomeRevision)?,
                Some(existing) if existing.revision_id == current.revision_id => {}
                Some(_) => return Err(PipelineError::StaleOutcomeRevision),
            }
        }
        let records = attribute_failures(&episode.episode_id, incidents, &episode.failures);
        let decisions = if let Some(store) = &self.observations {
            let mut aggregator = IssueAggregator::new();
            for observation in store.all().await.map_err(PipelineError::IssueObservation)? {
                aggregator.record_with_issue_id(
                    &observation.record,
                    &observation.episode_id,
                    &observation.fingerprint.genome_digest,
                    Some(observation.issue_id),
                );
            }
            for record in &records {
                let (issue, _) = aggregator.record(record, &episode.episode_id, genome_digest);
                store
                    .append(&IssueObservation::new(
                        issue.issue_id,
                        episode.episode_id.clone(),
                        genome_digest,
                        record.clone(),
                    ))
                    .await
                    .map_err(PipelineError::IssueObservation)?;
            }

            let mut rebuilt = IssueAggregator::new();
            for observation in store.all().await.map_err(PipelineError::IssueObservation)? {
                rebuilt.record_with_issue_id(
                    &observation.record,
                    &observation.episode_id,
                    &observation.fingerprint.genome_digest,
                    Some(observation.issue_id),
                );
            }
            let decisions = records
                .iter()
                .map(|record| rebuilt.record(record, &episode.episode_id, genome_digest))
                .collect::<Vec<_>>();
            *self.aggregator.lock().await = rebuilt;
            decisions
        } else {
            let mut aggregator = self.aggregator.lock().await;
            records
                .iter()
                .map(|record| aggregator.record(record, &episode.episode_id, genome_digest))
                .collect::<Vec<_>>()
        };

        // 写入前先验证所有路由所需能力，避免同一 Episode 只提交一部分确定性任务。
        for (issue, disposition) in &decisions {
            match disposition {
                FailureDisposition::RetryInTurn => {
                    return Err(PipelineError::RetryInTurnAfterEpisode(
                        issue.issue_id.clone(),
                    ));
                }
                FailureDisposition::ManualReview
                | FailureDisposition::PlatformEngineering
                | FailureDisposition::PluginMaintenance
                | FailureDisposition::SecurityIncident
                | FailureDisposition::InfrastructureOperations
                    if self.interventions.is_none() =>
                {
                    return Err(PipelineError::InterventionQueueUnavailable(*disposition));
                }
                FailureDisposition::Ignore
                | FailureDisposition::Observe
                | FailureDisposition::EvolutionCandidate
                | FailureDisposition::ManualReview
                | FailureDisposition::PlatformEngineering
                | FailureDisposition::PluginMaintenance
                | FailureDisposition::SecurityIncident
                | FailureDisposition::InfrastructureOperations => {}
            }
        }

        let mut summary = PipelineWriteSummary::default();
        for (issue, disposition) in decisions {
            let outcome = episode
                .outcome
                .clone()
                .unwrap_or(agent_evolution_protocol::Outcome::Unverifiable);
            match disposition {
                FailureDisposition::EvolutionCandidate => {
                    let item = EvolutionOutboxItem {
                        outbox_id: format!("out_{}", Uuid::new_v4().simple()),
                        episode_id: episode.episode_id.clone(),
                        outcome,
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
                    summary.evolution_candidates_written += 1;
                }
                FailureDisposition::ManualReview
                | FailureDisposition::PlatformEngineering
                | FailureDisposition::PluginMaintenance
                | FailureDisposition::SecurityIncident
                | FailureDisposition::InfrastructureOperations => {
                    let queue = self
                        .interventions
                        .as_ref()
                        .ok_or(PipelineError::InterventionQueueUnavailable(disposition))?;
                    let request = InterventionQueueItemV1::create(
                        episode.episode_id.clone(),
                        outcome,
                        disposition,
                        Some(issue.issue_id),
                        issue.status,
                        Some(issue.fingerprint.failure_class),
                        None,
                        episode.finished_at_ms,
                    )
                    .map_err(PipelineError::InterventionQueue)?;
                    queue
                        .append(&request)
                        .await
                        .map_err(PipelineError::InterventionQueue)?;
                    summary.interventions_written += 1;
                }
                FailureDisposition::Observe | FailureDisposition::Ignore => {}
                FailureDisposition::RetryInTurn => {
                    return Err(PipelineError::RetryInTurnAfterEpisode(issue.issue_id));
                }
            }
        }

        Ok(summary)
    }
}

/// 一次 Episode 分流成功写入两个独立队列的数量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineWriteSummary {
    /// 写入 Evolution Outbox 的进化候选数。
    pub evolution_candidates_written: usize,
    /// 写入人工干预队列的请求数。
    pub interventions_written: usize,
}

impl PipelineWriteSummary {
    /// 返回本次写入的任务总数，用于旧调用方逐步迁移。
    pub fn total_written(self) -> usize {
        self.evolution_candidates_written + self.interventions_written
    }
}

impl PartialEq<usize> for PipelineWriteSummary {
    fn eq(&self, other: &usize) -> bool {
        self.total_written() == *other
    }
}

/// 外循环编排错误。
#[derive(Debug, Error)]
pub enum PipelineError {
    /// Outbox 写入失败。
    #[error(transparent)]
    Outbox(crate::OutboxError),
    /// 人工干预请求构造或写入失败。
    #[error(transparent)]
    InterventionQueue(crate::InterventionQueueError),
    /// 处置要求人工介入，但应用层没有配置人工队列。
    #[error("人工处置要求独立队列，但当前未配置：{0:?}")]
    InterventionQueueUnavailable(FailureDisposition),
    /// Turn 已结束后不允许再执行 Turn 内重试。
    #[error("Issue {0} 在 Episode 收敛后仍请求 Turn 内重试")]
    RetryInTurnAfterEpisode(EvolutionIssueId),
    /// Outcome 修订写入失败。
    #[error(transparent)]
    OutcomeRevision(crate::OutcomeRevisionError),
    /// Issue 观察日志读写或完整性校验失败。
    #[error(transparent)]
    IssueObservation(crate::IssueObservationError),
    /// Incident 违反监督协议不变量。
    #[error("Incident 不合法：{0}")]
    InvalidIncident(String),
    /// Incident 属于另一 Episode。
    #[error("Incident Episode 不匹配：期望 {expected}，实际 {actual}")]
    MixedEpisodeIncident {
        /// 当前处理的 Episode。
        expected: agent_evolution_protocol::EpisodeId,
        /// Incident 声明的 Episode。
        actual: agent_evolution_protocol::EpisodeId,
    },
    /// Outcome 修订违反协议不变量。
    #[error("OutcomeRevision 不合法：{0}")]
    InvalidOutcomeRevision(String),
    /// Outcome 修订属于另一 Episode。
    #[error("OutcomeRevision 与 Episode 不匹配")]
    MixedEpisodeRevision,
    /// 本地历史已有另一条最新修订，拒绝覆盖。
    #[error("OutcomeRevision 不是当前 Episode 的最新可信修订")]
    StaleOutcomeRevision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EpisodeStore, EvolutionOutbox, FileEpisodeStore, FileEvolutionOutbox,
        FileInterventionQueue, FileIssueObservationStore, FileOutcomeRevisionStore,
        InterventionQueue, OutcomeRevisionStore,
    };
    use agent_evolution_protocol::{
        ArtifactDigest, ArtifactRef, ComponentRef, DetectorRef, Episode, EpisodeDataPolicy,
        EpisodeId, EventId, FailureClassification, FailureKind, GenomeRevisionId, Incident,
        IncidentId, IncidentKind, IncidentStatus, Outcome, OutcomeRevisionId, OutcomeSource,
        Recoverability, ReplayabilityGrade, RunId, Severity, TaskDescriptor, UsageSummary,
        EPISODE_SCHEMA_VERSION,
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
        let observations = Arc::new(FileIssueObservationStore::new(root.join("observations")));
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions.clone())
            .with_issue_observation_store(observations.clone());

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
            feedback: None,
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

        // 模拟应用进程重启；第二个 Pipeline 只能从只追加观察日志恢复第一次计数。
        drop(pipeline);
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions.clone())
            .with_issue_observation_store(observations);

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
            feedback: None,
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
    async fn safety_failure_goes_only_to_intervention_queue() {
        let root = temp_root();
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
        let interventions = Arc::new(FileInterventionQueue::new(root.join("interventions")));
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions.clone())
            .with_intervention_queue(interventions.clone());

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
        assert_eq!(written.evolution_candidates_written, 0);
        assert_eq!(written.interventions_written, 1);

        let pending = outbox.pending().await.expect("应读取 Outbox");
        assert!(pending.is_empty());
        let pending = interventions.pending().await.expect("应读取人工队列");
        assert_eq!(pending[0].disposition, FailureDisposition::SecurityIncident);
        assert_eq!(
            pending[0].failure_kind,
            Some(FailureKind::PermissionFailure)
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 插件故障与基础设施故障必须进入人工队列，不能污染进化候选。
    #[tokio::test]
    async fn plugin_and_infrastructure_failures_route_to_intervention_queue() {
        let root = temp_root();
        let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
        let interventions = Arc::new(FileInterventionQueue::new(root.join("interventions")));
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions)
            .with_intervention_queue(interventions.clone());

        let plugin_episode = episode();
        let mut plugin_incident = incident(&plugin_episode.episode_id);
        plugin_incident.kind = IncidentKind::PluginTrap;
        plugin_incident.component = ComponentRef::PluginHost;
        let plugin_written = pipeline
            .process_episode(&plugin_episode, &[plugin_incident], &digest(), None)
            .await
            .expect("插件故障应完成分流");
        assert_eq!(plugin_written.evolution_candidates_written, 0);
        assert_eq!(plugin_written.interventions_written, 1);

        let infrastructure_episode = episode();
        let mut infrastructure_incident = incident(&infrastructure_episode.episode_id);
        infrastructure_incident.kind = IncidentKind::StorageFailure;
        infrastructure_incident.component = ComponentRef::Storage;
        let infrastructure_written = pipeline
            .process_episode(
                &infrastructure_episode,
                &[infrastructure_incident],
                &digest(),
                None,
            )
            .await
            .expect("基础设施故障应完成分流");
        assert_eq!(infrastructure_written.evolution_candidates_written, 0);
        assert_eq!(infrastructure_written.interventions_written, 1);

        assert!(outbox.pending().await.expect("应读取 Outbox").is_empty());
        let pending = interventions.pending().await.expect("应读取人工队列");
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|item| {
            item.disposition == FailureDisposition::PluginMaintenance
                && item.failure_kind == Some(FailureKind::PluginFailure)
        }));
        assert!(pending.iter().any(|item| {
            item.disposition == FailureDisposition::InfrastructureOperations
                && item.failure_kind == Some(FailureKind::EnvironmentFailure)
        }));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 应用层遗漏人工队列时必须失败关闭，Evolution Outbox 保持为空。
    #[tokio::test]
    async fn missing_intervention_queue_fails_closed() {
        let root = temp_root();
        let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let pipeline = EvolutionPipeline::new(outbox.clone(), revisions);
        let episode = episode();
        let mut incident = incident(&episode.episode_id);
        incident.kind = IncidentKind::PluginTrap;
        incident.component = ComponentRef::PluginHost;

        assert!(matches!(
            pipeline
                .process_episode(&episode, &[incident], &digest(), None)
                .await,
            Err(PipelineError::InterventionQueueUnavailable(
                FailureDisposition::PluginMaintenance
            ))
        ));
        assert!(outbox.pending().await.expect("应读取 Outbox").is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
