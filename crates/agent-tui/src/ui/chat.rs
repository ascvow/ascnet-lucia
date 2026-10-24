//! 主对话区、输入编辑器、输入面板与状态栏渲染。

use crate::app::{input_ref_spans, App, ToolMessageHitRegion};
use crate::command::{reasoning_level_command_value, render_native_command_panel};
use crate::session::conversation::{Msg, MsgKind, PendingAttachment, PendingRef};
use crate::theme::{
    ACTIVITY_SPARK, COLOR_ACCENT, COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_META, COLOR_MUTED,
    COLOR_SEPARATOR, COLOR_SUCCESS, COLOR_TEXT, COLOR_USER, COLOR_WARNING,
};
use crate::ui::markdown::truncate_line;
#[cfg(feature = "plugins")]
use crate::ui::plugin_render::render_composer_panels;
use crate::ui::transcript_layout::TranscriptLayout;
use crate::ui::wrap_spans;
use agent_core::model::TokenUsage;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::fmt::Write as _;
use unicode_width::UnicodeWidthStr;

/// 主内容区上下留白行数。
///
/// 左右不再留白：正文、输入盒边框与底栏都从终端第 1 列开始、铺到最后一列。底部这一行
/// 是输入框上方的固定状态行：左侧活动状态、右侧跳转提示都落在这里，位置不随日志滚动
/// 或输入框长高移动；完全空闲时它退回纯留白，仍把日志与输入盒隔开。
const CHAT_PADDING_Y: u16 = 1;

/// 滚动条宽度：占正文右侧让出的那一列，不与正文争夺空间。
///
/// 横向边距去掉后右侧没有富余列，悬停只能提亮、不能再向左扩宽，否则会盖住正文。
const SCROLLBAR_WIDTH: u16 = 1;

/// 输入框上方最多同时展示的排队消息行数。
///
/// 只显示最早几条，编辑或发送后后面的自然补位；总数由状态行与排队计数提示。
const MAX_VISIBLE_QUEUED_INPUTS: usize = 3;

/// 滚动条滑块的最小高度。
///
/// 长对话里"视口 / 总行数"会把滑块压成一行，滚几十行它在屏幕上纹丝不动，看起来就像
/// 一根钉在右下角的短线。给一个最小高度，滑块才始终是能看出位置的把手。
pub(crate) const SCROLLBAR_MIN_THUMB: u16 = 3;

/// 状态行里活动状态的最小可读宽度。
///
/// 比它更窄时只剩「 ✳ Workin…」这类碎片，既读不出状态也认不出阶段，不如整段留白，
/// 把这一行完整让给右侧的跳转提示。
const MIN_STATUS_WIDTH: usize = 12;

/// 活动状态与右侧跳转提示之间至少保留的空列。
///
/// 两者都压到最短时仍要隔开一列，否则极窄终端里会出现「Thinking ·…↓26」这种连成
/// 一片的文本，读起来像同一句话。
const STATUS_HINT_GAP: u16 = 1;

/// 输入框内容折行后的光标视觉坐标。
///
/// 行号相对输入区首行（不含提示符所在行之外的行），列号是内容区的显示列偏移，
/// 不含「› 」提示符的两列。
#[derive(Clone, Copy, Debug, Default)]
struct InputCursor {
    row: u16,
    column: u16,
}

/// 把输入框内容折成视觉行，并算出光标的视觉坐标。
///
/// 空输入返回一条空行，保证内容区至少有一行。折行宽度由调用方按渲染时的同一宽度传入，
/// 两处必须一致，否则光标会偏离字符。光标恰落在满行行尾时补一条空行承载它，
/// 避免列坐标越过内容区右边界。
fn wrap_input(
    input: &str,
    cursor: usize,
    attachments: &[PendingAttachment],
    refs: &[PendingRef],
    width: usize,
) -> (Vec<Line<'static>>, InputCursor) {
    if input.is_empty() {
        return (vec![Line::default()], InputCursor::default());
    }
    let width = width.max(1);
    // 光标所在的逻辑行序号：统计其之前出现的换行符。
    let cursor_line = input[..cursor.min(input.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let mut wrapped = Vec::new();
    let mut input_cursor = InputCursor::default();
    let mut line_start = 0usize;
    for (index, logical) in input.split('\n').enumerate() {
        let spans = input_ref_spans(logical, attachments, refs);
        let (mut rows, starts) = wrap_spans(&spans, width);
        let base = wrapped.len();
        if index == cursor_line {
            let offset = cursor.saturating_sub(line_start).min(logical.len());
            let cursor_char = logical[..offset].chars().count();
            let row = starts
                .iter()
                .rposition(|start| *start <= cursor_char)
                .unwrap_or(0);
            let column: usize = logical
                .chars()
                .skip(starts[row])
                .take(cursor_char.saturating_sub(starts[row]))
                .map(|character| unicode_width::UnicodeWidthChar::width(character).unwrap_or(0))
                .sum();
            let at_row_end = column >= width && row + 1 == rows.len();
            if at_row_end {
                rows.push(Line::default());
            }
            input_cursor = InputCursor {
                row: u16::try_from(base + row + usize::from(at_row_end)).unwrap_or(u16::MAX),
                column: if at_row_end {
                    0
                } else {
                    u16::try_from(column).unwrap_or(u16::MAX)
                },
            };
        }
        wrapped.extend(rows);
        // 换行符本身占一个字节，还原下一逻辑行的起点。
        line_start += logical.len() + 1;
    }
    (wrapped, input_cursor)
}

/// 在插件占用后的中心区域渲染 Lucia 主界面，返回输入盒的区域。
///
/// 返回值是输入盒在画布中的矩形：浮层 Dialog 以它为锚点，贴在它上沿向上生长，
/// 因此对话框与命令面板永远共用同一条底边，不需要各自重算输入盒高度。
pub(crate) fn render_main(frame: &mut Frame, app: &mut App, workspace: Rect) -> Rect {
    // 输入框内容按显示宽度折行：宽度在布局前就已知（输入盒整行铺满、去掉左右边框后
    // 框内宽度等于区域宽度），因此先折行再据此决定框高，长文本不会再被横向截断。
    // 「› 」提示符占两列，折行宽度相应扣除。
    let input_text_width = usize::from(workspace.width.saturating_sub(2)).max(1);
    let input_rows = input_pane_rows(
        &app.input,
        app.cursor,
        &app.attachments,
        &app.refs,
        input_text_width,
    )
    .min(workspace.height.saturating_sub(6).clamp(1, 6));
    let input_height = input_rows + 2;
    #[cfg(feature = "plugins")]
    let timeline_uses_snapshot = app.main_event_snapshot.is_some();
    #[cfg(feature = "plugins")]
    let (timeline_messages, timeline_streaming_message) =
        app.main_event_snapshot.as_ref().map_or_else(
            || (app.messages.as_slice(), app.streaming_message),
            |snapshot| (snapshot.messages.as_slice(), snapshot.streaming_message),
        );
    #[cfg(not(feature = "plugins"))]
    let timeline_uses_snapshot = false;
    #[cfg(not(feature = "plugins"))]
    let (timeline_messages, timeline_streaming_message) =
        (app.messages.as_slice(), app.streaming_message);
    let native_panel_height = app.native_command_panel_height();
    #[cfg(feature = "plugins")]
    let plugin_panel_height = app.composer_panel_height();
    #[cfg(not(feature = "plugins"))]
    let plugin_panel_height = 0;
    let supplemental_height = if workspace.height >= 10 {
        workspace.height.saturating_sub(input_height + 5)
    } else {
        0
    };
    let panel_height = native_panel_height
        .saturating_add(plugin_panel_height)
        .min(supplemental_height);
    let queued_height = u16::try_from(app.queued_inputs.len().min(MAX_VISIBLE_QUEUED_INPUTS))
        .unwrap_or(0)
        .min(
            workspace
                .height
                .saturating_sub(input_height.saturating_add(5)),
        );
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(panel_height),
        Constraint::Length(queued_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(workspace);
    let (chat_section, panel_section, queued_section, input_section, footer_section) = (
        sections[0],
        sections[1],
        sections[2],
        sections[3],
        sections[4],
    );
    // 每帧重建排队消息的点击热区；面板不存在时不能让上一帧的矩形继续响应点击。
    app.queued_input_areas.clear();

    // 消息流不使用容器边框，长文交给自动换行控制。
    // 横向不留边距：正文从终端第 1 列开始，只在右端让出滚动条那一列；上下各留一行，
    // 底部这一行同时是输入框上方的分隔空行。
    let padded_chat = chat_section.inner(Margin {
        horizontal: 0,
        vertical: CHAT_PADDING_Y,
    });
    // 正文宽度比可用宽度少一列，那一列归滚动条：竖条与文字各占其位，既不会盖住正文，
    // 右边也不会多出一列空白。
    let chat_area = Rect {
        width: padded_chat.width.saturating_sub(SCROLLBAR_WIDTH),
        ..padded_chat
    };
    // 输入框上方的固定状态行：几何只由布局决定，日志滚动、输入框长高都不会移动它。
    let status_area = prompt_status_row(chat_section, padded_chat);
    // 正文下方被隐藏的行数；由正文分支按滚动几何填写，状态行据此决定是否画跳转提示。
    let mut jump_below = None;
    // 每帧重置热区，空会话和窗口缩小时不能保留上一帧的跳转按钮。
    app.jump_hint_area = None;
    // 每帧缓冲区由 Ratatui 重置；旧轨道的擦除由帧差分负责。
    // 这里只失效热区：旧坐标可能已属于本帧先画好的侧栏，不能再按它执行 Clear。
    app.scrollbar_area = None;
    #[cfg(feature = "plugins")]
    {
        app.last_message_width = chat_area.width.max(1);
    }
    // 首屏与底栏共用的工作目录缩写。
    let cwd = app.workspace.cwd.display().to_string();
    let cwd_display = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => cwd.replacen(&home, "~", 1),
        _ => cwd,
    };
    if timeline_messages.is_empty() && !app.running {
        // 空会话时以首屏替代消息流，首条消息或运行开始后自动消失。
        app.last_max_scroll = 0;
        app.last_viewport = chat_area.height;
        app.tool_message_hit_regions.clear();
        // 首屏没有可滚动内容：选区行号相对消息区顶部，提取时再映射回屏幕坐标。
        app.content_area = Some(chat_area);
        app.content_scroll = 0;
        app.content_rows = 0;
        // 首屏没有滚动条，悬停态必须跟着复位，否则首次溢出时会带着上一轮的高亮出现。
        app.is_scrollbar_hovered = false;
        render_hero(frame, chat_area, &cwd_display);
    } else {
        let viewport = render_transcript_pane(
            frame,
            chat_area,
            TranscriptPane {
                messages: timeline_messages,
                streaming: timeline_streaming_message,
                layout: &mut app.transcript_layout,
                scroll: &mut app.scroll,
                hit_regions: &mut app.tool_message_hit_regions,
                last_max_scroll: &mut app.last_max_scroll,
                last_viewport: &mut app.last_viewport,
                content_area: &mut app.content_area,
                content_scroll: &mut app.content_scroll,
                content_rows: &mut app.content_rows,
                scrollbar_area: &mut app.scrollbar_area,
                is_scrollbar_hovered: &mut app.is_scrollbar_hovered,
                // 审批快照期间主界面不提供工具展开：快照消息索引与实时列表不同源。
                record_hit_regions: !timeline_uses_snapshot,
            },
        );
        jump_below = viewport.below;
    }

    // 状态行在正文分支之后单独绘制：空会话首屏、正文溢出与否都不影响这一行的位置。
    if let Some(row) = status_area {
        render_prompt_status_row(frame, app, row, jump_below);
    }

    if panel_height > 0 {
        let native_height = native_panel_height.min(panel_height);
        let panels = Layout::vertical([Constraint::Length(native_height), Constraint::Min(0)])
            .split(panel_section);
        if native_height > 0 {
            render_native_command_panel(frame, app, panels[0]);
        }
        #[cfg(feature = "plugins")]
        if panels[1].height > 0 {
            render_composer_panels(frame, app, panels[1]);
        }
    }

    render_queued_inputs(frame, queued_section, app);

    // 输入盒与原生会话视图共用同一个渲染函数：边框、提示符、折行与光标规则只维护一份。
    #[cfg(feature = "plugins")]
    let main_input_focused = !app.native_dialog_active()
        && app.plugin_focus.is_none()
        && app.active_dialog_index().is_none();
    #[cfg(not(feature = "plugins"))]
    let main_input_focused = !app.native_dialog_active();
    let input_pane = InputPane {
        input: &app.input,
        cursor: app.cursor,
        attachments: &app.attachments,
        refs: &app.refs,
        running: app.running,
        focused: main_input_focused && app.queue_focus.is_none(),
        confirmation_pending: app.exit_confirmation_pending(),
        placeholder: if app.running {
            "Steer the current run..."
        } else {
            "Message Lucia..."
        },
    };
    render_input_pane(frame, input_section, &input_pane);

    // 底部信息行：模型（含配置里的上游名）、思考强度、上下文用量、权限模式、目录与分支。
    // 层级：模型是主动作信息，其余（上游名、思考强度、目录、分支、分隔符）退到元信息灰。
    app.context_chip_area = None;
    let mut footer: Vec<Span> = vec![Span::styled(
        app.model_name.clone(),
        Style::new().fg(COLOR_TEXT),
    )];
    if let Some(name) = app.model_upstream_name.as_deref() {
        // 同一模型 ID 可以对应多条配置（不同 base_url 或协议），底栏必须能指出生效的是哪一条。
        footer.push(Span::styled(
            format!(" [{name}]"),
            Style::new().fg(COLOR_META),
        ));
    }
    push_footer_item(
        &mut footer,
        reasoning_level_command_value(app.reasoning_level).to_string(),
        COLOR_META,
    );
    // 上下文用量常驻：它是最需要随手确认的数字，芯片同时是 `/context` 的点击入口。
    // 只有存在真实用量或分类快照时才出现，空会话不摆空指标。
    let mut context_chip = None;
    if let Some(tokens) = footer_context_tokens(app) {
        let (label, tone) = context_status(tokens, app.context_window);
        // 悬停提亮是「这里可以点」的唯一提示，比常驻文案更安静。
        let color = if app.is_context_chip_hovered {
            COLOR_ACCENT
        } else {
            tone
        };
        let text = format!("◔ {label}");
        // 偏移按显示宽度累加：前置条目与分隔符都可能含宽字符，用字符数会把命中区算偏。
        let offset = footer_display_width(&footer)
            + if footer.is_empty() {
                0
            } else {
                UnicodeWidthStr::width(FOOTER_SEPARATOR)
            };
        push_footer_item(&mut footer, text.clone(), color);
        context_chip = Some((offset, UnicodeWidthStr::width(text.as_str())));
    } else {
        // 芯片不再绘制时悬停态必须跟着复位，否则下一次出现会带着高亮。
        app.is_context_chip_hovered = false;
    }
    // 缓存命中率只在运行中提示：它是排查提示缓存是否生效的过程指标，结束后交给 /context。
    if app.running && workspace.width >= 72 {
        if let Some((label, color)) = cache_hit_status(&app.session_usage) {
            push_footer_item(&mut footer, label, color);
        }
    }
    #[cfg(feature = "plugins")]
    for (text, color) in app.plugin_footer_statuses.values() {
        // 插件状态通常是「标签: 值」：标签是固定字段名，只有值会变，风险色只给值。
        let text = truncate_line(text, 28);
        let (label, value) = split_status_value(&text);
        let (label, value) = (label.to_string(), value.map(str::to_string));
        match value {
            Some(value) => {
                if !footer.is_empty() {
                    footer.push(Span::styled(FOOTER_SEPARATOR, Style::new().fg(COLOR_META)));
                }
                footer.push(Span::styled(
                    format!("{label}:"),
                    Style::new().fg(COLOR_META),
                ));
                footer.push(Span::styled(format!(" {value}"), Style::new().fg(*color)));
            }
            // 整段只有值、没有标签时保持插件给的风险色：底栏不替插件重新分级。
            None => push_footer_item(&mut footer, label, *color),
        }
    }
    if app.has_copyable_text_selection() {
        push_footer_item(
            &mut footer,
            "Selection · Ctrl+C copy".to_string(),
            COLOR_SUCCESS,
        );
    }
    if workspace.width >= 96 {
        push_footer_item(&mut footer, cwd_display, COLOR_META);
    }
    if workspace.width >= 64 {
        if let Some(branch) = app.workspace.git_branch.as_deref() {
            push_footer_item(
                &mut footer,
                format!("⎇ {}", truncate_line(branch, 24)),
                COLOR_META,
            );
        }
    }
    // 底栏与正文同列：左侧品牌名与正文、输入盒边框共线。
    let footer_area = footer_section;
    // 稳定运行期右侧完全让位：只有加载中、启动详情、失败值得占用这块位置。
    #[cfg(feature = "plugins")]
    let (metadata_area, plugin_footer) = {
        let visible = app.plugins_loading
            || app.plugin_load_error.is_some()
            || app.plugin_status_ticks > 0
            || !app.plugin_failures.is_empty();
        // 空文本的状态没有信息量，不能为它切走左侧元数据的位置。
        let status = visible
            .then(|| app.plugin_status_content())
            .filter(|(_, text)| !text.is_empty());
        if let Some((icon, status)) = status {
            let color = app.plugin_status_color();
            let desired_width =
                unicode_width::UnicodeWidthStr::width(format!("{icon} {status}").as_str())
                    .saturating_add(2);
            let plugin_width = u16::try_from(desired_width)
                .unwrap_or(u16::MAX)
                .min(footer_area.width / 2);
            let columns =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(plugin_width)])
                    .split(footer_area);
            (columns[0], Some((columns[1], icon, status, color)))
        } else {
            (footer_area, None)
        }
    };
    #[cfg(not(feature = "plugins"))]
    let metadata_area = footer_area;
    // 底栏与正文同列：品牌名从终端第 1 列开始，不再额外缩进。
    frame.render_widget(Paragraph::new(Line::from(footer)), metadata_area);
    // 上下文芯片的热区取自绘制用的同一份偏移量：显示与命中区不会各算一套。
    // 只在左半区可见时登记，被插件状态挤掉的部分不再响应点击。
    if let Some((offset, chip_width)) = context_chip {
        let left = metadata_area
            .x
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        let chip_width = u16::try_from(chip_width).unwrap_or(u16::MAX);
        let visible = metadata_area.right().saturating_sub(left).min(chip_width);
        if visible > 0 {
            app.context_chip_area = Some(Rect::new(left, footer_area.y, visible, 1));
        }
    }
    // 在底栏右侧为插件详情预留独立区域，避免覆盖左侧元数据。
    #[cfg(feature = "plugins")]
    if let Some((plugin_area, plugin_icon, plugin_status, plugin_color)) = plugin_footer {
        let status = truncate_line(
            &plugin_status,
            usize::from(plugin_area.width.saturating_sub(4).max(1)),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{plugin_icon} "), Style::new().fg(plugin_color)),
                Span::styled(status, Style::new().fg(COLOR_MUTED)),
            ]))
            .alignment(Alignment::Right),
            plugin_area,
        );
    }
    input_section
}

/// 输入盒的可编辑状态；主界面与原生 Agent 会话视图共用同一份渲染输入。
pub(crate) struct InputPane<'a> {
    /// 完整输入文本。
    pub(crate) input: &'a str,
    /// UTF-8 字节光标位置。
    pub(crate) cursor: usize,
    /// 随消息发送的附件；原生会话视图为空。
    pub(crate) attachments: &'a [PendingAttachment],
    /// 随消息发送的上下文引用；原生会话视图为空。
    pub(crate) refs: &'a [PendingRef],
    /// 当前 Agent 是否正在运行，决定边框色与占位文案的措辞。
    pub(crate) running: bool,
    /// 输入框是否持有焦点；只有聚焦时才放置终端光标。
    pub(crate) focused: bool,
    /// 是否正在等待退出确认，边框改用警示色。
    pub(crate) confirmation_pending: bool,
    /// 输入为空时展示的占位文案。
    pub(crate) placeholder: &'a str,
}

/// 返回输入框折行后的视觉行数，限制在 1..=6 行。
///
/// 布局阶段先用它决定输入盒高度；渲染阶段由 [`render_input_pane`] 按同一宽度重新折行，
/// 两个调用点共用同一个函数，高度与内容不会错位。
pub(crate) fn input_pane_rows(
    input: &str,
    cursor: usize,
    attachments: &[PendingAttachment],
    refs: &[PendingRef],
    text_width: usize,
) -> u16 {
    let (wrapped, _) = wrap_input(input, cursor, attachments, refs, text_width);
    u16::try_from(wrapped.len()).unwrap_or(6).clamp(1, 6)
}

/// 渲染一个有上下边框的多行输入盒，返回内容区矩形。
/// 在输入框上方绘制待发送消息，每行可点击取回编辑。
///
/// 行首的 `↳` 与输入框的 `›` 同宽，右侧正文与输入内容左对齐。只展示最早的
/// [`MAX_VISIBLE_QUEUED_INPUTS`] 条；发送或取回后后面的条目会自动补位。
fn render_queued_inputs(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.is_empty() {
        return;
    }
    let height = usize::from(area.height);
    let len = app.queued_inputs.len();
    // 选中项尽量保持在可视窗口内；队列比面板长时窗口跟随选择滚动。
    let first = app
        .queue_focus
        .unwrap_or(0)
        .saturating_sub(height.saturating_sub(1))
        .min(len.saturating_sub(height));
    let mut areas = Vec::with_capacity(height);
    for offset in 0..height {
        let index = first + offset;
        if index >= len {
            break;
        }
        let row = Rect::new(area.x, area.y + offset as u16, area.width, 1);
        areas.push((index, row));
        // 换行会把一条消息撑成多行，预览只保留单行；长内容由 Paragraph 按宽度裁剪。
        let preview = app.queued_inputs[index].text.replace('\n', " ");
        let selected = app.queue_focus == Some(index);
        let line = if selected {
            Line::from(vec![
                Span::styled("▸ ", Style::new().fg(COLOR_USER).bold()),
                Span::styled(format!("{}. ", index + 1), Style::new().fg(COLOR_TEXT)),
                Span::styled(preview, Style::new().fg(COLOR_TEXT)),
            ])
            .style(Style::new().add_modifier(Modifier::REVERSED))
        } else {
            Line::from(vec![
                Span::styled("↳ ", Style::new().fg(COLOR_WARNING)),
                Span::styled(format!("{}. ", index + 1), Style::new().fg(COLOR_MUTED)),
                Span::styled(preview, Style::new().fg(COLOR_MUTED)),
            ])
        };
        frame.render_widget(Paragraph::new(line), row);
    }
    app.queued_input_areas = areas;
}

pub(crate) fn render_input_pane(frame: &mut Frame, area: Rect, pane: &InputPane<'_>) -> Rect {
    let text_width = usize::from(area.width.saturating_sub(2)).max(1);
    let (wrapped_input, input_cursor) = wrap_input(
        pane.input,
        pane.cursor,
        pane.attachments,
        pane.refs,
        text_width,
    );
    // 可用高度不足时按实际框高收敛：否则光标会落到输入盒之外。
    let available_rows = area.height.saturating_sub(2).max(1);
    let input_rows = u16::try_from(wrapped_input.len())
        .unwrap_or(6)
        .clamp(1, 6)
        .min(available_rows);
    let input_color = if pane.running {
        COLOR_WARNING
    } else {
        COLOR_USER
    };
    let input_block = Block::new()
        // 只保留上下边框：左右两条竖线在窄终端里会把正文压窄，也让输入区与消息区
        // 错位；去掉后输入文本与正文同列。不用圆角，避免上下边框两端出现拐角字符。
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::new().fg(if pane.confirmation_pending {
            COLOR_DANGER
        } else if pane.running {
            COLOR_WARNING
        } else if pane.focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_MUTED
        }))
        // 没有左右边框，行内不再需要内边距：内容直接从区域左边界开始，与正文对齐。
        .padding(Padding::new(0, 0, 0, 0));
    let input_inner = input_block.inner(area);

    if pane.input.is_empty() {
        // 占位符前留一列给光标：否则终端光标块会盖住首字母，显示成「› █essage」。
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", Style::new().fg(input_color).bold()),
                Span::styled(
                    format!(" {}", pane.placeholder),
                    Style::new().fg(COLOR_MUTED),
                ),
            ]))
            .block(input_block),
            area,
        );
        if pane.focused {
            frame.set_cursor_position((input_inner.x + 2, input_inner.y));
        }
        return input_inner;
    }

    // 每行已按显示宽度折行，追加「› 」提示符后就地渲染，长文本不会再被横向截断。
    let input_lines: Vec<Line> = wrapped_input
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let prefix = if index == 0 { "› " } else { "  " };
            let mut spans = vec![Span::styled(prefix, Style::new().fg(input_color).bold())];
            spans.extend(row.spans);
            Line::from(spans)
        })
        .collect();
    // 光标跟随折行后的视觉行：内容区滚动到让光标始终可见。
    let vertical_scroll = input_cursor
        .row
        .saturating_sub(input_rows.saturating_sub(1));
    let input_widget = Paragraph::new(input_lines)
        .scroll((vertical_scroll, 0))
        .block(input_block);
    frame.render_widget(input_widget, area);
    if pane.focused {
        // 内容区左边界加「› 」两列，再叠加折行后的列偏移；越界时夹在右边界内。
        let x =
            (input_inner.x + 2 + input_cursor.column).min(input_inner.right().saturating_sub(1));
        frame.set_cursor_position((x, input_inner.y + input_cursor.row - vertical_scroll));
    }
    input_inner
}

/// 时间线渲染需要的可变状态；主界面与原生 Agent 会话视图各持一份。
pub(crate) struct TranscriptPane<'a> {
    /// 本帧要显示的消息切片。
    pub(crate) messages: &'a [Msg],
    /// 正在接收增量文本的助手消息索引。
    pub(crate) streaming: Option<usize>,
    /// 消息布局缓存。
    pub(crate) layout: &'a mut TranscriptLayout,
    /// 手动滚动偏移；`None` 表示贴底跟随。
    pub(crate) scroll: &'a mut Option<u16>,
    /// 本帧可点击的工具消息热区。
    pub(crate) hit_regions: &'a mut Vec<ToolMessageHitRegion>,
    /// 本帧最大滚动偏移。
    pub(crate) last_max_scroll: &'a mut u16,
    /// 本帧消息区高度。
    pub(crate) last_viewport: &'a mut u16,
    /// 消息区屏幕矩形，供鼠标选区与滚轮换算。
    pub(crate) content_area: &'a mut Option<Rect>,
    /// 本帧实际滚动偏移。
    pub(crate) content_scroll: &'a mut u16,
    /// 逻辑总行数。
    pub(crate) content_rows: &'a mut u16,
    /// 滚动条热区。
    pub(crate) scrollbar_area: &'a mut Option<Rect>,
    /// 滚动条是否悬停。
    pub(crate) is_scrollbar_hovered: &'a mut bool,
    /// 是否登记工具消息点击热区；只读快照不登记，避免索引指向另一份列表。
    pub(crate) record_hit_regions: bool,
}

/// 一帧时间线渲染后的可见结果。
pub(crate) struct TranscriptViewport {
    /// 正文下方还有多少行没有显示；已在底部时为 `None`。
    pub(crate) below: Option<u16>,
}

/// 绘制可见时间线、滚动条与工具热区，返回本帧的滚动几何。
///
/// 只有与视口相交的消息会被构建成行，因此对话再长，每帧成本也只与视口高度相关。
pub(crate) fn render_transcript_pane(
    frame: &mut Frame,
    chat_area: Rect,
    pane: TranscriptPane<'_>,
) -> TranscriptViewport {
    // 共享时间线同样只重建热区；不要用上一帧的坐标清除本帧已绘制的其他区域。
    *pane.scrollbar_area = None;
    pane.hit_regions.clear();
    let inner_width = chat_area.width.max(1);
    let total_height = pane.layout.sync(pane.messages, pane.streaming, inner_width);
    let viewport = chat_area.height;
    let max_scroll = total_height.saturating_sub(viewport);
    *pane.last_max_scroll = max_scroll;
    *pane.last_viewport = viewport;
    // 手动滚动位置到达底部后恢复自动跟随。
    if pane.scroll.is_some_and(|value| value >= max_scroll) {
        *pane.scroll = None;
    }
    let scroll = pane.scroll.unwrap_or(max_scroll);
    // 记录内容区几何：鼠标选区用「内容行号」定位，需要在屏幕行与内容行之间换算。
    *pane.content_area = Some(chat_area);
    *pane.content_scroll = scroll;
    *pane.content_rows = total_height;

    pane.layout
        .render_visible(frame, chat_area, pane.messages, pane.streaming, scroll);
    render_scrollbar(
        frame,
        chat_area,
        total_height,
        viewport,
        scroll,
        pane.scrollbar_area,
        pane.is_scrollbar_hovered,
    );

    if pane.record_hit_regions && !chat_area.is_empty() {
        let viewport_end = scroll.saturating_add(viewport);
        *pane.hit_regions = pane
            .layout
            .tool_rows()
            .into_iter()
            .filter_map(|(message_index, start, height)| {
                let end = start.saturating_add(height);
                let visible_start = start.max(scroll);
                let visible_end = end.min(viewport_end);
                (visible_start < visible_end).then_some(ToolMessageHitRegion {
                    message_index,
                    area: Rect {
                        x: chat_area.x,
                        y: chat_area.y + visible_start.saturating_sub(scroll),
                        width: chat_area.width,
                        height: visible_end.saturating_sub(visible_start),
                    },
                })
            })
            .collect();
    }

    // 只记录「正文下方还有多少行」，具体提示由调用方的状态行绘制。
    let below = pane.scroll.and_then(|position| {
        let hidden = max_scroll.saturating_sub(position);
        (hidden > 0 && chat_area.height > 0).then_some(hidden)
    });
    TranscriptViewport { below }
}

/// 输入框上方的固定状态行几何。
///
/// 取正文区与输入盒之间那一行留白：位置只由布局决定，日志滚动与输入框长高都不会移动它。
/// 宽度铺满整行（含让给滚动条的那一列），右对齐的提示才能与输入盒右边框齐平。
/// 正文区高度不足、没有留白行时返回 `None`，此时状态与提示都不绘制。
fn prompt_status_row(section: Rect, padded: Rect) -> Option<Rect> {
    (padded.height > 0 && padded.bottom() < section.bottom()).then_some(Rect {
        x: padded.x,
        y: padded.bottom(),
        width: section.width,
        height: 1,
    })
}

/// 在输入框上方的固定状态行里绘制「活动状态 · 右对齐的跳转提示」。
///
/// 左侧是界面上唯一的活动状态出口，右侧是回到底部提示；两者共用同一行，滚轮翻页时
/// 无论是"正在想什么"还是"还有多少行"都不会跟着正文移动，也不会互相覆盖。
///
/// 右侧提示优先按需占宽（完整 → 紧凑 → 极简依次退化），剩余宽度交给活动状态；状态不足
/// [`MIN_STATUS_WIDTH`] 时整段省略——宁可留白，也不显示只剩几个字符的碎片。整行先清空
/// 再重画：上一帧的状态和提示都可能比这一帧长，靠覆盖写无法保证不残留。
///
/// `below` 是正文下方还有多少行没有显示；`None` 表示已在底部，右侧留空。
fn render_prompt_status_row(frame: &mut Frame, app: &mut App, area: Rect, below: Option<u16>) {
    frame.render_widget(Clear, area);
    let activity = prompt_status_text(app);
    let min_status_width = if activity.is_some() {
        u16::try_from(MIN_STATUS_WIDTH).unwrap_or(u16::MAX)
    } else {
        0
    };
    // 提示除了自己占的宽度，还要给状态和两者之间的空列留出位置。
    let reserved = if min_status_width == 0 {
        0
    } else {
        min_status_width.saturating_add(STATUS_HINT_GAP)
    };
    let mut used = 0;
    if let Some(below) = below {
        if let Some((spans, width)) = jump_hint_spans(below, area.width, reserved) {
            let hint_area = Rect {
                x: area.right().saturating_sub(width),
                y: area.y,
                width,
                height: 1,
            };
            // 只用文字色不用底色：这一行是留白行，铺满色块会把呼吸感挤掉。
            frame.render_widget(
                Paragraph::new(Line::from(
                    spans
                        .into_iter()
                        .map(|(text, style)| Span::styled(text, style))
                        .collect::<Vec<_>>(),
                )),
                hint_area,
            );
            app.jump_hint_area = Some(hint_area);
            used = width;
        }
    }
    let Some((title, color)) = activity else {
        return;
    };
    // 有提示时状态要让出一列空隙，两段文字不能贴在一起。
    let gap = if used > 0 { STATUS_HINT_GAP } else { 0 };
    let available = area.width.saturating_sub(used).saturating_sub(gap);
    if available < min_status_width {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            fit_status(title, usize::from(available)),
            Style::new().fg(color),
        )),
        Rect {
            width: available,
            height: 1,
            ..area
        },
    );
}

/// 选择宽度合适的跳转提示写法，返回片段与总显示宽度。
///
/// 三种写法由详到简，只要还能给左侧活动状态留下 `reserved` 列就优先用更完整的说法；
/// 全都放不下时返回 `None`：此时保留状态，不把提示压成看不懂的碎片。
///
/// `below` 是正文下方隐藏的行数，`width` 是状态行总宽度。
fn jump_hint_spans(below: u16, width: u16, reserved: u16) -> Option<(Vec<(String, Style)>, u16)> {
    // 行数与可视操作分成两种颜色：行数属元信息，`click to jump` 才是交互重点。
    let dim = Style::new().fg(COLOR_META);
    let hot = Style::new().fg(COLOR_WARNING).bold();
    let candidates = [
        vec![
            (format!(" ↓ {below} lines · "), dim),
            ("click to jump".to_string(), hot),
        ],
        vec![(format!(" ↓ {below} lines"), dim)],
        vec![(format!("↓{below}"), hot)],
    ];
    candidates.into_iter().find_map(|spans| {
        let total: usize = spans
            .iter()
            .map(|(text, _)| unicode_width::UnicodeWidthStr::width(text.as_str()))
            .sum();
        let total = u16::try_from(total).ok()?;
        (total <= width && total.saturating_add(reserved) <= width).then_some((spans, total))
    })
}

/// 在正文右侧那一列绘制滚动条，并发布本帧的悬停/拖拽热区。
///
/// **位置**：`x` 取正文右边界——正文宽度已在 [`render_main`] 里让出一列，所以这一列
/// 正好是终端最右一列。竖条与正文各占其位：文字不会被压住，右边也不会剩空白列。
///
/// **滑块**：几何在这里自己算，不走 `ratatui::Scrollbar`。原因是那个实现把滑块长度压成
/// `视口 / 总行数`，长对话里会退化成**一格**——滚几十行它在屏幕上纹丝不动，看起来就像
/// 一根钉在右下角的短线。这里给滑块一个最小高度（[`SCROLLBAR_MIN_THUMB`]），并让它在
/// 「轨道高度减滑块高度」这段可动区间里按 `scroll / max_scroll` 线性滑动：滚到顶贴住
/// 轨道顶端，滚到底贴住轨道底端，中间位置永远跟着滚动位置走。
///
/// 位置以**半行**为单位计算，边缘用 `▀`/`▄` 补齐：终端无法按像素定位，但把一格拆成
/// 上下两半后，滑块在长对话里能以半行为步长跟随滚动，而不是攒够一整行才跳一次。
///
/// **悬停**：`is_scrollbar_hovered` 为真时轨道与滑块各提亮一档。横向边距去掉后
/// 右侧没有富余列可以向左扩宽，加粗只能靠提亮表达，否则会盖住正文最后一列。
///
/// 内容不足一屏、或窄到连一列都挤不出时不绘制。此时悬停态一并复位：滚动条都没了，
/// 继续保持高亮只会在下一次内容溢出时凭空亮起。
///
/// `chat_area` 是正文区；`total_height`、`viewport` 与 `scroll` 只用于计算滑块长度与位置。
fn render_scrollbar(
    frame: &mut Frame,
    chat_area: Rect,
    total_height: u16,
    viewport: u16,
    scroll: u16,
    scrollbar_area: &mut Option<Rect>,
    is_scrollbar_hovered: &mut bool,
) {
    // 内容不足一屏时没有滚动条；正文宽度为零时同样不画，避免竖条飘在空白里。
    if chat_area.width == 0 || chat_area.height < 3 || total_height <= viewport {
        *is_scrollbar_hovered = false;
        return;
    }
    // 贴边列：正文右边界正是让给滚动条的那一列，同时兜底不超过画布本身，越界写单元格会 panic。
    let edge = chat_area
        .right()
        .min(frame.area().right().saturating_sub(1));
    // 连一列都挤不出时宁可不画，也不能让竖条压到正文最后一列。
    if edge < chat_area.right() {
        *is_scrollbar_hovered = false;
        return;
    }
    let hovered = *is_scrollbar_hovered;
    let column = Rect {
        x: edge,
        y: chat_area.y,
        width: SCROLLBAR_WIDTH,
        height: chat_area.height,
    };
    let thumb = scrollbar_thumb(chat_area.height, total_height, viewport, scroll);
    // 轨道与滑块在悬停时各提亮一档。常态下滑块也必须明显亮于轨道：两者只差一档时
    // 一格高的滑块会整根混进轨道里，看起来就像"滚动条没有动"。
    let (track_style, thumb_style) = if hovered {
        (Style::new().fg(COLOR_MUTED), Style::new().fg(COLOR_TEXT))
    } else {
        (
            Style::new().fg(COLOR_SEPARATOR),
            Style::new().fg(COLOR_MUTED),
        )
    };
    // 贴边的那一列画完整轨道与滑块：轨道占满内容区高度，滑块按位置落在其中。
    let track: Vec<Line<'_>> = (0..chat_area.height)
        .map(|row| match thumb.half_at(row) {
            ThumbHalf::Full => Line::from(Span::styled("█", thumb_style)),
            ThumbHalf::Upper => Line::from(Span::styled("▀", thumb_style)),
            ThumbHalf::Lower => Line::from(Span::styled("▄", thumb_style)),
            ThumbHalf::None => Line::from(Span::styled("│", track_style)),
        })
        .collect();
    frame.render_widget(Paragraph::new(track), column);
    *scrollbar_area = Some(column);
}

/// 滚动条滑块在轨道内的半行几何。
///
/// 终端只能按整格出字，但 `▀`/`▄` 可以把一格拆成上下两半；把位置记成半行后，长对话里
/// 滚轮推进时滑块能以半行为步长移动，而不是攒够一整行才跳一次。拖动逻辑复用同一份
/// 几何，滑块才会始终落在指针指向的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollbarThumb {
    /// 滑块顶端相对轨道顶端的半行数。
    pub(crate) top_half: u16,
    /// 滑块覆盖的半行数，至少为 `SCROLLBAR_MIN_THUMB * 2`。
    pub(crate) height_half: u16,
}

/// 滑块在一行内的覆盖方式，决定该行用轨道、整块还是半块字符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThumbHalf {
    /// 整行都是轨道。
    None,
    /// 只覆盖上半行。
    Upper,
    /// 只覆盖下半行。
    Lower,
    /// 整行都是滑块。
    Full,
}

impl ScrollbarThumb {
    /// 返回滑块覆盖的首个屏幕行（相对轨道顶端）。
    pub(crate) fn first_row(self) -> u16 {
        self.top_half / 2
    }

    /// 返回滑块覆盖的末个屏幕行（相对轨道顶端）。
    pub(crate) fn last_row(self) -> u16 {
        self.top_half
            .saturating_add(self.height_half)
            .saturating_sub(1)
            / 2
    }

    /// 判断某一行的上半/下半是否属于滑块。
    fn half_at(self, row: u16) -> ThumbHalf {
        let cell_top = u32::from(row) * 2;
        let start = u32::from(self.top_half);
        let end = start.saturating_add(u32::from(self.height_half));
        let covered_start = start.max(cell_top);
        let covered_end = end.min(cell_top + 2);
        match covered_end.saturating_sub(covered_start) {
            0 => ThumbHalf::None,
            1 if covered_start == cell_top => ThumbHalf::Upper,
            1 => ThumbHalf::Lower,
            _ => ThumbHalf::Full,
        }
    }
}

/// 计算滑块在轨道内的半行位置与高度。
///
/// 高度取 `视口 / 总行数` 的比例，但夹在 [`SCROLLBAR_MIN_THUMB`] 与轨道高度之间——
/// 长对话里比例会把滑块压成一行，滑块就失去了"当前位置"的信息量。
///
/// 顶端在「轨道高度 - 滑块高度」这段可动区间里按 `scroll / max_scroll` 线性映射并
/// 四舍五入到最近的半行，因此滚动位置与滑块位置一一对应：`scroll == 0` 时贴顶，
/// `scroll == max_scroll` 时贴底，中间任何一点都能看出移动。
pub(crate) fn scrollbar_thumb(
    track: u16,
    total_height: u16,
    viewport: u16,
    scroll: u16,
) -> ScrollbarThumb {
    // 比例用 u32 中间量：u16 下 `track * viewport` 在长对话里会溢出。
    let track_half = u32::from(track).saturating_mul(2);
    let scaled = track_half
        .saturating_mul(u32::from(viewport))
        .checked_div(u32::from(total_height))
        .unwrap_or(0);
    let height_half = scaled
        .max(u32::from(SCROLLBAR_MIN_THUMB).saturating_mul(2))
        .min(track_half);
    let span_half = track_half.saturating_sub(height_half);
    let max_scroll = total_height.saturating_sub(viewport);
    let top_half = if span_half == 0 || max_scroll == 0 {
        // 滑块占满轨道（或内容刚好一屏）时没有可动空间，贴顶即可。
        0
    } else {
        let moved = u32::from(scroll.min(max_scroll))
            .saturating_mul(span_half)
            .saturating_add(u32::from(max_scroll) / 2)
            / u32::from(max_scroll);
        moved.min(span_half)
    };
    ScrollbarThumb {
        top_half: u16::try_from(top_half).unwrap_or(u16::MAX),
        height_half: u16::try_from(height_half).unwrap_or(u16::MAX),
    }
}

/// 输入框上方固定状态行左侧展示的唯一活动状态。
///
/// 全局活动只在这一处汇总：主内容区不再输出任何状态文本，因此长任务滚出视口也不会
/// 丢失状态，同屏也不会出现两条互相矛盾的进度说明。状态行只由布局决定位置，输入框
/// 长高或变矮都不会把它挤走。
///
/// 这里只保留**模型核心认知动作**与宿主级提示，工具执行细节刻意缺席：工具名、命令与
/// 单次调用耗时属于主内容区的日志，不该在底栏随每次调用跳动，也不该与"模型在想什么"
/// 混淆。工具运行期间统一显示 [`PromptActivity::Working`]。
///
/// 独占 `Input` 插件视图（例如权限审批卡片）会整块覆盖主输入框并自带标题，因此这里
/// 不重复判断"等待授权"——宿主只认通用放置位，不解析具体插件语义。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PromptActivity {
    /// 空输入上按过一次 Ctrl+C，等待第二次确认退出。
    ExitConfirm,
    /// 后台会话上下文重载（如 `/compact`）正在执行，携带调用方提供的标签。
    Maintenance(String),
    /// 模型处于思考阶段。
    Thinking,
    /// 运行中但既未思考也未在生成正文，例如执行工具、等待模型首包。
    Working,
    /// 空闲但仍有 Agent 就绪前排队的输入。
    Queued(usize),
}

/// 汇总当前唯一的全局活动状态；完全空闲时返回 `None`。
///
/// 优先级从高到低为：退出确认、后台维护、思考、普通运行、排队。思考优先于一切运行
/// 细节，是因为并行工具运行期间模型才是主要推进者。工具执行不再单独成为一档：它折叠
/// 进 [`PromptActivity::Working`]，状态文案里既没有工具名也没有单次调用计时。工具结束后
/// 消息状态立刻离开 `ToolRunning`，不会残留旧状态。正文已经流式出现、也没有工具在跑
/// 时不再额外显示状态，避免与正在生成的回复争夺注意力。
pub(crate) fn prompt_activity(app: &App) -> Option<PromptActivity> {
    if app.exit_confirmation_pending() {
        return Some(PromptActivity::ExitConfirm);
    }
    if let Some(label) = app.pending_reload.as_deref() {
        return Some(PromptActivity::Maintenance(label.to_string()));
    }
    if app.running {
        if app.model_is_thinking {
            return Some(PromptActivity::Thinking);
        }
        // 工具执行期间始终保留通用状态：此时正文可能已经半截流式落地，但"还在干活"
        // 这件事必须一直看得见，只是不再说明在干哪一件。
        if tool_is_running(app) || app.streaming_message.is_none() {
            return Some(PromptActivity::Working);
        }
    }
    let queued = app.queued_inputs.len();
    (queued > 0).then_some(PromptActivity::Queued(queued))
}

/// 是否还有工具消息处于运行中。
///
/// 只看消息状态，不解析工具语义：底栏是否显示通用运行状态属于渲染层判断。
fn tool_is_running(app: &App) -> bool {
    app.messages
        .iter()
        .any(|message| matches!(message.kind, MsgKind::ToolRunning))
}

/// 把状态文案裁剪到可用宽度。
///
/// 文案本身够短且不含控制字符时直接复用原来的缓冲区，省掉每帧一次重新分配；否则交给
/// `truncate_line` 按显示宽度裁剪——工具目标可能带换行或宽字符，靠它保证状态始终单行
/// 且不撑破状态行。
fn fit_status(status: String, max_width: usize) -> String {
    let needs_fit = unicode_width::UnicodeWidthStr::width(status.as_str()) > max_width
        || status.chars().any(char::is_control);
    if needs_fit {
        // `truncate_line` 命中截断时会额外输出一个省略号，因此可用内容宽度先减一列，
        // 保证结果不比 `max_width` 宽。
        truncate_line(&status, max_width.saturating_sub(1))
    } else {
        status
    }
}

/// 构造状态行左侧的活动文案与颜色；空闲时返回 `None`，这一行只留白或只显示跳转提示。
///
/// 文案统一使用英文并直接从第 1 列开始，与正文左边界共线。状态每帧都会重绘，因此所有
/// 片段直接写进同一个缓冲区，不为耗时、排队数等临时片段分配 `String`。动画帧与耗时
/// 都取自 App 现有字段：`spinner_frame` 由 UI tick 推进，`run_started_at` 是本次运行
/// 的起点，这里不额外保存计时状态。
fn prompt_status_text(app: &App) -> Option<(String, Color)> {
    let activity = prompt_activity(app)?;
    let mut status = String::with_capacity(64);
    let color = match activity {
        PromptActivity::ExitConfirm => {
            if app.queued_inputs.is_empty() {
                status.push_str("再按一次 Ctrl+C 退出");
            } else {
                let _ = write!(
                    status,
                    "再按一次 Ctrl+C 退出（{} 条排队消息将丢失）",
                    app.queued_inputs.len()
                );
            }
            COLOR_DANGER
        }
        PromptActivity::Maintenance(label) => {
            push_activity_spark(&mut status, app);
            status.push_str(&label);
            COLOR_WARNING
        }
        PromptActivity::Thinking => {
            push_activity_spark(&mut status, app);
            status.push_str("Thinking");
            push_run_suffix(&mut status, app);
            COLOR_WARNING
        }
        PromptActivity::Working => {
            push_activity_spark(&mut status, app);
            status.push_str("Working");
            push_run_suffix(&mut status, app);
            COLOR_WARNING
        }
        PromptActivity::Queued(pending) => {
            if let Some(index) = app.queue_focus {
                let _ = write!(
                    status,
                    "Queue {}/{} · ↑↓ select · Enter send · Ctrl+O edit · Esc",
                    index + 1,
                    pending
                );
            } else {
                let _ = write!(status, "{pending} queued · Ctrl+O open");
            }
            COLOR_WARNING
        }
    };
    Some((status, color))
}

/// 追加当前动画帧的星号及其后的分隔空格。
///
/// 帧号由 UI tick 推进，两帧取一格，约 160ms 换一次，形成缓慢的呼吸感而不是快速闪烁。
fn push_activity_spark(title: &mut String, app: &App) {
    title.push_str(ACTIVITY_SPARK[(app.spinner_frame / 2) % ACTIVITY_SPARK.len()]);
    title.push(' ');
}

/// 追加运行状态的「耗时 · 停止提示 · 排队数」尾巴。
///
/// 耗时按秒取整，超过一分钟折算成 `3m21s`；排队数只在运行中出现，因为新一轮要等当前
/// 运行结束才会启动。
fn push_run_suffix(title: &mut String, app: &App) {
    if let Some(started) = app.run_started_at {
        let total = started.elapsed().as_secs();
        let _ = if total < 60 {
            write!(title, " · {total}s")
        } else {
            write!(title, " · {}m{:02}s", total / 60, total % 60)
        };
    }
    title.push_str(" · Esc to stop");
    if !app.queued_inputs.is_empty() {
        let _ = write!(title, " · {} queued", app.queued_inputs.len());
    }
    if app.queue_focus.is_some() {
        title.push_str(" · ↑↓/Enter send");
    }
}

/// 将 token 数格式化为紧凑显示：千以下原样，千级以 `x.xk`、百万级以 `x.xm` 表示。
fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    }
}

/// 格式化会话累计的提示缓存命中率；没有缓存读写记录时返回 `None`。
///
/// 分母取「缓存读取 + 未缓存输入」而不是全部输入：缓存写入是建立缓存的一次性成本，把它算进
/// 分母会让刚开始的会话看起来永远命中不了。命中率低于三成标为警告色，那通常意味着前缀在
/// 每轮之间发生了变化。
pub(crate) fn cache_hit_status(usage: &TokenUsage) -> Option<(String, Color)> {
    let cache_read = usage.cache_read_tokens?;
    let uncached = usage.input_tokens.unwrap_or(0);
    let considered = cache_read.saturating_add(uncached);
    if considered == 0 {
        return None;
    }
    let percentage =
        (u128::from(cache_read) * 100 + u128::from(considered) / 2) / u128::from(considered);
    let color = if percentage < 30 {
        COLOR_WARNING
    } else {
        COLOR_MUTED
    };
    Some((
        format!("↺ {percentage}% · {}", format_token_count(cache_read)),
        color,
    ))
}

/// 格式化上下文用量；配置窗口后追加百分比，并按 80% 与 95% 阈值着色。
pub(crate) fn context_status(tokens: u64, context_window: Option<u64>) -> (String, Color) {
    let Some(window) = context_window.filter(|window| *window > 0) else {
        return (format_token_count(tokens), COLOR_MUTED);
    };
    let scaled = u128::from(tokens) * 100;
    let window = u128::from(window);
    let percentage = (scaled + window / 2) / window;
    let color = if scaled > window * 95 {
        COLOR_DANGER
    } else if scaled > window * 80 {
        COLOR_WARNING
    } else {
        COLOR_MUTED
    };
    (
        format!("{percentage}% · {}", format_token_count(tokens)),
        color,
    )
}

/// 底栏条目之间的分隔符：元信息灰的竖线，只负责分组，不参与信息层级。
const FOOTER_SEPARATOR: &str = "  |  ";

/// 向底栏追加一个条目：非首条时先补分隔符。
///
/// 底栏所有条目都走这一个入口，分隔符的位置与颜色就不会各写一遍、也不会漏。
fn push_footer_item(footer: &mut Vec<Span<'static>>, text: String, color: Color) {
    if !footer.is_empty() {
        footer.push(Span::styled(FOOTER_SEPARATOR, Style::new().fg(COLOR_META)));
    }
    footer.push(Span::styled(text, Style::new().fg(color)));
}

/// 统计底栏已有条目的显示宽度。
///
/// 用于把上下文芯片的命中区对齐到屏幕列：必须按显示宽度累加，宽字符或中文目录名不会
/// 把热区算偏。
fn footer_display_width(footer: &[Span<'_>]) -> usize {
    footer
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .fold(0, usize::saturating_add)
}

/// 把插件发布的状态文本拆成「标签」与「值」：`Permission: allow all` → (`Permission`, `allow all`)。
///
/// 标签是插件固定不变的字段名，值是会变的状态，因此只有值值得占用底栏的强调色额度。
/// 底栏只认通用的 `标签: 值` 形状、不解析任何插件语义；没有该形状时返回 `None`，
/// 由调用方整段按插件声明的风险色显示。
#[cfg(feature = "plugins")]
fn split_status_value(text: &str) -> (&str, Option<&str>) {
    match text.split_once(": ") {
        Some((label, value)) if !label.is_empty() && !value.is_empty() => (label, Some(value)),
        _ => (text, None),
    }
}

/// 返回底栏可以展示的当前会话上下文规模：优先最近一次真实请求的用量，
/// 其次分类估算的总量。
///
/// 两类数据互斥（快照到达会清掉 `context_tokens`），统一成一个入口后底栏就不会在两次
/// 请求之间闪空。
fn footer_context_tokens(app: &App) -> Option<u64> {
    if let Some(tokens) = app.context_tokens {
        return Some(tokens);
    }
    app.context_snapshot.as_ref().map(|snapshot| {
        snapshot
            .tokens
            .values()
            .copied()
            .fold(0_u64, u64::saturating_add)
    })
}

/// Lucia 大头像素点阵；每个字符代表一个像素，`.` 表示透明背景。
///
/// 点阵只保留双马尾发束、刘海、红眼、面部与领口，确保常见 TUI 尺寸下仍可辨认。
const HERO_PORTRAIT: [&str; 18] = [
    ".........kkkkk........kkkkk.........",
    "........klllllk......klllllk........",
    "........kqqqkkkkkkkkkkkkqqqk........",
    "........khhhhqqqqqqqqqqhhhhk........",
    "......khhhllllhhhhhhhhllllhhhk......",
    ".....khhhhRRhhqqqqqqqqhhRRhhhhk.....",
    "....khhhhhhhhhhhhhhhhhhhhhhhhhhk....",
    ".khhhhkhhhhhhhhhhhhhhhhhhhhhkhhhhhk.",
    "krrrkhhkkhhhhshhhhshhhhshhhkkhhkrrrk",
    ".khhhhkrRqqqsssqqqssssqqqqqRrkhhhhk.",
    ".khhhhkrRsqeEwesqsssseEweqsRrkhhhhk.",
    ".khhhhhrRssEEpEssssssEEpEssRrhhhhhk.",
    ".khhhhhqqssssssssssssssssssqqhhhhhk.",
    ".khhhhhqqqqssssssssssssssqqqqhhhhhk.",
    "..krrrrhhhhhhkdddRRdddkhhhhhhrrrrk..",
    "...krrrqqqqqkddddRRddddkqqqqqrrrk...",
    "....kRRRrrkdddddRRRRdddddkrrRRRk....",
    ".....krrrddddddrrrrrrddddddrrrk.....",
];

/// 将头像点阵字符映射为终端 RGB 颜色；透明像素返回 `None`。
fn hero_pixel_color(pixel: u8) -> Option<Color> {
    match pixel {
        b'k' => Some(Color::Rgb(58, 48, 60)),
        b'q' => Some(Color::Rgb(73, 60, 72)),
        b'h' => Some(Color::Rgb(90, 72, 87)),
        b'l' => Some(Color::Rgb(118, 91, 108)),
        b'r' => Some(Color::Rgb(158, 30, 48)),
        b'R' => Some(Color::Rgb(230, 55, 72)),
        b's' => Some(Color::Rgb(255, 210, 202)),
        b'S' => Some(Color::Rgb(220, 159, 158)),
        b'e' => Some(Color::Rgb(201, 45, 58)),
        b'E' => Some(Color::Rgb(239, 71, 82)),
        b'p' => Some(Color::Rgb(111, 25, 38)),
        b'w' => Some(Color::Rgb(255, 242, 238)),
        b'd' => Some(Color::Rgb(64, 53, 67)),
        _ => None,
    }
}

/// 使用上下半块字符把两行头像像素压缩为一个终端行。
fn hero_portrait_lines() -> Vec<Line<'static>> {
    let width = HERO_PORTRAIT[0].len();
    // 头像行数必须是偶数行，否则最后一行没有配对的下半行可压缩。
    let (pairs, remainder) = HERO_PORTRAIT.as_chunks::<2>();
    debug_assert!(remainder.is_empty(), "首屏头像必须由偶数行组成");
    pairs
        .iter()
        .map(|rows| {
            let top = rows[0].as_bytes();
            let bottom = rows[1].as_bytes();
            let spans = (0..width)
                .map(|column| {
                    let top = hero_pixel_color(top[column]);
                    let bottom = hero_pixel_color(bottom[column]);
                    match (top, bottom) {
                        (None, None) => Span::raw(" "),
                        (Some(color), None) => Span::styled("▀", Style::new().fg(color)),
                        (None, Some(color)) => Span::styled("▄", Style::new().fg(color)),
                        (Some(top), Some(bottom)) if top == bottom => {
                            Span::styled("█", Style::new().fg(top))
                        }
                        (Some(top), Some(bottom)) => {
                            Span::styled("▀", Style::new().fg(top).bg(bottom))
                        }
                    }
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

/// 空会话首屏：垂直居中展示像素头像、版本、工作目录与快捷键速查。
///
/// 尺寸不足时按「先砍速查条目、再砍头像」的顺序降级：任何尺寸下都不会画出被矩形
/// 裁掉半行的内容，也不会为了塞下头像而把版本信息挤没。区域过小则完全不绘制。
fn render_hero(frame: &mut Frame, area: Rect, cwd: &str) {
    if area.width < 30 || area.height < 6 {
        return;
    }
    // 键位列右对齐、说明列按显示宽度补齐，保证整行居中后两列仍纵向对齐。
    let shortcuts = [
        ("Enter", "Send message"),
        ("Ctrl+J", "Insert newline"),
        ("/", "Slash commands"),
        ("Esc", "Stop · twice to clear"),
        ("Ctrl+C", "Clear · twice to exit"),
        ("Up/Down", "Recall input"),
        ("PgUp/PgDn", "Scroll history"),
    ];
    /// 头像占 36 列 × 9 行。
    const PORTRAIT_ROWS: usize = 9;
    /// 展示头像时至少要留出的速查条目数：只剩一两行说明的头像首屏比纯文字更零碎。
    const MIN_SHORTCUTS_WITH_PORTRAIT: usize = 4;
    // 版本行与它下方的空行各占一行。
    const FIXED_ROWS: usize = 2;
    let available = usize::from(area.height);
    let fits_with_portrait = available >= PORTRAIT_ROWS + FIXED_ROWS + MIN_SHORTCUTS_WITH_PORTRAIT;

    let mut lines: Vec<Line> = Vec::new();
    if fits_with_portrait && area.width >= 38 {
        lines.extend(hero_portrait_lines());
    }
    lines.push(Line::from(Span::styled(
        format!("lucia v{} · {cwd}", env!("CARGO_PKG_VERSION")),
        Style::new().fg(COLOR_MUTED),
    )));
    lines.push(Line::default());
    // 高度紧张时从末尾裁掉速查条目，剩余内容仍然完整可见。
    let budget = available.saturating_sub(lines.len());
    for (key, action) in shortcuts.into_iter().take(budget) {
        let pad = 18usize.saturating_sub(unicode_width::UnicodeWidthStr::width(action));
        lines.push(Line::from(vec![
            Span::styled(format!("{key:>9}"), Style::new().fg(COLOR_TEXT)),
            Span::styled(
                format!("  {action}{}", " ".repeat(pad)),
                Style::new().fg(COLOR_MUTED),
            ),
        ]));
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let hero_area = Rect {
        x: area.x,
        y: area.y + (area.height - height) / 2,
        width: area.width,
        height,
    };
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        hero_area,
    );
}

#[cfg(test)]
mod hero_portrait_tests {
    use super::*;

    /// 像素头像的每一行必须等宽，避免半块压缩时发生列索引越界。
    #[test]
    fn portrait_rows_have_equal_width() {
        let width = HERO_PORTRAIT[0].len();
        for (index, row) in HERO_PORTRAIT.iter().enumerate() {
            assert_eq!(row.len(), width, "头像第 {index} 行宽度不一致");
        }
    }

    /// 除透明标记外的每个像素字符都必须拥有颜色，避免静默显示为背景空洞。
    #[test]
    fn portrait_palette_covers_every_opaque_pixel() {
        for (row_index, row) in HERO_PORTRAIT.iter().enumerate() {
            for (column_index, pixel) in row.bytes().enumerate() {
                if pixel != b'.' {
                    assert!(
                        hero_pixel_color(pixel).is_some(),
                        "头像第 {row_index} 行第 {column_index} 列缺少颜色"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod scrollbar_geometry_tests {
    use super::*;

    /// 一次滚轮（3 行）在长对话里只推进半行：位置不再攒够一整行才跳。
    #[test]
    fn thumb_moves_in_half_rows() {
        let (track, viewport, total) = (34, 34, 400);
        let thumb = scrollbar_thumb(track, total, viewport, 0);
        assert_eq!(thumb.top_half, 0);
        assert_eq!(thumb.height_half, 6);

        let next = scrollbar_thumb(track, total, viewport, 3);
        assert_eq!(next.top_half, 1, "一次滚轮（3 行）应推进半行");
        assert_eq!(next.half_at(0), ThumbHalf::Lower, "滑块顶端应落在下半格");
    }

    /// 半行精度让同一段滚动产生超过轨道行数的位置数；整行映射只有轨道行数量级。
    #[test]
    fn half_rows_double_the_usable_positions() {
        let (track, viewport, total) = (34, 34, 400);
        let max_scroll = total - viewport;
        let mut positions = Vec::new();
        for scroll in 0..=max_scroll {
            let top = scrollbar_thumb(track, total, viewport, scroll).top_half;
            if positions.last() != Some(&top) {
                positions.push(top);
            }
        }
        assert!(
            positions.len() > usize::from(track),
            "半行精度应提供超过整行数的位置：{}",
            positions.len()
        );
        assert_eq!(*positions.first().expect("应有顶部位置"), 0);
        assert_eq!(*positions.last().expect("应有底部位置"), track * 2 - 6);
    }

    /// 滑块始终落在轨道内：顶端加高度不超过轨道半行数，且滚到顶/底精确贴边。
    #[test]
    fn thumb_stays_inside_track() {
        let (track, viewport, total) = (34, 34, 400);
        let max_scroll = total - viewport;
        for scroll in [0, 1, max_scroll / 2, max_scroll - 1, max_scroll] {
            let thumb = scrollbar_thumb(track, total, viewport, scroll);
            assert!(thumb.top_half + thumb.height_half <= track * 2);
            assert!(thumb.last_row() < track);
        }
        assert_eq!(
            scrollbar_thumb(track, total, viewport, max_scroll).top_half,
            track * 2 - 6,
            "滚到底部时滑块应贴住轨道底端"
        );
    }
}
