//! 插件复用的派生 Agent 主界面状态与交互逻辑。

use crate::{
    AgentContinueRequest, AgentEvent, AgentEventKind, AgentHandle, AgentId, AgentStatus,
    PluginHostApi, UiColor, UiCursor, UiInputEvent, UiLine, UiSpan, UiStyle,
};
use std::collections::VecDeque;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
                self.feedback = Some(Err(format!("Failed to read Agent status: {error}")));
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
                self.feedback = Some(Err(format!("Failed to read Agent events: {error}")));
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

    /// 按给定尺寸渲染事件时间线、交互反馈和固定在底部的输入框。
    pub fn render(&self, width: u16, height: u16) -> Vec<UiLine> {
        const INPUT_BOX_HEIGHT: usize = 3;
        let height = usize::from(height);
        let feedback_height = usize::from(self.feedback.is_some());
        let available = height.saturating_sub(INPUT_BOX_HEIGHT + feedback_height);
        let event_lines = agent_event_lines(&self.timeline, usize::from(width), self.status);
        let mut lines = event_lines
            .into_iter()
            .rev()
            .take(available)
            .rev()
            .collect::<Vec<_>>();
        if lines.is_empty() && available > 0 {
            lines.push(line(
                match self.status {
                    Some(AgentStatus::Queued) => "Waiting for capacity",
                    Some(AgentStatus::Ready) => "Waiting for a task",
                    _ => "Waiting for Agent events",
                },
                Some(UiColor::Gray),
                false,
            ));
        }
        while lines.len() < available {
            lines.push(UiLine { spans: Vec::new() });
        }
        if let Some(feedback) = &self.feedback {
            let (text, color) = match feedback {
                Ok(text) => (text.as_str(), UiColor::Green),
                Err(text) => (text.as_str(), UiColor::Red),
            };
            lines.push(line(text, Some(color), false));
        }
        lines.extend(input_box_lines(&self.input, usize::from(width)));
        lines.truncate(height);
        lines
    }

    /// 返回输入末尾相对 Agent 视图左上角的真实终端光标位置。
    ///
    /// 高度不足以完整显示三行输入框时不声明光标，避免宿主把光标放到边框外。
    pub fn cursor_position(&self, width: u16, height: u16) -> Option<UiCursor> {
        if width < 4 || height < 3 {
            return None;
        }
        let text_width = usize::from(width).saturating_sub(4);
        let displayed = clip_to_width(&self.input, text_width);
        Some(UiCursor {
            x: u16::try_from(2usize.saturating_add(displayed.width())).unwrap_or(u16::MAX),
            y: height.saturating_sub(2),
        })
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
                self.feedback = Some(Err(format!("Failed to read Agent status: {error}")));
                return None;
            }
        };
        let result = match snapshot.status {
            AgentStatus::Queued | AgentStatus::Running => host
                .steer_agent(&self.target, &input)
                .map(|_| (None, "Message sent to Agent".to_string())),
            AgentStatus::Succeeded => host
                .continue_agent(&AgentContinueRequest::new(
                    self.target.clone(),
                    input.clone(),
                ))
                .map(|handle| (Some(handle), "Started an Agent follow-up".to_string())),
            AgentStatus::Ready | AgentStatus::Failed | AgentStatus::Cancelled => Err(
                anyhow::anyhow!("The Agent cannot receive messages in its current state"),
            ),
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

/// 主 Agent 与派生 Agent 共用的会话消息形态。
enum AgentViewMessage {
    /// 从当前会话输入框提交的用户消息。
    User(String),
    /// 合并同一回复的连续模型文本增量。
    Assistant(String),
    /// 按调用 ID 合并开始、完成或跳过状态的工具消息。
    Tool(AgentViewTool),
    /// 需要在时间线中保留的运行错误。
    Error(String),
}

/// 工具调用在派生 Agent 时间线中的合并展示状态。
struct AgentViewTool {
    call_id: String,
    name: String,
    args: serde_json::Value,
    result: Option<serde_json::Value>,
    state: AgentViewToolState,
}

/// 工具消息圆点、结果和状态附注所使用的视觉状态。
enum AgentViewToolState {
    Running,
    Succeeded,
    Failed,
    Skipped,
}

/// 将 Runtime 事件归并为主界面的用户、助手和工具消息结构。
fn agent_event_lines(
    timeline: &VecDeque<AgentViewItem>,
    width: usize,
    status: Option<AgentStatus>,
) -> Vec<UiLine> {
    let mut messages = Vec::<AgentViewMessage>::new();
    for item in timeline {
        let event = match item {
            AgentViewItem::Event(event) => event,
            AgentViewItem::User(message) => {
                messages.push(AgentViewMessage::User(message.clone()));
                continue;
            }
        };
        match event.kind {
            AgentEventKind::ModelTextDelta => {
                let delta = event.payload["delta"].as_str().unwrap_or_default();
                if let Some(AgentViewMessage::Assistant(text)) = messages.last_mut() {
                    text.push_str(delta);
                } else if !delta.is_empty() {
                    messages.push(AgentViewMessage::Assistant(delta.to_string()));
                }
            }
            AgentEventKind::ToolStarted => {
                messages.push(AgentViewMessage::Tool(AgentViewTool {
                    call_id: event.payload["id"].as_str().unwrap_or_default().to_string(),
                    name: event.payload["name"].as_str().unwrap_or("tool").to_string(),
                    args: event.payload["args"].clone(),
                    result: None,
                    state: AgentViewToolState::Running,
                }));
            }
            AgentEventKind::ToolFinished => {
                let call_id = event.payload["call_id"].as_str().unwrap_or_default();
                let failed = event.payload["is_error"].as_bool().unwrap_or(false);
                let state = if failed {
                    AgentViewToolState::Failed
                } else {
                    AgentViewToolState::Succeeded
                };
                if let Some(tool) = find_tool_mut(&mut messages, call_id) {
                    tool.result = Some(event.payload["content"].clone());
                    tool.state = state;
                } else {
                    messages.push(AgentViewMessage::Tool(AgentViewTool {
                        call_id: call_id.to_string(),
                        name: event.payload["name"].as_str().unwrap_or("tool").to_string(),
                        args: serde_json::Value::Null,
                        result: Some(event.payload["content"].clone()),
                        state,
                    }));
                }
            }
            AgentEventKind::ToolSkipped => {
                let call = &event.payload["call"];
                let call_id = call["id"].as_str().unwrap_or_default();
                if let Some(tool) = find_tool_mut(&mut messages, call_id) {
                    tool.state = AgentViewToolState::Skipped;
                } else {
                    messages.push(AgentViewMessage::Tool(AgentViewTool {
                        call_id: call_id.to_string(),
                        name: call["name"].as_str().unwrap_or("tool").to_string(),
                        args: call["args"].clone(),
                        result: None,
                        state: AgentViewToolState::Skipped,
                    }));
                }
            }
            AgentEventKind::StepLimitReached => {
                messages.push(AgentViewMessage::Error("Run step limit reached".into()));
            }
            AgentEventKind::RunStarted
            | AgentEventKind::TurnStarted
            | AgentEventKind::SteeringInjected
            | AgentEventKind::RunFinished
            | AgentEventKind::Extension
            | AgentEventKind::ModelRequest
            | AgentEventKind::ModelThinkingDelta
            | AgentEventKind::ModelResponse
            | AgentEventKind::BillingUsage
            | AgentEventKind::TurnFinished
            | AgentEventKind::FollowUpInjected => {}
        }
    }
    let streaming = matches!(status, Some(AgentStatus::Running))
        && matches!(messages.last(), Some(AgentViewMessage::Assistant(_)));
    let mut lines = messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| {
            render_agent_message(message, width, streaming && index + 1 == messages.len())
        })
        .collect::<Vec<_>>();
    if matches!(status, Some(AgentStatus::Queued | AgentStatus::Running)) && !streaming {
        lines.push(UiLine {
            spans: vec![
                span("◐ ", Some(UiColor::Yellow), false),
                span("Working...", Some(UiColor::Gray), false),
            ],
        });
    }
    lines
}

/// 返回与完成事件调用 ID 对应的最近工具消息。
fn find_tool_mut<'a>(
    messages: &'a mut [AgentViewMessage],
    call_id: &str,
) -> Option<&'a mut AgentViewTool> {
    messages.iter_mut().rev().find_map(|message| match message {
        AgentViewMessage::Tool(tool) if tool.call_id == call_id => Some(tool),
        _ => None,
    })
}

/// 使用主界面的视觉层级渲染一条派生 Agent 消息。
fn render_agent_message(message: &AgentViewMessage, width: usize, streaming: bool) -> Vec<UiLine> {
    match message {
        AgentViewMessage::User(text) => user_message_lines(text, width),
        AgentViewMessage::Assistant(text) => assistant_message_lines(text, width, streaming),
        AgentViewMessage::Tool(tool) => tool_message_lines(tool, width),
        AgentViewMessage::Error(text) => vec![UiLine {
            spans: vec![
                span("✗ ", Some(UiColor::Red), true),
                span(text, Some(UiColor::Red), false),
            ],
        }],
    }
}

/// 绘制与主界面一致的左侧强调用户消息块。
fn user_message_lines(text: &str, width: usize) -> Vec<UiLine> {
    let text_width = width.saturating_sub(3).max(1);
    let mut lines = wrap_display(text, text_width)
        .into_iter()
        .map(|text| {
            let padding = " ".repeat(text_width.saturating_sub(text.width()));
            UiLine {
                spans: vec![
                    styled_span("▌ ", Some(UiColor::Magenta), Some(UiColor::Black), false),
                    styled_span(
                        &format!("{text}{padding} "),
                        Some(UiColor::White),
                        Some(UiColor::Black),
                        false,
                    ),
                ],
            }
        })
        .collect::<Vec<_>>();
    lines.push(UiLine { spans: Vec::new() });
    lines
}

/// 绘制无角色标签的助手正文，并在流式回复末尾显示光标。
fn assistant_message_lines(text: &str, width: usize, streaming: bool) -> Vec<UiLine> {
    let mut lines = text
        .lines()
        .flat_map(|line| wrap_display(line, width.max(1)))
        .map(|line| UiLine {
            spans: vec![span(&line, Some(UiColor::White), false)],
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(UiLine { spans: Vec::new() });
    }
    if streaming {
        lines
            .last_mut()
            .expect("助手消息至少包含一行")
            .spans
            .push(span(" ▌", Some(UiColor::Magenta), false));
    }
    lines.push(UiLine { spans: Vec::new() });
    lines
}

/// 绘制主界面样式的单个工具块，并限制结果预览高度。
fn tool_message_lines(tool: &AgentViewTool, width: usize) -> Vec<UiLine> {
    let (color, note) = match tool.state {
        AgentViewToolState::Running => (UiColor::Yellow, ""),
        AgentViewToolState::Succeeded => (UiColor::Green, ""),
        AgentViewToolState::Failed => (UiColor::Red, "failed"),
        AgentViewToolState::Skipped => (UiColor::Gray, "skipped"),
    };
    let args = summarize_json(&tool.args, 64);
    let mut first = vec![
        span("● ", Some(color), false),
        span(&tool.name, Some(UiColor::White), true),
    ];
    if !args.is_empty() {
        first.push(span(&format!("({args})"), Some(UiColor::Gray), false));
    }
    if !note.is_empty() {
        first.push(span(&format!("  {note}"), Some(color), false));
    }
    let mut lines = vec![UiLine { spans: first }];
    let preview_width = width.saturating_sub(5).max(1);
    for (index, result) in tool_result_lines(tool.result.as_ref(), 6, 96)
        .into_iter()
        .enumerate()
    {
        let prefix = if index == 0 { "  └ " } else { "    " };
        let result_color = if matches!(tool.state, AgentViewToolState::Failed) {
            UiColor::Red
        } else if result.starts_with('+') {
            UiColor::Green
        } else if result.starts_with('-') {
            UiColor::Red
        } else {
            UiColor::Gray
        };
        lines.push(UiLine {
            spans: vec![
                span(prefix, Some(UiColor::Gray), false),
                span(
                    &clip_to_width(&result, preview_width),
                    Some(result_color),
                    false,
                ),
            ],
        });
    }
    lines.push(UiLine { spans: Vec::new() });
    lines
}

/// 绘制固定高度的输入框，使成员事件增长时仍保留稳定的交互位置。
fn input_box_lines(input: &str, width: usize) -> Vec<UiLine> {
    let border_width = width.saturating_sub(2);
    let text_width = width.saturating_sub(4);
    let displayed = if input.is_empty() {
        "Message Agent..."
    } else {
        input
    };
    let displayed = clip_to_width(displayed, text_width);
    let padding = " ".repeat(text_width.saturating_sub(displayed.width()));
    let border = format!("┌{}┐", "─".repeat(border_width));
    let bottom = format!("└{}┘", "─".repeat(border_width));
    vec![
        line(&border, Some(UiColor::Gray), false),
        UiLine {
            spans: vec![
                span("│ ", Some(UiColor::Gray), false),
                span(
                    &displayed,
                    Some(if input.is_empty() {
                        UiColor::Gray
                    } else {
                        UiColor::White
                    }),
                    false,
                ),
                span(&padding, None, false),
                span(" │", Some(UiColor::Gray), false),
            ],
        },
        line(&bottom, Some(UiColor::Gray), false),
    ]
}

/// 按终端显示宽度裁剪输入内容，避免中文或宽字符挤出右侧边框。
fn clip_to_width(value: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0usize;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width) > max_width {
            break;
        }
        output.push(character);
        width = width.saturating_add(character_width);
    }
    output
}

/// 按终端显示宽度拆分文本，保证用户消息背景和助手正文不会越界。
fn wrap_display(value: &str, max_width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut width = 0usize;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width) > max_width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            width = 0;
        }
        current.push(character);
        width = width.saturating_add(character_width);
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 将工具参数压缩成与主界面一致的键值摘要。
fn summarize_json(value: &serde_json::Value, max_width: usize) -> String {
    let text = match value {
        serde_json::Value::Null => return String::new(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    serde_json::Value::String(value) => value.clone(),
                    serde_json::Value::Array(items) => format!("[{} items]", items.len()),
                    serde_json::Value::Object(_) => "{...}".to_string(),
                    value => value.to_string(),
                };
                format!("{key}: {value}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::String(value) => value.clone(),
        value => value.to_string(),
    };
    truncate_line(&text, max_width)
}

/// 提取工具结果的有限多行预览，防止大结果占满会话视口。
fn tool_result_lines(
    value: Option<&serde_json::Value>,
    max_lines: usize,
    max_width: usize,
) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let text = match value {
        serde_json::Value::Object(map) => match map.get("content") {
            Some(serde_json::Value::String(text)) => text.as_str(),
            _ => return vec![summarize_json(value, max_width)],
        },
        serde_json::Value::String(text) => text.as_str(),
        _ => return vec![summarize_json(value, max_width)],
    };
    let total = text.lines().count();
    let mut lines = text
        .lines()
        .take(max_lines)
        .map(|line| truncate_line(line.trim_end(), max_width))
        .collect::<Vec<_>>();
    if total > max_lines {
        lines.push(format!("… {total} lines"));
    }
    lines
}

/// 将文本压平为一行并按显示宽度截断。
fn truncate_line(value: &str, max_width: usize) -> String {
    let flattened = value.replace(['\n', '\r', '\t'], " ");
    let clipped = clip_to_width(&flattened, max_width);
    if clipped.len() < flattened.len() {
        format!(
            "{}…",
            clip_to_width(&flattened, max_width.saturating_sub(1))
        )
    } else {
        clipped
    }
}

/// 创建一行单样式文本。
fn line(text: &str, color: Option<UiColor>, bold: bool) -> UiLine {
    UiLine {
        spans: vec![span(text, color, bold)],
    }
}

/// 创建一个稳定 UI 文本片段。
fn span(text: &str, color: Option<UiColor>, bold: bool) -> UiSpan {
    styled_span(text, color, None, bold)
}

/// 创建同时声明前景色和背景色的稳定 UI 文本片段。
fn styled_span(
    text: &str,
    foreground: Option<UiColor>,
    background: Option<UiColor>,
    bold: bool,
) -> UiSpan {
    UiSpan {
        text: text.to_string(),
        style: UiStyle {
            foreground,
            background,
            bold,
            ..UiStyle::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 连续模型增量与工具事件应归并为主界面的助手正文和工具块。
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
        assert!(text.contains("你好，世界"), "{text}");
        assert!(!text.contains("Reply"), "{text}");
        assert!(text.contains("● read_file(path: a.rs)"), "{text}");
        assert!(text.contains("└ done"), "{text}");
        assert_eq!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.text == "● ")
                .and_then(|span| span.style.foreground),
            Some(UiColor::Green)
        );
    }

    /// 本地用户消息应使用主界面的强调块，而不是带角色标签的日志行。
    #[test]
    fn local_user_message_uses_main_conversation_style() {
        let target = AgentId::parse("agent-user").expect("创建测试 Agent ID");
        let mut session = AgentViewSession::new(target);
        session.push_item(AgentViewItem::User("检查这个项目".into()));

        let lines = session.render(24, 8);

        assert_eq!(lines[0].spans[0].text, "▌ ");
        assert_eq!(lines[0].spans[0].style.foreground, Some(UiColor::Magenta));
        assert_eq!(lines[0].spans[1].style.background, Some(UiColor::Black));
        assert!(!lines[0].spans.iter().any(|span| span.text.contains("You")));
    }

    /// 事件数量超过视口后，输入框仍应固定在底部并保持完整边框宽度。
    #[test]
    fn input_box_stays_at_bottom_of_agent_view() {
        let target = AgentId::parse("agent-input").expect("创建测试 Agent ID");
        let mut session = AgentViewSession::new(target);
        session.input = "review this".into();
        for step in 0..12 {
            session.push_item(AgentViewItem::Event(AgentEvent {
                id: format!("event-{step}"),
                run_id: "run-input".into(),
                timestamp_ms: 0,
                kind: AgentEventKind::TurnStarted,
                step,
                payload: serde_json::Value::Null,
            }));
        }

        let lines = session.render(24, 8);

        assert_eq!(lines.len(), 8);
        assert_eq!(lines[5].spans[0].text.width(), 24);
        assert_eq!(lines[7].spans[0].text.width(), 24);
        assert!(lines[6]
            .spans
            .iter()
            .any(|span| span.text.contains("review this")));
        assert_eq!(
            session.cursor_position(24, 8),
            Some(UiCursor { x: 13, y: 6 })
        );
    }
}
