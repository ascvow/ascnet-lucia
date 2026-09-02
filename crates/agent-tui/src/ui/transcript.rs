//! 工具调用块的转录排版。
//!
//! 目标是让每个工具调用一眼能读出"对什么做了什么、结果如何"：
//!
//! ```text
//! ● Write(src/tmp-debug.ts)
//!   └ 写入 36 行到 src/tmp-debug.ts
//!       1 /** 临时排查脚本。 */
//!       2 import { z } from 'zod'
//!     … +27 行
//! ```
//!
//! 本模块只负责排版，不认识具体插件工具：无法识别的工具退回通用 JSON 摘要，
//! 因此新增工具不需要改这里就能得到可读的默认呈现。

use crate::theme::{
    COLOR_DANGER, COLOR_DIFF_ADD_BG, COLOR_DIFF_ADD_FG, COLOR_DIFF_DEL_BG, COLOR_DIFF_DEL_FG,
    COLOR_LINE_NUMBER, COLOR_MUTED, COLOR_TEXT,
};
use crate::ui::highlight::{highlight_line, CodeLanguage};
use crate::ui::markdown::{summarize_json, truncate_line};
use agent_tool::{ToolCall, ToolResult};
use ratatui::prelude::*;
use serde_json::Value;

/// 工具标题中目标文本的最大显示宽度。
const MAX_TARGET_WIDTH: usize = 56;
/// 结果摘要行的最大显示宽度。
const MAX_SUMMARY_WIDTH: usize = 120;

/// 读取字符串字段。
fn text_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// 读取整数字段。
fn number_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// 把绝对路径压缩为相对当前工作目录的短路径，便于在窄终端阅读。
fn shorten_path(path: &str) -> String {
    let relative = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            let cwd = cwd.to_string_lossy().to_string();
            path.strip_prefix(&format!("{cwd}/")).map(str::to_string)
        })
        .unwrap_or_else(|| path.to_string());
    if unicode_width::UnicodeWidthStr::width(relative.as_str()) <= MAX_TARGET_WIDTH {
        return relative;
    }
    // 超长路径保留末尾的文件名部分，前缀折叠为省略号：定位文件时文件名比目录更关键。
    let tail: String = relative
        .chars()
        .rev()
        .take(MAX_TARGET_WIDTH - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// 返回工具标题括号中展示的目标。
///
/// 文件类工具用路径，搜索类用模式，命令类用命令本身；未识别的工具退回通用 JSON
/// 摘要，保证第三方插件工具也有可读标题。
pub(crate) fn tool_target(call: &ToolCall) -> Option<String> {
    let args = &call.args;
    let target = match call.name.as_str() {
        "read_file" | "write_file" | "list_directory" => text_field(args, "path").map(shorten_path),
        "search_files" => text_field(args, "pattern").map(str::to_string),
        "shell" => text_field(args, "command").map(str::to_string),
        _ => None,
    };
    let target = target.or_else(|| {
        // 通用回退：优先用常见的目标型字段，其次压缩整个参数对象。
        for key in [
            "path", "file", "target", "name", "query", "pattern", "command",
        ] {
            if let Some(value) = text_field(args, key) {
                return Some(value.to_string());
            }
        }
        let summary = summarize_json(args, MAX_TARGET_WIDTH);
        (!summary.is_empty()).then_some(summary)
    })?;
    let target = truncate_line(&target, MAX_TARGET_WIDTH);
    (!target.is_empty()).then_some(target)
}

/// 返回工具结果的一行摘要，即 `└` 行的内容。
///
/// 已知工具给出面向人的描述（写入多少行、命中多少处），未知工具退回结果正文首行。
pub(crate) fn tool_summary(call: Option<&ToolCall>, result: &ToolResult) -> Option<String> {
    if result.is_error {
        let text = result_body(result).unwrap_or_else(|| result.content_text());
        let first = text.lines().next().unwrap_or_default().trim();
        return (!first.is_empty()).then(|| truncate_line(first, MAX_SUMMARY_WIDTH));
    }

    let content = &result.content;
    let args = call.map(|call| &call.args);
    let summary = match result.name.as_str() {
        "write_file" => {
            let path = text_field(content, "path").map(shorten_path).or_else(|| {
                args.and_then(|args| text_field(args, "path"))
                    .map(shorten_path)
            })?;
            // 结果只回传字节数，行数按写入内容自行统计，与界面展示的预览一致。
            let lines = args
                .and_then(|args| text_field(args, "content"))
                .map_or(0, |content| content.lines().count());
            Some(format!("写入 {lines} 行到 {path}"))
        }
        "read_file" => {
            let path = args
                .and_then(|args| text_field(args, "path"))
                .map(shorten_path)?;
            // 缺少计数字段时按正文实际行数统计，避免报出误导性的"读取 0 行"。
            let read = number_field(content, "lines_read").unwrap_or_else(|| {
                result_body(result).map_or(0, |body| body.lines().count() as u64)
            });
            let total = number_field(content, "total_lines").unwrap_or(read);
            Some(if read == total {
                format!("读取 {read} 行，来自 {path}")
            } else {
                format!("读取 {read} 行（共 {total} 行），来自 {path}")
            })
        }
        "list_directory" => {
            let path = text_field(content, "path").map(shorten_path).or_else(|| {
                args.and_then(|args| text_field(args, "path"))
                    .map(shorten_path)
            })?;
            let count = number_field(content, "count").unwrap_or_default();
            Some(format!("{count} 个条目，来自 {path}"))
        }
        "search_files" => {
            let count = number_field(content, "count").unwrap_or_default();
            let truncated = content
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(if truncated {
                format!("命中 {count} 处（已截断）")
            } else {
                format!("命中 {count} 处")
            })
        }
        "shell" => {
            let stdout = text_field(content, "stdout").unwrap_or_default();
            let first = stdout.lines().find(|line| !line.trim().is_empty());
            match first {
                Some(line) => Some(truncate_line(line.trim_end(), MAX_SUMMARY_WIDTH)),
                // 只有确实拿到退出码时才报告它；缺字段说明结果不是标准 shell 形状。
                None => number_field(content, "exit_code").map(|exit| format!("退出码 {exit}")),
            }
        }
        _ => None,
    };
    let summary = summary.or_else(|| {
        let text = result_body(result)?;
        let rows: Vec<&str> = text.lines().collect();
        if looks_like_diff(&rows) {
            return Some(diff_summary(&rows));
        }
        let first = rows.iter().find(|line| !line.trim().is_empty())?;
        Some(truncate_line(first.trim_end(), MAX_SUMMARY_WIDTH))
    })?;
    (!summary.is_empty()).then_some(summary)
}

/// 预览块中的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewLine {
    /// 普通内容行；`number` 为原文件行号，未知时为 `None`。
    Context {
        /// 原文件中的行号。
        number: Option<usize>,
        /// 行正文。
        text: String,
    },
    /// diff 新增行。
    Added {
        /// 原文件中的行号。
        number: Option<usize>,
        /// 行正文，不含 `+` 标记。
        text: String,
    },
    /// diff 删除行。
    Removed {
        /// 原文件中的行号。
        number: Option<usize>,
        /// 行正文，不含 `-` 标记。
        text: String,
    },
    /// 省略提示，展示被折叠的行数。
    Elided {
        /// 被折叠的行数。
        hidden: usize,
    },
}

/// 剥离工具在正文前附加的行号前缀，返回行号与正文。
///
/// 支持 `12\t正文` 与 `12→正文` 两种写法：前者是原生 `read_file` 的格式，后者是
/// 常见的插件写法。不匹配时原样返回，避免把正文里恰好以数字开头的行当成行号。
fn split_line_number(text: &str) -> (Option<usize>, &str) {
    let trimmed = text.trim_start();
    let Some((head, tail)) = trimmed.split_once('\t').or_else(|| trimmed.split_once('→')) else {
        return (None, text);
    };
    match head.trim().parse::<usize>() {
        Ok(number) => (Some(number), tail),
        Err(_) => (None, text),
    }
}

/// 判断整段正文是否都带行号前缀。
///
/// 只有全部非空行都匹配时才剥离，避免把正文中偶然以数字加制表符开头的行误判为行号。
fn is_line_numbered(rows: &[&str]) -> bool {
    let mut numbered = 0;
    for row in rows {
        if row.trim().is_empty() {
            continue;
        }
        if split_line_number(row).0.is_none() {
            return false;
        }
        numbered += 1;
    }
    numbered > 0
}

/// 把字节数格式化为紧凑可读的体积。
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 把数组型结果展开为逐条预览行。
///
/// 目录列表与搜索结果的正文是数组而不是文本，压成单行 JSON 摘要会丢掉最有价值的
/// 条目本身（`entries: [3 项]` 对用户毫无信息量）。这里按已知形状逐条展开；
/// 形状不匹配时返回 `None`，由调用方退回通用路径。
fn collection_preview(result: &ToolResult, max_lines: usize) -> Option<Vec<PreviewLine>> {
    let rows: Vec<String> = match result.name.as_str() {
        "list_directory" => result
            .content
            .get("entries")?
            .as_array()?
            .iter()
            .map(|entry| {
                let name = text_field(entry, "name").unwrap_or("?");
                let kind = text_field(entry, "type").unwrap_or("file");
                // 目录以 `/` 结尾，与 shell 的列目录习惯一致；文件补上体积。
                match (kind, number_field(entry, "size")) {
                    ("dir", _) => format!("{name}/"),
                    (_, Some(size)) => format!("{name}  {}", format_bytes(size)),
                    _ => name.to_string(),
                }
            })
            .collect(),
        "search_files" => result
            .content
            .get("matches")?
            .as_array()?
            .iter()
            .map(|item| {
                let file = text_field(item, "file").unwrap_or("?");
                let line = number_field(item, "line").unwrap_or_default();
                let content = text_field(item, "content").unwrap_or_default().trim_end();
                format!("{file}:{line}  {content}")
            })
            .collect(),
        _ => return None,
    };

    let total = rows.len();
    let mut lines: Vec<PreviewLine> = rows
        .into_iter()
        .take(max_lines)
        .map(|text| PreviewLine::Context { number: None, text })
        .collect();
    if total > max_lines {
        lines.push(PreviewLine::Elided {
            hidden: total - max_lines,
        });
    }
    Some(lines)
}

/// 判断一段文本是否是统一 diff：存在 `+`/`-` 前缀行且不是普通正文。
fn looks_like_diff(lines: &[&str]) -> bool {
    let marked = lines
        .iter()
        .filter(|line| {
            (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
        })
        .count();
    marked > 0 && marked * 2 >= lines.len()
}

/// 从结构化工具结果中取出可逐行展开的正文。
///
/// 约定的正文字段优先：`content` 覆盖文件与通用工具，`stdout`/`stderr` 覆盖命令类。
/// 都不存在时返回 `None`，由调用方退回 JSON 摘要——直接序列化整个对象会把
/// `{"content":"..."}` 这类信封当成正文，预览也就失去意义。
fn result_body(result: &ToolResult) -> Option<String> {
    match &result.content {
        Value::String(text) => Some(text.clone()),
        Value::Object(_) => {
            for key in ["content", "error"] {
                if let Some(text) = text_field(&result.content, key) {
                    return Some(text.to_string());
                }
            }
            let stdout = text_field(&result.content, "stdout").unwrap_or_default();
            let stderr = text_field(&result.content, "stderr").unwrap_or_default();
            let combined = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
                (true, true) => return None,
                (false, true) => stdout.to_string(),
                (true, false) => stderr.to_string(),
                (false, false) => format!("{stdout}\n{stderr}"),
            };
            Some(combined)
        }
        _ => None,
    }
}

/// 统计统一 diff 的增删行数并给出面向人的摘要。
fn diff_summary(rows: &[&str]) -> String {
    let added = rows
        .iter()
        .filter(|row| row.starts_with('+') && !row.starts_with("+++"))
        .count();
    let removed = rows
        .iter()
        .filter(|row| row.starts_with('-') && !row.starts_with("---"))
        .count();
    match (added, removed) {
        (0, 0) => "无改动".to_string(),
        (added, 0) => format!("新增 {added} 行"),
        (0, removed) => format!("删除 {removed} 行"),
        (added, removed) => format!("新增 {added} 行，删除 {removed} 行"),
    }
}

/// 把工具结果正文转换为预览行。
///
/// 识别三种形态：统一 diff、带 `12→` 前缀的文件内容、以及普通文本；
/// 结构化但非文本的结果退回单行 JSON 摘要。
pub(crate) fn preview_lines(
    call: Option<&ToolCall>,
    result: &ToolResult,
    max_lines: usize,
) -> Vec<PreviewLine> {
    // 写入类工具的结果只回传路径与字节数，真正值得预览的是写进去的内容。
    let written = (result.name == "write_file" && !result.is_error)
        .then(|| call.and_then(|call| text_field(&call.args, "content")))
        .flatten()
        .map(str::to_string);
    if let Some(lines) = collection_preview(result, max_lines) {
        return lines;
    }
    let text = written.or_else(|| result_body(result)).unwrap_or_default();
    // 单行错误已经完整出现在摘要行，再编一次号只会重复同一句话。
    if result.is_error && text.lines().filter(|line| !line.trim().is_empty()).count() <= 1 {
        return Vec::new();
    }
    if text.trim().is_empty() {
        // 非文本结构没有正文可展开，用 JSON 摘要兜底；空值不产生预览行。
        if result.content.is_null() {
            return Vec::new();
        }
        let summary = summarize_json(&result.content, MAX_SUMMARY_WIDTH);
        return if summary.is_empty() {
            Vec::new()
        } else {
            vec![PreviewLine::Context {
                number: None,
                text: summary,
            }]
        };
    }

    let rows: Vec<&str> = text.lines().collect();
    let total = rows.len();
    let is_diff = looks_like_diff(&rows);
    // 只有整段都带行号前缀时才剥离，避免正文里偶然以"数字+制表符"开头的行被误拆。
    let numbered = !is_diff && is_line_numbered(&rows);
    let mut lines: Vec<PreviewLine> = Vec::with_capacity(max_lines);
    for (index, row) in rows.iter().take(max_lines).enumerate() {
        if is_diff {
            if let Some(body) = row.strip_prefix('+').filter(|_| !row.starts_with("+++")) {
                lines.push(PreviewLine::Added {
                    number: Some(index + 1),
                    text: body.to_string(),
                });
                continue;
            }
            if let Some(body) = row.strip_prefix('-').filter(|_| !row.starts_with("---")) {
                lines.push(PreviewLine::Removed {
                    number: Some(index + 1),
                    text: body.to_string(),
                });
                continue;
            }
        }
        // 统一 diff 的上下文行带一个前导空格标记，展示时应还原为原始缩进。
        let row = if is_diff {
            row.strip_prefix(' ').unwrap_or(row)
        } else {
            row
        };
        let (number, body) = if numbered {
            split_line_number(row)
        } else {
            (None, row as &str)
        };
        lines.push(PreviewLine::Context {
            number: number.or(Some(index + 1)),
            text: body.trim_end().to_string(),
        });
    }
    if total > max_lines {
        lines.push(PreviewLine::Elided {
            hidden: total - max_lines,
        });
    }
    lines
}

/// 把运行期增量输出转换为预览行；只保留最新若干行，不编行号。
pub(crate) fn output_preview_lines(text: &str, max_lines: usize) -> Vec<PreviewLine> {
    let rows: Vec<&str> = text.lines().collect();
    let start = rows.len().saturating_sub(max_lines);
    let mut lines: Vec<PreviewLine> = Vec::with_capacity(max_lines + 1);
    if start > 0 {
        lines.push(PreviewLine::Elided { hidden: start });
    }
    lines.extend(rows[start..].iter().map(|row| PreviewLine::Context {
        number: None,
        text: row.trim_end().to_string(),
    }));
    lines
}

/// 行号列宽度：右对齐到该宽度，与 Claude Code 的预览观感一致。
const GUTTER_WIDTH: usize = 4;
/// 预览块相对消息左边缘的缩进。
const PREVIEW_INDENT: &str = "    ";

/// 渲染行号列。
fn gutter(number: Option<usize>, color: Color) -> Span<'static> {
    let text = number.map_or_else(
        || " ".repeat(GUTTER_WIDTH),
        |number| format!("{number:>GUTTER_WIDTH$}"),
    );
    Span::styled(text, Style::new().fg(color))
}

/// 把内容补齐到指定显示宽度，使整行背景色铺满。
fn pad_to(spans: &mut Vec<Span<'static>>, used: usize, width: usize, style: Style) {
    if width > used {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
}

/// 计算若干片段的总显示宽度。
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// 把预览行渲染为终端行。
///
/// `width` 是消息区总宽度；diff 行按它补齐背景。`language` 决定内容行的高亮规则，
/// `error` 为真时内容统一使用错误色，不做语法着色。
pub(crate) fn render_preview(
    lines: &[PreviewLine],
    width: u16,
    language: CodeLanguage,
    error: bool,
) -> Vec<Line<'static>> {
    let total_width = usize::from(width).max(1);
    // 目录、搜索一类的条目没有行号，整块都没有时收起号位，避免白占一列缩进。
    let show_gutter = lines.iter().any(|line| match line {
        PreviewLine::Context { number, .. }
        | PreviewLine::Added { number, .. }
        | PreviewLine::Removed { number, .. } => number.is_some(),
        PreviewLine::Elided { .. } => false,
    });
    let gutter_width = if show_gutter { GUTTER_WIDTH } else { 0 };
    // 缩进、行号列与标记列之后剩余的正文宽度。
    let body_width = total_width
        .saturating_sub(PREVIEW_INDENT.len() + gutter_width + 2)
        .max(1);

    lines
        .iter()
        .map(|line| match line {
            PreviewLine::Elided { hidden } => Line::from(vec![
                Span::raw(PREVIEW_INDENT),
                Span::styled(format!("… +{hidden} 行"), Style::new().fg(COLOR_MUTED)),
            ]),
            PreviewLine::Context { number, text } => {
                let mut spans = vec![Span::raw(PREVIEW_INDENT)];
                if show_gutter {
                    spans.push(gutter(*number, COLOR_LINE_NUMBER));
                    spans.push(Span::raw(" "));
                }
                let body = truncate_line(text, body_width);
                if error {
                    spans.push(Span::styled(body, Style::new().fg(COLOR_DANGER)));
                } else {
                    spans.extend(highlight_line(&body, language));
                }
                Line::from(spans)
            }
            PreviewLine::Added { number, text } | PreviewLine::Removed { number, text } => {
                let added = matches!(line, PreviewLine::Added { .. });
                let (background, marker_color, marker) = if added {
                    (COLOR_DIFF_ADD_BG, COLOR_DIFF_ADD_FG, "+")
                } else {
                    (COLOR_DIFF_DEL_BG, COLOR_DIFF_DEL_FG, "-")
                };
                let row_style = Style::new().bg(background);
                let mut spans = vec![Span::raw(PREVIEW_INDENT)];
                if show_gutter {
                    spans.push(gutter(*number, marker_color).patch_style(row_style));
                }
                spans.push(Span::styled(marker.to_string(), row_style.fg(marker_color)));
                let body = truncate_line(text, body_width);
                // diff 正文保持语法着色，只叠加整行背景，便于同时读出改动与语义。
                if error {
                    spans.push(Span::styled(body, row_style.fg(COLOR_DANGER)));
                } else {
                    spans.extend(
                        highlight_line(&body, language)
                            .into_iter()
                            .map(|span| span.patch_style(row_style)),
                    );
                }
                // 缩进不参与背景，因此从行号列开始计算已占宽度。
                let used = spans_width(&spans) - PREVIEW_INDENT.len();
                pad_to(
                    &mut spans,
                    used,
                    total_width.saturating_sub(PREVIEW_INDENT.len()),
                    row_style,
                );
                Line::from(spans)
            }
        })
        .collect()
}

/// 推断工具预览应使用的高亮语言。
pub(crate) fn preview_language(call: Option<&ToolCall>, result: &ToolResult) -> CodeLanguage {
    let path = call
        .and_then(|call| text_field(&call.args, "path"))
        .or_else(|| text_field(&result.content, "path"));
    path.map_or(CodeLanguage::Plain, CodeLanguage::from_path)
}

/// 构造工具标题行的展示名称。
///
/// 内建工具映射为首字母大写的动词形式，与 Claude Code 的观感一致；
/// 插件工具保留注册时的原名，避免宿主猜测第三方命名。
pub(crate) fn tool_display_name(name: &str) -> &str {
    match name {
        "read_file" => "Read",
        "write_file" => "Write",
        "list_directory" => "List",
        "search_files" => "Search",
        "shell" => "Shell",
        other => other,
    }
}

/// 渲染工具标题行左侧的正文片段（不含状态圆点）。
pub(crate) fn tool_title_spans(name: &str, target: Option<&str>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        tool_display_name(name).to_string(),
        Style::new().fg(COLOR_TEXT).bold(),
    )];
    if let Some(target) = target {
        spans.push(Span::styled("(", Style::new().fg(COLOR_MUTED)));
        spans.push(Span::styled(
            target.to_string(),
            Style::new().fg(COLOR_MUTED),
        ));
        spans.push(Span::styled(")", Style::new().fg(COLOR_MUTED)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 拼接一行的全部片段文本。
    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// 文件类工具的标题目标使用路径，而不是整个参数对象。
    #[test]
    fn file_tools_use_path_as_target() {
        let call = ToolCall::new(
            "c",
            "write_file",
            json!({"path": "src/main.rs", "content": "x"}),
        );
        assert_eq!(tool_target(&call).as_deref(), Some("src/main.rs"));
    }

    /// 未识别的插件工具退回通用 JSON 摘要，不会没有标题目标。
    #[test]
    fn unknown_tools_fall_back_to_json_summary() {
        let call = ToolCall::new("c", "custom_tool", json!({"mode": "fast", "retries": 2}));
        let target = tool_target(&call).expect("未知工具也应有目标摘要");
        assert!(target.contains("mode"), "{target:?}");
    }

    /// 写入结果摘要按实际写入内容统计行数。
    #[test]
    fn write_summary_reports_line_count() {
        let call = ToolCall::new(
            "c",
            "write_file",
            json!({"path": "a.rs", "content": "one\ntwo\nthree"}),
        );
        let result = ToolResult::success(
            "c",
            "write_file",
            json!({"path": "a.rs", "bytes_written": 13}),
        );

        assert_eq!(
            tool_summary(Some(&call), &result).as_deref(),
            Some("写入 3 行到 a.rs")
        );
    }

    /// 部分读取时摘要必须同时给出已读行数与总行数。
    #[test]
    fn read_summary_distinguishes_partial_reads() {
        let call = ToolCall::new("c", "read_file", json!({"path": "a.rs"}));
        let partial = ToolResult::success(
            "c",
            "read_file",
            json!({"content": "x", "lines_read": 20, "total_lines": 100}),
        );
        let full = ToolResult::success(
            "c",
            "read_file",
            json!({"content": "x", "lines_read": 20, "total_lines": 20}),
        );

        assert_eq!(
            tool_summary(Some(&call), &partial).as_deref(),
            Some("读取 20 行（共 100 行），来自 a.rs")
        );
        assert_eq!(
            tool_summary(Some(&call), &full).as_deref(),
            Some("读取 20 行，来自 a.rs")
        );
    }

    /// 统一 diff 必须被识别为增删行，而不是普通正文。
    #[test]
    fn unified_diff_is_split_into_added_and_removed() {
        let result = ToolResult::success(
            "c",
            "apply_patch",
            json!({"content": "-old line\n+new line\n context"}),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Removed {
                number: Some(1),
                text: "old line".into()
            }
        );
        assert_eq!(
            lines[1],
            PreviewLine::Added {
                number: Some(2),
                text: "new line".into()
            }
        );
        assert!(matches!(lines[2], PreviewLine::Context { .. }));
    }

    /// `read_file` 的 `12→` 前缀应还原为行号列，不重复出现在正文里。
    #[test]
    fn numbered_content_reuses_original_line_numbers() {
        let result = ToolResult::success(
            "c",
            "read_file",
            json!({"content": "  10→fn main() {\n  11→}"}),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: Some(10),
                text: "fn main() {".into()
            }
        );
    }

    /// 超出预览上限时以 `… +N 行` 标注被折叠的行数。
    #[test]
    fn overflow_is_reported_as_hidden_line_count() {
        let content = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let result = ToolResult::success("c", "shell", json!({ "content": content }));

        let lines = preview_lines(None, &result, 4);

        assert_eq!(lines.last(), Some(&PreviewLine::Elided { hidden: 6 }));
        let rendered = render_preview(&lines, 60, CodeLanguage::Plain, false);
        assert!(text_of(rendered.last().expect("应有省略行")).contains("… +6 行"));
    }

    /// diff 行必须铺满整行背景，否则终端里会出现半截色块。
    #[test]
    fn diff_rows_fill_the_full_width() {
        let lines = vec![PreviewLine::Added {
            number: Some(1),
            text: "let x = 1;".into(),
        }];

        let rendered = render_preview(&lines, 40, CodeLanguage::CLike, false);
        let line = &rendered[0];
        let background = line
            .spans
            .iter()
            .filter(|span| span.style.bg == Some(COLOR_DIFF_ADD_BG))
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>();

        assert_eq!(background + PREVIEW_INDENT.len(), 40);
    }

    /// 内建工具名映射为动词形式，插件工具保留原名。
    #[test]
    fn builtin_tools_use_verb_display_names() {
        assert_eq!(tool_display_name("write_file"), "Write");
        assert_eq!(tool_display_name("mcp_fetch"), "mcp_fetch");
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::ui::wrapped_line_count;
    use serde_json::json;

    /// 铺满宽度的 diff 行必须仍然只占一个视觉行。
    ///
    /// 预览行按整行宽度补齐背景，如果换行估算把它算成两行，滚动上限就会大于实际
    /// 内容高度，界面底部会出现翻不到底的空白。
    #[test]
    fn full_width_diff_rows_occupy_a_single_visual_row() {
        for width in [40u16, 72, 96, 120] {
            let lines = render_preview(
                &[
                    PreviewLine::Added {
                        number: Some(1),
                        text: "let value = compute(input);".into(),
                    },
                    PreviewLine::Removed {
                        number: Some(2),
                        text: "let value = old(input);".into(),
                    },
                ],
                width,
                CodeLanguage::CLike,
                false,
            );

            for line in &lines {
                assert_eq!(
                    wrapped_line_count(line, width),
                    1,
                    "宽度 {width} 下的 diff 行应只占一行"
                );
            }
        }
    }

    /// 单行错误只保留摘要，不再重复编号展示同一句。
    #[test]
    fn single_line_errors_are_not_duplicated_in_preview() {
        let result = ToolResult::error("c", "shell", "error[E0432]: unresolved import");

        assert_eq!(
            tool_summary(None, &result).as_deref(),
            Some("error[E0432]: unresolved import")
        );
        assert!(preview_lines(None, &result, 6).is_empty());
    }

    /// 多行错误仍然展开预览，便于定位具体位置。
    #[test]
    fn multi_line_errors_keep_their_preview() {
        let result = ToolResult::error("c", "shell", "error: 失败\n  --> src/main.rs:3:1\n  |");

        assert_eq!(preview_lines(None, &result, 6).len(), 3);
    }

    /// 写入类工具预览的是写进去的内容，而不是结果信封字段。
    #[test]
    fn write_preview_shows_written_content() {
        let call = ToolCall::new(
            "c",
            "write_file",
            json!({"path": "a.rs", "content": "fn main() {}\n"}),
        );
        let result = ToolResult::success(
            "c",
            "write_file",
            json!({"path": "a.rs", "bytes_written": 13}),
        );

        let lines = preview_lines(Some(&call), &result, 6);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: Some(1),
                text: "fn main() {}".into()
            }
        );
    }
}

impl PreviewLine {
    /// 判断该行是否为省略提示。
    pub(crate) fn is_elided(&self) -> bool {
        matches!(self, Self::Elided { .. })
    }
}

#[cfg(test)]
mod result_shape_tests {
    use super::*;
    use serde_json::json;

    /// `read_file` 用 `N\t` 前缀编号，必须还原为行号列而不是留在正文里重复出现。
    #[test]
    fn tab_numbered_content_becomes_the_gutter() {
        let result = ToolResult::success(
            "c",
            "read_file",
            json!({ "content": "1\t{\n2\t  \"name\": \"demo\"" }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: Some(1),
                text: "{".into()
            }
        );
        assert_eq!(
            lines[1],
            PreviewLine::Context {
                number: Some(2),
                text: "  \"name\": \"demo\"".into()
            }
        );
    }

    /// 正文中偶发的"数字+制表符"不得被误判为行号，否则会吞掉真实内容。
    #[test]
    fn partially_numbered_text_keeps_original_lines() {
        let result = ToolResult::success(
            "c",
            "shell",
            json!({ "content": "普通输出\n42\t被误判的一行" }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[1],
            PreviewLine::Context {
                number: Some(2),
                text: "42\t被误判的一行".into()
            }
        );
    }

    /// 目录结果必须逐条展开，而不是压成 `entries: [N 项]` 这种无信息量的摘要。
    #[test]
    fn directory_entries_are_listed_individually() {
        let result = ToolResult::success(
            "c",
            "list_directory",
            json!({
                "path": "crates",
                "count": 2,
                "entries": [
                    {"name": "agent-core", "type": "dir", "size": null},
                    {"name": "README.md", "type": "file", "size": 2048}
                ]
            }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: None,
                text: "agent-core/".into()
            }
        );
        assert_eq!(
            lines[1],
            PreviewLine::Context {
                number: None,
                text: "README.md  2.0 KB".into()
            }
        );
        assert_eq!(
            tool_summary(None, &result).as_deref(),
            Some("2 个条目，来自 crates")
        );
    }

    /// 搜索命中必须展示文件、行号与命中正文。
    #[test]
    fn search_matches_show_file_line_and_content() {
        let result = ToolResult::success(
            "c",
            "search_files",
            json!({
                "count": 1,
                "truncated": false,
                "matches": [{"file": "src/bus.rs", "line": 12, "content": "pub struct EventBus {"}]
            }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: None,
                text: "src/bus.rs:12  pub struct EventBus {".into()
            }
        );
    }

    /// 整块没有行号时收起号位，条目不应白占一列缩进。
    #[test]
    fn gutter_collapses_when_no_line_is_numbered() {
        let numbered = render_preview(
            &[PreviewLine::Context {
                number: Some(7),
                text: "code".into(),
            }],
            60,
            CodeLanguage::Plain,
            false,
        );
        let plain = render_preview(
            &[PreviewLine::Context {
                number: None,
                text: "code".into(),
            }],
            60,
            CodeLanguage::Plain,
            false,
        );

        let text_of = |line: &Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };

        assert_eq!(text_of(&numbered[0]), format!("{PREVIEW_INDENT}   7 code"));
        assert_eq!(text_of(&plain[0]), format!("{PREVIEW_INDENT}code"));
    }
}
