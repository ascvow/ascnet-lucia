//! 可信监督平面：Event Envelope、确定性 Detector、RunSupervisor 与 Outcome Resolver。
//!
//! Supervisor 由 Recorder 驱动，只观察已经按 Episode 数据策略脱敏和收窄的
//! `AgentEvent` 流，把检测到的异常持久化为 Incident，并在运行收敛时产生初始
//! [`OutcomeRevision`]。它不修改 Agent 行为，只产生证据；真正的处置由聚合器与
//! Outbox 在 Turn 结束后决定。

use agent_core::ToolErrorKind;
use agent_core::{AgentEvent, AgentEventKind};
use agent_evolution_protocol::{
    default_component, default_recoverability, DetectorRef, EpisodeId, EventEnvelope, EventId,
    FailureKind, GenomeRevisionId, Incident, IncidentId, IncidentKind, IncidentStatus, Outcome,
    OutcomeResolution, OutcomeRevision, OutcomeRevisionId, OutcomeSource, RunId, Severity,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Host 注入 Outcome Resolver 输入时使用的稳定扩展事件名。
pub const OUTCOME_RESOLUTION_EVENT: &str = "evolution.outcome_resolution";
/// Plugin Host 写入 ToolResult 细节的可信故障键。
pub(crate) const PLUGIN_HOST_FAILURE_DETAIL_KEY: &str = "lucia_plugin_host_failure";

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
    /// 工具调用最近一次可信终态事件，供 Verifier 关联检测位置。
    tool_event_ids: BTreeMap<String, EventId>,
    /// 曾失败的动作指纹。
    failed_tool_actions: BTreeSet<String>,
    /// 失败后又成功的动作指纹。
    recovered_tool_actions: BTreeSet<String>,
    /// 最近一次失败动作，用于检测连续重复。
    last_failed_action: Option<String>,
    /// 已发出 incident 的重复动作键，避免无限重复。
    flagged_loops: BTreeSet<String>,
    /// 动作指纹对应的可恢复 Incident。
    action_incidents: BTreeMap<String, Vec<IncidentId>>,
    /// 最近一次真实 Context 压缩事件。
    latest_context_compression: Option<EventId>,
    /// Host 在 Turn 结束后提交的可信 Outcome Resolver 输入及其事件 ID。
    resolution: Option<(OutcomeResolution, EventId)>,
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
            tool_event_ids: BTreeMap::new(),
            failed_tool_actions: BTreeSet::new(),
            recovered_tool_actions: BTreeSet::new(),
            last_failed_action: None,
            flagged_loops: BTreeSet::new(),
            action_incidents: BTreeMap::new(),
            latest_context_compression: None,
            resolution: None,
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
        let incidents = self.detect(&envelope, event)?;
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

        let (outcome, source, reason) = if safety_failures > 0 {
            (
                Outcome::SafetyFailure,
                OutcomeSource::DeterministicRule,
                "出现 Critical 级安全或边界 Incident".to_string(),
            )
        } else if let Some((resolution, _)) = &self.resolution {
            let outcome = if resolution.outcome == Outcome::Success
                && tool_failures > 0
                && recovered == self.failed_tool_actions.len()
            {
                Outcome::SuccessWithRecovery
            } else {
                resolution.outcome.clone()
            };
            (outcome, resolution.source, resolution.reason.clone())
        } else if tool_failures > 0 && recovered == self.failed_tool_actions.len() {
            (
                Outcome::Unverifiable,
                OutcomeSource::DeterministicRule,
                "工具失败均在预算内被恢复，但缺少可信 Verifier".to_string(),
            )
        } else if tool_failures > 0 {
            (
                Outcome::Unverifiable,
                OutcomeSource::DeterministicRule,
                "存在未完全恢复的工具失败，需延迟证据判定".to_string(),
            )
        } else {
            (
                Outcome::Unverifiable,
                OutcomeSource::DeterministicRule,
                "缺少可信 Verifier，不能推断任务成功".to_string(),
            )
        };

        Some(OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: self.episode_id.clone(),
            supersedes: None,
            outcome,
            source,
            reason,
            feedback: None,
        })
    }

    /// 统计在失败后又出现成功结果的工具调用数。
    fn recovered_tool_failures(&self) -> usize {
        self.recovered_tool_actions.len()
    }

    /// 基于确定性规则检测单条事件是否触发 Incident。
    fn detect(
        &mut self,
        envelope: &EventEnvelope,
        event: &AgentEvent,
    ) -> Result<Vec<Incident>, SupervisionError> {
        let mut incidents = Vec::new();
        match event.kind {
            AgentEventKind::Extension => {
                self.detect_extension(envelope, &mut incidents)?;
            }
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
                if !call_id.is_empty() {
                    self.tool_event_ids
                        .insert(call_id.clone(), envelope.event_id.clone());
                }
                if is_error {
                    self.failed_tool_actions.insert(action_fingerprint.clone());
                } else if self.failed_tool_actions.contains(&action_fingerprint) {
                    self.recovered_tool_actions
                        .insert(action_fingerprint.clone());
                    if let Some(incident_ids) = self.action_incidents.get(&action_fingerprint) {
                        for incident in &mut self.incidents {
                            if incident_ids.contains(&incident.incident_id) {
                                incident.status = IncidentStatus::Recovered;
                                if !incident.evidence.contains(&envelope.event_id) {
                                    incident.evidence.push(envelope.event_id.clone());
                                }
                            }
                        }
                    }
                }

                let runtime_origin = envelope
                    .payload
                    .get("runtime_origin")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("legacy");
                let error_kind = envelope
                    .payload
                    .get("error_kind")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ToolErrorKind>(value).ok());
                let trusted_error_kind = trusted_tool_error_kind(runtime_origin, error_kind);
                let trusted_plugin_failure = (runtime_origin == "plugin")
                    .then(|| {
                        envelope
                            .payload
                            .pointer(&format!("/details/{PLUGIN_HOST_FAILURE_DETAIL_KEY}/kind"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(classify_plugin_host_failure)
                    })
                    .flatten();
                let classified =
                    trusted_plugin_failure.or_else(|| trusted_error_kind.map(classify_tool_error));

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
                    let (kind, severity, detector) = classified.unwrap_or((
                        IncidentKind::ToolExecutionFailed,
                        Severity::Warning,
                        DetectorRef::ToolExecution,
                    ));
                    let incident = self.new_incident(envelope, kind, severity, detector);
                    self.action_incidents
                        .entry(action_fingerprint.clone())
                        .or_default()
                        .push(incident.incident_id.clone());
                    incidents.push(incident);
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
        Ok(incidents)
    }

    /// 解析 Host Outcome 输入和 Context 压缩事件。
    ///
    /// 普通插件事件的 `source.type` 为 `plugin`，因此不能伪造 Host Outcome。Context
    /// 事件只作为疑似根因，真正的 `ContextLoss` 仍必须由可信 Verifier 判定。
    fn detect_extension(
        &mut self,
        envelope: &EventEnvelope,
        incidents: &mut Vec<Incident>,
    ) -> Result<(), SupervisionError> {
        let source_type = envelope
            .payload
            .pointer("/source/type")
            .and_then(serde_json::Value::as_str);
        let source_id = envelope
            .payload
            .pointer("/source/id")
            .and_then(serde_json::Value::as_str);
        let name = envelope
            .payload
            .get("name")
            .and_then(serde_json::Value::as_str);

        if source_type == Some("plugin")
            && source_id == Some("context")
            && matches!(
                name,
                Some("context.compaction.completed" | "context.micro_compaction.completed")
            )
        {
            self.latest_context_compression = Some(envelope.event_id.clone());
            return Ok(());
        }

        if source_type != Some("host") || name != Some(OUTCOME_RESOLUTION_EVENT) {
            return Ok(());
        }
        let resolution: OutcomeResolution = serde_json::from_value(
            envelope
                .payload
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(SupervisionError::InvalidResolutionPayload)?;
        resolution
            .validate()
            .map_err(|error| SupervisionError::InvalidResolution(error.to_string()))?;
        if self.resolution.is_some() {
            return Err(SupervisionError::DuplicateResolution);
        }

        if resolution.outcome == Outcome::TaskFailure {
            let failure_kind = resolution
                .failure_kind
                .expect("OutcomeResolution 校验保证 TaskFailure 有类别");
            let related_tool_event = resolution
                .related_tool_call_id
                .as_ref()
                .and_then(|call_id| self.tool_event_ids.get(call_id))
                .cloned();
            if resolution.related_tool_call_id.is_some() && related_tool_event.is_none() {
                return Err(SupervisionError::UnknownRelatedToolCall(
                    resolution.related_tool_call_id.clone().unwrap_or_default(),
                ));
            }
            // 插件、权限、沙箱、Runtime 与环境故障必须依赖各自的可信 Incident 进入人工
            // 队列，不能再合成 VerificationFailed 后绕行到 Evolution Outbox。
            if !crate::episode_selection::is_behavior_evolution_failure(failure_kind) {
                self.resolution = Some((resolution, envelope.event_id.clone()));
                return Ok(());
            }
            let (kind, observed_event_id, mut evidence) =
                if failure_kind == FailureKind::ContextLoss {
                    let detected = related_tool_event
                        .ok_or(SupervisionError::ContextLossRequiresRelatedToolCall)?;
                    let origin = self
                        .latest_context_compression
                        .clone()
                        .ok_or(SupervisionError::ContextLossRequiresCompression)?;
                    (
                        IncidentKind::ContextConstraintLost,
                        detected.clone(),
                        vec![detected, origin, envelope.event_id.clone()],
                    )
                } else {
                    (
                        IncidentKind::VerificationFailed,
                        envelope.event_id.clone(),
                        related_tool_event
                            .into_iter()
                            .chain(std::iter::once(envelope.event_id.clone()))
                            .collect(),
                    )
                };
            let mut seen = BTreeSet::new();
            evidence.retain(|event_id| seen.insert(event_id.clone()));
            let incident = Incident {
                incident_id: IncidentId::generate(),
                episode_id: self.episode_id.clone(),
                observed_event_id,
                kind,
                severity: Severity::Error,
                recoverability: default_recoverability(kind),
                component: default_component(kind),
                detector: DetectorRef::Custom("trusted_outcome_resolver".into()),
                evidence,
                status: IncidentStatus::Unrecovered,
            };
            incident
                .validate()
                .map_err(|error| SupervisionError::InvalidResolution(error.to_string()))?;
            incidents.push(incident);
        }
        self.resolution = Some((resolution, envelope.event_id.clone()));
        Ok(())
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
    /// Host Outcome 输入不是合法 JSON 协议。
    #[error("解析 OutcomeResolution 失败：{0}")]
    InvalidResolutionPayload(serde_json::Error),
    /// Host Outcome 输入违反协议不变量。
    #[error("OutcomeResolution 不合法：{0}")]
    InvalidResolution(String),
    /// 一次运行只能提交一个初始可信 Outcome 输入。
    #[error("同一次运行不能重复提交 OutcomeResolution")]
    DuplicateResolution,
    /// Outcome 输入引用了不存在的工具调用。
    #[error("OutcomeResolution 引用了未知工具调用：{0}")]
    UnknownRelatedToolCall(String),
    /// ContextLoss 必须指向检测到约束丢失的工具调用。
    #[error("ContextLoss 必须关联一个已记录的工具调用")]
    ContextLossRequiresRelatedToolCall,
    /// ContextLoss 必须有更早的真实 Context 压缩事件。
    #[error("ContextLoss 缺少可关联的 Context 压缩事件")]
    ContextLossRequiresCompression,
}

/// 把可信 ToolResult 类别映射为监督 Incident。
fn classify_tool_error(kind: ToolErrorKind) -> (IncidentKind, Severity, DetectorRef) {
    match kind {
        ToolErrorKind::UnknownTool => (
            IncidentKind::ToolNotFound,
            Severity::Warning,
            DetectorRef::ToolSchema,
        ),
        ToolErrorKind::InvalidArguments => (
            IncidentKind::ToolArgumentInvalid,
            Severity::Warning,
            DetectorRef::ToolSchema,
        ),
        ToolErrorKind::PermissionDenied | ToolErrorKind::PolicyDenied => (
            IncidentKind::PermissionDenied,
            Severity::Critical,
            DetectorRef::PermissionDenied,
        ),
        ToolErrorKind::PathBoundaryViolation => (
            IncidentKind::PathBoundaryViolation,
            Severity::Critical,
            DetectorRef::PermissionDenied,
        ),
        ToolErrorKind::ProcessBoundaryViolation => (
            IncidentKind::ProcessBoundaryViolation,
            Severity::Critical,
            DetectorRef::PermissionDenied,
        ),
        ToolErrorKind::SecretAccessAttempt => (
            IncidentKind::SecretAccessAttempt,
            Severity::Critical,
            DetectorRef::PermissionDenied,
        ),
        ToolErrorKind::Execution | ToolErrorKind::Cancelled => (
            IncidentKind::ToolExecutionFailed,
            Severity::Warning,
            DetectorRef::ToolExecution,
        ),
    }
}

/// 将 Plugin Host 注入的稳定故障名映射为可信 Incident；未知名称保持不受信任。
pub(crate) fn classify_plugin_host_failure(
    kind: &str,
) -> Option<(IncidentKind, Severity, DetectorRef)> {
    match kind {
        "trap" => Some((
            IncidentKind::PluginTrap,
            Severity::Error,
            DetectorRef::PluginTrap,
        )),
        "fuel_exhausted" => Some((
            IncidentKind::PluginFuelExhausted,
            Severity::Error,
            DetectorRef::PluginTrap,
        )),
        "memory_limit" => Some((
            IncidentKind::PluginMemoryLimit,
            Severity::Error,
            DetectorRef::PluginTrap,
        )),
        "capability_denied" => Some((
            IncidentKind::PluginCapabilityDenied,
            Severity::Critical,
            DetectorRef::PermissionDenied,
        )),
        "contract_violation" => Some((
            IncidentKind::PluginContractViolation,
            Severity::Error,
            DetectorRef::PluginTrap,
        )),
        _ => None,
    }
}

/// 根据 Core 注入的运行来源收窄可用于行为归因的工具错误类别。
///
/// Plugin Host 会先清洗 Guest 结果；这里再次拒绝 Guest 自报的权限与边界类别，避免归档
/// 导入或其他扩展绕过 Host。参数、普通执行与取消错误仍可用于 Prompt、Skill 或 Policy
/// 改进。
pub(crate) fn trusted_tool_error_kind(
    runtime_origin: &str,
    error_kind: Option<ToolErrorKind>,
) -> Option<ToolErrorKind> {
    match runtime_origin {
        "native" | "runtime" | "runtime_policy" => error_kind,
        "plugin" => error_kind.filter(|kind| {
            matches!(
                kind,
                ToolErrorKind::UnknownTool
                    | ToolErrorKind::InvalidArguments
                    | ToolErrorKind::Execution
                    | ToolErrorKind::Cancelled
            )
        }),
        _ => None,
    }
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
    use agent_evolution_protocol::ComponentRef;
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

    /// 只有 Core 标记为插件来源且携带 Host 专用细节的失败才能形成插件 Incident。
    #[test]
    fn trusted_plugin_host_failure_creates_plugin_incident() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        let event = AgentEvent::new(
            run_id.as_str(),
            AgentEventKind::ToolFinished,
            0,
            json!({
                "call_id": "plugin-call",
                "name": "plugin.tool",
                "is_error": true,
                "error_kind": "execution",
                "runtime_origin": "plugin",
                "details": {
                    "lucia_plugin_host_failure": {
                        "kind": "fuel_exhausted",
                        "plugin_id": "plugin"
                    }
                }
            }),
        );

        let (_, incidents) = supervisor.observe(&event).expect("应接收 Host 插件故障");

        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].kind, IncidentKind::PluginFuelExhausted);
        assert_eq!(incidents[0].component, ComponentRef::PluginHost);
    }

    /// Guest 自报的安全错误类别没有 Host 专用标记时只能作为普通工具失败。
    #[test]
    fn guest_plugin_error_kind_is_not_trusted() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        let event = AgentEvent::new(
            run_id.as_str(),
            AgentEventKind::ToolFinished,
            0,
            json!({
                "call_id": "forged-call",
                "name": "plugin.tool",
                "is_error": true,
                "error_kind": "permission_denied",
                "runtime_origin": "plugin"
            }),
        );

        let (_, incidents) = supervisor.observe(&event).expect("应接收 Guest 工具失败");

        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].kind, IncidentKind::ToolExecutionFailed);
    }

    /// 插件参数错误描述的是 Agent 调用方式，可以进入 Agent 侧行为归因。
    #[test]
    fn plugin_argument_error_remains_agent_behavior_incident() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        let event = AgentEvent::new(
            run_id.as_str(),
            AgentEventKind::ToolFinished,
            0,
            json!({
                "call_id": "argument-call",
                "name": "plugin.tool",
                "is_error": true,
                "error_kind": "invalid_arguments",
                "runtime_origin": "plugin"
            }),
        );

        let (_, incidents) = supervisor.observe(&event).expect("应接收插件参数失败");

        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].kind, IncidentKind::ToolArgumentInvalid);
    }

    /// 可信终态确认插件失败时不能再合成可进化的 VerificationFailed Incident。
    #[test]
    fn plugin_failure_resolution_does_not_create_behavior_incident() {
        let run_id = run_id();
        let mut supervisor = RunSupervisor::new(run_id.clone(), episode_id(), genome_revision_id());
        let resolution = OutcomeResolution::verified_failure(
            FailureKind::PluginFailure,
            "Plugin Host 确认插件实现失败",
        );
        let event = AgentEvent::new(
            run_id.as_str(),
            AgentEventKind::Extension,
            1,
            json!({
                "source": { "type": "host" },
                "name": OUTCOME_RESOLUTION_EVENT,
                "data": resolution,
            }),
        );

        let (_, incidents) = supervisor.observe(&event).expect("应接收插件失败终态");

        assert!(incidents.is_empty());
    }
}
