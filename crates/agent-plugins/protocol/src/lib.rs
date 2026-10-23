//! 插件事件总线与终端界面的宿主无关协议。
//!
//! 插件通过 JSON 参与宿主事件总线的分发，并用 JSON 描述停靠位置、带样式文本和输入
//! 事件；宿主负责实际的取消收敛、白名单合并与终端渲染。

#![deny(missing_docs)]

use agent_tool::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 宿主投递给插件的一次事件分发信封。
///
/// 插件看到的是事件的**快照**，而不是事件本身。改写只能通过
/// [`PluginEventResponse::Ready`] 的补丁提交，并由宿主按事件自身声明的白名单合并，
/// 因此插件无法改写调用 ID、运行 ID 或权限 scope 等宿主绑定字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEventEnvelope {
    /// 事件名，例如 `core.tool.call`。
    pub event: String,

    /// 事件当前是否已被取消。
    ///
    /// 插件在 manifest 中声明 `ignore_cancelled = true` 时不会收到已取消的事件。
    #[serde(default)]
    pub cancelled: bool,

    /// 首个取消者身份；未取消时为 `None`，取消后不再变化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceller: Option<String>,

    /// 当前插件在本次分发中的订阅身份，也是它取消事件时被记录的取消者。
    ///
    /// 该值由宿主注入，等于可信插件 ID；插件不能自行声明。
    pub subscriber: String,

    /// 事件负载的完整 JSON 快照。
    pub data: Value,
}

/// 插件对一次事件分发的应答。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PluginEventResponse {
    /// 插件已给出结论。
    Ready {
        /// 是否取消该事件。
        #[serde(default)]
        cancel: bool,

        /// 提交给宿主白名单合并的补丁；缺省表示不改写。
        ///
        /// 宿主只采纳目标事件显式开放的字段，其余键静默忽略。
        #[serde(default, skip_serializing_if = "Value::is_null")]
        patch: Value,
    },

    /// 插件尚未形成结论，宿主应在等待后向同一插件重新投递同一事件。
    ///
    /// 用于人工审批等需要外部输入的场景：插件先返回 `Pending`，在后台等待用户操作，
    /// 宿主按间隔重试直到得到 `Ready`。宿主会限制总等待时长，超时按 fail-closed 处理。
    Pending {
        /// 宿主重新投递前的等待毫秒数。
        #[serde(default = "default_event_retry_after_ms")]
        retry_after_ms: u64,
    },
}

impl PluginEventResponse {
    /// 构造放行且不改写的应答。
    pub fn allow() -> Self {
        Self::Ready {
            cancel: false,
            patch: Value::Null,
        }
    }

    /// 构造取消事件的应答。
    pub fn cancel() -> Self {
        Self::Ready {
            cancel: true,
            patch: Value::Null,
        }
    }

    /// 构造带补丁的应答。
    pub fn patched(cancel: bool, patch: Value) -> Self {
        Self::Ready { cancel, patch }
    }

    /// 构造等待应答。
    pub fn pending(retry_after_ms: u64) -> Self {
        Self::Pending { retry_after_ms }
    }
}

/// 插件省略轮询间隔时宿主使用的默认等待毫秒数。
fn default_event_retry_after_ms() -> u64 {
    100
}

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
    /// 可见时替换主输入区并立即接收按键。
    Input,
    /// A full-screen subview type whose instances are created by navigation requests.
    /// 替换主视图的全屏子视图类型，由导航请求创建实例。
    Subview,
    /// 渲染在主输入区上方的触发面板。
    ///
    /// 仅当视图声明的某个输入触发前缀处于激活状态且帧可见时显示，
    /// 供插件在用户键入期间展示候选或预览内容。
    InputPanel,
    /// 主输入区上方的常驻上下文架。
    ///
    /// 宿主可以同时堆叠多个可见视图；触发式 [`Self::InputPanel`] 或独占
    /// [`Self::Input`] 出现时会暂时隐藏这些常驻内容。
    ComposerShelf,
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
    /// 主输入触发前缀。
    ///
    /// 主输入去除前导空白后以任一前缀开头时该前缀激活：宿主把主输入快照
    /// 与无修饰的 Tab、Enter、方向键、Esc 手势转发给该视图，并按
    /// [`UiPlacement::InputPanel`] 规则渲染。空列表表示视图不参与主输入。
    #[serde(default)]
    pub input_triggers: Vec<String>,
}

/// 插件通过 `describe-ui` 返回的一项界面贡献。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum UiContribution {
    /// 持久或按需显示的普通视图。
    View(UiDeclaration),
    /// 绑定插件自有工具的消息列表渲染器。
    ToolRenderer(ToolRendererContribution),
}

/// 插件为自己拥有的工具声明的消息列表渲染器。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRendererContribution {
    /// 宿主注入的插件 ID，插件返回声明时必须留空。
    #[serde(default)]
    pub plugin_id: String,
    /// 插件内稳定且唯一的渲染器 ID，同时作为 `render-ui` 的 `view_id`。
    pub renderer_id: String,
    /// 该插件公开并拥有的工具名称。
    pub tool_name: String,
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
    /// 应用单调递增的渲染帧序号。
    pub frame: u64,
}

/// 通过同一 `render-ui` ABI 传递的具体渲染请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UiRenderRequestEnvelope {
    /// 普通插件视图渲染请求。
    View(UiRenderRequest),
    /// 工具消息列表渲染请求。
    Tool(Box<ToolRenderRequest>),
}

/// 宿主请求插件渲染一次工具调用在消息列表中的内容。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolRenderRequest {
    /// 目标插件的可信 ID。
    pub plugin_id: String,
    /// 目标 renderer ID。
    pub renderer_id: String,
    /// TUI 提供且不包含 owner 信息的工具展示上下文。
    #[serde(flatten)]
    pub context: ToolRenderContext,
}

/// 应用请求 Host 渲染工具消息时提供的通用上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolRenderContext {
    /// 未经裁剪的完整工具调用。
    pub call: ToolCall,
    /// 工具当前状态及其完整结果或跳过原因。
    pub state: ToolRenderState,
    /// 当前消息列表可用宽度。
    pub width: u16,
    /// 宿主允许 renderer 使用的最大行数。
    pub max_height: u16,
    /// 宿主单调递增的渲染帧序号。
    pub frame: u64,
}

/// 工具消息 renderer 可见的执行状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolRenderState {
    /// 工具正在执行。
    Running,
    /// 工具已经完成。
    Finished {
        /// 未经裁剪的完整工具结果。
        result: ToolResult,
    },
    /// 工具未执行。
    Skipped {
        /// 跳过该工具的原因。
        reason: String,
    },
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
    /// 聚焦时宿主应显示的终端光标位置，相对当前视图内容区左上角。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<UiCursor>,
}

/// 插件帧中的相对终端光标位置。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiCursor {
    /// 相对内容区左侧的终端列。
    pub x: u16,
    /// 相对内容区顶部的终端行。
    pub y: u16,
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
    /// 输入触发前缀激活期间宿主主输入框的内容快照。
    MainInput {
        /// 主输入框的完整文本。
        text: String,
        /// UTF-8 字节光标位置。
        cursor: u32,
    },
}

/// Stable extension event name used for host-level surface actions.
/// 插件请求宿主应用级动作使用的稳定扩展事件名。
///
/// 只有 manifest 在 `surface_action_allowlist` 授权对应动作的插件可以发布该事件。
pub const UI_HOST_ACTION_EVENT: &str = "ui.host.action";

/// An idempotent host action request sent by a plugin.
/// 插件发送给宿主应用的幂等动作请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiHostActionRequest {
    /// 插件内单调或随机的请求 ID，用于忽略重复交付。
    pub request_id: String,
    /// 宿主需执行的应用级动作。
    pub action: UiHostAction,
}

/// 宿主向插件公开的受控应用级动作词汇表。
///
/// 动作全部对应宿主自身的基础能力，不携带任何插件业务语义；
/// 宿主对不支持的动作应报告错误而不是静默忽略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiHostAction {
    /// 替换主输入框内容与光标。
    SetInput {
        /// 新的完整输入文本。
        text: String,
        /// UTF-8 字节光标；缺失时置于文本末尾。
        #[serde(default)]
        cursor: Option<u32>,
    },
    /// 结束当前会话并进入新的空白草稿。
    NewSession,
    /// 清空当前会话上下文并进入新的空白草稿。
    ClearSession,
    /// 通过已注册的上下文加载器立即重载并持久化当前会话上下文。
    ReloadContext {
        /// 后台进行期间展示的可选进度标签。
        #[serde(default)]
        label: Option<String>,
    },
    /// 正常退出宿主应用。
    Exit,
    /// 恢复当前项目内用户确认选中的会话。
    ResumeSession {
        /// 待恢复会话 ID。
        session_id: String,
        /// 选择时看到的修订号，供宿主进行并发校验。
        revision: u64,
    },
    /// 异步查询当前项目的一页会话摘要。
    ///
    /// 宿主完成查询后调用发起插件的 `reply_service` 服务，
    /// 载荷为 [`UiSessionsReply`]。
    QuerySessions {
        /// 用于丢弃过期应答的插件内单调查询 ID。
        query_id: u64,
        /// 不区分大小写的用户过滤文本。
        query: String,
        /// 可选分页游标。
        #[serde(default)]
        cursor: Option<String>,
        /// 本次查询最多返回的摘要数量。
        limit: u16,
        /// 接收应答的发起插件服务名。
        reply_service: String,
    },
}

/// Manifest 可单独授权的宿主界面动作种类。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum UiHostActionKind {
    /// 替换主输入框文本。
    SetInput,
    /// 新建空白会话。
    NewSession,
    /// 清空当前会话。
    ClearSession,
    /// 重载当前上下文。
    ReloadContext,
    /// 退出宿主应用。
    Exit,
    /// 恢复指定会话。
    ResumeSession,
    /// 查询会话摘要。
    QuerySessions,
}

impl UiHostActionKind {
    /// 返回 manifest 与审计快照使用的稳定协议名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SetInput => "set_input",
            Self::NewSession => "new_session",
            Self::ClearSession => "clear_session",
            Self::ReloadContext => "reload_context",
            Self::Exit => "exit",
            Self::ResumeSession => "resume_session",
            Self::QuerySessions => "query_sessions",
        }
    }
}

impl UiHostAction {
    /// 返回当前动作需要的最小 manifest 权限种类。
    pub const fn kind(&self) -> UiHostActionKind {
        match self {
            Self::SetInput { .. } => UiHostActionKind::SetInput,
            Self::NewSession => UiHostActionKind::NewSession,
            Self::ClearSession => UiHostActionKind::ClearSession,
            Self::ReloadContext { .. } => UiHostActionKind::ReloadContext,
            Self::Exit => UiHostActionKind::Exit,
            Self::ResumeSession { .. } => UiHostActionKind::ResumeSession,
            Self::QuerySessions { .. } => UiHostActionKind::QuerySessions,
        }
    }
}

/// 宿主完成一次会话查询后回送插件的应答。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiSessionsReply {
    /// 对应 [`UiHostAction::QuerySessions`] 的查询 ID。
    pub query_id: u64,
    /// 查询结果状态。
    pub status: UiSessionListStatus,
}

/// 会话查询的结果状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiSessionListStatus {
    /// 已取得一页会话摘要。
    Ready {
        /// 当前页会话摘要。
        items: Vec<UiSessionSummary>,
        /// 下一页游标；为空表示没有更多结果。
        next_cursor: Option<String>,
    },
    /// 当前查询没有匹配会话。
    Empty,
    /// 查询失败但插件界面可保持交互。
    Error {
        /// 面向用户的错误说明。
        message: String,
    },
}

/// 会话查询返回的一条轻量摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiSessionSummary {
    /// 当前项目内稳定且唯一的会话 ID。
    pub id: String,
    /// 用于列表展示的会话标题。
    pub title: String,
    /// 会话当前保存的消息数量。
    pub message_count: u64,
    /// 最近更新时间的 Unix 毫秒时间戳。
    pub updated_at_ms: u64,
    /// 由宿主根据当前时间生成的短标签，例如“10 分钟前”。
    #[serde(default)]
    pub updated_label: String,
    /// 用于恢复前并发校验的会话修订号。
    pub revision: u64,
    /// 是否为当前活动会话，活动会话不能重复恢复。
    #[serde(default)]
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 事件信封的线上形状由 Host 与 Guest 共享，不得在边界两侧各写一份。
    #[test]
    fn event_envelope_round_trips_cancellation_state() {
        let envelope = PluginEventEnvelope {
            event: "core.tool.call".into(),
            cancelled: true,
            canceller: Some("permission".into()),
            subscriber: "audit".into(),
            data: json!({ "call": { "name": "shell" } }),
        };

        let encoded = serde_json::to_value(&envelope).expect("序列化事件信封");
        assert_eq!(encoded["cancelled"], true);
        assert_eq!(encoded["canceller"], "permission");
        assert_eq!(
            serde_json::from_value::<PluginEventEnvelope>(encoded).expect("还原事件信封"),
            envelope
        );
    }

    /// 未取消的信封省略取消者字段，插件不应看到无意义的空身份。
    #[test]
    fn pristine_envelope_omits_canceller() {
        let encoded = serde_json::to_value(PluginEventEnvelope {
            event: "core.run.start".into(),
            cancelled: false,
            canceller: None,
            subscriber: "audit".into(),
            data: Value::Null,
        })
        .expect("序列化事件信封");

        assert!(encoded.get("canceller").is_none());
    }

    /// 放行应答不携带补丁，避免宿主对空对象做无谓的白名单合并。
    #[test]
    fn allow_response_carries_no_patch() {
        assert_eq!(
            serde_json::to_value(PluginEventResponse::allow()).expect("序列化放行应答"),
            json!({ "status": "ready", "cancel": false })
        );
    }

    /// 插件省略轮询间隔时必须使用稳定默认值，保证加法字段兼容。
    #[test]
    fn pending_response_uses_default_retry_interval() {
        let response: PluginEventResponse =
            serde_json::from_value(json!({ "status": "pending" })).expect("解析等待应答");

        assert_eq!(response, PluginEventResponse::pending(100));
    }

    /// 静态视图请求省略实例 ID 时必须解析为 `None`。
    #[test]
    fn static_ui_requests_default_to_no_instance() {
        let request: UiRenderRequest = serde_json::from_value(json!({
            "plugin_id": "demo",
            "view_id": "panel",
            "width": 40,
            "height": 12,
            "focused": false,
            "frame": 1
        }))
        .expect("解析静态视图渲染请求");

        assert_eq!(request.instance_id, None);
    }

    /// 旧插件省略新增光标字段时必须继续解析，并保持宿主原有光标行为。
    #[test]
    fn ui_frames_default_to_no_cursor() {
        let frame: UiFrame = serde_json::from_value(json!({
            "view_id": "panel",
            "visible": true,
            "lines": []
        }))
        .expect("解析旧版插件帧");

        assert_eq!(frame.cursor, None);
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

    /// 普通视图与工具 renderer 必须共享同一贡献数组且保持可区分。
    #[test]
    fn ui_contributions_round_trip_without_duplicate_envelopes() {
        let contributions = vec![UiContribution::ToolRenderer(ToolRendererContribution {
            plugin_id: String::new(),
            renderer_id: "task-message".into(),
            tool_name: "task".into(),
        })];

        let encoded = serde_json::to_value(&contributions).expect("序列化 UI 贡献");
        assert_eq!(encoded[0]["renderer_id"], "task-message");
        assert!(encoded[0].get("type").is_none());
        let decoded: Vec<UiContribution> =
            serde_json::from_value(encoded).expect("反序列化 UI 贡献");

        assert_eq!(decoded, contributions);
    }

    /// 普通视图与工具消息请求必须通过字段集合在同一 ABI 入口中准确分派。
    #[test]
    fn render_request_envelope_routes_view_and_tool_shapes() {
        let view = serde_json::to_value(UiRenderRequest {
            plugin_id: "demo".into(),
            view_id: "panel".into(),
            instance_id: None,
            width: 40,
            height: 12,
            focused: false,
            frame: 1,
        })
        .expect("序列化普通视图请求");
        let tool = serde_json::to_value(ToolRenderRequest {
            plugin_id: "demo".into(),
            renderer_id: "task-message".into(),
            context: ToolRenderContext {
                call: ToolCall::new("call-1", "task", json!({})),
                state: ToolRenderState::Running,
                width: 72,
                max_height: 20,
                frame: 2,
            },
        })
        .expect("序列化工具消息请求");

        assert!(matches!(
            serde_json::from_value::<UiRenderRequestEnvelope>(view).expect("分派普通视图请求"),
            UiRenderRequestEnvelope::View(_)
        ));
        assert!(matches!(
            serde_json::from_value::<UiRenderRequestEnvelope>(tool).expect("分派工具消息请求"),
            UiRenderRequestEnvelope::Tool(_)
        ));
    }
}

/// 插件私有持久存储的作用域。
///
/// 两个作用域都只能访问**本插件自己**的命名空间，插件之间互不可见；作用域只决定
/// 数据跟随项目还是跟随用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStoreScope {
    /// 按当前项目隔离；换项目即换命名空间。
    ///
    /// 宿主未提供项目标识时该作用域不可用，而不是退化为跨项目共享——那会让一个
    /// 项目的数据泄漏到另一个项目。
    Workspace,
    /// 跨项目共享的用户级存储。
    User,
}

/// 只读会话列表项。
///
/// 只投影选择会话所需的元数据，不包含任何消息正文；需要正文时另行读取。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSessionSummary {
    /// 会话稳定标识，可用于后续读取。
    pub id: String,
    /// 可供展示的标题；会话尚未生成标题时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 记录创建时间，单位为 UNIX epoch 毫秒。
    pub created_at_ms: u64,
    /// 最近一次保存时间，单位为 UNIX epoch 毫秒。
    pub updated_at_ms: u64,
    /// 最近一次保存后的修订号；插件可据此判断会话是否发生过变化。
    pub revision: u64,
    /// 会话内消息总数，含工具结果消息。
    pub message_count: usize,
}

/// 会话中一次工具调用的只读投影。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSessionToolCall {
    /// 被调用的公开工具名。
    pub name: String,
    /// 调用参数的 JSON 文本。
    pub arguments: String,
}

/// 会话中一条消息的只读投影。
///
/// 该结构**不包含**模型私有推理内容：推理块可能带有只能回传给来源路由的签名，
/// 把它暴露给插件既无检索价值，又会让私有推理越过它的路由边界。图片附件只保留
/// 存在标记，不投影 base64 字节。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSessionMessage {
    /// 消息在会话中的序号，从零开始；与 `PluginSessionReadRequest` 的偏移量一致。
    pub index: usize,
    /// 消息角色：`system`、`developer`、`user`、`assistant` 或 `tool`。
    pub role: String,
    /// 消息内全部文本块按顺序拼接后的正文。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// 该消息发起的工具调用。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<PluginSessionToolCall>,
    /// 该消息携带的工具结果正文。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<PluginSessionToolResult>,
    /// 该消息是否含有未投影的图片附件。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_attachments: bool,
}

/// 会话中一条工具结果的只读投影。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSessionToolResult {
    /// 产生该结果的公开工具名。
    pub name: String,
    /// 结果是否为失败。
    pub is_error: bool,
    /// 结果正文的 JSON 文本。
    pub content: String,
}

/// 读取会话消息的分页响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSessionPage {
    /// 会话稳定标识。
    pub id: String,
    /// 会话内消息总数，用于判断是否还有后续页。
    pub message_count: usize,
    /// 本页消息，按会话顺序排列。
    pub messages: Vec<PluginSessionMessage>,
}

/// Core 在 ReAct 循环各决策点发布的事件名。
///
/// 插件在 manifest 的 `[[events]]` 中按这些名字声明订阅。名字是 Host 与 Guest 之间的
/// 稳定契约，负载形状见本模块中对应的 `*EventData`。
pub mod core_events {
    /// 一次 Agent 运行即将开始；取消则该运行直接以取消终态收尾。
    pub const RUN_START: &str = "core.run.start";
    /// 一次 Agent 运行已结束；只作通告。
    pub const RUN_FINISH: &str = "core.run.finish";
    /// 一轮 ReAct 即将开始；取消则运行以取消终态收尾。
    pub const TURN_START: &str = "core.turn.start";
    /// 一轮 ReAct 已结束；只作通告。
    pub const TURN_FINISH: &str = "core.turn.finish";
    /// 一次模型请求即将发往服务商；取消则运行以取消终态收尾。
    pub const MODEL_REQUEST: &str = "core.model.request";
    /// 模型已返回完整响应；只作通告。
    pub const MODEL_RESPONSE: &str = "core.model.response";
    /// 一次工具调用即将执行；取消即拒绝执行该工具。
    pub const TOOL_CALL: &str = "core.tool.call";
    /// 一次工具调用已产生脱敏终态结果；只作通告。
    pub const TOOL_RESULT: &str = "core.tool.result";
    /// 一条 steering 或 follow-up 消息即将注入会话；取消则该消息被丢弃。
    pub const MESSAGE_INJECT: &str = "core.message.inject";
    /// Core 写入事件记录流的广播副本；只作通告，覆盖模型增量等细粒度记录。
    pub const AGENT_RECORD: &str = "core.agent.event";
}

/// `core.tool.call` 的事件负载。
///
/// 可改写字段：`call.name`、`call.args`、`block_reason`、`agent_guidance`、`cancel_run`。
/// `call.id`、`call.run_id` 与 `call.permission_scope` 由宿主绑定，插件的改写会被忽略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallEventData {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 即将执行的工具调用。
    pub call: ToolCall,
    /// 拒绝执行时面向可信界面的详细原因；不写入会话，也不广播给其他插件。
    #[serde(default)]
    pub block_reason: Option<String>,
    /// 拒绝执行时只注入下一次模型请求的脱敏指导。
    #[serde(default)]
    pub agent_guidance: Option<String>,
    /// 拒绝执行的同时是否结束整个运行。
    #[serde(default)]
    pub cancel_run: bool,
}

/// `core.tool.result` 的事件负载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultEventData {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 脱敏后的工具结果。
    pub result: ToolResult,
    /// 由 Core 注入的实际执行来源。
    pub origin: String,
}

/// `core.message.inject` 的事件负载；可改写 `text`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageInjectEventData {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// ReAct 步数索引。
    pub step: usize,
    /// 注入类型：`steering` 或 `follow_up`。
    pub kind: String,
    /// 即将写入会话的消息正文。
    pub text: String,
}

/// `core.run.finish` 的事件负载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunFinishEventData {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// 实际使用的模型步数。
    pub steps_used: usize,
    /// 是否因取消请求提前收尾。
    pub cancelled: bool,
}

/// `core.run.start` 的事件负载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunStartEventData {
    /// 本次运行的稳定 ID。
    pub run_id: String,
    /// 逻辑服务商名称。
    pub provider: String,
    /// 模型 ID。
    pub model: String,
}

/// 上下文引用 provider 在 manifest `[[provides]]` 中声明的能力 ID。
///
/// 应用据此发现「谁能在用户操作设备/编辑器时提供一段当前上下文」，而不需要认识
/// 任何具体插件或它的业务协议。
pub const CONTEXT_REF_CAPABILITY: &str = "agent.context-ref";

/// 上下文引用服务的稳定服务名。
pub const CONTEXT_REF_SERVICE: &str = "context.ref";

/// 上下文引用服务的契约版本。
pub const CONTEXT_REF_SERVICE_VERSION: &str = "1.0.0";

/// 单次快照最多接受的引用条数。
///
/// 引用会被自动带进下一条用户消息，条数上限是「自动」这件事的安全阀：没有上限的自动
/// 注入等于让插件单方面决定模型看到的上下文规模。
pub const MAX_CONTEXT_REFS: usize = 4;

/// 单个引用 ID 的字节上限。
pub const MAX_CONTEXT_REF_ID_BYTES: usize = 256;

/// 单个引用标题的字节上限。
pub const MAX_CONTEXT_REF_TITLE_BYTES: usize = 256;

/// 单个引用正文的字节上限。
pub const MAX_CONTEXT_REF_BODY_BYTES: usize = 64 * 1024;

/// 一次快照全部正文的字节上限。
pub const MAX_CONTEXT_REF_TOTAL_BODY_BYTES: usize = 128 * 1024;

/// 上下文引用服务的快照请求。
///
/// 当前不接受任何字段；保留对象形态是为了后续以可选字段演进，而不是改服务名。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContextRefRequest {}

/// 插件观测到的一段用户当前引用的上下文。
///
/// 宿主不解释 `body` 的语义，只做边界校验与传递；格式（文件、选区、网页……）由
/// provider 自己决定，因此新增一类引用不需要改宿主。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRef {
    /// provider 内稳定的引用标识。
    ///
    /// 同一段上下文必须给出同一个 ID：宿主用它在多次轮询之间判重与更新，ID 变化会被
    /// 当成「用户换了一段选区」。
    pub id: String,
    /// 面向用户的一行标题，例如 `App.swift:12-20`。
    pub title: String,
    /// 随下一条用户消息注入模型的正文。
    pub body: String,
}

/// 一次上下文引用快照。
///
/// 空数组是合法结果：表示 provider 当前没有可引用的内容（例如用户没有选区）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRefSnapshot {
    /// 当前可引用的上下文片段。
    #[serde(default)]
    pub refs: Vec<ContextRef>,
}

/// 上下文引用契约的边界校验失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRefError(String);

impl std::fmt::Display for ContextRefError {
    /// 输出面向插件作者的单行说明。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContextRefError {}

impl ContextRefError {
    /// 用给定原因构造校验错误。
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl ContextRef {
    /// 校验单条引用是否满足契约边界。
    ///
    /// # Errors
    ///
    /// ID 或标题不合规、正文为空或超限时返回错误。宿主据此丢弃整份快照，而不是把
    /// 可疑内容注入下一条消息。
    pub fn validate(&self) -> Result<(), ContextRefError> {
        if self.id.is_empty() || self.id.len() > MAX_CONTEXT_REF_ID_BYTES {
            return Err(ContextRefError::new(format!(
                "上下文引用 ID 长度必须在 1..={MAX_CONTEXT_REF_ID_BYTES} 字节之间"
            )));
        }
        if !self
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
        {
            return Err(ContextRefError::new(format!(
                "上下文引用 ID 只能包含 ASCII 字母、数字与 `.` `_` `:` `-`：`{}`",
                self.id
            )));
        }
        let title = self.title.trim();
        if title.is_empty() || self.title.len() > MAX_CONTEXT_REF_TITLE_BYTES {
            return Err(ContextRefError::new(format!(
                "上下文引用标题长度必须在 1..={MAX_CONTEXT_REF_TITLE_BYTES} 字节之间"
            )));
        }
        if title != self.title {
            return Err(ContextRefError::new("上下文引用标题不能包含首尾空白"));
        }
        if self.title.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
            return Err(ContextRefError::new("上下文引用标题必须是单行文本"));
        }
        if self.body.is_empty() {
            return Err(ContextRefError::new("上下文引用正文不能为空"));
        }
        if self.body.len() > MAX_CONTEXT_REF_BODY_BYTES {
            return Err(ContextRefError::new(format!(
                "上下文引用正文超过 {MAX_CONTEXT_REF_BODY_BYTES} 字节限制"
            )));
        }
        if self.body.bytes().any(|byte| byte == 0) {
            return Err(ContextRefError::new("上下文引用正文不能包含 NUL"));
        }
        Ok(())
    }
}

impl ContextRefSnapshot {
    /// 校验整份快照，并返回其中的引用。
    ///
    /// # Errors
    ///
    /// 条数超限、ID 重复、单条不合法或正文总量超限时返回错误。任何一项失败都拒绝
    /// 整份快照：部分接受会让「插件给出什么」与「宿主注入什么」难以对照。
    pub fn validate(&self) -> Result<&[ContextRef], ContextRefError> {
        if self.refs.len() > MAX_CONTEXT_REFS {
            return Err(ContextRefError::new(format!(
                "上下文引用最多 {MAX_CONTEXT_REFS} 条，实际 {} 条",
                self.refs.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        let mut total = 0usize;
        for entry in &self.refs {
            entry.validate()?;
            if !seen.insert(entry.id.as_str()) {
                return Err(ContextRefError::new(format!(
                    "上下文引用 ID 重复：`{}`",
                    entry.id
                )));
            }
            total = total.saturating_add(entry.body.len());
        }
        if total > MAX_CONTEXT_REF_TOTAL_BODY_BYTES {
            return Err(ContextRefError::new(format!(
                "上下文引用正文总量超过 {MAX_CONTEXT_REF_TOTAL_BODY_BYTES} 字节限制"
            )));
        }
        Ok(&self.refs)
    }
}

#[cfg(test)]
mod context_ref_tests {
    use super::*;

    /// 构造一条边界内的引用。
    fn entry(id: &str) -> ContextRef {
        ContextRef {
            id: id.to_string(),
            title: format!("App.swift:{id}"),
            body: "```swift\nlet value = 1\n```".to_string(),
        }
    }

    /// 正常快照必须通过，并保持插件给出的顺序。
    #[test]
    fn valid_snapshot_keeps_order() {
        let snapshot = ContextRefSnapshot {
            refs: vec![entry("a"), entry("b")],
        };
        let refs = snapshot.validate().expect("合法快照应通过");
        assert_eq!(refs[0].id, "a");
        assert_eq!(refs[1].id, "b");
    }

    /// 空快照合法：用户没有选区是常态，不是错误。
    #[test]
    fn empty_snapshot_is_valid() {
        assert!(ContextRefSnapshot::default()
            .validate()
            .expect("空快照应通过")
            .is_empty());
    }

    /// 条数超限必须整份拒绝，避免插件单方面放大自动注入的上下文。
    #[test]
    fn too_many_refs_is_rejected() {
        let snapshot = ContextRefSnapshot {
            refs: (0..=MAX_CONTEXT_REFS)
                .map(|index| entry(&format!("ref{index}")))
                .collect(),
        };
        let error = snapshot.validate().expect_err("超限应失败");
        assert!(error.to_string().contains("最多"), "{error}");
    }

    /// 重复 ID 会让宿主无法区分「更新」与「另一段上下文」。
    #[test]
    fn duplicate_ids_are_rejected() {
        let snapshot = ContextRefSnapshot {
            refs: vec![entry("same"), entry("same")],
        };
        let error = snapshot.validate().expect_err("重复 ID 应失败");
        assert!(error.to_string().contains("重复"), "{error}");
    }

    /// ID 会进入宿主内部索引，只接受稳定的 ASCII 字符集。
    #[test]
    fn unsafe_ids_are_rejected() {
        for id in ["", "with space", "path/segment", "换行\n"] {
            let mut value = entry("ok");
            value.id = id.to_string();
            assert!(value.validate().is_err(), "`{id}` 应被拒绝");
        }
    }

    /// 标题是单行展示文本，不能夹带换行或控制字符。
    #[test]
    fn title_must_be_single_line() {
        let mut value = entry("a");
        value.title = "App.swift\n第二行".to_string();
        assert!(value.validate().is_err());

        value.title = " 前后空白 ".to_string();
        assert!(value.validate().is_err());

        value.title = String::new();
        assert!(value.validate().is_err());
    }

    /// 正文是自动注入模型的输入，空值与 NUL 都必须拒绝。
    #[test]
    fn body_must_be_non_empty_and_clean() {
        let mut value = entry("a");
        value.body.clear();
        assert!(value.validate().is_err());

        value.body = "abc\0def".to_string();
        assert!(value.validate().is_err());
    }

    /// 单条与总量上限都必须生效。
    #[test]
    fn body_limits_are_enforced() {
        let mut value = entry("a");
        value.body = "x".repeat(MAX_CONTEXT_REF_BODY_BYTES + 1);
        assert!(value.validate().is_err());

        // 单条不超限但总量超限时同样拒绝。
        let chunk = "x".repeat(MAX_CONTEXT_REF_BODY_BYTES);
        let snapshot = ContextRefSnapshot {
            refs: (0..3)
                .map(|index| {
                    let mut value = entry(&format!("ref{index}"));
                    value.body = chunk.clone();
                    value
                })
                .collect(),
        };
        let error = snapshot.validate().expect_err("总量超限应失败");
        assert!(error.to_string().contains("总量"), "{error}");
    }

    /// 服务请求必须拒绝未知字段，避免调用方以为可选参数生效了。
    #[test]
    fn request_rejects_unknown_fields() {
        assert!(serde_json::from_value::<ContextRefRequest>(serde_json::json!({})).is_ok());
        assert!(
            serde_json::from_value::<ContextRefRequest>(serde_json::json!({
                "scope": "session"
            }))
            .is_err()
        );
    }
}
