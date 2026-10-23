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
    /// 换行后的视觉行数。
    height: u16,
    /// 展开当前工具消息是否能多看到内容；点击热区只对这类消息开放。
    expandable: bool,
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
            lines: None,
        }
    }

    /// 判断缓存是否可以复用于当前消息。
    fn matches(&self, message: &Msg, streaming: bool, width: u16) -> bool {
        self.width == width && self.fingerprint == fingerprint(message, streaming)
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
    pub(crate) fn sync(&mut self, messages: &[Msg], streaming: Option<usize>, width: u16) -> u16 {
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

        let mut total = 0u16;
        for (index, message) in messages.iter().enumerate() {
            let is_streaming = streaming == Some(index);
            let entry = &mut self.entries[index];
            if !entry.matches(message, is_streaming, width) {
                let lines = to_owned_lines(message.to_lines(is_streaming, width));
                entry.width = width;
                entry.fingerprint = fingerprint(message, is_streaming);
                entry.height = measure(&lines, width);
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
        scroll: u16,
    ) {
        if area.height == 0 || area.width == 0 {
            for entry in &mut self.entries {
                entry.lines = None;
            }
            return;
        }
        let viewport_top = i32::from(area.y);
        let viewport_bottom = i32::from(area.bottom());
        let mut cursor = viewport_top - i32::from(scroll);

        for (index, entry) in self.entries.iter_mut().enumerate() {
            let start = cursor;
            cursor = cursor.saturating_add(i32::from(entry.height));
            let visible = entry.height > 0 && cursor > viewport_top && start < viewport_bottom;
            if !visible {
                // 视口外的消息只需要高度，行内容立即释放。
                entry.lines = None;
                continue;
            }
            let Some(message) = messages.get(index) else {
                continue;
            };
            let width = entry.width;
            let lines = entry.lines.get_or_insert_with(|| {
                to_owned_lines(message.to_lines(streaming == Some(index), width))
            });

            // 顶部被裁掉的消息用 Paragraph 自身的滚动跳过整行，底部裁掉的部分由矩形高度截断。
            let clipped_top = start.max(viewport_top);
            let clipped_bottom = cursor.min(viewport_bottom);
            let skip = u16::try_from(clipped_top - start).unwrap_or(u16::MAX);
            let target = Rect {
                x: area.x,
                y: u16::try_from(clipped_top).unwrap_or(u16::MAX),
                width: area.width,
                height: u16::try_from(clipped_bottom - clipped_top).unwrap_or(0),
            };
            frame.render_widget(
                Paragraph::new(lines.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((skip, 0)),
                target,
            );
        }
    }

    /// 返回可点击工具消息的 `(消息索引, 起始行, 高度)`；起始行相对全部消息顶部。
    pub(crate) fn tool_rows(&self) -> Vec<(usize, u16, u16)> {
        let mut rows = Vec::new();
        let mut start = 0u16;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.expandable && entry.height > 0 {
                rows.push((index, start, entry.height));
            }
            start = start.saturating_add(entry.height);
        }
        rows
    }
}

/// 计算整条消息换行后的视觉行数。
///
/// 整条消息只构造一个 `Paragraph`，避免逐行调用 `line_count` 的重复开销。
fn measure(lines: &[Line<'static>], width: u16) -> u16 {
    u16::try_from(
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width),
    )
    .unwrap_or(u16::MAX)
}

/// 计算一组附加行换行后的视觉行数，供消息区之外的运行指示复用。
pub(crate) fn measure_lines(lines: &[Line<'static>], width: u16) -> u16 {
    if lines.is_empty() {
        return 0;
    }
    measure(lines, width.max(1))
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
fn write_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    hash = write_bytes_len(hash, bytes.len());
    for byte in bytes {
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
    fn render(layout: &mut TranscriptLayout, messages: &[Msg], height: u16, scroll: u16) -> usize {
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
