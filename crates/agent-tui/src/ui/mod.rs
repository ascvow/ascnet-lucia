//! Lucia TUI 的组件、视图状态和插件 UI 桥接。

pub(crate) mod chat;
pub(crate) mod markdown;
pub(crate) mod root;

#[cfg(feature = "plugins")]
pub(crate) mod plugin_bridge;
#[cfg(feature = "plugins")]
pub(crate) mod plugin_render;
#[cfg(feature = "plugins")]
pub(crate) mod view;

pub(crate) use chat::render_main;
#[cfg(feature = "plugins")]
pub(crate) use plugin_bridge::PluginRenderResult;
#[cfg(feature = "plugins")]
pub(crate) use plugin_render::{
    plugin_frame_lines, render_docked_plugin_views, render_plugin_dialog, render_plugin_input,
    render_plugin_subview,
};
