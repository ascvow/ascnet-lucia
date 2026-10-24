//! 原生命令、会话与插件页的回归测试。

use super::*;
use crate::demo::build_demo_gateway;
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementOutcome;
use ratatui::{backend::TestBackend, layout::Rect, style::Color, Frame, Terminal};
use tokio::sync::mpsc;

/// 构造贴住画布底边的输入盒锚点；浮层管理页以它为底边向上生长。
#[cfg(feature = "plugins")]
fn composer_anchor(frame: &Frame) -> Rect {
    Rect::new(0, frame.area().height, frame.area().width, 1)
}

/// 为插件页测试补充 Host 当前 ABI 固有授权，避免测试复制另一份权限基线。
#[cfg(feature = "plugins")]
fn test_plugin_authorization(
    manifest_snapshot: agent_plugin_host::audit::ManifestCapabilitySnapshot,
) -> agent_plugin_host::audit::PluginAuthorizationProfile {
    let manifest: agent_plugin_host::manifest::PluginManifest = toml::from_str(
        r#"
            [plugin]
            id = "command-test"
            name = "Command Test"
            version = "1.0.0"
            api_version = "0.10.0"
            wasm = "command-test.wasm"
        "#,
    )
    .expect("命令测试 manifest 应可解析");
    let mut profile = agent_plugin_host::audit::snapshot_plugin_authorization_profile(&manifest)
        .expect("生成命令测试授权 profile");
    profile.manifest = manifest_snapshot;
    profile
}

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
            "context",
            "new",
            "resume",
            "sessions",
            "compact",
            "clear",
            "reasoning",
            "model",
            "attach",
            "plugins",
            "tui",
            "keys",
            "help",
            "exit"
        ]
    );
    #[cfg(not(feature = "plugins"))]
    assert_eq!(
        names,
        vec![
            "context",
            "new",
            "resume",
            "sessions",
            "compact",
            "clear",
            "reasoning",
            "model",
            "attach",
            "tui",
            "keys",
            "help",
            "exit"
        ]
    );
    assert_eq!(
        NATIVE_COMMANDS.last().expect("应有退出命令").aliases,
        &["quit"]
    );
}

/// 取出终端缓冲区的每一行，便于按列断言行距与对齐。
fn palette_rows(terminal: &Terminal<TestBackend>, width: u16) -> Vec<String> {
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

/// 返回某一行中首个目标字符所在的显示列。
///
/// `str::find` 返回字节偏移，行内的 `▌` 是多字节字符，用它比较列号会偏；逐字符定位
/// 才能和终端列号一一对应。
fn palette_column(row: &str, needle: char) -> usize {
    row.chars()
        .position(|character| character == needle)
        .unwrap_or_else(|| panic!("行内应出现 {needle:?}：{row:?}"))
}

/// 命令补全菜单按两列对齐，条目逐行紧邻排布，横向铺满整个终端宽度。
#[test]
fn slash_panel_aligns_columns_and_stacks_rows() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/".into();
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).expect("应创建命令面板终端");

    terminal
        .draw(|frame| render_native_command_panel(frame, &app, frame.area()))
        .expect("应渲染命令面板");
    let rows = palette_rows(&terminal, 80);

    // 首行是顶部分隔边框，随后每条命令各占一行，中间不再插空行。
    assert!(rows[0].starts_with('─'), "{:?}", rows[0]);
    assert!(rows[1].contains("/context"), "{:?}", rows[1]);
    assert!(rows[2].contains("/new"), "{:?}", rows[2]);

    // 指示条直接落在第 1 列，命令名与面板左边界之间只隔一层行内边距。
    assert!(rows[1].starts_with("▌ /context"), "{:?}", rows[1]);
    assert!(rows[2].starts_with("  /new"), "{:?}", rows[2]);

    // 描述列起点在所有条目上一致，形成表格式对齐。
    // TestBackend 会把全角字符的占位列重置为空格，因此用首个说明字符定位列号。
    let new_column = palette_column(&rows[2], '新');
    let resume_column = palette_column(&rows[3], '恢');
    assert_eq!(new_column, resume_column, "{rows:?}");
}

/// 过滤到单条命令时命令列宽保持不变，描述不会因为筛选而左右跳动。
#[test]
fn slash_panel_keeps_column_width_under_filtering() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let full_app = {
        let mut app = App::new(tx.clone(), "测试模型".into());
        app.input = "/".into();
        app
    };
    let filtered_app = {
        let mut app = App::new(tx, "测试模型".into());
        app.input = "/n".into();
        app
    };

    let mut full_terminal = Terminal::new(TestBackend::new(80, 30)).expect("应创建完整面板终端");
    full_terminal
        .draw(|frame| render_native_command_panel(frame, &full_app, frame.area()))
        .expect("应渲染完整命令面板");
    let full_rows = palette_rows(&full_terminal, 80);

    let mut filtered_terminal =
        Terminal::new(TestBackend::new(80, 10)).expect("应创建过滤面板终端");
    filtered_terminal
        .draw(|frame| render_native_command_panel(frame, &filtered_app, frame.area()))
        .expect("应渲染过滤后的命令面板");
    let filtered_rows = palette_rows(&filtered_terminal, 80);

    // `/n` 只应留下 `/new`，其余命令不再出现。
    assert!(filtered_rows[1].contains("/new"), "{:?}", filtered_rows[1]);
    let command_rows = filtered_rows.iter().filter(|row| row.contains('/')).count();
    assert_eq!(command_rows, 1, "{filtered_rows:?}");
    // 列宽取自全部原生命令，因此说明文字仍落在完整列表的同一列。
    let full_column = palette_column(&full_rows[2], '新');
    let filtered_column = palette_column(&filtered_rows[1], '新');
    assert_eq!(full_column, filtered_column, "过滤不应改变命令列宽");
}

/// 选中项用柔和深灰底色铺满整行，命令名统一用青蓝色，描述保持中性灰。
#[test]
fn slash_panel_styles_selection_and_command_names() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/".into();
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).expect("应创建命令面板终端");

    terminal
        .draw(|frame| render_native_command_panel(frame, &app, frame.area()))
        .expect("应渲染命令面板");

    let buffer = terminal.backend().buffer();
    // 第 1 行是选中项：底色从中第 1 列一路铺到行尾，命令名保持青蓝。
    assert_eq!(
        buffer[(0, 1)].bg,
        crate::theme::COLOR_PALETTE_SELECTION_BG,
        "选中底色应从中第 1 列铺起"
    );
    assert_eq!(buffer[(2, 1)].fg, crate::theme::COLOR_PALETTE_COMMAND);
    assert_eq!(buffer[(79, 1)].bg, crate::theme::COLOR_PALETTE_SELECTION_BG);
    // 未选中行不铺底色，但命令名同样可辨识；紧邻选中行，中间没有空行。
    assert_eq!(buffer[(2, 2)].bg, Color::Reset);
    assert_eq!(buffer[(2, 2)].fg, crate::theme::COLOR_PALETTE_COMMAND);
    // 末行空行是列表与输入框之间的唯一留白，不留残余底色。
    assert_eq!(buffer[(3, 13)].bg, Color::Reset);
}

/// 面板高度不足时（终端过矮）列表跟随选中项滚动，高亮不会滚出可视区。
#[test]
fn slash_panel_scrolls_to_keep_selection_visible() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/".into();
    app.native_command.selected = 5;
    let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("应创建矮面板终端");

    terminal
        .draw(|frame| render_native_command_panel(frame, &app, frame.area()))
        .expect("应渲染矮面板");
    let rows = palette_rows(&terminal, 80);
    let marker_row = rows
        .iter()
        .position(|row| row.contains('▌'))
        .expect("选中项必须留在可视区内");

    assert!(rows[marker_row].contains("/clear"), "{rows:?}");
    // 滚动后首条命令让出位置，顶部分隔边框仍固定在首行。
    assert!(rows[0].starts_with('─'), "{rows:?}");
    assert!(!rows.iter().any(|row| row.contains("/context")), "{rows:?}");
}

/// 面板高度必须与行距一致，否则列表底部会被裁掉。
#[test]
fn slash_panel_height_matches_stacked_rows() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/".into();

    let count = app.slash_command_matches().len();
    assert_eq!(
        app.native_command_panel_height(),
        u16::try_from(count).unwrap() + 2,
        "每条命令占一行，加顶部分隔边框与底部空行"
    );
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
                api_version: "0.10.0".into(),
                enabled: false,
                manifest: "plugins/echo/1.0.0/plugin.toml".into(),
                sha256: "b".repeat(64),
                source: "fixture".into(),
            }],
            version_history: vec![agent_plugin_manager::InstalledPlugin {
                id: "echo".into(),
                name: "Echo".into(),
                version: "0.10.0".into(),
                api_version: "0.10.0".into(),
                enabled: true,
                manifest: "plugins/echo/0.10.0/plugin.toml".into(),
                sha256: "c".repeat(64),
                source: "fixture-history".into(),
            }],
            authorizations: std::collections::HashMap::from([(
                "echo".into(),
                test_plugin_authorization(agent_plugin_host::audit::ManifestCapabilitySnapshot {
                    plugin_id: "echo".into(),
                    requested: vec![agent_plugin_host::audit::ManifestCapabilityRequest {
                        capability_id: "process_exec".into(),
                        scopes: Vec::new(),
                    }],
                    provided: Vec::new(),
                }),
            )]),
        }),
        path_mode: None,
        path_input: String::new(),
        remove_confirmation: None,
        enable_confirmation: None,
        pending_owner_selection: None,
        review_scroll: 0,
    });
    let mut terminal = Terminal::new(TestBackend::new(112, 32)).expect("应创建测试终端");

    terminal
        .draw(|frame| render_native_plugin_dialog(frame, &mut app, composer_anchor(frame)))
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
    assert!(text.contains("echo@0.10.0"), "{text}");
    assert!(text.contains("process_exec"), "{text}");
    assert!(compact.contains("人工管理"), "{text}");
    assert!(!text.contains("Evolve Plugin"), "{text}");
    assert!(!text.contains("Generate Plugin"), "{text}");

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert_eq!(
        app.native_command
            .plugin_dialog
            .as_ref()
            .and_then(|dialog| {
                dialog
                    .enable_confirmation
                    .as_ref()
                    .map(|confirmation| confirmation.plugin_id.as_str())
            }),
        Some("echo")
    );
    assert_eq!(
        app.native_command
            .plugin_dialog
            .as_ref()
            .and_then(|dialog| dialog.enable_confirmation.as_ref())
            .map(|confirmation| confirmation.bundle_identity.clone()),
        Some(agent_plugin_manager::PluginBundleIdentity {
            source: "fixture".into(),
            version: "1.0.0".into(),
            sha256: "b".repeat(64),
        })
    );

    let mut review_terminal = Terminal::new(TestBackend::new(112, 32)).expect("应创建启用审查终端");
    review_terminal
        .draw(|frame| render_native_plugin_dialog(frame, &mut app, composer_anchor(frame)))
        .expect("应渲染完整启用审查页");
    let review_text = review_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let compact_review = review_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(compact_review.contains("启用授权确认"), "{review_text}");
    assert!(review_text.contains(&"b".repeat(64)), "{review_text}");
    assert!(review_text.contains("process_exec"), "{review_text}");

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Down, KeyModifiers::NONE));
    let dialog = app
        .native_command
        .plugin_dialog
        .as_ref()
        .expect("滚动后应保留审查页");
    assert_eq!(dialog.review_scroll, 1);
    assert!(dialog.enable_confirmation.is_some());

    app.native_command
        .plugin_dialog
        .as_mut()
        .expect("审查页应保持打开")
        .review_scroll = u16::MAX;
    let mut narrow_terminal =
        Terminal::new(TestBackend::new(36, 10)).expect("应创建窄终端审查视口");
    narrow_terminal
        .draw(|frame| render_native_plugin_dialog(frame, &mut app, composer_anchor(frame)))
        .expect("窄终端仍应安全渲染完整审查页");
    let clamped_scroll = app
        .native_command
        .plugin_dialog
        .as_ref()
        .expect("审查页应保持打开")
        .review_scroll;
    assert!(clamped_scroll < u16::MAX);
    let narrow_text = narrow_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let compact_narrow = narrow_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(compact_narrow.contains("再次按e"), "{narrow_text}");
    assert!(app.handle_native_plugin_dialog_key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        app.native_command
            .plugin_dialog
            .as_ref()
            .expect("向上滚动后审查页应保持打开")
            .review_scroll,
        clamped_scroll.saturating_sub(1)
    );
}

/// 缺少 Host 校验授权 profile 时必须阻止启用，不能让“未知授权”进入第二次确认。
#[cfg(feature = "plugins")]
#[test]
fn plugin_enable_fails_closed_without_authorization_profile() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.plugin_dialog = Some(NativePluginDialog {
        selected: 0,
        status: NativePluginDialogStatus::Ready(NativePluginDialogData {
            plugins: vec![agent_plugin_manager::InstalledPlugin {
                id: "unknown-capabilities".into(),
                name: "Unknown Capabilities".into(),
                version: "1.0.0".into(),
                api_version: "0.10.0".into(),
                enabled: false,
                manifest: "plugins/unknown-capabilities/1.0.0/plugin.toml".into(),
                sha256: "e".repeat(64),
                source: "fixture".into(),
            }],
            version_history: Vec::new(),
            authorizations: std::collections::HashMap::new(),
        }),
        path_mode: None,
        path_input: String::new(),
        remove_confirmation: None,
        enable_confirmation: None,
        pending_owner_selection: None,
        review_scroll: 0,
    });

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('e'), KeyModifiers::NONE));

    let dialog = app
        .native_command
        .plugin_dialog
        .as_ref()
        .expect("失败后应保留插件页");
    assert!(dialog.enable_confirmation.is_none());
    assert!(matches!(
        &dialog.status,
        NativePluginDialogStatus::Error(error) if error.contains("授权 profile 不可用")
    ));
}

/// 禁用插件被指定为 Capability Owner 时，必须先记录意图，再经过两次启用确认。
#[cfg(feature = "plugins")]
#[test]
fn disabled_capability_owner_requires_two_enable_confirmations() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.plugin_dialog = Some(NativePluginDialog {
        selected: 0,
        status: NativePluginDialogStatus::Ready(NativePluginDialogData {
            plugins: vec![agent_plugin_manager::InstalledPlugin {
                id: "provider".into(),
                name: "Provider".into(),
                version: "1.0.0".into(),
                api_version: "0.10.0".into(),
                enabled: false,
                manifest: "plugins/provider/1.0.0/plugin.toml".into(),
                sha256: "d".repeat(64),
                source: "github:https://github.com/example/provider@v1#provider.zip".into(),
            }],
            version_history: Vec::new(),
            authorizations: std::collections::HashMap::from([(
                "provider".into(),
                test_plugin_authorization(agent_plugin_host::audit::ManifestCapabilitySnapshot {
                    plugin_id: "provider".into(),
                    requested: vec![agent_plugin_host::audit::ManifestCapabilityRequest {
                        capability_id: "process_exec".into(),
                        scopes: Vec::new(),
                    }],
                    provided: Vec::new(),
                }),
            )]),
        }),
        path_mode: None,
        path_input: String::new(),
        remove_confirmation: None,
        enable_confirmation: None,
        pending_owner_selection: None,
        review_scroll: 0,
    });

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('o'), KeyModifiers::NONE));
    for character in "agent.context-loader provider".chars() {
        assert!(app.handle_native_plugin_dialog_key(KeyCode::Char(character), KeyModifiers::NONE,));
    }
    assert!(app.handle_native_plugin_dialog_key(KeyCode::Enter, KeyModifiers::NONE));
    let dialog = app
        .native_command
        .plugin_dialog
        .as_ref()
        .expect("插件页应保持打开");
    assert_eq!(
        dialog.pending_owner_selection.clone(),
        Some(PendingCapabilityOwnerSelection {
            capability_id: "agent.context-loader".into(),
            plugin_id: "provider".into(),
            bundle_identity: agent_plugin_manager::PluginBundleIdentity {
                source: "github:https://github.com/example/provider@v1#provider.zip".into(),
                version: "1.0.0".into(),
                sha256: "d".repeat(64),
            },
        })
    );
    assert!(dialog.enable_confirmation.is_none());
    assert!(matches!(dialog.status, NativePluginDialogStatus::Ready(_)));

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Down, KeyModifiers::NONE));
    assert!(app
        .native_command
        .plugin_dialog
        .as_ref()
        .is_some_and(|dialog| dialog.pending_owner_selection.is_none()));
    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('o'), KeyModifiers::NONE));
    for character in "agent.context-loader provider".chars() {
        assert!(app.handle_native_plugin_dialog_key(KeyCode::Char(character), KeyModifiers::NONE,));
    }
    assert!(app.handle_native_plugin_dialog_key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('e'), KeyModifiers::NONE));
    let dialog = app
        .native_command
        .plugin_dialog
        .as_ref()
        .expect("第一次确认后插件页应保持打开");
    assert_eq!(
        dialog
            .enable_confirmation
            .as_ref()
            .map(|confirmation| confirmation.plugin_id.as_str()),
        Some("provider")
    );
    assert_eq!(
        dialog
            .enable_confirmation
            .as_ref()
            .map(|confirmation| &confirmation.bundle_identity),
        dialog
            .pending_owner_selection
            .as_ref()
            .map(|pending| &pending.bundle_identity)
    );
    assert!(dialog.pending_owner_selection.is_some());
    assert!(matches!(dialog.status, NativePluginDialogStatus::Ready(_)));

    let mut terminal = Terminal::new(TestBackend::new(140, 32)).expect("应创建测试终端");
    terminal
        .draw(|frame| render_native_plugin_dialog(frame, &mut app, composer_anchor(frame)))
        .expect("应渲染 Owner 启用确认");
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
    assert!(compact.contains("再次按e"), "{text}");
    assert!(text.contains("agent.context-loader"), "{text}");
    assert!(text.contains("github.com/example/provider"), "{text}");
    assert!(text.contains("process_exec"), "{text}");

    assert!(app.handle_native_plugin_dialog_key(KeyCode::Char('e'), KeyModifiers::NONE));
    let dialog = app
        .native_command
        .plugin_dialog
        .as_ref()
        .expect("操作失败说明应保留插件页");
    assert!(dialog.pending_owner_selection.is_none());
    assert!(dialog.enable_confirmation.is_none());
    assert!(matches!(dialog.status, NativePluginDialogStatus::Error(_)));
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
        enable_confirmation: None,
        pending_owner_selection: None,
        review_scroll: 0,
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
        enable_confirmation: None,
        pending_owner_selection: None,
        review_scroll: 0,
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
    assert!(app.native_command.session_palette.is_none());
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
    app.native_command.session_palette = Some(NativeSessionPalette {
        mode: NativeSessionPaletteMode::Browse,
        selected: 0,
        status: NativeSessionPaletteStatus::Ready(vec![NativeSessionEntry::new(SessionSummary {
            id: session_id.clone(),
            revision: 7,
            created_at_ms: 1,
            updated_at_ms: 2,
            title: Some("待切换会话".into()),
            message_count: 3,
        })]),
    });

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.native_command.session_palette.is_none());
    assert!(matches!(
        rx.try_recv().expect("应发送原生会话切换请求"),
        UiEvent::NativeResumeRequested {
            session_id: requested_id,
            revision: 7,
        } if requested_id == session_id
    ));
}

/// 会话面板的输入框就是搜索框：按词过滤后回车恢复的是命中项，而不是列表首项。
#[test]
fn native_session_palette_filters_by_input_words() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut entries = Vec::new();
    for (id, title, revision) in [
        ("design-review", "设计评审", 3),
        ("design-draft", "设计草稿", 4),
        ("budget-plan", "预算规划", 5),
    ] {
        entries.push(NativeSessionEntry::new(SessionSummary {
            id: SessionId::new(id).expect("创建测试会话标识"),
            revision,
            created_at_ms: 1,
            updated_at_ms: 2,
            title: Some(title.into()),
            message_count: 1,
        }));
    }
    app.native_command.session_palette = Some(NativeSessionPalette {
        mode: NativeSessionPaletteMode::Resume,
        selected: 0,
        status: NativeSessionPaletteStatus::Ready(entries),
    });

    // 搜索词直接写进主输入；多个词必须同时命中。
    app.input = "设计 评审".into();
    assert_eq!(app.session_palette_rows(), 1);
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(matches!(
        rx.try_recv().expect("应发送恢复请求"),
        UiEvent::NativeResumeRequested { revision: 3, .. }
    ));
}

/// 没有命中时回车必须被吞掉：否则搜索词会作为一条普通消息发给模型。
#[test]
fn native_session_palette_swallows_enter_without_match() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.session_palette = Some(NativeSessionPalette {
        mode: NativeSessionPaletteMode::Resume,
        selected: 0,
        status: NativeSessionPaletteStatus::Ready(vec![NativeSessionEntry::new(SessionSummary {
            id: SessionId::new("only-session").expect("创建测试会话标识"),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 2,
            title: Some("唯一会话".into()),
            message_count: 2,
        })]),
    });
    app.input = "没有这个会话".into();

    assert_eq!(app.session_palette_rows(), 0);
    // 回车被面板消费，但因为没有命中，不会发出恢复请求。
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(rx.try_recv().is_err());
    assert!(app.native_command.session_palette.is_some());
}

/// Esc 关闭面板时必须丢弃搜索词，避免残留文本在下一次回车时被发给模型。
#[test]
fn native_session_palette_escape_clears_search_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.session_palette = Some(NativeSessionPalette {
        mode: NativeSessionPaletteMode::Browse,
        selected: 0,
        status: NativeSessionPaletteStatus::Loading,
    });
    app.input = "设计".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Esc, KeyModifiers::NONE, None));
    assert!(app.native_command.session_palette.is_none());
    assert!(app.input.is_empty());
    assert_eq!(app.cursor, 0);
}

/// `/resume` 必须打开会话搜索面板，并把输入框交还给搜索框。
#[tokio::test]
async fn resume_command_opens_session_palette_with_empty_query() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/resume".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    let palette = app
        .native_command
        .session_palette
        .as_ref()
        .expect("应打开会话面板");
    assert!(matches!(palette.mode, NativeSessionPaletteMode::Resume));
    assert!(matches!(
        palette.status,
        NativeSessionPaletteStatus::Loading
    ));
    // 命令文本必须清空：残留的 `/resume` 会变成面板的搜索词。
    assert!(app.input.is_empty());
    // 行几何固定：顶部分隔边框 + 一行状态文本 + 一行按键提示。
    assert_eq!(app.native_command_panel_height(), 3);
    // 内联面板不占用全屏 Dialog 焦点，主编辑器仍接收搜索词。
    assert!(!app.native_dialog_active());
}

/// `/sessions` 与 `/resume` 共用面板，只是标题与确认文案不同。
#[tokio::test]
async fn sessions_command_opens_palette_in_browse_mode() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/sessions".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    let palette = app
        .native_command
        .session_palette
        .as_ref()
        .expect("应打开会话面板");
    assert!(matches!(palette.mode, NativeSessionPaletteMode::Browse));
}

/// `/context` 在本地打开且可关闭，不向 Agent 发送消息。
#[test]
fn context_command_opens_and_closes_without_model_request() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/context".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.native_command.context_open);
    assert!(app.native_dialog_active());
    assert!(app.input.is_empty());
    assert!(rx.try_recv().is_err());
    assert!(app.handle_native_command_key(KeyCode::Esc, KeyModifiers::NONE, None));
    assert!(!app.native_dialog_active());
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

/// `/tui` 不带参数只报告当前渲染模式，不登记任何切换。
#[test]
fn native_tui_command_reports_current_mode() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/tui".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(app.fullscreen);
    assert!(app.pending_fullscreen.is_none());
    assert_eq!(app.messages.len(), 1);
    assert!(app.messages[0].text.contains("fullscreen"));
}

/// `/tui default` 必须登记切换意图，并把选择写回配置文件其余字段保持不变。
#[test]
fn native_tui_command_switches_and_persists_mode() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let root = std::env::temp_dir().join(format!(
        "lucia-tui-mode-{}",
        agent_session::SessionId::generate()
    ));
    std::fs::create_dir_all(&root).expect("创建配置目录");
    let path = root.join("config.json");
    std::fs::write(&path, r#"{"tui":{"sessions_dir":"projects"}}"#).expect("写入配置");
    let mut app = App::new(tx, "测试模型".into()).with_config_path(path.clone());
    app.input = "/tui default".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(!app.fullscreen);
    assert_eq!(app.pending_fullscreen, Some(false));
    let text = std::fs::read_to_string(&path).expect("读取写回后的配置");
    assert!(text.contains("\"fullscreen\": false"));
    assert!(text.contains("\"sessions_dir\": \"projects\""));
    std::fs::remove_dir_all(root).expect("清理配置目录");
}

/// 未知渲染模式必须报错，且不改变当前设置也不登记切换。
#[test]
fn native_tui_command_rejects_unknown_mode() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/tui htop".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(app.fullscreen);
    assert!(app.pending_fullscreen.is_none());
    assert_eq!(app.messages.len(), 1);
    assert!(app.messages[0].text.contains("未知渲染模式"));
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
    assert_eq!(app.native_command_panel_height(), 4);
    assert!(!app.native_dialog_active());
    assert!(app.messages.is_empty());
}

/// 只有一档时滑块没有任何可选位置，`/reasoning` 必须说明原因而不是画出残桩。
#[test]
fn native_reasoning_command_reports_missing_levels() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into())
        .with_reasoning_levels(vec![ReasoningLevel::Off], ReasoningLevel::Off);
    app.input = "/reasoning".into();
    app.cursor = app.input.len();

    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(app.native_command.reasoning_panel.is_none());
    assert_eq!(app.native_command_panel_height(), 0);
    assert!(
        app.messages
            .last()
            .is_some_and(|message| message.text.contains("没有可切换的思考等级")),
        "应说明等级缺失而不是静默无反应：{:?}",
        app.messages.last().map(|message| &message.text)
    );
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
    assert!(app.handle_native_command_key(KeyCode::Right, KeyModifiers::NONE, Some(&agent)));
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));

    assert!(app.native_command.reasoning_panel.is_none());
    assert_eq!(app.reasoning_level, ReasoningLevel::Medium);
    assert_eq!(agent.reasoning_level(), ReasoningLevel::Medium);
}

/// 滑块两端夹住不回绕：走到头继续按方向键不会跳回另一端。
#[test]
fn reasoning_slider_clamps_at_both_ends() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
        vec![
            ReasoningLevel::Off,
            ReasoningLevel::Low,
            ReasoningLevel::High,
        ],
        ReasoningLevel::Off,
    );
    app.input = "/reasoning".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    let selected = |app: &App| {
        app.native_command
            .reasoning_panel
            .as_ref()
            .expect("滑块应保持打开")
            .selected
    };
    assert_eq!(selected(&app), 0);
    assert!(app.handle_native_command_key(KeyCode::Left, KeyModifiers::NONE, None));
    assert_eq!(selected(&app), 0, "已在最左端，不应回绕到最右");
    for _ in 0..5 {
        app.handle_native_command_key(KeyCode::Right, KeyModifiers::NONE, None);
    }
    assert_eq!(selected(&app), 2, "走到最右端后应停住");
}

/// 思考强度面板画成横向滑块：旋钮落在当前等级，刻度与标签逐列对齐。
#[test]
fn reasoning_panel_renders_horizontal_slider() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
        vec![
            ReasoningLevel::Off,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ],
        ReasoningLevel::Medium,
    );
    app.input = "/reasoning".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    let width = 60;
    let mut terminal = Terminal::new(TestBackend::new(width, 6)).expect("应创建滑块终端");
    terminal
        .draw(|frame| render_native_command_panel(frame, &app, frame.area()))
        .expect("应渲染思考强度滑块");
    let rows = palette_rows(&terminal, width);

    // 首行是顶部分隔边框，随后是轨道行与刻度标签行。
    // TestBackend 会把全角字符的占位列重置为空格，因此逐字判断而不是匹配整个词。
    assert!(rows[0].starts_with('─'), "{rows:?}");
    assert!(
        rows[1].contains('思') && rows[1].contains('强'),
        "轨道行应带标题：{rows:?}"
    );
    assert!(rows[1].contains('●'), "轨道上应有旋钮：{rows:?}");
    assert_eq!(
        rows[1].matches('○').count(),
        3,
        "未选中的三个等级各留一个刻度：{rows:?}"
    );
    for label in ['关', '低', '中', '高'] {
        assert!(rows[2].contains(label), "标签行缺少 {label}：{rows:?}");
    }

    // 旋钮必须正对「中」的标签中心，否则滑块与刻度会看起来错位。
    let knob = palette_column(&rows[1], '●');
    let label_start = palette_column(&rows[2], '中');
    assert_eq!(knob, label_start + 1, "旋钮应对准标签中心：{rows:?}");
    // 面板高度与布局算出的一致，底部留一行空行。
    assert_eq!(app.native_command_panel_height(), 4);
    assert!(rows[3].trim().is_empty(), "{rows:?}");

    // 窄终端放不下完整轨道时退回单行形式，但行数不变、当前等级仍然可读。
    let mut narrow = Terminal::new(TestBackend::new(20, 6)).expect("应创建窄滑块终端");
    narrow
        .draw(|frame| render_native_command_panel(frame, &app, frame.area()))
        .expect("窄终端也应渲染滑块");
    let narrow_rows = palette_rows(&narrow, 20);
    assert!(narrow_rows[1].contains('中'), "{narrow_rows:?}");
    assert!(!narrow_rows[1].contains('○'), "{narrow_rows:?}");
}

/// `/attach` 必须把可读取的本地文件加入附件，一行可添加多个路径。
#[test]
fn native_attach_command_adds_local_file() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let first = std::env::temp_dir().join(format!(
        "lucia-attach-command-{}-a-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    let second = std::env::temp_dir().join(format!(
        "lucia-attach-command-{}-b-{}.pdf",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::write(&first, b"\x89PNG\r\n\x1a\n").expect("写入附件命令测试文件");
    std::fs::write(&second, b"%PDF-1.7").expect("写入附件命令测试文件");

    app.input = format!("/attach {} {}", first.display(), second.display());
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    std::fs::remove_file(&first).ok();
    std::fs::remove_file(&second).ok();

    assert_eq!(app.attachments.len(), 2);
    assert_eq!(app.attachments[0].media_type, "image/png");
    assert_eq!(app.attachments[1].media_type, "application/pdf");
    assert_eq!(app.input, "[IMAGE#1] [PDF#1] ");
    assert!(app.messages.is_empty());
}

/// 已有待发送附件时，`/attach` 与"直接输入路径"一致地继续追加，而不是被
/// "斜杠命令不能携带附件"误拒；其他斜杠命令仍保持拒绝。
#[test]
fn native_attach_command_appends_when_attachment_pending() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let first = std::env::temp_dir().join(format!(
        "lucia-attach-pending-a-{}-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    let second = std::env::temp_dir().join(format!(
        "lucia-attach-pending-b-{}-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::write(&first, b"\x89PNG\r\n\x1a\n").expect("写入附件命令测试文件");
    std::fs::write(&second, b"\x89PNG\r\n\x1a\n").expect("写入附件命令测试文件");

    app.input = format!("/attach {}", first.display());
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert_eq!(app.attachments.len(), 1);

    app.input = format!("/attach {}", second.display());
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    std::fs::remove_file(&first).ok();
    std::fs::remove_file(&second).ok();

    assert_eq!(app.attachments.len(), 2, "`/attach` 必须追加而不是拒绝");
    assert_eq!(app.input, "[IMAGE#1] [IMAGE#2] ");
}

/// 除 `/attach` 外的斜杠命令仍然拒绝携带既有附件。
#[test]
fn native_non_attach_command_rejects_pending_attachment() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = std::env::temp_dir().join(format!(
        "lucia-attach-reject-{}-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("写入附件命令测试文件");

    app.input = format!("/attach {}", path.display());
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    std::fs::remove_file(&path).ok();
    assert_eq!(app.attachments.len(), 1);

    app.input = "/help".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(app
        .messages
        .last()
        .is_some_and(|message| message.text.contains("斜杠命令不能携带附件")));
}

/// 直接输入以 `/` 开头的绝对路径时，不能被当成斜杠命令：整条输入原样留给主输入区按文本发送。
#[test]
fn native_command_route_lets_absolute_path_through_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = std::env::temp_dir().join(format!(
        "lucia-path-input-{}-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("写入路径输入测试文件");

    app.input = path.display().to_string();
    app.cursor = app.input.len();
    let consumed = app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None);
    std::fs::remove_file(&path).ok();

    assert!(!consumed, "绝对路径不能被命令路由消费");
    assert!(app.attachments.is_empty(), "路径文本不得被转成附件");
    assert_eq!(app.input, path.display().to_string(), "输入必须原样保留");
    assert!(app.messages.is_empty(), "不得报未知命令");
}

/// 已有待发送附件时再输入路径，路径按正文保留而不是追加第二个附件。
///
/// `/attach` 仍是显式附件入口；直接敲入或粘贴路径不再隐式生成附件。
#[test]
fn native_command_route_keeps_path_input_after_pending_attachment_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let first = std::env::temp_dir().join(format!(
        "lucia-path-append-a-{}-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    let second = std::env::temp_dir().join(format!(
        "lucia-path-append-b-{}-{}.pdf",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::write(&first, b"\x89PNG\r\n\x1a\n").expect("写入路径输入测试文件");
    std::fs::write(&second, b"%PDF-1.7").expect("写入路径输入测试文件");

    app.input = format!("/attach {}", first.display());
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert_eq!(app.attachments.len(), 1);

    // 真实场景：引用标签还留在输入框里，用户在后面继续敲入一个路径。
    app.input.push_str(&second.display().to_string());
    app.cursor = app.input.len();
    assert!(
        !app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None),
        "路径输入不能被命令路由消费"
    );
    std::fs::remove_file(&first).ok();
    std::fs::remove_file(&second).ok();

    assert_eq!(app.attachments.len(), 1, "路径文本不得追加成第二个附件");
    assert!(
        app.input.ends_with(&second.display().to_string()),
        "路径应原样留在输入框"
    );
}

/// "/路径 + 提问"写法：命令路由必须放行，整句作为普通消息文本提交。
#[test]
fn native_command_route_lets_leading_path_through_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = std::env::temp_dir().join(format!(
        "lucia-path-with-text-{}-{}.png",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("写入路径输入测试文件");

    app.input = format!("{} 这张图有什么问题", path.display());
    app.cursor = app.input.len();
    let consumed = app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None);
    std::fs::remove_file(&path).ok();

    assert!(!consumed, "带路径的句子不能被命令路由消费");
    assert!(app.attachments.is_empty(), "路径文本不得被转成附件");
    assert_eq!(
        app.input,
        format!("{} 这张图有什么问题", path.display()),
        "输入必须原样留给主输入区"
    );
    assert!(app.messages.is_empty(), "不得报未知命令或文件错误");
}

/// 以 `/` 开头但其实是正文的句子必须作为普通消息提交，不能被命令路由吞掉。
#[test]
fn native_command_route_lets_path_like_sentence_through() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/usr/bin 应该加入 PATH".into();
    app.cursor = app.input.len();
    assert!(
        !app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None),
        "带正文的路径句子必须交回主输入区提交"
    );

    assert!(app.attachments.is_empty());
    assert!(app.messages.is_empty(), "不得报未知命令或文件错误");
    assert_eq!(app.input, "/usr/bin 应该加入 PATH", "交回前不得改动输入");
    assert!(!app.input.is_empty());
}

/// 相对路径 + 正文与绝对路径走同一判据：都作为普通消息文本提交，不再转附件。
#[test]
fn relative_path_with_trailing_text_is_sent_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let dir = std::env::temp_dir().join(format!(
        "lucia-relative-path-{}-{}",
        std::process::id(),
        agent_session::SessionId::generate()
    ));
    std::fs::create_dir_all(&dir).expect("创建相对路径测试目录");
    let path = dir.join("相对图.png");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("写入相对路径测试文件");
    app.workspace.cwd = dir.clone();

    app.input = "./相对图.png 这张图有什么问题".into();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);

    assert!(app.attachments.is_empty(), "路径文本不得被转成附件");
    assert!(app.input.is_empty(), "应作为消息提交并清空输入");
    assert!(!app.running, "无 Agent 时进入排队而不是直接运行");

    std::fs::remove_dir_all(&dir).ok();
}

/// 普通单词与自然语言不能被路径判定劫持成附件或"找不到文件"。
#[test]
fn input_path_judgement_ignores_plain_words() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    for text in ["hello", "看看这个", "notes.md 怎么说"] {
        app.input = text.into();
        app.cursor = app.input.len();
        assert!(!app.input_looks_like_path(), "`{text}` 不是文件路径");
        assert!(app.messages.is_empty(), "`{text}` 不得产生文件错误");
        assert_eq!(app.input, text, "判定不得改动输入");
    }
}

/// 单段绝对路径与命令名无法区分，按命令处理，不能被当成路径吞掉。
#[test]
fn input_path_judgement_lets_single_segment_absolute_path_reach_command_route() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/tmp".into();
    app.cursor = app.input.len();
    assert!(!app.input_looks_like_path());
    assert!(app.messages.is_empty());
}

/// 路径输入时不得弹出命令补全面板。
#[test]
fn native_command_panel_hidden_for_path_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/usr/local/bin/lucia".into();
    app.cursor = app.input.len();
    assert!(!app.handle_native_command_key(KeyCode::Tab, KeyModifiers::NONE, None));
    assert_eq!(app.input, "/usr/local/bin/lucia", "Tab 不得改写路径输入");

    // 单独 `/` 仍是补全入口。
    app.input = "/".into();
    app.cursor = 1;
    assert!(app.handle_native_command_key(KeyCode::Tab, KeyModifiers::NONE, None));
    assert!(app.input.trim_end().contains('/'));
}

/// 形如路径但不存在的输入放行给主输入区当文本，不报"找不到文件"也不报未知命令。
#[test]
fn native_command_route_lets_missing_path_through_without_error() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/nonexistent/lucia.png".into();
    app.cursor = app.input.len();
    let consumed = app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None);

    assert!(!consumed, "路径输入不能被命令路由消费");
    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "/nonexistent/lucia.png", "输入必须原样保留");
    assert!(app.messages.is_empty(), "不得产生文件错误或未知命令提示");
}

/// 不含额外 `/` 的未知命令仍按命令名笔误提示，不被路径判定吞掉。
#[test]
fn native_command_route_keeps_unknown_command_hint() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/modle".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(app.attachments.is_empty());
    assert!(app
        .messages
        .last()
        .is_some_and(|message| message.text.contains("未知命令")));
}

/// `/attach` 找不到文件或缺少参数时必须给出提示，且不创建附件。
#[test]
fn native_attach_command_reports_missing_file_and_usage() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/attach".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.messages[0].text.contains("用法：/attach"));

    app.input = "/attach /nonexistent/lucia.png".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

    assert!(app.attachments.is_empty());
    assert!(app.messages.last().is_some_and(|message| {
        matches!(message.kind, MsgKind::Error) && message.text.contains("/nonexistent/lucia.png")
    }));
}

/// `/model` 必须同步 Agent 与两条压缩路径的路由和窗口；无上游时给出提示。
#[tokio::test]
async fn model_command_switches_shared_agent_upstream() {
    use agent_context::{NativeContextLoader, DEFAULT_MAX_SUMMARY_TOKENS};
    use agent_core::config::ModelConfig;
    use agent_core::model::{ChatModel, MessageRole, ModelMessage, ModelRequest, ModelResponse};
    use agent_core::{Agent, ContextLoadRequest, InMemoryEventSink};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// 记录摘要请求，防止热切换回归测试访问真实模型服务。
    struct CapturingSummaryModel(Arc<Mutex<Vec<ModelRequest>>>);

    #[async_trait]
    impl ChatModel for CapturingSummaryModel {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            self.0.lock().expect("摘要请求记录锁不应损坏").push(request);
            Ok(ModelResponse::text("保留用户任务、已完成步骤与下一步。"))
        }
    }

    impl agent_core::model::ProviderAdapter for CapturingSummaryModel {
        fn name(&self) -> &'static str {
            "capturing-summary"
        }
    }

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "gpt-5".into());
    app.model_upstreams = vec![
        ModelConfig {
            name: "openai-main".into(),
            provider: agent_core::model::ProviderKind::OpenAi,
            model: "gpt-5".into(),
            api_key: Some("test-key".into()),
            api_key_env: None,
            base_url: None,
            openai_protocol: agent_core::model::OpenAiProtocol::default(),
            extra_headers: Default::default(),
            gateway: Default::default(),
            aws_region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        },
        ModelConfig {
            name: "openai-compatible-secondary".into(),
            provider: agent_core::model::ProviderKind::OpenAiCompatible,
            model: "gpt-4o".into(),
            api_key: Some("test-key".into()),
            api_key_env: None,
            base_url: Some("https://compatible.example.test/v1".into()),
            openai_protocol: agent_core::model::OpenAiProtocol::ChatCompletions,
            extra_headers: Default::default(),
            gateway: Default::default(),
            aws_region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        },
    ];
    let (gateway, options) = build_demo_gateway();
    let native_context_loader = Arc::new(
        NativeContextLoader::new(
            gateway.clone(),
            options.provider.clone(),
            options.model.clone(),
            false,
            DEFAULT_MAX_SUMMARY_TOKENS,
            Arc::new(InMemoryEventSink::new()),
        )
        .expect("应创建原生上下文加载器")
        .with_context_window(Some(2_000_000)),
    );
    app = app
        .with_context_window(Some(2_000_000))
        .with_native_context_loader(native_context_loader.clone());
    let agent =
        Arc::new(Agent::new(gateway.clone(), options).with_context_loader(native_context_loader));

    // 面板打开后按 Enter 切换到当前高亮项。
    app.input = "/model".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));
    assert!(app.native_command.model_panel.is_some());
    assert!(app.handle_native_command_key(KeyCode::Down, KeyModifiers::NONE, Some(&agent)));
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));
    assert!(app.native_command.model_panel.is_none());
    assert_eq!(app.model_name, "gpt-4o");
    let route = agent.active_route();
    assert_eq!(route.provider, "openai-compatible-secondary");
    assert_eq!(route.model, "gpt-4o");

    let context_window = app.context_window.expect("切换后的模型应声明窗口");
    assert!(context_window < 2_000_000);
    assert!(Arc::ptr_eq(&app.context_loader, &agent.context_loader()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    gateway
        .upsert(
            "openai-compatible-secondary",
            Arc::new(CapturingSummaryModel(Arc::clone(&requests))),
        )
        .expect("应替换为不联网的摘要模型");
    // 约占新窗口的 85%，仍远低于旧窗口水位；只有窗口和路由一起更新才会调用新模型。
    let history_chars = usize::try_from(context_window).expect("测试窗口应可表示") * 4 * 85 / 100;
    let messages = vec![
        ModelMessage::text(MessageRole::User, "h".repeat(history_chars)),
        ModelMessage::text(MessageRole::Assistant, "历史任务已处理"),
        ModelMessage::text(MessageRole::User, "继续处理"),
        ModelMessage::text(MessageRole::Assistant, "正在处理"),
    ];
    for (context_loader, user_initiated, provider) in [
        (agent.context_loader(), false, "openai-compatible-secondary"),
        (app.context_loader.clone(), true, "native"),
    ] {
        let loaded = context_loader
            .load(ContextLoadRequest {
                run_id: format!("switched-model-{user_initiated}"),
                step: 0,
                provider: provider.into(),
                model: "gpt-4o".into(),
                system: None,
                messages: messages.clone(),
                user_initiated,
            })
            .await
            .expect("自动与手动压缩都应使用切换后的摘要模型");
        let summary = loaded.messages.first().expect("压缩后应保留摘要");
        assert_eq!(summary.role, MessageRole::Developer);
        assert!(summary
            .text_content()
            .contains("保留用户任务、已完成步骤与下一步。"));
        assert_eq!(loaded.messages.last(), messages.last());
    }
    let captured = requests.lock().expect("摘要请求记录锁不应损坏");
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|request| request.model == "gpt-4o"));
    drop(captured);

    // 按名称切换未知上游必须报错而不改变当前路由。
    app.input = "/model missing".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));
    assert_eq!(agent.active_route().provider, "openai-compatible-secondary");

    // 无上游配置时给出可读提示。
    let (tx2, _rx2) = mpsc::unbounded_channel();
    let mut empty = App::new(tx2, "gpt-5".into());
    empty.input = "/model".into();
    empty.cursor = empty.input.len();
    assert!(empty.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));
    assert!(empty.native_command.model_panel.is_none());
}

/// 插件管理页与命令面板共用一块底板：标题画在上分隔线，列表用指示条与面板底色。
#[cfg(feature = "plugins")]
#[test]
fn plugin_management_page_uses_the_command_palette_chrome() {
    use crate::command::{NativePluginDialog, NativePluginDialogData, NativePluginDialogStatus};
    use crate::theme::{COLOR_PALETTE_COMMAND, COLOR_PALETTE_SELECTION_BG};
    use agent_plugin_manager::InstalledPlugin;

    let plugin = |id: &str, enabled: bool| InstalledPlugin {
        id: id.into(),
        name: id.into(),
        version: "1.0.0".into(),
        api_version: "0.10.0".into(),
        enabled,
        manifest: format!("plugins/{id}/plugin.toml"),
        sha256: "a".repeat(64),
        source: "fixture".into(),
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.native_command.plugin_dialog = Some(NativePluginDialog {
        selected: 1,
        status: NativePluginDialogStatus::Ready(NativePluginDialogData {
            plugins: vec![plugin("alpha", true), plugin("beta", false)],
            version_history: Vec::new(),
            authorizations: std::collections::HashMap::new(),
        }),
        path_mode: None,
        path_input: String::new(),
        remove_confirmation: None,
        enable_confirmation: None,
        pending_owner_selection: None,
        review_scroll: 0,
    });
    let (width, height) = (80u16, 24u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("应创建测试终端");
    terminal
        .draw(|frame| render_native_plugin_dialog(frame, &mut app, composer_anchor(frame)))
        .expect("应渲染插件管理页");
    let buffer = terminal.backend().buffer();
    let lines: Vec<String> = (0..height)
        .map(|row| {
            buffer
                .content()
                .iter()
                .skip(usize::from(row) * usize::from(width))
                .take(usize::from(width))
                .map(|cell| cell.symbol())
                .collect()
        })
        .collect();
    let screen = lines.join("\n");
    let compact = |text: &str| -> String {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    };
    let row_of = |needle: &str| {
        lines
            .iter()
            .position(|line| compact(line).contains(needle))
            .unwrap_or_else(|| panic!("应出现 {needle}：{screen}"))
    };

    // 标题画在上分隔线上；页面不再有圆角与竖向边框。
    let title_row = row_of("Plugins·人工管理");
    assert!(lines[title_row].contains('─'), "{screen}");
    assert!(!screen.contains('╭'), "{screen}");
    assert!(!screen.contains('│'), "{screen}");
    assert!(compact(&screen).contains("e启停"), "{screen}");

    // 选中行带指示条并铺面板底色；未选中行的插件 ID 仍用命令面板的强调色。
    let selected_row = row_of("beta");
    assert!(lines[selected_row].contains('▌'), "{screen}");
    assert_eq!(
        buffer.content()[selected_row * usize::from(width)].bg,
        COLOR_PALETTE_SELECTION_BG
    );
    let alpha_row = row_of("alpha");
    let alpha_column = lines[alpha_row].find("alpha").expect("应定位插件 ID");
    assert_eq!(
        buffer.content()[alpha_row * usize::from(width) + alpha_column]
            .style()
            .fg,
        Some(COLOR_PALETTE_COMMAND)
    );
}
