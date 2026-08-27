//! AgentEvent 的去波动 Protocol Trace 与差异检测。
//!
//! Episode CAS 的完整性与合法状态回放继续由 `agent-evolution::ProtocolReplay` 负责。
//! 本模块只把真实 Parent/Candidate 运行归一化为不含 ID、时间戳和正文的状态机轨迹，
//! 用于相同 Fixture 下的确定性比较。

use agent_core::{AgentEvent, AgentEventKind};
use agent_evolution_protocol::TaskCaseId;
use agent_tool::ToolErrorKind;
use serde::{Deserialize, Serialize};

/// 一条去除运行 ID、事件 ID、时间戳、文本和参数后的协议事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolTraceEntry {
    /// Core 事件类别。
    pub kind: AgentEventKind,
    /// ReAct 状态机步数。
    pub step: usize,
    /// 工具事件中的稳定工具名；非工具事件为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 工具终态的可信错误类别；成功或非工具事件为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_error_kind: Option<ToolErrorKind>,
    /// ModelResponse 的停止原因；其他事件为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// ModelResponse 声明的工具调用数；其他事件为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<u64>,
}

/// 一次运行的可比较协议轨迹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolTrace {
    /// 按真实投递顺序排列的归一化事件。
    pub entries: Vec<ProtocolTraceEntry>,
}

impl ProtocolTrace {
    /// 从 Core 真实事件构建轨迹，并校验运行归属、步数单调与唯一终态。
    ///
    /// # Errors
    ///
    /// 事件为空、混入多个 Run、首事件不是 RunStarted、步数倒退、终态缺失/重复或终态后
    /// 仍出现事件时返回 [`ProtocolTraceError`]。
    pub fn from_events(events: &[AgentEvent]) -> Result<Self, ProtocolTraceError> {
        let Some(first) = events.first() else {
            return Err(ProtocolTraceError::Empty);
        };
        if first.kind != AgentEventKind::RunStarted {
            return Err(ProtocolTraceError::MissingStart);
        }
        let run_id = &first.run_id;
        let mut previous_step = 0;
        let mut terminal = false;
        let mut entries = Vec::with_capacity(events.len());
        for event in events {
            if event.run_id != *run_id {
                return Err(ProtocolTraceError::MixedRuns);
            }
            if event.step < previous_step {
                return Err(ProtocolTraceError::StepRegression {
                    previous: previous_step,
                    current: event.step,
                });
            }
            if terminal {
                return Err(ProtocolTraceError::EventAfterTerminal);
            }
            previous_step = event.step;
            terminal = matches!(
                event.kind,
                AgentEventKind::RunFinished | AgentEventKind::StepLimitReached
            );
            entries.push(normalize(event));
        }
        if !terminal {
            return Err(ProtocolTraceError::MissingTerminal);
        }
        Ok(Self { entries })
    }

    /// 比较 Parent 与 Candidate 的归一化状态机轨迹。
    ///
    /// 返回 `None` 表示状态转移完全一致；返回值只含首个差异位置和事件类别，不包含
    /// Hidden 正文、工具参数或模型输出。
    pub fn compare(
        task_case_id: TaskCaseId,
        repeat_index: u32,
        parent: &Self,
        candidate: &Self,
    ) -> Option<ProtocolDifference> {
        let max_len = parent.entries.len().max(candidate.entries.len());
        for index in 0..max_len {
            let left = parent.entries.get(index);
            let right = candidate.entries.get(index);
            if left != right {
                return Some(ProtocolDifference {
                    task_case_id,
                    repeat_index,
                    event_index: index,
                    parent_kind: left.map(|entry| entry.kind.clone()),
                    candidate_kind: right.map(|entry| entry.kind.clone()),
                });
            }
        }
        None
    }
}

/// 相同 Fixture 下 Parent 与 Candidate 的首个协议差异。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolDifference {
    /// 发生差异的 TaskCase。
    pub task_case_id: TaskCaseId,
    /// 发生差异的 Repeat 序号。
    pub repeat_index: u32,
    /// 归一化事件序列中的首个差异位置。
    pub event_index: usize,
    /// Parent 在该位置的事件类别；Parent 已结束时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<AgentEventKind>,
    /// Candidate 在该位置的事件类别；Candidate 已结束时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_kind: Option<AgentEventKind>,
}

/// Protocol Trace 结构错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolTraceError {
    /// 事件流为空。
    #[error("Protocol Trace 不能为空")]
    Empty,
    /// 首事件不是 RunStarted。
    #[error("Protocol Trace 必须以 run_started 开始")]
    MissingStart,
    /// 事件流包含多个 Run ID。
    #[error("Protocol Trace 不能混入多个 Run")]
    MixedRuns,
    /// ReAct step 发生倒退。
    #[error("Protocol Trace step 倒退：{previous} -> {current}")]
    StepRegression {
        /// 前一事件 step。
        previous: usize,
        /// 当前事件 step。
        current: usize,
    },
    /// 缺少 RunFinished 或 StepLimitReached。
    #[error("Protocol Trace 缺少终态")]
    MissingTerminal,
    /// 终态后仍出现 Core 事件。
    #[error("Protocol Trace 终态后出现额外事件")]
    EventAfterTerminal,
}

/// 提取比较所需的稳定载荷，不复制文本、参数、ID 或计费数据。
fn normalize(event: &AgentEvent) -> ProtocolTraceEntry {
    let tool_name = match event.kind {
        AgentEventKind::ToolStarted | AgentEventKind::ToolFinished => event
            .payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        AgentEventKind::ToolSkipped => event
            .payload
            .pointer("/call/name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    };
    let tool_error_kind = if event.kind == AgentEventKind::ToolFinished {
        event
            .payload
            .get("error_kind")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    } else {
        None
    };
    let (finish_reason, tool_call_count) = if event.kind == AgentEventKind::ModelResponse {
        (
            event
                .payload
                .get("finish_reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            event
                .payload
                .get("tool_call_count")
                .and_then(serde_json::Value::as_u64),
        )
    } else {
        (None, None)
    };
    ProtocolTraceEntry {
        kind: event.kind.clone(),
        step: event.step,
        tool_name,
        tool_error_kind,
        finish_reason,
        tool_call_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造指定类别和 step 的测试事件。
    fn event(kind: AgentEventKind, step: usize) -> AgentEvent {
        AgentEvent::new("run-1", kind, step, json!({}))
    }

    /// Protocol Trace 必须忽略易变 ID/时间，但检测真实状态机事件差异。
    #[test]
    fn detects_state_machine_difference() {
        let parent = ProtocolTrace::from_events(&[
            event(AgentEventKind::RunStarted, 0),
            event(AgentEventKind::TurnStarted, 0),
            event(AgentEventKind::ModelRequest, 0),
            event(AgentEventKind::ModelResponse, 0),
            event(AgentEventKind::RunFinished, 0),
        ])
        .expect("构造 Parent Trace");
        let candidate = ProtocolTrace::from_events(&[
            event(AgentEventKind::RunStarted, 0),
            event(AgentEventKind::TurnStarted, 0),
            event(AgentEventKind::ModelRequest, 0),
            event(AgentEventKind::StepLimitReached, 1),
        ])
        .expect("构造 Candidate Trace");

        let difference = ProtocolTrace::compare(
            TaskCaseId::new("case_protocol1").expect("TaskCase ID 合法"),
            0,
            &parent,
            &candidate,
        )
        .expect("必须检测状态机差异");
        assert_eq!(difference.event_index, 3);
        assert_eq!(difference.parent_kind, Some(AgentEventKind::ModelResponse));
        assert_eq!(
            difference.candidate_kind,
            Some(AgentEventKind::StepLimitReached)
        );
    }

    /// 终态之后的事件必须被拒绝，不能伪装成可比较轨迹。
    #[test]
    fn rejects_events_after_terminal() {
        let error = ProtocolTrace::from_events(&[
            event(AgentEventKind::RunStarted, 0),
            event(AgentEventKind::RunFinished, 0),
            event(AgentEventKind::TurnFinished, 0),
        ])
        .expect_err("终态后事件必须拒绝");
        assert_eq!(error, ProtocolTraceError::EventAfterTerminal);
    }
}
