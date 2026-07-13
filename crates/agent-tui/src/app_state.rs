//! TUI App 状态机、运行队列、插件视图状态和事件 sink。

use super::*;

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
    /// 当前运行的起始时间，用于渲染运行耗时。
    pub(crate) run_started_at: Option<std::time::Instant>,
    pub(crate) should_quit: bool,
    /// 下一轮使用的完整会话记录；最终保存失败时可暂存 dirty 完成态。
    pub(crate) session_record: SessionRecord,
    /// 执行 revision 比较并交换的会话存储。
    pub(crate) session_store: Arc<dyn SessionStore>,
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
    /// 最近一次模型请求消耗的上下文 token 数。
    pub(crate) context_tokens: Option<u64>,
    /// 配置的模型上下文窗口，用于状态栏计算占比。
    pub(crate) context_window: Option<u64>,
    /// 鼠标捕获是否开启；默认关闭以便终端可原生选择复制消息文本。
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
    /// Command Provider 发布的只读命令快照。
    #[cfg(feature = "plugins")]
    pub(crate) command_snapshot: Option<CommandSnapshot>,
    /// 当前命令预览中的选中项。
    #[cfg(feature = "plugins")]
    pub(crate) command_selection: usize,
    /// 用户按 Esc 后临时隐藏当前输入对应的命令预览。
    #[cfg(feature = "plugins")]
    pub(crate) command_preview_hidden: bool,
    /// 最近一次会话摘要查询任务；新输入会中止尚未完成的旧查询。
    #[cfg(feature = "plugins")]
    pub(crate) command_query_task: Option<tokio::task::JoinHandle<()>>,
    /// 控制运行期命令快照低频刷新。
    #[cfg(feature = "plugins")]
    pub(crate) command_snapshot_tick: u8,
    /// 防止同一时间存在多个命令快照请求。
    #[cfg(feature = "plugins")]
    pub(crate) command_snapshot_refreshing: bool,
    /// 当前输入可展示并选择的参数候选。
    #[cfg(feature = "plugins")]
    pub(crate) command_completion: Option<ResolvedCommandCompletion>,
    /// 正在运行的显式参数候选请求；输入变化时会中止。
    #[cfg(feature = "plugins")]
    pub(crate) command_completion_task: Option<tokio::task::JoinHandle<()>>,
    /// 参数候选请求是否尚未返回。
    #[cfg(feature = "plugins")]
    pub(crate) command_completion_loading: bool,
    /// 用于丢弃取消后仍到达的过期参数候选。
    #[cfg(feature = "plugins")]
    pub(crate) command_completion_generation: u64,
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
            run_started_at: None,
            should_quit: false,
            session_record,
            session_store: Arc::new(MemorySessionStore::new()),
            tx,
            model_name,
            spinner_frame: 0,
            streaming_message: None,
            scroll: None,
            last_max_scroll: 0,
            last_viewport: 0,
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

    /// 用完整记录替换当前会话，并清理只属于旧会话的瞬时界面状态。
    #[cfg(feature = "plugins")]
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
        #[cfg(feature = "plugins")]
        self.clear_command_completion();
        if let Some(notice) = notice {
            self.messages.push(Msg::new(MsgKind::Info, notice));
        }
    }

    /// 进入当前项目下尚未持久化的全新空白草稿。
    #[cfg(feature = "plugins")]
    pub(crate) fn start_new_draft(&mut self, notice: &str) -> Result<()> {
        let draft = self.workspace.draft_record()?;
        self.replace_session(draft, Some(notice));
        Ok(())
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
            format!("，依赖 {}", failure.blocked_by.join("、"))
        };
        let detail = format!(
            "{}: 加载失败{blocked} · {}",
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
                format!(" · 已就绪 {}", self.plugin_ids.len())
            };
            let queue = if self.queued_inputs.is_empty() {
                String::new()
            } else {
                format!(" · queued {}", self.queued_inputs.len())
            };
            let text = if plugins.is_empty() {
                format!("正在加载插件{ready}{queue}")
            } else {
                format!("正在加载插件 · {plugins}{ready}{queue}")
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
    pub(crate) fn plugin_status_color(&self) -> Color {
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
    pub(crate) fn set_command_snapshot(&mut self, snapshot: Option<CommandSnapshot>) {
        self.clear_command_completion();
        self.command_snapshot = snapshot;
        self.command_selection = 0;
        self.command_preview_hidden = false;
    }

    /// 返回与当前斜杠输入匹配的命令定义。
    #[cfg(feature = "plugins")]
    pub(crate) fn command_matches(&self) -> Vec<CommandSpec> {
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
    pub(crate) fn command_spec_for_input(&self, input: &str) -> Option<&CommandSpec> {
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
    pub(crate) fn start_command_query(
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
    pub(crate) fn schedule_command_snapshot_refresh(&mut self, plugin_host: Arc<LivePluginHost>) {
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
    pub(crate) fn clear_command_completion(&mut self) {
        if let Some(task) = self.command_completion_task.take() {
            task.abort();
        }
        self.command_completion = None;
        self.command_completion_loading = false;
        self.command_completion_generation = self.command_completion_generation.wrapping_add(1);
    }

    /// 在后台执行一次显式参数补全请求。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_command_completion(&mut self, plugin_host: Arc<LivePluginHost>) {
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
    pub(crate) fn apply_selected_command_completion(&mut self) -> bool {
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
    pub(crate) fn handle_command_preview_key(&mut self, code: KeyCode) -> bool {
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

    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) {
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
        // 历史输入回溯入口用 Ctrl+P/Ctrl+N：↑/↓ 必须保留给消息滚动，
        // 备用屏下鼠标滚轮会被终端映射为方向键，劫持它们等于禁用滚动。
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

        #[cfg(feature = "plugins")]
        if self.handle_command_preview_key(code) {
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
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
            KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
                self.cursor = next_word_boundary(&self.input, self.cursor);
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
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
                // Esc 三级行为：运行中请求取消，输入非空先清空，空闲才退出，
                // 避免打字中途误触直接丢内容退程序。
                if self.running {
                    if let Some(agent) = agent {
                        agent.cancel();
                        self.messages
                            .push(Msg::new(MsgKind::Info, "正在取消当前运行..."));
                    }
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                    self.input_history_cursor = None;
                    self.prune_attachments();
                    #[cfg(feature = "plugins")]
                    {
                        self.clear_command_completion();
                        self.command_selection = 0;
                        self.command_preview_hidden = false;
                    }
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::PageUp => self.scroll_up(self.last_viewport.saturating_sub(1).max(1)),
            KeyCode::PageDown => self.scroll_down(self.last_viewport.saturating_sub(1).max(1)),
            KeyCode::Up => {
                if self.input_history_cursor.is_some() {
                    self.recall_older_input();
                } else if self.input.contains('\n')
                    && move_cursor_vertically(&self.input, &mut self.cursor, true)
                {
                    #[cfg(feature = "plugins")]
                    self.clear_command_completion();
                } else {
                    self.scroll_up(1);
                }
            }
            KeyCode::Down => {
                if self.input_history_cursor.is_some() {
                    self.recall_newer_input();
                } else if self.input.contains('\n')
                    && move_cursor_vertically(&self.input, &mut self.cursor, false)
                {
                    #[cfg(feature = "plugins")]
                    self.clear_command_completion();
                } else {
                    self.scroll_down(1);
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
                #[cfg(feature = "plugins")]
                {
                    self.clear_command_completion();
                    self.command_selection = 0;
                    self.command_preview_hidden = false;
                }
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
                // 多行输入时回到当前逻辑行行首；单行输入等价于整段开头。
                self.cursor = self.input[..self.cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
            }
            KeyCode::End => {
                // 多行输入时到当前逻辑行行尾；单行输入等价于整段末尾。
                self.cursor = self.input[self.cursor..]
                    .find('\n')
                    .map_or(self.input.len(), |index| self.cursor + index);
                #[cfg(feature = "plugins")]
                self.clear_command_completion();
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

    /// 构造周期刷新请求，并跳过由用户操作定向刷新的隐藏 Command Dialog。
    #[cfg(feature = "plugins")]
    pub(crate) fn periodic_plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
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

    /// 在当前光标处插入文本，并退出历史回溯态、同步附件与命令补全状态。
    fn insert_input_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.finish_input_edit();
    }

    /// 完成一次输入内容修改后的统一清理。
    fn finish_input_edit(&mut self) {
        self.input_history_cursor = None;
        self.prune_attachments();
        #[cfg(feature = "plugins")]
        {
            self.clear_command_completion();
            self.command_selection = 0;
            self.command_preview_hidden = false;
        }
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
    pub(crate) fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.attachments.clear();
        self.cursor = 0;
        #[cfg(feature = "plugins")]
        self.clear_command_completion();
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
        #[cfg(feature = "plugins")]
        self.clear_command_completion();
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
    pub(crate) fn submit_steering(&mut self, agent: &Arc<Agent>) {
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
        UiPlacement::Input => 40,
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
                // 参数压缩为单行摘要，展示宽度由 UI 侧统一控制。
                let args = event
                    .payload
                    .get("args")
                    .map(|value| summarize_json(value, 64))
                    .unwrap_or_default();
                let _ = self.tx.send(UiEvent::ToolStarted { name: name(), args });
            }
            AgentEventKind::ToolFinished => {
                let is_error = event
                    .payload
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // 文本结果保留多行预览，首行作为摘要行展示。
                let mut lines = event
                    .payload
                    .get("result")
                    .map(|value| tool_result_lines(value, TOOL_RESULT_PREVIEW_LINES, 96))
                    .unwrap_or_default();
                let result = if lines.is_empty() {
                    String::new()
                } else {
                    lines.remove(0)
                };
                let _ = self.tx.send(UiEvent::ToolFinished {
                    name: name(),
                    is_error,
                    result,
                    detail: lines,
                });
            }
            AgentEventKind::ToolSkipped => {
                let _ = self.tx.send(UiEvent::ToolSkipped(name()));
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
