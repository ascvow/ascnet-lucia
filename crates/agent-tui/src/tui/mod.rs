//! Lucia TUI 的组件、视图状态和插件 UI 桥接。

mod chat;
mod root;

#[cfg(feature = "plugins")]
pub(crate) mod plugin_bridge;
#[cfg(feature = "plugins")]
pub(crate) mod plugin_render;
#[cfg(feature = "plugins")]
pub(crate) mod view;

#[cfg(test)]
pub(crate) use chat::context_status;
pub(crate) use chat::render_main;
#[cfg(feature = "plugins")]
pub(crate) use plugin_bridge::{
    apply_plugin_frames, apply_plugin_navigation_event, dispatch_plugin_input,
    drain_plugin_ui_events, refresh_plugin_view, refresh_plugin_views_for, render_plugin_views,
    PluginRenderResult,
};
#[cfg(feature = "plugins")]
pub(crate) use plugin_render::{
    plugin_frame_lines, render_composer_panels, render_docked_plugin_views, render_plugin_dialog,
    render_plugin_input, render_plugin_subview,
};
pub(crate) use root::render_root;
