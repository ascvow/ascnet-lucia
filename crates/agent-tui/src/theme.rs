//! TUI 统一使用的配色与动画节奏常量。

use ratatui::style::Color;

pub(crate) const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 输入区域的聚焦边框颜色。
pub(crate) const COLOR_BORDER_FOCUS: Color = Color::Rgb(112, 110, 104);
/// 主要文字颜色。
pub(crate) const COLOR_TEXT: Color = Color::Rgb(224, 222, 216);
/// 次要文字和边框颜色。
pub(crate) const COLOR_MUTED: Color = Color::Rgb(124, 122, 116);
/// 用户消息强调色：露西亚主题绯红。
pub(crate) const COLOR_USER: Color = Color::Rgb(226, 61, 82);
/// 成功状态颜色。
pub(crate) const COLOR_SUCCESS: Color = Color::Rgb(104, 190, 126);
/// 运行和等待状态颜色。
pub(crate) const COLOR_WARNING: Color = Color::Rgb(197, 164, 103);
/// 错误状态颜色。
pub(crate) const COLOR_DANGER: Color = Color::Rgb(205, 101, 101);
/// 用户消息块背景色：与绯红强调同系的暗红灰，用于与助手正文区分。
pub(crate) const COLOR_USER_BG: Color = Color::Rgb(58, 38, 43);

/// 代码预览左侧行号列颜色；比正文更暗，避免与内容争夺注意力。
pub(crate) const COLOR_LINE_NUMBER: Color = Color::Rgb(92, 90, 86);
/// diff 新增行的整行背景色。
pub(crate) const COLOR_DIFF_ADD_BG: Color = Color::Rgb(26, 56, 36);
/// diff 删除行的整行背景色。
pub(crate) const COLOR_DIFF_DEL_BG: Color = Color::Rgb(66, 30, 34);
/// diff 新增行的行号与标记色。
pub(crate) const COLOR_DIFF_ADD_FG: Color = Color::Rgb(126, 202, 146);
/// diff 删除行的行号与标记色。
pub(crate) const COLOR_DIFF_DEL_FG: Color = Color::Rgb(224, 122, 122);

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
pub(crate) const UI_TICK_INTERVAL_MS: u64 = 80;
/// 单个附件大小上限（10 MiB），兼顾常见模型接口的请求体限制。
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// 周期插件视图刷新间隔，约为一秒。
#[cfg(feature = "plugins")]
pub(crate) const PLUGIN_REFRESH_TICKS: u8 = 12;
