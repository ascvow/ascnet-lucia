//! 宿主原生 Agent 会话视图的渲染。
//!
//! 视图复用主界面的时间线与输入组件：消息、Markdown、工具卡片、滚动条与折行规则都只有
//! 一份实现，队员会话因此与主会话在视觉和交互上保持一致，而不是插件逐帧生成的纯文本。

use crate::app::{AgentSessionView, App};
use crate::theme::{
    COLOR_DANGER, COLOR_META, COLOR_MUTED, COLOR_SEPARATOR, COLOR_SUCCESS, COLOR_TEXT,
    COLOR_WARNING, SPINNER,
};
use crate::ui::chat::{
    conversation_sections, input_pane_rows, prompt_status_row, render_input_pane,
    render_session_status_row, render_transcript_pane, InputPane, TranscriptPane,
};
use crate::ui::markdown::truncate_line;
use crate::ui::plugin_render::render_subview_header;
use agent_runtime::AgentStatus;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

/// 正文区上下留白行数，与主界面保持一致。
const SESSION_PADDING_Y: u16 = 1;
/// 滚动条占用的列数。
const SCROLLBAR_WIDTH: u16 = 1;

/// 渲染原生 Agent 会话视图；面包屑、时间线、状态行与输入盒共用主界面组件。
pub(crate) fn render_agent_session_view(frame: &mut Frame, app: &mut App, outer: Rect) {
    let spinner_frame = app.spinner_frame;
    let breadcrumbs = app.view_stack.breadcrumbs().join(" / ");
    render_subview_header(frame, &breadcrumbs, outer);
    let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(outer);
    // 与插件子视图一致：整屏只保留一条顶部分隔线，不额外包盒子。
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(COLOR_SEPARATOR));
    let body = block.inner(sections[1]);
    frame.render_widget(block, sections[1]);

    let Some(view) = app.active_agent_session_view_mut() else {
        // 状态尚未建立：把导航随附的说明（或连接提示）居中显示，避免整屏空白。
        let notice = app
            .view_stack
            .active()
            .and_then(|state| state.body.notice())
            .map(str::to_string);
        render_placeholder(
            frame,
            body,
            notice.as_deref().unwrap_or("Connecting to Agent session…"),
            notice.is_some(),
        );
        return;
    };

    let placeholder = if view.has_input() {
        ""
    } else {
        match view.status {
            Some(AgentStatus::Running) => "Steer the Agent…",
            Some(AgentStatus::Queued | AgentStatus::Ready) => "Message the Agent after it starts…",
            Some(AgentStatus::Failed | AgentStatus::Cancelled) => "Agent is not accepting messages",
            _ => "Message the Agent…",
        }
    };
    let text_width = usize::from(body.width.saturating_sub(2)).max(1);
    let input_rows = input_pane_rows(&view.input, view.cursor, &[], &[], text_width);
    let [chat_section, _, _, input_area, footer_area] =
        conversation_sections(body, input_rows, 0, 0);
    let transcript_section = chat_section.inner(Margin {
        horizontal: 0,
        vertical: SESSION_PADDING_Y,
    });
    let chat_area = Rect {
        width: transcript_section.width.saturating_sub(SCROLLBAR_WIDTH),
        ..transcript_section
    };
    let status_area = prompt_status_row(chat_section, transcript_section);

    let mut below = None;
    view.jump_hint_area = None;
    view.scrollbar_area = None;
    if view.messages.is_empty() {
        view.is_scrollbar_hovered = false;
        view.hit_regions.clear();
        view.last_max_scroll = 0;
        view.last_viewport = chat_area.height;
        view.content_area = Some(chat_area);
        view.content_scroll = 0;
        view.content_rows = 0;
        render_empty_state(frame, chat_area, view);
    } else {
        let pane = TranscriptPane {
            messages: &view.messages,
            streaming: view.streaming_message,
            layout: &mut view.layout,
            scroll: &mut view.scroll,
            hit_regions: &mut view.hit_regions,
            last_max_scroll: &mut view.last_max_scroll,
            last_viewport: &mut view.last_viewport,
            content_area: &mut view.content_area,
            content_scroll: &mut view.content_scroll,
            content_rows: &mut view.content_rows,
            scrollbar_area: &mut view.scrollbar_area,
            is_scrollbar_hovered: &mut view.is_scrollbar_hovered,
            record_hit_regions: true,
        };
        let viewport = render_transcript_pane(frame, chat_area, pane);
        below = viewport.below;
    }
    if let Some(status_area) = status_area {
        render_status_row(frame, status_area, view, spinner_frame, below);
    }

    let input_pane = InputPane {
        input: &view.input,
        cursor: view.cursor,
        attachments: &[],
        refs: &[],
        running: !view.closed
            && matches!(
                view.status,
                Some(AgentStatus::Queued | AgentStatus::Running)
            ),
        focused: true,
        confirmation_pending: false,
        placeholder,
    };
    render_input_pane(frame, input_area, &input_pane);
    render_session_footer(frame, footer_area, view);
}

/// 绘制空时间线的引导文案：说明当前状态与可用按键，而不是留一片空白。
fn render_empty_state(frame: &mut Frame, area: Rect, view: &AgentSessionView) {
    if area.width < 20 || area.height == 0 {
        return;
    }
    let (title, color) = status_label(view);
    let mut lines = vec![
        Line::from(Span::styled(title, Style::new().fg(color).bold())),
        Line::default(),
        Line::from(Span::styled(
            "Tool calls and replies appear here as the Agent works.",
            Style::new().fg(COLOR_MUTED),
        )),
        Line::from(Span::styled(
            "Enter send · Shift+Enter newline · Esc back",
            Style::new().fg(COLOR_META),
        )),
    ];
    if view.closed {
        lines.insert(
            1,
            Line::from(Span::styled(
                "The event stream ended; new messages may be rejected.",
                Style::new().fg(COLOR_DANGER),
            )),
        );
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let text_area = Rect {
        x: area.x,
        y: area.y + (area.height - height) / 2,
        width: area.width,
        height,
    };
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        text_area,
    );
}

/// 绘制输入框上方的一行状态：左侧运行状态，右侧保留回看提示。
fn render_status_row(
    frame: &mut Frame,
    area: Rect,
    view: &mut AgentSessionView,
    spinner_frame: usize,
    below: Option<usize>,
) {
    let (mut label, color) = status_label(view);
    if !view.closed
        && matches!(
            view.status,
            Some(AgentStatus::Queued | AgentStatus::Running)
        )
    {
        let elapsed = view.status_since.elapsed().as_secs();
        label = format!(
            "{} {label} · {}",
            SPINNER[spinner_frame % SPINNER.len()],
            format_elapsed(elapsed)
        );
    }
    if view.closed {
        label.push_str("  · stream ended");
    }
    render_session_status_row(
        frame,
        area,
        Some((label, color)),
        below,
        &mut view.jump_hint_area,
    );
}

/// 绘制底栏：目标 Agent 身份与操作提示。
fn render_session_footer(frame: &mut Frame, area: Rect, view: &AgentSessionView) {
    let target = view.target.to_string();
    let mut spans = vec![
        Span::styled("Agent ", Style::new().fg(COLOR_META)),
        Span::styled(truncate_line(&target, 8), Style::new().fg(COLOR_TEXT)),
        Span::styled("  |  ", Style::new().fg(COLOR_SEPARATOR)),
        Span::styled("Enter send", Style::new().fg(COLOR_META)),
    ];
    if area.width >= 72 {
        spans.push(Span::styled(
            "  |  Shift+Enter newline",
            Style::new().fg(COLOR_META),
        ));
    }
    if area.width >= 96 {
        spans.extend([
            Span::styled("  |  PgUp/PgDn scroll", Style::new().fg(COLOR_META)),
            Span::styled("  |  Esc back", Style::new().fg(COLOR_META)),
        ]);
    }
    if view.has_input() {
        spans.push(Span::styled(
            "  |  Ctrl+C clear",
            Style::new().fg(COLOR_MUTED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 居中展示连接中/错误说明。
fn render_placeholder(frame: &mut Frame, area: Rect, text: &str, error: bool) {
    if area.is_empty() {
        return;
    }
    let color = if error { COLOR_DANGER } else { COLOR_MUTED };
    let row = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_line(text, usize::from(area.width)),
            Style::new().fg(color),
        )))
        .alignment(Alignment::Center),
        row,
    );
}

/// 返回当前状态的紧凑标签与语义颜色。
fn status_label(view: &AgentSessionView) -> (String, Color) {
    if view.closed
        && matches!(
            view.status,
            None | Some(AgentStatus::Queued | AgentStatus::Running)
        )
    {
        return ("Stream ended".into(), COLOR_MUTED);
    }
    match view.status {
        None => ("Connecting".into(), COLOR_MUTED),
        Some(AgentStatus::Ready) => ("Ready · waiting for a task".into(), COLOR_MUTED),
        Some(AgentStatus::Queued) => ("Queued".into(), COLOR_WARNING),
        Some(AgentStatus::Running) => ("Running".into(), COLOR_WARNING),
        Some(AgentStatus::Succeeded) => ("Completed".into(), COLOR_SUCCESS),
        Some(AgentStatus::Failed) => ("Failed".into(), COLOR_DANGER),
        Some(AgentStatus::Cancelled) => ("Cancelled".into(), COLOR_MUTED),
    }
}

/// 把秒数格式化为 `12s` 或 `3m21s`。
fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}
