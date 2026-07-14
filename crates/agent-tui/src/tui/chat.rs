//! 主对话区、输入编辑器、Command 预览与状态栏渲染。

use crate::*;

/// 在插件占用后的中心区域渲染 Lucia 主界面。
pub(crate) fn render_main(frame: &mut Frame, app: &mut App, workspace: Rect) {
    // 输入区随逻辑行数在一至六行间增长，边框额外占两行。
    let desired_input_rows = u16::try_from(app.input.split('\n').count())
        .unwrap_or(6)
        .clamp(1, 6);
    let input_rows = desired_input_rows.min(workspace.height.saturating_sub(5).clamp(1, 6));
    let input_height = input_rows + 2;
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
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(workspace);
    #[cfg(not(feature = "plugins"))]
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(input_height),
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
        app.last_viewport = chat_area.height;
        render_hero(frame, chat_area, &cwd_display);
    } else {
        let inner_width = chat_area.width.max(1);
        let mut lines: Vec<Line> = app
            .messages
            .iter()
            .enumerate()
            .flat_map(|(index, message)| {
                message.to_lines(app.streaming_message == Some(index), inner_width)
            })
            .collect();
        if app.running && app.streaming_message.is_none() {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            let mut spans = vec![
                Span::styled(format!("{spinner} "), Style::new().fg(COLOR_WARNING)),
                Span::styled("Working...", Style::new().fg(COLOR_MUTED)),
            ];
            // 运行耗时随 UI tick 刷新，长任务时可感知进展。
            if let Some(started) = app.run_started_at {
                spans.push(Span::styled(
                    format!(" {}", format_elapsed(started.elapsed())),
                    Style::new().fg(COLOR_MUTED),
                ));
            }
            lines.push(Line::from(spans));
        }

        // 按 Paragraph 的词边界规则计算高度，保证滚动范围与实际渲染一致。
        let wrapped_height = lines.iter().fold(0u16, |height, line| {
            height.saturating_add(wrapped_line_count(line, inner_width))
        });
        let chat = Paragraph::new(lines).wrap(Wrap { trim: false });
        let viewport = chat_area.height;
        let max_scroll = wrapped_height.saturating_sub(viewport);
        app.last_max_scroll = max_scroll;
        app.last_viewport = viewport;
        // 手动滚动位置到达底部后恢复自动跟随。
        if app.scroll.is_some_and(|value| value >= max_scroll) {
            app.scroll = None;
        }
        let scroll = app.scroll.unwrap_or(max_scroll);

        let chat = chat.scroll((scroll, 0));
        frame.render_widget(chat, chat_area);

        // 手动翻阅时在消息区右下角提示距底部的行数。
        if let Some(position) = app.scroll {
            let below = max_scroll.saturating_sub(position);
            if below > 0 && chat_area.height > 0 {
                let hint = format!(" ↓ {below} 行 ");
                let hint_width = unicode_width::UnicodeWidthStr::width(hint.as_str()) as u16;
                let width = hint_width.min(chat_area.width);
                let hint_area = Rect {
                    x: chat_area.x + chat_area.width - width,
                    y: chat_area.y + chat_area.height - 1,
                    width,
                    height: 1,
                };
                // 角标覆盖在消息文字上，补暗色背景保证可读。
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        hint,
                        Style::new().fg(COLOR_WARNING).bg(COLOR_USER_BG),
                    ))),
                    hint_area,
                );
            }
        }
    }

    #[cfg(feature = "plugins")]
    if command_preview_height > 0 {
        render_command_preview(frame, app, command_section, &command_matches);
    }

    // 输入区是四边圆角边框的多行输入盒，内容超过六行后内部垂直滚动。
    let input_area = input_section;
    let input_color = if app.running {
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
        .border_style(Style::new().fg(if app.running {
            COLOR_WARNING
        } else if main_input_focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_MUTED
        }))
        .padding(Padding::new(1, 1, 0, 0));
    // 插件渐进加载不会阻塞输入，只有 Agent 运行状态占用输入盒标题。
    let state_title = if app.running {
        " 运行中 · Esc 中断 ".to_string()
    } else {
        String::new()
    };
    if !state_title.is_empty() {
        input_block = input_block.title(Span::styled(state_title, Style::new().fg(COLOR_WARNING)));
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
    let mut footer = vec![
        Span::styled("lucia", Style::new().fg(COLOR_USER).bold()),
        Span::raw("  "),
        Span::styled(app.model_name.as_str(), Style::new().fg(COLOR_TEXT)),
    ];
    if workspace.width >= 96 {
        footer.extend([
            Span::styled("  |  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(cwd_display, Style::new().fg(COLOR_MUTED)),
        ]);
    }
    if workspace.width >= 64 {
        if let Some(branch) = app.workspace.git_branch.as_deref() {
            footer.extend([
                Span::styled("  |  ", Style::new().fg(COLOR_BORDER_FOCUS)),
                Span::styled(
                    format!("⎇ {}", truncate_line(branch, 24)),
                    Style::new().fg(COLOR_MUTED),
                ),
            ]);
        }
    }
    if let Some(tokens) = app.context_tokens {
        let (label, color) = context_status(tokens, app.context_window);
        footer.extend([
            Span::styled("  |  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(format!("◔ {label}"), Style::new().fg(color)),
        ]);
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
    frame.render_widget(
        Paragraph::new(Line::from(footer)).block(Block::new().padding(Padding::horizontal(1))),
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

/// 按 Ratatui `Wrap { trim: false }` 的词边界行为计算单个逻辑行的视觉行数。
fn wrapped_line_count(line: &Line<'_>, max_width: u16) -> u16 {
    if max_width == 0 {
        return 0;
    }
    let mut rows = 0u16;
    let mut line_width = 0u16;
    let mut word_width = 0u16;
    let mut whitespace = VecDeque::new();
    let mut whitespace_width = 0u16;
    let mut previous_was_word = false;

    for ch in line.spans.iter().flat_map(|span| span.content.chars()) {
        let is_whitespace = ch.is_whitespace();
        let width = u16::try_from(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
            .unwrap_or(u16::MAX);
        if width > max_width {
            continue;
        }
        let segment_finished = previous_was_word && is_whitespace;
        let segment_overflow = line_width == 0
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(width)
                > max_width;
        if segment_finished || segment_overflow {
            line_width = line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width);
            whitespace.clear();
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= max_width;
        let pending_overflow = width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= max_width;
        if line_full || pending_overflow {
            let mut remaining = max_width.saturating_sub(line_width);
            rows = rows.saturating_add(1);
            line_width = 0;
            while let Some(front) = whitespace.front().copied() {
                if front > remaining {
                    break;
                }
                whitespace.pop_front();
                whitespace_width = whitespace_width.saturating_sub(front);
                remaining = remaining.saturating_sub(front);
            }
            if is_whitespace && whitespace.is_empty() {
                previous_was_word = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace.push_back(width);
            whitespace_width = whitespace_width.saturating_add(width);
        } else {
            word_width = word_width.saturating_add(width);
        }
        previous_was_word = !is_whitespace;
    }

    if line_width > 0 || word_width > 0 || whitespace_width > 0 {
        rows = rows.saturating_add(1);
    }
    rows.max(1)
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
    // 头像宽 36 列、高 9 行；与版本和速查合计 17 行。
    if area.height >= 17 && area.width >= 38 {
        lines.extend(hero_portrait_lines());
    }
    lines.push(Line::from(Span::styled(
        format!("lucia v{} · {cwd}", env!("CARGO_PKG_VERSION")),
        Style::new().fg(COLOR_MUTED),
    )));
    lines.push(Line::default());
    // 键位列右对齐、说明列按显示宽度补齐，保证整行居中后两列仍纵向对齐。
    for (key, action) in [
        ("Enter", "发送消息"),
        ("Ctrl+J", "插入换行"),
        ("Esc", "中断 / 清空 / 退出"),
        ("PgUp/PgDn", "滚动历史"),
        ("Ctrl+P", "历史输入回溯"),
        ("Ctrl+Y", "复制最近回复"),
    ] {
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
