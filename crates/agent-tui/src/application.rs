//! TUI 启动组装、运行队列驱动与终端事件循环。

use super::*;

pub(crate) async fn run() -> Result<()> {
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
    // 默认保留终端原生鼠标选择，用户可通过 Ctrl+T 启用滚轮滚动。
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
            Some(UiEvent::ModelTextReady) => {
                let delta = model_deltas
                    .lock()
                    .expect("模型增量缓冲区锁不应中毒")
                    .take();
                if !delta.is_empty() {
                    app.append_model_delta(&delta);
                }
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
                record_run_failure_event(events_jsonl.as_deref(), result.as_ref()).await;
                let queue_may_advance = app.handle_agent_done(*result);
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    refresh_plugin_views(&mut app, host.as_ref()).await;
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
