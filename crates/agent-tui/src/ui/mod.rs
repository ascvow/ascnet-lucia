//! Lucia TUI 的组件、视图状态和插件 UI 桥接。

pub(crate) mod chat;
pub(crate) mod diff;
pub(crate) mod highlight;
pub(crate) mod markdown;
pub(crate) mod root;
pub(crate) mod transcript;
pub(crate) mod transcript_layout;

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

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

/// 使用 Ratatui 自身的字素及换行算法计算单个逻辑行的视觉行数。
///
/// `max_width` 为零时返回零；其他宽度至少返回一行。该结果用于把滚动上限绑定到
/// 实际渲染内容，避免窄终端中的长字段被截断或滚动到空白区域。
pub(crate) fn wrapped_line_count(line: &Line<'_>, max_width: u16) -> u16 {
    u16::try_from(
        Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(max_width),
    )
    .unwrap_or(u16::MAX)
}
