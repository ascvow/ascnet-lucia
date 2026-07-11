//! Lucia 交互式 TUI（基于 Ratatui）。

mod app_config;

#[cfg(feature = "plugins")]
use agent_core::AgentExtension;
use agent_core::{
    config::AgentRootConfig,
    event::{AgentEvent, AgentEventKind, CompositeEventSink, EventSink, JsonlEventSink},
    model::{
        ChatModel, ContentBlock, MessageRole, ModelGateway, ModelRequest, ModelResponse,
        ProviderAdapter,
    },
    Agent, AgentOptions, AgentRun, Session,
};
#[cfg(feature = "plugins")]
use agent_plugin_host::{
    manifest::{load_plugin_runtime_config, PluginManifest},
    ui::{
        UiColor, UiDeclaration, UiFrame as PluginUiFrame, UiInput, UiInputEvent, UiLine,
        UiPlacement, UiRenderRequest, UiSpan, UiStyle,
    },
    wasm::{load_wasm_plugins_resilient_with_selection, PluginLoadFailure},
    CompositePluginHost, PluginHost,
};
use agent_session::{FileSessionStore, MemorySessionStore, SessionId, SessionRecord, SessionStore};
use agent_tool::{JsonTool, ToolCall, ToolRegistry, ToolSpec};
use anyhow::{anyhow, Result};
#[cfg(feature = "plugins")]
use app_config::discover_official_plugin_manifests;
use app_config::{
    initialize_config, load_tui_settings, lucia_home_dir, resolve_config_path,
    resolve_config_relative_path, TuiSettings,
};
use async_trait::async_trait;
use clap::Parser;
#[cfg(feature = "plugins")]
use crossterm::event::MouseEvent;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{prelude::*, widgets::*};
use serde_json::{json, Value};
use std::collections::VecDeque;
#[cfg(feature = "plugins")]
use std::collections::{HashMap, HashSet};
use std::{path::Path, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;

// ─── CLI 参数 ───

#[derive(Debug, Parser)]
#[command(author, version, about = "Lucia 交互式 ReAct Agent")]
struct Args {
    /// 初始化配置文件后退出；默认写入 `$LUCIA_HOME/config.toml`。
    #[arg(long = "init", alias = "init-config")]
    init: bool,

    /// 使用内置脚本模型。
    #[arg(long)]
    demo: bool,

    /// TOML 配置文件路径；默认读取 `LUCIA_CONFIG` 或 `$LUCIA_HOME/config.toml`。
    #[arg(long)]
    config: Option<PathBuf>,

    /// 可选的 agent 事件 JSONL 输出文件，用于排查模型请求与工具调用。
    #[arg(long = "events-jsonl")]
    events_jsonl: Option<PathBuf>,

    /// 会话文件目录；覆盖配置文件和 `$LUCIA_HOME/sessions` 默认值。
    #[arg(long = "sessions-dir")]
    sessions_dir: Option<PathBuf>,

    /// 要恢复和持续更新的稳定会话标识；覆盖配置中的默认值。
    #[arg(long = "session-id")]
    session_id: Option<String>,

    /// 恢复最近更新的持久化会话；显式 `--session-id` 优先。
    #[arg(long = "resume-latest")]
    resume_latest: bool,

    /// 列出持久化会话后退出，不连接模型服务。
    #[arg(long = "list-sessions")]
    list_sessions: bool,

    /// 插件 manifest 路径；可以重复传入并按参数顺序占用 UI 插槽。
    #[cfg(feature = "plugins")]
    #[arg(long = "plugin-manifest")]
    plugin_manifests: Vec<PathBuf>,
}

// ─── UI 事件 ───

enum UiEvent {
    Input(Event),
    Tick,
    ModelStarted,
    ModelTextDelta(String),
    ToolStarted {
        name: String,
        /// 调用参数的单行摘要。
        args: String,
    },
    ToolFinished {
        name: String,
        is_error: bool,
        /// 返回内容的单行摘要。
        result: String,
    },
    ToolSkipped(String),
    SteeringInjected,
    FollowUpInjected,
    /// 扩展发布到主事件列表的结构化展示事件。
    Extension {
        text: String,
        color: Color,
        divider: bool,
    },
    /// 最近一次模型请求消耗的上下文 token 数。
    ContextUsage(u64),
    /// Agent 运行及 CAS 持久化均完成，成功值携带最新会话记录。
    AgentDone(Box<Result<(AgentRun, SessionRecord)>>),
    /// Background plugin loading completed and can now be attached to the pending Agent.
    /// 后台插件加载结束，可挂载到等待中的 Agent。
    #[cfg(feature = "plugins")]
    PluginsLoaded(Box<Result<LoadedPlugins>>),
}

/// Plugin runtime data prepared off the TUI event loop.
///
/// 在 TUI 事件循环之外准备完成的插件运行时数据。
#[cfg(feature = "plugins")]
struct LoadedPlugins {
    /// Composite host containing every successfully activated plugin. 已激活插件的组合宿主。
    host: Arc<CompositePluginHost>,
    /// Stable plugin IDs in dependency-resolved load order. 按依赖解析顺序排列的稳定插件 ID。
    plugin_ids: Vec<String>,
    /// UI declarations collected after activation. 激活后收集的 UI 声明。
    plugin_views: Vec<UiDeclaration>,
    /// Activation events consumed before the first Agent run. 首次 Agent 运行前消费的激活事件。
    startup_events: Vec<Value>,
    /// Plugins excluded by activation failures or required dependencies. 因激活或必选依赖失败而被剔除的插件。
    failures: Vec<PluginLoadFailure>,
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 输入区域的聚焦边框颜色。
const COLOR_BORDER_FOCUS: Color = Color::Rgb(112, 110, 104);
/// 主要文字颜色。
const COLOR_TEXT: Color = Color::Rgb(224, 222, 216);
/// 次要文字和边框颜色。
const COLOR_MUTED: Color = Color::Rgb(124, 122, 116);
/// 用户消息强调色。
const COLOR_USER: Color = Color::Rgb(104, 190, 126);
/// 成功状态颜色。
const COLOR_SUCCESS: Color = Color::Rgb(104, 190, 126);
/// 运行和等待状态颜色。
const COLOR_WARNING: Color = Color::Rgb(197, 164, 103);
/// 错误状态颜色。
const COLOR_DANGER: Color = Color::Rgb(205, 101, 101);
/// Number of 80 ms UI ticks before startup plugin details collapse into the compact counter.
/// 启动插件详情收敛为紧凑计数前保留的 80 毫秒 UI tick 数。
#[cfg(feature = "plugins")]
const PLUGIN_STATUS_DETAIL_TICKS: u16 = 75;

// ─── 聊天消息 ───

enum MsgKind {
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

struct Msg {
    kind: MsgKind,
    text: String,
    /// 工具调用参数的单行摘要（仅工具消息）。
    args: Option<String>,
    /// 工具返回内容的单行摘要（仅工具消息）。
    result: Option<String>,
    /// 扩展事件使用的强调色。
    accent: Option<Color>,
    /// 是否以分隔线形式展示扩展事件。
    divider: bool,
}

impl Msg {
    /// 创建普通消息，工具专用字段留空。
    fn new(kind: MsgKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            args: None,
            result: None,
            accent: None,
            divider: false,
        }
    }

    /// 创建由扩展事件驱动的主事件列表消息。
    fn extension(text: impl Into<String>, color: Color, divider: bool) -> Self {
        Self {
            kind: MsgKind::Extension,
            text: text.into(),
            args: None,
            result: None,
            accent: Some(color),
            divider,
        }
    }

    /// 将消息转换为行首标记加正文的紧凑样式，并标记正在流式生成的消息。
    fn to_lines(&self, streaming: bool) -> Vec<Line<'_>> {
        match self.kind {
            MsgKind::User => conversation_lines("❯", &self.text, COLOR_USER, COLOR_TEXT, false),
            MsgKind::Assistant => markdown_lines(&self.text, streaming),
            MsgKind::ToolRunning => self.tool_lines("", COLOR_WARNING),
            MsgKind::ToolOk => self.tool_lines("", COLOR_SUCCESS),
            MsgKind::ToolError => self.tool_lines("failed", COLOR_DANGER),
            MsgKind::ToolSkipped => self.tool_lines("skipped", COLOR_MUTED),
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

    /// 构造工具调用块：`● 名称(参数)` 加 `⎿ 返回摘要` 行，圆点以状态色区分，块尾留空行。
    fn tool_lines(&self, note: &str, color: Color) -> Vec<Line<'_>> {
        let mut first = vec![
            Span::styled("● ", Style::new().fg(color)),
            Span::styled(self.text.as_str(), Style::new().fg(COLOR_TEXT).bold()),
        ];
        if let Some(args) = &self.args {
            first.push(Span::styled(
                format!("({args})"),
                Style::new().fg(COLOR_MUTED),
            ));
        }
        if !note.is_empty() {
            first.push(Span::styled(format!("  {note}"), Style::new().fg(color)));
        }
        let mut lines = vec![Line::from(first)];
        if let Some(result) = &self.result {
            let result_color = if matches!(self.kind, MsgKind::ToolError) {
                COLOR_DANGER
            } else {
                COLOR_MUTED
            };
            lines.push(Line::from(vec![
                Span::styled("  ⎿ ", Style::new().fg(COLOR_MUTED)),
                Span::styled(result.as_str(), Style::new().fg(result_color)),
            ]));
        }
        lines.push(Line::default());
        lines
    }
}

/// 构造角色消息：首行带标记，续行对齐缩进，流式时在末尾附加光标。
fn conversation_lines<'a>(
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

/// Markdown 文本段：普通段落交给 tui-markdown，表格块单独排版。
enum MdSegment<'a> {
    Prose(&'a str),
    Table(Vec<&'a str>),
}

/// 将助手回复按 Markdown 渲染：表格块自行对齐排版（tui-markdown 不支持表格），
/// 首行带 `●` 标记，续行缩进对齐，连续空行折叠为一行。
fn markdown_lines(text: &str, streaming: bool) -> Vec<Line<'_>> {
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

/// 按行扫描文本，切分出表格块：首行以 `|` 开头且次行是 `|---|` 分隔行的连续竖线行。
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

/// 判断是否为表格行（修剪后以竖线开头）。
fn is_table_row(row: &str) -> bool {
    row.trim_start().starts_with('|')
}

/// 判断是否为表格分隔行（形如 `|---|:---:|`）。
fn is_table_separator(row: &str) -> bool {
    let cells = parse_table_row(row);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            !cell.is_empty() && cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':'))
        })
}

/// 拆出表格行的单元格：去掉首尾竖线后按 `|` 分列并修剪空白。
fn parse_table_row(row: &str) -> Vec<&str> {
    let trimmed = row.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(str::trim).collect()
}

/// 将表格排版为等宽对齐的行：表头加粗，分隔行转为横线，中文按显示宽度对齐。
fn render_table<'a>(rows: &[&'a str], out: &mut Vec<Line<'a>>) {
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

/// 清除 tui-markdown 主题自带的背景色：带背景的样式（标题、行内代码）改为前景强调色加粗。
fn restyle_markdown(style: Style, accent: Color) -> Style {
    if style.bg.is_none() {
        return style;
    }
    let mut modifiers = style.add_modifier;
    // 下划线在终端里过于花哨，标题保留加粗与颜色即可。
    modifiers.remove(Modifier::UNDERLINED);
    Style::new()
        .fg(accent)
        .add_modifier(modifiers | Modifier::BOLD)
}

/// 将 JSON 值压缩为单行摘要：对象展开为 `键: 值` 列表，嵌套结构折叠为计数，
/// 超出宽度按显示宽度截断。
fn summarize_json(value: &Value, max_width: usize) -> String {
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
fn truncate_line(text: &str, max_width: usize) -> String {
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

/// 将持久化的 provider-neutral Session 恢复为主事件列表消息。
///
/// system、developer 和 thinking 内容不会直接展示；工具调用与后续结果会合并为一条
/// 工具事件，避免恢复后出现重复块。
fn restore_session_messages(session: &Session) -> Vec<Msg> {
    let mut messages = Vec::new();
    for message in session.messages() {
        match message.role {
            MessageRole::User => {
                let text = message.text_content();
                if !text.is_empty() {
                    messages.push(Msg::new(MsgKind::User, text));
                }
            }
            MessageRole::Assistant => {
                let text = message.text_content();
                if !text.is_empty() {
                    messages.push(Msg::new(MsgKind::Assistant, text));
                }
                for block in &message.content {
                    if let ContentBlock::ToolCall { call } = block {
                        let mut restored = Msg::new(MsgKind::ToolRunning, call.name.clone());
                        let args = summarize_json(&call.args, 64);
                        restored.args = (!args.is_empty()).then_some(args);
                        messages.push(restored);
                    }
                }
            }
            MessageRole::Tool => {
                for block in &message.content {
                    let ContentBlock::ToolResult { result } = block else {
                        continue;
                    };
                    let kind = if result.is_error {
                        MsgKind::ToolError
                    } else {
                        MsgKind::ToolOk
                    };
                    let summary = summarize_json(&result.content, 96);
                    if let Some(restored) = messages.iter_mut().rev().find(|candidate| {
                        matches!(candidate.kind, MsgKind::ToolRunning)
                            && candidate.text == result.name
                    }) {
                        restored.kind = kind;
                        restored.result = (!summary.is_empty()).then_some(summary);
                    } else {
                        let mut restored = Msg::new(kind, result.name.clone());
                        restored.result = (!summary.is_empty()).then_some(summary);
                        messages.push(restored);
                    }
                }
            }
            MessageRole::System | MessageRole::Developer => {}
        }
    }
    messages
}

/// 从首次用户输入生成适合会话列表的短标题。
fn session_title(input: &str) -> Option<String> {
    let text = input.lines().find(|line| !line.trim().is_empty())?.trim();
    let mut chars = text.chars();
    let title = chars.by_ref().take(60).collect::<String>();
    Some(if chars.next().is_some() {
        format!("{title}…")
    } else {
        title
    })
}

// ─── 应用状态 ───

/// Builds load-order-preserving summaries from plugin activation events.
///
/// 根据插件激活事件生成保持加载顺序的启动摘要；没有事件文本的插件仅显示 ID。
#[cfg(feature = "plugins")]
fn plugin_startup_details(plugin_ids: &[String], events: &[Value]) -> Vec<String> {
    let mut status_by_id = HashMap::new();
    for event in events {
        let Some(plugin_id) = event.pointer("/source/id").and_then(Value::as_str) else {
            continue;
        };
        let text = event
            .pointer("/presentation/text")
            .and_then(Value::as_str)
            .or_else(|| event.pointer("/data/text").and_then(Value::as_str))
            .or_else(|| event.get("name").and_then(Value::as_str));
        if let Some(text) = text {
            status_by_id.insert(plugin_id, text);
        }
    }
    plugin_ids
        .iter()
        .map(|plugin_id| {
            status_by_id
                .get(plugin_id.as_str())
                .map(|text| format!("{plugin_id}: {text}"))
                .unwrap_or_else(|| plugin_id.clone())
        })
        .collect()
}

struct App {
    messages: Vec<Msg>,
    input: String,
    /// FIFO inputs accepted before the Agent becomes ready. Agent 就绪前接收的 FIFO 输入队列。
    queued_inputs: VecDeque<String>,
    /// 光标在 input 中的字节偏移。
    cursor: usize,
    running: bool,
    should_quit: bool,
    /// 当前已确认的会话记录；运行或保存失败时保持不变。
    session_record: SessionRecord,
    /// 执行 revision 比较并交换的会话存储。
    session_store: Arc<dyn SessionStore>,
    tx: mpsc::UnboundedSender<UiEvent>,
    model_name: String,
    spinner_frame: usize,
    /// 当前正在接收增量文本的助手消息索引。
    streaming_message: Option<usize>,
    /// 手动滚动偏移；None 表示跟随底部自动滚动。
    scroll: Option<u16>,
    /// 上一帧计算出的最大滚动偏移，供滚动操作作为起点。
    last_max_scroll: u16,
    /// 最近一次模型请求消耗的上下文 token 数。
    context_tokens: Option<u64>,
    /// 插件声明的视图及宿主缓存的最近一帧。
    #[cfg(feature = "plugins")]
    plugin_views: Vec<PluginViewState>,
    /// 当前通过 Tab 聚焦的停靠视图索引；模态对话框会临时覆盖该焦点。
    #[cfg(feature = "plugins")]
    plugin_focus: Option<usize>,
    /// 单调递增的插件渲染帧序号。
    #[cfg(feature = "plugins")]
    plugin_frame: u64,
    /// 控制插件 UI 刷新频率的主循环 tick 计数。
    #[cfg(feature = "plugins")]
    plugin_tick: u8,
    /// Loaded plugin IDs shown by the compact status counter. 紧凑状态计数展示的插件 ID。
    #[cfg(feature = "plugins")]
    plugin_ids: Vec<String>,
    /// Startup activation summaries shown once below the input. 输入框下方一次性展示的启动摘要。
    #[cfg(feature = "plugins")]
    plugin_startup_details: Vec<String>,
    /// Remaining ticks before startup details collapse. 启动详情收敛前的剩余 tick 数。
    #[cfg(feature = "plugins")]
    plugin_status_ticks: u16,
    /// Whether plugin activation is still running in the background. 插件是否仍在后台激活。
    #[cfg(feature = "plugins")]
    plugins_loading: bool,
    /// Plugin startup failure shown persistently in the footer. 底栏持续展示的插件启动错误。
    #[cfg(feature = "plugins")]
    plugin_load_error: Option<String>,
    /// Per-plugin failures retained alongside successful plugins. 与成功插件并存的单插件失败摘要。
    #[cfg(feature = "plugins")]
    plugin_failures: Vec<String>,
}

/// 主 TUI 为单个插件视图维护的运行时状态。
#[cfg(feature = "plugins")]
struct PluginViewState {
    /// 插件提供并由宿主补全插件 ID 的静态声明。
    declaration: UiDeclaration,
    /// 最近一次成功渲染的声明式内容。
    frame: Option<PluginUiFrame>,
    /// 最近一帧由主 TUI 分配的内容区域。
    area: Rect,
}

/// 键盘事件在主界面与插件界面之间的路由结果。
#[cfg(feature = "plugins")]
enum PluginKeyRoute {
    /// 继续交给主界面处理。
    Main,
    /// 焦点切换等宿主行为已经消费该事件。
    Consumed,
    /// 将转换后的事件发送给插件。
    Input(UiInput),
}

impl App {
    /// 创建空白会话，使首屏保持与参考界面一致的低干扰状态。
    fn new(tx: mpsc::UnboundedSender<UiEvent>, model_name: String) -> Self {
        let session_record = SessionRecord::new(SessionId::generate(), Session::new())
            .expect("创建进程内默认会话记录");
        Self {
            messages: Vec::new(),
            input: String::new(),
            queued_inputs: VecDeque::new(),
            cursor: 0,
            running: false,
            should_quit: false,
            session_record,
            session_store: Arc::new(MemorySessionStore::new()),
            tx,
            model_name,
            spinner_frame: 0,
            streaming_message: None,
            scroll: None,
            last_max_scroll: 0,
            context_tokens: None,
            #[cfg(feature = "plugins")]
            plugin_views: Vec::new(),
            #[cfg(feature = "plugins")]
            plugin_focus: None,
            #[cfg(feature = "plugins")]
            plugin_frame: 0,
            #[cfg(feature = "plugins")]
            plugin_tick: 0,
            #[cfg(feature = "plugins")]
            plugin_ids: Vec::new(),
            #[cfg(feature = "plugins")]
            plugin_startup_details: Vec::new(),
            #[cfg(feature = "plugins")]
            plugin_status_ticks: 0,
            #[cfg(feature = "plugins")]
            plugins_loading: false,
            #[cfg(feature = "plugins")]
            plugin_load_error: None,
            #[cfg(feature = "plugins")]
            plugin_failures: Vec::new(),
        }
    }

    /// 注入启动时加载的持久化记录及其存储实现。
    fn with_persistent_session(
        mut self,
        session_store: Arc<dyn SessionStore>,
        session_record: SessionRecord,
    ) -> Self {
        self.messages = restore_session_messages(&session_record.session);
        self.session_store = session_store;
        self.session_record = session_record;
        self
    }

    /// Replaces plugin view state after background activation completes.
    ///
    /// 后台激活完成后替换插件视图状态。
    #[cfg(feature = "plugins")]
    fn set_plugin_views(&mut self, declarations: Vec<UiDeclaration>) {
        self.plugin_views = declarations
            .into_iter()
            .map(|declaration| PluginViewState {
                declaration,
                frame: None,
                area: Rect::default(),
            })
            .collect();
    }

    /// Switches the footer from loading to ready using activation event summaries.
    ///
    /// 使用激活事件摘要将底栏从加载状态切换为就绪状态。
    #[cfg(feature = "plugins")]
    fn finish_plugin_loading(
        &mut self,
        plugin_ids: Vec<String>,
        startup_events: Vec<Value>,
        failures: Vec<PluginLoadFailure>,
    ) {
        let mut details = plugin_startup_details(&plugin_ids, &startup_events);
        self.plugin_failures = failures
            .into_iter()
            .map(|failure| {
                let blocked = if failure.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!("，依赖 {}", failure.blocked_by.join("、"))
                };
                format!(
                    "{}: 加载失败{blocked} · {}",
                    failure.plugin_id, failure.reason
                )
            })
            .collect();
        details.extend(self.plugin_failures.iter().cloned());
        self.plugin_startup_details = details;
        self.plugin_ids = plugin_ids;
        self.plugin_status_ticks = PLUGIN_STATUS_DETAIL_TICKS;
        self.plugins_loading = false;
        self.plugin_load_error = None;
    }

    /// Marks plugin IDs as loading while keeping the input queue available.
    ///
    /// 标记正在加载的插件 ID，同时保持输入队列可用。
    #[cfg(feature = "plugins")]
    fn with_loading_plugins(mut self, plugin_ids: Vec<String>) -> Self {
        self.plugin_ids = plugin_ids;
        self.plugins_loading = true;
        self.plugin_load_error = None;
        self.plugin_failures.clear();
        self
    }

    /// Records a plugin loading failure and switches the footer to a persistent error state.
    ///
    /// 记录插件加载失败，并将底栏切换为持续错误状态。
    #[cfg(feature = "plugins")]
    fn set_plugin_load_error(&mut self, error: &anyhow::Error) {
        self.plugins_loading = false;
        self.plugin_ids.clear();
        self.plugin_startup_details.clear();
        self.plugin_status_ticks = 0;
        self.plugin_load_error = Some(error.to_string());
        self.plugin_failures.clear();
    }

    /// Advances the transient startup status toward the compact counter.
    ///
    /// 推进一次性启动状态，并在计时结束后切换为紧凑计数。
    #[cfg(feature = "plugins")]
    fn tick_plugin_status(&mut self) {
        if !self.plugins_loading {
            self.plugin_status_ticks = self.plugin_status_ticks.saturating_sub(1);
        }
    }

    /// Returns the current plugin status icon and text for the footer's right side.
    ///
    /// 返回底部信息栏右侧当前使用的插件状态图标和文本。
    #[cfg(feature = "plugins")]
    fn plugin_status_content(&self) -> (&'static str, String) {
        if self.plugins_loading {
            let plugins = self.plugin_ids.join(" · ");
            let queue = if self.queued_inputs.is_empty() {
                String::new()
            } else {
                format!(" · queued {}", self.queued_inputs.len())
            };
            let text = if plugins.is_empty() {
                format!("正在加载插件{queue}")
            } else {
                format!("正在加载插件 · {plugins}{queue}")
            };
            return (SPINNER[self.spinner_frame % SPINNER.len()], text);
        }
        if let Some(error) = &self.plugin_load_error {
            return ("✗", format!("插件加载失败 · {error}"));
        }
        if self.plugin_status_ticks > 0 {
            let details = if self.plugin_startup_details.is_empty() {
                self.plugin_ids.join(" · ")
            } else {
                self.plugin_startup_details.join(" · ")
            };
            let text = if details.is_empty() {
                "未加载插件".to_string()
            } else if self.plugin_failures.is_empty() {
                format!("插件加载完成 · {details}")
            } else {
                format!("插件部分加载 · {details}")
            };
            (
                if self.plugin_failures.is_empty() {
                    "✓"
                } else {
                    "!"
                },
                text,
            )
        } else if self.plugin_failures.is_empty() {
            ("◈", format!("{} plugins", self.plugin_ids.len()))
        } else {
            (
                "◈",
                format!(
                    "{} plugins · ✗ {}",
                    self.plugin_ids.len(),
                    self.plugin_failures.len()
                ),
            )
        }
    }

    /// Returns the semantic color for the current plugin footer state.
    ///
    /// 返回当前插件底栏状态的语义颜色。
    #[cfg(feature = "plugins")]
    fn plugin_status_color(&self) -> Color {
        if self.plugin_load_error.is_some() {
            COLOR_DANGER
        } else if self.plugins_loading || !self.plugin_failures.is_empty() {
            COLOR_WARNING
        } else {
            COLOR_SUCCESS
        }
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers, agent: Option<&Arc<Agent>>) {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }

        match code {
            KeyCode::Enter => {
                if let Some(agent) = agent {
                    if self.running {
                        self.submit_steering(agent);
                    } else {
                        self.submit(agent);
                    }
                } else {
                    self.queue_input_until_ready();
                }
            }
            KeyCode::Esc => self.should_quit = true,
            KeyCode::PageUp => self.scroll_up(5),
            KeyCode::PageDown => self.scroll_down(5),
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1),
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                if let Some(prev) = self.input[..self.cursor].chars().last() {
                    self.cursor -= prev.len_utf8();
                    self.input.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if let Some(prev) = self.input[..self.cursor].chars().last() {
                    self.cursor -= prev.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(next) = self.input[self.cursor..].chars().next() {
                    self.cursor += next.len_utf8();
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            _ => {}
        }
    }

    /// 根据模态层和 Tab 焦点决定键盘事件的接收者。
    #[cfg(feature = "plugins")]
    fn route_plugin_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> PluginKeyRoute {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return PluginKeyRoute::Main;
        }

        if let Some(index) = self.active_dialog_index() {
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
            let reverse =
                matches!(code, KeyCode::BackTab) || modifiers.contains(KeyModifiers::SHIFT);
            self.cycle_plugin_focus(reverse);
            return PluginKeyRoute::Consumed;
        }

        if let Some(index) = self.plugin_focus {
            if matches!(code, KeyCode::Esc) {
                self.plugin_focus = None;
                return PluginKeyRoute::Consumed;
            }
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        PluginKeyRoute::Main
    }

    /// 将插件内容区内的鼠标事件转换为相对坐标，并在点击插件外区域时恢复主输入焦点。
    #[cfg(feature = "plugins")]
    fn route_plugin_mouse(&mut self, mouse: &MouseEvent) -> Option<UiInput> {
        let active_dialog = self.active_dialog_index();
        let target = active_dialog.or_else(|| {
            self.plugin_views
                .iter()
                .enumerate()
                .rev()
                .find(|(_, view)| {
                    plugin_view_visible(view) && point_in_rect(mouse.column, mouse.row, view.area)
                })
                .map(|(index, _)| index)
        });
        let Some(target) = target else {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.plugin_focus = None;
            }
            return None;
        };
        let view = &self.plugin_views[target];
        if !point_in_rect(mouse.column, mouse.row, view.area) {
            return None;
        }
        if active_dialog.is_none() && matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.plugin_focus = view.declaration.focusable.then_some(target);
        }
        Some(UiInput {
            plugin_id: view.declaration.plugin_id.clone(),
            view_id: view.declaration.view_id.clone(),
            event: UiInputEvent::Mouse {
                kind: plugin_mouse_kind(mouse.kind),
                x: mouse.column.saturating_sub(view.area.x),
                y: mouse.row.saturating_sub(view.area.y),
            },
        })
    }

    /// 生成当前焦点视图可识别的宿主无关键盘事件。
    #[cfg(feature = "plugins")]
    fn plugin_key_input(&self, index: usize, code: KeyCode, modifiers: KeyModifiers) -> UiInput {
        let declaration = &self.plugin_views[index].declaration;
        UiInput {
            plugin_id: declaration.plugin_id.clone(),
            view_id: declaration.view_id.clone(),
            event: UiInputEvent::Key {
                code: plugin_key_code(code),
                modifiers: plugin_key_modifiers(modifiers),
            },
        }
    }

    /// 在主输入区和所有可见、可聚焦的停靠视图之间循环焦点。
    #[cfg(feature = "plugins")]
    fn cycle_plugin_focus(&mut self, reverse: bool) {
        let focusable: Vec<usize> = self
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                view.declaration.focusable
                    && view.declaration.placement != UiPlacement::Dialog
                    && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
            .collect();
        if focusable.is_empty() {
            self.plugin_focus = None;
            return;
        }

        self.plugin_focus = match (self.plugin_focus, reverse) {
            (None, false) => focusable.first().copied(),
            (None, true) => focusable.last().copied(),
            (Some(current), false) => focusable
                .iter()
                .position(|index| *index == current)
                .and_then(|position| focusable.get(position + 1).copied()),
            (Some(current), true) => focusable
                .iter()
                .position(|index| *index == current)
                .and_then(|position| position.checked_sub(1))
                .and_then(|position| focusable.get(position).copied()),
        };
    }

    /// 返回最后声明且当前可见的模态对话框索引。
    #[cfg(feature = "plugins")]
    fn active_dialog_index(&self) -> Option<usize> {
        self.plugin_views
            .iter()
            .enumerate()
            .rev()
            .find(|(_, view)| {
                view.declaration.placement == UiPlacement::Dialog && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
    }

    /// 为每个插件视图构造下一次异步渲染请求。
    #[cfg(feature = "plugins")]
    fn plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
        self.plugin_frame = self.plugin_frame.wrapping_add(1);
        let active_dialog = self.active_dialog_index();
        self.plugin_views
            .iter()
            .enumerate()
            .map(|(index, view)| UiRenderRequest {
                plugin_id: view.declaration.plugin_id.clone(),
                view_id: view.declaration.view_id.clone(),
                width: if view.area.width == 0 {
                    view.declaration
                        .size
                        .width
                        .unwrap_or(default_plugin_width(view.declaration.placement))
                } else {
                    view.area.width
                },
                height: if view.area.height == 0 {
                    view.declaration
                        .size
                        .height
                        .unwrap_or(default_plugin_height(view.declaration.placement))
                } else {
                    view.area.height
                },
                focused: active_dialog == Some(index) || self.plugin_focus == Some(index),
                frame: self.plugin_frame,
            })
            .collect()
    }

    /// 用插件返回的新帧更新对应视图缓存。
    #[cfg(feature = "plugins")]
    fn update_plugin_frame(&mut self, plugin_id: &str, frame: PluginUiFrame) {
        if let Some(view) = self.plugin_views.iter_mut().find(|view| {
            view.declaration.plugin_id == plugin_id && view.declaration.view_id == frame.view_id
        }) {
            view.frame = Some(frame);
        }
        if self
            .plugin_focus
            .is_some_and(|index| !plugin_view_visible(&self.plugin_views[index]))
        {
            self.plugin_focus = None;
        }
    }

    /// 将单个插件的运行时 UI 错误限制在对应视图内展示。
    #[cfg(feature = "plugins")]
    fn set_plugin_ui_error(&mut self, plugin_id: &str, view_id: &str, error: &anyhow::Error) {
        self.update_plugin_frame(
            plugin_id,
            PluginUiFrame {
                view_id: view_id.to_string(),
                visible: true,
                lines: vec![UiLine {
                    spans: vec![UiSpan {
                        text: format!("插件界面错误：{error:#}"),
                        style: UiStyle {
                            foreground: Some(UiColor::Red),
                            ..UiStyle::default()
                        },
                    }],
                }],
            },
        );
    }

    /// 向上滚动 n 行，进入手动滚动模式。
    fn scroll_up(&mut self, n: u16) {
        let current = self.scroll.unwrap_or(self.last_max_scroll);
        self.scroll = Some(current.saturating_sub(n));
    }

    /// 向下滚动 n 行；到达底部时恢复自动跟随。
    fn scroll_down(&mut self, n: u16) {
        if let Some(current) = self.scroll {
            let next = current.saturating_add(n);
            if next >= self.last_max_scroll {
                self.scroll = None;
            } else {
                self.scroll = Some(next);
            }
        }
    }

    /// Takes and clears the current editor value, returning `None` for blank input.
    ///
    /// 取出并清空当前编辑器内容；空白输入返回 `None`。
    fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        (!input.is_empty()).then_some(input)
    }

    /// Handles commands that do not require a ready Agent and reports whether input was consumed.
    ///
    /// 处理无需 Agent 就绪的本地命令，并返回输入是否已被消费。
    fn handle_local_command(&mut self, input: &str) -> bool {
        match input {
            "/quit" | "/exit" => {
                self.should_quit = true;
                true
            }
            "/clear" => {
                self.session_record.session = Session::new();
                self.messages.clear();
                self.queued_inputs.clear();
                self.streaming_message = None;
                self.messages.push(Msg::new(MsgKind::Info, "会话已清空"));
                true
            }
            _ => false,
        }
    }

    /// Queues one complete input while plugin loading keeps the Agent unavailable.
    ///
    /// 插件加载导致 Agent 尚不可用时，将一条完整输入加入 FIFO 队列。
    fn queue_input_until_ready(&mut self) {
        let Some(input) = self.take_input() else {
            return;
        };
        if self.handle_local_command(&input) {
            return;
        }
        self.messages.push(Msg::new(MsgKind::User, input.clone()));
        self.queued_inputs.push_back(input);
        self.scroll = None;
    }

    /// 运行中提交 steering 插话：跳过剩余工具，让模型立即响应新指令。
    fn submit_steering(&mut self, agent: &Arc<Agent>) {
        let Some(input) = self.take_input() else {
            return;
        };
        agent.steer(input.clone());
        self.messages.push(Msg::new(MsgKind::User, input));
        self.messages.push(Msg::new(
            MsgKind::Info,
            "插话已排队，将在当前工具完成后生效",
        ));
    }

    /// Submits the current editor value immediately to a ready Agent.
    ///
    /// 将当前编辑器内容立即提交给已就绪的 Agent。
    fn submit(&mut self, agent: &Arc<Agent>) {
        let Some(input) = self.take_input() else {
            return;
        };
        if self.handle_local_command(&input) {
            return;
        }
        self.start_input_run(agent, input, true);
    }

    /// Starts one Agent run and optionally appends the user message to the visible history.
    ///
    /// 启动一次 Agent 运行，并按需把用户消息追加到可见历史。
    fn start_input_run(&mut self, agent: &Arc<Agent>, input: String, show_user_message: bool) {
        if show_user_message {
            self.messages.push(Msg::new(MsgKind::User, input.clone()));
        }
        self.running = true;
        self.streaming_message = None;
        self.scroll = None;

        let agent = Arc::clone(agent);
        let tx = self.tx.clone();
        let session_store = Arc::clone(&self.session_store);
        let session_record = self.session_record.clone();

        tokio::spawn(async move {
            let result = run_and_persist(
                agent.as_ref(),
                session_store.as_ref(),
                session_record,
                &input,
            )
            .await;
            let _ = tx.send(UiEvent::AgentDone(Box::new(result)));
        });
    }

    /// Starts the next pre-ready input after the Agent becomes idle.
    ///
    /// Agent 就绪且空闲后启动下一条预加载输入。
    fn run_next_queued(&mut self, agent: &Arc<Agent>) {
        if self.running {
            return;
        }
        if let Some(input) = self.queued_inputs.pop_front() {
            self.start_input_run(agent, input, false);
        }
    }

    /// 开始一个新的模型响应轮次，后续增量会写入新的助手消息。
    fn start_model_response(&mut self) {
        self.streaming_message = None;
    }

    /// 将文本增量追加到当前助手消息；不改动滚动位置，用户回看历史时不被拉回底部。
    fn append_model_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = self.streaming_message.unwrap_or_else(|| {
            self.messages
                .push(Msg::new(MsgKind::Assistant, String::new()));
            let index = self.messages.len() - 1;
            self.streaming_message = Some(index);
            index
        });
        if let Some(message) = self.messages.get_mut(index) {
            message.text.push_str(delta);
        }
    }

    /// 完成 Agent 运行，用最终文本校准流式消息并保存会话；保留用户当前滚动位置。
    fn handle_agent_done(&mut self, result: Result<(AgentRun, SessionRecord)>) {
        self.running = false;
        match result {
            Ok((run, saved_record)) => {
                let text = if run.final_text.trim().is_empty() {
                    "（模型返回了空回复）".to_string()
                } else {
                    run.final_text.clone()
                };
                if let Some(index) = self.streaming_message.take() {
                    if let Some(message) = self.messages.get_mut(index) {
                        message.text = text;
                    }
                } else {
                    self.messages.push(Msg::new(MsgKind::Assistant, text));
                }
                if !run.usage.is_empty() {
                    self.messages.push(Msg::new(
                        MsgKind::Info,
                        format!(
                            "↑{} ↓{} Σ{} tokens · {} 步",
                            run.usage.input_tokens.unwrap_or(0),
                            run.usage.output_tokens.unwrap_or(0),
                            run.usage.total_tokens.unwrap_or(0),
                            run.steps_used,
                        ),
                    ));
                }
                self.session_record = saved_record;
            }
            Err(e) => {
                self.streaming_message = None;
                self.messages.push(Msg::new(MsgKind::Error, e.to_string()));
            }
        }
    }
}

/// 在当前已确认会话上运行一轮 Agent，并以原 revision 执行 CAS 保存。
///
/// 只有 Agent 和存储均成功时才返回新记录。调用方应在错误时继续持有传入记录，
/// 避免把未完成或未持久化的会话暴露为当前状态。
async fn run_and_persist(
    agent: &Agent,
    session_store: &dyn SessionStore,
    mut session_record: SessionRecord,
    input: &str,
) -> Result<(AgentRun, SessionRecord)> {
    let expected_revision = (session_record.revision > 0).then_some(session_record.revision);
    let run = if session_record.session.messages().is_empty() {
        agent.run(input).await?
    } else {
        agent
            .run_continue(session_record.session.clone(), input)
            .await?
    };
    session_record.session = run.session.clone();
    if session_record.title.is_none() {
        session_record.title = session_title(input);
    }
    let saved_record = session_store
        .save(session_record, expected_revision)
        .await?;
    Ok((run, saved_record))
}

/// 判断插件视图是否已经返回可见帧。
#[cfg(feature = "plugins")]
fn plugin_view_visible(view: &PluginViewState) -> bool {
    view.frame.as_ref().is_some_and(|frame| frame.visible)
}

/// 判断终端坐标是否位于给定矩形内。
#[cfg(feature = "plugins")]
fn point_in_rect(x: u16, y: u16, area: Rect) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

/// 将 Crossterm 鼠标动作转换为稳定的字符串名称。
#[cfg(feature = "plugins")]
fn plugin_mouse_kind(kind: MouseEventKind) -> String {
    match kind {
        MouseEventKind::Down(button) => format!("down_{button:?}").to_ascii_lowercase(),
        MouseEventKind::Up(button) => format!("up_{button:?}").to_ascii_lowercase(),
        MouseEventKind::Drag(button) => format!("drag_{button:?}").to_ascii_lowercase(),
        MouseEventKind::Moved => "moved".into(),
        MouseEventKind::ScrollDown => "scroll_down".into(),
        MouseEventKind::ScrollUp => "scroll_up".into(),
        MouseEventKind::ScrollLeft => "scroll_left".into(),
        MouseEventKind::ScrollRight => "scroll_right".into(),
    }
}

/// 将 Crossterm 按键转换为稳定、可跨语言处理的名称。
#[cfg(feature = "plugins")]
fn plugin_key_code(code: KeyCode) -> String {
    match code {
        KeyCode::Char(character) => character.to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "page_up".into(),
        KeyCode::PageDown => "page_down".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "back_tab".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Esc => "escape".into(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// 按固定顺序输出按键修饰符，避免插件依赖 Crossterm 位标记。
#[cfg(feature = "plugins")]
fn plugin_key_modifiers(modifiers: KeyModifiers) -> Vec<String> {
    [
        (KeyModifiers::CONTROL, "control"),
        (KeyModifiers::ALT, "alt"),
        (KeyModifiers::SHIFT, "shift"),
        (KeyModifiers::SUPER, "super"),
        (KeyModifiers::HYPER, "hyper"),
        (KeyModifiers::META, "meta"),
    ]
    .into_iter()
    .filter(|(modifier, _)| modifiers.contains(*modifier))
    .map(|(_, name)| name.to_string())
    .collect()
}

/// 返回未实际布局前使用的插件视图默认宽度。
#[cfg(feature = "plugins")]
fn default_plugin_width(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Left | UiPlacement::Right => 28,
        UiPlacement::Dialog => 60,
        UiPlacement::Top | UiPlacement::Bottom => 40,
    }
}

/// 返回未实际布局前使用的插件视图默认高度。
#[cfg(feature = "plugins")]
fn default_plugin_height(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Top | UiPlacement::Bottom => 6,
        UiPlacement::Dialog => 20,
        UiPlacement::Left | UiPlacement::Right => 10,
    }
}

// ─── 事件 Sink：将 agent 事件转发到 UI 通道 ───

struct ChannelEventSink(mpsc::UnboundedSender<UiEvent>);

#[async_trait]
impl EventSink for ChannelEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        let name = || {
            event
                .payload
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string()
        };
        match event.kind {
            AgentEventKind::ModelRequest => {
                let _ = self.0.send(UiEvent::ModelStarted);
            }
            AgentEventKind::ModelResponse => {
                // 转发本轮请求的 input tokens，作为当前上下文大小显示在底栏。
                if let Some(tokens) = event
                    .payload
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                {
                    let _ = self.0.send(UiEvent::ContextUsage(tokens));
                }
            }
            AgentEventKind::ModelTextDelta => {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    let _ = self.0.send(UiEvent::ModelTextDelta(delta.to_string()));
                }
            }
            AgentEventKind::ToolStarted => {
                // 参数压缩为单行摘要，展示宽度由 UI 侧统一控制。
                let args = event
                    .payload
                    .get("args")
                    .map(|value| summarize_json(value, 64))
                    .unwrap_or_default();
                let _ = self.0.send(UiEvent::ToolStarted { name: name(), args });
            }
            AgentEventKind::ToolFinished => {
                let is_error = event
                    .payload
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let result = event
                    .payload
                    .get("result")
                    .map(|value| summarize_json(value, 96))
                    .unwrap_or_default();
                let _ = self.0.send(UiEvent::ToolFinished {
                    name: name(),
                    is_error,
                    result,
                });
            }
            AgentEventKind::ToolSkipped => {
                let _ = self.0.send(UiEvent::ToolSkipped(name()));
            }
            AgentEventKind::SteeringInjected => {
                let _ = self.0.send(UiEvent::SteeringInjected);
            }
            AgentEventKind::FollowUpInjected => {
                let _ = self.0.send(UiEvent::FollowUpInjected);
            }
            AgentEventKind::Extension => {
                let presentation = event.payload.get("presentation");
                let target = presentation
                    .and_then(|value| value.get("target"))
                    .and_then(Value::as_str)
                    .unwrap_or("main_event_list");
                if target == "main_event_list" {
                    let text = presentation
                        .and_then(|value| value.get("text"))
                        .and_then(Value::as_str)
                        .or_else(|| event.payload.pointer("/data/text").and_then(Value::as_str))
                        .or_else(|| event.payload.get("name").and_then(Value::as_str))
                        .unwrap_or("扩展事件")
                        .to_string();
                    let divider = presentation
                        .and_then(|value| value.get("variant"))
                        .and_then(Value::as_str)
                        == Some("divider");
                    let color = match presentation
                        .and_then(|value| value.get("tone"))
                        .and_then(Value::as_str)
                    {
                        Some("success") => COLOR_SUCCESS,
                        Some("warning") => COLOR_WARNING,
                        Some("error") => COLOR_DANGER,
                        Some("muted") => COLOR_MUTED,
                        _ => COLOR_USER,
                    };
                    let _ = self.0.send(UiEvent::Extension {
                        text,
                        color,
                        divider,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

// ─── 渲染 ───

/// 渲染完整界面，并同步当前对话区的最大滚动位置。
fn render(frame: &mut Frame, app: &mut App) {
    #[cfg(feature = "plugins")]
    for view in &mut app.plugin_views {
        view.area = Rect::default();
    }
    let outer = frame.area().inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    #[cfg(feature = "plugins")]
    let workspace = render_docked_plugin_views(frame, app, outer);
    #[cfg(not(feature = "plugins"))]
    let workspace = outer;
    render_main(frame, app, workspace);
    #[cfg(feature = "plugins")]
    render_plugin_dialog(frame, app, outer);
}

/// 在插件占用后的中心区域渲染 Lucia 主界面。
fn render_main(frame: &mut Frame, app: &mut App, workspace: Rect) {
    // Keep the input and footer compact so the released status row returns to the chat viewport.
    // 输入区与底栏保持紧凑，将释放的插件状态行归还给对话视口。
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(workspace);

    // 消息流不使用容器边框，宽屏下由居中工作区控制阅读长度。
    let chat_area = sections[0].inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut lines: Vec<Line> = app
        .messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| message.to_lines(app.streaming_message == Some(index)))
        .collect();
    if app.running && app.streaming_message.is_none() {
        let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), Style::new().fg(COLOR_WARNING)),
            Span::styled("Working...", Style::new().fg(COLOR_MUTED)),
        ]));
    }

    // 按换行后的实际显示行数计算滚动范围，兼容中文等宽字符。
    let inner_width = chat_area.width.max(1);
    let wrapped_height: u16 = lines
        .iter()
        .map(|line| {
            let width = line.width() as u16;
            width.div_ceil(inner_width).max(1)
        })
        .sum();
    let viewport = chat_area.height;
    let max_scroll = wrapped_height.saturating_sub(viewport);
    app.last_max_scroll = max_scroll;
    // 手动滚动位置到达底部后恢复自动跟随。
    if app.scroll.is_some_and(|value| value >= max_scroll) {
        app.scroll = None;
    }
    let scroll = app.scroll.unwrap_or(max_scroll);

    let chat = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(chat, chat_area);

    // 输入区仅保留顶部规则线，形成稳定的底部命令栏。
    let input_area = sections[1];
    #[cfg(feature = "plugins")]
    let agent_waiting = app.plugins_loading;
    #[cfg(not(feature = "plugins"))]
    let agent_waiting = false;
    let input_color = if app.running || agent_waiting {
        COLOR_WARNING
    } else {
        COLOR_USER
    };
    #[cfg(feature = "plugins")]
    let main_input_focused = app.plugin_focus.is_none() && app.active_dialog_index().is_none();
    #[cfg(not(feature = "plugins"))]
    let main_input_focused = true;
    let input_block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(if app.running || agent_waiting {
            COLOR_WARNING
        } else if main_input_focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_MUTED
        }))
        .padding(Padding::horizontal(1));
    let input_inner = input_block.inner(input_area);

    if app.input.is_empty() {
        let placeholder = if agent_waiting {
            "Queue while plugins load..."
        } else if app.running {
            "Steer the current run..."
        } else {
            "Message Lucia..."
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", Style::new().fg(input_color).bold()),
                Span::styled(placeholder, Style::new().fg(COLOR_MUTED)),
            ]))
            .block(input_block),
            input_area,
        );
        if main_input_focused {
            frame.set_cursor_position((input_inner.x + 2, input_inner.y));
        }
    } else {
        let input_widget = Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::new().fg(input_color).bold()),
            Span::styled(app.input.as_str(), Style::new().fg(COLOR_TEXT)),
        ]))
        .block(input_block);
        frame.render_widget(input_widget, input_area);
        // 使用显示宽度定位光标，确保中文等全角字符不会造成偏移。
        let cursor_width = unicode_width::UnicodeWidthStr::width(&app.input[..app.cursor]) as u16;
        if main_input_focused {
            frame.set_cursor_position((input_inner.x + 2 + cursor_width, input_inner.y));
        }
    }

    // 底部信息行：模型、工作目录与当前上下文 token 数，窄终端时隐藏目录。
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let cwd_display = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => cwd.replacen(&home, "~", 1),
        _ => cwd,
    };
    let mut footer = vec![Span::styled(
        app.model_name.as_str(),
        Style::new().fg(COLOR_TEXT),
    )];
    if workspace.width >= 64 {
        footer.extend([
            Span::styled("  ·  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(
                truncate_line(
                    &format!(
                        "session {} · r{}",
                        app.session_record.id, app.session_record.revision
                    ),
                    30,
                ),
                Style::new().fg(COLOR_MUTED),
            ),
        ]);
    }
    if workspace.width >= 96 {
        footer.extend([
            Span::styled("  ·  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(cwd_display, Style::new().fg(COLOR_MUTED)),
        ]);
    }
    if let Some(tokens) = app.context_tokens {
        footer.extend([
            Span::styled("  ·  ", Style::new().fg(COLOR_BORDER_FOCUS)),
            Span::styled(format!("ctx {tokens}"), Style::new().fg(COLOR_MUTED)),
        ]);
    }
    let footer_area = sections[2];
    #[cfg(feature = "plugins")]
    let (metadata_area, plugin_area, plugin_icon, plugin_status, plugin_color) = {
        let (icon, status) = app.plugin_status_content();
        let color = app.plugin_status_color();
        let desired_width =
            unicode_width::UnicodeWidthStr::width(format!("{icon} {status}").as_str())
                .saturating_add(2);
        let plugin_width = u16::try_from(desired_width)
            .unwrap_or(u16::MAX)
            .min(footer_area.width / 2);
        let columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(plugin_width)])
            .split(footer_area);
        (columns[0], columns[1], icon, status, color)
    };
    #[cfg(not(feature = "plugins"))]
    let metadata_area = footer_area;
    frame.render_widget(
        Paragraph::new(Line::from(footer)).block(Block::new().padding(Padding::horizontal(1))),
        metadata_area,
    );
    // Reserve a right-aligned footer region for plugin details without overlapping metadata.
    // 在底栏右侧为插件详情预留独立区域，避免覆盖左侧元数据。
    #[cfg(feature = "plugins")]
    {
        let status = truncate_line(
            &plugin_status,
            usize::from(plugin_area.width.saturating_sub(4).max(1)),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{plugin_icon} "), Style::new().fg(plugin_color)),
                Span::styled(status, Style::new().fg(COLOR_MUTED)),
            ]))
            .alignment(Alignment::Right)
            .block(Block::new().padding(Padding::horizontal(1))),
            plugin_area,
        );
    }
}

/// 按加载顺序分配四向停靠插槽，并返回剩余的主界面区域。
#[cfg(feature = "plugins")]
fn render_docked_plugin_views(frame: &mut Frame, app: &mut App, outer: Rect) -> Rect {
    let mut remaining = outer;
    for placement in [
        UiPlacement::Top,
        UiPlacement::Bottom,
        UiPlacement::Left,
        UiPlacement::Right,
    ] {
        let indices: Vec<usize> = app
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                view.declaration.placement == placement && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
            .collect();
        for index in indices {
            let requested = match placement {
                UiPlacement::Top | UiPlacement::Bottom => app.plugin_views[index]
                    .declaration
                    .size
                    .height
                    .unwrap_or_else(|| default_plugin_height(placement)),
                UiPlacement::Left | UiPlacement::Right => app.plugin_views[index]
                    .declaration
                    .size
                    .width
                    .unwrap_or_else(|| default_plugin_width(placement)),
                UiPlacement::Dialog => 0,
            };
            let (plugin_area, next_remaining) = split_plugin_area(remaining, placement, requested);
            remaining = next_remaining;
            if plugin_area.is_empty() {
                continue;
            }
            let focused = app.plugin_focus == Some(index);
            render_plugin_view(frame, &mut app.plugin_views[index], plugin_area, focused);
        }
    }
    remaining
}

/// 从剩余区域的一侧切出插件区域，同时为主界面保留最小可用空间。
#[cfg(feature = "plugins")]
fn split_plugin_area(area: Rect, placement: UiPlacement, requested: u16) -> (Rect, Rect) {
    match placement {
        UiPlacement::Top => {
            let size = requested.min(area.height.saturating_sub(6));
            (
                Rect::new(area.x, area.y, area.width, size),
                Rect::new(
                    area.x,
                    area.y.saturating_add(size),
                    area.width,
                    area.height.saturating_sub(size),
                ),
            )
        }
        UiPlacement::Bottom => {
            let size = requested.min(area.height.saturating_sub(6));
            (
                Rect::new(
                    area.x,
                    area.y.saturating_add(area.height.saturating_sub(size)),
                    area.width,
                    size,
                ),
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(size)),
            )
        }
        UiPlacement::Left => {
            let size = requested.min(area.width.saturating_sub(24));
            (
                Rect::new(area.x, area.y, size, area.height),
                Rect::new(
                    area.x.saturating_add(size),
                    area.y,
                    area.width.saturating_sub(size),
                    area.height,
                ),
            )
        }
        UiPlacement::Right => {
            let size = requested.min(area.width.saturating_sub(24));
            (
                Rect::new(
                    area.x.saturating_add(area.width.saturating_sub(size)),
                    area.y,
                    size,
                    area.height,
                ),
                Rect::new(area.x, area.y, area.width.saturating_sub(size), area.height),
            )
        }
        UiPlacement::Dialog => (Rect::default(), area),
    }
}

/// 渲染一个插件视图并记录去除边框后的实际内容区。
#[cfg(feature = "plugins")]
fn render_plugin_view(frame: &mut Frame, view: &mut PluginViewState, area: Rect, focused: bool) {
    let border_color = if focused {
        COLOR_BORDER_FOCUS
    } else {
        COLOR_MUTED
    };
    let block = Block::bordered()
        .title(format!(" {} ", view.declaration.title))
        .border_style(Style::new().fg(border_color));
    let content_area = block.inner(area);
    view.area = content_area;
    let lines = view
        .frame
        .as_ref()
        .map(plugin_frame_lines)
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if focused && !content_area.is_empty() {
        frame.set_cursor_position((content_area.x, content_area.y));
    }
}

/// 在主界面之上渲染最后一个可见对话框，并让它优先获得终端光标。
#[cfg(feature = "plugins")]
fn render_plugin_dialog(frame: &mut Frame, app: &mut App, outer: Rect) {
    let Some(index) = app.active_dialog_index() else {
        return;
    };
    let declaration = &app.plugin_views[index].declaration;
    let width = declaration
        .size
        .width
        .unwrap_or_else(|| default_plugin_width(UiPlacement::Dialog))
        .min(outer.width.saturating_sub(2));
    let height = declaration
        .size
        .height
        .unwrap_or_else(|| default_plugin_height(UiPlacement::Dialog))
        .min(outer.height.saturating_sub(2));
    let area = Rect::new(
        outer
            .x
            .saturating_add(outer.width.saturating_sub(width) / 2),
        outer
            .y
            .saturating_add(outer.height.saturating_sub(height) / 2),
        width,
        height,
    );
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    render_plugin_view(frame, &mut app.plugin_views[index], area, true);
}

/// 将插件声明式文本帧转换成 Ratatui 行。
#[cfg(feature = "plugins")]
fn plugin_frame_lines(plugin_frame: &PluginUiFrame) -> Vec<Line<'static>> {
    plugin_frame
        .lines
        .iter()
        .map(|line| {
            Line::from(
                line.spans
                    .iter()
                    .map(|span| Span::styled(span.text.clone(), plugin_style(&span.style)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// 将稳定插件样式子集映射到当前 Ratatui 样式。
#[cfg(feature = "plugins")]
fn plugin_style(plugin_style: &UiStyle) -> Style {
    let mut style = Style::new();
    if let Some(color) = plugin_style.foreground {
        style = style.fg(plugin_color(color));
    }
    if let Some(color) = plugin_style.background {
        style = style.bg(plugin_color(color));
    }
    if plugin_style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if plugin_style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if plugin_style.underlined {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if plugin_style.reversed {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// 将插件便携颜色映射到 Ratatui 颜色。
#[cfg(feature = "plugins")]
fn plugin_color(color: UiColor) -> Color {
    match color {
        UiColor::Black => Color::Black,
        UiColor::Red => Color::Red,
        UiColor::Green => Color::Green,
        UiColor::Yellow => Color::Yellow,
        UiColor::Blue => Color::Blue,
        UiColor::Magenta => Color::Magenta,
        UiColor::Cyan => Color::Cyan,
        UiColor::White => Color::White,
        UiColor::Gray => Color::DarkGray,
    }
}

/// 异步请求所有插件的新帧，并在主线程绘制前更新缓存。
#[cfg(feature = "plugins")]
async fn refresh_plugin_views(app: &mut App, plugin_host: &dyn PluginHost) {
    for request in app.plugin_render_requests() {
        let plugin_id = request.plugin_id.clone();
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            plugin_host.render_ui(&request),
        )
        .await
        {
            Ok(Ok(Some(frame))) => app.update_plugin_frame(&plugin_id, frame),
            Ok(Ok(None)) => {}
            Ok(Err(error)) => app.set_plugin_ui_error(&plugin_id, &request.view_id, &error),
            Err(_) => {
                app.set_plugin_ui_error(&plugin_id, &request.view_id, &anyhow!("插件界面渲染超时"))
            }
        }
    }
}

/// 向焦点插件发送输入，并限制单次调用阻塞主事件循环的时间。
#[cfg(feature = "plugins")]
async fn dispatch_plugin_input(plugin_host: &dyn PluginHost, input: &UiInput) -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        plugin_host.on_ui_input(input),
    )
    .await
    .map_err(|_| anyhow!("插件输入处理超时"))?
}

// ─── 主函数 ───

/// 解析 TUI 配置中的路径；CLI 路径保持相对当前工作目录的既有语义。
fn resolve_tui_path(
    cli_path: Option<&Path>,
    configured_path: Option<&Path>,
    config_path: &Path,
    fallback: PathBuf,
) -> PathBuf {
    if let Some(path) = cli_path {
        path.to_path_buf()
    } else if let Some(path) = configured_path {
        resolve_config_relative_path(config_path, path)
    } else {
        fallback
    }
}

/// 按 CLI、最近会话和配置默认值的优先级选择启动会话。
async fn load_startup_session(
    store: &dyn SessionStore,
    cli_session_id: Option<&str>,
    settings: &TuiSettings,
    cli_resume_latest: bool,
) -> Result<SessionRecord> {
    if cli_session_id.is_none() && (cli_resume_latest || settings.resume_latest) {
        let mut records = store.list().await?;
        records.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some(record) = records.into_iter().next() {
            return Ok(record);
        }
    }

    let id = cli_session_id
        .or(settings.default_session.as_deref())
        .unwrap_or("default");
    let id = SessionId::new(id)?;
    Ok(match store.load(&id).await? {
        Some(record) => record,
        None => SessionRecord::new(id, Session::new())?,
    })
}

/// 输出按最近更新时间排序的持久化会话摘要。
async fn print_persisted_sessions(store: &dyn SessionStore) -> Result<()> {
    let mut records = store.list().await?;
    records.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    if records.is_empty() {
        println!("没有持久化会话");
        return Ok(());
    }

    println!("SESSION\tREVISION\tMESSAGES\tUPDATED_MS\tTITLE");
    for record in records {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            record.id,
            record.revision,
            record.session.messages().len(),
            record.updated_at_ms,
            record.title.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// Appends default official plugins while preserving explicit manifests with the same ID.
///
/// 将默认官方插件补充到显式插件列表，并让同 ID 的显式声明优先。
#[cfg(feature = "plugins")]
fn merge_official_plugin_manifests(manifests: &mut Vec<PathBuf>, official_manifests: Vec<PathBuf>) {
    let mut plugin_ids = manifests
        .iter()
        .map(PluginManifest::load)
        .filter_map(Result::ok)
        .map(|manifest| manifest.plugin.id)
        .collect::<HashSet<_>>();
    for path in official_manifests {
        let should_append = PluginManifest::load(&path)
            .map(|manifest| plugin_ids.insert(manifest.plugin.id))
            // Keep invalid manifests for the background resilient loader to report after first paint.
            // 保留无效 manifest，由后台容错加载器报告，不在 TUI 首帧前中断。
            .unwrap_or(true);
        if should_append {
            manifests.push(path);
        }
    }
}

/// Reads stable plugin IDs for the loading footer before components are activated.
///
/// 在 component 激活前读取稳定插件 ID，供加载中的底栏展示。
#[cfg(feature = "plugins")]
fn plugin_manifest_ids(manifests: &[PathBuf]) -> Vec<String> {
    manifests
        .iter()
        .map(|path| {
            PluginManifest::load(path)
                .map(|manifest| manifest.plugin.id)
                .unwrap_or_else(|_| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| path.display().to_string())
                })
        })
        .collect()
}

/// Loads and activates plugins away from the TUI event loop.
///
/// 在 TUI 事件循环之外加载并激活插件；后续准备失败时会主动关闭已创建的宿主。
#[cfg(feature = "plugins")]
async fn load_plugins_for_tui(
    manifests: Vec<PathBuf>,
    capability_selection: HashMap<String, String>,
) -> Result<LoadedPlugins> {
    let report =
        load_wasm_plugins_resilient_with_selection(&manifests, &capability_selection).await?;
    let host = Arc::new(report.host);
    let failures = report.failures;
    let prepared = async {
        let plugin_ids = host
            .host_ids()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let plugin_views = host.ui_declarations().await?;
        let startup_events = host.drain_events().await?;
        Ok(LoadedPlugins {
            host: host.clone(),
            plugin_ids,
            plugin_views,
            startup_events,
            failures,
        })
    }
    .await;
    if prepared.is_err() {
        let _ = host.shutdown().await;
    }
    prepared
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = resolve_config_path(args.config.as_deref())?;
    if args.init {
        initialize_config(&config_path)?;
        println!("已创建 Lucia 配置：{}", config_path.display());
        println!(
            "填写 model.api_key（或配置 model.api_key_env）并确认 model.model 后即可运行 lucia"
        );
        return Ok(());
    }

    let mut config_exists = config_path.is_file();
    if args.config.is_some() && !config_exists {
        return Err(anyhow!("配置文件不存在：{}", config_path.display()));
    }
    let auto_initialized = !config_exists && !args.demo && !args.list_sessions;
    if auto_initialized {
        initialize_config(&config_path)?;
        config_exists = true;
    }
    let tui_settings = if config_exists {
        load_tui_settings(&config_path)?
    } else {
        TuiSettings::default()
    };
    let lucia_home = lucia_home_dir()?;
    let sessions_dir = resolve_tui_path(
        args.sessions_dir.as_deref(),
        tui_settings.sessions_dir.as_deref(),
        &config_path,
        lucia_home.join("sessions"),
    );
    let events_jsonl = args.events_jsonl.clone().or_else(|| {
        tui_settings
            .events_jsonl
            .as_deref()
            .map(|path| resolve_config_relative_path(&config_path, path))
    });
    let session_store = Arc::new(FileSessionStore::open(&sessions_dir).await?);
    if args.list_sessions {
        return print_persisted_sessions(session_store.as_ref()).await;
    }
    let session_record = load_startup_session(
        session_store.as_ref(),
        args.session_id.as_deref(),
        &tui_settings,
        args.resume_latest,
    )
    .await?;

    #[cfg(feature = "plugins")]
    let mut plugin_manifests = args.plugin_manifests.clone();
    #[cfg(feature = "plugins")]
    let mut capability_selection = HashMap::new();
    #[cfg(feature = "plugins")]
    if config_exists {
        let plugin_runtime = load_plugin_runtime_config(&config_path)?;
        plugin_manifests.extend(plugin_runtime.manifest_paths);
        capability_selection.extend(plugin_runtime.capability_selection);
    }
    #[cfg(feature = "plugins")]
    merge_official_plugin_manifests(
        &mut plugin_manifests,
        discover_official_plugin_manifests(&lucia_home)?,
    );

    let (gateway, options, demo_mode, mut startup_notices) = if args.demo {
        let (gateway, options) = build_demo_gateway();
        (
            gateway,
            options,
            true,
            vec!["当前使用本地演示模型，不会连接外部模型服务".to_string()],
        )
    } else if config_exists {
        let config = AgentRootConfig::load(&config_path)?;
        if configured_model_key_is_available(&config) {
            (
                config.build_gateway()?,
                config.agent_options(),
                false,
                Vec::new(),
            )
        } else {
            let key_hint = config
                .model
                .api_key_env
                .as_deref()
                .map(|name| format!("设置环境变量 {name}"))
                .unwrap_or_else(|| "在配置中设置 model.api_key 或 model.api_key_env".to_string());
            let (gateway, options) = build_demo_gateway();
            (
                gateway,
                options,
                true,
                vec![format!(
                    "未检测到模型密钥，当前使用本地演示模型；{key_hint} 后重新运行 lucia"
                )],
            )
        }
    } else {
        let (gateway, options) = build_demo_gateway();
        (gateway, options, true, Vec::new())
    };
    if auto_initialized {
        startup_notices.insert(0, format!("已创建默认配置：{}", config_path.display()));
    }

    let mut native_tools = ToolRegistry::new();
    if demo_mode {
        native_tools.register(JsonTool::new(echo_spec(), |args| async move {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(json!({ "echo": text, "source": "native" }))
        }))?;
    } else {
        // 真实模式注入内置工具集：读写文件、列目录、shell、搜索
        agent_tool::builtins::register_builtins(&mut native_tools)?;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let model_name = options.model.clone();

    // UI 通道 sink 之外，可选叠加 JSONL sink 用于排查请求与工具调用。
    let mut sink = CompositeEventSink::new();
    sink.push(Arc::new(ChannelEventSink(tx.clone())));
    if let Some(path) = events_jsonl {
        sink.push(Arc::new(JsonlEventSink::new(path.clone())));
    }

    let base_agent = Agent::new(gateway, options)
        .with_tools(native_tools)
        .with_event_sink(Arc::new(sink));
    #[cfg(feature = "plugins")]
    let mut pending_agent = Some(base_agent);
    #[cfg(feature = "plugins")]
    let mut agent: Option<Arc<Agent>> = None;
    #[cfg(not(feature = "plugins"))]
    let agent = Some(Arc::new(base_agent));
    #[cfg(feature = "plugins")]
    let mut plugin_host: Option<Arc<CompositePluginHost>> = None;
    #[cfg(feature = "plugins")]
    let loading_plugin_ids = plugin_manifest_ids(&plugin_manifests);

    let mut terminal = ratatui::init();
    // 启用鼠标捕获，支持滚轮滚动对话区
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);

    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if input_tx.send(UiEvent::Input(ev)).is_err() {
                break;
            }
        }
    });

    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));
        loop {
            interval.tick().await;
            if tick_tx.send(UiEvent::Tick).is_err() {
                break;
            }
        }
    });

    let mut app =
        App::new(tx.clone(), model_name).with_persistent_session(session_store, session_record);
    app.messages.extend(
        startup_notices
            .into_iter()
            .map(|notice| Msg::new(MsgKind::Info, notice)),
    );
    #[cfg(feature = "plugins")]
    let plugin_load_task = {
        app = app.with_loading_plugins(loading_plugin_ids);
        let load_tx = tx.clone();
        tokio::spawn(async move {
            let result = load_plugins_for_tui(plugin_manifests, capability_selection).await;
            let _ = load_tx.send(UiEvent::PluginsLoaded(Box::new(result)));
        })
    };

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;

        match rx.recv().await {
            Some(UiEvent::Input(Event::Key(key))) => {
                if key.kind == KeyEventKind::Press {
                    #[cfg(feature = "plugins")]
                    match app.route_plugin_key(key.code, key.modifiers) {
                        PluginKeyRoute::Main => {
                            app.handle_key(key.code, key.modifiers, agent.as_ref());
                        }
                        PluginKeyRoute::Consumed => {}
                        PluginKeyRoute::Input(input) => {
                            if let Some(host) = plugin_host.as_ref() {
                                if let Err(error) =
                                    dispatch_plugin_input(host.as_ref(), &input).await
                                {
                                    app.set_plugin_ui_error(
                                        &input.plugin_id,
                                        &input.view_id,
                                        &error,
                                    );
                                } else {
                                    refresh_plugin_views(&mut app, host.as_ref()).await;
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "plugins"))]
                    app.handle_key(key.code, key.modifiers, agent.as_ref());
                }
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::PluginsLoaded(result)) => {
                let mut ready_agent = pending_agent.take().expect("插件加载完成事件只能处理一次");
                match *result {
                    Ok(loaded) => {
                        ready_agent.set_extension(loaded.host.clone());
                        ready_agent.set_context_loader(loaded.host.clone());
                        app.set_plugin_views(loaded.plugin_views);
                        app.finish_plugin_loading(
                            loaded.plugin_ids,
                            loaded.startup_events,
                            loaded.failures,
                        );
                        refresh_plugin_views(&mut app, loaded.host.as_ref()).await;
                        plugin_host = Some(loaded.host);
                    }
                    Err(error) => {
                        app.set_plugin_load_error(&error);
                        app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!("插件加载失败，已切换为 Core Agent：{error}"),
                        ));
                    }
                }
                let ready_agent = Arc::new(ready_agent);
                app.run_next_queued(&ready_agent);
                agent = Some(ready_agent);
            }
            Some(UiEvent::ModelStarted) => {
                app.start_model_response();
            }
            Some(UiEvent::ModelTextDelta(delta)) => {
                app.append_model_delta(&delta);
            }
            Some(UiEvent::ToolStarted { name, args }) => {
                let mut msg = Msg::new(MsgKind::ToolRunning, name);
                msg.args = (!args.is_empty()).then_some(args);
                app.messages.push(msg);
            }
            Some(UiEvent::ToolFinished {
                name,
                is_error,
                result,
            }) => {
                // 把对应的"运行中"条目更新为最终状态，并挂上返回内容摘要
                let kind = if is_error {
                    MsgKind::ToolError
                } else {
                    MsgKind::ToolOk
                };
                let result = (!result.is_empty()).then_some(result);
                if let Some(msg) = app
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m.kind, MsgKind::ToolRunning) && m.text == name)
                {
                    msg.kind = kind;
                    msg.result = result;
                } else {
                    let mut msg = Msg::new(kind, name);
                    msg.result = result;
                    app.messages.push(msg);
                }
            }
            Some(UiEvent::ToolSkipped(name)) => {
                app.messages.push(Msg::new(MsgKind::ToolSkipped, name));
            }
            Some(UiEvent::SteeringInjected) => {
                app.messages.push(Msg::new(MsgKind::Info, "插话已生效"));
            }
            Some(UiEvent::FollowUpInjected) => {
                app.messages.push(Msg::new(MsgKind::Info, "追加任务开始"));
            }
            Some(UiEvent::Extension {
                text,
                color,
                divider,
            }) => {
                app.messages.push(Msg::extension(text, color, divider));
            }
            Some(UiEvent::ContextUsage(tokens)) => {
                app.context_tokens = Some(tokens);
            }
            Some(UiEvent::AgentDone(result)) => {
                app.handle_agent_done(*result);
                if let Some(agent) = agent.as_ref() {
                    app.run_next_queued(agent);
                }
            }
            Some(UiEvent::Tick) => {
                #[cfg(feature = "plugins")]
                let animate_spinner = app.running || app.plugins_loading;
                #[cfg(not(feature = "plugins"))]
                let animate_spinner = app.running;
                if animate_spinner {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
                #[cfg(feature = "plugins")]
                {
                    app.tick_plugin_status();
                    app.plugin_tick = app.plugin_tick.wrapping_add(1);
                    if app.plugin_tick >= 3 {
                        app.plugin_tick = 0;
                        if let Some(host) = plugin_host.as_ref() {
                            refresh_plugin_views(&mut app, host.as_ref()).await;
                        }
                    }
                }
            }
            Some(UiEvent::Input(Event::Mouse(mouse))) => {
                #[cfg(feature = "plugins")]
                {
                    let dialog_active = app.active_dialog_index().is_some();
                    if let Some(input) = app.route_plugin_mouse(&mouse) {
                        if let Some(host) = plugin_host.as_ref() {
                            if let Err(error) = dispatch_plugin_input(host.as_ref(), &input).await {
                                app.set_plugin_ui_error(&input.plugin_id, &input.view_id, &error);
                            } else {
                                refresh_plugin_views(&mut app, host.as_ref()).await;
                            }
                        }
                    } else if !dialog_active {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => app.scroll_up(3),
                            MouseEventKind::ScrollDown => app.scroll_down(3),
                            _ => {}
                        }
                    }
                }
                #[cfg(not(feature = "plugins"))]
                match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_up(3),
                    MouseEventKind::ScrollDown => app.scroll_down(3),
                    _ => {}
                }
            }
            Some(UiEvent::Input(_)) => {}
            None => break,
        }

        if app.should_quit {
            break;
        }
    }

    #[cfg(feature = "plugins")]
    plugin_load_task.abort();
    #[cfg(feature = "plugins")]
    let _ = plugin_load_task.await;
    #[cfg(feature = "plugins")]
    let plugin_shutdown_error = if let Some(host) = plugin_host {
        match tokio::time::timeout(std::time::Duration::from_secs(5), host.shutdown()).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some(anyhow!("插件宿主卸载超时")),
        }
    } else {
        None
    };

    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    #[cfg(feature = "plugins")]
    if let Some(error) = plugin_shutdown_error {
        return Err(error);
    }
    Ok(())
}

// ─── Demo 模型 ───

/// 判断配置中的模型密钥是否可以用于本次启动。
///
/// 明文密钥和环境变量任一包含非空值即视为可用；该检查不会读取或记录密钥内容。
fn configured_model_key_is_available(config: &AgentRootConfig) -> bool {
    config
        .model
        .api_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || config
            .model
            .api_key_env
            .as_deref()
            .and_then(std::env::var_os)
            .is_some_and(|value| !value.is_empty())
}

/// 构建无需外部模型服务的确定性演示运行时。
fn build_demo_gateway() -> (ModelGateway, AgentOptions) {
    let mut gateway = ModelGateway::new();
    gateway
        .register("default", Arc::new(ScriptedReActModel))
        .expect("注册脚本模型");
    let options = AgentOptions {
        provider: "default".to_string(),
        model: "scripted-react-demo".to_string(),
        ..AgentOptions::default()
    };
    (gateway, options)
}

fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo",
        "回显输入文本。",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "要回显的文本" }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    )
}

/// 确定性脚本模型，不联网即可演示 ReAct loop。
struct ScriptedReActModel;

#[async_trait]
impl ChatModel for ScriptedReActModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        if let Some(tool_text) = latest_tool_result_text(&req) {
            return Ok(ModelResponse::text(format!("工具返回: {tool_text}")));
        }
        let user_text = latest_user_text(&req).unwrap_or_default();
        if req.tools.iter().any(|t| t.name == "echo") {
            Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                "demo-call-1",
                "echo",
                json!({ "text": user_text }),
            )]))
        } else {
            Ok(ModelResponse::text(format!(
                "没有可用工具。用户说: {user_text}"
            )))
        }
    }
}

#[async_trait]
impl ProviderAdapter for ScriptedReActModel {
    fn name(&self) -> &'static str {
        "scripted-react-demo"
    }
}

fn latest_user_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(&m.role, MessageRole::User))
        .map(|m| m.text_content())
}

fn latest_tool_result_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(&m.role, MessageRole::Tool))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { result } => Some(result.content_text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    #[cfg(feature = "plugins")]
    use std::{fs, time::SystemTime};

    /// 将测试终端缓冲区转换为去除宽字符占位空格的纯文本。
    fn render_text(width: u16, height: u16, running: bool) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("创建测试终端");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.running = running;
        app.messages.extend([
            Msg::new(MsgKind::User, "测试消息"),
            Msg::new(MsgKind::Assistant, "测试回复"),
        ]);

        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("渲染测试界面");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    /// 验证常规尺寸下角色标记、输入提示和底部信息行均可见。
    #[test]
    fn render_shows_visual_hierarchy() {
        let text = render_text(100, 24, false);

        assert!(text.contains("测试模型"), "{text:?}");
        assert!(text.contains("❯测试消息"), "{text:?}");
        assert!(text.contains("●测试回复"), "{text:?}");
        assert!(text.contains("MessageLucia..."), "{text:?}");
        assert!(!text.contains("agentruntime"), "{text:?}");
        assert!(!text.contains("ReAct"), "{text:?}");
    }

    /// Startup activation events render in the footer once, then collapse into a plugin count.
    ///
    /// 启动激活事件应在底部信息栏右侧展示一次，随后收敛为插件数量。
    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_status_shows_startup_details_then_compact_count() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.finish_plugin_loading(
            vec!["mcp".into(), "skill".into()],
            vec![
                json!({
                    "source": {"id": "mcp"},
                    "data": {"text": "MCP 插件等待配置"}
                }),
                json!({
                    "source": {"id": "skill"},
                    "presentation": {"text": "已加载 1 个 Skill"}
                }),
            ],
            Vec::new(),
        );
        assert_eq!(
            app.plugin_status_content(),
            (
                "✓",
                "插件加载完成 · mcp: MCP 插件等待配置 · skill: 已加载 1 个 Skill".into()
            )
        );

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("创建插件状态测试终端");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("渲染插件启动状态");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(rendered.contains("插件加载完成"), "{rendered:?}");

        for _ in 0..PLUGIN_STATUS_DETAIL_TICKS {
            app.tick_plugin_status();
        }
        assert_eq!(app.plugin_status_content(), ("◈", "2 plugins".into()));
    }

    /// Partial plugin failures retain successes and remain visible in the compact footer count.
    ///
    /// 单插件失败应保留成功插件，并在紧凑底栏中持续显示失败数量。
    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_status_keeps_partial_successes() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.finish_plugin_loading(
            vec!["skill".into()],
            Vec::new(),
            vec![PluginLoadFailure {
                plugin_id: "mcp".into(),
                reason: "初始化超时".into(),
                blocked_by: Vec::new(),
            }],
        );

        let (icon, status) = app.plugin_status_content();
        assert_eq!(icon, "!");
        assert!(status.contains("插件部分加载"), "{status}");
        assert!(status.contains("mcp: 加载失败"), "{status}");
        assert_eq!(app.plugin_status_color(), COLOR_WARNING);

        for _ in 0..PLUGIN_STATUS_DETAIL_TICKS {
            app.tick_plugin_status();
        }
        assert_eq!(app.plugin_status_content(), ("◈", "1 plugins · ✗ 1".into()));
    }

    /// Inputs entered before readiness stay FIFO-ordered and remain visible in the loading footer.
    ///
    /// Agent 就绪前输入应保持 FIFO 顺序，并在加载底栏显示排队数量。
    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_loading_queues_inputs_until_agent_is_ready() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into())
            .with_loading_plugins(vec!["mcp".into(), "skill".into()]);

        for input in ["第一条任务", "第二条任务"] {
            app.input = input.into();
            app.cursor = app.input.len();
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
        }

        assert_eq!(
            app.queued_inputs
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["第一条任务", "第二条任务"]
        );
        assert_eq!(
            app.messages
                .iter()
                .filter(|message| matches!(message.kind, MsgKind::User))
                .count(),
            2
        );
        let (_, status) = app.plugin_status_content();
        assert!(status.contains("queued 2"), "{status}");

        app.input = "/clear".into();
        app.cursor = app.input.len();
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
        assert!(app.queued_inputs.is_empty());
    }

    /// Queued startup inputs execute sequentially and persist into one continuing session.
    ///
    /// 启动队列中的输入应逐条执行，并持久化到同一个连续 Session。
    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn ready_agent_drains_startup_queue_in_fifo_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into()).with_loading_plugins(vec!["skill".into()]);
        for input in ["第一条任务", "第二条任务"] {
            app.input = input.into();
            app.cursor = app.input.len();
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
        }

        let (gateway, options) = build_demo_gateway();
        let agent = Arc::new(Agent::new(gateway, options));
        app.run_next_queued(&agent);
        for expected_revision in [1, 2] {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let Some(UiEvent::AgentDone(result)) = rx.recv().await {
                        break *result;
                    }
                }
            })
            .await
            .expect("等待排队任务完成不应超时");
            app.handle_agent_done(result);
            assert_eq!(app.session_record.revision, expected_revision);
            app.run_next_queued(&agent);
        }

        assert!(app.queued_inputs.is_empty());
        assert!(!app.running);
        let user_messages = app
            .session_record
            .session
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.text_content())
            .collect::<Vec<_>>();
        assert_eq!(user_messages, vec!["第一条任务", "第二条任务"]);
    }

    /// 验证空模型密钥触发演示模式，而非空明文密钥允许构建真实模型运行时。
    #[test]
    fn model_key_availability_rejects_empty_values() {
        let without_key: AgentRootConfig = toml::from_str(
            r#"
                [model]
                provider = "open-ai"
                model = "test-model"
                api_key = "   "
            "#,
        )
        .expect("解析无密钥测试配置");
        assert!(!configured_model_key_is_available(&without_key));

        let with_key: AgentRootConfig = toml::from_str(
            r#"
                [model]
                provider = "open-ai"
                model = "test-model"
                api_key = "test-key"
            "#,
        )
        .expect("解析有密钥测试配置");
        assert!(configured_model_key_is_available(&with_key));
    }

    /// 验证运行状态使用 steering 文案，并在窄终端隐藏目录信息。
    #[test]
    fn render_adapts_to_running_state_and_narrow_width() {
        let text = render_text(60, 16, true);

        assert!(text.contains("Working..."), "{text:?}");
        assert!(text.contains("Steerthecurrentrun..."), "{text:?}");
        assert!(!text.contains("ascnet-lucia"), "{text:?}");
    }

    /// 验证工具行展示参数与返回内容摘要，且过长内容按显示宽度截断。
    #[test]
    fn tool_lines_show_args_and_truncated_result() {
        let mut msg = Msg::new(MsgKind::ToolOk, "read_file");
        msg.args = Some(summarize_json(&json!({ "path": "src/main.rs" }), 64));
        msg.result = Some(summarize_json(
            &json!({ "content": "很长的文件内容".repeat(30) }),
            24,
        ));

        let lines = msg.to_lines(false);
        let text: String = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("● read_file(path: src/main.rs)"), "{text:?}");
        assert!(text.contains("⎿ content: 很长的文件内容"), "{text:?}");
        assert!(text.contains('…'), "{text:?}");
    }

    /// 验证嵌套 JSON 摘要折叠为计数而不是原始序列化。
    #[test]
    fn summarize_json_folds_nested_structures() {
        let value = json!({
            "path": "src",
            "entries": [{ "name": "lib.rs" }, { "name": "main.rs" }],
            "meta": { "hidden": false }
        });

        let summary = summarize_json(&value, 96);

        assert_eq!(summary, "path: src, entries: [2 项], meta: {…}");
    }

    /// 验证持久化 Session 会恢复用户、助手和已完成工具事件，而不展示系统提示词。
    #[test]
    fn persisted_session_hydrates_main_event_list() {
        let mut session = Session::new();
        session.set_system("不应显示的系统提示词");
        session.push_user("读取项目配置");
        session.push_assistant_blocks(vec![ContentBlock::ToolCall {
            call: ToolCall::new("call-1", "read_file", json!({"path": "config.toml"})),
        }]);
        session.push_tool_result(agent_tool::ToolResult::success(
            "call-1",
            "read_file",
            json!({"content": "配置内容"}),
        ));
        session.push_assistant_text("配置已经读取");

        let messages = restore_session_messages(&session);

        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[0].kind, MsgKind::User));
        assert_eq!(messages[0].text, "读取项目配置");
        assert!(matches!(messages[1].kind, MsgKind::ToolOk));
        assert_eq!(messages[1].text, "read_file");
        assert_eq!(messages[1].args.as_deref(), Some("path: config.toml"));
        assert!(messages[1]
            .result
            .as_deref()
            .is_some_and(|result| result.contains("配置内容")));
        assert!(matches!(messages[2].kind, MsgKind::Assistant));
        assert_eq!(messages[2].text, "配置已经读取");
    }

    /// 显式会话 ID 必须优先于最近恢复，未显式指定时选择最新记录。
    #[tokio::test]
    async fn startup_session_respects_explicit_and_latest_priority() {
        let store = MemorySessionStore::new();
        let older = store
            .save(
                SessionRecord::new(
                    SessionId::new("older").expect("创建旧会话 ID"),
                    Session::new(),
                )
                .expect("创建旧会话"),
                None,
            )
            .await
            .expect("保存旧会话");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let newer = store
            .save(
                SessionRecord::new(
                    SessionId::new("newer").expect("创建新会话 ID"),
                    Session::new(),
                )
                .expect("创建新会话"),
                None,
            )
            .await
            .expect("保存新会话");
        let settings = TuiSettings {
            resume_latest: true,
            ..TuiSettings::default()
        };

        let latest = load_startup_session(&store, None, &settings, false)
            .await
            .expect("恢复最近会话");
        assert_eq!(latest.id, newer.id);

        let explicit = load_startup_session(&store, Some(older.id.as_str()), &settings, true)
            .await
            .expect("恢复显式会话");
        assert_eq!(explicit.id, older.id);
    }

    /// 验证 Markdown 表格排版为对齐行：分隔行转为横线，中文列按显示宽度补齐。
    #[test]
    fn markdown_tables_render_aligned() {
        let lines = markdown_lines(
            "说明\n\n| 模块 | 作用 |\n|---|---|\n| core | Agent 循环 |\n| tool | 工具注册 |\n\n结尾",
            false,
        );
        let rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let text = rows.join("\n");

        assert!(text.contains("┼"), "{text:?}");
        assert!(!text.contains("|---"), "{text:?}");
        // 两列数据行应等宽对齐：core 与 tool 后的竖线位置一致。
        let core_row = rows
            .iter()
            .find(|row| row.contains("core"))
            .expect("core 行");
        let tool_row = rows
            .iter()
            .find(|row| row.contains("tool"))
            .expect("tool 行");
        assert_eq!(core_row.find('│'), tool_row.find('│'), "{text:?}");
        assert!(text.contains("结尾"), "{text:?}");
    }

    /// 验证 Markdown 渲染保留标题与行内代码的强调样式，且不产生任何背景色。
    #[test]
    fn markdown_renders_emphasis_without_background() {
        let lines = markdown_lines("# 标题\n\n**重点** 与 `代码` 内容", false);

        let no_background = lines.iter().all(|line| {
            line.style.bg.is_none() && line.spans.iter().all(|span| span.style.bg.is_none())
        });
        assert!(no_background);

        let heading = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains("标题")))
            .expect("应包含标题行");
        assert!(heading.style.add_modifier.contains(Modifier::BOLD));

        let bold_span = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("重点"))
            .expect("应包含加粗片段");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    /// 验证流式增量与运行结束不会打断用户的手动滚动位置。
    #[test]
    fn manual_scroll_survives_streaming_updates() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.running = true;
        app.last_max_scroll = 40;
        app.scroll_up(10);
        assert_eq!(app.scroll, Some(30));

        app.append_model_delta("新的输出");
        let saved_record = app.session_record.clone();
        app.handle_agent_done(Ok((
            AgentRun {
                run_id: "run-scroll".into(),
                final_text: "完成".into(),
                steps_used: 1,
                usage: Default::default(),
                session: Session::new(),
            },
            saved_record,
        )));

        assert_eq!(app.scroll, Some(30));
    }

    /// 验证多个文本增量只更新一条助手消息，最终结果不会重复追加。
    #[test]
    fn streamed_deltas_update_one_assistant_message() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.running = true;
        app.start_model_response();
        app.append_model_delta("你");
        app.append_model_delta("好");

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].text, "你好");

        let saved_record = app.session_record.clone();
        app.handle_agent_done(Ok((
            AgentRun {
                run_id: "run-test".into(),
                final_text: "你好！".into(),
                steps_used: 1,
                usage: Default::default(),
                session: Session::new(),
            },
            saved_record,
        )));

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].text, "你好！");
        assert!(app.streaming_message.is_none());
    }

    /// 验证成功轮次创建并更新同一稳定会话，且每次保存都会推进 revision。
    #[tokio::test]
    async fn successful_runs_persist_with_cas_revision() {
        let (gateway, options) = build_demo_gateway();
        let agent = Agent::new(gateway, options);
        let store = MemorySessionStore::new();
        let record = SessionRecord::new(
            SessionId::new("stable-session").expect("创建稳定测试会话标识"),
            Session::new(),
        )
        .expect("创建测试会话记录");

        let (_, first) = run_and_persist(&agent, &store, record, "第一轮")
            .await
            .expect("首次运行和保存应成功");
        assert_eq!(first.revision, 1);
        assert_eq!(first.title.as_deref(), Some("第一轮"));

        let (_, second) = run_and_persist(&agent, &store, first.clone(), "第二轮")
            .await
            .expect("继续运行和保存应成功");
        assert_eq!(second.id, first.id);
        assert_eq!(second.revision, 2);
        assert_eq!(second.title.as_deref(), first.title.as_deref());
        assert_eq!(
            store.load(&second.id).await.expect("读取测试会话"),
            Some(second)
        );
    }

    /// 验证运行或保存错误不会替换应用持有的原会话记录。
    #[test]
    fn failed_completion_preserves_confirmed_session() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let original = app.session_record.clone();

        app.handle_agent_done(Err(anyhow!("模拟运行失败")));

        assert_eq!(app.session_record, original);
    }

    /// Explicit manifests override same-ID official plugins while retaining other defaults.
    ///
    /// 显式插件应覆盖同 ID 官方插件，同时保留其他官方插件。
    #[cfg(feature = "plugins")]
    #[test]
    fn explicit_plugin_manifest_overrides_official_manifest() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("生成测试时间戳")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "lucia-official-plugin-merge-{}-{nonce}",
            std::process::id()
        ));
        let explicit = root.join("explicit.toml");
        let official_same = root.join("official-same.toml");
        let official_other = root.join("official-other.toml");
        fs::create_dir_all(&root).expect("创建插件合并测试目录");
        let manifest = |id: &str, name: &str| {
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.6.0\"\nwasm = \"plugin.wasm\"\n"
            )
        };
        fs::write(&explicit, manifest("mcp", "显式 MCP")).expect("写入显式插件 manifest");
        fs::write(&official_same, manifest("mcp", "官方 MCP"))
            .expect("写入同 ID 官方插件 manifest");
        fs::write(&official_other, manifest("skill", "官方 Skill"))
            .expect("写入其他官方插件 manifest");

        let mut manifests = vec![explicit.clone()];
        merge_official_plugin_manifests(
            &mut manifests,
            vec![official_same, official_other.clone()],
        );

        assert_eq!(manifests, vec![explicit, official_other]);
        fs::remove_dir_all(root).expect("清理插件合并测试目录");
    }

    /// Invalid manifests remain visible to background loading instead of blocking first paint.
    ///
    /// 无效 manifest 应交给后台加载器报告，不应阻断 TUI 首帧。
    #[cfg(feature = "plugins")]
    #[test]
    fn invalid_plugin_manifest_does_not_block_startup_labels() {
        let invalid = PathBuf::from("/tmp/lucia-invalid-plugin.toml");
        let labels = plugin_manifest_ids(std::slice::from_ref(&invalid));

        assert_eq!(labels, vec!["lucia-invalid-plugin.toml"]);
    }

    /// 创建测试插件视图，覆盖停靠、对话框和焦点路由测试。
    #[cfg(feature = "plugins")]
    fn test_plugin_view(placement: UiPlacement, title: &str) -> PluginViewState {
        PluginViewState {
            declaration: UiDeclaration {
                plugin_id: "test-plugin".into(),
                view_id: format!("{placement:?}").to_ascii_lowercase(),
                title: title.into(),
                placement,
                size: agent_plugin_host::ui::UiSize {
                    width: Some(24),
                    height: Some(8),
                },
                focusable: true,
            },
            frame: Some(PluginUiFrame {
                view_id: format!("{placement:?}").to_ascii_lowercase(),
                visible: true,
                lines: vec![UiLine {
                    spans: vec![UiSpan {
                        text: "插件内容".into(),
                        style: UiStyle::default(),
                    }],
                }],
            }),
            area: Rect::default(),
        }
    }

    /// 验证右侧插槽与主界面可以同时渲染，且插件获得实际内容尺寸。
    #[test]
    #[cfg(feature = "plugins")]
    fn plugin_dock_renders_inside_main_ui() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("创建测试终端");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.messages.push(Msg::new(MsgKind::User, "主界面内容"));
        app.plugin_views
            .push(test_plugin_view(UiPlacement::Right, "右侧插件"));

        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("渲染带插件的界面");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();

        assert!(text.contains("主界面内容"), "{text:?}");
        assert!(text.contains("右侧插件"), "{text:?}");
        assert!(text.contains("插件内容"), "{text:?}");
        assert!(app.plugin_views[0].area.width > 0);
    }

    /// 验证可见对话框覆盖主界面并优先接收按键。
    #[test]
    #[cfg(feature = "plugins")]
    fn plugin_dialog_is_modal() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.plugin_views
            .push(test_plugin_view(UiPlacement::Dialog, "插件对话框"));

        let route = app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE);
        let PluginKeyRoute::Input(input) = route else {
            panic!("对话框应优先接收输入");
        };

        assert_eq!(input.plugin_id, "test-plugin");
        assert_eq!(input.view_id, "dialog");
        assert!(matches!(
            input.event,
            UiInputEvent::Key { ref code, .. } if code == "enter"
        ));
    }

    /// 验证 Tab 在主输入区与可聚焦停靠视图之间循环。
    #[test]
    #[cfg(feature = "plugins")]
    fn tab_cycles_plugin_focus() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.plugin_views
            .push(test_plugin_view(UiPlacement::Left, "左侧插件"));

        assert!(matches!(
            app.route_plugin_key(KeyCode::Tab, KeyModifiers::NONE),
            PluginKeyRoute::Consumed
        ));
        assert_eq!(app.plugin_focus, Some(0));
        assert!(matches!(
            app.route_plugin_key(KeyCode::Tab, KeyModifiers::NONE),
            PluginKeyRoute::Consumed
        ));
        assert_eq!(app.plugin_focus, None);
    }

    /// 验证点击插件外的主界面会释放插件焦点，使字符重新进入主输入框。
    #[test]
    #[cfg(feature = "plugins")]
    fn clicking_main_view_restores_input_focus() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let mut view = test_plugin_view(UiPlacement::Left, "左侧插件");
        view.area = Rect::new(0, 0, 10, 10);
        app.plugin_views.push(view);
        app.plugin_focus = Some(0);

        let routed = app.route_plugin_mouse(&MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 20,
            row: 20,
            modifiers: KeyModifiers::NONE,
        });

        assert!(routed.is_none());
        assert_eq!(app.plugin_focus, None);
        assert!(matches!(
            app.route_plugin_key(KeyCode::Char('a'), KeyModifiers::NONE),
            PluginKeyRoute::Main
        ));
    }
}
