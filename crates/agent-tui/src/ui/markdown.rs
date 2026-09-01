//! 会话正文的 Markdown、表格与工具输出行渲染。

use crate::theme::{
    COLOR_BORDER_FOCUS, COLOR_MUTED, COLOR_TEXT, COLOR_USER, COLOR_USER_BG, COLOR_WARNING,
};
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

/// Markdown 文本段：普通段落交给 tui-markdown，表格与围栏代码块单独排版。
enum MdSegment<'a> {
    Prose(&'a str),
    Table(Vec<&'a str>),
    Code {
        /// 围栏首行声明的语言名。
        language: &'a str,
        /// 不含围栏行的代码内容。
        lines: Vec<&'a str>,
    },
}

/// 将助手回复按 Markdown 渲染：表格块自行对齐排版（tui-markdown 不支持表格），
/// 首行带 `●` 标记，续行缩进对齐，连续空行折叠为一行。
pub(crate) fn markdown_lines(text: &str, streaming: bool) -> Vec<Line<'_>> {
    let mut raw: Vec<Line> = Vec::new();
    for segment in split_markdown_segments(text) {
        match segment {
            MdSegment::Prose(chunk) => {
                for line in tui_markdown::from_str(chunk).lines {
                    // 标题样式落在行级 style 上，重建行时必须保留，否则标题与正文无差别。
                    let line_style = restyle_markdown(line.style, COLOR_USER);
                    let spans: Vec<Span> = line
                        .spans
                        .into_iter()
                        .map(|span| {
                            let style = restyle_markdown(span.style, COLOR_WARNING);
                            Span::styled(span.content, style)
                        })
                        .collect();
                    raw.push(Line::from(spans).style(line_style));
                }
            }
            MdSegment::Table(rows) => render_table(&rows, &mut raw),
            MdSegment::Code { language, lines } => render_code(language, &lines, &mut raw),
        }
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut blank_pending = false;
    for line in raw {
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
            last.push_span(Span::styled(" ▌", Style::new().fg(COLOR_USER)));
        }
    }
    lines.push(Line::default());
    lines
}

/// 按行扫描文本，切分围栏代码块和 Markdown 表格块。
fn split_markdown_segments(text: &str) -> Vec<MdSegment<'_>> {
    let mut rows: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        rows.push((offset, line));
        offset += raw.len();
    }

    let mut segments = Vec::new();
    let mut prose_start = 0;
    let mut index = 0;
    while index < rows.len() {
        let trimmed = rows[index].1.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            if rows[index].0 > prose_start {
                segments.push(MdSegment::Prose(&text[prose_start..rows[index].0]));
            }
            let language = language.trim();
            index += 1;
            let mut code = Vec::new();
            while index < rows.len() && !rows[index].1.trim_start().starts_with("```") {
                code.push(rows[index].1);
                index += 1;
            }
            if index < rows.len() {
                index += 1;
            }
            segments.push(MdSegment::Code {
                language,
                lines: code,
            });
            prose_start = rows.get(index).map_or(text.len(), |(start, _)| *start);
            continue;
        }
        let is_table_start = is_table_row(rows[index].1)
            && rows
                .get(index + 1)
                .is_some_and(|(_, next)| is_table_separator(next));
        if is_table_start {
            if rows[index].0 > prose_start {
                segments.push(MdSegment::Prose(&text[prose_start..rows[index].0]));
            }
            let mut table = Vec::new();
            while index < rows.len() && is_table_row(rows[index].1) {
                table.push(rows[index].1);
                index += 1;
            }
            segments.push(MdSegment::Table(table));
            prose_start = rows.get(index).map_or(text.len(), |(start, _)| *start);
        } else {
            index += 1;
        }
    }
    if prose_start < text.len() {
        segments.push(MdSegment::Prose(&text[prose_start..]));
    }
    segments
}

/// 渲染围栏代码块：隐藏围栏行，以左侧竖线区分正文，并在首行标注语言。
fn render_code<'a>(language: &'a str, rows: &[&'a str], out: &mut Vec<Line<'a>>) {
    let rows = if rows.is_empty() {
        vec![""]
    } else {
        rows.to_vec()
    };
    for (index, row) in rows.into_iter().enumerate() {
        let mut spans = vec![
            Span::styled("▎ ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(row, Style::new().fg(COLOR_MUTED)),
        ];
        if index == 0 && !language.is_empty() {
            spans.push(Span::styled(
                format!("  {language}"),
                Style::new().fg(COLOR_BORDER_FOCUS),
            ));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::default());
}

/// 判断是否为表格行（修剪后以竖线开头）。
pub(crate) fn is_table_row(row: &str) -> bool {
    row.trim_start().starts_with('|')
}

/// 判断是否为表格分隔行（形如 `|---|:---:|`）。
pub(crate) fn is_table_separator(row: &str) -> bool {
    let cells = parse_table_row(row);
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

/// 将表格排版为等宽对齐的行：表头加粗，分隔行转为横线，中文按显示宽度对齐。
pub(crate) fn render_table<'a>(rows: &[&'a str], out: &mut Vec<Line<'a>>) {
    let parsed: Vec<Vec<&str>> = rows.iter().map(|row| parse_table_row(row)).collect();
    let columns = parsed.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return;
    }
    // 分隔行（第 2 行）不参与列宽统计。
    let mut widths = vec![0usize; columns];
    for (index, row) in parsed.iter().enumerate() {
        if index == 1 {
            continue;
        }
        for (col, cell) in row.iter().enumerate() {
            widths[col] = widths[col].max(unicode_width::UnicodeWidthStr::width(*cell));
        }
    }

    out.push(Line::default());
    for (index, row) in parsed.iter().enumerate() {
        if index == 1 {
            let rule = widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join("┼");
            out.push(Line::from(Span::styled(rule, Style::new().fg(COLOR_MUTED))));
            continue;
        }
        let mut spans: Vec<Span> = Vec::new();
        for (col, width) in widths.iter().enumerate() {
            if col > 0 {
                spans.push(Span::styled("│", Style::new().fg(COLOR_MUTED)));
            }
            let cell = row.get(col).copied().unwrap_or("");
            let pad = width.saturating_sub(unicode_width::UnicodeWidthStr::width(cell));
            let style = if index == 0 {
                Style::new().fg(COLOR_TEXT).bold()
            } else {
                Style::new().fg(COLOR_TEXT)
            };
            spans.push(Span::styled(format!(" {cell}{} ", " ".repeat(pad)), style));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::default());
}

/// 将 tui-markdown 主题的样式统一映射到 Lucia 色板，避免与主题脱节的杂色。
///
/// 带背景的样式（一级标题、行内代码）改为前景强调色加粗；仅带前景色的装饰
/// （次级标题、列表标记、强调、链接）按加粗与否映射为正文色或次要色。
pub(crate) fn restyle_markdown(style: Style, accent: Color) -> Style {
    if style.bg.is_some() {
        let mut modifiers = style.add_modifier;
        // 下划线在终端里过于花哨，标题保留加粗与颜色即可。
        modifiers.remove(Modifier::UNDERLINED);
        return Style::new()
            .fg(accent)
            .add_modifier(modifiers | Modifier::BOLD);
    }
    if style.fg.is_some() {
        let modifiers = style.add_modifier;
        let fg = if modifiers.contains(Modifier::BOLD) {
            COLOR_TEXT
        } else {
            COLOR_MUTED
        };
        return Style::new().fg(fg).add_modifier(modifiers);
    }
    style
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
