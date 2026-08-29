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

/// 最近一次推理摘要在运行结束后仍按 `thinking(content)` 留在正文时间线，并保持弱化样式。
#[test]
fn render_shows_reasoning_inside_message_timeline() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建推理摘要测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.messages.push(Msg::new(MsgKind::User, "检查任务"));
    app.start_model_response();
    app.model_thinking = "正在核对权限边界".into();
    app.running = false;

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染推理摘要");
    let buffer = terminal.backend().buffer();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let text = rows.join("");
    let message_row = rows
        .iter()
        .position(|row| row.contains("检查任务"))
        .expect("正文应可见");
    let thinking_row = rows
        .iter()
        .position(|row| row.contains("正在核对权限边界"))
        .expect("正文时间线内的思考应可见");
    let thinking_x = (0..buffer.area.width)
        .find(|x| buffer[(*x, thinking_row as u16)].symbol() == "正")
        .expect("思考内容应存在灰色单元格");

    assert!(text.contains("thinking(正在核对权限边界)"), "{text:?}");
    assert!(!text.contains("思考正在核对权限边界"), "{text:?}");
    assert!(message_row < thinking_row, "{rows:?}");
    assert_eq!(
        buffer[(thinking_x, thinking_row as u16)].fg,
        COLOR_MUTED,
        "思考内容必须使用低对比灰色"
    );
    assert!(
        buffer[(thinking_x, thinking_row as u16)]
            .modifier
            .contains(Modifier::ITALIC),
        "思考内容必须使用斜体区别于正文"
    );
    assert_eq!(
        app.messages
            .iter()
            .filter(|message| matches!(message.kind, MsgKind::Assistant))
            .count(),
        0
    );
}

/// 思考必须进入正文滚动范围，向上翻阅历史时不能继续悬浮在任务面板上方。
#[cfg(feature = "plugins")]
#[test]
fn reasoning_scrolls_with_message_timeline() {
    let backend = TestBackend::new(80, 28);
    let mut terminal = Terminal::new(backend).expect("创建思考布局测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.messages
        .extend((0..20).map(|index| Msg::new(MsgKind::User, format!("正文定位 {index}"))));
    app.start_model_response();
    app.model_thinking = "随正文滚动的思考".into();
    app.scroll = Some(0);
    let mut tasks = test_plugin_view(UiPlacement::ComposerShelf, "任务");
    tasks.frame.as_mut().expect("任务帧应存在").lines[0].spans[0].text = "任务面板定位".into();
    app.plugin_views.push(tasks);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染正文内思考与任务面板");
    let buffer = terminal.backend().buffer();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let body_row = rows
        .iter()
        .position(|row| row.contains("正文定位0"))
        .expect("上翻后的正文应可见");
    let task_row = rows
        .iter()
        .position(|row| row.contains("任务面板定位"))
        .expect("任务面板应可见");

    assert!(body_row < task_row, "{rows:?}");
    assert!(
        !rows.iter().any(|row| row.contains("随正文滚动的思考")),
        "上翻正文后，末尾思考不应悬浮显示：{rows:?}"
    );
}

/// 长思考必须作为完整正文内容参与滚动，不能只保留独立视口中的末尾四行。
#[test]
fn long_reasoning_uses_message_timeline_scrolling() {
    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).expect("创建长思考测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.messages.push(Msg::new(MsgKind::User, "正文保留"));
    app.start_model_response();
    app.model_thinking = (0..20)
        .map(|index| format!("思考阶段{index}"))
        .collect::<Vec<_>>()
        .join("\n");

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染长思考");
    let latest_rows = (0..terminal.backend().buffer().area.height)
        .map(|y| {
            (0..terminal.backend().buffer().area.width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert!(app.last_max_scroll > 0);
    assert!(
        latest_rows.iter().any(|row| row.contains("思考阶段19")),
        "{latest_rows:?}"
    );

    app.scroll = Some(0);
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("向上滚动长思考");
    let earliest_rows = (0..terminal.backend().buffer().area.height)
        .map(|y| {
            (0..terminal.backend().buffer().area.width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert!(
        earliest_rows.iter().any(|row| row.contains("正文保留")),
        "{earliest_rows:?}"
    );
    assert!(
        earliest_rows.iter().any(|row| row.contains("思考阶段0")),
        "{earliest_rows:?}"
    );
}

/// 验证工具行展示参数与返回内容摘要，且过长内容按显示宽度截断。
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

    let lines = msg.to_lines(false, 80);
    let text: String = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("● read_file(path: src/main.rs)"), "{text:?}");
    assert!(text.contains("└ 很长的文件内容"), "{text:?}");
    assert!(text.contains('…'), "{text:?}");
}

/// 运行中的工具应立即展示增量输出，而不必等待 ToolFinished。
#[test]
fn running_tool_lines_show_latest_output() {
    let mut msg = Msg::tool_started(ToolCall::new(
        "call-live",
        "shell",
        json!({"command": "cargo test"}),
    ));
    for line in 1..=8 {
        msg.append_tool_output(&format!("编译进度 {line}\n"));
    }

    let text = msg
        .to_lines(false, 100)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(text.contains("… 共 8 行"), "{text:?}");
    assert!(!text.contains("编译进度 1"), "{text:?}");
    assert!(text.contains("编译进度 8"), "{text:?}");
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

    assert!(text.contains("● shell"), "{text:?}");
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
