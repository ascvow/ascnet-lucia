//! Lucia TUI 的组件、视图状态和插件 UI 桥接。

pub(crate) mod chat;
pub(crate) mod diff;
pub(crate) mod highlight;
pub(crate) mod markdown;
pub(crate) mod root;
pub(crate) mod sheet;
pub(crate) mod transcript;
pub(crate) mod transcript_layout;

#[cfg(feature = "plugins")]
pub(crate) mod agent_view;
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

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

/// 把一条带样式的逻辑行按显示宽度折成多条视觉行。
///
/// 折行优先落在空格处，只有单词长于一整行时才按字符硬断；宽度按 Unicode 显示宽度计算，
/// 因此 CJK 与全角字符不会错位。返回值中的 `Vec<usize>` 记录每条视觉行首字符在该逻辑行
/// 中的字符索引，调用方据此把光标索引映射成视觉坐标；`starts` 与返回的 `Vec<Line>` 等长，
/// 且长度至少为 1。
///
/// `width` 为零时按 1 处理，避免除零并在极窄终端下退化为逐字符折行。
pub(crate) fn wrap_spans(spans: &[Span<'_>], width: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    // 展开成字符与样式：折行只关心显示宽度，渲染时再把同样式字符合并回 Span。
    let cells: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(move |character| (character, span.style))
        })
        .collect();
    let width = width.max(1);
    let cell_width = |(character, _): &(char, Style)| {
        unicode_width::UnicodeWidthChar::width(*character).unwrap_or(0)
    };

    let mut starts = vec![0usize];
    let mut row_start = 0usize;
    let mut row_width = 0usize;
    let mut index = 0usize;
    while index < cells.len() {
        let character_width = cell_width(&cells[index]);
        if row_width > 0 && row_width + character_width > width {
            // 优先断在最后一个空格，新行跳过紧随其后的连续空格，避免视觉行以空白开头。
            let mut next = cells[row_start..index]
                .iter()
                .rposition(|cell| cell.0 == ' ')
                .map(|offset| row_start + offset + 1)
                .filter(|next| *next > row_start && *next < index)
                .unwrap_or(index);
            while next < index && cells[next].0 == ' ' {
                next += 1;
            }
            starts.push(next);
            row_start = next;
            row_width = cells[row_start..index].iter().map(cell_width).sum();
            continue;
        }
        row_width += character_width;
        index += 1;
    }

    let lines = starts
        .iter()
        .enumerate()
        .map(|(row, &start)| {
            let mut end = starts.get(row + 1).copied().unwrap_or(cells.len());
            // 折行处留下的空格只用于占位，行尾空白不渲染：否则上一行会以空格结尾，
            // 看着像多出一列，也会让等宽对照出错。
            while end > start && cells[end - 1].0 == ' ' {
                end -= 1;
            }
            let mut spans_out: Vec<Span<'static>> = Vec::new();
            for &(character, style) in &cells[start..end] {
                match spans_out.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(character),
                    _ => spans_out.push(Span::styled(character.to_string(), style)),
                }
            }
            Line::from(spans_out)
        })
        .collect();
    (lines, starts)
}

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

#[cfg(test)]
mod wrap_tests {
    use super::*;
    use ratatui::style::Modifier;

    /// 把一条普通文本折行后按行取出可见内容，便于断言折行位置。
    fn rows_of(text: &str, width: usize) -> Vec<String> {
        let spans = [Span::raw(text.to_string())];
        wrap_spans(&spans, width)
            .0
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// 折行优先落在空格处，不把单词切成两半。
    #[test]
    fn wrap_prefers_word_boundaries() {
        assert_eq!(rows_of("hello world", 7), vec!["hello", "world"]);
        // 断行后新行不以空格开头。
        assert_eq!(rows_of("aaa bbb", 4), vec!["aaa", "bbb"]);
    }

    /// 单词长于一整行时才按字符硬断，避免整行被挤出一个字符。
    #[test]
    fn wrap_hard_breaks_words_longer_than_a_row() {
        assert_eq!(rows_of("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    /// 宽度按显示宽度计算，因此 CJK 字符占两列且不会被切开。
    #[test]
    fn wrap_counts_cjk_as_two_columns() {
        assert_eq!(rows_of("中文中文", 4), vec!["中文", "中文"]);
        // 中英混排时按累计显示宽度折行。
        assert_eq!(rows_of("中ab文", 4), vec!["中ab", "文"]);
    }

    /// 空行与零宽度输入都要返回至少一行的视觉行，供输入框撑起光标。
    #[test]
    fn wrap_always_returns_one_row() {
        assert_eq!(rows_of("", 10), vec![""]);
        assert_eq!(rows_of("abc", 0), vec!["a", "b", "c"]);
    }

    /// 返回的行首索引与视觉行一一对应，调用方据此定位光标。
    #[test]
    fn wrap_reports_row_start_indices() {
        let spans = [Span::raw("hello world")];
        let (lines, starts) = wrap_spans(&spans, 7);
        assert_eq!(lines.len(), starts.len());
        assert_eq!(starts, vec![0, 6]);
    }

    /// 折行不能丢样式：同一视觉行里连续同样式的字符合并回一个 Span。
    #[test]
    fn wrap_preserves_styles_across_rows() {
        let spans = [
            Span::styled("abc", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("def", Style::new().add_modifier(Modifier::ITALIC)),
        ];
        let (lines, _) = wrap_spans(&spans, 4);
        let rendered: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(rendered, vec!["abcd", "ef"]);
        // 第二行首个 Span 必须沿用被折开字符原来的斜体样式。
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC),
            "折行后样式丢失"
        );
    }
}
