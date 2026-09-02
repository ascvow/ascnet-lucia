//! TUI 启动组装、运行队列驱动与终端事件循环。

#[cfg(feature = "plugins")]
use crate::app::PluginKeyRoute;
use crate::app::{is_copy_shortcut, reasoning_level_label, App, ChannelEventSink};
use crate::cli::Args;
use crate::command::resume_native_session;
use crate::config::{
    configured_model_key_is_available, initialize_config, load_tui_settings, lucia_home_dir,
    resolve_config_path, resolve_config_relative_path, TuiSettings,
};
use crate::demo::{build_demo_gateway, echo_spec};
use crate::event::{AgentCompletion, UiEvent};
#[cfg(feature = "plugins")]
use crate::plugin::host_actions::{call_typed_plugin_service, HOST_SERVICE_CALLER};
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementService;
#[cfg(feature = "plugins")]
use crate::plugin::startup::{
    discover_plugin_manifests, load_plugins_for_tui, merge_plugin_manifests, plugin_manifest_ids,
    remove_disabled_plugin_manifests, PluginExecutionContext,
};
use crate::session::conversation::{Msg, MsgKind};
use crate::session::{
    load_startup_session, print_persisted_sessions, resolve_tui_path, start_session_context_reload,
    LazyFileSessionStore, WorkspaceContext,
};
#[cfg(feature = "plugins")]
use crate::theme::PLUGIN_REFRESH_TICKS;
use crate::theme::UI_TICK_INTERVAL_MS;
#[cfg(feature = "plugins")]
use crate::ui::plugin_bridge::{
    apply_plugin_footer_event, apply_plugin_frames, apply_plugin_navigation_event,
    dispatch_plugin_input, drain_plugin_ui_events, refresh_plugin_view, refresh_plugin_views_for,
};
use crate::ui::root::render_root;
use agent_context::{NativeContextLoader, DEFAULT_MAX_SUMMARY_TOKENS};
use agent_core::config::AgentRootConfig;
use agent_core::event::{
    AgentEvent, AgentEventKind, CompositeEventSink, EventSink, JsonlEventSink,
};
use agent_core::{Agent, ContextLoader};
#[cfg(feature = "plugins")]
use agent_plugin_host::manifest::load_plugin_runtime_config;
#[cfg(feature = "plugins")]
use agent_plugin_host::wasm::{configure_wasm_cache_directory, ProgressivePluginLoadUpdate};
#[cfg(feature = "plugins")]
use agent_plugin_host::{LivePluginHost, PluginHost};
#[cfg(feature = "plugins")]
use agent_runtime::AgentTemplate;
use agent_session::SessionStore;
use agent_skill::SkillCatalog;
use agent_tool::{JsonTool, ToolRegistry};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use serde_json::{json, Value};
#[cfg(feature = "plugins")]
use std::collections::HashMap;
use std::path::Path;
#[cfg(feature = "plugins")]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 创建以 TUI 启动目录为上界的原生工具工作区守卫。
///
/// # Errors
///
/// 工作区根目录不存在或无法规范化时返回错误。
fn native_workspace_guard(workspace_root: &Path) -> Result<agent_tool::WorkspaceGuard> {
    Ok(agent_tool::WorkspaceGuard::rooted(workspace_root)?)
}

/// 返回应用插件持久状态目录。
#[cfg(feature = "plugins")]
fn persistent_plugin_state_dir(lucia_home: &Path) -> PathBuf {
    lucia_home.join("plugin-state")
}

/// 把动态插件宿主同时装配到主 Agent 和后续派生模板。
///
/// 模板必须在挂载 Host 后捕获；否则 workflow、teammate 等插件派生的 worker 会绕过
/// permission 插件和 Host 启动期的 fail-closed 工具门禁。
#[cfg(feature = "plugins")]
fn assemble_plugin_agents(
    base_agent: Agent,
    live_plugin_host: Arc<LivePluginHost>,
) -> Result<(AgentTemplate, Arc<Agent>)> {
    // 插件在总线上参与决策，因此动态宿主必须挂到主 Agent 的总线上；派生 worker 从
    // 该总线派生子总线，从而继承同一套插件监听并叠加自己的 allowlist 门禁。
    live_plugin_host.attach_bus(base_agent.event_bus())?;
    let agent = base_agent.with_extension(live_plugin_host);
    let template = AgentTemplate::from_agent(&agent);
    Ok((template, Arc::new(agent)))
}

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
    let workspace = WorkspaceContext::capture()?;
    #[cfg(feature = "plugins")]
    let plugin_workspace_scope = workspace.project_id.clone();
    #[cfg(feature = "plugins")]
    let plugin_workspace_root = workspace.cwd.clone();
    let config_path = resolve_config_path(args.config.as_deref())?;
    if args.init {
        initialize_config(&config_path)?;
        println!("Created Lucia configuration: {}", config_path.display());
        println!("Set model.api_key (or model.api_key_env), confirm model.model, then run lucia");
        return Ok(());
    }

    let mut config_exists = config_path.is_file();
    if args.config.is_some() && !config_exists {
        return Err(anyhow!(
            "Configuration file not found: {}",
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
    let configured_agent = if !args.demo && config_exists {
        Some(AgentRootConfig::load(&config_path)?)
    } else {
        None
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
    let mut session_record = load_startup_session(
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
        // 用户禁用列表最后生效。
        remove_disabled_plugin_manifests(&mut plugin_manifests, &disabled_plugins);
    }
    let (gateway, options, demo_mode, mut startup_notices) = if args.demo {
        let (gateway, options) = build_demo_gateway();
        (
            gateway,
            options,
            true,
            vec![
                "Using the local demo model; no external model service will be contacted"
                    .to_string(),
            ],
        )
    } else if config_exists {
        let config = configured_agent
            .as_ref()
            .expect("config_exists 已保证真实配置存在")
            .clone();
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
                .map(|name| format!("set environment variable {name}"))
                .unwrap_or_else(|| {
                    "set model.api_key or model.api_key_env in the configuration".to_string()
                });
            let (gateway, options) = build_demo_gateway();
            (
                gateway,
                options,
                true,
                vec![format!(
                    "No model key detected; using the local demo model. {key_hint}, then restart lucia"
                )],
            )
        }
    } else {
        let (gateway, options) = build_demo_gateway();
        (gateway, options, true, Vec::new())
    };
    let mut options = options;
    let mut reasoning_levels = vec![options.reasoning];
    if !demo_mode {
        if let Some(metadata) = tui_settings.model_metadata.as_ref() {
            if let Some(limit) = metadata.max_output_tokens {
                if tui_settings.max_tokens_explicit {
                    if let Some(configured) = options.max_tokens {
                        if configured > limit {
                            return Err(anyhow!(
                                "配置的 agent.max_tokens {configured} 超过 models.dev 声明的模型输出上限 {limit}"
                            ));
                        }
                    }
                } else {
                    // 未显式配置时直接采用 models.dev 声明的输出上限，避免落回服务商默认值。
                    options.max_tokens = Some(limit);
                }
            }
            reasoning_levels = metadata.reasoning_levels.clone();
            if tui_settings.reasoning_explicit {
                if !reasoning_levels.contains(&options.reasoning) {
                    return Err(anyhow!(
                        "配置的思考等级 {} 不在 models.dev 声明的模型能力中",
                        reasoning_level_label(options.reasoning)
                    ));
                }
            } else if !reasoning_levels.contains(&options.reasoning) {
                options.reasoning = reasoning_levels[0];
            }
        } else {
            startup_notices.push(
                "内置 models.dev 快照中未找到当前模型，继续使用显式配置且不提供思考等级切换"
                    .to_string(),
            );
        }
    }
    let context_window = tui_settings.context_window.or_else(|| {
        tui_settings
            .model_metadata
            .as_ref()
            .and_then(|metadata| metadata.context_window)
    });
    let reasoning_level = options.reasoning;
    let model_input_modalities = tui_settings
        .model_metadata
        .as_ref()
        .map(|metadata| metadata.input_modalities.clone());
    if auto_initialized {
        startup_notices.insert(
            0,
            format!("Created default configuration: {}", config_path.display()),
        );
    }

    let skill_catalog = SkillCatalog::load_local([
        lucia_home.join("skills"),
        workspace.cwd.join("skills"),
        workspace.cwd.join(".lucia/skills"),
    ])
    .context("扫描原生 Skill 目录失败")?;
    if let Some(skill_prompt) = skill_catalog.prompt() {
        options.system_prompt.push_str("\n\n");
        options.system_prompt.push_str(&skill_prompt);
    }
    // 恢复历史会话时同步当前原生能力提示，避免旧 system 消息继续隐藏 Skill 索引。
    session_record
        .session
        .set_system(options.system_prompt.clone());

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
        // 真实模式注入内置工具集：读写文件、列目录、shell、搜索。工作区固定为启动目录。
        let guard = native_workspace_guard(&workspace.cwd)?;
        agent_tool::builtins::register_builtins(&mut native_tools, guard)?;
    }
    skill_catalog.register_tool(&mut native_tools)?;

    #[cfg(feature = "plugins")]
    let plugin_management = Some(Arc::new(PluginManagementService::new(lucia_home.clone())));

    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let model_name = options.model.clone();

    // UI 通道 sink 之外，可选叠加 JSONL sink 用于排查请求与工具调用。
    let mut sink = CompositeEventSink::new();
    let (ui_sink, model_deltas, thinking_deltas) = ChannelEventSink::new(tx.clone());
    sink.push(Arc::new(ui_sink));
    if let Some(path) = events_jsonl.as_ref() {
        sink.push(Arc::new(JsonlEventSink::new(path.clone())));
    }

    let application_events: Arc<dyn EventSink> = Arc::new(sink);
    let base_agent = Agent::new(gateway, options)
        .with_tools(native_tools)
        .with_event_sink(Arc::clone(&application_events));
    // 手动压缩发生在 Agent Run 之外，只写应用事件；两条路径使用相同模型路由和策略实现，
    // 且都不经过 Plugin Host。
    // 压缩水位由真实上下文窗口推导；窗口未知时压缩器按 200k 假定值兜底。
    let manual_context_loader: Arc<dyn ContextLoader> = Arc::new(
        NativeContextLoader::new(
            base_agent.gateway().clone(),
            base_agent.options().provider.clone(),
            base_agent.options().model.clone(),
            base_agent.options().stream,
            DEFAULT_MAX_SUMMARY_TOKENS,
            application_events,
        )?
        .with_context_window(context_window),
    );
    let agent_context_loader: Arc<dyn ContextLoader> = Arc::new(
        NativeContextLoader::new(
            base_agent.gateway().clone(),
            base_agent.options().provider.clone(),
            base_agent.options().model.clone(),
            base_agent.options().stream,
            DEFAULT_MAX_SUMMARY_TOKENS,
            base_agent.event_sink(),
        )?
        .with_context_window(context_window),
    );
    let base_agent = base_agent.with_context_loader(agent_context_loader);
    #[cfg(feature = "plugins")]
    let live_plugin_host = Arc::new(LivePluginHost::new());
    #[cfg(feature = "plugins")]
    let (plugin_agent_template, plugin_agent) =
        assemble_plugin_agents(base_agent, live_plugin_host.clone())?;
    #[cfg(feature = "plugins")]
    let agent = Some(plugin_agent);
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
    // 默认捕获鼠标：滚轮与拖选由 TUI 分别处理，避免终端把滚轮转换为输入框 Up/Down。
    let mouse_capture =
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).is_ok();
    // 启用 bracketed paste，将拖入的文件路径识别为附件。
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
        .with_config_path(config_path.clone())
        .with_context_window(context_window)
        .with_reasoning_levels(reasoning_levels, reasoning_level)
        .with_persistent_session(Arc::clone(&session_store), session_record)
        .with_context_loader(manual_context_loader);
    if let Some(input) = model_input_modalities {
        app = app.with_model_modalities(input);
    }
    #[cfg(feature = "plugins")]
    {
        app = app.with_plugin_management(plugin_management);
    }
    // 终端拒绝启用鼠标捕获时同步真实状态，使 Ctrl+T 下一次可重新尝试开启。
    app.mouse_capture = mouse_capture;
    app.messages.extend(
        startup_notices
            .into_iter()
            .map(|notice| Msg::new(MsgKind::Info, notice)),
    );
    #[cfg(feature = "plugins")]
    let plugin_execution = PluginExecutionContext::new(
        agent_tool::ExecutionPolicy::serve(),
        None,
        tui_settings.review_model.clone(),
        Some(persistent_plugin_state_dir(&lucia_home)),
    )?;
    #[cfg(feature = "plugins")]
    let plugin_agent_runtime = plugin_execution.runtime().clone();
    #[cfg(feature = "plugins")]
    let plugin_load_task = {
        app = app.with_loading_plugins(loading_plugin_ids);
        let load_tx = tx.clone();
        let load_host = Arc::clone(&live_plugin_host);
        let plugin_session_store = Arc::clone(&session_store);

        tokio::spawn(async move {
            let result = load_plugins_for_tui(
                plugin_manifests,
                capability_selection,
                plugin_agent_template,
                plugin_execution,
                plugin_workspace_scope,
                plugin_workspace_root,
                plugin_session_store,
                load_host,
                load_tx.clone(),
            )
            .await;
            let _ = load_tx.send(UiEvent::PluginsLoaded(Box::new(result)));
        })
    };

    loop {
        let rendered = terminal.draw(|frame| {
            render_root(frame, &mut app);
            app.render_text_selection(frame.buffer_mut());
        })?;
        let rendered_buffer = rendered.buffer;

        match rx.recv().await {
            Some(UiEvent::Input(Event::Key(key))) => {
                if key.kind == KeyEventKind::Press {
                    // 界面准入：监听者可以在按键进入任何路由之前吞掉它。
                    if let Some(agent) = agent.as_ref() {
                        let dispatched = agent
                            .event_bus()
                            .dispatch(crate::bus::KeyEvent {
                                code: crate::bus::plugin_key_code(key.code),
                                modifiers: crate::bus::plugin_key_modifiers(key.modifiers),
                                input: app.input.clone(),
                                running: app.running,
                            })
                            .await;
                        if dispatched.is_cancelled() {
                            continue;
                        }
                    }
                    // 诊断模式必须抢在命令面板与插件路由之前，才能看到原始上报组合。
                    if app.key_inspect {
                        app.handle_key(key.code, key.modifiers, agent.as_ref());
                        continue;
                    }
                    if is_copy_shortcut(key.code, key.modifiers) && app.copy_text_selection() {
                        continue;
                    }
                    app.dismiss_text_selection();
                    let before_input = app.input.clone();
                    if app.handle_native_command_key(key.code, key.modifiers, agent.as_ref()) {
                        app.sync_native_command_input(&before_input);
                        #[cfg(feature = "plugins")]
                        if app.input != before_input {
                            if let Some(host) = plugin_host.as_ref() {
                                forward_main_input_snapshot(&mut app, host).await;
                            }
                        }
                        continue;
                    }
                    #[cfg(feature = "plugins")]
                    match app.route_plugin_key(key.code, key.modifiers) {
                        PluginKeyRoute::Main => {
                            let before = (app.input.clone(), app.cursor);
                            let cancel_was_requested = app.run_stream_cancelled;
                            app.handle_key(key.code, key.modifiers, agent.as_ref());
                            if !cancel_was_requested && app.run_stream_cancelled {
                                plugin_agent_runtime.cancel_active_runs().await;
                            }
                            app.sync_native_command_input(&before_input);
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
                                                "Failed to process plugin UI event"
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
                    {
                        app.handle_key(key.code, key.modifiers, agent.as_ref());
                        app.sync_native_command_input(&before_input);
                    }
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
                        if let Err(error) = apply_plugin_footer_event(&mut app, event) {
                            app.messages.push(Msg::new(
                                MsgKind::Error,
                                format!("Plugin footer status failed: {error}"),
                            ));
                            continue;
                        }
                        if let Err(error) = apply_plugin_navigation_event(&mut app, event) {
                            app.messages.push(Msg::new(
                                MsgKind::Error,
                                format!("Plugin view navigation failed: {error}"),
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
                                format!("Failed to refresh plugin UI declarations: {error}"),
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
                if let Some(host) = plugin_host.as_ref() {
                    if let Err(error) = drain_plugin_ui_events(&mut app, host).await {
                        app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!("Failed to process plugin UI event: {error}"),
                        ));
                    }
                }
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
                            "Plugin load planning failed; currently ready plugins were retained: {error}"
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
                            format!(
                                "Plugin `{plugin_id}` became unavailable before the session query completed"
                            ),
                        )),
                        Err(error) => app.messages.push(Msg::new(
                            MsgKind::Error,
                            format!("Failed to return session query results: {error}"),
                        )),
                    }
                }
            }
            Some(UiEvent::ModelStarted) => {
                if app.accepts_run_stream_output() {
                    app.start_model_response();
                }
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
            Some(UiEvent::ModelThinkingReady) => {
                let delta = thinking_deltas
                    .lock()
                    .expect("推理摘要增量缓冲区锁不应中毒")
                    .take();
                if !delta.is_empty() {
                    app.mark_model_thinking();
                }
            }
            Some(UiEvent::ToolStarted(call)) => {
                if !app.accepts_run_stream_output() {
                    continue;
                }
                app.finish_model_thinking();
                #[cfg(feature = "plugins")]
                let call_id = call.id.clone();
                app.messages.push(Msg::tool_started(call));
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    app.schedule_tool_render(Arc::clone(host), &call_id);
                }
            }
            Some(UiEvent::ToolOutputDelta(output)) => {
                if app.accepts_run_stream_output() {
                    if let Some(message) = app
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|message| message.tool_call_id() == Some(output.call_id.as_str()))
                    {
                        message.append_tool_output(&output.delta);
                    }
                }
            }
            Some(UiEvent::ToolFinished(tool_result)) => {
                if !app.accepts_run_stream_output() {
                    continue;
                }
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
                if !app.accepts_run_stream_output() {
                    continue;
                }
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
            }) => {
                if app.accepts_run_stream_output() {
                    app.apply_tool_frame(&call_id, revision, width, result);
                }
            }
            Some(UiEvent::SteeringInjected) => {
                if !app.accepts_run_stream_output() {
                    continue;
                }
                app.messages
                    .push(Msg::new(MsgKind::Info, "Steering applied"));
            }
            Some(UiEvent::FollowUpInjected) => {
                if !app.accepts_run_stream_output() {
                    continue;
                }
                app.messages
                    .push(Msg::new(MsgKind::Info, "Follow-up started"));
            }
            Some(UiEvent::Extension {
                text,
                color,
                divider,
            }) => {
                if !app.accepts_run_stream_output() {
                    continue;
                }
                app.messages.push(Msg::extension(text, color, divider));
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::PluginFooterStatus {
                plugin_id,
                text,
                color,
            }) => {
                app.plugin_footer_statuses.insert(plugin_id, (text, color));
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::ViewNavigation { plugin_id, request }) => {
                if !app.accepts_run_stream_output() {
                    continue;
                }
                if let Err(error) = app.apply_view_navigation(&plugin_id, request) {
                    app.messages.push(Msg::new(
                        MsgKind::Error,
                        format!("Plugin view navigation failed: {error}"),
                    ));
                }
            }
            Some(UiEvent::ContextUsage(tokens)) => {
                app.context_tokens = Some(tokens);
            }
            Some(UiEvent::NativeSessionsLoaded(result)) => {
                app.handle_native_sessions_loaded(result);
            }
            Some(UiEvent::NativeResumeRequested {
                session_id,
                revision,
            }) => {
                if let Err(error) = resume_native_session(&mut app, session_id, revision).await {
                    app.messages
                        .push(Msg::new(MsgKind::Error, format!("恢复会话失败：{error}")));
                }
            }
            Some(UiEvent::NativeCompactRequested) => {
                start_session_context_reload(&mut app, "正在压缩当前上下文".into());
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::NativePluginsLoaded(result)) => {
                app.handle_native_plugins_loaded(result);
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::NativePluginActionFinished(result)) => {
                app.handle_native_plugin_action_finished(result);
            }
            Some(UiEvent::SessionContextReloaded(result)) => {
                app.handle_session_context_reloaded(*result);
            }
            Some(UiEvent::AgentDone(result)) => {
                record_run_failure_event(events_jsonl.as_deref(), result.as_ref()).await;
                let succeeded = result.error.is_none();
                let cancelled = result.run.as_ref().is_some_and(|run| run.cancelled);
                let queue_may_advance = app.handle_agent_done(*result);
                // 会话已持久化、界面状态已更新后再通告，插件据此可安全读取最终会话。
                if let Some(agent) = agent.as_ref() {
                    agent
                        .event_bus()
                        .dispatch(crate::bus::RunFinishEvent {
                            succeeded,
                            cancelled,
                        })
                        .await;
                }
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
                let animate_spinner = app.running || app.pending_reload.is_some();
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
                // 回到底部热区必须先于拖选判定，否则点击会被当成一次选择起点。
                if app.handle_jump_hint_click(&mouse) {
                    continue;
                }
                app.handle_text_selection(&mouse, rendered_buffer);
                if app.handle_tool_message_click(&mouse) {
                    continue;
                }
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
                                        format!("Failed to process plugin UI event: {error}"),
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
                app.dismiss_text_selection();
                // 粘贴只进入主输入框；插件视图聚焦或模态层激活时忽略。
                #[cfg(feature = "plugins")]
                let main_input_focused = !app.native_dialog_active()
                    && app.plugin_focus.is_none()
                    && app.active_dialog_index().is_none()
                    && app.view_stack.is_main();
                #[cfg(not(feature = "plugins"))]
                let main_input_focused = !app.native_dialog_active();
                // 粘贴准入：监听者可以取消粘贴，或改写实际写入主输入区的文本。
                let pasted = match agent.as_ref() {
                    Some(agent) => {
                        let dispatched = agent
                            .event_bus()
                            .dispatch(crate::bus::PasteEvent {
                                text: pasted.clone(),
                            })
                            .await;
                        match dispatched.into_event_unless_cancelled() {
                            Some(event) => event.text,
                            None => continue,
                        }
                    }
                    None => pasted,
                };
                if main_input_focused {
                    let before_input = app.input.clone();
                    app.handle_paste(&pasted);
                    app.sync_native_command_input(&before_input);
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
            Err(_) => Some(anyhow!("Plugin Host shutdown timed out")),
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
            format!("Failed to process plugin UI event: {error}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 插件持久状态目录必须固定落在 Lucia Home 之下。
    #[cfg(feature = "plugins")]
    #[test]
    fn persistent_plugin_state_lives_under_lucia_home() {
        let home = Path::new("/tmp/lucia-home");
        assert_eq!(persistent_plugin_state_dir(home), home.join("plugin-state"));
    }

    /// 插件派生模板必须继承动态 Host，确保 worker 工具同样经过审批门禁。
    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn plugin_worker_template_keeps_live_host_permission_gate() {
        let live_host = Arc::new(LivePluginHost::new());
        let base_agent = Agent::new(
            agent_core::model::ModelGateway::new(),
            agent_core::AgentOptions::default(),
        );
        let (template, _main_agent) =
            assemble_plugin_agents(base_agent, live_host.clone()).expect("装配插件 Agent 应成功");
        let (worker, _) = template
            .instantiate(
                &agent_runtime::AgentPermissions::default(),
                &agent_runtime::AgentDeriveConfig::default(),
            )
            .expect("派生 worker 应成功");

        // 运行快照由 RunStarted 记录事件开启，与真实 Core 的发布顺序一致。
        worker
            .event_bus()
            .dispatch(agent_core::AgentRecordEvent {
                record: agent_core::AgentEvent::new(
                    "worker-run",
                    agent_core::AgentEventKind::RunStarted,
                    0,
                    json!({}),
                ),
            })
            .await;
        let mut call =
            agent_tool::ToolCall::new("worker-read", "read_file", json!({"path": "README.md"}));
        call.bind_run_id("worker-run");
        let dispatched = worker
            .event_bus()
            .dispatch(agent_core::ToolCallEvent::new("worker-run", 0, call))
            .await;

        assert!(dispatched.is_cancelled(), "Host 未就绪时必须 fail-closed");
        let reason = dispatched.event.block_reason.expect("应给出可解释阻止原因");
        let guidance = dispatched.event.agent_guidance.expect("应附带一次性指导");
        assert!(reason.contains("能力归属仍在解析"));
        assert!(guidance.contains("当前调用未执行"));
    }

    /// 原生工具守卫必须固定为 TUI 启动目录。
    #[test]
    fn native_workspace_guard_is_rooted_at_workspace() {
        let root = std::env::temp_dir().join(format!(
            "lucia-workspace-guard-{}",
            agent_session::SessionId::generate()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("应创建测试工作区");

        let guard = native_workspace_guard(&workspace).expect("应创建工作区守卫");
        assert_eq!(
            guard.root(),
            Some(workspace.canonicalize().expect("应规范化工作区").as_path())
        );

        std::fs::remove_dir_all(root).expect("应清理测试工作区");
    }
}
