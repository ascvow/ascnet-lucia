//! TUI 统一使用的配色与动画节奏常量。

use ratatui::style::Color;

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
/// 正文明细文字颜色：偏灰而非纯白。
///
/// 长段落整屏用 [`COLOR_TEXT`] 会把界面拉成一片高亮，读者找不到落点；解释性正文降一档
/// 亮度后，标题、标记与错误信息才真正"跳"出来。
pub(crate) const COLOR_TEXT_SOFT: Color = Color::Rgb(180, 180, 180);
/// 次要文字和边框颜色。
pub(crate) const COLOR_MUTED: Color = Color::Rgb(124, 122, 116);
/// 元信息颜色：行号、路径、Git 状态与状态栏分隔符。
///
/// 比 [`COLOR_MUTED`] 再暗一档，让这些只在需要时被查找的信息退到背景里。
pub(crate) const COLOR_META: Color = Color::Rgb(100, 100, 100);
/// 用户消息强调色：露西亚主题绯红。
pub(crate) const COLOR_USER: Color = Color::Rgb(226, 61, 82);
/// 成功状态颜色。
pub(crate) const COLOR_SUCCESS: Color = Color::Rgb(104, 190, 126);
/// 运行和等待状态颜色。
pub(crate) const COLOR_WARNING: Color = Color::Rgb(197, 164, 103);
/// 错误状态颜色。
pub(crate) const COLOR_DANGER: Color = Color::Rgb(205, 101, 101);
/// 必须一眼看到的失败提示颜色：比 [`COLOR_DANGER`] 更亮。
///
/// 只用于错误标记与错误摘要这类"读漏就要付出代价"的信息；正文里的普通失败描述仍用
/// [`COLOR_DANGER`]，否则整屏都是警报色，警报本身也就不再是警报。
pub(crate) const COLOR_DANGER_BRIGHT: Color = Color::Rgb(255, 110, 110);
/// 用户消息块背景色：与绯红强调同系的暗红灰，用于与助手正文区分。
pub(crate) const COLOR_USER_BG: Color = Color::Rgb(58, 38, 43);
/// 助手回答卡片的背景色：只比终端底色亮一点点。
///
/// 回答需要成为一块"卡片"而不是飘在背景上的字，但底色一旦明显，正文的可读性就会被
/// 色块压过去；因此取一档极轻微的抬升，远看是一条连续的浅色块，近看与默认底色几乎
/// 没有差别。代码块与引用块自带更暗的底色，仍然压在这层之上。
pub(crate) const COLOR_ASSISTANT_BG: Color = Color::Rgb(28, 30, 34);

/// 面板之间分隔线的颜色。
///
/// 比次要文字更暗：分隔线只需要"这里换区域了"这一点信息，与正文同亮度会把界面
/// 切得很碎。
pub(crate) const COLOR_SEPARATOR: Color = Color::Rgb(68, 66, 72);

/// 信息强调色：用于插件语义色中的"提示、正在进行"一类状态。
pub(crate) const COLOR_ACCENT: Color = Color::Rgb(122, 172, 196);

/// 列表与菜单中选中项的背景色。
///
/// 中性抬升色而非强调色：选中态需要的是"这一行被拎出来了"，用强调色会和语义色
/// （成功、危险）争夺含义，反色则在深色主题下过于刺眼。
pub(crate) const COLOR_SELECTION_BG: Color = Color::Rgb(56, 54, 60);
/// 选中项左侧的指示条颜色。
pub(crate) const COLOR_SELECTION_MARK: Color = Color::Rgb(226, 61, 82);

/// 命令补全菜单中选中项的背景色。
///
/// 比通用 [`COLOR_SELECTION_BG`] 再暗一档：补全菜单紧贴输入框，行距与留白已经把选中
/// 行拎出来了，底色只需一层极浅的抬升，过亮会像一块糊在输入框上的色板。
pub(crate) const COLOR_PALETTE_SELECTION_BG: Color = Color::Rgb(45, 45, 52);

/// 命令补全菜单中命令名的颜色。
///
/// 青蓝只给命令名：描述文字保持中性灰，一眼就能把"能敲什么"和"它做什么"分开。
pub(crate) const COLOR_PALETTE_COMMAND: Color = Color::Rgb(126, 200, 216);

/// 代码预览左侧行号列颜色；比正文更暗，避免与内容争夺注意力。
pub(crate) const COLOR_LINE_NUMBER: Color = Color::Rgb(92, 90, 86);
/// 行内代码与围栏代码块的文字颜色：暖色，与正文的中性灰区分开。
pub(crate) const COLOR_CODE: Color = Color::Rgb(206, 184, 146);
/// 围栏代码块的整块底色：比消息区略亮的深灰，把代码从正文里垫出来。
pub(crate) const COLOR_CODE_BG: Color = Color::Rgb(34, 33, 38);
/// 行内代码的底色：与围栏代码块同族、略亮一档。
///
/// 行内代码只有几个字符宽，用代码块的底色会显得脏；提高一点亮度后它更像一枚
/// "标签"，在段落里能被一眼扫到。
pub(crate) const COLOR_INLINE_CODE_BG: Color = Color::Rgb(45, 44, 50);
/// 引用块的整块底色：比代码块更暗，靠色差而非竖线区分两种卡片。
pub(crate) const COLOR_QUOTE_BG: Color = Color::Rgb(28, 28, 32);
/// diff 新增行的整行背景色。
///
/// 底色必须远暗于文字：整行色块的作用是"扫一眼看出这里多了一块"，一旦底色接近文字
/// 亮度，色块就会盖过代码本身的可读性。
pub(crate) const COLOR_DIFF_ADD_BG: Color = Color::Rgb(30, 50, 30);
/// diff 删除行的整行背景色。
pub(crate) const COLOR_DIFF_DEL_BG: Color = Color::Rgb(60, 30, 30);
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
/// 单帧至少揭示的字符数。
///
/// 按 60fps 计算约 3000 字符/秒，远高于任何模型的实际输出速度（约 200 字符/秒）：
/// 因此正常的逐 token 流式会被一次性全部上屏，不引入额外延迟。
pub(crate) const REVEAL_CHARS_PER_FRAME: usize = 48;
/// 一次突发被摊平的目标帧数。
///
/// 网络一次推来 N 个字符时，每帧揭示 N/该值（不低于 [`REVEAL_CHARS_PER_FRAME`]）：
/// 突发越大摊得越快，几百字符的整段输出约 4~6 帧铺完，观感是连续吐字。
pub(crate) const REVEAL_SPREAD_FRAMES: usize = 6;
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
