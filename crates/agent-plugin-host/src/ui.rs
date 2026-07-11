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
    /// A full-screen subview type whose instances are created by navigation requests.
    /// 替换主视图的全屏子视图类型，由导航请求创建实例。
    Subview,
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
    /// Dynamic subview instance ID; docked views and dialogs use `None`.
    /// 动态子视图实例 ID；停靠视图和对话框为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// 当前可用宽度。
    pub width: u16,
    /// 当前可用高度。
    pub height: u16,
    /// 视图当前是否拥有输入焦点。
    pub focused: bool,
    /// 宿主单调递增的渲染帧序号。
    pub frame: u64,
}

/// A dynamic subview instance created by a plugin.
/// 插件创建的动态子视图实例。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiViewInstance {
    /// View type matching [`UiDeclaration::view_id`].
    /// 对应 [`UiDeclaration::view_id`] 的视图类型。
    pub view_id: String,
    /// Stable plugin-local instance ID, such as an opaque task or Agent ID.
    /// 插件内稳定唯一的实例 ID，例如某个任务或 Agent 的不透明 ID。
    pub instance_id: String,
    /// Optional instance title overriding the static declaration title.
    /// 覆盖静态声明标题的可选实例标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Navigation actions between the main view and plugin subviews.
/// 主视图与插件子视图之间的导航动作。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiNavigationAction {
    /// Pushes a subview above the current view.
    /// 在当前视图之上压入一个子视图。
    Push {
        /// 要压入导航栈的动态视图实例。
        view: UiViewInstance,
    },
    /// Replaces the current plugin-owned subview.
    /// 用新子视图替换当前插件子视图。
    Replace {
        /// 替换当前栈顶的动态视图实例。
        view: UiViewInstance,
    },
    /// Closes the current plugin-owned subview and returns to its parent.
    /// 关闭当前插件子视图，返回上一层。
    Pop,
}

/// An idempotent view navigation request sent by a plugin.
/// 插件发送给应用的幂等视图导航请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiNavigationRequest {
    /// Plugin-local monotonic or random request ID used for deduplication.
    /// 插件内单调或随机的请求 ID，用于忽略重复交付。
    pub request_id: String,
    /// Navigation action to be applied by the application.
    /// 应用需执行的导航动作。
    pub action: UiNavigationAction,
}

/// Stable extension event name used for plugin view navigation.
/// 插件视图导航使用的稳定扩展事件名。
pub const UI_NAVIGATION_EVENT: &str = "ui.view.navigation";

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
    /// Dynamic subview instance ID; static views use `None`.
    /// 动态子视图实例 ID；静态视图为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 旧插件不携带 instance_id 的 JSON 必须继续可被新 Host 解析。
    #[test]
    fn legacy_ui_requests_default_to_static_view() {
        let request: UiRenderRequest = serde_json::from_value(json!({
            "plugin_id": "demo",
            "view_id": "panel",
            "width": 40,
            "height": 12,
            "focused": false,
            "frame": 1
        }))
        .expect("解析旧版渲染请求");

        assert_eq!(request.instance_id, None);
    }

    /// 子视图导航请求必须保留动作和动态实例身份。
    #[test]
    fn subview_navigation_round_trips_through_json() {
        let request = UiNavigationRequest {
            request_id: "open-1".into(),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: "agent-detail".into(),
                    instance_id: "agent-1".into(),
                    title: Some("Reviewer".into()),
                },
            },
        };

        let encoded = serde_json::to_value(&request).expect("序列化导航请求");
        let decoded: UiNavigationRequest =
            serde_json::from_value(encoded).expect("反序列化导航请求");

        assert_eq!(decoded, request);
    }
}
