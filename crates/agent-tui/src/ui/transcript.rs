//! 工具调用块的转录排版。
//!
//! 目标是让每个工具调用一眼能读出"对什么做了什么、结果如何"：
//!
//! ```text
//! ● Write(src/tmp-debug.ts)
//!   └ 写入 36 行到 src/tmp-debug.ts
//!    1 /** 临时排查脚本。 */
//!    2 import { z } from 'zod'
//!       … +27 行
//! ```
//!
//! 本模块只负责排版，不认识具体插件工具：无法识别的工具退回通用 JSON 摘要，
//! 因此新增工具不需要改这里就能得到可读的默认呈现。

use crate::theme::{COLOR_DANGER, COLOR_LINE_NUMBER, COLOR_MUTED, COLOR_TEXT, COLOR_TEXT_SOFT};
use crate::ui::diff::{self, DiffLineKind};
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

/// 把绝对路径还原为相对当前工作目录的形式；不在工作目录之下时原样返回。
///
/// 工具结果回传的是绝对路径，这类路径动辄七八十列，放到正文里会把真正要读的内容挤到
/// 行尾。展示层统一按工作目录改写：读者就在这个目录里，前缀是纯噪音。
fn relative_path(path: &str) -> &str {
    let Ok(cwd) = std::env::current_dir() else {
        return path;
    };
    let mut prefix = cwd.to_string_lossy().to_string();
    prefix.push('/');
    path.strip_prefix(prefix.as_str()).unwrap_or(path)
}

/// 把绝对路径压缩为相对当前工作目录的短路径，便于在窄终端阅读。
fn shorten_path(path: &str) -> String {
    let relative = relative_path(path);
    if unicode_width::UnicodeWidthStr::width(relative) <= MAX_TARGET_WIDTH {
        return relative.to_string();
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
        "read_file" | "write_file" | "edit_file" | "list_directory" => {
            text_field(args, "path").map(shorten_path)
        }
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
        if diff::looks_like_diff(&rows) {
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
    /// 搜索结果的文件分组头。
    ///
    /// 同一文件的命中共用一次路径，下面的命中行只带行号与正文；路径已改写为相对工作
    /// 目录的形式，`text` 仍是原始相对路径（超宽时由渲染按当前宽度折叠前缀）。
    FileGroup {
        /// 组头文本，通常是相对工作目录的路径。
        text: String,
    },
    /// 文件级头部：`diff --git`、`index`、`---`、`+++` 等。
    FileHeader {
        /// 行正文，原样保留 `+++`/`---` 前缀。
        text: String,
    },
    /// hunk 头，例如 `@@ -10,7 +10,9 @@ fn main() {`。
    HunkHeader {
        /// 行正文。
        text: String,
    },
    /// 补丁附注，例如 `\ No newline at end of file`。
    Meta {
        /// 附注正文。
        text: String,
    },
}

/// 剥离工具在正文前附加的行号前缀，返回行号与正文。
///
/// 支持三种写法：`12\t正文`（原生 `read_file`）、`12→正文`（常见插件）与
/// `12:正文` / `12-正文`（`grep -n` 的匹配行与上下文行）。不匹配时原样返回，避免把
/// 正文里恰好以数字开头的行当成行号。
///
/// 第三种写法存在的意义是**对齐**：前缀留在正文里时，三位行号与四位行号会让代码
/// 起始列逐行漂移；收进行号列后，所有行的代码都从同一列开始。
fn split_line_number(text: &str) -> (Option<usize>, &str) {
    let trimmed = text.trim_start();
    if let Some((head, tail)) = trimmed.split_once('\t').or_else(|| trimmed.split_once('→')) {
        if let Ok(number) = head.trim().parse::<usize>() {
            return (Some(number), tail);
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return (None, text);
    }
    let rest = &trimmed[digits..];
    let body = match rest.chars().next() {
        Some(':' | '-') => &rest[1..],
        _ => return (None, text),
    };
    // 时间戳一类以数字开头的正文（`12:00:01 ...`）不是行号：分隔符后面紧跟数字时
    // 一律不认，否则整段日志会被当成"从第 12 行开始的代码"。
    if body.starts_with(|ch: char| ch.is_ascii_digit()) {
        return (None, text);
    }
    match trimmed[..digits].parse::<usize>() {
        Ok(number) => (Some(number), body),
        Err(_) => (None, text),
    }
}

/// 判断整段正文是否都带行号前缀。
///
/// 只有全部非空行都匹配时才剥离，避免把正文中偶然以数字开头的行误判为行号。
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
    match result.name.as_str() {
        "list_directory" => {
            let rows: Vec<String> = result
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
                .collect();
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
        "search_files" => search_preview(result, max_lines),
        _ => None,
    }
}

/// 按文件分组展开搜索命中。
///
/// 每条命中都自带完整路径时，路径列会吃掉半行宽度，命中正文被推到右边缘且每行起止
/// 位置互不相同，一列命中读不出对齐关系。搜索本来就按文件逐个遍历，同一文件的命中
/// 天然相邻，因此路径只在文件切换时输出一次组头，行号回到号位列，其余宽度全部让给
/// 命中正文。
fn search_preview(result: &ToolResult, max_lines: usize) -> Option<Vec<PreviewLine>> {
    let matches = result.content.get("matches")?.as_array()?;
    let mut lines: Vec<PreviewLine> = Vec::with_capacity(max_lines + 1);
    let mut current: Option<&str> = None;
    let mut shown = 0usize;
    for item in matches {
        let file = text_field(item, "file").unwrap_or("?");
        let new_group = current != Some(file);
        // 组头自己占一行：只剩一行时不再新开组，避免出现只有路径没有命中的空组。
        let cost = if new_group { 2 } else { 1 };
        if lines.len() + cost > max_lines {
            break;
        }
        if new_group {
            lines.push(PreviewLine::FileGroup {
                text: relative_path(file).to_string(),
            });
            current = Some(file);
        }
        let line = number_field(item, "line").unwrap_or_default();
        let content = text_field(item, "content").unwrap_or_default();
        lines.push(PreviewLine::Context {
            // 行号进号位列：1-based 行号 0 只可能是缺失字段，按无号处理。
            number: usize::try_from(line).ok().filter(|line| *line > 0),
            text: content.trim_end().to_string(),
        });
        shown += 1;
    }
    if shown < matches.len() {
        lines.push(PreviewLine::Elided {
            hidden: matches.len() - shown,
        });
    }
    Some(lines)
}

/// 判断工具结果携带的是否为编辑类工具的结构化 diff。
///
/// 内建 `edit_file` 这类工具在 `content.diff` 里回传统一 diff，它是给界面看的附属
/// 细节，不是模型回答的正文；摘要行已经报出增删行数，因此这份 diff 不再展开预览。
/// 命令输出（`shell` 的 `stdout`/`stderr`）不在此列：那是用户主动要的内容，仍按
/// diff 渲染。
fn is_edit_diff_result(result: &ToolResult) -> bool {
    text_field(&result.content, "diff").is_some()
}

/// 从 Agent Runtime 的 `outcome` 信封里取出成员正文或失败原因。
///
/// 插件拿到的终态是 `{"status":"succeeded","result":{"final_text":…}}`、`{"status":"failed","error":…}`
/// 或 `{"status":"cancelled"}`（`status` 是 tag 字段）。通用 JSON 摘要会把整个信封压成
/// `outcome: {…}`，成员跑出的报告一行也看不到；这里只认终态形状，其余返回 `None`。
fn outcome_body(value: &Value) -> Option<&str> {
    let outcome = value.get("outcome")?;
    match text_field(outcome, "status")? {
        "succeeded" => outcome
            .get("result")
            .and_then(|result| text_field(result, "final_text")),
        "failed" => text_field(outcome, "error"),
        _ => None,
    }
}

/// 从结构化工具结果中取出可逐行展开的正文。
///
/// 优先读取编辑结果的 `diff`；`content` 覆盖普通正文，`stdout`/`stderr` 覆盖命令类。
/// 都不存在时返回 `None`，由调用方退回 JSON 摘要——直接序列化整个对象会把
/// `{"content":"..."}` 这类信封当成正文，预览也就失去意义。
fn result_body(result: &ToolResult) -> Option<String> {
    match &result.content {
        Value::String(text) => Some(text.clone()),
        Value::Object(_) => {
            for key in ["diff", "content", "error"] {
                if let Some(text) = text_field(&result.content, key) {
                    return Some(text.to_string());
                }
            }
            // 信封式正文（成员结果、子 Agent 结果）按正文展开：预览取前若干行，
            // 展开后看全文，与内建工具的观感一致。
            if let Some(body) = outcome_body(&result.content) {
                return Some(body.to_string());
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
    // 编辑结果的补丁正文不进预览：行数已经在摘要行里，展开整段只会在看代码时刷屏。
    if is_edit_diff_result(result) {
        return Vec::new();
    }
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
    let is_diff = diff::looks_like_diff(&rows);
    // 分类一次算完整段：hunk 状态必须按原顺序推进，不能只看被保留的前若干行。
    let kinds = is_diff.then(|| diff::classify_rows(&rows));
    // 只有整段都带行号前缀时才剥离，避免正文里偶然以"数字+制表符"开头的行被误拆。
    let numbered = !is_diff && is_line_numbered(&rows);
    let mut lines: Vec<PreviewLine> = Vec::with_capacity(max_lines);
    for (index, row) in rows.iter().take(max_lines).enumerate() {
        if let Some(kind) = kinds.as_ref().map(|kinds| kinds[index]) {
            match kind {
                // 结构行原样保留前缀：`+++ b/src/main.rs` 的信息量就在前缀里。
                DiffLineKind::FileHeader => {
                    lines.push(PreviewLine::FileHeader {
                        text: row.trim_end().to_string(),
                    });
                    continue;
                }
                DiffLineKind::HunkHeader => {
                    lines.push(PreviewLine::HunkHeader {
                        text: row.trim_end().to_string(),
                    });
                    continue;
                }
                DiffLineKind::Meta => {
                    lines.push(PreviewLine::Meta {
                        text: row.trim_end().to_string(),
                    });
                    continue;
                }
                DiffLineKind::Added => {
                    let body = row.strip_prefix('+').unwrap_or(row);
                    lines.push(PreviewLine::Added {
                        number: Some(index + 1),
                        text: body.to_string(),
                    });
                    continue;
                }
                DiffLineKind::Removed => {
                    let body = row.strip_prefix('-').unwrap_or(row);
                    lines.push(PreviewLine::Removed {
                        number: Some(index + 1),
                        text: body.to_string(),
                    });
                    continue;
                }
                DiffLineKind::Context => {}
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
///
/// 运行中的 `git diff` 同样要按 diff 渲染：否则命令跑完是一个样子、跑的过程中又是另一个
/// 样子，读者会以为界面在闪烁。分类按完整的已收输出计算，hunk 状态才不会从中间断掉。
pub(crate) fn output_preview_lines(text: &str, max_lines: usize) -> Vec<PreviewLine> {
    let rows: Vec<&str> = text.lines().collect();
    let start = rows.len().saturating_sub(max_lines);
    let mut lines: Vec<PreviewLine> = Vec::with_capacity(max_lines + 1);
    if start > 0 {
        lines.push(PreviewLine::Elided { hidden: start });
    }
    let is_diff = diff::looks_like_diff(&rows);
    // 只分类要渲染的尾部：运行期输出可涨到 200KB，逐帧全量分类是纯粹的浪费。
    let kinds = is_diff.then(|| diff::classify_tail(&rows, start));
    lines.extend(rows[start..].iter().enumerate().map(|(offset, row)| {
        let kind = kinds.as_ref().map(|kinds| kinds[offset]);
        match kind {
            Some(DiffLineKind::FileHeader) => PreviewLine::FileHeader {
                text: row.trim_end().to_string(),
            },
            Some(DiffLineKind::HunkHeader) => PreviewLine::HunkHeader {
                text: row.trim_end().to_string(),
            },
            Some(DiffLineKind::Meta) => PreviewLine::Meta {
                text: row.trim_end().to_string(),
            },
            Some(DiffLineKind::Added) => PreviewLine::Added {
                number: None,
                text: row.strip_prefix('+').unwrap_or(row).to_string(),
            },
            Some(DiffLineKind::Removed) => PreviewLine::Removed {
                number: None,
                text: row.strip_prefix('-').unwrap_or(row).to_string(),
            },
            // 上下文行去掉 diff 的前导标记空格，正文才与增删行对齐。
            Some(DiffLineKind::Context) => PreviewLine::Context {
                number: None,
                text: row.strip_prefix(' ').unwrap_or(row).trim_end().to_string(),
            },
            None => PreviewLine::Context {
                number: None,
                text: row.trim_end().to_string(),
            },
        }
    }));
    lines
}

/// 预览块相对消息左边缘的缩进列数。
///
/// 正文与标题行里 `● ` 之后的工具名同列：工具输出是从属于这次调用的背景信息，顶到
/// 消息左边缘就与标题行脱开了从属关系，读起来像另起一段正文。缩进也不宜再宽——两列
/// 正好对齐工具名，长代码在窄终端里不至于提前折行。
const PREVIEW_INDENT: usize = 2;

/// 行号列的最小宽度：两位行号也需要一列右对齐空间。
///
/// 行号列按块内最大行号的位数取宽，不再固定四列：几十行的预览只占两列，正文因此
/// 紧贴行号列，而不是被一整块空号位推出去。
const GUTTER_MIN_WIDTH: usize = 2;
/// 行号列与正文之间的标记列宽：一列标记左侧留白加一列标记本身。
///
/// 留白的作用是让 `+`/`-` 不直接顶住行号列：标记贴着数字时，整行色块看起来像从行号里
/// 长出来的，而不是一条独立的改动记录。
const DIFF_MARKER_PAD: usize = 1;
/// 标记列总宽（留白 + 标记），用于把正文宽度与行号列一并扣掉。
const MARKER_COLUMNS: usize = DIFF_MARKER_PAD + 1;

/// 把预览正文裁到 `width` 列，截断时把省略号也算进可用宽度。
///
/// 共享的 [`truncate_line`] 在用满 `width` 之后才补省略号，结果比 `width` 多一列；其他
/// 调用方都预留了余量，预览行却是逐列铺满的，多出的一列会被 `Paragraph` 折到下一行，
/// 一行日志变成两行。这里只在真的放不下时把省略号计进宽度。
fn truncate_body(text: &str, width: usize) -> String {
    let body = truncate_line(text, width);
    if unicode_width::UnicodeWidthStr::width(body.as_str()) <= width {
        return body;
    }
    truncate_line(text, width.saturating_sub(1))
}

/// 把路径裁到 `width` 列，超出时按目录段折叠前缀，保留文件名。
///
/// 与正文用的尾部截断相反：定位一条命中时文件名才是关键，先被丢掉的不该是它。这里从
/// 右往左拼回目录段，拼不下的部分统一折叠为一个 `…/` 前缀。
fn elide_path(text: &str, width: usize) -> String {
    let width = width.max(1);
    if unicode_width::UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut tail = String::new();
    for segment in text.split('/').rev() {
        let candidate = if tail.is_empty() {
            segment.to_string()
        } else {
            format!("{segment}/{tail}")
        };
        // 省略号前缀本身占两列，必须一并算进预算，否则折叠后仍超宽、还要再被截一次。
        if unicode_width::UnicodeWidthStr::width(candidate.as_str()) + 2 > width {
            break;
        }
        tail = candidate;
    }
    if tail.is_empty() {
        // 连文件名都放不下：退回按字符保留尾部，至少留下可辨识的后缀。
        let mut kept = String::new();
        let mut used = 0usize;
        for character in text.chars().rev() {
            let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            if used + character_width + 1 > width {
                break;
            }
            used += character_width;
            kept.insert(0, character);
        }
        return format!("…{kept}");
    }
    format!("…/{tail}")
}

/// 计算行号列宽度：按块内最大行号的位数取，至少 [`GUTTER_MIN_WIDTH`] 列。
///
/// 整块共用一个宽度，行号才能逐列右对齐；宽度只取决于这块内容自己的行号，因此
/// 十几行的预览不会为四位行号预留空间。
fn preview_gutter_width(lines: &[PreviewLine]) -> usize {
    lines
        .iter()
        .filter_map(|line| match line {
            PreviewLine::Context { number, .. }
            | PreviewLine::Added { number, .. }
            | PreviewLine::Removed { number, .. } => *number,
            _ => None,
        })
        .max()
        .map_or(GUTTER_MIN_WIDTH, |max| {
            max.to_string().len().max(GUTTER_MIN_WIDTH)
        })
}

/// 渲染行号列，宽度由 [`preview_gutter_width`] 决定。
fn gutter(number: Option<usize>, color: Color, width: usize) -> Span<'static> {
    let text = number.map_or_else(|| " ".repeat(width), |number| format!("{number:>width$}"));
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
/// `width` 是消息区总宽度；diff 行的整行底色按它铺满。`language` 决定上下文行的高亮
/// 规则，`error` 为真时正文统一使用错误色，不做语法着色。
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
        PreviewLine::Elided { .. }
        | PreviewLine::FileGroup { .. }
        | PreviewLine::FileHeader { .. }
        | PreviewLine::HunkHeader { .. }
        | PreviewLine::Meta { .. } => false,
    });
    // 含 diff 结构行或增删行的块才需要标记列；纯文本预览保持紧凑排版。
    let diff_block = lines.iter().any(|line| {
        matches!(
            line,
            PreviewLine::Added { .. }
                | PreviewLine::Removed { .. }
                | PreviewLine::FileHeader { .. }
                | PreviewLine::HunkHeader { .. }
                | PreviewLine::Meta { .. }
        )
    });
    let gutter_width = if show_gutter {
        preview_gutter_width(lines)
    } else {
        0
    };
    // 标记列只在真的渲染标记时占位：diff 块留出「留白 + 标记」两列，只有行号的块留一列
    // 与增删行对齐，纯文本预览（shell 输出）不为它留空，把整段宽度让给正文。
    let marker_width = if diff_block {
        MARKER_COLUMNS
    } else if show_gutter {
        1
    } else {
        0
    };
    // 缩进、行号列与标记列之后剩余的正文宽度。缩进只占两列（与工具名同列），其余宽度
    // 全部让给代码与日志。
    let body_width = total_width
        .saturating_sub(PREVIEW_INDENT + gutter_width + marker_width)
        .max(1);

    lines
        .iter()
        .map(|line| match line {
            PreviewLine::Elided { hidden } => Line::from(vec![
                // 省略行与正文同列：它标记的是"这里还该有代码"，不是块级提示。
                Span::raw(" ".repeat(PREVIEW_INDENT + gutter_width + marker_width)),
                Span::styled(format!("… +{hidden} 行"), Style::new().fg(COLOR_MUTED)),
            ]),
            // 组头落在缩进列上，命中行再向右缩进一个号位：路径是这一组的标题，不是正文。
            PreviewLine::FileGroup { text } => Line::from(vec![
                Span::raw(" ".repeat(PREVIEW_INDENT)),
                Span::styled(
                    elide_path(text, total_width.saturating_sub(PREVIEW_INDENT).max(1)),
                    Style::new().fg(COLOR_TEXT_SOFT),
                ),
            ]),
            PreviewLine::FileHeader { text } => {
                diff_structure_row(text, DiffLineKind::FileHeader, gutter_width, body_width)
            }
            PreviewLine::HunkHeader { text } => {
                diff_structure_row(text, DiffLineKind::HunkHeader, gutter_width, body_width)
            }
            PreviewLine::Meta { text } => {
                diff_structure_row(text, DiffLineKind::Meta, gutter_width, body_width)
            }
            PreviewLine::Context { number, text } => {
                let mut spans = vec![Span::raw(" ".repeat(PREVIEW_INDENT))];
                if show_gutter {
                    spans.push(gutter(*number, COLOR_LINE_NUMBER, gutter_width));
                }
                if diff_block {
                    // 上下文行不显示标记，但标记列必须占住，正文才与增删行逐列对齐。
                    spans.push(Span::raw(" ".repeat(MARKER_COLUMNS)));
                } else if show_gutter {
                    spans.push(Span::raw(" "));
                }
                let body = truncate_body(text, body_width);
                if error {
                    spans.push(Span::styled(body, Style::new().fg(COLOR_DANGER)));
                } else {
                    spans.extend(highlight_line(&body, language));
                }
                Line::from(spans)
            }
            PreviewLine::Added { number, text } => diff_changed_row(
                text,
                true,
                *number,
                gutter_width,
                total_width,
                body_width,
                error,
            ),
            PreviewLine::Removed { number, text } => diff_changed_row(
                text,
                false,
                *number,
                gutter_width,
                total_width,
                body_width,
                error,
            ),
        })
        .collect()
}

/// 渲染一条增删行。
///
/// 三个要点决定了它读起来像"改动记录"而不是"一段带色的文字"：
///
/// - 整行底色从消息左边缘开始铺到右边界，色块贯穿整宽；
/// - 标记左侧留一列底色，`+`/`-` 不贴在行号列上；
/// - 行号降为暗灰，方向只由标记与底色表达。
fn diff_changed_row(
    text: &str,
    added: bool,
    number: Option<usize>,
    gutter_width: usize,
    total_width: usize,
    body_width: usize,
    error: bool,
) -> Line<'static> {
    let kind = if added {
        DiffLineKind::Added
    } else {
        DiffLineKind::Removed
    };
    let palette = diff::style_of(kind);
    // 增删行必然带底色；取不到时退回终端默认背景，宁可少一块色也不能让预览崩掉。
    let background = palette.bg.unwrap_or(Color::Reset);
    let marker = if added { '+' } else { '-' };
    let row_style = Style::new().bg(background);

    let mut spans = Vec::new();
    // 缩进也带底色：色块从消息左边缘一直铺到右边界，中途不断开。
    spans.push(Span::styled(" ".repeat(PREVIEW_INDENT), row_style));
    if gutter_width > 0 {
        // 行号用暗灰而不是红绿：它是定位坐标，不该比改动本身更抢眼。
        spans.push(gutter(number, COLOR_LINE_NUMBER, gutter_width).patch_style(row_style));
    }
    spans.push(Span::styled(" ".repeat(DIFF_MARKER_PAD), row_style));
    spans.push(Span::styled(marker.to_string(), row_style.fg(palette.fg)));
    let body = truncate_body(text, body_width);
    // 增删行正文与标记使用一致的红绿前景，避免语法配色掩盖修改方向。
    let body_color = if error { COLOR_DANGER } else { palette.fg };
    spans.push(Span::styled(body, row_style.fg(body_color)));
    // 底色一直铺到终端右边界：半截色块比不铺色更难看。
    let used = spans_width(&spans);
    pad_to(&mut spans, used, total_width, row_style);
    Line::from(spans)
}

/// 渲染 diff 的结构行：文件头、hunk 头与补丁附注。
///
/// 这些行不铺底色：它们是"改的是哪个文件、从第几行开始"的坐标，铺底色会和增删行的
/// 色块争夺注意力。行号列留空但保留宽度，正文才能与增删行逐列对齐。
fn diff_structure_row(
    text: &str,
    kind: DiffLineKind,
    gutter_width: usize,
    body_width: usize,
) -> Line<'static> {
    let palette = diff::style_of(kind);
    let mut style = Style::new().fg(palette.fg);
    if palette.bold {
        style = style.bold();
    }
    let mut spans = vec![Span::raw(" ".repeat(PREVIEW_INDENT))];
    if gutter_width > 0 {
        spans.push(Span::raw(" ".repeat(gutter_width)));
    }
    // 结构文本落在标记列上，与原样补丁的列位置一致。
    spans.push(Span::raw(" ".repeat(DIFF_MARKER_PAD)));
    spans.push(Span::styled(truncate_body(text, body_width), style));
    Line::from(spans)
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
        "edit_file" => "Edit",
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
    use crate::theme::{COLOR_DIFF_ADD_FG, COLOR_DIFF_FILE_HEADER, COLOR_DIFF_HUNK};
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

    /// 真实文件编辑只报增删行数，不再把补丁正文展开到工具卡片里。
    #[tokio::test]
    async fn real_file_edit_only_reports_the_change_summary() {
        use agent_tool::{builtins::EditFileTool, Tool, WorkspaceGuard};
        let root = std::env::temp_dir().join(format!(
            "lucia-diff-ui-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("sample.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\nlet old = 1;\nfn d() {}\nfn e() {}\nfn f() {}\n",
        )
        .unwrap();
        let tool = EditFileTool::new(WorkspaceGuard::rooted(&root).unwrap());
        let call = ToolCall::new(
            "edit-ui",
            "edit_file",
            json!({"path":"sample.rs", "old_string":"let old = 1;", "new_string":"let new = 2;"}),
        );
        let result = tool.call(call.clone()).await.unwrap();
        assert!(!result.is_error);
        // 结果里确实带着 diff，只是界面不再展开它。
        assert!(result.content["diff"].as_str().is_some_and(|diff| {
            diff.contains("+let new = 2;") && diff.contains("-let old = 1;")
        }));
        assert!(preview_lines(Some(&call), &result, 8).is_empty());
        assert_eq!(
            tool_summary(Some(&call), &result).as_deref(),
            Some("新增 1 行，删除 1 行")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 编辑结果的结构化 `diff` 不进预览，摘要行仍然是增删行数。
    #[test]
    fn structured_edit_diff_is_not_previewed() {
        let result = ToolResult::success(
            "c",
            "edit_file",
            json!({
                "path": "a.rs",
                "diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line"
            }),
        );

        assert!(preview_lines(None, &result, 8).is_empty());
        assert_eq!(
            tool_summary(None, &result).as_deref(),
            Some("新增 1 行，删除 1 行")
        );
    }

    /// 纯文本预览（shell 输出）不为标记列留空：省下的列全部让给正文。
    ///
    /// 只有 diff 块才会真的画出 `+`/`-` 与结构行；无行号、非 diff 的正文如果仍按标记列
    /// 扣宽，长命令会在还能放下的时候被提前截断。
    #[test]
    fn plain_preview_uses_the_full_body_width() {
        let lines = vec![PreviewLine::Context {
            number: None,
            text: "x".repeat(60),
        }];

        let rendered = render_preview(&lines, 40, CodeLanguage::Plain, false);

        // 40 列减去两列缩进，正文吃满剩余 38 列：整行恰好铺满 40 列，末列是省略号。
        let text = text_of(&rendered[0]);
        assert_eq!(unicode_width::UnicodeWidthStr::width(text.as_str()), 40);
        assert!(text.starts_with("  xxx"), "{text:?}");
        assert!(text.ends_with('…'), "{text:?}");
    }

    /// 只有行号的块只留一列标记位，而不是 diff 块的「留白 + 标记」两列。
    #[test]
    fn numbered_preview_reserves_a_single_marker_column() {
        let lines = vec![PreviewLine::Context {
            number: Some(7),
            text: "x".repeat(60),
        }];

        let rendered = render_preview(&lines, 40, CodeLanguage::Plain, false);

        // 40 列减去缩进 2、行号列 2（行号 7 只占一位，按最小宽度右对齐）、标记位 1，
        // 正文剩 35 列：整行恰好铺满 40 列。
        let text = text_of(&rendered[0]);
        assert_eq!(unicode_width::UnicodeWidthStr::width(text.as_str()), 40);
        assert!(text.starts_with("   7 "), "{text:?}");
        assert!(text.ends_with('…'), "{text:?}");
    }

    /// 工具块正文与标题行里的工具名同列：缩进只占两列，宽出的空间留给代码与日志。
    #[test]
    fn tool_preview_is_indented_two_columns() {
        assert_eq!(PREVIEW_INDENT, 2);
        let lines = vec![PreviewLine::Context {
            number: None,
            text: "cargo build".into(),
        }];

        let rendered = render_preview(&lines, 40, CodeLanguage::Plain, false);

        assert_eq!(text_of(&rendered[0]), "  cargo build");
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

    /// `grep -n` 的 `172:` / `173-` 前缀同样收进行号列，正文因此从同一列开始。
    #[test]
    fn grep_prefix_moves_into_the_gutter() {
        let result = ToolResult::success(
            "c",
            "shell",
            json!({"content": "172:pub(crate) fn markdown_lines() {\n173-    let body_width = 1;\n174-        .max(1);"}),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: Some(172),
                text: "pub(crate) fn markdown_lines() {".into()
            }
        );
        assert_eq!(
            lines[2],
            PreviewLine::Context {
                number: Some(174),
                text: "        .max(1);".into()
            }
        );
        // 渲染后正文列一致：只有源码自身的缩进不同，前缀不再推动正文。
        let rendered = render_preview(&lines, 40, CodeLanguage::Plain, false);
        assert!(
            text_of(&rendered[0]).starts_with("  172 pub(crate)"),
            "{:?}",
            text_of(&rendered[0])
        );
        assert!(
            text_of(&rendered[1]).starts_with("  173     let body_width"),
            "{:?}",
            text_of(&rendered[1])
        );
    }

    /// 以时间戳开头的日志不是行号：冒号后面紧跟数字时保留原样。
    #[test]
    fn timestamp_like_lines_are_not_line_numbers() {
        let result = ToolResult::success(
            "c",
            "shell",
            json!({"content": "12:00:01 starting\n12:00:02 done"}),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::Context {
                number: Some(1),
                text: "12:00:01 starting".into()
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

    /// diff 行的底色必须贯穿整行，否则终端里会出现半截色块。
    ///
    /// 色块从消息左边缘起铺、一直铺到右边界：只包住文字的底色读起来仍是一段带色的
    /// 文本，而不是一条独立的改动记录。
    #[test]
    fn diff_rows_fill_the_full_width() {
        let lines = vec![
            PreviewLine::Added {
                number: Some(1),
                text: "let x = 1;".into(),
            },
            PreviewLine::Removed {
                number: Some(2),
                text: "let x = 0;".into(),
            },
        ];

        let rendered = render_preview(&lines, 40, CodeLanguage::CLike, false);
        for (index, line) in rendered.iter().enumerate() {
            let background = line
                .spans
                .iter()
                .filter(|span| span.style.bg.is_some())
                .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();

            assert_eq!(background, 40, "第 {index} 行的底色应铺满整宽");
            // 第一段就带底色：色块从第 0 列开始，而不是从某处缩进之后。
            assert!(line.spans[0].style.bg.is_some(), "第 {index} 行应从头铺色");
        }
    }

    /// 标记左侧留一列底色，`+`/`-` 不顶住行号列。
    #[test]
    fn diff_marker_is_padded_away_from_the_gutter() {
        let lines = vec![PreviewLine::Added {
            number: Some(7),
            text: "let x = 1;".into(),
        }];

        let rendered = render_preview(&lines, 40, CodeLanguage::CLike, false);
        // 行号 7 只占一位，行号列按最小宽度两列右对齐。
        let gutter_width = GUTTER_MIN_WIDTH;
        let spans = &rendered[0].spans;
        let marker_index = spans
            .iter()
            .position(|span| span.content == "+")
            .expect("应包含新增标记");
        // 标记之前只剩缩进与行号列；行号列之后必须还有一列留白。
        let before_marker: usize = spans[..marker_index]
            .iter()
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        assert_eq!(
            before_marker,
            PREVIEW_INDENT + gutter_width + DIFF_MARKER_PAD
        );
        assert_eq!(
            text_of(&rendered[0]).find('+'),
            Some(PREVIEW_INDENT + gutter_width + 1)
        );
    }

    /// 行号必须弱化成暗灰，方向只由标记与底色表达。
    #[test]
    fn diff_line_numbers_are_dimmed() {
        let lines = vec![
            PreviewLine::Added {
                number: Some(12),
                text: "let x = 1;".into(),
            },
            PreviewLine::Removed {
                number: Some(13),
                text: "let x = 0;".into(),
            },
        ];

        let rendered = render_preview(&lines, 40, CodeLanguage::CLike, false);
        for (line, number) in rendered.iter().zip(["12", "13"]) {
            let gutter = line
                .spans
                .iter()
                .find(|span| span.content.trim() == number)
                .expect("应包含行号");
            assert_eq!(gutter.style.fg, Some(COLOR_LINE_NUMBER));
            // 行号仍带着整行底色，色块才能连贯。
            assert!(gutter.style.bg.is_some());
        }
    }

    /// 真实 `git diff` 输出必须被拆成结构行与增删行，而不是一整段单色正文。
    ///
    /// 这是"Shell 里的 diff 全白"的根因：标准补丁的头部行加上下文行远多于改动行，
    /// 旧的比例判据永远不成立，整个补丁因此退化成普通文本。
    #[test]
    fn shell_git_diff_is_detected_and_classified() {
        let diff = [
            "diff --git a/src/main.rs b/src/main.rs",
            "index 83f2a1b..9c31d4e 100644",
            "--- a/src/main.rs",
            "+++ b/src/main.rs",
            "@@ -10,7 +10,9 @@ fn main() {",
            "     let a = 1;",
            "-    let c = 3;",
            "+    let c = 4;",
            "+    let c5 = 5;",
            "     let d = 6;",
            "     let e = 7;",
            "     let f = 8;",
        ];
        let result = ToolResult::success("c", "shell", json!({ "stdout": diff.join("\n") }));

        let lines = preview_lines(None, &result, 20);

        assert!(matches!(lines[0], PreviewLine::FileHeader { .. }));
        assert!(matches!(lines[2], PreviewLine::FileHeader { .. }));
        assert!(matches!(lines[4], PreviewLine::HunkHeader { .. }));
        assert_eq!(
            lines[6],
            PreviewLine::Removed {
                number: Some(7),
                text: "    let c = 3;".into()
            }
        );
        assert_eq!(
            lines[7],
            PreviewLine::Added {
                number: Some(8),
                text: "    let c = 4;".into()
            }
        );
    }

    /// 文件头加粗且用青蓝、hunk 头青色加粗，两者都不铺底色；增删行铺满红绿底色。
    #[test]
    fn diff_structure_rows_are_colored_without_background() {
        let lines = vec![
            PreviewLine::FileHeader {
                text: "diff --git a/a.rs b/a.rs".into(),
            },
            PreviewLine::HunkHeader {
                text: "@@ -1,2 +1,3 @@".into(),
            },
            PreviewLine::Added {
                number: Some(2),
                text: "let y = 2;".into(),
            },
        ];

        let rendered = render_preview(&lines, 60, CodeLanguage::CLike, false);

        let header = &rendered[0];
        let header_text = header
            .spans
            .iter()
            .find(|span| span.content.contains("diff --git"))
            .expect("应包含文件头文本");
        assert_eq!(header_text.style.fg, Some(COLOR_DIFF_FILE_HEADER));
        assert!(header_text.style.add_modifier.contains(Modifier::BOLD));
        assert!(header.spans.iter().all(|span| span.style.bg.is_none()));

        let hunk = &rendered[1];
        let hunk_text = hunk
            .spans
            .iter()
            .find(|span| span.content.contains("@@ -1,2"))
            .expect("应包含 hunk 头文本");
        assert_eq!(hunk_text.style.fg, Some(COLOR_DIFF_HUNK));
        assert!(hunk_text.style.add_modifier.contains(Modifier::BOLD));
        assert!(hunk.spans.iter().all(|span| span.style.bg.is_none()));

        let added = &rendered[2];
        assert!(added
            .spans
            .iter()
            .any(|span| span.content == "let y = 2;" && span.style.fg == Some(COLOR_DIFF_ADD_FG)));
    }

    /// diff 上下文行不铺底色，但正文与增删行逐列对齐。
    #[test]
    fn diff_context_rows_align_with_changed_rows() {
        let lines = vec![
            PreviewLine::Added {
                number: Some(1),
                text: "let a = 1;".into(),
            },
            PreviewLine::Context {
                number: Some(2),
                text: "let b = 2;".into(),
            },
        ];

        let rendered = render_preview(&lines, 60, CodeLanguage::Plain, false);
        let body_column = |line: &Line<'_>| {
            let mut column = 0;
            for span in &line.spans {
                if span.content.contains("let ") {
                    return column;
                }
                column += unicode_width::UnicodeWidthStr::width(span.content.as_ref());
            }
            panic!("{}", text_of(line));
        };

        assert_eq!(body_column(&rendered[0]), body_column(&rendered[1]));
        assert!(
            rendered[1].spans.iter().all(|span| span.style.bg.is_none()),
            "上下文行不应铺底色"
        );
    }

    /// hunk 内以 `--` 开头的删除行渲染成 `--- ...`，必须仍是删除行而不是文件头。
    ///
    /// 这一行与补丁的 `--- a/file` 文件头逐字节相同，只有"是否已在 hunk 内"能区分：
    /// 判定错了，删除行会突然变成一行蓝色文件头。
    #[test]
    fn dashes_inside_a_hunk_stay_removed_rows() {
        let diff = [
            "diff --git a/q.sql b/q.sql",
            "--- a/q.sql",
            "+++ b/q.sql",
            "@@ -1,2 +1,2 @@",
            "--- 旧注释",
            "+++ 新注释",
        ];
        let result = ToolResult::success("c", "shell", json!({ "stdout": diff.join("\n") }));

        let lines = preview_lines(None, &result, 10);

        assert!(matches!(lines[0], PreviewLine::FileHeader { .. }));
        assert!(matches!(lines[1], PreviewLine::FileHeader { .. }));
        assert_eq!(
            lines[4],
            PreviewLine::Removed {
                number: Some(5),
                text: "-- 旧注释".into()
            }
        );
        assert_eq!(
            lines[5],
            PreviewLine::Added {
                number: Some(6),
                text: "++ 新注释".into()
            }
        );
    }

    /// 运行中的增量输出同样按 diff 渲染，避免跑完前后观感突变。
    #[test]
    fn live_output_of_a_diff_is_classified_too() {
        let text = [
            "diff --git a/a.rs b/a.rs",
            "--- a/a.rs",
            "+++ b/a.rs",
            "@@ -1 +1 @@",
            "-old",
            "+new",
        ]
        .join("\n");

        let lines = output_preview_lines(&text, 10);

        assert!(matches!(lines[0], PreviewLine::FileHeader { .. }));
        assert_eq!(
            lines[4],
            PreviewLine::Removed {
                number: None,
                text: "old".into()
            }
        );
        assert_eq!(
            lines[5],
            PreviewLine::Added {
                number: None,
                text: "new".into()
            }
        );
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

    /// 取出正文行的文本；断言用，其它行型直接失败。
    fn body_of(line: &PreviewLine) -> &str {
        match line {
            PreviewLine::Context { text, .. } => text,
            other => panic!("该行不是正文行：{other:?}"),
        }
    }

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

    /// 搜索命中按文件分组：路径只出现一次，行号进号位列，命中正文同列开始。
    #[test]
    fn search_matches_group_under_the_file_path() {
        let result = ToolResult::success(
            "c",
            "search_files",
            json!({
                "count": 3,
                "truncated": false,
                "matches": [
                    {"file": "src/bus.rs", "line": 12, "content": "pub struct EventBus {"},
                    {"file": "src/bus.rs", "line": 41, "content": "impl EventBus {"},
                    {"file": "src/lib.rs", "line": 3, "content": "pub use bus::EventBus;"}
                ]
            }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines,
            vec![
                PreviewLine::FileGroup {
                    text: "src/bus.rs".into()
                },
                PreviewLine::Context {
                    number: Some(12),
                    text: "pub struct EventBus {".into()
                },
                PreviewLine::Context {
                    number: Some(41),
                    text: "impl EventBus {".into()
                },
                PreviewLine::FileGroup {
                    text: "src/lib.rs".into()
                },
                PreviewLine::Context {
                    number: Some(3),
                    text: "pub use bus::EventBus;".into()
                },
            ]
        );
    }

    /// 绝对路径在组头里改写成相对工作目录的形式，前缀不再占掉整行宽度。
    #[test]
    fn search_group_header_is_relative_to_the_working_directory() {
        let cwd = std::env::current_dir().expect("测试进程需要工作目录");
        let absolute = cwd.join("src/bus.rs").to_string_lossy().to_string();
        let result = ToolResult::success(
            "c",
            "search_files",
            json!({
                "count": 1,
                "truncated": false,
                "matches": [{"file": absolute, "line": 12, "content": "pub struct EventBus {"}]
            }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines[0],
            PreviewLine::FileGroup {
                text: "src/bus.rs".into()
            }
        );
    }

    /// 组头也占预览行：宁可少显示一条命中，也不让某一行超出上限。
    #[test]
    fn search_preview_counts_group_headers_against_the_limit() {
        let matches: Vec<serde_json::Value> = (1..=10)
            .map(|line| json!({"file": "src/bus.rs", "line": line, "content": "x"}))
            .collect();
        let result = ToolResult::success(
            "c",
            "search_files",
            json!({"count": 10, "truncated": false, "matches": matches}),
        );

        let lines = preview_lines(None, &result, 8);

        // 1 行组头 + 7 行命中用满 8 行上限，剩余 3 条进省略行。
        assert_eq!(lines.len(), 9);
        assert_eq!(lines.last(), Some(&PreviewLine::Elided { hidden: 3 }));
    }

    /// 上限只够一行时不再新开组，避免出现只有路径、没有命中的空组头。
    #[test]
    fn search_preview_drops_the_group_header_when_it_alone_fills_the_budget() {
        let result = ToolResult::success(
            "c",
            "search_files",
            json!({
                "count": 1,
                "truncated": false,
                "matches": [{"file": "src/bus.rs", "line": 12, "content": "pub struct EventBus {"}]
            }),
        );

        let lines = preview_lines(None, &result, 1);

        assert_eq!(lines, vec![PreviewLine::Elided { hidden: 1 }]);
    }

    /// 超宽路径按目录段折叠前缀，文件名必须留住。
    #[test]
    fn elided_paths_keep_the_file_name() {
        let path = "crates/agent-plugins/host/src/capability/mod.rs";

        assert_eq!(elide_path(path, 120), path);
        assert_eq!(
            elide_path(path, 24),
            "…/src/capability/mod.rs",
            "折叠后仍要落在预算内"
        );
        // 连文件名都放不下时至少留下可辨识的后缀，不会退化成空串。
        assert!(unicode_width::UnicodeWidthStr::width(elide_path(path, 4).as_str()) <= 4);
    }

    /// 分组后的命中正文逐列对齐：宽度只由号位列决定。
    #[test]
    fn grouped_search_hits_share_one_content_column() {
        let lines = vec![
            PreviewLine::FileGroup {
                text: "src/bus.rs".into(),
            },
            PreviewLine::Context {
                number: Some(12),
                text: "pub struct EventBus {".into(),
            },
            PreviewLine::Context {
                number: Some(1041),
                text: "impl EventBus {".into(),
            },
        ];

        let rendered = render_preview(&lines, 60, CodeLanguage::Plain, false);
        let texts: Vec<String> = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(texts[0], "  src/bus.rs");
        assert_eq!(texts[1], "    12 pub struct EventBus {");
        assert_eq!(texts[2], "  1041 impl EventBus {");
    }

    /// 成员结果按「预览若干行 + 展开看全文」呈现，而不是把整段报告塞进卡片。
    #[test]
    fn member_result_is_previewed_like_a_tool_result() {
        let report = (1..=30)
            .map(|line| format!("第 {line} 行审查结论"))
            .collect::<Vec<_>>();
        let result = ToolResult::success(
            "c",
            "teammate_result",
            json!({
                "completed": true,
                "retired": false,
                "outcome": {
                    "status": "succeeded",
                    "result": {
                        "run_id": "run-1",
                        "final_text": report.join("\n"),
                        "steps_used": 4,
                        "usage": {}
                    }
                }
            }),
        );

        // 收起态：8 行预览 + 一行省略提示（可展开）。
        let collapsed = preview_lines(None, &result, 8);
        assert_eq!(collapsed.len(), 9);
        // 正文行与其它文本类结果一样进号位列，展开后翻看时能对上行号。
        assert_eq!(
            collapsed[0],
            PreviewLine::Context {
                number: Some(1),
                text: "第 1 行审查结论".into()
            }
        );
        assert_eq!(body_of(&collapsed[7]), "第 8 行审查结论");
        assert_eq!(collapsed[8], PreviewLine::Elided { hidden: 22 });

        // 展开态：整段报告都进预览，不再有省略提示。
        let expanded = preview_lines(None, &result, 40);
        assert_eq!(expanded.len(), 30);
        assert!(expanded.iter().all(|line| !line.is_elided()));

        // 摘要行取结果首行，让收起态也知道成员回了什么。
        assert_eq!(
            tool_summary(None, &result).as_deref(),
            Some("第 1 行审查结论")
        );
    }

    /// 失败的成员结果要显示失败原因，而不是 `outcome: {…}`。
    #[test]
    fn failed_member_result_previews_the_error() {
        let result = ToolResult::success(
            "c",
            "teammate_result",
            json!({
                "completed": true,
                "retired": true,
                "outcome": {"status": "failed", "error": "provider 返回 429：请求过于频繁"}
            }),
        );

        let lines = preview_lines(None, &result, 8);

        assert_eq!(
            lines,
            vec![PreviewLine::Context {
                number: Some(1),
                text: "provider 返回 429：请求过于频繁".into()
            }]
        );
    }

    /// 成员仍在运行时没有正文可展开，退回 JSON 摘要而不是空白卡片。
    #[test]
    fn running_member_result_falls_back_to_json_summary() {
        let result = ToolResult::success(
            "c",
            "teammate_result",
            json!({"completed": false, "retired": false, "outcome": null}),
        );

        let lines = preview_lines(None, &result, 8);

        let text = body_of(&lines[0]);
        assert!(text.contains("completed: false"), "{text}");
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

        // 有行号时留出「缩进 + 最小行号列 + 一列间隔」，没有行号时正文只带缩进。
        assert_eq!(text_of(&numbered[0]), "   7 code");
        assert_eq!(text_of(&plain[0]), "  code");
    }

    /// 行号列按块内最大行号的位数取宽，不再固定四列。
    #[test]
    fn gutter_width_follows_the_widest_line_number() {
        let render = |number: usize| {
            let lines = render_preview(
                &[PreviewLine::Context {
                    number: Some(number),
                    text: "code".into(),
                }],
                60,
                CodeLanguage::Plain,
                false,
            );
            lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        // 一位行号也按最小宽度两列右对齐。
        assert_eq!(render(7), "   7 code");
        // 三位行号占满三列，正文紧随其后。
        assert_eq!(render(174), "  174 code");
        // 位数继续增长时行号列跟着变宽，行号本身始终完整可见。
        assert_eq!(render(123_456), "  123456 code");
    }
}
