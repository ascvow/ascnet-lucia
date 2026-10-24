//! `/context` 的只读上下文占用视图，消费 Core 的脱敏模型请求快照。
//!
//! 明细按列对齐而不是把数字串成一句话：这张表的意义是横向比较"哪一类在吃容量"，
//! 数字左对齐、每行再跟一段长短不一的说明，会让同名列落在不同位置，读表退化成逐行读句。

use crate::app::App;
use crate::theme::{
    COLOR_ACCENT, COLOR_DANGER_BRIGHT, COLOR_META, COLOR_MUTED, COLOR_SEPARATOR, COLOR_SUCCESS,
    COLOR_TEXT, COLOR_TEXT_SOFT, COLOR_USER, COLOR_WARNING,
};
use crate::ui::sheet::{palette_hint, palette_sheet_viewport, render_palette_sheet, PaletteSheet};
use agent_core::model::TokenUsage;
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use serde::Deserialize;
use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;

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

/// 分类顺序、标签与颜色保持固定：同一类在分段条、明细行与逐行条形图里永远是同一格、同一色。
///
/// 配色按色相排开并错开明度，而不是直接借终端 16 色：`Cyan`/`LightCyan`、`Green`/`LightGreen`
/// 这类成对色在深色主题下几乎分不出来，而 `LightRed` 会让占比最大的工具结果看起来像一条
/// 错误提示。工具结果取暗钢蓝：它通常是体量最大的一块，必须退到背景里，让真正该关注的
/// 类别跳出来。
const CATEGORIES: [(&str, &str, Color); 8] = [
    ("system", "系统提示", COLOR_ACCENT),
    ("developer", "开发者指令", Color::Rgb(150, 148, 214)),
    ("tools", "工具定义", Color::Rgb(214, 168, 118)),
    ("user", "用户消息", COLOR_USER),
    ("assistant", "助手消息", COLOR_SUCCESS),
    ("calls", "工具调用", Color::Rgb(196, 132, 190)),
    ("results", "工具结果", Color::Rgb(112, 126, 148)),
    ("thinking", "推理文本", Color::Rgb(176, 196, 132)),
];

/// 标记列宽度：`■` 加一列空格。
const MARKER_WIDTH: usize = 2;
/// 相邻列之间的固定间距。
const COLUMN_GAP: usize = 2;
/// 占比列宽度：刚好放得下 `100.0%`。
const RATIO_WIDTH: usize = 6;
/// 标签列的最小可读宽度：两个汉字。再窄就认不出分类了。
const MIN_LABEL_WIDTH: usize = 4;
/// 逐行条形图的最小宽度：比这更短的条看不出比例，只会变成噪点。
const MIN_BAR_WIDTH: usize = 8;

/// 分类明细各列的宽度预算，由面板宽度与本次快照的数字长度共同决定。
#[derive(Debug, Default, Clone, Copy)]
struct Columns {
    /// 分类名所在列（按显示宽度算，含补白）。
    label: usize,
    /// token 数列；右对齐，保证个位对齐。
    tokens: usize,
    /// 内容占比列。
    ratio: usize,
    /// 行尾占比条可用列数；为零时不画。
    bar: usize,
}

impl Columns {
    /// 按"数值优先"的顺序分配列宽：标记 → 标签 → token 数 → 占比 → 条形图。
    ///
    /// 面板变窄时先丢条形图，再丢占比列，最后才把标签截短：装饰可以没有，
    /// 分类名和数字必须读得出来。`token_width` 由调用方按本次快照里最长的数字算好，
    /// 因此同一张表里所有数字都落在同一列。
    fn for_width(width: usize, label_max: usize, token_width: usize) -> Self {
        let label_max = label_max.max(MIN_LABEL_WIDTH);
        let room = width.saturating_sub(MARKER_WIDTH);
        // 标签列之后固定存在的部分：间距 + 数字 + 间距 + 占比。
        let tail = COLUMN_GAP + token_width + COLUMN_GAP + RATIO_WIDTH;
        if room >= label_max + tail {
            // 条形图还要自己的列间距；放不下最小宽度就整列不画，而不是画一条残条。
            let spare = room - label_max - tail;
            let bar = if spare >= COLUMN_GAP + MIN_BAR_WIDTH {
                spare - COLUMN_GAP
            } else {
                0
            };
            return Self {
                label: label_max,
                tokens: token_width,
                ratio: RATIO_WIDTH,
                bar,
            };
        }
        // 标签列收窄到剩余空间，但不少于两个汉字。
        if room >= MIN_LABEL_WIDTH + tail {
            return Self {
                label: label_max.min(room - tail),
                tokens: token_width,
                ratio: RATIO_WIDTH,
                bar: 0,
            };
        }
        // 再窄就放弃占比列：占比是派生值，token 数才是原始数据。
        let tail = COLUMN_GAP + token_width;
        if room >= MIN_LABEL_WIDTH + tail {
            return Self {
                label: label_max.min(room - tail),
                tokens: token_width,
                ratio: 0,
                bar: 0,
            };
        }
        // 只剩标记与标签时仍要认出分类；数字全丢也不画出边界。
        if room >= MIN_LABEL_WIDTH {
            return Self {
                label: label_max.min(room),
                tokens: 0,
                ratio: 0,
                bar: 0,
            };
        }
        Self::default()
    }

    /// 一行实际占用的列数；列预算守恒是"明细行永不折行"的唯一保证。
    ///
    /// 只供测试校验：绘制侧不依赖它，避免生产路径为了自检再算一遍列宽。
    #[cfg(test)]
    fn total(&self) -> usize {
        let mut total = MARKER_WIDTH + self.label;
        for width in [self.tokens, self.ratio, self.bar] {
            if width > 0 {
                total += COLUMN_GAP + width;
            }
        }
        total
    }
}

/// 千位分组：五位以上的 token 数不分组几乎无法直观比较量级。
fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

/// 按显示宽度右对齐：`format!` 的对齐按字符数算，中文列头会差出半列。
///
/// 只补白、不裁剪：被截断的数字就是在撒谎，因此 `width` 不小于数字本身宽度这件事
/// 由调用方（列预算按本次快照最长数字算出）保证。
fn align_right(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(UnicodeWidthStr::width(text));
    format!("{}{text}", " ".repeat(pad))
}

/// 把标签截断或补空格到列宽；窄面板里宁可让标签带省略号，也不能把后面的数字挤出行外。
fn fit_label(label: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let label_width = UnicodeWidthStr::width(label);
    if label_width <= width {
        return format!("{label}{}", " ".repeat(width - label_width));
    }
    // 省略号自己也要占一列，所以先让出这一列再截断。
    let truncated = crate::ui::markdown::truncate_line(label, width - 1);
    let truncated_width = UnicodeWidthStr::width(truncated.as_str());
    format!(
        "{truncated}{}",
        " ".repeat(width.saturating_sub(truncated_width))
    )
}

/// 用量比例的告警分级：平常读成"还很空"，接近窗口转警戒，超限才是警报。
fn usage_color(percent: f64) -> Color {
    if percent > 100.0 {
        COLOR_DANGER_BRIGHT
    } else if percent >= 70.0 {
        COLOR_WARNING
    } else {
        COLOR_SUCCESS
    }
}

/// 构造表头：列含义只解释一次，不必让每一行都重复"内容占比"这种后缀。
fn column_header(columns: &Columns) -> Line<'static> {
    let meta = Style::new().fg(COLOR_META);
    let mut spans = vec![Span::raw(" ".repeat(MARKER_WIDTH))];
    if columns.label > 0 {
        spans.push(Span::styled(fit_label("分类", columns.label), meta));
    }
    if columns.tokens > 0 {
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        spans.push(Span::styled(align_right("tokens", columns.tokens), meta));
    }
    if columns.ratio > 0 {
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        spans.push(Span::styled(align_right("占比", columns.ratio), meta));
    }
    Line::from(spans)
}

/// 构造一条分类明细行：色块、分类名、token 数、内容占比与等长的占比条。
///
/// `ratio` 是该类占内容的百分比；条形图按同一比例缩放，把"谁在吃容量"从数字变成形状。
fn category_row(
    label: &str,
    color: Color,
    value: u64,
    ratio: f64,
    columns: &Columns,
) -> Line<'static> {
    let mut spans = vec![Span::styled("■ ", Style::new().fg(color))];
    if columns.label > 0 {
        spans.push(Span::styled(
            fit_label(label, columns.label),
            Style::new().fg(COLOR_TEXT_SOFT),
        ));
    }
    if columns.tokens > 0 {
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        spans.push(Span::styled(
            align_right(&group_thousands(value), columns.tokens),
            Style::new().fg(COLOR_TEXT),
        ));
    }
    if columns.ratio > 0 {
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        spans.push(Span::styled(
            align_right(&format!("{ratio:.1}%"), columns.ratio),
            Style::new().fg(COLOR_META),
        ));
    }
    if columns.bar > 0 {
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        let filled = ((ratio / 100.0) * columns.bar as f64).round() as usize;
        // 非零占比至少留一格：否则"有但极小"会被画成"没有"。
        let filled = if value == 0 {
            0
        } else {
            filled.clamp(1, columns.bar)
        };
        spans.push(Span::styled(" ".repeat(filled), Style::new().bg(color)));
        spans.push(Span::styled(
            " ".repeat(columns.bar - filled),
            Style::new().bg(COLOR_SEPARATOR),
        ));
    }
    Line::from(spans)
}

/// 与明细表共用列宽的收尾行：数字仍落在 token 列，读者不必换一种读法。
fn summary_row(label: &str, value: Option<&str>, columns: &Columns) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(MARKER_WIDTH))];
    if columns.label > 0 {
        spans.push(Span::styled(
            fit_label(label, columns.label),
            Style::new().fg(COLOR_MUTED),
        ));
    }
    if columns.tokens > 0 {
        if let Some(value) = value {
            spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
            spans.push(Span::styled(
                align_right(value, columns.tokens),
                Style::new().fg(COLOR_TEXT_SOFT),
            ));
        }
    }
    Line::from(spans)
}

/// 说明性备注：统一用元信息色，避免与数据行争夺注意力。
fn note_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::new().fg(COLOR_META)))
}

/// 窗口占用行：总量与窗口是这一屏最重要的两个数，占比随用量变色。
fn window_line(total: u64, window: Option<u64>, percent: Option<f64>) -> Line<'static> {
    let mut spans = vec![
        Span::styled("约 ", Style::new().fg(COLOR_MUTED)),
        Span::styled(group_thousands(total), Style::new().fg(COLOR_TEXT).bold()),
    ];
    if let (Some(window), Some(percent)) = (window, percent) {
        spans.push(Span::styled(" / ", Style::new().fg(COLOR_MUTED)));
        spans.push(Span::styled(
            format!("{} tokens", group_thousands(window)),
            Style::new().fg(COLOR_TEXT_SOFT),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(COLOR_SEPARATOR)));
        spans.push(Span::styled(
            format!("{percent:.1}%"),
            Style::new().fg(usage_color(percent)).bold(),
        ));
        if total > window {
            spans.push(Span::styled(
                " · 超出窗口",
                Style::new().fg(COLOR_DANGER_BRIGHT),
            ));
        }
    } else {
        spans.push(Span::styled(" tokens", Style::new().fg(COLOR_TEXT_SOFT)));
        spans.push(Span::styled(
            " · 窗口大小未知",
            Style::new().fg(COLOR_MUTED),
        ));
    }
    Line::from(spans)
}

/// 会话累计统计行：标签退成元信息色，数字用正文色，扫数字时不会被文字带走。
fn session_usage_line(usage: &TokenUsage) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "本次打开会话累计：",
        Style::new().fg(COLOR_MUTED),
    )];
    let items = [
        ("输出", usage.output_tokens),
        ("缓存读", usage.cache_read_tokens),
        ("写", usage.cache_write_tokens),
    ];
    for (index, (label, value)) in items.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(COLOR_SEPARATOR)));
        }
        spans.push(Span::styled(
            format!("{label} "),
            Style::new().fg(COLOR_META),
        ));
        spans.push(Span::styled(
            group_thousands(value.unwrap_or(0)),
            Style::new().fg(COLOR_TEXT_SOFT),
        ));
    }
    Line::from(spans)
}

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
    let mut lines = vec![note_line("最近模型请求 · 分类为文本估算")];
    // 占比色条与逻辑行下标一起保存：它整行都是空格，混在折行段落里会被算成两行。
    let mut bar: Option<(usize, Line<'static>)> = None;
    if let Some(snapshot) = &app.context_snapshot {
        let values: Vec<u64> = CATEGORIES
            .iter()
            .map(|(key, _, _)| snapshot.tokens.get(*key).copied().unwrap_or(0))
            .collect();
        let total = values.iter().copied().fold(0_u64, u64::saturating_add);
        let window = app.context_window.filter(|value| *value > 0);
        let percent = window.map(|window| total as f64 / window as f64 * 100.0);
        lines.push(window_line(total, window, percent));
        // 数字列宽按本张表里最长的数字取：总量、余量与每一行因此落在同一列。
        let label_max = CATEGORIES
            .iter()
            .map(|(_, label, _)| UnicodeWidthStr::width(*label))
            .max()
            .unwrap_or(0);
        let mut token_width = UnicodeWidthStr::width("tokens");
        for value in values.iter().copied().chain(std::iter::once(total)) {
            token_width = token_width.max(group_thousands(value).len());
        }
        let remaining = window.map(|window| window.saturating_sub(total));
        if let Some(remaining) = remaining {
            token_width = token_width.max(group_thousands(remaining).len());
        }
        let columns = Columns::for_width(usize::from(anchor.width), label_max, token_width);
        let widths = segment_widths(&values, window.unwrap_or(total), anchor.width);
        let mut spans: Vec<Span> = CATEGORIES
            .iter()
            .zip(&widths)
            .map(|((_, _, color), width)| Span::styled(" ".repeat(*width), Style::new().bg(*color)))
            .collect();
        // 剩余容量用面板分隔线那种暗灰：`DarkGray` 在多数主题里接近正文亮度，会把整条
        // 色条压成一块比分类本身还显眼的实心带。
        spans.push(Span::styled(
            " ".repeat(widths[8]),
            Style::new().bg(COLOR_SEPARATOR),
        ));
        bar = Some((lines.len(), Line::from(spans)));
        // 占位空行保住色条那一行的高度：段落折行与滚动几何仍按一行计算。
        lines.push(Line::from(""));
        lines.push(Line::from(""));
        lines.push(column_header(&columns));
        for ((_, label, color), value) in CATEGORIES.iter().zip(values) {
            let ratio = if total == 0 {
                0.0
            } else {
                value as f64 / total as f64 * 100.0
            };
            lines.push(category_row(label, *color, value, ratio, &columns));
        }
        lines.push(Line::from(""));
        if let Some(remaining) = remaining {
            let text = group_thousands(remaining);
            lines.push(summary_row("剩余容量", Some(&text), &columns));
        }
        lines.push(note_line(&format!(
            "{} 条消息 · {} 个工具定义",
            snapshot.messages, snapshot.tools
        )));
        if snapshot.attachments > 0 {
            lines.push(note_line(&format!(
                "另有 {} 个附件/不透明推理块，未计入估算",
                snapshot.attachments
            )));
        }
        if let Some(tokens) = app.context_tokens {
            lines.push(note_line(&format!(
                "服务商实际输入 {} tokens（含缓存）",
                group_thousands(tokens)
            )));
        }
    } else {
        lines.push(note_line("本次打开会话后尚无模型请求快照。"));
    }
    let usage = &app.session_usage;
    lines.push(Line::from(""));
    lines.push(session_usage_line(usage));
    lines.push(note_line(
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
            // 快照里只出现系统提示、工具定义与工具结果：三者各自的分段色与剩余容量的
            // 暗色轨道都必须画出来。
            for color in [
                COLOR_ACCENT,
                CATEGORIES[2].2,
                CATEGORIES[6].2,
                COLOR_SEPARATOR,
            ] {
                assert!(
                    buffer.content().iter().any(|cell| cell.bg == color),
                    "缺少色块：{color:?}"
                );
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

    /// 数字分组：五位以上不分组，量级比较要一个字符一个字符地数。
    #[test]
    fn token_counts_are_grouped_by_thousands() {
        for (value, expected) in [
            (0_u64, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (53_747, "53,747"),
            (1_048_576, "1,048,576"),
            (2_622_976, "2,622,976"),
        ] {
            assert_eq!(group_thousands(value), expected, "{value}");
        }
    }

    /// 列预算在任意面板宽度下都必须守恒；明细行一旦溢出折行，后面的行会被整体推走，
    /// 色条的视觉行号也会随之错位。
    #[test]
    fn column_budget_holds_at_every_width() {
        // 数字宽度由调用方按本次快照最长数字算出，这里只取一个必定放得下的值。
        let value = 999_u64;
        for width in MARKER_WIDTH..80 {
            for label_max in [MIN_LABEL_WIDTH, 10] {
                for token_width in [3_usize, 7, 9] {
                    let columns = Columns::for_width(width, label_max, token_width);
                    assert!(
                        columns.total() <= width,
                        "width={width} label_max={label_max} token_width={token_width} {columns:?}"
                    );
                    // 绘制与预算同源：行宽必须等于预算，多一列就会折行。
                    let line = category_row("工具结果", CATEGORIES[6].2, value, 90.3, &columns);
                    assert_eq!(
                        line.width(),
                        columns.total(),
                        "width={width} {columns:?} spans={:?}",
                        line.spans
                    );
                }
            }
        }
    }

    /// 窄面板先丢条形图、再丢占比列，但分类名与 token 数必须留下。
    #[test]
    fn narrow_panels_drop_decoration_before_numbers() {
        let wide = Columns::for_width(100, 10, 6);
        assert_eq!(wide.label, 10);
        assert!(wide.bar >= MIN_BAR_WIDTH, "{wide:?}");
        let narrow = Columns::for_width(30, 10, 6);
        assert_eq!(narrow.bar, 0, "{narrow:?}");
        assert!(narrow.label >= MIN_LABEL_WIDTH);
        assert!(narrow.tokens > 0 && narrow.ratio > 0, "{narrow:?}");
        let tight = Columns::for_width(17, 10, 6);
        assert!(tight.label >= MIN_LABEL_WIDTH, "{tight:?}");
        assert_eq!(tight.ratio, 0, "{tight:?}");
    }

    /// token 列右对齐：不同位数的数字必须结束在同一列，否则量级要靠数位数才能比出来。
    #[test]
    fn token_column_ends_on_one_column() {
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.native_command.context_open = true;
        app.context_window = Some(10_000);
        app.context_snapshot = Some(ContextSnapshot {
            tokens: [
                ("system".into(), 1_000),
                ("tools".into(), 2_000),
                ("results".into(), 3_000),
            ]
            .into(),
            attachments: 0,
            messages: 12,
            tools: 5,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                let anchor = anchor(frame);
                render_context_dialog(frame, &mut app, anchor)
            })
            .unwrap();
        // 每个单元格一个字符，因此字符下标就是终端列号；汉字占两格，第二格是空白。
        let rows: Vec<Vec<char>> = (0..30u16)
            .map(|row| {
                terminal.backend().buffer().content()[usize::from(row) * 100..]
                    .iter()
                    .take(100)
                    .map(|cell| cell.symbol())
                    .flat_map(|symbol| symbol.chars())
                    .collect()
            })
            .collect();
        let mut ends = Vec::new();
        for value in ["1,000", "2,000", "3,000"] {
            let needle: Vec<char> = value.chars().collect();
            let (row, start) = rows
                .iter()
                .enumerate()
                .find_map(|(index, row)| {
                    row.windows(needle.len())
                        .position(|window| window == needle.as_slice())
                        .map(|start| (index, start))
                })
                .unwrap_or_else(|| panic!("缺少 token 数 {value}：{rows:?}"));
            let after = rows[row].get(start + needle.len()).copied();
            // 数字后面必须是空白或行尾，否则匹配到的是别的串（例如占比列）。
            assert!(
                after.is_none_or(|ch| ch == ' '),
                "第 {row} 行的 {value} 不是独立数字列：{rows:?}"
            );
            ends.push(start + needle.len());
        }
        assert!(
            ends.windows(2).all(|pair| pair[0] == pair[1]),
            "token 列未对齐：{ends:?}"
        );
    }
}
