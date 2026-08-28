//! TUI App 状态机、运行队列、插件视图状态和事件 sink。

use super::*;

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
    /// 当前进程固定的 Session Genome 行为绑定；独立于 Evidence 隐私开关。
    pub(crate) genome_runtime: GenomeSessionRuntime,
    /// 可选 Evidence Plane；启用后每次主会话运行必须形成 Episode。
    pub(crate) evidence: Option<EvidenceRuntime>,
    pub(crate) tx: mpsc::UnboundedSender<UiEvent>,
    pub(crate) model_name: String,
    pub(crate) spinner_frame: usize,
    /// 当前正在接收增量文本的助手消息索引。
    pub(crate) streaming_message: Option<usize>,
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
    /// 鼠标捕获是否开启；默认关闭以允许终端原生选择复制，用户可用 Ctrl+T 临时开启。
    pub(crate) mouse_capture: bool,
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
            #[cfg(not(test))]
            genome_runtime: GenomeSessionRuntime::default(),
            #[cfg(test)]
            genome_runtime: GenomeSessionRuntime::TestOnly,
            evidence: None,
            tx,
            model_name,
            spinner_frame: 0,
            streaming_message: None,
            scroll: None,
            last_max_scroll: 0,
            last_viewport: 0,
            #[cfg(feature = "plugins")]
            last_message_width: 0,
            context_tokens: None,
            context_window: None,
            mouse_capture: false,
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

    /// 注入模型上下文窗口；该值只影响状态栏展示。
    pub(crate) fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        self.context_window = context_window;
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

    /// 注入启动时按 Session 精确解析的 Genome 行为运行时。
    pub(crate) fn with_genome_runtime(mut self, genome_runtime: GenomeSessionRuntime) -> Self {
        self.genome_runtime = genome_runtime;
        self
    }

    /// 注入启动时已经验证 Genome Revision 的 Evidence Plane。
    pub(crate) fn with_evidence(mut self, evidence: Option<EvidenceRuntime>) -> Self {
        self.evidence = evidence;
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
        self.scroll = None;
        self.context_tokens = None;
        self.last_escape_at = None;
        self.native_command.dialog = None;
        self.native_command.dismissed_input = None;
        if let Some(notice) = notice {
            self.messages.push(Msg::new(MsgKind::Info, notice));
        }
    }

    /// 进入当前项目下尚未持久化的全新空白草稿。
    pub(crate) fn start_new_draft(&mut self, notice: &str) -> Result<()> {
        let mut draft = self.workspace.draft_record()?;
        self.genome_runtime.bind_or_validate_session(&mut draft)?;
        self.replace_session(draft, Some(notice));
        Ok(())
    }

    /// 处理后台会话上下文重载结果：替换会话时保留正在编辑的输入。
    ///
    /// 处理说明由原生加载器发布的展示事件提供，这里只负责会话状态切换。
    pub(crate) fn handle_session_context_reloaded(&mut self, result: Result<SessionReloadOutcome>) {
        self.pending_reload = None;
        match result {
            Ok(SessionReloadOutcome::Replaced(saved)) => {
                // 重载在后台完成，用户可能仍在编辑输入，替换会话后原样恢复。
                let input = std::mem::take(&mut self.input);
                let cursor = self.cursor;
                let attachments = std::mem::take(&mut self.attachments);
                self.replace_session(saved, None);
                self.input = input;
                self.cursor = cursor;
                self.attachments = attachments;
            }
            Ok(SessionReloadOutcome::Unchanged) => {}
            Err(error) => self.messages.push(Msg::new(
                MsgKind::Error,
                format!("重新加载会话上下文失败：{error}"),
            )),
        }
    }

    /// 追加一个刚 Ready 插件的视图声明，不重置其他插件的帧、焦点或导航栈。
    #[cfg(feature = "plugins")]
    pub(crate) fn add_plugin_views(&mut self, declarations: Vec<UiDeclaration>) {
        self.plugin_views
            .extend(declarations.into_iter().map(|declaration| PluginViewState {
                declaration,
                frame: None,
                area: Rect::default(),
            }));
    }

    /// 为指定工具消息请求一次通用 Host 渲染；没有贡献时保持默认样式。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_tool_render(&mut self, host: Arc<LivePluginHost>, call_id: &str) {
        let width = self.last_message_width.max(1);
        let max_height = self.last_viewport.max(1);
        let Some(index) = self
            .messages
            .iter()
            .position(|message| message.tool_call_id() == Some(call_id))
        else {
            return;
        };
        let message = &self.messages[index];
        let Some(call) = message.tool_call.clone() else {
            return;
        };
        let state = match message.kind {
            MsgKind::ToolRunning => ToolRenderState::Running,
            MsgKind::ToolOk | MsgKind::ToolError => {
                let Some(result) = message.tool_result.clone() else {
                    return;
                };
                ToolRenderState::Finished { result }
            }
            MsgKind::ToolSkipped => ToolRenderState::Skipped {
                reason: message.skip_reason.clone().unwrap_or_default(),
            },
            _ => return,
        };
        self.plugin_frame = self.plugin_frame.wrapping_add(1);
        let context = ToolRenderContext {
            call,
            state,
            width,
            max_height,
            frame: self.plugin_frame,
        };
        let revision = self.messages[index].tool_render_revision.wrapping_add(1);
        self.messages[index].tool_render_revision = revision;
        self.messages[index].tool_render_pending_width = Some(width);
        let tx = self.tx.clone();
        let call_id = call_id.to_string();
        tokio::spawn(async move {
            let result = host.render_tool_message(&context).await;
            let _ = tx.send(UiEvent::ToolFrameLoaded {
                call_id,
                revision,
                width,
                result,
            });
        });
    }

    /// 对没有当前宽度帧的工具消息发起渲染，用于插件就绪和终端尺寸变化。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_stale_tool_renders(&mut self, host: Arc<LivePluginHost>) {
        let width = self.last_message_width.max(1);
        let call_ids = self
            .messages
            .iter()
            .filter(|message| {
                message.tool_call.is_some()
                    && message.tool_render_pending_width != Some(width)
                    && message.tool_frame_width != Some(width)
            })
            .filter_map(|message| message.tool_call_id().map(str::to_string))
            .collect::<Vec<_>>();
        for call_id in call_ids {
            self.schedule_tool_render(Arc::clone(&host), &call_id);
        }
    }

    /// 清除 Host 路由变化前的工具帧，并使并发中的旧渲染结果失效。
    #[cfg(feature = "plugins")]
    pub(crate) fn invalidate_tool_frames(&mut self) {
        for message in self
            .messages
            .iter_mut()
            .filter(|message| message.tool_call.is_some())
        {
            message.tool_frame = None;
            message.tool_frame_width = None;
            message.tool_render_pending_width = None;
            message.tool_render_revision = message.tool_render_revision.wrapping_add(1);
        }
    }

    /// 提交一次工具消息 renderer 结果；失败时保留默认工具展示。
    #[cfg(feature = "plugins")]
    pub(crate) fn apply_tool_frame(
        &mut self,
        call_id: &str,
        revision: u64,
        width: u16,
        result: Result<Option<PluginUiFrame>>,
    ) {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.tool_call_id() == Some(call_id))
        else {
            return;
        };
        if message.tool_render_revision != revision {
            return;
        }
        message.tool_render_pending_width = None;
        message.tool_frame_width = Some(width);
        match result {
            Ok(Some(frame)) => message.tool_frame = Some(frame),
            Ok(None) => message.tool_frame = None,
            Err(error) => {
                message.tool_frame = None;
                self.plugin_failures.push(format!(
                    "Failed to render tool message `{call_id}`: {error}"
                ));
            }
        }
    }

    /// 在应用导航栈上执行一次经 Host 标记来源的插件视图请求。
    #[cfg(feature = "plugins")]
    pub(crate) fn apply_view_navigation(
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

    /// Marks plugin IDs as loading while keeping the input queue available.
    ///
    /// 标记正在加载的插件 ID，同时保持输入队列可用。
    #[cfg(feature = "plugins")]
    pub(crate) fn with_loading_plugins(mut self, plugin_ids: Vec<String>) -> Self {
        self.plugin_ids.clear();
        self.plugin_loading_ids = plugin_ids;
        self.plugins_loading = true;
        self.plugin_load_error = None;
        self.plugin_failures.clear();
        self
    }

    /// 记录一个渐进加载完成的插件，使其立即进入 Ready 计数并移出加载列表。
    #[cfg(feature = "plugins")]
    pub(crate) fn mark_plugin_ready(
        &mut self,
        plugin_id: String,
        events: &[Value],
        load_duration_ms: u64,
    ) {
        self.plugin_loading_ids.retain(|id| id != &plugin_id);
        if !self.plugin_ids.contains(&plugin_id) {
            self.plugin_ids.push(plugin_id.clone());
        }
        let mut details = plugin_startup_details(std::slice::from_ref(&plugin_id), events);
        if load_duration_ms > 0 {
            for detail in &mut details {
                detail.push_str(&format!(" · {load_duration_ms} ms"));
            }
        }
        self.plugin_startup_details.extend(details);
    }

    /// 记录一个渐进加载失败的插件并立即从加载列表移除。
    #[cfg(feature = "plugins")]
    pub(crate) fn mark_plugin_failed(&mut self, failure: PluginLoadFailure) {
        self.plugin_loading_ids
            .retain(|id| id != &failure.plugin_id);
        let blocked = if failure.blocked_by.is_empty() {
            String::new()
        } else {
            format!("; depends on {}", failure.blocked_by.join(", "))
        };
        let detail = format!(
            "{}: load failed{blocked} · {}",
            failure.plugin_id, failure.reason
        );
        self.plugin_failures.push(detail.clone());
        self.plugin_startup_details.push(detail);
    }

    /// 启动或合并一次后台插件视图刷新，保证任一时刻至多存在一个渲染批次。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_plugin_views_refresh(&mut self, host: Arc<LivePluginHost>) {
        if self.plugin_refresh_task.is_some() {
            self.plugin_refresh_pending = true;
            return;
        }
        let requests = self.periodic_plugin_render_requests();
        if requests.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        self.plugin_refresh_task = Some(tokio::spawn(async move {
            let rendered = render_plugin_views(host, requests).await;
            let _ = tx.send(UiEvent::PluginFramesLoaded(rendered));
        }));
    }

    /// 结束渐进加载并保留已经逐项收集的成功详情与失败信息。
    #[cfg(feature = "plugins")]
    pub(crate) fn finish_progressive_plugin_loading(&mut self) {
        self.plugin_loading_ids.clear();
        self.plugins_loading = false;
        self.plugin_load_error = None;
        self.plugin_status_ticks = PLUGIN_STATUS_DETAIL_TICKS;
    }

    /// 记录渐进加载的全局规划错误，保留此前已经 Ready 的插件。
    #[cfg(feature = "plugins")]
    pub(crate) fn set_progressive_plugin_load_error(&mut self, error: &anyhow::Error) {
        self.plugins_loading = false;
        self.plugin_loading_ids.clear();
        self.plugin_status_ticks = 0;
        self.plugin_load_error = Some(error.to_string());
    }

    /// Advances the transient startup status toward the compact counter.
    ///
    /// 推进一次性启动状态，并在计时结束后切换为紧凑计数。
    #[cfg(feature = "plugins")]
    pub(crate) fn tick_plugin_status(&mut self) {
        if !self.plugins_loading {
            self.plugin_status_ticks = self.plugin_status_ticks.saturating_sub(1);
        }
    }

    /// Returns the current plugin status icon and text for the footer's right side.
    ///
    /// 返回底部信息栏右侧当前使用的插件状态图标和文本。
    #[cfg(feature = "plugins")]
    pub(crate) fn plugin_status_content(&self) -> (&'static str, String) {
        if self.plugins_loading {
            let plugins = self.plugin_loading_ids.join(" · ");
            let ready = if self.plugin_ids.is_empty() {
                String::new()
            } else {
                format!(" · {} ready", self.plugin_ids.len())
            };
            let queue = if self.queued_inputs.is_empty() {
                String::new()
            } else {
                format!(" · queued {}", self.queued_inputs.len())
            };
            let text = if plugins.is_empty() {
                format!("Loading plugins{ready}{queue}")
            } else {
                format!("Loading plugins · {plugins}{ready}{queue}")
            };
            return (SPINNER[self.spinner_frame % SPINNER.len()], text);
        }
        if let Some(error) = &self.plugin_load_error {
            return ("✗", format!("Plugin loading failed · {error}"));
        }
        if self.plugin_status_ticks > 0 {
            let details = if self.plugin_startup_details.is_empty() {
                self.plugin_ids.join(" · ")
            } else {
                self.plugin_startup_details.join(" · ")
            };
            let text = if details.is_empty() {
                "No plugins loaded".to_string()
            } else if self.plugin_failures.is_empty() {
                format!("Plugins loaded · {details}")
            } else {
                format!("Plugins partially loaded · {details}")
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
    pub(crate) fn plugin_status_color(&self) -> Color {
        if self.plugin_load_error.is_some() {
            COLOR_DANGER
        } else if self.plugins_loading || !self.plugin_failures.is_empty() {
            COLOR_WARNING
        } else {
            COLOR_SUCCESS
        }
    }

    /// 判断 Evidence Genome 的固定插件组合是否已经完整 Ready。
    ///
    /// 普通模式保持渐进加载期间可提交输入的既有体验；Evidence 模式必须等待全部插件
    /// 完成，且任一加载失败都会阻止 Run，避免 Episode 绑定的 Genome 与真实组合不同。
    #[cfg(feature = "plugins")]
    pub(crate) fn evidence_genome_run_is_ready(&self) -> bool {
        self.evidence.is_none()
            || (!self.plugins_loading
                && self.plugin_load_error.is_none()
                && self.plugin_failures.is_empty())
    }

    /// 返回主输入当前激活的触发视图索引。
    ///
    /// 主输入去除前导空白后以某个视图声明的触发前缀开头即视为激活；
    /// 多个视图声明重叠前缀时按声明顺序取第一个。
    #[cfg(feature = "plugins")]
    pub(crate) fn active_trigger_view(&self) -> Option<usize> {
        // `/` 由默认原生命令独占，插件不能通过同名触发前缀覆盖命令状态机。
        if self.input.starts_with('/') {
            return None;
        }
        let input = self.input.trim_start();
        if input.is_empty() {
            return None;
        }
        self.plugin_views.iter().position(|view| {
            view.declaration
                .input_triggers
                .iter()
                .any(|trigger| !trigger.is_empty() && input.starts_with(trigger.as_str()))
        })
    }

    /// 返回当前应显示在输入区上方的输入面板视图索引。
    #[cfg(feature = "plugins")]
    pub(crate) fn visible_input_panel(&self) -> Option<usize> {
        let index = self.active_trigger_view()?;
        let view = &self.plugin_views[index];
        (view.declaration.placement == UiPlacement::InputPanel && plugin_view_visible(view))
            .then_some(index)
    }

    /// 返回当前独占主输入区的最后一个可见插件视图。
    #[cfg(feature = "plugins")]
    pub(crate) fn visible_plugin_input(&self) -> Option<usize> {
        self.plugin_views
            .iter()
            .enumerate()
            .rev()
            .find(|(_, view)| {
                view.declaration.placement == UiPlacement::Input && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
    }

    /// 返回输入框上方当前参与布局的视图，并落实独占输入、触发面板和常驻架优先级。
    #[cfg(feature = "plugins")]
    pub(crate) fn visible_composer_panels(&self) -> Vec<usize> {
        if self.visible_plugin_input().is_some() {
            return Vec::new();
        }
        if self.native_command_panel_height() > 0 {
            return Vec::new();
        }
        if let Some(index) = self.visible_input_panel() {
            return vec![index];
        }
        self.plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                view.declaration.placement == UiPlacement::ComposerShelf
                    && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// 计算当前输入上方视图所需总高度；每个视图包含一行宿主分隔线。
    #[cfg(feature = "plugins")]
    pub(crate) fn composer_panel_height(&self) -> u16 {
        self.visible_composer_panels()
            .into_iter()
            .fold(0u16, |height, index| {
                height.saturating_add(self.composer_panel_height_at(index))
            })
    }

    /// 返回单个输入上方视图受声明上限约束后的布局高度。
    #[cfg(feature = "plugins")]
    pub(crate) fn composer_panel_height_at(&self, index: usize) -> u16 {
        let view = &self.plugin_views[index];
        let lines = view
            .frame
            .as_ref()
            .map(|frame| frame.lines.len())
            .unwrap_or(0);
        if lines == 0 {
            return 0;
        }
        let max = view
            .declaration
            .size
            .height
            .unwrap_or_else(|| default_plugin_height(view.declaration.placement));
        u16::try_from(lines)
            .unwrap_or(max)
            .saturating_add(1)
            .min(max)
    }

    /// 构造发送给触发视图的主输入快照事件。
    #[cfg(feature = "plugins")]
    pub(crate) fn main_input_snapshot(&self, index: usize) -> UiInput {
        let declaration = &self.plugin_views[index].declaration;
        UiInput {
            plugin_id: declaration.plugin_id.clone(),
            view_id: declaration.view_id.clone(),
            instance_id: None,
            event: UiInputEvent::MainInput {
                text: self.input.clone(),
                cursor: u32::try_from(self.cursor).unwrap_or(u32::MAX),
            },
        }
    }

    /// 记录一条宿主动作请求；返回 `false` 表示重复交付应被忽略。
    #[cfg(feature = "plugins")]
    pub(crate) fn mark_host_action(&mut self, plugin_id: &str, request_id: &str) -> bool {
        let key = (plugin_id.to_string(), request_id.to_string());
        if self.applied_host_actions.contains(&key) {
            return false;
        }
        if self.applied_host_actions.len() >= 64 {
            self.applied_host_actions.pop_front();
        }
        self.applied_host_actions.push_back(key);
        true
    }

    /// 按插件请求替换主输入内容与光标，并保持附件引用一致。
    #[cfg(feature = "plugins")]
    pub(crate) fn set_main_input(&mut self, text: String, cursor: Option<u32>) {
        self.input = text;
        let cursor = cursor
            .and_then(|cursor| usize::try_from(cursor).ok())
            .unwrap_or(self.input.len())
            .min(self.input.len());
        // 光标落在字符中间时回退到最近的合法边界。
        self.cursor = (0..=cursor)
            .rev()
            .find(|offset| self.input.is_char_boundary(*offset))
            .unwrap_or(0);
        self.input_history_cursor = None;
        self.last_escape_at = None;
        self.prune_attachments();
    }

    /// 启动可取消的后台会话摘要查询，结果经 UI 事件回送发起插件。
    #[cfg(feature = "plugins")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_sessions_query(
        &mut self,
        plugin_id: String,
        reply_service: String,
        query_id: u64,
        query: String,
        cursor: Option<String>,
        limit: u16,
    ) {
        if let Some(task) = self.sessions_query_task.take() {
            task.abort();
        }
        let session_store = Arc::clone(&self.session_store);
        let active_session_id = self.session_record.id.clone();
        let tx = self.tx.clone();
        self.sessions_query_task = Some(tokio::spawn(async move {
            // 搜索输入短暂防抖，连续键入只扫描一次项目会话目录。
            if cursor.is_none() && !query.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            }
            let status = sessions_page(
                session_store.as_ref(),
                &active_session_id,
                &query,
                cursor.as_deref(),
                limit,
            )
            .await;
            let _ = tx.send(UiEvent::SessionsQueryDone {
                plugin_id,
                reply_service,
                reply: Box::new(UiSessionsReply { query_id, status }),
            });
        }));
    }

    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) {
        if !matches!(code, KeyCode::Esc) {
            self.last_escape_at = None;
        }
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
        // Ctrl+P/Ctrl+N 作为方向键历史回溯的兼容入口。
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('p')) {
            // 仅空输入或已处于回溯态时生效，避免覆盖未发送的内容。
            if self.input.is_empty() || self.input_history_cursor.is_some() {
                self.recall_older_input();
            }
            return;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('n')) {
            if self.input_history_cursor.is_some() {
                self.recall_newer_input();
            }
            return;
        }
        // 多行编辑键优先于命令预览，避免补全弹层吞掉换行手势。
        // Shift+Enter 依赖键盘增强协议；Alt+Enter 与 Ctrl+J 在传统终端也可用。
        if (matches!(code, KeyCode::Enter)
            && modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT))
            || (modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('j')))
        {
            self.insert_input_text("\n");
            return;
        }

        match code {
            KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                let start = previous_word_boundary(&self.input, self.cursor);
                if start < self.cursor {
                    self.input.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.finish_input_edit();
                }
            }
            KeyCode::End if modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll = None;
            }
            KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
                self.cursor = previous_word_boundary(&self.input, self.cursor);
            }
            KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
                self.cursor = next_word_boundary(&self.input, self.cursor);
            }
            KeyCode::Enter => {
                // 后台命令可能随时替换会话记录，期间禁止并发提交新消息。
                if self.pending_reload.is_some() {
                    self.messages
                        .push(Msg::new(MsgKind::Info, "上下文压缩仍在进行中，请稍候"));
                    return;
                }
                #[cfg(feature = "plugins")]
                if !self.evidence_genome_run_is_ready() {
                    self.messages.push(Msg::new(
                        MsgKind::Info,
                        "Evidence Genome 的插件组合尚未完整就绪，当前不能开始 Run。".to_string(),
                    ));
                    return;
                }
                if let Some(agent) = agent {
                    if self.running {
                        // steering 只支持纯文本，附件必须等当前回合结束后随消息发送。
                        if self.attachments.is_empty() {
                            self.submit_steering(agent);
                        } else {
                            self.messages.push(Msg::new(
                                MsgKind::Info,
                                "Attachments cannot be sent during a run. Wait for it to finish.",
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
                // 运行中优先中断；空闲输入必须连续双按才清空，Esc 永不直接退出。
                if self.running {
                    self.last_escape_at = None;
                    if let Some(agent) = agent {
                        agent.cancel();
                        self.messages
                            .push(Msg::new(MsgKind::Info, "Cancelling the current run..."));
                    }
                } else if self.input.is_empty() && self.attachments.is_empty() {
                    self.last_escape_at = None;
                } else {
                    let now = std::time::Instant::now();
                    let double_pressed = self.last_escape_at.take().is_some_and(|previous| {
                        now.saturating_duration_since(previous) <= ESC_DOUBLE_PRESS_WINDOW
                    });
                    if double_pressed {
                        self.input.clear();
                        self.cursor = 0;
                        self.finish_input_edit();
                    } else {
                        self.last_escape_at = Some(now);
                    }
                }
            }
            KeyCode::PageUp => self.scroll_up(self.last_viewport.saturating_sub(1).max(1)),
            KeyCode::PageDown => self.scroll_down(self.last_viewport.saturating_sub(1).max(1)),
            KeyCode::Up => {
                if self.input_history_cursor.is_some() {
                    self.recall_older_input();
                } else if !(self.input.contains('\n')
                    && move_cursor_vertically(&self.input, &mut self.cursor, true))
                {
                    // 空输入框直接回填最近一次提交；非空单行草稿不得被历史覆盖。
                    if self.input.is_empty() {
                        self.recall_older_input();
                    }
                }
            }
            KeyCode::Down => {
                if self.input_history_cursor.is_some() {
                    self.recall_newer_input();
                } else if !(self.input.contains('\n')
                    && move_cursor_vertically(&self.input, &mut self.cursor, false))
                {
                    // 未处于历史回溯态时向下键不修改当前草稿。
                }
            }
            KeyCode::Char(c) => {
                let mut encoded = [0; 4];
                self.insert_input_text(c.encode_utf8(&mut encoded));
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
                self.finish_input_edit();
            }
            KeyCode::Delete => {
                if let Some(next) = self.input[self.cursor..].chars().next() {
                    let end = self.cursor + next.len_utf8();
                    self.input.replace_range(self.cursor..end, "");
                    self.finish_input_edit();
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
            KeyCode::Home => {
                // 多行输入时回到当前逻辑行行首；单行输入等价于整段开头。
                self.cursor = self.input[..self.cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
            }
            KeyCode::End => {
                // 多行输入时到当前逻辑行行尾；单行输入等价于整段末尾。
                self.cursor = self.input[self.cursor..]
                    .find('\n')
                    .map_or(self.input.len(), |index| self.cursor + index);
            }
            _ => {}
        }
    }

    /// 根据模态层和 Tab 焦点决定键盘事件的接收者。
    #[cfg(feature = "plugins")]
    pub(crate) fn route_plugin_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> PluginKeyRoute {
        if !matches!(code, KeyCode::Esc) {
            self.last_escape_at = None;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return PluginKeyRoute::Main;
        }

        if self.view_stack.active().is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
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
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        // 触发前缀激活时，无修饰的补全与提交手势交给触发视图处理；
        // 方向键和 Esc 仅在输入面板可见时转发，避免面板隐藏后劫持滚动。
        if let Some(index) = self.active_trigger_view() {
            if modifiers.is_empty() {
                let panel_visible = self.visible_input_panel() == Some(index);
                let forward = match code {
                    KeyCode::Tab | KeyCode::Enter => true,
                    KeyCode::Up | KeyCode::Down | KeyCode::Esc => panel_visible,
                    _ => false,
                };
                if forward {
                    if matches!(code, KeyCode::Esc) {
                        self.last_escape_at = None;
                    }
                    return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
                }
            }
        }

        if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
            let reverse =
                matches!(code, KeyCode::BackTab) || modifiers.contains(KeyModifiers::SHIFT);
            self.cycle_plugin_focus(reverse);
            return PluginKeyRoute::Consumed;
        }

        if let Some(index) = self.plugin_focus {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
                self.plugin_focus = None;
                return PluginKeyRoute::Consumed;
            }
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        PluginKeyRoute::Main
    }

    /// 将插件内容区内的鼠标事件转换为相对坐标，并在点击插件外区域时恢复主输入焦点。
    #[cfg(feature = "plugins")]
    pub(crate) fn route_plugin_mouse(&mut self, mouse: &MouseEvent) -> Option<UiInput> {
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
                    plugin_view_visible(view)
                        && point_in_rect(mouse.column, mouse.row, plugin_view_hit_area(view))
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
        if !point_in_rect(mouse.column, mouse.row, plugin_view_hit_area(view)) {
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
    pub(crate) fn plugin_key_input(
        &self,
        index: usize,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> UiInput {
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
    pub(crate) fn cycle_plugin_focus(&mut self, reverse: bool) {
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
    pub(crate) fn active_dialog_index(&self) -> Option<usize> {
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
    pub(crate) fn plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
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

    /// 构造周期刷新请求，并跳过触发前缀未激活的输入面板。
    ///
    /// 输入面板只随主输入快照与手势的定向刷新更新，周期渲染它没有意义。
    #[cfg(feature = "plugins")]
    pub(crate) fn periodic_plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
        let inactive_panels = self
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(index, view)| {
                view.declaration.placement == UiPlacement::InputPanel
                    && self.active_trigger_view() != Some(*index)
            })
            .map(|(_, view)| {
                (
                    view.declaration.plugin_id.clone(),
                    view.declaration.view_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut requests = self.plugin_render_requests();
        requests.retain(|request| {
            !inactive_panels.iter().any(|(plugin_id, view_id)| {
                &request.plugin_id == plugin_id && &request.view_id == view_id
            })
        });
        requests
    }

    /// 用插件返回的新帧更新对应视图缓存。
    #[cfg(feature = "plugins")]
    pub(crate) fn update_plugin_frame(
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
    pub(crate) fn set_plugin_ui_error(
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
                        text: format!("Plugin UI error: {error:#}"),
                        style: UiStyle {
                            foreground: Some(UiColor::Red),
                            ..UiStyle::default()
                        },
                    }],
                }],
                cursor: None,
            },
        );
    }

    /// 向上滚动 n 行，进入手动滚动模式。
    pub(crate) fn scroll_up(&mut self, n: u16) {
        let current = self.scroll.unwrap_or(self.last_max_scroll);
        self.scroll = Some(current.saturating_sub(n));
    }

    /// 向下滚动 n 行；到达底部时恢复自动跟随。
    pub(crate) fn scroll_down(&mut self, n: u16) {
        if let Some(current) = self.scroll {
            let next = current.saturating_add(n);
            if next >= self.last_max_scroll {
                self.scroll = None;
            } else {
                self.scroll = Some(next);
            }
        }
    }

    /// 在当前光标处插入文本，并退出历史回溯态、同步附件状态。
    fn insert_input_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.finish_input_edit();
    }

    /// 完成一次输入内容修改后的统一清理。
    fn finish_input_edit(&mut self) {
        self.input_history_cursor = None;
        self.last_escape_at = None;
        self.prune_attachments();
    }

    /// 召回更早的一条输入；空历史保持当前编辑器不变。
    fn recall_older_input(&mut self) {
        let Some(index) = self.input_history_cursor.map_or_else(
            || self.input_history.len().checked_sub(1),
            |current| current.checked_sub(1),
        ) else {
            return;
        };
        self.input_history_cursor = Some(index);
        self.input.clone_from(&self.input_history[index]);
        self.cursor = self.input.len();
        self.prune_attachments();
    }

    /// 向较新的输入回溯；越过最新记录时恢复空编辑器。
    fn recall_newer_input(&mut self) {
        let Some(current) = self.input_history_cursor else {
            return;
        };
        if current + 1 < self.input_history.len() {
            let index = current + 1;
            self.input_history_cursor = Some(index);
            self.input.clone_from(&self.input_history[index]);
        } else {
            self.input_history_cursor = None;
            self.input.clear();
        }
        self.cursor = self.input.len();
        self.prune_attachments();
    }

    /// 记录成功提交的非空输入，忽略连续重复并只保留最近五十条。
    fn remember_input(&mut self, input: &str) {
        if input.is_empty() || self.input_history.last().is_some_and(|last| last == input) {
            self.input_history_cursor = None;
            return;
        }
        if self.input_history.len() == 50 {
            self.input_history.remove(0);
        }
        self.input_history.push(input.to_string());
        self.input_history_cursor = None;
    }

    /// 处理终端粘贴：单个存在的文件路径转为附件，其余内容保留换行插入光标处。
    pub(crate) fn handle_paste(&mut self, pasted: &str) {
        if let Some(path) = pasted_file_path(pasted) {
            self.attach_file(&path);
            return;
        }
        // 统一平台换行，制表符仍降级为空格以避免终端宽度不一致。
        let text = pasted
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', " ");
        if text.is_empty() {
            return;
        }
        self.insert_input_text(&text);
    }

    /// 读取文件并加入待发送附件，同时在光标处插入引用标签。
    ///
    /// 读取失败或超过 [`MAX_ATTACHMENT_BYTES`] 时追加错误消息，不修改输入。
    pub(crate) fn attach_file(&mut self, path: &Path) {
        use base64::Engine;

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("Failed to read attachment ({}): {error}", path.display()),
                ));
                return;
            }
        };
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!(
                    "Attachment exceeds the {} MiB limit: {}",
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
        self.finish_input_edit();
    }

    /// 生成不与现有附件冲突的引用标签：图片使用递增序号，文件使用文件名。
    pub(crate) fn next_attachment_label(&self, name: &str, is_image: bool) -> String {
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
    pub(crate) fn prune_attachments(&mut self) {
        if self.attachments.is_empty() {
            return;
        }
        let input = &self.input;
        self.attachments.retain(|a| input.contains(&a.label));
    }

    /// 光标紧跟附件引用标签时整体删除标签及对应附件，返回是否命中。
    pub(crate) fn remove_attachment_before_cursor(&mut self) -> bool {
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
    pub(crate) fn copy_last_assistant_message(&mut self) {
        let Some(text) = self
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.kind, MsgKind::Assistant) && !m.text.is_empty())
            .map(|m| m.text.clone())
        else {
            self.messages
                .push(Msg::new(MsgKind::Info, "No assistant reply to copy"));
            return;
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self.messages.push(Msg::new(
                MsgKind::Info,
                "Copied the latest reply to the clipboard",
            )),
            Err(error) => self
                .messages
                .push(Msg::new(MsgKind::Error, format!("Copy failed: {error}"))),
        }
    }

    /// 切换鼠标捕获：暂停后终端可用原生选择复制任意消息文本，恢复后支持滚轮滚动。
    pub(crate) fn toggle_mouse_capture(&mut self) {
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
            "Mouse capture enabled; press Ctrl+T to restore text selection"
        } else {
            "Text selection restored; press Ctrl+T to enable mouse interaction"
        };
        self.messages.push(Msg::new(MsgKind::Info, notice));
    }

    /// 取出并清空当前编辑器内容；空白输入返回 `None`。
    ///
    /// steering 等纯文本路径不携带附件，残留附件一并清除，
    /// 避免引用标签消失后附件悬挂。
    pub(crate) fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.attachments.clear();
        self.cursor = 0;
        self.last_escape_at = None;
        if input.is_empty() {
            return None;
        }
        self.remember_input(&input);
        Some(input)
    }

    /// 取出并清空当前输入与附件；文本和附件都为空时返回 `None`。
    pub(crate) fn take_submission(&mut self) -> Option<UserSubmission> {
        let text = self.input.trim().to_string();
        let attachments = std::mem::take(&mut self.attachments);
        self.input.clear();
        self.cursor = 0;
        self.last_escape_at = None;
        if text.is_empty() && attachments.is_empty() {
            return None;
        }
        self.remember_input(&text);
        Some(UserSubmission { text, attachments })
    }

    /// Queues one complete input while plugin loading keeps the Agent unavailable.
    ///
    /// 插件加载导致 Agent 尚不可用时，将一条完整输入加入 FIFO 队列。
    pub(crate) fn queue_input_until_ready(&mut self) {
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.messages
            .push(Msg::new(MsgKind::User, submission.text.clone()));
        self.queued_inputs.push_back(submission);
        self.scroll = None;
    }

    /// 运行中提交 steering 插话：跳过剩余工具，让模型立即响应新指令。
    pub(crate) fn submit_steering(&mut self, agent: &Arc<Agent>) {
        let Some(input) = self.take_input() else {
            return;
        };
        agent.steer(input.clone());
        self.messages.push(Msg::new(MsgKind::User, input));
        self.messages.push(Msg::new(
            MsgKind::Info,
            "Steering queued; it will be applied after the current tool finishes",
        ));
    }

    /// Submits the current editor value immediately to a ready Agent.
    ///
    /// 将当前编辑器内容立即提交给已就绪的 Agent。
    pub(crate) fn submit(&mut self, agent: &Arc<Agent>) {
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.queued_run_active = false;
        self.start_input_run(agent, submission, true);
    }

    /// Starts one Agent run and optionally appends the user message to the visible history.
    ///
    /// 启动一次 Agent 运行，并按需把用户消息追加到可见历史。
    pub(crate) fn start_input_run(
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
        self.run_started_at = Some(std::time::Instant::now());
        self.streaming_message = None;
        self.scroll = None;

        let agent = Arc::clone(agent);
        let tx = self.tx.clone();
        let session_store = Arc::clone(&self.session_store);
        let session_record = self.session_record.clone();
        let genome_runtime = self.genome_runtime.clone();
        let evidence = self.evidence.clone();

        tokio::spawn(async move {
            let result = match evidence.as_ref() {
                Some(evidence) => {
                    run_and_persist_with_evidence(
                        agent.as_ref(),
                        session_store.as_ref(),
                        session_record,
                        submission,
                        &genome_runtime,
                        Some(evidence),
                    )
                    .await
                }
                None => {
                    run_and_persist(
                        agent.as_ref(),
                        session_store.as_ref(),
                        session_record,
                        submission,
                        &genome_runtime,
                    )
                    .await
                }
            };
            let _ = tx.send(UiEvent::AgentDone(Box::new(result)));
        });
    }

    /// Starts the next pre-ready input after the Agent becomes idle.
    ///
    /// Agent 就绪且空闲后启动下一条预加载输入。
    pub(crate) fn run_next_queued(&mut self, agent: &Arc<Agent>) {
        if self.running {
            return;
        }
        if let Some(submission) = self.queued_inputs.front().cloned() {
            self.queued_run_active = true;
            self.start_input_run(agent, submission, false);
        }
    }

    /// 开始一个新的模型响应轮次，后续增量会写入新的助手消息。
    pub(crate) fn start_model_response(&mut self) {
        self.streaming_message = None;
    }

    /// 将文本增量追加到当前助手消息；不改动滚动位置，用户回看历史时不被拉回底部。
    pub(crate) fn append_model_delta(&mut self, delta: &str) {
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
    ///
    /// 失败信息使用完整错误链，确保插件、WASM 或协议层的根因可直接在主事件列表中查看。
    pub(crate) fn handle_agent_done(&mut self, completion: AgentCompletion) -> bool {
        let AgentCompletion {
            run,
            session_record,
            error,
            input_committed,
            queue_may_advance,
            input,
        } = completion;
        // 仅模型运行失败且没有完成态 Session 时保留当前界面；保存失败时 run 已携带
        // 完整会话，必须以它重建界面，避免保留过期的流式内容。
        let preserve_visible_history = run.is_none() && error.is_some();
        self.running = false;
        self.run_started_at = None;
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
            if !preserve_visible_history {
                self.messages = restore_session_messages(&self.session_record.session);
                self.messages.extend(
                    self.queued_inputs
                        .iter()
                        .map(|pending| Msg::new(MsgKind::User, pending.text.clone())),
                );
            }
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
                self.messages.push(Msg::new(
                    MsgKind::Info,
                    "Run cancelled; generated content was preserved",
                ));
            }
            if !run.usage.is_empty() {
                self.messages.push(Msg::new(
                    MsgKind::Info,
                    format!(
                        "↑{} ↓{} Σ{} tokens · {} steps",
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
                .push(Msg::new(MsgKind::Error, format!("{error:#}")));
        }
        queue_may_advance
    }
}

/// 判断插件视图是否已经返回可见帧。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_view_visible(view: &PluginViewState) -> bool {
    view.frame.as_ref().is_some_and(|frame| frame.visible)
}

/// 判断终端坐标是否位于给定矩形内。
#[cfg(feature = "plugins")]
pub(crate) fn point_in_rect(x: u16, y: u16, area: Rect) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

/// 返回鼠标可命中的插件区域；侧栏额外包含边框和内边距，点击留白也能聚焦。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_view_hit_area(view: &PluginViewState) -> Rect {
    if view.area.is_empty()
        || !matches!(
            view.declaration.placement,
            UiPlacement::Left | UiPlacement::Right
        )
    {
        return view.area;
    }
    Rect::new(
        view.area.x.saturating_sub(2),
        view.area.y.saturating_sub(2),
        view.area.width.saturating_add(4),
        view.area.height.saturating_add(4),
    )
}

/// 将 Crossterm 鼠标动作转换为稳定的字符串名称。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_mouse_kind(kind: MouseEventKind) -> String {
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
pub(crate) fn plugin_key_code(code: KeyCode) -> String {
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
pub(crate) fn plugin_key_modifiers(modifiers: KeyModifiers) -> Vec<String> {
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
pub(crate) fn default_plugin_width(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Left | UiPlacement::Right => 28,
        UiPlacement::Dialog | UiPlacement::Subview => 60,
        UiPlacement::Input | UiPlacement::InputPanel | UiPlacement::ComposerShelf => 40,
        UiPlacement::Top | UiPlacement::Bottom => 40,
    }
}

/// 返回未实际布局前使用的插件视图默认高度。
#[cfg(feature = "plugins")]
pub(crate) fn default_plugin_height(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Top | UiPlacement::Bottom => 6,
        UiPlacement::Dialog | UiPlacement::Subview => 20,
        UiPlacement::Input => 3,
        UiPlacement::InputPanel => 8,
        UiPlacement::ComposerShelf => 4,
        UiPlacement::Left | UiPlacement::Right => 10,
    }
}

// ─── 事件 Sink：将 agent 事件转发到 UI 通道 ───

/// 合并模型文本增量，避免慢速终端为每个 token 累积一个无界通道事件。
#[derive(Default)]
pub(crate) struct ModelDeltaBuffer {
    text: String,
    notification_pending: bool,
}

impl ModelDeltaBuffer {
    /// 追加文本，并返回是否需要向 UI 发送新的就绪通知。
    fn push(&mut self, delta: &str) -> bool {
        self.text.push_str(delta);
        if self.notification_pending {
            false
        } else {
            self.notification_pending = true;
            true
        }
    }

    /// 取出当前全部文本并允许生产者发送下一次就绪通知。
    pub(crate) fn take(&mut self) -> String {
        self.notification_pending = false;
        std::mem::take(&mut self.text)
    }
}

/// 将 Agent 事件转换为 TUI 事件；高频模型文本通过共享缓冲区合并。
pub(crate) struct ChannelEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
    model_deltas: Arc<std::sync::Mutex<ModelDeltaBuffer>>,
}

impl ChannelEventSink {
    /// 创建事件 sink，并返回供 UI 消费模型文本的共享缓冲区。
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<UiEvent>,
    ) -> (Self, Arc<std::sync::Mutex<ModelDeltaBuffer>>) {
        let model_deltas = Arc::new(std::sync::Mutex::new(ModelDeltaBuffer::default()));
        (
            Self {
                tx,
                model_deltas: model_deltas.clone(),
            },
            model_deltas,
        )
    }
}

/// 判断字符是否为按词移动与删除使用的分隔符。
fn is_word_separator(ch: char) -> bool {
    ch.is_whitespace() || ch.is_ascii_punctuation()
}

/// 返回光标左侧一个词的起始字节位置。
fn previous_word_boundary(input: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while let Some(ch) = input[..position].chars().next_back() {
        if !is_word_separator(ch) {
            break;
        }
        position -= ch.len_utf8();
    }
    while let Some(ch) = input[..position].chars().next_back() {
        if is_word_separator(ch) {
            break;
        }
        position -= ch.len_utf8();
    }
    position
}

/// 返回光标右侧下一个词的起始字节位置。
fn next_word_boundary(input: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while let Some(ch) = input[position..].chars().next() {
        if is_word_separator(ch) {
            break;
        }
        position += ch.len_utf8();
    }
    while let Some(ch) = input[position..].chars().next() {
        if !is_word_separator(ch) {
            break;
        }
        position += ch.len_utf8();
    }
    position
}

/// 在目标逻辑行中找到不超过指定显示列的字节位置。
fn byte_at_display_column(line: &str, target: usize) -> usize {
    let mut width = 0;
    let mut offset = 0;
    for ch in line.chars() {
        let next = width + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if next > target {
            break;
        }
        width = next;
        offset += ch.len_utf8();
    }
    offset
}

/// 按显示列在相邻逻辑行间移动光标；到达首行或末行时返回 `false`。
fn move_cursor_vertically(input: &str, cursor: &mut usize, upward: bool) -> bool {
    let line_start = input[..*cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_end = input[*cursor..]
        .find('\n')
        .map_or(input.len(), |index| *cursor + index);
    let column = unicode_width::UnicodeWidthStr::width(&input[line_start..*cursor]);
    let (target_start, target_end) = if upward {
        if line_start == 0 {
            return false;
        }
        let end = line_start - 1;
        let start = input[..end].rfind('\n').map_or(0, |index| index + 1);
        (start, end)
    } else {
        if line_end == input.len() {
            return false;
        }
        let start = line_end + 1;
        let end = input[start..]
            .find('\n')
            .map_or(input.len(), |index| start + index);
        (start, end)
    };
    *cursor = target_start + byte_at_display_column(&input[target_start..target_end], column);
    true
}

#[async_trait]
impl EventSink for ChannelEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        match event.kind {
            AgentEventKind::ModelRequest => {
                let _ = self.tx.send(UiEvent::ModelStarted);
            }
            AgentEventKind::ModelResponse => {
                // 转发本轮请求的 input tokens，作为当前上下文大小显示在底栏。
                if let Some(tokens) = event
                    .payload
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                {
                    let _ = self.tx.send(UiEvent::ContextUsage(tokens));
                }
            }
            AgentEventKind::ModelTextDelta => {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    let should_notify = self
                        .model_deltas
                        .lock()
                        .expect("模型增量缓冲区锁不应中毒")
                        .push(delta);
                    if should_notify {
                        let _ = self.tx.send(UiEvent::ModelTextReady);
                    }
                }
            }
            AgentEventKind::ToolStarted => {
                let call = serde_json::from_value::<ToolCall>(event.payload.clone())?;
                let _ = self.tx.send(UiEvent::ToolStarted(call));
            }
            AgentEventKind::ToolOutputDelta => {
                let output = serde_json::from_value::<ToolOutputDelta>(event.payload.clone())?;
                let _ = self.tx.send(UiEvent::ToolOutputDelta(output));
            }
            AgentEventKind::ToolFinished => {
                let result = serde_json::from_value::<ToolResult>(event.payload.clone())?;
                let _ = self.tx.send(UiEvent::ToolFinished(result));
            }
            AgentEventKind::ToolSkipped => {
                let call = serde_json::from_value::<ToolCall>(
                    event.payload.get("call").cloned().unwrap_or(Value::Null),
                )?;
                let reason = event
                    .payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("工具跳过事件缺少原因"))?
                    .to_string();
                let _ = self.tx.send(UiEvent::ToolSkipped { call, reason });
            }
            AgentEventKind::SteeringInjected => {
                let _ = self.tx.send(UiEvent::SteeringInjected);
            }
            AgentEventKind::FollowUpInjected => {
                let _ = self.tx.send(UiEvent::FollowUpInjected);
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
                    let _ = self.tx.send(UiEvent::ViewNavigation {
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
                    let _ = self.tx.send(UiEvent::Extension {
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
