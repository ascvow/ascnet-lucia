//! 输入编辑、附件与按键处理的回归测试。

use super::*;

/// 验证流式增量与运行结束不会打断用户的手动滚动位置。
#[test]
fn manual_scroll_survives_streaming_updates() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.last_max_scroll = 40;
    app.scroll_up(10);
    assert_eq!(app.scroll, Some(30));

    app.append_model_delta("新的输出");
    let saved_record = app.session_record.clone();
    app.handle_agent_done(AgentCompletion {
        run: Some(AgentRun {
            run_id: "run-scroll".into(),
            final_text: "完成".into(),
            steps_used: 1,
            usage: Default::default(),
            session: Session::new(),
            cancelled: false,
        }),
        session_record: saved_record,
        error: None,
        input_committed: true,
        queue_may_advance: true,
        input: "测试".into(),
    });

    assert_eq!(app.scroll, Some(30));
}

/// 触发未激活的输入面板只接受定向刷新，不参与周期性跨 WASM 轮询。
#[test]
#[cfg(feature = "plugins")]
fn inactive_input_panel_is_excluded_from_periodic_refresh() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::InputPanel, "筛选");
    view.declaration.input_triggers = vec![":".into()];
    app.plugin_views.push(view);

    assert!(app.periodic_plugin_render_requests().is_empty());
    assert_eq!(app.plugin_render_requests().len(), 1);

    app.input = ":res".into();
    app.cursor = app.input.len();
    assert_eq!(app.periodic_plugin_render_requests().len(), 1);
}

/// 验证插件替换主输入时光标被限制在合法的 UTF-8 边界内。
#[test]
#[cfg(feature = "plugins")]
fn set_main_input_clamps_cursor_to_char_boundary() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.set_main_input("/deploy 上海".into(), None);
    assert_eq!(app.input, "/deploy 上海");
    assert_eq!(app.cursor, app.input.len());

    // 光标落在多字节字符中间时回退到最近的合法边界。
    app.set_main_input("/deploy 上海".into(), Some(9));
    assert_eq!(app.cursor, 8);

    // 越界光标收敛到文本末尾。
    app.set_main_input("/x".into(), Some(99));
    assert_eq!(app.cursor, 2);
}

/// 拖入图片生成 [IMAGE#N] 引用标签，提交时转换为文本加图片内容块。
#[test]
fn attached_image_becomes_ref_clip_and_blocks() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("图.png", b"\x89PNG\r\n\x1a\n");

    app.input = "看这张 ".to_string();
    app.cursor = app.input.len();
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    assert_eq!(app.input, "看这张 [IMAGE#1] ");
    assert_eq!(app.attachments.len(), 1);
    assert!(app.attachments[0].is_image);
    assert_eq!(app.attachments[0].media_type, "image/png");

    let submission = app.take_submission().expect("应有提交内容");
    let blocks = submission.blocks();
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "看这张"));
    assert!(
        matches!(&blocks[1], ContentBlock::Image { media_type, .. } if media_type == "image/png")
    );
    assert!(app.attachments.is_empty());
    assert!(app.input.is_empty());
}

/// 普通文件使用 [FILE#N] 标签，多个文件按加入顺序编号。
#[test]
fn attached_files_use_name_labels_with_dedup() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("note.txt", "内容".as_bytes());

    app.attach_file(&path);
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    let labels: Vec<&str> = app
        .attachments
        .iter()
        .map(|attachment| attachment.label.as_str())
        .collect();
    assert_eq!(labels, vec!["[FILE#1]", "[FILE#2]"]);
    assert!(app.input.contains(labels[0]));
    assert!(app.input.contains(labels[1]));
}

/// PDF 使用独立编号标签，避免与普通文件混淆。
#[test]
fn attached_pdf_uses_pdf_number_label() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("报告.pdf", b"%PDF-1.7");

    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    assert_eq!(app.input, "[PDF#1] ");
    assert_eq!(app.attachments[0].media_type, "application/pdf");
}

/// models.dev 未声明图片输入时，TUI 必须在读取后拒绝加入图片附件。
#[test]
fn image_attachment_is_rejected_when_model_lacks_image_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into())
        .with_model_modalities(vec![crate::config::model_metadata::ModelModality::Text]);
    let path = write_temp_attachment("图.png", b"png");

    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    assert!(app.attachments.is_empty());
    assert!(app.input.is_empty());
    assert!(app
        .messages
        .last()
        .is_some_and(|message| message.text.contains("不支持图片输入")));
}

/// models.dev 未声明 PDF 输入时，原生 PDF 内容块不得进入待提交队列。
#[test]
fn pdf_attachment_is_rejected_when_model_lacks_pdf_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into())
        .with_model_modalities(vec![crate::config::model_metadata::ModelModality::Text]);
    let path = write_temp_attachment("报告.pdf", b"%PDF-1.7");

    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    assert!(app.attachments.is_empty());
    assert!(app
        .messages
        .last()
        .is_some_and(|message| message.text.contains("不支持PDF输入")));
}

/// models.dev 声明图片输入时，现有图片内容块路径保持可用。
#[test]
fn image_attachment_is_allowed_when_model_supports_image_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_model_modalities(vec![
        crate::config::model_metadata::ModelModality::Text,
        crate::config::model_metadata::ModelModality::Image,
    ]);
    let path = write_temp_attachment("图.png", b"png");

    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    let blocks = app.take_submission().expect("应有图片提交").blocks();
    assert!(matches!(blocks.as_slice(), [ContentBlock::Image { .. }]));
}

/// 音频即使由模型声明支持，也必须因 Lucia 内容块协议未实现而拒绝。
#[test]
fn audio_attachment_is_rejected_until_core_protocol_supports_it() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into())
        .with_model_modalities(vec![crate::config::model_metadata::ModelModality::Audio]);
    let path = write_temp_attachment("录音.mp3", b"audio");

    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    assert!(app.attachments.is_empty());
    assert!(app
        .messages
        .last()
        .is_some_and(|message| message.text.contains("协议尚未实现")));
}

/// 普通粘贴文本在输入框折叠显示，提交内容块仍包含原始文本而非占位标签。
#[test]
fn pasted_text_uses_clip_label_and_preserves_original_content() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste("第一行\r\n第二行");

    assert_eq!(app.input, "[粘贴的文本] ");
    let submission = app.take_submission().expect("应有粘贴文本提交");
    let blocks = submission.blocks();
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "第一行\n第二行"));
}

/// 删除粘贴标签必须同步删除绑定原文，提交时不得残留隐藏正文。
#[test]
fn deleting_pasted_text_label_removes_bound_content() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.handle_paste("不应发送的正文");
    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE, None);

    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE, None);

    assert!(app.input.is_empty());
    assert!(app.attachments.is_empty());
    assert!(app.take_submission().is_none());
}

/// Backspace 紧跟引用标签时整体删除标签与对应附件。
#[test]
fn backspace_removes_whole_ref_clip() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("pic.png", b"png");
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();
    // 删除标签后自动附加的空格，使光标紧跟标签结尾
    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE, None);

    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE, None);

    assert!(app.input.is_empty());
    assert!(app.attachments.is_empty());
}

/// Esc 在主界面不得直接退出，且非空输入必须连续双按才会清空。
#[test]
fn escape_double_press_clears_input_without_quitting() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "保留中的草稿".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);
    assert_eq!(app.input, "保留中的草稿");
    assert!(!app.should_quit);

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);
    assert!(app.input.is_empty());
    assert_eq!(app.cursor, 0);
    assert!(!app.should_quit);

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);
    assert!(!app.should_quit);
}

/// 两次 Esc 之间出现其他按键时必须重新计数，避免误清空新编辑的内容。
#[test]
fn escape_double_press_is_reset_by_input_edit() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "草稿".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Char('新'), KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);

    assert_eq!(app.input, "草稿新");
    assert!(!app.should_quit);
}

/// 超过连续双按时间窗的第二次 Esc 只能重新计数，不能清空草稿。
#[test]
fn escape_double_press_expires_before_clearing_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "超时后保留".into();
    app.cursor = app.input.len();
    app.last_escape_at = Some(
        std::time::Instant::now()
            .checked_sub(ESC_DOUBLE_PRESS_WINDOW + std::time::Duration::from_millis(1))
            .expect("测试时间点应可回退"),
    );

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);

    assert_eq!(app.input, "超时后保留");
    assert!(app.last_escape_at.is_some());
    assert!(!app.should_quit);
}

/// Esc 请求取消后，已经排队但尚未消费的正文和思考增量不得继续改变界面。
#[test]
fn escape_immediately_stops_visible_stream_deltas() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.running = true;
    app.append_model_delta("取消前");
    app.append_model_thinking("思考前");

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));
    app.append_model_delta("取消后");
    app.append_model_thinking("思考后");

    assert_eq!(app.messages[0].text, "取消前");
    assert_eq!(app.model_thinking, "思考前");
}

/// 独占审批视图可见时必须冻结整个主事件区，隐藏后一次性显示期间积累的事件。
#[cfg(feature = "plugins")]
#[test]
fn visible_permission_input_freezes_main_event_timeline() {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("创建审批冻结测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.append_model_delta("审批前");
    app.append_model_thinking("思考前");
    let mut approval = test_plugin_view(UiPlacement::Input, "Permission");
    approval.frame.as_mut().expect("审批帧应存在").visible = false;
    app.plugin_views.push(approval);
    apply_plugin_frames(
        &mut app,
        vec![(
            "test-plugin".into(),
            "input".into(),
            None,
            Ok(Some(PluginUiFrame {
                view_id: "input".into(),
                visible: true,
                lines: vec![UiLine {
                    spans: vec![UiSpan {
                        text: "等待审批".into(),
                        style: UiStyle::default(),
                    }],
                }],
                cursor: None,
            })),
        )],
    );

    app.append_model_delta("审批中正文");
    app.append_model_thinking("审批中思考");
    app.messages.push(Msg::tool_started(ToolCall::new(
        "call-during-approval",
        "审批期间工具",
        json!({}),
    )));
    app.sync_main_event_snapshot();
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染审批期间主事件区");
    let frozen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '▌')
        .collect::<String>();
    assert!(frozen.contains("审批前"), "{frozen:?}");
    assert!(!frozen.contains("审批中正文"), "{frozen:?}");
    assert!(!frozen.contains("审批中思考"), "{frozen:?}");
    assert!(!frozen.contains("审批期间工具"), "{frozen:?}");
    assert_eq!(app.messages.len(), 2, "后台事件仍应更新真实状态");

    apply_plugin_frames(
        &mut app,
        vec![(
            "test-plugin".into(),
            "input".into(),
            None,
            Ok(Some(PluginUiFrame {
                view_id: "input".into(),
                visible: false,
                lines: Vec::new(),
                cursor: None,
            })),
        )],
    );
    app.streaming_message = None;
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染审批结束后的主事件区");
    let resumed = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '▌')
        .collect::<String>();
    assert!(resumed.contains("审批中正文"), "{resumed:?}");
    assert!(resumed.contains("审批中思考"), "{resumed:?}");
    assert!(resumed.contains("审批期间工具"), "{resumed:?}");
}

/// 审批视图隐藏后必须归还主输入焦点，避免下一次 Esc 被旧插件焦点吞掉。
#[cfg(feature = "plugins")]
#[test]
fn hidden_permission_input_returns_escape_to_main() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Input, "Permission"));
    app.plugin_focus = Some(0);
    apply_plugin_frames(
        &mut app,
        vec![(
            "test-plugin".into(),
            "input".into(),
            None,
            Ok(Some(PluginUiFrame {
                view_id: "input".into(),
                visible: false,
                lines: Vec::new(),
                cursor: None,
            })),
        )],
    );

    assert!(app.plugin_focus.is_none());
    assert!(matches!(
        app.route_plugin_key(KeyCode::Esc, KeyModifiers::NONE),
        PluginKeyRoute::Main
    ));
}

/// 粘贴的引号包裹或转义路径解析为文件；普通文本返回 None。
#[test]
fn pasted_file_path_parses_dropped_paths() {
    let path = write_temp_attachment("带 空格.txt", b"x");
    let display = path.display().to_string();

    let quoted = format!("'{display}'");
    assert_eq!(pasted_file_path(&quoted), Some(path.clone()));

    let escaped = display.replace(' ', "\\ ");
    assert_eq!(pasted_file_path(&escaped), Some(path.clone()));

    std::fs::remove_file(&path).ok();
    assert_eq!(pasted_file_path("普通粘贴文本"), None);
    assert_eq!(pasted_file_path(&display), None);
}

/// 粘贴非路径文本时折叠为标签，提交时保留统一后的逻辑换行。
#[test]
fn paste_preserves_multiline_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste("第一行\r\n第二行\t内容");

    assert_eq!(app.input, "[粘贴的文本] ");
    assert_eq!(app.cursor, app.input.len());
    let blocks = app.take_submission().expect("应保留粘贴正文").blocks();
    assert!(matches!(
        blocks.as_slice(),
        [ContentBlock::Text { text }] if text == "第一行\n第二行 内容"
    ));
}

/// 空输入框用上方向键回填历史，方向键可继续导航，编辑后退出回溯态。
#[test]
fn input_history_recalls_recent_submissions() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    for input in ["第一条", "第一条", "第二条"] {
        app.input = input.into();
        app.cursor = app.input.len();
        assert!(app.take_input().is_some());
    }

    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.input, "第二条");
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.input, "第一条");
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.input, "第二条");
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert!(app.input.is_empty());

    app.handle_key(KeyCode::Char('p'), KeyModifiers::CONTROL, None);
    app.handle_key(KeyCode::Char('改'), KeyModifiers::NONE, None);
    assert_eq!(app.input, "第二条改");
    assert_eq!(app.input_history_cursor, None);
}

/// 空输入时上方向键回填最近提交，下方向键越过最新记录后恢复空输入。
#[test]
fn arrow_keys_recall_input_history() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "已提交".into();
    app.cursor = app.input.len();
    assert!(app.take_input().is_some());
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.input, "已提交");
    assert_eq!(app.input_history_cursor, Some(0));
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert!(app.input.is_empty());
    assert_eq!(app.input_history_cursor, None);
}

/// TUI 默认捕获鼠标，由应用同时处理滚轮和文本拖选。
#[test]
fn app_defaults_to_mouse_wheel_and_text_selection() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let app = App::new(tx, "测试模型".into());

    assert!(app.mouse_capture);
}

/// Ctrl+R 只能在 models.dev 声明的等级中循环，并同步更新共享 Agent。
#[test]
fn reasoning_shortcut_cycles_model_supported_levels() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
        vec![
            ReasoningLevel::Off,
            ReasoningLevel::Low,
            ReasoningLevel::High,
        ],
        ReasoningLevel::Off,
    );
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, Some(&agent));

    assert_eq!(app.reasoning_level, ReasoningLevel::Low);
    assert_eq!(agent.reasoning_level(), ReasoningLevel::Low);
}

/// 配置持久化失败时，TUI 与共享 Agent 都必须保持原思考等级。
#[test]
fn reasoning_change_is_transactional_when_config_write_fails() {
    let missing_config = std::env::temp_dir().join(format!(
        "lucia-missing-config-{}.toml",
        SessionId::generate()
    ));
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into())
        .with_config_path(missing_config)
        .with_reasoning_levels(
            vec![ReasoningLevel::Off, ReasoningLevel::High],
            ReasoningLevel::Off,
        );
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));

    app.set_reasoning_level(ReasoningLevel::High, Some(&agent));

    assert_eq!(app.reasoning_level, ReasoningLevel::Off);
    assert_eq!(agent.reasoning_level(), ReasoningLevel::Off);
    assert!(app.messages.last().is_some_and(|message| {
        matches!(message.kind, MsgKind::Error) && message.text.contains("当前设置未改变")
    }));
}

/// 应用内拖选应保留换行和中文宽字符，同时移除每行末尾的布局空白。
#[test]
fn mouse_text_selection_extracts_visible_buffer() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
    buffer.set_string(0, 0, "甲乙", Style::default());
    buffer.set_string(0, 1, "text", Style::default());
    let mut selection = MouseTextSelection::new(Position::new(0, 0));
    selection.update(Position::new(3, 1));

    assert_eq!(selection.extract(&buffer), "甲乙\ntext");
}

/// 换行手势（Shift+Enter / Alt+Enter / Ctrl+J）插入换行，Home/End 使用行内语义。
#[test]
fn multiline_newline_gestures_and_line_navigation() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Enter, KeyModifiers::SHIFT, None);
    app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Enter, KeyModifiers::ALT, None);
    app.handle_key(KeyCode::Char('j'), KeyModifiers::CONTROL, None);
    app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE, None);
    assert_eq!(app.input, "a\nb\n\nc");

    // Home/End 停留在当前逻辑行内，而不是整段首尾。
    app.handle_key(KeyCode::Home, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, 5);
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::End, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, 3);
}

/// 编辑快捷键按词边界与 UTF-8 字符边界移动和删除。
#[test]
fn input_editor_supports_word_and_forward_deletion() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "hello, 世界".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Left, KeyModifiers::ALT, None);
    assert_eq!(&app.input[app.cursor..], "世界");
    app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL, None);
    assert_eq!(app.input, "世界");
    app.cursor = 0;
    app.handle_key(KeyCode::Delete, KeyModifiers::NONE, None);
    assert_eq!(app.input, "界");
}

/// 多行输入支持插入换行和按显示列上下移动光标。
#[test]
fn multiline_input_moves_cursor_between_lines() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "甲乙\nabc\n终".into();
    app.cursor = "甲乙\nab".len();

    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, "甲".len());
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, "甲乙\nab".len());
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::SHIFT, None);
    assert!(app.input.ends_with('\n'));
}

/// PageUp 和 PageDown 使用最近视口高度减一作为整页步长，Ctrl+End 回到底部。
#[test]
fn page_scroll_uses_last_viewport() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.last_max_scroll = 80;
    app.last_viewport = 20;

    app.handle_key(KeyCode::PageUp, KeyModifiers::NONE, None);
    assert_eq!(app.scroll, Some(61));
    app.handle_key(KeyCode::PageDown, KeyModifiers::NONE, None);
    assert_eq!(app.scroll, None);
    app.handle_key(KeyCode::PageUp, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::End, KeyModifiers::CONTROL, None);
    assert_eq!(app.scroll, None);
}

/// MIME 推断：图片按扩展名，未知扩展按内容区分文本与二进制。
#[test]
fn attachment_media_type_detects_kinds() {
    let png = attachment_media_type(Path::new("a.PNG"), b"binary");
    assert_eq!(png, ("image/png".to_string(), true));

    let pdf = attachment_media_type(Path::new("b.pdf"), b"%PDF");
    assert_eq!(pdf, ("application/pdf".to_string(), false));

    let text = attachment_media_type(Path::new("c.rs"), "fn main() {}".as_bytes());
    assert_eq!(text, ("text/plain".to_string(), false));

    let binary = attachment_media_type(Path::new("d.bin"), &[0u8, 159, 146, 150]);
    assert_eq!(binary, ("application/octet-stream".to_string(), false));
}

/// 输入渲染将引用标签切分为独立的高亮片段。
#[test]
fn input_ref_spans_highlight_labels() {
    let attachments = vec![PendingAttachment {
        label: "[IMAGE#1]".to_string(),
        name: "pic.png".to_string(),
        media_type: "image/png".to_string(),
        data: String::new(),
        is_image: true,
        pasted_text: None,
    }];

    let spans = input_ref_spans("看 [IMAGE#1] 这个", &attachments);
    let contents: Vec<&str> = spans.iter().map(|span| span.content.as_ref()).collect();

    assert_eq!(contents, vec!["看 ", "[IMAGE#1]", " 这个"]);
}
