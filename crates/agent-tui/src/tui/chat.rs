//! 主对话区、输入编辑器、Command 预览与状态栏渲染。

use crate::*;

/// 在插件占用后的中心区域渲染 Lucia 主界面。
pub(crate) fn render_main(frame: &mut Frame, app: &mut App, workspace: Rect) {
    // 输入区是三行高的圆角输入盒，命令预览与状态栏分列其上下。
    #[cfg(feature = "plugins")]
    let command_matches = if workspace.height >= 10 {
        app.command_matches()
    } else {
        Vec::new()
    };
    #[cfg(feature = "plugins")]
    let command_preview_items = app
        .command_completion
        .as_ref()
        .map(|completion| completion.items.len())
        .unwrap_or(command_matches.len())
        .min(6);
    #[cfg(feature = "plugins")]
    let command_preview_height = if app.command_preview_hidden || command_preview_items == 0 {
        0
    } else {
        u16::try_from(command_preview_items + 2).unwrap_or(8)
    };
    #[cfg(feature = "plugins")]
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(command_preview_height),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(workspace);
    #[cfg(not(feature = "plugins"))]
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(workspace);
    #[cfg(feature = "plugins")]
    let (chat_section, command_section, input_section, footer_section) =
        (sections[0], sections[1], sections[2], sections[3]);
    #[cfg(not(feature = "plugins"))]
    let (chat_section, input_section, footer_section) = (sections[0], sections[1], sections[2]);

    // 消息流不使用容器边框，长文交给自动换行控制。
    let chat_area = chat_section.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    // 首屏与底栏共用的工作目录缩写。
    let cwd = app.workspace.cwd.display().to_string();
    let cwd_display = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => cwd.replacen(&home, "~", 1),
        _ => cwd,
    };
    if app.messages.is_empty() && !app.running {
        // 空会话时以首屏替代消息流，首条消息或运行开始后自动消失。
        app.last_max_scroll = 0;
        render_hero(frame, chat_area, &cwd_display);
    } else {
        let mut lines: Vec<Line> = app
            .messages
            .iter()
            .enumerate()
            .flat_map(|(index, message)| message.to_lines(app.streaming_message == Some(index)))
            .collect();
        if app.running && app.streaming_message.is_none() {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            lines.push(Line::from(vec![
                Span::styled(format!("{spinner} "), Style::new().fg(COLOR_WARNING)),
                Span::styled("Working...", Style::new().fg(COLOR_MUTED)),
            ]));
        }

        // 按换行后的实际显示行数计算滚动范围，兼容中文等宽字符。
        let inner_width = chat_area.width.max(1);
        let wrapped_height: u16 = lines
            .iter()
            .map(|line| {
                let width = line.width() as u16;
                width.div_ceil(inner_width).max(1)
            })
            .sum();
        let viewport = chat_area.height;
        let max_scroll = wrapped_height.saturating_sub(viewport);
        app.last_max_scroll = max_scroll;
        // 手动滚动位置到达底部后恢复自动跟随。
        if app.scroll.is_some_and(|value| value >= max_scroll) {
            app.scroll = None;
        }
        let scroll = app.scroll.unwrap_or(max_scroll);

        let chat = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(chat, chat_area);
    }

    #[cfg(feature = "plugins")]
    if command_preview_height > 0 {
        render_command_preview(frame, app, command_section, &command_matches);
    }

    // 输入区是四边圆角边框的输入盒：上边框、编辑行与下边框恰好占满三行高度。
    let input_area = input_section;
    #[cfg(feature = "plugins")]
    let agent_waiting = app.plugins_loading;
    #[cfg(not(feature = "plugins"))]
    let agent_waiting = false;
    let input_color = if app.running || agent_waiting {
        COLOR_WARNING
    } else {
        COLOR_USER
    };
    #[cfg(feature = "plugins")]
    let main_input_focused = app.plugin_focus.is_none() && app.active_dialog_index().is_none();
    #[cfg(not(feature = "plugins"))]
    let main_input_focused = true;
    let mut input_block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.running || agent_waiting {
            COLOR_WARNING
        } else if main_input_focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_MUTED
        }))
        .padding(Padding::new(1, 1, 0, 0));
    // 运行与排队状态在上边框以标题提示，空闲时保持无标题的干净边框。
    let queued = app.queued_inputs.len();
    let state_title = if agent_waiting && queued > 0 {
        format!(" 插件加载中 · {queued} 条排队 ")
    } else if agent_waiting {
        " 插件加载中 ".to_string()
    } else if app.running {
        " 运行中 · Esc 中断 ".to_string()
    } else {
        String::new()
    };
    if !state_title.is_empty() {
        input_block = input_block.title(Span::styled(state_title, Style::new().fg(COLOR_WARNING)));
    }
    let input_inner = input_block.inner(input_area);

    if app.input.is_empty() {
        let placeholder = if agent_waiting {
            "Queue while plugins load..."
        } else if app.running {
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
        // 附件引用标签（如 [Image#1]）以高亮 clip 样式区别于普通文本。
        let mut spans = vec![Span::styled("› ", Style::new().fg(input_color).bold())];
        spans.extend(input_ref_spans(&app.input, &app.attachments));
        let input_widget = Paragraph::new(Line::from(spans)).block(input_block);
        frame.render_widget(input_widget, input_area);
        // 使用显示宽度定位光标，确保中文等全角字符不会造成偏移。
        let cursor_width = unicode_width::UnicodeWidthStr::width(&app.input[..app.cursor]) as u16;
        if main_input_focused {
            frame.set_cursor_position((input_inner.x + 2 + cursor_width, input_inner.y));
        }
    }

    // 底部信息行：品牌块、模型、工作目录与当前上下文 token 数，窄终端时隐藏目录。
    let mut footer = vec![
        Span::styled(
            " lucia ",
            Style::new().fg(COLOR_CHIP_FG).bg(COLOR_USER).bold(),
        ),
        Span::raw("  "),
        Span::styled(app.model_name.as_str(), Style::new().fg(COLOR_TEXT)),
    ];
    if workspace.width >= 64 {
        footer.extend([
            Span::styled("  ·  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(
                truncate_line(
                    &format!(
                        "session {} · r{}",
                        app.session_record.id, app.session_record.revision
                    ),
                    30,
                ),
                Style::new().fg(COLOR_MUTED),
            ),
        ]);
    }
    if workspace.width >= 96 {
        footer.extend([
            Span::styled("  ·  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(cwd_display, Style::new().fg(COLOR_MUTED)),
        ]);
    }
    if let Some(tokens) = app.context_tokens {
        footer.extend([
            Span::styled("  ·  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(format!("ctx {tokens}"), Style::new().fg(COLOR_MUTED)),
        ]);
    }
    let footer_area = footer_section;
    #[cfg(feature = "plugins")]
    let (metadata_area, plugin_area, plugin_icon, plugin_status, plugin_color) = {
        let (icon, status) = app.plugin_status_content();
        let color = app.plugin_status_color();
        let desired_width =
            unicode_width::UnicodeWidthStr::width(format!("{icon} {status}").as_str())
                .saturating_add(2);
        let plugin_width = u16::try_from(desired_width)
            .unwrap_or(u16::MAX)
            .min(footer_area.width / 2);
        let columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(plugin_width)])
            .split(footer_area);
        (columns[0], columns[1], icon, status, color)
    };
    #[cfg(not(feature = "plugins"))]
    let metadata_area = footer_area;
    frame.render_widget(
        Paragraph::new(Line::from(footer)).block(Block::new().padding(Padding::horizontal(1))),
        metadata_area,
    );
    // Reserve a right-aligned footer region for plugin details without overlapping metadata.
    // 在底栏右侧为插件详情预留独立区域，避免覆盖左侧元数据。
    #[cfg(feature = "plugins")]
    {
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

/// 空会话首屏：垂直居中展示字标、版本、工作目录与快捷键速查。
///
/// 高度不足以容纳字标时退化为纯文字信息，区域过小则完全不绘制。
fn render_hero(frame: &mut Frame, area: Rect, cwd: &str) {
    if area.width < 30 || area.height < 6 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    if area.height >= 12 {
        for row in [
            "█   █ █ ▄▀▀ █ ▄▀▄",
            "█   █ █ █   █ █▄█",
            "█▄▄ ▀▄▀ ▀▄▄ █ █ █",
        ] {
            lines.push(Line::from(Span::styled(
                row,
                Style::new().fg(COLOR_USER).bold(),
            )));
        }
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        format!("lucia v{} · {cwd}", env!("CARGO_PKG_VERSION")),
        Style::new().fg(COLOR_MUTED),
    )));
    lines.push(Line::default());
    // 键位列右对齐、说明列按显示宽度补齐，保证整行居中后两列仍纵向对齐。
    for (key, action) in [
        ("Enter", "发送消息"),
        ("Esc", "中断运行 / 退出"),
        ("PgUp/PgDn", "滚动历史"),
        ("Ctrl+Y", "复制最近回复"),
    ] {
        let pad = 15usize.saturating_sub(unicode_width::UnicodeWidthStr::width(action));
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

/// 渲染内存命令快照中的匹配项和当前选中命令说明。
#[cfg(feature = "plugins")]
pub(crate) fn render_command_preview(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    matches: &[CommandSpec],
) {
    if area.is_empty() {
        return;
    }
    if let Some(completion) = app.command_completion.as_ref() {
        let visible = completion.items.iter().take(6).collect::<Vec<_>>();
        if visible.is_empty() {
            return;
        }
        let selected = app.command_selection.min(visible.len().saturating_sub(1));
        let mut lines = visible
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let selected = index == selected;
                let marker = if selected { "› " } else { "  " };
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::new().fg(if selected { COLOR_USER } else { COLOR_MUTED }),
                    ),
                    Span::styled(
                        item.label.as_str(),
                        Style::new()
                            .fg(if selected { COLOR_TEXT } else { COLOR_MUTED })
                            .bold(),
                    ),
                    Span::styled("  ", Style::new()),
                    Span::styled(
                        item.description.as_deref().unwrap_or_default(),
                        Style::new().fg(COLOR_MUTED),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        let detail = app
            .command_snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .commands
                    .iter()
                    .find(|command| command.name == completion.context.command)
            })
            .and_then(|command| {
                command
                    .arguments
                    .get(usize::from(completion.context.argument_index))
            })
            .map(|argument| argument.description.as_str())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "/{} · {}",
                    completion.context.command, completion.context.argument
                ),
                Style::new().fg(COLOR_BORDER_FOCUS),
            ),
            Span::styled("  ", Style::new()),
            Span::styled(
                truncate_line(detail, usize::from(area.width.saturating_sub(24).max(1))),
                Style::new().fg(COLOR_MUTED),
            ),
        ]));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                    .padding(Padding::horizontal(1)),
            ),
            area,
        );
        return;
    }
    if matches.is_empty() {
        return;
    }
    let visible = matches.iter().take(6).collect::<Vec<_>>();
    let selected = app.command_selection.min(visible.len().saturating_sub(1));
    let mut lines = visible
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let selected = index == selected;
            let marker = if selected { "› " } else { "  " };
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::new().fg(if selected { COLOR_USER } else { COLOR_MUTED }),
                ),
                Span::styled(
                    command.display_usage(),
                    Style::new()
                        .fg(if selected { COLOR_TEXT } else { COLOR_MUTED })
                        .bold(),
                ),
                Span::styled("  ", Style::new()),
                Span::styled(command.summary.as_str(), Style::new().fg(COLOR_MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    let selected_command = visible[selected];
    let availability = if app.command_completion_loading {
        "  候选加载中..."
    } else if app.running && selected_command.availability == CommandAvailability::IdleOnly {
        "  Agent 运行结束后可用"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled(
            truncate_line(
                &selected_command.description,
                usize::from(area.width.saturating_sub(24).max(1)),
            ),
            Style::new().fg(COLOR_MUTED),
        ),
        Span::styled(availability, Style::new().fg(COLOR_BORDER_FOCUS)),
    ]));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}
