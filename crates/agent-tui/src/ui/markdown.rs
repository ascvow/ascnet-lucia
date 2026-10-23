//! 会话正文的 Markdown、表格与工具输出行渲染。

use crate::theme::{
    COLOR_ACCENT, COLOR_BORDER_FOCUS, COLOR_CODE, COLOR_CODE_BG, COLOR_MUTED, COLOR_SEPARATOR,
    COLOR_TEXT, COLOR_USER, COLOR_USER_BG,
};
use crate::ui::highlight::{highlight_line, CodeLanguage};
use ratatui::prelude::*;
use serde_json::Value;

/// 构造用户消息块：按显示宽度预换行，每个视觉行都带左侧竖条与统一背景色。
///
/// 预换行避免 Paragraph 自动换行时续行丢失竖条，保证消息块边缘连续；
/// 每行按显示宽度补齐尾部空格，使背景色铺满整个消息区宽度。
pub(crate) fn user_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    // 竖条与右侧留白共占 3 列。
    let max = usize::from(width).saturating_sub(3).max(1);
    let body = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines()
            .flat_map(|line| wrap_display(line, max))
            .collect::<Vec<_>>()
    };
    let mut lines: Vec<Line> = body
        .into_iter()
        .map(|line| {
            // 补齐到 max 列后再加 1 列右侧留白，整行恰好占满 width。
            let pad = max.saturating_sub(unicode_width::UnicodeWidthStr::width(line.as_str()));
            Line::from(vec![
                Span::styled("▌ ", Style::new().fg(COLOR_USER).bg(COLOR_USER_BG)),
                Span::styled(
                    format!("{line}{}", " ".repeat(pad + 1)),
                    Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG),
                ),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines
}

/// 将单个逻辑行按显示宽度拆分为若干视觉行，按字符边界处理全角字符。
fn wrap_display(line: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut width = 0;
    for ch in line.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            width = 0;
        }
        current.push(ch);
        width += ch_width;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 构造角色消息：首行带标记，续行对齐缩进，流式时在末尾附加光标。
pub(crate) fn conversation_lines<'a>(
    marker: &'a str,
    text: &'a str,
    marker_color: Color,
    body_color: Color,
    streaming: bool,
) -> Vec<Line<'a>> {
    let body = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect::<Vec<_>>()
    };
    let last_line = body.len().saturating_sub(1);
    let mut lines: Vec<Line> = body
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 { marker } else { " " };
            let mut spans = vec![
                Span::styled(format!("{prefix} "), Style::new().fg(marker_color).bold()),
                Span::styled(line, Style::new().fg(body_color)),
            ];
            if streaming && index == last_line {
                spans.push(Span::styled(" ▌", Style::new().fg(COLOR_USER)));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::default());
    lines
}

/// 消息区行首标记列宽度：首行 `● `，其余行同宽留白。
const PREFIX_WIDTH: usize = 2;
/// 表格单列最小内容宽度；再窄竖线之间就只剩空白。
const TABLE_MIN_COLUMN: usize = 4;

/// Markdown 语法块。
///
/// 逐行扫描后归类：表格与围栏代码块按整块排版，其余按行排版。块级结构由这里
/// 自己负责，`tui-markdown` 只用于**行内**语法（粗体、斜体、行内代码、链接）。
enum MdBlock<'a> {
    /// 普通正文行。
    Prose(&'a str),
    /// 空行，最终折叠为块间距。
    Blank,
    /// ATX 标题，`level` 为 `#` 的个数（1..=6）。
    Heading {
        /// 标题层级。
        level: u8,
        /// 去掉 `#` 前缀与结尾闭合标记的正文。
        text: &'a str,
    },
    /// 列表项。
    ListItem {
        /// 原始行首空白折算的显示列数。
        indent: usize,
        /// 项目符号或原文序号。
        marker: ListMarker<'a>,
        /// 去掉标记后的正文。
        text: &'a str,
    },
    /// 引用行，已去掉 `>` 前缀。
    Quote(&'a str),
    /// 分隔线。
    Rule,
    /// 表格块，含表头与分隔行。
    Table(Vec<&'a str>),
    /// 围栏代码块。
    Code {
        /// 围栏首行声明的语言标签。
        language: &'a str,
        /// 不含围栏行的代码内容。
        lines: Vec<&'a str>,
    },
}

/// 列表项标记。
enum ListMarker<'a> {
    /// 无序列表，统一渲染为圆点。
    Bullet,
    /// 有序列表，保留原文序号（如 `1.`）。
    Ordered(&'a str),
}

/// 将助手回复按 Markdown 渲染，首行带 `●` 标记，续行缩进对齐。
///
/// `width` 为消息区可用显示宽度：表格列宽、列表悬挂缩进、代码块底色宽度都按它
/// 计算，缺了它这些结构会被 `Paragraph` 的自动换行从中间折断。连续空行折叠为
/// 一行；`streaming` 为真时在末尾追加光标。
pub(crate) fn markdown_lines(text: &str, streaming: bool, width: u16) -> Vec<Line<'static>> {
    // 行首标记列由本函数统一添加，正文排版宽度必须先扣掉它。
    let body_width = usize::from(width).saturating_sub(PREFIX_WIDTH).max(1);
    let mut rendered: Vec<Line<'static>> = Vec::new();
    for block in split_markdown_blocks(text) {
        render_block(block, body_width, &mut rendered);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut blank_pending = false;
    for line in rendered {
        if line.width() == 0 {
            // 空行只在两段内容之间保留一行，避免 Markdown 块间距叠加。
            blank_pending = !lines.is_empty();
            continue;
        }
        if blank_pending {
            lines.push(Line::default());
            blank_pending = false;
        }
        let prefix = if lines.is_empty() { "● " } else { "  " };
        let line_style = line.style;
        let mut spans = vec![Span::styled(prefix, Style::new().fg(COLOR_TEXT).bold())];
        spans.extend(line.spans);
        lines.push(Line::from(spans).style(line_style));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "● ",
            Style::new().fg(COLOR_TEXT).bold(),
        )));
    }
    if streaming {
        if let Some(last) = lines.last_mut() {
            // 代码块与表格行铺满整行，先收回尾部留白再放光标。
            trim_line_end(last);
            last.push_span(Span::styled(" ▌", Style::new().fg(COLOR_USER)));
        }
    }
    lines.push(Line::default());
    lines
}

/// 按行扫描文本并归类为 Markdown 块，围栏代码块与表格整块切出。
fn split_markdown_blocks(text: &str) -> Vec<MdBlock<'_>> {
    let rows: Vec<&str> = text
        .split_inclusive('\n')
        .map(|raw| raw.trim_end_matches(['\n', '\r']))
        .collect();

    let mut blocks = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        let trimmed = rows[index].trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            let language = language.trim();
            index += 1;
            let mut code = Vec::new();
            while index < rows.len() && !rows[index].trim_start().starts_with("```") {
                code.push(rows[index]);
                index += 1;
            }
            // 跳过闭合围栏；未闭合的围栏一直吃到文本结尾。
            if index < rows.len() {
                index += 1;
            }
            blocks.push(MdBlock::Code {
                language,
                lines: code,
            });
            continue;
        }
        if is_table_row(rows[index])
            && rows
                .get(index + 1)
                .is_some_and(|next| is_table_separator(next))
        {
            let mut table = Vec::new();
            while index < rows.len() && is_table_row(rows[index]) {
                table.push(rows[index]);
                index += 1;
            }
            blocks.push(MdBlock::Table(table));
            continue;
        }
        blocks.push(classify_line(rows[index]));
        index += 1;
    }
    blocks
}

/// 把单行文本归类为对应的 Markdown 块，无法识别的按正文处理。
fn classify_line(line: &str) -> MdBlock<'_> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return MdBlock::Blank;
    }
    if let Some((level, text)) = atx_heading(trimmed) {
        return MdBlock::Heading { level, text };
    }
    if is_thematic_break(trimmed) {
        return MdBlock::Rule;
    }
    if let Some(text) = quote_body(trimmed) {
        return MdBlock::Quote(text);
    }
    // 列表项按原文缩进分级：制表符按四列折算，避免混排时层级跳变。
    let indent = line[..line.len() - trimmed.len()]
        .chars()
        .map(|ch| if ch == '\t' { 4 } else { 1 })
        .sum();
    if let Some((marker, text)) = list_item(trimmed) {
        return MdBlock::ListItem {
            indent,
            marker,
            text,
        };
    }
    // 正文保留行首缩进：列表项下的续行段落靠它对齐，去掉会退回到行首。
    MdBlock::Prose(line)
}

/// 识别 ATX 标题（`# 标题`），返回层级与去掉前缀的正文。
///
/// `#` 后面必须跟空白或行尾，否则 `#tag` 一类文本会被误判成标题；结尾用于
/// 闭合的 `#` 串也一并去掉。
fn atx_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim();
    let text = match text.rsplit_once(char::is_whitespace) {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|ch| ch == '#') => {
            head.trim_end()
        }
        _ => text,
    };
    Some((hashes as u8, text))
}

/// 判断是否为分隔线（三个以上连续的 `-`、`*` 或 `_`）。
fn is_thematic_break(line: &str) -> bool {
    let mut marker = None;
    let mut count = 0;
    for ch in line.chars() {
        if ch.is_whitespace() {
            continue;
        }
        match marker {
            None if matches!(ch, '-' | '*' | '_') => marker = Some(ch),
            Some(expected) if expected == ch => {}
            _ => return false,
        }
        count += 1;
    }
    count >= 3
}

/// 剥掉引用前缀，返回正文；非引用行返回 `None`。
fn quote_body(line: &str) -> Option<&str> {
    let mut rest = line.strip_prefix('>')?;
    // 嵌套引用连续剥掉多层，正文里不再残留 `>` 标记。
    loop {
        rest = rest.strip_prefix(' ').unwrap_or(rest);
        match rest.strip_prefix('>') {
            Some(next) => rest = next,
            None => return Some(rest),
        }
    }
}

/// 识别列表项标记，返回标记与去掉标记的正文。
fn list_item(line: &str) -> Option<(ListMarker<'_>, &str)> {
    if let Some(first) = line.chars().next() {
        if matches!(first, '-' | '*' | '+') {
            let rest = line[first.len_utf8()..].strip_prefix(' ')?;
            return Some((ListMarker::Bullet, rest.trim_start()));
        }
    }
    // 有序列表：数字加 `.` 或 `)`，后面必须跟空格才是列表项。
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let delimiter = line[digits..].chars().next()?;
    if !matches!(delimiter, '.' | ')') {
        return None;
    }
    let text = line[digits + 1..].strip_prefix(' ')?;
    Some((ListMarker::Ordered(&line[..digits + 1]), text.trim_start()))
}

/// 渲染单个 Markdown 块，`body_width` 为消息区扣除行首标记后的可用列数。
fn render_block(block: MdBlock<'_>, body_width: usize, out: &mut Vec<Line<'static>>) {
    match block {
        MdBlock::Blank => out.push(Line::default()),
        MdBlock::Heading { level, text } => render_heading(level, text, body_width, out),
        MdBlock::ListItem {
            indent,
            marker,
            text,
        } => render_list_item(indent, marker, text, body_width, out),
        MdBlock::Quote(text) => render_quote(text, body_width, out),
        MdBlock::Rule => out.push(Line::from(Span::styled(
            "─".repeat(body_width),
            Style::new().fg(COLOR_SEPARATOR),
        ))),
        MdBlock::Table(rows) => render_table(&rows, body_width, out),
        MdBlock::Code { language, lines } => render_code(language, &lines, body_width, out),
        MdBlock::Prose(text) => {
            let chunks = wrap_inline(inline_spans(text), body_width);
            push_wrapped(out, chunks, Vec::new(), 0);
        }
    }
}

/// 渲染标题：终端没有字号，层级靠标记形状与颜色区分。
///
/// 标记刻意不用竖条：竖条已被代码块（`▎`）与引用（`▏`）占用，标题再用会让三种
/// 结构混在一起。层级越深方块越小、颜色越次要。
fn render_heading(level: u8, text: &str, body_width: usize, out: &mut Vec<Line<'static>>) {
    if out.last().is_some_and(|line| line.width() > 0) {
        out.push(Line::default());
    }
    let (mark, mark_color, text_style) = match level {
        1 => ("■", COLOR_USER, Style::new().fg(COLOR_TEXT).bold()),
        2 => ("▪", COLOR_MUTED, Style::new().fg(COLOR_TEXT).bold()),
        3 => ("▫", COLOR_MUTED, Style::new().fg(COLOR_TEXT).bold()),
        _ => ("·", COLOR_SEPARATOR, Style::new().fg(COLOR_MUTED).bold()),
    };
    let spans: Vec<Span<'static>> = inline_spans(text)
        .into_iter()
        .map(|span| {
            let style = text_style.patch(span.style);
            Span::styled(span.content, style)
        })
        .collect();
    let chunks = wrap_inline(spans, body_width.saturating_sub(2).max(1));
    let first = vec![Span::styled(
        format!("{mark} "),
        Style::new().fg(mark_color).bold(),
    )];
    push_wrapped(out, chunks, first, 2);
}

/// 渲染列表项：无序列表用圆点，续行与正文首列对齐形成悬挂缩进。
fn render_list_item(
    indent: usize,
    marker: ListMarker<'_>,
    text: &str,
    body_width: usize,
    out: &mut Vec<Line<'static>>,
) {
    // 缩进不能吃掉太多正文宽度，否则窄终端下每行只剩一两个字。
    let indent = indent.min(body_width.saturating_sub(8));
    let marker_text = match marker {
        ListMarker::Bullet => "•".to_string(),
        ListMarker::Ordered(original) => original.to_string(),
    };
    let marker_width = unicode_width::UnicodeWidthStr::width(marker_text.as_str()) + 1;
    let available = body_width.saturating_sub(indent + marker_width).max(1);
    let chunks = wrap_inline(inline_spans(text), available);
    let first = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(format!("{marker_text} "), Style::new().fg(COLOR_MUTED)),
    ];
    push_wrapped(out, chunks, first, indent + marker_width);
}

/// 渲染引用：每行都带一条暗色竖线，正文降为次要色。
fn render_quote(text: &str, body_width: usize, out: &mut Vec<Line<'static>>) {
    let chunks = wrap_inline(inline_spans(text), body_width.saturating_sub(2).max(1));
    for chunk in chunks {
        let mut spans = vec![Span::styled("▏ ", Style::new().fg(COLOR_SEPARATOR))];
        spans.extend(chunk.into_iter().map(|span| {
            let style = Style::new().fg(COLOR_MUTED).patch(span.style);
            Span::styled(span.content, style)
        }));
        out.push(Line::from(spans));
    }
}

/// 渲染围栏代码块：隐藏围栏行，标注语言，并按语言族做轻量高亮。
///
/// 整块铺一层比正文略亮的底色，左侧竖线沿用聚焦边框色，让代码与正文之间形成
/// 明确的分区；超宽代码按显示宽度自行折行，避免被 `Paragraph` 折到行首。
fn render_code(language: &str, rows: &[&str], body_width: usize, out: &mut Vec<Line<'static>>) {
    let language = language.trim();
    if out.last().is_some_and(|line| line.width() > 0) {
        out.push(Line::default());
    }
    let code_language = CodeLanguage::from_tag(language);
    if !language.is_empty() {
        out.push(code_line(
            vec![Span::styled(
                language.to_string(),
                Style::new().fg(COLOR_MUTED),
            )],
            body_width,
        ));
    }
    let rows: Vec<&str> = if rows.is_empty() {
        vec![""]
    } else {
        rows.to_vec()
    };
    let available = body_width.saturating_sub(2).max(1);
    for row in rows {
        for chunk in wrap_display(row, available) {
            out.push(code_line(highlight_line(&chunk, code_language), body_width));
        }
    }
    out.push(Line::default());
}

/// 构造铺满消息区宽度的一行代码：左侧竖条、整块底色与尾部补齐。
fn code_line(content: Vec<Span<'static>>, body_width: usize) -> Line<'static> {
    let used = spans_width(&content);
    let mut spans = vec![Span::styled(
        "▎ ",
        Style::new().fg(COLOR_BORDER_FOCUS).bg(COLOR_CODE_BG),
    )];
    spans.extend(content.into_iter().map(|span| {
        let style = span.style.patch(Style::new().bg(COLOR_CODE_BG));
        Span::styled(span.content, style)
    }));
    let pad = body_width.saturating_sub(used + 2);
    spans.push(Span::styled(
        " ".repeat(pad),
        Style::new().bg(COLOR_CODE_BG),
    ));
    Line::from(spans)
}

/// 判断是否为表格行（修剪后以竖线开头）。
pub(crate) fn is_table_row(row: &str) -> bool {
    row.trim_start().starts_with('|')
}

/// 判断是否为表格分隔行（形如 `|---|:---:|`）。
pub(crate) fn is_table_separator(row: &str) -> bool {
    is_separator_cells(&parse_table_row(row))
}

/// 判断一组单元格是否构成表格分隔行。
fn is_separator_cells(cells: &[&str]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| {
            !cell.is_empty() && cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':'))
        })
}

/// 拆出表格行的单元格：去掉首尾竖线后按 `|` 分列并修剪空白。
pub(crate) fn parse_table_row(row: &str) -> Vec<&str> {
    let trimmed = row.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(str::trim).collect()
}

/// 将表格排版为对齐的行：表头加粗，分隔行转为横线，超宽列在列内折行。
///
/// `body_width` 是消息区可用列数：列宽先按内容取理想值，再逐步压缩最宽列直到
/// 整表能放进终端，避免自动换行把单元格顶过竖线。
fn render_table(rows: &[&str], body_width: usize, out: &mut Vec<Line<'static>>) {
    // 分隔行只描述对齐方式，既不参与列宽统计，也不作为正文输出。
    let cells: Vec<Vec<Vec<Span<'static>>>> = rows
        .iter()
        .map(|row| parse_table_row(row))
        .filter(|cells| !is_separator_cells(cells))
        .map(|cells| cells.iter().map(|cell| inline_spans(cell)).collect())
        .collect();
    let Some(header) = cells.first() else {
        return;
    };
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return;
    }
    let widths = table_column_widths(&cells, columns, body_width);

    out.push(Line::default());
    push_table_row(header, &widths, true, out);
    let rule = widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("┼");
    out.push(Line::from(Span::styled(
        rule,
        Style::new().fg(COLOR_SEPARATOR),
    )));
    for row in cells.iter().skip(1) {
        push_table_row(row, &widths, false, out);
    }
    out.push(Line::default());
}

/// 计算每列的内容宽度：先按内容取理想值，再压缩到终端宽度以内。
fn table_column_widths(
    body: &[Vec<Vec<Span<'static>>>],
    columns: usize,
    body_width: usize,
) -> Vec<usize> {
    let mut widths = vec![TABLE_MIN_COLUMN; columns];
    for row in body {
        for (col, cell) in row.iter().enumerate() {
            widths[col] = widths[col].max(spans_width(cell));
        }
    }
    // 每列左右各一列内边距，列之间还需要一列竖线；超宽时反复压缩当前最宽的列。
    let mut total = widths.iter().sum::<usize>() + columns * 2 + columns.saturating_sub(1);
    while total > body_width {
        let mut widest = 0;
        for col in 1..columns {
            if widths[col] > widths[widest] {
                widest = col;
            }
        }
        if widths[widest] <= TABLE_MIN_COLUMN {
            break;
        }
        widths[widest] -= 1;
        total -= 1;
    }
    widths
}

/// 追加一行表格：单元格超出列宽时折行，同一行的高度取最高的单元格。
fn push_table_row(
    cells: &[Vec<Span<'static>>],
    widths: &[usize],
    header: bool,
    out: &mut Vec<Line<'static>>,
) {
    let style = if header {
        Style::new().fg(COLOR_TEXT).bold()
    } else {
        Style::new().fg(COLOR_TEXT)
    };
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .enumerate()
        .map(|(col, width)| {
            let spans = cells.get(col).cloned().unwrap_or_default();
            wrap_inline(spans, *width)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (col, width) in widths.iter().enumerate() {
            if col > 0 {
                spans.push(Span::styled("│", Style::new().fg(COLOR_SEPARATOR)));
            }
            let content = wrapped[col].get(line).cloned().unwrap_or_default();
            let used = spans_width(&content);
            spans.push(Span::styled(" ", style));
            spans.extend(content.into_iter().map(|span| {
                let style = style.patch(span.style);
                Span::styled(span.content, style)
            }));
            spans.push(Span::styled(
                format!("{} ", " ".repeat(width.saturating_sub(used))),
                style,
            ));
        }
        out.push(Line::from(spans));
    }
}

/// 把一行行内 Markdown 解析为带样式的片段。
///
/// 空行返回空片段；行首缩进原样保留——`pulldown-cmark` 会把四空格缩进当成代码块，
/// 直接交给它会凭空多出一对围栏。
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let trimmed = text.trim_start();
    let mut spans: Vec<Span<'static>> = Vec::new();
    if trimmed.len() != text.len() {
        spans.push(Span::raw(text[..text.len() - trimmed.len()].to_string()));
    }
    if trimmed.is_empty() {
        return spans;
    }
    for line in tui_markdown::from_str(trimmed).lines {
        for span in line.spans {
            let style = restyle_inline(span.style);
            spans.push(Span::styled(span.content.into_owned(), style));
        }
    }
    spans
}

/// 计算一组片段的显示宽度。
fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// 把 tui-markdown 的行内样式映射到 Lucia 色板。
///
/// 行内代码在 tui-markdown 里是黑底白字，这里去掉背景换成暖色前景；链接去掉
/// 下划线改用强调色。粗体、斜体不带前景色，原样保留。
fn restyle_inline(style: Style) -> Style {
    if style.bg.is_some() {
        return Style::new().fg(COLOR_CODE);
    }
    if style.fg.is_some() {
        let mut modifiers = style.add_modifier;
        // 下划线在终端里过于花哨，链接用颜色区分即可。
        modifiers.remove(Modifier::UNDERLINED);
        return Style::new().fg(COLOR_ACCENT).add_modifier(modifiers);
    }
    style
}

/// 按显示宽度折行：优先在空白或宽字符（CJK）之后断开，超长单词才按字符硬切。
///
/// 至少返回一行；结果行尾的空白会被去掉，缩进由调用方按块类型补齐。
fn wrap_inline(spans: Vec<Span<'static>>, max: usize) -> Vec<Vec<Span<'static>>> {
    let max = max.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut width = 0;
    // 最近一个允许断行的位置。中文没有空格，只认空白会让整段退化成整行不换行。
    let mut last_break = 0;
    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + ch_width > max && !current.is_empty() {
                let break_at = if last_break > 0 {
                    last_break.min(current.len())
                } else {
                    current.len()
                };
                let mut tail = current.split_off(break_at);
                lines.push(collect_spans(current));
                // 续行不以空白开头，否则悬挂缩进会多出一列。
                while tail.first().is_some_and(|(ch, _)| ch.is_whitespace()) {
                    tail.remove(0);
                }
                current = tail;
                width = display_width(&current);
                last_break = break_position(&current);
            }
            current.push((ch, style));
            width += ch_width;
            if is_break_opportunity(ch) {
                last_break = current.len();
            }
        }
    }
    lines.push(collect_spans(current));
    lines
}

/// 判断字符之后是否允许断行：空白，以及 CJK 一类的双宽字符。
fn is_break_opportunity(ch: char) -> bool {
    ch.is_whitespace() || unicode_width::UnicodeWidthChar::width(ch) == Some(2)
}

/// 计算字符序列的显示宽度。
fn display_width(chars: &[(char, Style)]) -> usize {
    chars
        .iter()
        .map(|(ch, _)| unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0))
        .sum()
}

/// 返回字符序列中最后一个断行位置（该位置之后的字符进入下一行）。
fn break_position(chars: &[(char, Style)]) -> usize {
    chars
        .iter()
        .rposition(|(ch, _)| is_break_opportunity(*ch))
        .map_or(0, |position| position + 1)
}

/// 把字符与样式合并回片段序列，并去掉行尾空白。
fn collect_spans(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let mut buffer_style: Option<Style> = None;
    for (ch, style) in chars {
        if buffer_style != Some(style) {
            if let Some(previous) = buffer_style.replace(style) {
                if !buffer.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buffer), previous));
                }
            }
        }
        buffer.push(ch);
    }
    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, buffer_style.unwrap_or_default()));
    }
    while let Some(last) = spans.last_mut() {
        let text = last.content.trim_end_matches([' ', '\t']);
        if text.len() == last.content.len() {
            break;
        }
        last.content = text.to_string().into();
    }
    spans
}

/// 追加折行后的行：首行用 `first` 前缀，续行缩进 `hanging` 列。
fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    chunks: Vec<Vec<Span<'static>>>,
    first: Vec<Span<'static>>,
    hanging: usize,
) {
    for (index, chunk) in chunks.into_iter().enumerate() {
        let mut spans = if index == 0 {
            first.clone()
        } else if hanging > 0 {
            vec![Span::raw(" ".repeat(hanging))]
        } else {
            Vec::new()
        };
        spans.extend(chunk);
        out.push(Line::from(spans));
    }
}

/// 去掉行尾空白，用于给流式光标腾出位置。
///
/// 代码块与表格行会补齐到整行宽度，直接追加光标会把它挤到下一行行首。
fn trim_line_end(line: &mut Line<'static>) {
    let mut index = line.spans.len();
    while index > 0 {
        index -= 1;
        let trimmed = line.spans[index]
            .content
            .trim_end_matches([' ', '\t'])
            .to_string();
        if trimmed.is_empty() {
            if index > 0 {
                line.spans.truncate(index);
                continue;
            }
            // 整行只有空白：保留一个空片段，行本身仍然占位。
            line.spans[0].content = trimmed.into();
            return;
        }
        line.spans[index].content = trimmed.into();
        return;
    }
}

/// 将 JSON 值压缩为单行摘要：对象展开为 `键: 值` 列表，嵌套结构折叠为计数，
/// 超出宽度按显示宽度截断。
pub(crate) fn summarize_json(value: &Value, max_width: usize) -> String {
    let text = match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::String(s) => s.clone(),
                    // 嵌套数组和对象展开后噪音过大，折叠为条目计数。
                    Value::Array(items) => format!("[{} 项]", items.len()),
                    Value::Object(_) => "{…}".to_string(),
                    other => other.to_string(),
                };
                format!("{key}: {value}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    truncate_line(&text, max_width)
}

/// 将文本压平为单行并按显示宽度截断，超出部分以省略号结尾。
pub(crate) fn truncate_line(text: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars() {
        // 换行与制表符替换为空格，保证摘要始终是单行。
        let ch = if ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else {
            ch
        };
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            output.push('…');
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output
}
