//! TUI App 状态机、运行队列、插件视图状态和事件 sink。

#[cfg(feature = "plugins")]
mod agent_view;
mod attachments;
pub(crate) use attachments::{AttachmentIntent, AttachmentJob, PreparedAttachments};
mod event_sink;
mod input;
#[cfg(feature = "plugins")]
mod plugin_view;
mod run;
#[cfg(test)]
mod tests;

#[cfg(feature = "plugins")]
pub(crate) use agent_view::{AgentSessionView, AgentViewKeyOutcome};
pub(crate) use event_sink::ChannelEventSink;
#[cfg(feature = "plugins")]
pub(crate) use plugin_view::{default_plugin_height, default_plugin_width, plugin_view_visible};

use crate::command::NativeCommandState;
use crate::event::UiEvent;
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementService;
use crate::session::conversation::{Msg, MsgKind, PendingAttachment, PendingRef, UserSubmission};
use crate::session::{restore_session_messages, WorkspaceContext};
use crate::theme::{COLOR_TEXT, COLOR_WARNING};
#[cfg(feature = "plugins")]
use crate::ui::view::ViewStack;
use agent_context::NativeContextLoader;
use agent_core::model::{ReasoningLevel, TokenUsage};
use agent_core::{ContextLoader, Session};
#[cfg(feature = "plugins")]
use agent_plugin_protocol::UiFrame as PluginUiFrame;
#[cfg(feature = "plugins")]
use agent_plugin_protocol::{UiDeclaration, UiInput};
use agent_session::{MemorySessionStore, SessionId, SessionRecord, SessionStore};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
#[cfg(feature = "plugins")]
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 判断修饰键组合是否构成 Lucia 的"命令修饰键"。
///
/// 同时接受 `Control` 与 `Super`（macOS 的 Command、部分 Linux 桌面的 Meta）。
/// macOS 用户常把 Control、Option 与 Command 互换以贴近 Windows 手感，终端随之把
/// 同一个物理键上报成不同修饰键；只认 `Control` 会让全部快捷键在这些配置下失效。
/// 两者语义不冲突，因此一并接受而不是要求用户改回系统设置。
pub(crate) fn is_command_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

/// 判断终端按键是否表示复制快捷键；兼容增强键盘协议返回的大写字符和附加 Shift。
pub(crate) fn is_copy_shortcut(code: KeyCode, modifiers: KeyModifiers) -> bool {
    is_command_modifier(modifiers) && matches!(code, KeyCode::Char('c' | 'C'))
}

/// 主输入区识别连续两次 Esc 的最长间隔，避免单次误触清空草稿。
pub(crate) const ESC_DOUBLE_PRESS_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(500);

/// 识别连续两次 Ctrl+C 的最长间隔。
///
/// 比 Esc 的窗口宽松得多：清空草稿可以重打，退出会丢掉整个会话，用户需要时间
/// 读到提示再决定。
pub(crate) const EXIT_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// 将输入文本切分为普通文本与附件/引用标签片段，标签使用高亮样式。
pub(crate) fn input_ref_spans<'a>(
    input: &'a str,
    attachments: &'a [PendingAttachment],
    refs: &'a [PendingRef],
) -> Vec<Span<'a>> {
    let base = Style::new().fg(COLOR_TEXT);
    let clip = Style::new().fg(COLOR_WARNING).bold();
    if attachments.is_empty() && refs.is_empty() {
        return vec![Span::styled(input, base)];
    }
    let mut spans = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        // 取最靠前的引用标签作为下一个高亮片段。
        let earliest = attachments
            .iter()
            .map(|attachment| attachment.label.as_str())
            .chain(refs.iter().map(|entry| entry.label.as_str()))
            .filter_map(|label| rest.find(label).map(|pos| (pos, label.len())))
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
}

/// 最近一帧中可点击的工具消息区域，索引始终指向实时消息列表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolMessageHitRegion {
    /// 工具消息在 `App::messages` 中的索引。
    pub(crate) message_index: usize,
    /// 工具消息经过换行、滚动和裁剪后的可见终端区域。
    pub(crate) area: Rect,
}

pub(crate) struct App {
    /// 本次进程固定使用的项目工作目录。
    pub(crate) workspace: WorkspaceContext,
    /// 用户在消息里显式引用过的只读路径授权，与 Agent 的只读工具共享。
    pub(crate) read_path_grants: agent_tool::ReadPathGrants,
    pub(crate) messages: Vec<Msg>,
    pub(crate) input: String,
    pub(crate) async_attachments: bool,
    pub(crate) attachment_sequence: u64,
    pub(crate) editor_generation: u64,
    pub(crate) attachment_job: Option<AttachmentJob>,
    pub(crate) mentioned_attachments_ready: bool,
    /// 等待随下一条消息发送的附件；引用标签内嵌在 `input` 文本中。
    pub(crate) attachments: Vec<PendingAttachment>,
    /// 等待随下一条消息发送的上下文引用；标签同样内嵌在 `input` 文本中。
    pub(crate) refs: Vec<PendingRef>,
    /// 上一次向上下文引用 provider 拉取快照的时间。
    #[cfg(feature = "plugins")]
    pub(crate) context_ref_polled_at: Option<std::time::Instant>,
    /// 用户删除标签后主动丢弃的引用 ID；同一段上下文被重新选中前不再自动补回。
    pub(crate) dismissed_refs: std::collections::HashSet<String>,
    /// 最近一次引用快照的校验失败原因，按 provider 记录以抑制重复提示。
    #[cfg(feature = "plugins")]
    pub(crate) context_ref_error: Option<(String, String)>,
    /// 在途的上下文引用拉取任务；任一时刻至多一个。
    #[cfg(feature = "plugins")]
    pub(crate) context_ref_task: Option<tokio::task::JoinHandle<()>>,
    /// FIFO inputs accepted before the Agent becomes ready. Agent 就绪前接收的 FIFO 输入队列。
    pub(crate) queued_inputs: VecDeque<UserSubmission>,
    /// 当前运行是否来自 FIFO 队首；成功预写后才允许移除对应输入。
    pub(crate) queued_run_active: bool,
    /// 输入框上方排队消息面板的可点击行；每帧由渲染层重建。
    pub(crate) queued_input_areas: Vec<(usize, Rect)>,
    /// 排队面板的键盘焦点与选中下标；`None` 表示焦点在输入框。
    pub(crate) queue_focus: Option<usize>,
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
    /// 最近一次在空输入上按下 Ctrl+C 的时间，用于要求二次确认后才退出。
    ///
    /// 单击即退出会让一次误触丢掉整个会话，因此退出必须显式确认。
    pub(crate) last_exit_request_at: Option<std::time::Instant>,
    pub(crate) should_quit: bool,
    /// 下一轮使用的完整会话记录；最终保存失败时可暂存 dirty 完成态。
    pub(crate) session_record: SessionRecord,
    /// 执行 revision 比较并交换的会话存储。
    pub(crate) session_store: Arc<dyn SessionStore>,
    /// 会话标题生成器；demo 模式与测试环境为 `None`，此时只保留兜底标题。
    ///
    /// 与持久会话绑定：标题属于某个具体会话记录，切换会话后新记录仍复用同一路由。
    pub(crate) title_generator: Option<Arc<crate::session::title::SessionTitleGenerator>>,
    /// 本进程已经为该会话请求过标题；用会话 ID 而不是布尔值，避免切换会话后误跳过。
    ///
    /// 失败不重试：模型不可用时每次运行都补一次请求，只会把一次故障放大成持续延迟。
    pub(crate) title_attempted: Option<SessionId>,
    /// 已生成但尚未写入存储的标题。
    ///
    /// 运行中的轮次持有更早的记录副本，此刻保存会与该轮次的 CAS 抢同一个 revision，
    /// 因此先暂存，由完成态落地后的 `flush_session_title` 应用。
    pub(crate) pending_session_title: Option<String>,
    /// `/compact` 与模型请求共同使用的原生上下文加载器。
    pub(crate) context_loader: Arc<dyn ContextLoader>,
    /// 共享原生加载器的控制句柄，用于同步热切换后的模型路由与上下文窗口。
    pub(crate) native_context_loader: Option<Arc<NativeContextLoader>>,
    /// TUI 人工插件控制面；只在插件构建中存在。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_management: Option<Arc<PluginManagementService>>,
    pub(crate) tx: mpsc::UnboundedSender<UiEvent>,
    pub(crate) model_name: String,
    /// 当前生效上游在配置文件里的名称，底栏用它区分同一模型 ID 的不同配置项。
    ///
    /// 演示模式或配置缺失时为 `None`：此时模型来自应用内置的演示路由，没有对应配置项，
    /// 显示配置名会让人误以为配置里的上游正在生效。
    pub(crate) model_upstream_name: Option<String>,
    /// 当前进程实际加载的配置路径；思考等级切换会原子更新该文件。
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) spinner_frame: usize,
    /// 当前正在接收增量文本的助手消息索引。
    pub(crate) streaming_message: Option<usize>,
    /// 已收到但尚未显示到屏幕上的模型增量文本（打字机揭示缓冲）。
    ///
    /// 网络层按 TCP/SSE 的节奏交付文本，而屏幕需要固定的帧节拍：直接把每个增量写进
    /// 消息会让"屏幕刷新节奏"等于"网络到达节奏"。网络抖动时就表现为一会儿静止、
    /// 一会儿蹦出一整段。这里把到达与显示分开，由 `reveal_pacer` 按帧决定上屏多少。
    pub(crate) pending_stream: String,
    /// 打字机节拍器：按真实流逝时间把待揭示文本匀速释放到屏幕上。
    ///
    /// 与 `pending_stream` 是一对：缓冲解决"到达节奏 ≠ 屏幕节奏"，节拍器解决"每帧吐多少"。
    /// 只有它能让一次网络突发变成连续若干帧的小步吐字，而不是一帧蹦半屏。
    pub(crate) reveal_pacer: run::RevealPacer,
    /// 用户取消当前运行后立即关闭可见流，防止已排队的模型或工具增量继续刷新界面。
    pub(crate) run_stream_cancelled: bool,
    /// 当前模型是否处于思考阶段；仅控制状态文案，不保存推理摘要文本。
    pub(crate) model_is_thinking: bool,
    /// 独占 Input 视图可见期间使用的主事件区快照；隐藏后恢复实时状态。
    #[cfg(feature = "plugins")]
    pub(crate) main_event_snapshot: Option<MainEventSnapshot>,
    /// 手动滚动偏移；None 表示跟随底部自动滚动。
    pub(crate) scroll: Option<usize>,
    /// 上一帧计算出的最大滚动偏移，供滚动操作作为起点。
    pub(crate) last_max_scroll: usize,
    /// 最近一次渲染得到的消息区高度，用于整页滚动。
    pub(crate) last_viewport: u16,
    /// 是否处于按键诊断模式；开启后按键只回显不执行，`Esc` 退出。
    pub(crate) key_inspect: bool,
    /// 最近一帧"回到底部"提示所占的区域；未展示时为 `None`。
    ///
    /// 提示同时是可点击热区：手动翻阅后用鼠标即可回到底部，不必记快捷键。
    pub(crate) jump_hint_area: Option<Rect>,
    /// 最近一帧底栏上下文用量芯片所占的区域；没有用量数据时为 `None`。
    ///
    /// 芯片同时是 `/context` 的鼠标入口：点它直接展开当前会话上下文的分类明细。
    pub(crate) context_chip_area: Option<Rect>,
    /// 鼠标是否悬停在底栏上下文用量芯片上。
    ///
    /// 与滚动条悬停同源：只由鼠标移动更新、不消费事件，悬停时芯片提亮为强调色，
    /// 让「这个数字可以点」在键盘优先的界面里依然能被发现。
    pub(crate) is_context_chip_hovered: bool,
    /// 最近一帧滚动条所占的区域；内容未溢出时为 `None`。
    ///
    /// 该区域同时是可拖拽热区与悬停判定区：在滑块轨道上按下或拖动即可定位到对应位置。
    /// 几何只占贴边的一列，悬停仅提亮配色、不改变宽度，避免盖住正文最后一列。
    pub(crate) scrollbar_area: Option<Rect>,
    /// 拖动期间指针相对滑块顶端的偏移，单位是半行；未拖动时为 `None`。
    ///
    /// 按下滑块时记录它，后续拖动只按指针位移平移滑块，因此抓住滑块的任何位置都不会
    /// 在按下瞬间跳动。滑块几何由渲染函数与拖动共用，输入侧不额外保存屏幕坐标。
    pub(crate) scrollbar_grab_half: Option<i32>,
    /// 是否正在拖拽滚动条；拖拽期间只响应左键拖动与释放。
    pub(crate) scrollbar_drag: bool,
    /// 鼠标是否悬停在滚动条上；悬停时轨道与滑块各提亮一档。
    ///
    /// 只由鼠标移动事件在 [`App::handle_mouse_event`] 中更新：悬停是纯视觉状态，不消费
    /// 事件、也不改变滚动位置。滚动条不再绘制时由渲染层复位，避免高亮残留。
    pub(crate) is_scrollbar_hovered: bool,
    /// 最近一帧可见且具有详情的工具消息点击区域。
    pub(crate) tool_message_hit_regions: Vec<ToolMessageHitRegion>,
    /// 主事件区的消息布局缓存；只保留可见窗口内的渲染结果。
    pub(crate) transcript_layout: crate::ui::transcript_layout::TranscriptLayout,
    /// 左键按下时命中的工具消息；拖动或释放到其他区域时取消。
    pub(crate) tool_click_candidate: Option<usize>,
    /// 最近一次渲染得到的消息区宽度，用于工具消息 renderer 分配尺寸。
    #[cfg(feature = "plugins")]
    pub(crate) last_message_width: u16,
    /// 最近一次请求时当前会话占用的上下文 token 数（服务商实际输入）。
    pub(crate) context_tokens: Option<u64>,
    /// 当前会话占用的上下文的脱敏分类估算；切换会话后失效。
    pub(crate) context_snapshot: Option<crate::command::ContextSnapshot>,
    /// 本次打开当前会话期间累计的模型用量，切换或重载会话时清零。
    ///
    /// 与 `context_tokens` 的区别：后者是最近一轮的上下文规模，会随压缩回落；这里是累计
    /// 消耗，用于观察缓存命中率和总输出量。
    pub(crate) session_usage: TokenUsage,
    /// 配置的模型上下文窗口，用于状态栏计算占比。
    pub(crate) context_window: Option<u64>,
    /// models.dev 为当前模型声明的可选推理等级。
    pub(crate) reasoning_levels: Vec<ReasoningLevel>,
    /// 当前 Agent 使用的推理等级。
    pub(crate) reasoning_level: ReasoningLevel,
    /// models.dev 为当前模型声明的输入模态；`None` 表示快照未匹配，不限制旧行为。
    pub(crate) model_input_modalities: Option<Vec<crate::config::model_metadata::ModelModality>>,
    /// 配置文件声明的全部上游；`/model` 面板从中选择并热切换。
    pub(crate) model_upstreams: Vec<agent_core::config::ModelConfig>,
    /// 鼠标捕获是否开启；默认开启，由应用同时处理滚轮、文本拖选和插件点击。
    pub(crate) mouse_capture: bool,
    /// 当前是否使用全屏渲染（终端备用屏幕）。
    pub(crate) fullscreen: bool,
    /// 等待主循环应用的渲染模式切换；终端句柄由主循环持有，应用只登记意图。
    pub(crate) pending_fullscreen: Option<bool>,
    /// 等待主循环执行的挂起；raw mode 关闭了终端自身的 `SIGTSTP` 生成，必须先恢复终端再停止。
    pub(crate) pending_suspend: bool,
    /// 当前屏幕文本选择；鼠标释放只保留高亮，不主动写入系统剪贴板。
    ///
    /// 选区坐标以**内容行号**为基准，而不是屏幕行：滚轮滚动或拖到屏幕外自动滚动后，
    /// 高亮仍然贴在同一段文字上，也才能复制跨越一屏以上的内容。
    pub(crate) text_selection: Option<MouseTextSelection>,
    /// 左键释放时冻结的选区文本，供后续显式复制使用。
    pub(crate) text_selection_content: Option<String>,
    /// 消息内容区在屏幕上的矩形，用于在"内容行号"与"屏幕行"之间换算。
    ///
    /// 由 `ui::chat` 在每帧布局后写入；`None` 表示本帧没有可选择的正文区域。
    pub(crate) content_area: Option<Rect>,
    /// 上一帧消息区实际使用的滚动偏移（内容行），换算选区行号时使用。
    pub(crate) content_scroll: usize,
    /// 消息内容的逻辑总行数；滚出屏幕的历史同样在可选范围内。
    pub(crate) content_rows: usize,
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
    /// 插件通过可信来源发布的常驻底栏状态；同一来源的新状态覆盖旧状态。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_footer_statuses: BTreeMap<String, (String, Color)>,
    /// 主视图之上的插件子视图导航栈。
    #[cfg(feature = "plugins")]
    pub(crate) view_stack: ViewStack,
    /// 导航栈中宿主原生 Agent 会话视图的运行状态，按成员实例 ID 对齐。
    #[cfg(feature = "plugins")]
    pub(crate) agent_views: Vec<AgentSessionView>,
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

/// 内容行与屏幕列分离；逻辑行号可超过终端的 65535 行范围。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ContentPosition {
    x: u16,
    y: usize,
}
impl From<Position> for ContentPosition {
    fn from(position: Position) -> Self {
        Self {
            x: position.x,
            y: usize::from(position.y),
        }
    }
}

/// 应用内鼠标文本选择的**内容坐标**范围。
///
/// `y` 不是终端屏幕行，而是整段消息内容的逻辑行号（0 为内容顶部）：滚轮滚动只改变
/// 视口偏移，不会改变选区指向的文字，因此"滚动后再复制"能拿到正确内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MouseTextSelection {
    /// 拖动起点，对应内容坐标。
    anchor: ContentPosition,
    /// 最近一次拖动或释放位置。
    current: ContentPosition,
    /// 是否已经离开起点；普通点击不会触发复制或高亮。
    dragged: bool,
}

impl MouseTextSelection {
    /// 从鼠标按下位置创建尚未发生拖动的选择。
    pub(crate) fn new(anchor: impl Into<ContentPosition>) -> Self {
        let anchor = anchor.into();
        Self {
            anchor,
            current: anchor,
            dragged: false,
        }
    }

    /// 更新选择终点，并记录是否形成了非空拖动范围。
    pub(crate) fn update(&mut self, current: impl Into<ContentPosition>) {
        let current = current.into();
        self.current = current;
        self.dragged |= current != self.anchor;
    }

    /// 选区覆盖的内容行范围（已排序，含端点）。
    pub(crate) fn row_span(self) -> (usize, usize) {
        let (start, end) = if (self.anchor.y, self.anchor.x) <= (self.current.y, self.current.x) {
            (self.anchor, self.current)
        } else {
            (self.current, self.anchor)
        };
        (start.y, end.y)
    }

    /// 只遍历当前缓冲区窗口；`top` 指定它的首行对应的逻辑内容行。
    fn for_each_position(self, area: Rect, top: usize, mut visit: impl FnMut(Position)) {
        if area.is_empty() {
            return;
        }
        let (start, end) = if (self.anchor.y, self.anchor.x) <= (self.current.y, self.current.x) {
            (self.anchor, self.current)
        } else {
            (self.current, self.anchor)
        };
        let bottom = top.saturating_add(usize::from(area.height));
        for row in start.y.max(top)..end.y.saturating_add(1).min(bottom) {
            let first = if row == start.y {
                start.x.clamp(area.left(), area.right() - 1)
            } else {
                area.left()
            };
            let last = if row == end.y {
                end.x.clamp(area.left(), area.right() - 1)
            } else {
                area.right() - 1
            };
            for x in first..=last {
                visit(Position::new(
                    x,
                    area.y + u16::try_from(row - top).unwrap_or(0),
                ));
            }
        }
    }

    /// 从已渲染缓冲区提取选择文本；保留内部换行，并移除每行末尾的布局空白。
    ///
    /// 中文等宽字符的后续占位单元格不会重复写入返回值；越界端点按缓冲区范围收窄。
    pub(crate) fn extract(self, buffer: &Buffer) -> String {
        self.extract_window(buffer, usize::from(buffer.area.y), true)
    }

    /// 局部缓冲与逻辑坐标分离；分页提取时保留中间空行，最后一页才裁掉尾部空白行。
    fn extract_window(self, buffer: &Buffer, top: usize, trim_trailing: bool) -> String {
        let mut rows = Vec::<String>::new();
        let mut row = String::new();
        let mut previous_y = None;
        let mut trailing_columns = 0usize;
        self.for_each_position(buffer.area, top, |position| {
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
        while trim_trailing && rows.last().is_some_and(String::is_empty) {
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

/// 返回当前应显示在消息流里的消息切片，以及正在流式输出的消息索引。
///
/// 插件审批快照存在时，消息流切换到快照内容。渲染与文本选择必须看到同一份内容，
/// 否则选区行号会指向另一份列表。
///
/// 参数是**具体字段**而不是 `&App`：调用方通常还要同时可变借用
/// `app.transcript_layout`，若这里借用整个 `App`，借用检查会直接拒绝。
//
// 生命周期在插件构建下是必需的：`snapshot` 与 `messages` 是两个输入借用，返回值只借用
// 后者。纯 Core 构建里 `snapshot` 参数被 cfg 掉，只剩一个输入生命周期，clippy 因此认为
// 它可以省略。
#[allow(clippy::needless_lifetimes)]
pub(crate) fn timeline_messages<'a>(
    #[cfg(feature = "plugins")] snapshot: &'a Option<MainEventSnapshot>,
    messages: &'a [Msg],
    streaming: Option<usize>,
) -> (&'a [Msg], Option<usize>) {
    #[cfg(feature = "plugins")]
    if let Some(snapshot) = snapshot.as_ref() {
        return (snapshot.messages.as_slice(), snapshot.streaming_message);
    }
    (messages, streaming)
}

impl App {
    /// 创建空白会话，使首屏保持与参考界面一致的低干扰状态。
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>, model_name: String) -> Self {
        let session_record = SessionRecord::new(SessionId::generate(), Session::new())
            .expect("创建进程内默认会话记录");
        Self {
            workspace: WorkspaceContext::capture().expect("当前工作目录应可用"),
            read_path_grants: agent_tool::ReadPathGrants::new(),
            messages: Vec::new(),
            input: String::new(),
            async_attachments: false,
            attachment_sequence: 0,
            editor_generation: 0,
            attachment_job: None,
            mentioned_attachments_ready: false,
            attachments: Vec::new(),
            refs: Vec::new(),
            #[cfg(feature = "plugins")]
            context_ref_polled_at: None,
            dismissed_refs: std::collections::HashSet::new(),
            #[cfg(feature = "plugins")]
            context_ref_error: None,
            #[cfg(feature = "plugins")]
            context_ref_task: None,
            queued_inputs: VecDeque::new(),
            queued_run_active: false,
            queued_input_areas: Vec::new(),
            queue_focus: None,
            cursor: 0,
            input_history: Vec::new(),
            input_history_cursor: None,
            running: false,
            native_command: NativeCommandState::default(),
            pending_reload: None,
            run_started_at: None,
            last_escape_at: None,
            last_exit_request_at: None,
            should_quit: false,
            session_record,
            session_store: Arc::new(MemorySessionStore::new()),
            title_generator: None,
            title_attempted: None,
            pending_session_title: None,
            context_loader: Arc::new(agent_core::PassthroughContextLoader),
            native_context_loader: None,
            #[cfg(feature = "plugins")]
            plugin_management: None,
            tx,
            model_name,
            model_upstream_name: None,
            config_path: None,
            spinner_frame: 0,
            streaming_message: None,
            pending_stream: String::new(),
            reveal_pacer: run::RevealPacer::default(),
            run_stream_cancelled: false,
            model_is_thinking: false,
            #[cfg(feature = "plugins")]
            main_event_snapshot: None,
            scroll: None,
            last_max_scroll: 0,
            last_viewport: 0,
            key_inspect: false,
            jump_hint_area: None,
            context_chip_area: None,
            is_context_chip_hovered: false,
            scrollbar_area: None,
            scrollbar_grab_half: None,
            scrollbar_drag: false,
            is_scrollbar_hovered: false,
            tool_message_hit_regions: Vec::new(),
            transcript_layout: crate::ui::transcript_layout::TranscriptLayout::default(),
            tool_click_candidate: None,
            #[cfg(feature = "plugins")]
            last_message_width: 0,
            context_tokens: None,
            context_snapshot: None,
            session_usage: TokenUsage::default(),
            context_window: None,
            reasoning_levels: vec![ReasoningLevel::Off],
            reasoning_level: ReasoningLevel::Off,
            model_input_modalities: None,
            model_upstreams: Vec::new(),
            mouse_capture: true,
            fullscreen: true,
            pending_fullscreen: None,
            pending_suspend: false,
            text_selection: None,
            text_selection_content: None,
            content_area: None,
            content_scroll: 0,
            content_rows: 0,
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
            plugin_footer_statuses: BTreeMap::new(),
            #[cfg(feature = "plugins")]
            view_stack: ViewStack::default(),
            #[cfg(feature = "plugins")]
            agent_views: Vec::new(),
            #[cfg(feature = "plugins")]
            sessions_query_task: None,
            #[cfg(feature = "plugins")]
            applied_host_actions: VecDeque::new(),
        }
    }

    /// 判断一次 UI tick 是否需要重绘。
    ///
    /// tick 只推进动画帧、插件状态倒计时和插件视图刷新；空闲时这些都不会改变可见内容，
    /// 因此主循环可以跳过绘制，避免空转占用 CPU。运行中、后台重载和插件加载期间的
    /// 旋转指示与耗时文本仍需逐帧刷新。
    pub(crate) fn tick_requires_redraw(&self) -> bool {
        #[cfg(feature = "plugins")]
        let plugin_activity = self.plugins_loading || self.plugin_status_ticks > 0;
        #[cfg(not(feature = "plugins"))]
        let plugin_activity = false;
        // 原生会话视图运行中时旋转指示与耗时每秒都在变化，必须继续出帧。
        #[cfg(feature = "plugins")]
        let agent_view_activity = self.active_agent_session_view().is_some_and(|view| {
            !view.closed
                && matches!(
                    view.status,
                    Some(agent_runtime::AgentStatus::Queued | agent_runtime::AgentStatus::Running)
                )
        });
        #[cfg(not(feature = "plugins"))]
        let agent_view_activity = false;
        // 有待揭示文本时必须继续出帧，否则打字机会停在当前字符上等下一个网络事件。
        self.running
            || self.attachment_job.is_some()
            || self.has_pending_stream()
            || self.pending_reload.is_some()
            || plugin_activity
            || agent_view_activity
    }

    /// 注入启动时读取的渲染模式；缺省为全屏。
    pub(crate) fn with_fullscreen(mut self, fullscreen: bool) -> Self {
        self.fullscreen = fullscreen;
        self
    }

    /// 注入主函数启动时捕获的项目上下文。
    pub(crate) fn with_workspace(mut self, workspace: WorkspaceContext) -> Self {
        self.workspace = workspace;
        self
    }

    /// 注入与只读工具共享的用户路径授权集合。
    pub(crate) fn with_read_path_grants(mut self, grants: agent_tool::ReadPathGrants) -> Self {
        self.read_path_grants = grants;
        self
    }

    /// 注入本次启动实际读取的配置文件，供交互设置执行原子持久化。
    pub(crate) fn with_config_path(mut self, config_path: PathBuf) -> Self {
        self.config_path = Some(config_path);
        self
    }

    /// 注入会话标题生成器；未注入时（demo 模式、测试）保留首条消息兜底标题。
    pub(crate) fn with_title_generator(
        mut self,
        generator: Arc<crate::session::title::SessionTitleGenerator>,
    ) -> Self {
        self.title_generator = Some(generator);
        self
    }

    /// 注入状态栏使用的模型上下文窗口；压缩器在启动时同步接收同一窗口。
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

    /// 判断某个配置条目是不是当前正在使用的上游。
    ///
    /// 同一个上游名可以挂多个模型，因此「当前」必须同时比对模型 ID 与上游名：只看模型 ID
    /// 会把不同上游下相同模型的条目一起标成当前项。上游名未知时（演示模式）退化为只比
    /// 模型 ID。
    pub(crate) fn is_current_upstream(&self, entry: &agent_core::config::ModelConfig) -> bool {
        entry.model == self.model_name
            && self
                .model_upstream_name
                .as_deref()
                .is_none_or(|name| name == entry.name)
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

    /// 注入与 Agent 共用的原生上下文加载器，并保留模型热切换控制句柄。
    pub(crate) fn with_native_context_loader(
        mut self,
        context_loader: Arc<NativeContextLoader>,
    ) -> Self {
        self.context_loader = context_loader.clone();
        self.native_context_loader = Some(context_loader);
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
        // 压缩等操作会用同一会话的新记录替换内存状态，此时路径授权继续有效。
        let session_changed = self.session_record.id != session_record.id;
        self.invalidate_attachment_job();
        self.messages = restore_session_messages(&session_record.session);
        self.session_record = session_record;
        self.input.clear();
        self.attachments.clear();
        if session_changed {
            self.read_path_grants.clear();
        }
        self.cursor = 0;
        self.queued_inputs.clear();
        self.queued_input_areas.clear();
        self.queue_focus = None;
        self.streaming_message = None;
        self.run_stream_cancelled = false;
        self.model_is_thinking = false;
        #[cfg(feature = "plugins")]
        {
            self.main_event_snapshot = None;
        }
        self.scroll = None;
        self.tool_message_hit_regions.clear();
        self.tool_click_candidate = None;
        self.context_tokens = None;
        self.context_snapshot = None;
        self.session_usage = TokenUsage::default();
        self.native_command.context_open = false;
        self.last_escape_at = None;
        self.native_command.session_palette = None;
        self.native_command.reasoning_panel = None;
        // 标题属于旧会话：切换后到达的结果会被 ID 校验丢弃，暂存内容也必须一并清掉，
        // 否则下一个会话可能继承上一个会话的名字。
        self.pending_session_title = None;
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
