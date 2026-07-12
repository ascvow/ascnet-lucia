//! Core Agent 的可观察运行状态。

use crate::{Session, TokenUsage};
use agent_tool::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

/// Agent 当前所处的生命周期阶段。
///
/// 该阶段只描述单个 Core Agent 的 ReAct 循环，不包含 Runtime 的排队、派生或权限状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    /// 尚未开始运行。
    #[default]
    Idle,
    /// 已创建运行，正在准备首轮或下一轮请求。
    Preparing,
    /// 正在构造或发送模型请求。
    RequestingModel,
    /// 正在接收模型流式响应。
    StreamingModel,
    /// 正在执行模型返回的工具调用。
    ExecutingTools,
    /// 运行正常完成。
    Succeeded,
    /// 运行按取消请求完成收尾。
    Cancelled,
    /// 运行因错误结束。
    Failed,
}

impl AgentPhase {
    /// 返回当前阶段是否仍占用 Agent 的唯一运行槽位。
    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::RequestingModel | Self::StreamingModel | Self::ExecutingTools
        )
    }

    /// 返回当前阶段是否为一次运行的终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Cancelled | Self::Failed)
    }
}

/// 单个工具调用在当前模型轮次中的执行状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolCallStatus {
    /// 已由模型请求，等待执行。
    #[default]
    Pending,
    /// 已通过前置检查，正在执行。
    Running,
    /// 工具返回成功结果。
    Succeeded,
    /// 工具返回错误结果或被策略阻止。
    Failed,
    /// 因取消或 steering 跳过。
    Skipped,
}

/// 当前模型轮次中一个工具调用及其可观察结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolCallState {
    /// 模型提供的完整工具调用。
    pub call: ToolCall,
    /// 当前执行状态。
    pub status: AgentToolCallStatus,
    /// 工具完成后的完整结果；等待、运行或跳过时可为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResult>,
}

impl AgentToolCallState {
    /// 创建等待执行的工具调用状态。
    pub fn pending(call: ToolCall) -> Self {
        Self {
            call,
            status: AgentToolCallStatus::Pending,
            result: None,
        }
    }
}

/// Core Agent 的完整可观察状态快照。
///
/// 快照可跨线程读取，不暴露内部锁。`session` 保存最近一次状态转换时已确认的完整会话；
/// 流式增量尚未写入会话时保存在 `streamed_text` 与 `thinking_text` 中。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    /// 当前生命周期阶段。
    pub phase: AgentPhase,
    /// 当前或最近一次运行 ID；从未运行时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// 当前 ReAct 步索引；终态时为最后处理的步索引。
    pub step: usize,
    /// 最近一次状态转换时已确认的完整会话。
    pub session: Session,
    /// 当前模型轮次已接收、尚未确认进会话的文本增量。
    pub streamed_text: String,
    /// 当前模型轮次已接收的推理增量。
    pub thinking_text: String,
    /// 当前模型轮次返回的工具调用及执行结果。
    pub tool_calls: Vec<AgentToolCallState>,
    /// 当前运行累计的服务商 token 用量。
    pub usage: TokenUsage,
    /// 失败终态的诊断文本；其他阶段为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 等待注入的 steering 消息数量。
    pub pending_steering: usize,
    /// 等待注入的 follow-up 消息数量。
    pub pending_follow_ups: usize,
    /// 是否存在尚未被运行循环消费的取消请求。
    pub cancel_requested: bool,
}
