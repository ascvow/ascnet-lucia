//! Issue 聚合与处置路由。
//!
//! 单次普通错误只产生 [`FailureDisposition::Observe`]；同一指纹的失败达到阈值后
//! 才聚合为 [`EvolutionIssue`] 并进入 [`FailureDisposition::EvolutionCandidate`]。
//! 安全相关失败单次即 [`FailureDisposition::SecurityIncident`]。

use agent_evolution_protocol::{
    default_disposition, DiagnosticStatus, EpisodeId, EvolutionIssue, EvolutionIssueId,
    FailureDisposition, FailureFingerprint, FailureKind, FailureRecord,
};
use std::collections::{BTreeMap, BTreeSet};

/// 内存版 Issue 聚合器；进程重启后由持久化 Outbox 重建。
///
/// 聚合键完全由 [`FailureFingerprint::stable_key`] 决定，与插入顺序无关。
#[derive(Debug, Default)]
pub struct IssueAggregator {
    issues: BTreeMap<String, EvolutionIssue>,
    /// 每个指纹已出现的不同 Episode；同一 Episode 的重复 Incident 只计一次。
    occurrences: BTreeMap<String, BTreeSet<EpisodeId>>,
}

impl IssueAggregator {
    /// 创建空聚合器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 录入一条失败记录，返回该指纹对应的 Issue 快照与建议处置。
    ///
    /// 已有 Issue 会合并证据 Episode 与事件；未达到聚合阈值前 Issue 处于
    /// [`DiagnosticStatus::Observed`]，达到后进入 [`DiagnosticStatus::Clustered`]。
    /// 返回的 [`EvolutionIssue`] 是独立快照，调用方无需持有聚合器锁。
    pub fn record(
        &mut self,
        record: &FailureRecord,
        episode_id: &EpisodeId,
        genome_digest: &agent_evolution_protocol::GenomeDigest,
    ) -> (EvolutionIssue, FailureDisposition) {
        self.record_with_issue_id(record, episode_id, genome_digest, None)
    }

    /// 使用持久化 Issue ID 录入失败记录，供只追加观察日志在进程重启后重建聚合状态。
    pub(crate) fn record_with_issue_id(
        &mut self,
        record: &FailureRecord,
        episode_id: &EpisodeId,
        genome_digest: &agent_evolution_protocol::GenomeDigest,
        issue_id: Option<EvolutionIssueId>,
    ) -> (EvolutionIssue, FailureDisposition) {
        let fingerprint = fingerprint_for(record, genome_digest);
        let key = fingerprint.stable_key();
        let occurrences = self.occurrences.entry(key.clone()).or_default();
        occurrences.insert(episode_id.clone());
        let occurrence_count = occurrences.len();

        let issue = self.issues.entry(key).or_insert_with(|| EvolutionIssue {
            issue_id: issue_id.unwrap_or_else(EvolutionIssueId::generate),
            fingerprint,
            evidence_episode_ids: Vec::new(),
            evidence_events: Vec::new(),
            suspected_surface: None,
            root_cause_hypothesis: hypothesis(record),
            expected_behavior: expected_behavior(record),
            confidence: record.attribution.confidence,
            status: DiagnosticStatus::Observed,
        });

        if !issue.evidence_episode_ids.contains(episode_id) {
            issue.evidence_episode_ids.push(episode_id.clone());
        }
        for event in &record.attribution.evidence {
            if !issue.evidence_events.contains(event) {
                issue.evidence_events.push(event.clone());
            }
        }
        issue.confidence = issue.confidence.max(record.attribution.confidence);
        if matches!(
            record.attribution.failure_class,
            FailureKind::VerificationFailure | FailureKind::ContextLoss
        ) {
            issue.status = DiagnosticStatus::EligibleForEvolution;
        } else if occurrence_count >= 2 {
            issue.status = DiagnosticStatus::Clustered;
        }

        let disposition = route(record, occurrence_count);
        (issue.clone(), disposition)
    }

    /// 返回当前全部 Issue 的稳定快照。
    pub fn issues(&self) -> Vec<&EvolutionIssue> {
        self.issues.values().collect()
    }
}

/// 从失败记录和运行 Genome 生成稳定聚合指纹。
pub(crate) fn fingerprint_for(
    record: &FailureRecord,
    genome_digest: &agent_evolution_protocol::GenomeDigest,
) -> FailureFingerprint {
    FailureFingerprint {
        task_family: String::new(),
        failure_class: record.attribution.failure_class,
        component: format!("{:?}", record.attribution.method),
        tool: None,
        plugin: None,
        error_code: None,
        genome_digest: genome_digest.clone(),
        normalized_pattern: normalized_pattern(record),
    }
}

/// 生成供人阅读的根因假设。
fn hypothesis(record: &FailureRecord) -> String {
    format!(
        "检测到 {:?} 失败，疑似由 {:?} 引入",
        record.attribution.failure_class, record.attribution.detected_at
    )
}

/// 生成期望行为描述。
fn expected_behavior(record: &FailureRecord) -> String {
    match record.attribution.failure_class {
        FailureKind::ToolExecution => "工具应成功完成或返回可恢复错误".into(),
        FailureKind::ToolArgument => "工具参数应通过 Schema 校验".into(),
        FailureKind::PermissionFailure => "不应发生越权访问".into(),
        FailureKind::TerminationFailure => "运行应在预算内正常终止".into(),
        FailureKind::ModelFailure => "模型请求应成功返回协议合规响应".into(),
        _ => "运行应符合任务契约".into(),
    }
}

/// 归一化模式：当前版本只使用失败类别与归因方法，后续版本应加入
/// 工具签名与错误码的规范化文本。
fn normalized_pattern(record: &FailureRecord) -> String {
    format!(
        "{:?}/{:?}",
        record.attribution.failure_class, record.attribution.method
    )
}

/// 把失败记录路由到默认处置。
fn route(record: &FailureRecord, occurrences: usize) -> FailureDisposition {
    default_disposition(record.attribution.failure_class, occurrences)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        AttributionMethod, DiagnosticStatus, EpisodeId, EventId, FailureAttribution, FailureKind,
        FailureRecord, FailureRecordId, GenomeDigest,
    };

    fn record(kind: FailureKind) -> FailureRecord {
        FailureRecord {
            record_id: FailureRecordId::generate(),
            episode_id: EpisodeId::generate(),
            attribution: FailureAttribution {
                detected_at: EventId::generate(),
                suspected_origin: None,
                propagation_path: Vec::new(),
                decisive_step: None,
                failure_class: kind,
                confidence: 0.9,
                evidence: Vec::new(),
                method: AttributionMethod::DeterministicRule,
            },
            status: DiagnosticStatus::Observed,
        }
    }

    fn digest() -> GenomeDigest {
        GenomeDigest::from_sha256_hex("a".repeat(64)).expect("摘要应合法")
    }

    #[test]
    fn single_tool_error_is_observed_not_evolution() {
        let mut aggregator = IssueAggregator::new();
        let (issue, disposition) = aggregator.record(
            &record(FailureKind::ToolExecution),
            &EpisodeId::generate(),
            &digest(),
        );
        assert_eq!(disposition, FailureDisposition::Observe);
        assert_eq!(issue.status, DiagnosticStatus::Observed);
    }

    #[test]
    fn repeated_failures_cluster_into_evolution_candidate() {
        let mut aggregator = IssueAggregator::new();
        let digest = digest();
        let first = aggregator.record(
            &record(FailureKind::ToolExecution),
            &EpisodeId::generate(),
            &digest,
        );
        assert_eq!(first.1, FailureDisposition::Observe);

        let second = aggregator.record(
            &record(FailureKind::ToolExecution),
            &EpisodeId::generate(),
            &digest,
        );
        assert_eq!(second.1, FailureDisposition::EvolutionCandidate);
        assert_eq!(second.0.status, DiagnosticStatus::Clustered);
        assert_eq!(second.0.evidence_episode_ids.len(), 2);
    }

    #[test]
    fn safety_failure_is_security_incident_on_first_occurrence() {
        let mut aggregator = IssueAggregator::new();
        let (_, disposition) = aggregator.record(
            &record(FailureKind::PermissionFailure),
            &EpisodeId::generate(),
            &digest(),
        );
        assert_eq!(disposition, FailureDisposition::SecurityIncident);
    }
}
