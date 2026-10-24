//! 命令面板风格的浮层面板：单条上分隔线、画在线上的标题、底部按键提示与列表行。
//!
//! 原生 Dialog 与插件 Dialog 共用这套外形。它们像命令补全面板一样贴在输入盒上方、
//! 横向铺满，而不是用四面包围的盒子悬浮在屏幕正中：临时面板只保留一种视觉语言，
//! 用户不必为每块面板重新辨认"这里是列表、那里是对话框"。

use crate::theme::{
    COLOR_BORDER_FOCUS, COLOR_META, COLOR_PALETTE_COMMAND, COLOR_PALETTE_SELECTION_BG,
    COLOR_SELECTION_MARK, COLOR_TEXT, COLOR_TEXT_SOFT,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

/// 面板提示行与输入盒之间保留的空行数。
///
/// 与命令补全面板末尾那一行留白同源：提示行直接贴住输入盒上边框会让整块面板看起来
/// 被压扁，留一行空白让面板有收尾，也不会把输入框推离视线。
const SHEET_BOTTOM_GAP: u16 = 1;

/// 选中行指示条到正文之间的内边距列数。
const PALETTE_ROW_INSET: usize = 1;

/// 名称列与说明列之间的固定间距。
const PALETTE_COLUMN_GAP: usize = 2;

/// 一块命令面板风格面板的完整内容。
///
/// 面板自身只负责外框、内容视口与提示行；内容是什么、如何滚动由调用方决定，
/// 因此这里不持有任何命令或插件业务状态。
pub(crate) struct PaletteSheet<'a> {
    /// 画在上分隔线上、两侧自动补空格的标题。
    pub(crate) title: &'a str,
    /// 内容行；超出视口时由 `scroll` 指定起点。
    pub(crate) lines: Vec<Line<'static>>,
    /// 提示行；`None` 表示面板不需要按键提示，内容视口相应多一行。
    pub(crate) hint: Option<Line<'static>>,
    /// 内容滚动偏移；由调用方按 [`palette_sheet_viewport`] 给出的行数夹住。
    pub(crate) scroll: u16,
    /// 是否按面板宽度折行；插件帧自带断行，不应二次折行。
    pub(crate) wrap: bool,
}

/// 面板占用的完整高度：顶部分隔线 + 内容行 + 可选提示行 + 底部空行。
pub(crate) fn palette_sheet_height(content_rows: u16, has_hint: bool) -> u16 {
    content_rows
        // 顶部分隔线；标题画在它上面，不额外占行。
        .saturating_add(1)
        .saturating_add(u16::from(has_hint))
        .saturating_add(SHEET_BOTTOM_GAP)
}

/// 给定面板总高度时真正能显示的内容行数。
pub(crate) fn palette_sheet_content_rows(height: u16, has_hint: bool) -> u16 {
    height
        .saturating_sub(1)
        .saturating_sub(u16::from(has_hint))
        .saturating_sub(SHEET_BOTTOM_GAP)
}

/// 输入盒上方的可用高度：面板向上生长，但不能越过画布顶部。
pub(crate) fn palette_sheet_capacity(frame: &Frame, anchor: Rect) -> u16 {
    anchor.y.saturating_sub(frame.area().y)
}

/// 把"希望的内容行数"收窄成终端真正给得起的（面板高度, 内容行数）。
///
/// 布局与渲染必须共用这一份几何：调用方先用它夹住滚动偏移，再按同一个高度绘制，
/// 否则滚动上限会与可见行数对不上，表现为列表末尾永远差几行。
pub(crate) fn palette_sheet_viewport(
    frame: &Frame,
    anchor: Rect,
    content_rows: u16,
    has_hint: bool,
) -> (u16, u16) {
    let height =
        palette_sheet_height(content_rows, has_hint).min(palette_sheet_capacity(frame, anchor));
    (height, palette_sheet_content_rows(height, has_hint))
}

/// 绘制一块命令面板风格的面板，返回实际的内容区矩形。
///
/// `anchor` 是下方输入盒的区域：面板宽度与它相同，底边贴在它上沿，向上占据 `height` 行。
/// `height` 应来自 [`palette_sheet_viewport`]；这里再夹一次是为了让调用方漏夹时也不会
/// 画出画布。
pub(crate) fn render_palette_sheet(
    frame: &mut Frame,
    anchor: Rect,
    height: u16,
    sheet: PaletteSheet<'_>,
) -> Rect {
    let has_hint = sheet.hint.is_some();
    let height = height.min(palette_sheet_capacity(frame, anchor));
    let content_rows = palette_sheet_content_rows(height, has_hint);
    let area = Rect {
        x: anchor.x,
        y: anchor.y.saturating_sub(height),
        width: anchor.width,
        height,
    };
    let content_area = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: content_rows,
    };
    if area.is_empty() {
        return content_area;
    }
    // 面板浮在正文之上：插件帧的行宽未必铺满，先清掉这一块，否则旧文字会从行尾透出来。
    frame.render_widget(Clear, area);
    let mut content = Paragraph::new(sheet.lines).scroll((sheet.scroll, 0));
    if sheet.wrap {
        content = content.wrap(Wrap { trim: false });
    }
    frame.render_widget(content, content_area);
    // 终端矮到只剩分隔线时不再画提示：那一行已经属于输入盒，覆盖它会撕掉边框。
    if let Some(hint) = sheet
        .hint
        .filter(|_| content_rows.saturating_add(1) < height)
    {
        frame.render_widget(
            Paragraph::new(hint),
            Rect {
                x: area.x,
                y: area.y.saturating_add(1).saturating_add(content_rows),
                width: area.width,
                height: 1,
            },
        );
    }
    // 只画朝上的一条分隔线：面板下方就是输入盒，四面包围会与它的边框叠成双层框。
    frame.render_widget(
        Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
            .title(Span::styled(
                format!(" {} ", sheet.title),
                Style::new().fg(COLOR_TEXT_SOFT),
            )),
        area,
    );
    content_area
}

/// 构造面板的提示行：与命令面板同一起点、同一元信息色。
pub(crate) fn palette_hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {text}"),
        Style::new().fg(COLOR_META),
    ))
}

/// 按显示宽度裁剪文本；宽度为零时直接返回空串。
///
/// [`truncate_line`](crate::ui::markdown::truncate_line) 会在填满预算后再追加一个省略号，
/// 结果可能超出预算一列；用于列对齐时会挤掉后面的列。这里先判断是否放得下，放不下时
/// 把省略号那一列也让出来。
fn fit_column(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if unicode_width::UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    crate::ui::markdown::truncate_line(text, width - 1)
}

/// 构造面板列表的一行：选中指示条、名称列、说明列，整行底色铺到面板右边界。
///
/// `name_width` 是名称列宽，`row_width` 是整行宽度。两列各自裁剪到列宽内，保证一行
/// 永远只占一个终端行——面板高度按一行一条计算，这里一旦换行，渲染结果就会和布局
/// 算出的高度对不上，底色也会缺一角。
pub(crate) fn palette_row(
    name: &str,
    detail: &str,
    selected: bool,
    name_width: usize,
    row_width: usize,
) -> Line<'static> {
    // 未选中行不铺底色：整列底色会让列表糊成一片，选中项也就失去了锚点。
    let row = if selected {
        Style::new().bg(COLOR_PALETTE_SELECTION_BG)
    } else {
        Style::new()
    };
    // 指示条固定占一列；终端窄到连它都放不下时整行留白，绝不越界。
    if row_width == 0 {
        return Line::default();
    }
    let inset = PALETTE_ROW_INSET.min(row_width - 1);
    let remaining = row_width - 1 - inset;
    // 列间距只在确实还排得下说明列时保留：极窄终端里名称列自己撑满剩余宽度。
    let gap = PALETTE_COLUMN_GAP.min(remaining.saturating_sub(1));
    let name_width = name_width.min(remaining - gap);
    let detail_width = remaining - gap - name_width;
    let name = fit_column(name, name_width);
    let name_pad = name_width.saturating_sub(unicode_width::UnicodeWidthStr::width(name.as_str()));
    let detail = fit_column(detail, detail_width);
    // 选中项只加粗名称：整行加粗会让底色块里全是高亮字，反而更难扫。
    let name_style = if selected {
        row.fg(COLOR_PALETTE_COMMAND).bold()
    } else {
        row.fg(COLOR_PALETTE_COMMAND)
    };
    let mut spans = vec![
        Span::styled(
            if selected { "▌" } else { " " },
            Style::new().fg(COLOR_SELECTION_MARK).patch(row),
        ),
        Span::styled(" ".repeat(inset), row),
        Span::styled(name, name_style),
        Span::styled(" ".repeat(name_pad + gap), row),
    ];
    // 行尾补白把底色铺到面板右边界；它同时是说明列的右内边距。
    let detail_pad =
        detail_width.saturating_sub(unicode_width::UnicodeWidthStr::width(detail.as_str()));
    let detail_style = if selected {
        row.fg(COLOR_TEXT)
    } else {
        row.fg(COLOR_TEXT_SOFT)
    };
    spans.push(Span::styled(detail, detail_style));
    spans.push(Span::styled(" ".repeat(detail_pad), row));
    Line::from(spans)
}

/// 构造只有一列正文的面板行：模型上游这类没有说明列的列表复用同一套选中视觉。
pub(crate) fn palette_plain_row(label: &str, selected: bool, row_width: usize) -> Line<'static> {
    let row = if selected {
        Style::new().bg(COLOR_PALETTE_SELECTION_BG)
    } else {
        Style::new()
    };
    if row_width == 0 {
        return Line::default();
    }
    let inset = PALETTE_ROW_INSET.min(row_width - 1);
    let body_width = row_width - 1 - inset;
    let body = fit_column(label, body_width);
    let filler = body_width.saturating_sub(unicode_width::UnicodeWidthStr::width(body.as_str()));
    let body_style = if selected {
        row.fg(COLOR_TEXT).bold()
    } else {
        row.fg(COLOR_TEXT_SOFT)
    };
    Line::from(vec![
        Span::styled(
            if selected { "▌" } else { " " },
            Style::new().fg(COLOR_SELECTION_MARK).patch(row),
        ),
        Span::styled(" ".repeat(inset), row),
        Span::styled(body, body_style),
        Span::styled(" ".repeat(filler), row),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// 内容行、提示行与顶部线的加减必须闭合，否则布局与渲染会算出不同高度。
    #[test]
    fn sheet_height_round_trips_content_rows() {
        for has_hint in [false, true] {
            for rows in [0u16, 1, 7, 40] {
                let height = palette_sheet_height(rows, has_hint);
                assert_eq!(palette_sheet_content_rows(height, has_hint), rows);
            }
        }
    }

    /// 面板以输入盒上沿为底边向上生长，宽度与输入盒一致。
    #[test]
    fn sheet_is_anchored_above_the_input_box() {
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let anchor = Rect::new(2, 15, 30, 3);
        terminal
            .draw(|frame| {
                let (height, rows) = palette_sheet_viewport(frame, anchor, 4, true);
                let content_area = render_palette_sheet(
                    frame,
                    anchor,
                    height,
                    PaletteSheet {
                        title: "测试面板",
                        lines: vec![Line::from("正文")],
                        hint: Some(palette_hint("Esc 关闭")),
                        scroll: 0,
                        wrap: false,
                    },
                );
                assert_eq!(rows, 4);
                assert_eq!(content_area.height, 4);
                assert_eq!(content_area.y, anchor.y - height + 1);
                assert_eq!(content_area.width, anchor.width);
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // 宽字符在缓冲区里各占两格、第二格是空白；比较前去掉空白才能匹配原生文本。
        let compact: String = screen.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("测试面板"), "{screen}");
        assert!(compact.contains("Esc关闭"), "{screen}");
    }

    /// 输入盒以上空间不足时面板整体收进画布，不能出现越界写入。
    #[test]
    fn sheet_never_escapes_the_canvas() {
        let mut terminal = Terminal::new(TestBackend::new(6, 4)).unwrap();
        let anchor = Rect::new(0, 1, 6, 1);
        terminal
            .draw(|frame| {
                let (height, rows) = palette_sheet_viewport(frame, anchor, 30, true);
                assert_eq!(height, 1, "画布只剩一行时面板只能占这一行");
                assert_eq!(rows, 0);
                render_palette_sheet(
                    frame,
                    anchor,
                    height,
                    PaletteSheet {
                        title: "窄",
                        lines: vec![Line::from("正文")],
                        hint: Some(palette_hint("Esc")),
                        scroll: 0,
                        wrap: true,
                    },
                );
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // 只剩一行时只画分隔线与标题：提示不能盖到下一行的输入盒上。
        assert!(screen.contains('窄'), "{screen}");
        assert!(!screen.contains("Esc"), "{screen}");
    }

    /// 列表行必须正好铺满给定宽度：底色铺到右边界，且不会溢出换行。
    #[test]
    fn palette_rows_fill_exactly_the_row_width() {
        for row_width in [4usize, 10, 40, 120] {
            let line = palette_row("插件", "1.0.0 已启用", true, 8, row_width);
            assert_eq!(line.width(), row_width, "row_width={row_width}");
            let line = palette_plain_row("gpt-5.6-sol [default]", false, row_width);
            assert_eq!(line.width(), row_width, "row_width={row_width}");
        }
    }
}
