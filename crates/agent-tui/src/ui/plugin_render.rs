//! 插件停靠视图、Dialog 和全屏子视图的 Ratatui 渲染组件。

use crate::app::{
    default_plugin_height, default_plugin_width, plugin_view_visible, App, PluginViewState,
};
use crate::theme::{
    COLOR_ACCENT, COLOR_BORDER_FOCUS, COLOR_DANGER, COLOR_MUTED, COLOR_SEPARATOR, COLOR_SUCCESS,
    COLOR_TEXT, COLOR_USER, COLOR_WARNING,
};
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
/// 对话框是唯一保留完整边框的插件容器：它悬浮在内容之上，需要一圈边界把自己和
/// 下方内容分开；停靠视图有布局位置做区分，不需要盒子。
fn render_plugin_dialog_view(
    frame: &mut Frame,
    view: &mut PluginViewState,
    area: Rect,
    focused: bool,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(COLOR_SEPARATOR))
        .title(Span::styled(
            format!(" {} ", view.declaration.title),
            Style::new().fg(COLOR_TEXT).bold(),
        ))
        .padding(Padding::horizontal(1));
    let content_area = block.inner(area);
    view.area = content_area;
    let lines = view
        .frame
        .as_ref()
        .map(plugin_frame_lines)
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if focused {
        set_plugin_cursor(frame, view.frame.as_ref(), content_area);
    }
}

/// 在主界面之上渲染最后一个可见对话框。
pub(crate) fn render_plugin_dialog(frame: &mut Frame, app: &mut App, outer: Rect) {
    let Some(index) = app.active_dialog_index() else {
        return;
    };
    let declaration = &app.plugin_views[index].declaration;
    let width = declaration
        .size
        .width
        .unwrap_or_else(|| default_plugin_width(UiPlacement::Dialog))
        .min(outer.width.saturating_sub(2));
    let height = declaration
        .size
        .height
        .unwrap_or_else(|| default_plugin_height(UiPlacement::Dialog))
        .min(outer.height.saturating_sub(2));
    let area = Rect::new(
        outer
            .x
            .saturating_add(outer.width.saturating_sub(width) / 2),
        outer
            .y
            .saturating_add(outer.height.saturating_sub(height) / 2),
        width,
        height,
    );
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    render_plugin_dialog_view(frame, &mut app.plugin_views[index], area, true);
}

/// 以全屏内容替换主视图，并保留宿主统一的面包屑与返回交互。
pub(crate) fn render_plugin_subview(frame: &mut Frame, app: &mut App, outer: Rect) {
    let breadcrumbs = app.view_stack.breadcrumbs().join(" / ");
    let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(outer);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(breadcrumbs, Style::new().fg(COLOR_TEXT).bold()),
            Span::styled("  Esc Back", Style::new().fg(COLOR_MUTED)),
        ])),
        sections[0],
    );

    let Some(active) = app.view_stack.active_mut() else {
        return;
    };
    // 全屏子视图不再包一圈盒子：整屏边框只是噪声，面包屑已经说明了当前位置。
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(COLOR_SEPARATOR))
        .padding(Padding::horizontal(1));
    active.area = block.inner(sections[1]);
    let lines = active
        .frame
        .as_ref()
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
    set_plugin_cursor(frame, active.frame.as_ref(), active.area);
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
                    .map(|span| Span::styled(span.text.clone(), plugin_style(&span.style)))
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

/// 用带标题边框的可见 Input 插件视图替换主文本输入区。
pub(crate) fn render_plugin_input(frame: &mut Frame, app: &mut App, workspace: Rect) {
    let Some(index) = app.visible_plugin_input() else {
        return;
    };
    let requested_height = app.plugin_views[index]
        .declaration
        .size
        .height
        .unwrap_or(3)
        .max(3)
        .min(workspace.height.saturating_sub(1));
    let area = Rect::new(
        workspace.x,
        workspace
            .y
            .saturating_add(workspace.height.saturating_sub(requested_height + 1)),
        workspace.width,
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
        style = style.fg(plugin_color(color));
    }
    if let Some(color) = plugin_style.background {
        style = style.bg(plugin_color(color));
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

/// 将插件便携颜色映射到 Lucia 色板。
///
/// 刻意不直接落到终端 16 色：`Color::DarkGray` 之类的原始 ANSI 值在深色主题下比
/// 应用自身的次要色暗得多，插件面板会明显比原生面板"发灰"，同一个界面里出现两套
/// 观感。这里把插件的语义色收敛到与原生组件相同的色板，插件只需表达"这是次要文字"
/// 或"这是危险状态"，具体呈现由宿主主题统一决定。
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

    /// 左右停靠视图应为全局底栏保留一行，使边框底部与主输入框对齐。
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
}
