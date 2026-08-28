//! 由监督证据生成失败归因的确定性规则。
//!
//! 归因只使用已经持久化的 Incident 与 Episode 失败分类，不引入模型判断；
//! 模型辅助归因此后可以作为附加层叠加，但置信度必须低于 1.0。

use agent_evolution_protocol::{
    AttributionMethod, ComponentRef, EpisodeId, FailureAttribution, FailureKind, FailureRecord,
    FailureRecordId, Incident, IncidentKind,
};

/// 从一次运行的 Incident 与 Episode 失败分类推导失败归因记录。
///
/// 规则按可信度从高到低匹配：第一条命中的规则决定 `failure_class` 与
/// `suspected_origin`。没有 Incident 时返回空列表，成功运行不产生归因。
pub fn attribute_failures(
    episode_id: &EpisodeId,
    incidents: &[Incident],
    episode_failures: &[agent_evolution_protocol::FailureClassification],
) -> Vec<FailureRecord> {
    let mut records = Vec::new();

    for incident in incidents {
        // Turn 内已经恢复的异常属于运行质量证据，不是失败归因或进化候选。
        if incident.status == agent_evolution_protocol::IncidentStatus::Recovered {
            continue;
        }
        let record = attribute_incident(incident, episode_failures);
        records.push(record);
    }

    // StepLimit 出现在 Episode 失败分类但无 Incident 时补一条终止失败归因。
    let has_step_limit_incident = incidents
        .iter()
        .any(|incident| incident.kind == IncidentKind::StepLimitExceeded);
    if !has_step_limit_incident {
        for failure in episode_failures {
            if failure.kind == FailureKind::TerminationFailure {
                let detected = failure
                    .evidence_event_ids
                    .first()
                    .and_then(|id| agent_evolution_protocol::EventId::new(id.clone()).ok());
                if let Some(detected_at) = detected {
                    records.push(FailureRecord {
                        record_id: FailureRecordId::generate(),
                        episode_id: episode_id.clone(),
                        attribution: FailureAttribution {
                            detected_at,
                            suspected_origin: None,
                            propagation_path: Vec::new(),
                            decisive_step: None,
                            failure_class: FailureKind::TerminationFailure,
                            confidence: 0.9,
                            evidence: Vec::new(),
                            method: AttributionMethod::DeterministicRule,
                        },
                        status: agent_evolution_protocol::DiagnosticStatus::Suspected,
                    });
                }
            }
        }
    }

    records
}

/// 单条 Incident 到失败归因的确定性映射。
fn attribute_incident(
    incident: &Incident,
    episode_failures: &[agent_evolution_protocol::FailureClassification],
) -> FailureRecord {
    let (failure_class, confidence) = classify_incident(incident, episode_failures);
    let decisive_step = episode_failures
        .iter()
        .find(|failure| failure.kind == failure_class)
        .and_then(|failure| failure.evidence_event_ids.first())
        .and_then(|id| agent_evolution_protocol::EventId::new(id.clone()).ok());

    let suspected_origin = if incident.kind == IncidentKind::ContextConstraintLost {
        incident
            .evidence
            .iter()
            .find(|event_id| *event_id != &incident.observed_event_id)
            .cloned()
    } else {
        Some(incident.observed_event_id.clone())
    };

    FailureRecord {
        record_id: FailureRecordId::generate(),
        episode_id: incident.episode_id.clone(),
        attribution: FailureAttribution {
            detected_at: incident.observed_event_id.clone(),
            suspected_origin,
            propagation_path: incident.evidence.clone(),
            decisive_step,
            failure_class,
            confidence,
            evidence: incident.evidence.clone(),
            method: AttributionMethod::DeterministicRule,
        },
        status: agent_evolution_protocol::DiagnosticStatus::Confirmed,
    }
}

/// 把 Incident 类别映射到稳定的失败类别与置信度。
fn classify_incident(
    incident: &Incident,
    _episode_failures: &[agent_evolution_protocol::FailureClassification],
) -> (FailureKind, f32) {
    let class = match incident.kind {
        IncidentKind::ModelTimeout
        | IncidentKind::ModelRateLimited
        | IncidentKind::ModelAuthenticationFailed
        | IncidentKind::InvalidModelResponse => FailureKind::ModelFailure,
        IncidentKind::ToolNotFound | IncidentKind::ToolArgumentInvalid => FailureKind::ToolArgument,
        IncidentKind::ToolExecutionFailed | IncidentKind::ToolContractViolation => {
            FailureKind::ToolExecution
        }
        IncidentKind::PluginTrap
        | IncidentKind::PluginFuelExhausted
        | IncidentKind::PluginMemoryLimit
        | IncidentKind::PluginContractViolation => FailureKind::PluginFailure,
        IncidentKind::PluginCapabilityDenied => FailureKind::PermissionFailure,
        IncidentKind::PermissionDenied
        | IncidentKind::PathBoundaryViolation
        | IncidentKind::ProcessBoundaryViolation
        | IncidentKind::SecretAccessAttempt => FailureKind::PermissionFailure,
        IncidentKind::StepLimitExceeded | IncidentKind::LoopDetected => {
            FailureKind::TerminationFailure
        }
        IncidentKind::BudgetExceeded => FailureKind::TerminationFailure,
        IncidentKind::CancellationRequested => FailureKind::Unknown,
        IncidentKind::ContextConstraintLost => FailureKind::ContextLoss,
        IncidentKind::PlanDeviation => FailureKind::PlanningFailure,
        IncidentKind::VerificationFailed => FailureKind::VerificationFailure,
        IncidentKind::StorageFailure | IncidentKind::ArtifactIntegrityFailure => {
            FailureKind::EnvironmentFailure
        }
        IncidentKind::Unknown => FailureKind::Unknown,
    };
    let confidence = match incident.component {
        ComponentRef::Model | ComponentRef::Tool | ComponentRef::PluginHost => 0.95,
        ComponentRef::Runtime | ComponentRef::Storage => 0.9,
        ComponentRef::Other(_) => 0.5,
    };
    (class, confidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        DetectorRef, EpisodeId, EventId, FailureClassification, Incident, IncidentId,
        IncidentStatus, Recoverability, Severity,
    };

    fn incident(kind: IncidentKind, component: ComponentRef) -> Incident {
        let observed = EventId::generate();
        Incident {
            incident_id: IncidentId::generate(),
            episode_id: EpisodeId::generate(),
            observed_event_id: observed.clone(),
            kind,
            severity: Severity::Warning,
            recoverability: Recoverability::Recoverable,
            component,
            detector: DetectorRef::ToolExecution,
            evidence: vec![observed],
            status: IncidentStatus::Observed,
        }
    }

    #[test]
    fn maps_tool_incident_to_tool_execution() {
        let incidents = vec![incident(
            IncidentKind::ToolExecutionFailed,
            ComponentRef::Tool,
        )];
        let failures = vec![FailureClassification {
            kind: FailureKind::ToolExecution,
            evidence_event_ids: vec![EventId::generate().to_string()],
            confidence: 1.0,
            rule_derived: true,
            model_assisted: false,
        }];

        let episode_id = incidents[0].episode_id.clone();
        let records = attribute_failures(&episode_id, &incidents, &failures);
        assert_eq!(records.len(), 1);
        let attribution = &records[0].attribution;
        assert_eq!(attribution.failure_class, FailureKind::ToolExecution);
        assert_eq!(attribution.method, AttributionMethod::DeterministicRule);
        assert_eq!(
            records[0].status,
            agent_evolution_protocol::DiagnosticStatus::Confirmed
        );
    }

    #[test]
    fn permission_denied_maps_to_permission_failure() {
        let incidents = vec![incident(
            IncidentKind::PermissionDenied,
            ComponentRef::Runtime,
        )];
        let episode_id = incidents[0].episode_id.clone();
        let records = attribute_failures(&episode_id, &incidents, &[]);
        assert_eq!(
            records[0].attribution.failure_class,
            FailureKind::PermissionFailure
        );
        assert_eq!(records[0].attribution.confidence, 0.9);
    }

    /// 插件契约违规属于插件维护，不能落入 Agent 行为变异。
    #[test]
    fn plugin_contract_violation_maps_to_plugin_failure() {
        let incidents = vec![incident(
            IncidentKind::PluginContractViolation,
            ComponentRef::PluginHost,
        )];
        let records = attribute_failures(&incidents[0].episode_id.clone(), &incidents, &[]);

        assert_eq!(
            records[0].attribution.failure_class,
            FailureKind::PluginFailure
        );
    }

    /// 插件越权属于安全事件，不得作为普通插件维护或 Evolution Candidate。
    #[test]
    fn plugin_capability_denial_maps_to_permission_failure() {
        let incidents = vec![incident(
            IncidentKind::PluginCapabilityDenied,
            ComponentRef::PluginHost,
        )];
        let records = attribute_failures(&incidents[0].episode_id.clone(), &incidents, &[]);

        assert_eq!(
            records[0].attribution.failure_class,
            FailureKind::PermissionFailure
        );
    }

    #[test]
    fn empty_incidents_yield_no_records() {
        assert!(attribute_failures(&EpisodeId::generate(), &[], &[]).is_empty());
    }
}
