//! Root view composition for Lucia TUI.
//! Lucia TUI 的根视图编排组件。

use super::render_main;
#[cfg(feature = "plugins")]
use super::{
    render_docked_plugin_views, render_plugin_dialog, render_plugin_input, render_plugin_subview,
};
use crate::app::App;
#[cfg(feature = "plugins")]
use crate::command::render_native_plugin_dialog;
#[cfg(feature = "plugins")]
use crate::ui::agent_view::render_agent_session_view;
use ratatui::prelude::*;

/// Selects and renders the main view, plugin slots, dialogs, or the active subview.
/// 选择并渲染主视图、插件插槽、对话框或当前激活的子视图。
pub(crate) fn render_root(frame: &mut Frame, app: &mut App) {
    #[cfg(feature = "plugins")]
    for view in &mut app.plugin_views {
        view.area = Rect::default();
    }
    // 直接占满整屏：留白交给各区域自己的内边距，正文与底栏靠 horizontal padding 对齐。
    let outer = frame.area();
    #[cfg(feature = "plugins")]
    if !app.view_stack.is_main() {
        // 离开主视图后清空主界面热区，避免点击被上一帧的滚动条或跳转提示拦截。
        app.jump_hint_area = None;
        app.scrollbar_area = None;
        app.scrollbar_grab_half = None;
        app.scrollbar_drag = false;
        app.is_scrollbar_hovered = false;
        // 底栏不会在子视图里绘制，它的上下文芯片热区同样必须失效。
        app.context_chip_areas.clear();
        app.is_context_chip_hovered = false;
        if app.active_agent_session_view().is_some() {
            render_agent_session_view(frame, app, outer);
        } else {
            render_plugin_subview(frame, app, outer);
        }
        return;
    }
    #[cfg(feature = "plugins")]
    let workspace = render_docked_plugin_views(frame, app, outer);
    #[cfg(not(feature = "plugins"))]
    let workspace = outer;
    // 输入盒几何是浮层 Dialog 的锚点：它们与命令面板共用同一条底边。
    let composer = render_main(frame, app, workspace);
    #[cfg(feature = "plugins")]
    render_plugin_input(frame, app, workspace, composer);
    #[cfg(feature = "plugins")]
    render_plugin_dialog(frame, app, composer);
    crate::command::render_context_dialog(frame, app, composer);
    #[cfg(feature = "plugins")]
    render_native_plugin_dialog(frame, app, composer);
}
