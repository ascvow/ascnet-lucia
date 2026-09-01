//! Lucia TUI 的组件、视图状态和插件 UI 桥接。

pub(crate) mod chat;
pub(crate) mod highlight;
pub(crate) mod markdown;
pub(crate) mod root;
pub(crate) mod transcript;

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
use std::collections::VecDeque;

/// 按 Ratatui `Wrap { trim: false }` 的词边界行为计算单个逻辑行的视觉行数。
///
/// `max_width` 为零时返回零；其他宽度至少返回一行。该结果用于把滚动上限绑定到
/// 实际渲染内容，避免窄终端中的长字段被截断或滚动到空白区域。
pub(crate) fn wrapped_line_count(line: &Line<'_>, max_width: u16) -> u16 {
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
