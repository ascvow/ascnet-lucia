//! 主界面渲染与 Markdown 呈现的回归测试。

use super::*;

use crate::ui::chat::{prompt_activity, PromptActivity};

/// 长篇混排回复之后的下一轮用户消息必须立即出现在自动跟随视口中。
#[test]
fn long_mixed_reply_keeps_next_user_message_visible() {
    for width in [40, 79, 100, 123] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let body = "这是一个架构方向正确、复杂业务边界考虑充分的项目。Worker reliability 与 shared contracts 需要持续验证。\n\n- 状态恢复：apps/web/src/services/project-session.ts\n- 组合字符：e\u{301} 与 emoji 👩‍💻 测试。\n\n".repeat(100);
        app.messages.push(Msg::new(
            MsgKind::Assistant,
            format!("{}\n\n{body}", "👩‍💻".repeat(600)),
        ));
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        for marker in ["NEXT-TURN-ONE", "NEXT-TURN-TWO"] {
            app.messages.push(Msg::new(MsgKind::User, marker));
            app.scroll = None;
            app.running = true;
            terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                text.contains(marker),
                "width={width}, marker={marker}, screen={text}"
            );
        }
    }
}

/// 跳转提示占用输入框上方那一行留白；连续重绘、窗口缩小与回到底部不能截断或残留点击区域。
#[test]
fn jump_hint_uses_separate_row_and_clears_when_hidden() {
    for width in [100, 40, 16] {
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.messages.push(Msg::new(
            MsgKind::Assistant,
            "中文宽字符正文测试\n".repeat(80),
        ));
        for position in [0, 1, 20] {
            app.scroll = Some(position);
            terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
            let area = app.jump_hint_area.expect("滚动时应显示提示");
            assert_eq!(
                area.y,
                1 + app.last_viewport,
                "提示应紧接正文视口之后，占正文下方的留白行"
            );
            assert_eq!(area.right(), width, "提示右对齐：末尾贴齐终端最右一列");
            let text: String = (area.x..area.right())
                .map(|x| terminal.backend().buffer()[(x, area.y)].symbol())
                .collect();
            assert!(text.starts_with(" ↓ ") || text.starts_with('↓'), "{text}");
            assert!(!text.contains("中文"));
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(text.as_str()),
                usize::from(area.width)
            );
            assert!(app
                .tool_message_hit_regions
                .iter()
                .all(|region| region.area.bottom() <= area.y));
            // 留白行上方仍为正文，提示不能覆盖最后一行。
            let above: String = (0..width)
                .map(|x| terminal.backend().buffer()[(x, area.y - 1)].symbol())
                .collect();
            assert!(!above.contains("click to jump"));
        }
        app.scroll = None;
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        assert!(app.jump_hint_area.is_none());
        app.messages.clear();
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        assert!(app.jump_hint_area.is_none());
    }
}

/// 活动状态靠左、跳转提示靠右，两者共享输入框上方同一行；翻页不会移动这一行。
#[test]
fn status_and_jump_hint_share_the_row_above_the_prompt() {
    let width = 80;
    let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(Msg::new(
        MsgKind::Assistant,
        "状态行与提示共用的正文测试\n".repeat(80),
    ));
    app.running = true;
    app.start_model_response();
    app.mark_model_thinking();
    app.run_started_at = Some(std::time::Instant::now());

    for position in [0, 5, 40] {
        app.scroll = Some(position);
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        let hint = app.jump_hint_area.expect("滚动时应显示提示");
        let rows = buffer_rows(&terminal, width);
        let row = status_row(&rows);
        assert_eq!(hint.y as usize, row, "提示应与活动状态同一行");
        assert_eq!(hint.right(), width, "提示应贴齐终端右边界");
        let text = &rows[row];
        assert!(text.contains("Thinking"), "{text:?}");
        assert!(text.contains("click to jump"), "{text:?}");
        assert!(
            text.find("Thinking").unwrap() < text.find("click to jump").unwrap(),
            "活动状态应在左、跳转提示应在右：{text:?}"
        );
        // 这一行紧贴输入盒上边框，位置与滚动无关。
        assert_eq!(prompt_border_row(&rows), row + 1, "{text:?}");
    }
}

/// 滚动条只在内容溢出时占用右侧留白列，正文不被覆盖；清空后不得残留轨道。
#[test]
fn scrollbar_marks_overflow_and_clears_when_content_fits() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(Msg::new(
        MsgKind::Assistant,
        "滚动条轨道测试正文\n".repeat(40),
    ));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();

    let area = app.scrollbar_area.expect("内容溢出时应显示滚动条");
    assert_eq!(area.width, 1);
    assert_eq!(area.height, app.last_viewport);
    assert_eq!(area.x, 59, "滚动条应贴在终端最右一列");
    assert_eq!(area.right(), 60, "滚动条右边不得再留任何空白列");
    // 轨道与滑块只用一列宽字符，且不会挤掉正文：正文右边界在滚动条左侧。
    let column: String = (area.y..area.bottom())
        .map(|y| terminal.backend().buffer()[(area.x, y)].symbol())
        .collect();
    assert!(
        column
            .chars()
            .all(|glyph| matches!(glyph, '│' | '█' | '▀' | '▄')),
        "滚动条列只应出现轨道或滑块：{column:?}"
    );
    assert!(
        column.chars().any(|glyph| matches!(glyph, '█' | '▀' | '▄')),
        "应能看到滑块：{column:?}"
    );

    // 内容不再溢出时滚动条消失，上一帧的轨道必须被擦掉。
    app.messages.clear();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    assert!(app.scrollbar_area.is_none());
    let leftover: String = (area.y..area.bottom())
        .map(|y| terminal.backend().buffer()[(area.x, y)].symbol())
        .collect();
    assert_eq!(leftover.trim(), "", "残留轨道：{leftover:?}");
}

/// 共享时间线缩窄时只更新自己的热区，不得清除本帧已经画好的相邻组件。
#[test]
fn transcript_resize_preserves_already_rendered_neighbor() {
    use crate::ui::chat::{render_transcript_pane, TranscriptPane};
    use ratatui::widgets::Paragraph;

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test-model".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "长正文\n".repeat(40)));
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let neighbor = Rect::new(40, 0, 20, 10);
    for width in [59, 39] {
        terminal
            .draw(|frame| {
                if width == 39 {
                    frame.render_widget(
                        Paragraph::new("01234567890123456789\n".repeat(10)),
                        neighbor,
                    );
                }
                render_transcript_pane(
                    frame,
                    Rect::new(0, 0, width, 10),
                    TranscriptPane {
                        messages: &app.messages,
                        streaming: None,
                        layout: &mut app.transcript_layout,
                        scroll: &mut app.scroll,
                        hit_regions: &mut app.tool_message_hit_regions,
                        last_max_scroll: &mut app.last_max_scroll,
                        last_viewport: &mut app.last_viewport,
                        content_area: &mut app.content_area,
                        content_scroll: &mut app.content_scroll,
                        content_rows: &mut app.content_rows,
                        scrollbar_area: &mut app.scrollbar_area,
                        is_scrollbar_hovered: &mut app.is_scrollbar_hovered,
                        record_hit_regions: true,
                    },
                );
            })
            .unwrap();
        assert_eq!(app.scrollbar_area, Some(Rect::new(width, 0, 1, 10)));
    }
    for y in neighbor.y..neighbor.bottom() {
        let row: String = (neighbor.x..neighbor.right())
            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
            .collect();
        assert_eq!(row, "01234567890123456789", "相邻组件第 {y} 行被旧轨道擦除");
    }
}

/// 鼠标悬停在滚动条上时轨道与滑块各提亮一档，几何位置保持不变。
///
/// 横向边距去掉后右侧没有富余列可以向左扩宽，加粗只能靠提亮表达——一旦改宽度就会
/// 盖住正文最后一列。这条断言守住两件事：几何在悬停前后完全一致，以及轨道与滑块都
/// 提亮而不是整根轨道变成双线。
#[test]
fn scrollbar_brightens_on_hover_without_widening() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(Msg::new(
        MsgKind::Assistant,
        "滚动条悬停测试正文\n".repeat(40),
    ));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();

    let idle = app.scrollbar_area.expect("内容溢出时应显示滚动条");
    assert_eq!((idle.x, idle.width), (59, 1), "常态应只占贴边的最后一列");
    let buffer = terminal.backend().buffer();
    assert!(
        (idle.y..idle.bottom())
            .map(|y| buffer[(59, y)].fg)
            .all(|color| color == crate::theme::COLOR_SEPARATOR
                || color == crate::theme::COLOR_MUTED),
        "常态轨道用分隔线灰、滑块用正文灰"
    );

    let pointer = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    // 指针停在滚动条上：状态翻转，本帧就该提亮，位置不动。
    assert!(
        app.handle_mouse_event(&pointer(MouseEventKind::Moved, 59, idle.y + 2)),
        "指针移入滚动条应翻转悬停态"
    );
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let hovered = app.scrollbar_area.expect("悬停时滚动条仍然存在");
    assert_eq!(
        (hovered.x, hovered.width),
        (idle.x, idle.width),
        "悬停不得改变几何：右侧没有富余列可扩"
    );

    let buffer = terminal.backend().buffer();
    let mut thumb_rows = 0;
    for y in hovered.y..hovered.bottom() {
        match buffer[(59, y)].symbol() {
            "█" | "▀" | "▄" => {
                thumb_rows += 1;
                assert_eq!(
                    buffer[(59, y)].fg,
                    crate::theme::COLOR_TEXT,
                    "悬停滑块应提亮：第 {y} 行"
                );
            }
            _ => assert_eq!(
                buffer[(59, y)].fg,
                crate::theme::COLOR_MUTED,
                "悬停轨道应提亮：第 {y} 行"
            ),
        }
    }
    assert!(thumb_rows > 0, "应至少有一行滑块用来验证提亮");

    // 指针移开：恢复常态配色。
    assert!(
        app.handle_mouse_event(&pointer(MouseEventKind::Moved, 40, idle.y + 2)),
        "指针离开滚动条应还原悬停态"
    );
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let restored = app.scrollbar_area.expect("内容仍溢出，滚动条不应消失");
    assert_eq!((restored.x, restored.width), (59, 1), "移开后几何不变");
    let buffer = terminal.backend().buffer();
    assert!(
        (restored.y..restored.bottom())
            .map(|y| buffer[(59, y)].fg)
            .all(|color| color == crate::theme::COLOR_SEPARATOR
                || color == crate::theme::COLOR_MUTED),
        "移开后应回到常态配色"
    );
}

/// 短对话与超窄终端都不绘制滚动条，避免为一屏以内的内容添加噪声。
#[test]
fn scrollbar_hidden_for_short_or_narrow_content() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(Msg::new(MsgKind::User, "短消息"));
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    assert!(app.scrollbar_area.is_none(), "内容不足一屏时不应显示滚动条");

    app.messages.push(Msg::new(
        MsgKind::Assistant,
        "窄终端滚动条测试\n".repeat(40),
    ));
    let mut terminal = Terminal::new(TestBackend::new(12, 10)).unwrap();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    assert_eq!(
        app.scrollbar_area.map(|area| area.height),
        Some(app.last_viewport),
        "窄终端仍保留一列滚动条，正文宽度自动收窄"
    );
}

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

/// 底栏应常驻上下文用量与配置里的上游名，用量芯片同时是可点击的 `/context` 入口。
///
/// 依赖点：芯片的屏幕列必须与绘制位置一致——否则点击热区会与看到的数字错位。
#[test]
fn footer_shows_context_usage_and_configured_upstream_name() {
    let backend = TestBackend::new(100, 16);
    let mut terminal = Terminal::new(backend).expect("创建上下文用量底栏测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "deepseek-v4-flash".into()).with_context_window(Some(200_000));
    // 同一模型 ID 可以有多条配置：底栏用配置名把生效的那一条指出来。
    app.model_upstream_name = Some("deepseek".into());
    app.context_tokens = Some(50_000);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染上下文用量底栏");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("deepseek-v4-flash [deepseek]"), "{text:?}");
    // 未在运行中也必须显示：这是随时要确认的数字，不是运行期进度。
    assert!(!app.running);
    assert!(text.contains("◔ 25% · 50.0k"), "{text:?}");

    let area = app.context_chip_area.expect("用量芯片应登记点击热区");
    let row: String = (area.x..area.x + area.width)
        .map(|x| terminal.backend().buffer()[(x, area.y)].symbol())
        .collect();
    assert_eq!(row, "◔ 25% · 50.0k", "热区必须与绘制位置对齐");
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

/// 输入提示应紧跟输入盒上边框，下边框之前不再留内部空白，之后立即进入状态栏。
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
    // 编辑行之后直接是下边框：框高等于内容高，底部不再多出一行空白。
    assert!(
        rows[rule_row + 2]
            .chars()
            .filter(|character| *character == '─')
            .count()
            > 20,
        "输入盒底部不应留空行：{rows:?}"
    );
    assert!(!rows[rule_row + 3].trim().is_empty());
}

/// 助手回答直接落在终端底色上，并且与随后的工具日志隔一行真正的空白。
///
/// 回答不再铺整块卡片底色：一层横贯消息区的色带会把正文压成"色块里的字"，宽终端上
/// 尤其像一条被拉长的横幅。回答与工具日志之间的空行必须保留，两者才不会粘成一片。
#[test]
fn assistant_answer_sits_on_the_terminal_background() {
    let width = 80;
    let backend = TestBackend::new(width, 26);
    let mut terminal = Terminal::new(backend).expect("创建卡片测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "第一段\n\n第二段"));
    let mut tool = Msg::tool_started(ToolCall::new(
        "call-card",
        "shell",
        json!({"command": "cargo test"}),
    ));
    tool.append_tool_output("line one\n");
    app.messages.push(tool);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染卡片场景");
    let buffer = terminal.backend().buffer();
    let rows = buffer
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    // 宽字符占两列，缓冲区里每列各存一个符号，比较前先去掉空白。
    let compact = |row: &str| -> String {
        row.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    };
    let card_rows = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            let row = compact(row);
            row.contains("第一段") || row.contains("第二段")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(card_rows.len(), 2, "应渲染两段回答：{rows:?}");
    let (first, second) = (card_rows[0], card_rows[1]);
    assert_eq!(second, first + 2, "段落之间应只隔一行：{rows:?}");

    // 回答内部（含段落间空行）每一个单元格都没有底色：正文直接画在终端背景上。
    //
    // 只看空白单元格：宽字符的第二列由终端按字形铺满，缓冲里不写样式（写了也没有
    // 视觉效果），检查它会得到假失败。
    for row in first..=second {
        for column in 2..width - 2 {
            let cell = &buffer[(column, row as u16)];
            // 宽字符的续占单元格由终端按字形铺满，缓冲里不写样式，跳过它。
            let wide_tail = column > 2
                && unicode_width::UnicodeWidthStr::width(buffer[(column - 1, row as u16)].symbol())
                    == 2;
            if cell.symbol() == " " && !wide_tail {
                assert_eq!(
                    cell.bg,
                    Color::Reset,
                    "第 {row} 行第 {column} 列不应有卡片底色：{rows:?}"
                );
            }
        }
    }
    // 卡片下方一行是消息间距：回到终端底色，工具日志因此不会与卡片粘连。
    assert_eq!(
        buffer[(2, second as u16 + 1)].bg,
        Color::Reset,
        "卡片与工具日志之间应留一行空白：{rows:?}"
    );
    let tool_row = rows
        .iter()
        .position(|row| row.contains("Shell"))
        .expect("应展示工具标题");
    assert_eq!(
        buffer[(2, tool_row as u16)].bg,
        Color::Reset,
        "工具日志不铺卡片底色"
    );
}

/// 主内容区横向不留边距：正文贴住终端第 1 列，输入盒边框铺满整行。
///
/// 助手回答的行首标记不额外缩进：`●` 与用户消息的 `▌` 落在同一列，两种消息的正文
/// 都从第二列起笔。那属于消息自身的版式，不是主内容区的外边距。
#[test]
fn main_content_uses_the_full_width() {
    let width = 80;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建整宽测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "整宽测试正文"));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染整宽版面");
    let rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();

    let marker_row = rows
        .iter()
        .position(|row| row.contains('●'))
        .expect("应展示助手回复");
    assert!(
        rows[marker_row].starts_with("● "),
        "标记应贴住内容区第一列，与用户消息的竖条同列：{:?}",
        rows[marker_row]
    );
    // 回答不铺卡片底色：第 0 列就是标记本身，前面没有任何留白列。
    assert_eq!(
        terminal.backend().buffer()[(0, marker_row as u16)].bg,
        Color::Reset,
        "第 0 列不应属于留白"
    );
    let footer_row = rows.last().expect("底栏应存在");
    // 底栏不再有品牌前缀，仍必须从终端第 1 列开始，不能留缩进。
    assert!(
        !footer_row.starts_with(' '),
        "底栏应从第 1 列开始：{footer_row:?}"
    );
    // 输入盒边框铺满整行：不再有左右边框，上下两条边框直接贴住左右边缘。
    let prompt_row = rows
        .iter()
        .position(|row| row.starts_with('─'))
        .expect("应展示输入盒上边框");
    assert!(
        rows[prompt_row].starts_with('─') && rows[prompt_row].trim_end().ends_with('─'),
        "输入盒应铺满整行：{:?}",
        rows[prompt_row]
    );
}

/// 连续的工具结果之间留一行空白：两次调用的输出不再连成一片。
#[test]
fn consecutive_tool_results_are_separated_by_a_blank_row() {
    let width = 80;
    let backend = TestBackend::new(width, 24);
    let mut terminal = Terminal::new(backend).expect("创建工具间距测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    for (id, command) in [("call-a", "cargo build"), ("call-b", "cargo test")] {
        let mut tool = Msg::tool_started(ToolCall::new(id, "shell", json!({ "command": command })));
        tool.append_tool_output("done\n");
        app.messages.push(tool);
    }

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染工具间距");
    let rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();

    let first = rows
        .iter()
        .position(|row| row.contains("cargo build"))
        .expect("应展示第一次调用");
    let second = rows
        .iter()
        .position(|row| row.contains("cargo test"))
        .expect("应展示第二次调用");
    assert!(second > first + 1, "两次调用之间应隔一行：{rows:?}");
    assert!(
        rows[second - 1].trim().is_empty(),
        "两次调用之间应是空行：{rows:?}"
    );
}

/// 输入框上方始终留一行空白，日志与输入盒之间不再紧贴。
#[test]
fn prompt_is_separated_from_log_by_blank_row() {
    let width = 80;
    let backend = TestBackend::new(width, 18);
    let mut terminal = Terminal::new(backend).expect("创建输入留白测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    // 填满视口：日志末尾贴着消息区底部，留白行才是唯一的分隔。
    app.messages
        .push(Msg::new(MsgKind::Assistant, "日志行\n".repeat(40)));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染输入留白");
    let rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();

    let border_row = prompt_border_row(&rows);
    assert!(border_row >= 2, "{rows:?}");
    assert_eq!(
        rows[border_row - 1].trim(),
        "",
        "输入框上方应有一行空白：{rows:?}"
    );
    assert!(
        !rows[border_row - 2].trim().is_empty(),
        "留白只占一行，再往上应仍有内容：{rows:?}"
    );
}

/// 输入框只保留上下两条边框：没有左右竖线，也没有圆角。
#[test]
fn prompt_box_has_only_horizontal_borders() {
    let width = 60;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建输入框边框测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染输入框边框");
    let rows = buffer_rows(&terminal, width);

    for corner in ['╭', '╮', '╰', '╯', '│'] {
        assert!(
            !rows.iter().any(|row| row.contains(corner)),
            "输入框不应再出现 `{corner}`：{rows:?}"
        );
    }

    // 上下两条边框都是一整排横线，直接铺满左右边缘。
    let border_rows: Vec<&String> = rows
        .iter()
        .filter(|row| row.starts_with('─') && row.ends_with('─'))
        .collect();
    assert_eq!(border_rows.len(), 2, "应只有上下两条边框：{rows:?}");
    assert!(
        border_rows
            .iter()
            .all(|row| row.chars().filter(|c| *c == '─').count() > usize::from(width / 2)),
        "边框应铺满整行：{rows:?}"
    );
}

/// 长文本在输入框内自动折行，框高随之增长而不是把尾部挤出屏幕。
#[test]
fn long_prompt_wraps_instead_of_scrolling() {
    let width = 40;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建长文本折行终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    // 不含空格的长词：折行只能按字符硬断，拼接结果必须与输入完全一致，才能证明
    // 尾部没有被截断。内容宽度为 38 列，因此正好折成两行。
    let token: String = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMN"
        .chars()
        .take(50)
        .collect();
    app.input = token.clone();
    app.cursor = app.input.len();

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染长文本折行");
    let rows = buffer_rows(&terminal, width);

    let content_row = rows
        .iter()
        .position(|row| row.starts_with("› "))
        .expect("输入框内容行应存在");
    // 逐个内容行剥掉提示符与续行缩进，拼回来必须还原原始输入。
    let mut restored = String::new();
    for row in &rows[content_row..] {
        if row.starts_with('─') {
            break;
        }
        let body = row.strip_prefix("› ").or_else(|| row.strip_prefix("  "));
        restored.push_str(body.unwrap_or(row).trim_end());
    }
    assert_eq!(restored, token, "折行不得丢字符：{rows:?}");
    assert_eq!(
        rows[content_row..]
            .iter()
            .take_while(|row| !row.starts_with('─'))
            .count(),
        2,
        "50 列内容应折成两行：{rows:?}"
    );
}

/// 光标跟随折行后的视觉行：长文本行尾的光标落在最后一个内容行。
#[test]
fn long_prompt_cursor_follows_wrapped_row() {
    let width = 40;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建折行光标终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let token: String = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMN".into();
    app.input = token.clone();
    app.cursor = app.input.len();

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染折行光标");
    let rows = buffer_rows(&terminal, width);
    let content_row = rows
        .iter()
        .position(|row| row.starts_with("› "))
        .expect("输入框内容行应存在");

    let cursor = terminal.get_cursor_position().expect("读取光标位置");
    assert_eq!(
        cursor.y,
        u16::try_from(content_row + 1).expect("内容行号应可转换"),
        "光标应落到第二个内容行：{rows:?}"
    );
    // 内容从区域左边界起算，「› 」占两列，第二行承载剩余的 12 个字符。
    assert_eq!(cursor.x, 2 + 12, "光标列应跟随折行后的列偏移");
}

/// 发送后输入框必须清空并回到占位提示，不能留下上一帧的草稿。
///
/// 断言只看输入框内容行：发送的正文会出现在消息区，因此整屏文本里当然还能找到它，
/// 真正要保证的是输入区自己没有残留。缓冲里宽字符的第二格是空白占位，
/// 因此比较前先去掉所有空白。
#[tokio::test]
async fn sending_clears_prompt_back_to_placeholder() {
    let width = 50;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建发送清空测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    let draft = "确认发送后会残留草稿";

    app.input = draft.into();
    app.cursor = app.input.len();
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染发送前草稿");
    let rows = buffer_rows(&terminal, width);
    let content = rows
        .iter()
        .find(|row| row.starts_with("› "))
        .expect("输入框内容行应存在");
    assert!(
        compact(content).contains(&compact(draft)),
        "发送前输入框应显示草稿：{content:?}"
    );

    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert!(app.input.is_empty(), "发送后必须清空输入");
    assert_eq!(app.cursor, 0, "光标应回到行首");
    assert!(app.attachments.is_empty());
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染发送后输入框");
    let rows = buffer_rows(&terminal, width);
    let content = rows
        .iter()
        .find(|row| row.starts_with("› "))
        .expect("输入框内容行应存在");
    assert!(
        content.contains("Message Lucia...") || content.contains("Steer the current run..."),
        "输入框应回到占位提示：{content:?}"
    );
    assert!(
        !compact(content).contains(&compact(draft)),
        "输入框不得残留已发送的草稿：{content:?}"
    );
}

/// 长草稿发送后输入盒收缩回单行，不能保留折行撑开的高度。
#[tokio::test]
async fn sending_collapses_wrapped_prompt_back_to_one_row() {
    let width = 40;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建发送收缩测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    // 50 列内容在 38 列里必然折成两行。
    app.input = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMN".into();
    app.cursor = app.input.len();
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染折行草稿");
    let rows = buffer_rows(&terminal, width);
    assert_eq!(
        prompt_content_rows(&rows, width),
        2,
        "50 列草稿应折成两行：{rows:?}"
    );

    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染发送后输入框");
    let rows = buffer_rows(&terminal, width);
    assert_eq!(
        prompt_content_rows(&rows, width),
        1,
        "发送后输入盒应收缩为一行：{rows:?}"
    );
}

/// 引用块渲染为独立卡片：整行铺代码底色，并在卡片内部左右各留余量。
#[test]
fn quote_blocks_render_as_cards() {
    let width = 40;
    let lines = markdown_lines("> 引用内容", false, width);
    let quote = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("引用内容"))
        })
        .expect("应包含引用行");

    assert!(
        quote
            .spans
            .iter()
            .any(|span| span.style.bg == Some(crate::theme::COLOR_QUOTE_BG)),
        "引用行应铺卡片底色"
    );
    // 左内边距两列 + 消息标记两列之外，右侧还要留两列卡片底色。
    assert_eq!(
        quote.width(),
        usize::from(width) - 2,
        "引用卡片应铺满正文列并留下右内边距"
    );
}

/// 代码块卡片内部上下各留一行只铺底色的内边距。
#[test]
fn code_blocks_have_card_padding_rows() {
    let lines = markdown_lines("```rust\nfn main() {}\n```", false, 40);
    let padding_rows = lines
        .iter()
        .filter(|line| {
            let body = card_body_spans(line);
            body.iter().all(|span| {
                let text = span.content.trim();
                text.is_empty() || text == "▎"
            }) && body
                .iter()
                .any(|span| span.style.bg == Some(crate::theme::COLOR_CODE_BG))
        })
        .count();

    assert!(
        padding_rows >= 2,
        "代码卡片上下应各有一行内边距：{padding_rows}"
    );
}

/// ```diff 围栏块按行语义着色：增删行整行换底色，文件头与 hunk 头加粗且不铺底色。
///
/// Markdown 与工具预览共用 [`crate::ui::diff`] 的分类，因此同一个补丁在两处呈现一致。
#[test]
fn diff_fence_colors_rows_by_meaning() {
    let lines = markdown_lines(
        "```diff\ndiff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-let old = 1;\n+let new = 2;\n context();\n```",
        false,
        60,
    );

    let find = |needle: &str| {
        lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains(needle)))
            .unwrap_or_else(|| panic!("应包含 {needle} 行"))
    };
    // 正体片段才铺 diff/代码底色：首尾的内边距属于回答卡片本身。
    let card_backgrounds = |line: &Line<'_>| {
        card_body_spans(line)
            .iter()
            .map(|span| span.style.bg)
            .collect::<Vec<_>>()
    };

    // 文件头与 hunk 头：加粗的结构色，底色仍是卡片底色而不是红绿。
    for (needle, color) in [
        ("diff --git", crate::theme::COLOR_DIFF_FILE_HEADER),
        ("@@ -1,2", crate::theme::COLOR_DIFF_HUNK),
    ] {
        let line = find(needle);
        let span = line
            .spans
            .iter()
            .find(|span| span.content.contains(needle))
            .expect("应包含结构文本");
        assert_eq!(span.style.fg, Some(color), "{needle}");
        assert!(span.style.add_modifier.contains(Modifier::BOLD), "{needle}");
        assert!(card_backgrounds(line)
            .iter()
            .all(|bg| *bg == Some(crate::theme::COLOR_CODE_BG)));
    }

    // 增删行：整行（含左侧竖条与尾部留白）换成 diff 底色。
    let removed = find("-let old = 1;");
    assert!(card_backgrounds(removed)
        .iter()
        .all(|bg| *bg == Some(crate::theme::COLOR_DIFF_DEL_BG)));
    assert!(removed
        .spans
        .iter()
        .any(|span| span.content.contains("let old = 1;")
            && span.style.fg == Some(crate::theme::COLOR_DIFF_DEL_FG)));

    let added = find("+let new = 2;");
    assert!(card_backgrounds(added)
        .iter()
        .all(|bg| *bg == Some(crate::theme::COLOR_DIFF_ADD_BG)));
    assert!(added
        .spans
        .iter()
        .any(|span| span.content.contains("let new = 2;")
            && span.style.fg == Some(crate::theme::COLOR_DIFF_ADD_FG)));

    // 上下文行保持卡片底色与正文色，不参与红绿。
    let context = find(" context();");
    assert!(card_backgrounds(context)
        .iter()
        .all(|bg| *bg == Some(crate::theme::COLOR_CODE_BG)));
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

/// 把终端缓冲区按行切分，保留框线字符以便定位输入盒边框。
fn buffer_rows(terminal: &Terminal<TestBackend>, width: u16) -> Vec<String> {
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

/// 定位输入盒上边框所在行。
///
/// 输入盒只保留上下两条边框，两行都是一整排 `─`，因此不能按边框字符判断方向；
/// 改用内容行的「› 」提示符做锚点：上边框就在首个内容行的上一行。
fn prompt_border_row(rows: &[String]) -> usize {
    let content = rows
        .iter()
        .position(|row| row.starts_with("› "))
        .expect("输入盒内容行应存在");
    content.saturating_sub(1)
}

/// 定位输入框上方那行固定状态行：跳过排队消息面板后紧贴输入盒上边框的上一行。
fn status_row(rows: &[String]) -> usize {
    let mut row = prompt_border_row(rows);
    while row > 0 && matches!(rows[row - 1].trim_start().chars().next(), Some('↳' | '▸')) {
        row -= 1;
    }
    row.saturating_sub(1)
}

/// 统计输入盒里有几行内容：从「› 」行起，直到下一条横边框为止。
///
/// 两条边框都是纯框线，但仍然只认"横线占满大半行"的那两行，避免把正文里偶然
/// 以 `─` 开头的行误判成边框。
fn prompt_content_rows(rows: &[String], width: u16) -> usize {
    let threshold = usize::from(width / 2);
    let is_border =
        |row: &String| row.chars().filter(|character| *character == '─').count() > threshold;
    let start = rows
        .iter()
        .position(|row| row.starts_with("› "))
        .expect("输入盒内容行应存在");
    rows[start..]
        .iter()
        .take_while(|row| !is_border(row))
        .count()
}

/// 去掉所有空白，用于比较被宽字符占位拆开的行文本。
fn compact(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// 验证运行状态（含耗时与停止提示）只落在输入框上方的状态行，日志区与边框都不重复输出，
/// 同时窄终端仍隐藏工作目录。
#[test]
fn running_activity_renders_on_the_fixed_status_row_only() {
    let width = 60;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建运行状态测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.messages.extend([
        Msg::new(MsgKind::User, "测试消息"),
        Msg::new(MsgKind::Assistant, "测试回复"),
    ]);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染运行状态");
    let rows = buffer_rows(&terminal, width);
    let status = status_row(&rows);
    let text = &rows[status];

    assert!(text.contains("Working"), "{text:?}");
    assert!(text.contains("Esc to stop"), "{text:?}");
    // 状态从第 1 列开始，与正文左边界共线：行首是动画星号而不是空格。
    let first = text.chars().next().expect("状态行不应为空");
    assert!("✳✶✷✸✹✺✻✼".contains(first), "{text:?}");
    assert!(
        rows[..status]
            .iter()
            .all(|row| !row.contains("Working") && !row.contains("Esc to stop")),
        "日志区不再输出活动状态：{rows:?}"
    );
    // 输入盒边框保持纯框线：状态不再占用边框标题。
    let border = &rows[prompt_border_row(&rows)];
    assert!(
        border
            .chars()
            .all(|character| character == '─' || character == ' '),
        "边框不应带状态标题：{border:?}"
    );
    let text: String = rows
        .concat()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    assert!(text.contains("Steerthecurrentrun..."), "{text:?}");
    assert!(!text.contains("ascnet-lucia"), "{text:?}");
}

/// 状态行中某个片段的单元格颜色，用于验证语义配色。
fn status_text_color(terminal: &Terminal<TestBackend>, rows: &[String], needle: &str) -> Color {
    let row = status_row(rows);
    let x = rows[row].find(needle).expect("状态片段应存在") as u16;
    terminal.backend().buffer()[(x, row as u16)].fg
}

/// 后台上下文压缩不需要 Agent 运行中：状态行显示调用方标签，且不出现运行态尾巴。
#[test]
fn maintenance_activity_shows_label_without_run_suffix() {
    let width = 60;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建维护状态测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.pending_reload = Some("Compacting context".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染维护状态");
    let rows = buffer_rows(&terminal, width);
    let status = &rows[status_row(&rows)];

    assert!(status.contains("Compacting context"), "{status:?}");
    assert!(
        !status.contains("Esc to stop"),
        "压缩不是可中断的 Agent 运行：{status:?}"
    );
}

/// 动画帧随 UI tick 推进：相邻两格必须给出不同的星号，否则状态行是死画面。
#[test]
fn activity_spark_advances_with_tick() {
    let width = 60;
    let mut terminal = Terminal::new(TestBackend::new(width, 16)).expect("创建动画测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.mark_model_thinking();

    let mut frames = Vec::new();
    for frame in [0, 2] {
        app.spinner_frame = frame;
        terminal
            .draw(|frame| render_root(frame, &mut app))
            .expect("渲染动画帧");
        let rows = buffer_rows(&terminal, width);
        let status = rows[status_row(&rows)].clone();
        assert!(status.contains("Thinking"), "{status:?}");
        frames.push(status);
    }

    assert_ne!(frames[0], frames[1], "两帧状态应不同：{frames:?}");
}

/// 超过一分钟的耗时折算成 `1m05s`，避免状态行随秒数无限变宽。
#[test]
fn activity_elapsed_switches_to_minutes() {
    let width = 60;
    let mut terminal = Terminal::new(TestBackend::new(width, 16)).expect("创建耗时测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.run_started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(125));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染耗时状态");
    let rows = buffer_rows(&terminal, width);
    let status = &rows[status_row(&rows)];

    assert!(status.contains("2m05s"), "{status:?}");
}

/// 变窄时状态先按宽度截断，窄到只剩碎片则整段隐藏，保持一行留白或只显示跳转提示。
#[test]
fn activity_status_degrades_on_narrow_rows() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.run_started_at = Some(std::time::Instant::now());

    // 中等宽度：状态放不下完整文案，按显示宽度截断并留下省略号。
    let width = 24;
    let mut terminal = Terminal::new(TestBackend::new(width, 16)).expect("创建窄状态行测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染窄状态行");
    let rows = buffer_rows(&terminal, width);
    let status = &rows[status_row(&rows)];
    assert!(
        status.contains('…'),
        "窄状态行应截断而不是撑破行宽：{status:?}"
    );

    // 极窄宽度：可用宽度不足一行有意义的文字，状态整段隐藏，边框仍是纯框线。
    let width = 10;
    let mut terminal = Terminal::new(TestBackend::new(width, 16)).expect("创建极窄状态行测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染极窄状态行");
    let rows = buffer_rows(&terminal, width);
    assert_eq!(rows[status_row(&rows)].trim(), "", "过窄时状态行应保持留白");
    let border = &rows[prompt_border_row(&rows)];
    assert!(
        border
            .chars()
            .all(|character| character == '─' || character == ' '),
        "边框只应有框线：{border:?}"
    );
}

/// 工具执行期间状态行只显示通用运行状态，不暴露工具名、命令或单次调用计时。
#[test]
fn tool_activity_falls_back_to_generic_working_title() {
    let width = 72;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建工具状态测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.messages.push(Msg::tool_started(ToolCall::new(
        "call-running",
        "shell",
        json!({ "command": "cargo test" }),
    )));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染工具状态");
    let rows = buffer_rows(&terminal, width);
    let status = &rows[status_row(&rows)];

    assert!(status.contains("Working"), "{status:?}");
    assert!(status.contains("Esc to stop"), "{status:?}");
    // 工具名、命令与思考文案都不该出现在状态行。
    assert!(!status.contains("Shell"), "{status:?}");
    assert!(!status.contains("cargo test"), "{status:?}");
    assert!(!status.contains("Thinking"), "{status:?}");
    assert_eq!(
        status_text_color(&terminal, &rows, "Working"),
        crate::theme::COLOR_WARNING,
        "通用运行状态使用警示色：{status:?}"
    );
    // 工具结果仍然落在主内容区，只是不再进状态行。
    let transcript = rows[..status_row(&rows)].concat();
    assert!(transcript.contains("Shell(cargo test)"), "{transcript:?}");
}

/// 空闲时状态行与输入框边框都保持干净：边框是纯框线，上方只剩留白。
#[test]
fn idle_status_row_and_prompt_border_are_blank() {
    let width = 60;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建空闲状态测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.extend([
        Msg::new(MsgKind::User, "测试消息"),
        Msg::new(MsgKind::Assistant, "测试回复"),
    ]);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染空闲状态");
    let rows = buffer_rows(&terminal, width);
    let status = &rows[status_row(&rows)];
    let border = &rows[prompt_border_row(&rows)];

    assert_eq!(status.trim(), "", "空闲时状态行不应有内容：{status:?}");
    assert!(
        border
            .chars()
            .all(|character| character == '─' || character == ' '),
        "空闲时边框只应有框线：{border:?}"
    );
}

/// 活动状态只取唯一最高优先级的一项，供状态行直接使用。
#[test]
fn prompt_activity_reports_single_highest_priority_state() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    assert_eq!(prompt_activity(&app), None, "空闲时没有活动状态");

    app.queued_inputs.push_back("排队输入".into());
    assert_eq!(prompt_activity(&app), Some(PromptActivity::Queued(1)));

    app.running = true;
    app.pending_reload = Some("Reloading context".into());
    assert_eq!(
        prompt_activity(&app),
        Some(PromptActivity::Maintenance("Reloading context".into())),
        "后台维护优先于运行状态"
    );

    app.pending_reload = None;
    assert_eq!(prompt_activity(&app), Some(PromptActivity::Working));

    app.messages.push(Msg::tool_started(ToolCall::new(
        "call-running",
        "shell",
        json!({ "command": "cargo test" }),
    )));
    assert_eq!(
        prompt_activity(&app),
        Some(PromptActivity::Working),
        "工具执行折叠为通用运行状态，不携带工具名"
    );

    // 正文已经半截流式落地时，工具仍在跑也继续显示通用状态：活动可见性优先。
    app.start_model_response();
    assert_eq!(
        prompt_activity(&app),
        Some(PromptActivity::Working),
        "流式正文与工具并行时仍显示运行状态"
    );

    app.mark_model_thinking();
    assert_eq!(
        prompt_activity(&app),
        Some(PromptActivity::Thinking),
        "思考阶段优先于工具执行"
    );

    app.last_exit_request_at = Some(std::time::Instant::now());
    assert_eq!(
        prompt_activity(&app),
        Some(PromptActivity::ExitConfirm),
        "退出确认优先于一切活动状态"
    );
}

/// 待发送消息显示在输入框上方，并登记可点击的编辑热区。
#[test]
fn queued_messages_render_above_input() {
    let width = 80;
    let mut terminal = Terminal::new(TestBackend::new(width, 20)).expect("创建排队渲染终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs.push_back("第一条排队消息".into());
    app.queued_inputs.push_back("第二条排队消息".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染排队消息");

    let rows = buffer_rows(&terminal, width);
    let border = prompt_border_row(&rows);
    // 宽字符在缓冲区里占两格，去掉填充空格后再比对文本。
    let first = rows[border - 2].replace(' ', "");
    let second = rows[border - 1].replace(' ', "");
    assert!(
        first.contains("第一条排队消息"),
        "第一条应紧贴输入框上方的可视区顶部：{:?}",
        &rows[border - 3..=border]
    );
    assert!(
        second.contains("第二条排队消息"),
        "第二条应在输入框正上方：{:?}",
        &rows[border - 3..=border]
    );
    assert_eq!(app.queued_input_areas.len(), 2);
    assert_eq!(app.queued_input_areas[0].1.y, (border - 2) as u16);
    assert_eq!(app.queued_input_areas[1].1.y, (border - 1) as u16);
}

/// 排队面板聚焦时高亮选中行；超过可视高度时窗口跟随选择滚动。
#[test]
fn queue_focus_highlights_selection_and_scrolls() {
    let width = 80;
    let mut terminal = Terminal::new(TestBackend::new(width, 20)).expect("创建排队焦点终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.queued_inputs = VecDeque::from([
        "第一条".into(),
        "第二条".into(),
        "第三条".into(),
        "第四条".into(),
    ]);
    app.queue_focus = Some(3);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染排队焦点");

    let rows = buffer_rows(&terminal, width);
    let border = prompt_border_row(&rows);
    let visible: Vec<String> = (border - 3..border)
        .map(|row| rows[row].replace(' ', ""))
        .collect();
    // 面板高度 3；选中第 4 条时窗口滚到第 2~4 条，最后一行带选中标记。
    assert!(visible[0].contains("2.第二条"), "{visible:?}");
    assert!(visible[1].contains("3.第三条"), "{visible:?}");
    assert!(
        visible[2].contains("▸4.第四条"),
        "选中行应有 `▸` 标记：{visible:?}"
    );
    assert_eq!(
        app.queued_input_areas
            .iter()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let status = rows[status_row(&rows)].replace(' ', "");
    assert!(
        status.contains("Queue4/4"),
        "状态行应显示队列位置：{status:?}"
    );
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
    let rows = buffer_rows(&terminal, 80);
    let row = status_row(&rows);
    let status = &rows[row];
    assert!(status.contains("Thinking"), "{status:?}");
    assert!(status.contains("Esc to stop"), "{status:?}");
    assert!(!status.contains("Working"), "{status:?}");
    // 推理摘要文本不得进入任何一行，包括状态行与正文。
    let text = rows.concat();
    assert!(!text.contains("thinking("), "{text:?}");
    // 思考状态只出现在这一行：日志区与输入盒边框都不重复。
    assert_eq!(
        rows.iter().filter(|row| row.contains("Thinking")).count(),
        1,
        "思考状态只应出现在状态行：{rows:?}"
    );
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

/// 相邻工具结果之间、工具组与后续回复之间都保留一行空行。
///
/// 每次调用都带一段自己的输出，紧贴着排会让人分不清哪几行属于哪一次调用；工具组与
/// 助手正文之间同样要留白，否则 Shell 输出与随后的分析会糊成一片。
#[test]
fn tool_results_are_separated_by_blank_rows() {
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
        .expect("渲染工具组间距");
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
    assert_eq!(rows[tool_rows[0] + 1].trim(), "", "{rows:?}");
    assert_eq!(reply_row, tool_rows[1] + 2, "{rows:?}");
    assert_eq!(rows[reply_row - 1].trim(), "", "{rows:?}");
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
    // 选区必须真的覆盖到文字：视口首行可能是只有缩进的省略提示行。
    for (kind, column) in [
        (
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            area.x,
        ),
        (
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            area.x + 8,
        ),
        (
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            area.x + 8,
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

/// 拼接一组渲染行的可读文本，供 Markdown 断言使用。
fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

/// 取出回答卡片内部的片段（代码卡片、引用卡片、表格行等）。
///
/// 卡片化之后每一行都以「左侧两列内边距 + 消息标记」开头，之后才是正体；卡片底色
/// 由绘制层整块铺出，因此正体片段里只会出现块级结构自己的色块。
fn card_body_spans<'a>(line: &'a Line<'a>) -> &'a [Span<'a>] {
    &line.spans[2.min(line.spans.len())..]
}

/// 验证 Markdown 表格排版为对齐行：分隔行转为横线，中文列按显示宽度补齐。
#[test]
fn markdown_tables_render_aligned() {
    let width = 60;
    let lines = markdown_lines(
        "说明\n\n| 模块 | 作用 |\n|---|---|\n| core | Agent 循环 |\n| tool | 工具注册 |\n\n结尾",
        false,
        width,
    );
    let rows = line_texts(&lines);
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
    // 表格必须整体收进消息区，任何一行都不能越界。
    for line in &lines {
        assert!(line.width() <= usize::from(width), "{line:?}");
    }
}

/// 窄终端下表格压缩列宽并在单元格内折行，竖线位置仍然对齐、不出现字面分隔行。
#[test]
fn narrow_markdown_table_wraps_inside_columns() {
    let width = 42;
    let lines = markdown_lines(
        "| 问题 | 表现 | 修法 |\n|---|---|---|\n| 光标越界 | 粘贴长路径时触发 panic，因为光标落在输入长度之外 | clamp 到输入长度内 |",
        false,
        width,
    );
    let rows = line_texts(&lines);
    let text = rows.join("\n");

    assert!(!text.contains("|---"), "{text:?}");
    assert!(text.contains("光标越界"), "{text:?}");
    for line in &lines {
        assert!(
            line.width() <= usize::from(width),
            "行宽越界：{:?}",
            line_texts(std::slice::from_ref(line))
        );
    }
    // 折行后各行仍共用同一组竖线列，单元格不会顶过竖线。
    let columns: Vec<Vec<usize>> = rows
        .iter()
        .filter(|row| !row.trim().is_empty() && !row.contains('┼'))
        .map(|row| {
            row.char_indices()
                .filter(|(_, ch)| *ch == '│')
                .map(|(index, _)| unicode_width::UnicodeWidthStr::width(&row[..index]))
                .collect()
        })
        .collect();
    assert!(columns.len() > 3, "{text:?}");
    assert!(
        columns.windows(2).all(|pair| pair[0] == pair[1]),
        "竖线列不一致：{columns:?}\n{text}"
    );
}

/// 验证 Markdown 渲染保留标题与行内代码的强调样式，且正文不自己铺底色。
///
/// 卡片底色由绘制层整块铺出（`render_card`），正文片段只带前景色：若这里也给
/// 片段刷底色，空行与行尾留白就会变成"只有空格的行"，段落间距会被算成两行。
#[test]
fn markdown_renders_emphasis_inside_the_answer_card() {
    let lines = markdown_lines("# 标题\n\n**重点** 与 `代码` 内容", false, 60);

    let no_line_background = lines.iter().all(|line| line.style.bg.is_none());
    assert!(no_line_background);
    assert!(lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .all(|span| span.style.bg.is_none()));

    let heading = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.contains("标题")))
        .expect("应包含标题行");
    assert!(
        heading
            .spans
            .iter()
            .filter(|span| span.content.contains("标题"))
            .all(|span| span.style.add_modifier.contains(Modifier::BOLD)),
        "标题正文应加粗"
    );

    let bold_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("重点"))
        .expect("应包含加粗片段");
    assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
}

/// 回答卡片的 `●` 与用户消息的 `▌` 同列，右侧留白交给整块底色。
#[test]
fn assistant_card_aligns_with_the_user_marker_and_leaves_room_on_the_right() {
    let width = 60;
    let lines = markdown_lines("卡片正文", false, width);
    let body = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("卡片正文"))
        })
        .expect("应包含正文行");

    assert_eq!(body.spans[0].content, "● ", "标记应贴住第一列");
    assert_eq!(
        body.spans[0].style.bg, None,
        "标记不自己铺色：底色由卡片矩形统一铺出"
    );
    // 卡片宽度 = 60，标记两列 + 右侧留白两列，正文最多排到第 58 列。
    assert!(
        body.width() <= usize::from(width) - crate::ui::markdown::CARD_PADDING,
        "正文不应顶到卡片右边缘：{}",
        body.width()
    );
}

/// 段落正文使用明细灰而非纯白，强调片段保留自己的前景色。
#[test]
fn prose_body_uses_soft_gray() {
    let lines = markdown_lines("普通说明文字与 **重点** 内容", false, 80);
    let body = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("普通说明文字"))
        .expect("应包含正文片段");
    assert_eq!(body.style.fg, Some(crate::theme::COLOR_TEXT_SOFT));
    assert!(!body.style.add_modifier.contains(Modifier::BOLD));
}

/// 正文必须用满可用宽度：不存在任何阅读宽度上限，也不做居中。
///
/// 这条断言覆盖的正是"两侧留出夸张空白"的回归：一旦有人再引入宽度上限，长段落就会
/// 提前折行、右侧留下空白带。
#[test]
fn prose_uses_the_full_available_width() {
    let text = "字".repeat(400);
    let lines = markdown_lines(&text, false, 200);
    let longest = lines.iter().map(|line| line.width()).max().unwrap_or(0);

    // 行宽 = 可用宽度 - 卡片右侧内边距：左侧内边距与行首标记已含在行内容里，
    // 因此唯一余下的空白就是那 2 列对称边距，没有额外窄列。
    assert_eq!(
        longest,
        200 - crate::ui::markdown::CARD_PADDING,
        "正文行应铺满可用宽度，只在右侧留出与左侧对称的内边距"
    );
    // 代码块与正文同宽：两者都吃满整行，不存在"正文列"与"代码列"两套宽度。
    let code = markdown_lines("```rust\nlet value = 1;\n```", false, 200);
    let widest_code = code.iter().map(|line| line.width()).max().unwrap_or(0);
    assert_eq!(widest_code, longest, "代码块应与正文同宽");
}

/// 表格列宽随终端宽度伸展：宽终端上长测试用例名不再被挤成碎块。
///
/// 表格是最容易暴露"宽度被限制"的结构：一旦可用宽度被压到百来列，一行的用例名会被
/// 折成三段，读者无法看出这是一条还是三条结果。
#[test]
fn table_columns_expand_with_available_width() {
    const CASE: &str = "renders_a_very_long_test_case_name_without_breaking_the_columns";
    let table = format!("| 测试 | 结果 |\n| --- | --- |\n| {CASE} | ok |\n");
    let narrow = line_texts(&markdown_lines(&table, false, 60));
    let wide = line_texts(&markdown_lines(&table, false, 180));
    let widest = |rows: &[String]| {
        rows.iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0)
    };

    assert!(
        widest(&wide) > widest(&narrow),
        "表格应随终端变宽：{} → {}",
        widest(&narrow),
        widest(&wide)
    );
    // 宽终端上用例名必须完整落在同一行里，这是"没被挤碎"的直接证据。
    assert!(
        wide.iter().any(|row| row.contains(CASE)),
        "宽终端上用例名不应折行：{wide:#?}"
    );
    // 窄终端允许折行，但必须用满窄宽度，不能缩进到小盒子里。
    assert!(
        !narrow.iter().any(|row| row.contains(CASE)),
        "窄终端上用例名应当折行：{narrow:#?}"
    );
    assert_eq!(
        widest(&narrow),
        60 - crate::ui::markdown::CARD_PADDING,
        "窄终端上表格也要占满可用宽度"
    );
}

/// 行内代码去掉反引号、改用代码色，并保持与正文一致的背景。
#[test]
fn inline_code_drops_backticks_and_gains_chip_background() {
    let lines = markdown_lines("调用 `convert_input_path_to_attachment` 转换", false, 80);
    let text = line_texts(&lines).join("\n");

    assert!(
        text.contains("convert_input_path_to_attachment"),
        "{text:?}"
    );
    assert!(!text.contains('`'), "行内代码不应显示反引号：{text:?}");
    let span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "convert_input_path_to_attachment")
        .expect("应包含行内代码片段");
    assert_eq!(span.style.fg, Some(crate::theme::COLOR_CODE));
    assert_eq!(span.style.bg, None, "行内代码不应铺背景色");
}

/// 标题不再输出字面 `#`：层级靠标记块形状与文字权重区分。
#[test]
fn markdown_headings_render_without_hash_marks() {
    let lines = markdown_lines("# 一级\n\n## 二级\n\n### 三级", false, 60);
    let rows = line_texts(&lines);
    let text = rows.join("\n");

    assert!(!text.contains('#'), "标题不应残留 `#`：{text:?}");
    assert!(rows[0].contains("■ 一级"), "{text:?}");
    let second = rows
        .iter()
        .find(|row| row.contains("二级"))
        .expect("二级标题");
    let third = rows
        .iter()
        .find(|row| row.contains("三级"))
        .expect("三级标题");
    // 层级越深方块越小：一级实心方块、二级小方块、三级空心方块。
    assert!(second.contains("▪ 二级"), "{text:?}");
    assert!(third.contains("▫ 三级"), "{text:?}");
    let first_mark = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "■ ")
        .and_then(|span| span.style.fg);
    assert_eq!(first_mark, Some(crate::theme::COLOR_USER));
}

/// 列表项渲染为圆点并带悬挂缩进：续行对齐正文首列，而不是回到行首。
#[test]
fn markdown_lists_use_bullets_with_hanging_indent() {
    let lines = markdown_lines(
        "- 第一项内容足够长，用来检查折行后的续行是否对齐到正文列而不是行首\n- 第二项",
        false,
        36,
    );
    let rows = line_texts(&lines);
    let text = rows.join("\n");

    // 行首标记两列，圆点正文从第三列开始。
    assert!(rows[0].starts_with("● • 第一项"), "{text:?}");
    assert!(!text.contains("- 第一项"), "列表标记不应残留字面 `- `");
    let continuation = rows.get(1).expect("第一项应折行产生续行").clone();
    // 消息前缀两列 + 悬挂缩进两列，正文从第五列开始。
    assert!(continuation.starts_with("    "), "{text:?}");
    assert!(!continuation.starts_with("     "), "{text:?}");
    assert!(rows.iter().any(|row| row.contains("• 第二项")), "{text:?}");
}

/// 验证围栏代码块隐藏围栏、保留语言标签、整行铺底色，并且不干扰相邻表格。
#[test]
fn markdown_code_blocks_are_visually_separated() {
    let lines = markdown_lines(
        "```rust\nfn main() {}\n```\n| 列一 | 列二 |\n|---|---|\n| 值一 | 值二 |\n```txt\n未闭合",
        false,
        60,
    );
    let rows = line_texts(&lines);
    let text = rows.join("\n");

    assert!(!text.contains("```"), "{text:?}");
    assert!(text.contains("rust"), "{text:?}");
    assert!(text.contains("fn main() {}"), "{text:?}");
    assert!(text.contains('┼'), "{text:?}");
    assert!(text.contains("未闭合"), "{text:?}");

    // 代码行整行铺代码底色（含尾部补齐），正体之外只有回答卡片自己的内边距；
    // 正文与表格行只铺卡片底色，不出现代码色块。
    let code = rows
        .iter()
        .position(|row| row.contains("fn main()"))
        .expect("代码行");
    assert!(
        card_body_spans(&lines[code])
            .iter()
            .all(|span| span.style.bg == Some(crate::theme::COLOR_CODE_BG)),
        "{:?}",
        rows[code]
    );
    // 底色必须铺满正文列，否则代码块右边缘会缺一块。
    assert_eq!(
        lines[code].width(),
        58,
        "代码行应铺满卡片的正文列（60 列卡片扣除两侧各 2 列内边距）"
    );
    let prose = rows
        .iter()
        .position(|row| row.contains("列一"))
        .expect("表格行");
    assert!(lines[prose]
        .spans
        .iter()
        .all(|span| span.style.bg != Some(crate::theme::COLOR_CODE_BG)));
}

/// 极窄终端下 Markdown 渲染不得 panic：列宽与折行宽度都会被压到下界。
#[test]
fn markdown_survives_very_narrow_width() {
    let body = "## 标题\n\n- 列表项内容很长需要折行\n\n| 甲 | 乙 |\n|---|---|\n| 一 | 二 |\n\n```rust\nfn main() {}\n```\n\n> 引用";
    for width in [6u16, 10, 16, 24] {
        let lines = markdown_lines(body, true, width);
        assert!(!lines.is_empty(), "width={width}");
    }
}

/// 流式光标跟在代码块最后一行代码之后，不会被卡片内边距或整行底色挤到别处。
#[test]
fn streaming_cursor_stays_on_last_code_line() {
    let width = 40;
    let lines = markdown_lines("```rust\nfn main() {}\n```", true, width);
    // 代码卡片底部的内边距行只铺底色与竖条，不算"内容行"。
    let last = lines
        .iter()
        .rev()
        .find(|line| {
            line.spans.iter().any(|span| {
                span.content
                    .chars()
                    .any(|ch| !ch.is_whitespace() && !matches!(ch, '▎' | '▏'))
            })
        })
        .expect("应有内容行");
    let text: String = last
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert!(text.trim_end().ends_with('▌'), "{text:?}");
    assert!(text.contains("fn main() {}"), "{text:?}");
    assert!(
        last.width() <= usize::from(width),
        "光标不应撑出消息区：{}",
        last.width()
    );
}

/// 拖选高亮只修改单元格样式，不得改写可复制的字符内容。
///
/// 选区存内容行号，绘制时按当前滚动偏移换算成屏幕行。
#[test]
fn mouse_text_selection_renders_reversed_cells() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.content_area = Some(Rect::new(0, 0, 4, 1));
    app.content_rows = 1;
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

/// 向长消息区域发送一次鼠标事件。
fn send_mouse(app: &mut App, buffer: &Buffer, kind: MouseEventKind, column: u16, row: u16) {
    app.handle_text_selection(
        &MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        buffer,
    );
}

/// 构造一条足够长的助手回复，每行都带可检索的标记。
fn long_assistant_reply(lines: usize) -> Msg {
    let mut text = String::new();
    for index in 0..lines {
        text.push_str(&format!("MARK-{index:02}\n"));
    }
    Msg::new(MsgKind::Assistant, text)
}

/// 向下拖动越过内容区底部时必须自动滚动，并且选区继续向屏幕外延伸。
///
/// 这是"选中一屏以上内容再复制"的核心：没有自动滚动，选区永远只有一屏。
#[test]
fn drag_past_bottom_edge_autoscrolls_and_extends_selection() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(long_assistant_reply(80));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let area = app.content_area.expect("应记录内容区几何");
    // 先回到顶部，确认后续滚动完全由拖动触发。
    app.scroll = Some(0);
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        area.x,
        area.y,
    );
    let mut last_row = area.y;
    for _ in 0..3 {
        // 每次都在下边界之外一格：越界即触发滚动与选区延伸。
        send_mouse(
            &mut app,
            &buffer,
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            area.x + 2,
            area.bottom(),
        );
        last_row = area.bottom();
    }

    let selection = app.text_selection.expect("拖动后应存在选区");
    assert!(
        app.scroll.unwrap_or(0) > 0,
        "拖动越过下边界后视口应向前滚动"
    );
    assert!(
        selection.current.y >= area.height,
        "选区终点应扩展到首屏之外：{:?}（视口高 {}）",
        selection.current,
        area.height
    );
    assert_eq!(selection.current.x, area.x + 2);
    assert_eq!(last_row, area.bottom());
}

/// 同一段文字在滚动前后必须保持被选中：滚轮不该取消选区，高亮也随内容一起移动。
#[test]
fn wheel_scroll_keeps_text_selection() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(long_assistant_reply(80));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    app.scroll = Some(10);
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let area = app.content_area.expect("应记录内容区几何");
    let buffer = terminal.backend().buffer().clone();

    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        area.x + 1,
        area.y + 1,
    );
    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        area.x + 3,
        area.y + 3,
    );
    let before = app.text_selection.expect("拖动后应存在选区");
    let highlighted_before = reversed_rows(&mut terminal, &mut app);

    app.scroll_up(5);

    let after = app.text_selection.expect("滚轮滚动不应取消选区");
    assert_eq!(before.anchor, after.anchor, "滚动不能移动选区起点");
    assert_eq!(before.current, after.current, "滚动不能移动选区终点");
    let highlighted_after = reversed_rows(&mut terminal, &mut app);
    assert_eq!(
        highlighted_after.len(),
        highlighted_before.len(),
        "高亮行数应保持不变：{highlighted_before:?} → {highlighted_after:?}"
    );
    // 内容向下移了 5 行，高亮必须跟着一起下移，而不是留在原地。
    for (before_row, after_row) in highlighted_before.iter().zip(&highlighted_after) {
        assert_eq!(
            *after_row,
            before_row + 5,
            "高亮应随内容一起滚动：{highlighted_before:?} → {highlighted_after:?}"
        );
    }
}

/// 读出当前帧里被 REVERSED 标记的行号，用于断言高亮位置。
fn reversed_rows(terminal: &mut Terminal<TestBackend>, app: &mut App) -> Vec<u16> {
    terminal
        .draw(|frame| {
            render_root(frame, app);
            app.render_text_selection(frame.buffer_mut());
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let reversed =
            (0..buffer.area.width).any(|x| buffer[(x, y)].modifier.contains(Modifier::REVERSED));
        if reversed {
            rows.push(y);
        }
    }
    rows
}

/// 跨屏选区在释放后要能复制到屏幕之外的内容。
///
/// 提取走离屏渲染而非当前帧缓冲区，因此被滚出视口的历史同样在复制范围内。
#[test]
fn selection_extending_above_viewport_is_fully_copyable() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(long_assistant_reply(80));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let area = app.content_area.expect("应记录内容区几何");
    assert!(area.y > 0, "内容区上方应留出可越界的行：{area:?}");
    // 起点停在中间偏下：上方仍有大量未显示内容。
    let start_scroll = 12;
    app.scroll = Some(start_scroll);
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        area.x + 1,
        area.y,
    );
    // 向上拖出内容区：每次越界都会滚动，直到回到内容顶部。
    for _ in 0..20 {
        if app.scroll == Some(0) {
            break;
        }
        send_mouse(
            &mut app,
            &buffer,
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            area.x + 1,
            area.y - 1,
        );
    }
    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        area.x + 1,
        area.y - 1,
    );

    let selection = app.text_selection.expect("应保留选区");
    let copied = app
        .text_selection_content
        .expect("跨屏选区释放后应生成可复制文本");
    assert!(
        selection.row_span().0 < start_scroll,
        "选区起点应早于按下时的视口顶部：{:?} vs {start_scroll}",
        selection.row_span()
    );
    // 这些行在按下时都已滚出视口，只有离屏渲染才拿得到。
    for marker in ["MARK-00", "MARK-01", "MARK-02"] {
        assert!(
            copied.contains(marker),
            "应能复制到视口之上的内容：{copied:?}"
        );
    }
}

/// 首屏没有可滚动内容，选区行号相对消息区顶部：仍能选中并复制首屏文字。
///
/// 这条回退路径容易被"内容行数为零"的夹取逻辑压成一行，因此单独锁住。
#[test]
fn hero_screen_text_can_still_be_selected_and_copied() {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let area = app.content_area.expect("首屏也应记录可选区域");
    assert_eq!(app.content_rows, 0, "首屏没有消息内容");
    let buffer = terminal.backend().buffer().clone();

    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        area.x,
        area.y + 2,
    );
    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        area.x + 20,
        area.y + 4,
    );
    send_mouse(
        &mut app,
        &buffer,
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        area.x + 20,
        area.y + 4,
    );

    let selection = app.text_selection.expect("首屏应保留选区");
    assert!(
        selection.row_span().0 < selection.row_span().1,
        "首屏选区应能跨多行：{:?}",
        selection.row_span()
    );
    let copied = app.text_selection_content.expect("首屏选区应可复制");
    assert!(!copied.trim().is_empty(), "首屏复制内容不应为空");
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

/// 工具正文与标题行里的工具名同列：`● ` 之后是工具名，正文从同一列开始。
///
/// 工具块是从属这次调用的背景信息，与标题同列才看得出归属；缩进同时只有两列，长命令行
/// 与代码行在窄终端里不至于提前折行。缩进被去掉或加宽都会让这条断言失败。
#[test]
fn tool_output_aligns_with_its_title() {
    let width = 80;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建工具缩进测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut message = Msg::tool_started(ToolCall::new(
        "call-indent",
        "shell",
        json!({"command": "cargo build"}),
    ));
    message.append_tool_output("cargo build --release\n");
    app.messages.push(message);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染工具缩进");
    let rows = buffer_rows(&terminal, width);
    let title = rows
        .iter()
        .position(|row| row.contains("Shell(cargo build)"))
        .expect("应展示工具标题");
    let output = rows
        .iter()
        .position(|row| row.contains("cargo build --release"))
        .expect("应展示工具输出");

    // 主内容区不再留左右边距：标题从终端第 1 列画 `●`，工具名紧随其后。
    // `find` 给的是字节偏移，`●` 占 3 字节，因此按显示宽度重新算列号。
    let column = |row: &str, needle: &str| {
        let prefix = &row[..row.find(needle).expect("应包含目标文本")];
        unicode_width::UnicodeWidthStr::width(prefix)
    };
    let title_column = column(&rows[title], "Shell(cargo build)");
    let output_column = column(&rows[output], "cargo build --release");
    assert_eq!(title_column, 2, "工具名应紧跟标记列：{:?}", rows[title]);
    assert_eq!(
        output_column, title_column,
        "工具正文应与工具名同列：{:?} / {:?}",
        rows[title], rows[output]
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

/// 内容溢出时缩小窗口：即使旧滚动条已在画布之外，也只失效热区而不按旧坐标清屏。
#[test]
fn shrinking_window_clears_stale_scrollbar_safely() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "长正文\n".repeat(200)));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let stale = app.scrollbar_area.expect("内容溢出时应绘制滚动条");
    assert!(stale.bottom() > 2, "上一帧轨道应跨过新画布的第二行");

    // 高度缩到 1 时，上一帧轨道整体落在画布之外；宽度收窄则只留下部分列。
    for (width, height) in [(80u16, 1u16), (80, 2), (20, 3), (1, 1), (200, 40)] {
        terminal.backend_mut().resize(width, height);
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    }
}

/// 会话搜索面板画在输入框上方，并随终端缩小一起收进画布而不会越界清屏。
#[test]
fn session_palette_renders_inline_and_survives_tiny_terminal() {
    use crate::command::{
        NativeSessionEntry, NativeSessionPalette, NativeSessionPaletteMode,
        NativeSessionPaletteStatus,
    };
    use agent_session::SessionSummary;

    // 一条固定摘要供三种状态复用；`NativeSessionPaletteStatus` 不要求 `Clone`，
    // 因此每轮都重新构造面板状态，只克隆不可变的摘要。
    let summary = SessionSummary {
        id: SessionId::new("inline-session").expect("创建测试会话标识"),
        revision: 3,
        created_at_ms: 1,
        updated_at_ms: 2,
        title: Some("内联面板会话".into()),
        message_count: 5,
    };
    for status_index in 0..3 {
        for (width, height) in [(1u16, 1u16), (8, 4), (19, 5), (20, 6), (80, 24)] {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(tx, "测试模型".into());
            app.messages
                .push(Msg::new(MsgKind::Assistant, "正文".to_string()));
            let status = match status_index {
                0 => NativeSessionPaletteStatus::Loading,
                1 => NativeSessionPaletteStatus::Error("磁盘读取失败".into()),
                _ => NativeSessionPaletteStatus::Ready(vec![NativeSessionEntry::new(
                    summary.clone(),
                )]),
            };
            app.native_command.session_palette = Some(NativeSessionPalette {
                mode: NativeSessionPaletteMode::Resume,
                selected: 0,
                status,
            });
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        }
    }

    // 面板占输入框上方的独立区域：标题、候选与按键提示都必须落在画布内。
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "正文".to_string()));
    app.native_command.session_palette = Some(NativeSessionPalette {
        mode: NativeSessionPaletteMode::Resume,
        selected: 0,
        status: NativeSessionPaletteStatus::Ready(vec![NativeSessionEntry::new(summary)]),
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    // 宽字符在缓冲区里各占两格，第二格是空白；比较前去掉空白才能匹配原生文本。
    let compact: String = screen.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(compact.contains("恢复项目会话"), "screen={screen}");
    assert!(compact.contains("内联面板会话"), "screen={screen}");
    assert!(compact.contains("输入即搜索"), "screen={screen}");
}

/// 面板要把搜索命中与「当前会话」标出来：前者的作用是解释这条为什么被留下，
/// 后者的作用是避免用户在候选里误选自己正在用的会话。
#[test]
fn session_palette_marks_current_session_and_search_hits() {
    use crate::command::{
        NativeSessionEntry, NativeSessionPalette, NativeSessionPaletteMode,
        NativeSessionPaletteStatus,
    };
    use crate::theme::COLOR_PALETTE_COMMAND;
    use agent_session::SessionSummary;

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "正文".to_string()));
    // 当前会话本身也在候选里：它必须带标记，但不能被当成搜索命中高亮。
    let current = SessionSummary {
        id: app.session_record.id.clone(),
        revision: 3,
        created_at_ms: 1,
        updated_at_ms: 2,
        title: Some("clippy 告警清零".into()),
        message_count: 5,
    };
    let other = SessionSummary {
        id: SessionId::new("budget-plan").expect("创建测试会话标识"),
        revision: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        title: Some("预算规划".into()),
        message_count: 2,
    };
    app.native_command.session_palette = Some(NativeSessionPalette {
        mode: NativeSessionPaletteMode::Browse,
        selected: 0,
        status: NativeSessionPaletteStatus::Ready(vec![
            NativeSessionEntry::new(current),
            NativeSessionEntry::new(other),
        ]),
    });
    // 检索词同时命中两条的检索文本（标题或稳定 ID），才能在一屏里同时校验高亮与否。
    app.input = "p".into();
    app.cursor = app.input.len();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    // 宽字符各占两格、第二格是空白，比较前去掉空白才能匹配原生文本。
    let compact: String = screen.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(compact.contains("当前"), "当前会话应有标记：{screen}");

    // 中文按单元格查样式要跨过宽字符的第二格，因此先换算成单元格下标。
    let cell_of = |needle: &str| {
        let byte = screen
            .find(needle)
            .unwrap_or_else(|| panic!("应展示 {needle}：{screen}"));
        screen[..byte].chars().count()
    };
    // 命中词按强调色显示；未命中的候选保持普通文字色，不能整行变强调色。
    assert_eq!(
        buffer.content()[cell_of("p")].style().fg,
        Some(COLOR_PALETTE_COMMAND),
        "命中词应使用强调色：{screen}"
    );
    assert_ne!(
        buffer.content()[cell_of("预")].style().fg,
        Some(COLOR_PALETTE_COMMAND),
        "未命中的标题不应高亮：{screen}"
    );
}

/// 临时视觉检查：打印不同宽度下的正文与表格排版。
#[test]
fn scratch_dump_widths() {
    let table = "| 用例 | 耗时 | 结果 | 说明 |\n| --- | --- | --- | --- |\n| renders_a_very_long_test_case_name_without_breaking_the_columns_and_still_keeps_going | 1.2s | ok | 覆盖宽度限制移除后的排版 |\n| short | 3ms | ok | - |\n";
    for width in [200u16, 120, 80] {
        eprintln!("===== width {width} =====");
        for line in markdown_lines(table, false, width) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            eprintln!("[{:>3}] {text}", line.width());
        }
    }
}

/// 滑块位置必须跟着滚动位置走：滚到顶贴轨道顶端，滚到底贴轨道底端，中间线性移动。
///
/// 这条测试守住用户直接看到的现象——"滚动条钉在右下角不动"。滑块一旦退化成固定的一格，
/// 或位置算错到永远贴底，这里会立刻失败。
#[test]
fn scrollbar_thumb_tracks_the_scroll_position() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "滑块跟随测试\n".repeat(200)));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();

    let area = app.scrollbar_area.expect("内容溢出时应显示滚动条");
    let max_scroll = app.last_max_scroll;
    assert!(max_scroll > 10, "测试数据应产生足够长的滚动区间");

    // 读出滑块在轨道内的起止行（相对轨道顶端）。
    let thumb_rows = |terminal: &Terminal<TestBackend>, area: Rect| -> (u16, u16) {
        let buffer = terminal.backend().buffer();
        let rows: Vec<u16> = (0..area.height)
            .filter(|offset| matches!(buffer[(area.x, area.y + offset)].symbol(), "█" | "▀" | "▄"))
            .collect();
        let start = rows.first().copied().expect("应能看到滑块");
        let end = rows.last().copied().unwrap_or(start);
        assert_eq!(
            end - start + 1,
            rows.len() as u16,
            "滑块必须连续，不能断成两段"
        );
        (start, end)
    };

    app.scroll = Some(0);
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let (top_start, top_end) = thumb_rows(&terminal, area);
    assert_eq!(top_start, 0, "滚到顶部时滑块应贴住轨道顶端");

    app.scroll = Some(max_scroll);
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let (bottom_start, bottom_end) = thumb_rows(&terminal, area);
    assert_eq!(bottom_end, area.height - 1, "滚到底部时滑块应贴住轨道底端");
    assert!(
        bottom_start > top_start,
        "从顶部滚到底部，滑块必须真的移动：{top_start} -> {bottom_start}"
    );

    // 中间位置严格夹在两端之间，并且随滚动单调下移。
    let mut previous = top_start;
    for step in [max_scroll / 4, max_scroll / 2, max_scroll * 3 / 4] {
        app.scroll = Some(step);
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        let (start, _) = thumb_rows(&terminal, area);
        assert!(
            start >= previous,
            "滚动位置增大时滑块不能上移：{previous} -> {start}"
        );
        assert!(
            start > top_start && start < bottom_start,
            "中间滚动位置 {step} 的滑块应落在两端之间：{start}"
        );
        previous = start;
    }
    let _ = (top_end, bottom_start);
}

/// 长对话里滑块不能被"视口 / 总行数"压成一行——一格滑块看不出任何位置信息。
#[test]
fn scrollbar_thumb_keeps_a_visible_height_for_long_content() {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "超长对话\n".repeat(4000)));
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();

    let area = app.scrollbar_area.expect("内容溢出时应显示滚动条");
    let buffer = terminal.backend().buffer();
    // 滑块用整块与上下半块拼成；按半格计数才能同时覆盖"最小高度"和"半行精度"。
    let mut thumb_half_cells = 0;
    let mut thumb_rows = 0;
    let mut track_rows = 0;
    for offset in 0..area.height {
        match buffer[(area.x, area.y + offset)].symbol() {
            "█" => {
                thumb_half_cells += 2;
                thumb_rows += 1;
            }
            "▀" | "▄" => {
                thumb_half_cells += 1;
                thumb_rows += 1;
            }
            "│" => track_rows += 1,
            other => panic!("滚动条列出现意外字符：{other:?}"),
        }
    }
    assert_eq!(
        thumb_half_cells,
        usize::from(crate::ui::chat::SCROLLBAR_MIN_THUMB) * 2,
        "比例算出的滑块低于下限时应补到最小高度"
    );
    // 轨道铺满整个内容区：除了滑块以外的每一行都必须是轨道字符。
    assert_eq!(
        thumb_rows + track_rows,
        usize::from(area.height),
        "轨道应铺满内容区高度，不留空洞"
    );
}
