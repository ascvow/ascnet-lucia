//! 原生命令面板、会话与插件对话框的渲染。

use crate::app::{reasoning_level_label, App};
use crate::command::{
    reasoning_level_command_value, NativeSessionDialogMode, NativeSessionDialogStatus,
};
#[cfg(feature = "plugins")]
use crate::command::{NativePluginDialogStatus, NativePluginPathMode};
use crate::theme::{COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_MUTED, COLOR_TEXT, COLOR_USER_BG};
#[cfg(feature = "plugins")]
use crate::theme::{COLOR_USER, COLOR_WARNING};
use crate::ui::markdown::truncate_line;
use ratatui::prelude::*;
use ratatui::widgets::*;
/// 在输入区上方绘制原生命令补全面板。
pub(crate) fn render_native_command_panel(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    if app.native_command.reasoning_panel.is_some() {
        render_native_reasoning_panel(frame, app, area);
        return;
    }
    let matches = app.slash_command_matches();
    if matches.is_empty() {
        return;
    }
    let selected = app.native_command.selected.min(matches.len() - 1);
    let lines = matches
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let style = if index == selected {
                Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
            } else {
                Style::new().fg(COLOR_MUTED)
            };
            Line::from(vec![
                Span::styled(format!("/{}  ", command.name), style),
                Span::styled(command.summary.clone(), style),
            ])
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

/// 在主界面上方绘制原生会话对话框。
pub(crate) fn render_native_session_dialog(frame: &mut Frame, app: &App, outer: Rect) {
    let Some(dialog) = app.native_command.dialog.as_ref() else {
        return;
    };
    let width = outer.width.saturating_sub(4).clamp(20, 88);
    let height = outer.height.saturating_sub(4).clamp(6, 24);
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
                    let style = if index == selected {
                        Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
                    } else {
                        Style::new().fg(COLOR_MUTED)
                    };
                    Line::from(Span::styled(label, style))
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
            let style = if index == selected {
                Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
            } else {
                Style::new().fg(COLOR_MUTED)
            };
            Line::from(Span::styled(label, style))
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
pub(crate) fn render_native_plugin_dialog(frame: &mut Frame, app: &App, outer: Rect) {
    let Some(dialog) = app.native_command.plugin_dialog.as_ref() else {
        return;
    };
    let width = outer.width.saturating_sub(2).clamp(30, 104);
    let height = outer.height.saturating_sub(2).clamp(10, 30);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let inner_height = usize::from(height.saturating_sub(5));
    let mut lines = match &dialog.status {
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
                            let style = if index == selected {
                                Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
                            } else {
                                Style::new().fg(COLOR_MUTED)
                            };
                            Line::from(Span::styled(label, style))
                        }),
                );
                content
            }
        }
    };
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
        lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), Style::new().fg(COLOR_USER)),
            Span::styled(&dialog.path_input, Style::new().fg(COLOR_TEXT)),
        ]));
    } else if let Some(plugin_id) = &dialog.remove_confirmation {
        lines.push(Line::from(Span::styled(
            format!("再次按 x 确认移除 `{plugin_id}`；该操作不会热重载当前 Session"),
            Style::new().fg(COLOR_WARNING),
        )));
    }
    let footer = if dialog.path_mode.is_some() {
        " 输入路径 · Enter 提交 · Esc 取消 "
    } else {
        " ↑↓ · e 启停 · g/h/i 安装 · t/u 更新 · b 回滚 · o/w Owner · x 移除 · r 刷新 · Esc "
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .title("Plugins · 人工管理")
                .title_bottom(Line::from(footer).alignment(Alignment::Center))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
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
