//! `/context` 的只读上下文占用视图，消费 Core 的脱敏模型请求快照。

use crate::app::App;
use crate::ui::sheet::{palette_hint, palette_sheet_viewport, render_palette_sheet, PaletteSheet};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
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
///
/// `anchor` 是下方输入盒的区域：面板与它同宽、贴在它上沿，向上占满能够容纳的行数。
/// 终端矮到装不下完整内容时靠 `↑`/`↓` 滚动，而不是把行裁成半截。
pub(crate) fn render_context_dialog(frame: &mut Frame, app: &mut App, anchor: Rect) {
    if !app.native_command.context_open {
        return;
    }
    let mut lines = vec![Line::from("最近模型请求 · 分类为文本估算")];
    // 占比色条与逻辑行下标一起保存：它整行都是空格，混在折行段落里会被算成两行。
    let mut bar: Option<(usize, Line<'static>)> = None;
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
        let widths = segment_widths(&values, window.unwrap_or(total), anchor.width);
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
        bar = Some((lines.len(), Line::from(spans)));
        // 占位空行保住色条那一行的高度：段落折行与滚动几何仍按一行计算。
        lines.push(Line::from(""));
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
        .map(|line| crate::ui::wrapped_line_count(line, anchor.width))
        .fold(0_u16, u16::saturating_add);
    // 色条在滚动内容里的视觉行号：它之前每一行的折行结果都要算进去。
    let bar_row = bar.as_ref().map(|(index, _)| {
        lines[..*index]
            .iter()
            .map(|line| crate::ui::wrapped_line_count(line, anchor.width))
            .fold(0_u16, u16::saturating_add)
    });
    let requested_rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let (height, content_rows) = palette_sheet_viewport(frame, anchor, requested_rows, true);
    app.native_command.context_scroll = app
        .native_command
        .context_scroll
        .min(content_height.saturating_sub(content_rows));
    let scroll = app.native_command.context_scroll;
    let content_area = render_palette_sheet(
        frame,
        anchor,
        height,
        PaletteSheet {
            title: "上下文 · /context",
            lines,
            hint: Some(palette_hint("↑↓ 滚动 · Esc 关闭")),
            scroll,
            wrap: true,
        },
    );
    // 色条单独落在占位行上：不折行，因此始终只占一行，也不会吃掉后面一行。
    if let (Some((_, bar)), Some(row)) = (bar, bar_row) {
        if row >= scroll && row - scroll < content_rows {
            frame.render_widget(
                Paragraph::new(bar),
                Rect {
                    x: content_area.x,
                    y: content_area.y.saturating_add(row - scroll),
                    width: content_area.width,
                    height: 1,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造贴住画布底部的输入盒锚点；面板据此向上生长。
    fn anchor(frame: &Frame) -> Rect {
        Rect {
            x: 0,
            y: frame.area().height,
            width: frame.area().width,
            height: 1,
        }
    }

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
                .draw(|frame| {
                    let anchor = anchor(frame);
                    render_context_dialog(frame, &mut app, anchor)
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for color in [Color::Cyan, Color::Yellow, Color::LightRed, Color::DarkGray] {
                assert!(buffer.content().iter().any(|cell| cell.bg == color));
            }
            app.native_command.context_scroll = u16::MAX;
            terminal
                .draw(|frame| {
                    let anchor = anchor(frame);
                    render_context_dialog(frame, &mut app, anchor)
                })
                .unwrap();
            assert!(app.native_command.context_scroll < u16::MAX);
        }
        app.session_usage.cache_read_tokens = Some(100);
        app.replace_session(app.session_record.clone(), None);
        assert!(app.context_snapshot.is_none());
        assert!(app.session_usage.is_empty());
        assert!(!app.native_command.context_open);
    }

    /// 上下文面板用命令面板的外形：标题画在上分隔线上，四周没有盒子边框。
    #[test]
    fn context_dialog_uses_the_command_palette_chrome() {
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.native_command.context_open = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let anchor = anchor(frame);
                render_context_dialog(frame, &mut app, anchor)
            })
            .unwrap();
        let lines: Vec<String> = (0..24u16)
            .map(|row| {
                terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .skip(usize::from(row) * 80)
                    .take(80)
                    .map(|cell| cell.symbol())
                    .collect()
            })
            .collect();
        let screen = lines.join("\n");
        // 宽字符占两格、第二格是空白；去掉空白后再匹配标题与提示。
        let compact: Vec<String> = lines
            .iter()
            .map(|line| line.chars().filter(|ch| !ch.is_whitespace()).collect())
            .collect();
        let title_row = compact
            .iter()
            .position(|line| line.contains("上下文·/context"))
            .unwrap_or_else(|| panic!("标题应画在面板上：{screen}"));
        assert!(
            lines[title_row].contains('─'),
            "标题必须画在上分隔线上：{screen}"
        );
        assert!(
            compact.iter().any(|line| line.contains("Esc关闭")),
            "{screen}"
        );
        // 面板不再有左右与下边框：盒子只剩一条朝向正文的上分隔线。
        assert!(!screen.contains('╭'), "{screen}");
        assert!(!screen.contains('│'), "{screen}");
    }

    /// 色条必须只占一行：它多算的那一行会把最后一条说明挤出视口。
    #[test]
    fn utilization_bar_does_not_steal_a_row_from_the_caveat() {
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
            attachments: 0,
            messages: 12,
            tools: 5,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let anchor = anchor(frame);
                render_context_dialog(frame, &mut app, anchor)
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let compact: String = screen.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            compact.contains("分类不含协议开销"),
            "色条多占一行会把这条说明挤出视口：{screen}"
        );
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
            .draw(|frame| {
                let anchor = anchor(frame);
                render_context_dialog(frame, &mut app, anchor)
            })
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
