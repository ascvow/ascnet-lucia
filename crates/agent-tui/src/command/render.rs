//! 原生命令面板、会话与插件对话框的渲染。

use crate::app::{reasoning_level_label, App};
use crate::command::{
    reasoning_level_command_value, NativeSessionDialogMode, NativeSessionDialogStatus,
    SlashCommandMatch,
};
#[cfg(feature = "plugins")]
use crate::command::{NativePluginDialogStatus, NativePluginPathMode};
use crate::theme::{
    COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_MUTED, COLOR_PALETTE_COMMAND,
    COLOR_PALETTE_SELECTION_BG, COLOR_SELECTION_BG, COLOR_SELECTION_MARK, COLOR_TEXT,
    COLOR_TEXT_SOFT,
};
#[cfg(feature = "plugins")]
use crate::theme::{COLOR_USER, COLOR_WARNING};
use crate::ui::markdown::truncate_line;
#[cfg(feature = "plugins")]
use crate::ui::wrapped_line_count;
use ratatui::prelude::*;
use ratatui::widgets::*;
/// 构造列表中一行的选中态片段。
///
/// 选中项用中性抬升背景铺满整行并加左侧指示条；未选中项保持正文色，不整行压暗——
/// 整行同色会让列表退化成一团难以扫读的灰字。
fn selected_row(label: &str, active: bool, row_width: usize) -> Vec<Span<'static>> {
    let row = if active {
        Style::new().bg(COLOR_SELECTION_BG)
    } else {
        Style::new()
    };
    let pad = row_width.saturating_sub(unicode_width::UnicodeWidthStr::width(label));
    let body = format!("{label}{}", " ".repeat(pad));
    vec![
        Span::styled(
            if active { "▌" } else { " " },
            Style::new().fg(COLOR_SELECTION_MARK).patch(row),
        ),
        Span::styled(
            body,
            if active {
                row.fg(COLOR_TEXT).bold()
            } else {
                row.fg(COLOR_MUTED)
            },
        ),
    ]
}

/// 命令补全菜单的左侧留白列数。
///
/// 补全面板贴着输入框，正文若直接顶到终端最左侧，扫读时视线要来回蹭屏幕边缘。
const PALETTE_PADDING_X: u16 = 2;

/// 选中行指示条左侧到命令名之间的左内边距列数。
///
/// 同时是选中底色与命令名之间的呼吸空间，右侧留白由行尾补足，两者对称。
const PALETTE_ROW_INSET: usize = 1;

/// 命令名列与描述列之间的固定间距。
const PALETTE_COLUMN_GAP: usize = 2;

/// 命令补全菜单中每个条目的行数：一行正文加一行间隔空行。
const PALETTE_ROW_HEIGHT: u16 = 2;

/// 返回 `count` 个命令占用的面板高度（含顶部分隔边框）。
///
/// 行距由渲染时的空行决定，布局拿到的高度必须出自同一份行几何：两边各算一套迟早会
/// 对不上，表现为列表底部被裁掉或输入框上方多出一片空白。
pub(super) fn command_palette_height(count: usize) -> u16 {
    let count = u16::try_from(count).unwrap_or(u16::MAX);
    count
        .saturating_mul(PALETTE_ROW_HEIGHT)
        .saturating_add(u16::from(count > 0))
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

/// 按宽度裁剪文本；宽度为零时直接返回空串。
///
/// [`truncate_line`] 在零宽度下仍会输出一个省略号，用于列对齐时会挤掉后面的列，因此
/// 这里把"没有空间"与"空间不足"分开处理。
fn fit_column(text: &str, width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        truncate_line(text, width)
    }
}

/// 构造命令补全菜单的一行：指示条、命令名、描述三列加整行底色。
///
/// `name_width` 是命令名列宽，`row_width` 是去掉边框与左侧留白后的整行宽度。命令名与
/// 描述各自裁剪到列宽内，保证一行永远只占一个终端行——行距由渲染列表追加的空行决定，
/// 一旦这里换行，面板高度就会和布局算出的高度对不上。
fn palette_row(
    command: &SlashCommandMatch,
    selected: bool,
    name_width: usize,
    row_width: usize,
) -> Line<'static> {
    // 未选中行不铺底色：整列底色会让菜单糊成一片，选中项也就失去了锚点。
    let row = if selected {
        Style::new().bg(COLOR_PALETTE_SELECTION_BG)
    } else {
        Style::new()
    };
    // 指示条 1 列 + 左内边距 + 命令列 + 列间距，右侧再留一列内边距。
    let prefix_width = 1 + PALETTE_ROW_INSET + name_width + PALETTE_COLUMN_GAP;
    let summary_width = row_width.saturating_sub(prefix_width + PALETTE_ROW_INSET);
    let name = fit_column(&format!("/{}", command.name), name_width);
    let name_pad = name_width.saturating_sub(unicode_width::UnicodeWidthStr::width(name.as_str()));
    let summary = fit_column(&command.summary, summary_width);
    // 行尾补白把底色铺到整行右边界；它同时是描述的右内边距，因此不再额外预留一列。
    let filler = row_width
        .saturating_sub(prefix_width + unicode_width::UnicodeWidthStr::width(summary.as_str()));
    // 选中项只加粗命令名：整行加粗会让底色块里全是高亮字，反而更难扫。
    let name_style = if selected {
        row.fg(COLOR_PALETTE_COMMAND).bold()
    } else {
        row.fg(COLOR_PALETTE_COMMAND)
    };
    Line::from(vec![
        Span::styled(
            if selected { "▌" } else { " " },
            Style::new().fg(COLOR_SELECTION_MARK).patch(row),
        ),
        Span::styled(" ".repeat(PALETTE_ROW_INSET), row),
        Span::styled(name, name_style),
        Span::styled(" ".repeat(name_pad + PALETTE_COLUMN_GAP), row),
        Span::styled(
            summary,
            if selected {
                row.fg(COLOR_TEXT)
            } else {
                row.fg(COLOR_TEXT_SOFT)
            },
        ),
        Span::styled(" ".repeat(filler), row),
    ])
}

/// 在输入区上方绘制原生命令补全面板。
pub(crate) fn render_native_command_panel(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
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
    // 去掉左侧留白后剩余的整行宽度，用于让选中底色铺满。
    let row_width = usize::from(area.width.saturating_sub(PALETTE_PADDING_X * 2));
    let name_width = command_column_width(&matches);
    // 每个条目前后各留一行空行：条目之间靠空行分段，最后一条下方也留一格，
    // 让面板与紧贴其下的输入框之间不至于粘在一起。
    let mut lines = Vec::with_capacity(matches.len() * usize::from(PALETTE_ROW_HEIGHT));
    for (index, command) in matches.iter().enumerate() {
        lines.push(palette_row(
            command,
            index == selected,
            name_width,
            row_width,
        ));
        lines.push(Line::default());
    }
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
                    .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                    .padding(Padding::horizontal(PALETTE_PADDING_X)),
            ),
        area,
    );
}

/// 在主界面上方绘制原生会话对话框。
pub(crate) fn render_native_session_dialog(frame: &mut Frame, app: &App, outer: Rect) {
    let Some(dialog) = app.native_command.dialog.as_ref() else {
        return;
    };
    // 尺寸下限是为了保证可读性，但终端比下限还小时必须先收回画布以内：`Clear`
    // 按坐标直接写单元格、不做裁剪，越界会 panic。
    let width = outer.width.saturating_sub(4).clamp(20, 88).min(outer.width);
    let height = outer
        .height
        .saturating_sub(4)
        .clamp(6, 24)
        .min(outer.height);
    if width == 0 || height == 0 {
        return;
    }
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let title = match dialog.mode {
        NativeSessionDialogMode::Resume => "恢复项目会话",
        NativeSessionDialogMode::Browse => "项目会话",
    };
    let inner_height = usize::from(height.saturating_sub(2));
    let row_width = usize::from(width.saturating_sub(4));
    let lines = match &dialog.status {
        NativeSessionDialogStatus::Loading => vec![Line::from(Span::styled(
            "正在读取会话…",
            Style::new().fg(COLOR_MUTED),
        ))],
        NativeSessionDialogStatus::Error(error) => vec![Line::from(Span::styled(
            format!("读取失败：{error}"),
            Style::new().fg(COLOR_DANGER),
        ))],
        NativeSessionDialogStatus::Ready(items) if items.is_empty() => vec![Line::from(
            Span::styled("当前项目还没有持久化会话", Style::new().fg(COLOR_MUTED)),
        )],
        NativeSessionDialogStatus::Ready(items) => {
            let selected = dialog.selected.min(items.len() - 1);
            let start = selected
                .saturating_sub(inner_height.saturating_sub(1) / 2)
                .min(items.len().saturating_sub(inner_height));
            items
                .iter()
                .enumerate()
                .skip(start)
                .take(inner_height)
                .map(|(index, summary)| {
                    let title = summary
                        .title
                        .as_deref()
                        .unwrap_or_else(|| summary.id.as_str());
                    let label = format!(
                        "{}  {} 条消息  {}",
                        truncate_line(title, 42),
                        summary.message_count,
                        relative_native_time(summary.updated_at_ms)
                    );
                    Line::from(selected_row(&label, index == selected, row_width))
                })
                .collect()
        }
    };
    let footer = match dialog.mode {
        NativeSessionDialogMode::Resume => " ↑↓ 选择 · Enter 恢复 · Esc 关闭 ",
        NativeSessionDialogMode::Browse => " ↑↓ 选择 · Enter 切换 · Esc 关闭 ",
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .title(title)
                .title_bottom(Line::from(footer).alignment(Alignment::Center))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// 在输入框上方绘制命令面板风格的思考等级选择；选项只来自 models.dev 元数据。
fn render_native_reasoning_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = app.native_command.reasoning_panel.as_ref() else {
        return;
    };
    let selected = panel
        .selected
        .min(app.reasoning_levels.len().saturating_sub(1));
    let row_width = usize::from(area.width.saturating_sub(4));
    let lines = app
        .reasoning_levels
        .iter()
        .enumerate()
        .map(|(index, level)| {
            let current = if *level == app.reasoning_level {
                "●"
            } else {
                " "
            };
            let label = format!(
                "{current} {:<6} {}",
                reasoning_level_command_value(*level),
                reasoning_level_label(*level)
            );
            Line::from(selected_row(&label, index == selected, row_width))
        })
        .collect::<Vec<_>>();
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

/// 在输入框上方绘制命令面板风格的模型上游选择；选项来自配置文件的多上游定义。
fn render_native_model_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = app.native_command.model_panel.as_ref() else {
        return;
    };
    let selected = panel
        .selected
        .min(app.model_upstreams.len().saturating_sub(1));
    let row_width = usize::from(area.width.saturating_sub(4));
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
            Line::from(selected_row(&label, index == selected, row_width))
        })
        .collect::<Vec<_>>();
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

/// 在主界面上方绘制人工插件管理页。
#[cfg(feature = "plugins")]
pub(crate) fn render_native_plugin_dialog(frame: &mut Frame, app: &mut App, outer: Rect) {
    let Some(dialog) = app.native_command.plugin_dialog.as_mut() else {
        return;
    };
    let reviewing_enable = dialog.enable_confirmation.is_some();
    let width_limit = if reviewing_enable { 132 } else { 104 };
    let width = outer.width.saturating_sub(2).min(width_limit);
    let height = if reviewing_enable {
        outer.height.saturating_sub(2)
    } else {
        outer.height.saturating_sub(2).min(30)
    };
    if width == 0 || height == 0 {
        return;
    }
    let row_width = usize::from(width.saturating_sub(4));
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let inner_height = usize::from(height.saturating_sub(5));
    let mut lines =
        match &dialog.status {
            NativePluginDialogStatus::Loading => vec![Line::from(Span::styled(
                "正在读取插件锁…",
                Style::new().fg(COLOR_MUTED),
            ))],
            NativePluginDialogStatus::Applying => vec![Line::from(Span::styled(
                "正在执行插件写操作；当前 Session 不会热重载…",
                Style::new().fg(COLOR_WARNING),
            ))],
            NativePluginDialogStatus::Error(error) => vec![
                Line::from(Span::styled(
                    format!("读取或提交失败：{error}"),
                    Style::new().fg(COLOR_DANGER),
                )),
                Line::from(Span::styled(
                    "按 r 重试，按 Esc 关闭",
                    Style::new().fg(COLOR_MUTED),
                )),
            ],
            NativePluginDialogStatus::Ready(data) => {
                let mut content = vec![
                    Line::from(Span::styled(
                        "Plugins are fixed dependencies for this Session.",
                        Style::new().fg(COLOR_MUTED),
                    )),
                    Line::from(Span::styled(
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
                    )),
                ];
                if data.plugins.is_empty() {
                    content.push(Line::from(Span::styled(
                        "未安装受管理插件；按 i 从本地 Bundle 安装",
                        Style::new().fg(COLOR_MUTED),
                    )));
                    content
                } else {
                    let selected = dialog.selected.min(data.plugins.len() - 1);
                    let selected_plugin = &data.plugins[selected];
                    let authorization_text = data
                        .authorizations
                        .get(&selected_plugin.id)
                        .map(format_plugin_authorization)
                        .unwrap_or_else(|| "授权 profile 不可用".into());
                    let capability_color = if data
                        .authorizations
                        .get(&selected_plugin.id)
                        .is_some_and(|profile| {
                            profile.manifest.requested.iter().any(|request| {
                                matches!(
                                    request.capability_id.as_str(),
                                    "process_exec" | "secrets" | "fs_write"
                                )
                            })
                        }) {
                        COLOR_WARNING
                    } else {
                        COLOR_MUTED
                    };
                    content.push(Line::from(Span::styled(
                        format!("授权：{}", truncate_line(&authorization_text, 96)),
                        Style::new().fg(capability_color),
                    )));
                    content.push(Line::from(Span::styled(
                        format!("来源：{}", truncate_line(&selected_plugin.source, 96)),
                        Style::new().fg(COLOR_MUTED),
                    )));
                    let visible_height = inner_height.saturating_sub(content.len());
                    let start = selected
                        .saturating_sub(visible_height.saturating_sub(1) / 2)
                        .min(data.plugins.len().saturating_sub(visible_height));
                    content.extend(
                        data.plugins
                            .iter()
                            .enumerate()
                            .skip(start)
                            .take(visible_height)
                            .map(|(index, plugin)| {
                                let state = if plugin.enabled {
                                    "已启用"
                                } else {
                                    "已禁用"
                                };
                                let label = format!(
                                    "{:<18} {:<10} {:<6} sha256:{}",
                                    truncate_line(&plugin.id, 18),
                                    truncate_line(&plugin.version, 10),
                                    state,
                                    truncate_line(&plugin.sha256, 12)
                                );
                                Line::from(selected_row(&label, index == selected, row_width))
                            }),
                    );
                    content
                }
            }
        };
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
        lines = format_plugin_enable_review(
            &confirmation.plugin_id,
            &confirmation.bundle_identity,
            data.authorizations.get(&confirmation.plugin_id),
            owner_capability,
        )
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::new().fg(COLOR_WARNING))))
        .collect();
    }
    if reviewing_enable {
        // 启用确认使用独立完整审查页，避免列表摘要和固定高度吞掉后续高风险能力。
    } else if let Some(mode) = dialog.path_mode {
        let label = match mode {
            NativePluginPathMode::InstallRegistry => "Registry name[@semver]",
            NativePluginPathMode::InstallGithub => "GitHub owner/repository 或 URL",
            NativePluginPathMode::Install => "本地安装 Bundle 路径",
            NativePluginPathMode::Replace => "本地更新 Bundle 路径",
            NativePluginPathMode::Rollback => "插件 ID 与历史版本",
            NativePluginPathMode::SelectCapabilityOwner => "Capability ID 与插件 ID",
            NativePluginPathMode::ClearCapabilityOwner => "待清除的 Capability ID",
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), Style::new().fg(COLOR_USER)),
            Span::styled(&dialog.path_input, Style::new().fg(COLOR_TEXT)),
        ]));
    } else if let Some(pending) = &dialog.pending_owner_selection {
        lines.push(Line::from(Span::styled(
            format!(
                "已记录将 `{}` 设为 `{}` Owner；按 e 审查来源与能力",
                pending.plugin_id, pending.capability_id
            ),
            Style::new().fg(COLOR_WARNING),
        )));
    } else if let Some(plugin_id) = &dialog.remove_confirmation {
        lines.push(Line::from(Span::styled(
            format!("再次按 x 确认移除 `{plugin_id}`；该操作不会热重载当前 Session"),
            Style::new().fg(COLOR_WARNING),
        )));
    }
    let footer = if reviewing_enable {
        " ↑↓/PgUp/PgDn 滚动审查 · e 确认 · Esc 取消 "
    } else if dialog.path_mode.is_some() {
        " 输入路径 · Enter 提交 · Esc 取消 "
    } else {
        " ↑↓ · e 启停 · g/h/i 安装 · t/u 更新 · b 回滚 · o/w Owner · x 移除 · r 刷新 · Esc "
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
        .title("Plugins · 人工管理")
        .title_bottom(Line::from(footer).alignment(Alignment::Center))
        .padding(Padding::horizontal(1));
    if reviewing_enable {
        let content_width = area.width.saturating_sub(4);
        let viewport_height = area.height.saturating_sub(2);
        let wrapped_height = lines.iter().fold(0u16, |height, line| {
            height.saturating_add(wrapped_line_count(line, content_width))
        });
        let max_scroll = wrapped_height.saturating_sub(viewport_height);
        dialog.review_scroll = dialog.review_scroll.min(max_scroll);
    } else {
        dialog.review_scroll = 0;
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((dialog.review_scroll, 0))
            .block(block),
        area,
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
