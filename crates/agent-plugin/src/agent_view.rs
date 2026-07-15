//! 插件复用的派生 Agent 主界面状态与交互逻辑。

use crate::{
    AgentContinueRequest, AgentEvent, AgentEventKind, AgentHandle, AgentId, AgentStatus,
    PluginHostApi, UiColor, UiInputEvent, UiLine, UiSpan, UiStyle,
};
use std::collections::VecDeque;

/// 单个 Agent 主界面最多保留的 Runtime 事件和本地用户消息数量。
const EVENT_LIMIT: usize = 512;

/// 插件可嵌入子视图的通用 Agent 主界面状态。
///
/// 该类型只管理一个当前 Agent 句柄的实时事件、输入和反馈。成员目录、Workflow
/// 节点映射、导航标题和权限策略仍由调用插件负责。
pub struct AgentViewSession {
    target: AgentId,
    timeline: VecDeque<AgentViewItem>,
    input: String,
    feedback: Option<std::result::Result<String, String>>,
    status: Option<AgentStatus>,
}

/// Agent 主界面按到达顺序保存的 Runtime 事件或本地用户消息。
enum AgentViewItem {
    /// Host 回放或实时投递的 Agent 事件。
    Event(AgentEvent),
    /// 用户从当前 Agent 主界面提交的消息。
    User(String),
}

impl AgentViewSession {
    /// 为一个可信 Agent 句柄创建空白主界面状态。
    pub fn new(target: AgentId) -> Self {
        Self {
            target,
            timeline: VecDeque::new(),
            input: String::new(),
            feedback: None,
            status: None,
        }
    }

    /// 返回当前查询和交互使用的 Agent 身份。
    pub fn target(&self) -> &AgentId {
        &self.target
    }

    /// 返回最近一次从 Host 读取到的执行状态。
    pub fn status(&self) -> Option<AgentStatus> {
        self.status
    }

    /// 切换到同一逻辑成员或节点的新运行句柄，并保留已有会话展示。
    pub fn replace_target(&mut self, target: AgentId) {
        if self.target != target {
            self.target = target;
            self.status = None;
        }
    }

    /// 非阻塞刷新 Agent 状态和增量事件；失败信息保存在视图中供下一帧展示。
    pub fn refresh(&mut self, host: &dyn PluginHostApi) {
        match host.agent_status(&self.target) {
            Ok(snapshot) => self.status = Some(snapshot.status),
            Err(error) => {
                self.feedback = Some(Err(format!("读取 Agent 状态失败：{error}")));
                return;
            }
        }
        match host.agent_events(&self.target, 256) {
            Ok(events) => {
                self.feedback = self.feedback.take().filter(|result| result.is_err());
                for event in events {
                    self.push_item(AgentViewItem::Event(event));
                }
            }
            Err(error) => {
                self.feedback = Some(Err(format!("读取 Agent 事件失败：{error}")));
            }
        }
    }

    /// 处理通用子视图输入；成功续跑时返回新句柄供 owner 更新稳定映射。
    ///
    /// 排队或运行中的 Agent 接收 steering；成功终态 Agent 创建后续运行。失败或
    /// 取消状态不隐式重试，错误会保留在视图反馈中。
    pub fn handle_input(
        &mut self,
        host: &dyn PluginHostApi,
        event: &UiInputEvent,
    ) -> Option<AgentHandle> {
        let UiInputEvent::Key { code, modifiers } = event else {
            return None;
        };
        if code == "enter" {
            return self.submit(host);
        }
        if code == "backspace" {
            self.input.pop();
            self.feedback = None;
            return None;
        }
        if code.chars().count() == 1
            && !modifiers
                .iter()
                .any(|modifier| matches!(modifier.as_str(), "control" | "alt" | "super" | "meta"))
        {
            self.input.push_str(code);
            self.feedback = None;
        }
        None
    }

    /// 按给定尺寸渲染事件时间线、交互反馈和当前输入行。
    pub fn render(&self, width: u16, height: u16) -> Vec<UiLine> {
        let reserved = if self.feedback.is_some() { 3 } else { 2 };
        let available = usize::from(height).saturating_sub(reserved);
        let event_lines = agent_event_lines(&self.timeline, usize::from(width));
        let mut lines = event_lines
            .into_iter()
            .rev()
            .take(available)
            .rev()
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(line(
                match self.status {
                    Some(AgentStatus::Queued) => "等待运行资源",
                    Some(AgentStatus::Ready) => "等待任务",
                    _ => "等待 Agent 事件",
                },
                Some(UiColor::Gray),
                false,
            ));
        }
        lines.push(UiLine { spans: Vec::new() });
        if let Some(feedback) = &self.feedback {
            let (text, color) = match feedback {
                Ok(text) => (text.as_str(), UiColor::Green),
                Err(text) => (text.as_str(), UiColor::Red),
            };
            lines.push(line(text, Some(color), false));
        }
        lines.push(UiLine {
            spans: vec![
                span("> ", Some(UiColor::Green), true),
                span(&self.input, None, false),
            ],
        });
        lines.truncate(usize::from(height));
        lines
    }

    /// 提交当前输入并根据 Agent 状态选择 steering 或成功会话续跑。
    fn submit(&mut self, host: &dyn PluginHostApi) -> Option<AgentHandle> {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return None;
        }
        let snapshot = match host.agent_status(&self.target) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.feedback = Some(Err(format!("读取 Agent 状态失败：{error}")));
                return None;
            }
        };
        let result = match snapshot.status {
            AgentStatus::Queued | AgentStatus::Running => host
                .steer_agent(&self.target, &input)
                .map(|_| (None, "消息已发送给 Agent".to_string())),
            AgentStatus::Succeeded => host
                .continue_agent(&AgentContinueRequest::new(
                    self.target.clone(),
                    input.clone(),
                ))
                .map(|handle| {
                    let message = "已开始 Agent 后续会话".to_string();
                    (Some(handle), message)
                }),
            AgentStatus::Ready | AgentStatus::Failed | AgentStatus::Cancelled => {
                Err(anyhow::anyhow!("Agent 当前状态不能接收消息"))
            }
        };
        match result {
            Ok((handle, message)) => {
                self.push_item(AgentViewItem::User(input));
                self.input.clear();
                self.feedback = Some(Ok(message));
                if let Some(handle) = handle {
                    self.replace_target(handle.id.clone());
                    self.status = Some(AgentStatus::Queued);
                    Some(handle)
                } else {
                    self.status = Some(snapshot.status);
                    None
                }
            }
            Err(error) => {
                self.feedback = Some(Err(error.to_string()));
                None
            }
        }
    }

    /// 追加一项并维持固定内存上限。
    fn push_item(&mut self, item: AgentViewItem) {
        if self.timeline.len() >= EVENT_LIMIT {
            self.timeline.pop_front();
        }
        self.timeline.push_back(item);
    }
}

/// 将 Agent 事件转换为主界面可见行，并合并连续模型文本增量。
fn agent_event_lines(timeline: &VecDeque<AgentViewItem>, width: usize) -> Vec<UiLine> {
    let text_width = width.saturating_sub(4).max(12);
    let mut rows: Vec<(String, UiColor)> = Vec::new();
    for item in timeline {
        let event = match item {
            AgentViewItem::Event(event) => event,
            AgentViewItem::User(message) => {
                rows.push((format!("你  {message}"), UiColor::Green));
                continue;
            }
        };
        match event.kind {
            AgentEventKind::RunStarted => rows.push(("开始运行".into(), UiColor::Cyan)),
            AgentEventKind::TurnStarted => {
                rows.push((format!("分析中 · 第 {} 步", event.step + 1), UiColor::Blue));
            }
            AgentEventKind::ModelTextDelta => {
                let delta = event.payload["delta"].as_str().unwrap_or_default();
                if let Some((text, _)) = rows
                    .last_mut()
                    .filter(|(text, _)| text.starts_with("回复  "))
                {
                    text.push_str(delta);
                } else if !delta.is_empty() {
                    rows.push((format!("回复  {delta}"), UiColor::White));
                }
            }
            AgentEventKind::ToolStarted => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let args = compact_json(&event.payload["args"], 120);
                rows.push((format!("调用  {name}{}", suffix(&args)), UiColor::Yellow));
            }
            AgentEventKind::ToolFinished => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let failed = event.payload["is_error"].as_bool().unwrap_or(false);
                let result = compact_json(&event.payload["content"], 120);
                rows.push((
                    format!(
                        "{}  {name}{}",
                        if failed { "失败" } else { "完成" },
                        suffix(&result)
                    ),
                    if failed { UiColor::Red } else { UiColor::Green },
                ));
            }
            AgentEventKind::ToolSkipped => {
                let name = event.payload["call"]["name"].as_str().unwrap_or("tool");
                rows.push((format!("跳过  {name}"), UiColor::Gray));
            }
            AgentEventKind::SteeringInjected => {
                rows.push(("已接收新的互动消息".into(), UiColor::Cyan));
            }
            AgentEventKind::RunFinished => rows.push(("运行完成".into(), UiColor::Green)),
            AgentEventKind::StepLimitReached => {
                rows.push(("达到运行步数上限".into(), UiColor::Red));
            }
            AgentEventKind::Extension
            | AgentEventKind::ModelRequest
            | AgentEventKind::ModelThinkingDelta
            | AgentEventKind::ModelResponse
            | AgentEventKind::BillingUsage
            | AgentEventKind::TurnFinished
            | AgentEventKind::FollowUpInjected => {}
        }
    }
    rows.into_iter()
        .map(|(text, color)| line(&clip(&text, text_width), Some(color), false))
        .collect()
}

/// 为非空工具参数或结果增加紧凑分隔。
fn suffix(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!("  {value}")
    }
}

/// 将 JSON 压缩成一行并限制可见字符数。
fn compact_json(value: &serde_json::Value, max_chars: usize) -> String {
    if value.is_null() {
        return String::new();
    }
    clip(&serde_json::to_string(value).unwrap_or_default(), max_chars)
}

/// 按字符边界裁剪文本并在发生裁剪时追加省略号。
fn clip(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut output = value.chars().take(keep).collect::<String>();
    output.push('…');
    output
}

/// 创建一行单样式文本。
fn line(text: &str, color: Option<UiColor>, bold: bool) -> UiLine {
    UiLine {
        spans: vec![span(text, color, bold)],
    }
}

/// 创建一个稳定 UI 文本片段。
fn span(text: &str, color: Option<UiColor>, bold: bool) -> UiSpan {
    UiSpan {
        text: text.to_string(),
        style: UiStyle {
            foreground: color,
            bold,
            ..UiStyle::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 连续模型增量应合并为一条回复，工具事件使用完整共享载荷字段。
    #[test]
    fn event_lines_merge_model_text_and_render_tool_payloads() {
        let target = AgentId::parse("agent-1").expect("创建测试 Agent ID");
        let mut session = AgentViewSession::new(target);
        for (kind, payload) in [
            (AgentEventKind::ModelTextDelta, json!({"delta": "你好"})),
            (AgentEventKind::ModelTextDelta, json!({"delta": "，世界"})),
            (
                AgentEventKind::ToolStarted,
                json!({"id": "call-1", "name": "read_file", "args": {"path": "a.rs"}}),
            ),
            (
                AgentEventKind::ToolFinished,
                json!({"call_id": "call-1", "name": "read_file", "content": "done", "is_error": false}),
            ),
        ] {
            session.push_item(AgentViewItem::Event(AgentEvent {
                id: String::new(),
                run_id: "run-1".into(),
                timestamp_ms: 0,
                kind,
                step: 0,
                payload,
            }));
        }

        let lines = session.render(80, 12);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(text.contains("回复  你好，世界"), "{text}");
        assert!(text.contains("调用  read_file"), "{text}");
        assert!(text.contains("完成  read_file"), "{text}");
    }
}
