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

/// 短粘贴按普通输入插入，不生成引用标签，用户可以接着就地改字。
#[test]
fn short_paste_lands_in_the_input_verbatim() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste("终端项目是：lucia");

    assert_eq!(app.input, "终端项目是：lucia");
    assert_eq!(app.cursor, app.input.len());
    assert!(app.attachments.is_empty(), "短文本不应产生附件");
    let blocks = app.take_submission().expect("应有文本提交").blocks();
    assert!(matches!(
        blocks.as_slice(),
        [ContentBlock::Text { text }] if text == "终端项目是：lucia"
    ));
}

/// 从终端复制的一行常带结尾换行：落进输入框时去掉，光标不该被推到空行。
#[test]
fn single_line_paste_drops_the_trailing_newline() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste("cargo test -p lucia\r\n");

    assert_eq!(app.input, "cargo test -p lucia");
    assert_eq!(app.cursor, app.input.len());
    assert!(app.attachments.is_empty());
}

/// 单行但超宽的长文本仍然折叠成标签，避免把输入盒撑成一大团。
#[test]
fn long_single_line_paste_folds_into_a_label() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let long = "x".repeat(crate::app::input::PASTE_TAG_COLUMNS + 1);

    app.handle_paste(&long);

    assert_eq!(app.input, "[粘贴的文本] ");
    let blocks = app.take_submission().expect("应有粘贴文本提交").blocks();
    assert!(matches!(blocks.as_slice(), [ContentBlock::Text { text }] if text == &long));
}

/// 删除粘贴标签必须同步删除绑定原文，提交时不得残留隐藏正文。
#[test]
fn deleting_pasted_text_label_removes_bound_content() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.handle_paste("不应发送的正文\n第二行");
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
    app.mark_model_thinking();

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));
    app.append_model_delta("取消后");
    app.mark_model_thinking();

    assert_eq!(app.messages[0].text, "取消前");
    assert!(!app.model_is_thinking);
    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[1].text, "Cancelling the current run...");
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
    app.mark_model_thinking();
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
    app.mark_model_thinking();
    app.finish_model_thinking();
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
    assert!(!resumed.contains("审批中思考"), "{resumed:?}");
    assert!(resumed.contains("审批期间工具"), "{resumed:?}");
    // 审批结束后仍在运行的工具接管状态行，思考状态不再残留。
    assert!(resumed.contains("Esctostop"), "{resumed:?}");
    assert!(!resumed.contains("Thinking"), "{resumed:?}");
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

/// 粘贴的引号包裹或转义路径解析为文件；相对路径按基准目录解析；普通文本返回 None。
#[test]
fn pasted_file_path_parses_dropped_paths() {
    let path = write_temp_attachment("带 空格.txt", b"x");
    let display = path.display().to_string();
    let base = std::env::temp_dir();

    let quoted = format!("'{display}'");
    assert_eq!(pasted_file_path(&quoted, &base), Some(path.clone()));

    let escaped = display.replace(' ', "\\ ");
    assert_eq!(pasted_file_path(&escaped, &base), Some(path.clone()));

    // 裸文件名按基准目录解析：Finder 复制文件时剪贴板文本只有文件名。
    let bare = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(pasted_file_path(&bare, &base), Some(path.clone()));

    // `./` 前缀同样按基准目录解析。
    let relative = format!("./{bare}");
    assert_eq!(pasted_file_path(&relative, &base), Some(path.clone()));

    // 基准目录不含该文件时，相对路径不得误判为附件。
    assert_eq!(
        pasted_file_path(&bare, Path::new("/nonexistent-base")),
        None
    );

    std::fs::remove_file(&path).ok();
    assert_eq!(pasted_file_path("普通粘贴文本", &base), None);
    assert_eq!(pasted_file_path(&display, &base), None);
}

/// 直接输入相对路径回车：路径文本原样发送，媒体文件同时作为附件带给模型。
///
/// 路径不被改写成引用标签，因为同一段文字可能是一条待执行的命令；附件是追加的，
/// 模型既能看到用户原话，也能真正读到图片内容。
#[test]
fn enter_sends_relative_media_path_with_attachment() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("直接输入的图.png", b"\x89PNG\r\n\x1a\n");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    app.workspace.cwd = path.parent().unwrap().to_path_buf();

    app.input = format!("./{name}");
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);

    assert!(app.input.is_empty(), "路径文本应作为消息提交并清空输入");
    assert!(!app.running, "无 Agent 时进入排队而不是直接运行");
    let queued = app.queued_inputs.back().expect("应排入一条待发送输入");
    assert_eq!(queued.text, format!("./{name}"), "正文必须保留用户原话");
    assert_eq!(queued.attachments.len(), 1, "相对路径指向的图片应转成附件");
    assert!(queued.attachments[0].is_image);

    std::fs::remove_file(&path).ok();
}

/// 运行中发送带附件的输入：不能当 steering 丢掉附件，必须整条排队。
#[test]
fn running_input_with_attachment_queues_instead_of_steering() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    let path = write_temp_attachment("排队图.png", b"\x89PNG\r\n\x1a\n");
    let display = path.display().to_string();

    app.running = true;
    app.handle_paste_with(&display, Vec::new);
    assert_eq!(app.attachments.len(), 1);
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert!(app.input.is_empty(), "输入应被队列取走");
    let queued = app.queued_inputs.back().expect("带附件时必须排队");
    assert_eq!(queued.attachments.len(), 1);
    assert!(queued.attachments[0].is_image);
    assert_eq!(queued.text, "[IMAGE#1]", "路径已由标签替换");

    std::fs::remove_file(&path).ok();
}

/// 运行中普通回车默认排队：消息显示在输入框上方，可继续修改。
#[test]
fn running_plain_text_queues_by_default() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.running = true;
    app.input = "排队中的话".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert_eq!(app.queued_inputs.len(), 1);
    assert_eq!(app.queued_inputs[0].text, "排队中的话");
    assert!(app.input.is_empty());
    assert!(
        app.messages
            .iter()
            .all(|message| !message.text.contains("Steering queued")),
        "普通回车不应走 steering"
    );
}

/// Ctrl+Enter 保留立即插话：纯文本直接作为 steering 发送，不进入排队面板。
#[test]
fn ctrl_enter_steers_plain_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.running = true;
    app.input = "立即插话".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));

    assert!(app.queued_inputs.is_empty(), "Ctrl+Enter 不进入队列");
    assert!(app.input.is_empty());
    assert!(
        app.messages
            .iter()
            .any(|message| message.text.contains("Steering queued")),
        "应提示已作为 steering 发送"
    );
}

/// 运行中发送带上下文引用的输入：引用同样不能当 steering 丢掉。
#[test]
fn running_input_with_refs_queues_instead_of_steering() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));

    app.refs.push(crate::session::conversation::PendingRef {
        label: "[REF#1]".into(),
        provider: "test".into(),
        id: "ref-1".into(),
        title: "测试引用".into(),
        body: "引用正文".into(),
    });
    app.input = "[REF#1] 看看这个".into();
    app.cursor = app.input.len();
    app.running = true;

    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert_eq!(app.queued_inputs.len(), 1, "带引用时必须排队");
    assert_eq!(app.queued_inputs[0].refs.len(), 1);
}

/// 从排队面板取回消息：正文与附件回到输入框，队列移除该项。
#[test]
fn editing_queued_input_restores_text_and_attachments() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "排队正文 [FILE#1] ".into();
    app.attachments.push(PendingAttachment {
        label: "[FILE#1]".into(),
        name: "a.txt".into(),
        media_type: "text/plain".into(),
        data: String::new(),
        is_image: false,
        pasted_text: None,
        source_path: None,
    });
    app.queue_input_until_ready();
    assert_eq!(app.queued_inputs.len(), 1);

    assert!(app.edit_queued_input(0));

    assert_eq!(app.input, "排队正文 [FILE#1]");
    assert_eq!(app.attachments.len(), 1);
    assert!(app.queued_inputs.is_empty());
}

/// 发送时自动附加、正文里没有标签的图片，取回编辑时补上标签并保留附件。
#[test]
fn editing_queued_input_keeps_send_time_attachment() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("排队正文图.png", b"\x89PNG");
    app.input = format!("看 {}", path.display());
    app.cursor = app.input.len();
    app.queue_input_until_ready();
    assert_eq!(app.queued_inputs.len(), 1);
    assert!(!app.queued_inputs[0].text.contains("[IMAGE#1]"));

    assert!(app.edit_queued_input(0));

    assert!(
        app.input.contains("[IMAGE#1]"),
        "取回时应补上附件标签：{:?}",
        app.input
    );
    assert_eq!(app.attachments.len(), 1, "编辑不能丢掉自动附加的附件");

    std::fs::remove_file(&path).ok();
}

/// 输入框已有草稿时取回排队消息：草稿排到队尾，两边内容都不丢。
#[test]
fn editing_queued_input_keeps_current_draft() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs.push_back("排队一".into());
    app.input = "当前草稿".into();
    app.cursor = app.input.len();

    assert!(app.edit_queued_input(0));

    assert_eq!(app.input, "排队一");
    assert_eq!(
        app.queued_inputs
            .back()
            .map(|submission| submission.text.as_str()),
        Some("当前草稿")
    );
}

/// 正在发送的队首不能取回，避免完成回调找不到对应的 FIFO 输入。
#[test]
fn editing_running_queue_head_is_rejected() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs.push_back("排队一".into());
    app.queued_run_active = true;

    assert!(!app.edit_queued_input(0));
    assert_eq!(app.queued_inputs.len(), 1);
    assert!(app.input.is_empty());
}

/// Ctrl+O 取回最早的一条排队消息。
#[test]
fn ctrl_o_focuses_queue() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs.push_back("排队一".into());
    app.queued_inputs.push_back("排队二".into());

    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL, None);

    assert_eq!(app.queue_focus, Some(0), "应聚焦排队区并选中队首");
    assert!(app.input.is_empty(), "Ctrl+O 不再直接取回编辑");
    assert_eq!(app.queued_inputs.len(), 2);
}

/// 排队面板内方向键移动选择，越界时停住；Esc 返回输入框。
#[test]
fn queue_focus_moves_selection_and_escapes() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs = VecDeque::from(["一".into(), "二".into(), "三".into()]);
    app.queue_focus = Some(0);

    app.handle_queue_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.queue_focus, Some(1));
    app.handle_queue_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.queue_focus, Some(2));
    app.handle_queue_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.queue_focus, Some(2), "末尾不应越界");
    app.handle_queue_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.queue_focus, Some(1));

    app.handle_queue_key(KeyCode::Esc, KeyModifiers::NONE, None);
    assert_eq!(app.queue_focus, None);
}

/// 空闲时 Enter 发送选中项；剩余队列保持原顺序。
#[tokio::test]
async fn queue_enter_idle_sends_selected() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.queued_inputs = VecDeque::from(["一".into(), "二".into(), "三".into()]);
    app.queue_focus = Some(1);

    app.handle_queue_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert!(app.running, "选中的排队消息应立即开始新一轮");
    let texts: Vec<&str> = app
        .queued_inputs
        .iter()
        .map(|submission| submission.text.as_str())
        .collect();
    assert_eq!(texts, ["一", "三"], "其余队列保持原顺序");
    assert!(
        app.messages
            .iter()
            .any(|message| matches!(message.kind, MsgKind::User) && message.text == "二"),
        "发送的消息应出现在正文"
    );
}

/// 运行中 Enter 只置顶，不打断当前轮次；正在发送的队首不能重复发送。
#[test]
fn queue_enter_running_promotes_and_protects_head() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.queued_run_active = true;
    app.queued_inputs = VecDeque::from(["running".into(), "a".into(), "b".into()]);
    app.queue_focus = Some(2);

    app.handle_queue_key(KeyCode::Enter, KeyModifiers::NONE, None);

    let texts: Vec<&str> = app
        .queued_inputs
        .iter()
        .map(|submission| submission.text.as_str())
        .collect();
    assert_eq!(texts, ["running", "b", "a"], "选中项应移到队首之后");
    assert_eq!(app.queue_focus, Some(1));

    app.queue_focus = Some(0);
    app.handle_queue_key(KeyCode::Enter, KeyModifiers::NONE, None);
    let texts: Vec<&str> = app
        .queued_inputs
        .iter()
        .map(|submission| submission.text.as_str())
        .collect();
    assert_eq!(texts, ["running", "b", "a"], "正在发送的队首不能重复发送");
    assert!(app
        .messages
        .iter()
        .any(|message| message.text.contains("正在发送")));
}

/// 排队面板内 Ctrl+O 取回选中项编辑，焦点回到输入框。
#[test]
fn queue_ctrl_o_edits_selected() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs = VecDeque::from(["一".into(), "二".into()]);
    app.queue_focus = Some(1);

    app.handle_queue_key(KeyCode::Char('o'), KeyModifiers::CONTROL, None);

    assert_eq!(app.input, "二");
    assert_eq!(app.queued_inputs.len(), 1);
    assert_eq!(app.queue_focus, None);
}

/// Delete 丢弃选中项，选择落在下一个条目。
#[test]
fn queue_delete_removes_selected() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs = VecDeque::from(["一".into(), "二".into(), "三".into()]);
    app.queue_focus = Some(1);

    app.handle_queue_key(KeyCode::Delete, KeyModifiers::NONE, None);

    let texts: Vec<&str> = app
        .queued_inputs
        .iter()
        .map(|submission| submission.text.as_str())
        .collect();
    assert_eq!(texts, ["一", "三"]);
    assert_eq!(app.queue_focus, Some(1));
}

/// 点击排队行只是选中并聚焦，不直接取回编辑；点击面板外退出焦点。
#[test]
fn clicking_queued_row_focuses_it() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs.push_back("排队一".into());
    app.queued_input_areas = vec![(0, Rect::new(0, 5, 80, 1))];
    let mouse = MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };

    assert!(app.handle_queued_input_click(&mouse));
    assert_eq!(app.queue_focus, Some(0));
    assert!(app.input.is_empty(), "点击不再直接取回编辑");
    assert_eq!(app.queued_inputs.len(), 1);

    let outside = MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 10,
        row: 8,
        modifiers: KeyModifiers::NONE,
    };
    assert!(!app.handle_queued_input_click(&outside));
    assert_eq!(app.queue_focus, None, "点击面板外应退出焦点");
}

/// 正文中点名的本地图片在提交前自动附加，且同一路径只附加一次。
#[test]
fn submit_attaches_mentioned_media() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("正文提到的图.png", b"\x89PNG\r\n\x1a\n");
    let display = path.display().to_string();
    app.workspace.cwd = path.parent().unwrap().to_path_buf();

    let message = format!("看这张图，{display}，另外 {display} 也要看");
    app.input = message.clone();
    app.cursor = app.input.len();
    let submission = app.take_submission().expect("应产生一次提交");

    assert_eq!(submission.text, message, "正文不得被改写成引用标签");
    assert_eq!(submission.attachments.len(), 1, "同一路径只应附加一次");
    assert_eq!(
        submission.attachments[0].source_path.as_deref(),
        Some(path.as_path())
    );
    assert!(
        submission
            .blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })),
        "附件必须以图片内容块发送给模型"
    );

    std::fs::remove_file(&path).ok();
}

/// 嵌在句子里的文本文件路径不内联，但会登记为只读授权供 `read_file` 按需读取。
#[test]
fn submit_keeps_text_file_paths_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("说明.txt", b"hello");
    app.workspace.cwd = path.parent().unwrap().to_path_buf();

    app.input = format!("看 {} 这个文件", path.display());
    app.cursor = app.input.len();
    let submission = app.take_submission().expect("应产生一次提交");

    assert!(
        submission.attachments.is_empty(),
        "句子里的文本文件不得被自动附加"
    );
    assert!(
        app.read_path_grants
            .contains(&path.canonicalize().expect("测试文件应可解析")),
        "正文里的路径仍应授权只读访问"
    );

    std::fs::remove_file(&path).ok();
}

/// 独立成行的文本文件路径在发送时内联为附件。
#[test]
fn submit_attaches_standalone_text_path() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("独立文本.txt", b"hello");
    app.workspace.cwd = path.parent().unwrap().to_path_buf();

    app.input = path.display().to_string();
    app.cursor = app.input.len();
    let submission = app.take_submission().expect("应产生一次提交");

    assert_eq!(submission.attachments.len(), 1, "独立路径应内联为附件");
    assert!(submission.attachments[0].name.ends_with("独立文本.txt"));
    assert_eq!(submission.attachments[0].media_type, "text/plain");

    std::fs::remove_file(&path).ok();
}

/// 目录不能作为附件，但会登记为只读授权，让 Agent 用 list_directory 查看。
#[test]
fn submit_grants_standalone_directory_path() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let directory = std::env::temp_dir().join(format!(
        "lucia-grant-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).expect("应创建测试目录");

    app.input = directory.display().to_string();
    app.cursor = app.input.len();
    let submission = app.take_submission().expect("应产生一次提交");

    assert!(submission.attachments.is_empty(), "目录不能作为附件");
    assert!(
        app.read_path_grants
            .contains(&directory.canonicalize().expect("测试目录应可解析")),
        "目录路径应授权只读访问"
    );

    std::fs::remove_dir_all(&directory).ok();
}

/// 切换会话必须清空路径授权：新会话的用户没有再次引用过这些文件。
#[test]
fn switching_session_clears_read_grants() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("旧会话授权.txt", b"hi");
    app.read_path_grants.grant(&path).expect("应登记授权");
    let next = SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建新会话记录");

    app.replace_session(next, None);

    assert!(app.read_path_grants.is_empty());

    std::fs::remove_file(&path).ok();
}

/// 压缩等操作替换同一会话的记录时，路径授权不能被误清。
#[test]
fn replacing_same_session_keeps_read_grants() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("压缩保留授权.txt", b"hi");
    app.read_path_grants.grant(&path).expect("应登记授权");

    app.replace_session(app.session_record.clone(), None);

    assert!(!app.read_path_grants.is_empty());

    std::fs::remove_file(&path).ok();
}

/// 已通过 `/attach` 附加的文件不得因为正文里又写出路径而发送两次。
#[test]
fn mentioned_media_is_not_attached_twice() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("重复判重图.png", b"\x89PNG\r\n\x1a\n");
    let display = path.display().to_string();
    app.workspace.cwd = path.parent().unwrap().to_path_buf();

    app.attach_file(&path);
    app.input.push_str(&format!("看 {display}"));
    app.cursor = app.input.len();
    let submission = app.take_submission().expect("应产生一次提交");

    assert_eq!(
        submission.attachments.len(),
        1,
        "同一来源路径只应作为一份附件发送"
    );

    std::fs::remove_file(&path).ok();
}

/// 不是本地文件的输入仍按普通消息发送，不被路径判定吞掉。
#[test]
fn enter_sends_non_path_input_as_message() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "看看 ./不存在的图.png 这个".into();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);

    assert!(app.attachments.is_empty());
    assert!(app.input.is_empty(), "普通消息应被提交并清空输入");
    assert!(!app.running, "无 Agent 时进入排队而不是直接运行");
}

/// 以 `/` 开头但其实是正文的句子按普通消息提交，不被路径或命令判定吞掉。
#[test]
fn enter_sends_slash_prefixed_sentence_as_message() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.input = "/usr/bin 应该加入 PATH".into();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);

    assert!(app.attachments.is_empty());
    assert!(app.input.is_empty(), "普通消息应被提交并清空输入");
    assert!(
        !app.messages
            .iter()
            .any(|message| matches!(message.kind, MsgKind::Error)),
        "不得报未知命令或文件错误"
    );
}

/// 直接输入绝对路径回车后按普通文本发送：只有剪贴板里的文件引用才生成附件。
#[test]
fn enter_sends_absolute_path_input_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("绝对路径的图.png", b"\x89PNG\r\n\x1a\n");
    let text = path.display().to_string();

    app.input = text.clone();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);

    assert!(app.attachments.is_empty(), "路径文本不得被转成附件");
    assert!(app.input.is_empty(), "路径文本应作为消息提交并清空输入");
    assert!(!app.running, "无 Agent 时进入排队而不是直接运行");

    std::fs::remove_file(&path).ok();
}

/// Finder 复制文件时粘贴的文本只有文件名：应回查剪贴板文件引用并附加该文件。
#[test]
fn paste_bare_filename_uses_clipboard_file_reference() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("截图.png", b"\x89PNG\r\n\x1a\n");
    let bare = path.file_name().unwrap().to_string_lossy().into_owned();

    app.handle_paste_with(&bare, || vec![path.clone()]);
    std::fs::remove_file(&path).ok();

    assert_eq!(app.input, "[IMAGE#1] ");
    assert_eq!(app.attachments.len(), 1);
    assert!(app.attachments[0].name.ends_with("截图.png"));
    assert_eq!(app.attachments[0].media_type, "image/png");
}

/// 剪贴板只有文件引用、终端只送来一次空粘贴时，仍按文件引用附加。
#[test]
fn paste_empty_event_attaches_clipboard_files() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("空粘贴图.png", b"\x89PNG");

    app.handle_paste_with("", || vec![path.clone()]);
    std::fs::remove_file(&path).ok();

    assert_eq!(app.attachments.len(), 1);
    assert_eq!(app.input, "[IMAGE#1] ");

    // 剪贴板也没有文件引用时，空粘贴保持无操作。
    app.input.clear();
    app.attachments.clear();
    app.handle_paste_with("", Vec::new);
    assert!(app.input.is_empty());
    assert!(app.attachments.is_empty());
}

/// 非媒体类型的文件同样按附件发送：判据是"剪贴板有文件引用"，不是扩展名。
#[test]
fn paste_bare_source_file_uses_clipboard_file_reference() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("MacTerminalView.swift", b"import AppKit\n");
    let bare = path.file_name().unwrap().to_string_lossy().into_owned();

    app.handle_paste_with(&bare, || vec![path.clone()]);
    std::fs::remove_file(&path).ok();

    assert_eq!(app.attachments.len(), 1, "任何类型的文件都应能附加");
    assert!(app.attachments[0].name.ends_with("MacTerminalView.swift"));
    assert_eq!(app.attachments[0].media_type, "text/plain");
}

/// 剪贴板没有文件引用时，裸文件名退化为普通文本落进输入框，而不是报错。
#[test]
fn paste_bare_filename_without_file_reference_lands_as_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste_with("不存在的截图.png", Vec::new);

    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "不存在的截图.png");
    assert!(
        app.messages.is_empty(),
        "找不到剪贴板文件引用时应退化为普通文本，不产生错误"
    );
}

/// 粘贴单条文本文件路径：附件替换路径文本，输入框只留引用标签。
#[test]
fn paste_existing_text_path_attaches_without_duplicate_path() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("粘贴路径的笔记.txt", "第一行".as_bytes());
    let text = path.display().to_string();

    // 剪贴板里残留的文件引用不能劫持路径粘贴：含分隔符的文本根本不会回查剪贴板。
    app.handle_paste_with(&text, || vec![PathBuf::from("/tmp/别的文件.png")]);

    assert_eq!(app.attachments.len(), 1, "文本文件应作为附件发送");
    assert!(app.attachments[0].name.ends_with("粘贴路径的笔记.txt"));
    assert_eq!(app.input, "[FILE#1] ", "附加成功后不应重复保留路径文本");
    assert!(app.messages.is_empty(), "成功附加不应产生提示");

    // 删除标签即丢弃附件。
    app.input.clear();
    app.finish_input_edit();
    assert!(app.attachments.is_empty());

    std::fs::remove_file(&path).ok();
}

/// 粘贴图片路径：内容进附件，输入框只留引用标签。
#[test]
fn paste_existing_image_path_attaches_once() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("粘贴路径的图.png", b"\x89PNG\r\n\x1a\n");
    let text = path.display().to_string();

    app.handle_paste_with(&text, Vec::new);
    assert_eq!(app.attachments.len(), 1);
    assert!(app.attachments[0].is_image);
    assert_eq!(app.input, "[IMAGE#1] ");

    // 提交时同一张图片只发送一次，且不再附带重复的路径正文。
    let blocks = app.take_submission().expect("应有图片提交").blocks();
    assert_eq!(
        blocks
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .count(),
        1,
        "同一张图片只应附加一次"
    );
    assert_eq!(blocks.len(), 1, "附件已替换路径文本，不应再发送路径正文");

    std::fs::remove_file(&path).ok();
}

/// 一次粘贴多条路径：按顺序逐个附加，输入框只留引用标签。
#[test]
fn paste_multiple_paths_attaches_all_as_labels() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let image = write_temp_attachment("多路径图.png", b"\x89PNG");
    let notes = write_temp_attachment("多路径笔记.txt", b"hi");
    let text = format!("{}\n{}", image.display(), notes.display());

    app.handle_paste_with(&text, Vec::new);

    assert_eq!(app.attachments.len(), 2, "两条路径都应成为附件");
    assert_eq!(app.input, "[IMAGE#1] [FILE#1] ");
    assert_eq!(app.attachments[0].media_type, "image/png");
    assert_eq!(app.attachments[1].media_type, "text/plain");

    std::fs::remove_file(&image).ok();
    std::fs::remove_file(&notes).ok();
}

/// 粘贴单条不存在的路径：保留文本并说明原因，而不是静默退化成普通文本。
#[test]
fn paste_missing_single_path_reports_reason() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste_with("/nonexistent/lucia-图.png", Vec::new);

    assert!(app.attachments.is_empty());
    assert_eq!(
        app.input, "/nonexistent/lucia-图.png",
        "路径文本必须原样保留"
    );
    let notice = app
        .messages
        .last()
        .map(|message| message.text.clone())
        .unwrap_or_default();
    assert!(
        notice.contains("未找到本地文件"),
        "应说明路径未命中：{notice}"
    );
}

/// 大文本与目录、二进制文件保留为路径文本，并说明为什么没有附件。
#[test]
fn paste_unattachable_paths_keep_text_with_reason() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let large = write_temp_attachment(
        "巨大的日志.log",
        "x".repeat(MAX_AUTO_TEXT_ATTACHMENT_BYTES as usize + 1)
            .as_bytes(),
    );
    let binary = write_temp_attachment("二进制.bin", &[0x00, 0xff, 0xfe, 0x01]);
    let directory = std::env::temp_dir();

    for (path, expected) in [
        (&large, "read_file"),
        (&binary, "二进制"),
        (&directory, "list_directory"),
    ] {
        app.input.clear();
        app.cursor = 0;
        app.messages.clear();
        app.handle_paste_with(&path.display().to_string(), Vec::new);

        assert!(
            app.attachments.is_empty(),
            "{} 不应成为附件",
            path.display()
        );
        assert_eq!(
            app.input,
            path.display().to_string(),
            "路径文本必须原样留在输入框"
        );
        let notice = app
            .messages
            .last()
            .map(|message| message.text.clone())
            .unwrap_or_default();
        assert!(
            notice.contains(expected),
            "提示应说明处理方式（期望包含 {expected}）：{notice}"
        );
    }

    std::fs::remove_file(&large).ok();
    std::fs::remove_file(&binary).ok();
}

/// 文件管理器多选复制：粘贴文本是多行文件名时按剪贴板引用逐个附加。
#[test]
fn paste_multiple_clipboard_files_attaches_all() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let image = write_temp_attachment("多选图.png", b"\x89PNG");
    let notes = write_temp_attachment("多选笔记.txt", b"hi");
    let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();

    app.handle_paste_with(&format!("{}\n{}", name(&image), name(&notes)), || {
        vec![image.clone(), notes.clone()]
    });
    std::fs::remove_file(&image).ok();
    std::fs::remove_file(&notes).ok();

    assert_eq!(app.attachments.len(), 2, "两个文件都应附加");
    assert_eq!(app.input, "[IMAGE#1] [FILE#1] ");
    assert_eq!(app.attachments[0].media_type, "image/png");
    assert_eq!(app.attachments[1].media_type, "text/plain");
}

/// 终端拖入文件给出的 `file://` URI 列表直接换成附件标签，URI 不留在正文里。
#[test]
fn paste_file_uris_attaches_files() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let image = write_temp_attachment("拖入的 图.png", b"\x89PNG");
    let notes = write_temp_attachment("拖入的 笔记.txt", b"hi");
    let uri = |path: &Path| format!("file://{}", path.display().to_string().replace(' ', "%20"));

    app.handle_paste_with(&format!("{}\n{}", uri(&image), uri(&notes)), Vec::new);
    std::fs::remove_file(&image).ok();
    std::fs::remove_file(&notes).ok();

    assert_eq!(app.input, "[IMAGE#1] [FILE#1] ", "URI 不应留在正文里");
    assert_eq!(app.attachments.len(), 2);

    // 普通链接或指向不存在文件的 URI 都按文本处理，不能变成附件。
    app.input.clear();
    app.cursor = 0;
    app.handle_paste_with("file:///nonexistent/lucia.png", Vec::new);
    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "file:///nonexistent/lucia.png");
}

/// 多选复制里混入目录时，能附加的照常附加，无法附加的合并成一条说明。
#[test]
fn paste_clipboard_files_reports_skipped_entries() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let image = write_temp_attachment("可附加.png", b"\x89PNG");
    let directory = std::env::temp_dir();
    let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();

    app.handle_paste_with(&format!("{}\n{}", name(&image), name(&directory)), || {
        vec![image.clone(), directory.clone()]
    });
    std::fs::remove_file(&image).ok();

    assert_eq!(app.attachments.len(), 1, "只有文件成为附件");
    assert_eq!(
        app.input,
        format!("[IMAGE#1] {}", directory.display()),
        "附加成功的只留标签，目录保留路径文本"
    );
    let notice = app
        .messages
        .last()
        .map(|message| message.text.clone())
        .unwrap_or_default();
    assert!(notice.contains("目录"), "应说明目录未附加：{notice}");
}

/// 光标停在正文中间时粘贴路径：标签与前后文字之间都要断开，不能粘成一个词。
#[test]
fn paste_path_separates_from_surrounding_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("中间粘贴.txt", b"hi");
    let text = path.display().to_string();

    app.input = "看这个".into();
    app.cursor = app.input.len();
    app.handle_paste_with(&text, Vec::new);
    app.input.push('吧');
    app.cursor = app.input.len();

    assert_eq!(app.attachments.len(), 1);
    assert_eq!(app.input, "看这个 [FILE#1] 吧", "标签两侧都必须与正文断开");

    std::fs::remove_file(&path).ok();
}

/// 粘贴含目录信息的普通句子时不能回查剪贴板，避免每次粘贴都启动系统进程。
#[test]
fn paste_plain_text_skips_clipboard_lookup() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste_with("看 /tmp/no-such-file.png 这张", || {
        panic!("含路径分隔符的文本不应回查剪贴板")
    });

    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "看 /tmp/no-such-file.png 这张");
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
        "lucia-missing-config-{}.json",
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

/// 鼠标释放只结束拖选并保留高亮范围，选区的行号是内容行号而非屏幕行。
///
/// 这是"滚动复制"的前提：滚动只改变视口偏移，选区指向的内容不受影响。
#[test]
fn mouse_release_preserves_selection_in_content_coordinates() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let buffer = Buffer::with_lines(["01234567", "abcdefgh", "ABCDEFGH"]);
    // 内容区从屏幕第 1 行开始、视口已滚动 5 行：屏幕第 2 行对应内容第 6 行。
    app.content_area = Some(Rect::new(0, 1, 8, 2));
    app.content_scroll = 5;
    app.content_rows = 20;
    app.scroll = Some(5);
    app.last_max_scroll = 15;
    for (kind, column) in [
        (MouseEventKind::Down(crossterm::event::MouseButton::Left), 1),
        (MouseEventKind::Drag(crossterm::event::MouseButton::Left), 4),
        (MouseEventKind::Up(crossterm::event::MouseButton::Left), 4),
    ] {
        app.handle_text_selection(
            &MouseEvent {
                kind,
                column,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            &buffer,
        );
    }

    let selection = app.text_selection.expect("释放后应保留拖选高亮");
    assert!(selection.dragged);
    // 屏幕第 2 行 - 内容区顶部第 1 行 + 滚动 5 行 = 内容第 6 行。
    assert_eq!(selection.anchor, Position::new(1, 6).into());
    assert_eq!(selection.current, Position::new(4, 6).into());
    assert_eq!(selection.row_span(), (6, 6));
}

/// 65535 行后的选区按逻辑坐标提取，高亮只遍历当前视口。
#[test]
fn selection_beyond_terminal_coordinate_limit() {
    use crate::app::ContentPosition;
    use crossterm::event::MouseButton;
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    let body = (0..70_000)
        .map(|index| format!("row-{index}\n"))
        .collect::<String>();
    app.messages.push(Msg::new(MsgKind::User, body));
    let total = app.transcript_layout.sync(&app.messages, None, 40);
    app.content_rows = total;
    app.content_area = Some(Rect::new(0, 2, 40, 10));
    app.content_scroll = 69_990;
    let mut selection = MouseTextSelection::new(ContentPosition { x: 2, y: 69_990 });
    selection.update(ContentPosition { x: 12, y: 69_992 });
    let empty = Buffer::empty(Rect::new(0, 0, 40, 14));
    app.text_selection = Some(selection);
    app.scroll = Some(69_990);
    app.handle_text_selection(
        &MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        },
        &empty,
    );
    let text = app
        .text_selection_content
        .as_deref()
        .expect("必须捕获跨范围选区");
    assert!(text.contains("row-69990"), "{text}");
    assert!(text.contains("row-69992"), "{text}");
    app.text_selection = Some(selection);
    let mut frame = empty;
    app.render_text_selection(&mut frame);
    assert!(frame[(2, 2)].modifier.contains(Modifier::REVERSED));
    assert!(!frame[(2, 8)].modifier.contains(Modifier::REVERSED));
}

/// 显式复制使用的选区文本必须保留换行和中文宽字符，并移除行尾布局空白。
#[test]
fn mouse_text_selection_extracts_visible_buffer_for_explicit_copy() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
    buffer.set_string(0, 0, "甲乙", Style::default());
    buffer.set_string(0, 1, "text", Style::default());
    let mut selection = MouseTextSelection::new(Position::new(0, 0));
    selection.update(Position::new(3, 1));

    assert_eq!(selection.extract(&buffer), "甲乙\ntext");
}

/// 复制快捷键必须兼容增强协议携带的 Shift、大写字符与 Cmd，无修饰时不得误触发。
#[test]
fn copy_shortcut_accepts_terminal_key_variants() {
    assert!(is_copy_shortcut(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(is_copy_shortcut(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    ));
    // macOS 把 Command 上报为 Super；重映射修饰键后它就是用户实际按下的复制键。
    assert!(is_copy_shortcut(KeyCode::Char('c'), KeyModifiers::SUPER));
    assert!(!is_copy_shortcut(KeyCode::Char('c'), KeyModifiers::NONE));
    assert!(!is_copy_shortcut(KeyCode::Char('c'), KeyModifiers::ALT));
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
        source_path: None,
    }];

    let spans = input_ref_spans("看 [IMAGE#1] 这个", &attachments, &[]);
    let contents: Vec<&str> = spans.iter().map(|span| span.content.as_ref()).collect();

    assert_eq!(contents, vec!["看 ", "[IMAGE#1]", " 这个"]);
}

/// Ctrl+C 在空输入上必须二次确认才退出，单次误触只给提示。
#[test]
fn ctrl_c_requires_confirmation_before_quitting() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);
    assert!(!app.should_quit);
    assert!(app.exit_confirmation_pending());

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);
    assert!(app.should_quit);
}

/// Ctrl+Z 在 raw mode 下收不到终端信号，只能登记挂起意图交给主循环处理。
#[test]
fn ctrl_z_requests_suspend_without_touching_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "未发送的草稿".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Char('z'), KeyModifiers::CONTROL, None);

    assert!(app.pending_suspend, "Ctrl+Z 应登记挂起意图");
    assert_eq!(app.input, "未发送的草稿", "挂起不得动输入框内容");
    assert!(!app.should_quit);
}

/// 两次 Ctrl+C 之间出现其他按键时必须重新计数，避免误退出。
#[test]
fn ctrl_c_confirmation_is_reset_by_other_keys() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);
    app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE, None);
    assert!(!app.exit_confirmation_pending());

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);
    assert!(!app.should_quit, "重新计数后首次 Ctrl+C 不得退出");
}

/// 超过确认窗口的第二次 Ctrl+C 只能重新计数，不能退出。
#[test]
fn ctrl_c_confirmation_expires() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.last_exit_request_at = Some(
        std::time::Instant::now()
            .checked_sub(EXIT_CONFIRM_WINDOW + std::time::Duration::from_millis(1))
            .expect("测试时间点应可回退"),
    );

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);

    assert!(!app.should_quit);
    assert!(app.exit_confirmation_pending());
}

/// 有草稿时 Ctrl+C 先清空输入与附件，不进入退出确认。
#[test]
fn ctrl_c_clears_draft_before_offering_exit() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "未发送的草稿".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);

    assert!(app.input.is_empty());
    assert_eq!(app.cursor, 0);
    assert!(!app.should_quit);
    assert!(!app.exit_confirmation_pending());
}

/// 运行期间的 Ctrl+C 只中断本次运行，绝不退出程序。
#[test]
fn ctrl_c_interrupts_run_instead_of_quitting() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.messages
        .push(Msg::tool_started(agent_tool::ToolCall::new(
            "call-1",
            "shell",
            serde_json::json!({}),
        )));

    app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, None);

    assert!(!app.should_quit);
    assert!(app.run_stream_cancelled);
    assert!(matches!(app.messages[0].kind, MsgKind::ToolSkipped));
}

/// Ctrl+D 在空输入上按终端 EOF 约定直接退出，有内容时执行前向删除。
#[test]
fn ctrl_d_exits_only_on_empty_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "abc".into();
    app.cursor = 0;

    app.handle_key(KeyCode::Char('d'), KeyModifiers::CONTROL, None);
    assert_eq!(app.input, "bc");
    assert!(!app.should_quit);

    app.input.clear();
    app.cursor = 0;
    app.handle_key(KeyCode::Char('d'), KeyModifiers::CONTROL, None);
    assert!(app.should_quit);
}

/// readline 行内编辑键覆盖行首行尾移动与前后半行删除。
#[test]
fn readline_line_editing_shortcuts() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "first\nsecond line".into();
    app.cursor = app.input.len();

    // Ctrl+A / Ctrl+E 在当前逻辑行内移动，不跨行。
    app.handle_key(KeyCode::Char('a'), KeyModifiers::CONTROL, None);
    assert_eq!(app.cursor, "first\n".len());
    app.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL, None);
    assert_eq!(app.cursor, app.input.len());

    // Ctrl+U 删除光标之前的半行，保留上一行。
    app.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL, None);
    assert_eq!(app.input, "first\n");

    // Ctrl+K 删除光标之后的半行。
    app.input = "keep|drop".into();
    app.cursor = "keep".len();
    app.handle_key(KeyCode::Char('k'), KeyModifiers::CONTROL, None);
    assert_eq!(app.input, "keep");
}

/// Ctrl 与 Alt 都必须映射为按词移动：终端对这两个修饰键的上报并不一致。
#[test]
fn word_motion_accepts_ctrl_and_alt() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "alpha beta".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Left, KeyModifiers::CONTROL, None);
    assert_eq!(app.cursor, "alpha ".len());
    app.handle_key(KeyCode::Left, KeyModifiers::ALT, None);
    assert_eq!(app.cursor, 0);
    app.handle_key(KeyCode::Right, KeyModifiers::CONTROL, None);
    assert_eq!(app.cursor, "alpha ".len());
}

/// 点击底栏上下文芯片应开关 `/context` 详情面板，再点一次收起。
#[test]
fn clicking_context_chip_toggles_context_details() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.context_chip_areas = vec![Rect::new(20, 15, 14, 1), Rect::new(0, 16, 5, 1)];

    let inside = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 24,
        row: 15,
        modifiers: KeyModifiers::NONE,
    };
    assert!(app.handle_context_chip_click(&inside));
    assert!(app.native_command.context_open, "点击芯片应展开上下文详情");

    assert!(app.handle_context_chip_click(&inside));
    assert!(!app.native_command.context_open, "再次点击应收起详情");

    // 芯片之外、芯片不存在的场景都必须把事件交还给其他热区与文本拖选。
    let outside = MouseEvent {
        column: 60,
        ..inside
    };
    assert!(!app.handle_context_chip_click(&outside));
    // 折到下一行的片段也能点击和悬停；片段右侧空白不能命中。
    let wrapped = MouseEvent {
        column: 2,
        row: 16,
        ..inside
    };
    assert!(app.handle_context_chip_click(&wrapped));
    assert!(app.native_command.context_open);
    assert!(app.handle_mouse_event(&MouseEvent {
        kind: MouseEventKind::Moved,
        ..wrapped
    }));
    assert!(app.is_context_chip_hovered);
    let blank = MouseEvent {
        column: 6,
        ..wrapped
    };
    assert!(!app.handle_context_chip_click(&blank));
    assert!(app.handle_mouse_event(&MouseEvent {
        kind: MouseEventKind::Moved,
        ..blank
    }));
    assert!(!app.is_context_chip_hovered);

    app.context_chip_areas.clear();
    assert!(!app.handle_context_chip_click(&inside));
}

/// 手动翻阅后点击右下角提示应回到底部，并优先于文本拖选消费该事件。
#[test]
fn clicking_jump_hint_returns_to_bottom() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.scroll = Some(3);
    app.jump_hint_area = Some(Rect::new(60, 20, 20, 1));

    let inside = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 65,
        row: 20,
        modifiers: KeyModifiers::NONE,
    };
    assert!(app.handle_jump_hint_click(&inside));
    assert_eq!(app.scroll, None, "点击热区应恢复自动跟随底部");

    // 热区之外的点击必须原样交还给拖选逻辑。
    app.scroll = Some(3);
    let outside = MouseEvent {
        column: 10,
        ..inside
    };
    assert!(!app.handle_jump_hint_click(&outside));
    assert_eq!(app.scroll, Some(3));
}

/// 未展示提示时热区不存在，任何点击都不得改变滚动位置。
#[test]
fn jump_hint_click_is_inert_without_hint() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.scroll = Some(5);
    app.jump_hint_area = None;

    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 65,
        row: 20,
        modifiers: KeyModifiers::NONE,
    };

    assert!(!app.handle_jump_hint_click(&click));
    assert_eq!(app.scroll, Some(5));
}

/// 滚动条上的按下与拖拽把行号线性映射为滚动偏移，拖到底部恢复自动跟随。
#[test]
fn scrollbar_drag_maps_rows_to_scroll_positions() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    // 视口 21 行、总内容 521 行：滑块退化成最小高度（3 行 = 6 个半行），
    // 可动区间是 2*21-6 = 36 个半行，其中点正好对应滚动范围的中点。
    app.last_max_scroll = 500;
    app.last_viewport = 21;
    // 轨道覆盖第 2 至 22 行，共 21 行；首行对应顶部、末行对应底部。
    app.scrollbar_area = Some(Rect::new(80, 2, 1, 21));

    let event = |kind, row| MouseEvent {
        kind,
        column: 80,
        row,
        modifiers: KeyModifiers::NONE,
    };

    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Down(MouseButton::Left), 2)));
    assert_eq!(app.scroll, Some(0));
    assert!(app.scrollbar_drag);

    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Drag(MouseButton::Left), 11)));
    assert_eq!(app.scroll, Some(250), "可动区间中点应落在滚动范围的中点");
    assert!(app.scrollbar_drag);

    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Drag(MouseButton::Left), 22)));
    assert_eq!(app.scroll, None, "拖到末行应恢复自动跟随");
    assert!(app.scrollbar_drag, "释放前仍处于拖拽态");

    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Up(MouseButton::Left), 22)));
    assert!(!app.scrollbar_drag, "释放后退出拖拽态");
}

/// 按住滑块本身拖动时保持抓取点：按下不跳、移动一行滑块也只移动一行。
#[test]
fn scrollbar_drag_preserves_grab_offset() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.last_max_scroll = 100;
    app.last_viewport = 21;
    // 50/100 的滚动位置对应滑块顶端 18 个半行、高 7 个半行，覆盖第 11..=14 行。
    app.scroll = Some(50);
    app.scrollbar_area = Some(Rect::new(80, 2, 1, 21));

    let event = |kind, row| MouseEvent {
        kind,
        column: 80,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // 按在滑块中部：只记录抓取点，滚动位置不得在按下瞬间改变。
    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Down(MouseButton::Left), 13)));
    assert_eq!(app.scroll, Some(50), "抓住滑块按下时不应跳转");
    assert_eq!(app.scrollbar_grab_half, Some(4));

    // 指针下移一行：滑块顶端也下移一行，滚动位置按同一段行程换算。
    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Drag(MouseButton::Left), 14)));
    assert_eq!(app.scroll, Some(57), "拖动一行应只推进一行对应的滚动量");

    // 拖到底部仍能精确命中自动跟随。
    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Drag(MouseButton::Left), 22)));
    assert_eq!(app.scroll, None);

    assert!(app.handle_scrollbar_drag(&event(MouseEventKind::Up(MouseButton::Left), 22)));
    assert!(!app.scrollbar_drag);
    assert_eq!(app.scrollbar_grab_half, None, "释放后清空抓取点");
}

/// 滚动条热区之外的点击、以及未开始的拖拽都必须交还给其他鼠标处理。
#[test]
fn scrollbar_ignores_events_outside_track() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.last_max_scroll = 100;
    app.last_viewport = 21;
    app.scroll = Some(40);
    app.scrollbar_area = Some(Rect::new(80, 2, 1, 21));

    let outside = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 79,
        row: 10,
        modifiers: KeyModifiers::NONE,
    };
    assert!(!app.handle_scrollbar_drag(&outside));
    assert_eq!(app.scroll, Some(40));

    // 没有先按下时，拖动不能被滚动条截获。
    let drag = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 80,
        row: 20,
        modifiers: KeyModifiers::NONE,
    };
    assert!(!app.handle_scrollbar_drag(&drag));
    assert_eq!(app.scroll, Some(40));

    // 滚动条未展示时任何按下都不生效。
    app.scrollbar_area = None;
    assert!(!app.handle_scrollbar_drag(&outside));
    assert_eq!(app.scroll, Some(40));
}

/// 只有指针移动事件才能翻转滚动条悬停态，滚轮与按键携带的坐标不算。
#[test]
fn scrollbar_hover_follows_pointer_moves_only() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.scrollbar_area = Some(Rect::new(80, 2, 1, 21));

    let event = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // 移入轨道：翻转并报告变化，重复上报同一位置则不再算变化。
    assert!(app.handle_mouse_event(&event(MouseEventKind::Moved, 80, 10)));
    assert!(app.is_scrollbar_hovered);
    assert!(!app.handle_mouse_event(&event(MouseEventKind::Moved, 80, 10)));

    // 移出轨道（含首行上方与末行下方）：立刻还原。
    assert!(app.handle_mouse_event(&event(MouseEventKind::Moved, 79, 10)));
    assert!(!app.is_scrollbar_hovered);
    assert!(!app.handle_mouse_event(&event(MouseEventKind::Moved, 80, 23)));
    assert!(!app.is_scrollbar_hovered);

    // 按下、拖拽与滚轮携带的是事件点而非指针位置，不能凭空点亮高亮。
    assert!(!app.handle_mouse_event(&event(MouseEventKind::Down(MouseButton::Left), 80, 10)));
    assert!(!app.handle_mouse_event(&event(MouseEventKind::ScrollUp, 80, 10)));
    assert!(!app.is_scrollbar_hovered);

    // 滚动条未展示时任何坐标都不算悬停。
    app.scrollbar_area = None;
    assert!(!app.handle_mouse_event(&event(MouseEventKind::Moved, 80, 10)));
    assert!(!app.is_scrollbar_hovered);
}

/// 命令修饰键必须同时接受 Control 与 Super，覆盖 macOS 重映射后的上报差异。
#[test]
fn command_modifier_accepts_control_and_super() {
    assert!(is_command_modifier(KeyModifiers::CONTROL));
    assert!(is_command_modifier(KeyModifiers::SUPER));
    assert!(is_command_modifier(
        KeyModifiers::SUPER | KeyModifiers::SHIFT
    ));
    assert!(!is_command_modifier(KeyModifiers::ALT));
    assert!(!is_command_modifier(KeyModifiers::NONE));
}

/// Cmd（Super）必须与 Ctrl 等价地触发复制与退出确认。
#[test]
fn super_modifier_drives_the_same_shortcuts_as_control() {
    assert!(is_copy_shortcut(KeyCode::Char('c'), KeyModifiers::SUPER));

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "草稿".into();
    app.cursor = app.input.len();

    // Cmd+C 与 Ctrl+C 一样先清空草稿。
    app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER, None);
    assert!(app.input.is_empty());
    assert!(!app.should_quit);

    // Cmd+U / Cmd+A 等行内编辑同样可用。
    app.input = "abc".into();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Char('u'), KeyModifiers::SUPER, None);
    assert!(app.input.is_empty());
}

/// 诊断模式回显实际上报的组合，并说明是否被识别为命令修饰键。
#[test]
fn key_inspection_reports_reported_modifiers() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.start_key_inspection();

    app.handle_key(KeyCode::Char('c'), KeyModifiers::ALT, None);

    let last = app.messages.last().expect("诊断应回显按键").text.clone();
    assert!(last.contains("alt + c"), "{last:?}");
    assert!(last.contains("不是命令修饰键"), "{last:?}");
    // 诊断期间按键不得改变输入内容。
    assert!(app.input.is_empty());

    app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER, None);
    let last = app.messages.last().expect("诊断应回显按键").text.clone();
    assert!(last.contains("super + c"), "{last:?}");
    assert!(last.contains("识别为命令修饰键"), "{last:?}");
}

/// Esc 退出诊断模式后按键恢复正常处理。
#[test]
fn key_inspection_exits_on_escape() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.start_key_inspection();

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, None);
    assert!(!app.key_inspect);

    app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE, None);
    assert_eq!(app.input, "x");
}
