//! 从只追加 Episode 与 CAS 恢复经过完整绑定校验的监督证据。

use crate::{ArtifactStore, ArtifactStoreError, EpisodeStore, EpisodeStoreError};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, Episode, EpisodeEvent, EpisodeId, EventEnvelope, EventId,
    Incident, OutcomeRevision,
};
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;
use thiserror::Error;

/// Pipeline、CLI 与延迟反馈共用的可信 Episode 证据快照。
#[derive(Debug, Clone)]
pub struct EpisodeEvidence {
    /// 只追加 Episode Header。
    pub episode: Episode,
    /// 已按 Episode 数据策略脱敏的完整事件流。
    pub events: Vec<EpisodeEvent>,
    /// Supervisor 产生的全部 Incident；没有异常时为空。
    pub incidents: Vec<Incident>,
    /// Recorder 产生的初始 Outcome 修订。
    pub initial_outcome_revision: Option<OutcomeRevision>,
}

/// 从 Episode Store 与 Artifact CAS 读取并验证一条完整监督证据。
///
/// 校验覆盖 CAS 媒体类型和长度、Incident/Revision 的 Episode 绑定，以及每个证据 Event ID
/// 确实存在于 Episode Event Stream。调用方不得用外部 JSON 替换返回内容。
///
/// # Errors
///
/// Episode 或制品缺失、CAS 引用不匹配、JSON 损坏或跨 Episode/Event 引用时返回
/// [`EpisodeEvidenceError`]。
pub async fn load_episode_evidence(
    episodes: &dyn EpisodeStore,
    artifacts: &dyn ArtifactStore,
    episode_id: &EpisodeId,
) -> Result<EpisodeEvidence, EpisodeEvidenceError> {
    let episode = episodes
        .get(episode_id)
        .await?
        .ok_or_else(|| EpisodeEvidenceError::EpisodeNotFound(episode_id.clone()))?;
    episode
        .validate()
        .map_err(|error| EpisodeEvidenceError::InvalidEpisode(error.to_string()))?;
    let event_bytes =
        load_artifact(artifacts, &episode.event_stream_ref, "application/x-ndjson").await?;
    let events: Vec<EpisodeEvent> = parse_ndjson(&event_bytes, "Episode Event Stream")?;
    if events.len() as u64 != episode.event_count {
        return Err(EpisodeEvidenceError::EventCountMismatch {
            expected: episode.event_count,
            actual: events.len() as u64,
        });
    }
    let mut event_ids = BTreeSet::new();
    for event in &events {
        EventId::new(event.event_id.clone())
            .map_err(|error| EpisodeEvidenceError::InvalidEvent(error.to_string()))?;
        if event.run_id != episode.run_id {
            return Err(EpisodeEvidenceError::EventRunMismatch);
        }
        if !event_ids.insert(event.event_id.as_str()) {
            return Err(EpisodeEvidenceError::DuplicateEventId(
                event.event_id.clone(),
            ));
        }
    }
    if episode
        .failures
        .iter()
        .flat_map(|failure| &failure.evidence_event_ids)
        .any(|event_id| !event_ids.contains(event_id.as_str()))
    {
        return Err(EpisodeEvidenceError::UnknownFailureEvent);
    }

    let mut incidents: Vec<Incident> = Vec::new();
    let mut initial_outcome_revision = None;
    if let Some(supervision) = &episode.supervision {
        let envelope_bytes = load_artifact(
            artifacts,
            &supervision.event_envelopes_ref,
            "application/x-ndjson",
        )
        .await?;
        let envelopes: Vec<EventEnvelope> = parse_ndjson(&envelope_bytes, "Event Envelope Stream")?;
        validate_envelopes(&episode, &events, &envelopes)?;
        if let Some(reference) = &supervision.incidents_ref {
            let bytes = load_artifact(artifacts, reference, "application/x-ndjson").await?;
            incidents = parse_ndjson(&bytes, "Incident Stream")?;
            for incident in &incidents {
                incident
                    .validate()
                    .map_err(|error| EpisodeEvidenceError::InvalidIncident(error.to_string()))?;
                if incident.episode_id != episode.episode_id {
                    return Err(EpisodeEvidenceError::IncidentEpisodeMismatch);
                }
                if incident
                    .evidence
                    .iter()
                    .any(|event_id| !event_ids.contains(event_id.as_str()))
                {
                    return Err(EpisodeEvidenceError::UnknownIncidentEvent);
                }
            }
        }
        if let Some(reference) = &supervision.outcome_revision_ref {
            let bytes = load_artifact(artifacts, reference, "application/json").await?;
            let revision: OutcomeRevision = serde_json::from_slice(&bytes).map_err(|source| {
                EpisodeEvidenceError::InvalidJson {
                    artifact: "Outcome Revision",
                    source,
                }
            })?;
            revision
                .validate()
                .map_err(|error| EpisodeEvidenceError::InvalidRevision(error.to_string()))?;
            if revision.episode_id != episode.episode_id
                || revision.supersedes.is_some()
                || episode.outcome.as_ref() != Some(&revision.outcome)
            {
                return Err(EpisodeEvidenceError::RevisionEpisodeMismatch);
            }
            initial_outcome_revision = Some(revision);
        }
    }

    Ok(EpisodeEvidence {
        episode,
        events,
        incidents,
        initial_outcome_revision,
    })
}

/// 校验可信信封与公开事件流逐条一致，拒绝跨 Episode、Run 或 Genome 的证据拼接。
fn validate_envelopes(
    episode: &Episode,
    events: &[EpisodeEvent],
    envelopes: &[EventEnvelope],
) -> Result<(), EpisodeEvidenceError> {
    if envelopes.len() != events.len() {
        return Err(EpisodeEvidenceError::EnvelopeCountMismatch {
            expected: events.len() as u64,
            actual: envelopes.len() as u64,
        });
    }
    for (index, (event, envelope)) in events.iter().zip(envelopes).enumerate() {
        envelope
            .validate()
            .map_err(|error| EpisodeEvidenceError::InvalidEnvelope(error.to_string()))?;
        let sequence = index as u64 + 1;
        if envelope.sequence != sequence {
            return Err(EpisodeEvidenceError::EnvelopeMismatch {
                sequence,
                reason: "sequence 不连续",
            });
        }
        if envelope.episode_id != episode.episode_id
            || envelope.run_id != episode.run_id
            || envelope.genome_revision_id != episode.genome_revision_id
        {
            return Err(EpisodeEvidenceError::EnvelopeMismatch {
                sequence,
                reason: "Episode、Run 或 Genome 绑定不一致",
            });
        }
        if envelope.event_id.to_string() != event.event_id
            || envelope.timestamp_ms != event.timestamp_ms
            || envelope.kind != event.kind
            || envelope.step != event.step
            || envelope.payload != event.payload
        {
            return Err(EpisodeEvidenceError::EnvelopeMismatch {
                sequence,
                reason: "事件内容不一致",
            });
        }
    }
    Ok(())
}

/// 读取一个 CAS 引用并校验媒体类型和长度。
async fn load_artifact(
    artifacts: &dyn ArtifactStore,
    reference: &ArtifactRef,
    expected_media_type: &'static str,
) -> Result<Vec<u8>, EpisodeEvidenceError> {
    if reference.media_type != expected_media_type {
        return Err(EpisodeEvidenceError::InvalidMediaType {
            expected: expected_media_type,
            actual: reference.media_type.clone(),
        });
    }
    let bytes = artifacts
        .get(&reference.digest)
        .await?
        .ok_or_else(|| EpisodeEvidenceError::ArtifactNotFound(reference.digest.clone()))?;
    if bytes.len() as u64 != reference.size_bytes {
        return Err(EpisodeEvidenceError::ArtifactSizeMismatch {
            digest: reference.digest.clone(),
            expected: reference.size_bytes,
            actual: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

/// 解析逐行 JSON，拒绝空文件中的伪记录和任一损坏行。
fn parse_ndjson<T: DeserializeOwned>(
    bytes: &[u8],
    artifact: &'static str,
) -> Result<Vec<T>, EpisodeEvidenceError> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line)
                .map_err(|source| EpisodeEvidenceError::InvalidJson { artifact, source })
        })
        .collect()
}

/// 恢复可信 Episode 证据时的错误。
#[derive(Debug, Error)]
pub enum EpisodeEvidenceError {
    /// Episode 不存在。
    #[error("Episode 不存在：{0}")]
    EpisodeNotFound(EpisodeId),
    /// Episode Header 不满足协议不变量。
    #[error("Episode Header 不合法：{0}")]
    InvalidEpisode(String),
    /// CAS 制品不存在。
    #[error("Episode 引用的 CAS 制品不存在：{0}")]
    ArtifactNotFound(ArtifactDigest),
    /// CAS 媒体类型与协议不一致。
    #[error("CAS 媒体类型不匹配：期望 {expected}，实际 {actual}")]
    InvalidMediaType {
        /// 协议要求的媒体类型。
        expected: &'static str,
        /// 引用中声明的媒体类型。
        actual: String,
    },
    /// CAS 引用长度与实际内容不一致。
    #[error("CAS 制品 {digest} 长度不匹配：期望 {expected}，实际 {actual}")]
    ArtifactSizeMismatch {
        /// 制品摘要。
        digest: ArtifactDigest,
        /// 引用声明长度。
        expected: u64,
        /// 实际长度。
        actual: u64,
    },
    /// Episode 事件数不匹配。
    #[error("Episode 事件数不匹配：期望 {expected}，实际 {actual}")]
    EventCountMismatch {
        /// Header 声明数量。
        expected: u64,
        /// 实际解析数量。
        actual: u64,
    },
    /// Event ID 或其他事件字段不合法。
    #[error("Episode Event 不合法：{0}")]
    InvalidEvent(String),
    /// 事件绑定了其他 Run。
    #[error("Episode Event 与 Header 的 Run 绑定不一致")]
    EventRunMismatch,
    /// 同一 Event Stream 重复使用事件 ID。
    #[error("Episode Event ID 重复：{0}")]
    DuplicateEventId(String),
    /// Episode 失败分类引用了 Event Stream 中不存在的事件。
    #[error("FailureClassification 引用了不存在的 Episode Event")]
    UnknownFailureEvent,
    /// Event Envelope 数量与公开事件流不一致。
    #[error("Event Envelope 数量不匹配：期望 {expected}，实际 {actual}")]
    EnvelopeCountMismatch {
        /// 公开事件数。
        expected: u64,
        /// 信封数。
        actual: u64,
    },
    /// Event Envelope 自身违反协议不变量。
    #[error("Event Envelope 不合法：{0}")]
    InvalidEnvelope(String),
    /// Event Envelope 与公开事件或 Header 绑定不一致。
    #[error("Event Envelope 不匹配：sequence={sequence}: {reason}")]
    EnvelopeMismatch {
        /// 从 1 开始的信封序号。
        sequence: u64,
        /// 稳定错误原因。
        reason: &'static str,
    },
    /// JSON 或 NDJSON 制品损坏。
    #[error("解析 {artifact} 失败：{source}")]
    InvalidJson {
        /// 制品语义名称。
        artifact: &'static str,
        /// JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// Incident 不满足协议不变量。
    #[error("Incident 不合法：{0}")]
    InvalidIncident(String),
    /// Incident 绑定了其他 Episode。
    #[error("Incident 与 Episode 绑定不一致")]
    IncidentEpisodeMismatch,
    /// Incident 引用了 Event Stream 中不存在的事件。
    #[error("Incident 引用了 Episode Event Stream 中不存在的事件")]
    UnknownIncidentEvent,
    /// Outcome 修订不满足协议不变量。
    #[error("OutcomeRevision 不合法：{0}")]
    InvalidRevision(String),
    /// Outcome 修订与 Episode Header 不一致。
    #[error("OutcomeRevision 与 Episode Header 不一致")]
    RevisionEpisodeMismatch,
    /// Episode Store 访问失败。
    #[error(transparent)]
    Episode(#[from] EpisodeStoreError),
    /// Artifact CAS 访问失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileArtifactStore, FileEpisodeStore};
    use agent_evolution_protocol::{
        ComponentRef, DetectorRef, EpisodeDataPolicy, EpisodeSupervisionRefs, GenomeRevisionId,
        IncidentId, IncidentKind, IncidentStatus, Outcome, Recoverability, ReplayabilityGrade,
        RunId, Severity, TaskDescriptor, UsageSummary, EPISODE_SCHEMA_VERSION,
    };
    use serde::Serialize;
    use serde_json::json;
    use std::{path::PathBuf, sync::Arc};
    use uuid::Uuid;

    /// 创建独立证据加载测试目录。
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "lucia-episode-evidence-{}",
            Uuid::new_v4().simple()
        ))
    }

    /// 把记录序列化为带换行的 NDJSON CAS 制品。
    async fn put_ndjson<T: Serialize>(artifacts: &FileArtifactStore, records: &[T]) -> ArtifactRef {
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, record).expect("测试记录应序列化");
            bytes.push(b'\n');
        }
        artifacts
            .put("application/x-ndjson", &bytes)
            .await
            .expect("测试制品应写入 CAS")
    }

    /// 写入一条可按参数制造跨 Episode 或未知事件引用的测试 Episode。
    async fn persist_episode_with_incident(
        root: &std::path::Path,
        incident_episode_id: Option<EpisodeId>,
        incident_event_id: Option<EventId>,
    ) -> (Arc<FileEpisodeStore>, Arc<FileArtifactStore>, EpisodeId) {
        let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let episode_id = EpisodeId::generate();
        let run_id = RunId::generate();
        let genome_revision_id = GenomeRevisionId::generate();
        let event_id = EventId::generate();
        let event = EpisodeEvent {
            event_id: event_id.to_string(),
            run_id: run_id.clone(),
            timestamp_ms: 1,
            kind: "run_started".into(),
            step: 0,
            payload: json!({}),
        };
        let envelope = EventEnvelope {
            event_id: event_id.clone(),
            run_id: run_id.clone(),
            episode_id: episode_id.clone(),
            sequence: 1,
            span_id: None,
            parent_span_id: None,
            agent_execution_id: None,
            genome_revision_id: genome_revision_id.clone(),
            timestamp_ms: 1,
            kind: "run_started".into(),
            step: 0,
            payload: json!({}),
        };
        let observed_event_id = incident_event_id.unwrap_or(event_id);
        let incident = Incident {
            incident_id: IncidentId::generate(),
            episode_id: incident_episode_id.unwrap_or_else(|| episode_id.clone()),
            observed_event_id: observed_event_id.clone(),
            kind: IncidentKind::ToolExecutionFailed,
            severity: Severity::Warning,
            recoverability: Recoverability::Recoverable,
            component: ComponentRef::Tool,
            detector: DetectorRef::ToolExecution,
            evidence: vec![observed_event_id],
            status: IncidentStatus::Observed,
        };
        let event_stream_ref = put_ndjson(artifacts.as_ref(), &[event]).await;
        let event_envelopes_ref = put_ndjson(artifacts.as_ref(), &[envelope]).await;
        let incidents_ref = put_ndjson(artifacts.as_ref(), &[incident]).await;
        let episode = Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: episode_id.clone(),
            run_id,
            session_id: "evidence-test".into(),
            genome_revision_id,
            task: TaskDescriptor::default(),
            event_stream_ref,
            supervision: Some(EpisodeSupervisionRefs {
                event_envelopes_ref,
                incidents_ref: Some(incidents_ref),
                outcome_revision_ref: None,
            }),
            environment_ref: None,
            outcome: Some(Outcome::Unverifiable),
            failures: Vec::new(),
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
            data_policy: EpisodeDataPolicy::default(),
            event_count: 1,
            started_at_ms: 1,
            finished_at_ms: 1,
        };
        episodes
            .append(&episode)
            .await
            .expect("测试 Episode 应写入");
        (episodes, artifacts, episode_id)
    }

    /// Pipeline 前的证据重载必须拒绝绑定到另一 Episode 的 Incident。
    #[tokio::test]
    async fn rejects_incident_from_foreign_episode() {
        let root = temp_root();
        let (episodes, artifacts, episode_id) =
            persist_episode_with_incident(&root, Some(EpisodeId::generate()), None).await;

        let error = load_episode_evidence(episodes.as_ref(), artifacts.as_ref(), &episode_id)
            .await
            .expect_err("跨 Episode Incident 应被拒绝");
        assert!(matches!(
            error,
            EpisodeEvidenceError::IncidentEpisodeMismatch
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Incident 的证据 ID 必须真实存在于规范 Event Stream。
    #[tokio::test]
    async fn rejects_incident_with_unknown_event_id() {
        let root = temp_root();
        let (episodes, artifacts, episode_id) =
            persist_episode_with_incident(&root, None, Some(EventId::generate())).await;

        let error = load_episode_evidence(episodes.as_ref(), artifacts.as_ref(), &episode_id)
            .await
            .expect_err("未知事件引用应被拒绝");
        assert!(matches!(error, EpisodeEvidenceError::UnknownIncidentEvent));
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
