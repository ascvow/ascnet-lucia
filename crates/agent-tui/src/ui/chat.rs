//! 主对话区、输入编辑器、输入面板与状态栏渲染。

use crate::app::{input_ref_spans, App, ToolMessageHitRegion};
use crate::command::{reasoning_level_command_value, render_native_command_panel};
use crate::session::conversation::MsgKind;
use crate::theme::{
    ACTIVITY_SPARK, COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_META, COLOR_MUTED, COLOR_SEPARATOR,
    COLOR_SUCCESS, COLOR_TEXT, COLOR_USER, COLOR_WARNING,
};
use crate::ui::markdown::truncate_line;
#[cfg(feature = "plugins")]
use crate::ui::plugin_render::render_composer_panels;
use crate::ui::transcript::{tool_display_name, tool_target};
use agent_core::model::TokenUsage;
use ratatui::prelude::*;
use ratatui::widgets::*;

/// 主内容区左右留白列数：正文两侧各留两列，任何一行都不再贴住终端边缘。
const CHAT_PADDING_X: u16 = 2;
/// 主内容区上下留白行数。
///
/// 底部这一行就是输入框上方的分隔空行：日志最后一行与输入盒之间始终隔着一行空白，
/// 滚动到顶部时它同时充当跳转提示的落点。
const CHAT_PADDING_Y: u16 = 1;

/// 在插件占用后的中心区域渲染 Lucia 主界面。
pub(crate) fn render_main(frame: &mut Frame, app: &mut App, workspace: Rect) {
    // 输入区随逻辑行数在一至六行间增长，边框额外占两行。
    let desired_input_rows = u16::try_from(app.input.split('\n').count())
        .unwrap_or(6)
        .clamp(1, 6);
    let input_rows = desired_input_rows.min(workspace.height.saturating_sub(5).clamp(1, 6));
    let input_height = input_rows + 2;
    #[cfg(feature = "plugins")]
    let timeline_uses_snapshot = app.main_event_snapshot.is_some();
    #[cfg(feature = "plugins")]
    let (timeline_messages, timeline_streaming_message) =
        app.main_event_snapshot
            .as_ref()
            .map_or_else(
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
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(panel_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(workspace);
    let (chat_section, panel_section, input_section, footer_section) =
        (sections[0], sections[1], sections[2], sections[3]);

    // 消息流不使用容器边框，长文交给自动换行控制。
    // 留白：左右各 2 列（原为 1 列），上下各 1 行；底部的这一行同时是输入框上方的分隔空行。
    let chat_area = chat_section.inner(Margin {
        horizontal: CHAT_PADDING_X,
        vertical: CHAT_PADDING_Y,
    });
    // 每帧重置热区，空会话和窗口缩小时不能保留上一帧的跳转按钮。
    app.jump_hint_area = None;
    // 滚动条同理：内容不再溢出时先擦掉上一帧的轨道，避免右侧留白残留竖线。
    //
    // 这一块记录的是上一帧的几何尺寸，拖动缩放后可能落在新画布之外；`Clear` 直接
    // 按坐标写单元格、不做裁剪，越界就会 panic，所以先与当前画布求交集。
    if let Some(previous) = app.scrollbar_area.take() {
        let stale = frame.area().intersection(previous);
        if !stale.is_empty() {
            frame.render_widget(Clear, stale);
        }
    }
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
        render_hero(frame, chat_area, &cwd_display);
    } else {
        let inner_width = chat_area.width.max(1);
        // 布局缓存按内容指纹复用高度，只有变化的消息会重新排版。
        let mut total_height =
            app.transcript_layout
                .sync(timeline_messages, timeline_streaming_message, inner_width);

        // 运行指示与后台重载提示不属于消息，作为尾部附加行单独测量与绘制。
        let mut trailing: Vec<Line<'static>> = Vec::new();
        if app.running && timeline_streaming_message.is_none() {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            let mut spans = vec![
                Span::styled(format!("{spinner} "), Style::new().fg(COLOR_WARNING)),
                Span::styled(
                    if timeline_is_thinking {
                        "Thinking..."
                    } else {
                        "Working..."
                    },
                    Style::new().fg(COLOR_MUTED),
                ),
            ];
            // 运行耗时随 UI tick 刷新，长任务时可感知进展。
            if let Some(started) = app.run_started_at {
                spans.push(Span::styled(
                    format!(" {}", format_elapsed(started.elapsed())),
                    Style::new().fg(COLOR_MUTED),
                ));
            }
            trailing.push(Line::from(spans));
        }
        // 后台原生上下文重载以调用方提供的标签作为进行中指示。
        #[cfg(feature = "plugins")]
        if let Some(label) = app.pending_reload.as_deref() {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            trailing.push(Line::from(vec![
                Span::styled(format!("{spinner} "), Style::new().fg(COLOR_WARNING)),
                Span::styled(label.to_string(), Style::new().fg(COLOR_MUTED)),
            ]));
        }
        let trailing_height = crate::ui::transcript_layout::measure_lines(&trailing, inner_width);
        total_height = total_height.saturating_add(trailing_height);

        let viewport = chat_area.height;
        let max_scroll = total_height.saturating_sub(viewport);
        app.last_max_scroll = max_scroll;
        app.last_viewport = viewport;
        // 手动滚动位置到达底部后恢复自动跟随。
        if app.scroll.is_some_and(|value| value >= max_scroll) {
            app.scroll = None;
        }
        let scroll = app.scroll.unwrap_or(max_scroll);

        // 只渲染与视口相交的消息，窗口之外的历史不构建任何行。
        app.transcript_layout.render_visible(
            frame,
            chat_area,
            timeline_messages,
            timeline_streaming_message,
            scroll,
        );
        render_trailing_lines(
            frame,
            chat_area,
            &trailing,
            trailing_height,
            total_height,
            scroll,
        );
        // 滚动条落在消息区右侧的留白列上，与正文互不重叠。
        render_scrollbar(frame, app, chat_area, total_height, viewport, scroll);

        // 将未滚动的逻辑行范围映射到当前终端视口，供左键释放时切换详情。
        let viewport_end = scroll.saturating_add(viewport);
        let tool_message_rows = if timeline_uses_snapshot {
            Vec::new()
        } else {
            app.transcript_layout.tool_rows()
        };
        app.tool_message_hit_regions = tool_message_rows
            .into_iter()
            .filter_map(|(message_index, start, height)| {
                let end = start.saturating_add(height);
                let visible_start = start.max(scroll);
                let visible_end = end.min(viewport_end);
                (chat_area.width > 0 && visible_start < visible_end).then_some(
                    ToolMessageHitRegion {
                        message_index,
                        area: Rect {
                            x: chat_area.x,
                            y: chat_area.y + visible_start.saturating_sub(scroll),
                            width: chat_area.width,
                            height: visible_end.saturating_sub(visible_start),
                        },
                    },
                )
            })
            .collect();

        // 使用消息区底部的留白行，避免覆盖正文及宽字符的续占单元格。
        if let Some(position) = app.scroll {
            let below = max_scroll.saturating_sub(position);
            if below > 0 && chat_area.height > 0 && chat_area.bottom() < chat_section.bottom() {
                // 宽终端显式写出点击提示；窄终端退回只带行数的紧凑形式。
                // 行数与可视操作分成两种颜色：行数属元信息，`click to jump` 才是交互重点。
                let dim = Style::new().fg(COLOR_META);
                let hot = Style::new().fg(COLOR_WARNING).bold();
                let verbose = vec![
                    (format!(" ↓ {below} lines · "), dim),
                    ("click to jump ".to_string(), hot),
                ];
                let compact = vec![(format!(" ↓ {below} lines "), dim)];
                let minimal = vec![(format!("↓{below}"), hot)];
                let fits = |spans: &[(String, Style)]| {
                    let width: usize = spans
                        .iter()
                        .map(|(text, _)| unicode_width::UnicodeWidthStr::width(text.as_str()))
                        .sum();
                    u16::try_from(width).unwrap_or(u16::MAX) <= chat_area.width
                };
                let hint = [verbose, compact, minimal]
                    .into_iter()
                    .find(|spans| fits(spans));
                if let Some(hint) = hint {
                    let hint_width: u16 = hint
                        .iter()
                        .map(|(text, _)| {
                            unicode_width::UnicodeWidthStr::width(text.as_str()) as u16
                        })
                        .sum();
                    let width = hint_width;
                    let hint_area = Rect {
                        x: chat_area.x + chat_area.width - width,
                        y: chat_area.bottom(),
                        width,
                        height: 1,
                    };
                    // 清空提示区域后绘制完整文本，尺寸变化时不依赖底层内容覆盖。
                    frame.render_widget(Clear, hint_area);
                    // 只用文字色不用底色：这行是留白行，铺满色块会把呼吸感挤掉。
                    frame.render_widget(
                        Paragraph::new(Line::from(
                            hint.into_iter()
                                .map(|(text, style)| Span::styled(text, style))
                                .collect::<Vec<_>>(),
                        )),
                        hint_area,
                    );
                    app.jump_hint_area = Some(hint_area);
                }
            }
        }
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

    // 输入区是四边圆角边框的多行输入盒，内容超过六行后内部垂直滚动。
    // 与消息区共用左右留白：圆角边框与正文左边界对齐，输入框也不再贴住终端边缘。
    let input_area = input_section.inner(Margin {
        horizontal: CHAT_PADDING_X,
        vertical: 0,
    });
    let input_color = if app.running {
        COLOR_WARNING
    } else {
        COLOR_USER
    };
    #[cfg(feature = "plugins")]
    let main_input_focused = !app.native_dialog_active()
        && app.plugin_focus.is_none()
        && app.active_dialog_index().is_none();
    #[cfg(not(feature = "plugins"))]
    let main_input_focused = !app.native_dialog_active();
    let mut input_block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.exit_confirmation_pending() {
            COLOR_DANGER
        } else if app.running {
            COLOR_WARNING
        } else if main_input_focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_MUTED
        }))
        .padding(Padding::new(1, 1, 0, 0));
    // 输入盒标题按优先级展示当前最需要用户注意的状态：待确认退出 > 运行中 > 排队。
    let state_title = if app.exit_confirmation_pending() {
        " 再按一次 Ctrl+C 退出 ".to_string()
    } else if app.running {
        let queued = app.queued_inputs.len();
        if queued > 0 {
            format!(" Running · Esc to stop · {queued} queued ")
        } else {
            " Running · Esc to stop ".to_string()
        }
    } else if !app.queued_inputs.is_empty() {
        format!(" {} queued ", app.queued_inputs.len())
    } else {
        String::new()
    };
    let state_color = if app.exit_confirmation_pending() {
        COLOR_DANGER
    } else {
        COLOR_WARNING
    };
    if !state_title.is_empty() {
        input_block = input_block.title(Span::styled(state_title, Style::new().fg(state_color)));
    }
    let input_inner = input_block.inner(input_area);

    if app.input.is_empty() {
        let placeholder = if app.running {
            "Steer the current run..."
        } else {
            "Message Lucia..."
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", Style::new().fg(input_color).bold()),
                Span::styled(placeholder, Style::new().fg(COLOR_MUTED)),
            ]))
            .block(input_block),
            input_area,
        );
        if main_input_focused {
            frame.set_cursor_position((input_inner.x + 2, input_inner.y));
        }
    } else {
        // 每个逻辑行独立渲染，附件引用标签继续沿用高亮样式。
        let input_lines: Vec<Line> = app
            .input
            .split('\n')
            .enumerate()
            .map(|(index, line)| {
                let prefix = if index == 0 { "› " } else { "  " };
                let mut spans = vec![Span::styled(prefix, Style::new().fg(input_color).bold())];
                spans.extend(input_ref_spans(line, &app.attachments));
                Line::from(spans)
            })
            .collect();
        let cursor_line = u16::try_from(
            app.input[..app.cursor]
                .bytes()
                .filter(|b| *b == b'\n')
                .count(),
        )
        .unwrap_or(u16::MAX);
        let cursor_line_start = app.input[..app.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let cursor_column = u16::try_from(unicode_width::UnicodeWidthStr::width(
            &app.input[cursor_line_start..app.cursor],
        ))
        .unwrap_or(u16::MAX);
        let vertical_scroll = cursor_line.saturating_sub(input_rows.saturating_sub(1));
        let available = input_inner.width.saturating_sub(1);
        let horizontal_scroll = cursor_column.saturating_add(2).saturating_sub(available);
        let input_widget = Paragraph::new(input_lines)
            .scroll((vertical_scroll, horizontal_scroll))
            .block(input_block);
        frame.render_widget(input_widget, input_area);
        if main_input_focused {
            frame.set_cursor_position((
                input_inner.x + 2 + cursor_column - horizontal_scroll,
                input_inner.y + cursor_line - vertical_scroll,
            ));
        }
    }

    // 底部信息行：品牌标识、模型、工作目录与当前上下文 token 数，窄终端时隐藏目录。
    // 层级：品牌与模型是主动作信息，其余（思考强度、目录、分支、分隔符）退到元信息灰。
    let mut footer = vec![
        Span::styled("lucia", Style::new().fg(COLOR_USER).bold()),
        Span::raw("  "),
        Span::styled(app.model_name.as_str(), Style::new().fg(COLOR_TEXT)),
        Span::styled("  |  ", Style::new().fg(COLOR_META)),
        Span::styled(
            reasoning_level_command_value(app.reasoning_level),
            Style::new().fg(COLOR_META),
        ),
    ];
    #[cfg(feature = "plugins")]
    for (text, color) in app.plugin_footer_statuses.values() {
        footer.extend([
            Span::styled("  |  ", Style::new().fg(COLOR_META)),
            Span::styled(truncate_line(text, 28), Style::new().fg(*color)),
        ]);
    }
    if app.has_copyable_text_selection() {
        footer.extend([
            Span::styled("  |  ", Style::new().fg(COLOR_META)),
            Span::styled("Selection · Ctrl+C copy", Style::new().fg(COLOR_SUCCESS)),
        ]);
    }
    if workspace.width >= 96 {
        footer.extend([
            Span::styled("  |  ", Style::new().fg(COLOR_META)),
            Span::styled(cwd_display, Style::new().fg(COLOR_META)),
        ]);
    }
    if workspace.width >= 64 {
        if let Some(branch) = app.workspace.git_branch.as_deref() {
            footer.extend([
                Span::styled("  |  ", Style::new().fg(COLOR_META)),
                Span::styled(
                    format!("⎇ {}", truncate_line(branch, 24)),
                    Style::new().fg(COLOR_META),
                ),
            ]);
        }
    }
    // 用量仅在运行中提示，结束后通过 /context 按需查看。
    if app.running {
        if let Some(tokens) = app.context_tokens {
            let (label, color) = context_status(tokens, app.context_window);
            footer.extend([
                Span::styled("  |  ", Style::new().fg(COLOR_META)),
                Span::styled(format!("◔ {label}"), Style::new().fg(color)),
            ]);
        }
        // 缓存命中率直接反映提示缓存是否真的在生效，只在窄终端下让位给更基础的信息。
        if workspace.width >= 72 {
            if let Some((label, color)) = cache_hit_status(&app.session_usage) {
                footer.extend([
                    Span::styled("  |  ", Style::new().fg(COLOR_META)),
                    Span::styled(label, Style::new().fg(color)),
                ]);
            }
        }
    }
    let footer_area = footer_section;
    // 稳定运行期隐藏常驻插件计数，仅在加载中、启动详情或存在失败时占用右侧区域。
    #[cfg(feature = "plugins")]
    let (metadata_area, plugin_footer) = {
        let visible = app.plugins_loading
            || app.plugin_load_error.is_some()
            || app.plugin_status_ticks > 0
            || !app.plugin_failures.is_empty();
        if visible {
            let (icon, status) = app.plugin_status_content();
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
    // 底栏与正文左对齐：留白 2 列，和消息区左右留白保持一致。
    frame.render_widget(
        Paragraph::new(Line::from(footer))
            .block(Block::new().padding(Padding::horizontal(CHAT_PADDING_X))),
        metadata_area,
    );
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
            .alignment(Alignment::Right)
            .block(Block::new().padding(Padding::horizontal(1))),
            plugin_area,
        );
    }
}

/// 在消息区右侧留白列绘制滚动条。
///
/// 内容不足一屏时不绘制，短对话里多一根竖线只是噪声。轨道与滑块的字符都取自常见
/// 制表符，缩到一列也不会挤压正文；滑块长度按"视口行数 / 总行数"计算，因此长对话
/// 里滑块很短，能直观看出当前位置在整体中的比例。
///
/// 竖条贴在终端最右一列，正文与竖条之间因此始终留有一列空白：滚动条不贴在字上，
/// 内容也不会看起来被"框"住。
fn render_scrollbar(
    frame: &mut Frame,
    app: &mut App,
    chat_area: Rect,
    total_height: u16,
    viewport: u16,
    scroll: u16,
) {
    // 留白列位于正文右侧；正文宽度为零时不画，避免竖条飘在空白里。
    if chat_area.width == 0 || chat_area.height < 3 || total_height <= viewport {
        return;
    }
    let x = chat_area
        .right()
        .saturating_add(1)
        .min(frame.area().right().saturating_sub(1));
    // 画布太窄时宁可不画，也不能让竖条压到正文最后一列。
    if x <= chat_area.right() {
        return;
    }
    let column = Rect {
        x,
        y: chat_area.y,
        width: 1,
        height: chat_area.height,
    };
    let mut state = ScrollbarState::new(usize::from(total_height))
        .position(usize::from(scroll))
        .viewport_content_length(usize::from(viewport));
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(Style::new().fg(COLOR_SEPARATOR))
            .thumb_symbol("┃")
            .thumb_style(Style::new().fg(COLOR_BORDER_FOCUS)),
        column,
        &mut state,
    );
    app.scrollbar_area = Some(column);
}

/// 将运行指示等尾部附加行绘制到消息区末尾。
///
/// `trailing_height` 是这些行换行后的视觉行数，`total_height` 是消息区总行数；两者与
/// `scroll` 一起决定附加行在视口中的位置，被裁掉的部分由 `Paragraph` 自身跳过。
fn render_trailing_lines(
    frame: &mut Frame,
    area: Rect,
    lines: &[Line<'static>],
    trailing_height: u16,
    total_height: u16,
    scroll: u16,
) {
    if lines.is_empty() || trailing_height == 0 {
        return;
    }
    let top = i32::from(area.y) + i32::from(total_height - trailing_height) - i32::from(scroll);
    let bottom = top + i32::from(trailing_height);
    let clipped_top = top.max(i32::from(area.y));
    let clipped_bottom = bottom.min(i32::from(area.bottom()));
    if clipped_top >= clipped_bottom {
        return;
    }
    let skip = u16::try_from(clipped_top - top).unwrap_or(0);
    frame.render_widget(
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .scroll((skip, 0)),
        Rect {
            x: area.x,
            y: u16::try_from(clipped_top).unwrap_or(u16::MAX),
            width: area.width,
            height: u16::try_from(clipped_bottom - clipped_top).unwrap_or(0),
        },
    );
}

/// 将运行耗时格式化为 `47s` 或 `3m21s` 的紧凑显示。
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let total = elapsed.as_secs();
    if total < 60 {
        format!("{total}s")
    } else {
        format!("{}m{:02}s", total / 60, total % 60)
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
    HERO_PORTRAIT
        .chunks_exact(2)
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
/// 尺寸不足以容纳头像时退化为纯文字信息，区域过小则完全不绘制。
fn render_hero(frame: &mut Frame, area: Rect, cwd: &str) {
    if area.width < 30 || area.height < 6 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    // 头像宽 36 列、高 9 行；与版本和速查合计 18 行。
    if area.height >= 18 && area.width >= 38 {
        lines.extend(hero_portrait_lines());
    }
    lines.push(Line::from(Span::styled(
        format!("lucia v{} · {cwd}", env!("CARGO_PKG_VERSION")),
        Style::new().fg(COLOR_MUTED),
    )));
    lines.push(Line::default());
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
    for (key, action) in shortcuts {
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
