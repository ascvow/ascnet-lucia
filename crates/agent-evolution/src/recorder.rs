//! `AgentEvent` 到不可变 Episode 的脱敏记录器。

use crate::{ArtifactStore, ArtifactStoreError, EpisodeStore, EpisodeStoreError};
use agent_core::{AgentEvent, AgentEventKind, EventSink};
use agent_evolution_protocol::{
    Episode, EpisodeDataPolicy, EpisodeEvent, EpisodeId, FailureClassification, FailureKind,
    GenomeRevisionId, Outcome, RawToolResultPolicy, RedactionRule, Redactor, ReplayabilityGrade,
    RunId, TaskDescriptor, UsageSummary, EPISODE_SCHEMA_VERSION, REDACTION_RULES_VERSION,
};
use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::Mutex;

/// 单次运行的 Episode Recorder 配置。
#[derive(Debug, Clone)]
pub struct EpisodeRecorderConfig {
    /// 运行开始前生成的强类型 Run ID；必须同时传给 Core。
    pub run_id: RunId,
    /// 应用层会话标识。
    pub session_id: String,
    /// 运行开始前固定的 Genome 修订。
    pub genome_revision_id: GenomeRevisionId,
    /// 脱敏任务描述。
    pub task: TaskDescriptor,
    /// Episode 数据策略；Recorder 会写入实际脱敏规则版本。
    pub data_policy: EpisodeDataPolicy,
    /// 正常 RunFinished 且未取消时使用的终态。
    ///
    /// 没有可信 Verifier 的在线运行应保持 [`Outcome::Unverifiable`]。
    pub completed_outcome: Outcome,
    /// 该事件流可支持的回放等级。
    pub replayability: ReplayabilityGrade,
}

impl EpisodeRecorderConfig {
    /// 构造默认不进入变异流程的在线运行配置。
    pub fn online(session_id: impl Into<String>, genome_revision_id: GenomeRevisionId) -> Self {
        Self {
            run_id: RunId::generate(),
            session_id: session_id.into(),
            genome_revision_id,
            task: TaskDescriptor::default(),
            data_policy: EpisodeDataPolicy::default(),
            completed_outcome: Outcome::Unverifiable,
            replayability: ReplayabilityGrade::Exact,
        }
    }
}

/// 将一个 Agent 运行收敛为 Episode 的事件 sink。
///
/// Recorder 只接受单个运行，检测到混合 run ID 会报错。`RunFinished` 到达时自动写入
/// Event Stream CAS 和 Episode Store；没有 `RunFinished` 的基础设施错误可由
/// [`EpisodeRecorder::finish`] 显式收敛。
pub struct EpisodeRecorder {
    config: EpisodeRecorderConfig,
    artifacts: Arc<dyn ArtifactStore>,
    episodes: Arc<dyn EpisodeStore>,
    redactor: Redactor,
    state: Mutex<RecorderState>,
}

#[derive(Debug, Default)]
struct RecorderState {
    source_run_id: Option<String>,
    run_id: Option<RunId>,
    events: Vec<EpisodeEvent>,
    applied_redactions: BTreeSet<RedactionRule>,
    finalized: Option<EpisodeId>,
}

impl EpisodeRecorder {
    /// 创建单次运行 Recorder。
    pub fn new(
        config: EpisodeRecorderConfig,
        artifacts: Arc<dyn ArtifactStore>,
        episodes: Arc<dyn EpisodeStore>,
    ) -> Self {
        Self {
            config,
            artifacts,
            episodes,
            redactor: Redactor::new(),
            state: Mutex::new(RecorderState::default()),
        }
    }

    /// 指定宿主主目录以强化私有路径脱敏。
    pub fn with_home(mut self, home: impl Into<String>) -> Self {
        self.redactor = self.redactor.with_home(home);
        self
    }

    /// 返回已经写入的 Episode ID；运行未结束时为 `None`。
    pub async fn episode_id(&self) -> Option<EpisodeId> {
        self.state.lock().await.finalized.clone()
    }

    /// 返回必须传给 `Agent::run_session_with_id` 的运行标识。
    pub fn run_id(&self) -> &RunId {
        &self.config.run_id
    }

    /// 显式收敛没有正常 `RunFinished` 的运行。
    ///
    /// # Errors
    ///
    /// 尚未收到任何事件、已经收敛、事件来自多个运行，或制品与 Episode 写入失败时
    /// 返回错误。调用方应把模型/工具环境故障传为 `InfrastructureFailure`，不要伪装成
    /// 候选能力失败。
    pub async fn finish(&self, outcome: Outcome) -> Result<EpisodeId, EpisodeRecorderError> {
        let mut state = self.state.lock().await;
        self.finalize_locked(&mut state, outcome).await
    }

    /// 把一条 Core 事件转换成符合数据策略的 Episode 事件。
    fn sanitize_event(
        &self,
        event: &AgentEvent,
        run_id: &RunId,
        applied: &mut BTreeSet<RedactionRule>,
    ) -> Option<EpisodeEvent> {
        // 隐藏思考增量既不构成公开响应，也不是可验证证据，必须彻底丢弃。
        if event.kind == AgentEventKind::ModelThinkingDelta {
            return None;
        }

        let payload = match event.kind {
            AgentEventKind::ToolFinished | AgentEventKind::ToolOutputDelta => {
                self.sanitize_tool_payload(event, applied)
            }
            _ => {
                let (payload, rules) = self.redactor.redact_json(&event.payload);
                applied.extend(rules);
                payload
            }
        };
        Some(EpisodeEvent {
            event_id: event.id.clone(),
            run_id: run_id.clone(),
            timestamp_ms: event.timestamp_ms,
            kind: event_kind_name(&event.kind).to_string(),
            step: event.step as u64,
            payload,
        })
    }

    /// 按 RawToolResultPolicy 收窄工具输出正文。
    fn sanitize_tool_payload(
        &self,
        event: &AgentEvent,
        applied: &mut BTreeSet<RedactionRule>,
    ) -> Value {
        match self.config.data_policy.raw_tool_results {
            RawToolResultPolicy::Discard => json!({
                "call_id": event.payload.get("call_id").and_then(Value::as_str),
                "name": event.payload.get("name").and_then(Value::as_str),
                "is_error": event.payload.get("is_error").and_then(Value::as_bool),
                "content_discarded": true,
            }),
            RawToolResultPolicy::StoreRedacted => {
                let (payload, rules) = self.redactor.redact_json(&event.payload);
                applied.extend(rules);
                payload
            }
            RawToolResultPolicy::StoreRaw => event.payload.clone(),
        }
    }

    /// 把内存事件流写为 NDJSON CAS，并追加 Episode Header。
    async fn finalize_locked(
        &self,
        state: &mut RecorderState,
        outcome: Outcome,
    ) -> Result<EpisodeId, EpisodeRecorderError> {
        if let Some(id) = &state.finalized {
            return Err(EpisodeRecorderError::AlreadyFinalized(id.clone()));
        }
        let run_id = state.run_id.clone().ok_or(EpisodeRecorderError::NoEvents)?;
        let first = state.events.first().ok_or(EpisodeRecorderError::NoEvents)?;
        let last = state.events.last().ok_or(EpisodeRecorderError::NoEvents)?;

        let mut stream = Vec::new();
        for event in &state.events {
            serde_json::to_writer(&mut stream, event)
                .map_err(EpisodeRecorderError::SerializeEvent)?;
            stream.push(b'\n');
        }
        let event_stream_ref = self.artifacts.put("application/x-ndjson", &stream).await?;
        let mut data_policy = self.config.data_policy.clone();
        data_policy.redaction_rules_version = Some(REDACTION_RULES_VERSION.to_string());
        let episode = Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: EpisodeId::generate(),
            run_id,
            session_id: self.config.session_id.clone(),
            genome_revision_id: self.config.genome_revision_id.clone(),
            task: self.config.task.clone(),
            event_stream_ref,
            environment_ref: None,
            outcome: Some(outcome),
            failures: classify_failures(&state.events),
            usage: usage_summary(&state.events),
            replayability: self.config.replayability,
            data_policy,
            event_count: state.events.len() as u64,
            started_at_ms: first.timestamp_ms,
            finished_at_ms: last.timestamp_ms,
        };
        self.episodes.append(&episode).await?;
        state.finalized = Some(episode.episode_id.clone());
        Ok(episode.episode_id)
    }
}

#[async_trait]
impl EventSink for EpisodeRecorder {
    async fn record(&self, event: &AgentEvent) -> AnyResult<()> {
        let mut state = self.state.lock().await;
        if state.finalized.is_some() {
            return Err(EpisodeRecorderError::EventAfterFinalization.into());
        }
        if let Some(source) = &state.source_run_id {
            if source != &event.run_id {
                return Err(EpisodeRecorderError::MixedRuns {
                    expected: source.clone(),
                    actual: event.run_id.clone(),
                }
                .into());
            }
        } else {
            if event.run_id != self.config.run_id.as_str() {
                return Err(EpisodeRecorderError::UnexpectedRunId {
                    expected: self.config.run_id.clone(),
                    actual: event.run_id.clone(),
                }
                .into());
            }
            state.source_run_id = Some(event.run_id.clone());
            state.run_id = Some(self.config.run_id.clone());
        }
        let run_id = state.run_id.clone().expect("首次事件已建立运行标识");
        let sanitized = self.sanitize_event(event, &run_id, &mut state.applied_redactions);
        if let Some(sanitized) = sanitized {
            state.events.push(sanitized);
        }
        if event.kind == AgentEventKind::RunFinished {
            let outcome = if event
                .payload
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Outcome::Cancelled
            } else {
                self.config.completed_outcome.clone()
            };
            self.finalize_locked(&mut state, outcome).await?;
        }
        Ok(())
    }
}

/// Episode Recorder 错误。
#[derive(Debug, thiserror::Error)]
pub enum EpisodeRecorderError {
    /// Recorder 未收到任何可持久化事件。
    #[error("Recorder 尚未收到可持久化事件")]
    NoEvents,
    /// 单次运行 Recorder 收到了另一 run 的事件。
    #[error("Recorder 收到混合运行事件：期望 {expected}，实际 {actual}")]
    MixedRuns {
        /// 首个事件的源 run ID。
        expected: String,
        /// 后续事件的源 run ID。
        actual: String,
    },
    /// 首事件的 run ID 与运行前绑定不一致。
    #[error("事件运行标识与 Recorder 绑定不一致：期望 {expected}，实际 {actual}")]
    UnexpectedRunId {
        /// Recorder 绑定的强类型 ID。
        expected: RunId,
        /// Core 事件中的实际 ID。
        actual: String,
    },
    /// Episode 已经持久化。
    #[error("Recorder 已收敛为 Episode：{0}")]
    AlreadyFinalized(EpisodeId),
    /// 收敛后又收到事件。
    #[error("Recorder 收敛后不能继续接收事件")]
    EventAfterFinalization,
    /// 事件序列化失败。
    #[error("序列化 Episode 事件失败：{0}")]
    SerializeEvent(serde_json::Error),
    /// Artifact CAS 失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// Episode Store 失败。
    #[error(transparent)]
    Episode(#[from] EpisodeStoreError),
}

/// 返回 AgentEventKind 的稳定 serde 名称。
fn event_kind_name(kind: &AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::RunStarted => "run_started",
        AgentEventKind::Extension => "extension",
        AgentEventKind::TurnStarted => "turn_started",
        AgentEventKind::ModelRequest => "model_request",
        AgentEventKind::ModelThinkingDelta => "model_thinking_delta",
        AgentEventKind::ModelTextDelta => "model_text_delta",
        AgentEventKind::ModelResponse => "model_response",
        AgentEventKind::BillingUsage => "billing_usage",
        AgentEventKind::ToolStarted => "tool_started",
        AgentEventKind::ToolOutputDelta => "tool_output_delta",
        AgentEventKind::ToolFinished => "tool_finished",
        AgentEventKind::ToolSkipped => "tool_skipped",
        AgentEventKind::StepLimitReached => "step_limit_reached",
        AgentEventKind::TurnFinished => "turn_finished",
        AgentEventKind::SteeringInjected => "steering_injected",
        AgentEventKind::FollowUpInjected => "follow_up_injected",
        AgentEventKind::RunFinished => "run_finished",
    }
}

/// 从确定性事件形态汇总 Usage。
fn usage_summary(events: &[EpisodeEvent]) -> UsageSummary {
    let finished = events
        .iter()
        .rev()
        .find(|event| event.kind == "run_finished");
    let usage = finished.and_then(|event| event.payload.get("usage"));
    UsageSummary {
        input_tokens: usage
            .and_then(|value| value.get("input_tokens"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|value| value.get("output_tokens"))
            .and_then(Value::as_u64),
        total_tokens: usage
            .and_then(|value| value.get("total_tokens"))
            .and_then(Value::as_u64),
        react_steps: finished
            .and_then(|event| event.payload.get("steps_used"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| events.iter().map(|event| event.step).max().unwrap_or(0)),
        elapsed_ms: events
            .first()
            .zip(events.last())
            .map(|(first, last)| last.timestamp_ms.saturating_sub(first.timestamp_ms)),
    }
}

/// 只使用可验证事件形态生成第一层规则分类。
fn classify_failures(events: &[EpisodeEvent]) -> Vec<FailureClassification> {
    let mut failures = Vec::new();
    if let Some(event) = events
        .iter()
        .find(|event| event.kind == "step_limit_reached")
    {
        failures.push(FailureClassification {
            kind: FailureKind::TerminationFailure,
            evidence_event_ids: vec![event.event_id.clone()],
            confidence: 1.0,
            rule_derived: true,
            model_assisted: false,
        });
    }
    for event in events.iter().filter(|event| event.kind == "tool_finished") {
        if event
            .payload
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            failures.push(FailureClassification {
                kind: FailureKind::ToolExecution,
                evidence_event_ids: vec![event.event_id.clone()],
                confidence: 1.0,
                rule_derived: true,
                model_assisted: false,
            });
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileArtifactStore, FileEpisodeStore};
    use agent_core::AgentEvent;
    use agent_evolution_protocol::{DataClass, EvolutionEligibility};
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-recorder-{}", Uuid::new_v4().simple()))
    }

    #[tokio::test]
    async fn records_redacted_exact_episode_and_discards_hidden_reasoning() {
        let root = temp_root();
        let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let mut config = EpisodeRecorderConfig::online("session-1", GenomeRevisionId::generate());
        config.data_policy = EpisodeDataPolicy::for_class(DataClass::Internal);
        config.data_policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
        let run_id = config.run_id.to_string();
        let recorder = EpisodeRecorder::new(config, artifacts.clone(), episodes.clone());

        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::RunStarted,
                0,
                json!({"authorization": "Bearer secret-value-123456"}),
            ))
            .await
            .expect("应记录开始");
        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::ModelThinkingDelta,
                0,
                json!({"delta": "私有推理"}),
            ))
            .await
            .expect("应丢弃思考");
        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::RunFinished,
                0,
                json!({"steps_used": 1, "usage": {"total_tokens": 3}}),
            ))
            .await
            .expect("应收敛");

        let episode_id = recorder.episode_id().await.expect("应有 Episode");
        let episode = episodes
            .get(&episode_id)
            .await
            .expect("应读取")
            .expect("记录应存在");
        assert_eq!(episode.event_count, 2);
        assert!(episode.data_policy.permits_mutation_input());
        let stream = artifacts
            .get(&episode.event_stream_ref.digest)
            .await
            .expect("应读取制品")
            .expect("制品应存在");
        let text = String::from_utf8(stream).expect("应为 UTF-8");
        assert!(!text.contains("secret-value"));
        assert!(!text.contains("私有推理"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
