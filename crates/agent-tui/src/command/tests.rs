//! 原生命令、会话与插件页的回归测试。

use super::*;
use crate::demo::build_demo_gateway;
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementOutcome;
use ratatui::{backend::TestBackend, Terminal};
use tokio::sync::mpsc;

/// 默认注册表必须包含用户依赖的全部内置命令和退出别名。
#[test]
fn native_registry_contains_default_commands() {
    let names = NATIVE_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    #[cfg(feature = "plugins")]
    assert_eq!(
        names,
        vec![
            "new",
            "resume",
            "sessions",
            "compact",
            "clear",
            "reasoning",
            "plugins",
            "help",
            "exit"
        ]
    );
    #[cfg(not(feature = "plugins"))]
    assert_eq!(
        names,
        vec![
            "new",
            "resume",
            "sessions",
            "compact",
            "clear",
            "reasoning",
            "help",
            "exit"
        ]
    );
    assert_eq!(
        NATIVE_COMMANDS.last().expect("应有退出命令").aliases,
        &["quit"]
    );
}

/// 命令与说明使用固定紧凑间距，不因最长命令扩大整列留白。
#[test]
fn slash_panel_uses_compact_command_spacing() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/".into();
    let mut terminal = Terminal::new(TestBackend::new(80, 14)).expect("应创建命令面板终端");

    terminal
        .draw(|frame| render_native_command_panel(frame, &app, frame.area()))
        .expect("应渲染命令面板");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    // TestBackend 为全角字符保留空白单元格，因此只比较命令与首个说明字符的间距。
    assert!(text.contains("/new  新"), "{text}");
    assert!(!text.contains("/new      新"), "{text}");
}

/// 空斜杠菜单必须展示完整原生命令，不能因固定高度截掉思考入口。
#[test]
fn slash_panel_includes_reasoning_command() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/".into();

    let matches = app.slash_command_matches();
    assert!(matches.iter().any(|command| command.name == "reasoning"));
    assert!(app.native_command_panel_height() >= 9);
}

/// 插件构建只暴露人工管理入口，不得出现自动变异或生成命令。
#[cfg(feature = "plugins")]
#[test]
fn plugin_page_only_exposes_manual_actions() {
    let names = NATIVE_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"plugins"));
    assert!(!names.iter().any(|name| {
        name.contains("evolve") || name.contains("mutation") || name.contains("generate")
    }));
    let plugin_help = NATIVE_COMMANDS
        .iter()
        .find(|command| command.name == "plugins")
        .expect("应保留人工插件管理入口");
    assert!(plugin_help.description.contains("人工插件管理"));
    assert!(plugin_help.description.contains("不热重载"));
}

/// 插件页必须展示已安装插件与版本历史，且不能出现自动变异按钮文案。
#[cfg(feature = "plugins")]
#[test]
fn plugin_page_shows_installed_plugins_and_manual_actions() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.plugin_dialog = Some(NativePluginDialog {
        selected: 0,
        status: NativePluginDialogStatus::Ready(NativePluginDialogData {
            plugins: vec![agent_plugin_manager::InstalledPlugin {
                id: "echo".into(),
                name: "Echo".into(),
                version: "1.0.0".into(),
                api_version: "0.7.0".into(),
                enabled: true,
                manifest: "plugins/echo/1.0.0/plugin.toml".into(),
                sha256: "b".repeat(64),
                source: "fixture".into(),
            }],
            version_history: vec![agent_plugin_manager::InstalledPlugin {
                id: "echo".into(),
                name: "Echo".into(),
                version: "0.9.0".into(),
                api_version: "0.7.0".into(),
                enabled: true,
                manifest: "plugins/echo/0.9.0/plugin.toml".into(),
                sha256: "c".repeat(64),
                source: "fixture-history".into(),
            }],
        }),
        path_mode: None,
        path_input: String::new(),
        remove_confirmation: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(112, 32)).expect("应创建测试终端");

    terminal
        .draw(|frame| render_native_plugin_dialog(frame, &app, frame.area()))
        .expect("应渲染插件管理页");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let compact = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    assert!(text.contains("Installed Version History"), "{text}");
    assert!(text.contains("echo@0.9.0"), "{text}");
    assert!(compact.contains("人工管理"), "{text}");
    assert!(!text.contains("Evolve Plugin"), "{text}");
    assert!(!text.contains("Generate Plugin"), "{text}");
}

/// 本地插件路径必须接受由 Shift 产生的大写字符，同时拒绝控制类组合键。
#[cfg(feature = "plugins")]
#[test]
fn plugin_path_input_accepts_uppercase_characters() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.plugin_dialog = Some(NativePluginDialog {
        selected: 0,
        status: NativePluginDialogStatus::Loading,
        path_mode: Some(NativePluginPathMode::Install),
        path_input: String::new(),
        remove_confirmation: None,
    });

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('U'), KeyModifiers::SHIFT,));
    assert_eq!(
        app.native_command
            .plugin_dialog
            .as_ref()
            .map(|dialog| dialog.path_input.as_str()),
        Some("U")
    );
    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('x'), KeyModifiers::ALT));
    assert_eq!(
        app.native_command
            .plugin_dialog
            .as_ref()
            .map(|dialog| dialog.path_input.as_str()),
        Some("U")
    );
}

/// 人工插件变更提示必须说明新 Session 生效且当前 Session 保持不变。
#[cfg(feature = "plugins")]
#[test]
fn plugin_change_warning_is_displayed() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.plugin_dialog = Some(NativePluginDialog {
        selected: 0,
        status: NativePluginDialogStatus::Applying,
        path_mode: None,
        path_input: String::new(),
        remove_confirmation: None,
    });

    app.handle_native_plugin_action_finished(Ok(PluginManagementOutcome {
        message: "已启用 echo 1.0.0".into(),
        changed: true,
    }));

    let text = &app.messages.last().expect("应显示人工变更提示").text;
    assert!(text.contains("新 Session"), "{text}");
    assert!(text.contains("当前 Session 继续使用启动时快照"), "{text}");
}

/// Tab 必须只补全规范命令名，不能提前执行命令。
#[test]
fn native_completion_replaces_input_without_plugin_host() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/res".into();
    app.cursor = app.input.len();

    assert!(app.native_command_panel_height() > 0);
    assert!(app.handle_native_command_key(KeyCode::Tab, KeyModifiers::NONE, None));
    assert_eq!(app.input, "/resume ");
    assert_eq!(app.cursor, app.input.len());
    assert!(app.native_command.dialog.is_none());
}

/// Enter 必须立即执行当前高亮候选，而不是把命令缩写当作未知命令。
#[test]
fn native_completion_executes_selected_command_on_enter() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/h".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.input.is_empty());
    assert_eq!(app.messages.len(), 1);
    assert!(app.messages[0].text.contains("/help"));
}

/// `/sessions` 的浏览入口必须允许切换至当前高亮的精确会话修订。
#[test]
fn native_sessions_browse_switches_selected_session_on_enter() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let session_id = SessionId::new("selected-session").expect("创建测试会话标识");
    app.native_command.dialog = Some(NativeSessionDialog {
        mode: NativeSessionDialogMode::Browse,
        selected: 0,
        status: NativeSessionDialogStatus::Ready(vec![SessionSummary {
            id: session_id.clone(),
            revision: 7,
            created_at_ms: 1,
            updated_at_ms: 2,
            title: Some("待切换会话".into()),
            message_count: 3,
        }]),
    });

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.native_command.dialog.is_none());
    assert!(matches!(
        rx.try_recv().expect("应发送原生会话切换请求"),
        UiEvent::NativeResumeRequested {
            session_id: requested_id,
            revision: 7,
        } if requested_id == session_id
    ));
}

/// `/compact` 在任何 feature 组合下都必须进入原生重载路径。
#[test]
fn native_compact_requests_context_reload_without_plugin_host() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.execute_native_compact();

    assert!(matches!(
        rx.try_recv().expect("原生命令应发布重载请求"),
        UiEvent::NativeCompactRequested
    ));
}

/// 原生命令必须消费输入并直接产生界面结果，不能作为消息发送给模型。
#[test]
fn native_help_executes_inside_tui() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/help resume".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.input.is_empty());
    assert_eq!(app.messages.len(), 1);
    assert!(app.messages[0].text.contains("恢复历史会话"));
}

/// `/reasoning` 必须按 models.dev 可用等级更新 TUI 与共享 Agent。
#[test]
fn native_reasoning_command_sets_supported_level() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
        vec![ReasoningLevel::Off, ReasoningLevel::High],
        ReasoningLevel::Off,
    );
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.input = "/reasoning high".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));

    assert_eq!(app.reasoning_level, ReasoningLevel::High);
    assert_eq!(agent.reasoning_level(), ReasoningLevel::High);
    assert!(app
        .messages
        .last()
        .is_some_and(|message| message.text.contains("思考等级已切换为 高")));
}

/// `/reasoning` 不带参数时必须打开二级界面，并默认高亮当前等级。
#[test]
fn native_reasoning_command_opens_inline_panel() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
        vec![ReasoningLevel::Off, ReasoningLevel::Medium],
        ReasoningLevel::Medium,
    );
    app.input = "/reasoning".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    let panel = app
        .native_command
        .reasoning_panel
        .as_ref()
        .expect("应打开思考等级二级界面");
    assert_eq!(panel.selected, 1);
    assert_eq!(app.native_command_panel_height(), 3);
    assert!(!app.native_dialog_active());
    assert!(app.messages.is_empty());
}

/// 思考等级二级界面必须支持方向键选择，并在 Enter 后同步共享 Agent。
#[test]
fn native_reasoning_panel_applies_selected_level() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
        vec![ReasoningLevel::Off, ReasoningLevel::Medium],
        ReasoningLevel::Off,
    );
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.input = "/reasoning".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));
    assert!(app.handle_native_command_key(KeyCode::Down, KeyModifiers::NONE, Some(&agent)));
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));

    assert!(app.native_command.reasoning_panel.is_none());
    assert_eq!(app.reasoning_level, ReasoningLevel::Medium);
    assert_eq!(agent.reasoning_level(), ReasoningLevel::Medium);
}
