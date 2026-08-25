//! 一次 Agent 运行形成的结构化证据协议。
//!
//! 本模块只描述可持久化的数据形态，不依赖 `agent-core` 的运行时类型。Recorder
//! 负责在两个协议之间转换，避免 Serve 平面反向依赖 Evolution 实现。

use crate::{ArtifactDigest, EpisodeDataPolicy, EpisodeId, GenomeRevisionId, RunId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Episode 协议版本；不兼容字段语义变化时必须递增。
pub const EPISODE_SCHEMA_VERSION: u32 = 1;

/// CAS 中一个不可变制品的引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// 制品内容的 SHA-256 摘要。
    pub digest: ArtifactDigest,
    /// 制品的媒体类型。
    pub media_type: String,
    /// 原始字节长度。
    pub size_bytes: u64,
}

/// 对一次任务的最小、可脱敏描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDescriptor {
    /// 稳定任务族；空字符串表示尚未分类。
    #[serde(default)]
    pub family: String,
    /// 脱敏任务输入制品；未获授权时不保存。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ref: Option<ArtifactRef>,
    /// 用于查询和数据集构建的非敏感标签。
    #[serde(default)]
    pub tags: BTreeSet<String>,
}

impl Default for TaskDescriptor {
    fn default() -> Self {
        Self {
            family: String::new(),
            input_ref: None,
            tags: BTreeSet::new(),
        }
    }
}

/// 一次运行的客观资源用量汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummary {
    /// 输入 Token 总数；Provider 未报告时为 `None`。
    pub input_tokens: Option<u64>,
    /// 输出 Token 总数；Provider 未报告时为 `None`。
    pub output_tokens: Option<u64>,
    /// 总 Token 数；Provider 未报告时为 `None`。
    pub total_tokens: Option<u64>,
    /// 实际完成的 ReACT 步数。
    pub react_steps: u64,
    /// 运行墙钟时间；无法可靠测量时为 `None`。
    pub elapsed_ms: Option<u64>,
}

/// Episode 的可信终态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// 任务成功；是否成功应由调用方或可信 Verifier 判定。
    Success,
    /// Agent 完成运行，但没有完成任务。
    TaskFailure,
    /// 触发安全策略或执行了不安全行为。
    SafetyFailure,
    /// 超出 Token、时间、步骤或其他资源预算。
    BudgetFailure,
    /// 用户或可信控制器取消运行。
    Cancelled,
    /// 模型服务、工具环境或存储等基础设施失败。
    InfrastructureFailure,
    /// 缺少可信成功定义，不能把运行统计为成功或失败。
    Unverifiable,
}

/// 第一版稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// 关键上下文在运行中丢失。
    ContextLoss,
    /// 计划生成、执行或重规划失败。
    PlanningFailure,
    /// 选择了错误工具或遗漏必要工具。
    ToolSelection,
    /// 工具参数不合法或不符合任务意图。
    ToolArgument,
    /// 工具自身执行失败。
    ToolExecution,
    /// 模型请求、响应或协议适配失败。
    ModelFailure,
    /// 未执行必要验证，或验证结论错误。
    VerificationFailure,
    /// 过早终止、无限循环或未正确收尾。
    TerminationFailure,
    /// 权限检查拒绝或权限策略异常。
    PermissionFailure,
    /// 隔离环境建立或约束失败。
    SandboxFailure,
    /// 插件加载、Hook 或服务调用失败。
    PluginFailure,
    /// Agent Runtime 调度或生命周期失败。
    RuntimeFailure,
    /// 外部任务环境不可用或不可重建。
    EnvironmentFailure,
    /// 证据不足以归入已知类别。
    Unknown,
}

/// 一条带证据来源的失败分类。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureClassification {
    /// 失败类别。
    pub kind: FailureKind,
    /// 支撑结论的事件 ID；必须来自本 Episode 的事件流。
    #[serde(default)]
    pub evidence_event_ids: Vec<String>,
    /// `[0, 1]` 范围内的置信度。
    pub confidence: f32,
    /// 是否由确定性规则直接判定。
    pub rule_derived: bool,
    /// 是否使用模型辅助判断。
    pub model_assisted: bool,
}

/// Episode 可用于何种强度的回放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayabilityGrade {
    /// 所有外部输出均已固定，可精确回放协议状态。
    Exact,
    /// 可在 Mock Model、Mock Tool 与固定环境中重建。
    FixtureReproducible,
    /// 只能在受控条件下尽力重现。
    ControlledBestEffort,
    /// 依赖不可重建外部状态，不能回放。
    NonReplayable,
}

/// 从 `AgentEvent` 转换得到的稳定事件记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodeEvent {
    /// 原始事件 ID。
    pub event_id: String,
    /// 事件所属运行。
    pub run_id: RunId,
    /// Unix 毫秒时间戳。
    pub timestamp_ms: u64,
    /// `AgentEventKind` 的 snake_case 名称。
    pub kind: String,
    /// ReACT 步序号。
    pub step: u64,
    /// 按 Episode 数据策略处理后的公开 payload。
    pub payload: Value,
}

/// 一次执行的不可变证据头。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    /// Episode Schema 版本。
    pub schema_version: u32,
    /// Episode 标识。
    pub episode_id: EpisodeId,
    /// Agent 运行标识。
    pub run_id: RunId,
    /// 会话标识；由应用层会话协议校验。
    pub session_id: String,
    /// 运行开始时固定的 Genome 修订。
    pub genome_revision_id: GenomeRevisionId,
    /// 脱敏后的任务描述。
    pub task: TaskDescriptor,
    /// 完整事件流 CAS 引用。
    pub event_stream_ref: ArtifactRef,
    /// 可选环境快照引用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_ref: Option<ArtifactRef>,
    /// 可信终态；尚未解析时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// 失败分类；成功或证据不足时可为空。
    #[serde(default)]
    pub failures: Vec<FailureClassification>,
    /// 资源用量。
    pub usage: UsageSummary,
    /// 回放能力等级。
    pub replayability: ReplayabilityGrade,
    /// 数据分级、资格、脱敏和保留策略。
    pub data_policy: EpisodeDataPolicy,
    /// 事件数，用于读取前的完整性检查。
    pub event_count: u64,
    /// 运行开始时间。
    pub started_at_ms: u64,
    /// 运行结束时间；未正常结束时仍由 Recorder 收敛。
    pub finished_at_ms: u64,
}

impl Episode {
    /// 校验不依赖存储的结构不变量。
    ///
    /// # Errors
    ///
    /// Schema 不兼容、时间倒退、空会话标识、置信度越界或失败证据重复时返回错误。
    pub fn validate(&self) -> Result<(), InvalidEpisode> {
        if self.schema_version != EPISODE_SCHEMA_VERSION {
            return Err(InvalidEpisode::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: EPISODE_SCHEMA_VERSION,
            });
        }
        if self.session_id.is_empty() {
            return Err(InvalidEpisode::EmptySessionId);
        }
        if self.finished_at_ms < self.started_at_ms {
            return Err(InvalidEpisode::InvalidTimeRange);
        }
        for failure in &self.failures {
            if !failure.confidence.is_finite() || !(0.0..=1.0).contains(&failure.confidence) {
                return Err(InvalidEpisode::InvalidConfidence);
            }
            let unique = failure.evidence_event_ids.iter().collect::<BTreeSet<_>>();
            if unique.len() != failure.evidence_event_ids.len() {
                return Err(InvalidEpisode::DuplicateEvidenceEvent);
            }
        }
        self.data_policy
            .validate()
            .map_err(|error| InvalidEpisode::InvalidDataPolicy(error.to_string()))
    }
}

/// Episode 结构校验错误。
#[derive(Debug, thiserror::Error)]
pub enum InvalidEpisode {
    /// Schema 版本不受支持。
    #[error("不支持的 Episode schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchemaVersion {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// 会话标识为空。
    #[error("Episode 的 session_id 不能为空")]
    EmptySessionId,
    /// 结束时间早于开始时间。
    #[error("Episode 的结束时间不能早于开始时间")]
    InvalidTimeRange,
    /// 失败分类置信度不在合法范围。
    #[error("失败分类置信度必须是 0 到 1 之间的有限数")]
    InvalidConfidence,
    /// 同一失败分类重复引用事件。
    #[error("同一失败分类不能重复引用事件")]
    DuplicateEvidenceEvent,
    /// 数据策略不合法。
    #[error("Episode 数据策略不合法：{0}")]
    InvalidDataPolicy(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactDigest, EpisodeDataPolicy};

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            digest: ArtifactDigest::from_sha256_hex("1".repeat(64)).expect("摘要应合法"),
            media_type: "application/x-ndjson".into(),
            size_bytes: 10,
        }
    }

    fn episode() -> Episode {
        Episode {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_id: EpisodeId::generate(),
            run_id: RunId::generate(),
            session_id: "session-1".into(),
            genome_revision_id: GenomeRevisionId::generate(),
            task: TaskDescriptor::default(),
            event_stream_ref: artifact(),
            environment_ref: None,
            outcome: Some(Outcome::Unverifiable),
            failures: Vec::new(),
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
            data_policy: EpisodeDataPolicy::default(),
            event_count: 2,
            started_at_ms: 1,
            finished_at_ms: 2,
        }
    }

    #[test]
    fn valid_episode_round_trips() {
        let episode = episode();
        episode.validate().expect("Episode 应合法");
        let encoded = serde_json::to_string(&episode).expect("应可序列化");
        let decoded: Episode = serde_json::from_str(&encoded).expect("应可反序列化");
        assert_eq!(decoded, episode);
    }

    #[test]
    fn rejects_invalid_confidence_and_time_range() {
        let mut invalid = episode();
        invalid.finished_at_ms = 0;
        assert!(matches!(
            invalid.validate(),
            Err(InvalidEpisode::InvalidTimeRange)
        ));

        let mut invalid = episode();
        invalid.failures.push(FailureClassification {
            kind: FailureKind::Unknown,
            evidence_event_ids: Vec::new(),
            confidence: f32::NAN,
            rule_derived: false,
            model_assisted: true,
        });
        assert!(matches!(
            invalid.validate(),
            Err(InvalidEpisode::InvalidConfidence)
        ));
    }
}
