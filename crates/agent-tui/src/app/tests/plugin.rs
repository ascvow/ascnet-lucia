//! 插件视图、渲染器与触发面板的回归测试。

#[cfg(feature = "plugins")]
use super::*;

/// Startup activation events render in the footer once, then collapse into a plugin count.
///
/// 启动激活事件应在底部信息栏右侧展示一次，随后收敛为插件数量。
#[cfg(feature = "plugins")]
#[test]
fn plugin_status_shows_startup_details_then_compact_count() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app = app.with_loading_plugins(vec!["mcp".into(), "skill".into()]);
    app.mark_plugin_ready(
        "mcp".into(),
        &[json!({
            "source": {"id": "mcp"},
            "data": {"text": "MCP 插件等待配置"}
        })],
        0,
    );
    app.mark_plugin_ready(
        "skill".into(),
        &[json!({
            "source": {"id": "skill"},
            "presentation": {"text": "已加载 1 个 Skill"}
        })],
        0,
    );
    app.finish_progressive_plugin_loading();
    assert_eq!(
        app.plugin_status_content(),
        (
            "✓",
            "Plugins loaded · mcp: MCP 插件等待配置 · skill: 已加载 1 个 Skill".into()
        )
    );

    let backend = TestBackend::new(100, 18);
    let mut terminal = Terminal::new(backend).expect("创建插件状态测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染插件启动状态");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(rendered.contains("Pluginsloaded"), "{rendered:?}");

    for _ in 0..PLUGIN_STATUS_DETAIL_TICKS {
        app.tick_plugin_status();
    }
    assert_eq!(app.plugin_status_content(), ("◈", "2 plugins".into()));
}

/// 通用底栏事件必须按 Host 注入的插件来源覆盖状态，并持续显示在底部信息行。
#[cfg(feature = "plugins")]
#[test]
fn plugin_footer_status_is_trusted_persistent_and_replaceable() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    for (text, tone) in [
        ("Permission: review", "muted"),
        ("Permission: allow all", "warning"),
    ] {
        assert!(apply_plugin_footer_event(
            &mut app,
            &json!({
                "source": {"type": "plugin", "id": "permission"},
                "name": "permission.mode.status",
                "presentation": {"target": "footer", "text": text, "tone": tone}
            })
        )
        .expect("应用权限底栏状态"));
    }
    assert_eq!(app.plugin_footer_statuses.len(), 1);
    assert_eq!(
        app.plugin_footer_statuses["permission"].0,
        "Permission: allow all"
    );

    let backend = TestBackend::new(100, 18);
    let mut terminal = Terminal::new(backend).expect("创建权限底栏测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染权限底栏状态");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Permission: allow all"), "{rendered:?}");
}

/// 底栏状态缺少可信插件来源时必须拒绝，避免 Guest 伪造其他插件的常驻状态。
#[cfg(feature = "plugins")]
#[test]
fn plugin_footer_status_requires_trusted_source() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let error = apply_plugin_footer_event(
        &mut app,
        &json!({"presentation": {"target": "footer", "text": "伪造状态"}}),
    )
    .expect_err("无可信来源的底栏状态必须失败");

    assert!(error.to_string().contains("可信插件来源"));
    assert!(app.plugin_footer_statuses.is_empty());
}

/// 渐进加载期间应同时展示剩余插件和已经可用的插件数量。
#[cfg(feature = "plugins")]
#[test]
fn plugin_status_reports_progressive_ready_count() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app =
        App::new(tx, "测试模型".into()).with_loading_plugins(vec!["command".into(), "mcp".into()]);

    app.mark_plugin_ready("command".into(), &[], 0);

    let (_, status) = app.plugin_status_content();
    assert!(status.contains("mcp"), "{status}");
    assert!(!status.contains("command"), "{status}");
    assert!(status.contains("1 ready"), "{status}");
}

/// Partial plugin failures retain successes and remain visible in the compact footer count.
///
/// 单插件失败应保留成功插件，并在紧凑底栏中持续显示失败数量。
#[cfg(feature = "plugins")]
#[test]
fn plugin_status_keeps_partial_successes() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app = app.with_loading_plugins(vec!["skill".into(), "mcp".into()]);
    app.mark_plugin_ready("skill".into(), &[], 0);
    app.mark_plugin_failed(PluginLoadFailure {
        plugin_id: "mcp".into(),
        reason: "初始化超时".into(),
        blocked_by: Vec::new(),
    });
    app.finish_progressive_plugin_loading();

    let (icon, status) = app.plugin_status_content();
    assert_eq!(icon, "!");
    assert!(status.contains("Plugins partially loaded"), "{status}");
    assert!(status.contains("mcp: load failed"), "{status}");
    assert_eq!(app.plugin_status_color(), COLOR_WARNING);

    for _ in 0..PLUGIN_STATUS_DETAIL_TICKS {
        app.tick_plugin_status();
    }
    assert_eq!(app.plugin_status_content(), ("◈", "1 plugins · ✗ 1".into()));
}

/// 插件仍在渐进加载时，普通输入应立即启动已有工具快照，不进入等待队列。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_loading_does_not_block_ready_agent_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app =
        App::new(tx, "测试模型".into()).with_loading_plugins(vec!["mcp".into(), "skill".into()]);
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.input = "立即执行".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert!(app.running);
    assert!(app.queued_inputs.is_empty());
    assert!(app.input.is_empty());
    assert!(app
        .messages
        .iter()
        .any(|message| matches!(message.kind, MsgKind::User) && message.text == "立即执行"));
}

/// 先声明的显式 manifest 应覆盖同 ID 受管理插件，同时保留其他插件。
#[cfg(feature = "plugins")]
#[test]
fn explicit_plugin_manifest_overrides_managed_manifest() {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("生成测试时间戳")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "lucia-plugin-manifest-merge-{}-{nonce}",
        std::process::id()
    ));
    let explicit = root.join("explicit.toml");
    let managed_same = root.join("managed-same.toml");
    let managed_other = root.join("managed-other.toml");
    fs::create_dir_all(&root).expect("创建插件合并测试目录");
    let manifest = |id: &str, name: &str| {
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.8.0\"\nwasm = \"plugin.wasm\"\n"
        )
    };
    fs::write(&explicit, manifest("mcp", "显式 MCP")).expect("写入显式插件 manifest");
    fs::write(&managed_same, manifest("mcp", "受管理 MCP")).expect("写入同 ID 受管理插件 manifest");
    fs::write(&managed_other, manifest("skill", "受管理 Skill"))
        .expect("写入其他受管理插件 manifest");

    let mut manifests = vec![explicit.clone()];
    merge_plugin_manifests(&mut manifests, vec![managed_same, managed_other.clone()]);

    assert_eq!(manifests, vec![explicit, managed_other]);
    fs::remove_dir_all(root).expect("清理插件合并测试目录");
}

/// 自动发现只接受插件根目录的直接 bundle，避免把受管理版本目录或散落文件重复加载。
#[cfg(feature = "plugins")]
#[test]
fn plugin_discovery_scans_direct_bundles_only() {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("生成测试时间戳")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "lucia-plugin-discovery-{}-{nonce}",
        std::process::id()
    ));
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    let managed = root.join("managed/1.0.0");
    fs::create_dir_all(&alpha).expect("创建 alpha bundle");
    fs::create_dir_all(&beta).expect("创建 beta bundle");
    fs::create_dir_all(&managed).expect("创建嵌套受管理 bundle");
    fs::write(alpha.join("plugin.toml"), "alpha").expect("写入 alpha manifest");
    fs::write(beta.join("plugin.toml"), "beta").expect("写入 beta manifest");
    fs::write(managed.join("plugin.toml"), "managed").expect("写入嵌套 manifest");
    fs::write(root.join("plugin.toml"), "loose").expect("写入散落 manifest");

    let discovered = discover_plugin_manifests(&root).expect("插件目录应可扫描");

    assert_eq!(
        discovered,
        vec![alpha.join("plugin.toml"), beta.join("plugin.toml")]
    );
    fs::remove_dir_all(root).expect("清理插件发现测试目录");
}

/// 不存在的自动发现目录应视为空目录，使首次启动无需预创建插件文件夹。
#[cfg(feature = "plugins")]
#[test]
fn missing_plugin_discovery_root_is_empty() {
    let root = std::env::temp_dir().join(format!(
        "lucia-missing-plugin-discovery-{}",
        std::process::id()
    ));
    fs::remove_dir_all(&root).ok();

    assert!(discover_plugin_manifests(&root)
        .expect("缺失目录不应报错")
        .is_empty());
}

/// 用户禁用列表按插件 ID 剔除受管理与显式 manifest，无法解析的保留给容错加载器。
#[cfg(feature = "plugins")]
#[test]
fn disabled_plugins_are_removed_from_runtime_composition() {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("生成测试时间戳")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "lucia-disabled-plugin-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("创建插件禁用测试目录");
    let manifest = |id: &str| {
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\napi_version = \"0.8.0\"\nwasm = \"plugin.wasm\"\n"
        )
    };
    let teammate = root.join("teammate.toml");
    let context = root.join("context.toml");
    let invalid = root.join("invalid.toml");
    fs::write(&teammate, manifest("teammate")).expect("写入待禁用插件 manifest");
    fs::write(&context, manifest("context")).expect("写入保留插件 manifest");
    fs::write(&invalid, "not-a-manifest").expect("写入无效 manifest");

    let mut manifests = vec![teammate, context.clone(), invalid.clone()];
    remove_disabled_plugin_manifests(&mut manifests, &["teammate".to_string()]);

    assert_eq!(manifests, vec![context, invalid]);
    fs::remove_dir_all(root).expect("清理插件禁用测试目录");
}

/// Invalid manifests remain visible to background loading instead of blocking first paint.
///
/// 无效 manifest 应交给后台加载器报告，不应阻断 TUI 首帧。
#[cfg(feature = "plugins")]
#[test]
fn invalid_plugin_manifest_does_not_block_startup_labels() {
    let invalid = PathBuf::from("/tmp/lucia-invalid-plugin.toml");
    let labels = plugin_manifest_ids(std::slice::from_ref(&invalid));

    assert_eq!(labels, vec!["lucia-invalid-plugin.toml"]);
}

/// 周期插件视图刷新运行期间的新请求必须合并，不能并行堆积后台任务。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_view_refresh_coalesces_pending_requests() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Right, "后台刷新"));
    let host = Arc::new(LivePluginHost::new());

    app.schedule_plugin_views_refresh(host.clone());
    assert!(app.plugin_refresh_task.is_some());
    assert!(!app.plugin_refresh_pending);

    app.schedule_plugin_views_refresh(host);
    assert!(app.plugin_refresh_pending);

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("后台插件刷新应按时返回")
        .expect("后台插件刷新通道不应关闭");
    assert!(matches!(event, UiEvent::PluginFramesLoaded(_)));
    app.plugin_refresh_task
        .take()
        .expect("后台刷新任务应存在")
        .await
        .expect("后台刷新任务不应 panic");
}

/// 工具 renderer 返回的帧必须替换默认工具行并保持消息调用身份。
#[tokio::test]
#[cfg(feature = "plugins")]
async fn tool_renderer_frame_is_applied_to_timeline_message() {
    let host = Arc::new(LivePluginHost::new());
    host.publish(Arc::new(ToolMessagePluginHost))
        .expect("发布测试插件");
    host.ui_declarations().await.expect("建立 UI 路由");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.last_message_width = 72;
    app.last_viewport = 20;
    app.messages.push(Msg::tool_started(ToolCall::new(
        "call-1",
        "test_tool",
        json!({"title": "整理方案"}),
    )));

    app.schedule_tool_render(Arc::clone(&host), "call-1");
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("工具 renderer 应按时返回")
        .expect("UI 通道不应关闭");
    let UiEvent::ToolFrameLoaded {
        call_id,
        revision,
        width,
        result,
    } = event
    else {
        panic!("应返回工具消息帧");
    };
    app.apply_tool_frame(&call_id, revision, width, result);

    let text = app.messages[0]
        .to_lines(false, 72)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(app.messages[0].tool_call_id(), Some("call-1"));
    assert!(text.contains("自定义工具 · 运行中"), "{text:?}");
    assert!(!text.contains("● test_tool"), "{text:?}");
}

/// 多行工具 renderer 默认只显示首行，展开后显示其余详情行。
#[test]
#[cfg(feature = "plugins")]
fn multi_line_tool_renderer_supports_collapsed_details() {
    let mut message = Msg::tool_started(ToolCall::new("call-1", "test_tool", json!({})));
    message.tool_frame = Some(PluginUiFrame {
        view_id: "test-message".into(),
        visible: true,
        lines: ["自定义摘要", "自定义详情"]
            .into_iter()
            .map(|text| UiLine {
                spans: vec![UiSpan {
                    text: text.into(),
                    style: UiStyle::default(),
                }],
            })
            .collect(),
        cursor: None,
    });

    let collapsed = message
        .to_lines(false, 72)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(collapsed.contains("▸ 自定义摘要"), "{collapsed:?}");
    assert!(!collapsed.contains("自定义详情"), "{collapsed:?}");

    message.tool_expanded = true;
    let expanded = message
        .to_lines(false, 72)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(expanded.contains("▾ 自定义摘要"), "{expanded:?}");
    assert!(expanded.contains("自定义详情"), "{expanded:?}");
}

/// 晚到的旧 revision 不得覆盖较新的工具消息帧。
#[test]
#[cfg(feature = "plugins")]
fn stale_tool_renderer_frame_is_ignored() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut message = Msg::tool_started(ToolCall::new("call-1", "test_tool", json!({})));
    message.tool_render_revision = 2;
    app.messages.push(message);
    let frame = |text: &str| PluginUiFrame {
        view_id: "test-message".into(),
        visible: true,
        lines: vec![UiLine {
            spans: vec![UiSpan {
                text: text.into(),
                style: UiStyle::default(),
            }],
        }],
        cursor: None,
    };

    app.apply_tool_frame("call-1", 1, 72, Ok(Some(frame("旧帧"))));
    assert!(app.messages[0].tool_frame.is_none());
    app.apply_tool_frame("call-1", 2, 72, Ok(Some(frame("最终帧"))));
    assert_eq!(
        app.messages[0].tool_frame.as_ref().unwrap().lines[0].spans[0].text,
        "最终帧"
    );
}

/// 独占输入插件必须使用完整边框和声明标题，形成与主输入区分离的审批卡片。
#[test]
#[cfg(feature = "plugins")]
fn input_plugin_renders_bordered_title_card() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建输入插件测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Input, "Permission"));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染输入插件卡片");
    let rows = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let title_row = rows
        .iter()
        .position(|row| row.contains(" Permission "))
        .expect("应显示插件声明标题");

    assert!(rows[title_row].contains('┌'));
    assert!(rows[title_row].contains('┐'));
    assert!(rows[title_row + 1].contains('│'));
    assert!(rows[title_row + 1].contains('插'));
    assert!(rows[title_row + 1].contains('件'));
    assert!(rows[title_row + 7].contains('└'));
    assert!(rows[title_row + 7].contains('┘'));
}

/// 验证右侧插槽与主界面可以同时渲染，且插件获得实际内容尺寸。
#[test]
#[cfg(feature = "plugins")]
fn plugin_dock_renders_inside_main_ui() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(Msg::new(MsgKind::User, "主界面内容"));
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Right, "右侧插件"));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染带插件的界面");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    assert!(text.contains("主界面内容"), "{text:?}");
    assert!(text.contains("右侧插件"), "{text:?}");
    assert!(text.contains("插件内容"), "{text:?}");
    assert!(app.plugin_views[0].area.width > 0);
}

/// 验证可见对话框覆盖主界面并优先接收按键。
#[test]
#[cfg(feature = "plugins")]
fn plugin_dialog_is_modal() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Dialog, "插件对话框"));

    let route = app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE);
    let PluginKeyRoute::Input(input) = route else {
        panic!("对话框应优先接收输入");
    };

    assert_eq!(input.plugin_id, "test-plugin");
    assert_eq!(input.view_id, "dialog");
    assert!(matches!(
        input.event,
        UiInputEvent::Key { ref code, .. } if code == "enter"
    ));
}

/// A dynamic subview replaces the main view while preserving instance input routing.
/// 动态子视图应替换主视图，并保留实例化输入路由。
#[test]
#[cfg(feature = "plugins")]
fn plugin_subview_replaces_main_view_and_routes_instance_input() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::User, "main-view-content"));
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Subview, "Agent 详情"));
    app.apply_view_navigation(
        "test-plugin",
        UiNavigationRequest {
            request_id: "open-agent-1".into(),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: "subview".into(),
                    instance_id: "agent-1".into(),
                    title: Some("Reviewer Agent".into()),
                },
            },
        },
    )
    .expect("打开插件子视图");
    app.update_plugin_frame(
        "test-plugin",
        Some("agent-1"),
        PluginUiFrame {
            view_id: "subview".into(),
            visible: true,
            lines: vec![UiLine {
                spans: vec![UiSpan {
                    text: "subview-content".into(),
                    style: UiStyle::default(),
                }],
            }],
            cursor: Some(agent_plugin_protocol::UiCursor { x: 5, y: 2 }),
        },
    );

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染子视图");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Reviewer Agent"), "{rendered:?}");
    assert!(rendered.contains("subview-content"), "{rendered:?}");
    assert!(!rendered.contains("main-view-content"), "{rendered:?}");
    let active_area = app.view_stack.active().expect("子视图保持激活").area;
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("再次渲染子视图");
    assert_eq!(
        terminal.get_cursor_position().expect("读取终端光标"),
        ratatui::layout::Position::new(active_area.x + 5, active_area.y + 2)
    );

    let PluginKeyRoute::Input(input) = app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("子视图应接收键盘输入");
    };
    assert_eq!(input.instance_id.as_deref(), Some("agent-1"));
    assert_eq!(input.view_id, "subview");

    assert!(matches!(
        app.route_plugin_key(KeyCode::Esc, KeyModifiers::NONE),
        PluginKeyRoute::Consumed
    ));
    assert!(app.view_stack.is_main());
}

/// 验证 Tab 在主输入区与可聚焦停靠视图之间循环。
#[test]
#[cfg(feature = "plugins")]
fn tab_cycles_plugin_focus() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Left, "左侧插件"));

    assert!(matches!(
        app.route_plugin_key(KeyCode::Tab, KeyModifiers::NONE),
        PluginKeyRoute::Consumed
    ));
    assert_eq!(app.plugin_focus, Some(0));
    assert!(matches!(
        app.route_plugin_key(KeyCode::Tab, KeyModifiers::NONE),
        PluginKeyRoute::Consumed
    ));
    assert_eq!(app.plugin_focus, None);
}

/// 验证触发激活时输入面板渲染插件提供的帧内容。
#[test]
#[cfg(feature = "plugins")]
fn input_panel_renders_plugin_frame_when_trigger_active() {
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).expect("创建输入面板测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::InputPanel, "筛选");
    view.declaration.input_triggers = vec![":".into()];
    view.frame = Some(PluginUiFrame {
        view_id: view.declaration.view_id.clone(),
        visible: true,
        lines: vec![UiLine {
            spans: vec![UiSpan {
                text: ":review  筛选待审查项目".into(),
                style: UiStyle::default(),
            }],
        }],
        cursor: None,
    });
    app.plugin_views.push(view);
    app.input = ":rev".into();
    app.cursor = app.input.len();

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染输入面板");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(text.contains(":review"), "{text}");
    assert!(text.contains("筛选待审查项目"), "{text}");

    // 触发退出激活后面板整体消失，不依赖插件端状态。
    app.input.clear();
    app.cursor = 0;
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染无触发界面");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!text.contains("筛选待审查项目"), "{text}");
}

/// 常驻上下文架必须显示在输入框上方，并依次让位给触发面板和独占输入视图。
#[test]
#[cfg(feature = "plugins")]
fn composer_shelf_obeys_input_priority() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut shelf = test_plugin_view(UiPlacement::ComposerShelf, "计划");
    shelf.frame.as_mut().unwrap().lines[0].spans[0].text = "当前计划步骤".into();
    app.plugin_views.push(shelf);

    assert_eq!(app.visible_composer_panels(), vec![0]);
    assert!(app.composer_panel_height() > 0);

    let mut filter = test_plugin_view(UiPlacement::InputPanel, "筛选");
    filter.declaration.input_triggers = vec![":".into()];
    app.plugin_views.push(filter);
    app.input = ":res".into();
    app.cursor = app.input.len();
    assert_eq!(app.visible_composer_panels(), vec![1]);

    app.plugin_views
        .push(test_plugin_view(UiPlacement::Input, "审批"));
    assert!(app.visible_composer_panels().is_empty());
}

/// 验证触发手势键按面板可见性路由给触发视图。
#[test]
#[cfg(feature = "plugins")]
fn trigger_gestures_route_to_trigger_view() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::InputPanel, "筛选");
    view.declaration.input_triggers = vec![":".into()];
    app.plugin_views.push(view);
    app.input = ":res".into();
    app.cursor = app.input.len();

    // Tab 与 Enter 在触发激活时始终交给触发视图。
    for code in [KeyCode::Tab, KeyCode::Enter] {
        let PluginKeyRoute::Input(input) = app.route_plugin_key(code, KeyModifiers::NONE) else {
            panic!("触发手势应路由给触发视图");
        };
        assert_eq!(input.view_id, "inputpanel");
    }
    // 面板可见时方向键交给触发视图；隐藏时保留给主界面。
    assert!(matches!(
        app.route_plugin_key(KeyCode::Up, KeyModifiers::NONE),
        PluginKeyRoute::Input(_)
    ));
    app.plugin_views[0]
        .frame
        .as_mut()
        .expect("测试视图应包含帧")
        .visible = false;
    assert!(matches!(
        app.route_plugin_key(KeyCode::Up, KeyModifiers::NONE),
        PluginKeyRoute::Main
    ));
    // 带修饰键的 Enter 是换行手势，必须留给输入编辑器。
    assert!(matches!(
        app.route_plugin_key(KeyCode::Enter, KeyModifiers::ALT),
        PluginKeyRoute::Main
    ));
    // 无触发前缀时手势键全部回归主界面语义。
    app.input.clear();
    assert!(matches!(
        app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE),
        PluginKeyRoute::Main
    ));
}

/// 原生命令未声明的斜杠前缀应交给插件触发视图，不能被当作未知命令提前消费。
#[test]
#[cfg(feature = "plugins")]
fn plugin_slash_command_routes_after_native_commands() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::InputPanel, "权限模式");
    view.declaration.input_triggers = vec!["/permission".into()];
    app.plugin_views.push(view);
    app.input = "/per".into();
    app.cursor = app.input.len();

    assert!(app.native_command_panel_height() > 0);
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert_eq!(app.input, "/permission");
    assert_eq!(app.native_command_panel_height(), 0);
    assert_eq!(app.visible_input_panel(), Some(0));
    assert!(!app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    let PluginKeyRoute::Input(input) = app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("插件斜杠命令应路由给触发视图");
    };
    assert_eq!(input.view_id, "inputpanel");

    app.input = "/reasoning".into();
    app.cursor = app.input.len();
    assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
    assert!(app.native_command.reasoning_panel.is_some());
}

/// 验证会话摘要查询按标题过滤并返回 UI 契约状态。
#[tokio::test]
#[cfg(feature = "plugins")]
async fn sessions_page_filters_by_title() {
    let store = MemorySessionStore::new();
    for (id, title) in [("alpha", "架构讨论"), ("beta", "发布计划")] {
        let mut record = SessionRecord::new(
            SessionId::new(id).expect("创建候选会话标识"),
            Session::new(),
        )
        .expect("创建候选会话");
        record.title = Some(title.into());
        store.save(record, None).await.expect("保存候选会话");
    }
    let active = SessionId::new("alpha").expect("创建当前会话标识");

    let status = sessions_page(&store, &active, "架构", None, 6).await;
    let UiSessionListStatus::Ready { items, next_cursor } = status else {
        panic!("匹配查询应返回就绪状态");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "架构讨论");
    assert!(items[0].active);
    assert_eq!(next_cursor, None);

    let status = sessions_page(&store, &active, "不存在", None, 6).await;
    assert!(matches!(status, UiSessionListStatus::Empty));
}

/// 点击侧栏边框或内边距也应聚焦该视图，避免只有文字区域可点击。
#[test]
#[cfg(feature = "plugins")]
fn clicking_side_dock_chrome_focuses_plugin() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::Right, "右侧插件");
    view.area = Rect::new(12, 4, 20, 10);
    app.plugin_views.push(view);

    let routed = app.route_plugin_mouse(&MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 10,
        row: 3,
        modifiers: KeyModifiers::NONE,
    });

    assert!(routed.is_some());
    assert_eq!(app.plugin_focus, Some(0));
}
