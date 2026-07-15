//! ascnet-lucia 插件 TUI 能力展示组件。

use agent_plugin::{
    export_plugin, AgentEvent, AgentPlugin, PluginHostApi, Result, ToolCall, ToolDecision,
    ToolDecisionStatus, ToolResult, ToolSpec, UiColor, UiDeclaration, UiFrame, UiInput,
    UiInputEvent, UiLine, UiNavigationAction, UiNavigationRequest, UiPlacement, UiRenderRequest,
    UiSize, UiSpan, UiStyle, UiViewInstance,
};
use serde_json::json;

const TOP_VIEW: &str = "showcase-top";
const RIGHT_VIEW: &str = "showcase-right";
const BOTTOM_VIEW: &str = "showcase-bottom";
const LEFT_VIEW: &str = "showcase-left";
const DIALOG_VIEW: &str = "showcase-dialog";
const SUBVIEW: &str = "showcase-detail";

/// 保存工具、事件和界面输入共同修改的展示状态。
struct UiShowcasePlugin {
    counter: i64,
    palette: usize,
    dialog_visible: bool,
    message: String,
    events_seen: u64,
    interactions: u64,
    tool_results_seen: u64,
    last_event: String,
}

impl Default for UiShowcasePlugin {
    fn default() -> Self {
        Self {
            counter: 0,
            palette: 0,
            dialog_visible: false,
            message: "插件状态由工具、事件和输入共同驱动".into(),
            events_seen: 0,
            interactions: 0,
            tool_results_seen: 0,
            last_event: "等待 Agent 事件".into(),
        }
    }
}

impl AgentPlugin for UiShowcasePlugin {
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "ui_showcase_control",
            "控制 UI 展示插件：打开或关闭对话框、调整计数、设置面板消息。",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["open_dialog", "close_dialog", "increment", "decrement", "set_message", "reset"]
                    },
                    "message": {
                        "type": "string",
                        "description": "set_message 操作使用的新消息。"
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        )]
    }

    fn before_tool(&mut self, call: ToolCall) -> ToolDecisionStatus {
        if call.name == "ui_showcase_control"
            && call.args.get("action").and_then(|value| value.as_str()) == Some("set_message")
            && call
                .args
                .get("message")
                .and_then(|value| value.as_str())
                .is_none_or(|message| message.trim().is_empty())
        {
            return ToolDecision::Block {
                reason: "set_message 必须提供非空 message".into(),
            }
            .into();
        }
        ToolDecision::Allow.into()
    }

    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        let action = call
            .args
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let message = call
            .args
            .get("message")
            .and_then(|value| value.as_str())
            .map(str::to_string);

        match action.as_str() {
            "open_dialog" => self.dialog_visible = true,
            "close_dialog" => self.dialog_visible = false,
            "increment" => self.counter += 1,
            "decrement" => self.counter -= 1,
            "set_message" => self.message = message.unwrap_or_default(),
            "reset" => self.reset_interactive_state(),
            _ => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("不支持的 action：{action}"),
                ));
            }
        }

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "action": action,
                "counter": self.counter,
                "dialog_visible": self.dialog_visible,
                "message": self.message,
            }),
        ))
    }

    fn after_tool(&mut self, _result: ToolResult) {
        self.tool_results_seen += 1;
    }

    fn on_event(&mut self, event: AgentEvent) {
        self.events_seen += 1;
        self.last_event = format!("{:?} · step {}", event.kind, event.step);
    }

    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            declaration(TOP_VIEW, "顶部状态", UiPlacement::Top, None, Some(4)),
            declaration(LEFT_VIEW, "计数控制", UiPlacement::Left, Some(20), None),
            declaration(RIGHT_VIEW, "事件监视", UiPlacement::Right, Some(26), None),
            declaration(BOTTOM_VIEW, "快捷操作", UiPlacement::Bottom, None, Some(4)),
            declaration(
                DIALOG_VIEW,
                "插件对话框",
                UiPlacement::Dialog,
                Some(56),
                Some(13),
            ),
            declaration(SUBVIEW, "动态子视图", UiPlacement::Subview, None, None),
        ]
    }

    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        let lines = match request.view_id.as_str() {
            TOP_VIEW => self.render_top(&request),
            LEFT_VIEW => self.render_left(&request),
            RIGHT_VIEW => self.render_right(&request),
            BOTTOM_VIEW => self.render_bottom(&request),
            DIALOG_VIEW => self.render_dialog(&request),
            SUBVIEW => self.render_subview(&request),
            _ => return None,
        };
        Some(UiFrame {
            view_id: request.view_id.clone(),
            visible: request.view_id != DIALOG_VIEW || self.dialog_visible,
            lines,
        })
    }

    fn on_ui_input(&mut self, input: UiInput) {
        self.interactions += 1;
        match input.event {
            UiInputEvent::Key { code, .. } => self.handle_key(&input.view_id, &code),
            UiInputEvent::Mouse { kind, x, y } if kind.starts_with("down_") => {
                self.counter += 1;
                self.message = format!("{} 收到鼠标点击 ({x}, {y})", input.view_id);
                if input.view_id == RIGHT_VIEW {
                    self.dialog_visible = true;
                }
            }
            UiInputEvent::Mouse { .. } => {}
            // 展示插件不声明输入触发前缀，不会收到主输入快照。
            UiInputEvent::MainInput { .. } => {}
        }
    }

    /// 展示插件可在不暴露业务字段的前提下创建动态子视图。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        let open_subview = input.view_id == RIGHT_VIEW
            && matches!(input.event, UiInputEvent::Key { ref code, .. } if code == "s");
        self.on_ui_input(input);
        if open_subview {
            self.interactions = self.interactions.saturating_add(1);
            let instance_id = format!("detail-{}", self.interactions);
            let request = UiNavigationRequest {
                request_id: format!("open-{instance_id}"),
                action: UiNavigationAction::Push {
                    view: UiViewInstance {
                        view_id: SUBVIEW.into(),
                        instance_id: instance_id.clone(),
                        title: Some(format!("展示实例 {instance_id}")),
                    },
                },
            };
            if let Err(error) = host.navigate_view(request) {
                self.message = format!("子视图导航失败：{error}");
            }
        }
    }
}

impl UiShowcasePlugin {
    /// 渲染顶部状态条，展示样式、实际尺寸和焦点信息。
    fn render_top(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let accent = self.accent_color();
        vec![line(vec![
            styled("UI Showcase", accent, true),
            plain("  声明式 WASM TUI  "),
            styled(
                if request.focused {
                    "FOCUSED"
                } else {
                    "Tab 聚焦"
                },
                UiColor::Cyan,
                request.focused,
            ),
            plain(&format!("  {}x{}", request.width, request.height)),
        ])]
    }

    /// 渲染左侧可交互计数器。
    fn render_left(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        vec![
            line(vec![styled(
                &format!("计数：{}", self.counter),
                self.accent_color(),
                true,
            )]),
            line(vec![plain("↑ / →  增加")]),
            line(vec![plain("↓ / ←  减少")]),
            line(vec![plain("Enter  切换颜色")]),
            focus_line(request.focused),
        ]
    }

    /// 渲染右侧 Agent 事件与插件生命周期统计。
    fn render_right(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        vec![
            line(vec![styled("生命周期", UiColor::Yellow, true)]),
            line(vec![plain(&format!("事件：{}", self.events_seen))]),
            line(vec![plain(&format!(
                "工具结果：{}",
                self.tool_results_seen
            ))]),
            line(vec![plain(&format!("输入：{}", self.interactions))]),
            line(vec![styled(&self.last_event, UiColor::Gray, false)]),
            line(vec![plain("d / Enter 对话框  s 子视图")]),
            focus_line(request.focused),
        ]
    }

    /// 渲染底部快捷键和共享消息。
    fn render_bottom(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        vec![
            line(vec![
                styled("共享消息：", UiColor::Magenta, true),
                plain(&self.message),
            ]),
            line(vec![
                plain("Tab 切换焦点  "),
                styled("r", UiColor::Red, true),
                plain(" 重置  鼠标点击任意面板增加计数  "),
                styled(
                    if request.focused {
                        "已聚焦"
                    } else {
                        "未聚焦"
                    },
                    UiColor::Cyan,
                    false,
                ),
            ]),
        ]
    }

    /// 渲染按需显示的模态对话框。
    fn render_dialog(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        vec![
            line(vec![styled("插件模态层", self.accent_color(), true)]),
            line(vec![plain("")]),
            line(vec![plain("该区域覆盖主 UI，并优先接收全部输入。")]),
            line(vec![plain(&format!(
                "宿主分配尺寸：{}x{}",
                request.width, request.height
            ))]),
            line(vec![plain(&format!("当前计数：{}", self.counter))]),
            line(vec![plain("")]),
            line(vec![styled("按 Esc、Enter 或 d 关闭", UiColor::Cyan, true)]),
            line(vec![reversed(" DIALOG FOCUS ")]),
        ]
    }

    /// 渲染一个与业务无关的动态视图实例，实例 ID 由宿主原样回传。
    fn render_subview(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let instance_id = request.instance_id.as_deref().unwrap_or("未知实例");
        vec![
            line(vec![styled("插件动态子视图", UiColor::Cyan, true)]),
            line(vec![plain("")]),
            line(vec![plain(&format!("实例 ID：{instance_id}"))]),
            line(vec![plain(&format!(
                "宿主分配尺寸：{}x{}",
                request.width, request.height
            ))]),
            line(vec![plain("")]),
            line(vec![plain(
                "这里可以由插件渲染 sub-agent、workflow 或任意业务实例。",
            )]),
            line(vec![styled("Esc 返回上一层", UiColor::Yellow, true)]),
        ]
    }

    /// 根据焦点视图处理稳定按键名称。
    fn handle_key(&mut self, view_id: &str, code: &str) {
        match (view_id, code) {
            (DIALOG_VIEW, "escape" | "enter" | "d") => self.dialog_visible = false,
            (RIGHT_VIEW, "d" | "enter") => self.dialog_visible = true,
            (LEFT_VIEW, "up" | "right") => self.counter += 1,
            (LEFT_VIEW, "down" | "left") => self.counter -= 1,
            (TOP_VIEW | LEFT_VIEW, "enter") => {
                self.palette = (self.palette + 1) % 6;
            }
            (BOTTOM_VIEW, "r") => self.reset_interactive_state(),
            (_, "d") => self.dialog_visible = true,
            _ => {}
        }
    }

    /// 恢复交互状态，同时保留生命周期统计。
    fn reset_interactive_state(&mut self) {
        self.counter = 0;
        self.palette = 0;
        self.dialog_visible = false;
        self.message = "交互状态已重置".into();
    }

    /// 返回当前强调色。
    fn accent_color(&self) -> UiColor {
        [
            UiColor::Cyan,
            UiColor::Green,
            UiColor::Yellow,
            UiColor::Magenta,
            UiColor::Blue,
            UiColor::Red,
        ][self.palette]
    }
}

/// 创建一个可聚焦的插件视图声明。
fn declaration(
    view_id: &str,
    title: &str,
    placement: UiPlacement,
    width: Option<u16>,
    height: Option<u16>,
) -> UiDeclaration {
    UiDeclaration {
        plugin_id: String::new(),
        view_id: view_id.into(),
        title: title.into(),
        placement,
        size: UiSize { width, height },
        focusable: true,
        input_triggers: Vec::new(),
    }
}

/// 创建由多个样式片段组成的终端行。
fn line(spans: Vec<UiSpan>) -> UiLine {
    UiLine { spans }
}

/// 创建无额外样式的文本片段。
fn plain(text: &str) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle::default(),
    }
}

/// 创建带前景色和可选粗体的文本片段。
fn styled(text: &str, foreground: UiColor, bold: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(foreground),
            bold,
            ..UiStyle::default()
        },
    }
}

/// 创建用于展示反色能力的文本片段。
fn reversed(text: &str) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(UiColor::Black),
            background: Some(UiColor::White),
            bold: true,
            reversed: true,
            ..UiStyle::default()
        },
    }
}

/// 创建带斜体和下划线的焦点状态行。
fn focus_line(focused: bool) -> UiLine {
    UiLine {
        spans: vec![UiSpan {
            text: if focused {
                "当前面板拥有输入焦点"
            } else {
                "按 Tab 聚焦此面板"
            }
            .into(),
            style: UiStyle {
                foreground: Some(if focused {
                    UiColor::Green
                } else {
                    UiColor::Gray
                }),
                italic: true,
                underlined: focused,
                ..UiStyle::default()
            },
        }],
    }
}

export_plugin!(UiShowcasePlugin);
