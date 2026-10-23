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

/// 跳转提示独占留白行；连续重绘、窗口缩小与回到底部不能截断或残留点击区域。
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
                "提示应在正文视口之后的留白行"
            );
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
    // 轨道与滑块只用一列宽字符，且不会挤掉正文：正文右边界在滚动条左侧。
    let column: String = (area.y..area.bottom())
        .map(|y| terminal.backend().buffer()[(area.x, y)].symbol())
        .collect();
    assert!(
        column.chars().all(|glyph| glyph == '│' || glyph == '┃'),
        "滚动条列只应出现轨道或滑块：{column:?}"
    );
    assert!(column.contains('┃'), "应能看到滑块：{column:?}");

    // 内容不再溢出时滚动条消失，上一帧的轨道必须被擦掉。
    app.messages.clear();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    assert!(app.scrollbar_area.is_none());
    let leftover: String = (area.y..area.bottom())
        .map(|y| terminal.backend().buffer()[(area.x, y)].symbol())
        .collect();
    assert_eq!(leftover.trim(), "", "残留轨道：{leftover:?}");
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

/// 输入提示应紧跟输入盒上边框；下边框之前留一行内部空白，之后立即进入状态栏。
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
    // 编辑行之后是输入盒内部的留白行（只剩左右边框），再往下才是下边框与状态栏。
    let interior = rows[rule_row + 2].trim().trim_matches('│').trim();
    assert_eq!(interior, "", "单行输入时输入盒内部应有第二行留白：{rows:?}");
    assert!(
        rows[rule_row + 3]
            .chars()
            .filter(|character| *character == '─')
            .count()
            > 20
    );
    assert!(!rows[rule_row + 4].trim().is_empty());
}

/// 助手回答渲染为一块连续的卡片底色，并与随后的工具日志隔一行真正的空白。
///
/// 这三件事必须同时成立：回答内部（含段落之间的空行）色块不断；卡片右边界之外回到
/// 终端底色；卡片与工具日志之间留一行没有底色的空行，两者才不会粘成一片。
#[test]
fn assistant_card_is_continuous_and_separated_from_tool_logs() {
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

    let card_bg = crate::theme::COLOR_ASSISTANT_BG;
    // 回答内部（含段落间空行）直到卡片右边界都是卡片底色。
    //
    // 只看空白单元格：宽字符的第二列由终端按字形铺满，缓冲里不写样式（写了也没有
    // 视觉效果），检查它会得到假失败。文字之外的每一列才真正体现卡片底色是否连续。
    for row in first..=second {
        // 主内容区左右各留两列，卡片右边界就在 width - 2。
        for column in 2..width - 2 {
            let cell = &buffer[(column, row as u16)];
            // 宽字符的续占单元格由终端按字形铺满，缓冲里不写样式，跳过它。
            let wide_tail = column > 2
                && unicode_width::UnicodeWidthStr::width(buffer[(column - 1, row as u16)].symbol())
                    == 2;
            if cell.symbol() == " " && !wide_tail {
                assert_eq!(
                    cell.bg, card_bg,
                    "第 {row} 行第 {column} 列应是卡片底色：{rows:?}"
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

/// 主内容区左右各留 2 列：正文不贴终端边缘，状态栏与正文区左边界对齐。
///
/// 助手回答在最外层留白之内还有自己的卡片内边距，因此行首标记整体缩进四列。
#[test]
fn main_content_keeps_two_column_padding() {
    let width = 80;
    let backend = TestBackend::new(width, 20);
    let mut terminal = Terminal::new(backend).expect("创建左右留白测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::Assistant, "左右留白测试正文"));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染左右留白");
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
        rows[marker_row].starts_with("    ● "),
        "正文应缩进两列、卡片内边距再缩进两列：{:?}",
        rows[marker_row]
    );
    // 卡片底色从正文区左边界开始铺：内边距本身就带底色，卡片左边缘因此是一条直线。
    assert_eq!(
        terminal.backend().buffer()[(2, marker_row as u16)].bg,
        crate::theme::COLOR_ASSISTANT_BG,
        "卡片底色应铺到正文区左边界"
    );
    let footer_row = rows.last().expect("底栏应存在");
    assert!(
        footer_row.starts_with("  lucia"),
        "底栏应与正文左对齐：{footer_row:?}"
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

    let border_row = rows
        .iter()
        .position(|row| row.contains('╭'))
        .expect("输入盒上边框应存在");
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

/// 返回输入盒上边框所在行；边框圆角是唯一可靠的定位锚点。
fn prompt_border_row(rows: &[String]) -> usize {
    rows.iter()
        .position(|row| row.contains('╭'))
        .expect("输入盒上边框应存在")
}

/// 验证运行状态（含耗时与停止提示）落在输入框边框上，日志区不再重复输出，
/// 同时窄终端仍隐藏工作目录。
#[test]
fn running_activity_renders_on_prompt_border_only() {
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
    let border_row = prompt_border_row(&rows);
    let title = &rows[border_row];

    assert!(title.contains("Working"), "{title:?}");
    assert!(title.contains("Esc to stop"), "{title:?}");
    // 标题与边框之间必须留出空隙，否则状态会贴着圆角。
    assert!(title.contains(" Working"), "{title:?}");
    assert!(
        rows[..border_row]
            .iter()
            .all(|row| !row.contains("Working") && !row.contains("Esc to stop")),
        "日志区不再输出活动状态：{rows:?}"
    );
    let text: String = rows
        .concat()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    assert!(text.contains("Steerthecurrentrun..."), "{text:?}");
    assert!(!text.contains("ascnet-lucia"), "{text:?}");
}

/// 输入框边框标题中某个片段的单元格颜色，用于验证语义配色。
fn border_title_color(terminal: &Terminal<TestBackend>, rows: &[String], needle: &str) -> Color {
    let row = prompt_border_row(rows);
    let x = rows[row].find(needle).expect("标题片段应存在") as u16;
    terminal.backend().buffer()[(x, row as u16)].fg
}

/// 后台上下文压缩不需要 Agent 运行中：标题显示调用方标签，且不出现运行态尾巴。
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
    let title = &rows[prompt_border_row(&rows)];

    assert!(title.contains("Compacting context"), "{title:?}");
    assert!(
        !title.contains("Esc to stop"),
        "压缩不是可中断的 Agent 运行：{title:?}"
    );
}

/// 动画帧随 UI tick 推进：相邻两格必须给出不同的星号，否则标题是死画面。
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
        let title = rows[prompt_border_row(&rows)].clone();
        assert!(title.contains("Thinking"), "{title:?}");
        frames.push(title);
    }

    assert_ne!(frames[0], frames[1], "两帧标题应不同：{frames:?}");
}

/// 超过一分钟的耗时折算成 `1m05s`，避免标题随秒数无限变宽。
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
    let title = &rows[prompt_border_row(&rows)];

    assert!(title.contains("2m05s"), "{title:?}");
}

/// 边框变窄时标题先按宽度截断，窄到只剩碎片则整体隐藏，始终保持纯框线或可读标题。
#[test]
fn activity_title_degrades_on_narrow_borders() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.run_started_at = Some(std::time::Instant::now());

    // 中等宽度：标题放不下完整文案，按显示宽度截断并留下省略号。
    let width = 24;
    let mut terminal = Terminal::new(TestBackend::new(width, 16)).expect("创建窄边框测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染窄边框");
    let rows = buffer_rows(&terminal, width);
    let title = &rows[prompt_border_row(&rows)];
    assert!(title.contains('…'), "窄边框应截断而不是撑破边框：{title:?}");

    // 极窄宽度：可用宽度不足一行有意义的文字，标题整体隐藏。
    let width = 14;
    let mut terminal = Terminal::new(TestBackend::new(width, 16)).expect("创建极窄边框测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染极窄边框");
    let rows = buffer_rows(&terminal, width);
    let title = &rows[prompt_border_row(&rows)];
    assert!(
        title
            .chars()
            .all(|character| character == ' ' || "╭╮╰╯─│".contains(character)),
        "过窄时不应绘制标题：{title:?}"
    );
}

/// 工具执行状态用工具名与目标作为边框标题，并以成功色区分思考状态。
#[test]
fn tool_activity_renders_tool_title_on_prompt_border() {
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
    let title = &rows[prompt_border_row(&rows)];

    assert!(title.contains("Shell(cargo test)"), "{title:?}");
    assert!(title.contains("Esc to stop"), "{title:?}");
    assert!(!title.contains("Thinking"), "{title:?}");
    assert_eq!(
        border_title_color(&terminal, &rows, "Shell"),
        crate::theme::COLOR_SUCCESS,
        "工具执行状态应使用成功色：{title:?}"
    );
}

/// 空闲时输入框上边框不带任何标题，保持干净。
#[test]
fn idle_prompt_border_has_no_title() {
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
    let border_row = &rows[prompt_border_row(&rows)];

    assert!(
        border_row
            .chars()
            .all(|character| character == ' ' || "╭╮╰╯─│".contains(character)),
        "空闲时边框只应有框线字符：{border_row:?}"
    );
}

/// 活动状态只取唯一最高优先级的一项，供边框标题直接使用。
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
        Some(PromptActivity::Tool("Shell(cargo test)".into())),
        "工具执行状态应展示工具名与目标"
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
    let border_row = prompt_border_row(&rows);
    let title = &rows[border_row];
    assert!(title.contains("Thinking"), "{title:?}");
    assert!(title.contains("Esc to stop"), "{title:?}");
    assert!(!title.contains("Working"), "{title:?}");
    // 推理摘要文本不得进入任何一行，包括状态行与正文。
    let text = rows.concat();
    assert!(!text.contains("thinking("), "{text:?}");
    assert!(
        rows[..border_row]
            .iter()
            .all(|row| !row.contains("Thinking")),
        "思考状态只应出现在输入框边框：{rows:?}"
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

/// 同一工具组内紧凑排列；组结束后与后续回复之间保留一行空行。
///
/// 工具组内的连续调用属于同一次动作，插入空行会把一次动作拆散；组与助手正文之间
/// 则是不同逻辑模块，必须留白，否则 Shell 输出与随后的分析会糊成一片。
#[test]
fn tool_group_keeps_compact_rows_and_blank_line_before_reply() {
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
    assert_eq!(tool_rows[1], tool_rows[0] + 1, "{rows:?}");
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
    let only_inline_code_paints = lines.iter().flat_map(|line| line.spans.iter()).all(|span| {
        span.style.bg.is_none() || span.style.bg == Some(crate::theme::COLOR_INLINE_CODE_BG)
    });
    assert!(only_inline_code_paints);

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

/// 回答卡片左侧留两列内边距，右侧留白交给整块底色。
#[test]
fn assistant_card_indents_and_leaves_room_on_the_right() {
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

    assert_eq!(body.spans[0].content, "  ", "左侧应有两列内边距");
    assert_eq!(
        body.spans[0].style.bg, None,
        "内边距不自己铺色：底色由卡片矩形统一铺出"
    );
    // 卡片宽度 = 60，左右内边距各两列、消息标记两列，正文最多排到第 54 列。
    assert!(
        body.width() <= usize::from(width) - 4,
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

/// 解释性正文最多排到阅读宽度：助手卡片整体收在 100 列阅读宽 + 两侧内边距内。
#[test]
fn prose_lines_stop_at_reading_width() {
    let text = "字".repeat(200);
    let lines = markdown_lines(&text, false, 200);
    let longest = lines.iter().map(|line| line.width()).max().unwrap_or(0);

    assert!(
        longest <= 104,
        "正文卡片宽度应限制在阅读宽度内（100 列 + 两侧 2 列内边距）：{longest}"
    );
    // 代码块在卡片内部仍然吃满整个卡片宽度：它的对齐语义依赖可用列数。
    let code = markdown_lines("```rust\nlet value = 1;\n```", false, 200);
    let widest_code = code.iter().map(|line| line.width()).max().unwrap_or(0);
    assert_eq!(widest_code, longest, "代码块应铺满卡片宽度");
}

/// 行内代码去掉反引号、改用代码色，并带一层与代码块同族的标签底色。
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
    assert_eq!(
        span.style.bg,
        Some(crate::theme::COLOR_INLINE_CODE_BG),
        "行内代码应带标签底色"
    );
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

    // 卡片内边距两列 + 行首标记两列，圆点正文从第五列开始。
    assert!(rows[0].starts_with("  ● • 第一项"), "{text:?}");
    assert!(!text.contains("- 第一项"), "列表标记不应残留字面 `- `");
    let continuation = rows.get(1).expect("第一项应折行产生续行").clone();
    // 卡片内边距两列 + 消息前缀两列 + 悬挂缩进两列，正文从第七列开始。
    assert!(continuation.starts_with("      "), "{text:?}");
    assert!(!continuation.starts_with("       "), "{text:?}");
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

/// 内容溢出时缩小窗口：上一帧的滚动条轨道会被新画布裁掉，清屏必须按交集处理。
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

/// 终端比对话框的设计下限还小时，会话对话框仍要落在画布内而不是越界清屏。
#[test]
fn session_dialog_survives_tiny_terminal() {
    use crate::command::{NativeSessionDialog, NativeSessionDialogMode, NativeSessionDialogStatus};

    for (width, height) in [(1u16, 1u16), (8, 4), (19, 5), (20, 6), (88, 24)] {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.messages
            .push(Msg::new(MsgKind::Assistant, "正文".to_string()));
        app.native_command.dialog = Some(NativeSessionDialog {
            mode: NativeSessionDialogMode::Resume,
            selected: 0,
            status: NativeSessionDialogStatus::Loading,
        });
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    }
}
