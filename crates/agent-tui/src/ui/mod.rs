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

/// 在交给终端后端前清除 Cell 文本中的控制字符，阻断模型或插件注入 ESC/OSC。
/// 仅修改显示缓冲，不改会话、模型内容或字素中的 ZWJ/组合符/变体选择符。
pub(crate) fn sanitize_terminal_buffer(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol().chars().any(char::is_control) {
            let text: String = cell
                .symbol()
                .chars()
                .filter(|ch| !ch.is_control())
                .collect();
            cell.set_symbol(if text.is_empty() { " " } else { &text });
        }
    }
}

/// 将日志中的终端命令降为纯文本，必须在测量、截断与 diff 分类之前调用。
///
/// 只清洗显示副本：CSI/OSC/DCS 等序列（包括未收完的尾部）整段丢弃，不解释光标、
/// 剪贴板或颜色指令；CRLF 归一为换行，单独 CR 作为日志行边界，不模拟终端覆盖。
/// 制表符留给行号识别与代码缩进处理，Unicode 组合符与 ZWJ 原样保留。
pub(crate) fn plain_terminal_text(text: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let command = match ch {
            '\x1b' => match chars.next() {
                Some(next) => next,
                None => break,
            },
            '\u{009b}' => '[',
            '\u{009d}' => ']',
            '\u{0090}' => 'P',
            '\u{0098}' => 'X',
            '\u{009e}' => '^',
            '\u{009f}' => '_',
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
                continue;
            }
            '\n' | '\t' => {
                out.push(ch);
                continue;
            }
            ch if ch.is_control() => continue,
            ch => {
                out.push(ch);
                continue;
            }
        };
        match command {
            '[' => {
                for next in chars.by_ref() {
                    if ('\u{0040}'..='\u{007e}').contains(&next) {
                        break;
                    }
                }
            }
            ']' | 'P' | 'X' | '^' | '_' => {
                while let Some(next) = chars.next() {
                    if next == '\u{009c}' || (command == ']' && next == '\x07') {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            '\u{0020}'..='\u{002f}' => {
                for next in chars.by_ref() {
                    if !('\u{0020}'..='\u{002f}').contains(&next) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Cow::Owned(out)
}

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

    /// 实际 Paragraph 单元格不得携带终端控制符；Unicode 字素保持原逻辑文本。
    #[test]
    fn terminal_controls_do_not_reach_rendered_cells() {
        use crate::session::conversation::{Msg, MsgKind};
        use ratatui::prelude::*;
        let payload = "中 e\u{301} 👩\u{200d}💻 \x1b[2J \x1b]52;c;dGVzdA==\x07 \u{009b}2J end";
        for kind in [
            MsgKind::User,
            MsgKind::Assistant,
            MsgKind::Info,
            MsgKind::Error,
            MsgKind::Extension,
        ] {
            let message = Msg::new(kind, payload);
            let area = Rect::new(0, 0, 120, 20);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(message.to_lines(false, 120))
                .wrap(Wrap { trim: false })
                .render(area, &mut buffer);
            sanitize_terminal_buffer(&mut buffer);
            assert_eq!(message.text, payload, "只清洗显示，不改变原始消息");
            assert!(buffer
                .content
                .iter()
                .all(|cell| !cell.symbol().chars().any(char::is_control)));
        }
    }

    /// 显示清洗必须保留组合符、ZWJ、变体选择符和 RTL 逻辑文本。
    #[test]
    fn terminal_sanitizer_preserves_unicode_graphemes() {
        use ratatui::prelude::*;
        let area = Rect::new(0, 0, 120, 4);
        let mut buffer = Buffer::empty(area);
        let text = "中 e\u{301} 👩\u{200d}💻 ✈\u{fe0f} 🇨🇳 العربية";
        Paragraph::new(text).render(area, &mut buffer);
        let before = buffer.clone();
        sanitize_terminal_buffer(&mut buffer);
        assert_eq!(buffer, before);
        buffer[(0, 0)].set_symbol("\x1b]52;test\x07");
        sanitize_terminal_buffer(&mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "]52;test");
    }

    /// 带颜色的诊断在排版前完整去掉控制序列，未完成序列不泄漏可打印残片。
    #[test]
    fn terminal_log_sequences_are_removed_before_layout() {
        let text = "\x1b[36m788 |\x1b[0m code\r\n\x1b[33mwarning\x1b[0m\n";
        assert_eq!(plain_terminal_text(text), "788 | code\nwarning\n");
        assert_eq!(plain_terminal_text("old\rnew\n"), "old\nnew\n");
        assert_eq!(
            plain_terminal_text("a\x1b]52;c;secret\x07b\x1bPprivate\x1b\\c"),
            "abc"
        );
        assert_eq!(
            plain_terminal_text("a\u{009b}31mb\u{009d}secret\u{009c}c"),
            "abc"
        );
        for tail in [
            "\x1b",
            "\x1b[38;2;",
            "\x1b]8;;unfinished",
            "\x1bPunfinished",
        ] {
            assert_eq!(plain_terminal_text(&format!("正文{tail}")), "正文");
        }
        let unicode = "中 e\u{301} 👩\u{200d}💻 ✈\u{fe0f} العربية\tend";
        assert_eq!(plain_terminal_text(unicode), unicode);
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
