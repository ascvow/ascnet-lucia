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
use crate::command::render_native_session_dialog;
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
        app.scrollbar_drag = false;
        render_plugin_subview(frame, app, outer);
        return;
    }
    #[cfg(feature = "plugins")]
    let workspace = render_docked_plugin_views(frame, app, outer);
    #[cfg(not(feature = "plugins"))]
    let workspace = outer;
    render_main(frame, app, workspace);
    #[cfg(feature = "plugins")]
    render_plugin_input(frame, app, workspace);
    #[cfg(feature = "plugins")]
    render_plugin_dialog(frame, app, outer);
    render_native_session_dialog(frame, app, outer);
    crate::command::render_context_dialog(frame, app, outer);
    #[cfg(feature = "plugins")]
    render_native_plugin_dialog(frame, app, outer);
}
