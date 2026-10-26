//! 宿主原生 Agent 会话视图的导航、输入与渲染回归测试。

#[cfg(feature = "plugins")]
use super::*;
#[cfg(feature = "plugins")]
use crate::app::{AgentSessionView, AgentViewKeyOutcome};
#[cfg(feature = "plugins")]
use agent_plugin_protocol::{UiDeclaration, UiNavigationAction, UiNavigationRequest, UiPlacement};
#[cfg(feature = "plugins")]
use agent_runtime::{
    AgentEventStream, AgentHandle, AgentId, AgentLineage, AgentOutcome, AgentRuntimeApi,
    AgentSnapshot, AgentSpawnRequest, AgentStatus, RuntimePrincipal, RuntimeResult,
};
use std::str::FromStr;

/// 测试用 Runtime API：只提供路由测试需要的状态与订阅。
#[cfg(feature = "plugins")]
struct StubAgentApi {
    target: AgentId,
}

/// 构造一个固定成功句柄。
#[cfg(feature = "plugins")]
fn stub_handle(id: &str) -> AgentHandle {
    let id = AgentId::from_str(id).expect("测试 Agent ID 应合法");
    AgentHandle {
        id: id.clone(),
        lineage: AgentLineage {
            parent: None,
            root: id,
            depth: 0,
        },
    }
}

#[cfg(feature = "plugins")]
#[async_trait::async_trait]
impl AgentRuntimeApi for StubAgentApi {
    fn principal(&self) -> RuntimePrincipal {
        RuntimePrincipal::host()
    }

    fn identity(&self) -> AgentId {
        self.target.clone()
    }

    async fn spawn(&self, _request: AgentSpawnRequest) -> RuntimeResult<AgentHandle> {
        Ok(stub_handle(&self.target.to_string()))
    }

    async fn continue_agent(&self, target: &AgentId, _input: String) -> RuntimeResult<AgentHandle> {
        Ok(stub_handle(&target.to_string()))
    }

    async fn status(&self, target: &AgentId) -> RuntimeResult<AgentSnapshot> {
        Ok(AgentSnapshot {
            id: target.clone(),
            lineage: AgentLineage {
                parent: None,
                root: target.clone(),
                depth: 0,
            },
            status: AgentStatus::Running,
            permissions: Default::default(),
        })
    }

    async fn result(&self, _target: &AgentId) -> RuntimeResult<Option<AgentOutcome>> {
        Ok(None)
    }

    async fn wait(&self, _target: &AgentId) -> RuntimeResult<AgentOutcome> {
        Ok(AgentOutcome::Cancelled)
    }

    async fn cancel(&self, _target: &AgentId) -> RuntimeResult<bool> {
        Ok(true)
    }

    async fn subscribe(&self, _target: &AgentId) -> RuntimeResult<AgentEventStream> {
        Ok(AgentEventStream::empty())
    }
}

/// 打开一个成员原生会话视图，并挂上仅用于渲染的视图状态。
#[cfg(feature = "plugins")]
fn open_member_view(app: &mut App, member: &str, target: &str) -> AgentId {
    let target = AgentId::from_str(target).expect("测试 Agent ID 应合法");
    app.add_plugin_views(vec![UiDeclaration {
        plugin_id: "teammate".into(),
        view_id: "teammate-member-session".into(),
        title: "Member session".into(),
        placement: UiPlacement::Subview,
        size: Default::default(),
        focusable: true,
        input_triggers: Vec::new(),
    }]);
    app.apply_view_navigation(
        "teammate",
        UiNavigationRequest {
            request_id: format!("open-{member}"),
            action: UiNavigationAction::OpenAgentSession {
                view_id: "teammate-member-session".into(),
                instance_id: member.into(),
                target: target.to_string(),
                title: Some("Researcher".into()),
                prompt: Some("检查首轮实现".into()),
                notice: None,
            },
        },
    )
    .expect("原生会话导航应被接受");
    app.agent_views.push(AgentSessionView::new(
        "teammate".into(),
        member.into(),
        target.clone(),
        Arc::new(StubAgentApi {
            target: target.clone(),
        }),
        Some("检查首轮实现".into()),
        None,
    ));
    target
}

/// 子 Agent 会话同样消费重连元数据，失败增量不应残留到成功回答。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_retry_discards_only_the_partial_answer() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let view = app.active_agent_session_view_mut().unwrap();
    view.messages.clear();
    view.messages.push(Msg::new(MsgKind::ToolOk, "完成的工具"));
    view.apply_event(&AgentEvent::new(
        "retry",
        AgentEventKind::ModelTextDelta,
        0,
        json!({"delta":"失败文本"}),
    ));
    view.flush_pending_stream();
    view.apply_event(&AgentEvent::new(
        "retry",
        AgentEventKind::ModelTextDelta,
        0,
        json!({"delta":"未揭示的失败文本"}),
    ));
    view.apply_event(&AgentEvent::new(
        "retry",
        AgentEventKind::ModelRequest,
        0,
        json!({"retry_attempt":1,"max_retries":5,"retry_interval_ms":2000}),
    ));
    assert!(!view.has_pending_stream());
    view.apply_event(&AgentEvent::new(
        "retry",
        AgentEventKind::ModelTextDelta,
        0,
        json!({"delta":"恢复成功"}),
    ));
    view.flush_pending_stream();
    assert_eq!(view.messages.len(), 3);
    assert_eq!(view.messages[0].text, "完成的工具");
    assert!(view.messages[1].text.contains("1/5"));
    assert_eq!(view.messages[2].text, "恢复成功");
}

/// 原生会话视图沿用导航栈：Esc 返回主界面，重复打开只更新目标句柄。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_navigation_uses_the_shared_stack() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let first = open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");

    let route = app
        .active_agent_session_route()
        .expect("原生会话应处于激活状态");
    assert_eq!(route.plugin_id, "teammate");
    assert_eq!(route.instance_id, "member-1");
    assert_eq!(route.target, first);

    // 续跑换了句柄：实例 ID 不变，视图仍只有一个。
    app.apply_view_navigation(
        "teammate",
        UiNavigationRequest {
            request_id: "open-member-2".into(),
            action: UiNavigationAction::OpenAgentSession {
                view_id: "teammate-member-session".into(),
                instance_id: "member-1".into(),
                target: "3f2504e0-4f89-11d3-9a0c-0305e82c3302".into(),
                title: Some("Researcher".into()),
                prompt: None,
                notice: None,
            },
        },
    )
    .expect("换句柄应被接受");
    assert_eq!(app.agent_session_routes().len(), 1);

    assert!(matches!(
        app.handle_agent_session_key(KeyCode::Esc, KeyModifiers::NONE),
        AgentViewKeyOutcome::Exit
    ));
    assert!(app.view_stack.is_main());
}

/// 成员会话无插件帧时仍复用通用简洁标题，不能漏出计划或动态工具的注册名。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_tool_fallbacks_use_readable_titles() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let view = app.active_agent_session_view_mut().unwrap();
    for (index, name, expected) in [
        (1, "update_plan", "Update(Plan)"),
        (
            2,
            "mcp__mock__get_design_node",
            "Get(Mcp / Mock · Design Node)",
        ),
    ] {
        let id = format!("call-{index}");
        let call = ToolCall::new(&id, name, json!({"payload":"不塞进标题"}));
        view.apply_event(&AgentEvent::new(
            "run",
            AgentEventKind::ToolStarted,
            0,
            serde_json::to_value(&call).unwrap(),
        ));
        let running = view
            .messages
            .iter()
            .find(|message| message.tool_call_id() == Some(id.as_str()))
            .unwrap();
        assert!(running.to_lines(false, 80)[0]
            .to_string()
            .starts_with(&format!("● {expected}")));
        let result = ToolResult::error(&id, name, "权限拒绝");
        view.apply_event(&AgentEvent::new(
            "run",
            AgentEventKind::ToolFinished,
            0,
            serde_json::to_value(result).unwrap(),
        ));
        let message = view
            .messages
            .iter()
            .find(|message| message.tool_call_id() == Some(id.as_str()))
            .unwrap();
        assert_eq!(message.tool_call.as_ref().unwrap(), &call);
        let lines = message.to_lines(false, 80);
        assert!(lines[0].to_string().starts_with(&format!("● {expected}")));
        assert!(lines
            .iter()
            .any(|line| line.to_string().contains("权限拒绝")));
    }
}

/// 事件时间线复用主界面组件：助手正文、工具调用与结果都进入消息列表。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_events_build_main_ui_components() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let target = open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let view = app.active_agent_session_view_mut().expect("视图状态应存在");
    view.set_status(AgentStatus::Running);
    let mut push = |kind: AgentEventKind, payload: serde_json::Value| {
        view.apply_event(&AgentEvent::new("run-1", kind, 0, payload));
    };
    push(
        AgentEventKind::ModelTextDelta,
        json!({"index": 0, "delta": "我先查看文件"}),
    );
    push(
        AgentEventKind::ToolStarted,
        json!({"id": "call-1", "name": "read_file", "args": {"path": "src/lib.rs"}}),
    );
    push(
        AgentEventKind::ToolFinished,
        json!({
            "call_id": "call-1",
            "name": "read_file",
            "content": {"path": "src/lib.rs", "content": "fn main() {}"},
            "is_error": false,
            "details": null
        }),
    );

    assert!(view.messages.iter().any(|message| {
        matches!(message.kind, MsgKind::Assistant) && message.text.contains("我先查看文件")
    }));
    let tool = view
        .messages
        .iter()
        .find(|message| message.is_tool())
        .expect("工具调用应成为独立的工具组件");
    assert_eq!(
        tool.tool_call.as_ref().map(|call| call.name.as_str()),
        Some("read_file")
    );
    assert!(matches!(tool.kind, MsgKind::ToolOk));
    assert!(tool.tool_result.is_some());

    let backend = TestBackend::new(90, 20);
    let mut terminal = Terminal::new(backend).expect("创建原生会话测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染原生会话视图");
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
    assert!(rendered.contains("Lucia/Researcher"), "{rendered:?}");
    assert!(rendered.contains("检查首轮实现"), "{rendered:?}");
    assert!(rendered.contains("Running"), "{rendered:?}");
    assert!(rendered.contains("我先查看文件"), "{rendered:?}");
    assert!(rendered.contains("Read(src/lib.rs)"), "{rendered:?}");
    assert!(rendered.contains("SteertheAgent…"), "{rendered:?}");
    let _ = target;
}

/// 原生会话的输入框由 TUI 编辑，只有提交时才跨越插件边界。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_edits_and_submits_locally() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");

    for character in "继续分析".chars() {
        app.handle_agent_session_key(KeyCode::Char(character), KeyModifiers::NONE);
    }
    assert_eq!(
        app.active_agent_session_view()
            .expect("视图状态应存在")
            .input,
        "继续分析"
    );
    match app.handle_agent_session_key(KeyCode::Enter, KeyModifiers::NONE) {
        AgentViewKeyOutcome::Submit {
            plugin_id,
            view_id,
            instance_id,
            text,
        } => {
            assert_eq!(plugin_id, "teammate");
            assert_eq!(view_id, "teammate-member-session");
            assert_eq!(instance_id, "member-1");
            assert_eq!(text, "继续分析");
        }
        _ => panic!("非空输入提交必须产生 Submit"),
    }
    let view = app.active_agent_session_view().expect("视图状态应存在");
    assert!(view.input.is_empty(), "提交后输入框应清空");
    assert!(view
        .messages
        .iter()
        .any(|message| matches!(message.kind, MsgKind::User) && message.text == "继续分析"));

    // 空输入上的 Enter 不提交；Shift+Enter 与主输入框一样插入换行。
    assert!(matches!(
        app.handle_agent_session_key(KeyCode::Enter, KeyModifiers::NONE),
        AgentViewKeyOutcome::Consumed
    ));
    app.handle_agent_session_key(KeyCode::Char('a'), KeyModifiers::NONE);
    app.handle_agent_session_key(KeyCode::Enter, KeyModifiers::SHIFT);
    app.handle_agent_session_key(KeyCode::Char('b'), KeyModifiers::NONE);
    assert_eq!(
        app.active_agent_session_view()
            .expect("视图状态应存在")
            .input,
        "a\nb"
    );
}

/// 相同纯文本编辑在主/子会话得到相同草稿与 UTF-8 光标，提交仍由插件路由。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_editor_matches_main_editor() {
    let (tx, _) = mpsc::unbounded_channel();
    let mut main = App::new(tx.clone(), "测试模型".into());
    let mut child = App::new(tx, "测试模型".into());
    open_member_view(
        &mut child,
        "member-1",
        "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
    );
    let seed = "你好/path-one world\n第二行🙂结束";
    main.input = seed.into();
    main.cursor = seed.len();
    let view = child.active_agent_session_view_mut().unwrap();
    view.input = seed.into();
    view.cursor = seed.len();
    let keys = [
        (KeyCode::Home, KeyModifiers::NONE),
        (KeyCode::Right, KeyModifiers::NONE),
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Right, KeyModifiers::ALT),
        (KeyCode::Char('w'), KeyModifiers::CONTROL),
        (KeyCode::End, KeyModifiers::NONE),
        (KeyCode::Char('k'), KeyModifiers::CONTROL),
        (KeyCode::Down, KeyModifiers::NONE),
        (KeyCode::Char('a'), KeyModifiers::CONTROL),
        (KeyCode::Delete, KeyModifiers::NONE),
        (KeyCode::Char('d'), KeyModifiers::CONTROL),
        (KeyCode::Enter, KeyModifiers::SHIFT),
        (KeyCode::Char('好'), KeyModifiers::NONE),
        (KeyCode::Backspace, KeyModifiers::NONE),
        (KeyCode::Left, KeyModifiers::CONTROL),
        (KeyCode::Char('u'), KeyModifiers::CONTROL),
    ];
    for (code, modifiers) in keys {
        main.handle_key(code, modifiers, None);
        assert!(matches!(
            child.handle_agent_session_key(code, modifiers),
            AgentViewKeyOutcome::Consumed
        ));
        let view = child.active_agent_session_view().unwrap();
        assert_eq!(
            (&view.input, view.cursor),
            (&main.input, main.cursor),
            "{code:?} {modifiers:?}"
        );
        assert!(view.input.is_char_boundary(view.cursor));
    }
}

/// 共享滚动动作和半行滑块：越界可达首尾，到底恢复跟随，悬停不污染主会话。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_scroll_and_drag_match_main() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    let (tx, _) = mpsc::unbounded_channel();
    let mut main = App::new(tx.clone(), "测试模型".into());
    let mut child = App::new(tx, "测试模型".into());
    open_member_view(
        &mut child,
        "member-1",
        "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
    );
    main.last_max_scroll = 300;
    main.last_viewport = 20;
    main.scrollbar_area = Some(Rect::new(40, 4, 1, 20));
    {
        let view = child.active_agent_session_view_mut().unwrap();
        view.last_max_scroll = main.last_max_scroll;
        view.last_viewport = main.last_viewport;
        view.scrollbar_area = main.scrollbar_area;
    }
    let event = |kind, row| MouseEvent {
        kind,
        row,
        column: 40,
        modifiers: KeyModifiers::NONE,
    };
    child.handle_agent_session_mouse(&event(MouseEventKind::ScrollUp, 10));
    main.scroll_up(3);
    assert_eq!(child.active_agent_session_view().unwrap().scroll, Some(297));
    child.handle_agent_session_key(KeyCode::PageUp, KeyModifiers::NONE);
    main.handle_key(KeyCode::PageUp, KeyModifiers::NONE, None);
    assert_eq!(
        child.active_agent_session_view().unwrap().scroll,
        main.scroll
    );
    for mouse in [
        event(MouseEventKind::Down(MouseButton::Left), 22),
        event(MouseEventKind::Drag(MouseButton::Left), 0),
        event(MouseEventKind::Drag(MouseButton::Left), 50),
        event(MouseEventKind::Up(MouseButton::Left), 50),
    ] {
        main.handle_scrollbar_drag(&mouse);
        child.handle_agent_session_mouse(&mouse);
        let view = child.active_agent_session_view().unwrap();
        assert_eq!(view.scroll, main.scroll);
        assert_eq!(view.scrollbar_drag, main.scrollbar_drag);
    }
    assert_eq!(main.scroll, None);
    assert!(!main.scrollbar_drag);
    assert!(child.handle_mouse_event(&event(MouseEventKind::Moved, 10)));
    assert!(!child.handle_mouse_event(&event(MouseEventKind::Moved, 10)));
    assert!(!child.is_scrollbar_hovered);
}

/// 原生子会话经过同一窗口连续缩放后，输入光标、滚动条和跳转提示仍在正确区域。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_resize_keeps_composer_and_clears_scrollbar() {
    let (tx, _) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let view = app.active_agent_session_view_mut().unwrap();
    view.messages.push(Msg::new(
        MsgKind::Assistant,
        "中文正文与工具输出\n".repeat(120),
    ));
    view.input = "长行🙂中文与折行\n第二行\n第三行\n第四行".into();
    view.cursor = view.input.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    for (width, height) in [(100, 30), (40, 16), (20, 8), (80, 24), (32, 10), (100, 30)] {
        terminal.backend_mut().resize(width, height);
        terminal.autoresize().unwrap();
        app.active_agent_session_view_mut().unwrap().scroll = Some(0);
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        let view = app.active_agent_session_view().unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        assert!(cursor.x < width && cursor.y < height - 2);
        assert!((0..width).all(|x| terminal.backend().buffer()[(x, height - 2)].symbol() == "─"));
        if let Some(area) = view.scrollbar_area {
            assert_eq!(area.right(), width);
            assert!(area.bottom() < cursor.y);
        }
        if let Some(hint) = view.jump_hint_area {
            assert_eq!(hint.right(), width);
            assert!(hint.y < cursor.y);
        }
    }
    let old = app
        .active_agent_session_view()
        .unwrap()
        .scrollbar_area
        .unwrap();
    app.active_agent_session_view_mut()
        .unwrap()
        .messages
        .clear();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let view = app.active_agent_session_view().unwrap();
    assert!(view.scrollbar_area.is_none());
    assert!(view.jump_hint_area.is_none());
    for y in old.y..old.bottom() {
        assert!(!matches!(
            terminal.backend().buffer()[(old.x, y)].symbol(),
            "│" | "█" | "▀" | "▄"
        ));
    }
}

/// 工具点击只在松开时展开；拖动不得误展开或强迫历史视口贴底。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_tool_click_keeps_manual_scroll_and_jump_hint_works() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let (tx, _) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let view = app.active_agent_session_view_mut().unwrap();
    view.messages.clear();
    view.messages.push(Msg::new(MsgKind::ToolOk, "tool"));
    view.scroll = Some(2);
    view.hit_regions.push(super::super::ToolMessageHitRegion {
        message_index: 0,
        area: Rect::new(0, 4, 30, 2),
    });
    view.jump_hint_area = Some(Rect::new(40, 10, 10, 1));
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.handle_agent_session_mouse(&mouse(kind, 2, 4));
    }
    assert!(!app.active_agent_session_view().unwrap().messages[0].tool_expanded);
    app.handle_agent_session_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 4));
    assert!(!app.active_agent_session_view().unwrap().messages[0].tool_expanded);
    app.handle_agent_session_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 4));
    let view = app.active_agent_session_view().unwrap();
    assert!(view.messages[0].tool_expanded);
    assert_eq!(view.scroll, Some(2));
    app.handle_agent_session_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 45, 10));
    assert_eq!(app.active_agent_session_view().unwrap().scroll, None);
}

/// 子会话复用主会话节拍，中文不会被切开，结束边界补完且不改变历史滚动。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_reveal_matches_shared_pacer_and_flushes_boundaries() {
    let (tx, _) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let text = "中文🙂流式内容".repeat(80);
    let view = app.active_agent_session_view_mut().unwrap();
    view.scroll = Some(3);
    view.apply_event(&AgentEvent::new(
        "run",
        AgentEventKind::ModelTextDelta,
        0,
        json!({"delta": text}),
    ));
    assert!(view.has_pending_stream());
    let mut expected_pending = text.clone();
    let mut expected_pacer = super::super::run::RevealPacer::default();
    let mut expected = String::new();
    let now = std::time::Instant::now();
    for frame in 0..12 {
        let time = now + std::time::Duration::from_millis(frame * 16);
        expected.push_str(&expected_pacer.take_chunk(&mut expected_pending, time));
        app.reveal_pending_stream_at(time);
        let view = app.active_agent_session_view().unwrap();
        let actual = view
            .messages
            .iter()
            .filter(|m| matches!(m.kind, MsgKind::Assistant))
            .map(|m| m.text.as_str())
            .collect::<String>();
        assert_eq!(actual, expected);
        assert_eq!(view.scroll, Some(3));
    }
    app.active_agent_session_view_mut()
        .unwrap()
        .apply_event(&AgentEvent::new(
            "run",
            AgentEventKind::RunFinished,
            0,
            json!({}),
        ));
    let view = app.active_agent_session_view().unwrap();
    assert!(!view.has_pending_stream());
    assert_eq!(view.messages.last().unwrap().text, text);
    assert_eq!(view.scroll, Some(3));
}

/// 稳定实例换执行句柄保留草稿与时间线，迟到事件不允许写入新目标。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn agent_session_retarget_preserves_draft_and_rejects_stale_stream() {
    let (tx, _) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let old = open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let next: AgentId = "3f2504e0-4f89-11d3-9a0c-0305e82c3302".parse().unwrap();
    let view = app.active_agent_session_view_mut().unwrap();
    view.input = "下一次草稿".into();
    view.cursor = view.input.len();
    view.scroll = Some(2);
    view.scrollbar_drag = true;
    view.apply_event(&AgentEvent::new(
        "old",
        AgentEventKind::ModelTextDelta,
        0,
        json!({"delta":"旧回答"}),
    ));
    view.replace_target(next.clone());
    assert!(!view.has_pending_stream());
    assert!(!view.scrollbar_drag);
    app.apply_agent_view_events(
        "teammate",
        "member-1",
        &old,
        vec![AgentEvent::new(
            "old",
            AgentEventKind::ModelTextDelta,
            1,
            json!({"delta":"不应出现"}),
        )],
    )
    .await;
    app.apply_agent_view_events(
        "teammate",
        "member-1",
        &next,
        vec![AgentEvent::new(
            "next",
            AgentEventKind::ModelTextDelta,
            0,
            json!({"delta":"新回答"}),
        )],
    )
    .await;
    let view = app
        .agent_views
        .iter_mut()
        .find(|view| view.instance_id == "member-1")
        .unwrap();
    view.flush_pending_stream();
    let answers = view
        .messages
        .iter()
        .filter(|m| matches!(m.kind, MsgKind::Assistant))
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(answers, ["旧回答", "新回答"]);
    assert_eq!(view.input, "下一次草稿");
    assert_eq!(view.cursor, view.input.len());
    assert_eq!(view.scroll, Some(2));
}

/// 隐藏会话继续归档但不累积动画；另一成员草稿和主会话不受影响。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn agent_session_hidden_stream_archives_without_animation() {
    let (tx, _) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let first = open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    open_member_view(&mut app, "member-2", "3f2504e0-4f89-11d3-9a0c-0305e82c3302");
    app.active_agent_session_view_mut().unwrap().input = "另一个成员".into();
    app.apply_agent_view_events(
        "teammate",
        "member-1",
        &first,
        vec![AgentEvent::new(
            "first",
            AgentEventKind::ModelTextDelta,
            0,
            json!({"delta":"后台内容"}),
        )],
    )
    .await;
    let view = app
        .agent_views
        .iter_mut()
        .find(|view| view.instance_id == "member-1")
        .unwrap();
    assert!(!view.has_pending_stream());
    assert_eq!(view.messages.last().unwrap().text, "后台内容");
    assert!(!app.has_pending_stream());
    assert_eq!(app.active_agent_session_view().unwrap().input, "另一个成员");
    assert!(app.input.is_empty());
}

/// 订阅同步只为导航栈中的实例建立状态，关闭视图后立即释放。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn agent_session_subscriptions_follow_the_navigation_stack() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let target = open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    let api: Arc<dyn AgentRuntimeApi> = Arc::new(StubAgentApi {
        target: target.clone(),
    });
    app.sync_agent_views_with(|_| Some(Arc::clone(&api))).await;

    let view = app
        .active_agent_session_view()
        .expect("同步后应建立视图状态");
    assert_eq!(view.status, Some(AgentStatus::Running));
    assert!(view.subscribed_for(&target));

    // 事件经后台转发到达后进入同一条时间线。
    app.apply_agent_view_events(
        "teammate",
        "member-1",
        &target,
        vec![AgentEvent::new(
            "run-1",
            AgentEventKind::ModelTextDelta,
            0,
            json!({"index": 0, "delta": "继续"}),
        )],
    )
    .await;
    assert!(app.has_pending_stream());
    app.active_agent_session_view_mut()
        .unwrap()
        .flush_pending_stream();
    assert!(app
        .active_agent_session_view()
        .expect("视图状态应存在")
        .messages
        .iter()
        .any(|message| message.text.contains("继续")));

    // 事件流结束后不再逐帧重绘：终态成员不能继续显示旋转指示与耗时。
    if let Some(view) = app.active_agent_session_view_mut() {
        view.mark_closed(Some(AgentStatus::Running), None);
    }
    assert!(!app.tick_requires_redraw(), "结束的流不应继续驱动帧时钟");

    // 返回主界面后同步必须丢弃旧实例与它的订阅，避免后台任务继续占用通道。
    app.handle_agent_session_key(KeyCode::Esc, KeyModifiers::NONE);
    app.sync_agent_views_with(|_| Some(Arc::clone(&api))).await;
    assert!(app.agent_views.is_empty());
}

/// 原生会话视图不向插件请求任何帧：成员会话由 TUI 自己渲染。
#[cfg(feature = "plugins")]
#[test]
fn agent_session_view_skips_plugin_frames() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.add_plugin_views(vec![UiDeclaration {
        plugin_id: "teammate".into(),
        view_id: "teammate-team-dock".into(),
        title: "Team".into(),
        placement: UiPlacement::Right,
        size: Default::default(),
        focusable: true,
        input_triggers: Vec::new(),
    }]);
    assert_eq!(
        app.periodic_plugin_render_requests().len(),
        1,
        "主视图可见时停靠视图仍按周期刷新"
    );

    open_member_view(&mut app, "member-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    assert!(
        app.periodic_plugin_render_requests().is_empty(),
        "原生会话视图不应让宿主调用插件渲染"
    );
}
