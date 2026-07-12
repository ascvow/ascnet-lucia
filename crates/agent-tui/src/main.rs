//! Lucia 交互式 TUI（基于 Ratatui）。

mod app_config;
mod tui;

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
use agent_core::{ContextLoadRequest, LoadedContext};
#[cfg(feature = "plugins")]
use agent_plugin_host::{
    manifest::{load_plugin_runtime_config, PluginManifest},
    ui::{
        UiColor, UiDeclaration, UiFrame as PluginUiFrame, UiInput, UiInputEvent, UiLine,
        UiNavigationRequest, UiPlacement, UiRenderRequest, UiSpan, UiStyle, UI_NAVIGATION_EVENT,
    },
    wasm::{load_wasm_plugins_resilient_with_selection_and_services, PluginLoadFailure},
    CompositePluginHost, PluginHost, PluginHostServices, PluginServiceCall,
};
#[cfg(feature = "plugins")]
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate, RuntimeLimits,
};
use agent_session::{
    FileSessionStore, MemorySessionStore, SessionId, SessionRecord, SessionStore,
    SessionStoreError, SessionSummary,
};
use agent_tool::{JsonTool, ToolCall, ToolRegistry, ToolSpec};
#[cfg(feature = "plugins")]
use anyhow::Context;
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
use command_protocol::{
    CommandAvailability, CommandCallbackResponse, CommandSnapshot, CommandSpec, CompletionContext,
    CompletionItem, PrepareCompletionRequest, PrepareCompletionResponse, PrepareExecuteRequest,
    PrepareExecuteResponse, SessionListStatus, SessionSummary as CommandSessionSummary,
    SnapshotRequest, SurfaceAction, SurfaceEffect, SurfaceEffectsResponse, SurfaceUpdateRequest,
    PREPARE_COMPLETION_SERVICE, PREPARE_EXECUTE_SERVICE, PROVIDER_PLUGIN_ID,
    SESSION_COMPLETION_SOURCE, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE, SURFACE_POLL_EFFECTS_SERVICE,
    SURFACE_UPDATE_SERVICE,
};
#[cfg(feature = "plugins")]
use crossterm::event::MouseEvent;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{prelude::*, widgets::*};
#[cfg(feature = "plugins")]
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
#[cfg(feature = "plugins")]
use std::collections::{HashMap, HashSet};
use std::{
    path::Path,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::mpsc;
use tui::render_root;
#[cfg(feature = "plugins")]
use tui::{
    apply_plugin_navigation_event, dispatch_plugin_input, drain_plugin_ui_events,
    refresh_plugin_view, refresh_plugin_views, view::ViewStack,
};

/// 启动目录派生出的稳定项目上下文。
#[derive(Debug, Clone)]
struct WorkspaceContext {
    /// 规范化后的项目工作目录。
    cwd: PathBuf,
    /// 用于隔离项目会话空间的稳定摘要。
    project_id: String,
}

impl WorkspaceContext {
    /// 捕获当前进程工作目录并生成稳定项目标识。
    fn capture() -> Result<Self> {
        let cwd = std::env::current_dir()?.canonicalize()?;
        let project_id = workspace_project_id(&cwd);
        Ok(Self { cwd, project_id })
    }

    /// 创建尚未落盘的空白会话记录，并绑定当前项目标识。
    fn draft_record(&self) -> Result<SessionRecord> {
        self.record_with_id(SessionId::generate())
    }

    /// 使用指定标识创建绑定当前项目的空白会话记录。
    fn record_with_id(&self, id: SessionId) -> Result<SessionRecord> {
        let mut record = SessionRecord::new(id, Session::new())?;
        record.metadata.insert(
            "lucia.project_id".to_string(),
            Value::String(self.project_id.clone()),
        );
        Ok(record)
    }

    /// 返回当前项目的会话存储目录。
    fn sessions_dir(&self, root: &Path) -> PathBuf {
        root.join(&self.project_id).join("sessions")
    }
}

/// 使用操作系统原始路径表示生成稳定项目标识，避免有损字符串转换造成目录碰撞。
fn workspace_project_id(cwd: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"lucia-project-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        digest.update(cwd.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in cwd.as_os_str().encode_wide() {
            digest.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    digest.update(cwd.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
}

/// 只在首次实际读取已有目录或保存消息时打开文件会话存储。
struct LazyFileSessionStore {
    /// 当前项目的最终会话目录。
    root: PathBuf,
    /// 首次实际打开后复用的文件存储实例。
    store: tokio::sync::OnceCell<FileSessionStore>,
}

impl LazyFileSessionStore {
    /// 创建不会触碰文件系统的惰性存储句柄。
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            store: tokio::sync::OnceCell::new(),
        }
    }

    /// 打开存储目录；只有保存路径会在目录不存在时调用本方法。
    async fn open(&self) -> Result<&FileSessionStore, SessionStoreError> {
        self.store
            .get_or_try_init(|| async { FileSessionStore::open(&self.root).await })
            .await
    }

    /// 目录不存在时保持惰性，已有目录则通过文件存储完成安全校验。
    async fn existing(&self) -> Result<Option<&FileSessionStore>, SessionStoreError> {
        if let Some(store) = self.store.get() {
            return Ok(Some(store));
        }
        match tokio::fs::symlink_metadata(&self.root).await {
            Ok(_) => self.open().await.map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SessionStoreError::Io {
                operation: "检查会话存储目录",
                path: self.root.clone(),
                source,
            }),
        }
    }
}

#[async_trait]
impl SessionStore for LazyFileSessionStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.load(id).await,
            None => Ok(None),
        }
    }

    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        self.open().await?.save(record, expected_revision).await
    }

    async fn delete(
        &self,
        id: &SessionId,
        expected_revision: u64,
    ) -> Result<(), SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.delete(id, expected_revision).await,
            None => Err(SessionStoreError::RevisionConflict {
                id: id.clone(),
                expected: Some(expected_revision),
                actual: None,
            }),
        }
    }

    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.list().await,
            None => Ok(Vec::new()),
        }
    }

    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.list_summaries().await,
            None => Ok(Vec::new()),
        }
    }
}

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

    /// 项目会话根目录；覆盖配置文件和 `$LUCIA_HOME/projects` 默认值。
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
    /// 插件请求主应用更新子视图导航栈。
    #[cfg(feature = "plugins")]
    ViewNavigation {
        plugin_id: String,
        request: UiNavigationRequest,
    },
    /// 最近一次模型请求消耗的上下文 token 数。
    ContextUsage(u64),
    /// Agent 运行结束，携带至少一次持久化后的会话状态。
    AgentDone(Box<AgentCompletion>),
    /// 后台会话摘要查询完成，等待注入 Command 插件界面。
    #[cfg(feature = "plugins")]
    CommandSurfaceUpdate {
        request_id: u64,
        status: SessionListStatus,
    },
    /// 后台取得新的命令注册表快照。
    #[cfg(feature = "plugins")]
    CommandSnapshotLoaded(Box<Result<Option<CommandSnapshot>>>),
    /// 显式参数补全请求完成。
    #[cfg(feature = "plugins")]
    CommandCompletionLoaded {
        generation: u64,
        result: Box<Result<Option<ResolvedCommandCompletion>>>,
    },
    /// Background plugin loading completed and can now be attached to the pending Agent.
    /// 后台插件加载结束，可挂载到等待中的 Agent。
    #[cfg(feature = "plugins")]
    PluginsLoaded(Box<Result<LoadedPlugins>>),
}

/// 输入框中等待随下一条消息发送的附件。
#[derive(Clone, Debug, PartialEq)]
struct PendingAttachment {
    /// 输入框中展示的引用标签，如 `[Image#1]`、`[FILE#report.pdf]`。
    label: String,
    /// 文件名（不含路径）。
    name: String,
    /// MIME 类型。
    media_type: String,
    /// base64 编码的文件内容。
    data: String,
    /// 是否图片附件。
    is_image: bool,
}

/// 一次用户提交：含附件引用标签的文本与对应附件。
#[derive(Clone, Debug, PartialEq)]
struct UserSubmission {
    /// 含附件引用标签的输入文本。
    text: String,
    /// 与文本中引用标签对应的附件，按插入顺序排列。
    attachments: Vec<PendingAttachment>,
}

impl UserSubmission {
    /// 转换为一次用户消息的内容块：文本在前，附件按加入顺序排在其后。
    fn blocks(&self) -> Vec<ContentBlock> {
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

/// 一次用户输入从先行持久化到模型运行结束的完整结果。
struct AgentCompletion {
    /// 模型已成功完成时携带运行结果。
    run: Option<AgentRun>,
    /// 下一轮必须使用的完整记录；最终保存失败时可能是尚未落盘的 dirty 完成态。
    session_record: SessionRecord,
    /// 保存或模型运行失败时的错误。
    error: Option<anyhow::Error>,
    /// 用户输入是否已经写入会话存储。
    input_committed: bool,
    /// 当前完成态是否允许自动执行下一条 FIFO 输入。
    queue_may_advance: bool,
    /// 用于保存失败时恢复编辑器内容的原始输入。
    input: UserSubmission,
}

/// 一次与输入快照绑定的参数候选结果。
#[cfg(feature = "plugins")]
struct ResolvedCommandCompletion {
    /// 发起请求时的完整编辑器内容。
    source_input: String,
    /// 发起请求时的 UTF-8 字节光标。
    source_cursor: usize,
    /// Provider 校验后的参数位置与替换区间。
    context: CompletionContext,
    /// 已经过 Provider、SDK 或宿主数据源限制的候选。
    items: Vec<CompletionItem>,
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
/// 原生 TUI 调用 Command Provider 时使用的稳定身份。
#[cfg(feature = "plugins")]
const TUI_COMMAND_CALLER: &str = "lucia-tui";
/// 官方 Context 插件的稳定 ID。
#[cfg(feature = "plugins")]
const CONTEXT_PLUGIN_ID: &str = "context";
/// 官方 Context 插件立即压缩当前 Session 的服务名。
#[cfg(feature = "plugins")]
const CONTEXT_COMPACT_SERVICE: &str = "context.compact";

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
/// UI 动画和后台维护使用的基础 tick 间隔。
const UI_TICK_INTERVAL_MS: u64 = 80;
/// 单个附件大小上限（10 MiB），兼顾常见模型接口的请求体限制。
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// 周期插件视图刷新间隔，约为一秒。
#[cfg(feature = "plugins")]
const PLUGIN_REFRESH_TICKS: u8 = 12;
/// Command 注册表兜底刷新间隔，约为十五秒。
#[cfg(feature = "plugins")]
const COMMAND_SNAPSHOT_REFRESH_TICKS: u8 = 188;

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

/// 从粘贴内容解析拖拽的单个文件路径。
///
/// 支持引号包裹与反斜杠转义（macOS 终端拖入文件的两种形态）以及 `~/` 前缀；
/// 仅接受指向普通文件的绝对路径，其余内容按普通文本粘贴处理。
fn pasted_file_path(pasted: &str) -> Option<PathBuf> {
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
fn unescape_shell_path(text: &str) -> String {
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
fn attachment_media_type(path: &Path, bytes: &[u8]) -> (String, bool) {
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
fn copy_to_clipboard(text: &str) -> Result<()> {
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
fn copy_via_command(text: &str) -> bool {
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

/// 将输入文本切分为普通文本与附件引用标签片段，标签使用高亮样式。
fn input_ref_spans<'a>(input: &'a str, attachments: &'a [PendingAttachment]) -> Vec<Span<'a>> {
    let base = Style::new().fg(COLOR_TEXT);
    let clip = Style::new().fg(COLOR_WARNING).bold();
    if attachments.is_empty() {
        return vec![Span::styled(input, base)];
    }
    let mut spans = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        // 取最靠前的附件标签作为下一个高亮片段。
        let earliest = attachments
            .iter()
            .filter_map(|a| rest.find(&a.label).map(|pos| (pos, a.label.len())))
            .min_by_key(|(pos, _)| *pos);
        let Some((start, len)) = earliest else {
            spans.push(Span::styled(rest, base));
            break;
        };
        if start > 0 {
            spans.push(Span::styled(&rest[..start], base));
        }
        spans.push(Span::styled(&rest[start..start + len], clip));
        rest = &rest[start + len..];
    }
    if spans.is_empty() {
        spans.push(Span::styled("", base));
    }
    spans
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
                let mut text = message.text_content();
                // TUI 提交的附件引用标签内嵌在文本中；其他来源的纯附件消息
                // 没有文本时补充占位标签，保证恢复后可见。
                if text.is_empty() {
                    let labels: Vec<String> = message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Image { .. } => Some("[Image]".to_string()),
                            ContentBlock::File { name, .. } => Some(format!("[FILE#{name}]")),
                            _ => None,
                        })
                        .collect();
                    text = labels.join(" ");
                }
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
    /// 本次进程固定使用的项目工作目录。
    workspace: WorkspaceContext,
    messages: Vec<Msg>,
    input: String,
    /// 等待随下一条消息发送的附件；引用标签内嵌在 `input` 文本中。
    attachments: Vec<PendingAttachment>,
    /// FIFO inputs accepted before the Agent becomes ready. Agent 就绪前接收的 FIFO 输入队列。
    queued_inputs: VecDeque<UserSubmission>,
    /// 当前运行是否来自 FIFO 队首；成功预写后才允许移除对应输入。
    queued_run_active: bool,
    /// 光标在 input 中的字节偏移。
    cursor: usize,
    running: bool,
    should_quit: bool,
    /// 下一轮使用的完整会话记录；最终保存失败时可暂存 dirty 完成态。
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
    /// 鼠标捕获是否开启；暂停后终端可用原生选择复制消息文本。
    mouse_capture: bool,
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
    /// 主视图之上的插件子视图导航栈。
    #[cfg(feature = "plugins")]
    view_stack: ViewStack,
    /// Command Provider 发布的只读命令快照。
    #[cfg(feature = "plugins")]
    command_snapshot: Option<CommandSnapshot>,
    /// 当前命令预览中的选中项。
    #[cfg(feature = "plugins")]
    command_selection: usize,
    /// 用户按 Esc 后临时隐藏当前输入对应的命令预览。
    #[cfg(feature = "plugins")]
    command_preview_hidden: bool,
    /// 最近一次会话摘要查询任务；新输入会中止尚未完成的旧查询。
    #[cfg(feature = "plugins")]
    command_query_task: Option<tokio::task::JoinHandle<()>>,
    /// 控制运行期命令快照低频刷新。
    #[cfg(feature = "plugins")]
    command_snapshot_tick: u8,
    /// 防止同一时间存在多个命令快照请求。
    #[cfg(feature = "plugins")]
    command_snapshot_refreshing: bool,
    /// 当前输入可展示并选择的参数候选。
    #[cfg(feature = "plugins")]
    command_completion: Option<ResolvedCommandCompletion>,
    /// 正在运行的显式参数候选请求；输入变化时会中止。
    #[cfg(feature = "plugins")]
    command_completion_task: Option<tokio::task::JoinHandle<()>>,
    /// 参数候选请求是否尚未返回。
    #[cfg(feature = "plugins")]
    command_completion_loading: bool,
    /// 用于丢弃取消后仍到达的过期参数候选。
    #[cfg(feature = "plugins")]
    command_completion_generation: u64,
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
            workspace: WorkspaceContext::capture().expect("当前工作目录应可用"),
            messages: Vec::new(),
            input: String::new(),
            attachments: Vec::new(),
            queued_inputs: VecDeque::new(),
            queued_run_active: false,
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
            mouse_capture: true,
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
            #[cfg(feature = "plugins")]
            view_stack: ViewStack::default(),
            #[cfg(feature = "plugins")]
            command_snapshot: None,
            #[cfg(feature = "plugins")]
            command_selection: 0,
            #[cfg(feature = "plugins")]
            command_preview_hidden: false,
            #[cfg(feature = "plugins")]
            command_query_task: None,
            #[cfg(feature = "plugins")]
            command_snapshot_tick: 0,
            #[cfg(feature = "plugins")]
            command_snapshot_refreshing: false,
            #[cfg(feature = "plugins")]
            command_completion: None,
            #[cfg(feature = "plugins")]
            command_completion_task: None,
            #[cfg(feature = "plugins")]
            command_completion_loading: false,
            #[cfg(feature = "plugins")]
            command_completion_generation: 0,
        }
    }

    /// 注入主函数启动时捕获的项目上下文。
    fn with_workspace(mut self, workspace: WorkspaceContext) -> Self {
        self.workspace = workspace;
        self
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

    /// 用完整记录替换当前会话，并清理只属于旧会话的瞬时界面状态。
    #[cfg(feature = "plugins")]
    fn replace_session(&mut self, session_record: SessionRecord, notice: Option<&str>) {
        self.messages = restore_session_messages(&session_record.session);
        self.session_record = session_record;
        self.input.clear();
        self.attachments.clear();
        self.cursor = 0;
        self.queued_inputs.clear();
        self.streaming_message = None;
        self.scroll = None;
        self.context_tokens = None;
        #[cfg(feature = "plugins")]
        self.clear_command_completion();
        if let Some(notice) = notice {
            self.messages.push(Msg::new(MsgKind::Info, notice));
        }
    }

    /// 进入当前项目下尚未持久化的全新空白草稿。
    #[cfg(feature = "plugins")]
    fn start_new_draft(&mut self, notice: &str) -> Result<()> {
        let draft = self.workspace.draft_record()?;
        self.replace_session(draft, Some(notice));
        Ok(())
    }

    /// Replaces plugin view state after background activation completes.
    ///
    /// 后台激活完成后替换插件视图状态。
    #[cfg(feature = "plugins")]
    fn set_plugin_views(&mut self, declarations: Vec<UiDeclaration>) {
        self.view_stack = ViewStack::default();
        self.plugin_views = declarations
            .into_iter()
            .map(|declaration| PluginViewState {
                declaration,
                frame: None,
                area: Rect::default(),
            })
            .collect();
    }

    /// 在应用导航栈上执行一次经 Host 标记来源的插件视图请求。
    #[cfg(feature = "plugins")]
    fn apply_view_navigation(
        &mut self,
        plugin_id: &str,
        request: UiNavigationRequest,
    ) -> Result<bool> {
        let declarations = self
            .plugin_views
            .iter()
            .map(|view| view.declaration.clone())
            .collect::<Vec<_>>();
        let changed = self.view_stack.apply(plugin_id, request, &declarations)?;
        if changed {
            self.plugin_focus = None;
        }
        Ok(changed)
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

    /// 替换 Command Provider 快照，并重置本地预览状态。
    #[cfg(feature = "plugins")]
    fn set_command_snapshot(&mut self, snapshot: Option<CommandSnapshot>) {
        self.clear_command_completion();
        self.command_snapshot = snapshot;
        self.command_selection = 0;
        self.command_preview_hidden = false;
    }

    /// 返回与当前斜杠输入匹配的命令定义。
    #[cfg(feature = "plugins")]
    fn command_matches(&self) -> Vec<CommandSpec> {
        if self.command_preview_hidden {
            return Vec::new();
        }
        let Some(body) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        let prefix = body.split_whitespace().next().unwrap_or_default();
        let Some(snapshot) = &self.command_snapshot else {
            return Vec::new();
        };
        snapshot
            .commands
            .iter()
            .filter(|command| {
                command.name.starts_with(prefix)
                    || command
                        .aliases
                        .iter()
                        .any(|alias| alias.starts_with(prefix))
            })
            .take(6)
            .cloned()
            .collect()
    }

    /// 根据用户输入解析当前命令定义，仅用于宿主状态校验和展示。
    #[cfg(feature = "plugins")]
    fn command_spec_for_input(&self, input: &str) -> Option<&CommandSpec> {
        let name = input
            .trim()
            .strip_prefix('/')?
            .split_whitespace()
            .next()?
            .to_ascii_lowercase();
        self.command_snapshot
            .as_ref()?
            .commands
            .iter()
            .find(|command| {
                command.name == name || command.aliases.iter().any(|alias| alias == &name)
            })
    }

    /// 启动可取消的后台会话摘要查询，并通过 UI 事件返回类型化结果。
    #[cfg(feature = "plugins")]
    fn start_command_query(
        &mut self,
        request_id: u64,
        query: String,
        cursor: Option<String>,
        limit: u16,
    ) {
        if let Some(task) = self.command_query_task.take() {
            task.abort();
        }
        let session_store = Arc::clone(&self.session_store);
        let active_session_id = self.session_record.id.clone();
        let tx = self.tx.clone();
        self.command_query_task = Some(tokio::spawn(async move {
            // 搜索输入短暂防抖，连续键入只扫描一次项目会话目录。
            if cursor.is_none() && !query.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            }
            let status = command_session_page(
                session_store.as_ref(),
                &active_session_id,
                &query,
                cursor.as_deref(),
                limit,
            )
            .await;
            let _ = tx.send(UiEvent::CommandSurfaceUpdate { request_id, status });
        }));
    }

    /// 在后台低频刷新命令注册表，不让运行期注册或注销阻塞输入线程。
    #[cfg(feature = "plugins")]
    fn schedule_command_snapshot_refresh(&mut self, plugin_host: Arc<CompositePluginHost>) {
        if self.command_snapshot_refreshing
            || !self
                .plugin_ids
                .iter()
                .any(|plugin_id| plugin_id == PROVIDER_PLUGIN_ID)
        {
            return;
        }
        self.command_snapshot_refreshing = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_command_snapshot(plugin_host.as_ref()).await;
            let _ = tx.send(UiEvent::CommandSnapshotLoaded(Box::new(result)));
        });
    }

    /// 清理参数候选并使尚未返回的旧请求失效。
    #[cfg(feature = "plugins")]
    fn clear_command_completion(&mut self) {
        if let Some(task) = self.command_completion_task.take() {
            task.abort();
        }
        self.command_completion = None;
        self.command_completion_loading = false;
        self.command_completion_generation = self.command_completion_generation.wrapping_add(1);
    }

    /// 在后台执行一次显式参数补全请求。
    #[cfg(feature = "plugins")]
    fn schedule_command_completion(&mut self, plugin_host: Arc<CompositePluginHost>) {
        self.clear_command_completion();
        self.command_completion_loading = true;
        let generation = self.command_completion_generation;
        let source_input = self.input.clone();
        let source_cursor = self.cursor;
        let session_store = Arc::clone(&self.session_store);
        let tx = self.tx.clone();
        self.command_completion_task = Some(tokio::spawn(async move {
            let result = resolve_command_completion(
                plugin_host.as_ref(),
                session_store.as_ref(),
                source_input,
                source_cursor,
            )
            .await;
            let _ = tx.send(UiEvent::CommandCompletionLoaded {
                generation,
                result: Box::new(result),
            });
        }));
    }

    /// 将当前选中的参数候选写入 Provider 指定的 UTF-8 替换区间。
    #[cfg(feature = "plugins")]
    fn apply_selected_command_completion(&mut self) -> bool {
        let Some(completion) = self.command_completion.as_ref() else {
            return false;
        };
        if completion.source_input != self.input || completion.source_cursor != self.cursor {
            self.clear_command_completion();
            return false;
        }
        let start = usize::try_from(completion.context.replacement_start).ok();
        let end = usize::try_from(completion.context.replacement_end).ok();
        let selected = self
            .command_selection
            .min(completion.items.len().saturating_sub(1));
        let Some((start, end, item)) = start.zip(end).and_then(|(start, end)| {
            completion
                .items
                .get(selected)
                .map(|item| (start, end, item))
        }) else {
            self.clear_command_completion();
            return false;
        };
        if start > end
            || end > self.input.len()
            || !self.input.is_char_boundary(start)
            || !self.input.is_char_boundary(end)
        {
            self.clear_command_completion();
            return false;
        }
        let insert_text = item.insert_text.clone();
        self.input.replace_range(start..end, &insert_text);
        self.cursor = start + insert_text.len();
        self.clear_command_completion();
        self.command_selection = 0;
        self.command_preview_hidden = false;
        true
    }

    /// 在命令预览打开时处理选择、补全与关闭操作。
    #[cfg(feature = "plugins")]
    fn handle_command_preview_key(&mut self, code: KeyCode) -> bool {
        let completion_len = self
            .command_completion
            .as_ref()
            .map(|completion| completion.items.len())
            .unwrap_or(0);
        if completion_len > 0 {
            return match code {
                KeyCode::Up => {
                    self.command_selection = self.command_selection.saturating_sub(1);
                    true
                }
                KeyCode::Down => {
                    self.command_selection =
                        (self.command_selection + 1).min(completion_len.saturating_sub(1));
                    true
                }
                KeyCode::Tab => self.apply_selected_command_completion(),
                KeyCode::Esc => {
                    self.clear_command_completion();
                    self.command_preview_hidden = true;
                    true
                }
                _ => false,
            };
        }

        let matches = self.command_matches();
        if matches.is_empty() {
            return false;
        }
        match code {
            KeyCode::Up => {
                self.command_selection = self.command_selection.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.command_selection =
                    (self.command_selection + 1).min(matches.len().saturating_sub(1));
                true
            }
            KeyCode::Tab => {
                let body = self.input.strip_prefix('/').unwrap_or_default();
                if body.chars().any(char::is_whitespace) {
                    return false;
                }
                let selected = self.command_selection.min(matches.len().saturating_sub(1));
                self.input = format!("/{} ", matches[selected].name);
                self.cursor = self.input.len();
                self.command_selection = 0;
                true
            }
            KeyCode::Esc => {
                self.command_preview_hidden = true;
                true
            }
            _ => false,
        }
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers, agent: Option<&Arc<Agent>>) {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('y')) {
            self.copy_last_assistant_message();
            return;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('t')) {
            self.toggle_mouse_capture();
            return;
        }

        #[cfg(feature = "plugins")]
        if self.handle_command_preview_key(code) {
            return;
        }

        match code {
            KeyCode::Enter => {
                if let Some(agent) = agent {
                    if self.running {
                        // steering 只支持纯文本，附件必须等当前回合结束后随消息发送。
                        if self.attachments.is_empty() {
                            self.submit_steering(agent);
                        } else {
                            self.messages.push(Msg::new(
                                MsgKind::Info,
                                "运行中无法发送附件，请等待本轮完成后再提交",
                            ));
                        }
                    } else {
                        self.submit(agent);
                    }
                } else {
                    self.queue_input_until_ready();
                }
            }
            KeyCode::Esc => {
                // 运行中 Esc 请求优雅取消当前回合；空闲时保持原退出行为。
                if self.running {
                    if let Some(agent) = agent {
                        agent.cancel();
                        self.messages
                            .push(Msg::new(MsgKind::Info, "正在取消当前运行..."));
                    }
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::PageUp => self.scroll_up(5),
            KeyCode::PageDown => self.scroll_down(5),
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1),
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                // 插入可能拆散附件引用标签，同步丢弃失去标签的附件。
                self.prune_attachments();
                #[cfg(feature = "plugins")]
                {
                    self.clear_command_completion();
                    self.command_selection = 0;
                    self.command_preview_hidden = false;
                }
            }
            KeyCode::Backspace => {
                // 光标紧跟附件引用标签时整体删除标签与附件。
                if !self.remove_attachment_before_cursor() {
                    if let Some(prev) = self.input[..self.cursor].chars().last() {
                        self.cursor -= prev.len_utf8();
                        self.input.remove(self.cursor);
                    }
                    self.prune_attachments();
                }
                #[cfg(feature = "plugins")]
                {
                    self.clear_command_completion();
                    self.command_selection = 0;
                    self.command_preview_hidden = false;
                }
            }
            KeyCode::Left => {
                if let Some(prev) = self.input[..self.cursor].chars().last() {
                    self.cursor -= prev.len_utf8();
                }
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
            KeyCode::Right => {
                if let Some(next) = self.input[self.cursor..].chars().next() {
                    self.cursor += next.len_utf8();
                }
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
            KeyCode::Home => {
                self.cursor = 0;
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
            _ => {}
        }
    }

    /// 根据模态层和 Tab 焦点决定键盘事件的接收者。
    #[cfg(feature = "plugins")]
    fn route_plugin_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> PluginKeyRoute {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return PluginKeyRoute::Main;
        }

        if self.view_stack.active().is_some() {
            if matches!(code, KeyCode::Esc) {
                self.view_stack.pop_for_user();
                return PluginKeyRoute::Consumed;
            }
            let active = self.view_stack.active().expect("子视图已确认存在");
            return PluginKeyRoute::Input(UiInput {
                plugin_id: active.plugin_id.clone(),
                view_id: active.view.view_id.clone(),
                instance_id: Some(active.view.instance_id.clone()),
                event: UiInputEvent::Key {
                    code: plugin_key_code(code),
                    modifiers: plugin_key_modifiers(modifiers),
                },
            });
        }

        if let Some(index) = self.active_dialog_index() {
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        if matches!(code, KeyCode::Tab) && self.input.trim_start().starts_with('/') {
            return PluginKeyRoute::Main;
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
        if let Some(active) = self.view_stack.active() {
            if !point_in_rect(mouse.column, mouse.row, active.area) {
                return None;
            }
            return Some(UiInput {
                plugin_id: active.plugin_id.clone(),
                view_id: active.view.view_id.clone(),
                instance_id: Some(active.view.instance_id.clone()),
                event: UiInputEvent::Mouse {
                    kind: plugin_mouse_kind(mouse.kind),
                    x: mouse.column.saturating_sub(active.area.x),
                    y: mouse.row.saturating_sub(active.area.y),
                },
            });
        }
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
            instance_id: None,
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
            instance_id: None,
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
                    && view.declaration.placement != UiPlacement::Input
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
        let mut requests = self
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| view.declaration.placement != UiPlacement::Subview)
            .map(|(index, view)| UiRenderRequest {
                plugin_id: view.declaration.plugin_id.clone(),
                view_id: view.declaration.view_id.clone(),
                instance_id: None,
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
            .collect::<Vec<_>>();
        if let Some(active) = self
            .view_stack
            .active()
            .filter(|active| !active.area.is_empty())
        {
            requests.push(UiRenderRequest {
                plugin_id: active.plugin_id.clone(),
                view_id: active.view.view_id.clone(),
                instance_id: Some(active.view.instance_id.clone()),
                width: active.area.width.max(1),
                height: active.area.height.max(1),
                focused: true,
                frame: self.plugin_frame,
            });
        }
        requests
    }

    /// 构造周期刷新请求，并跳过由用户操作定向刷新的隐藏 Command Dialog。
    #[cfg(feature = "plugins")]
    fn periodic_plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
        let command_dialog_hidden = self.plugin_views.iter().any(|view| {
            view.declaration.plugin_id == PROVIDER_PLUGIN_ID
                && view.declaration.view_id == SESSION_DIALOG_VIEW
                && view.frame.as_ref().is_some_and(|frame| !frame.visible)
        });
        let mut requests = self.plugin_render_requests();
        if command_dialog_hidden {
            requests.retain(|request| {
                request.plugin_id != PROVIDER_PLUGIN_ID || request.view_id != SESSION_DIALOG_VIEW
            });
        }
        requests
    }

    /// 用插件返回的新帧更新对应视图缓存。
    #[cfg(feature = "plugins")]
    fn update_plugin_frame(
        &mut self,
        plugin_id: &str,
        instance_id: Option<&str>,
        frame: PluginUiFrame,
    ) {
        if let Some(instance_id) = instance_id {
            if let Some(active) = self.view_stack.active_mut() {
                if active.plugin_id == plugin_id
                    && active.view.view_id == frame.view_id
                    && active.view.instance_id == instance_id
                {
                    active.frame = Some(frame);
                }
            }
            return;
        }
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
    fn set_plugin_ui_error(
        &mut self,
        plugin_id: &str,
        view_id: &str,
        instance_id: Option<&str>,
        error: &anyhow::Error,
    ) {
        self.update_plugin_frame(
            plugin_id,
            instance_id,
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

    /// 处理终端粘贴：单个存在的文件路径转为附件，其余内容压平为单行插入光标处。
    fn handle_paste(&mut self, pasted: &str) {
        if let Some(path) = pasted_file_path(pasted) {
            self.attach_file(&path);
            return;
        }
        // 输入框为单行编辑器，换行和制表符统一替换为空格。
        let text: String = pasted
            .chars()
            .map(|c| {
                if matches!(c, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        if text.is_empty() {
            return;
        }
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
        #[cfg(feature = "plugins")]
        {
            self.clear_command_completion();
            self.command_selection = 0;
            self.command_preview_hidden = false;
        }
    }

    /// 读取文件并加入待发送附件，同时在光标处插入引用标签。
    ///
    /// 读取失败或超过 [`MAX_ATTACHMENT_BYTES`] 时追加错误消息，不修改输入。
    fn attach_file(&mut self, path: &Path) {
        use base64::Engine;

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("读取附件失败（{}）：{error}", path.display()),
                ));
                return;
            }
        };
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!(
                    "附件超过 {} MiB 上限：{}",
                    MAX_ATTACHMENT_BYTES / 1024 / 1024,
                    path.display()
                ),
            ));
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        let (media_type, is_image) = attachment_media_type(path, &bytes);
        let label = self.next_attachment_label(&name, is_image);
        self.attachments.push(PendingAttachment {
            label: label.clone(),
            name,
            media_type,
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            is_image,
        });
        let clip = format!("{label} ");
        self.input.insert_str(self.cursor, &clip);
        self.cursor += clip.len();
    }

    /// 生成不与现有附件冲突的引用标签：图片使用递增序号，文件使用文件名。
    fn next_attachment_label(&self, name: &str, is_image: bool) -> String {
        let taken = |label: &str| self.attachments.iter().any(|a| a.label == label);
        if is_image {
            let mut index = self.attachments.iter().filter(|a| a.is_image).count() + 1;
            loop {
                let label = format!("[Image#{index}]");
                if !taken(&label) {
                    return label;
                }
                index += 1;
            }
        }
        let base = format!("[FILE#{name}]");
        if !taken(&base) {
            return base;
        }
        let mut index = 2;
        loop {
            let label = format!("[FILE#{name}#{index}]");
            if !taken(&label) {
                return label;
            }
            index += 1;
        }
    }

    /// 丢弃引用标签已不在输入文本中的附件，保持标签与附件一一对应。
    fn prune_attachments(&mut self) {
        if self.attachments.is_empty() {
            return;
        }
        let input = &self.input;
        self.attachments.retain(|a| input.contains(&a.label));
    }

    /// 光标紧跟附件引用标签时整体删除标签及对应附件，返回是否命中。
    fn remove_attachment_before_cursor(&mut self) -> bool {
        let prefix = &self.input[..self.cursor];
        let Some(index) = self
            .attachments
            .iter()
            .position(|a| prefix.ends_with(&a.label))
        else {
            return false;
        };
        let start = self.cursor - self.attachments[index].label.len();
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.attachments.remove(index);
        true
    }

    /// 复制最近一条助手回复原文（Markdown）到系统剪贴板。
    fn copy_last_assistant_message(&mut self) {
        let Some(text) = self
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.kind, MsgKind::Assistant) && !m.text.is_empty())
            .map(|m| m.text.clone())
        else {
            self.messages
                .push(Msg::new(MsgKind::Info, "没有可复制的助手回复"));
            return;
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self
                .messages
                .push(Msg::new(MsgKind::Info, "已复制最近一条回复到剪贴板")),
            Err(error) => self
                .messages
                .push(Msg::new(MsgKind::Error, format!("复制失败：{error}"))),
        }
    }

    /// 切换鼠标捕获：暂停后终端可用原生选择复制任意消息文本，恢复后支持滚轮滚动。
    fn toggle_mouse_capture(&mut self) {
        let result = if self.mouse_capture {
            crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
        } else {
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
        };
        if result.is_err() {
            return;
        }
        self.mouse_capture = !self.mouse_capture;
        let notice = if self.mouse_capture {
            "已恢复鼠标捕获与滚轮滚动"
        } else {
            "已暂停鼠标捕获：现在可用鼠标选择并复制文本，Ctrl+T 恢复"
        };
        self.messages.push(Msg::new(MsgKind::Info, notice));
    }

    /// Takes and clears the current editor value, returning `None` for blank input.
    ///
    /// 取出并清空当前编辑器内容；空白输入返回 `None`。斜杠命令等纯文本路径
    /// 不携带附件，残留附件一并清除，避免引用标签消失后附件悬挂。
    fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.attachments.clear();
        self.cursor = 0;
        #[cfg(feature = "plugins")]
        self.clear_command_completion();
        (!input.is_empty()).then_some(input)
    }

    /// 取出并清空当前输入与附件；文本和附件都为空时返回 `None`。
    fn take_submission(&mut self) -> Option<UserSubmission> {
        let text = self.input.trim().to_string();
        let attachments = std::mem::take(&mut self.attachments);
        self.input.clear();
        self.cursor = 0;
        #[cfg(feature = "plugins")]
        self.clear_command_completion();
        if text.is_empty() && attachments.is_empty() {
            return None;
        }
        Some(UserSubmission { text, attachments })
    }

    /// Queues one complete input while plugin loading keeps the Agent unavailable.
    ///
    /// 插件加载导致 Agent 尚不可用时，将一条完整输入加入 FIFO 队列。
    fn queue_input_until_ready(&mut self) {
        // 斜杠命令必须等待官方插件完成加载，不能误入 Agent 消息队列。
        if self.input.trim_start().starts_with('/') {
            return;
        }
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.messages
            .push(Msg::new(MsgKind::User, submission.text.clone()));
        self.queued_inputs.push_back(submission);
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
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.queued_run_active = false;
        self.start_input_run(agent, submission, true);
    }

    /// Starts one Agent run and optionally appends the user message to the visible history.
    ///
    /// 启动一次 Agent 运行，并按需把用户消息追加到可见历史。
    fn start_input_run(
        &mut self,
        agent: &Arc<Agent>,
        submission: UserSubmission,
        show_user_message: bool,
    ) {
        if show_user_message {
            self.messages
                .push(Msg::new(MsgKind::User, submission.text.clone()));
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
                submission,
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
        if let Some(submission) = self.queued_inputs.front().cloned() {
            self.queued_run_active = true;
            self.start_input_run(agent, submission, false);
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

    /// 完成 Agent 运行，以完整 Session 校准界面，并返回是否可自动推进 FIFO。
    fn handle_agent_done(&mut self, completion: AgentCompletion) -> bool {
        let AgentCompletion {
            run,
            session_record,
            error,
            input_committed,
            queue_may_advance,
            input,
        } = completion;
        self.running = false;
        let queued_run = std::mem::take(&mut self.queued_run_active);
        if queued_run && input_committed {
            let committed_input = self
                .queued_inputs
                .pop_front()
                .expect("队列运行必须对应一个 FIFO 输入");
            debug_assert_eq!(committed_input, input);
        }

        if input_committed {
            self.session_record = session_record;
            self.messages = restore_session_messages(&self.session_record.session);
            self.messages.extend(
                self.queued_inputs
                    .iter()
                    .map(|pending| Msg::new(MsgKind::User, pending.text.clone())),
            );
        } else {
            // 队列首存失败时保留原队列和已展示顺序，等待后续重试同一队首。
            if !queued_run && self.input.is_empty() && self.attachments.is_empty() {
                self.input = input.text.clone();
                self.cursor = self.input.len();
                self.attachments = input.attachments.clone();
            }
            if !queued_run
                && self.messages.last().is_some_and(|message| {
                    matches!(message.kind, MsgKind::User) && message.text == input.text
                })
            {
                self.messages.pop();
            }
        }

        self.streaming_message = None;
        if let Some(run) = run {
            if run.cancelled {
                self.messages
                    .push(Msg::new(MsgKind::Info, "本轮运行已取消，已生成内容保留"));
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
        }
        if let Some(error) = error {
            self.messages
                .push(Msg::new(MsgKind::Error, error.to_string()));
        }
        queue_may_advance
    }
}

/// 判断回读记录是否包含一次 save 尝试的同一业务 payload。
///
/// 存储成功时会自行更新 revision 与更新时间，因此这两个规范化字段不参与比较。
fn session_record_payload_matches(stored: &SessionRecord, attempted: &SessionRecord) -> bool {
    stored.schema_version == attempted.schema_version
        && stored.id == attempted.id
        && stored.created_at_ms == attempted.created_at_ms
        && stored.title == attempted.title
        && stored.metadata == attempted.metadata
        && stored.session == attempted.session
}

/// 保存记录，并通过同 ID 回读协调“已写入但返回错误”的不确定提交。
async fn save_session_record_reconciled(
    session_store: &dyn SessionStore,
    record: SessionRecord,
    expected_revision: Option<u64>,
) -> Result<SessionRecord, SessionStoreError> {
    match session_store.save(record.clone(), expected_revision).await {
        Ok(saved) => Ok(saved),
        Err(error) => match session_store.load(&record.id).await {
            Ok(Some(stored)) if session_record_payload_matches(&stored, &record) => Ok(stored),
            _ => Err(error),
        },
    }
}

/// 先保存用户输入，再运行 Agent 并保存完整回复。
///
/// save 返回错误时先回读并协调可能已经落盘的不确定提交。确认未落盘后，最终保存失败
/// 才会分叉完整 Session；分叉也失败则返回 dirty 完成态，避免从旧记录继续。
async fn run_and_persist(
    agent: &Agent,
    session_store: &dyn SessionStore,
    mut session_record: SessionRecord,
    input: impl Into<UserSubmission>,
) -> AgentCompletion {
    let submission: UserSubmission = input.into();
    let expected_revision = (session_record.revision > 0).then_some(session_record.revision);
    session_record.session =
        agent.prepare_session_blocks(session_record.session.clone(), submission.blocks());
    if session_record.title.is_none() {
        // 纯附件输入没有可用文本时，用首个附件引用标签作为会话标题。
        session_record.title = session_title(&submission.text)
            .or_else(|| submission.attachments.first().map(|a| a.label.clone()));
    }
    let committed_record = match save_session_record_reconciled(
        session_store,
        session_record.clone(),
        expected_revision,
    )
    .await
    {
        Ok(record) => record,
        Err(error) => {
            return AgentCompletion {
                run: None,
                session_record,
                error: Some(error.into()),
                input_committed: false,
                queue_may_advance: false,
                input: submission,
            };
        }
    };

    let run = match agent.run_session(committed_record.session.clone()).await {
        Ok(run) => run,
        Err(error) => {
            return AgentCompletion {
                run: None,
                session_record: committed_record,
                error: Some(error),
                input_committed: true,
                queue_may_advance: true,
                input: submission,
            };
        }
    };

    let mut completed_record = committed_record.clone();
    completed_record.session = run.session.clone();
    match save_session_record_reconciled(
        session_store,
        completed_record.clone(),
        Some(committed_record.revision),
    )
    .await
    {
        Ok(saved_record) => AgentCompletion {
            run: Some(run),
            session_record: saved_record,
            error: None,
            input_committed: true,
            queue_may_advance: true,
            input: submission,
        },
        Err(save_error) => {
            let fork_result =
                match SessionRecord::new(SessionId::generate(), completed_record.session.clone()) {
                    Ok(mut fork_record) => {
                        fork_record.title = completed_record.title.clone();
                        fork_record.metadata = completed_record.metadata.clone();
                        save_session_record_reconciled(session_store, fork_record, None).await
                    }
                    Err(error) => Err(error),
                };
            match fork_result {
                Ok(fork_record) => AgentCompletion {
                    run: Some(run),
                    error: Some(anyhow!(
                        "原会话的模型回复保存失败（{save_error}），完整回复已分叉保存为会话 {}",
                        fork_record.id
                    )),
                    session_record: fork_record,
                    input_committed: true,
                    queue_may_advance: true,
                    input: submission,
                },
                Err(fork_error) => AgentCompletion {
                    run: Some(run),
                    session_record: completed_record,
                    error: Some(anyhow!(
                        "模型回复未能保存：{save_error}；分叉保存也失败：{fork_error}。完整回复已保留在当前内存会话中"
                    )),
                    input_committed: true,
                    queue_may_advance: false,
                    input: submission,
                },
            }
        }
    }
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
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
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
        UiPlacement::Dialog | UiPlacement::Subview => 60,
        UiPlacement::Input => 40,
        UiPlacement::Top | UiPlacement::Bottom => 40,
    }
}

/// 返回未实际布局前使用的插件视图默认高度。
#[cfg(feature = "plugins")]
fn default_plugin_height(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Top | UiPlacement::Bottom => 6,
        UiPlacement::Dialog | UiPlacement::Subview => 20,
        UiPlacement::Input => 3,
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
                #[cfg(feature = "plugins")]
                if event.payload.get("name").and_then(Value::as_str) == Some(UI_NAVIGATION_EVENT) {
                    let plugin_id = event
                        .payload
                        .pointer("/source/id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("视图导航事件缺少可信插件来源"))?;
                    let request = serde_json::from_value::<UiNavigationRequest>(
                        event.payload.get("data").cloned().unwrap_or(Value::Null),
                    )?;
                    let _ = self.0.send(UiEvent::ViewNavigation {
                        plugin_id: plugin_id.to_string(),
                        request,
                    });
                    return Ok(());
                }
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

/// 在插件占用后的中心区域渲染 Lucia 主界面。
fn render_main(frame: &mut Frame, app: &mut App, workspace: Rect) {
    // Keep the input and footer compact so the released status row returns to the chat viewport.
    // 输入区与底栏保持紧凑，将释放的插件状态行归还给对话视口。
    #[cfg(feature = "plugins")]
    let command_matches = if workspace.height >= 10 {
        app.command_matches()
    } else {
        Vec::new()
    };
    #[cfg(feature = "plugins")]
    let command_preview_items = app
        .command_completion
        .as_ref()
        .map(|completion| completion.items.len())
        .unwrap_or(command_matches.len())
        .min(6);
    #[cfg(feature = "plugins")]
    let command_preview_height = if app.command_preview_hidden || command_preview_items == 0 {
        0
    } else {
        u16::try_from(command_preview_items + 2).unwrap_or(8)
    };
    #[cfg(feature = "plugins")]
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(command_preview_height),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(workspace);
    #[cfg(not(feature = "plugins"))]
    let sections = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(workspace);
    #[cfg(feature = "plugins")]
    let (chat_section, command_section, input_section, footer_section) =
        (sections[0], sections[1], sections[2], sections[3]);
    #[cfg(not(feature = "plugins"))]
    let (chat_section, input_section, footer_section) = (sections[0], sections[1], sections[2]);

    // 消息流不使用容器边框，宽屏下由居中工作区控制阅读长度。
    let chat_area = chat_section.inner(Margin {
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

    #[cfg(feature = "plugins")]
    if command_preview_height > 0 {
        render_command_preview(frame, app, command_section, &command_matches);
    }

    // 输入区在顶部规则线后保留一行空白，使边线与编辑文本保持稳定间距。
    let input_area = input_section;
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
        .padding(Padding::new(1, 1, 1, 0));
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
        // 附件引用标签（如 [Image#1]）以高亮 clip 样式区别于普通文本。
        let mut spans = vec![Span::styled("› ", Style::new().fg(input_color).bold())];
        spans.extend(input_ref_spans(&app.input, &app.attachments));
        let input_widget = Paragraph::new(Line::from(spans)).block(input_block);
        frame.render_widget(input_widget, input_area);
        // 使用显示宽度定位光标，确保中文等全角字符不会造成偏移。
        let cursor_width = unicode_width::UnicodeWidthStr::width(&app.input[..app.cursor]) as u16;
        if main_input_focused {
            frame.set_cursor_position((input_inner.x + 2 + cursor_width, input_inner.y));
        }
    }

    // 底部信息行：模型、工作目录与当前上下文 token 数，窄终端时隐藏目录。
    let cwd = app.workspace.cwd.display().to_string();
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
    let footer_area = footer_section;
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

/// 渲染内存命令快照中的匹配项和当前选中命令说明。
#[cfg(feature = "plugins")]
fn render_command_preview(frame: &mut Frame, app: &App, area: Rect, matches: &[CommandSpec]) {
    if area.is_empty() {
        return;
    }
    if let Some(completion) = app.command_completion.as_ref() {
        let visible = completion.items.iter().take(6).collect::<Vec<_>>();
        if visible.is_empty() {
            return;
        }
        let selected = app.command_selection.min(visible.len().saturating_sub(1));
        let mut lines = visible
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let selected = index == selected;
                let marker = if selected { "› " } else { "  " };
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::new().fg(if selected { COLOR_USER } else { COLOR_MUTED }),
                    ),
                    Span::styled(
                        item.label.as_str(),
                        Style::new()
                            .fg(if selected { COLOR_TEXT } else { COLOR_MUTED })
                            .bold(),
                    ),
                    Span::styled("  ", Style::new()),
                    Span::styled(
                        item.description.as_deref().unwrap_or_default(),
                        Style::new().fg(COLOR_MUTED),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        let detail = app
            .command_snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .commands
                    .iter()
                    .find(|command| command.name == completion.context.command)
            })
            .and_then(|command| {
                command
                    .arguments
                    .get(usize::from(completion.context.argument_index))
            })
            .map(|argument| argument.description.as_str())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "/{} · {}",
                    completion.context.command, completion.context.argument
                ),
                Style::new().fg(COLOR_BORDER_FOCUS),
            ),
            Span::styled("  ", Style::new()),
            Span::styled(
                truncate_line(detail, usize::from(area.width.saturating_sub(24).max(1))),
                Style::new().fg(COLOR_MUTED),
            ),
        ]));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                    .padding(Padding::horizontal(1)),
            ),
            area,
        );
        return;
    }
    if matches.is_empty() {
        return;
    }
    let visible = matches.iter().take(6).collect::<Vec<_>>();
    let selected = app.command_selection.min(visible.len().saturating_sub(1));
    let mut lines = visible
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let selected = index == selected;
            let marker = if selected { "› " } else { "  " };
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::new().fg(if selected { COLOR_USER } else { COLOR_MUTED }),
                ),
                Span::styled(
                    command.display_usage(),
                    Style::new()
                        .fg(if selected { COLOR_TEXT } else { COLOR_MUTED })
                        .bold(),
                ),
                Span::styled("  ", Style::new()),
                Span::styled(command.summary.as_str(), Style::new().fg(COLOR_MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    let selected_command = visible[selected];
    let availability = if app.command_completion_loading {
        "  候选加载中..."
    } else if app.running && selected_command.availability == CommandAvailability::IdleOnly {
        "  Agent 运行结束后可用"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled(
            truncate_line(
                &selected_command.description,
                usize::from(area.width.saturating_sub(24).max(1)),
            ),
            Style::new().fg(COLOR_MUTED),
        ),
        Span::styled(availability, Style::new().fg(COLOR_BORDER_FOCUS)),
    ]));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// 调用插件的类型化服务，并统一限制原生 TUI 的等待时间。
#[cfg(feature = "plugins")]
async fn call_typed_plugin_service<Request, Response>(
    plugin_host: &dyn PluginHost,
    caller_id: &str,
    plugin_id: &str,
    service: &str,
    request: &Request,
) -> Result<Option<Response>>
where
    Request: Serialize,
    Response: DeserializeOwned,
{
    let payload = serde_json::to_value(request)
        .with_context(|| format!("序列化插件服务 `{service}` 请求失败"))?;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        plugin_host.call_service(&PluginServiceCall {
            caller_id: caller_id.to_string(),
            plugin_id: plugin_id.to_string(),
            name: service.to_string(),
            payload,
        }),
    )
    .await
    .map_err(|_| anyhow!("插件服务 `{service}` 调用超时"))??;
    response
        .map(|value| {
            serde_json::from_value(value)
                .with_context(|| format!("解析插件服务 `{service}` 响应失败"))
        })
        .transpose()
}

/// 从官方 Command Provider 读取可在输入热路径中复用的命令快照。
#[cfg(feature = "plugins")]
async fn load_command_snapshot(plugin_host: &dyn PluginHost) -> Result<Option<CommandSnapshot>> {
    call_typed_plugin_service(
        plugin_host,
        TUI_COMMAND_CALLER,
        PROVIDER_PLUGIN_ID,
        SNAPSHOT_SERVICE,
        &SnapshotRequest::default(),
    )
    .await
}

/// 按 Provider 计划解析一次显式参数候选请求。
#[cfg(feature = "plugins")]
async fn resolve_command_completion(
    plugin_host: &dyn PluginHost,
    session_store: &dyn SessionStore,
    source_input: String,
    source_cursor: usize,
) -> Result<Option<ResolvedCommandCompletion>> {
    let cursor = u32::try_from(source_cursor).context("命令补全光标超出协议范围")?;
    let response: PrepareCompletionResponse = call_typed_plugin_service(
        plugin_host,
        TUI_COMMAND_CALLER,
        PROVIDER_PLUGIN_ID,
        PREPARE_COMPLETION_SERVICE,
        &PrepareCompletionRequest {
            input: source_input.clone(),
            cursor: Some(cursor),
            limit: 6,
        },
    )
    .await?
    .ok_or_else(|| anyhow!("Command 插件不可用，无法补全参数"))?;

    match response {
        PrepareCompletionResponse::Candidates { context, items } => Ok(
            resolved_command_completion(&source_input, source_cursor, context, items),
        ),
        PrepareCompletionResponse::Callback {
            context,
            owner_plugin_id,
            service,
            request,
        } => {
            let response: CommandCallbackResponse = call_typed_plugin_service(
                plugin_host,
                PROVIDER_PLUGIN_ID,
                &owner_plugin_id,
                &service,
                &request,
            )
            .await?
            .ok_or_else(|| anyhow!("候选提供插件 `{owner_plugin_id}` 不可用"))?;
            match response {
                CommandCallbackResponse::Completed { items } => Ok(resolved_command_completion(
                    &source_input,
                    source_cursor,
                    context,
                    items,
                )),
                CommandCallbackResponse::Executed { .. } => {
                    Err(anyhow!("参数补全回调返回了执行结果"))
                }
            }
        }
        PrepareCompletionResponse::Surface { context, request } => {
            if request.source != SESSION_COMPLETION_SOURCE {
                return Err(anyhow!("不支持的命令候选数据源：{}", request.source));
            }
            let items = session_completion_items(
                session_store,
                &request.request.prefix,
                request.request.limit,
            )
            .await?;
            Ok(resolved_command_completion(
                &source_input,
                source_cursor,
                context,
                items,
            ))
        }
        PrepareCompletionResponse::NoMatch => Ok(None),
        PrepareCompletionResponse::Error { message } => Err(anyhow!(message)),
    }
}

/// 构造最多六项的稳定 TUI 参数候选。
#[cfg(feature = "plugins")]
fn resolved_command_completion(
    source_input: &str,
    source_cursor: usize,
    context: CompletionContext,
    mut items: Vec<CompletionItem>,
) -> Option<ResolvedCommandCompletion> {
    items.truncate(6);
    (!items.is_empty()).then(|| ResolvedCommandCompletion {
        source_input: source_input.to_string(),
        source_cursor,
        context,
        items,
    })
}

/// 从当前项目的轻量摘要生成 Session 参数候选。
#[cfg(feature = "plugins")]
async fn session_completion_items(
    session_store: &dyn SessionStore,
    prefix: &str,
    limit: u16,
) -> Result<Vec<CompletionItem>> {
    let mut summaries = session_store.list_summaries().await?;
    summaries.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let prefix = prefix.to_lowercase();
    Ok(summaries
        .into_iter()
        .filter(|summary| {
            prefix.is_empty()
                || summary.id.as_str().to_lowercase().contains(&prefix)
                || summary
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(&prefix))
        })
        .take(usize::from(limit.clamp(1, 6)))
        .map(|summary| CompletionItem {
            label: summary
                .title
                .clone()
                .unwrap_or_else(|| summary.id.to_string()),
            insert_text: summary.id.to_string(),
            description: Some(format!(
                "{} 条消息 · {}",
                summary.message_count,
                relative_time_label(summary.updated_at_ms)
            )),
        })
        .collect())
}

/// 执行 Command Provider 生成的受控计划，并刷新可能变化的命令快照。
#[cfg(feature = "plugins")]
async fn execute_command(app: &mut App, plugin_host: &dyn PluginHost, input: String) -> Result<()> {
    if app.running
        && app
            .command_spec_for_input(&input)
            .is_some_and(|spec| spec.availability == CommandAvailability::IdleOnly)
    {
        app.messages
            .push(Msg::new(MsgKind::Error, "该命令只能在 Agent 空闲时执行"));
        return Ok(());
    }

    let response: PrepareExecuteResponse = call_typed_plugin_service(
        plugin_host,
        TUI_COMMAND_CALLER,
        PROVIDER_PLUGIN_ID,
        PREPARE_EXECUTE_SERVICE,
        &PrepareExecuteRequest {
            input,
            agent_idle: !app.running,
        },
    )
    .await?
    .ok_or_else(|| anyhow!("Command 插件不可用，无法执行斜杠命令"))?;

    match response {
        PrepareExecuteResponse::Callback {
            owner_plugin_id,
            service,
            request,
        } => {
            let response: CommandCallbackResponse = call_typed_plugin_service(
                plugin_host,
                PROVIDER_PLUGIN_ID,
                &owner_plugin_id,
                &service,
                &request,
            )
            .await?
            .ok_or_else(|| anyhow!("命令处理插件 `{owner_plugin_id}` 不可用"))?;
            match response {
                CommandCallbackResponse::Executed { result } => {
                    let content = match result {
                        Value::Null => "命令执行完成".to_string(),
                        Value::String(content) => content,
                        value => serde_json::to_string_pretty(&value)
                            .context("格式化命令执行结果失败")?,
                    };
                    app.messages
                        .push(Msg::new(MsgKind::Info, truncate_line(&content, 4_000)));
                }
                CommandCallbackResponse::Completed { .. } => {
                    return Err(anyhow!("命令执行回调返回了补全结果"));
                }
            }
        }
        PrepareExecuteResponse::SurfaceAction { action } => match action {
            SurfaceAction::NewSession => app.start_new_draft("已新建空白会话")?,
            SurfaceAction::ClearSession => app.start_new_draft("会话上下文已清空")?,
            SurfaceAction::CompactSession => compact_current_session(app, plugin_host).await?,
            SurfaceAction::ExitApplication => app.should_quit = true,
        },
        PrepareExecuteResponse::SurfaceOpened { view_id } => {
            if view_id != SESSION_DIALOG_VIEW {
                return Err(anyhow!("Command 插件返回了未知界面：{view_id}"));
            }
            process_command_surface_effects(app, plugin_host).await?;
            refresh_plugin_view(app, plugin_host, PROVIDER_PLUGIN_ID, SESSION_DIALOG_VIEW).await;
        }
        PrepareExecuteResponse::Output { content } => {
            app.messages.push(Msg::new(MsgKind::Info, content));
        }
        PrepareExecuteResponse::Error { message, usage } => {
            let content = usage
                .filter(|usage| !usage.trim().is_empty())
                .map(|usage| format!("{message}\n用法：{usage}"))
                .unwrap_or(message);
            app.messages.push(Msg::new(MsgKind::Error, content));
        }
    }

    if let Some(snapshot) = load_command_snapshot(plugin_host).await? {
        app.set_command_snapshot(Some(snapshot));
    }
    Ok(())
}

/// 立即调用 Context 插件压缩当前 Session，成功持久化后再替换内存与界面状态。
#[cfg(feature = "plugins")]
async fn compact_current_session(app: &mut App, plugin_host: &dyn PluginHost) -> Result<()> {
    let before_session = app.session_record.session.clone();
    let request = ContextLoadRequest {
        run_id: format!(
            "compact:{}:{}",
            app.session_record.id, app.session_record.revision
        ),
        step: 0,
        provider: "manual".into(),
        model: app.model_name.clone(),
        system: before_session.system().cloned(),
        messages: before_session.model_messages(),
    };
    let before_messages = request.messages.len();
    let loaded: LoadedContext = call_typed_plugin_service(
        plugin_host,
        TUI_COMMAND_CALLER,
        CONTEXT_PLUGIN_ID,
        CONTEXT_COMPACT_SERVICE,
        &request,
    )
    .await?
    .ok_or_else(|| anyhow!("Context 插件不可用，无法压缩当前会话"))?;

    if loaded.system == request.system && loaded.messages == request.messages {
        app.messages
            .push(Msg::new(MsgKind::Info, "当前会话没有可压缩的历史上下文"));
        return Ok(());
    }

    let after_messages = loaded.messages.len();
    let mut compacted_record = app.session_record.clone();
    compacted_record.session = Session::from_parts(loaded.system, loaded.messages);
    let expected_revision = (compacted_record.revision > 0).then_some(compacted_record.revision);
    let saved = save_session_record_reconciled(
        app.session_store.as_ref(),
        compacted_record,
        expected_revision,
    )
    .await?;
    let notice = format!("上下文已立即压缩：{before_messages} 条消息变为 {after_messages} 条");
    app.replace_session(saved, Some(&notice));
    Ok(())
}

/// 处理 Command 会话界面产生的查询、恢复和关闭动作。
#[cfg(feature = "plugins")]
async fn process_command_surface_effects(
    app: &mut App,
    plugin_host: &dyn PluginHost,
) -> Result<()> {
    // 单次输入最多处理有限轮 effect，避免异常插件使事件循环无法返回。
    for _ in 0..8 {
        let response: SurfaceEffectsResponse = call_typed_plugin_service(
            plugin_host,
            TUI_COMMAND_CALLER,
            PROVIDER_PLUGIN_ID,
            SURFACE_POLL_EFFECTS_SERVICE,
            &SnapshotRequest::default(),
        )
        .await?
        .ok_or_else(|| anyhow!("Command 插件在会话界面打开后不可用"))?;
        if response.effects.is_empty() {
            return Ok(());
        }

        for effect in response.effects {
            match effect {
                SurfaceEffect::QuerySessions {
                    request_id,
                    query,
                    cursor,
                    limit,
                } => {
                    app.start_command_query(request_id, query, cursor, limit);
                }
                SurfaceEffect::ResumeSession {
                    session_id,
                    revision,
                } => {
                    resume_selected_session(app, &session_id, revision).await?;
                    app.plugin_focus = None;
                }
                SurfaceEffect::CloseSurface => app.plugin_focus = None,
            }
        }
    }
    Err(anyhow!("Command 会话界面产生了过多连续动作"))
}

/// 读取、过滤并分页当前项目的轻量会话摘要。
#[cfg(feature = "plugins")]
async fn command_session_page(
    session_store: &dyn SessionStore,
    active_session_id: &SessionId,
    query: &str,
    cursor: Option<&str>,
    limit: u16,
) -> SessionListStatus {
    let mut summaries = match session_store.list_summaries().await {
        Ok(summaries) => summaries,
        Err(error) => {
            return SessionListStatus::Error {
                message: format!("读取会话列表失败：{error}"),
            };
        }
    };
    let query = query.trim().to_lowercase();
    summaries.retain(|summary| {
        query.is_empty()
            || summary.id.as_str().to_lowercase().contains(&query)
            || summary
                .title
                .as_deref()
                .is_some_and(|title| title.to_lowercase().contains(&query))
    });
    summaries.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });

    let offset = cursor
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(summaries.len());
    let page_limit = usize::from(limit.clamp(1, 100));
    let page_end = offset.saturating_add(page_limit).min(summaries.len());
    let items = summaries[offset..page_end]
        .iter()
        .map(|summary| command_session_summary(active_session_id, summary))
        .collect::<Vec<_>>();
    if items.is_empty() {
        SessionListStatus::Empty
    } else {
        SessionListStatus::Ready {
            items,
            next_cursor: (page_end < summaries.len()).then(|| page_end.to_string()),
        }
    }
}

/// 将存储层摘要映射为 Command 插件稳定协议。
#[cfg(feature = "plugins")]
fn command_session_summary(
    active_session_id: &SessionId,
    summary: &SessionSummary,
) -> CommandSessionSummary {
    CommandSessionSummary {
        id: summary.id.to_string(),
        title: summary
            .title
            .clone()
            .unwrap_or_else(|| summary.id.to_string()),
        preview: String::new(),
        message_count: u64::try_from(summary.message_count).unwrap_or(u64::MAX),
        updated_at_ms: summary.updated_at_ms,
        updated_label: relative_time_label(summary.updated_at_ms),
        revision: summary.revision,
        active: &summary.id == active_session_id,
    }
}

/// 生成人类可读且无需额外依赖的会话更新时间标签。
#[cfg(feature = "plugins")]
fn relative_time_label(updated_at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(updated_at_ms);
    let elapsed_seconds = now_ms.saturating_sub(updated_at_ms) / 1_000;
    match elapsed_seconds {
        0..=59 => "刚刚".into(),
        60..=3_599 => format!("{} 分钟前", elapsed_seconds / 60),
        3_600..=86_399 => format!("{} 小时前", elapsed_seconds / 3_600),
        _ => format!("{} 天前", elapsed_seconds / 86_400),
    }
}

/// 在切换前重新读取并校验用户选择的会话修订号。
#[cfg(feature = "plugins")]
async fn resume_selected_session(app: &mut App, session_id: &str, revision: u64) -> Result<()> {
    let id = SessionId::new(session_id)?;
    let record = app
        .session_store
        .load(&id)
        .await?
        .ok_or_else(|| anyhow!("会话 `{session_id}` 已不存在"))?;
    if record.revision != revision {
        return Err(anyhow!(
            "会话 `{session_id}` 已更新，请重新打开 /resume 后选择"
        ));
    }
    let notice = format!("已恢复会话 {}", record.id);
    app.replace_session(record, Some(&notice));
    Ok(())
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

/// 普通启动创建空白 Draft；只有显式 CLI 参数才恢复已有会话。
async fn load_startup_session(
    store: &dyn SessionStore,
    cli_session_id: Option<&str>,
    workspace: &WorkspaceContext,
    cli_resume_latest: bool,
) -> Result<SessionRecord> {
    if cli_session_id.is_none() && cli_resume_latest {
        let mut summaries = store.list_summaries().await?;
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some(summary) = summaries.into_iter().next() {
            if let Some(record) = store.load(&summary.id).await? {
                return Ok(record);
            }
        }
    }

    let Some(id) = cli_session_id else {
        return workspace.draft_record();
    };
    let id = SessionId::new(id)?;
    Ok(match store.load(&id).await? {
        Some(record) => record,
        None => workspace.record_with_id(id)?,
    })
}

/// 输出按最近更新时间排序的持久化会话摘要。
async fn print_persisted_sessions(store: &dyn SessionStore) -> Result<()> {
    let mut summaries = store.list_summaries().await?;
    summaries.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    if summaries.is_empty() {
        println!("没有持久化会话");
        return Ok(());
    }

    println!("SESSION\tREVISION\tMESSAGES\tUPDATED_MS\tTITLE");
    for summary in summaries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            summary.id,
            summary.revision,
            summary.message_count,
            summary.updated_at_ms,
            summary.title.as_deref().unwrap_or("")
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
    agent_template: AgentTemplate,
) -> Result<LoadedPlugins> {
    let runtime =
        AgentRuntime::new(RuntimeLimits::default()).context("创建 TUI Agent Runtime 失败")?;
    let controller_profile =
        AgentProfileId::new("tui-controller").context("创建 TUI controller profile 失败")?;
    runtime
        .register_profile(
            controller_profile.clone(),
            agent_template,
            AgentPermissions::default(),
        )
        .await
        .context("注册 TUI controller profile 失败")?;
    let host_services = PluginHostServices::new().with_agent_runtime(
        Arc::new(runtime),
        controller_profile,
        HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
    )?;
    let report = load_wasm_plugins_resilient_with_selection_and_services(
        &manifests,
        &capability_selection,
        host_services,
    )
    .await?;
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
    let workspace = WorkspaceContext::capture()?;
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
    let sessions_root = resolve_tui_path(
        args.sessions_dir.as_deref(),
        tui_settings.sessions_dir.as_deref(),
        &config_path,
        lucia_home.join("projects"),
    );
    let sessions_dir = workspace.sessions_dir(&sessions_root);
    let events_jsonl = args.events_jsonl.clone().or_else(|| {
        tui_settings
            .events_jsonl
            .as_deref()
            .map(|path| resolve_config_relative_path(&config_path, path))
    });
    let session_store: Arc<dyn SessionStore> = Arc::new(LazyFileSessionStore::new(sessions_dir));
    if args.list_sessions {
        return print_persisted_sessions(session_store.as_ref()).await;
    }
    let session_record = load_startup_session(
        session_store.as_ref(),
        args.session_id.as_deref(),
        &workspace,
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
    let plugin_agent_template = AgentTemplate::from_agent(&base_agent);
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
    // 启用 bracketed paste，将拖入的文件路径识别为附件
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);

    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if input_tx.send(UiEvent::Input(ev)).is_err() {
                break;
            }
        }
    });

    let tick_tx = tx.clone();
    let tick_pending = Arc::new(AtomicBool::new(false));
    let producer_tick_pending = Arc::clone(&tick_pending);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(UI_TICK_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if producer_tick_pending.swap(true, Ordering::Relaxed) {
                continue;
            }
            if tick_tx.send(UiEvent::Tick).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(tx.clone(), model_name)
        .with_workspace(workspace)
        .with_persistent_session(session_store, session_record);
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
            let result = load_plugins_for_tui(
                plugin_manifests,
                capability_selection,
                plugin_agent_template,
            )
            .await;
            let _ = load_tx.send(UiEvent::PluginsLoaded(Box::new(result)));
        })
    };

    loop {
        terminal.draw(|frame| render_root(frame, &mut app))?;

        match rx.recv().await {
            Some(UiEvent::Input(Event::Key(key))) => {
                if key.kind == KeyEventKind::Press {
                    #[cfg(feature = "plugins")]
                    match app.route_plugin_key(key.code, key.modifiers) {
                        PluginKeyRoute::Main => {
                            let argument_completion = matches!(key.code, KeyCode::Tab)
                                && app
                                    .input
                                    .trim_start()
                                    .strip_prefix('/')
                                    .is_some_and(|body| body.chars().any(char::is_whitespace));
                            let command_submission = matches!(key.code, KeyCode::Enter)
                                && app.input.trim_start().starts_with('/');
                            if argument_completion && !app.plugins_loading {
                                if !app.apply_selected_command_completion()
                                    && !app.command_completion_loading
                                {
                                    if let Some(host) = plugin_host.as_ref() {
                                        app.schedule_command_completion(Arc::clone(host));
                                    } else {
                                        app.messages.push(Msg::new(
                                            MsgKind::Error,
                                            "Command 插件不可用，无法补全参数",
                                        ));
                                    }
                                }
                            } else if command_submission && !app.plugins_loading {
                                if let Some(input) = app.take_input() {
                                    if let Some(host) = plugin_host.as_ref() {
                                        if let Err(error) =
                                            execute_command(&mut app, host.as_ref(), input).await
                                        {
                                            app.messages.push(Msg::new(
                                                MsgKind::Error,
                                                format!("命令执行失败：{error}"),
                                            ));
                                        }
                                    } else {
                                        app.messages.push(Msg::new(
                                            MsgKind::Error,
                                            "Command 插件不可用，无法执行斜杠命令",
                                        ));
                                    }
                                }
                            } else {
                                app.handle_key(key.code, key.modifiers, agent.as_ref());
                            }
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
                                        input.instance_id.as_deref(),
                                        &error,
                                    );
                                } else {
                                    if let Err(error) =
                                        drain_plugin_ui_events(&mut app, host.as_ref()).await
                                    {
                                        app.messages.push(Msg::new(
                                            MsgKind::Error,
                                            format!("插件 UI 事件处理失败：{error}"),
                                        ));
                                    }
                                    if input.plugin_id == PROVIDER_PLUGIN_ID
                                        && input.view_id == SESSION_DIALOG_VIEW
                                    {
                                        if let Err(error) =
                                            process_command_surface_effects(&mut app, host.as_ref())
                                                .await
                                        {
                                            app.messages.push(Msg::new(
                                                MsgKind::Error,
                                                format!("会话界面操作失败：{error}"),
                                            ));
                                        }
                                    }
                                    refresh_plugin_view(
                                        &mut app,
                                        host.as_ref(),
                                        &input.plugin_id,
                                        &input.view_id,
                                    )
                                    .await;
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
                        for event in &loaded.startup_events {
                            if let Err(error) = apply_plugin_navigation_event(&mut app, event) {
                                app.messages.push(Msg::new(
                                    MsgKind::Error,
                                    format!("插件视图导航失败：{error}"),
                                ));
                            }
                        }
                        app.finish_plugin_loading(
                            loaded.plugin_ids,
                            loaded.startup_events,
                            loaded.failures,
                        );
                        match load_command_snapshot(loaded.host.as_ref()).await {
                            Ok(snapshot) => app.set_command_snapshot(snapshot),
                            Err(error) => app.messages.push(Msg::new(
                                MsgKind::Error,
                                format!("Command 命令快照加载失败：{error}"),
                            )),
                        }
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
            #[cfg(feature = "plugins")]
            Some(UiEvent::CommandSurfaceUpdate { request_id, status }) => {
                if let Some(host) = plugin_host.as_ref() {
                    let update = SurfaceUpdateRequest { request_id, status };
                    match call_typed_plugin_service::<_, Value>(
                        host.as_ref(),
                        TUI_COMMAND_CALLER,
                        PROVIDER_PLUGIN_ID,
                        SURFACE_UPDATE_SERVICE,
                        &update,
                    )
                    .await
                    {
                        Ok(Some(_)) => {
                            refresh_plugin_view(
                                &mut app,
                                host.as_ref(),
                                PROVIDER_PLUGIN_ID,
                                SESSION_DIALOG_VIEW,
                            )
                            .await
                        }
                        Ok(None) => app.messages.push(Msg::new(
                            MsgKind::Error,
                            "Command 插件在会话查询完成前已不可用",
                        )),
                        Err(error) => app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!("更新会话界面失败：{error}"),
                        )),
                    }
                }
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::CommandSnapshotLoaded(result)) => {
                app.command_snapshot_refreshing = false;
                if let Ok(snapshot) = *result {
                    let changed = snapshot.as_ref().map(|snapshot| snapshot.generation)
                        != app
                            .command_snapshot
                            .as_ref()
                            .map(|snapshot| snapshot.generation);
                    if changed {
                        app.set_command_snapshot(snapshot);
                    }
                }
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::CommandCompletionLoaded { generation, result }) => {
                if generation == app.command_completion_generation {
                    app.command_completion_loading = false;
                    app.command_completion_task = None;
                    match *result {
                        Ok(Some(completion))
                            if completion.source_input == app.input
                                && completion.source_cursor == app.cursor =>
                        {
                            let apply_immediately = completion.items.len() == 1;
                            app.command_completion = Some(completion);
                            app.command_selection = 0;
                            app.command_preview_hidden = false;
                            if apply_immediately {
                                app.apply_selected_command_completion();
                            }
                        }
                        Ok(_) => {}
                        Err(error) => app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!("命令参数补全失败：{error}"),
                        )),
                    }
                }
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
            #[cfg(feature = "plugins")]
            Some(UiEvent::ViewNavigation { plugin_id, request }) => {
                if let Err(error) = app.apply_view_navigation(&plugin_id, request) {
                    app.messages.push(Msg::new(
                        MsgKind::Error,
                        format!("插件视图导航失败：{error}"),
                    ));
                }
            }
            Some(UiEvent::ContextUsage(tokens)) => {
                app.context_tokens = Some(tokens);
            }
            Some(UiEvent::AgentDone(result)) => {
                let queue_may_advance = app.handle_agent_done(*result);
                if queue_may_advance {
                    if let Some(agent) = agent.as_ref() {
                        app.run_next_queued(agent);
                    }
                }
            }
            Some(UiEvent::Tick) => {
                tick_pending.store(false, Ordering::Relaxed);
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
                    if app.plugin_tick >= PLUGIN_REFRESH_TICKS {
                        app.plugin_tick = 0;
                        if let Some(host) = plugin_host.as_ref() {
                            refresh_plugin_views(&mut app, host.as_ref()).await;
                        }
                    }
                    app.command_snapshot_tick = app.command_snapshot_tick.wrapping_add(1);
                    if app.command_snapshot_tick >= COMMAND_SNAPSHOT_REFRESH_TICKS {
                        app.command_snapshot_tick = 0;
                        if let Some(host) = plugin_host.as_ref() {
                            app.schedule_command_snapshot_refresh(Arc::clone(host));
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
                                app.set_plugin_ui_error(
                                    &input.plugin_id,
                                    &input.view_id,
                                    input.instance_id.as_deref(),
                                    &error,
                                );
                            } else {
                                if let Err(error) =
                                    drain_plugin_ui_events(&mut app, host.as_ref()).await
                                {
                                    app.messages.push(Msg::new(
                                        MsgKind::Error,
                                        format!("插件 UI 事件处理失败：{error}"),
                                    ));
                                }
                                if input.plugin_id == PROVIDER_PLUGIN_ID
                                    && input.view_id == SESSION_DIALOG_VIEW
                                {
                                    if let Err(error) =
                                        process_command_surface_effects(&mut app, host.as_ref())
                                            .await
                                    {
                                        app.messages.push(Msg::new(
                                            MsgKind::Error,
                                            format!("会话界面操作失败：{error}"),
                                        ));
                                    }
                                }
                                refresh_plugin_view(
                                    &mut app,
                                    host.as_ref(),
                                    &input.plugin_id,
                                    &input.view_id,
                                )
                                .await;
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
            Some(UiEvent::Input(Event::Paste(pasted))) => {
                // 粘贴只进入主输入框；插件视图聚焦或模态层激活时忽略。
                #[cfg(feature = "plugins")]
                let main_input_focused = app.plugin_focus.is_none()
                    && app.active_dialog_index().is_none()
                    && app.view_stack.is_main();
                #[cfg(not(feature = "plugins"))]
                let main_input_focused = true;
                if main_input_focused {
                    app.handle_paste(&pasted);
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

    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
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
#[path = "main/tests.rs"]
mod tests;
