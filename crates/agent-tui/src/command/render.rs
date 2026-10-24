//! 原生命令面板、会话搜索面板与插件对话框的渲染。

use super::session_palette::session_matches;
use crate::app::{reasoning_level_label, App};
#[cfg(feature = "plugins")]
use crate::command::{NativePluginDialogStatus, NativePluginPathMode};
use crate::command::{
    NativeSessionEntry, NativeSessionPaletteMode, NativeSessionPaletteStatus, SlashCommandMatch,
};
use crate::theme::{
    COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_META, COLOR_MUTED, COLOR_PALETTE_COMMAND,
    COLOR_SELECTION_BG, COLOR_SELECTION_MARK, COLOR_SEPARATOR, COLOR_SUCCESS, COLOR_TEXT,
    COLOR_TEXT_SOFT,
};
#[cfg(feature = "plugins")]
use crate::theme::{COLOR_USER, COLOR_WARNING};
use crate::ui::markdown::truncate_line;
// 插件对话框专用：纯 Core 构建里 `render_native_plugin_dialog` 不存在，这些符号也就没有使用者。
#[cfg(feature = "plugins")]
use crate::ui::sheet::{palette_hint, palette_sheet_viewport, render_palette_sheet, PaletteSheet};
use crate::ui::sheet::{palette_plain_row, palette_row};
#[cfg(feature = "plugins")]
use crate::ui::wrapped_line_count;
use agent_core::model::ReasoningLevel;
use ratatui::prelude::*;
use ratatui::widgets::*;

/// 命令补全菜单中每个条目占用的行数。
///
/// 条目紧邻排布、条目之间不再插空行：候选列表只是输入过程中的临时提示，用行距分段
/// 会让十几条命令吃掉半个屏幕，把对话区挤成一条缝。条目边界由选中底色和列对齐承担，
/// 不需要空行。
const PALETTE_ROW_HEIGHT: u16 = 1;

/// 面板最后一条命令与下方输入框之间保留的空行数。
///
/// 面板只有顶部一条分隔线，底部再贴住输入框边框会让整块候选看起来被压扁；留一行
/// 空白让列表有收尾，也不会把输入框推离视线。
const PALETTE_BOTTOM_GAP: u16 = 1;

/// 返回 `count` 个命令占用的面板高度（含顶部分隔边框与底部空行）。
///
/// 行距由渲染时的空行决定，布局拿到的高度必须出自同一份行几何：两边各算一套迟早会
/// 对不上，表现为列表底部被裁掉或输入框上方多出一片空白。
pub(super) fn command_palette_height(count: usize) -> u16 {
    if count == 0 {
        return 0;
    }
    let count = u16::try_from(count).unwrap_or(u16::MAX);
    count
        .saturating_mul(PALETTE_ROW_HEIGHT)
        // 顶部分隔边框
        .saturating_add(1)
        .saturating_add(PALETTE_BOTTOM_GAP)
}

/// 计算命令名列应占的显示宽度。
///
/// 取全部原生命令而不是当前匹配集：输入 `/n` 过滤到单条时列宽保持不变，描述列不会
/// 随筛选左右跳动。插件斜杠触发器等更长的名字参与取宽，避免命令名压到描述上。
fn command_column_width(matches: &[SlashCommandMatch]) -> usize {
    let native = super::NATIVE_COMMANDS
        .iter()
        .map(|command| unicode_width::UnicodeWidthStr::width(command.name).saturating_add(1));
    let dynamic = matches.iter().map(|command| {
        unicode_width::UnicodeWidthStr::width(command.name.as_str()).saturating_add(1)
    });
    native.chain(dynamic).max().unwrap_or(0)
}

/// 在输入区上方绘制原生命令补全面板。
pub(crate) fn render_native_command_panel(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    // 会话搜索面板与命令补全争用同一块位置，它一旦打开就独占该区域。
    if app.native_command.session_palette.is_some() {
        render_native_session_palette(frame, app, area);
        return;
    }
    if app.native_command.reasoning_panel.is_some() {
        render_native_reasoning_panel(frame, app, area);
        return;
    }
    if app.native_command.model_panel.is_some() {
        render_native_model_panel(frame, app, area);
        return;
    }
    let matches = app.slash_command_matches();
    if matches.is_empty() {
        return;
    }
    let selected = app.native_command.selected.min(matches.len() - 1);
    // 面板横向铺满：与下方的输入盒同宽，选中底色因此能铺到终端最右一列。
    let row_width = usize::from(area.width);
    let name_width = command_column_width(&matches);
    // 条目逐行紧邻排布，只有末尾留一行空行把列表与输入框隔开。
    let mut lines = Vec::with_capacity(matches.len() + 1);
    for (index, command) in matches.iter().enumerate() {
        lines.push(palette_row(
            &format!("/{}", command.name),
            &command.summary,
            index == selected,
            name_width,
            row_width,
        ));
    }
    lines.push(Line::default());
    // 面板高度不足时（终端过矮）用行偏移跟随选中项，避免高亮滚出可视区。
    let visible_rows = usize::from(area.height.saturating_sub(1));
    let selected_top = usize::from(PALETTE_ROW_HEIGHT) * selected;
    let offset = (selected_top + usize::from(PALETTE_ROW_HEIGHT))
        .saturating_sub(visible_rows)
        .min(lines.len().saturating_sub(visible_rows));
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0))
            .block(
                Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::new().fg(COLOR_BORDER_FOCUS)),
            ),
        area,
    );
}

/// 返回会话搜索面板占用的高度（含顶部分隔边框与底部按键提示行）。
///
/// 面板与命令补全共用输入框上方那块位置和同一套行几何：行距、边框与底部留白全部出自
/// 这里。布局与渲染各算一套迟早会对不上，表现为候选被裁掉或输入框上方多出一片空白。
/// 没有候选时仍保留一行状态文本（正在读取、读取失败或空结果）。
pub(super) fn session_palette_height(rows: usize) -> u16 {
    let rows = u16::try_from(rows).unwrap_or(u16::MAX);
    rows.max(1)
        // 顶部分隔边框；面板标题画在它上面，不额外占行。
        .saturating_add(1)
        .saturating_add(PALETTE_BOTTOM_GAP)
}

/// 构造会话面板的一行候选：选中指示条、可选的「当前」标记、命中高亮的标题、消息数与相对时间。
///
/// 一行必须只占一个终端行——面板高度按一行一条候选计算，这里一旦换行，渲染结果就会
/// 和布局算出的高度对不上，底色也会缺一角。因此整行按显示宽度裁剪后精确补白到
/// `row_width`。
///
/// `tokens` 是本次搜索实际使用的检索词：把它们在标题里标出来，读者才能看出"这条为什么
/// 被留下"，而不是把命中当成巧合。
fn session_row(
    entry: &NativeSessionEntry,
    active: bool,
    row_width: usize,
    tokens: &[String],
    is_current: bool,
) -> Line<'static> {
    let summary = &entry.summary;
    let row = if active {
        Style::new().bg(COLOR_SELECTION_BG)
    } else {
        Style::new()
    };
    let base = if active {
        row.fg(COLOR_TEXT).bold()
    } else {
        row.fg(COLOR_MUTED)
    };
    let mut spans = vec![Span::styled(
        if active { "▌" } else { " " },
        Style::new().fg(COLOR_SELECTION_MARK).patch(row),
    )];
    // 指示条已占一列，剩余的列才是内容可用宽度。
    let mut used = 1;
    // 「当前」标记单独着色：它说明这一行是正在使用的会话，而不是搜索命中的结果。
    // 窄到放不下标记时宁可不显示，也不能把标记裁成半个字。
    let marker = "当前 ";
    let marker_width = unicode_width::UnicodeWidthStr::width(marker);
    if is_current && row_width > used + marker_width {
        spans.push(Span::styled(marker, row.fg(COLOR_SUCCESS).bold()));
        used += marker_width;
    }
    let title = summary.title.as_deref().unwrap_or(summary.id.as_str());
    let label = format!(
        "{title}  {} 条消息  {}",
        summary.message_count,
        relative_native_time(summary.updated_at_ms)
    );
    let body = fit_row(&label, row_width - used);
    used += unicode_width::UnicodeWidthStr::width(body.as_str());
    spans.extend(highlight_matches(&body, tokens, base, row));
    // 行尾补白把底色铺到整行右边界。
    spans.push(Span::styled(" ".repeat(row_width - used), row));
    Line::from(spans)
}

/// 把文本裁到给定显示宽度内。
///
/// 不能用 `truncate_line` 直接裁：它会在预算内再追加一个省略号，结果最多超出预算一列，
/// 贴在行尾就会把底色挤到下一行。这里先判断是否放得下，放不下时把省略号那一列也让出来。
fn fit_row(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if unicode_width::UnicodeWidthStr::width(text) <= budget {
        return text.to_string();
    }
    truncate_line(text, budget - 1)
}

/// 把检索词在正文里标出来，返回覆盖整段正文的片段。
///
/// 只在小写化不改变字节长度时做高亮：某些字符小写后长度会变（例如 `İ`），此时按下标
/// 切分会落到多字节字符中间。这类标题退回整段同色显示，宁可不高亮也不能显示乱码。
fn highlight_matches(text: &str, tokens: &[String], base: Style, row: Style) -> Vec<Span<'static>> {
    let lowered = text.to_lowercase();
    if tokens.is_empty() || lowered.len() != text.len() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for token in tokens {
        let mut from = 0;
        while let Some(offset) = lowered[from..].find(token.as_str()) {
            let start = from + offset;
            let end = start + token.len();
            ranges.push((start, end));
            from = end.max(start + 1);
        }
    }
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    ranges.sort_unstable();
    // 合并重叠区间，避免同一段被拆成多个相邻片段。
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let highlight = row.fg(COLOR_PALETTE_COMMAND).bold();
    let mut spans = Vec::with_capacity(merged.len() * 2 + 1);
    let mut cursor = 0;
    for (start, end) in merged {
        if cursor < start {
            spans.push(Span::styled(text[cursor..start].to_string(), base));
        }
        spans.push(Span::styled(text[start..end].to_string(), highlight));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base));
    }
    spans
}

/// 构造会话面板的状态行：与候选正文同一起点，避免状态文本贴着终端左边缘。
fn session_status_line(text: &str, color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(text.to_string(), Style::new().fg(color)),
    ])
}

/// 在输入框上方绘制原生会话搜索面板。
///
/// 输入框本身就是搜索框，面板因此不画输入行，只画候选与按键提示。候选由
/// [`session_matches`](super::session_palette::session_matches) 逐帧过滤，最多展示
/// [`MAX_VISIBLE_SESSION_ROWS`](super::session_palette::MAX_VISIBLE_SESSION_ROWS) 行；
/// 终端矮到装不下时用行偏移跟随选中项，保证高亮始终可见。
fn render_native_session_palette(frame: &mut Frame, app: &App, area: Rect) {
    let Some(palette) = app.native_command.session_palette.as_ref() else {
        return;
    };
    if area.is_empty() {
        return;
    }
    let matched = match &palette.status {
        NativeSessionPaletteStatus::Ready(entries) => {
            Some((session_matches(entries, &app.input), entries))
        }
        NativeSessionPaletteStatus::Loading | NativeSessionPaletteStatus::Error(_) => None,
    };
    // 候选行宽度让出一列：行首的选中指示条自己占一列。
    let row_width = usize::from(area.width).saturating_sub(1);
    let mut lines = Vec::new();
    let mut selected = 0;
    match (&palette.status, matched.as_ref()) {
        (NativeSessionPaletteStatus::Loading, _) => {
            lines.push(session_status_line("正在读取会话…", COLOR_MUTED));
        }
        (NativeSessionPaletteStatus::Error(error), _) => {
            lines.push(session_status_line(
                &format!("读取失败：{error}"),
                COLOR_DANGER,
            ));
        }
        (NativeSessionPaletteStatus::Ready(entries), Some((matched, _))) => {
            if entries.is_empty() {
                lines.push(session_status_line("当前项目还没有持久化会话", COLOR_MUTED));
            } else if matched.indices.is_empty() {
                lines.push(session_status_line("没有匹配的会话", COLOR_MUTED));
            } else {
                // 搜索词变化后选中项可能停在已消失的下标上，先夹住再渲染。
                selected = palette.selected.min(matched.indices.len() - 1);
                lines.extend(matched.indices.iter().enumerate().map(|(position, index)| {
                    let entry = &entries[*index];
                    // 当前正在使用的会话单独标出来：它就在候选里，选中它等于原地重载。
                    let is_current = entry.summary.id == app.session_record.id;
                    session_row(
                        entry,
                        position == selected,
                        row_width,
                        &matched.tokens,
                        is_current,
                    )
                }));
            }
        }
        (NativeSessionPaletteStatus::Ready(_), None) => {}
    }
    // 顶部边框占一行；最后一行留给按键提示，终端矮到只剩一行时不提示也要让选中项可见。
    let inner = usize::from(area.height.saturating_sub(1));
    if inner == 0 {
        return;
    }
    let hint_rows = usize::from(inner >= 2);
    let list_rows = inner - hint_rows;
    // 候选多于可见行时整体上移，让选中项贴着列表底部继续前进。
    let offset = (selected + 1).saturating_sub(list_rows);
    let mut visible: Vec<Line<'static>> = lines.into_iter().skip(offset).take(list_rows).collect();
    if hint_rows == 1 {
        let hint = match palette.mode {
            NativeSessionPaletteMode::Resume => " ↑↓ 选择 · Enter 恢复 · 输入即搜索 · Esc 关闭",
            NativeSessionPaletteMode::Browse => " ↑↓ 选择 · Enter 切换 · 输入即搜索 · Esc 关闭",
        };
        visible.push(Line::from(Span::styled(hint, Style::new().fg(COLOR_META))));
    }
    // 标题只放在边框上，不占内容行；命中数用于解释候选为什么被截断。
    let mode_label = match palette.mode {
        NativeSessionPaletteMode::Resume => "恢复项目会话",
        NativeSessionPaletteMode::Browse => "项目会话",
    };
    let title = match matched.as_ref() {
        Some((matched, _)) if matched.total > matched.indices.len() => format!(
            " {mode_label} · 显示 {} / 共 {} ",
            matched.indices.len(),
            matched.total
        ),
        Some((matched, _)) if matched.total > 0 => {
            format!(" {mode_label} · {} 个 ", matched.total)
        }
        _ => format!(" {mode_label} "),
    };
    frame.render_widget(
        Paragraph::new(visible).block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .title(Span::styled(title, Style::new().fg(COLOR_TEXT_SOFT))),
        ),
        area,
    );
}

/// 思考强度滑块的轨道前缀：标题与它右侧的两列空白。
///
/// 轨道与刻度标签共用这一列宽，两行的起点因此完全对齐。
const SLIDER_PREFIX: &str = "  思考强度  ";

/// 思考强度滑块的内容行数：轨道一行、刻度标签一行。
const REASONING_SLIDER_ROWS: u16 = 2;

/// 思考强度面板占用的总高度：顶部边框 + 轨道 + 刻度标签 + 与输入框之间的空行。
///
/// 高度必须与 [`render_native_reasoning_panel`] 实际画出的行数一致，否则滑块底部会
/// 被裁掉或输入框上方多出一片空白。
pub(super) fn reasoning_panel_height() -> u16 {
    REASONING_SLIDER_ROWS + 2
}

/// 在输入框上方绘制思考强度滑块；等级只来自 models.dev 元数据。
///
/// 用横向滑块而不是竖排列表：思考强度是一条从"关闭"到"最大"的有序轴，位置本身就
/// 携带信息——排成列表时读者还得自己数第几项，滑块的相对位置则一眼可见。
fn render_native_reasoning_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = app.native_command.reasoning_panel.as_ref() else {
        return;
    };
    if app.reasoning_levels.is_empty() || area.is_empty() {
        return;
    }
    let selected = panel.selected.min(app.reasoning_levels.len() - 1);
    frame.render_widget(
        Paragraph::new(reasoning_slider_lines(
            &app.reasoning_levels,
            selected,
            area.width,
        ))
        .block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS)),
        ),
        area,
    );
}

/// 构造滑块的两行：轨道行与刻度标签行。
///
/// 每个等级占一个等宽单元格，刻度落在单元格中心，标签在单元格内居中，因此滑块位置与
/// 标签始终逐列对齐。终端窄到放不下完整轨道时退回单行紧凑形式，但行数不变——布局高度
/// 不能随宽度变化。
fn reasoning_slider_lines(
    levels: &[ReasoningLevel],
    selected: usize,
    width: u16,
) -> Vec<Line<'static>> {
    let labels: Vec<String> = levels
        .iter()
        .map(|level| reasoning_level_label(*level).to_string())
        .collect();
    let label_width = labels
        .iter()
        .map(|label| unicode_width::UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(2)
        .max(2);
    // 单元格宽度 = 最宽标签 + 两列间距，放不下时先压间距再退回紧凑形式：
    // 最宽标签 + 一列间距是下限，再窄标签就会连成一片。
    let ideal_cell = (label_width + 2).max(4);
    let min_cell = (label_width + 1).max(3);
    let prefix_width = unicode_width::UnicodeWidthStr::width(SLIDER_PREFIX);
    let available = usize::from(width).saturating_sub(prefix_width);
    // 轨道必须铺到最后一个刻度所在的列，否则最右一档没有旋钮位、标签也会探出轨道。
    let track_width_of = |cell: usize| (labels.len().saturating_sub(1)) * cell + cell / 2 + 1;
    let Some(cell) = (min_cell..=ideal_cell)
        .rev()
        .find(|cell| track_width_of(*cell) <= available)
    else {
        return vec![
            Line::from(vec![
                Span::styled(SLIDER_PREFIX, Style::new().fg(COLOR_MUTED)),
                Span::styled("‹ ", Style::new().fg(COLOR_META)),
                Span::styled(labels[selected].clone(), Style::new().fg(COLOR_TEXT).bold()),
                Span::styled(" ›", Style::new().fg(COLOR_META)),
            ]),
            Line::default(),
        ];
    };
    let track_width = track_width_of(cell);
    let stop = |index: usize| index * cell + cell / 2;
    let knob = stop(selected);
    let column_of = |position: usize| slider_column(position, knob, cell, labels.len());
    // 轨道：旋钮、可选刻度、已走过与未走到的区段各用一档，同档连续列合并成一个片段，
    // 片段数量因此只与等级数同阶，不随终端宽度增长。
    let mut spans = vec![Span::styled(SLIDER_PREFIX, Style::new().fg(COLOR_MUTED))];
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for position in 0..track_width {
        let (glyph, style) = column_of(position);
        if run_style.is_some_and(|current| current != style) {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                run_style.unwrap_or_default(),
            ));
        }
        run_style = Some(style);
        run.push(glyph);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style.unwrap_or_default()));
    }
    // 标签行：按单元格中心对齐，当前等级加粗提亮。
    let mut label_spans = vec![Span::raw(" ".repeat(prefix_width))];
    let mut cursor = 0usize;
    for (index, label) in labels.iter().enumerate() {
        let label_width = unicode_width::UnicodeWidthStr::width(label.as_str());
        let start = index * cell + cell.saturating_sub(label_width) / 2;
        label_spans.push(Span::raw(" ".repeat(start.saturating_sub(cursor))));
        label_spans.push(Span::styled(
            label.clone(),
            if index == selected {
                Style::new().fg(COLOR_TEXT).bold()
            } else {
                Style::new().fg(COLOR_MUTED)
            },
        ));
        cursor = start + unicode_width::UnicodeWidthStr::width(label.as_str());
    }
    vec![Line::from(spans), Line::from(label_spans)]
}

/// 轨道某一列的字符与样式：旋钮 `●`、可选刻度 `○`、已走过 `━`、未走到 `─`。
///
/// 旋钮是唯一用强调色的元素；刻度退到元信息灰，两侧区段靠亮度区分"已走过/未走到"。
fn slider_column(position: usize, knob: usize, cell: usize, level_count: usize) -> (char, Style) {
    if position == knob {
        return ('●', Style::new().fg(COLOR_SELECTION_MARK).bold());
    }
    if (0..level_count).any(|index| index * cell + cell / 2 == position) {
        return ('○', Style::new().fg(COLOR_META));
    }
    if position < knob {
        ('━', Style::new().fg(COLOR_MUTED))
    } else {
        ('─', Style::new().fg(COLOR_SEPARATOR))
    }
}

/// 在输入框上方绘制命令面板风格的模型上游选择；选项来自配置文件的多上游定义。
fn render_native_model_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = app.native_command.model_panel.as_ref() else {
        return;
    };
    let selected = panel
        .selected
        .min(app.model_upstreams.len().saturating_sub(1));
    // 面板横向铺满，与下方输入盒同宽；整行宽度含左侧选中指示条。
    let row_width = usize::from(area.width);
    let lines = app
        .model_upstreams
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let current = if entry.model == app.model_name {
                "●"
            } else {
                " "
            };
            // 模型 ID 决定实际请求，方括号里的上游名才是 `/model <名称>` 的切换键。
            let label = format!("{current} {} [{}]", entry.model, entry.name);
            palette_plain_row(&label, index == selected, row_width)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS)),
        ),
        area,
    );
}

/// 插件列表一次最多占据的内容行数。
///
/// 面板高度跟着内容走，但列表再长也不该把对话区挤成一条缝：更长的列表靠方向键滚动，
/// 读者始终能看见顶部固定信息与底部按键提示。
#[cfg(feature = "plugins")]
const MAX_VISIBLE_PLUGIN_ROWS: usize = 10;

/// 插件 ID 列宽：与版本、状态、摘要等后续列拉开固定距离。
#[cfg(feature = "plugins")]
const PLUGIN_NAME_WIDTH: usize = 18;

/// 在主界面上方绘制人工插件管理页。
///
/// `anchor` 是下方输入盒的区域：面板与它同宽、贴在它上沿，和命令面板是同一块底板。
/// 固定信息留在顶部，插件列表按可用行数开窗；启用审查页独占可用高度并整体滚动。
#[cfg(feature = "plugins")]
pub(crate) fn render_native_plugin_dialog(frame: &mut Frame, app: &mut App, anchor: Rect) {
    let Some(dialog) = app.native_command.plugin_dialog.as_mut() else {
        return;
    };
    let reviewing_enable = dialog.enable_confirmation.is_some();
    // 先构造固定信息行与可开窗的插件列表：只有列表会被裁掉，授权与来源始终留在顶部。
    let mut fixed: Vec<Line<'static>> = Vec::new();
    let mut list: Vec<Line<'static>> = Vec::new();
    let mut selected_row = 0;
    let row_width = usize::from(anchor.width);
    match &dialog.status {
        NativePluginDialogStatus::Loading => fixed.push(Line::from(Span::styled(
            "正在读取插件锁…",
            Style::new().fg(COLOR_MUTED),
        ))),
        NativePluginDialogStatus::Applying => fixed.push(Line::from(Span::styled(
            "正在执行插件写操作；当前 Session 不会热重载…",
            Style::new().fg(COLOR_WARNING),
        ))),
        NativePluginDialogStatus::Error(error) => {
            fixed.push(Line::from(Span::styled(
                format!("读取或提交失败：{error}"),
                Style::new().fg(COLOR_DANGER),
            )));
            fixed.push(Line::from(Span::styled(
                "按 r 重试，按 Esc 关闭",
                Style::new().fg(COLOR_MUTED),
            )));
        }
        NativePluginDialogStatus::Ready(data) => {
            fixed.push(Line::from(Span::styled(
                "Plugins are fixed dependencies for this Session.",
                Style::new().fg(COLOR_MUTED),
            )));
            fixed.push(Line::from(Span::styled(
                if data.version_history.is_empty() {
                    "Installed Version History: 无".to_string()
                } else {
                    format!(
                        "Installed Version History: {}",
                        data.version_history
                            .iter()
                            .map(|plugin| format!("{}@{}", plugin.id, plugin.version))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
                Style::new().fg(COLOR_MUTED),
            )));
            if data.plugins.is_empty() {
                fixed.push(Line::from(Span::styled(
                    "未安装受管理插件；按 i 从本地 Bundle 安装",
                    Style::new().fg(COLOR_MUTED),
                )));
            } else {
                let selected = dialog.selected.min(data.plugins.len() - 1);
                selected_row = selected;
                let selected_plugin = &data.plugins[selected];
                let authorization_text = data
                    .authorizations
                    .get(&selected_plugin.id)
                    .map(format_plugin_authorization)
                    .unwrap_or_else(|| "授权 profile 不可用".into());
                let capability_color =
                    if data
                        .authorizations
                        .get(&selected_plugin.id)
                        .is_some_and(|profile| {
                            profile.manifest.requested.iter().any(|request| {
                                matches!(
                                    request.capability_id.as_str(),
                                    "process_exec" | "secrets" | "fs_write"
                                )
                            })
                        })
                    {
                        COLOR_WARNING
                    } else {
                        COLOR_MUTED
                    };
                fixed.push(Line::from(Span::styled(
                    format!("授权：{}", truncate_line(&authorization_text, 96)),
                    Style::new().fg(capability_color),
                )));
                fixed.push(Line::from(Span::styled(
                    format!("来源：{}", truncate_line(&selected_plugin.source, 96)),
                    Style::new().fg(COLOR_MUTED),
                )));
                // ID 走强调列，版本、状态与摘要作为说明列；选中行因此和命令面板同一种观感。
                list = data
                    .plugins
                    .iter()
                    .enumerate()
                    .map(|(index, plugin)| {
                        let state = if plugin.enabled {
                            "已启用"
                        } else {
                            "已禁用"
                        };
                        let detail = format!(
                            "{}  {state}  sha256:{}",
                            truncate_line(&plugin.version, 10),
                            truncate_line(&plugin.sha256, 12)
                        );
                        palette_row(
                            &plugin.id,
                            &detail,
                            index == selected,
                            PLUGIN_NAME_WIDTH,
                            row_width,
                        )
                    })
                    .collect();
            }
        }
    }
    // 启用确认是独立审查页：审查内容替换列表，任何一条授权字段都不能被摘要挤掉。
    if let (Some(confirmation), NativePluginDialogStatus::Ready(data)) =
        (&dialog.enable_confirmation, &dialog.status)
    {
        let owner_capability = dialog
            .pending_owner_selection
            .as_ref()
            .filter(|pending| {
                pending.plugin_id == confirmation.plugin_id
                    && pending.bundle_identity == confirmation.bundle_identity
            })
            .map(|pending| pending.capability_id.as_str());
        fixed = format_plugin_enable_review(
            &confirmation.plugin_id,
            &confirmation.bundle_identity,
            data.authorizations.get(&confirmation.plugin_id),
            owner_capability,
        )
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::new().fg(COLOR_WARNING))))
        .collect();
        list.clear();
    }
    // 输入框与一次性确认提示排在列表之后；预算先扣除它们，列表只拿剩下的行数。
    let mut extras: Vec<Line<'static>> = Vec::new();
    if !reviewing_enable {
        if let Some(mode) = dialog.path_mode {
            let label = match mode {
                NativePluginPathMode::InstallRegistry => "Registry name[@semver]",
                NativePluginPathMode::InstallGithub => "GitHub owner/repository 或 URL",
                NativePluginPathMode::Install => "本地安装 Bundle 路径",
                NativePluginPathMode::Replace => "本地更新 Bundle 路径",
                NativePluginPathMode::Rollback => "插件 ID 与历史版本",
                NativePluginPathMode::SelectCapabilityOwner => "Capability ID 与插件 ID",
                NativePluginPathMode::ClearCapabilityOwner => "待清除的 Capability ID",
            };
            extras.push(Line::from(vec![
                Span::styled(format!("{label}: "), Style::new().fg(COLOR_USER)),
                Span::styled(dialog.path_input.clone(), Style::new().fg(COLOR_TEXT)),
            ]));
        } else if let Some(pending) = &dialog.pending_owner_selection {
            extras.push(Line::from(Span::styled(
                format!(
                    "已记录将 `{}` 设为 `{}` Owner；按 e 审查来源与能力",
                    pending.plugin_id, pending.capability_id
                ),
                Style::new().fg(COLOR_WARNING),
            )));
        } else if let Some(plugin_id) = &dialog.remove_confirmation {
            extras.push(Line::from(Span::styled(
                format!("再次按 x 确认移除 `{plugin_id}`；该操作不会热重载当前 Session"),
                Style::new().fg(COLOR_WARNING),
            )));
        }
    }
    let hint = palette_hint(if reviewing_enable {
        "↑↓/PgUp/PgDn 滚动审查 · e 确认 · Esc 取消"
    } else if dialog.path_mode.is_some() {
        "输入路径 · Enter 提交 · Esc 取消"
    } else {
        "↑↓ · e 启停 · g/h/i 安装 · t/u 更新 · b 回滚 · o/w Owner · x 移除 · r 刷新 · Esc"
    });
    // 面板高度只由内容与可用空间决定：列表先按上限收窄，再由真实内容行数开窗。
    let requested_rows = if reviewing_enable {
        u16::try_from(fixed.len()).unwrap_or(u16::MAX)
    } else {
        u16::try_from(fixed.len() + list.len().min(MAX_VISIBLE_PLUGIN_ROWS) + extras.len())
            .unwrap_or(u16::MAX)
    };
    let (height, content_rows) = palette_sheet_viewport(frame, anchor, requested_rows, true);
    let mut lines = fixed;
    if !reviewing_enable && !list.is_empty() {
        let visible = usize::from(content_rows)
            .saturating_sub(lines.len() + extras.len())
            .min(list.len());
        // 选中项居中偏上：朝任一方向移动都还有上下文可看。
        let start = selected_row
            .saturating_sub(visible.saturating_sub(1) / 2)
            .min(list.len().saturating_sub(visible));
        lines.extend(list.into_iter().skip(start).take(visible));
    }
    lines.extend(extras);
    if reviewing_enable {
        // 审查页按折行后的真实行数限制滚动，长路径与 Scope 才能一屏屏读完。
        let wrapped_height = lines.iter().fold(0u16, |height, line| {
            height.saturating_add(wrapped_line_count(line, anchor.width))
        });
        dialog.review_scroll = dialog
            .review_scroll
            .min(wrapped_height.saturating_sub(content_rows));
    } else {
        dialog.review_scroll = 0;
    }
    render_palette_sheet(
        frame,
        anchor,
        height,
        PaletteSheet {
            title: "Plugins · 人工管理",
            lines,
            hint: Some(hint),
            scroll: dialog.review_scroll,
            wrap: reviewing_enable,
        },
    );
}

/// 生成启用前的完整多行审查内容；调用方负责提供滚动视口，不能截断任一授权字段。
#[cfg(feature = "plugins")]
fn format_plugin_enable_review(
    plugin_id: &str,
    identity: &agent_plugin_manager::PluginBundleIdentity,
    profile: Option<&agent_plugin_host::audit::PluginAuthorizationProfile>,
    owner_capability: Option<&str>,
) -> Vec<String> {
    let operation = owner_capability.map_or_else(
        || format!("操作：启用插件 `{}`", escape_plugin_metadata(plugin_id)),
        |capability_id| {
            format!(
                "操作：启用插件 `{}`，并设为 `{}` 的 Owner",
                escape_plugin_metadata(plugin_id),
                escape_plugin_metadata(capability_id)
            )
        },
    );
    let mut lines = vec![
        "启用授权确认".into(),
        operation,
        format!("来源：{}", escape_plugin_metadata(&identity.source)),
        format!("版本：{}", escape_plugin_metadata(&identity.version)),
        format!("SHA-256：{}", escape_plugin_metadata(&identity.sha256)),
        String::new(),
    ];
    match profile {
        Some(profile) => {
            lines.push(format!(
                "插件 ABI {} 固有权限（启用即授权，无需额外 Host capability）：",
                escape_plugin_metadata(&profile.abi.api_version)
            ));
            lines.extend(
                profile
                    .abi
                    .intrinsic_permissions
                    .iter()
                    .map(|permission| format!("  - {}", permission.review_description_zh_cn())),
            );
            lines.push(String::new());
            lines.extend(format_plugin_capability_lines(&profile.manifest));
        }
        None => lines.push("授权 profile 不可用；不能确认未知授权".into()),
    }
    lines.extend([
        String::new(),
        "请滚动检查全部内容；再次按 e 只授权以上精确 Bundle 身份。".into(),
    ]);
    lines
}

/// 把 Host 校验后的请求、Scope、服务依赖和提供能力展开为完整的逐项审查文本。
#[cfg(feature = "plugins")]
fn format_plugin_capability_lines(
    snapshot: &agent_plugin_host::audit::ManifestCapabilitySnapshot,
) -> Vec<String> {
    let mut lines = Vec::new();
    if snapshot.requested.is_empty() {
        lines.push("请求 Host 能力：无".into());
    } else {
        lines.push("请求 Host 能力：".into());
        for request in &snapshot.requested {
            let label = match request.capability_id.as_str() {
                "service_dependencies" => "服务依赖",
                "workspace_read" => "当前项目只读",
                capability_id => capability_id,
            };
            lines.push(format!("  - {}", escape_plugin_metadata(label)));
            for scope in &request.scopes {
                lines.push(format!("      {}", escape_plugin_metadata(scope)));
            }
        }
    }
    if snapshot.provided.is_empty() {
        lines.push("提供插件能力：无".into());
    } else {
        lines.push("提供插件能力：".into());
        for provided in &snapshot.provided {
            let mode = match provided.mode {
                agent_plugin_host::manifest::ProvidedCapabilityMode::Multi => "multi",
                agent_plugin_host::manifest::ProvidedCapabilityMode::Exclusive => "exclusive",
            };
            lines.push(format!(
                "  - {}@{}；mode={mode}",
                escape_plugin_metadata(&provided.capability_id),
                escape_plugin_metadata(&provided.version)
            ));
        }
    }
    lines
}

/// 转义元数据中的控制字符，防止来源或 Scope 伪造额外审查行，同时保留全部可见字符。
#[cfg(feature = "plugins")]
fn escape_plugin_metadata(value: &str) -> String {
    value.chars().flat_map(char::escape_debug).collect()
}

/// 把 Host 校验后的完整授权 profile 压缩为插件列表摘要。
#[cfg(feature = "plugins")]
fn format_plugin_authorization(
    profile: &agent_plugin_host::audit::PluginAuthorizationProfile,
) -> String {
    let intrinsic = format!(
        "ABI {} 固有权限 {} 项",
        profile.abi.api_version,
        profile.abi.intrinsic_permissions.len()
    );
    if profile.manifest.requested.is_empty() {
        return format!("{intrinsic}，无额外 Host 能力");
    }
    let requested = profile
        .manifest
        .requested
        .iter()
        .map(|request| {
            let label = match request.capability_id.as_str() {
                "service_dependencies" => "插件服务依赖",
                "workspace_read" => "当前项目只读",
                _ => request.capability_id.as_str(),
            };
            if request.scopes.is_empty() {
                label.to_string()
            } else {
                format!("{label}({})", request.scopes.join(","))
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{intrinsic} · {requested}")
}

#[cfg(all(test, feature = "plugins"))]
mod plugin_capability_summary_tests {
    use super::{format_plugin_authorization, format_plugin_enable_review};
    use agent_plugin_host::audit::{
        snapshot_plugin_authorization_profile, ManifestCapabilityRequest,
        ManifestCapabilitySnapshot, ManifestProvidedCapability, PluginAuthorizationProfile,
    };
    use agent_plugin_host::manifest::{PluginManifest, ProvidedCapabilityMode};
    use agent_plugin_manager::PluginBundleIdentity;

    /// 为渲染测试补充 Host 当前 ABI 固有授权，避免测试另行复制权限基线。
    fn authorization_profile(
        manifest_snapshot: ManifestCapabilitySnapshot,
    ) -> PluginAuthorizationProfile {
        let manifest: PluginManifest = toml::from_str(
            r#"
                [plugin]
                id = "render-test"
                name = "Render Test"
                version = "1.0.0"
                api_version = "0.10.0"
                wasm = "render-test.wasm"
            "#,
        )
        .expect("渲染测试 manifest 应可解析");
        let mut profile =
            snapshot_plugin_authorization_profile(&manifest).expect("生成渲染测试授权 profile");
        profile.manifest = manifest_snapshot;
        profile
    }

    /// 启用确认必须同时展示来源、版本和 Bundle 摘要，不能只按插件 ID 授权。
    #[test]
    fn bundle_identity_is_visible_in_enable_confirmation() {
        let identity = PluginBundleIdentity {
            source: "registry:permission".into(),
            version: "2.0.0".into(),
            sha256: "0123456789abcdef".into(),
        };
        let review = format_plugin_enable_review("permission", &identity, None, None);

        assert!(review
            .iter()
            .any(|line| line == "来源：registry:permission"));
        assert!(review.iter().any(|line| line == "版本：2.0.0"));
        assert!(review
            .iter()
            .any(|line| line == "SHA-256：0123456789abcdef"));
    }

    /// 启用确认必须向用户展示依赖 owner、版本范围和可选语义。
    #[test]
    fn service_dependencies_are_visible_in_capability_summary() {
        let profile = authorization_profile(ManifestCapabilitySnapshot {
            plugin_id: "consumer".into(),
            requested: vec![ManifestCapabilityRequest {
                capability_id: "service_dependencies".into(),
                scopes: vec!["provider@^1.0;optional=false".into()],
            }],
            provided: Vec::new(),
        });

        assert_eq!(
            format_plugin_authorization(&profile),
            "ABI 0.10.0 固有权限 9 项 · 插件服务依赖(provider@^1.0;optional=false)"
        );
    }

    /// 当前工作区读取权限必须使用面向用户的明确文案，而不是暴露内部 capability ID。
    #[test]
    fn workspace_read_uses_project_readonly_review_label() {
        let profile = authorization_profile(ManifestCapabilitySnapshot {
            plugin_id: "repository-reader".into(),
            requested: vec![ManifestCapabilityRequest {
                capability_id: "workspace_read".into(),
                scopes: Vec::new(),
            }],
            provided: Vec::new(),
        });
        let identity = PluginBundleIdentity {
            source: "registry:repository-reader".into(),
            version: "1.0.0".into(),
            sha256: "b".repeat(64),
        };

        assert!(format_plugin_authorization(&profile).contains("当前项目只读"));
        let review =
            format_plugin_enable_review("repository-reader", &identity, Some(&profile), None)
                .join("\n");
        assert!(review.contains("当前项目只读"), "{review}");
        assert!(!review.contains("workspace_read"), "{review}");
    }

    /// 长路径不能吞掉排在后面的进程能力、服务依赖或提供能力。
    #[test]
    fn enable_review_keeps_every_capability_and_scope_visible() {
        let long_path = format!("/workspace/{}/secrets", "nested/".repeat(40));
        let profile = authorization_profile(ManifestCapabilitySnapshot {
            plugin_id: "consumer".into(),
            requested: vec![
                ManifestCapabilityRequest {
                    capability_id: "fs_read".into(),
                    scopes: vec![long_path.clone()],
                },
                ManifestCapabilityRequest {
                    capability_id: "process_exec".into(),
                    scopes: Vec::new(),
                },
                ManifestCapabilityRequest {
                    capability_id: "service_dependencies".into(),
                    scopes: vec!["provider@^1.2;optional=false".into()],
                },
            ],
            provided: vec![ManifestProvidedCapability {
                capability_id: "agent.context-loader".into(),
                version: "1.4.0".into(),
                mode: ProvidedCapabilityMode::Exclusive,
            }],
        });
        let identity = PluginBundleIdentity {
            source: "registry:consumer".into(),
            version: "2.0.0".into(),
            sha256: "a".repeat(64),
        };

        let review = format_plugin_enable_review(
            "consumer",
            &identity,
            Some(&profile),
            Some("agent.context-loader"),
        )
        .join("\n");

        assert!(review.contains(&long_path), "{review}");
        assert!(review.contains("process_exec"), "{review}");
        assert!(review.contains("provider@^1.2;optional=false"), "{review}");
        assert!(
            review.contains("agent.context-loader@1.4.0；mode=exclusive"),
            "{review}"
        );
        assert!(review.contains(&"a".repeat(64)), "{review}");
        assert!(review.contains("developer prompt"), "{review}");
        assert!(review.contains("取消当前 Agent 运行"), "{review}");
        assert!(review.contains("工具执行后返回给模型的结果"), "{review}");
        assert!(review.contains("Host 已脱敏"), "{review}");
        assert!(
            review.contains("主事件列表、Footer 或插件视图导航"),
            "{review}"
        );
        assert!(review.contains("依赖 ACL"), "{review}");
    }
}

/// 生成人类可读的会话更新时间标签。
fn relative_native_time(updated_at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(updated_at_ms);
    let seconds = now_ms.saturating_sub(updated_at_ms) / 1_000;
    match seconds {
        0..=59 => "刚刚".into(),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        _ => format!("{} 天前", seconds / 86_400),
    }
}

#[cfg(test)]
mod model_panel_tests {
    use super::render_native_model_panel;
    use crate::app::App;
    use crate::command::NativeModelPanel;
    use agent_core::config::ModelConfig;
    use agent_core::model::{OpenAiProtocol, ProviderKind};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    /// 构造仅用于面板渲染的上游配置，避免测试复制完整模型网关。
    fn upstream(name: &str, model: &str) -> ModelConfig {
        ModelConfig {
            name: name.into(),
            provider: ProviderKind::OpenAi,
            model: model.into(),
            api_key: Some("test-key".into()),
            api_key_env: None,
            base_url: Some("https://api.example.test/v1".into()),
            openai_protocol: OpenAiProtocol::Responses,
            extra_headers: Default::default(),
            gateway: Default::default(),
            aws_region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }
    }

    /// 面板每行按「模型 ID [上游名]」显示；方括号里必须是 `/model <名称>` 的切换键。
    #[test]
    fn model_panel_lists_model_id_with_upstream_name() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "gpt-5.6-sol".into());
        app.model_upstreams = vec![
            upstream("default", "gpt-5.6-sol"),
            upstream("gpt-6-astra", "gpt-6-astra"),
        ];
        app.native_command.model_panel = Some(NativeModelPanel { selected: 1 });

        let mut terminal = Terminal::new(TestBackend::new(60, 6)).expect("创建测试终端");
        terminal
            .draw(|frame| render_native_model_panel(frame, &app, frame.area()))
            .expect("渲染模型面板");
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(screen.contains("gpt-5.6-sol [default]"), "{screen}");
        assert!(screen.contains("gpt-6-astra [gpt-6-astra]"), "{screen}");
    }
}

#[cfg(test)]
mod session_palette_render_tests {
    use super::*;
    use agent_session::{SessionId, SessionSummary};

    /// 构造一条固定摘要候选。
    fn entry(title: &str, id: &str) -> NativeSessionEntry {
        NativeSessionEntry::new(SessionSummary {
            id: SessionId::new(id).expect("创建测试会话标识"),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 2,
            title: Some(title.to_string()),
            message_count: 3,
        })
    }

    /// 拼接片段内容，便于断言"高亮没有吞字或重复"。
    fn flatten(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// 高亮必须覆盖整段正文：丢字会让标题与检索词对不上。
    #[test]
    fn highlight_preserves_the_whole_label() {
        let base = Style::new().fg(COLOR_MUTED);
        let row = Style::new();
        let tokens = vec!["设计".to_string(), "评审".to_string()];
        let spans = highlight_matches("设计评审记录", &tokens, base, row);
        assert_eq!(flatten(&spans), "设计评审记录");
    }

    /// 相邻命中区间合并成一个片段，避免同一段被拆成多块。
    #[test]
    fn highlight_merges_adjacent_ranges() {
        let base = Style::new().fg(COLOR_MUTED);
        let row = Style::new();
        let tokens = vec!["设计".to_string(), "评审".to_string()];
        let spans = highlight_matches("设计评审", &tokens, base, row);
        assert_eq!(spans.len(), 1, "相邻命中应合并为单个片段");
        assert_eq!(flatten(&spans), "设计评审");
    }

    /// 没有检索词时整行按普通样式显示，不产生多余片段。
    #[test]
    fn highlight_without_tokens_is_plain() {
        let base = Style::new().fg(COLOR_MUTED);
        let row = Style::new();
        let spans = highlight_matches("普通标题", &[], base, row);
        assert_eq!(spans.len(), 1);
        assert_eq!(flatten(&spans), "普通标题");
    }

    /// 候选行的显示宽度必须正好等于可用宽度：底色要铺到右边界，且不能溢出换行。
    #[test]
    fn session_row_fills_exactly_the_available_width() {
        let entry = entry("clippy 告警清零", "session-a");
        let tokens = vec!["clippy".to_string()];
        for row_width in [4usize, 10, 40, 120] {
            let line = session_row(&entry, false, row_width, &tokens, false);
            assert_eq!(
                line.width(),
                row_width,
                "row_width={row_width} 时行宽应正好铺满"
            );
        }
    }

    /// 当前会话要带「当前」标记，普通候选不能有。
    #[test]
    fn current_session_row_is_marked() {
        let entry = entry("clippy 告警清零", "session-a");
        let line = session_row(&entry, false, 60, &[], true);
        assert!(flatten(&line.spans).contains("当前"));
        let line = session_row(&entry, false, 60, &[], false);
        assert!(!flatten(&line.spans).contains("当前"));
    }
}
