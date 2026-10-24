//! 插件停靠视图、Dialog 和全屏子视图的 Ratatui 渲染组件。

use crate::app::{
    default_plugin_height, default_plugin_width, plugin_view_visible, App, PluginViewState,
};
use crate::theme::{
    COLOR_ACCENT, COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_MUTED, COLOR_SEPARATOR, COLOR_SUCCESS,
    COLOR_TEXT, COLOR_USER, COLOR_WARNING,
};
use crate::ui::sheet::{palette_sheet_viewport, render_palette_sheet, PaletteSheet, SheetShape};
use agent_plugin_protocol::UiFrame as PluginUiFrame;
use agent_plugin_protocol::{UiColor, UiPlacement, UiStyle};
use ratatui::prelude::*;
use ratatui::widgets::*;

/// 按加载顺序分配四向停靠插槽，并返回剩余的主界面区域。
pub(crate) fn render_docked_plugin_views(frame: &mut Frame, app: &mut App, outer: Rect) -> Rect {
    let mut remaining = outer;
    for placement in [
        UiPlacement::Top,
        UiPlacement::Bottom,
        UiPlacement::Left,
        UiPlacement::Right,
    ] {
        let indices: Vec<usize> = app
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                view.declaration.placement == placement && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
            .collect();
        for index in indices {
            let requested = match placement {
                UiPlacement::Top | UiPlacement::Bottom => app.plugin_views[index]
                    .declaration
                    .size
                    .height
                    .unwrap_or_else(|| default_plugin_height(placement)),
                UiPlacement::Left | UiPlacement::Right => app.plugin_views[index]
                    .declaration
                    .size
                    .width
                    .unwrap_or_else(|| default_plugin_width(placement)),
                UiPlacement::Dialog
                | UiPlacement::Input
                | UiPlacement::Subview
                | UiPlacement::InputPanel
                | UiPlacement::ComposerShelf => 0,
            };
            let (plugin_area, next_remaining) = split_plugin_area(remaining, placement, requested);
            remaining = next_remaining;
            if plugin_area.is_empty() {
                continue;
            }
            let focused = app.plugin_focus == Some(index);
            render_plugin_view(frame, &mut app.plugin_views[index], plugin_area, focused);
        }
    }
    remaining
}

/// 从剩余区域的一侧切出插件区域，同时为主界面保留最小可用空间。
fn split_plugin_area(area: Rect, placement: UiPlacement, requested: u16) -> (Rect, Rect) {
    match placement {
        UiPlacement::Top => {
            let size = requested.min(area.height.saturating_sub(6));
            (
                Rect::new(area.x, area.y, area.width, size),
                Rect::new(
                    area.x,
                    area.y.saturating_add(size),
                    area.width,
                    area.height.saturating_sub(size),
                ),
            )
        }
        UiPlacement::Bottom => {
            let size = requested.min(area.height.saturating_sub(6));
            (
                Rect::new(
                    area.x,
                    area.y.saturating_add(area.height.saturating_sub(size)),
                    area.width,
                    size,
                ),
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(size)),
            )
        }
        UiPlacement::Left => {
            let size = requested.min(area.width.saturating_sub(24));
            (
                Rect::new(area.x, area.y, size, area.height.saturating_sub(1)),
                Rect::new(
                    area.x.saturating_add(size),
                    area.y,
                    area.width.saturating_sub(size),
                    area.height,
                ),
            )
        }
        UiPlacement::Right => {
            let size = requested.min(area.width.saturating_sub(24));
            (
                Rect::new(
                    area.x.saturating_add(area.width.saturating_sub(size)),
                    area.y,
                    size,
                    area.height.saturating_sub(1),
                ),
                Rect::new(area.x, area.y, area.width.saturating_sub(size), area.height),
            )
        }
        UiPlacement::Dialog
        | UiPlacement::Input
        | UiPlacement::Subview
        | UiPlacement::InputPanel
        | UiPlacement::ComposerShelf => (Rect::default(), area),
    }
}

/// 返回停靠视图与主界面之间那一条分隔线所在的边。
///
/// 只画朝向主内容的一条细线，而不是四面包围的盒子：盒子会在每个插件周围堆叠出
/// 大量框线，与原生命令面板的单横线风格也不一致。
fn panel_edge(placement: UiPlacement) -> Borders {
    match placement {
        UiPlacement::Top => Borders::BOTTOM,
        UiPlacement::Bottom => Borders::TOP,
        UiPlacement::Left => Borders::RIGHT,
        UiPlacement::Right => Borders::LEFT,
        _ => Borders::TOP,
    }
}

/// 构造插件面板的标题行。
///
/// 标题作为内容首行而不是嵌在边框上：侧边停靠只有一条竖线，边框上放不下标题，
/// 统一成内容行后四个方向的观感才一致。
fn panel_title_line(title: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new().fg(COLOR_TEXT).bold()
    } else {
        Style::new().fg(COLOR_MUTED).bold()
    };
    Line::from(Span::styled(title.to_string(), style))
}

/// 渲染一个静态插件视图并记录去除外框后的内容区。
fn render_plugin_view(frame: &mut Frame, view: &mut PluginViewState, area: Rect, focused: bool) {
    let block = Block::new()
        .borders(panel_edge(view.declaration.placement))
        .border_style(Style::new().fg(if focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_SEPARATOR
        }))
        .padding(Padding::horizontal(1));
    let content_area = block.inner(area);
    view.area = content_area;
    let mut lines = vec![panel_title_line(&view.declaration.title, focused)];
    lines.extend(
        view.frame
            .as_ref()
            .map(plugin_frame_lines)
            .unwrap_or_default(),
    );
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if focused {
        set_plugin_cursor(frame, view.frame.as_ref(), content_area);
    }
}

/// 渲染浮在主界面之上的插件对话框。
///
/// 对话框与命令面板共用一块底板：横向铺满输入盒、只有一条上分隔线，标题画在线上。
/// 插件声明的 `size.height` 表示可见内容行数，宽度由宿主统一取满——对话框是临时面板，
/// 与它下方输入盒对齐才不会在屏幕中央割出一块孤立的盒子。
fn render_plugin_dialog_view(
    frame: &mut Frame,
    view: &mut PluginViewState,
    anchor: Rect,
    content_rows: u16,
) {
    let lines = view
        .frame
        .as_ref()
        .map(plugin_frame_lines)
        .unwrap_or_default();
    let title = view.declaration.title.clone();
    let (height, _) =
        palette_sheet_viewport(frame, anchor, content_rows, false, SheetShape::Composer);
    let content_area = render_palette_sheet(
        frame,
        anchor,
        height,
        PaletteSheet {
            title: &title,
            lines,
            hint: None,
            scroll: 0,
            wrap: false,
            shape: SheetShape::Composer,
        },
    );
    view.area = content_area;
    set_plugin_cursor(frame, view.frame.as_ref(), content_area);
}

/// 在主界面之上渲染最后一个可见对话框。
pub(crate) fn render_plugin_dialog(frame: &mut Frame, app: &mut App, anchor: Rect) {
    let Some(index) = app.active_dialog_index() else {
        return;
    };
    let content_rows = app.plugin_views[index]
        .declaration
        .size
        .height
        .unwrap_or_else(|| default_plugin_height(UiPlacement::Dialog));
    render_plugin_dialog_view(frame, &mut app.plugin_views[index], anchor, content_rows);
}

/// 以全屏内容替换主视图，并保留宿主统一的面包屑与返回交互。
pub(crate) fn render_plugin_subview(frame: &mut Frame, app: &mut App, outer: Rect) {
    let breadcrumbs = app.view_stack.breadcrumbs().join(" / ");
    render_subview_header(frame, &breadcrumbs, outer);
    let Some(active) = app.view_stack.active_mut() else {
        return;
    };
    // 全屏子视图不再包一圈盒子：整屏边框只是噪声，面包屑已经说明了当前位置。
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(COLOR_SEPARATOR))
        .padding(Padding::horizontal(1));
    let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(outer);
    active.area = block.inner(sections[1]);
    let plugin_frame = active.frame();
    let lines = plugin_frame
        .filter(|plugin_frame| plugin_frame.visible)
        .map(plugin_frame_lines)
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "Loading view...",
                Style::new().fg(COLOR_MUTED),
            ))]
        });
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        sections[1],
    );
    set_plugin_cursor(frame, plugin_frame, active.area);
}

/// 绘制子视图共用的一行面包屑与返回提示。
///
/// 插件视图与宿主原生 Agent 会话共用同一位置和样式，切换视图时顶栏不会跳动。
pub(crate) fn render_subview_header(frame: &mut Frame, breadcrumbs: &str, outer: Rect) {
    let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(outer);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(breadcrumbs.to_string(), Style::new().fg(COLOR_TEXT).bold()),
            Span::styled("  Esc Back", Style::new().fg(COLOR_MUTED)),
        ])),
        sections[0],
    );
}

/// 将插件声明的相对坐标限制在内容区内，并恢复真实终端光标供输入法组合文本使用。
fn set_plugin_cursor(frame: &mut Frame, plugin_frame: Option<&PluginUiFrame>, area: Rect) {
    let Some(cursor) = plugin_frame.and_then(|plugin_frame| plugin_frame.cursor) else {
        return;
    };
    if area.is_empty() {
        return;
    }
    frame.set_cursor_position((
        area.x
            .saturating_add(cursor.x.min(area.width.saturating_sub(1))),
        area.y
            .saturating_add(cursor.y.min(area.height.saturating_sub(1))),
    ));
}

/// 将插件声明式文本帧转换成 Ratatui 行。
/// 将宿主无关插件帧转换为 Ratatui 行，供固定视图和消息 renderer 共用。
pub(crate) fn plugin_frame_lines(plugin_frame: &PluginUiFrame) -> Vec<Line<'static>> {
    plugin_frame
        .lines
        .iter()
        .map(|line| {
            Line::from(
                line.spans
                    .iter()
                    .map(|span| {
                        Span::styled(
                            crate::ui::plain_terminal_text(&span.text).into_owned(),
                            plugin_style(&span.style),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// 在输入区上方依次渲染当前参与布局的触发面板或常驻上下文架。
pub(crate) fn render_composer_panels(frame: &mut Frame, app: &mut App, area: Rect) {
    let indices = app.visible_composer_panels();
    if indices.is_empty() {
        return;
    }
    if area.is_empty() {
        return;
    }
    let constraints = indices
        .iter()
        .map(|index| Constraint::Length(app.composer_panel_height_at(*index)))
        .collect::<Vec<_>>();
    let sections = Layout::vertical(constraints).split(area);
    for (index, panel_area) in indices.into_iter().zip(sections.iter().copied()) {
        if panel_area.is_empty() {
            continue;
        }
        let focused = app.plugin_focus == Some(index);
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().fg(if focused {
                COLOR_BORDER_FOCUS
            } else {
                COLOR_SEPARATOR
            }))
            .padding(Padding::horizontal(1));
        app.plugin_views[index].area = block.inner(panel_area);
        let lines = app.plugin_views[index]
            .frame
            .as_ref()
            .map(plugin_frame_lines)
            .unwrap_or_default();
        frame.render_widget(Paragraph::new(lines).block(block), panel_area);
    }
}

/// 用带标题边框的可见 Input 插件视图替换主文本输入区；底边以实际输入盒为锚点，避开多行底栏。
pub(crate) fn render_plugin_input(
    frame: &mut Frame,
    app: &mut App,
    workspace: Rect,
    composer: Rect,
) {
    let Some(index) = app.visible_plugin_input() else {
        return;
    };
    let requested_height = app.plugin_views[index]
        .declaration
        .size
        .height
        .unwrap_or(3)
        .max(3)
        .min(composer.bottom().saturating_sub(workspace.y));
    let area = Rect::new(
        composer.x,
        composer.bottom().saturating_sub(requested_height),
        composer.width,
        requested_height,
    );
    if area.is_empty() {
        return;
    }
    let title = app.plugin_views[index].declaration.title.clone();
    // 独占输入是需要用户决策的审批卡片，保留完整边框把它与主输入区分开；
    // 圆角与主输入框一致，避免同一位置出现两种边框风格。
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(COLOR_TEXT).bold(),
        ))
        .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
        .padding(Padding::horizontal(1));
    app.plugin_views[index].area = block.inner(area);
    let lines = app.plugin_views[index]
        .frame
        .as_ref()
        .map(plugin_frame_lines)
        .unwrap_or_default();
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// 将稳定插件样式子集映射到当前 Ratatui 样式。
fn plugin_style(plugin_style: &UiStyle) -> Style {
    let mut style = Style::new();
    if let Some(color) = plugin_style.foreground {
        // 显式底色通常与前景配成一对；不能只把 White 前景替换成终端默认黑字。
        style = style.fg(if plugin_style.background.is_some() {
            plugin_palette_color(color)
        } else {
            plugin_color(color)
        });
    }
    if let Some(color) = plugin_style.background {
        style = style.bg(plugin_palette_color(color));
    }
    if plugin_style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if plugin_style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if plugin_style.underlined {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if plugin_style.reversed {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// 显式前景/背景配对保持终端色板槽位，避免把其中一侧错误地替换为默认色。
fn plugin_palette_color(color: UiColor) -> Color {
    match color {
        UiColor::Black => Color::Black,
        UiColor::Red => Color::Red,
        UiColor::Green => Color::Green,
        UiColor::Yellow => Color::Yellow,
        UiColor::Blue => Color::Blue,
        UiColor::Magenta => Color::Magenta,
        UiColor::Cyan => Color::Cyan,
        UiColor::White => Color::White,
        UiColor::Gray => Color::Gray,
    }
}

/// 将插件便携颜色映射到 Lucia 色板。
///
/// 插件只声明语义色，沿用原生组件的色标；最终由 `theme::apply_terminal_theme` 一并
/// 映射到终端默认色和 ANSI 色板，不维护一套深色专用的插件配色。
fn plugin_color(color: UiColor) -> Color {
    match color {
        UiColor::Black => Color::Black,
        UiColor::Red => COLOR_DANGER,
        UiColor::Green => COLOR_SUCCESS,
        UiColor::Yellow => COLOR_WARNING,
        UiColor::Blue => COLOR_BORDER_FOCUS,
        UiColor::Magenta => COLOR_USER,
        UiColor::Cyan => COLOR_ACCENT,
        UiColor::White => COLOR_TEXT,
        UiColor::Gray => COLOR_MUTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use agent_plugin_protocol::{UiDeclaration, UiLine, UiSize, UiSpan};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    /// 工具组件完全替代默认参数/摘要，Unicode、多行与终端控制序列安全进入窄屏布局。
    #[test]
    fn tool_component_is_single_safe_multiline_block() {
        use crate::session::conversation::{Msg, MsgKind};
        use crate::ui::transcript_layout::TranscriptLayout;
        use agent_tool::{ToolCall, ToolResult};
        let mut message = Msg::tool_started(ToolCall::new(
            "send-1",
            "example_send",
            serde_json::json!({"payload":"不应重复"}),
        ));
        message.kind = MsgKind::ToolOk;
        message.tool_result = Some(ToolResult::success(
            "send-1",
            "example_send",
            serde_json::json!({"content":"不应重复"}),
        ));
        message.tool_frame = Some(PluginUiFrame {
            view_id: "example-message".into(),
            visible: true,
            lines: [
                "✓ 消息 · 已投递",
                "  @审查员: 第一行",
                "  中文 👩‍💻 e\u{301}",
                "  \u{001b}[31m第二行\u{001b}[0m",
            ]
            .into_iter()
            .map(|text| UiLine {
                spans: vec![UiSpan {
                    text: text.into(),
                    style: UiStyle::default(),
                }],
            })
            .collect(),
            cursor: None,
        });
        let lines = message.to_lines(false, 80);
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("@审查员:").count(), 1);
        assert!(!text.contains("example_send"));
        assert!(!text.contains("不应重复"));
        assert!(!text.contains("[31m"));
        assert!(text.contains("👩‍💻 e\u{301}"));
        assert!(!message.is_tool_expandable());
        for width in [12, 40, 80] {
            let mut layout = TranscriptLayout::default();
            let height = layout.sync(std::slice::from_ref(&message), None, width);
            let buffer = layout.render_range(
                Rect::new(0, 0, width, height as u16),
                std::slice::from_ref(&message),
                None,
                0,
                height as u16,
            );
            let rendered = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .replace(' ', "");
            assert!(rendered.contains("第二行"), "{rendered}");
            assert!(buffer
                .content
                .iter()
                .all(|cell| !cell.symbol().chars().any(char::is_control)));
        }
    }

    /// 插件默认文字跟随终端；显式前景/背景必须成对保留色板槽位，不能变成黑底黑字。
    #[test]
    fn plugin_theme_preserves_explicit_color_pairs_and_modifiers() {
        use crate::theme::{apply_terminal_theme, TerminalColors};
        let styles = [
            UiStyle {
                foreground: Some(UiColor::White),
                ..UiStyle::default()
            },
            UiStyle {
                foreground: Some(UiColor::White),
                background: Some(UiColor::Black),
                ..UiStyle::default()
            },
            UiStyle {
                foreground: Some(UiColor::Black),
                background: Some(UiColor::White),
                reversed: true,
                ..UiStyle::default()
            },
            UiStyle {
                foreground: Some(UiColor::Red),
                underlined: true,
                ..UiStyle::default()
            },
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        for (index, style) in styles.iter().enumerate() {
            buffer[(index as u16, 0)]
                .set_symbol("x")
                .set_style(plugin_style(style));
        }
        apply_terminal_theme(&mut buffer, TerminalColors::TrueColor);
        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(
            (buffer[(1, 0)].fg, buffer[(1, 0)].bg),
            (Color::White, Color::Black)
        );
        assert_eq!(
            (buffer[(2, 0)].fg, buffer[(2, 0)].bg),
            (Color::Black, Color::White)
        );
        assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(3, 0)].fg, Color::Red);
        assert!(buffer[(3, 0)].modifier.contains(Modifier::UNDERLINED));
        apply_terminal_theme(&mut buffer, TerminalColors::Monochrome);
        assert!(buffer
            .content
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));
        assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// 构造一个只用于渲染的插件对话框视图。
    fn dialog_view() -> PluginViewState {
        PluginViewState {
            declaration: UiDeclaration {
                plugin_id: "test-plugin".into(),
                view_id: "dialog".into(),
                title: "插件对话框".into(),
                placement: UiPlacement::Dialog,
                size: UiSize {
                    width: Some(24),
                    height: Some(4),
                },
                focusable: true,
                input_triggers: Vec::new(),
            },
            frame: Some(PluginUiFrame {
                view_id: "dialog".into(),
                visible: true,
                lines: vec![UiLine {
                    spans: vec![UiSpan {
                        text: "插件内容".into(),
                        style: UiStyle::default(),
                    }],
                }],
                cursor: None,
            }),
            area: Rect::default(),
        }
    }

    /// 独占输入卡片跟随实际输入盒底边，不能覆盖折行后的底栏或它的点击区域。
    #[test]
    fn plugin_input_stays_above_wrapped_footer() {
        let mut view = dialog_view();
        view.declaration.placement = UiPlacement::Input;
        view.declaration.title = "Approval".into();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "long-model-name-that-wraps".into());
        app.context_tokens = Some(50_000);
        app.context_window = Some(200_000);
        app.plugin_views.push(view);
        let mut terminal = Terminal::new(TestBackend::new(24, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::root::render_root(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let footer_row = buffer
            .content
            .chunks(24)
            .position(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .starts_with("long-model")
            })
            .unwrap();
        assert!(app.plugin_views[0].area.bottom() < footer_row as u16);
        for area in &app.context_chip_areas {
            assert!(usize::from(area.y) >= footer_row);
        }
        assert_eq!(buffer[(0, footer_row as u16 - 1)].symbol(), "╰");
    }

    /// 左右停靠视图保留一行原有外侧空白；主会话在自己的剩余区域内分配多行底栏。
    #[test]
    fn side_docks_leave_one_footer_row() {
        let area = Rect::new(2, 3, 80, 24);

        let (left, left_remaining) = split_plugin_area(area, UiPlacement::Left, 30);
        assert_eq!(left, Rect::new(2, 3, 30, 23));
        assert_eq!(left_remaining, Rect::new(32, 3, 50, 24));

        let (right, right_remaining) = split_plugin_area(area, UiPlacement::Right, 30);
        assert_eq!(right, Rect::new(52, 3, 30, 23));
        assert_eq!(right_remaining, Rect::new(2, 3, 50, 24));
    }

    /// 侧栏变化后的第一帧必须与全新绘制一致；旧滚动条不能擦掉新侧栏的边线或文字。
    #[test]
    fn right_dock_remains_intact_across_layout_changes() {
        let mut dock = dialog_view();
        dock.declaration.placement = UiPlacement::Right;
        dock.declaration.title = "Team".into();
        dock.frame.as_mut().unwrap().lines = vec![
            UiLine {
                spans: vec![UiSpan {
                    text: "✓ 中文成员 · Ready".into(),
                    style: UiStyle::default(),
                }],
            },
            UiLine::default(),
            UiLine {
                spans: vec![UiSpan {
                    text: "0123456789abcdef".into(),
                    style: UiStyle::default(),
                }],
            },
        ];
        let new_app = || {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(tx, "test-model".into());
            app.messages.push(crate::session::conversation::Msg::new(
                crate::session::conversation::MsgKind::Assistant,
                "```rust\n    render(frame, &app); // 中文\n```\n".repeat(30),
            ));
            app.plugin_views.push(PluginViewState {
                declaration: dock.declaration.clone(),
                frame: dock.frame.clone(),
                area: Rect::default(),
            });
            app
        };
        let mut app = new_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // 先让旧滚动条落到新边线，再落到新内容区；随后覆盖隐藏、重现与连续缩放。
        for (width, height, dock_width, visible, empty) in [
            (100, 24, 28, true, false),
            (100, 24, 29, true, false),
            (100, 24, 37, true, false),
            (92, 20, 37, true, false),
            (92, 20, 37, false, false),
            (92, 20, 37, true, false),
            (56, 12, 28, true, false),
            (12, 8, 28, true, false),
            (110, 26, 28, true, false),
            (110, 26, 36, true, true),
        ] {
            let configure = |app: &mut App| {
                app.plugin_views[0].declaration.size.width = Some(dock_width);
                app.plugin_views[0].frame.as_mut().unwrap().visible = visible;
                if empty {
                    app.messages.clear();
                }
            };
            configure(&mut app);
            terminal.backend_mut().resize(width, height);
            let rendered = terminal
                .draw(|frame| crate::ui::root::render_root(frame, &mut app))
                .unwrap()
                .buffer
                .clone();

            let mut fresh = new_app();
            configure(&mut fresh);
            let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
            let expected_frame = expected
                .draw(|frame| crate::ui::root::render_root(frame, &mut fresh))
                .unwrap();
            assert_eq!(
                &rendered,
                expected_frame.buffer,
                "首帧不能依赖旧几何：{width}x{height}, dock={dock_width}, visible={visible}, empty={empty}"
            );
            // TestBackend 只接收差分，不模拟终端覆盖双宽字后清掉延续格的行为。
            // 逐一比较可见格，跳过当前字素占用的延续格；完整布局已由上面的帧断言验证。
            for y in 0..height {
                let mut x = 0;
                while x < width {
                    let cell = &rendered[(x, y)];
                    assert_eq!(terminal.backend().buffer()[(x, y)], *cell);
                    x += unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
                }
            }
            assert_eq!(app.scrollbar_area, fresh.scrollbar_area);
            assert_eq!(app.plugin_views[0].area, fresh.plugin_views[0].area);
        }
    }

    /// 对话框与命令面板共用底板：铺满输入盒宽度、标题画在上分隔线上、没有四周圆角框。
    #[test]
    fn plugin_dialog_uses_the_command_palette_chrome() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.plugin_views.push(dialog_view());
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).expect("创建测试终端");
        // 输入盒占据最后三行，锚点取它的上沿。
        let anchor = Rect::new(0, 13, 40, 1);
        terminal
            .draw(|frame| render_plugin_dialog(frame, &mut app, anchor))
            .expect("渲染插件对话框");

        let lines: Vec<String> = (0..16u16)
            .map(|row| {
                terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .skip(usize::from(row) * 40)
                    .take(40)
                    .map(|cell| cell.symbol())
                    .collect()
            })
            .collect();
        let screen = lines.join("\n");
        let compact = |text: &str| -> String {
            text.chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        };

        // 声明的 height 是可见内容行数；宽度由宿主取满输入盒。
        let area = app.plugin_views[0].area;
        assert_eq!(area.width, 40);
        assert_eq!(area.height, 4);
        assert_eq!(area.bottom(), anchor.y - 1, "面板应与输入盒隔一行");
        assert!(compact(&screen).contains("插件对话框"), "{screen}");
        assert!(compact(&screen).contains("插件内容"), "{screen}");

        let title_row = lines
            .iter()
            .position(|line| compact(line).contains("插件对话框"))
            .expect("标题应画在面板上");
        assert!(
            lines[title_row].contains('─'),
            "标题必须画在上分隔线：{screen}"
        );
        // 只剩朝向正文的上分隔线：圆角与竖向边框都不应出现。
        assert!(!screen.contains('╭'), "{screen}");
        assert!(!screen.contains('│'), "{screen}");
    }
}
