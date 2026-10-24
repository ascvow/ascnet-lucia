//! TUI 的语义色标、终端主题适配与动画节奏常量。
//!
//! 下方 RGB 值仅作为缓存中的语义色标，不直接交给终端。最终帧由 [`apply_terminal_theme`]
//! 统一转为终端默认前景/背景与 ANSI 调色板；切换终端主题无需重启，也无需探测操作系统
//! 主题或读取 OSC 回复。头像像素不是界面主题色，支持真彩色时保留原画色。

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

/// 底部状态栏的旋转进度点；只有插件启动计数使用，纯 Core 构建不需要。
#[cfg(feature = "plugins")]
pub(crate) const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 输入框边框活动标题使用的星号闪烁帧。
///
/// 与状态栏旋转点的分工：进度点用于插件加载这类后台任务，星号只用于输入框边框上的
/// 思考与维护提示——边框是静态装饰，需要一个更轻的呼吸感，而不是从头转到底的进度点。
pub(crate) const ACTIVITY_SPARK: &[&str] = &["✳", "✶", "✷", "✸", "✹", "✺", "✻", "✼"];

/// 输入区域的聚焦边框颜色。
pub(crate) const COLOR_BORDER_FOCUS: Color = Color::Rgb(112, 110, 104);
/// 主要文字颜色。
pub(crate) const COLOR_TEXT: Color = Color::Rgb(224, 222, 216);
/// 正文明细色标；最终沿用终端前景，标题和标记通过粗体建立层级。
pub(crate) const COLOR_TEXT_SOFT: Color = Color::Rgb(180, 180, 180);
/// 次要文字和边框颜色。
pub(crate) const COLOR_MUTED: Color = Color::Rgb(124, 122, 116);
/// 元信息色标：行号、路径、Git 状态与状态栏分隔符；最终以默认前景加 DIM 显示。
pub(crate) const COLOR_META: Color = Color::Rgb(100, 100, 100);
/// 用户消息强调色：露西亚主题绯红。
pub(crate) const COLOR_USER: Color = Color::Rgb(226, 61, 82);
/// 成功状态颜色。
pub(crate) const COLOR_SUCCESS: Color = Color::Rgb(104, 190, 126);
/// 运行和等待状态颜色。
pub(crate) const COLOR_WARNING: Color = Color::Rgb(197, 164, 103);
/// 错误状态颜色。
pub(crate) const COLOR_DANGER: Color = Color::Rgb(205, 101, 101);
/// 必须一眼看到的失败提示色标；最终使用终端红色并加粗。
///
/// 只用于错误标记与摘要，普通失败描述仍用 [`COLOR_DANGER`]。
pub(crate) const COLOR_DANGER_BRIGHT: Color = Color::Rgb(255, 110, 110);

/// 面板分隔线色标；最终以默认前景加 DIM 降低装饰线的视觉权重。
pub(crate) const COLOR_SEPARATOR: Color = Color::Rgb(68, 66, 72);

/// 信息强调色：用于插件语义色中的"提示、正在进行"一类状态。
pub(crate) const COLOR_ACCENT: Color = Color::Rgb(122, 172, 196);

/// 列表选中背景色标；最终转换为默认前景/背景反色，不依赖固定深灰底色。
pub(crate) const COLOR_SELECTION_BG: Color = Color::Rgb(56, 54, 60);
/// 选中项左侧的指示条颜色。
pub(crate) const COLOR_SELECTION_MARK: Color = Color::Rgb(226, 61, 82);

/// 命令补全选中背景色标；与通用选中项一样在最终帧中使用反色。
pub(crate) const COLOR_PALETTE_SELECTION_BG: Color = Color::Rgb(45, 45, 52);

/// 命令补全菜单中命令名的颜色。
///
/// 青蓝只给命令名：描述文字保持中性灰，一眼就能把"能敲什么"和"它做什么"分开。
pub(crate) const COLOR_PALETTE_COMMAND: Color = Color::Rgb(126, 200, 216);

/// 代码预览左侧行号列颜色；比正文更暗，避免与内容争夺注意力。
pub(crate) const COLOR_LINE_NUMBER: Color = Color::Rgb(92, 90, 86);
/// 行内代码与围栏代码块的文字颜色：暖色，与正文的中性灰区分开。
pub(crate) const COLOR_CODE: Color = Color::Rgb(206, 184, 146);
/// 围栏代码背景色标；输出时还原终端底色，以竖线、内边距和语法色区分代码。
pub(crate) const COLOR_CODE_BG: Color = Color::Rgb(46, 44, 40);
/// 引用背景色标；输出时还原终端底色，保留引用竖线。
pub(crate) const COLOR_QUOTE_BG: Color = Color::Rgb(39, 38, 35);
/// diff 新增背景色标；输出时还原终端底色，新增语义由绿色和 `+` 保留。
pub(crate) const COLOR_DIFF_ADD_BG: Color = Color::Rgb(42, 58, 40);
/// diff 删除行的整行背景色。
pub(crate) const COLOR_DIFF_DEL_BG: Color = Color::Rgb(66, 42, 40);
/// diff 新增行的文字与标记色。
pub(crate) const COLOR_DIFF_ADD_FG: Color = Color::Rgb(100, 200, 100);
/// diff 删除行的文字与标记色。
pub(crate) const COLOR_DIFF_DEL_FG: Color = Color::Rgb(200, 100, 100);
/// diff 文件头的文字色：柔和的青蓝。
///
/// 文件头是"改的是哪个文件"的定位信息，既不能与 hunk 头的青色混同，也不能用正文色
/// 淹没在上下文行里。
pub(crate) const COLOR_DIFF_FILE_HEADER: Color = Color::Rgb(100, 150, 200);
/// diff hunk 头的文字色：青色。
///
/// 与文件头的青蓝拉开色相：hunk 头是"跳到下一段改动"的锚点，需要在整屏 diff 里被
/// 一眼找到。
pub(crate) const COLOR_DIFF_HUNK: Color = Color::Rgb(120, 200, 200);

/// 代码高亮：关键字。
pub(crate) const COLOR_SYNTAX_KEYWORD: Color = Color::Rgb(214, 122, 160);
/// 代码高亮：字符串字面量。
pub(crate) const COLOR_SYNTAX_STRING: Color = Color::Rgb(176, 196, 132);
/// 代码高亮：注释。
pub(crate) const COLOR_SYNTAX_COMMENT: Color = Color::Rgb(110, 118, 112);
/// 代码高亮：数值字面量。
pub(crate) const COLOR_SYNTAX_NUMBER: Color = Color::Rgb(214, 168, 118);
/// 代码高亮：类型与首字母大写标识符。
pub(crate) const COLOR_SYNTAX_TYPE: Color = Color::Rgb(210, 180, 120);
/// 代码高亮：标点与运算符。
pub(crate) const COLOR_SYNTAX_PUNCT: Color = Color::Rgb(150, 148, 142);
/// 终端输出能力；只影响最终帧，不改变消息与插件渲染缓存中的语义色标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalColors {
    /// 使用默认前景/背景，保留粗体、反色等非颜色提示。
    Monochrome,
    /// 使用终端可配置的基础 ANSI 色板，不发送固定 RGB 值。
    Ansi,
    /// 语义色仍使用终端色板，仅头像等非主题图像允许 RGB。
    TrueColor,
}

impl TerminalColors {
    /// 启动时读取颜色能力；不探测操作系统主题、不读终端输入，也不会发起阻塞查询。
    pub(crate) fn from_env() -> Self {
        Self::detect(
            std::env::var("TERM").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        )
    }

    /// 显式禁色优先；缺失能力信息时按基础 ANSI 终端降级。
    fn detect(term: Option<&str>, colorterm: Option<&str>, no_color: bool) -> Self {
        if no_color || term == Some("dumb") {
            Self::Monochrome
        } else if matches!(colorterm, Some("truecolor" | "24bit"))
            || term.is_some_and(|term| term.ends_with("-direct"))
        {
            Self::TrueColor
        } else {
            Self::Ansi
        }
    }
}

/// 在帧提交给终端前把语义色标解析成终端色板；原生与插件 UI 共用这一出口。
///
/// 必须在完整视图与选区绘制后、Ratatui 差分之前调用一次。正文和卡片使用终端默认底色，
/// 次要信息以 DIM 降级；选中行使用默认前景/背景反色，避免浅色终端出现黑底黑字。
/// 不修改文本、几何或缓存；终端主题切换直接重解释默认色和 ANSI 槽位，不需要重建历史消息。
pub(crate) fn apply_terminal_theme(buffer: &mut Buffer, colors: TerminalColors) {
    for cell in &mut buffer.content {
        if matches!(cell.bg, COLOR_SELECTION_BG | COLOR_PALETTE_SELECTION_BG) {
            cell.fg = Color::Reset;
            cell.bg = Color::Reset;
            cell.modifier.remove(Modifier::DIM);
            cell.modifier.insert(Modifier::REVERSED);
        } else {
            if matches!(
                cell.fg,
                COLOR_MUTED
                    | COLOR_META
                    | COLOR_SEPARATOR
                    | COLOR_LINE_NUMBER
                    | COLOR_SYNTAX_COMMENT
            ) {
                cell.modifier.insert(Modifier::DIM);
            }
            if cell.fg == COLOR_DANGER_BRIGHT {
                cell.modifier.insert(Modifier::BOLD);
            }
            cell.fg = terminal_color(cell.fg, colors);
            // 卡片以现有竖线、缩进和 diff 的 +/- 区分，不假定用户使用深色背景。
            cell.bg = match cell.bg {
                COLOR_CODE_BG | COLOR_QUOTE_BG | COLOR_DIFF_ADD_BG | COLOR_DIFF_DEL_BG => {
                    Color::Reset
                }
                background => terminal_color(background, colors),
            };
        }
        cell.underline_color = terminal_color(cell.underline_color, colors);
    }
}

/// 只解析本模块拥有的语义色；未知颜色属于图像，按终端能力保留或量化。
fn terminal_color(color: Color, colors: TerminalColors) -> Color {
    if colors == TerminalColors::Monochrome {
        return Color::Reset;
    }
    match color {
        COLOR_TEXT | COLOR_TEXT_SOFT | COLOR_BORDER_FOCUS | COLOR_MUTED | COLOR_META
        | COLOR_SEPARATOR | COLOR_LINE_NUMBER | COLOR_SYNTAX_COMMENT | COLOR_SYNTAX_PUNCT => {
            Color::Reset
        }
        COLOR_USER | COLOR_DANGER | COLOR_DANGER_BRIGHT | COLOR_DIFF_DEL_FG => Color::Red,
        COLOR_SUCCESS | COLOR_DIFF_ADD_FG | COLOR_SYNTAX_STRING => Color::Green,
        COLOR_WARNING | COLOR_CODE | COLOR_SYNTAX_NUMBER | COLOR_SYNTAX_TYPE => Color::Yellow,
        COLOR_ACCENT | COLOR_PALETTE_COMMAND | COLOR_DIFF_HUNK => Color::Cyan,
        COLOR_DIFF_FILE_HEADER => Color::Blue,
        COLOR_SYNTAX_KEYWORD => Color::Magenta,
        Color::Rgb(red, green, blue) if colors == TerminalColors::Ansi => {
            // 仅图像使用最近基础色量化；语义色始终走上方显式映射，不随图像算法漂移。
            let palette = [
                (Color::Black, [0, 0, 0]),
                (Color::Red, [170, 0, 0]),
                (Color::Green, [0, 170, 0]),
                (Color::Yellow, [170, 85, 0]),
                (Color::Blue, [0, 0, 170]),
                (Color::Magenta, [170, 0, 170]),
                (Color::Cyan, [0, 170, 170]),
                (Color::Gray, [170, 170, 170]),
            ];
            palette
                .into_iter()
                .min_by_key(|(_, rgb)| {
                    [red, green, blue]
                        .into_iter()
                        .zip(rgb)
                        .map(|(value, target)| (i32::from(value) - target).pow(2))
                        .sum::<i32>()
                })
                .unwrap()
                .0
        }
        color => color,
    }
}

/// Number of 80 ms UI ticks before startup plugin details collapse into the compact counter.
/// 启动插件详情收敛为紧凑计数前保留的 80 毫秒 UI tick 数。
#[cfg(feature = "plugins")]
pub(crate) const PLUGIN_STATUS_DETAIL_TICKS: u16 = 75;
/// UI 动画和后台维护使用的基础 tick 间隔。
///
/// 这个 tick 只负责推进动画（spinner、插件状态计时），不承担流式文本的节拍：文本由
/// [`REVEAL_FRAME_INTERVAL_MS`] 的揭示时钟驱动，只有在确实有待显示文本时才启用。
/// 因此调大或调小它都不会改变打字机的流畅度。
pub(crate) const UI_TICK_INTERVAL_MS: u64 = 80;
/// 流式文本揭示帧间隔（约 60fps）。
///
/// 屏幕节奏必须与网络到达节奏解耦：若只在收到数据时重绘，网络一抖动就会"静止一段时间
/// 后整段蹦出"。这个间隔只在有待揭示文本时驱动一次重绘，空闲时完全不唤醒。
pub(crate) const REVEAL_FRAME_INTERVAL_MS: u64 = 16;
/// 打字机揭示的最低速率（字符/秒），约等于 60fps 下每帧一个字符。
///
/// 低于这个速率的到达（英文长回答、纯中文输出都可能慢于此）不会排队累积：每个新字符
/// 最多等一帧就会上屏。它同时是"匀速"的基准节拍。
pub(crate) const REVEAL_MIN_CHARS_PER_SEC: usize = 60;
/// 打字机揭示的最高速率（字符/秒），约等于 60fps 下每帧三十个字符。
///
/// 网络一次推来整段文本时，揭示速率会随积压上冲；上限保证再大的突发也是"每帧均匀地
/// 多吐几个字"，而不是一帧糊满半屏。
pub(crate) const REVEAL_MAX_CHARS_PER_SEC: usize = 1800;
/// 排空积压的目标时间（毫秒）：揭示速率取"在这么长时间内排空当前积压"的反推值。
///
/// 这是平滑度与延迟之间的唯一旋钮。稳态下积压恒为 `到达速率 × 该值`，因此文字始终以
/// 与网络节奏相匹配的速度匀速吐出，代价是屏幕落后网络约这么长时间。
pub(crate) const REVEAL_DRAIN_MS: u64 = 250;
/// 待揭示缓冲的背压上限（字节）。
///
/// 网络远快于屏幕时，允许积压到这个量级；超过后立即清空积压，保证模型结束时屏幕上
/// 不会还差半屏没显示。
pub(crate) const REVEAL_BACKLOG_LIMIT: usize = 4096;
/// 单个附件大小上限（10 MiB），兼顾常见模型接口的请求体限制。
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// 周期插件视图刷新间隔，约为一秒。
#[cfg(feature = "plugins")]
pub(crate) const PLUGIN_REFRESH_TICKS: u8 = 12;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::widgets::{Paragraph, Widget};

    /// 显式禁色优先，缺失能力声明时保守降级；不依赖 COLORFGBG 或操作系统主题。
    #[test]
    fn detects_terminal_color_capabilities_without_theme_queries() {
        assert_eq!(
            TerminalColors::detect(Some("dumb"), Some("truecolor"), false),
            TerminalColors::Monochrome
        );
        assert_eq!(
            TerminalColors::detect(Some("xterm"), Some("truecolor"), true),
            TerminalColors::Monochrome
        );
        assert_eq!(
            TerminalColors::detect(Some("xterm-direct"), None, false),
            TerminalColors::TrueColor
        );
        assert_eq!(
            TerminalColors::detect(None, Some("24bit"), false),
            TerminalColors::TrueColor
        );
        assert_eq!(
            TerminalColors::detect(Some("xterm-256color"), None, false),
            TerminalColors::Ansi
        );
        assert_eq!(
            TerminalColors::detect(None, None, false),
            TerminalColors::Ansi
        );
    }

    /// 所有界面色标最终都落在终端默认色或基础色板，而不是残留固定的深色 RGB。
    #[test]
    fn semantic_colors_use_terminal_palette_even_with_truecolor() {
        for (semantic, expected) in [
            (COLOR_BORDER_FOCUS, Color::Reset),
            (COLOR_TEXT, Color::Reset),
            (COLOR_TEXT_SOFT, Color::Reset),
            (COLOR_MUTED, Color::Reset),
            (COLOR_META, Color::Reset),
            (COLOR_SEPARATOR, Color::Reset),
            (COLOR_LINE_NUMBER, Color::Reset),
            (COLOR_SYNTAX_COMMENT, Color::Reset),
            (COLOR_SYNTAX_PUNCT, Color::Reset),
            (COLOR_USER, Color::Red),
            (COLOR_DANGER, Color::Red),
            (COLOR_DANGER_BRIGHT, Color::Red),
            (COLOR_SELECTION_MARK, Color::Red),
            (COLOR_DIFF_DEL_FG, Color::Red),
            (COLOR_SUCCESS, Color::Green),
            (COLOR_DIFF_ADD_FG, Color::Green),
            (COLOR_SYNTAX_STRING, Color::Green),
            (COLOR_WARNING, Color::Yellow),
            (COLOR_CODE, Color::Yellow),
            (COLOR_SYNTAX_NUMBER, Color::Yellow),
            (COLOR_SYNTAX_TYPE, Color::Yellow),
            (COLOR_ACCENT, Color::Cyan),
            (COLOR_PALETTE_COMMAND, Color::Cyan),
            (COLOR_DIFF_HUNK, Color::Cyan),
            (COLOR_DIFF_FILE_HEADER, Color::Blue),
            (COLOR_SYNTAX_KEYWORD, Color::Magenta),
        ] {
            for colors in [TerminalColors::Ansi, TerminalColors::TrueColor] {
                assert_eq!(terminal_color(semantic, colors), expected);
            }
        }
    }

    /// 正文使用默认色，深浅主题切换仅更换终端色板；选中项始终是默认色反色的高对比配对。
    #[test]
    fn text_surfaces_and_selection_follow_light_and_dark_defaults() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        for (index, background) in [
            Color::Reset,
            COLOR_CODE_BG,
            COLOR_QUOTE_BG,
            COLOR_DIFF_ADD_BG,
            COLOR_DIFF_DEL_BG,
            COLOR_SELECTION_BG,
            COLOR_PALETTE_SELECTION_BG,
        ]
        .into_iter()
        .enumerate()
        {
            buffer[(index as u16, 0)]
                .set_symbol("x")
                .set_style(Style::new().fg(COLOR_TEXT).bg(background));
        }
        buffer[(7, 0)].set_style(Style::new().fg(COLOR_MUTED));
        apply_terminal_theme(&mut buffer, TerminalColors::TrueColor);
        for (foreground, background) in [(0xdddddd, 0x181818), (0x202020, 0xfafafa)] {
            for index in 0..7 {
                let cell = &buffer[(index, 0)];
                assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset));
                let selected = cell.modifier.contains(Modifier::REVERSED);
                assert_eq!(selected, index >= 5);
                let (visible_fg, visible_bg) = if selected {
                    (background, foreground)
                } else {
                    (foreground, background)
                };
                assert_ne!(visible_fg, visible_bg);
            }
        }
        assert!(buffer[(7, 0)].modifier.contains(Modifier::DIM));
    }

    /// 真实代码/引用/diff 渲染保持文字与结构，移除深色底板但保留语义色及 +/- 标记。
    #[test]
    fn markdown_surfaces_keep_structure_with_terminal_background() {
        let lines = crate::ui::markdown::markdown_lines(
            "> quoted\n\n```rust\nlet value = 42;\n```\n\n```diff\n-old\n+new\n```",
            false,
            60,
        );
        let area = Rect::new(0, 0, 60, 24);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines).render(area, &mut buffer);
        let original: Vec<_> = buffer
            .content
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        apply_terminal_theme(&mut buffer, TerminalColors::TrueColor);
        assert_eq!(
            buffer
                .content
                .iter()
                .map(|cell| cell.symbol().to_string())
                .collect::<Vec<_>>(),
            original
        );
        assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "+" && cell.fg == Color::Green));
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "-" && cell.fg == Color::Red));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "▎"));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "▏"));
    }

    /// 单色模式清除前景、背景与下划线颜色，但不丢掉选中态、文字及字形提示。
    #[test]
    fn monochrome_keeps_non_color_cues_and_text() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buffer = Buffer::empty(area);
        buffer[(0, 0)]
            .set_symbol("!")
            .set_style(Style::new().fg(COLOR_DANGER_BRIGHT).underlined());
        buffer[(0, 0)].underline_color = Color::Red;
        buffer[(1, 0)]
            .set_symbol("▌")
            .set_style(Style::new().fg(COLOR_TEXT).bg(COLOR_SELECTION_BG));
        buffer[(2, 0)].set_symbol("▀").set_style(
            Style::new()
                .fg(Color::Rgb(10, 20, 30))
                .bg(Color::Rgb(40, 50, 60)),
        );
        apply_terminal_theme(&mut buffer, TerminalColors::Monochrome);
        assert!(buffer.content.iter().all(|cell| cell.fg == Color::Reset
            && cell.bg == Color::Reset
            && cell.underline_color == Color::Reset));
        assert!(buffer[(0, 0)]
            .modifier
            .contains(Modifier::BOLD | Modifier::UNDERLINED));
        assert!(buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(2, 0)].symbol(), "▀");
    }

    /// 原生命令菜单和历史正文经过最终出口后不携带深色 RGB，选中行仍铺满反色。
    #[test]
    fn full_frame_theme_keeps_menu_selection_and_cached_history_readable() {
        use crate::app::App;
        use crate::session::conversation::{Msg, MsgKind};
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, "theme-test".into());
        app.messages.push(Msg::new(
            MsgKind::Assistant,
            "# Heading\n\nBody\n\n```rust\nlet n = 42;\n```",
        ));
        app.input = "/".into();
        app.cursor = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        for colors in [
            TerminalColors::TrueColor,
            TerminalColors::Ansi,
            TerminalColors::Monochrome,
        ] {
            terminal
                .draw(|frame| {
                    crate::ui::root::render_root(frame, &mut app);
                    apply_terminal_theme(frame.buffer_mut(), colors);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!(buffer
                .content
                .iter()
                .all(|cell| !matches!(cell.fg, Color::Rgb(..)) && cell.bg == Color::Reset));
            let selected = buffer
                .content
                .chunks(100)
                .find(|row| {
                    row.iter().any(|cell| {
                        cell.symbol() == "▌" && cell.modifier.contains(Modifier::REVERSED)
                    })
                })
                .unwrap();
            for (column, cell) in selected.iter().enumerate() {
                // 宽字符的续占格由 Ratatui 清空，不独立输出样式。
                if column > 0
                    && unicode_width::UnicodeWidthStr::width(selected[column - 1].symbol()) > 1
                {
                    continue;
                }
                assert!(
                    cell.modifier.contains(Modifier::REVERSED) && cell.fg == Color::Reset,
                    "column={column}: {cell:?}"
                );
            }
            if colors == TerminalColors::Monochrome {
                assert!(buffer.content.iter().all(|cell| cell.fg == Color::Reset));
            }
        }
    }

    /// 头像颜色不冒充主题色，只有声明真彩色能力时保留；否则落在基础终端色板。
    #[test]
    fn image_pixels_follow_color_capability_not_semantic_mapping() {
        let pixel = Color::Rgb(255, 210, 202);
        assert_eq!(terminal_color(pixel, TerminalColors::TrueColor), pixel);
        assert!(!matches!(
            terminal_color(pixel, TerminalColors::Ansi),
            Color::Rgb(..) | Color::Indexed(_)
        ));
    }
}
