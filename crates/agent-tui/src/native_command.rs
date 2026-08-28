//! Lucia TUI 原生斜杠命令、补全面板和会话对话框。

use super::*;

/// 原生命令面板最多展示的候选数。
const MAX_VISIBLE_COMMANDS: usize = 7;

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

/// Lucia 默认提供的全部斜杠命令。
const NATIVE_COMMANDS: &[NativeCommandSpec] = &[
    NativeCommandSpec {
        name: "help",
        aliases: &[],
        summary: "查看命令帮助",
        description: "显示全部原生命令，或显示指定命令的详细用法。",
        idle_only: false,
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
    #[cfg(feature = "plugins")]
    NativeCommandSpec {
        name: "plugins",
        aliases: &[],
        summary: "管理固定插件依赖",
        description: "打开人工插件管理页；变更创建新基线，当前会话不热重载。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "new",
        aliases: &[],
        summary: "新建会话",
        description: "结束当前会话并进入尚未持久化的空白草稿。",
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
        name: "compact",
        aliases: &[],
        summary: "立即压缩上下文",
        description: "请求当前 Context Loader 压缩旧会话并持久化结果。",
        idle_only: true,
    },
    NativeCommandSpec {
        name: "reasoning",
        aliases: &["think"],
        summary: "查看或切换思考等级",
        description: "用法：/reasoning [off|minimal|low|medium|high|xhigh|max|next]；不带参数时打开等级选择界面。",
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
fn reasoning_level_command_value(level: ReasoningLevel) -> &'static str {
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

/// 思考等级二级选择界面的交互状态。
#[derive(Debug)]
pub(crate) struct NativeReasoningDialog {
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
    /// 当前 Stable Genome 修订。
    pub(crate) stable_revision: agent_evolution_protocol::GenomeRevisionId,
    /// 当前冻结插件环境摘要。
    pub(crate) environment_digest: agent_evolution_protocol::PluginEnvironmentDigest,
    /// 当前 Stable 代数。
    pub(crate) generation: u64,
}

/// 插件管理页的异步状态。
#[cfg(feature = "plugins")]
#[derive(Debug)]
pub(crate) enum NativePluginDialogStatus {
    /// 正在读取 Manager 与 Stable Genome。
    Loading,
    /// 列表和冻结环境已经校验。
    Ready(NativePluginDialogData),
    /// 正在提交 Manager 写入和人工基线。
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
    /// 当前思考等级选择界面。
    pub(crate) reasoning_dialog: Option<NativeReasoningDialog>,
    /// 当前人工插件管理页；纯 Core 构建不包含该状态。
    #[cfg(feature = "plugins")]
    pub(crate) plugin_dialog: Option<NativePluginDialog>,
}

impl App {
    /// 返回任一原生全屏 Dialog 是否正在占用键盘焦点。
    pub(crate) fn native_dialog_active(&self) -> bool {
        self.native_command.dialog.is_some()
            || self.native_command.reasoning_dialog.is_some()
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

    /// 返回当前输入匹配的原生命令，排序与稳定注册顺序一致。
    fn native_command_matches(&self) -> Vec<&'static NativeCommandSpec> {
        let Some(rest) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        NATIVE_COMMANDS
            .iter()
            .filter(|command| {
                command.name.starts_with(rest)
                    || command.aliases.iter().any(|alias| alias.starts_with(rest))
            })
            .take(MAX_VISIBLE_COMMANDS)
            .collect()
    }

    /// 返回当前补全面板所需高度；输入不是命令或面板已被显式隐藏时返回零。
    pub(crate) fn native_command_panel_height(&self) -> u16 {
        if self.native_command.dismissed_input.as_deref() == Some(self.input.as_str()) {
            return 0;
        }
        let count = u16::try_from(self.native_command_matches().len())
            .unwrap_or(u16::MAX)
            .min(MAX_VISIBLE_COMMANDS as u16);
        count.saturating_add(u16::from(count > 0))
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
        if self.native_command.reasoning_dialog.is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return self.handle_native_reasoning_dialog_key(code, modifiers, agent);
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
        let matches = self.native_command_matches();
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
                if !matches.is_empty() {
                    let selected = self.native_command.selected.min(matches.len() - 1);
                    self.input = format!("/{}", matches[selected].name);
                    self.cursor = self.input.len();
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
            self.native_command.reasoning_dialog = Some(NativeReasoningDialog { selected });
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

    /// 处理思考等级二级界面的键盘导航，并在确认后同步 TUI 与共享 Agent。
    fn handle_native_reasoning_dialog_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return false;
        }
        let item_count = self.reasoning_levels.len();
        let Some(dialog) = self.native_command.reasoning_dialog.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Esc => self.native_command.reasoning_dialog = None,
            KeyCode::Up if item_count > 0 => {
                dialog.selected = dialog.selected.checked_sub(1).unwrap_or(item_count - 1);
            }
            KeyCode::Down if item_count > 0 => {
                dialog.selected = (dialog.selected + 1) % item_count;
            }
            KeyCode::Enter if item_count > 0 => {
                let level = self.reasoning_levels[dialog.selected.min(item_count - 1)];
                self.native_command.reasoning_dialog = None;
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

    /// 打开原生会话 Dialog，并异步读取当前项目的轻量摘要。
    fn open_native_sessions(&mut self, mode: NativeSessionDialogMode) {
        self.native_command.dialog = Some(NativeSessionDialog {
            mode,
            selected: 0,
            status: NativeSessionDialogStatus::Loading,
        });
        let session_store = Arc::clone(&self.session_store);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session_store.list_summaries().await.map(|mut summaries| {
                summaries.sort_by(|left, right| {
                    right
                        .updated_at_ms
                        .cmp(&left.updated_at_ms)
                        .then_with(|| left.id.cmp(&right.id))
                });
                summaries.truncate(MAX_VISIBLE_SESSIONS);
                summaries
            });
            let _ = tx.send(UiEvent::NativeSessionsLoaded(result));
        });
    }

    /// 把异步摘要结果提交到仍然打开的原生会话 Dialog。
    pub(crate) fn handle_native_sessions_loaded(
        &mut self,
        result: Result<Vec<SessionSummary>, SessionStoreError>,
    ) {
        let Some(dialog) = self.native_command.dialog.as_mut() else {
            return;
        };
        dialog.selected = 0;
        dialog.status = match result {
            Ok(summaries) => NativeSessionDialogStatus::Ready(summaries),
            Err(error) => NativeSessionDialogStatus::Error(error.to_string()),
        };
    }

    /// 处理会话 Dialog 的稳定键盘交互。
    fn handle_native_session_dialog_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return false;
        }
        let Some(dialog) = self.native_command.dialog.as_mut() else {
            return false;
        };
        let item_count = match &dialog.status {
            NativeSessionDialogStatus::Ready(items) => items.len(),
            NativeSessionDialogStatus::Loading | NativeSessionDialogStatus::Error(_) => 0,
        };
        match code {
            KeyCode::Esc => {
                self.native_command.dialog = None;
            }
            KeyCode::Up if item_count > 0 => {
                dialog.selected = dialog.selected.checked_sub(1).unwrap_or(item_count - 1);
            }
            KeyCode::Down if item_count > 0 => {
                dialog.selected = (dialog.selected + 1) % item_count;
            }
            KeyCode::Enter if item_count > 0 => {
                let NativeSessionDialogStatus::Ready(items) = &dialog.status else {
                    unreachable!("会话数量只会来自 Ready 状态");
                };
                let summary = items[dialog.selected.min(items.len() - 1)].clone();
                self.native_command.dialog = None;
                let _ = self.tx.send(UiEvent::NativeResumeRequested {
                    session_id: summary.id,
                    revision: summary.revision,
                });
            }
            _ => {}
        }
        true
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

    /// 打开人工插件管理页，并异步读取 Manager 与冻结 Stable 环境。
    #[cfg(feature = "plugins")]
    fn open_native_plugins(&mut self) {
        self.native_command.plugin_dialog = Some(NativePluginDialog {
            selected: 0,
            status: NativePluginDialogStatus::Loading,
            path_mode: None,
            path_input: String::new(),
            remove_confirmation: None,
        });
        self.reload_native_plugins();
    }

    /// 重新加载插件管理页；未配置 Stable Genome 时以只读错误说明阻止写操作。
    #[cfg(feature = "plugins")]
    fn reload_native_plugins(&mut self) {
        let Some(dialog) = self.native_command.plugin_dialog.as_mut() else {
            return;
        };
        dialog.status = NativePluginDialogStatus::Loading;
        dialog.remove_confirmation = None;
        let Some(service) = self.plugin_management.as_ref().map(Arc::clone) else {
            dialog.status = NativePluginDialogStatus::Error(
                "未配置可发布的 Stable Genome；插件写操作已禁用".into(),
            );
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_native_plugin_dialog(service.as_ref())
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::NativePluginsLoaded(result));
        });
    }

    /// 把插件页异步加载结果应用到仍然打开的 Dialog。
    #[cfg(feature = "plugins")]
    pub(crate) fn handle_native_plugins_loaded(
        &mut self,
        result: Result<NativePluginDialogData, String>,
    ) {
        let Some(dialog) = self.native_command.plugin_dialog.as_mut() else {
            return;
        };
        dialog.selected = 0;
        dialog.status = match result {
            Ok(data) => NativePluginDialogStatus::Ready(data),
            Err(error) => NativePluginDialogStatus::Error(error),
        };
    }

    /// 把人工管理事务结果显示到消息流，并刷新插件页的 Stable 摘要。
    #[cfg(feature = "plugins")]
    pub(crate) fn handle_native_plugin_action_finished(
        &mut self,
        result: Result<PluginManagementOutcome, String>,
    ) {
        match result {
            Ok(outcome) => {
                let suffix = outcome.baseline.as_ref().map_or_else(
                    || "插件环境没有行为变化，未创建新基线。".to_string(),
                    |baseline| {
                        format!(
                            "插件环境已改变；新 Session 使用人工基线 {}（代数 {}，Genome 摘要 {}，插件摘要 {}）。当前 Session 继续使用启动时快照，旧 Evolution Candidate 不得自动应用。",
                            baseline.revision_id,
                            baseline.generation,
                            baseline.genome_digest,
                            baseline.plugin_environment_digest
                        )
                    },
                );
                self.messages.push(Msg::new(
                    MsgKind::Info,
                    format!("{}\n{}", outcome.message, suffix),
                ));
                self.reload_native_plugins();
            }
            Err(error) => {
                if let Some(dialog) = self.native_command.plugin_dialog.as_mut() {
                    dialog.status = NativePluginDialogStatus::Error(error.clone());
                }
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("人工插件管理失败：{error}"),
                ));
            }
        }
    }

    /// 处理插件管理页的列表导航、路径输入、启停和二次确认移除。
    #[cfg(feature = "plugins")]
    fn handle_native_plugin_dialog_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return false;
        }
        let Some(dialog) = self.native_command.plugin_dialog.as_mut() else {
            return false;
        };
        if let Some(mode) = dialog.path_mode {
            match code {
                KeyCode::Esc => {
                    dialog.path_mode = None;
                    dialog.path_input.clear();
                }
                KeyCode::Backspace => {
                    dialog.path_input.pop();
                }
                KeyCode::Char(character) if modifiers.is_empty() => {
                    dialog.path_input.push(character);
                }
                KeyCode::Enter if !dialog.path_input.trim().is_empty() => {
                    let action = match mode {
                        NativePluginPathMode::InstallRegistry => {
                            ManualPluginAction::InstallRegistry {
                                request: dialog.path_input.trim().to_string(),
                                enabled: true,
                            }
                        }
                        NativePluginPathMode::InstallGithub => ManualPluginAction::InstallGithub {
                            source: dialog.path_input.trim().to_string(),
                            enabled: true,
                        },
                        NativePluginPathMode::Install => ManualPluginAction::InstallLocal {
                            bundle: PathBuf::from(dialog.path_input.trim()),
                            enabled: true,
                        },
                        NativePluginPathMode::Replace => ManualPluginAction::ReplaceLocal {
                            bundle: PathBuf::from(dialog.path_input.trim()),
                            enabled: true,
                        },
                        NativePluginPathMode::Rollback => {
                            let mut parts = dialog.path_input.split_whitespace();
                            let plugin_id = parts.next().unwrap_or_default();
                            let version = parts.next().unwrap_or_default();
                            if plugin_id.is_empty() || version.is_empty() || parts.next().is_some()
                            {
                                return true;
                            }
                            ManualPluginAction::Rollback {
                                plugin_id: plugin_id.to_string(),
                                version: version.to_string(),
                            }
                        }
                        NativePluginPathMode::SelectCapabilityOwner => {
                            let mut parts = dialog.path_input.split_whitespace();
                            let capability_id = parts.next().unwrap_or_default();
                            let plugin_id = parts.next().unwrap_or_default();
                            if capability_id.is_empty()
                                || plugin_id.is_empty()
                                || parts.next().is_some()
                            {
                                return true;
                            }
                            ManualPluginAction::SelectCapabilityOwner {
                                capability_id: capability_id.to_string(),
                                plugin_id: plugin_id.to_string(),
                            }
                        }
                        NativePluginPathMode::ClearCapabilityOwner => {
                            ManualPluginAction::ClearCapabilityOwner {
                                capability_id: dialog.path_input.trim().to_string(),
                            }
                        }
                    };
                    dialog.path_mode = None;
                    dialog.path_input.clear();
                    self.start_native_plugin_action(action);
                }
                _ => {}
            }
            return true;
        }
        let item_count = match &dialog.status {
            NativePluginDialogStatus::Ready(data) => data.plugins.len(),
            NativePluginDialogStatus::Loading
            | NativePluginDialogStatus::Applying
            | NativePluginDialogStatus::Error(_) => 0,
        };
        match code {
            KeyCode::Esc => self.native_command.plugin_dialog = None,
            KeyCode::Char('r') => self.reload_native_plugins(),
            KeyCode::Char('i') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::Install);
                dialog.path_input.clear();
            }
            KeyCode::Char('g') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::InstallRegistry);
                dialog.path_input.clear();
            }
            KeyCode::Char('h') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::InstallGithub);
                dialog.path_input.clear();
            }
            KeyCode::Char('u') if item_count > 0 => {
                dialog.path_mode = Some(NativePluginPathMode::Replace);
                dialog.path_input.clear();
            }
            KeyCode::Char('b') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::Rollback);
                dialog.path_input.clear();
            }
            KeyCode::Char('o') if item_count > 0 => {
                dialog.path_mode = Some(NativePluginPathMode::SelectCapabilityOwner);
                dialog.path_input.clear();
            }
            KeyCode::Char('w') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::ClearCapabilityOwner);
                dialog.path_input.clear();
            }
            KeyCode::Char('t') if item_count > 0 => {
                let NativePluginDialogStatus::Ready(data) = &dialog.status else {
                    unreachable!("插件数量只会来自 Ready 状态");
                };
                let plugin_id = data.plugins[dialog.selected.min(data.plugins.len() - 1)]
                    .id
                    .clone();
                self.start_native_plugin_action(ManualPluginAction::UpdateRegistry { plugin_id });
            }
            KeyCode::Up if item_count > 0 => {
                dialog.selected = dialog.selected.checked_sub(1).unwrap_or(item_count - 1);
                dialog.remove_confirmation = None;
            }
            KeyCode::Down if item_count > 0 => {
                dialog.selected = (dialog.selected + 1) % item_count;
                dialog.remove_confirmation = None;
            }
            KeyCode::Char('e') if item_count > 0 => {
                let NativePluginDialogStatus::Ready(data) = &dialog.status else {
                    unreachable!("插件数量只会来自 Ready 状态");
                };
                let plugin = &data.plugins[dialog.selected.min(data.plugins.len() - 1)];
                let action = if plugin.enabled {
                    ManualPluginAction::Disable {
                        plugin_id: plugin.id.clone(),
                    }
                } else {
                    ManualPluginAction::Enable {
                        plugin_id: plugin.id.clone(),
                    }
                };
                self.start_native_plugin_action(action);
            }
            KeyCode::Char('x') if item_count > 0 => {
                let NativePluginDialogStatus::Ready(data) = &dialog.status else {
                    unreachable!("插件数量只会来自 Ready 状态");
                };
                let plugin_id = data.plugins[dialog.selected.min(data.plugins.len() - 1)]
                    .id
                    .clone();
                if dialog.remove_confirmation.as_deref() == Some(plugin_id.as_str()) {
                    dialog.remove_confirmation = None;
                    self.start_native_plugin_action(ManualPluginAction::Remove { plugin_id });
                } else {
                    dialog.remove_confirmation = Some(plugin_id);
                }
            }
            _ => {}
        }
        true
    }

    /// 在后台执行插件人工管理事务，期间 Dialog 保持不可编辑。
    #[cfg(feature = "plugins")]
    fn start_native_plugin_action(&mut self, action: ManualPluginAction) {
        let Some(service) = self.plugin_management.as_ref().map(Arc::clone) else {
            if let Some(dialog) = self.native_command.plugin_dialog.as_mut() {
                dialog.status = NativePluginDialogStatus::Error(
                    "未配置可发布的 Stable Genome；插件写操作已禁用".into(),
                );
            }
            return;
        };
        if let Some(dialog) = self.native_command.plugin_dialog.as_mut() {
            dialog.status = NativePluginDialogStatus::Applying;
            dialog.remove_confirmation = None;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = service
                .apply(action)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::NativePluginActionFinished(result));
        });
    }
}

/// 读取人工插件页所需的 Manager 列表与冻结 Stable 摘要。
#[cfg(feature = "plugins")]
async fn load_native_plugin_dialog(
    service: &PluginManagementService,
) -> Result<NativePluginDialogData> {
    let plugins = service.list()?;
    let version_history = service.list_version_history()?;
    let (stable, environment_digest) = service.frozen_environment().await?;
    Ok(NativePluginDialogData {
        plugins,
        version_history,
        stable_revision: stable.revision_id,
        environment_digest,
        generation: stable.generation,
    })
}

/// 恢复用户在原生 Dialog 中选择的精确会话修订。
///
/// # Errors
///
/// 会话不存在、选择后已更新、存储读取失败或 Genome 行为绑定不一致时返回错误。
pub(crate) async fn resume_native_session(
    app: &mut App,
    session_id: SessionId,
    revision: u64,
) -> Result<()> {
    let mut record = app
        .session_store
        .load(&session_id)
        .await?
        .ok_or_else(|| anyhow!("会话 `{session_id}` 已不存在"))?;
    if record.revision != revision {
        return Err(anyhow!("会话 `{session_id}` 已更新，请刷新列表后重新选择"));
    }
    app.genome_runtime.bind_or_validate_session(&mut record)?;
    let notice = format!("已恢复会话 {}", record.id);
    app.replace_session(record, Some(&notice));
    Ok(())
}

/// 在输入区上方绘制原生命令补全面板。
pub(crate) fn render_native_command_panel(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let matches = app.native_command_matches();
    if matches.is_empty() {
        return;
    }
    let selected = app.native_command.selected.min(matches.len() - 1);
    let lines = matches
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let style = if index == selected {
                Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
            } else {
                Style::new().fg(COLOR_MUTED)
            };
            Line::from(vec![
                Span::styled(format!("/{:<10}", command.name), style),
                Span::styled(command.summary, style),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// 在主界面上方绘制原生会话对话框。
pub(crate) fn render_native_session_dialog(frame: &mut Frame, app: &App, outer: Rect) {
    let Some(dialog) = app.native_command.dialog.as_ref() else {
        return;
    };
    let width = outer.width.saturating_sub(4).clamp(20, 88);
    let height = outer.height.saturating_sub(4).clamp(6, 24);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let title = match dialog.mode {
        NativeSessionDialogMode::Resume => "恢复项目会话",
        NativeSessionDialogMode::Browse => "项目会话",
    };
    let inner_height = usize::from(height.saturating_sub(2));
    let lines = match &dialog.status {
        NativeSessionDialogStatus::Loading => vec![Line::from(Span::styled(
            "正在读取会话…",
            Style::new().fg(COLOR_MUTED),
        ))],
        NativeSessionDialogStatus::Error(error) => vec![Line::from(Span::styled(
            format!("读取失败：{error}"),
            Style::new().fg(COLOR_DANGER),
        ))],
        NativeSessionDialogStatus::Ready(items) if items.is_empty() => vec![Line::from(
            Span::styled("当前项目还没有持久化会话", Style::new().fg(COLOR_MUTED)),
        )],
        NativeSessionDialogStatus::Ready(items) => {
            let selected = dialog.selected.min(items.len() - 1);
            let start = selected
                .saturating_sub(inner_height.saturating_sub(1) / 2)
                .min(items.len().saturating_sub(inner_height));
            items
                .iter()
                .enumerate()
                .skip(start)
                .take(inner_height)
                .map(|(index, summary)| {
                    let title = summary
                        .title
                        .as_deref()
                        .unwrap_or_else(|| summary.id.as_str());
                    let label = format!(
                        "{}  {} 条消息  {}",
                        truncate_line(title, 42),
                        summary.message_count,
                        relative_native_time(summary.updated_at_ms)
                    );
                    let style = if index == selected {
                        Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
                    } else {
                        Style::new().fg(COLOR_MUTED)
                    };
                    Line::from(Span::styled(label, style))
                })
                .collect()
        }
    };
    let footer = match dialog.mode {
        NativeSessionDialogMode::Resume => " ↑↓ 选择 · Enter 恢复 · Esc 关闭 ",
        NativeSessionDialogMode::Browse => " ↑↓ 选择 · Enter 切换 · Esc 关闭 ",
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .title(title)
                .title_bottom(Line::from(footer).alignment(Alignment::Center))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// 在主界面上方绘制思考等级二级选择界面；选项只来自当前模型的 models.dev 元数据。
pub(crate) fn render_native_reasoning_dialog(frame: &mut Frame, app: &App, outer: Rect) {
    let Some(dialog) = app.native_command.reasoning_dialog.as_ref() else {
        return;
    };
    let width = outer.width.saturating_sub(4).clamp(24, 48);
    let desired_height = u16::try_from(app.reasoning_levels.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let height = desired_height.min(outer.height.saturating_sub(4)).max(4);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let selected = dialog
        .selected
        .min(app.reasoning_levels.len().saturating_sub(1));
    let lines = app
        .reasoning_levels
        .iter()
        .enumerate()
        .map(|(index, level)| {
            let current = if *level == app.reasoning_level {
                "●"
            } else {
                " "
            };
            let label = format!(
                "{current} {:<6} {}",
                reasoning_level_command_value(*level),
                reasoning_level_label(*level)
            );
            let style = if index == selected {
                Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
            } else {
                Style::new().fg(COLOR_MUTED)
            };
            Line::from(Span::styled(label, style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .title("选择思考等级")
                .title_bottom(
                    Line::from(" ↑↓ 选择 · Enter 应用 · Esc 关闭 ").alignment(Alignment::Center),
                )
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// 在主界面上方绘制人工插件管理页。
#[cfg(feature = "plugins")]
pub(crate) fn render_native_plugin_dialog(frame: &mut Frame, app: &App, outer: Rect) {
    let Some(dialog) = app.native_command.plugin_dialog.as_ref() else {
        return;
    };
    let width = outer.width.saturating_sub(2).clamp(30, 104);
    let height = outer.height.saturating_sub(2).clamp(10, 30);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let inner_height = usize::from(height.saturating_sub(5));
    let mut lines = match &dialog.status {
        NativePluginDialogStatus::Loading => vec![Line::from(Span::styled(
            "正在读取插件锁和冻结环境…",
            Style::new().fg(COLOR_MUTED),
        ))],
        NativePluginDialogStatus::Applying => vec![Line::from(Span::styled(
            "正在提交人工插件管理基线；当前 Session 不会热重载…",
            Style::new().fg(COLOR_WARNING),
        ))],
        NativePluginDialogStatus::Error(error) => vec![
            Line::from(Span::styled(
                format!("读取或提交失败：{error}"),
                Style::new().fg(COLOR_DANGER),
            )),
            Line::from(Span::styled(
                "按 r 重试，按 Esc 关闭",
                Style::new().fg(COLOR_MUTED),
            )),
        ],
        NativePluginDialogStatus::Ready(data) => {
            let mut content = vec![
                Line::from(vec![
                    Span::styled("Frozen Plugin Environment  ", Style::new().fg(COLOR_MUTED)),
                    Span::styled(
                        truncate_line(data.environment_digest.as_ref(), 24),
                        Style::new().fg(COLOR_USER),
                    ),
                ]),
                Line::from(Span::styled(
                    format!(
                        "Stable {} · generation {} · Plugins are fixed dependencies for this Session.",
                        data.stable_revision, data.generation
                    ),
                    Style::new().fg(COLOR_MUTED),
                )),
                Line::from(Span::styled(
                    if data.version_history.is_empty() {
                        "Installed Version History: 无".to_string()
                    } else {
                        format!(
                            "Installed Version History: {}",
                            data.version_history
                                .iter()
                                .map(|plugin| format!("{}@{}", plugin.id, plugin.version))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                    Style::new().fg(COLOR_MUTED),
                )),
            ];
            if data.plugins.is_empty() {
                content.push(Line::from(Span::styled(
                    "未安装受管理插件；按 i 从本地 Bundle 安装",
                    Style::new().fg(COLOR_MUTED),
                )));
                content
            } else {
                let selected = dialog.selected.min(data.plugins.len() - 1);
                let visible_height = inner_height.saturating_sub(content.len());
                let start = selected
                    .saturating_sub(visible_height.saturating_sub(1) / 2)
                    .min(data.plugins.len().saturating_sub(visible_height));
                content.extend(
                    data.plugins
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(visible_height)
                        .map(|(index, plugin)| {
                            let state = if plugin.enabled {
                                "已启用"
                            } else {
                                "已禁用"
                            };
                            let label = format!(
                                "{:<18} {:<10} {:<6} sha256:{}",
                                truncate_line(&plugin.id, 18),
                                truncate_line(&plugin.version, 10),
                                state,
                                truncate_line(&plugin.sha256, 12)
                            );
                            let style = if index == selected {
                                Style::new().fg(COLOR_TEXT).bg(COLOR_USER_BG).bold()
                            } else {
                                Style::new().fg(COLOR_MUTED)
                            };
                            Line::from(Span::styled(label, style))
                        }),
                );
                content
            }
        }
    };
    if let Some(mode) = dialog.path_mode {
        let label = match mode {
            NativePluginPathMode::InstallRegistry => "Registry name[@semver]",
            NativePluginPathMode::InstallGithub => "GitHub owner/repository 或 URL",
            NativePluginPathMode::Install => "本地安装 Bundle 路径",
            NativePluginPathMode::Replace => "本地更新 Bundle 路径",
            NativePluginPathMode::Rollback => "插件 ID 与历史版本",
            NativePluginPathMode::SelectCapabilityOwner => "Capability ID 与插件 ID",
            NativePluginPathMode::ClearCapabilityOwner => "待清除的 Capability ID",
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), Style::new().fg(COLOR_USER)),
            Span::styled(&dialog.path_input, Style::new().fg(COLOR_TEXT)),
        ]));
    } else if let Some(plugin_id) = &dialog.remove_confirmation {
        lines.push(Line::from(Span::styled(
            format!("再次按 x 确认移除 `{plugin_id}`；该操作会创建人工基线"),
            Style::new().fg(COLOR_WARNING),
        )));
    }
    let footer = if dialog.path_mode.is_some() {
        " 输入路径 · Enter 提交 · Esc 取消 "
    } else {
        " ↑↓ · e 启停 · g/h/i 安装 · t/u 更新 · b 人工回滚 · o/w Owner · x 移除 · r 刷新 · Esc "
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(COLOR_BORDER_FOCUS))
                .title("Plugins · 人工管理")
                .title_bottom(Line::from(footer).alignment(Alignment::Center))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// 生成人类可读的会话更新时间标签。
fn relative_native_time(updated_at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(updated_at_ms);
    let seconds = now_ms.saturating_sub(updated_at_ms) / 1_000;
    match seconds {
        0..=59 => "刚刚".into(),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        _ => format!("{} 天前", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "plugins")]
    use ratatui::{backend::TestBackend, Terminal};

    /// 默认注册表必须包含用户依赖的全部内置命令和退出别名。
    #[test]
    fn native_registry_contains_default_commands() {
        let names = NATIVE_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        #[cfg(feature = "plugins")]
        assert_eq!(
            names,
            vec![
                "help",
                "resume",
                "sessions",
                "plugins",
                "new",
                "clear",
                "compact",
                "reasoning",
                "exit"
            ]
        );
        #[cfg(not(feature = "plugins"))]
        assert_eq!(
            names,
            vec![
                "help",
                "resume",
                "sessions",
                "new",
                "clear",
                "compact",
                "reasoning",
                "exit"
            ]
        );
        assert_eq!(
            NATIVE_COMMANDS.last().expect("应有退出命令").aliases,
            &["quit"]
        );
    }

    /// 插件构建只暴露人工管理入口，不得重新出现插件进化或生成命令。
    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_page_has_no_evolution_actions() {
        let names = NATIVE_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"plugins"));
        assert!(!names.iter().any(|name| {
            name.contains("evolve") || name.contains("mutation") || name.contains("generate")
        }));
        let plugin_help = NATIVE_COMMANDS
            .iter()
            .find(|command| command.name == "plugins")
            .expect("应保留人工插件管理入口");
        assert!(plugin_help.description.contains("人工插件管理"));
        assert!(plugin_help.description.contains("不热重载"));
    }

    /// 插件页必须展示冻结环境摘要和人工操作，且不能出现进化按钮文案。
    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_page_shows_frozen_digest_and_manual_actions() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let environment_digest =
            agent_evolution_protocol::PluginEnvironmentDigest::from_sha256_hex("a".repeat(64))
                .expect("插件环境摘要应合法");
        app.native_command.plugin_dialog = Some(NativePluginDialog {
            selected: 0,
            status: NativePluginDialogStatus::Ready(NativePluginDialogData {
                plugins: vec![agent_plugin_manager::InstalledPlugin {
                    id: "echo".into(),
                    name: "Echo".into(),
                    version: "1.0.0".into(),
                    api_version: "0.7.0".into(),
                    enabled: true,
                    manifest: "plugins/echo/1.0.0/plugin.toml".into(),
                    sha256: "b".repeat(64),
                    source: "fixture".into(),
                }],
                version_history: vec![agent_plugin_manager::InstalledPlugin {
                    id: "echo".into(),
                    name: "Echo".into(),
                    version: "0.9.0".into(),
                    api_version: "0.7.0".into(),
                    enabled: true,
                    manifest: "plugins/echo/0.9.0/plugin.toml".into(),
                    sha256: "c".repeat(64),
                    source: "fixture-history".into(),
                }],
                stable_revision: agent_evolution_protocol::GenomeRevisionId::generate(),
                environment_digest,
                generation: 3,
            }),
            path_mode: None,
            path_input: String::new(),
            remove_confirmation: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(112, 32)).expect("应创建测试终端");

        terminal
            .draw(|frame| render_native_plugin_dialog(frame, &app, frame.area()))
            .expect("应渲染插件管理页");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();

        assert!(text.contains("Frozen Plugin Environment"), "{text}");
        assert!(text.contains("sha256:aaaaaaaaaaaaaaaaa"), "{text}");
        assert!(text.contains("Installed Version History"), "{text}");
        assert!(text.contains("echo@0.9.0"), "{text}");
        assert!(compact.contains("人工管理"), "{text}");
        assert!(!text.contains("Evolve Plugin"), "{text}");
        assert!(!text.contains("Generate Plugin"), "{text}");
    }

    /// 人工插件变更提示必须说明新基线、旧 Session 固定和旧 Candidate 不可应用。
    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_change_warning_is_displayed() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.native_command.plugin_dialog = Some(NativePluginDialog {
            selected: 0,
            status: NativePluginDialogStatus::Applying,
            path_mode: None,
            path_input: String::new(),
            remove_confirmation: None,
        });
        let digest = agent_evolution_protocol::GenomeDigest::from_sha256_hex("b".repeat(64))
            .expect("Genome 摘要应合法");
        let environment_digest =
            agent_evolution_protocol::PluginEnvironmentDigest::from_sha256_hex("c".repeat(64))
                .expect("插件环境摘要应合法");

        app.handle_native_plugin_action_finished(Ok(PluginManagementOutcome {
            message: "已启用 echo 1.0.0".into(),
            baseline: Some(PluginManagementBaseline {
                revision_id: agent_evolution_protocol::GenomeRevisionId::generate(),
                genome_digest: digest,
                plugin_environment_digest: environment_digest,
                generation: 4,
            }),
        }));

        let text = &app.messages.last().expect("应显示人工变更提示").text;
        assert!(text.contains("新 Session"), "{text}");
        assert!(text.contains("当前 Session 继续使用启动时快照"), "{text}");
        assert!(
            text.contains("旧 Evolution Candidate 不得自动应用"),
            "{text}"
        );
    }

    /// Tab 必须只补全规范命令名，不能提前执行命令。
    #[test]
    fn native_completion_replaces_input_without_plugin_host() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.input = "/res".into();
        app.cursor = app.input.len();

        assert!(app.native_command_panel_height() > 0);
        assert!(app.handle_native_command_key(KeyCode::Tab, KeyModifiers::NONE, None));
        assert_eq!(app.input, "/resume ");
        assert_eq!(app.cursor, app.input.len());
        assert!(app.native_command.dialog.is_none());
    }

    /// Enter 必须立即执行当前高亮候选，而不是把命令缩写当作未知命令。
    #[test]
    fn native_completion_executes_selected_command_on_enter() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.input = "/h".into();
        app.cursor = app.input.len();

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
        assert!(app.input.is_empty());
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].text.contains("/help"));
    }

    /// `/sessions` 的浏览入口必须允许切换至当前高亮的精确会话修订。
    #[test]
    fn native_sessions_browse_switches_selected_session_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let session_id = SessionId::new("selected-session").expect("创建测试会话标识");
        app.native_command.dialog = Some(NativeSessionDialog {
            mode: NativeSessionDialogMode::Browse,
            selected: 0,
            status: NativeSessionDialogStatus::Ready(vec![SessionSummary {
                id: session_id.clone(),
                revision: 7,
                created_at_ms: 1,
                updated_at_ms: 2,
                title: Some("待切换会话".into()),
                message_count: 3,
            }]),
        });

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
        assert!(app.native_command.dialog.is_none());
        assert!(matches!(
            rx.try_recv().expect("应发送原生会话切换请求"),
            UiEvent::NativeResumeRequested {
                session_id: requested_id,
                revision: 7,
            } if requested_id == session_id
        ));
    }

    /// `/compact` 在任何 feature 组合下都必须进入原生重载路径。
    #[test]
    fn native_compact_requests_context_reload_without_plugin_host() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());

        app.execute_native_compact();

        assert!(matches!(
            rx.try_recv().expect("原生命令应发布重载请求"),
            UiEvent::NativeCompactRequested
        ));
    }

    /// 原生命令必须消费输入并直接产生界面结果，不能作为消息发送给模型。
    #[test]
    fn native_help_executes_inside_tui() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.input = "/help resume".into();
        app.cursor = app.input.len();

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));
        assert!(app.input.is_empty());
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].text.contains("恢复历史会话"));
    }

    /// `/reasoning` 必须按 models.dev 可用等级更新 TUI 与共享 Agent。
    #[test]
    fn native_reasoning_command_sets_supported_level() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
            vec![ReasoningLevel::Off, ReasoningLevel::High],
            ReasoningLevel::Off,
        );
        let (gateway, options) = build_demo_gateway();
        let agent = Arc::new(Agent::new(gateway, options));
        app.input = "/reasoning high".into();
        app.cursor = app.input.len();

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));

        assert_eq!(app.reasoning_level, ReasoningLevel::High);
        assert_eq!(agent.reasoning_level(), ReasoningLevel::High);
        assert!(app
            .messages
            .last()
            .is_some_and(|message| message.text.contains("思考等级已切换为 高")));
    }

    /// `/reasoning` 不带参数时必须打开二级界面，并默认高亮当前等级。
    #[test]
    fn native_reasoning_command_opens_level_dialog() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
            vec![ReasoningLevel::Off, ReasoningLevel::Medium],
            ReasoningLevel::Medium,
        );
        app.input = "/reasoning".into();
        app.cursor = app.input.len();

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, None));

        let dialog = app
            .native_command
            .reasoning_dialog
            .as_ref()
            .expect("应打开思考等级二级界面");
        assert_eq!(dialog.selected, 1);
        assert!(app.messages.is_empty());
    }

    /// 思考等级二级界面必须支持方向键选择，并在 Enter 后同步共享 Agent。
    #[test]
    fn native_reasoning_dialog_applies_selected_level() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into()).with_reasoning_levels(
            vec![ReasoningLevel::Off, ReasoningLevel::Medium],
            ReasoningLevel::Off,
        );
        let (gateway, options) = build_demo_gateway();
        let agent = Arc::new(Agent::new(gateway, options));
        app.input = "/reasoning".into();
        app.cursor = app.input.len();

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));
        assert!(app.handle_native_command_key(KeyCode::Down, KeyModifiers::NONE, Some(&agent)));
        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent)));

        assert!(app.native_command.reasoning_dialog.is_none());
        assert_eq!(app.reasoning_level, ReasoningLevel::Medium);
        assert_eq!(agent.reasoning_level(), ReasoningLevel::Medium);
    }
}
