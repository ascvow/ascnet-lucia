//! `/context` 的只读上下文占用视图，消费 Core 的脱敏模型请求快照。

use crate::app::App;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// 最近一次实际请求的分类估算，不保留提示正文或附件数据。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ContextSnapshot {
    /// 按内容类别统计的文本 token 估算。
    pub(crate) tokens: BTreeMap<String, u64>,
    /// 无法按文本估算的附件及不透明推理块数量。
    pub(crate) attachments: u64,
    /// 经过上下文加载器处理的消息数。
    pub(crate) messages: usize,
    /// 经过权限筛选的工具定义数。
    pub(crate) tools: usize,
}

/// 分类顺序及颜色保持固定，避免相邻请求改变读图含义。
const CATEGORIES: [(&str, &str, Color); 8] = [
    ("system", "System prompt", Color::Cyan),
    ("developer", "开发者指令", Color::Blue),
    ("tools", "工具定义", Color::Yellow),
    ("user", "用户消息", Color::Green),
    ("assistant", "助手消息", Color::LightGreen),
    ("calls", "工具调用", Color::Magenta),
    ("results", "工具结果", Color::LightRed),
    ("thinking", "推理文本", Color::LightCyan),
];

/// 用累计比例分配终端列；包含剩余容量，未知窗口或超限时按估算总量归一化。
fn segment_widths(values: &[u64], capacity: u64, width: u16) -> Vec<usize> {
    let total = values.iter().copied().fold(0_u64, u64::saturating_add);
    let denominator = capacity.max(total).max(1) as u128;
    let mut consumed = 0_u128;
    let mut previous = 0;
    let mut widths = Vec::new();
    for value in values {
        consumed += u128::from(*value);
        let end = ((consumed * u128::from(width)) / denominator) as usize;
        widths.push(end - previous);
        previous = end;
    }
    widths.push(usize::from(width).saturating_sub(previous));
    widths
}

/// 绘制可滚动的上下文检查视图；实际用量独立展示，不伪造分类的精确 token 数。
pub(crate) fn render_context_dialog(frame: &mut Frame, app: &mut App, outer: Rect) {
    if !app.native_command.context_open {
        return;
    }
    let width = outer.width.min(88);
    let height = outer.height.min(23);
    let area = Rect::new(
        outer.x + (outer.width - width) / 2,
        outer.y + (outer.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 上下文 · /context ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![Line::from("最近模型请求 · 分类为文本估算")];
    if let Some(snapshot) = &app.context_snapshot {
        let values: Vec<u64> = CATEGORIES
            .iter()
            .map(|(key, _, _)| snapshot.tokens.get(*key).copied().unwrap_or(0))
            .collect();
        let total = values.iter().copied().fold(0_u64, u64::saturating_add);
        let window = app.context_window.filter(|value| *value > 0);
        lines.push(Line::from(match window {
            Some(window) => format!(
                "约 {total} / {window} tokens · {:.1}%{}",
                total as f64 / window as f64 * 100.0,
                if total > window {
                    " · 超出窗口"
                } else {
                    ""
                }
            ),
            None => format!("约 {total} tokens · 窗口大小未知"),
        }));
        let widths = segment_widths(&values, window.unwrap_or(total), inner.width);
        let mut spans: Vec<Span> = CATEGORIES
            .iter()
            .zip(&widths)
            .map(|((_, _, color), width)| {
                Span::styled(" ".repeat(*width), Style::default().bg(*color))
            })
            .collect();
        spans.push(Span::styled(
            " ".repeat(widths[8]),
            Style::default().bg(Color::DarkGray),
        ));
        lines.push(Line::from(spans));
        lines.push(Line::from(""));
        for ((_, label, color), value) in CATEGORIES.iter().zip(values) {
            let ratio = if total == 0 {
                0.0
            } else {
                value as f64 / total as f64 * 100.0
            };
            lines.push(Line::from(vec![
                Span::styled("■ ", Style::default().fg(*color)),
                Span::raw(format!("{label}  ≈{value}  ({ratio:.1}% 内容占比)")),
            ]));
        }
        if let Some(window) = window {
            lines.push(Line::from(format!(
                "剩余容量（估算） {} tokens",
                window.saturating_sub(total)
            )));
        }
        lines.push(Line::from(format!(
            "{} 条消息 · {} 个工具定义",
            snapshot.messages, snapshot.tools
        )));
        if snapshot.attachments > 0 {
            lines.push(Line::from(format!(
                "另有 {} 个附件/不透明推理块，未计入估算",
                snapshot.attachments
            )));
        }
        if let Some(tokens) = app.context_tokens {
            lines.push(Line::from(format!(
                "服务商实际输入 {tokens} tokens（含缓存）"
            )));
        }
    } else {
        lines.push(Line::from("本次打开会话后尚无模型请求快照。"));
    }
    let usage = &app.session_usage;
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "本次打开会话累计：输出 {} · 缓存读 {} · 写 {}",
        usage.output_tokens.unwrap_or(0),
        usage.cache_read_tokens.unwrap_or(0),
        usage.cache_write_tokens.unwrap_or(0)
    )));
    lines.push(Line::from(
        "分类不含协议开销；尚未发送的消息不在最近请求内。",
    ));
    let content_height = lines
        .iter()
        .map(|line| crate::ui::wrapped_line_count(line, inner.width))
        .fold(0_u16, u16::saturating_add);
    app.native_command.context_scroll = app
        .native_command
        .context_scroll
        .min(content_height.saturating_sub(inner.height));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.native_command.context_scroll, 0)),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宽屏和窄屏均绘制完整色条；窄屏可滚动到底，换会话后不得残留旧统计。
    #[test]
    fn context_dialog_renders_colors_and_resets_with_session() {
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.native_command.context_open = true;
        app.context_window = Some(10000);
        app.context_snapshot = Some(ContextSnapshot {
            tokens: [
                ("system".into(), 1000),
                ("tools".into(), 2000),
                ("results".into(), 3000),
            ]
            .into(),
            attachments: 1,
            messages: 12,
            tools: 5,
        });
        for (width, height) in [(100, 30), (36, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            app.native_command.context_scroll = 0;
            terminal
                .draw(|frame| render_context_dialog(frame, &mut app, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            for color in [Color::Cyan, Color::Yellow, Color::LightRed, Color::DarkGray] {
                assert!(buffer.content().iter().any(|cell| cell.bg == color));
            }
            app.native_command.context_scroll = u16::MAX;
            terminal
                .draw(|frame| render_context_dialog(frame, &mut app, frame.area()))
                .unwrap();
            assert!(app.native_command.context_scroll < u16::MAX);
        }
        app.session_usage.cache_read_tokens = Some(100);
        app.replace_session(app.session_record.clone(), None);
        assert!(app.context_snapshot.is_none());
        assert!(app.session_usage.is_empty());
        assert!(!app.native_command.context_open);
    }

    /// 尚无快照时展示明确空态，不能把未知用量显示成精确的零占用。
    #[test]
    fn context_dialog_without_request_has_no_colored_usage() {
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.native_command.context_open = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render_context_dialog(frame, &mut app, frame.area()))
            .unwrap();
        assert!(!terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.bg == Color::Cyan));
    }

    /// 分段取整后总宽度必须守恒；空输入、未知窗口和超限都不能溢出终端。
    #[test]
    fn segmented_bar_preserves_width() {
        for width in [0, 1, 17, 80] {
            for (values, capacity) in [
                (vec![2, 3], 10),
                (vec![0, 0], 0),
                (vec![7, 4], 5),
                (vec![1, 1, 1], 0),
            ] {
                let widths = segment_widths(&values, capacity, width);
                assert_eq!(widths.iter().sum::<usize>(), usize::from(width));
            }
        }
        assert_eq!(segment_widths(&[2, 3], 10, 20), vec![4, 6, 10]);
    }
}
