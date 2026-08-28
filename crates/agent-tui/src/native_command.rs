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
        name: "exit",
        aliases: &["quit"],
        summary: "退出 Lucia",
        description: "正常退出 TUI，不修改历史会话。",
        idle_only: true,
    },
];

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

/// TUI 原生命令状态；不依赖 Plugin Host，纯 Core 构建同样可用。
#[derive(Debug, Default)]
pub(crate) struct NativeCommandState {
    /// 补全面板当前高亮候选。
    pub(crate) selected: usize,
    /// 用户按 Esc 隐藏面板时的完整输入；输入发生变化后自动失效。
    pub(crate) dismissed_input: Option<String>,
    /// 当前会话对话框。
    pub(crate) dialog: Option<NativeSessionDialog>,
}

impl App {
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
                self.execute_native_command();
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
    fn execute_native_command(&mut self) {
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
        let Some(command) = NATIVE_COMMANDS.iter().find(|command| {
            command.name == name || command.aliases.iter().any(|alias| *alias == name)
        }) else {
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
            "exit" => self.should_quit = true,
            _ => unreachable!("原生命令表与执行路由必须同步"),
        }
    }

    /// 在消息流中显示全部命令或指定命令的说明。
    fn show_native_command_help(&mut self, target: Option<&str>) {
        let content = match target.map(|value| value.trim_start_matches('/')) {
            Some(target) => match NATIVE_COMMANDS.iter().find(|command| {
                command.name == target || command.aliases.iter().any(|alias| *alias == target)
            }) {
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
    let width = outer.width.saturating_sub(4).min(88).max(20);
    let height = outer.height.saturating_sub(4).min(24).max(6);
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

    /// 默认注册表必须包含用户依赖的全部内置命令和退出别名。
    #[test]
    fn native_registry_contains_default_commands() {
        let names = NATIVE_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["help", "resume", "sessions", "new", "clear", "compact", "exit"]
        );
        assert_eq!(
            NATIVE_COMMANDS.last().expect("应有退出命令").aliases,
            &["quit"]
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
        assert!(app.handle_native_command_key(KeyCode::Tab, KeyModifiers::NONE));
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

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE));
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

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE));
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

        assert!(app.handle_native_command_key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].text.contains("恢复历史会话"));
    }
}
