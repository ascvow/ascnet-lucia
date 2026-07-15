//! Example ascnet-lucia WASM plugin.
//! ascnet-lucia 示例 WASM 插件。

use agent_plugin::{
    export_plugin, AgentEvent, AgentPlugin, Result, ToolCall, ToolDecision, ToolResult, ToolSpec,
    UiColor, UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine, UiPlacement, UiRenderRequest,
    UiSize, UiSpan, UiStyle,
};
use serde_json::json;

/// Simple stateful plugin.
/// 简单的有状态插件。
#[derive(Default)]
struct EchoPlugin {
    calls_seen: u64,
    events_seen: u64,
    show_help: bool,
}

impl AgentPlugin for EchoPlugin {
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "echo",
            "Echo text back to the model. / 将文本回显给模型。",
            json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to echo. / 要回显的文本。"
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        )]
    }

    fn before_tool(&mut self, call: ToolCall) -> ToolDecision {
        // Demonstrate a real policy hook: block empty echo calls.
        // 演示真实策略 hook：阻止空 echo 调用。
        if call.name == "echo" {
            let is_empty = call
                .args
                .get("text")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .unwrap_or_default()
                .is_empty();
            if is_empty {
                return ToolDecision::Block {
                    reason: "echo.text cannot be empty / echo.text 不能为空".to_string(),
                };
            }
        }
        ToolDecision::Allow
    }

    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        self.calls_seen += 1;
        let text = call
            .args
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "echo": text,
                "source": "wasm-plugin",
                "calls_seen": self.calls_seen,
                "events_seen": self.events_seen,
            }),
        ))
    }

    fn after_tool(&mut self, _result: ToolResult) {
        // A real hook can update plugin state, metrics, or audit logs.
        // 真实 hook 可以更新插件状态、指标或审计日志。
    }

    fn on_event(&mut self, _event: AgentEvent) {
        self.events_seen += 1;
    }

    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![UiDeclaration {
            plugin_id: String::new(),
            view_id: "echo-status".into(),
            title: "Echo 插件".into(),
            placement: UiPlacement::Right,
            size: UiSize {
                width: Some(28),
                height: None,
            },
            focusable: true,
            input_triggers: Vec::new(),
        }]
    }

    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        if request.view_id != "echo-status" {
            return None;
        }
        let mut lines = vec![
            ui_line("WASM 插件运行中", Some(UiColor::Green), true),
            ui_line(&format!("工具调用：{}", self.calls_seen), None, false),
            ui_line(&format!("Agent 事件：{}", self.events_seen), None, false),
        ];
        if self.show_help {
            lines.push(ui_line("Enter：隐藏帮助", Some(UiColor::Cyan), false));
            lines.push(ui_line("Tab：返回主输入区", Some(UiColor::Gray), false));
        } else {
            lines.push(ui_line("Enter：显示帮助", Some(UiColor::Cyan), false));
        }
        Some(UiFrame {
            view_id: request.view_id,
            visible: true,
            lines,
        })
    }

    fn on_ui_input(&mut self, input: UiInput) {
        if input.view_id == "echo-status"
            && matches!(input.event, UiInputEvent::Key { ref code, .. } if code == "enter")
        {
            self.show_help = !self.show_help;
        }
    }
}

/// 创建示例插件面板使用的单行文本。
fn ui_line(text: &str, foreground: Option<UiColor>, bold: bool) -> UiLine {
    UiLine {
        spans: vec![UiSpan {
            text: text.to_string(),
            style: UiStyle {
                foreground,
                bold,
                ..UiStyle::default()
            },
        }],
    }
}

export_plugin!(EchoPlugin);
