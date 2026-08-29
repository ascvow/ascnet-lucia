//! TUI App 状态机、运行队列、插件视图状态和事件 sink。

mod event_sink;
mod input;
#[cfg(feature = "plugins")]
mod plugin_view;
mod run;
#[cfg(test)]
mod tests;

pub(crate) use event_sink::ChannelEventSink;
#[cfg(feature = "plugins")]
pub(crate) use plugin_view::{default_plugin_height, default_plugin_width, plugin_view_visible};

use crate::command::NativeCommandState;
use crate::event::UiEvent;
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementService;
use crate::session::conversation::{Msg, MsgKind, PendingAttachment, UserSubmission};
use crate::session::{restore_session_messages, WorkspaceContext};
use crate::theme::{COLOR_TEXT, COLOR_WARNING};
#[cfg(feature = "plugins")]
use crate::ui::view::ViewStack;
use agent_core::model::ReasoningLevel;
use agent_core::{ContextLoader, Session};
#[cfg(feature = "plugins")]
use agent_plugin_protocol::UiFrame as PluginUiFrame;
#[cfg(feature = "plugins")]
use agent_plugin_protocol::{UiDeclaration, UiInput};
use agent_session::{MemorySessionStore, SessionId, SessionRecord, SessionStore};
use anyhow::Result;
use ratatui::prelude::*;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 主输入区识别连续两次 Esc 的最长间隔，避免单次误触清空草稿。
pub(crate) const ESC_DOUBLE_PRESS_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(500);

/// 将输入文本切分为普通文本与附件引用标签片段，标签使用高亮样式。
pub(crate) fn input_ref_spans<'a>(
    input: &'a str,
    attachments: &'a [PendingAttachment],
) -> Vec<Span<'a>> {
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

/// 独占审批显示期间固定使用的主事件区快照。
///
/// 后台模型与并行工具仍可更新真实状态；审批结束后丢弃快照即可一次性追上，避免丢失结果。
#[cfg(feature = "plugins")]
pub(crate) struct MainEventSnapshot {
    /// 快照建立时已经存在的主事件消息。
    pub(crate) messages: Vec<Msg>,
    /// 快照建立时正在接收增量的助手消息索引。
    pub(crate) streaming_message: Option<usize>,
    /// 快照建立时模型是否处于思考阶段。
    pub(crate) model_is_thinking: bool,
}

pub(crate) struct App {
    /// 本次进程固定使用的项目工作目录。
    pub(crate) workspace: WorkspaceContext,
    pub(crate) messages: Vec<Msg>,
    pub(crate) input: String,
    /// 等待随下一条消息发送的附件；引用标签内嵌在 `input` 文本中。
    pub(crate) attachments: Vec<PendingAttachment>,
    /// FIFO inputs accepted before the Agent becomes ready. Agent 就绪前接收的 FIFO 输入队列。
    pub(crate) queued_inputs: VecDeque<UserSubmission>,
    /// 当前运行是否来自 FIFO 队首；成功预写后才允许移除对应输入。
    pub(crate) queued_run_active: bool,
    /// 光标在 input 中的字节偏移。
    pub(crate) cursor: usize,
    /// 最近成功提交的输入，供空输入框使用上下方向键回溯。
    pub(crate) input_history: Vec<String>,
    /// 当前回溯到的历史下标；`None` 表示正在编辑新输入。
    pub(crate) input_history_cursor: Option<usize>,
    pub(crate) running: bool,
    /// 不依赖 Plugin Host 的默认斜杠命令与会话 Dialog 状态。
    pub(crate) native_command: NativeCommandState,
    /// 正在后台重载会话上下文时展示的进度标签；期间禁止提交新消息。
    pub(crate) pending_reload: Option<String>,
    /// 当前运行的起始时间，用于渲染运行耗时。
    pub(crate) run_started_at: Option<std::time::Instant>,
    /// 主输入区最近一次未被其他按键打断的 Esc 时间，用于识别连续双按。
    pub(crate) last_escape_at: Option<std::time::Instant>,
    pub(crate) should_quit: bool,
    /// 下一轮使用的完整会话记录；最终保存失败时可暂存 dirty 完成态。
    pub(crate) session_record: SessionRecord,
    /// 执行 revision 比较并交换的会话存储。
    pub(crate) session_store: Arc<dyn SessionStore>,
    /// `/compact` 与模型请求共同使用的原生上下文加载器。
    pub(crate) context_loader: Arc<dyn ContextLoader>,
    /// TUI 人工插件控制面；只在插件构建中存在。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_management: Option<Arc<PluginManagementService>>,
    pub(crate) tx: mpsc::UnboundedSender<UiEvent>,
    pub(crate) model_name: String,
    /// 当前进程实际加载的配置路径；思考等级切换会原子更新该文件。
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) spinner_frame: usize,
    /// 当前正在接收增量文本的助手消息索引。
    pub(crate) streaming_message: Option<usize>,
    /// 用户取消当前运行后立即关闭可见流，防止已排队的模型或工具增量继续刷新界面。
    pub(crate) run_stream_cancelled: bool,
    /// 当前模型是否处于思考阶段；仅控制状态文案，不保存推理摘要文本。
    pub(crate) model_is_thinking: bool,
    /// 独占 Input 视图可见期间使用的主事件区快照；隐藏后恢复实时状态。
    #[cfg(feature = "plugins")]
    pub(crate) main_event_snapshot: Option<MainEventSnapshot>,
    /// 手动滚动偏移；None 表示跟随底部自动滚动。
    pub(crate) scroll: Option<u16>,
    /// 上一帧计算出的最大滚动偏移，供滚动操作作为起点。
    pub(crate) last_max_scroll: u16,
    /// 最近一次渲染得到的消息区高度，用于整页滚动。
    pub(crate) last_viewport: u16,
    /// 最近一次渲染得到的消息区宽度，用于工具消息 renderer 分配尺寸。
    #[cfg(feature = "plugins")]
    pub(crate) last_message_width: u16,
    /// 最近一次模型请求消耗的上下文 token 数。
    pub(crate) context_tokens: Option<u64>,
    /// 配置的模型上下文窗口，用于状态栏计算占比。
    pub(crate) context_window: Option<u64>,
    /// models.dev 为当前模型声明的可选推理等级。
    pub(crate) reasoning_levels: Vec<ReasoningLevel>,
    /// 当前 Agent 使用的推理等级。
    pub(crate) reasoning_level: ReasoningLevel,
    /// models.dev 为当前模型声明的输入模态；`None` 表示快照未匹配，不限制旧行为。
    pub(crate) model_input_modalities: Option<Vec<crate::config::model_metadata::ModelModality>>,
    /// 鼠标捕获是否开启；默认开启，由应用同时处理滚轮、拖选复制和插件点击。
    pub(crate) mouse_capture: bool,
    /// 当前屏幕文本选择；鼠标释放时把可见缓冲区中的选中文本写入系统剪贴板。
    pub(crate) text_selection: Option<MouseTextSelection>,
    /// 插件声明的视图及宿主缓存的最近一帧。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_views: Vec<PluginViewState>,
    /// 当前通过 Tab 聚焦的停靠视图索引；模态对话框会临时覆盖该焦点。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_focus: Option<usize>,
    /// 单调递增的插件渲染帧序号。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_frame: u64,
    /// 控制插件 UI 刷新频率的主循环 tick 计数。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_tick: u8,
    /// 当前唯一的后台插件视图刷新任务。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_refresh_task: Option<tokio::task::JoinHandle<()>>,
    /// 刷新任务运行期间是否又收到了一次合并后的刷新请求。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_refresh_pending: bool,
    /// Loaded plugin IDs shown by the compact status counter. 紧凑状态计数展示的插件 ID。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_ids: Vec<String>,
    /// 尚未结束激活的插件 ID，按 manifest 发现顺序展示。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_loading_ids: Vec<String>,
    /// Startup activation summaries shown once below the input. 输入框下方一次性展示的启动摘要。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_startup_details: Vec<String>,
    /// Remaining ticks before startup details collapse. 启动详情收敛前的剩余 tick 数。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_status_ticks: u16,
    /// Whether plugin activation is still running in the background. 插件是否仍在后台激活。
    #[cfg(feature = "plugins")]
    pub(crate) plugins_loading: bool,
    /// Plugin startup failure shown persistently in the footer. 底栏持续展示的插件启动错误。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_load_error: Option<String>,
    /// Per-plugin failures retained alongside successful plugins. 与成功插件并存的单插件失败摘要。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_failures: Vec<String>,
    /// 主视图之上的插件子视图导航栈。
    #[cfg(feature = "plugins")]
    pub(crate) view_stack: ViewStack,
    /// 最近一次会话摘要查询任务；新查询会中止尚未完成的旧查询。
    #[cfg(feature = "plugins")]
    pub(crate) sessions_query_task: Option<tokio::task::JoinHandle<()>>,
    /// 最近处理过的宿主动作请求，用于忽略插件的重复交付。
    #[cfg(feature = "plugins")]
    pub(crate) applied_host_actions: VecDeque<(String, String)>,
}

/// 主 TUI 为单个插件视图维护的运行时状态。
#[cfg(feature = "plugins")]
pub(crate) struct PluginViewState {
    /// 插件提供并由宿主补全插件 ID 的静态声明。
    pub(crate) declaration: UiDeclaration,
    /// 最近一次成功渲染的声明式内容。
    pub(crate) frame: Option<PluginUiFrame>,
    /// 最近一帧由主 TUI 分配的内容区域。
    pub(crate) area: Rect,
}

/// 应用内鼠标文本选择的屏幕坐标范围。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MouseTextSelection {
    /// 拖动起点，对应终端缓冲区坐标。
    anchor: Position,
    /// 最近一次拖动或释放位置。
    current: Position,
    /// 是否已经离开起点；普通点击不会触发复制或高亮。
    dragged: bool,
}

impl MouseTextSelection {
    /// 从鼠标按下位置创建尚未发生拖动的选择。
    pub(crate) fn new(anchor: Position) -> Self {
        Self {
            anchor,
            current: anchor,
            dragged: false,
        }
    }

    /// 更新选择终点，并记录是否形成了非空拖动范围。
    pub(crate) fn update(&mut self, current: Position) {
        self.current = current;
        self.dragged |= current != self.anchor;
    }

    /// 按终端线性选择顺序遍历范围内坐标，越界端点会收窄到缓冲区。
    fn for_each_position(self, area: Rect, mut visit: impl FnMut(Position)) {
        if area.is_empty() {
            return;
        }
        let clamp = |position: Position| {
            Position::new(
                position
                    .x
                    .clamp(area.left(), area.right().saturating_sub(1)),
                position
                    .y
                    .clamp(area.top(), area.bottom().saturating_sub(1)),
            )
        };
        let anchor = clamp(self.anchor);
        let current = clamp(self.current);
        let (start, end) = if (anchor.y, anchor.x) <= (current.y, current.x) {
            (anchor, current)
        } else {
            (current, anchor)
        };
        for y in start.y..=end.y {
            let first = if y == start.y { start.x } else { area.left() };
            let last = if y == end.y {
                end.x
            } else {
                area.right().saturating_sub(1)
            };
            for x in first..=last {
                visit(Position::new(x, y));
            }
        }
    }

    /// 从已渲染缓冲区提取选择文本；返回值保留内部换行并移除行尾布局空白。
    pub(crate) fn extract(self, buffer: &Buffer) -> String {
        let mut rows = Vec::<String>::new();
        let mut row = String::new();
        let mut previous_y = None;
        let mut trailing_columns = 0usize;
        self.for_each_position(buffer.area, |position| {
            if previous_y.is_some_and(|y| y != position.y) {
                rows.push(row.trim_end().to_string());
                row.clear();
                trailing_columns = 0;
            }
            previous_y = Some(position.y);
            if trailing_columns > 0 {
                trailing_columns -= 1;
                return;
            }
            if let Some(cell) = buffer.cell(position) {
                let symbol = cell.symbol();
                row.push_str(symbol);
                trailing_columns = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
            }
        });
        if previous_y.is_some() {
            rows.push(row.trim_end().to_string());
        }
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows.join("\n")
    }
}

/// 键盘事件在主界面与插件界面之间的路由结果。
#[cfg(feature = "plugins")]
pub(crate) enum PluginKeyRoute {
    /// 继续交给主界面处理。
    Main,
    /// 焦点切换等宿主行为已经消费该事件。
    Consumed,
    /// 将转换后的事件发送给插件。
    Input(UiInput),
}

impl App {
    /// 创建空白会话，使首屏保持与参考界面一致的低干扰状态。
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>, model_name: String) -> Self {
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
            input_history: Vec::new(),
            input_history_cursor: None,
            running: false,
            native_command: NativeCommandState::default(),
            pending_reload: None,
            run_started_at: None,
            last_escape_at: None,
            should_quit: false,
            session_record,
            session_store: Arc::new(MemorySessionStore::new()),
            context_loader: Arc::new(agent_core::PassthroughContextLoader),
            #[cfg(feature = "plugins")]
            plugin_management: None,
            tx,
            model_name,
            config_path: None,
            spinner_frame: 0,
            streaming_message: None,
            run_stream_cancelled: false,
            model_is_thinking: false,
            #[cfg(feature = "plugins")]
            main_event_snapshot: None,
            scroll: None,
            last_max_scroll: 0,
            last_viewport: 0,
            #[cfg(feature = "plugins")]
            last_message_width: 0,
            context_tokens: None,
            context_window: None,
            reasoning_levels: vec![ReasoningLevel::Off],
            reasoning_level: ReasoningLevel::Off,
            model_input_modalities: None,
            mouse_capture: true,
            text_selection: None,
            #[cfg(feature = "plugins")]
            plugin_views: Vec::new(),
            #[cfg(feature = "plugins")]
            plugin_focus: None,
            #[cfg(feature = "plugins")]
            plugin_frame: 0,
            #[cfg(feature = "plugins")]
            plugin_tick: 0,
            #[cfg(feature = "plugins")]
            plugin_refresh_task: None,
            #[cfg(feature = "plugins")]
            plugin_refresh_pending: false,
            #[cfg(feature = "plugins")]
            plugin_ids: Vec::new(),
            #[cfg(feature = "plugins")]
            plugin_loading_ids: Vec::new(),
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
            sessions_query_task: None,
            #[cfg(feature = "plugins")]
            applied_host_actions: VecDeque::new(),
        }
    }

    /// 注入主函数启动时捕获的项目上下文。
    pub(crate) fn with_workspace(mut self, workspace: WorkspaceContext) -> Self {
        self.workspace = workspace;
        self
    }

    /// 注入本次启动实际读取的配置文件，供交互设置执行原子持久化。
    pub(crate) fn with_config_path(mut self, config_path: PathBuf) -> Self {
        self.config_path = Some(config_path);
        self
    }

    /// 注入模型上下文窗口；该值只影响状态栏展示。
    pub(crate) fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        self.context_window = context_window;
        self
    }

    /// 注入当前模型可切换的推理等级及启动时实际等级。
    pub(crate) fn with_reasoning_levels(
        mut self,
        reasoning_levels: Vec<ReasoningLevel>,
        reasoning_level: ReasoningLevel,
    ) -> Self {
        self.reasoning_levels = reasoning_levels;
        self.reasoning_level = reasoning_level;
        self
    }

    /// 注入 models.dev 声明的输入模态；匹配到模型后附件入口会严格校验能力。
    pub(crate) fn with_model_modalities(
        mut self,
        input: Vec<crate::config::model_metadata::ModelModality>,
    ) -> Self {
        self.model_input_modalities = Some(input);
        self
    }

    /// 注入启动时加载的持久化记录及其存储实现。
    pub(crate) fn with_persistent_session(
        mut self,
        session_store: Arc<dyn SessionStore>,
        session_record: SessionRecord,
    ) -> Self {
        self.messages = restore_session_messages(&session_record.session);
        self.session_store = session_store;
        self.session_record = session_record;
        self
    }

    /// 注入应用默认的原生上下文加载器。
    pub(crate) fn with_context_loader(mut self, context_loader: Arc<dyn ContextLoader>) -> Self {
        self.context_loader = context_loader;
        self
    }

    /// 注入只供 TUI 原生插件页使用的人工管理服务。
    #[cfg(feature = "plugins")]
    pub(crate) fn with_plugin_management(
        mut self,
        plugin_management: Option<Arc<PluginManagementService>>,
    ) -> Self {
        self.plugin_management = plugin_management;
        self
    }

    /// 用完整记录替换当前会话，并清理只属于旧会话的瞬时界面状态。
    pub(crate) fn replace_session(&mut self, session_record: SessionRecord, notice: Option<&str>) {
        self.messages = restore_session_messages(&session_record.session);
        self.session_record = session_record;
        self.input.clear();
        self.attachments.clear();
        self.cursor = 0;
        self.queued_inputs.clear();
        self.streaming_message = None;
        self.run_stream_cancelled = false;
        self.model_is_thinking = false;
        #[cfg(feature = "plugins")]
        {
            self.main_event_snapshot = None;
        }
        self.scroll = None;
        self.context_tokens = None;
        self.last_escape_at = None;
        self.native_command.dialog = None;
        self.native_command.reasoning_panel = None;
        #[cfg(feature = "plugins")]
        {
            self.native_command.plugin_dialog = None;
        }
        self.native_command.dismissed_input = None;
        if let Some(notice) = notice {
            self.messages.push(Msg::new(MsgKind::Info, notice));
        }
    }

    /// 进入当前项目下尚未持久化的全新空白草稿。
    pub(crate) fn start_new_draft(&mut self, notice: &str) -> Result<()> {
        let draft = self.workspace.draft_record()?;
        self.replace_session(draft, Some(notice));
        Ok(())
    }
}

/// 返回推理等级的紧凑中文标签，供底栏和切换反馈复用。
pub(crate) fn reasoning_level_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "关闭",
        ReasoningLevel::Minimal => "最小",
        ReasoningLevel::Low => "低",
        ReasoningLevel::Medium => "中",
        ReasoningLevel::High => "高",
        ReasoningLevel::XHigh => "超高",
        ReasoningLevel::Max => "最大",
    }
}
