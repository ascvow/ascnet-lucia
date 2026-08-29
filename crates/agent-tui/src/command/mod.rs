//! Lucia TUI 原生斜杠命令、补全面板和会话对话框。

#[cfg(feature = "plugins")]
mod plugin_dialog;
mod render;
mod session_dialog;
#[cfg(test)]
mod tests;

#[cfg(feature = "plugins")]
pub(crate) use render::render_native_plugin_dialog;
pub(crate) use render::{render_native_command_panel, render_native_session_dialog};

use crate::app::App;
use crate::event::UiEvent;
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementService;
use crate::session::conversation::{Msg, MsgKind};
use agent_core::model::ReasoningLevel;
use agent_core::Agent;
use agent_session::{SessionId, SessionSummary};
use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

/// 原生命令面板最多展示的候选数。
const MAX_VISIBLE_COMMANDS: usize = 12;

/// 会话对话框一次展示的最大摘要数，避免大型历史目录占满终端。
const MAX_VISIBLE_SESSIONS: usize = 100;

/// 一条稳定的原生命令定义。
#[derive(Debug, Clone, Copy)]
struct NativeCommandSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    description: &'static str,
    idle_only: bool,
}

/// 统一斜杠补全候选；插件仅提供声明式触发前缀，不进入原生命令执行路由。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashCommandMatch {
    /// 不含前导斜杠的规范命令名。
    name: String,
    /// 补全面板展示的简短用途。
    summary: String,
    /// 是否由 TUI 原生命令状态机执行。
    native: bool,
}

/// Lucia 默认提供的全部斜杠命令。
const NATIVE_COMMANDS: &[NativeCommandSpec] = &[
    NativeCommandSpec {
        name: "new",
        aliases: &[],
        summary: "新建会话",
        description: "结束当前会话并进入尚未持久化的空白草稿。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "resume",
        aliases: &[],
        summary: "恢复历史会话",
        description: "打开当前项目的会话列表，并恢复选中的完整会话。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "sessions",
        aliases: &[],
        summary: "切换项目会话",
        description: "打开当前项目的会话摘要列表，并切换至选中的完整会话。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "compact",
        aliases: &[],
        summary: "立即压缩上下文",
        description: "请求当前 Context Loader 压缩旧会话并持久化结果。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "clear",
        aliases: &[],
        summary: "清空当前上下文",
        description: "保留历史会话文件，并进入新的空白草稿。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "reasoning",
        aliases: &["think", "effort", "effo"],
        summary: "查看或切换思考等级",
        description: "用法：/reasoning [off|minimal|low|medium|high|xhigh|max|next]；不带参数时打开等级选择界面。",
        idle_only: false,
    },
    #[cfg(feature = "plugins")]
    NativeCommandSpec {
        name: "plugins",
        aliases: &[],
        summary: "管理固定插件依赖",
        description: "打开人工插件管理页；变更创建新基线，当前会话不热重载。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "help",
        aliases: &[],
        summary: "查看命令帮助",
        description: "显示全部原生命令，或显示指定命令的详细用法。",
        idle_only: false,
    },
    NativeCommandSpec {
        name: "exit",
        aliases: &["quit"],
        summary: "退出 Lucia",
        description: "正常退出 TUI，不修改历史会话。",
        idle_only: true,
    },
];

/// 解析 `/reasoning` 参数，兼容稳定英文值与底栏使用的中文标签。
fn parse_reasoning_level(value: &str) -> Option<ReasoningLevel> {
    match value.to_ascii_lowercase().as_str() {
        "off" | "none" | "关闭" => Some(ReasoningLevel::Off),
        "minimal" | "最小" => Some(ReasoningLevel::Minimal),
        "low" | "低" => Some(ReasoningLevel::Low),
        "medium" | "中" => Some(ReasoningLevel::Medium),
        "high" | "高" => Some(ReasoningLevel::High),
        "xhigh" | "超高" => Some(ReasoningLevel::XHigh),
        "max" | "最大" => Some(ReasoningLevel::Max),
        _ => None,
    }
}

/// 返回可复制到 `/reasoning` 命令中的稳定英文等级值。
pub(crate) fn reasoning_level_command_value(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "off",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
    }
}

/// 会话对话框的入口用途；两种模式都允许切换至选中的会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSessionDialogMode {
    /// 选择后恢复完整会话。
    Resume,
    /// 从项目会话列表浏览并切换。
    Browse,
}

/// 会话列表的异步加载状态。
#[derive(Debug)]
pub(crate) enum NativeSessionDialogStatus {
    /// 正在读取项目会话目录。
    Loading,
    /// 会话摘要已经按更新时间降序加载。
    Ready(Vec<SessionSummary>),
    /// 会话目录读取失败。
    Error(String),
}

/// 原生会话 Dialog 的完整交互状态。
#[derive(Debug)]
pub(crate) struct NativeSessionDialog {
    /// 对话框用途。
    pub(crate) mode: NativeSessionDialogMode,
    /// 当前高亮摘要下标。
    pub(crate) selected: usize,
    /// 异步列表状态。
    pub(crate) status: NativeSessionDialogStatus,
}

/// 思考等级内联选择面板的交互状态。
#[derive(Debug)]
pub(crate) struct NativeReasoningPanel {
    /// 当前高亮等级在 models.dev 可用等级列表中的下标。
    pub(crate) selected: usize,
}

/// 插件管理页加载完成后的只读数据。
#[cfg(feature = "plugins")]
#[derive(Debug, Clone)]
pub(crate) struct NativePluginDialogData {
    /// 全部受管理插件，包含禁用项。
    pub(crate) plugins: Vec<agent_plugin_manager::InstalledPlugin>,
    /// 不参与当前运行时加载的人工安装版本历史。
    pub(crate) version_history: Vec<agent_plugin_manager::InstalledPlugin>,
}

/// 插件管理页的异步状态。
#[cfg(feature = "plugins")]
#[derive(Debug)]
pub(crate) enum NativePluginDialogStatus {
    /// 正在读取 Plugin Manager 状态。
    Loading,
    /// 插件列表已经就绪。
    Ready(NativePluginDialogData),
    /// 正在执行 Manager 写入。
    Applying,
    /// 读取或写入失败；用户可刷新重试。
    Error(String),
}

/// 插件管理页当前接受的表单输入用途。
#[cfg(feature = "plugins")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePluginPathMode {
    /// 输入 Registry `name[@semver]` 请求。
    InstallRegistry,
    /// 输入 GitHub owner/repository 或 URL。
    InstallGithub,
    /// 安装新本地 Bundle。
    Install,
    /// 使用本地 Bundle 替换已安装版本。
    Replace,
    /// 输入插件 ID 和历史版本执行人工回滚。
    Rollback,
    /// 输入 Capability ID 和插件 ID 选择 Owner。
    SelectCapabilityOwner,
    /// 输入 Capability ID 清除 Owner 选择。
    ClearCapabilityOwner,
}

/// 插件管理页的完整交互状态。
#[cfg(feature = "plugins")]
#[derive(Debug)]
pub(crate) struct NativePluginDialog {
    /// 当前高亮插件下标。
    pub(crate) selected: usize,
    /// 异步数据状态。
    pub(crate) status: NativePluginDialogStatus,
    /// 本地 Bundle 路径输入模式；`None` 表示列表导航。
    pub(crate) path_mode: Option<NativePluginPathMode>,
    /// 正在编辑的本地 Bundle 路径。
    pub(crate) path_input: String,
    /// 第一次删除按键记录的插件 ID；再次确认才执行。
    pub(crate) remove_confirmation: Option<String>,
}

/// TUI 原生命令状态；不依赖 Plugin Host，纯 Core 构建同样可用。
#[derive(Debug, Default)]
pub(crate) struct NativeCommandState {
    /// 补全面板当前高亮候选。
    pub(crate) selected: usize,
    /// 用户按 Esc 隐藏面板时的完整输入；输入发生变化后自动失效。
    pub(crate) dismissed_input: Option<String>,
    /// 当前会话对话框。
    pub(crate) dialog: Option<NativeSessionDialog>,
    /// 当前显示在输入框上方的思考等级选择面板。
    pub(crate) reasoning_panel: Option<NativeReasoningPanel>,
    /// 当前人工插件管理页；纯 Core 构建不包含该状态。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_dialog: Option<NativePluginDialog>,
}

impl App {
    /// 返回任一原生全屏 Dialog 是否正在占用键盘焦点；内联面板不计入。
    pub(crate) fn native_dialog_active(&self) -> bool {
        self.native_command.dialog.is_some()
            || cfg!(feature = "plugins") && {
                #[cfg(feature = "plugins")]
                {
                    self.native_command.plugin_dialog.is_some()
                }
                #[cfg(not(feature = "plugins"))]
                {
                    false
                }
            }
    }

    /// 返回当前输入匹配的原生命令与插件斜杠触发器，并保持稳定的功能分组顺序。
    fn slash_command_matches(&self) -> Vec<SlashCommandMatch> {
        let Some(rest) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        let mut matches = NATIVE_COMMANDS
            .iter()
            .filter(|command| {
                command.name.starts_with(rest)
                    || command.aliases.iter().any(|alias| alias.starts_with(rest))
            })
            .map(|command| SlashCommandMatch {
                name: command.name.to_string(),
                summary: command.summary.to_string(),
                native: true,
            })
            .collect::<Vec<_>>();
        #[cfg(feature = "plugins")]
        let mut plugin_matches = Vec::new();
        #[cfg(feature = "plugins")]
        for view in &self.plugin_views {
            for trigger in &view.declaration.input_triggers {
                let Some(name) = trigger.strip_prefix('/') else {
                    continue;
                };
                if name.is_empty()
                    || name.contains(char::is_whitespace)
                    || !name.starts_with(rest)
                    || matches.iter().any(|candidate| candidate.name == name)
                    || plugin_matches
                        .iter()
                        .any(|candidate: &SlashCommandMatch| candidate.name == name)
                {
                    continue;
                }
                plugin_matches.push(SlashCommandMatch {
                    name: name.to_string(),
                    summary: view.declaration.title.clone(),
                    native: false,
                });
            }
        }
        #[cfg(feature = "plugins")]
        {
            // 插件命令属于扩展操作，固定排在原生插件入口之后、帮助与退出之前。
            let insertion = matches
                .iter()
                .position(|candidate| matches!(candidate.name.as_str(), "help" | "exit"))
                .unwrap_or(matches.len());
            matches.splice(insertion..insertion, plugin_matches);
        }
        matches.truncate(MAX_VISIBLE_COMMANDS);
        matches
    }

    /// 返回当前补全面板所需高度；输入不是命令或面板已被显式隐藏时返回零。
    pub(crate) fn native_command_panel_height(&self) -> u16 {
        if self.native_command.reasoning_panel.is_some() {
            return u16::try_from(self.reasoning_levels.len())
                .unwrap_or(u16::MAX)
                .saturating_add(1);
        }
        #[cfg(feature = "plugins")]
        if self.plugin_slash_trigger_is_exact() {
            return 0;
        }
        if self.native_command.dismissed_input.as_deref() == Some(self.input.as_str()) {
            return 0;
        }
        let count = u16::try_from(self.slash_command_matches().len())
            .unwrap_or(u16::MAX)
            .min(MAX_VISIBLE_COMMANDS as u16);
        count.saturating_add(u16::from(count > 0))
    }

    /// 判断主输入是否精确命中插件斜杠触发器；命中后应让位给插件二级面板。
    #[cfg(feature = "plugins")]
    fn plugin_slash_trigger_is_exact(&self) -> bool {
        let input = self.input.trim();
        self.plugin_views.iter().any(|view| {
            view.declaration
                .input_triggers
                .iter()
                .any(|trigger| trigger.starts_with('/') && trigger == input)
        })
    }

    /// 主输入变化后重置补全选择，并让此前按 Esc 隐藏的面板重新参与匹配。
    pub(crate) fn sync_native_command_input(&mut self, previous_input: &str) {
        if self.input == previous_input {
            return;
        }
        self.native_command.selected = 0;
        if self.native_command.dismissed_input.as_deref() != Some(self.input.as_str()) {
            self.native_command.dismissed_input = None;
        }
    }

    /// 优先处理原生命令补全面板或会话 Dialog 的键盘输入。
    ///
    /// 返回 `true` 表示事件已被原生状态机消费，调用方不得再转发给插件或主编辑器。
    pub(crate) fn handle_native_command_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) -> bool {
        if !matches!(code, KeyCode::Esc) {
            self.last_escape_at = None;
        }
        if self.native_command.dialog.is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return self.handle_native_session_dialog_key(code, modifiers);
        }
        if self.native_command.reasoning_panel.is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return self.handle_native_reasoning_panel_key(code, modifiers, agent);
        }
        #[cfg(feature = "plugins")]
        if self.native_command.plugin_dialog.is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return self.handle_native_plugin_dialog_key(code, modifiers);
        }
        #[cfg(feature = "plugins")]
        if !self.view_stack.is_main()
            || self.active_dialog_index().is_some()
            || self.visible_plugin_input().is_some()
            || self.plugin_focus.is_some()
        {
            return false;
        }
        if !self.input.starts_with('/') || !modifiers.is_empty() {
            return false;
        }
        if self.native_command.dismissed_input.as_deref() == Some(self.input.as_str()) {
            return false;
        }
        #[cfg(feature = "plugins")]
        if self.plugin_slash_trigger_is_exact() {
            return false;
        }
        let matches = self.slash_command_matches();
        match code {
            KeyCode::Up if !matches.is_empty() => {
                self.native_command.selected = self
                    .native_command
                    .selected
                    .checked_sub(1)
                    .unwrap_or(matches.len() - 1);
                true
            }
            KeyCode::Down if !matches.is_empty() => {
                self.native_command.selected = (self.native_command.selected + 1) % matches.len();
                true
            }
            KeyCode::Tab if !matches.is_empty() => {
                let selected = self.native_command.selected.min(matches.len() - 1);
                self.input = format!("/{} ", matches[selected].name);
                self.cursor = self.input.len();
                self.native_command.selected = 0;
                self.native_command.dismissed_input = None;
                true
            }
            KeyCode::Enter => {
                #[cfg(feature = "plugins")]
                if matches.is_empty() && self.active_trigger_view().is_some() {
                    return false;
                }
                if !matches.is_empty() {
                    let selected = self.native_command.selected.min(matches.len() - 1);
                    self.input = format!("/{}", matches[selected].name);
                    self.cursor = self.input.len();
                    if !matches[selected].native {
                        return true;
                    }
                }
                self.execute_native_command(agent);
                true
            }
            KeyCode::Esc if !matches.is_empty() => {
                self.last_escape_at = None;
                self.native_command.dismissed_input = Some(self.input.clone());
                true
            }
            _ => false,
        }
    }

    /// 执行当前输入中的一个原生命令，并保证命令文本不会作为用户消息发送给模型。
    fn execute_native_command(&mut self, agent: Option<&Arc<Agent>>) {
        if !self.attachments.is_empty() {
            self.messages.push(Msg::new(
                MsgKind::Error,
                "斜杠命令不能携带附件，请先移除附件。",
            ));
            return;
        }
        let Some(input) = self.take_input() else {
            return;
        };
        self.native_command.dismissed_input = None;
        let mut parts = input.split_whitespace();
        let invoked = parts.next().unwrap_or_default();
        let name = invoked.trim_start_matches('/');
        let Some(command) = NATIVE_COMMANDS
            .iter()
            .find(|command| command.name == name || command.aliases.contains(&name))
        else {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!("未知命令 `{invoked}`；输入 `/help` 查看可用命令。"),
            ));
            return;
        };
        if command.idle_only && self.running {
            self.messages.push(Msg::new(
                MsgKind::Info,
                format!("命令 `/{}` 只能在 Agent 空闲时执行。", command.name),
            ));
            return;
        }
        match command.name {
            "help" => self.show_native_command_help(parts.next()),
            "resume" => self.open_native_sessions(NativeSessionDialogMode::Resume),
            "sessions" => self.open_native_sessions(NativeSessionDialogMode::Browse),
            #[cfg(feature = "plugins")]
            "plugins" => self.open_native_plugins(),
            "new" | "clear" => {
                let notice = if command.name == "new" {
                    "已进入新的空白会话"
                } else {
                    "已清空当前上下文并进入新会话"
                };
                if let Err(error) = self.start_new_draft(notice) {
                    self.messages
                        .push(Msg::new(MsgKind::Error, format!("新建会话失败：{error}")));
                }
            }
            "compact" => self.execute_native_compact(),
            "reasoning" => self.execute_reasoning_command(parts.next(), agent),
            "exit" => self.should_quit = true,
            _ => unreachable!("原生命令表与执行路由必须同步"),
        }
    }

    /// 执行 `/reasoning`：无参数打开二级选择界面，`next` 循环，其余参数精确设置等级。
    fn execute_reasoning_command(&mut self, value: Option<&str>, agent: Option<&Arc<Agent>>) {
        let Some(value) = value else {
            let selected = self
                .reasoning_levels
                .iter()
                .position(|level| *level == self.reasoning_level)
                .unwrap_or(0);
            self.native_command.reasoning_panel = Some(NativeReasoningPanel { selected });
            return;
        };
        if value.eq_ignore_ascii_case("next") || value == "下一个" {
            self.cycle_reasoning_level(agent);
            return;
        }
        let Some(level) = parse_reasoning_level(value) else {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!("未知思考等级 `{value}`；输入 `/help reasoning` 查看用法。"),
            ));
            return;
        };
        self.set_reasoning_level(level, agent);
    }

    /// 处理思考等级内联面板的键盘导航，并在确认后同步 TUI 与共享 Agent。
    fn handle_native_reasoning_panel_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return false;
        }
        let item_count = self.reasoning_levels.len();
        let Some(panel) = self.native_command.reasoning_panel.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Esc => self.native_command.reasoning_panel = None,
            KeyCode::Up if item_count > 0 => {
                panel.selected = panel.selected.checked_sub(1).unwrap_or(item_count - 1);
            }
            KeyCode::Down if item_count > 0 => {
                panel.selected = (panel.selected + 1) % item_count;
            }
            KeyCode::Enter if item_count > 0 => {
                let level = self.reasoning_levels[panel.selected.min(item_count - 1)];
                self.native_command.reasoning_panel = None;
                self.set_reasoning_level(level, agent);
            }
            _ => {}
        }
        true
    }

    /// 在消息流中显示全部命令或指定命令的说明。
    fn show_native_command_help(&mut self, target: Option<&str>) {
        let content = match target.map(|value| value.trim_start_matches('/')) {
            Some(target) => match NATIVE_COMMANDS
                .iter()
                .find(|command| command.name == target || command.aliases.contains(&target))
            {
                Some(command) => format!(
                    "/{} — {}\n{}{}",
                    command.name,
                    command.summary,
                    command.description,
                    if command.aliases.is_empty() {
                        String::new()
                    } else {
                        format!("\n别名：{}", command.aliases.join("、"))
                    }
                ),
                None => format!("未知命令 `/{target}`；输入 `/help` 查看可用命令。"),
            },
            None => NATIVE_COMMANDS
                .iter()
                .map(|command| format!("/{:<9} {}", command.name, command.summary))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        self.messages.push(Msg::new(MsgKind::Info, content));
        self.scroll = None;
    }

    /// 执行原生 `/compact`，并阻止重复发起后台压缩。
    fn execute_native_compact(&mut self) {
        if self.pending_reload.is_some() {
            self.messages
                .push(Msg::new(MsgKind::Info, "上下文压缩仍在执行，请稍后重试。"));
            return;
        }
        let _ = self.tx.send(UiEvent::NativeCompactRequested);
    }
}

/// 读取人工插件页所需的 Manager 列表与冻结 Stable 摘要。
#[cfg(feature = "plugins")]
async fn load_native_plugin_dialog(
    service: &PluginManagementService,
) -> Result<NativePluginDialogData> {
    Ok(NativePluginDialogData {
        plugins: service.list()?,
        version_history: service.list_version_history()?,
    })
}

/// 恢复用户在原生 Dialog 中选择的精确会话修订。
///
/// # Errors
///
/// 会话不存在、选择后已更新、存储读取失败时返回错误。
pub(crate) async fn resume_native_session(
    app: &mut App,
    session_id: SessionId,
    revision: u64,
) -> Result<()> {
    let record = app
        .session_store
        .load(&session_id)
        .await?
        .ok_or_else(|| anyhow!("会话 `{session_id}` 已不存在"))?;
    if record.revision != revision {
        return Err(anyhow!("会话 `{session_id}` 已更新，请刷新列表后重新选择"));
    }
    let notice = format!("已恢复会话 {}", record.id);
    app.replace_session(record, Some(&notice));
    Ok(())
}
