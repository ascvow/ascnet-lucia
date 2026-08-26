//! 可信监督平面：Event Envelope、确定性 Detector、RunSupervisor 与 Outcome Resolver。
//!
//! Supervisor 由 Recorder 驱动，只观察已经按 Episode 数据策略脱敏和收窄的
//! `AgentEvent` 流，把检测到的异常持久化为 Incident，并在运行收敛时产生初始
//! [`OutcomeRevision`]。它不修改 Agent 行为，只产生证据；真正的处置由聚合器与
//! Outbox 在 Turn 结束后决定。

use agent_core::{AgentEvent, AgentEventKind};
use agent_evolution_protocol::{
    default_component, default_recoverability, DetectorRef, EpisodeId, EventEnvelope, EventId,
    GenomeRevisionId, Incident, IncidentId, IncidentKind, IncidentStatus, Outcome, OutcomeRevision,
    OutcomeRevisionId, OutcomeSource, RunId, Severity,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// 事件监督与收敛时产生的完整证据包。
#[derive(Debug, Clone)]
pub struct SupervisionReport {
    /// 事件信封流；顺序即接收顺序。
    pub envelopes: Vec<EventEnvelope>,
    /// 检测到的全部 Incident。
    pub incidents: Vec<Incident>,
    /// 收敛时生成的初始 Outcome 修订；尚无足够证据时为 `None`。
    pub outcome_revision: Option<OutcomeRevision>,
}

/// 可信事件收集与异常检测器。
///
/// 每次运行一个实例，先调用 [`RunSupervisor::observe`] 记录事件，再调用
/// [`RunSupervisor::finalize`] 收敛终态。
#[derive(Debug, Clone)]
pub struct RunSupervisor {
    run_id: RunId,
    episode_id: EpisodeId,
    genome_revision_id: GenomeRevisionId,
    envelopes: Vec<EventEnvelope>,
    incidents: Vec<Incident>,
    /// ToolStarted 中按 call_id 记录的脱敏动作指纹。
    tool_fingerprints: BTreeMap<String, String>,
    /// 曾失败的动作指纹。
    failed_tool_actions: BTreeSet<String>,
    /// 失败后又成功的动作指纹。
    recovered_tool_actions: BTreeSet<String>,
    /// 最近一次失败动作，用于检测连续重复。
    last_failed_action: Option<String>,
    /// 已发出 incident 的重复动作键，避免无限重复。
    flagged_loops: BTreeSet<String>,
}

impl RunSupervisor {
    /// 创建绑定单次运行的 Supervisor。
    pub fn new(run_id: RunId, episode_id: EpisodeId, genome_revision_id: GenomeRevisionId) -> Self {
        Self {
            run_id,
            episode_id,
            genome_revision_id,
            envelopes: Vec::new(),
            incidents: Vec::new(),
            tool_fingerprints: BTreeMap::new(),
            failed_tool_actions: BTreeSet::new(),
            recovered_tool_actions: BTreeSet::new(),
            last_failed_action: None,
            flagged_loops: BTreeSet::new(),
        }
    }

    /// 记录一条已经按 Episode 数据策略脱敏和收窄的事件，返回信封与可能触发的
    /// Incident。
    ///
    /// 信封序号为当前已收事件数 + 1，因此同一 Supervisor 内严格单调递增。
    /// 直接调用者必须先完成脱敏；常规运行应通过 `EpisodeRecorder` 驱动本方法。
    ///
    /// # Errors
    ///
    /// run_id 与绑定不一致时返回 [`SupervisionError::MixedRun`]。
    pub fn observe(
        &mut self,
        event: &AgentEvent,
    ) -> Result<(EventEnvelope, Vec<Incident>), SupervisionError> {
        self.observe_with_event_id(event, EventId::generate())
    }

    /// 使用 Recorder 已分配的可信事件 ID 记录事件。
    ///
    /// 该入口让 Episode Event Stream、Event Envelope 与 Incident 引用同一个事件 ID，
    /// 避免监督证据指向无法从规范事件流找到的另一套标识。
    ///
    /// # Errors
    ///
    /// run_id 与绑定不一致时返回 [`SupervisionError::MixedRun`]。
    pub fn observe_with_event_id(
        &mut self,
        event: &AgentEvent,
        event_id: EventId,
    ) -> Result<(EventEnvelope, Vec<Incident>), SupervisionError> {
        if event.run_id != self.run_id.as_str() {
            return Err(SupervisionError::MixedRun {
                expected: self.run_id.clone(),
                actual: event.run_id.clone(),
            });
        }
        let envelope = EventEnvelope {
            event_id,
            run_id: self.run_id.clone(),
            episode_id: self.episode_id.clone(),
            sequence: self.envelopes.len() as u64 + 1,
            span_id: None,
            parent_span_id: None,
            agent_execution_id: None,
            genome_revision_id: self.genome_revision_id.clone(),
            timestamp_ms: event.timestamp_ms,
            kind: event_kind_name(&event.kind).to_string(),
            step: event.step as u64,
            payload: event.payload.clone(),
        };
        let incidents = self.detect(&envelope, event);
        self.envelopes.push(envelope);
        self.incidents.extend(incidents.iter().cloned());
        Ok((self.envelopes.last().expect("刚写入").clone(), incidents))
    }

    /// 收敛运行，返回监督报告。
    ///
    /// 没有 `RunFinished` 时视为基础设施失败，不产生 `Unverifiable` 的假象。
    pub fn finalize(self) -> SupervisionReport {
        let outcome_revision = self.initial_outcome_revision();
        SupervisionReport {
            envelopes: self.envelopes,
            incidents: self.incidents,
            outcome_revision,
        }
    }

    /// 依据已收事件给出初始 Outcome 修订；证据不足时不产生修订。
    fn initial_outcome_revision(&self) -> Option<OutcomeRevision> {
        let finished = self
            .envelopes
            .iter()
            .find(|envelope| envelope.kind == "run_finished")?;
        let cancelled = finished
            .payload
            .get("cancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if cancelled {
            return Some(OutcomeRevision {
                revision_id: OutcomeRevisionId::generate(),
                episode_id: self.episode_id.clone(),
                supersedes: None,
                outcome: Outcome::Cancelled,
                source: OutcomeSource::Runtime,
                reason: "用户或控制器取消".into(),
                feedback: None,
            });
        }

        let tool_failures = self
            .incidents
            .iter()
            .filter(|incident| {
                matches!(
                    incident.kind,
                    IncidentKind::ToolExecutionFailed | IncidentKind::ToolArgumentInvalid
                )
            })
            .count();
        let recovered = self.recovered_tool_failures();
        let safety_failures = self
            .incidents
            .iter()
            .filter(|incident| incident.severity == Severity::Critical)
            .count();

        let (outcome, reason) = if safety_failures > 0 {
            (
                Outcome::SafetyFailure,
                "出现 Critical 级安全或边界 Incident".to_string(),
            )
        } else if tool_failures > 0 && recovered == self.failed_tool_actions.len() {
            (
                Outcome::Unverifiable,
                "工具失败均在预算内被恢复，但缺少可信 Verifier".to_string(),
            )
        } else if tool_failures > 0 {
            (
                Outcome::Unverifiable,
                "存在未完全恢复的工具失败，需延迟证据判定".to_string(),
            )
        } else {
            (
                Outcome::Unverifiable,
                "缺少可信 Verifier，不能推断任务成功".to_string(),
            )
        };

        Some(OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: self.episode_id.clone(),
            supersedes: None,
            outcome,
            source: OutcomeSource::DeterministicRule,
            reason,
            feedback: None,
        })
    }

    /// 统计在失败后又出现成功结果的工具调用数。
    fn recovered_tool_failures(&self) -> usize {
        self.recovered_tool_actions.len()
    }

    /// 基于确定性规则检测单条事件是否触发 Incident。
    fn detect(&mut self, envelope: &EventEnvelope, event: &AgentEvent) -> Vec<Incident> {
        let mut incidents = Vec::new();
        match event.kind {
            AgentEventKind::ToolStarted => {
                let call_id = envelope
                    .payload
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !call_id.is_empty() {
                    let name = envelope
                        .payload
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let args = envelope
                        .payload
                        .get("args")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    self.tool_fingerprints.insert(
                        call_id.to_string(),
                        format!("{name}:{}", canonical_json(&args)),
                    );
                }
            }
            AgentEventKind::ToolFinished => {
                let call_id = envelope
                    .payload
                    .get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = envelope
                    .payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let is_error = envelope
                    .payload
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                let action_fingerprint = self
                    .tool_fingerprints
                    .remove(&call_id)
                    .unwrap_or_else(|| name.clone());
                if is_error {
                    self.failed_tool_actions.insert(action_fingerprint.clone());
                } else if self.failed_tool_actions.contains(&action_fingerprint) {
                    self.recovered_tool_actions
                        .insert(action_fingerprint.clone());
                }

                if name == "unknown"
                    || envelope.payload.get("content").is_some_and(|content| {
                        content
                            .as_str()
                            .is_some_and(|text| text.contains("unknown tool"))
                    })
                {
                    incidents.push(self.new_incident(
                        envelope,
                        IncidentKind::ToolNotFound,
                        Severity::Warning,
                        DetectorRef::ToolSchema,
                    ));
                } else if is_error {
                    incidents.push(self.new_incident(
                        envelope,
                        IncidentKind::ToolExecutionFailed,
                        Severity::Warning,
                        DetectorRef::ToolExecution,
                    ));
                }

                // 重复动作检测：同一工具与脱敏参数连续失败两次以上。
                if is_error {
                    if self.last_failed_action.as_ref() == Some(&action_fingerprint) {
                        let key = format!("{}:{}", self.run_id, action_fingerprint);
                        if self.flagged_loops.insert(key) {
                            incidents.push(self.new_incident(
                                envelope,
                                IncidentKind::LoopDetected,
                                Severity::Error,
                                DetectorRef::LoopDetection,
                            ));
                        }
                    }
                    self.last_failed_action = Some(action_fingerprint);
                } else {
                    self.last_failed_action = None;
                }
            }
            AgentEventKind::StepLimitReached => {
                incidents.push(self.new_incident(
                    envelope,
                    IncidentKind::StepLimitExceeded,
                    Severity::Error,
                    DetectorRef::ResourceBudget,
                ));
            }
            _ => {}
        }
        incidents
    }

    /// 构造一条已校验的 Incident。
    fn new_incident(
        &self,
        envelope: &EventEnvelope,
        kind: IncidentKind,
        severity: Severity,
        detector: DetectorRef,
    ) -> Incident {
        let incident = Incident {
            incident_id: IncidentId::generate(),
            episode_id: self.episode_id.clone(),
            observed_event_id: envelope.event_id.clone(),
            kind,
            severity,
            recoverability: default_recoverability(kind),
            component: default_component(kind),
            detector,
            evidence: vec![envelope.event_id.clone()],
            status: IncidentStatus::Observed,
        };
        debug_assert!(incident.validate().is_ok());
        incident
    }
}

/// 递归规范化 JSON 对象键顺序，使动作指纹不受模型参数字段顺序影响。
fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(entries) => {
            let mut keys = entries.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut output = serde_json::Map::new();
            for key in keys {
                output.insert(key.clone(), canonical_json(&entries[key]));
            }
            serde_json::Value::Object(output)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_json).collect())
        }
        _ => value.clone(),
    }
}

/// 监督过程中的错误。
#[derive(Debug, Error)]
pub enum SupervisionError {
    /// 事件 run_id 与绑定不一致。
    #[error("Supervisor 收到混合运行事件：期望 {expected}，实际 {actual}")]
    MixedRun {
        /// 绑定的 run ID。
        expected: RunId,
        /// 实际收到的 run ID。
        actual: String,
    },
}

/// 返回 AgentEventKind 的稳定 serde 名称。
pub(crate) fn event_kind_name(kind: &AgentEventKind) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::AgentEvent;
    use serde_json::json;

    fn run_id() -> RunId {
        RunId::generate()
    }

    fn episode_id() -> EpisodeId {
        EpisodeId::generate()
    }

    fn genome_revision_id() -> GenomeRevisionId {
        GenomeRevisionId::generate()
    }

    fn tool_finished(run_id: &RunId, call_id: &str, name: &str, is_error: bool) -> AgentEvent {
        AgentEvent::new(
            run_id.as_str(),
            AgentEventKind::ToolFinished,
            0,
            json!({
                "call_id": call_id,
                "name": name,
                "is_error": is_error,
                "content": if is_error { "EACCES" } else { "ok" },
            }),
        )
    }

    /// 构造带完整动作参数的工具开始事件，用于验证动作指纹不依赖调用 ID。
    fn tool_started(
        run_id: &RunId,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
    ) -> AgentEvent {
        AgentEvent::new(
            run_id.as_str(),
            AgentEventKind::ToolStarted,
            0,
            json!({
                "id": call_id,
                "name": name,
                "args": args,
            }),
        )
    }

    #[test]
    fn assigns_monotonic_sequences_and_rejects_mixed_runs() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        let first = supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .expect("应记录");
        let second = supervisor
            .observe(&tool_finished(&run_id, "call-1", "read_file", false))
            .expect("应记录");
        assert_eq!(first.0.sequence, 1);
        assert_eq!(second.0.sequence, 2);

        let wrong = RunId::generate();
        let error = supervisor
            .observe(&AgentEvent::new(
                wrong.as_str(),
                AgentEventKind::TurnStarted,
                0,
                json!({}),
            ))
            .expect_err("混合 run 应被拒绝");
        assert!(matches!(error, SupervisionError::MixedRun { .. }));
    }

    #[test]
    fn tool_error_then_success_remains_unverifiable_without_verifier() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .expect("应记录");
        supervisor
            .observe(&tool_finished(&run_id, "call-1", "write_file", true))
            .expect("应记录");
        supervisor
            .observe(&tool_finished(&run_id, "call-1", "write_file", false))
            .expect("应记录");
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunFinished,
                1,
                json!({"steps_used": 1}),
            ))
            .expect("应记录");

        let report = supervisor.finalize();
        assert_eq!(report.incidents.len(), 1);
        assert_eq!(report.incidents[0].kind, IncidentKind::ToolExecutionFailed);
        let revision = report.outcome_revision.expect("应有初始修订");
        assert_eq!(revision.outcome, Outcome::Unverifiable);
        assert!(revision.reason.contains("缺少可信 Verifier"));
    }

    #[test]
    fn unrecovered_tool_failure_stays_unverifiable() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .expect("应记录");
        supervisor
            .observe(&tool_finished(&run_id, "call-1", "write_file", true))
            .expect("应记录");
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunFinished,
                1,
                json!({"steps_used": 1}),
            ))
            .expect("应记录");

        let report = supervisor.finalize();
        let revision = report.outcome_revision.expect("应有初始修订");
        assert_eq!(revision.outcome, Outcome::Unverifiable);
    }

    #[test]
    fn cancelled_run_outcome_is_cancelled() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .expect("应记录");
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunFinished,
                0,
                json!({"cancelled": true}),
            ))
            .expect("应记录");

        let report = supervisor.finalize();
        let revision = report.outcome_revision.expect("应有初始修订");
        assert_eq!(revision.outcome, Outcome::Cancelled);
        assert_eq!(revision.source, OutcomeSource::Runtime);
    }

    /// 验证不同 call_id 的同一工具动作连续失败时只产生一次循环异常。
    #[test]
    fn repeated_tool_failure_flags_loop_once() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        supervisor
            .observe(&AgentEvent::new(
                run_id.as_str(),
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .expect("应记录");
        let calls = [
            (
                "call-1",
                json!({"path": "workspace/result.txt", "mode": "append"}),
            ),
            (
                "call-2",
                json!({"mode": "append", "path": "workspace/result.txt"}),
            ),
            (
                "call-3",
                json!({"path": "workspace/result.txt", "mode": "append"}),
            ),
        ];
        for (call_id, args) in calls {
            supervisor
                .observe(&tool_started(&run_id, call_id, "write_file", args))
                .expect("应记录工具开始");
            supervisor
                .observe(&tool_finished(&run_id, call_id, "write_file", true))
                .expect("应记录工具失败");
        }

        let report = supervisor.finalize();
        let loops = report
            .incidents
            .iter()
            .filter(|incident| incident.kind == IncidentKind::LoopDetected)
            .count();
        assert_eq!(loops, 1);
    }
}
