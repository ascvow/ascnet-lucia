//! TUI 启动组装、运行队列驱动与终端事件循环。

#[cfg(feature = "plugins")]
use crate::app::AgentViewKeyOutcome;
#[cfg(feature = "plugins")]
use crate::app::PluginKeyRoute;
use crate::app::{is_copy_shortcut, reasoning_level_label, App, ChannelEventSink};
use crate::cli::Args;
use crate::command::resume_native_session;
use crate::config::instructions::{load_project_instructions, render_instructions_prompt};
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
use crate::session::title::{flush_session_title, SessionTitleGenerator};
use crate::session::{
    load_startup_session, print_persisted_sessions, resolve_tui_path, start_session_context_reload,
    LazyFileSessionStore, WorkspaceContext,
};
use crate::stream_probe::StreamProbe;
use crate::terminal;
#[cfg(feature = "plugins")]
use crate::theme::PLUGIN_REFRESH_TICKS;
use crate::theme::{REVEAL_FRAME_INTERVAL_MS, UI_TICK_INTERVAL_MS};
#[cfg(feature = "plugins")]
use crate::ui::plugin_bridge::{
    apply_plugin_footer_event, apply_plugin_frames, apply_plugin_navigation_event,
    dispatch_plugin_input, drain_plugin_ui_events, refresh_changed_plugin_focus,
    refresh_plugin_view, refresh_plugin_views_for,
};
use crate::ui::root::render_root;
use agent_context::{NativeContextLoader, DEFAULT_MAX_SUMMARY_TOKENS};
use agent_core::config::AgentRootConfig;
use agent_core::event::{
    AgentEvent, AgentEventKind, CompositeEventSink, EventSink, JsonlEventSink,
};
#[cfg(feature = "plugins")]
use agent_core::model::ModelProviderConfig;
use agent_core::Agent;
#[cfg(feature = "plugins")]
use agent_plugin_host::manifest::load_plugin_runtime_config;
#[cfg(feature = "plugins")]
use agent_plugin_host::wasm::{configure_wasm_cache_directory, ProgressivePluginLoadUpdate};
#[cfg(feature = "plugins")]
use agent_plugin_host::{LivePluginHost, PluginHost};
#[cfg(feature = "plugins")]
use agent_plugin_protocol::{UiInput, UiInputEvent};
#[cfg(feature = "plugins")]
use agent_runtime::AgentTemplate;
use agent_session::SessionStore;
use agent_skill::SkillCatalog;
use agent_tool::{JsonTool, ToolRegistry};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::SynchronizedUpdate;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{json, Value};
#[cfg(feature = "plugins")]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// 返回用户级 Skill 根目录。
///
/// Skill 的安装位置固定在 Lucia Home 下，与配置文件路径无关：`--config` 只改变配置来源，
/// 不改变用户级能力目录。
fn user_skill_root(lucia_home: &Path) -> PathBuf {
    lucia_home.join("skills")
}

/// 初始化用户级 Skill 根目录，返回本次是否新建。
///
/// 只处理 `$LUCIA_HOME/skills`。项目级 `skills/` 与 `.lucia/skills` 位于用户工作区，存在才
/// 扫描：自动创建会在别人的仓库里凭空生成目录，也可能与项目自有同名目录冲突。
///
/// 空目录不改变模型可见行为——不生成索引提示，也不注册 `skill_read`；这里只是把用户
/// 该往哪里放 `SKILL.md` 变成可直接看到的入口。
///
/// # Errors
///
/// 目录无法创建或目标不是非符号链接目录时返回错误，由调用方决定是否降级。
fn initialize_user_skill_root(lucia_home: &Path) -> Result<bool> {
    let root = user_skill_root(lucia_home);
    let existed = root.is_dir();
    agent_skill::ensure_skill_root(&root)?;
    Ok(!existed)
}

/// 返回应用插件持久状态目录。
#[cfg(feature = "plugins")]
fn persistent_plugin_state_dir(lucia_home: &Path, workspace_scope: &str) -> PathBuf {
    lucia_home.join("plugin-state").join(workspace_scope)
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
        // 先注销接管状态：退出途中到达的挂起信号若再写一遍恢复序列，会和下面的清理交叉。
        terminal::release();
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

/// 按配置初始化终端。
///
/// 全屏模式进入终端备用屏幕（类似 vim），退出后恢复原屏幕内容；关闭时直接在主屏绘制，
/// 退出后保留最后一帧。两种模式都启用 raw mode，按键与鼠标处理保持一致。
fn init_terminal(fullscreen: bool) -> Result<ratatui::DefaultTerminal> {
    let mut terminal = if fullscreen {
        ratatui::init()
    } else {
        crossterm::terminal::enable_raw_mode().context("启用终端原始模式失败")?;
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .context("初始化终端失败")?
    };
    // 首帧的差分缓冲区默认全空白，不能依赖终端在切入备用屏幕时恰好也已清空。
    // 两种模式都先显式清屏，使真实屏幕与 Ratatui 的初始状态一致；后续仍只输出差分。
    clear_without_cursor_query(&mut terminal).context("清空终端失败")?;
    Ok(terminal)
}

/// 两种模式都由应用定位整屏光标；不要调用会等待 DSR 回复的 Terminal::clear。
/// 重置当前尺寸同时清屏并失效差分缓存，兼容不回应 DSR 的终端和 PTY。
fn clear_without_cursor_query(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let size = terminal.size().context("读取终端尺寸失败")?;
    terminal
        .resize(Rect::new(0, 0, size.width, size.height))
        .context("重置终端视口失败")
}

/// 在运行期切换全屏渲染。
///
/// 终端由主循环持有，`/tui` 只登记意图；这里完成备用屏幕的进出并强制清屏，避免残留
/// 上一次渲染模式的画面。
fn apply_fullscreen_switch(
    terminal: &mut ratatui::DefaultTerminal,
    fullscreen: bool,
) -> Result<()> {
    use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
    if fullscreen {
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen).context("进入备用屏幕失败")?;
    } else {
        crossterm::execute!(std::io::stdout(), LeaveAlternateScreen).context("退出备用屏幕失败")?;
    }
    // 挂起处理器按这份状态决定是否退出备用屏幕，切换后必须同步。
    terminal::set_screen(fullscreen);
    clear_without_cursor_query(terminal).context("切换渲染模式后清屏失败")?;
    Ok(())
}

/// 从挂起或后台作业重新接管终端。
///
/// 挂起时处理器已经退出备用屏幕并交还终端属性，这里按启动时的能力集合原样重建：
/// raw mode、备用屏幕、键盘增强、鼠标捕获与括号粘贴，最后清屏让下一帧整屏重绘。
/// 返回鼠标捕获的实际状态：终端可能拒绝该能力，`Ctrl+T` 需要据此重试。
fn resume_terminal(
    terminal: &mut ratatui::DefaultTerminal,
    fullscreen: bool,
    keyboard_enhanced: bool,
    mouse_capture: bool,
) -> Result<bool> {
    use crossterm::terminal::EnterAlternateScreen;
    crossterm::terminal::enable_raw_mode().context("恢复终端原始模式失败")?;
    if fullscreen {
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen).context("恢复备用屏幕失败")?;
    }
    if keyboard_enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    let mouse_capture = mouse_capture
        && crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).is_ok();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    terminal.show_cursor().context("恢复光标显示失败")?;
    clear_without_cursor_query(terminal).context("恢复后清屏失败")?;
    terminal::mark_attached();
    Ok(mouse_capture)
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
        // `--init` 同时准备用户级 Skill 入口，使首次安装后就有可放 `SKILL.md` 的目录；
        // 目录创建失败只提示，不撤销已成功创建的配置文件。
        let lucia_home = lucia_home_dir()?;
        match initialize_user_skill_root(&lucia_home) {
            Ok(true) => println!(
                "Created Skill directory: {}",
                user_skill_root(&lucia_home).display()
            ),
            Ok(false) => {}
            Err(error) => eprintln!("Warning: 无法初始化 Skill 目录：{error:#}"),
        }
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
                .default_upstream()
                .ok()
                .and_then(|upstream| upstream.api_key_env.clone())
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
        let Some(metadata) = tui_settings.model_metadata.as_ref() else {
            // 思考强度、输出上限、模态校验全部只来自 models.dev，缺了它就是"不知道这个
            // 模型能做什么"。继续启动只能猜，猜错会在请求阶段以服务商报错的形式出现，
            // 比在这里拒绝更难定位。
            let upstream = configured_agent
                .as_ref()
                .and_then(|config| config.default_upstream().ok());
            let model_id = upstream
                .map(|upstream| upstream.model.as_str())
                .unwrap_or("<未知>");
            let base_url = upstream
                .and_then(|upstream| upstream.base_url.as_deref())
                .map(|url| format!("（base_url {url}）"))
                .unwrap_or_default();
            return Err(anyhow!(
                "models.dev 快照中没有模型 `{model_id}`{base_url}；请确认配置里的 model 与 models.dev 声明的模型 ID 一致，或在配置里用 models_dev_provider 指定服务商"
            ));
        };
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

    #[cfg(not(feature = "plugins"))]
    startup_notices.push("纯 Core 模式未配置工具审批策略；Shell 属于完整原生进程信任。".into());
    if let Some(config) = configured_agent.as_ref() {
        for upstream in config.upstreams() {
            if upstream
                .base_url
                .as_deref()
                .is_some_and(|url| url.trim().starts_with("http://"))
            {
                // 不回显 URL 或凭证，只提示传输语义；本地端点也不冒充 TLS。
                startup_notices
                    .push("注意：模型端点使用明文 HTTP，凭证和会话内容不受 TLS 保护。".into());
                break;
            }
        }
    }

    // 项目约定并入系统提示：模型从第一次请求就知道本项目的规矩，且随 system 段进入
    // 提示缓存前缀，不会每轮重复计费。
    let project_instructions =
        load_project_instructions(&lucia_home, &workspace.cwd).context("加载项目约定失败")?;
    let instructions_count = project_instructions.len();
    if let Some(instructions_prompt) = render_instructions_prompt(&project_instructions) {
        options.system_prompt.push_str("\n\n");
        options.system_prompt.push_str(&instructions_prompt);
    }
    if instructions_count > 0 {
        startup_notices.push(format!(
            "已加载 {instructions_count} 份项目约定 (AGENTS.md)"
        ));
    }

    // 扫描前先确保用户级 Skill 入口存在：空目录不改变提示与工具注册，但用户能直接看到
    // 该往哪里放 SKILL.md。创建失败只降级为启动提示，工作区内的 Skill 仍然可用。
    if let Err(error) = initialize_user_skill_root(&lucia_home) {
        startup_notices.push(format!("{error:#}"));
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

    // 用户在消息里显式引用的工作区外路径：TUI 在提交时登记，只读工具按需访问。
    let read_path_grants = agent_tool::ReadPathGrants::new();
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
        agent_tool::builtins::register_builtins(
            &mut native_tools,
            guard,
            read_path_grants.clone(),
        )?;
    }
    skill_catalog.register_tool(&mut native_tools)?;

    #[cfg(feature = "plugins")]
    let plugin_management = Some(Arc::new(PluginManagementService::new(lucia_home.clone())));

    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let model_name = options.model.clone();

    // UI 通道 sink 之外，可选叠加 JSONL sink 用于排查请求与工具调用。
    let mut sink = CompositeEventSink::new();
    // 流式诊断探针：只有显式设置 LUCIA_STREAM_DEBUG=1 时才创建，默认零开销。
    let probe = StreamProbe::from_env();
    let (ui_sink, model_deltas, thinking_deltas, tool_deltas) = ChannelEventSink::new(tx.clone());
    let ui_sink = ui_sink.with_probe(probe.clone());
    sink.push(Arc::new(ui_sink));
    if let Some(path) = events_jsonl.as_ref() {
        sink.push(Arc::new(JsonlEventSink::new(path.clone())));
    }

    let application_events: Arc<dyn EventSink> = Arc::new(sink);
    // 会话标题复用主 Agent 的模型路由；demo 脚本模型没有语义，起名只会得到固定噪声。
    let title_generator = if demo_mode {
        None
    } else {
        Some(Arc::new(SessionTitleGenerator::new(
            gateway.clone(),
            options.provider.clone(),
            options.model.clone(),
            options.stream,
        )?))
    };
    // 插件默认路由要按名索引网关里已注册的服务商，先留存实际生效的名称。
    #[cfg(feature = "plugins")]
    let active_provider_name = options.provider.clone();
    let base_agent = Agent::new(gateway, options)
        .with_tools(native_tools)
        .with_event_sink(Arc::clone(&application_events));
    // 手动与自动压缩共用加载器，确保模型热切换同时更新两条路径的路由与窗口。
    // 手动压缩发生在 Agent Run 之外，仍只写应用事件，不经过 Plugin Host。
    // 压缩水位由真实上下文窗口推导；窗口未知时压缩器按 200k 假定值兜底。
    let native_context_loader = Arc::new(
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
    let base_agent = base_agent.with_context_loader(native_context_loader.clone());
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

    // 作业控制处理器必须先于 raw mode 安装：原始终端属性只有那时读得到，挂起后的
    // 恢复也只能靠这份快照，不能用 crossterm 的全局状态。
    let fullscreen = tui_settings.fullscreen.unwrap_or(true);
    terminal::install(fullscreen);
    // 初始化和首次清屏也可能失败；必须在接管之前建立守卫，覆盖这些提前返回路径。
    let mut terminal_guard = TerminalGuard {
        keyboard_enhanced: false,
    };
    let mut terminal = init_terminal(fullscreen)?;
    let terminal_colors = crate::theme::TerminalColors::from_env();
    // 键盘增强协议让 Shift+Enter 等修饰组合可被区分（传统协议下它就是裸 Enter）。
    // 该查询会读取终端应答，必须在输入读取线程启动前完成，否则应答被线程吞掉。
    let keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    terminal::set_keyboard_enhanced(keyboard_enhanced);
    if keyboard_enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    terminal_guard.keyboard_enhanced = keyboard_enhanced;
    // 默认捕获鼠标：滚轮与拖选由 TUI 分别处理，避免终端把滚轮转换为输入框 Up/Down。
    let mouse_capture =
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).is_ok();
    terminal::set_mouse_capture(mouse_capture);
    // 启用 bracketed paste，将拖入的文件路径识别为附件。
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    // 协议全部就绪后登记接管：此前的挂起信号没有终端状态需要恢复。
    terminal::mark_attached();

    let input_tx = tx.clone();
    std::thread::spawn(move || {
        // 读失败可能是挂起/恢复期间的 EINTR、EIO（后台读终端），不能就此退出：
        // 否则恢复后键盘彻底失效。只有事件通道关闭才代表应用结束。
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    for event in coalesce_injected_paste(event) {
                        if input_tx.send(UiEvent::Input(event)).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => {
                    // 恢复接管由主循环的 tick 负责，这里只需退避，避免后台读错误空转。
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if input_tx.is_closed() {
                        return;
                    }
                }
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
        .with_async_attachments()
        .with_workspace(workspace)
        .with_read_path_grants(read_path_grants)
        .with_config_path(config_path.clone())
        .with_fullscreen(fullscreen)
        .with_context_window(context_window)
        .with_reasoning_levels(reasoning_levels, reasoning_level)
        .with_persistent_session(Arc::clone(&session_store), session_record)
        .with_native_context_loader(native_context_loader);
    // /model 面板的上游列表来自配置文件；demo 模式没有配置，保持为空。
    if let Some(config) = configured_agent.as_ref() {
        app.model_upstreams = config.upstreams().into_iter().cloned().collect();
    }
    // 底栏展示的配置名只在真实上游生效时存在：密钥缺失回退演示模型时配置项并未生效。
    if !demo_mode {
        if let Some(upstream) = configured_agent
            .as_ref()
            .and_then(|config| config.default_upstream().ok())
        {
            app.model_upstream_name = Some(upstream.name.clone());
        }
    }
    if let Some(generator) = title_generator {
        app = app.with_title_generator(generator);
    }
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
    // 插件默认路由复用主 Agent 实际生效的服务商；配置了专属路由的插件会覆盖它。
    // 判断依据是 demo_mode 而不是配置是否存在：密钥缺失时主 Agent 也会回退到演示模型，
    // 此时配置里的服务商并未注册到网关。
    #[cfg(feature = "plugins")]
    let plugin_default_provider = match configured_agent.as_ref().filter(|_| !demo_mode) {
        Some(config) => config.default_upstream()?.to_provider_config()?,
        // 演示模型由应用直接注册到网关，没有对应的服务商配置，这里只需要它的名称。
        None => ModelProviderConfig::new(
            active_provider_name,
            agent_core::model::ProviderKind::OpenAiCompatible,
            String::new(),
        ),
    };
    #[cfg(feature = "plugins")]
    let mut plugin_execution = PluginExecutionContext::new(
        agent_tool::ExecutionPolicy::serve(),
        None,
        tui_settings.review_model.clone(),
        plugin_default_provider,
        Some(persistent_plugin_state_dir(
            &lucia_home,
            &plugin_workspace_scope,
        )),
    )?;
    #[cfg(feature = "plugins")]
    let capability_audit = args
        .capability_audit_dir
        .as_ref()
        .map(|path| agent_plugin_host::audit::RotatingCapabilityAudit::open(path).map(Arc::new))
        .transpose()?;
    #[cfg(feature = "plugins")]
    {
        plugin_execution.audit = capability_audit.clone();
    }
    #[cfg(feature = "plugins")]
    let mut reported_audit_drops = 0;
    #[cfg(feature = "plugins")]
    let plugin_agent_runtime = plugin_execution.runtime().clone();
    // 插件配置与主配置同源；解析失败直接终止启动，避免带着半套路由进入交互。
    #[cfg(feature = "plugins")]
    let plugin_configs = crate::plugin::config::load_main_plugin_configs(&config_path)?;
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
                plugin_configs,
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

    // 首帧必须绘制；之后只在状态可能变化时重绘，空闲 tick 不再整帧刷新。
    let mut needs_redraw = true;
    let mut rendered_buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
    loop {
        // 原生 Agent 会话视图的订阅与导航栈保持一致：新建、换句柄、关闭都只在这里收敛。
        // 同步是纯内存操作，没有原生视图时立即返回。
        #[cfg(feature = "plugins")]
        if let Some(host) = plugin_host.as_ref() {
            app.sync_agent_views(host.as_ref()).await;
        }
        // `/tui` 只登记意图，真正的备用屏幕切换在这里完成。
        if let Some(fullscreen) = app.pending_fullscreen.take() {
            apply_fullscreen_switch(&mut terminal, fullscreen)?;
            needs_redraw = true;
        }
        // 挂起恢复：处理器交还过终端，这里按启动时的能力集合重新接管并整屏重绘。
        if terminal::take_resume_request() {
            app.mouse_capture = resume_terminal(
                &mut terminal,
                app.fullscreen,
                keyboard_enhanced,
                app.mouse_capture,
            )?;
            terminal::set_mouse_capture(app.mouse_capture);
            needs_redraw = true;
        }
        // Ctrl+Z 在 raw mode 下不会触发终端自己的 SIGTSTP：先由处理器恢复终端，再真正停止。
        if std::mem::take(&mut app.pending_suspend) {
            terminal::suspend();
            // 返回即代表已被继续，但终端此刻仍在 shell 手里：重新接管由下一轮的
            // resume 分支完成。这里必须直接回到循环顶部，否则这一帧会画进用户
            // shell 的主屏幕（交还期间写入的任何内容都会留在 scrollback 里）。
            needs_redraw = true;
            continue;
        }
        // 打字机揭示：先按本帧预算把已收到但未显示的文本铺上屏，再决定是否重绘。
        // 这一步让"屏幕节奏"由帧时钟决定，而不是由网络到达节奏决定。
        let revealed = app.reveal_pending_stream();
        if revealed > 0 {
            needs_redraw = true;
        }
        if needs_redraw && !terminal::is_detached() {
            // 同步更新事务：终端在收到结束标记前不刷新画面，避免一帧内的多次写入被
            // 逐条呈现（高帧率下表现为闪烁/撕裂）。不支持该扩展的终端会忽略它。
            let frame_started = std::time::Instant::now();
            let mut stdout = std::io::stdout();
            let rendered = stdout.sync_update(|_| {
                terminal.draw(|frame| {
                    render_root(frame, &mut app);
                    app.render_text_selection(frame.buffer_mut());
                    crate::theme::apply_terminal_theme(frame.buffer_mut(), terminal_colors);
                    crate::ui::sanitize_terminal_buffer(frame.buffer_mut());
                })
            })??;
            // 复制一份帧缓冲区：鼠标选择需要读取刚绘制的内容，而终端在下一次绘制时
            // 会复用同一块缓冲区，跨迭代持有借用会与可变借用冲突。
            rendered_buffer.clone_from(rendered.buffer);
            needs_redraw = false;
            if let Some(probe) = probe.as_ref() {
                probe.record_frame(frame_started.elapsed(), revealed);
            }
        }

        // 有待揭示文本时按揭示帧时钟等待：既不会漏掉网络事件，也不会因为网络静止而
        // 停止出帧。没有待揭示文本时保持纯事件驱动，空闲不空转。
        let event = if app.has_pending_stream() {
            match tokio::time::timeout(
                std::time::Duration::from_millis(REVEAL_FRAME_INTERVAL_MS),
                rx.recv(),
            )
            .await
            {
                Ok(event) => event,
                // 超时只代表"这一帧没有新事件"：继续揭示下一批，不算通道关闭。
                Err(_) => continue,
            }
        } else {
            rx.recv().await
        };
        // 普通移动由实际悬停变化及插件路由标脏，不丢弃输入事件。
        needs_redraw |= event_requires_redraw(event.as_ref(), app.tick_requires_redraw());

        match event {
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
                    #[cfg(feature = "plugins")]
                    let before_session = app.session_record.id.clone();
                    // 原生 Agent 会话视图自己处理按键：编辑、滚动与提交都在 TUI 内完成，
                    // 只有提交时才把一条 Submit 交给发起插件。
                    #[cfg(feature = "plugins")]
                    if app.active_agent_session_route().is_some() {
                        match app.handle_agent_session_key(key.code, key.modifiers) {
                            AgentViewKeyOutcome::Submit {
                                plugin_id,
                                view_id,
                                instance_id,
                                text,
                            } => {
                                if let Some(host) = plugin_host.as_ref() {
                                    let input = UiInput {
                                        plugin_id: plugin_id.clone(),
                                        view_id,
                                        instance_id: Some(instance_id),
                                        event: UiInputEvent::Submit { text },
                                    };
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
                                            drain_plugin_ui_events(&mut app, host).await
                                        {
                                            app.messages.push(Msg::new(
                                                MsgKind::Error,
                                                format!(
                                                    "Failed to process plugin UI event: {error}"
                                                ),
                                            ));
                                        }
                                        refresh_plugin_views_for(
                                            &mut app,
                                            host.as_ref(),
                                            &plugin_id,
                                        )
                                        .await;
                                    }
                                }
                            }
                            AgentViewKeyOutcome::Exit | AgentViewKeyOutcome::Consumed => {}
                        }
                        continue;
                    }
                    // 排队面板聚焦时吞掉按键：先于命令补全与插件路由，避免字符落进输入框。
                    let queue_keys_active =
                        app.queue_focus.is_some() && !app.native_dialog_active();
                    #[cfg(feature = "plugins")]
                    let queue_keys_active = queue_keys_active
                        && app.view_stack.is_main()
                        && app.plugin_focus.is_none()
                        && app.active_dialog_index().is_none();
                    if queue_keys_active {
                        #[cfg(feature = "plugins")]
                        let cancel_was_requested = app.run_stream_cancelled;
                        app.handle_queue_key(key.code, key.modifiers, agent.as_ref());
                        #[cfg(feature = "plugins")]
                        if !cancel_was_requested && app.run_stream_cancelled {
                            plugin_agent_runtime.cancel_active_runs().await;
                        }
                        continue;
                    }
                    if app.handle_native_command_key(key.code, key.modifiers, agent.as_ref()) {
                        app.sync_native_command_input(&before_input);
                        #[cfg(feature = "plugins")]
                        if app.input != before_input {
                            if let Some(host) = plugin_host.as_ref() {
                                forward_main_input_snapshot(&mut app, host).await;
                            }
                        }
                        // `/new`、`/clear` 与恢复会话会在这里切换会话，插件视图必须立即
                        // 按新会话重绘，否则会继续展示上一个会话的会话级状态。
                        #[cfg(feature = "plugins")]
                        if app.session_record.id != before_session {
                            if let Some(host) = plugin_host.as_ref() {
                                app.schedule_plugin_views_refresh(Arc::clone(host));
                            }
                        }
                        continue;
                    }
                    #[cfg(feature = "plugins")]
                    let previous_plugin_focus = app.plugin_focus;
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
                        PluginKeyRoute::Consumed => {
                            if let Some(host) = plugin_host.as_ref() {
                                refresh_changed_plugin_focus(
                                    &mut app,
                                    host.as_ref(),
                                    previous_plugin_focus,
                                )
                                .await;
                            }
                        }
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
            Some(UiEvent::PluginFrameLoaded(rendered)) => {
                apply_plugin_frames(&mut app, vec![rendered]);
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::ContextRefsLoaded(outcomes)) => {
                app.context_ref_task = None;
                if app.apply_context_refs(outcomes) {
                    needs_redraw = true;
                }
            }
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
            Some(UiEvent::PluginsLoaded(result)) => {
                match *result {
                    Ok(()) => {
                        app.finish_progressive_plugin_loading();
                        if plugin_host.as_ref().is_some_and(|host| {
                            matches!(host.planned_capability_owner("agent.tool-policy"), Ok(None))
                        }) {
                            app.messages.push(Msg::new(MsgKind::Info, "未配置工具审批策略；文件工具边界仍生效，Shell 属于完整原生进程信任。"));
                        }
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
                }
            }
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
            #[cfg(feature = "plugins")]
            Some(UiEvent::AgentViewEvents {
                plugin_id,
                instance_id,
                target,
                events,
            }) => {
                app.apply_agent_view_events(&plugin_id, &instance_id, &target, events)
                    .await;
            }
            #[cfg(feature = "plugins")]
            Some(UiEvent::AgentViewClosed {
                plugin_id,
                instance_id,
                target,
            }) => {
                app.apply_agent_view_closed(&plugin_id, &instance_id, &target)
                    .await;
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
                    // 只入缓冲，由每帧的揭示预算决定上屏多少：一次突发的整段文本不会
                    // 变成一次整屏重排。
                    app.queue_model_delta(&delta);
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
                // 工具卡片是正文结束边界：先补完当前批次，再收起光标，后续轮次另起消息。
                app.start_model_response();
                #[cfg(feature = "plugins")]
                let call_id = call.id.clone();
                app.messages.push(Msg::tool_started(call));
                #[cfg(feature = "plugins")]
                if let Some(host) = plugin_host.as_ref() {
                    app.schedule_tool_render(Arc::clone(host), &call_id);
                }
            }
            Some(UiEvent::ToolOutputReady) => {
                // 一次取出本帧积累的全部 chunk：命令狂打日志时事件率不再等于帧率。
                let deltas = tool_deltas.lock().expect("工具输出缓冲区锁不应中毒").take();
                if deltas.is_empty() || !app.accepts_run_stream_output() {
                    continue;
                }
                for delta in deltas {
                    if let Some(message) = app
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|message| message.tool_call_id() == Some(delta.call_id.as_str()))
                    {
                        message.append_tool_stream(delta.stream, &delta.delta);
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
                app.start_model_response();
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
            Some(UiEvent::ModelUsage(usage)) => {
                // 输入合计代表本轮上下文规模：未缓存输入加上缓存读写都要真实送达模型。
                let context_tokens = usage
                    .input_tokens
                    .into_iter()
                    .chain(usage.cache_read_tokens)
                    .chain(usage.cache_write_tokens)
                    .fold(0, u64::saturating_add);
                app.context_tokens = Some(context_tokens);
                app.session_usage.add_assign(&usage);
            }
            Some(UiEvent::ContextSnapshot(snapshot)) => {
                app.context_snapshot = Some(snapshot);
                app.context_tokens = None;
            }
            Some(UiEvent::SessionTitleReady { session_id, result }) => {
                app.handle_session_title(session_id, result);
                flush_session_title(&mut app).await;
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
                } else {
                    // 会话级插件状态（例如计划进度架）必须立刻切换，不能等周期刷新。
                    #[cfg(feature = "plugins")]
                    if let Some(host) = plugin_host.as_ref() {
                        app.schedule_plugin_views_refresh(Arc::clone(host));
                    }
                }
            }
            Some(UiEvent::NativeCompactRequested) => {
                start_session_context_reload(&mut app, "Compacting context".into());
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
            Some(UiEvent::AttachmentsPrepared { id, result }) => {
                let before = app.input.clone();
                #[cfg(feature = "plugins")]
                let before_session = app.session_record.id.clone();
                #[cfg(feature = "plugins")]
                let cancel_was_requested = app.run_stream_cancelled;
                app.apply_prepared_attachments(id, result);
                #[cfg(feature = "plugins")]
                if !cancel_was_requested && app.run_stream_cancelled {
                    plugin_agent_runtime.cancel_active_runs().await;
                }
                #[cfg(feature = "plugins")]
                if app.session_record.id != before_session {
                    if let Some(host) = plugin_host.as_ref() {
                        app.schedule_plugin_views_refresh(Arc::clone(host));
                    }
                }
                if before != app.input {
                    app.sync_native_command_input(&before);
                    #[cfg(feature = "plugins")]
                    if let Some(host) = plugin_host.as_ref() {
                        forward_main_input_snapshot(&mut app, host).await;
                    }
                }
            }
            Some(UiEvent::AgentDone(result)) => {
                record_run_failure_event(events_jsonl.as_deref(), result.as_ref()).await;
                let succeeded = result.error.is_none();
                let cancelled = app.run_stream_cancelled
                    || result.run.as_ref().is_some_and(|run| run.cancelled);
                let queue_may_advance = app.handle_agent_done(*result);
                // 标题只在空闲时写入：排队输入会立刻开启下一轮，那时保存会与该轮的
                // CAS 抢同一个 revision，把正常保存挤进分叉路径。
                flush_session_title(&mut app).await;
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
                let animate_spinner = app.running
                    || app.plugins_loading
                    || app.pending_reload.is_some()
                    || app.active_agent_session_view().is_some_and(|view| {
                        !view.closed
                            && matches!(
                                view.status,
                                Some(
                                    agent_runtime::AgentStatus::Queued
                                        | agent_runtime::AgentStatus::Running
                                )
                            )
                    });
                #[cfg(not(feature = "plugins"))]
                let animate_spinner = app.running || app.pending_reload.is_some();
                if animate_spinner {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
                #[cfg(feature = "plugins")]
                {
                    if let Some(audit) = capability_audit.as_ref() {
                        let dropped = audit.dropped_count();
                        if dropped != reported_audit_drops {
                            reported_audit_drops = dropped;
                            app.messages.push(Msg::new(
                                MsgKind::Error,
                                format!(
                                    "能力审计记录不完整：已丢失 {dropped} 条，请检查目录或磁盘。"
                                ),
                            ));
                            needs_redraw = true;
                        }
                    }
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
                    // 上下文引用按固定节拍拉取：Eden 一类的编辑器协议 v1 没有推送通道，
                    // 由应用在空闲时索要快照，用户选中后无需再手动触发任何工具。
                    // 拉取本身在后台任务里完成，避免 Eden 无响应时卡住渲染与输入。
                    if let Some(host) = plugin_host.as_ref() {
                        app.schedule_context_ref_poll(Arc::clone(host));
                    }
                }
            }
            Some(UiEvent::Input(Event::Mouse(mouse))) => {
                // 原生会话视图独占屏幕：滚轮与工具展开都作用于视图自己的时间线，
                // 不能再落到主界面的选区与滚动条热区上。
                #[cfg(feature = "plugins")]
                if app.active_agent_session_route().is_some() {
                    needs_redraw |= app.handle_mouse_event(&mouse);
                    app.handle_agent_session_mouse(&mouse);
                    continue;
                }
                // 悬停态先更新：它只影响滚动条的宽度与配色，不消费事件。
                needs_redraw |= app.handle_mouse_event(&mouse);
                // 滚动条与回到底部热区都先于拖选判定：否则按下会被当成一次选择起点。
                if app.handle_scrollbar_drag(&mouse) {
                    continue;
                }
                if app.handle_jump_hint_click(&mouse) {
                    continue;
                }
                // 底栏上下文芯片是 `/context` 的鼠标入口，先于文本选区判定。
                if app.handle_context_chip_click(&mouse) {
                    continue;
                }
                // 排队消息面板位于输入框上方，点击行把该消息取回编辑；必须先于文本选区。
                if app.handle_queued_input_click(&mouse) {
                    continue;
                }
                let selection_before = (app.text_selection, app.has_copyable_text_selection());
                app.handle_text_selection(&mouse, &rendered_buffer);
                needs_redraw |=
                    selection_before != (app.text_selection, app.has_copyable_text_selection());
                if app.handle_tool_message_click(&mouse) {
                    continue;
                }
                #[cfg(feature = "plugins")]
                {
                    let dialog_active = app.active_dialog_index().is_some();
                    let previous_plugin_focus = app.plugin_focus;
                    if let Some(input) = app.route_plugin_mouse(&mouse) {
                        needs_redraw = true;
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
                            if previous_plugin_focus != app.plugin_focus {
                                if let Some(previous) = previous_plugin_focus
                                    .and_then(|index| app.plugin_views.get(index))
                                {
                                    let plugin_id = previous.declaration.plugin_id.clone();
                                    let view_id = previous.declaration.view_id.clone();
                                    refresh_plugin_view(
                                        &mut app,
                                        host.as_ref(),
                                        &plugin_id,
                                        &view_id,
                                    )
                                    .await;
                                }
                            }
                        }
                    } else if !dialog_active {
                        if let Some(host) = plugin_host.as_ref() {
                            needs_redraw |= refresh_changed_plugin_focus(
                                &mut app,
                                host.as_ref(),
                                previous_plugin_focus,
                            )
                            .await;
                        }
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
                #[cfg(feature = "plugins")]
                let agent_session_focused = app.active_agent_session_route().is_some();
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
                #[cfg(feature = "plugins")]
                if agent_session_focused {
                    app.handle_agent_session_paste(&pasted);
                    continue;
                }
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
    // 诊断报告在终端恢复之后写：文件写入与提示都不能打扰 TUI 画面。
    if let Some(probe) = probe.as_ref() {
        if let Some(path) = probe.write_report(&lucia_home) {
            println!("流式诊断报告已写入 {}", path.display());
        }
    }
    #[cfg(feature = "plugins")]
    if let Some(audit) = capability_audit {
        if let Err(error) = audit.flush(std::time::Duration::from_secs(2)) {
            eprintln!("能力审计刷新失败：{error}");
        }
        if audit.dropped_count() > 0 {
            eprintln!("能力审计丢失 {} 条记录", audit.dropped_count());
        }
    }
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

/// 注入式粘贴合并所需的最少字符数。
///
/// 终端或宿主程序把整段文本直接写进 PTY、不发送 bracketed paste 标记时，
/// crossterm 会把粘贴拆成逐字符按键。只有同一次读取后立即可读的字符才会被合并，
/// 而人工打字的间隔远大于读取循环，因此这个阈值不会把正常输入误判成粘贴。
const INJECTED_PASTE_MIN_CHARS: usize = 4;

/// 把一次连续写入的字符按键合并成粘贴事件，返回按原顺序转发的事件列表。
///
/// 部分终端（以及把粘贴路径直接写进 PTY 的宿主程序）不发送 bracketed paste 标记，
/// 路径粘贴因此退化成逐字符输入，附件识别与长文本折叠都不会触发。这里把同一次
/// 写入产生的普通字符合并为 [`Event::Paste`]，让它们与终端原生粘贴走同一条处理
/// 链路；紧随其后的非字符事件（回车、鼠标等）保持在粘贴之后，顺序不变。
fn coalesce_injected_paste(first: Event) -> Vec<Event> {
    if plain_char(&first).is_none() {
        return vec![first];
    }
    let mut events = vec![first];
    // `poll(0)` 只检查已经缓冲的事件：同一次 write 的剩余字符会立即命中，
    // 人类的下一次按键则尚未到达。
    while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let Ok(event) = crossterm::event::read() else {
            break;
        };
        let is_plain_char = plain_char(&event).is_some();
        events.push(event);
        // 非字符事件属于这次写入之后的独立操作，不能并入粘贴正文。
        if !is_plain_char {
            break;
        }
    }
    coalesce_event_batch(events)
}

/// 判断事件是否是可合并的普通字符按键。
///
/// 终端上报大写字母时会附带 `SHIFT`，它仍属于路径正文；`Ctrl`/`Alt` 组合则可能
/// 是快捷键，必须中断合并。
fn plain_char(event: &Event) -> Option<char> {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) if modifiers.difference(KeyModifiers::SHIFT).is_empty() => Some(*ch),
        _ => None,
    }
}

/// 把一批以普通字符开头的事件合并成一次粘贴，返回按原顺序转发的事件。
///
/// 字符不足阈值时原样返回，保持普通逐键输入的语义；末尾若跟着非字符事件，
/// 它必须排在粘贴之后，不能被吞进粘贴正文。
fn coalesce_event_batch(mut events: Vec<Event>) -> Vec<Event> {
    let text: String = events.iter().filter_map(plain_char).collect();
    if text.chars().count() < INJECTED_PASTE_MIN_CHARS {
        return events;
    }
    let tail = events.pop().expect("至少包含首个字符事件");
    if plain_char(&tail).is_some() {
        vec![Event::Paste(text)]
    } else {
        vec![Event::Paste(text), tail]
    }
}

// ─── Demo 模型 ───

/// 普通移动完整路由但不预先标脏；点击、拖动、滚轮保守重绘。
fn event_requires_redraw(event: Option<&UiEvent>, tick: bool) -> bool {
    match event {
        Some(UiEvent::Tick) => tick,
        Some(UiEvent::Input(Event::Mouse(mouse))) => mouse.kind != MouseEventKind::Moved,
        Some(UiEvent::Input(Event::Key(key))) => key.kind == KeyEventKind::Press,
        None => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 插件持久状态目录必须同时按 Lucia Home 和项目作用域隔离。
    #[cfg(feature = "plugins")]
    #[test]
    fn persistent_plugin_state_lives_under_project_scope() {
        let home = Path::new("/tmp/lucia-home");
        assert_eq!(
            persistent_plugin_state_dir(home, "project-a"),
            home.join("plugin-state").join("project-a")
        );
        assert_ne!(
            persistent_plugin_state_dir(home, "project-a"),
            persistent_plugin_state_dir(home, "project-b")
        );
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
        assert!(guidance.starts_with("<tool_admission>"));
        assert!(guidance.ends_with("</tool_admission>"));
        assert!(guidance.contains("this call was not executed"));
    }

    /// 非热点移动不重绘，悬停改变仍会标脏；其他鼠标输入不丢。
    #[test]
    fn pointer_redraw_preserves_hover_and_interactions() {
        use crossterm::event::{MouseButton, MouseEvent};
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "test".into());
        app.scrollbar_area = Some(Rect::new(80, 2, 1, 20));
        for (column, expected) in [(40, false), (80, true), (80, false), (40, true)] {
            let mouse = MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row: 5,
                modifiers: KeyModifiers::NONE,
            };
            assert!(!event_requires_redraw(
                Some(&UiEvent::Input(Event::Mouse(mouse))),
                false
            ));
            assert_eq!(app.handle_mouse_event(&mouse), expected);
        }
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
        ] {
            let event = UiEvent::Input(Event::Mouse(MouseEvent {
                kind,
                column: 40,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }));
            assert!(event_requires_redraw(Some(&event), false));
        }
    }

    /// 用户级 Skill 目录必须在装配层幂等创建，且空目录不会注册读取工具。
    #[test]
    fn initializes_user_skill_root_without_registering_tool() {
        let home = std::env::temp_dir().join(format!(
            "lucia-user-skill-root-{}",
            agent_session::SessionId::generate()
        ));
        std::fs::create_dir_all(&home).expect("应创建测试应用目录");

        assert!(
            initialize_user_skill_root(&home).expect("应创建用户级 Skill 目录"),
            "首次调用应报告新建"
        );
        assert!(user_skill_root(&home).is_dir());
        assert!(
            !initialize_user_skill_root(&home).expect("重复调用应保持幂等"),
            "已存在的目录不应报告新建"
        );

        let catalog = SkillCatalog::load_local([user_skill_root(&home)]).expect("空目录应加载成功");
        assert!(catalog.prompt().is_none(), "空目录不应生成索引提示");
        let mut registry = ToolRegistry::new();
        catalog
            .register_tool(&mut registry)
            .expect("空目录应跳过注册");
        assert!(
            registry.get(agent_skill::SKILL_READ_TOOL_NAME).is_none(),
            "空目录不应注册 skill_read"
        );

        std::fs::remove_dir_all(home).expect("应清理测试目录");
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

    /// 构造一个无修饰的普通字符按键事件。
    fn plain_key(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    /// 连续写入的字符达到阈值后合并成一次粘贴，附件识别才能拿到完整路径。
    #[test]
    fn injected_character_burst_becomes_single_paste() {
        let events = [
            plain_key('/'),
            plain_key('t'),
            plain_key('m'),
            plain_key('p'),
        ];
        let coalesced = coalesce_event_batch(Vec::from(events));
        assert_eq!(coalesced.len(), 1);
        assert!(matches!(&coalesced[0], Event::Paste(text) if text.as_str() == "/tmp"));

        // 终端把大写字母上报为 `Char + SHIFT`，它仍是路径正文。
        let shifted = [
            Event::Key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT)),
            plain_key('s'),
            plain_key('e'),
            plain_key('r'),
        ];
        let coalesced = coalesce_event_batch(Vec::from(shifted));
        assert_eq!(coalesced.len(), 1);
        assert!(matches!(&coalesced[0], Event::Paste(text) if text.as_str() == "User"));
    }

    /// 不足阈值或带修饰键的输入保持逐键语义，不能把正常打字改写成粘贴。
    #[test]
    fn short_or_modified_keys_keep_individual_events() {
        let short = [plain_key('a'), plain_key('b'), plain_key('c')];
        assert_eq!(coalesce_event_batch(Vec::from(short)).len(), 3);

        let modified = [
            plain_key('a'),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        ];
        assert_eq!(coalesce_event_batch(Vec::from(modified)).len(), 2);
    }

    /// 粘贴之后的回车等非字符事件必须保持独立，并排在粘贴之后。
    #[test]
    fn trailing_non_character_event_follows_paste() {
        let events = [
            plain_key('/'),
            plain_key('t'),
            plain_key('m'),
            plain_key('p'),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ];
        let coalesced = coalesce_event_batch(Vec::from(events));
        assert_eq!(coalesced.len(), 2);
        assert!(matches!(&coalesced[0], Event::Paste(text) if text.as_str() == "/tmp"));
        assert!(matches!(
            &coalesced[1],
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            })
        ));
    }
}
