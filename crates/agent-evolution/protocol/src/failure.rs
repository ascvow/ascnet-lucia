//! 失败归因、问题聚合与进化路由协议。
//!
//! 失败归因区分"错误被发现的位置"与"错误最早被引入的位置"。聚合把重复发生的
//! 同类失败合并为稳定 Issue，再由 [`FailureDisposition`] 决定处置去向。
//! 单次普通错误默认只观察，不进入进化队列。

use crate::episode::FailureKind;
use crate::ids::{EpisodeId, EventId, EvolutionIssueId, FailureRecordId, GenomeDigest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 失败被定位的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionMethod {
    /// 由确定性规则直接定位。
    DeterministicRule,
    /// 由事件依赖图回溯。
    DependencyGraph,
    /// 命中已知失败模式。
    KnownFailurePattern,
    /// 通过反事实回放确认。
    CounterfactualReplay,
    /// 模型辅助分析。
    ModelAssisted,
    /// 人工复核。
    HumanReview,
}

impl AttributionMethod {
    /// 判断该方法是否足以把失败归因标记为 Confirmed。
    pub fn is_deterministic(self) -> bool {
        matches!(
            self,
            Self::DeterministicRule
                | Self::DependencyGraph
                | Self::KnownFailurePattern
                | Self::CounterfactualReplay
        )
    }
}

/// 一次失败的结构化归因。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureAttribution {
    /// 错误被可信信号发现的位置。
    pub detected_at: EventId,
    /// 推测错误最早被引入的位置；证据不足时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspected_origin: Option<EventId>,
    /// 错误从起源到发现处的传播路径。
    #[serde(default)]
    pub propagation_path: Vec<EventId>,
    /// 直接导致最终失败的决定性步骤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisive_step: Option<EventId>,
    /// 失败类别。
    pub failure_class: FailureKind,
    /// `[0, 1]` 范围内的置信度。
    pub confidence: f32,
    /// 支撑证据事件。
    #[serde(default)]
    pub evidence: Vec<EventId>,
    /// 归因方法。
    pub method: AttributionMethod,
}

impl FailureAttribution {
    /// 校验不依赖存储的结构不变量。
    ///
    /// # Errors
    ///
    /// 置信度越界或模型辅助方法被标记为确定结论时返回错误。
    pub fn validate(&self) -> Result<(), InvalidFailure> {
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(InvalidFailure::InvalidConfidence);
        }
        if self.method == AttributionMethod::ModelAssisted && self.confidence >= 1.0 {
            return Err(InvalidFailure::ModelAssistedCannotBeCertain);
        }
        Ok(())
    }
}

/// 问题从首次发现到解决的统一状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    /// 确定发生了异常现象。
    Observed,
    /// 怀疑某步骤为根因，但尚无确定性证据。
    Suspected,
    /// 有确定性证据或反事实回放支持。
    Confirmed,
    /// 多个分析器结论冲突。
    Disputed,
    /// 最初认为有问题，后续证据表明是误报。
    FalsePositive,
    /// 已被聚合到重复问题。
    Clustered,
    /// 满足进化前置条件。
    EligibleForEvolution,
    /// 已有稳定 Candidate 修复。
    Resolved,
    /// 曾修复的问题重新出现。
    Regressed,
}

/// 一条失败证据记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// 记录标识。
    pub record_id: FailureRecordId,
    /// 所属 Episode。
    pub episode_id: EpisodeId,
    /// 结构化归因。
    pub attribution: FailureAttribution,
    /// 诊断状态。
    pub status: DiagnosticStatus,
}

/// 失败的稳定指纹。
///
/// 同一指纹的失败会被聚合，避免单次偶发错误直接触发进化。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FailureFingerprint {
    /// 任务族；空字符串表示尚未分类。
    #[serde(default)]
    pub task_family: String,
    /// 失败类别。
    pub failure_class: FailureKind,
    /// 主要涉及的组件。
    pub component: String,
    /// 可选工具名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// 可选插件 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// 可选稳定错误码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// 行为配置摘要。
    pub genome_digest: GenomeDigest,
    /// 归一化后的模式描述。
    pub normalized_pattern: String,
}

impl FailureFingerprint {
    /// 生成可排序的稳定键，用于聚合。
    pub fn stable_key(&self) -> String {
        format!(
            "{}|{:?}|{}|{}|{}|{}|{}|{}",
            self.task_family,
            self.failure_class,
            self.component,
            self.tool.as_deref().unwrap_or("-"),
            self.plugin.as_deref().unwrap_or("-"),
            self.error_code.as_deref().unwrap_or("-"),
            self.genome_digest.as_str(),
            self.normalized_pattern
        )
    }
}

/// 聚合后的稳定进化问题。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionIssue {
    /// Issue 标识。
    pub issue_id: EvolutionIssueId,
    /// 稳定指纹。
    pub fingerprint: FailureFingerprint,
    /// 支撑 Episode ID，去重后排序。
    #[serde(default)]
    pub evidence_episode_ids: Vec<EpisodeId>,
    /// 支撑事件。
    #[serde(default)]
    pub evidence_events: Vec<EventId>,
    /// 疑似可变表面；不确定时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspected_surface: Option<String>,
    /// 根因假设。
    pub root_cause_hypothesis: String,
    /// 期望行为。
    pub expected_behavior: String,
    /// `[0, 1]` 范围内的置信度。
    pub confidence: f32,
    /// 诊断状态。
    pub status: DiagnosticStatus,
}

impl EvolutionIssue {
    /// 校验不依赖存储的结构不变量。
    pub fn validate(&self) -> Result<(), InvalidFailure> {
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(InvalidFailure::InvalidConfidence);
        }
        if self.evidence_episode_ids.is_empty() {
            return Err(InvalidFailure::MissingEvidence);
        }
        let unique = self.evidence_episode_ids.iter().collect::<BTreeSet<_>>();
        if unique.len() != self.evidence_episode_ids.len() {
            return Err(InvalidFailure::DuplicateEvidence);
        }
        Ok(())
    }
}

/// 对一条失败记录或聚合 Issue 的处置决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureDisposition {
    /// 无需处理。
    Ignore,
    /// 仅观察，不进入进化队列。
    Observe,
    /// 在当前 Turn 内允许有限重试。
    RetryInTurn,
    /// 需要人工复核。
    ManualReview,
    /// 进入进化候选队列。
    EvolutionCandidate,
    /// 属于平台工程任务，不能由 Agent 自进化解决。
    PlatformEngineering,
    /// 插件实现、Bundle 或契约需要由开发者人工维护。
    PluginMaintenance,
    /// 安全事件，立即隔离并告警。
    SecurityIncident,
    /// 基础设施或运维问题。
    InfrastructureOperations,
}

/// 失败协议的不变量校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidFailure {
    /// 置信度不在合法范围。
    #[error("失败置信度必须是 0 到 1 之间的有限数")]
    InvalidConfidence,
    /// 模型辅助不能标记为确定结论。
    #[error("ModelAssisted 归因的置信度不能达到 1.0")]
    ModelAssistedCannotBeCertain,
    /// Issue 缺少支撑证据。
    #[error("EvolutionIssue 必须至少包含一条支撑 Episode")]
    MissingEvidence,
    /// Issue 的证据重复。
    #[error("EvolutionIssue 的 evidence_episode_ids 不能重复")]
    DuplicateEvidence,
}

/// 根据失败类别和发生次数给出默认处置。
///
/// 这是一个确定性建议，不替代具体策略配置；单次普通错误默认 `Observe`，
/// 安全相关错误立即 `SecurityIncident`。
pub fn default_disposition(kind: FailureKind, occurrences: usize) -> FailureDisposition {
    match kind {
        FailureKind::PermissionFailure | FailureKind::SandboxFailure => {
            FailureDisposition::SecurityIncident
        }
        FailureKind::EnvironmentFailure | FailureKind::RuntimeFailure => {
            FailureDisposition::InfrastructureOperations
        }
        FailureKind::PluginFailure => FailureDisposition::PluginMaintenance,
        FailureKind::VerificationFailure | FailureKind::ContextLoss => {
            FailureDisposition::EvolutionCandidate
        }
        _ if occurrences >= 2 => FailureDisposition::EvolutionCandidate,
        _ => FailureDisposition::Observe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ArtifactDigest;

    fn digest() -> GenomeDigest {
        GenomeDigest::from_sha256_hex("a".repeat(64)).expect("摘要应合法")
    }

    fn fingerprint(tool: Option<&str>) -> FailureFingerprint {
        FailureFingerprint {
            task_family: "code-edit".into(),
            failure_class: FailureKind::ToolExecution,
            component: "tool".into(),
            tool: tool.map(str::to_string),
            plugin: None,
            error_code: Some("EACCES".into()),
            genome_digest: digest(),
            normalized_pattern: "permission denied on write".into(),
        }
    }

    #[test]
    fn fingerprint_key_is_stable_and_distinguishes_tools() {
        let left = fingerprint(Some("write_file"));
        let right = fingerprint(Some("write_file"));
        let other = fingerprint(Some("read_file"));

        assert_eq!(left.stable_key(), right.stable_key());
        assert_ne!(left.stable_key(), other.stable_key());
    }

    #[test]
    fn issue_requires_unique_episodes() {
        let episode = EpisodeId::generate();
        let issue = EvolutionIssue {
            issue_id: EvolutionIssueId::generate(),
            fingerprint: fingerprint(None),
            evidence_episode_ids: vec![episode.clone(), episode],
            evidence_events: Vec::new(),
            suspected_surface: None,
            root_cause_hypothesis: "写入权限被拒绝".into(),
            expected_behavior: "工具应返回可重试错误".into(),
            confidence: 0.7,
            status: DiagnosticStatus::Clustered,
        };
        assert_eq!(
            issue.validate().expect_err("重复证据应被拒绝"),
            InvalidFailure::DuplicateEvidence
        );
    }

    #[test]
    fn model_assisted_cannot_be_certain() {
        let attribution = FailureAttribution {
            detected_at: EventId::generate(),
            suspected_origin: None,
            propagation_path: Vec::new(),
            decisive_step: None,
            failure_class: FailureKind::Unknown,
            confidence: 1.0,
            evidence: Vec::new(),
            method: AttributionMethod::ModelAssisted,
        };
        assert_eq!(
            attribution.validate().expect_err("模型辅助不能置信度 1.0"),
            InvalidFailure::ModelAssistedCannotBeCertain
        );
    }

    #[test]
    fn single_observation_is_not_evolution_candidate() {
        assert_eq!(
            default_disposition(FailureKind::ToolExecution, 1),
            FailureDisposition::Observe
        );
        assert_eq!(
            default_disposition(FailureKind::ToolExecution, 3),
            FailureDisposition::EvolutionCandidate
        );
        assert_eq!(
            default_disposition(FailureKind::PermissionFailure, 1),
            FailureDisposition::SecurityIncident
        );
        assert_eq!(
            default_disposition(FailureKind::PluginFailure, 3),
            FailureDisposition::PluginMaintenance
        );
        let _ = ArtifactDigest::from_sha256_hex("b".repeat(64)).expect("摘要应合法");
    }
}
