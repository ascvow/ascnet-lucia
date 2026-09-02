//! 主界面渲染与 Markdown 呈现的回归测试。

use super::*;

/// 验证常规尺寸下角色标记、输入提示和底部信息行均可见。
#[test]
fn render_shows_visual_hierarchy() {
    let text = render_text(100, 24, false);

    assert!(text.contains("测试模型"), "{text:?}");
    assert!(text.contains("▌测试消息"), "{text:?}");
    assert!(text.contains("●测试回复"), "{text:?}");
    assert!(text.contains("MessageLucia..."), "{text:?}");
    assert!(!text.contains("agentruntime"), "{text:?}");
    assert!(!text.contains("ReAct"), "{text:?}");
}

/// 底栏只显示稳定英文思考强度，不重复展示“思考”标签。
#[test]
fn footer_shows_compact_english_reasoning_level() {
    let backend = TestBackend::new(100, 16);
    let mut terminal = Terminal::new(backend).expect("创建底栏思考强度测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into())
        .with_reasoning_levels(vec![ReasoningLevel::XHigh], ReasoningLevel::XHigh);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染英文思考强度");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("xhigh"), "{text:?}");
    assert!(!text.contains("思考 xhigh"), "{text:?}");
    assert!(!text.contains("思考 超高"), "{text:?}");
}

/// 左键释放形成可复制选区后，底栏必须展示一次性操作提示。
#[test]
fn footer_shows_copy_shortcut_for_released_selection() {
    let backend = TestBackend::new(100, 16);
    let mut terminal = Terminal::new(backend).expect("创建选区操作提示测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.text_selection_content = Some("待复制文本".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染选区操作提示");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("Selection · Ctrl+C copy"), "{text:?}");
}

/// 空会话首屏应在常见终端尺寸内绘制压缩后的彩色像素头像。
#[test]
fn empty_session_renders_pixel_portrait() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建像素头像测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染像素头像测试界面");
    let portrait_cells = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| matches!(cell.symbol(), "▀" | "▄" | "█"))
        .count();

    assert!(portrait_cells >= 80, "像素头像应包含足够的可见终端像素");
}

/// 输入提示应紧跟输入盒上边框，下边框之后立即进入状态栏。
#[test]
fn input_editor_starts_immediately_after_rule() {
    let width = 80;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建输入间距测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染输入间距测试界面");
    let rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let rule_row = rows
        .iter()
        .position(|row| row.chars().filter(|character| *character == '─').count() > 20)
        .expect("输入盒上边框应存在");

    assert!(rows[rule_row + 1].contains("Message Lucia..."));
    // 编辑行之后是输入盒下边框，再往下一行是状态栏。
    assert!(
        rows[rule_row + 2]
            .chars()
            .filter(|character| *character == '─')
            .count()
            > 20
    );
    assert!(!rows[rule_row + 3].trim().is_empty());
}

/// 多行输入框最多展示六个逻辑行，并自动滚动到光标所在的末行。
#[test]
fn multiline_input_renders_six_visible_rows() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建多行输入测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = (1..=8)
        .map(|index| format!("input-line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.cursor = app.input.len();

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染多行输入界面");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(!text.contains("input-line-2"), "{text:?}");
    for index in 3..=8 {
        assert!(text.contains(&format!("input-line-{index}")), "{text:?}");
    }
}

/// 验证运行状态使用 steering 文案，并在窄终端隐藏目录信息。
#[test]
fn render_adapts_to_running_state_and_narrow_width() {
    let text = render_text(60, 16, true);

    assert!(text.contains("Working..."), "{text:?}");
    assert!(text.contains("Steerthecurrentrun..."), "{text:?}");
    assert!(!text.contains("ascnet-lucia"), "{text:?}");
}

/// 思考阶段只显示状态文案，不渲染服务商返回的推理摘要文本。
#[test]
fn render_hides_reasoning_text_and_shows_thinking_status() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建思考阶段测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.messages.push(Msg::new(MsgKind::User, "检查任务"));
    app.start_model_response();
    app.mark_model_thinking();

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染思考阶段");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("Thinking..."), "{text:?}");
    assert!(!text.contains("thinking("), "{text:?}");
    assert!(!text.contains("Working..."), "{text:?}");
}

/// 验证工具结果默认收起，展开后展示目标内容且过长行按显示宽度截断。
#[test]
fn tool_lines_show_args_and_truncated_result() {
    let mut msg = Msg::tool_started(ToolCall::new(
        "call-1",
        "read_file",
        json!({ "path": "src/main.rs" }),
    ));
    msg.kind = MsgKind::ToolOk;
    msg.tool_result = Some(ToolResult::success(
        "call-1",
        "read_file",
        json!({ "content": "很长的文件内容".repeat(30) }),
    ));

    let collapsed = msg
        .to_lines(false, 80)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // 有结果就默认展示详情；内容未超过预览上限时不出现展开提示。
    assert!(collapsed.contains("● Read(src/main.rs)"), "{collapsed:?}");
    assert!(!collapsed.contains("Click to expand"), "{collapsed:?}");
    assert!(
        collapsed.contains("└ 读取 1 行，来自 src/main.rs"),
        "{collapsed:?}"
    );
    assert!(collapsed.contains("1 很长的文件内容"), "{collapsed:?}");

    // 内容没有被藏起来，因此不可展开：既没有操作提示，点击也不该改变任何东西。
    assert!(!msg.is_tool_expandable());
}

/// 运行中的工具应立即展示增量输出，而不必等待 ToolFinished。
#[test]
fn running_tool_lines_show_latest_output() {
    let mut msg = Msg::tool_started(ToolCall::new(
        "call-live",
        "shell",
        json!({"command": "cargo test"}),
    ));
    for line in 1..=48 {
        msg.append_tool_output(&format!("编译进度 {line}\n"));
    }

    let collapsed = msg
        .to_lines(false, 100)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    // 默认展示最新若干行；超出上限的部分折叠并提示可展开。
    assert!(collapsed.contains("● Shell"), "{collapsed:?}");
    assert!(!collapsed.contains('▸'), "{collapsed:?}");
    assert!(collapsed.contains("编译进度 48"), "{collapsed:?}");
    assert!(collapsed.contains("… +40 行"), "{collapsed:?}");
    assert!(collapsed.contains("Click to expand"), "{collapsed:?}");
    assert!(!collapsed.contains("编译进度 20"), "{collapsed:?}");

    msg.tool_expanded = true;
    let text = msg
        .to_lines(false, 100)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();

    // 展开详情只保留最新 40 行，被折叠的行数以 `… +N 行` 标注。
    assert!(!text.contains('▾'), "{text:?}");
    assert!(text.contains("… +8 行"), "{text:?}");
    assert!(text.contains("Click to collapse"), "{text:?}");
    assert!(!text.contains("编译进度 1\n"), "{text:?}");
    assert!(text.contains("编译进度 48"), "{text:?}");
}

/// 空工具结果只显示完成状态，不应因缺少预览行而越界崩溃。
#[test]
fn tool_lines_accept_empty_result() {
    let msg = Msg::tool_finished(ToolResult::success("call-empty", "shell", json!(null)));

    let lines = msg.to_lines(false, 80);
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(text.contains("● Shell"), "{text:?}");
    // 空结果没有可展开的正文，不应凭空造出预览行。
    assert!(!text.contains("null"), "{text:?}");
}

/// 连续工具消息之间保留一行间隔，工具组结束后与普通回复同样保留一行间隔。
#[test]
fn consecutive_tool_messages_keep_blank_line_spacing() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建工具组间距测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.extend([
        Msg::tool_started(ToolCall::new("call-1", "first_tool", json!({}))),
        Msg::tool_started(ToolCall::new("call-2", "second_tool", json!({}))),
        Msg::new(MsgKind::Assistant, "follow-up reply"),
    ]);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染带间距的工具组");
    let rows = (0..18)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let tool_rows = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            (row.contains("first_tool") || row.contains("second_tool")).then_some(index)
        })
        .collect::<Vec<_>>();
    let reply_row = rows
        .iter()
        .position(|row| row.contains("follow-up reply"))
        .expect("应展示工具组后的回复");

    assert_eq!(tool_rows.len(), 2, "{rows:?}");
    assert_eq!(tool_rows[1], tool_rows[0] + 2, "{rows:?}");
    assert_eq!(reply_row, tool_rows[1] + 2, "{rows:?}");
}

/// 左键按下与释放命中同一工具消息时切换详情，第二次点击可以收起。
#[test]
fn clicking_tool_message_toggles_expanded_details() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建工具点击测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut message = Msg::tool_started(ToolCall::new(
        "call-click",
        "read_file",
        json!({"path": "src/main.rs"}),
    ));
    message.kind = MsgKind::ToolOk;
    let detail_content = (1..=20)
        .map(|index| {
            if index == 20 {
                "details-visible".to_string()
            } else {
                format!("detail-{index:02}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    message.tool_result = Some(ToolResult::success(
        "call-click",
        "read_file",
        json!({"content": detail_content}),
    ));
    app.messages.push(message);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染收起工具消息");
    let area = app.tool_message_hit_regions[0].area;
    let click = |kind| MouseEvent {
        kind,
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    };

    let down = click(MouseEventKind::Down(crossterm::event::MouseButton::Left));
    app.handle_text_selection(&down, terminal.backend().buffer());
    assert!(!app.handle_tool_message_click(&down));
    let up = click(MouseEventKind::Up(crossterm::event::MouseButton::Left));
    app.handle_text_selection(&up, terminal.backend().buffer());
    assert!(app.handle_tool_message_click(&up));
    assert!(app.messages[0].tool_expanded);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染展开工具消息");
    let expanded_text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        expanded_text.contains("details-visible"),
        "{expanded_text:?}"
    );
    assert!(
        expanded_text.contains("Click to collapse"),
        "{expanded_text:?}"
    );

    let area = app.tool_message_hit_regions[0].area;
    assert!(area.height < 20, "长详情热区应按当前视口裁剪：{area:?}");
    for kind in [
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        let event = MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_text_selection(&event, terminal.backend().buffer());
        app.handle_tool_message_click(&event);
    }
    assert!(!app.messages[0].tool_expanded);
}

/// 工具消息上的左键拖选只能形成文本选区，不得同时展开详情。
#[test]
fn dragging_tool_message_does_not_toggle_details() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建工具拖选测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut message = Msg::tool_started(ToolCall::new(
        "call-drag",
        "shell",
        json!({"command": "cargo test"}),
    ));
    for line in 1..=24 {
        message.append_tool_output(&format!("正在执行测试 {line}\n"));
    }
    app.messages.push(message);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染工具拖选场景");
    let area = app.tool_message_hit_regions[0].area;
    for (kind, column) in [
        (
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            area.x,
        ),
        (
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            area.x + 2,
        ),
        (
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            area.x + 2,
        ),
    ] {
        let event = MouseEvent {
            kind,
            column,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_text_selection(&event, terminal.backend().buffer());
        assert!(!app.handle_tool_message_click(&event));
    }

    assert!(!app.messages[0].tool_expanded);
    assert!(app.text_selection_content.is_some());
}

/// 独占审批快照可见时，即使上一帧残留工具热区也不得修改实时消息状态。
#[cfg(feature = "plugins")]
#[test]
fn approval_snapshot_disables_tool_message_clicks() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut message = Msg::tool_started(ToolCall::new(
        "call-approval",
        "shell",
        json!({"command": "cargo test"}),
    ));
    message.append_tool_output("等待审批\n");
    app.messages.push(message);
    app.tool_message_hit_regions
        .push(crate::app::ToolMessageHitRegion {
            message_index: 0,
            area: Rect::new(1, 1, 20, 1),
        });
    app.main_event_snapshot = Some(crate::app::MainEventSnapshot {
        messages: app.messages.clone(),
        streaming_message: None,
        model_is_thinking: false,
    });

    for kind in [
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        let consumed = app.handle_tool_message_click(&MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!consumed);
    }

    assert!(!app.messages[0].tool_expanded);
    assert!(app.tool_click_candidate.is_none());
}

/// 验证嵌套 JSON 摘要折叠为计数而不是原始序列化。
#[test]
fn summarize_json_folds_nested_structures() {
    let value = json!({
        "path": "src",
        "entries": [{ "name": "lib.rs" }, { "name": "main.rs" }],
        "meta": { "hidden": false }
    });

    let summary = summarize_json(&value, 96);

    assert_eq!(summary, "path: src, entries: [2 项], meta: {…}");
}

/// 验证 Markdown 表格排版为对齐行：分隔行转为横线，中文列按显示宽度补齐。
#[test]
fn markdown_tables_render_aligned() {
    let lines = markdown_lines(
        "说明\n\n| 模块 | 作用 |\n|---|---|\n| core | Agent 循环 |\n| tool | 工具注册 |\n\n结尾",
        false,
    );
    let rows: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let text = rows.join("\n");

    assert!(text.contains("┼"), "{text:?}");
    assert!(!text.contains("|---"), "{text:?}");
    // 两列数据行应等宽对齐：core 与 tool 后的竖线位置一致。
    let core_row = rows
        .iter()
        .find(|row| row.contains("core"))
        .expect("core 行");
    let tool_row = rows
        .iter()
        .find(|row| row.contains("tool"))
        .expect("tool 行");
    assert_eq!(core_row.find('│'), tool_row.find('│'), "{text:?}");
    assert!(text.contains("结尾"), "{text:?}");
}

/// 验证 Markdown 渲染保留标题与行内代码的强调样式，且不产生任何背景色。
#[test]
fn markdown_renders_emphasis_without_background() {
    let lines = markdown_lines("# 标题\n\n**重点** 与 `代码` 内容", false);

    let no_background = lines.iter().all(|line| {
        line.style.bg.is_none() && line.spans.iter().all(|span| span.style.bg.is_none())
    });
    assert!(no_background);

    let heading = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.contains("标题")))
        .expect("应包含标题行");
    assert!(heading.style.add_modifier.contains(Modifier::BOLD));

    let bold_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("重点"))
        .expect("应包含加粗片段");
    assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
}

/// 验证围栏代码块隐藏围栏、保留语言标签，并且不干扰相邻表格。
#[test]
fn markdown_code_blocks_are_visually_separated() {
    let lines = markdown_lines(
        "```rust\nfn main() {}\n```\n| 列一 | 列二 |\n|---|---|\n| 值一 | 值二 |\n```txt\n未闭合",
        false,
    );
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<String>();

    assert!(!text.contains("```"), "{text:?}");
    assert!(text.contains("▎ fn main() {}  rust"), "{text:?}");
    assert!(text.contains('┼'), "{text:?}");
    assert!(text.contains("▎ 未闭合  txt"), "{text:?}");
}

/// 拖选高亮只修改单元格样式，不得改写可复制的字符内容。
#[test]
fn mouse_text_selection_renders_reversed_cells() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut selection = MouseTextSelection::new(Position::new(1, 0));
    selection.update(Position::new(2, 0));
    app.text_selection = Some(selection);
    let mut buffer = Buffer::with_lines(["abcd"]);

    app.render_text_selection(&mut buffer);

    assert_eq!(buffer[(1, 0)].symbol(), "b");
    assert!(buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
    assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
}

/// 上下文状态在配置窗口后展示占比，并按超过 80% 与 95% 的阈值着色。
#[test]
fn context_status_uses_configured_window_thresholds() {
    assert_eq!(
        crate::ui::chat::context_status(86_900, Some(200_000)),
        ("43% · 86.9k".into(), COLOR_MUTED)
    );
    assert_eq!(
        crate::ui::chat::context_status(160_000, Some(200_000)).1,
        COLOR_MUTED
    );
    assert_eq!(
        crate::ui::chat::context_status(160_001, Some(200_000)).1,
        COLOR_WARNING
    );
    assert_eq!(
        crate::ui::chat::context_status(190_001, Some(200_000)).1,
        COLOR_DANGER
    );
    assert_eq!(
        crate::ui::chat::context_status(5_300, None),
        ("5.3k".into(), COLOR_MUTED)
    );
}

/// 内容未被折叠的工具消息不注册点击热区，点击也不得切换展开状态。
///
/// 这类消息展开后看到的内容完全一样，允许点击只会制造"点了没反应"的困惑。
#[test]
fn short_tool_message_is_neither_marked_nor_clickable() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建短工具消息测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut message = Msg::tool_started(ToolCall::new(
        "call-short",
        "read_file",
        json!({"path": "a.rs"}),
    ));
    message.kind = MsgKind::ToolOk;
    message.tool_result = Some(ToolResult::success(
        "call-short",
        "read_file",
        json!({"content": "fn main() {}", "lines_read": 1, "total_lines": 1}),
    ));
    app.messages.push(message);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染短工具消息");
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(!app.messages[0].is_tool_expandable());
    assert!(!screen.contains("Click to expand"), "{screen}");
    assert!(
        app.tool_message_hit_regions.is_empty(),
        "短结果不应注册热区"
    );

    // 即便直接构造一次落在消息上的点击，也不得改变展开状态。
    let click = |kind| MouseEvent {
        kind,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_tool_message_click(&click(MouseEventKind::Down(
        crossterm::event::MouseButton::Left,
    )));
    assert!(!app.handle_tool_message_click(&click(MouseEventKind::Up(
        crossterm::event::MouseButton::Left
    ))));
    assert!(!app.messages[0].tool_expanded);
}
