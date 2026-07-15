//! 对话消息、Markdown 渲染、附件引用与剪贴板处理。

use super::*;

/// 输入框中等待随下一条消息发送的附件。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingAttachment {
    /// 输入框中展示的引用标签，如 `[Image#1]`、`[FILE#report.pdf]`。
    pub(crate) label: String,
    /// 文件名（不含路径）。
    pub(crate) name: String,
    /// MIME 类型。
    pub(crate) media_type: String,
    /// base64 编码的文件内容。
    pub(crate) data: String,
    /// 是否图片附件。
    pub(crate) is_image: bool,
}

/// 一次用户提交：含附件引用标签的文本与对应附件。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UserSubmission {
    /// 含附件引用标签的输入文本。
    pub(crate) text: String,
    /// 与文本中引用标签对应的附件，按插入顺序排列。
    pub(crate) attachments: Vec<PendingAttachment>,
}

impl UserSubmission {
    /// 转换为一次用户消息的内容块：文本在前，附件按加入顺序排在其后。
    pub(crate) fn blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::with_capacity(self.attachments.len() + 1);
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }
        for attachment in &self.attachments {
            blocks.push(if attachment.is_image {
                ContentBlock::Image {
                    media_type: attachment.media_type.clone(),
                    data: attachment.data.clone(),
                }
            } else {
                ContentBlock::File {
                    name: attachment.name.clone(),
                    media_type: attachment.media_type.clone(),
                    data: attachment.data.clone(),
                }
            });
        }
        blocks
    }
}

impl From<&str> for UserSubmission {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            attachments: Vec::new(),
        }
    }
}

impl From<String> for UserSubmission {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
        }
    }
}

// ─── 聊天消息 ───

pub(crate) enum MsgKind {
    User,
    Assistant,
    /// 工具运行中。
    ToolRunning,
    /// 工具成功完成。
    ToolOk,
    /// 工具失败。
    ToolError,
    /// 工具因 steering 被跳过。
    ToolSkipped,
    Error,
    Info,
    Extension,
}

pub(crate) struct Msg {
    pub(crate) kind: MsgKind,
    pub(crate) text: String,
    /// 工具消息关联的完整调用；普通消息为 `None`。
    pub(crate) tool_call: Option<ToolCall>,
    /// 工具消息关联的完整结果；运行中或普通消息为 `None`。
    pub(crate) tool_result: Option<ToolResult>,
    /// 工具未执行时的原因；其他消息为 `None`。
    pub(crate) skip_reason: Option<String>,
    /// 插件为当前工具消息返回的声明式帧。
    #[cfg(feature = "plugins")]
    pub(crate) tool_frame: Option<PluginUiFrame>,
    /// 当前插件帧对应的消息区宽度。
    #[cfg(feature = "plugins")]
    pub(crate) tool_frame_width: Option<u16>,
    /// 最新一次工具消息渲染请求的版本号。
    #[cfg(feature = "plugins")]
    pub(crate) tool_render_revision: u64,
    /// 当前仍在运行的渲染请求所使用的消息区宽度。
    #[cfg(feature = "plugins")]
    pub(crate) tool_render_pending_width: Option<u16>,
    /// 扩展事件使用的强调色。
    pub(crate) accent: Option<Color>,
    /// 是否以分隔线形式展示扩展事件。
    pub(crate) divider: bool,
}

impl Msg {
    /// 创建普通消息，工具专用字段留空。
    pub(crate) fn new(kind: MsgKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            tool_call: None,
            tool_result: None,
            skip_reason: None,
            #[cfg(feature = "plugins")]
            tool_frame: None,
            #[cfg(feature = "plugins")]
            tool_frame_width: None,
            #[cfg(feature = "plugins")]
            tool_render_revision: 0,
            #[cfg(feature = "plugins")]
            tool_render_pending_width: None,
            accent: None,
            divider: false,
        }
    }

    /// 创建运行中的工具消息，并以调用 ID 作为后续状态更新依据。
    pub(crate) fn tool_started(call: ToolCall) -> Self {
        let mut message = Self::new(MsgKind::ToolRunning, call.name.clone());
        message.tool_call = Some(call);
        message
    }

    /// 创建只有最终结果的工具消息，用于事件缺失时的安全恢复。
    pub(crate) fn tool_finished(result: ToolResult) -> Self {
        let kind = if result.is_error {
            MsgKind::ToolError
        } else {
            MsgKind::ToolOk
        };
        let mut message = Self::new(kind, result.name.clone());
        message.tool_result = Some(result);
        message
    }

    /// 返回工具消息的稳定调用 ID。
    pub(crate) fn tool_call_id(&self) -> Option<&str> {
        self.tool_call
            .as_ref()
            .map(|call| call.id.as_str())
            .or_else(|| {
                self.tool_result
                    .as_ref()
                    .map(|result| result.call_id.as_str())
            })
    }

    /// 创建由扩展事件驱动的主事件列表消息。
    pub(crate) fn extension(text: impl Into<String>, color: Color, divider: bool) -> Self {
        Self {
            kind: MsgKind::Extension,
            text: text.into(),
            tool_call: None,
            tool_result: None,
            skip_reason: None,
            #[cfg(feature = "plugins")]
            tool_frame: None,
            #[cfg(feature = "plugins")]
            tool_frame_width: None,
            #[cfg(feature = "plugins")]
            tool_render_revision: 0,
            #[cfg(feature = "plugins")]
            tool_render_pending_width: None,
            accent: Some(color),
            divider,
        }
    }

    /// 将消息转换为行首标记加正文的紧凑样式，并标记正在流式生成的消息。
    ///
    /// `width` 为消息区可用显示宽度，用户消息按它预换行以保持竖条连续。
    pub(crate) fn to_lines(&self, streaming: bool, width: u16) -> Vec<Line<'_>> {
        match self.kind {
            MsgKind::User => user_lines(&self.text, width),
            MsgKind::Assistant => markdown_lines(&self.text, streaming),
            MsgKind::ToolRunning => self.tool_lines("", COLOR_WARNING, width),
            MsgKind::ToolOk => self.tool_lines("", COLOR_SUCCESS, width),
            MsgKind::ToolError => self.tool_lines("failed", COLOR_DANGER, width),
            MsgKind::ToolSkipped => self.tool_lines("skipped", COLOR_MUTED, width),
            MsgKind::Error => {
                conversation_lines("✗", &self.text, COLOR_DANGER, COLOR_DANGER, false)
            }
            // Info 行作为块结束标记（token 统计、插话提示），追加空行与后续消息分隔。
            MsgKind::Info => vec![
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(self.text.as_str(), Style::new().fg(COLOR_MUTED)),
                ]),
                Line::default(),
            ],
            MsgKind::Extension => {
                let color = self.accent.unwrap_or(COLOR_MUTED);
                let text = if self.divider {
                    format!("── {} ──", self.text)
                } else {
                    self.text.clone()
                };
                vec![
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(text, Style::new().fg(color).bold()),
                    ]),
                    Line::default(),
                ]
            }
        }
    }

    /// 构造工具调用块：`● 名称(参数)` 加 `└ 返回摘要` 与缩进的多行预览，
    /// 圆点以状态色区分，`+`/`-` 前缀的预览行按 diff 着色，块尾留空行。
    ///
    /// 摘要与预览行按 `width` 显示宽度截断，避免窄终端下自动换行
    /// 把续行顶到行首、破坏缩进对齐。
    pub(crate) fn tool_lines(&self, note: &str, color: Color, width: u16) -> Vec<Line<'_>> {
        #[cfg(feature = "plugins")]
        if let Some(frame) = self
            .tool_frame
            .as_ref()
            .filter(|frame| frame.visible && !frame.lines.is_empty())
        {
            let mut lines = tui::plugin_frame_lines(frame);
            lines.push(Line::default());
            return lines;
        }

        // 前缀占 4 列，再留 1 列吸收截断省略号，保证整行不超宽。
        let preview_width = usize::from(width).saturating_sub(5).max(1);
        let mut first = vec![
            Span::styled("● ", Style::new().fg(color)),
            Span::styled(self.text.as_str(), Style::new().fg(COLOR_TEXT).bold()),
        ];
        if let Some(args) = self
            .tool_call
            .as_ref()
            .map(|call| summarize_json(&call.args, 64))
            .filter(|args| !args.is_empty())
        {
            first.push(Span::styled(
                format!("({args})"),
                Style::new().fg(COLOR_MUTED),
            ));
        }
        if !note.is_empty() {
            first.push(Span::styled(format!("  {note}"), Style::new().fg(color)));
        }
        let mut lines = vec![Line::from(first)];
        let result_color = if matches!(self.kind, MsgKind::ToolError) {
            COLOR_DANGER
        } else {
            COLOR_MUTED
        };
        let result_lines = self
            .tool_result
            .as_ref()
            .map(|result| tool_result_lines(&result.content, TOOL_RESULT_PREVIEW_LINES, 96))
            .unwrap_or_default();
        if let Some(result) = result_lines.first() {
            lines.push(Line::from(vec![
                Span::styled("  └ ", Style::new().fg(COLOR_MUTED)),
                Span::styled(
                    truncate_line(result, preview_width),
                    Style::new().fg(result_color),
                ),
            ]));
        }
        for detail in result_lines.iter().skip(1) {
            let detail_color = if matches!(self.kind, MsgKind::ToolError) {
                COLOR_DANGER
            } else if detail.starts_with('+') {
                COLOR_SUCCESS
            } else if detail.starts_with('-') {
                COLOR_DANGER
            } else {
                COLOR_MUTED
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    truncate_line(&detail, preview_width),
                    Style::new().fg(detail_color),
                ),
            ]));
        }
        lines.push(Line::default());
        lines
    }
}

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

/// 工具结果预览保留的最大行数。
pub(crate) const TOOL_RESULT_PREVIEW_LINES: usize = 6;

/// 从工具结果 JSON 中提取多行文本预览：首行作摘要，其余行缩进展示。
///
/// 文本型 `content` 按行拆分并按显示宽度截断；非文本结构退回单行 JSON 摘要；
/// 超出预览行数时以省略行标注总行数。
pub(crate) fn tool_result_lines(value: &Value, max_lines: usize, max_width: usize) -> Vec<String> {
    let text = match value {
        Value::Object(map) => match map.get("content") {
            Some(Value::String(text)) => text.as_str(),
            _ => return vec![summarize_json(value, max_width)],
        },
        Value::String(text) => text.as_str(),
        other => return vec![summarize_json(other, max_width)],
    };
    let total = text.lines().count();
    let mut lines: Vec<String> = text
        .lines()
        .take(max_lines)
        .map(|line| truncate_line(line.trim_end(), max_width))
        .collect();
    if total > max_lines {
        lines.push(format!("… 共 {total} 行"));
    }
    lines
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

/// 从粘贴内容解析拖拽的单个文件路径。
///
/// 支持引号包裹与反斜杠转义（macOS 终端拖入文件的两种形态）以及 `~/` 前缀；
/// 仅接受指向普通文件的绝对路径，其余内容按普通文本粘贴处理。
pub(crate) fn pasted_file_path(pasted: &str) -> Option<PathBuf> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .map(str::to_string)
        .unwrap_or_else(|| unescape_shell_path(trimmed));
    let path = if let Some(rest) = unquoted.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => PathBuf::from(home).join(rest),
            _ => return None,
        }
    } else {
        PathBuf::from(&unquoted)
    };
    if !path.is_absolute() {
        return None;
    }
    std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|_| path)
}

/// 去掉终端拖拽路径中的反斜杠转义（如 `\ ` 与 `\(`）。
pub(crate) fn unescape_shell_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 根据扩展名与内容推断附件 MIME 类型，返回 `(media_type, 是否图片)`。
///
/// 图片仅识别主流模型接口支持的四种格式；未知扩展名按内容判断：
/// 可解码为 UTF-8 视为文本，否则视为二进制。
pub(crate) fn attachment_media_type(path: &Path, bytes: &[u8]) -> (String, bool) {
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let media_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        _ => {
            if std::str::from_utf8(bytes).is_ok() {
                "text/plain"
            } else {
                "application/octet-stream"
            }
        }
    };
    (media_type.to_string(), media_type.starts_with("image/"))
}

/// 将文本复制到系统剪贴板。
///
/// 优先调用平台剪贴板命令（pbcopy/wl-copy/xclip/xsel/clip）；命令不可用时
/// 回退为 OSC 52 转义序列，支持 SSH 远程终端。
pub(crate) fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine;
    use std::io::Write;

    if copy_via_command(text) {
        return Ok(());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;
    Ok(())
}

/// 尝试通过平台剪贴板命令写入文本，返回是否成功。
pub(crate) fn copy_via_command(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&["pbcopy"]];
    #[cfg(target_os = "windows")]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[&[&str]] = &[
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
        &["xsel", "--clipboard", "--input"],
    ];

    for candidate in candidates {
        let Ok(mut child) = Command::new(candidate[0])
            .args(&candidate[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let written = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if written && child.wait().is_ok_and(|status| status.success()) {
            return true;
        }
    }
    false
}
