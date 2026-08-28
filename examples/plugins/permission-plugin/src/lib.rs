//! Agent 工具策略与交互审批插件。

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, ExtensionEvent, ModelCompletionRequest,
    ModelCompletionResponse, PluginHostApi, ToolCall, ToolDecision, ToolDecisionStatus, UiColor,
    UiDeclaration, UiFrame, UiHostAction, UiHostActionRequest, UiInput, UiInputEvent, UiLine,
    UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Component,
};

const APPROVAL_VIEW: &str = "permission-approval";
const MODE_VIEW: &str = "permission-mode";
const PERMISSION_COMMAND: &str = "/permission";
const PERMISSION_MODE_STATE_KEY: &str = "permission_mode";

/// 权限插件对有副作用工具调用采用的默认处理模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PermissionMode {
    /// 每次通过 TUI 由用户决定。
    #[default]
    Manual,
    /// 使用 Host 固定路由的受限小模型进行二元安全审核。
    ModelReview,
    /// 自动允许普通副作用调用；硬安全边界仍会先行拒绝。
    AllowAll,
}

impl PermissionMode {
    /// 返回跨插件重启使用的稳定存储值；该值独立于界面文案和命令别名。
    fn storage_value(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::ModelReview => "model_review",
            Self::AllowAll => "allow_all",
        }
    }

    /// 从插件状态恢复模式；未知值返回 `None`，由调用方安全回到手动审批。
    fn from_storage_value(value: &serde_json::Value) -> Option<Self> {
        match value.as_str()? {
            "manual" => Some(Self::Manual),
            "model_review" => Some(Self::ModelReview),
            "allow_all" => Some(Self::AllowAll),
            _ => None,
        }
    }
}

/// 单次工具审批的最终选择。
#[derive(Debug, Clone)]
enum ApprovalResolution {
    Allow,
    Cancel(String),
}

/// 等待用户处理的一次工具调用。
#[derive(Debug, Clone)]
struct PendingApproval {
    call_id: String,
    tool_name: String,
    summary: String,
    /// 审批卡片展示的操作原因，不包含模型输入或敏感参数。
    reason: &'static str,
    /// 审批卡片展示的宿主资源范围。
    scope: &'static str,
    rule_key: String,
}

/// 权限插件状态；策略只检查调用参数，不持有宿主文件、进程或网络能力。
#[derive(Default)]
pub struct PermissionPlugin {
    pending: Vec<PendingApproval>,
    resolutions: BTreeMap<String, ApprovalResolution>,
    allowed_similar: BTreeSet<String>,
    mode: PermissionMode,
    selected: usize,
    mode_selected: usize,
    command_input: String,
    next_action_id: u64,
    review_notice: Option<String>,
    mode_notice: Option<String>,
}

impl AgentPlugin for PermissionPlugin {
    /// 激活时恢复 Host 为当前插件隔离保存的权限模式；无状态时保持最安全的手动审批。
    fn activate(
        &mut self,
        host: &dyn PluginHostApi,
        _context: ActivationContext,
    ) -> agent_plugin::Result<()> {
        let Some(value) = host.get_state(PERMISSION_MODE_STATE_KEY)? else {
            return Ok(());
        };
        let mode = PermissionMode::from_storage_value(&value).unwrap_or(PermissionMode::Manual);
        self.set_mode(mode);
        self.mode_selected = permission_mode_index(mode);
        Ok(())
    }

    fn before_tool(&mut self, call: ToolCall) -> ToolDecisionStatus {
        if let Some(resolution) = self.resolutions.remove(&call.id) {
            return match resolution {
                ApprovalResolution::Allow => ToolDecision::Allow.into(),
                ApprovalResolution::Cancel(reason) => ToolDecision::CancelRun { reason }.into(),
            };
        }

        if let Some(reason) = blocked_reason(&call) {
            return ToolDecision::Block { reason }.into();
        }

        if !requires_approval(&call) {
            return ToolDecision::Allow.into();
        }

        let rule_key = approval_rule_key(&call);
        if self.mode == PermissionMode::AllowAll || self.allowed_similar.contains(&rule_key) {
            return ToolDecision::Allow.into();
        }

        if !self
            .pending
            .iter()
            .any(|pending| pending.call_id == call.id)
        {
            self.pending.push(PendingApproval {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                summary: approval_summary(&call),
                reason: approval_reason(&call),
                scope: approval_scope(&call),
                rule_key,
            });
        }
        ToolDecisionStatus::Pending {
            retry_after_ms: 100,
        }
    }

    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: APPROVAL_VIEW.into(),
                title: "Permission".into(),
                placement: UiPlacement::Input,
                size: UiSize {
                    width: None,
                    height: Some(14),
                },
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: MODE_VIEW.into(),
                title: "权限模式".into(),
                placement: UiPlacement::InputPanel,
                size: UiSize {
                    width: None,
                    height: Some(4),
                },
                focusable: true,
                input_triggers: vec![PERMISSION_COMMAND.into()],
            },
        ]
    }

    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        if request.view_id == MODE_VIEW {
            return Some(UiFrame {
                view_id: MODE_VIEW.into(),
                visible: self
                    .command_input
                    .trim_start()
                    .starts_with(PERMISSION_COMMAND),
                lines: permission_mode_lines(
                    self.mode,
                    self.mode_selected,
                    self.mode_notice.as_deref(),
                ),
                cursor: None,
            });
        }
        if request.view_id != APPROVAL_VIEW {
            return None;
        }
        let Some(pending) = self.pending.first() else {
            return Some(UiFrame {
                view_id: APPROVAL_VIEW.into(),
                visible: false,
                lines: Vec::new(),
                cursor: None,
            });
        };

        Some(UiFrame {
            view_id: APPROVAL_VIEW.into(),
            visible: true,
            lines: approval_lines(pending, self.selected, self.review_notice.as_deref()),
            cursor: None,
        })
    }

    /// 模型代审模式在渲染审批卡片前完成一次受限审核；失败时退回手动审批。
    fn render_ui_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        request: UiRenderRequest,
    ) -> Option<UiFrame> {
        if request.view_id == APPROVAL_VIEW
            && self.mode == PermissionMode::ModelReview
            && !self.pending.is_empty()
        {
            self.review_current(host);
        }
        self.render_ui(request)
    }

    fn on_ui_input(&mut self, input: UiInput) {
        if input.view_id != APPROVAL_VIEW || self.pending.is_empty() {
            return;
        }
        let UiInputEvent::Key { code, modifiers } = input.event else {
            return;
        };
        match code.as_str() {
            "up" => {
                self.selected = self.selected.checked_sub(1).unwrap_or(2);
            }
            "down" | "tab" => {
                self.selected = (self.selected + 1) % 3;
            }
            "a" | "A" | "y" | "Y" if modifiers.is_empty() => self.resolve_current(0),
            "s" | "S" if modifiers.is_empty() => self.resolve_current(1),
            "d" | "D" | "c" | "C" if modifiers.is_empty() => self.resolve_current(2),
            "escape" => self.resolve_current(2),
            "enter" => self.resolve_current(self.selected),
            _ => {}
        }
    }

    /// 处理 `/permission` 命令面板，并通过通用宿主动作清空已经消费的主输入。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        if input.view_id != MODE_VIEW {
            self.on_approval_input_with_host(host, input);
            return;
        }
        match &input.event {
            UiInputEvent::MainInput { text, .. } if self.pending.is_empty() => {
                self.command_input = text.clone();
                if let Some(mode) = parse_permission_mode_argument(text) {
                    self.mode_selected = permission_mode_index(mode);
                }
            }
            UiInputEvent::Key { code, .. } if self.pending.is_empty() => match code.as_str() {
                "up" | "left" => {
                    self.mode_selected = self.mode_selected.checked_sub(1).unwrap_or(2);
                }
                "down" | "right" | "tab" => {
                    self.mode_selected = (self.mode_selected + 1) % 3;
                }
                "1" => self.finish_mode_command(host, PermissionMode::Manual),
                "2" => self.finish_mode_command(host, PermissionMode::ModelReview),
                "3" => self.finish_mode_command(host, PermissionMode::AllowAll),
                "enter" => {
                    let mode = parse_permission_mode_argument(&self.command_input)
                        .unwrap_or_else(|| permission_mode_at(self.mode_selected));
                    self.finish_mode_command(host, mode);
                }
                "escape" => self.clear_command_input(host),
                _ => {}
            },
            _ => self.on_ui_input(input),
        }
    }
}

impl PermissionPlugin {
    /// 处理真实 WASM 宿主路由的审批按键；审批卡片仅处理本次、会话同类或拒绝。
    fn on_approval_input_with_host(&mut self, _host: &dyn PluginHostApi, input: UiInput) {
        self.on_ui_input(input);
    }

    /// 处理当前审批并重置下一条请求的默认选项；会话授权只记录保守的同类规则键。
    fn resolve_current(&mut self, action: usize) {
        if self.pending.is_empty() {
            return;
        }
        let pending = self.pending.remove(0);
        let resolution = match action {
            0 => ApprovalResolution::Allow,
            1 => {
                self.allowed_similar.insert(pending.rule_key);
                ApprovalResolution::Allow
            }
            _ => ApprovalResolution::Cancel(format!(
                "用户拒绝工具 `{}`，Agent 运行已暂停",
                pending.tool_name
            )),
        };
        self.resolutions.insert(pending.call_id, resolution);
        self.selected = 0;
        self.review_notice = None;
    }

    /// 切换默认权限模式；模式变化会清除旧的“允许相似”临时授权，避免手动模式被历史规则绕过。
    fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
        self.allowed_similar.clear();
        self.review_notice = None;
        self.mode_notice = None;
    }

    /// 使用 Host 固定模型路由审核当前调用，仅接受精确的 `ALLOW` 或 `DENY` 结论。
    fn review_current(&mut self, host: &dyn PluginHostApi) {
        self.review_current_with_state(
            |request| host.complete_model(request),
            |mode| {
                host.set_state(
                    PERMISSION_MODE_STATE_KEY,
                    &serde_json::Value::String(mode.storage_value().into()),
                )
            },
        );
    }

    /// 执行一次可测试的受限审核；完成器错误或协议无效时安全退回人工审批。
    #[cfg(test)]
    fn review_current_with(
        &mut self,
        complete: impl FnOnce(&ModelCompletionRequest) -> agent_plugin::Result<ModelCompletionResponse>,
    ) {
        self.review_current_with_state(complete, |_| Ok(()));
    }

    /// 执行模型审核并在安全回退时同步持久状态；保存失败不阻止当前运行退回手动审批。
    fn review_current_with_state(
        &mut self,
        complete: impl FnOnce(&ModelCompletionRequest) -> agent_plugin::Result<ModelCompletionResponse>,
        persist: impl FnOnce(PermissionMode) -> agent_plugin::Result<()>,
    ) {
        let Some(pending) = self.pending.first().cloned() else {
            return;
        };
        let request = ModelCompletionRequest {
            system: Some(
                "你是工具调用安全审核器。只根据工具名称和脱敏摘要判断是否可执行。\
                 仅输出 ALLOW 或 DENY，不得输出解释。信息不足、命令可破坏数据、扩大权限或访问凭据时输出 DENY。"
                    .into(),
            ),
            messages: vec![serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("工具：{}\n脱敏摘要：{}", pending.tool_name, pending.summary)
                }]
            })],
            max_tokens: Some(8),
        };
        let fallback_notice = match complete(&request) {
            Ok(response) => match parse_review_decision(&response.text) {
                Some(true) => {
                    self.resolve_current(0);
                    return;
                }
                Some(false) => {
                    self.deny_current("安全审核模型拒绝了该工具调用，Agent 运行已暂停".into());
                    return;
                }
                None => "模型返回了无效结论，已退回手动审批".to_string(),
            },
            Err(error) => format!("模型审核失败，已退回手动审批：{error}"),
        };
        let persist_error = persist(PermissionMode::Manual).err();
        self.set_mode(PermissionMode::Manual);
        self.mode_selected = permission_mode_index(PermissionMode::Manual);
        self.review_notice = Some(match persist_error {
            Some(error) => format!("{fallback_notice}；保存回退模式失败：{error}"),
            None => fallback_notice,
        });
    }

    /// 将命令选择写入 Host 后再更新内存模式；失败时保留命令输入供用户重试。
    fn persist_command_mode(&mut self, host: &dyn PluginHostApi, mode: PermissionMode) -> bool {
        self.persist_command_mode_with(mode, |mode| {
            host.set_state(
                PERMISSION_MODE_STATE_KEY,
                &serde_json::Value::String(mode.storage_value().into()),
            )
        })
    }

    /// 使用可测试的状态写入切换命令模式，保证持久提交早于内存状态变更。
    fn persist_command_mode_with(
        &mut self,
        mode: PermissionMode,
        persist: impl FnOnce(PermissionMode) -> agent_plugin::Result<()>,
    ) -> bool {
        if let Err(error) = persist(mode) {
            self.mode_notice = Some(format!("保存权限模式失败，当前设置未改变：{error}"));
            return false;
        }
        self.set_mode(mode);
        self.mode_selected = permission_mode_index(mode);
        true
    }

    /// 使用明确原因拒绝当前请求，供模型审核与后续可信策略复用。
    fn deny_current(&mut self, reason: String) {
        let Some(pending) = self.pending.first().cloned() else {
            return;
        };
        self.pending.remove(0);
        self.resolutions
            .insert(pending.call_id, ApprovalResolution::Cancel(reason));
        self.selected = 0;
        self.review_notice = None;
    }

    /// 应用命令选择的权限模式，并请求宿主清空命令文本。
    fn finish_mode_command(&mut self, host: &dyn PluginHostApi, mode: PermissionMode) {
        if self.persist_command_mode(host, mode) {
            self.clear_command_input(host);
        }
    }

    /// 清空权限命令输入；宿主动作失败时保留原输入，便于用户重试或退出。
    fn clear_command_input(&mut self, host: &dyn PluginHostApi) {
        self.next_action_id = self.next_action_id.saturating_add(1);
        let request = UiHostActionRequest {
            request_id: format!("permission-command-{}", self.next_action_id),
            action: UiHostAction::SetInput {
                text: String::new(),
                cursor: Some(0),
            },
        };
        if host
            .emit_event(&ExtensionEvent::host_action(request).expect("宿主动作必须可序列化"))
            .is_ok()
        {
            self.command_input.clear();
        }
    }
}

/// 从 `/permission` 命令参数解析权限模式；无参数或未知参数返回 `None` 以保留菜单选择。
fn parse_permission_mode_argument(input: &str) -> Option<PermissionMode> {
    let mut parts = input.split_whitespace();
    let command = parts.next()?;
    if !matches!(command, "/permission" | "/permissions") {
        return None;
    }
    match parts.next()?.to_ascii_lowercase().as_str() {
        "manual" | "手动" | "手动审批" => Some(PermissionMode::Manual),
        "assist" | "review" | "模型" | "帮我审批" => Some(PermissionMode::ModelReview),
        "allow" | "allow-all" | "all" | "全部放行" => Some(PermissionMode::AllowAll),
        _ => None,
    }
}

/// 返回权限模式在二级菜单中的稳定下标。
fn permission_mode_index(mode: PermissionMode) -> usize {
    match mode {
        PermissionMode::Manual => 0,
        PermissionMode::ModelReview => 1,
        PermissionMode::AllowAll => 2,
    }
}

/// 按菜单下标返回权限模式；越界时回到最安全的手动审批。
fn permission_mode_at(index: usize) -> PermissionMode {
    match index {
        1 => PermissionMode::ModelReview,
        2 => PermissionMode::AllowAll,
        _ => PermissionMode::Manual,
    }
}

/// 解析小模型的最小二元协议；任何附加解释都视为无效并退回人工审批。
fn parse_review_decision(text: &str) -> Option<bool> {
    match text.trim().to_ascii_uppercase().as_str() {
        "ALLOW" => Some(true),
        "DENY" => Some(false),
        _ => None,
    }
}

/// 对已知只读工具执行路径检查；敏感路径永不进入审批流程。
fn blocked_reason(call: &ToolCall) -> Option<String> {
    if call.name == "shell" {
        return None;
    }
    let path = call.args.get("path").and_then(|value| value.as_str())?;
    if !is_safe_relative_path(path) {
        return Some(format!(
            "Permission policy denied a path outside the workspace: {path}"
        ));
    }
    if contains_sensitive_segment(path) {
        return Some(
            "Permission policy denied access to a sensitive file or credential directory".into(),
        );
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

/// 构造“允许相似调用”使用的保守规则键。
fn approval_rule_key(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" => {
            let command = call
                .args
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            format!("shell:{}", shell_command_family(command))
        }
        "write_file" => {
            let parent = call
                .args
                .get("path")
                .and_then(|value| value.as_str())
                .and_then(|path| std::path::Path::new(path).parent())
                .and_then(|path| path.to_str())
                .filter(|path| !path.is_empty())
                .unwrap_or(".");
            format!("write_file:{parent}")
        }
        name => format!("tool:{name}"),
    }
}

/// 提取 Shell 命令的前两个命令族词，避免按完整动态参数过度细分。
fn shell_command_family(command: &str) -> String {
    let family = command
        .split_whitespace()
        .filter(|token| !token.contains('='))
        .take(2)
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if family.is_empty() {
        "unknown".into()
    } else {
        family
    }
}

/// 生成不包含文件内容或密钥值的审批摘要。
fn approval_summary(call: &ToolCall) -> String {
    match call.name.as_str() {
        "write_file" => call
            .args
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| "file".into()),
        "shell" => call
            .args
            .get("command")
            .and_then(|value| value.as_str())
            .map(redact_command)
            .unwrap_or_else(|| "shell command".into()),
        _ => "plugin tool call".into(),
    }
}

/// 返回审批卡片中的静态原因说明，避免把未经信任的参数重复展示为解释文本。
fn approval_reason(call: &ToolCall) -> &'static str {
    match call.name.as_str() {
        "write_file" => "Agent 请求修改当前工作区中的文件。",
        "shell" => "Agent 请求在当前工作区执行该命令。",
        _ => "Agent 请求执行这个插件工具。",
    }
}

/// 返回工具调用触达的宿主资源范围。
fn approval_scope(call: &ToolCall) -> &'static str {
    match call.name.as_str() {
        "write_file" => "filesystem",
        "shell" => "process",
        _ => "plugin",
    }
}

/// 返回审批卡片首行使用的紧凑操作动词。
fn approval_action(tool_name: &str) -> &'static str {
    match tool_name {
        "write_file" => "WRITE",
        "shell" => "RUN",
        _ => "CALL",
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
        "shell command with sensitive arguments".into()
    } else {
        preview
    }
}

/// 构造纵向审批卡片，把脱敏操作详情固定放在动作列表上方。
fn approval_lines(pending: &PendingApproval, selected: usize, notice: Option<&str>) -> Vec<UiLine> {
    let mut lines = vec![
        UiLine {
            spans: vec![
                styled_span(
                    format!("{:<6}", approval_action(&pending.tool_name)),
                    UiColor::Cyan,
                    true,
                ),
                styled_span(&pending.summary, UiColor::White, false),
            ],
        },
        UiLine { spans: Vec::new() },
        UiLine {
            spans: vec![styled_span("Reason", UiColor::Gray, true)],
        },
        UiLine {
            spans: vec![styled_span(pending.reason, UiColor::White, false)],
        },
        UiLine { spans: Vec::new() },
        UiLine {
            spans: vec![
                styled_span("Scope   ", UiColor::Gray, true),
                styled_span(pending.scope, UiColor::White, false),
            ],
        },
        UiLine {
            spans: vec![
                styled_span("Risk    ", UiColor::Gray, true),
                styled_span("●●●○  elevated", UiColor::Yellow, false),
            ],
        },
    ];
    if let Some(notice) = notice {
        lines.push(UiLine {
            spans: vec![styled_span(notice, UiColor::Yellow, false)],
        });
    } else {
        lines.push(UiLine { spans: Vec::new() });
    }
    lines.extend([
        option_line("A", "允许一次", selected == 0, UiColor::Green),
        option_line("S", "本会话允许", selected == 1, UiColor::Cyan),
        option_line("D", "拒绝", selected == 2, UiColor::Red),
        UiLine {
            spans: vec![styled_span(
                "↑↓ 选择   Enter 确认   Esc 拒绝",
                UiColor::Gray,
                false,
            )],
        },
    ]);
    lines
}

/// 构造 `/permission` 二级模式选择面板。
fn permission_mode_lines(
    mode: PermissionMode,
    selected: usize,
    notice: Option<&str>,
) -> Vec<UiLine> {
    let mut lines = vec![
        mode_line(
            "/permission manual",
            "手动审批",
            "每次由你确认",
            mode == PermissionMode::Manual,
            selected == 0,
        ),
        mode_line(
            "/permission assist",
            "帮我审批",
            "受限小模型审核",
            mode == PermissionMode::ModelReview,
            selected == 1,
        ),
        mode_line(
            "/permission allow-all",
            "全部放行",
            "仅保留硬安全边界",
            mode == PermissionMode::AllowAll,
            selected == 2,
        ),
    ];
    if let Some(notice) = notice {
        lines.push(UiLine {
            spans: vec![styled_span(notice, UiColor::Red, false)],
        });
    }
    lines
}

/// 渲染一个纵向审批选项；每个动作使用稳定语义色，选中项增加箭头与加粗。
fn option_line(shortcut: &str, label: &str, selected: bool, color: UiColor) -> UiLine {
    UiLine {
        spans: vec![
            UiSpan {
                text: if selected { "❯ " } else { "  " }.into(),
                style: UiStyle {
                    foreground: Some(UiColor::White),
                    bold: selected,
                    ..UiStyle::default()
                },
            },
            UiSpan {
                text: format!("[{shortcut}] "),
                style: UiStyle {
                    foreground: Some(color),
                    bold: selected,
                    ..UiStyle::default()
                },
            },
            UiSpan {
                text: label.into(),
                style: UiStyle {
                    foreground: Some(if selected { UiColor::White } else { color }),
                    bold: selected,
                    ..UiStyle::default()
                },
            },
        ],
    }
}

/// 渲染命令补全面板风格的权限模式选项，并同时标记当前模式与键盘选择。
fn mode_line(command: &str, label: &str, detail: &str, current: bool, selected: bool) -> UiLine {
    let style = UiStyle {
        foreground: Some(if selected {
            UiColor::White
        } else {
            UiColor::Gray
        }),
        bold: selected,
        reversed: selected,
        ..UiStyle::default()
    };
    UiLine {
        spans: vec![
            UiSpan {
                text: format!("{command:<24}"),
                style: style.clone(),
            },
            UiSpan {
                text: format!("{label} · {detail}"),
                style: style.clone(),
            },
            UiSpan {
                text: if current {
                    "  当前".into()
                } else {
                    String::new()
                },
                style,
            },
        ],
    }
}

/// 构造一段统一风格的声明式 UI 文本。
fn styled_span(text: impl Into<String>, color: UiColor, bold: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(color),
            bold,
            ..UiStyle::default()
        },
    }
}

export_plugin!(PermissionPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 审批界面应在顶部展示脱敏详情，并只保留三项纵向动作。
    #[test]
    fn approval_ui_uses_compact_visual_hierarchy() {
        let mut plugin = PermissionPlugin::default();
        plugin.before_tool(ToolCall::new(
            "shell-ui",
            "shell",
            json!({"command": "cargo test"}),
        ));

        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "permission".into(),
                view_id: APPROVAL_VIEW.into(),
                instance_id: None,
                width: 68,
                height: 14,
                focused: true,
                frame: 1,
            })
            .expect("审批界面应返回可见帧");
        let text = frame
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();

        assert!(frame.visible);
        assert_eq!(frame.lines.len(), 12);
        assert!(text.starts_with("RUN   cargo test"), "{text}");
        assert!(text.contains("Reason"), "{text}");
        assert!(text.contains("Scope   process"), "{text}");
        assert!(text.contains("Risk    ●●●○  elevated"), "{text}");
        assert!(text.contains("❯ [A] 允许一次"), "{text}");
        assert!(text.contains("[S] 本会话允许"), "{text}");
        assert!(text.contains("[D] 拒绝"), "{text}");
        assert!(!text.contains("全部放行"), "{text}");
        assert!(!text.contains("详情"), "{text}");
        assert!(frame
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .all(|span| !span.style.reversed));
    }

    /// 权限命令面板必须展示三种默认策略，并标记当前手动模式。
    #[test]
    fn permission_command_renders_three_modes() {
        let mut plugin = PermissionPlugin {
            command_input: "/permission".into(),
            ..PermissionPlugin::default()
        };

        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "permission".into(),
                view_id: MODE_VIEW.into(),
                instance_id: None,
                width: 68,
                height: 9,
                focused: true,
                frame: 1,
            })
            .expect("权限模式界面应返回可见帧");
        let text = frame
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();

        assert!(frame.visible);
        assert_eq!(frame.lines.len(), 3);
        assert!(text.contains("/permission manual"), "{text}");
        assert!(text.contains("手动审批 · 每次由你确认  当前"), "{text}");
        assert!(text.contains("帮我审批"), "{text}");
        assert!(text.contains("全部放行"), "{text}");
        assert!(frame.lines[0].spans.iter().all(|span| span.style.reversed));
    }

    /// 命令参数必须稳定映射三种权限模式，未知值不得改变选择。
    #[test]
    fn permission_command_parses_mode_arguments() {
        assert_eq!(
            parse_permission_mode_argument("/permission manual"),
            Some(PermissionMode::Manual)
        );
        assert_eq!(
            parse_permission_mode_argument("/permission assist"),
            Some(PermissionMode::ModelReview)
        );
        assert_eq!(
            parse_permission_mode_argument("/permissions allow-all"),
            Some(PermissionMode::AllowAll)
        );
        assert_eq!(parse_permission_mode_argument("/permission unknown"), None);
    }

    /// 权限模式磁盘值必须稳定且未知值安全回到调用方默认策略。
    #[test]
    fn permission_mode_storage_values_are_stable() {
        for (mode, value) in [
            (PermissionMode::Manual, "manual"),
            (PermissionMode::ModelReview, "model_review"),
            (PermissionMode::AllowAll, "allow_all"),
        ] {
            assert_eq!(mode.storage_value(), value);
            assert_eq!(
                PermissionMode::from_storage_value(&json!(value)),
                Some(mode)
            );
        }
        assert_eq!(PermissionMode::from_storage_value(&json!("unknown")), None);
    }

    /// 命令模式保存失败时必须保留原模式，并把错误加入模式面板。
    #[test]
    fn permission_command_keeps_mode_when_state_save_fails() {
        let mut plugin = PermissionPlugin {
            command_input: "/permission allow-all".into(),
            ..PermissionPlugin::default()
        };

        assert!(
            !plugin.persist_command_mode_with(PermissionMode::AllowAll, |_| {
                Err(agent_plugin::anyhow!("状态目录只读"))
            })
        );
        assert_eq!(plugin.mode, PermissionMode::Manual);
        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "permission".into(),
                view_id: MODE_VIEW.into(),
                instance_id: None,
                width: 68,
                height: 4,
                focused: true,
                frame: 1,
            })
            .expect("权限模式界面应返回错误帧");
        assert_eq!(frame.lines.len(), 4);
        assert!(frame.lines[3]
            .spans
            .iter()
            .any(|span| span.text.contains("当前设置未改变")));
    }

    /// 模型代审只接受精确二元结论，避免解释文本被误判为放行。
    #[test]
    fn model_review_requires_exact_decision() {
        assert_eq!(parse_review_decision("ALLOW\n"), Some(true));
        assert_eq!(parse_review_decision("deny"), Some(false));
        assert_eq!(parse_review_decision("ALLOW because it is safe"), None);
        assert_eq!(parse_review_decision(""), None);
    }

    /// 模型代审允许结论必须只放行当前请求，并使用严格受限的短输出请求。
    #[test]
    fn model_review_allows_exact_positive_decision() {
        let call = ToolCall::new("model-allow", "shell", json!({"command": "cargo test"}));
        let mut plugin = PermissionPlugin {
            mode: PermissionMode::ModelReview,
            ..PermissionPlugin::default()
        };
        plugin.before_tool(call.clone());

        plugin.review_current_with(|request| {
            assert_eq!(request.max_tokens, Some(8));
            assert!(request
                .system
                .as_deref()
                .is_some_and(|text| text.contains("ALLOW")));
            Ok(ModelCompletionResponse {
                text: "ALLOW".into(),
                usage: None,
            })
        });

        assert_eq!(
            plugin.before_tool(call),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
    }

    /// 模型代审拒绝结论必须取消当前运行，不能仅从待审批队列移除请求。
    #[test]
    fn model_review_denial_cancels_run() {
        let call = ToolCall::new("model-deny", "shell", json!({"command": "rm -rf build"}));
        let mut plugin = PermissionPlugin {
            mode: PermissionMode::ModelReview,
            ..PermissionPlugin::default()
        };
        plugin.before_tool(call.clone());

        plugin.review_current_with(|_| {
            Ok(ModelCompletionResponse {
                text: "DENY".into(),
                usage: None,
            })
        });

        assert!(matches!(
            plugin.before_tool(call),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::CancelRun { .. }
            }
        ));
    }

    /// 模型调用失败时必须保留待审批请求并自动退回手动模式。
    #[test]
    fn model_review_failure_falls_back_to_manual() {
        let call = ToolCall::new("model-error", "shell", json!({"command": "cargo test"}));
        let mut plugin = PermissionPlugin {
            mode: PermissionMode::ModelReview,
            ..PermissionPlugin::default()
        };
        plugin.before_tool(call.clone());

        plugin.review_current_with(|_| Err(agent_plugin::anyhow!("测试模型不可用")));

        assert_eq!(plugin.mode, PermissionMode::Manual);
        assert_eq!(plugin.pending.len(), 1);
        assert!(plugin
            .review_notice
            .as_deref()
            .is_some_and(|text| text.contains("已退回手动审批")));
        assert!(matches!(
            plugin.before_tool(call),
            ToolDecisionStatus::Pending { .. }
        ));
    }

    /// 模型审核回退必须尝试保存手动模式，保存失败也不能阻止当前运行收紧权限。
    #[test]
    fn model_review_fallback_is_safe_when_state_save_fails() {
        let call = ToolCall::new(
            "model-state-error",
            "shell",
            json!({"command": "cargo test"}),
        );
        let mut plugin = PermissionPlugin {
            mode: PermissionMode::ModelReview,
            ..PermissionPlugin::default()
        };
        plugin.before_tool(call);
        let mut persisted = None;

        plugin.review_current_with_state(
            |_| Err(agent_plugin::anyhow!("测试模型不可用")),
            |mode| {
                persisted = Some(mode);
                Err(agent_plugin::anyhow!("状态目录只读"))
            },
        );

        assert_eq!(persisted, Some(PermissionMode::Manual));
        assert_eq!(plugin.mode, PermissionMode::Manual);
        assert_eq!(plugin.pending.len(), 1);
        assert!(plugin
            .review_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("保存回退模式失败")));
    }

    /// 敏感文件读取必须直接拒绝，不能通过审批临时放行。
    #[test]
    fn blocks_sensitive_file_reads() {
        let mut plugin = PermissionPlugin::default();
        let decision = plugin.before_tool(ToolCall::new(
            "read-secret",
            "read_file",
            json!({"path": ".env"}),
        ));
        assert!(matches!(
            decision,
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Block { .. }
            }
        ));
    }

    /// 写入请求应等待 UI 决策，批准后同一调用只放行一次。
    #[test]
    fn approves_write_once_from_ui() {
        let call = ToolCall::new("write-1", "write_file", json!({"path": "src/lib.rs"}));
        let mut plugin = PermissionPlugin::default();
        assert!(matches!(
            plugin.before_tool(call.clone()),
            ToolDecisionStatus::Pending { .. }
        ));
        plugin.on_ui_input(UiInput {
            plugin_id: "permission".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        });
        assert_eq!(
            plugin.before_tool(call),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
    }

    /// 工作区外路径必须在工具执行前被拒绝。
    #[test]
    fn blocks_parent_path_escape() {
        let mut plugin = PermissionPlugin::default();
        let decision = plugin.before_tool(ToolCall::new(
            "escape",
            "write_file",
            json!({"path": "../important.txt"}),
        ));
        assert!(matches!(
            decision,
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Block { .. }
            }
        ));
    }

    /// 方向键切换到取消后，Enter 应暂停当前 Agent 运行。
    #[test]
    fn arrow_selection_cancels_run() {
        let call = ToolCall::new("shell-1", "shell", json!({"command": "cargo test"}));
        let mut plugin = PermissionPlugin::default();
        assert!(matches!(
            plugin.before_tool(call.clone()),
            ToolDecisionStatus::Pending { .. }
        ));
        for code in ["down", "down", "enter"] {
            plugin.on_ui_input(UiInput {
                plugin_id: "permission".into(),
                view_id: APPROVAL_VIEW.into(),
                instance_id: None,
                event: UiInputEvent::Key {
                    code: code.into(),
                    modifiers: Vec::new(),
                },
            });
        }
        assert!(matches!(
            plugin.before_tool(call),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::CancelRun { .. }
            }
        ));
    }

    /// A 快捷键应无需移动选择直接允许一次。
    #[test]
    fn a_shortcut_approves_once() {
        let call = ToolCall::new("shell-a", "shell", json!({"command": "cargo check"}));
        let mut plugin = PermissionPlugin::default();
        plugin.before_tool(call.clone());
        plugin.on_ui_input(UiInput {
            plugin_id: "permission".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "a".into(),
                modifiers: Vec::new(),
            },
        });
        assert_eq!(
            plugin.before_tool(call),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
    }

    /// D 快捷键必须直接拒绝当前调用并暂停 Agent 运行。
    #[test]
    fn d_shortcut_cancels_run() {
        let call = ToolCall::new("shell-d", "shell", json!({"command": "cargo check"}));
        let mut plugin = PermissionPlugin::default();
        plugin.before_tool(call.clone());
        plugin.on_ui_input(UiInput {
            plugin_id: "permission".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "d".into(),
                modifiers: Vec::new(),
            },
        });

        assert!(matches!(
            plugin.before_tool(call),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::CancelRun { .. }
            }
        ));
    }

    /// 带修饰键的 A 不得触发审批，避免终端选择快捷键意外放行工具。
    #[test]
    fn modified_a_does_not_approve() {
        let call = ToolCall::new("shell-meta-a", "shell", json!({"command": "cargo check"}));
        let mut plugin = PermissionPlugin::default();
        plugin.before_tool(call.clone());
        plugin.on_ui_input(UiInput {
            plugin_id: "permission".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "a".into(),
                modifiers: vec!["meta".into()],
            },
        });

        assert!(matches!(
            plugin.before_tool(call),
            ToolDecisionStatus::Pending { .. }
        ));
    }

    /// 允许相似 Shell 调用只应放行同一命令族。
    #[test]
    fn similar_mode_matches_shell_command_family() {
        let first = ToolCall::new("first", "shell", json!({"command": "cargo test -p one"}));
        let similar = ToolCall::new("second", "shell", json!({"command": "cargo test -p two"}));
        let different = ToolCall::new("third", "shell", json!({"command": "cargo check"}));
        let mut plugin = PermissionPlugin::default();
        plugin.before_tool(first.clone());
        plugin.on_ui_input(UiInput {
            plugin_id: "permission".into(),
            view_id: APPROVAL_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "s".into(),
                modifiers: Vec::new(),
            },
        });

        assert_eq!(
            plugin.before_tool(first),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
        assert_eq!(
            plugin.before_tool(similar),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
        assert!(matches!(
            plugin.before_tool(different),
            ToolDecisionStatus::Pending { .. }
        ));
    }

    /// 通过权限模式开启全部放行后仍不得绕过敏感路径硬拒绝。
    #[test]
    fn allow_all_keeps_sensitive_path_blocked() {
        let shell = ToolCall::new("shell-all", "shell", json!({"command": "cargo test"}));
        let write = ToolCall::new(
            "write-all",
            "write_file",
            json!({"path": "src/lib.rs", "content": ""}),
        );
        let secret = ToolCall::new("secret-all", "read_file", json!({"path": ".env"}));
        let mut plugin = PermissionPlugin::default();
        plugin.set_mode(PermissionMode::AllowAll);

        assert_eq!(
            plugin.before_tool(shell),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
        assert_eq!(
            plugin.before_tool(write),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Allow
            }
        );
        assert!(matches!(
            plugin.before_tool(secret),
            ToolDecisionStatus::Ready {
                decision: ToolDecision::Block { .. }
            }
        ));
    }
}
