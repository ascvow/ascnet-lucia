//! TUI 启动组装、运行队列驱动与终端事件循环。

use super::*;

/// 终端状态守卫：正常退出、`?` 提前返回或 panic 展开时都恢复终端。
///
/// raw mode 或 bracketed paste 残留会让外层 shell 换行不回车、粘贴出现
/// 转义序列，且 `clear` 无法恢复，因此恢复必须挂在 Drop 上而不是顺序代码。
struct TerminalGuard {
    /// 启动时是否推送了键盘增强协议，退出时需要对应弹出。
    keyboard_enhanced: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.keyboard_enhanced {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::PopKeyboardEnhancementFlags
            );
        }
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableMouseCapture
        );
        ratatui::restore();
    }
}

pub(crate) async fn run(args: Args) -> Result<()> {
    let locale = Locale::detect();
    let workspace = WorkspaceContext::capture()?;
    let config_path = resolve_config_path(args.config.as_deref())?;
    if args.init {
        initialize_config(&config_path)?;
        println!(
            "{}: {}",
            locale.select("Created Lucia configuration", "已创建 Lucia 配置"),
            config_path.display()
        );
        println!(
            "{}",
            locale.select(
                "Set model.api_key (or model.api_key_env), confirm model.model, then run lucia",
                "填写 model.api_key（或配置 model.api_key_env）并确认 model.model 后即可运行 lucia",
            )
        );
        return Ok(());
    }

    let mut config_exists = config_path.is_file();
    if args.config.is_some() && !config_exists {
        return Err(anyhow!(
            "{}: {}",
            locale.select("Configuration file not found", "配置文件不存在"),
            config_path.display()
        ));
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
    #[cfg(feature = "plugins")]
    {
        let absolute_home = if lucia_home.is_absolute() {
            lucia_home.clone()
        } else {
            std::env::current_dir()
                .context("解析 Lucia Home 绝对路径失败")?
                .join(&lucia_home)
        };
        configure_wasm_cache_directory(absolute_home.join("cache/wasmtime"))?;
    }
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
    let mut disabled_plugins = Vec::new();
    #[cfg(feature = "plugins")]
    if config_exists {
        let plugin_runtime = load_plugin_runtime_config(&config_path)?;
        plugin_manifests.extend(plugin_runtime.manifest_paths);
        capability_selection.extend(plugin_runtime.capability_selection);
        disabled_plugins.extend(plugin_runtime.disabled_plugins);
    }
    #[cfg(feature = "plugins")]
    {
        let user_manifests = discover_plugin_manifests(&lucia_home.join("plugins"))
            .context("扫描用户插件目录失败")?;
        merge_plugin_manifests(&mut plugin_manifests, user_manifests);
        let managed_runtime = agent_plugin_manager::PluginManager::new(&lucia_home)
            .runtime_config()
            .context("读取受管理插件配置失败；请运行 `lucia doctor`")?;
        merge_plugin_manifests(&mut plugin_manifests, managed_runtime.manifest_paths);
        for (capability, owner) in managed_runtime.capability_selection {
            capability_selection.entry(capability).or_insert(owner);
        }
        let official_manifests = discover_plugin_manifests(&lucia_home.join("official-plugins"))
            .context("扫描官方插件目录失败")?;
        merge_plugin_manifests(&mut plugin_manifests, official_manifests);
    }
    // 用户禁用列表最后生效：受管理插件和显式声明都会被剔除。
    #[cfg(feature = "plugins")]
    remove_disabled_plugin_manifests(&mut plugin_manifests, &disabled_plugins);

    let (gateway, options, demo_mode, mut startup_notices) = if args.demo {
        let (gateway, options) = build_demo_gateway();
        (
            gateway,
            options,
            true,
            vec![locale
                .select(
                    "Using the local demo model; no external model service will be contacted",
                    "当前使用本地演示模型，不会连接外部模型服务",
                )
                .to_string()],
        )
    } else if config_exists {
        let config = AgentRootConfig::load(&config_path)?;
        if configured_model_key_is_available(&config) {
            let mut options = config.agent_options();
            if config.agent.max_steps.is_none() {
                // 交互主会话默认不设总步数上限；后台运行可显式传入正数限制。
                options.max_steps = 0;
            }
            (config.build_gateway()?, options, false, Vec::new())
        } else {
            let key_hint = config
                .model
                .api_key_env
                .as_deref()
                .map(|name| {
                    format!(
                        "{} {name}",
                        locale.select("set environment variable", "设置环境变量")
                    )
                })
                .unwrap_or_else(|| {
                    locale
                        .select(
                            "set model.api_key or model.api_key_env in the configuration",
                            "在配置中设置 model.api_key 或 model.api_key_env",
                        )
                        .to_string()
                });
            let (gateway, options) = build_demo_gateway();
            (
                gateway,
                options,
                true,
                vec![match locale {
                    Locale::English => format!(
                        "No model key detected; using the local demo model. {key_hint}, then restart lucia"
                    ),
                    Locale::SimplifiedChinese => format!(
                        "未检测到模型密钥，当前使用本地演示模型；{key_hint} 后重新运行 lucia"
                    ),
                }],
            )
        }
    } else {
        let (gateway, options) = build_demo_gateway();
        (gateway, options, true, Vec::new())
    };
    if auto_initialized {
        startup_notices.insert(
            0,
            format!(
                "{}: {}",
                locale.select("Created default configuration", "已创建默认配置"),
                config_path.display()
            ),
        );
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
    let (ui_sink, model_deltas) = ChannelEventSink::new(tx.clone());
    sink.push(Arc::new(ui_sink));
    if let Some(path) = events_jsonl.as_ref() {
        sink.push(Arc::new(JsonlEventSink::new(path.clone())));
    }

    let base_agent = Agent::new(gateway, options)
        .with_tools(native_tools)
        .with_event_sink(Arc::new(sink));
    #[cfg(feature = "plugins")]
    let plugin_agent_template = AgentTemplate::from_agent(&base_agent);
    #[cfg(feature = "plugins")]
    let live_plugin_host = Arc::new(LivePluginHost::new());
    #[cfg(feature = "plugins")]
    let agent = Some(Arc::new(
        base_agent
            .with_extension(live_plugin_host.clone())
            .with_context_loader(live_plugin_host.clone()),
    ));
    #[cfg(not(feature = "plugins"))]
    let agent = Some(Arc::new(base_agent));
    #[cfg(feature = "plugins")]
    let plugin_host = Some(Arc::clone(&live_plugin_host));
    #[cfg(feature = "plugins")]
    let loading_plugin_ids = plugin_manifest_ids(&plugin_manifests);

    let mut terminal = ratatui::init();
    // 键盘增强协议让 Shift+Enter 等修饰组合可被区分（传统协议下它就是裸 Enter）。
    // 该查询会读取终端应答，必须在输入读取线程启动前完成，否则应答被线程吞掉。
    let keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    // 守卫覆盖所有退出路径，包括 draw 失败的 `?` 提前返回。
    let terminal_guard = TerminalGuard { keyboard_enhanced };
    // 默认启用鼠标交互以支持点击插件聚焦；Ctrl+T 可暂停捕获并恢复终端原生选择。
    // 启用 bracketed paste，将拖入的文件路径识别为附件。
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture
    );

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
        .with_locale(locale)
        .with_context_window(tui_settings.context_window)
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
        let load_host = Arc::clone(&live_plugin_host);
        tokio::spawn(async move {
            let result = load_plugins_for_tui(
                plugin_manifests,
                capability_selection,
                plugin_agent_template,
                locale,
                load_host,
                load_tx.clone(),
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
                            let before = (app.input.clone(), app.cursor);
                            app.handle_key(key.code, key.modifiers, agent.as_ref());
                            // 编辑结果变化且触发前缀激活时同步主输入快照。
                            if app.input != before.0 || app.cursor != before.1 {
                                if let Some(host) = plugin_host.as_ref() {
                                    forward_main_input_snapshot(&mut app, host).await;
                                }
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
                                    if let Err(error) = drain_plugin_ui_events(&mut app, host).await
                                    {
                                        app.messages.push(Msg::new(
                                            MsgKind::Error,
                                            format!(
                                                "{}: {error}",
                                                locale.select(
                                                    "Failed to process plugin UI event",
                                                    "插件 UI 事件处理失败"
                                                )
                                            ),
                                        ));
                                    }
                                    // 刷新该插件的全部视图，插件可借输入处理开关对话框。
                                    refresh_plugin_views_for(
                                        &mut app,
                                        host.as_ref(),
                                        &input.plugin_id,
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
            Some(UiEvent::PluginLoadUpdate(update)) => match update {
                ProgressivePluginLoadUpdate::Ready {
                    plugin_id,
                    startup_events,
                    ui_declarations,
                    load_duration_ms,
                } => {
                    app.mark_plugin_ready(plugin_id.clone(), &startup_events, load_duration_ms);
                    app.add_plugin_views(ui_declarations);
                    for event in &startup_events {
                        if let Err(error) = apply_plugin_navigation_event(&mut app, event) {
                            app.messages.push(Msg::new(
                                MsgKind::Error,
                                format!(
                                    "{}: {error}",
                                    locale.select(
                                        "Plugin view navigation failed",
                                        "插件视图导航失败"
                                    )
                                ),
                            ));
                        }
                    }
                    if let Some(host) = plugin_host.as_ref() {
                        match host.ui_declarations().await {
                            Ok(_) => {
                                app.invalidate_tool_frames();
                                app.schedule_stale_tool_renders(Arc::clone(host));
                            }
                            Err(error) => app.messages.push(Msg::new(
                                MsgKind::Error,
                                format!(
                                    "{}: {error}",
                                    locale.select(
                                        "Failed to refresh plugin UI declarations",
                                        "插件 UI 声明刷新失败"
                                    )
                                ),
                            )),
                        }
                        app.schedule_plugin_views_refresh(Arc::clone(host));
                    }
                }
                ProgressivePluginLoadUpdate::Failed(failure) => {
                    app.mark_plugin_failed(failure);
                }
            },
            #[cfg(feature = "plugins")]
            Some(UiEvent::PluginFramesLoaded(rendered)) => {
                app.plugin_refresh_task = None;
                apply_plugin_frames(&mut app, rendered);
                if app.plugin_refresh_pending {
                    app.plugin_refresh_pending = false;
                    if let Some(host) = plugin_host.as_ref() {
                        app.schedule_plugin_views_refresh(Arc::clone(host));
                    }
                }
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::PluginsLoaded(result)) => match *result {
                Ok(()) => {
                    app.finish_progressive_plugin_loading();
                }
                Err(error) => {
                    app.set_progressive_plugin_load_error(&error);
                    app.messages.push(Msg::new(
                        MsgKind::Error,
                        format!(
                            "{}: {error}",
                            locale.select(
                                "Plugin load planning failed; currently ready plugins were retained",
                                "插件加载规划失败，已保留当前 Ready 插件"
                            )
                        ),
                    ));
                }
            },
            #[cfg(feature = "plugins")]
            Some(UiEvent::SessionsQueryDone {
                plugin_id,
                reply_service,
                reply,
            }) => {
                if let Some(host) = plugin_host.as_ref() {
                    match call_typed_plugin_service::<_, Value>(
                        host.as_ref(),
                        HOST_SERVICE_CALLER,
                        &plugin_id,
                        &reply_service,
                        reply.as_ref(),
                    )
                    .await
                    {
                        Ok(Some(_)) => {
                            refresh_plugin_views_for(&mut app, host.as_ref(), &plugin_id).await
                        }
                        Ok(None) => app.messages.push(Msg::new(
                            MsgKind::Error,
                            match locale {
                                Locale::English => format!(
                                    "Plugin `{plugin_id}` became unavailable before the session query completed"
                                ),
                                Locale::SimplifiedChinese => {
                                    format!("插件 `{plugin_id}` 在会话查询完成前已不可用")
                                }
                            },
                        )),
                        Err(error) => app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!(
                                "{}: {error}",
                                locale.select(
                                    "Failed to return session query results",
                                    "回送会话查询结果失败"
                                )
                            ),
                        )),
                    }
                }
            }
            Some(UiEvent::ModelStarted) => {
                app.start_model_response();
            }
            Some(UiEvent::ModelTextReady) => {
                let delta = model_deltas
                    .lock()
                    .expect("模型增量缓冲区锁不应中毒")
                    .take();
                if !delta.is_empty() {
                    app.append_model_delta(&delta);
                }
            }
            Some(UiEvent::ToolStarted(call)) => {
                #[cfg(feature = "plugins")]
                let call_id = call.id.clone();
                app.messages.push(Msg::tool_started(call));
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    app.schedule_tool_render(Arc::clone(host), &call_id);
                }
            }
            Some(UiEvent::ToolFinished(tool_result)) => {
                // 只按稳定调用 ID 更新运行项，避免同名并发工具互相覆盖。
                let kind = if tool_result.is_error {
                    MsgKind::ToolError
                } else {
                    MsgKind::ToolOk
                };
                let call_id = tool_result.call_id.clone();
                if let Some(msg) = app.messages.iter_mut().rev().find(|message| {
                    matches!(message.kind, MsgKind::ToolRunning)
                        && message.tool_call_id() == Some(call_id.as_str())
                }) {
                    msg.kind = kind;
                    msg.tool_result = Some(tool_result);
                } else {
                    app.messages.push(Msg::tool_finished(tool_result));
                }
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    app.schedule_tool_render(Arc::clone(host), &call_id);
                }
            }
            Some(UiEvent::ToolSkipped { call, reason }) => {
                #[cfg(feature = "plugins")]
                let call_id = call.id.clone();
                let mut message = Msg::tool_started(call);
                message.kind = MsgKind::ToolSkipped;
                message.skip_reason = Some(reason);
                app.messages.push(message);
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    app.schedule_tool_render(Arc::clone(host), &call_id);
                }
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::ToolFrameLoaded {
                call_id,
                revision,
                width,
                result,
            }) => app.apply_tool_frame(&call_id, revision, width, result),
            Some(UiEvent::SteeringInjected) => {
                app.messages.push(Msg::new(
                    MsgKind::Info,
                    locale.select("Steering applied", "插话已生效"),
                ));
            }
            Some(UiEvent::FollowUpInjected) => {
                app.messages.push(Msg::new(
                    MsgKind::Info,
                    locale.select("Follow-up started", "追加任务开始"),
                ));
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
                        format!(
                            "{}: {error}",
                            locale.select("Plugin view navigation failed", "插件视图导航失败")
                        ),
                    ));
                }
            }
            Some(UiEvent::ContextUsage(tokens)) => {
                app.context_tokens = Some(tokens);
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::SessionContextReloaded(result)) => {
                app.handle_session_context_reloaded(*result);
                // 重载期间加载器插件发布的展示事件补进主事件列表。
                if let Some(host) = plugin_host.as_ref() {
                    if let Err(error) = drain_plugin_ui_events(&mut app, host).await {
                        app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!(
                                "{}: {error}",
                                locale.select("Failed to process plugin event", "插件事件处理失败")
                            ),
                        ));
                    }
                }
            }
            Some(UiEvent::AgentDone(result)) => {
                record_run_failure_event(events_jsonl.as_deref(), result.as_ref()).await;
                let queue_may_advance = app.handle_agent_done(*result);
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    app.schedule_plugin_views_refresh(Arc::clone(host));
                }
                if queue_may_advance {
                    if let Some(agent) = agent.as_ref() {
                        app.run_next_queued(agent);
                    }
                }
            }
            Some(UiEvent::Tick) => {
                tick_pending.store(false, Ordering::Relaxed);
                #[cfg(feature = "plugins")]
                let animate_spinner =
                    app.running || app.plugins_loading || app.pending_reload.is_some();
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
                            app.schedule_plugin_views_refresh(Arc::clone(host));
                        }
                    }
                    if let Some(host) = plugin_host.as_ref() {
                        app.schedule_stale_tool_renders(Arc::clone(host));
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
                                if let Err(error) = drain_plugin_ui_events(&mut app, host).await {
                                    app.messages.push(Msg::new(
                                        MsgKind::Error,
                                        format!(
                                            "{}: {error}",
                                            locale.select(
                                                "Failed to process plugin UI event",
                                                "插件 UI 事件处理失败"
                                            )
                                        ),
                                    ));
                                }
                                refresh_plugin_views_for(&mut app, host.as_ref(), &input.plugin_id)
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
                    // 粘贴同样可能激活触发前缀，同步一次主输入快照。
                    #[cfg(feature = "plugins")]
                    if let Some(host) = plugin_host.as_ref() {
                        forward_main_input_snapshot(&mut app, host).await;
                    }
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
    if let Some(refresh_task) = app.plugin_refresh_task.take() {
        let _ = refresh_task.await;
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
            Err(_) => Some(anyhow!(
                locale.select("Plugin Host shutdown timed out", "插件宿主卸载超时")
            )),
        }
    } else {
        None
    };

    // 终端恢复由 TerminalGuard 的 Drop 统一执行。
    drop(terminal_guard);
    #[cfg(feature = "plugins")]
    if let Some(error) = plugin_shutdown_error {
        return Err(error);
    }
    Ok(())
}

/// 触发前缀激活时，把主输入快照转发给触发视图并刷新其面板。
///
/// 触发未激活时不产生任何插件调用；面板的隐藏由渲染层按触发状态兜底。
#[cfg(feature = "plugins")]
async fn forward_main_input_snapshot(app: &mut App, plugin_host: &Arc<LivePluginHost>) {
    let Some(index) = app.active_trigger_view() else {
        return;
    };
    let input = app.main_input_snapshot(index);
    if let Err(error) = dispatch_plugin_input(plugin_host.as_ref(), &input).await {
        app.set_plugin_ui_error(&input.plugin_id, &input.view_id, None, &error);
        return;
    }
    if let Err(error) = drain_plugin_ui_events(app, plugin_host).await {
        app.messages.push(Msg::new(
            MsgKind::Error,
            format!(
                "{}: {error}",
                app.locale
                    .select("Failed to process plugin UI event", "插件 UI 事件处理失败")
            ),
        ));
    }
    refresh_plugin_view(app, plugin_host.as_ref(), &input.plugin_id, &input.view_id).await;
}

/// 将 Agent 运行失败补写到可选 JSONL 事件文件，不影响主界面的错误呈现。
async fn record_run_failure_event(events_jsonl: Option<&Path>, completion: &AgentCompletion) {
    let (Some(path), Some(error)) = (events_jsonl, completion.error.as_ref()) else {
        return;
    };
    let event = AgentEvent::new(
        format!("session:{}", completion.session_record.id),
        AgentEventKind::Extension,
        0,
        json!({
            "name": "tui.run.failed",
            "error": format!("{error:#}"),
            "session_id": completion.session_record.id.to_string(),
        }),
    );
    let _ = JsonlEventSink::new(path).record(&event).await;
}

// ─── Demo 模型 ───
