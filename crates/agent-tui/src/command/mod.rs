//! Lucia TUI 原生斜杠命令、补全面板和会话对话框。

mod context;
#[cfg(feature = "plugins")]
mod plugin_dialog;
mod render;
pub(crate) use context::{render_context_dialog, ContextSnapshot};
mod session_dialog;
#[cfg(test)]
mod tests;

#[cfg(feature = "plugins")]
pub(crate) use render::render_native_plugin_dialog;
pub(crate) use render::{render_native_command_panel, render_native_session_dialog};

use crate::app::App;
use crate::config::persist_tui_fullscreen;
use crate::event::UiEvent;
#[cfg(feature = "plugins")]
use crate::plugin::management::PluginManagementService;
use crate::session::attachment::pasted_file_path;
use crate::session::conversation::{Msg, MsgKind};
use agent_core::model::ReasoningLevel;
use agent_core::Agent;
use agent_session::{SessionId, SessionSummary};
use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyModifiers};
#[cfg(feature = "plugins")]
use std::collections::HashMap;
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
        name: "context",
        aliases: &[],
        summary: "查看上下文占用",
        description: "查看最近模型请求的上下文分类、窗口占用和当前会话用量。",
        idle_only: false,
    },
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
    NativeCommandSpec {
        name: "model",
        aliases: &[],
        summary: "查看或切换模型上游",
        description: "用法：/model [名称]；不带参数时打开上游选择界面。切换只在当前会话生效，重启后回到配置的默认上游。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "attach",
        aliases: &["file"],
        summary: "添加本地附件",
        description: "用法：/attach <路径> [路径...]；把本地文件加入待发送附件，路径可为绝对路径、`~/` 前缀或相对当前项目目录。已有附件时需先移除。",
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
        name: "tui",
        aliases: &[],
        summary: "查看或切换全屏渲染",
        description: "用法：/tui [fullscreen|default]；不带参数时显示当前渲染模式。fullscreen 使用终端备用屏幕（类似 vim，只渲染可见消息），default 回到主屏绘制。切换会写入配置文件。",
        idle_only: false,
    },
    NativeCommandSpec {
        name: "keys",
        aliases: &["key"],
        summary: "查看终端上报的按键",
        description: "进入按键诊断：逐条显示终端实际上报的键名与修饰键，按 Esc 退出。修饰键被系统重映射时用它确认实际到达的组合。",
        idle_only: false,
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

/// 模型上游内联选择面板的交互状态。
#[derive(Debug)]
pub(crate) struct NativeModelPanel {
    /// 当前高亮上游在配置列表中的下标。
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
    /// 按插件 ID 索引的 Host 校验授权 profile，包含 manifest 与 ABI 固有权限。
    pub(crate) authorizations:
        HashMap<String, agent_plugin_host::audit::PluginAuthorizationProfile>,
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

/// 第一次 `e` 绑定的插件启用确认，第二次确认只能提交同一 Bundle 身份。
#[cfg(feature = "plugins")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingPluginEnable {
    /// 插件稳定 ID。
    pub(crate) plugin_id: String,
    /// 用户查看来源与能力时对应的不可变 Bundle 身份。
    pub(crate) bundle_identity: agent_plugin_manager::PluginBundleIdentity,
}

/// 禁用插件被选择为 Capability Owner 后，等待独立启用确认的用户意图。
#[cfg(feature = "plugins")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCapabilityOwnerSelection {
    /// Capability 稳定 ID。
    pub(crate) capability_id: String,
    /// 等待来源与能力确认的插件稳定 ID。
    pub(crate) plugin_id: String,
    /// Owner 意图创建时用户看到的不可变 Bundle 身份。
    pub(crate) bundle_identity: agent_plugin_manager::PluginBundleIdentity,
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
    /// 第一次启用按键记录的 Bundle 身份；再次确认只能启用该精确身份。
    pub(crate) enable_confirmation: Option<PendingPluginEnable>,
    /// 禁用插件的待确认 Capability Owner 选择；任一无关操作都会使其失效。
    pub(crate) pending_owner_selection: Option<PendingCapabilityOwnerSelection>,
    /// 启用审查页的垂直滚动行；只影响展示，不参与授权身份比较。
    pub(crate) review_scroll: u16,
}

/// TUI 原生命令状态；不依赖 Plugin Host，纯 Core 构建同样可用。
#[derive(Debug, Default)]
pub(crate) struct NativeCommandState {
    /// 上下文检查视图是否占用键盘焦点。
    pub(crate) context_open: bool,
    /// 窄终端上下文视图的垂直滚动位置。
    pub(crate) context_scroll: u16,
    /// 补全面板当前高亮候选。
    pub(crate) selected: usize,
    /// 用户按 Esc 隐藏面板时的完整输入；输入发生变化后自动失效。
    pub(crate) dismissed_input: Option<String>,
    /// 当前会话对话框。
    pub(crate) dialog: Option<NativeSessionDialog>,
    /// 当前显示在输入框上方的思考等级选择面板。
    pub(crate) reasoning_panel: Option<NativeReasoningPanel>,
    /// 当前显示在输入框上方的模型上游选择面板。
    pub(crate) model_panel: Option<NativeModelPanel>,
    /// 当前人工插件管理页；纯 Core 构建不包含该状态。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_dialog: Option<NativePluginDialog>,
}

impl App {
    /// 返回任一原生全屏 Dialog 是否正在占用键盘焦点；内联面板不计入。
    pub(crate) fn native_dialog_active(&self) -> bool {
        self.native_command.context_open
            || self.native_command.dialog.is_some()
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
        if self.native_command.model_panel.is_some() {
            return u16::try_from(self.model_upstreams.len().max(1))
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
        if self.native_command.context_open {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.native_command.context_open = false,
                KeyCode::Up => {
                    self.native_command.context_scroll =
                        self.native_command.context_scroll.saturating_sub(1)
                }
                KeyCode::Down => {
                    self.native_command.context_scroll =
                        self.native_command.context_scroll.saturating_add(1)
                }
                KeyCode::Home => self.native_command.context_scroll = 0,
                _ => {}
            }
            return true;
        }
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
        if self.native_command.model_panel.is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return self.handle_native_model_panel_key(code, modifiers, agent);
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
        let invoked = input.split_whitespace().next().unwrap_or_default();
        let name = invoked.trim_start_matches('/');
        // 首个空白之后的完整内容作为参数，保留路径里的空格。
        let argument = input
            .split_once(char::is_whitespace)
            .map(|(_, rest)| rest.trim())
            .filter(|rest| !rest.is_empty());
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
            "context" => {
                self.native_command.context_open = true;
                self.native_command.context_scroll = 0;
            }
            "help" => {
                let topic = argument.and_then(|rest| rest.split_whitespace().next());
                self.show_native_command_help(topic);
            }
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
            "attach" => self.execute_attach_command(argument),
            "reasoning" => self.execute_reasoning_command(argument, agent),
            "model" => self.execute_model_command(argument, agent),
            "tui" => self.execute_tui_command(argument),
            "keys" => self.start_key_inspection(),
            "exit" => self.should_quit = true,
            _ => unreachable!("原生命令表与执行路由必须同步"),
        }
    }

    /// 执行 `/attach`：把本地文件加入待发送附件。
    ///
    /// 路径解析复用粘贴路径逻辑，因此支持绝对路径、`~/` 前缀、引号包裹以及相对
    /// 当前项目目录的写法。先尝试把整段参数当作一个路径（保留其中的空格），失败后
    /// 再按空白拆成多个路径，便于一次添加多个附件；无法解析的路径逐个报错，
    /// 已成功的附件不受影响。
    fn execute_attach_command(&mut self, argument: Option<&str>) {
        let Some(argument) = argument else {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "用法：/attach <路径> [路径...]；路径可为绝对路径、`~/` 前缀或相对当前项目目录。",
            ));
            return;
        };
        let cwd = self.workspace.cwd.clone();
        // 整段就是一个可读路径（含未加引号的空格）时不再拆分。
        if let Some(path) = pasted_file_path(argument, &cwd) {
            self.append_attachment(&path);
            return;
        }
        for candidate in argument.split_whitespace() {
            match pasted_file_path(candidate, &cwd) {
                Some(path) => self.append_attachment(&path),
                None => self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("找不到可读取的文件 `{candidate}`；请确认路径存在且是普通文件。"),
                )),
            }
        }
    }

    /// 追加一个附件引用标签到输入末尾，符合"继续添加"的直觉。
    fn append_attachment(&mut self, path: &std::path::Path) {
        self.cursor = self.input.len();
        self.attach_file(path);
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

    /// 执行 `/tui`：无参数显示当前渲染模式，带参数切换全屏渲染。
    ///
    /// 渲染模式的真正切换由持有终端的主循环完成；这里只登记意图并写回配置。写回失败时
    /// 不切换，避免出现"本次生效、下次启动回退"的不一致。
    fn execute_tui_command(&mut self, value: Option<&str>) {
        let Some(value) = value else {
            let current = if self.fullscreen {
                "fullscreen（备用屏幕，只渲染可见消息）"
            } else {
                "default（主屏绘制）"
            };
            self.messages
                .push(Msg::new(MsgKind::Info, format!("当前渲染模式：{current}")));
            return;
        };
        let target = if value.eq_ignore_ascii_case("fullscreen") || value == "全屏" {
            true
        } else if value.eq_ignore_ascii_case("default") || value == "默认" {
            false
        } else {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!("未知渲染模式 `{value}`；用法：/tui [fullscreen|default]。"),
            ));
            return;
        };
        if target == self.fullscreen {
            self.messages
                .push(Msg::new(MsgKind::Info, "渲染模式未变化。"));
            return;
        }
        if let Some(path) = self.config_path.as_deref() {
            if let Err(error) = persist_tui_fullscreen(path, target) {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("保存渲染模式失败，当前设置未改变：{error:#}"),
                ));
                return;
            }
        }
        self.fullscreen = target;
        self.pending_fullscreen = Some(target);
        self.messages.push(Msg::new(
            MsgKind::Info,
            if target {
                "已切换到全屏渲染"
            } else {
                "已切换到主屏渲染"
            },
        ));
    }

    /// 执行 `/model`：无参数打开上游选择界面，带参数按名称直接切换。
    ///
    /// 切换为会话级：只覆盖共享 Agent 的路由，不写回配置文件。
    fn execute_model_command(&mut self, value: Option<&str>, agent: Option<&Arc<Agent>>) {
        if self.model_upstreams.is_empty() {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "配置中没有可切换的上游；在 config.json 添加 models 数组后即可使用 /model。",
            ));
            return;
        }
        if let Some(value) = value {
            let matched = self
                .model_upstreams
                .iter()
                .find(|entry| entry.name == value)
                .cloned();
            let Some(entry) = matched else {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!(
                        "未知上游 `{value}`；可用上游：{}",
                        self.model_upstreams
                            .iter()
                            .map(|entry| entry.name.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    ),
                ));
                return;
            };
            self.switch_to_upstream(&entry, agent);
            return;
        }
        let selected = self
            .model_upstreams
            .iter()
            .position(|entry| entry.model == self.model_name)
            .unwrap_or(0);
        self.native_command.model_panel = Some(NativeModelPanel { selected });
    }

    /// 处理模型上游内联面板的键盘导航，并在确认后热切换共享 Agent。
    fn handle_native_model_panel_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) -> bool {
        if crate::app::is_copy_shortcut(code, modifiers) {
            return false;
        }
        let item_count = self.model_upstreams.len();
        let Some(panel) = self.native_command.model_panel.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Esc => self.native_command.model_panel = None,
            KeyCode::Up if item_count > 0 => {
                panel.selected = panel.selected.checked_sub(1).unwrap_or(item_count - 1);
            }
            KeyCode::Down if item_count > 0 => {
                panel.selected = (panel.selected + 1) % item_count;
            }
            KeyCode::Enter if item_count > 0 => {
                let entry = self.model_upstreams[panel.selected.min(item_count - 1)].clone();
                self.native_command.model_panel = None;
                self.switch_to_upstream(&entry, agent);
            }
            _ => {}
        }
        true
    }

    /// 把共享 Agent 热切换到指定上游，并刷新状态栏与 models.dev 元数据。
    fn switch_to_upstream(
        &mut self,
        entry: &agent_core::config::ModelConfig,
        agent: Option<&Arc<Agent>>,
    ) {
        let Some(agent) = agent else {
            self.messages
                .push(Msg::new(MsgKind::Info, "模型尚未就绪，无法切换上游"));
            return;
        };
        let provider_config = match entry.to_provider_config() {
            Ok(config) => config,
            Err(error) => {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("上游 `{}` 凭据不可用：{error}", entry.name),
                ));
                return;
            }
        };
        if let Err(error) = agent.switch_model_upstream(provider_config, entry.model.clone()) {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!("切换到上游 `{}` 失败：{error}", entry.name),
            ));
            return;
        }
        self.model_name = entry.model.clone();
        self.refresh_model_metadata(entry);
        self.messages.push(Msg::new(
            MsgKind::Info,
            format!(
                "模型已切换为 {}（{}，{}）",
                entry.name,
                entry.provider.protocol_id(),
                entry.model
            ),
        ));
    }

    /// 按切换后的上游重新解析 models.dev 元数据；未命中时恢复缺省展示。
    fn refresh_model_metadata(&mut self, entry: &agent_core::config::ModelConfig) {
        let provider_id = match entry.provider.protocol_id() {
            "open-ai" | "open-ai-compatible" => Some("openai"),
            "anthropic" => Some("anthropic"),
            _ => None,
        };
        let metadata = provider_id.and_then(|provider| {
            crate::config::model_metadata::find_model_metadata(provider, &entry.model)
                .ok()
                .flatten()
        });
        self.context_window = metadata.as_ref().and_then(|meta| meta.context_window);
        self.model_input_modalities = metadata.as_ref().map(|meta| meta.input_modalities.clone());
        let levels = metadata
            .map(|meta| meta.reasoning_levels)
            .filter(|levels| !levels.is_empty());
        match levels {
            Some(levels) => {
                if !levels.contains(&self.reasoning_level) {
                    self.reasoning_level = ReasoningLevel::Off;
                }
                self.reasoning_levels = levels;
            }
            None => {
                self.reasoning_levels = vec![ReasoningLevel::Off];
                self.reasoning_level = ReasoningLevel::Off;
            }
        }
    }

    /// 处理思考等级内联面板的键盘导航，并在确认后同步 TUI 与共享 Agent。
    fn handle_native_reasoning_panel_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) -> bool {
        if crate::app::is_copy_shortcut(code, modifiers) {
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
    let plugins = service.list()?;
    let authorizations = plugins
        .iter()
        .filter_map(|plugin| {
            service
                .authorization_profile(plugin)
                .ok()
                .map(|profile| (plugin.id.clone(), profile))
        })
        .collect::<HashMap<_, _>>();
    Ok(NativePluginDialogData {
        plugins,
        version_history: service.list_version_history()?,
        authorizations,
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
