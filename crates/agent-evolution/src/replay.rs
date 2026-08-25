//! 已保存事件输出驱动的确定性 Protocol Replay。

use crate::{ArtifactStore, ArtifactStoreError};
use agent_evolution_protocol::{Episode, EpisodeEvent};
use async_trait::async_trait;
use std::{collections::BTreeSet, sync::Arc};

/// Protocol Replay 的事件接收器。
#[async_trait]
pub trait ReplayEventSink: Send + Sync {
    /// 按原始顺序接收一条经过完整性验证的 Episode 事件。
    ///
    /// # Errors
    ///
    /// 下游状态机拒绝事件时返回错误，Replay 会立即停止且不会跳过该事件。
    async fn apply(&self, event: &EpisodeEvent) -> Result<(), ProtocolReplayError>;
}

/// 一次协议回放的确定性报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// 已验证并投递的事件总数。
    pub event_count: u64,
    /// 事件流中的最大 ReACT step。
    pub max_step: u64,
    /// 是否观察到正常 `run_finished` 终态。
    pub finished: bool,
}

/// 读取不可变 Event Stream 并重新驱动观察者的协议回放器。
pub struct ProtocolReplay {
    artifacts: Arc<dyn ArtifactStore>,
}

impl ProtocolReplay {
    /// 使用指定 Artifact CAS 创建回放器。
    pub fn new(artifacts: Arc<dyn ArtifactStore>) -> Self {
        Self { artifacts }
    }

    /// 验证 Episode 引用和事件状态序列，然后按原始顺序投递。
    ///
    /// 该回放不调用真实模型或工具，只使用已保存输出，因此同一制品的报告和投递顺序
    /// 完全确定。
    ///
    /// # Errors
    ///
    /// 制品缺失或损坏、事件数不匹配、run ID 不一致、时间或 step 倒退、终态非法，
    /// 或 sink 拒绝事件时返回错误。
    pub async fn replay(
        &self,
        episode: &Episode,
        sink: &dyn ReplayEventSink,
    ) -> Result<ReplayReport, ProtocolReplayError> {
        episode
            .validate()
            .map_err(|error| ProtocolReplayError::InvalidEpisode(error.to_string()))?;
        let bytes = self
            .artifacts
            .get(&episode.event_stream_ref.digest)
            .await?
            .ok_or_else(|| {
                ProtocolReplayError::MissingArtifact(episode.event_stream_ref.digest.clone())
            })?;
        if bytes.len() as u64 != episode.event_stream_ref.size_bytes {
            return Err(ProtocolReplayError::ArtifactSizeMismatch {
                expected: episode.event_stream_ref.size_bytes,
                actual: bytes.len() as u64,
            });
        }

        let mut events = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let event = serde_json::from_slice::<EpisodeEvent>(line).map_err(|source| {
                ProtocolReplayError::InvalidEvent {
                    line: index + 1,
                    source,
                }
            })?;
            events.push(event);
        }
        if events.len() as u64 != episode.event_count {
            return Err(ProtocolReplayError::EventCountMismatch {
                expected: episode.event_count,
                actual: events.len() as u64,
            });
        }
        validate_sequence(episode, &events)?;
        for event in &events {
            sink.apply(event).await?;
        }
        Ok(ReplayReport {
            event_count: events.len() as u64,
            max_step: events.iter().map(|event| event.step).max().unwrap_or(0),
            finished: events
                .last()
                .is_some_and(|event| event.kind == "run_finished"),
        })
    }
}

/// Protocol Replay 错误。
#[derive(Debug, thiserror::Error)]
pub enum ProtocolReplayError {
    /// Episode Header 不合法。
    #[error("Episode 不合法：{0}")]
    InvalidEpisode(String),
    /// Event Stream 制品不存在。
    #[error("Episode Event Stream 制品不存在：{0}")]
    MissingArtifact(agent_evolution_protocol::ArtifactDigest),
    /// Header 中长度与 CAS 字节长度不一致。
    #[error("Event Stream 长度不匹配：期望 {expected}，实际 {actual}")]
    ArtifactSizeMismatch {
        /// Header 声明长度。
        expected: u64,
        /// 实际长度。
        actual: u64,
    },
    /// NDJSON 中某行不是合法 EpisodeEvent。
    #[error("Event Stream 第 {line} 行损坏：{source}")]
    InvalidEvent {
        /// 从 1 开始的行号。
        line: usize,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// Header 声明事件数不匹配。
    #[error("事件数不匹配：期望 {expected}，实际 {actual}")]
    EventCountMismatch {
        /// Header 声明数量。
        expected: u64,
        /// 实际数量。
        actual: u64,
    },
    /// 事件 run ID 与 Episode 不一致。
    #[error("事件 {event_id} 的 run ID 与 Episode 不一致")]
    RunIdMismatch {
        /// 错误事件 ID。
        event_id: String,
    },
    /// 事件 ID 重复。
    #[error("事件 ID 重复：{0}")]
    DuplicateEventId(String),
    /// 时间戳或 step 倒退。
    #[error("事件顺序倒退：{event_id}")]
    OrderRegression {
        /// 首个倒退事件 ID。
        event_id: String,
    },
    /// 首事件不是 run_started。
    #[error("Protocol Replay 必须以 run_started 开始")]
    MissingRunStart,
    /// run_started 重复。
    #[error("Protocol Replay 中 run_started 只能出现一次")]
    DuplicateRunStart,
    /// run_finished 不是最后一个事件或重复出现。
    #[error("Protocol Replay 中 run_finished 必须唯一且位于末尾")]
    InvalidRunFinish,
    /// Artifact CAS 操作失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// 下游状态机拒绝事件。
    #[error("回放 sink 拒绝事件：{0}")]
    Sink(String),
}

/// 验证协议状态序列的稳定不变量。
fn validate_sequence(
    episode: &Episode,
    events: &[EpisodeEvent],
) -> Result<(), ProtocolReplayError> {
    if events
        .first()
        .is_none_or(|event| event.kind != "run_started")
    {
        return Err(ProtocolReplayError::MissingRunStart);
    }
    let mut ids = BTreeSet::new();
    let mut last_timestamp = 0;
    let mut last_step = 0;
    let mut starts = 0;
    let mut finishes = 0;
    for (index, event) in events.iter().enumerate() {
        if event.run_id != episode.run_id {
            return Err(ProtocolReplayError::RunIdMismatch {
                event_id: event.event_id.clone(),
            });
        }
        if !ids.insert(event.event_id.clone()) {
            return Err(ProtocolReplayError::DuplicateEventId(
                event.event_id.clone(),
            ));
        }
        if index > 0 && (event.timestamp_ms < last_timestamp || event.step < last_step) {
            return Err(ProtocolReplayError::OrderRegression {
                event_id: event.event_id.clone(),
            });
        }
        last_timestamp = event.timestamp_ms;
        last_step = event.step;
        match event.kind.as_str() {
            "run_started" => starts += 1,
            "run_finished" => {
                finishes += 1;
                if index + 1 != events.len() {
                    return Err(ProtocolReplayError::InvalidRunFinish);
                }
            }
            _ => {}
        }
    }
    if starts != 1 {
        return Err(ProtocolReplayError::DuplicateRunStart);
    }
    if finishes > 1 {
        return Err(ProtocolReplayError::InvalidRunFinish);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactStore, FileArtifactStore};
    use agent_evolution_protocol::{
        EpisodeDataPolicy, EpisodeId, GenomeRevisionId, Outcome, ReplayabilityGrade, RunId,
        TaskDescriptor, UsageSummary, EPISODE_SCHEMA_VERSION,
    };
    use std::{path::PathBuf, sync::Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    struct Collector(Mutex<Vec<String>>);

    #[async_trait]
    impl ReplayEventSink for Collector {
        async fn apply(&self, event: &EpisodeEvent) -> Result<(), ProtocolReplayError> {
            self.0.lock().expect("锁不应中毒").push(event.kind.clone());
            Ok(())
        }
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-replay-{}", Uuid::new_v4().simple()))
    }

    #[tokio::test]
    async fn deterministically_replays_valid_stream() {
        let root = temp_root();
        let artifacts = Arc::new(FileArtifactStore::new(&root));
        let run_id = RunId::generate();
        let events = vec![
            EpisodeEvent {
                event_id: "event-1".into(),
                run_id: run_id.clone(),
                timestamp_ms: 1,
                kind: "run_started".into(),
                step: 0,
                payload: serde_json::json!({}),
            },
            EpisodeEvent {
                event_id: "event-2".into(),
                run_id: run_id.clone(),
                timestamp_ms: 2,
                kind: "run_finished".into(),
                step: 0,
                payload: serde_json::json!({}),
            },
        ];
        let mut bytes = Vec::new();
        for event in &events {
            serde_json::to_writer(&mut bytes, event).expect("应序列化");
            bytes.push(b'\n');
        }
        let event_stream_ref = artifacts
            .put("application/x-ndjson", &bytes)
            .await
            .expect("应写入");
        let episode = Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: EpisodeId::generate(),
            run_id,
            session_id: "session-1".into(),
            genome_revision_id: GenomeRevisionId::generate(),
            task: TaskDescriptor::default(),
            event_stream_ref,
            environment_ref: None,
            outcome: Some(Outcome::Unverifiable),
            failures: Vec::new(),
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
            data_policy: EpisodeDataPolicy::default(),
            event_count: 2,
            started_at_ms: 1,
            finished_at_ms: 2,
        };
        let collector = Collector::default();
        let report = ProtocolReplay::new(artifacts)
            .replay(&episode, &collector)
            .await
            .expect("应回放");
        assert_eq!(report.event_count, 2);
        assert!(report.finished);
        assert_eq!(
            *collector.0.lock().expect("锁不应中毒"),
            vec!["run_started", "run_finished"]
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
