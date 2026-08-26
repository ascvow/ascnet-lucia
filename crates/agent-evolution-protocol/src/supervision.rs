//! 可信监督平面的稳定数据协议。
//!
//! 本模块描述 RunSupervisor、Incident Detector 与 Outcome Resolver 产生的结构化记录。
//! 它们只定义可持久化的形态与不变量，不包含判定逻辑；判定实现属于
//! `agent-evolution` crate，避免 Serve 平面链接监督实现。
//!
//! 核心原则：**Incident 是执行中的异常现象，不等于任务失败**；终态由
//! [`OutcomeRevision`] 经可信优先级判定，Agent 自报永远只是弱信号。

use crate::ids::{
    EpisodeId, EventId, FeedbackId, GenomeRevisionId, IncidentId, OutcomeRevisionId, RunId, SpanId,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Supervision 协议版本；不兼容字段语义变化时必须递增。
pub const SUPERVISION_SCHEMA_VERSION: u32 = 1;

/// 一次调用或子任务的标识域，用于表达事件归属。
///
/// Runtime 无法可靠拿到每个调用的内部身份时保持为 `None`；这不会破坏记录，
/// 只是父子关系该侧未知。
pub type AgentExecutionId = Option<String>;

/// 统一事件信封：一次 Episode 内唯一、可排序、可追溯的可信事件。
///
/// `sequence` 由可信 Recorder 在接收时分配并单调递增，事件内容本身不携带序号，
/// 因此回放方可以检测缺失或乱序。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// 该条信封的全局标识。
    pub event_id: EventId,
    /// 所属运行。
    pub run_id: RunId,
    /// 所属 Episode。
    pub episode_id: EpisodeId,
    /// Episode 内单调递增序号，从 1 开始。
    pub sequence: u64,
    /// 当前调用的 Span。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<SpanId>,
    /// 父调用的 Span。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
    /// Agent 执行实例标识。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_execution_id: AgentExecutionId,
    /// 运行开始时固定的 Genome 修订。
    pub genome_revision_id: GenomeRevisionId,
    /// Unix 毫秒时间戳。
    pub timestamp_ms: u64,
    /// 事件类型名；稳定 snake_case，见 `EpisodeEvent::kind`。
    pub kind: String,
    /// ReACT 步序号。
    pub step: u64,
    /// 已按数据策略处理后的公开载荷。
    pub payload: serde_json::Value,
}

impl EventEnvelope {
    /// 校验不依赖存储的结构不变量。
    ///
    /// # Errors
    ///
    /// Schema 不支持、序号为 0、时间戳倒退在回放侧另行检测；此处只查结构。
    pub fn validate(&self) -> Result<(), InvalidSupervision> {
        if self.sequence == 0 {
            return Err(InvalidSupervision::InvalidSequence);
        }
        if self.kind.is_empty() {
            return Err(InvalidSupervision::EmptyKind);
        }
        Ok(())
    }
}

/// 事件相对运行异常的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    /// 模型请求超时。
    ModelTimeout,
    /// 模型服务端限流。
    ModelRateLimited,
    /// 模型认证或凭据失败。
    ModelAuthenticationFailed,
    /// 模型响应结构与协议不符。
    InvalidModelResponse,

    /// 模型请求了一个未注册的工具。
    ToolNotFound,
    /// 工具入参无法通过 Schema 校验。
    ToolArgumentInvalid,
    /// 工具自身执行失败。
    ToolExecutionFailed,
    /// 工具结果与其公开契约不一致。
    ToolContractViolation,

    /// 插件 WASM 崩溃。
    PluginTrap,
    /// 插件耗尽燃料。
    PluginFuelExhausted,
    /// 插件超出内存上限。
    PluginMemoryLimit,
    /// 插件试图调用未授权能力。
    PluginCapabilityDenied,

    /// Host 或 Runtime 拒绝了越权操作。
    PermissionDenied,
    /// 试图越过文件边界。
    PathBoundaryViolation,
    /// 试图越过进程边界。
    ProcessBoundaryViolation,
    /// 试图读取或写入 Secret。
    SecretAccessAttempt,

    /// 超出最大步数。
    StepLimitExceeded,
    /// 检测到不可收敛的循环。
    LoopDetected,
    /// 超出 Token、时间或并发预算。
    BudgetExceeded,
    /// 用户或可信控制器取消。
    CancellationRequested,

    /// 关键上下文约束在压缩或截断后丢失。
    ContextConstraintLost,
    /// 实际执行偏离已批准计划。
    PlanDeviation,
    /// 确定性 Verifier 判定任务失败。
    VerificationFailed,

    /// 持久化或读取证据失败。
    StorageFailure,
    /// CAS 摘要或事件完整性校验失败。
    ArtifactIntegrityFailure,
    /// 证据不足以归入已知类别。
    Unknown,
}

/// 异常的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// 仅提示，不影响主任务。
    Info,
    /// 已出现，但可由 Turn 内恢复。
    Warning,
    /// 导致当前目标失败，但非系统性安全问题。
    Error,
    /// 安全边界被触发或核心证据链受损。
    Critical,
}

/// 异常是否可能通过 Turn 内重试解决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recoverability {
    /// 可直接重试或替换参数重试。
    Recoverable,
    /// 需要新输入或人工介入，不能自动重试。
    RequiresIntervention,
    /// 不可恢复，必须终止。
    Fatal,
}

/// 引用产生异常的可信组件。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentRef {
    /// ReACT 主循环或 Runtime 调度。
    Runtime,
    /// 模型网关或服务商。
    Model,
    /// 原生工具执行层。
    Tool,
    /// WASM 插件宿主。
    PluginHost,
    /// Episode / Artifact 持久化。
    Storage,
    /// 其他未分类可信组件。
    Other(String),
}

/// 引用探测到异常的确定性 Detector。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorRef {
    /// 模型请求层错误分类规则。
    ModelError,
    /// 工具入参 Schema 校验。
    ToolSchema,
    /// 工具执行结果分类。
    ToolExecution,
    /// 插件崩溃与资源耗尽。
    PluginTrap,
    /// 权限拒绝与路径逃逸。
    PermissionDenied,
    /// 步数、Token、时间等资源上限。
    ResourceBudget,
    /// 重复动作与循环检测。
    LoopDetection,
    /// 计划状态偏离。
    PlanDeviation,
    /// CAS 完整性检查。
    ArtifactIntegrity,
    /// Session 或 Episode 写入失败。
    StorageFailure,
    /// 其他确定性规则。
    Custom(String),
}

/// 异常现象的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentStatus {
    /// 已被可信信号观测，但尚未尝试恢复。
    Observed,
    /// 已把结构化 Observation 返回 Agent，等待其响应。
    Recovering,
    /// Agent 在预算内完成了有效替代，异常被消化。
    Recovered,
    /// Agent 用尽重试预算仍未消化异常。
    Unrecovered,
    /// 需要升级给外层控制器或人工。
    Escalated,
    /// 后续证据表明该异常是误报。
    FalsePositive,
}

/// 一条被可信监督平面记录的异常现象。
///
/// Incident 的判定者是 Runtime 内的确定性 Detector，不是普通插件，也不是 Agent 自报。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    /// 本次异常的标识。
    pub incident_id: IncidentId,
    /// 所属 Episode。
    pub episode_id: EpisodeId,
    /// 触发判定的原始可信事件。
    pub observed_event_id: EventId,
    /// 类别。
    pub kind: IncidentKind,
    /// 严重程度。
    pub severity: Severity,
    /// 可恢复性。
    pub recoverability: Recoverability,
    /// 产生异常的可信组件。
    pub component: ComponentRef,
    /// 触发判定的 Detector。
    pub detector: DetectorRef,
    /// 支撑证据事件 ID；至少包含 `observed_event_id`。
    #[serde(default)]
    pub evidence: Vec<EventId>,
    /// 生命周期状态。
    pub status: IncidentStatus,
}

impl Incident {
    /// 校验不依赖存储的结构不变量。
    pub fn validate(&self) -> Result<(), InvalidSupervision> {
        if !self.evidence.contains(&self.observed_event_id) {
            return Err(InvalidSupervision::IncidentEvidenceMissingObserved);
        }
        if self.detector == DetectorRef::Custom(String::new()) {
            return Err(InvalidSupervision::EmptyDetector);
        }
        Ok(())
    }
}

/// 判定终态的可信来源，按可信度从高到低排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSource {
    /// 由可信、确定性的 Verifier 给出。
    TrustedVerifier,
    /// 由确定性规则给出。
    DeterministicRule,
    /// 由用户显式确认。
    UserFeedback,
    /// 由 Runtime 的运行结果给出。
    Runtime,
    /// 由独立评审 Agent 给出。
    IndependentJudge,
    /// 由模型辅助分析给出。
    ModelAssisted,
    /// 由 Agent 自评给出。
    SelfAssessment,
    /// 来源不明。
    Unknown,
}

impl OutcomeSource {
    /// 返回来源的显式可信优先级；数值越大，来源越可信。
    ///
    /// 不应直接依赖枚举声明顺序比较可信度，因为序列化协议的枚举顺序与门控语义是
    /// 两个独立约束。
    pub const fn trust_priority(self) -> u8 {
        match self {
            Self::TrustedVerifier => 7,
            Self::DeterministicRule => 6,
            Self::UserFeedback => 5,
            Self::Runtime => 4,
            Self::IndependentJudge => 3,
            Self::ModelAssisted => 2,
            Self::SelfAssessment => 1,
            Self::Unknown => 0,
        }
    }

    /// 判断该来源是否满足 Promotion 所需的可信门槛。
    ///
    /// `ModelAssisted` 与 `SelfAssessment` 单独存在时永远不足以触发 Promotion。
    pub fn is_trusted_for_promotion(self) -> bool {
        matches!(
            self,
            Self::TrustedVerifier | Self::DeterministicRule | Self::UserFeedback
        )
    }
}

/// 一次 Episode 终态的一次修订。
///
/// 终态永远不覆盖：新证据到来时追加新修订，`supersedes` 指向前一条。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRevision {
    /// 本次修订标识。
    pub revision_id: OutcomeRevisionId,
    /// 所属 Episode。
    pub episode_id: EpisodeId,
    /// 前一条修订；首条为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<OutcomeRevisionId>,
    /// 终态。
    pub outcome: crate::episode::Outcome,
    /// 判定来源。
    pub source: OutcomeSource,
    /// 修订理由，供人工与审计使用。
    pub reason: String,
    /// 触发本次修订的延迟反馈；非反馈修订或旧记录为 `None`。
    ///
    /// 该字段以加法兼容方式保存反馈、运行绑定和脱敏证据引用，避免只在理由文本中留下
    /// 无法机器校验的关联。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<FeedbackEvent>,
}

impl OutcomeRevision {
    /// 校验不依赖存储的结构不变量。
    pub fn validate(&self) -> Result<(), InvalidSupervision> {
        if self.reason.trim().is_empty() {
            return Err(InvalidSupervision::EmptyReason);
        }
        if matches!(
            self.outcome,
            crate::episode::Outcome::Success | crate::episode::Outcome::SuccessWithRecovery
        ) && self.source == OutcomeSource::SelfAssessment
        {
            return Err(InvalidSupervision::SelfDeclaredSuccess);
        }
        if let Some(feedback) = &self.feedback {
            feedback.validate()?;
            if feedback.related_episode_id != self.episode_id {
                return Err(InvalidSupervision::FeedbackEpisodeMismatch);
            }
            if feedback.source.outcome_source() != self.source {
                return Err(InvalidSupervision::FeedbackSourceMismatch);
            }
            let outcome_matches = matches!(
                (&feedback.signal, &self.outcome),
                (
                    FeedbackSignal::ConfirmedSuccess,
                    crate::episode::Outcome::Success
                ) | (
                    FeedbackSignal::ConfirmedFailure
                        | FeedbackSignal::PartialFailure
                        | FeedbackSignal::ConstraintViolation,
                    crate::episode::Outcome::TaskFailure
                )
            );
            if !outcome_matches {
                return Err(InvalidSupervision::FeedbackOutcomeMismatch);
            }
        }
        Ok(())
    }
}

/// 延迟反馈信号的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSource {
    /// 用户在后续消息中明确纠正或确认。
    User,
    /// CI、静态检查或确定性回归。
    DeterministicCheck,
    /// Canary 或受控环境观察。
    Canary,
    /// 其他来源。
    Other,
}

impl FeedbackSource {
    /// 映射为 Outcome 修订使用的可信来源。
    ///
    /// `Canary` 只代表受控运行观察，因此仍按 Runtime 证据处理；未知来源不会被提升为
    /// 用户反馈或确定性规则。
    pub const fn outcome_source(self) -> OutcomeSource {
        match self {
            Self::User => OutcomeSource::UserFeedback,
            Self::DeterministicCheck => OutcomeSource::DeterministicRule,
            Self::Canary => OutcomeSource::Runtime,
            Self::Other => OutcomeSource::Unknown,
        }
    }
}

/// 延迟反馈的内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSignal {
    /// 明确确认成功。
    ConfirmedSuccess,
    /// 明确指出任务失败。
    ConfirmedFailure,
    /// 指出部分子任务失败。
    PartialFailure,
    /// 指出关键约束被违反。
    ConstraintViolation,
    /// 其他非敏感信号。
    Note(String),
}

/// 指向既有 Episode 的延迟反馈。
///
/// 反馈只追加新的 [`OutcomeRevision`]，不修改原记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackEvent {
    /// 反馈标识。
    pub feedback_id: FeedbackId,
    /// 来源。
    pub source: FeedbackSource,
    /// 指向的 Episode。
    pub related_episode_id: EpisodeId,
    /// 指向的运行。
    pub related_run_id: RunId,
    /// 信号内容。
    pub signal: FeedbackSignal,
    /// 可选的脱敏证据制品。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<crate::episode::ArtifactRef>,
}

impl FeedbackEvent {
    /// 校验延迟反馈自身不依赖存储的结构不变量。
    ///
    /// # Errors
    ///
    /// `Note` 只包含空白字符时返回错误；Episode、Run 与证据制品是否存在由应用层校验。
    pub fn validate(&self) -> Result<(), InvalidSupervision> {
        if matches!(&self.signal, FeedbackSignal::Note(note) if note.trim().is_empty()) {
            return Err(InvalidSupervision::EmptyFeedbackNote);
        }
        Ok(())
    }
}

/// 监督记录的不变量校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSupervision {
    /// Event Envelope 序号非法。
    #[error("Event Envelope 的 sequence 必须从 1 开始")]
    InvalidSequence,
    /// 事件类型名为空。
    #[error("Event Envelope 的 kind 不能为空")]
    EmptyKind,
    /// Incident 的 evidence 必须包含 observed_event_id。
    #[error("Incident 的 evidence 必须包含 observed_event_id")]
    IncidentEvidenceMissingObserved,
    /// Detector 为空字符串。
    #[error("DetectorRef::Custom 不能为空")]
    EmptyDetector,
    /// OutcomeRevision 的理由不能为空。
    #[error("OutcomeRevision 的 reason 不能为空")]
    EmptyReason,
    /// Agent 自评不能独立判定任何成功终态。
    #[error("SelfAssessment 不能独立判定成功 Outcome")]
    SelfDeclaredSuccess,
    /// 延迟反馈的 Note 不能为空。
    #[error("FeedbackSignal::Note 不能为空")]
    EmptyFeedbackNote,
    /// Outcome 修订与反馈指向不同 Episode。
    #[error("OutcomeRevision 与 FeedbackEvent 必须指向同一 Episode")]
    FeedbackEpisodeMismatch,
    /// Outcome 修订来源与延迟反馈来源不一致。
    #[error("OutcomeRevision 的 source 与 FeedbackEvent 来源不一致")]
    FeedbackSourceMismatch,
    /// Outcome 修订终态与延迟反馈信号不一致。
    #[error("OutcomeRevision 的 outcome 与 FeedbackEvent 信号不一致")]
    FeedbackOutcomeMismatch,
}

/// 把 IncidentKind 映射到推荐的可信组件。
///
/// 这只是默认映射；调用方可以基于更具体的上下文修正。
pub fn default_component(kind: IncidentKind) -> ComponentRef {
    match kind {
        IncidentKind::ModelTimeout
        | IncidentKind::ModelRateLimited
        | IncidentKind::ModelAuthenticationFailed
        | IncidentKind::InvalidModelResponse => ComponentRef::Model,
        IncidentKind::ToolNotFound
        | IncidentKind::ToolArgumentInvalid
        | IncidentKind::ToolExecutionFailed
        | IncidentKind::ToolContractViolation => ComponentRef::Tool,
        IncidentKind::PluginTrap
        | IncidentKind::PluginFuelExhausted
        | IncidentKind::PluginMemoryLimit
        | IncidentKind::PluginCapabilityDenied => ComponentRef::PluginHost,
        IncidentKind::PermissionDenied
        | IncidentKind::PathBoundaryViolation
        | IncidentKind::ProcessBoundaryViolation
        | IncidentKind::SecretAccessAttempt => ComponentRef::Runtime,
        IncidentKind::StepLimitExceeded
        | IncidentKind::LoopDetected
        | IncidentKind::BudgetExceeded
        | IncidentKind::CancellationRequested => ComponentRef::Runtime,
        IncidentKind::ContextConstraintLost
        | IncidentKind::PlanDeviation
        | IncidentKind::VerificationFailed => ComponentRef::Runtime,
        IncidentKind::StorageFailure | IncidentKind::ArtifactIntegrityFailure => {
            ComponentRef::Storage
        }
        IncidentKind::Unknown => ComponentRef::Other("unknown".to_string()),
    }
}

/// 把 IncidentKind 映射到推荐的可恢复性。
///
/// 只是默认建议；真正的 Turn 内重试由 Supervisor 的预算策略决定。
pub fn default_recoverability(kind: IncidentKind) -> Recoverability {
    match kind {
        IncidentKind::ModelTimeout
        | IncidentKind::ModelRateLimited
        | IncidentKind::ToolNotFound
        | IncidentKind::ToolArgumentInvalid
        | IncidentKind::ToolExecutionFailed => Recoverability::Recoverable,
        IncidentKind::ModelAuthenticationFailed
        | IncidentKind::InvalidModelResponse
        | IncidentKind::ToolContractViolation => Recoverability::RequiresIntervention,
        IncidentKind::PluginTrap
        | IncidentKind::PluginFuelExhausted
        | IncidentKind::PluginMemoryLimit
        | IncidentKind::PluginCapabilityDenied => Recoverability::RequiresIntervention,
        IncidentKind::PermissionDenied
        | IncidentKind::PathBoundaryViolation
        | IncidentKind::ProcessBoundaryViolation
        | IncidentKind::SecretAccessAttempt => Recoverability::Fatal,
        IncidentKind::StepLimitExceeded
        | IncidentKind::LoopDetected
        | IncidentKind::BudgetExceeded
        | IncidentKind::CancellationRequested => Recoverability::Fatal,
        IncidentKind::ContextConstraintLost
        | IncidentKind::PlanDeviation
        | IncidentKind::VerificationFailed => Recoverability::RequiresIntervention,
        IncidentKind::StorageFailure
        | IncidentKind::ArtifactIntegrityFailure
        | IncidentKind::Unknown => Recoverability::Fatal,
    }
}

/// 去重并按稳定顺序排序事件 ID 列表。
pub fn sorted_unique_event_ids(ids: impl IntoIterator<Item = EventId>) -> Vec<EventId> {
    let set: BTreeSet<EventId> = ids.into_iter().collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_incident() -> Incident {
        let observed = EventId::generate();
        Incident {
            incident_id: IncidentId::generate(),
            episode_id: EpisodeId::generate(),
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

    #[test]
    fn incident_round_trips_and_validates() {
        let incident = sample_incident();
        incident.validate().expect("合法 Incident 应通过校验");
        let encoded = serde_json::to_string(&incident).expect("应可序列化");
        let decoded: Incident = serde_json::from_str(&encoded).expect("应可反序列化");
        assert_eq!(decoded, incident);
    }

    #[test]
    fn incident_rejects_missing_observed_evidence() {
        let mut incident = sample_incident();
        incident.evidence = vec![EventId::generate()];
        assert_eq!(
            incident.validate().expect_err("缺少 observed 应被拒绝"),
            InvalidSupervision::IncidentEvidenceMissingObserved
        );
    }

    #[test]
    fn outcome_revision_rejects_self_declared_success() {
        for outcome in [
            crate::episode::Outcome::Success,
            crate::episode::Outcome::SuccessWithRecovery,
        ] {
            let revision = OutcomeRevision {
                revision_id: OutcomeRevisionId::generate(),
                episode_id: EpisodeId::generate(),
                supersedes: None,
                outcome,
                source: OutcomeSource::SelfAssessment,
                reason: "Agent 自报完成".into(),
                feedback: None,
            };
            assert_eq!(
                revision.validate().expect_err("自报成功应被拒绝"),
                InvalidSupervision::SelfDeclaredSuccess
            );
        }
    }

    #[test]
    fn promotion_trust_excludes_weak_sources() {
        assert!(OutcomeSource::TrustedVerifier.is_trusted_for_promotion());
        assert!(OutcomeSource::DeterministicRule.is_trusted_for_promotion());
        assert!(OutcomeSource::UserFeedback.is_trusted_for_promotion());
        assert!(!OutcomeSource::ModelAssisted.is_trusted_for_promotion());
        assert!(!OutcomeSource::SelfAssessment.is_trusted_for_promotion());
        assert!(!OutcomeSource::Runtime.is_trusted_for_promotion());
    }

    #[test]
    fn feedback_validates_note_and_revision_binding() {
        let episode_id = EpisodeId::generate();
        let run_id = RunId::generate();
        let mut feedback = FeedbackEvent {
            feedback_id: FeedbackId::generate(),
            source: FeedbackSource::User,
            related_episode_id: episode_id.clone(),
            related_run_id: run_id,
            signal: FeedbackSignal::Note("约束没有满足".into()),
            evidence: None,
        };
        feedback.validate().expect("非空 Note 应通过校验");

        feedback.signal = FeedbackSignal::ConfirmedFailure;
        let revision = OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id,
            supersedes: None,
            outcome: crate::episode::Outcome::TaskFailure,
            source: OutcomeSource::UserFeedback,
            reason: "用户延迟纠正确认任务失败".into(),
            feedback: Some(feedback.clone()),
        };
        revision.validate().expect("反馈绑定应合法");

        feedback.signal = FeedbackSignal::Note("  ".into());
        assert_eq!(
            feedback.validate().expect_err("空 Note 应被拒绝"),
            InvalidSupervision::EmptyFeedbackNote
        );
    }

    #[test]
    fn outcome_revision_reads_legacy_record_without_feedback() {
        let revision_id = OutcomeRevisionId::generate();
        let episode_id = EpisodeId::generate();
        let encoded = format!(
            r#"{{"revision_id":"{revision_id}","episode_id":"{episode_id}","outcome":"unverifiable","source":"deterministic_rule","reason":"旧版记录"}}"#
        );
        let decoded: OutcomeRevision = serde_json::from_str(&encoded).expect("旧记录应可读取");
        assert_eq!(decoded.feedback, None);
        decoded.validate().expect("旧记录仍应合法");
    }

    #[test]
    fn sorted_unique_event_ids_are_stable() {
        let first = EventId::generate();
        let second = EventId::generate();
        let (small, large) = if first < second {
            (first, second)
        } else {
            (second, first)
        };
        let ids = sorted_unique_event_ids(vec![
            large.clone(),
            small.clone(),
            large.clone(),
            small.clone(),
        ]);
        assert_eq!(ids, vec![small, large]);
    }
}
