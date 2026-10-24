//! 主事件区的消息布局缓存与可见窗口渲染。
//!
//! 全屏渲染的核心约束是"只渲染可见消息"：对话再长，每帧真正排版的内容也只覆盖当前
//! 视口。这里把两件事分开：
//!
//! 1. **测量**：每条消息按当前宽度换行后占多少视觉行，结果按内容指纹缓存，内容不变
//!    就不重算。测量时构建的行只在消息可见期间保留，滚出窗口立即释放，因此内存占用
//!    与对话长度基本无关。
//! 2. **绘制**：每帧只重建与视口相交的那几条消息，并按各自的行偏移落到对应矩形，
//!    避免把整段历史拼成一个大 `Paragraph` 再整体滚动。

use crate::session::conversation::{Msg, MsgKind};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

/// FNV-1a 64 位初始值。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 位质数。
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 单条消息的布局结果。
struct LayoutEntry {
    /// 测量时使用的消息区宽度；宽度变化后换行结果失效。
    width: u16,
    /// 内容指纹；与当前消息不一致时重新测量。
    fingerprint: u64,
    /// 换行后的视觉行数，逻辑高度不能使用终端的 u16 坐标。
    height: usize,
    /// 展开当前工具消息是否能多看到内容；点击热区只对这类消息开放。
    expandable: bool,
    /// 可见窗口内的逻辑行累计视觉终点，滚出窗口后释放。
    row_ends: Vec<usize>,
    /// 可见窗口内保留的渲染结果；滚出窗口后释放。
    lines: Option<Vec<Line<'static>>>,
}

impl LayoutEntry {
    /// 空条目；首次同步时必然重新测量。
    fn empty() -> Self {
        Self {
            width: 0,
            fingerprint: 0,
            height: 0,
            expandable: false,
            row_ends: Vec::new(),
            lines: None,
        }
    }

    /// 判断缓存是否可以复用于当前消息。
    fn matches(&self, fingerprint: u64, width: u16) -> bool {
        self.width == width && self.fingerprint == fingerprint
    }
}

/// 与消息列表按索引对齐的布局缓存。
#[derive(Default)]
pub(crate) struct TranscriptLayout {
    /// 上一次测量使用的宽度；整体变化时逐条重新测量。
    width: u16,
    /// 与消息列表等长的布局条目。
    entries: Vec<LayoutEntry>,
}

impl TranscriptLayout {
    /// 同步布局到当前消息列表，返回全部消息的视觉行数之和。
    ///
    /// 只有宽度或内容指纹变化的条目会重新排版；其余条目直接复用缓存的高度。
    pub(crate) fn sync(&mut self, messages: &[Msg], streaming: Option<usize>, width: u16) -> usize {
        let width = width.max(1);
        if self.width != width {
            // 宽度变化会让全部换行结果失效，逐条重新测量。
            self.width = width;
            self.entries.clear();
        }
        if self.entries.len() != messages.len() {
            // 数量变化时按索引重新对齐；仍然匹配的前缀在下面的循环里继续复用。
            self.entries.truncate(messages.len());
            self.entries.resize_with(messages.len(), LayoutEntry::empty);
        }

        let mut total = 0usize;
        for (index, message) in messages.iter().enumerate() {
            let is_streaming = streaming == Some(index);
            let fingerprint = fingerprint(message, is_streaming);
            let entry = &mut self.entries[index];
            if !entry.matches(fingerprint, width) {
                let lines = message_lines(messages, index, is_streaming, width);
                entry.width = width;
                entry.fingerprint = fingerprint;
                entry.row_ends = line_row_ends(&lines, width);
                entry.height = entry.row_ends.last().copied().unwrap_or(0);
                entry.expandable = message.is_tool_expandable();
                entry.lines = Some(lines);
            }
            total = total.saturating_add(entry.height);
        }
        total
    }

    /// 渲染与视口相交的消息，并释放滚出窗口的缓存行。
    ///
    /// `scroll` 是相对全部消息顶部的视觉行偏移；调用方负责把滚动位置收窄到
    /// `[0, 总行数 - 视口高度]`。
    pub(crate) fn render_visible(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        messages: &[Msg],
        streaming: Option<usize>,
        scroll: usize,
    ) {
        if area.height == 0 || area.width == 0 {
            for entry in &mut self.entries {
                entry.lines = None;
                entry.row_ends = Vec::new();
            }
            return;
        }
        let viewport_bottom = scroll.saturating_add(usize::from(area.height));
        let mut cursor = 0usize;

        for (index, entry) in self.entries.iter_mut().enumerate() {
            let height = entry.height;
            let start = cursor;
            cursor = cursor.saturating_add(height);
            let visible = height > 0 && cursor > scroll && start < viewport_bottom;
            if !visible {
                // 视口外的消息只需要高度，行内容立即释放。
                entry.lines = None;
                entry.row_ends = Vec::new();
                continue;
            }
            // 消息列表比布局条目短时跳过：没有内容可画，也不该继续推进光标。
            if messages.get(index).is_none() {
                continue;
            }
            let width = entry.width;
            if entry.lines.is_none() {
                let lines = message_lines(messages, index, streaming == Some(index), width);
                entry.row_ends = line_row_ends(&lines, width);
                entry.lines = Some(lines);
            }
            let lines = entry.lines.as_ref().expect("可见行已构建");

            let clipped_top = start.max(scroll);
            let clipped_bottom = cursor.min(viewport_bottom);
            let target = Rect {
                x: area.x,
                y: area.y + u16::try_from(clipped_top - scroll).unwrap_or(0),
                width: area.width,
                height: u16::try_from(clipped_bottom - clipped_top).unwrap_or(0),
            };
            render_lines_window(
                lines,
                &entry.row_ends,
                target,
                clipped_top - start,
                frame.buffer_mut(),
            );
        }
    }

    /// 把内容行区间 `[top, top + height)` 渲染到一个独立缓冲区，供跨屏选区提取纯文本。
    ///
    /// 选区存在内容坐标里，因此滚出屏幕的历史也要能被复制；这里按内容行号重新排版
    /// 选中的那几行；缓冲区始终以零为纵向原点，调用方用 `top` 映射逻辑行号。
    /// 视口外的消息按需重建行、但不写回缓存——缓存的"只保留可见窗口"策略不受影响。
    ///
    /// 助手卡片的底色不铺：提取纯文本只关心字符，多画一层底色没有意义。
    pub(crate) fn render_range(
        &mut self,
        area: Rect,
        messages: &[Msg],
        streaming: Option<usize>,
        top: usize,
        height: u16,
    ) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(area.x, 0, area.width, height));
        if area.width == 0 || height == 0 {
            return buffer;
        }
        let range_top = top;
        let range_bottom = top.saturating_add(usize::from(height));
        let mut cursor = 0usize;

        for (index, entry) in self.entries.iter_mut().enumerate() {
            let entry_height = entry.height;
            let start = cursor;
            cursor = cursor.saturating_add(entry_height);
            if entry_height == 0 || cursor <= range_top || start >= range_bottom {
                entry.lines = None;
                entry.row_ends = Vec::new();
                continue;
            }
            // 布局条目比消息列表长时跳过：没有内容可画。
            if messages.get(index).is_none() {
                continue;
            }
            let width = entry.width;
            if entry.lines.is_none() {
                let lines = message_lines(messages, index, streaming == Some(index), width);
                entry.row_ends = line_row_ends(&lines, width);
                entry.lines = Some(lines);
            }
            let lines = entry.lines.as_ref().expect("页内消息已构建");
            let clipped_top = start.max(range_top);
            let clipped_bottom = cursor.min(range_bottom);
            let target = Rect {
                x: area.x,
                y: u16::try_from(clipped_top - top).unwrap_or(0),
                width: area.width,
                height: u16::try_from(clipped_bottom.saturating_sub(clipped_top)).unwrap_or(0),
            };
            if target.height == 0 {
                continue;
            }
            render_lines_window(
                lines,
                &entry.row_ends,
                target,
                clipped_top - start,
                &mut buffer,
            );
        }
        buffer
    }

    /// 返回可点击工具消息的 `(消息索引, 起始行, 高度)`；起始行相对全部消息顶部。
    pub(crate) fn tool_rows(&self) -> Vec<(usize, usize, usize)> {
        let mut rows = Vec::new();
        let mut start = 0usize;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.expandable && entry.height > 0 {
                rows.push((index, start, entry.height));
            }
            start = start.saturating_add(entry.height);
        }
        rows
    }
}

/// 构造一条消息的渲染行，并处理消息之间的段落空行。
///
/// 每条工具消息之后都补一行空行：连续的调用各自带一段输出，紧贴着排会让人分不清
/// 哪几行属于哪一次调用；空行计入该条消息的高度，因此滚动与点击热区都能保持一致。
fn message_lines(
    messages: &[Msg],
    index: usize,
    streaming: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let message = &messages[index];
    // 极端单逻辑行也需先变成有界视觉行，Paragraph 自身的 scroll 只有 u16。
    let mut lines = normalize_long_lines(to_owned_lines(message.to_lines(streaming, width)), width);
    if message.is_tool() {
        lines.push(Line::default());
    }
    lines
}

/// 只有超过 Paragraph 行内偏移范围的极端逻辑行才按字素硬折行；普通换行语义不变。
/// 使用 Ratatui 自身的字素迭代，保留宽字符、组合音标和 ZWJ 序列。
fn normalize_long_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    for line in lines {
        if line
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>()
            <= usize::from(u16::MAX)
        {
            result.push(line);
            continue;
        }
        let height = Paragraph::new(borrowed_lines(std::slice::from_ref(&line)))
            .wrap(Wrap { trim: false })
            .line_count(width);
        if height <= usize::from(u16::MAX) {
            result.push(line);
            continue;
        }
        let mut row = Vec::new();
        let mut columns = 0;
        for grapheme in line.styled_graphemes(Style::default()) {
            let cells = unicode_width::UnicodeWidthStr::width(grapheme.symbol);
            if columns + cells > usize::from(width) && !row.is_empty() {
                result.push(Line {
                    spans: std::mem::take(&mut row),
                    style: line.style,
                    alignment: line.alignment,
                });
                columns = 0;
            }
            row.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            columns += cells;
        }
        if !row.is_empty() {
            result.push(Line {
                spans: row,
                style: line.style,
                alignment: line.alignment,
            });
        }
    }
    result
}

/// 缓存每条逻辑行的视觉终点；只在内容或宽度变化时测量。
fn line_row_ends(lines: &[Line<'_>], width: u16) -> Vec<usize> {
    let mut total = 0usize;
    lines
        .iter()
        .map(|line| {
            total = total.saturating_add(
                Paragraph::new(borrowed_lines(std::slice::from_ref(line)))
                    .wrap(Wrap { trim: false })
                    .line_count(width),
            );
            total
        })
        .collect()
}

/// 二分跳过离屏逻辑行，再让 Paragraph 只排可见段，行内偏移已由正规化保证可表达。
fn render_lines_window(
    lines: &[Line<'_>],
    row_ends: &[usize],
    target: Rect,
    skip: usize,
    buffer: &mut Buffer,
) {
    let first = row_ends.partition_point(|end| *end <= skip);
    if first == lines.len() {
        return;
    }
    let end_row = skip.saturating_add(usize::from(target.height));
    let end = (row_ends.partition_point(|end| *end < end_row) + 1).min(lines.len());
    let previous = first.checked_sub(1).map_or(0, |index| row_ends[index]);
    Paragraph::new(borrowed_lines(&lines[first..end]))
        .wrap(Wrap { trim: false })
        .scroll((
            u16::try_from(skip - previous).expect("超长逻辑行已正规化"),
            0,
        ))
        .render(target, buffer);
}

/// 为 Paragraph 分配行描述但借用正文，保留样式/对齐，不跨帧保存借用。
fn borrowed_lines<'a>(lines: &'a [Line<'_>]) -> Vec<Line<'a>> {
    lines
        .iter()
        .map(|line| Line {
            spans: line
                .spans
                .iter()
                .map(|span| Span::styled(span.content.as_ref(), span.style))
                .collect(),
            style: line.style,
            alignment: line.alignment,
        })
        .collect()
}

/// 把借用消息文本的行转换为可独立保存的行。
fn to_owned_lines(lines: Vec<Line<'_>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| Line {
            spans: line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect(),
            style: line.style,
            alignment: line.alignment,
        })
        .collect()
}

/// 计算消息的内容指纹。
///
/// 指纹覆盖所有会影响排版的字段：种类、文本、流式标记、工具增量输出、展开状态和插件
/// 帧版本。工具结果在消息生命周期内只写入一次，且写入时 `kind` 同步变化，因此不必
/// 逐帧序列化 JSON 载荷。`accent` 与 `divider` 只在创建时确定，同样不参与指纹。
fn fingerprint(message: &Msg, streaming: bool) -> u64 {
    let mut hash = FNV_OFFSET;
    hash = write_byte(hash, kind_tag(&message.kind));
    hash = write_byte(hash, u8::from(streaming));
    hash = write_byte(hash, u8::from(message.tool_expanded));
    hash = write_bytes(hash, message.text.as_bytes());
    hash = write_bytes(hash, message.tool_output.as_bytes());
    if let Some(reason) = message.skip_reason.as_deref() {
        hash = write_bytes(hash, reason.as_bytes());
    }
    if let Some(result) = message.tool_result.as_ref() {
        hash = write_byte(hash, u8::from(result.is_error));
        hash = write_bytes(hash, result.name.as_bytes());
    }
    #[cfg(feature = "plugins")]
    {
        hash = write_bytes(hash, &message.tool_render_revision.to_le_bytes());
        hash = write_byte(hash, u8::from(message.tool_frame.is_some()));
        hash = write_bytes(
            hash,
            &message.tool_frame_width.unwrap_or_default().to_le_bytes(),
        );
    }
    hash
}

/// 返回消息种类的稳定标记，用于指纹。
fn kind_tag(kind: &MsgKind) -> u8 {
    match kind {
        MsgKind::User => 0,
        MsgKind::Assistant => 1,
        MsgKind::ToolRunning => 2,
        MsgKind::ToolOk => 3,
        MsgKind::ToolError => 4,
        MsgKind::ToolSkipped => 5,
        MsgKind::Error => 6,
        MsgKind::Info => 7,
        MsgKind::Extension => 8,
    }
}

/// 把单个字节混入指纹。
fn write_byte(mut hash: u64, byte: u8) -> u64 {
    hash ^= u64::from(byte);
    hash.wrapping_mul(FNV_PRIME)
}

/// 把字节序列混入指纹。
///
/// 每次吃掉 8 字节再混合，而不是逐字节乘一遍：长对话每帧都要重算全部消息的指纹，
/// 逐字节版本会稳定吃掉整个帧预算的大部分（1200 条消息约 0.75ms），按机器字混合
/// 后同样的输入降到十分之一量级。指纹只用于判断"内容是否变化"，不做安全用途。
fn write_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    hash = write_bytes_len(hash, bytes.len());
    let (words, tail) = bytes.as_chunks::<8>();
    for word in words {
        hash = (hash ^ u64::from_le_bytes(*word)).wrapping_mul(FNV_PRIME);
    }
    for byte in tail {
        hash = write_byte(hash, *byte);
    }
    hash
}

/// 把长度混入指纹，避免拼接不同字段时产生歧义。
fn write_bytes_len(mut hash: u64, len: usize) -> u64 {
    for byte in len.to_le_bytes() {
        hash = write_byte(hash, byte);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::conversation::{Msg, MsgKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 构造若干单行用户消息，行数完全由消息条数决定。
    fn messages(count: usize) -> Vec<Msg> {
        (0..count)
            .map(|index| Msg::new(MsgKind::User, format!("message {index}")))
            .collect()
    }

    /// 渲染一帧，返回布局在该帧保留的可见消息数量。
    fn render(
        layout: &mut TranscriptLayout,
        messages: &[Msg],
        height: u16,
        scroll: usize,
    ) -> usize {
        let mut terminal = Terminal::new(TestBackend::new(40, height)).expect("创建布局测试终端");
        terminal
            .draw(|frame| {
                let area = frame.area();
                layout.render_visible(frame, area, messages, None, scroll);
            })
            .expect("渲染布局测试帧");
        layout
            .entries
            .iter()
            .filter(|entry| entry.lines.is_some())
            .count()
    }

    /// 借用文本的 Paragraph 保持 Unicode、样式、对齐与实际绘制结果。
    #[test]
    fn borrowed_paragraph_matches_owned_render() {
        let lines = vec![Line::from(Span::styled(
            "中文 e\u{301} 👩\u{200d}💻".to_string(),
            Style::default().fg(Color::Green),
        ))
        .alignment(Alignment::Right)];
        let borrowed = borrowed_lines(&lines);
        assert!(std::ptr::eq(
            lines[0].spans[0].content.as_ref(),
            borrowed[0].spans[0].content.as_ref()
        ));
        for width in [1, 7, 40] {
            let area = Rect::new(0, 0, width, 10);
            let mut expected = Buffer::empty(area);
            let mut actual = Buffer::empty(area);
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .render(area, &mut expected);
            Paragraph::new(borrowed_lines(&lines))
                .wrap(Wrap { trim: false })
                .render(area, &mut actual);
            assert_eq!(expected, actual);
        }
    }

    /// 同长历史编辑仍必须使指纹失效，不能以指针或长度代替内容判定。
    #[test]
    fn same_length_history_edit_invalidates_layout() {
        let mut layout = TranscriptLayout::default();
        let mut messages = messages(20);
        layout.sync(&messages, None, 40);
        let previous = layout.entries[0].fingerprint;
        messages[0].text.replace_range(..1, "M");
        layout.sync(&messages, None, 40);
        assert_ne!(layout.entries[0].fingerprint, previous);
    }

    /// 单条超长消息及跨消息历史都能到达 65535 行之后，离屏页使用局部原点。
    #[test]
    fn long_timeline_renders_tail_without_saturating() {
        let body = (0..70_010)
            .map(|index| format!("row-{index}\n"))
            .collect::<String>();
        let messages = vec![
            Msg::new(MsgKind::User, body),
            Msg::new(MsgKind::Info, "FINAL-MARKER"),
        ];
        let mut layout = TranscriptLayout::default();
        let total = layout.sync(&messages, None, 40);
        assert!(total > 70_010);
        let page = layout.render_range(Rect::new(0, 0, 40, 8), &messages, None, total - 8, 8);
        assert_eq!(page.area.y, 0);
        let text = page
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("FINAL-MARKER"), "{text}");
        assert!(text.contains("row-70009"));
        render(&mut layout, &messages, 8, total - 8);
    }

    /// 极长单行超过 Paragraph 的行内滚动范围时仍能显示最后的标记。
    #[test]
    fn huge_single_line_keeps_tail_reachable() {
        let text = format!("{}Z", "x".repeat(70_000));
        let lines = normalize_long_lines(vec![Line::from(text)], 1);
        let ends = line_row_ends(&lines, 1);
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        render_lines_window(&lines, &ends, area, 70_000, &mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "Z");
    }

    /// 视口之外的消息只保留高度，渲染结果随滚动释放，内存不随对话增长。
    #[test]
    fn releases_rendered_lines_outside_the_viewport() {
        let mut layout = TranscriptLayout::default();
        let messages = messages(60);
        let total = layout.sync(&messages, None, 40);
        assert!(total >= 60, "每条消息至少占一行");

        let retained = render(&mut layout, &messages, 10, 0);
        assert!(retained > 0, "视口顶部必须有消息可见");
        assert!(retained < messages.len(), "视口之外的消息不应保留行内容");
        assert!(
            layout.entries.last().expect("应有布局条目").lines.is_none(),
            "末尾消息在滚动到顶部时必须已释放"
        );
    }

    /// 内容未变化的消息复用缓存高度，只有变化的消息重新排版。
    #[test]
    fn remeasures_only_changed_messages() {
        let mut layout = TranscriptLayout::default();
        let mut messages = messages(6);
        let total = layout.sync(&messages, None, 40);
        // 只让最后一行可见，其余条目在渲染时释放缓存行。
        render(&mut layout, &messages, 4, total.saturating_sub(1));
        assert!(layout.entries[0].lines.is_none());

        let before = layout.entries[2].height;
        messages[2]
            .text
            .push_str(" with a much longer tail that must wrap");
        layout.sync(&messages, None, 40);

        assert!(
            layout.entries[0].lines.is_none(),
            "未变化的消息不应重新排版"
        );
        assert!(
            layout.entries[2].lines.is_some(),
            "内容变化的消息应重新排版"
        );
        assert!(layout.entries[2].height >= before);
    }

    /// 宽度变化会让全部换行结果失效，必须整体重新测量。
    #[test]
    fn remeasures_every_message_after_width_change() {
        let mut layout = TranscriptLayout::default();
        let messages = messages(4);
        let total = layout.sync(&messages, None, 40);
        render(&mut layout, &messages, 2, total.saturating_sub(1));
        assert!(layout.entries[0].lines.is_none());

        layout.sync(&messages, None, 20);

        assert!(
            layout.entries[0].lines.is_some(),
            "宽度变化后每条消息都必须重新测量"
        );
    }

    /// 消息被整体替换时按索引重新对齐，条目数量始终与消息数量一致。
    #[test]
    fn realigns_entries_after_message_list_shrinks() {
        let mut layout = TranscriptLayout::default();
        let messages = messages(5);
        layout.sync(&messages, None, 40);
        assert_eq!(layout.entries.len(), 5);

        layout.sync(&messages[..2], None, 40);

        assert_eq!(layout.entries.len(), 2);
    }
}
