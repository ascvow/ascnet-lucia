//! 插件终端界面的宿主无关协议。
//!
//! 插件通过 JSON 描述停靠位置、带样式文本和输入事件，宿主负责实际布局与终端渲染。

use serde::{Deserialize, Serialize};

/// 插件界面可以挂载的位置。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiPlacement {
    /// 主界面上方插槽。
    Top,
    /// 主界面右侧插槽。
    Right,
    /// 主界面下方插槽。
    Bottom,
    /// 主界面左侧插槽。
    Left,
    /// 覆盖主界面的模态对话框。
    Dialog,
}

/// 插件界面期望尺寸，宿主可按终端空间缩小该尺寸。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiSize {
    /// 期望宽度；左右插槽和对话框使用该值。
    pub width: Option<u16>,
    /// 期望高度；上下插槽和对话框使用该值。
    pub height: Option<u16>,
}

/// 一个插件视图的静态声明。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiDeclaration {
    /// 宿主注入的插件 ID，插件返回声明时可以留空。
    #[serde(default)]
    pub plugin_id: String,
    /// 插件内唯一且稳定的视图 ID。
    pub view_id: String,
    /// 宿主显示的视图标题。
    pub title: String,
    /// 视图挂载位置。
    pub placement: UiPlacement,
    /// 视图期望尺寸。
    #[serde(default)]
    pub size: UiSize,
    /// 视图是否可以通过 Tab 或模态焦点接收输入。
    #[serde(default)]
    pub focusable: bool,
}

/// 宿主请求插件渲染一帧时提供的上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiRenderRequest {
    /// 目标插件 ID。
    pub plugin_id: String,
    /// 目标视图 ID。
    pub view_id: String,
    /// 当前可用宽度。
    pub width: u16,
    /// 当前可用高度。
    pub height: u16,
    /// 视图当前是否拥有输入焦点。
    pub focused: bool,
    /// 宿主单调递增的渲染帧序号。
    pub frame: u64,
}

/// 插件返回的一帧声明式终端内容。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiFrame {
    /// 对应的视图 ID。
    pub view_id: String,
    /// 是否显示该视图；对话框可借此控制打开和关闭。
    #[serde(default = "default_visible")]
    pub visible: bool,
    /// 按终端行排列的内容。
    #[serde(default)]
    pub lines: Vec<UiLine>,
}

fn default_visible() -> bool {
    true
}

/// 一行由多个不同样式的文本片段组成。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiLine {
    /// 从左到右渲染的文本片段。
    #[serde(default)]
    pub spans: Vec<UiSpan>,
}

/// 带终端样式的文本片段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiSpan {
    /// 要显示的文本，不应包含 ANSI 控制序列。
    pub text: String,
    /// 宿主可映射到当前终端能力的样式。
    #[serde(default)]
    pub style: UiStyle,
}

/// 宿主支持的稳定终端样式子集。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiStyle {
    /// 前景色。
    pub foreground: Option<UiColor>,
    /// 背景色。
    pub background: Option<UiColor>,
    /// 是否加粗。
    #[serde(default)]
    pub bold: bool,
    /// 是否使用斜体。
    #[serde(default)]
    pub italic: bool,
    /// 是否添加下划线。
    #[serde(default)]
    pub underlined: bool,
    /// 是否反转前景色和背景色。
    #[serde(default)]
    pub reversed: bool,
}

/// 插件可使用的便携终端颜色。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiColor {
    /// 黑色。
    Black,
    /// 红色。
    Red,
    /// 绿色。
    Green,
    /// 黄色。
    Yellow,
    /// 蓝色。
    Blue,
    /// 洋红色。
    Magenta,
    /// 青色。
    Cyan,
    /// 白色。
    White,
    /// 灰色。
    Gray,
}

/// 路由给插件视图的输入事件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiInput {
    /// 目标插件 ID。
    pub plugin_id: String,
    /// 目标视图 ID。
    pub view_id: String,
    /// 已转换为宿主无关形式的事件。
    pub event: UiInputEvent,
}

/// 插件界面支持的输入事件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiInputEvent {
    /// 键盘按键事件。
    Key {
        /// 稳定按键名称或单个字符。
        code: String,
        /// 按固定顺序提供的修饰键名称。
        #[serde(default)]
        modifiers: Vec<String>,
    },
    /// 鼠标事件，坐标相对于插件视图内容区。
    Mouse {
        /// 按键、释放、移动或滚轮动作名称。
        kind: String,
        /// 内容区横坐标。
        x: u16,
        /// 内容区纵坐标。
        y: u16,
    },
}
