//! Agent 工具策略与交互审批插件。

use agent_plugin::{
    export_plugin, AgentPlugin, ToolCall, ToolDecision, ToolResult, UiColor, UiDeclaration,
    UiFrame, UiInput, UiInputEvent, UiLine, UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle,
};
use std::{collections::BTreeMap, path::Component};

const APPROVAL_VIEW: &str = "sandbox-approval";

/// 单次工具审批的最终选择。
#[derive(Debug, Clone)]
enum ApprovalResolution {
    Allow,
    Deny,
}

/// 等待用户处理的一次工具调用。
#[derive(Debug, Clone)]
struct PendingApproval {
    request_id: String,
    call_id: String,
    tool_name: String,
    summary: String,
}

/// 沙盒插件状态；策略只检查调用参数，不持有宿主文件、进程或网络能力。
#[derive(Default)]
pub struct SandboxPlugin {
    pending: Vec<PendingApproval>,
    resolutions: BTreeMap<String, ApprovalResolution>,
    allowed: u64,
    denied: u64,
    completed: u64,
}

impl AgentPlugin for SandboxPlugin {
    fn before_tool(&mut self, call: ToolCall) -> ToolDecision {
        if let Some(resolution) = self.resolutions.remove(&call.id) {
            return match resolution {
                ApprovalResolution::Allow => {
                    self.allowed = self.allowed.saturating_add(1);
                    ToolDecision::Allow
                }
                ApprovalResolution::Deny => {
                    self.denied = self.denied.saturating_add(1);
                    ToolDecision::Block {
                        reason: format!("用户拒绝执行工具 `{}`", call.name),
                    }
                }
            };
        }

        if let Some(reason) = blocked_reason(&call) {
            self.denied = self.denied.saturating_add(1);
            return ToolDecision::Block { reason };
        }

        if !requires_approval(&call) {
            self.allowed = self.allowed.saturating_add(1);
            return ToolDecision::Allow;
        }

        let request_id = format!("sandbox-{}", call.id);
        if !self
            .pending
            .iter()
            .any(|pending| pending.call_id == call.id)
        {
            self.pending.push(PendingApproval {
                request_id: request_id.clone(),
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                summary: approval_summary(&call),
            });
        }
        ToolDecision::RequireApproval {
            request_id,
            reason: format!("工具 `{}` 需要用户审批", call.name),
            poll_interval_ms: 100,
        }
    }

    fn after_tool(&mut self, _result: ToolResult) {
        self.completed = self.completed.saturating_add(1);
    }

    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![UiDeclaration {
            plugin_id: String::new(),
            view_id: APPROVAL_VIEW.into(),
            title: "Agent 工具审批".into(),
            placement: UiPlacement::Dialog,
            size: UiSize {
                width: Some(68),
                height: Some(12),
            },
            focusable: true,
        }]
    }

    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        if request.view_id != APPROVAL_VIEW {
            return None;
        }
        let Some(pending) = self.pending.first() else {
            return Some(UiFrame {
                view_id: APPROVAL_VIEW.into(),
                visible: false,
                lines: Vec::new(),
            });
        };

        Some(UiFrame {
            view_id: APPROVAL_VIEW.into(),
            visible: true,
            lines: vec![
                styled_line("Agent 请求执行受控工具", UiColor::Yellow, true),
                plain_line(format!("工具：{}", pending.tool_name)),
                plain_line(format!("请求：{}", pending.request_id)),
                plain_line(format!("摘要：{}", pending.summary)),
                plain_line(""),
                styled_line("Enter 允许一次    Esc/D 拒绝", UiColor::Cyan, false),
                plain_line(format!(
                    "已允许 {} · 已拒绝 {} · 已完成 {}",
                    self.allowed, self.denied, self.completed
                )),
            ],
        })
    }

    fn on_ui_input(&mut self, input: UiInput) {
        if input.view_id != APPROVAL_VIEW || self.pending.is_empty() {
            return;
        }
        let resolution = match input.event {
            UiInputEvent::Key { ref code, .. } if code == "enter" => ApprovalResolution::Allow,
            UiInputEvent::Key { ref code, .. }
                if code == "escape" || code.eq_ignore_ascii_case("d") =>
            {
                ApprovalResolution::Deny
            }
            _ => return,
        };
        let pending = self.pending.remove(0);
        self.resolutions.insert(pending.call_id, resolution);
    }
}

/// 对已知只读工具执行路径检查；敏感路径永不进入审批流程。
fn blocked_reason(call: &ToolCall) -> Option<String> {
    if call.name == "shell" {
        return None;
    }
    let path = call.args.get("path").and_then(|value| value.as_str())?;
    if !is_safe_relative_path(path) {
        return Some(format!("沙盒拒绝访问工作区外路径：{path}"));
    }
    if contains_sensitive_segment(path) {
        return Some("沙盒拒绝访问敏感文件或凭据目录".into());
    }
    None
}

/// 判断工具是否需要用户逐次确认；未知插件工具按有副作用处理。
fn requires_approval(call: &ToolCall) -> bool {
    !matches!(
        call.name.as_str(),
        "read_file" | "list_directory" | "search_files"
    )
}

/// 生成不包含文件内容或密钥值的审批摘要。
fn approval_summary(call: &ToolCall) -> String {
    match call.name.as_str() {
        "write_file" => call
            .args
            .get("path")
            .and_then(|value| value.as_str())
            .map(|path| format!("写入 {path}"))
            .unwrap_or_else(|| "写入文件".into()),
        "shell" => call
            .args
            .get("command")
            .and_then(|value| value.as_str())
            .map(redact_command)
            .unwrap_or_else(|| "执行 Shell 命令".into()),
        _ => format!("调用插件工具 {}", call.name),
    }
}

/// 仅允许当前工作区内的词法相对路径，拒绝绝对路径和父目录穿越。
fn is_safe_relative_path(path: &str) -> bool {
    !path.trim().is_empty()
        && std::path::Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// 判断路径是否包含常见密钥、身份或版本库内部文件。
fn contains_sensitive_segment(path: &str) -> bool {
    std::path::Path::new(path).components().any(|component| {
        let Component::Normal(segment) = component else {
            return false;
        };
        let name = segment.to_string_lossy().to_ascii_lowercase();
        name == ".git"
            || name == ".ssh"
            || name == ".gnupg"
            || name == ".aws"
            || name == ".config"
            || name == "credentials"
            || name == "id_rsa"
            || name == "id_ed25519"
            || name.starts_with(".env")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.ends_with(".p12")
    })
}

/// 截断命令预览并隐藏明显的凭据赋值，避免审批 UI 二次暴露密钥。
fn redact_command(command: &str) -> String {
    let preview = if command.chars().count() > 120 {
        format!("{}...", command.chars().take(120).collect::<String>())
    } else {
        command.to_string()
    };
    if preview.to_ascii_lowercase().contains("token=")
        || preview.to_ascii_lowercase().contains("api_key=")
        || preview.to_ascii_lowercase().contains("password=")
    {
        "执行包含敏感参数的 Shell 命令".into()
    } else {
        format!("执行：{preview}")
    }
}

fn plain_line(text: impl Into<String>) -> UiLine {
    UiLine {
        spans: vec![UiSpan {
            text: text.into(),
            style: UiStyle::default(),
        }],
    }
}

fn styled_line(text: impl Into<String>, color: UiColor, bold: bool) -> UiLine {
    UiLine {
        spans: vec![UiSpan {
            text: text.into(),
            style: UiStyle {
                foreground: Some(color),
                bold,
                ..UiStyle::default()
            },
        }],
    }
}

export_plugin!(SandboxPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 敏感文件读取必须直接拒绝，不能通过审批临时放行。
    #[test]
    fn blocks_sensitive_file_reads() {
        let mut plugin = SandboxPlugin::default();
        let decision = plugin.before_tool(ToolCall::new(
            "read-secret",
            "read_file",
            json!({"path": ".env"}),
        ));
        assert!(matches!(decision, ToolDecision::Block { .. }));
    }

    /// 写入请求应等待 UI 决策，批准后同一调用只放行一次。
    #[test]
    fn approves_write_once_from_ui() {
        let call = ToolCall::new("write-1", "write_file", json!({"path": "src/lib.rs"}));
        let mut plugin = SandboxPlugin::default();
        assert!(matches!(
            plugin.before_tool(call.clone()),
            ToolDecision::RequireApproval { .. }
        ));
        plugin.on_ui_input(UiInput {
            plugin_id: "sandbox".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        });
        assert_eq!(plugin.before_tool(call), ToolDecision::Allow);
    }

    /// 工作区外路径必须在工具执行前被拒绝。
    #[test]
    fn blocks_parent_path_escape() {
        let mut plugin = SandboxPlugin::default();
        let decision = plugin.before_tool(ToolCall::new(
            "escape",
            "write_file",
            json!({"path": "../important.txt"}),
        ));
        assert!(matches!(decision, ToolDecision::Block { .. }));
    }
}
