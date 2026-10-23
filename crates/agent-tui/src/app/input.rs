//! 主输入区的按键编辑、附件引用、滚动与文本选择。

use super::App;
use crate::app::{is_command_modifier, MouseTextSelection, ESC_DOUBLE_PRESS_WINDOW};
use crate::session::attachment::{
    attachment_media_type, attachment_modality, clipboard_file_path, copy_to_clipboard,
    looks_like_file_path, pasted_file_path,
};
use crate::session::conversation::{
    submission_text, Msg, MsgKind, PendingAttachment, UserSubmission,
};
use crate::theme::MAX_ATTACHMENT_BYTES;
use agent_core::Agent;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use std::path::Path;
use std::sync::Arc;

impl App {
    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) {
        // 诊断模式只回显不执行，让用户看到终端实际上报的组合。
        if self.key_inspect {
            self.report_inspected_key(code, modifiers);
            return;
        }
        if !matches!(code, KeyCode::Esc) {
            self.last_escape_at = None;
        }
        // 退出确认只对连续两次 Ctrl+C 有效，其余按键立即撤销待退出状态。
        if super::is_copy_shortcut(code, modifiers) {
            self.handle_interrupt_or_exit(agent);
            return;
        }
        self.last_exit_request_at = None;
        // Ctrl+D 是终端约定的 EOF：空输入直接退出，有内容时按 readline 语义前向删除。
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('d')) {
            if self.input.is_empty() && self.attachments.is_empty() {
                self.should_quit = true;
            } else if let Some(next) = self.input[self.cursor..].chars().next() {
                let end = self.cursor + next.len_utf8();
                self.input.replace_range(self.cursor..end, "");
                self.finish_input_edit();
            }
            return;
        }
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('y')) {
            self.copy_last_assistant_message();
            return;
        }
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('t')) {
            self.toggle_mouse_capture();
            return;
        }
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('r')) {
            self.cycle_reasoning_level(agent);
            return;
        }
        // Ctrl+P/Ctrl+N 作为方向键历史回溯的兼容入口。
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('p')) {
            // 仅空输入或已处于回溯态时生效，避免覆盖未发送的内容。
            if self.input.is_empty() || self.input_history_cursor.is_some() {
                self.recall_older_input();
            }
            return;
        }
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('n')) {
            if self.input_history_cursor.is_some() {
                self.recall_newer_input();
            }
            return;
        }
        // 多行编辑键优先于命令预览，避免补全弹层吞掉换行手势。
        // Shift+Enter 依赖键盘增强协议；Alt+Enter 与 Ctrl+J 在传统终端也可用。
        if (matches!(code, KeyCode::Enter)
            && modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT))
            || (is_command_modifier(modifiers) && matches!(code, KeyCode::Char('j')))
        {
            self.insert_input_text("\n");
            return;
        }

        match code {
            KeyCode::Char('w') if is_command_modifier(modifiers) => {
                let start = previous_word_boundary(&self.input, self.cursor);
                if start < self.cursor {
                    self.input.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.finish_input_edit();
                }
            }
            // readline 行内编辑：终端用户的肌肉记忆，缺失时只能靠退格逐字删。
            KeyCode::Char('a') if is_command_modifier(modifiers) => {
                self.cursor = self.current_line_start();
            }
            KeyCode::Char('e') if is_command_modifier(modifiers) => {
                self.cursor = self.current_line_end();
            }
            KeyCode::Char('u') if is_command_modifier(modifiers) => {
                let start = self.current_line_start();
                if start < self.cursor {
                    self.input.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.finish_input_edit();
                }
            }
            KeyCode::Char('k') if is_command_modifier(modifiers) => {
                let end = self.current_line_end();
                if end > self.cursor {
                    self.input.replace_range(self.cursor..end, "");
                    self.finish_input_edit();
                }
            }
            KeyCode::Home if is_command_modifier(modifiers) => {
                self.scroll_up(u16::MAX);
            }
            KeyCode::End if is_command_modifier(modifiers) => {
                self.scroll = None;
            }
            // Alt 与 Ctrl 都映射为按词移动：不同终端对这两个修饰键的上报并不一致。
            KeyCode::Left
                if modifiers.contains(KeyModifiers::ALT) || is_command_modifier(modifiers) =>
            {
                self.cursor = previous_word_boundary(&self.input, self.cursor);
            }
            KeyCode::Right
                if modifiers.contains(KeyModifiers::ALT) || is_command_modifier(modifiers) =>
            {
                self.cursor = next_word_boundary(&self.input, self.cursor);
            }
            KeyCode::Enter => {
                // 后台命令可能随时替换会话记录，期间禁止并发提交新消息。
                if self.pending_reload.is_some() {
                    self.messages
                        .push(Msg::new(MsgKind::Info, "上下文压缩仍在进行中，请稍候"));
                    return;
                }
                if let Some(agent) = agent {
                    if self.running {
                        // steering 只支持纯文本；带附件的输入排队到本轮结束后整条发送，
                        // 而不是拒绝用户已经打好的内容。
                        if self.attachments.is_empty() {
                            self.submit_steering(agent);
                        } else {
                            self.queue_input_until_ready();
                        }
                    } else {
                        self.submit(agent);
                    }
                } else {
                    self.queue_input_until_ready();
                }
            }
            KeyCode::Esc => {
                // 运行中优先中断；空闲输入必须连续双按才清空，Esc 永不直接退出。
                if self.running {
                    self.last_escape_at = None;
                    self.interrupt_run(agent);
                } else if self.input.is_empty() && self.attachments.is_empty() {
                    self.last_escape_at = None;
                } else {
                    let now = std::time::Instant::now();
                    let double_pressed = self.last_escape_at.take().is_some_and(|previous| {
                        now.saturating_duration_since(previous) <= ESC_DOUBLE_PRESS_WINDOW
                    });
                    if double_pressed {
                        self.input.clear();
                        self.cursor = 0;
                        self.finish_input_edit();
                    } else {
                        self.last_escape_at = Some(now);
                    }
                }
            }
            KeyCode::PageUp => self.scroll_up(self.last_viewport.saturating_sub(1).max(1)),
            KeyCode::PageDown => self.scroll_down(self.last_viewport.saturating_sub(1).max(1)),
            KeyCode::Up => {
                if self.input_history_cursor.is_some() {
                    self.recall_older_input();
                } else if !(self.input.contains('\n')
                    && move_cursor_vertically(&self.input, &mut self.cursor, true))
                {
                    // 空输入框直接回填最近一次提交；非空单行草稿不得被历史覆盖。
                    if self.input.is_empty() {
                        self.recall_older_input();
                    }
                }
            }
            KeyCode::Down => {
                if self.input_history_cursor.is_some() {
                    self.recall_newer_input();
                } else if !(self.input.contains('\n')
                    && move_cursor_vertically(&self.input, &mut self.cursor, false))
                {
                    // 未处于历史回溯态时向下键不修改当前草稿。
                }
            }
            KeyCode::Char(c) => {
                let mut encoded = [0; 4];
                self.insert_input_text(c.encode_utf8(&mut encoded));
            }
            KeyCode::Backspace => {
                // 光标紧跟附件引用标签时整体删除标签与附件。
                if !self.remove_attachment_before_cursor() {
                    if let Some(prev) = self.input[..self.cursor].chars().last() {
                        self.cursor -= prev.len_utf8();
                        self.input.remove(self.cursor);
                    }
                    self.prune_attachments();
                }
                self.finish_input_edit();
            }
            KeyCode::Delete => {
                if let Some(next) = self.input[self.cursor..].chars().next() {
                    let end = self.cursor + next.len_utf8();
                    self.input.replace_range(self.cursor..end, "");
                    self.finish_input_edit();
                }
            }
            KeyCode::Left => {
                if let Some(prev) = self.input[..self.cursor].chars().last() {
                    self.cursor -= prev.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(next) = self.input[self.cursor..].chars().next() {
                    self.cursor += next.len_utf8();
                }
            }
            KeyCode::Home => {
                // 多行输入时回到当前逻辑行行首；单行输入等价于整段开头。
                self.cursor = self.input[..self.cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
            }
            KeyCode::End => {
                // 多行输入时到当前逻辑行行尾；单行输入等价于整段末尾。
                self.cursor = self.input[self.cursor..]
                    .find('\n')
                    .map_or(self.input.len(), |index| self.cursor + index);
            }
            _ => {}
        }
    }

    /// 进入按键诊断模式。
    ///
    /// 修饰键被系统重映射（macOS 常见的 Control/Option/Command 互换）后，同一个物理
    /// 键到达终端时可能变成另一个修饰键。诊断模式直接回显实际收到的组合，用来确认
    /// 快捷键失效是重映射导致还是应用本身没有绑定。
    pub(crate) fn start_key_inspection(&mut self) {
        self.key_inspect = true;
        self.messages.push(Msg::new(
            MsgKind::Info,
            "按键诊断已开启：逐个按下要检查的组合，界面会显示终端实际上报的键名与修饰键。按 Esc 退出。",
        ));
    }

    /// 回显一次被诊断模式捕获的按键；`Esc` 退出诊断。
    fn report_inspected_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if matches!(code, KeyCode::Esc) && modifiers.is_empty() {
            self.key_inspect = false;
            self.messages
                .push(Msg::new(MsgKind::Info, "按键诊断已关闭。"));
            return;
        }
        let key = crate::bus::plugin_key_code(code);
        let names = crate::bus::plugin_key_modifiers(modifiers);
        let combination = if names.is_empty() {
            key
        } else {
            format!("{} + {key}", names.join(" + "))
        };
        // 同时说明该组合是否会被识别为命令修饰键，直接回答"我的快捷键为什么不生效"。
        let recognized = if super::is_command_modifier(modifiers) {
            "识别为命令修饰键"
        } else if modifiers.is_empty() {
            "无修饰键"
        } else {
            "不是命令修饰键，快捷键不会触发"
        };
        self.messages.push(Msg::new(
            MsgKind::Info,
            format!("按键：{combination} · {recognized}"),
        ));
    }

    /// 返回光标所在逻辑行的起始字节位置。
    fn current_line_start(&self) -> usize {
        self.input[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    /// 返回光标所在逻辑行的结束字节位置。
    fn current_line_end(&self) -> usize {
        self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |index| self.cursor + index)
    }

    /// 中断当前运行：关闭可见流、标记未完成工具为跳过，并通知 Agent 取消。
    ///
    /// 重复调用是幂等的，不会重复推送提示；Esc 与 Ctrl+C 共用同一条路径。
    fn interrupt_run(&mut self, agent: Option<&Arc<Agent>>) {
        if self.run_stream_cancelled {
            return;
        }
        self.run_stream_cancelled = true;
        self.model_is_thinking = false;
        self.streaming_message = None;
        for message in &mut self.messages {
            if matches!(message.kind, MsgKind::ToolRunning) {
                message.kind = MsgKind::ToolSkipped;
                message.skip_reason = Some("Cancelled by user".into());
            }
        }
        if let Some(agent) = agent {
            agent.cancel();
        }
        self.messages
            .push(Msg::new(MsgKind::Info, "Cancelling the current run..."));
    }

    /// 处理 Ctrl+C：按当前状态依次表示"中断运行""清空输入""确认退出"。
    ///
    /// 退出必须二次确认——单击即退会让一次误触丢掉整个会话，而中断和清空都是可恢复的。
    fn handle_interrupt_or_exit(&mut self, agent: Option<&Arc<Agent>>) {
        if self.running {
            self.last_exit_request_at = None;
            self.interrupt_run(agent);
            return;
        }
        if !self.input.is_empty() || !self.attachments.is_empty() {
            self.last_exit_request_at = None;
            self.input.clear();
            self.attachments.clear();
            self.cursor = 0;
            self.finish_input_edit();
            return;
        }
        let now = std::time::Instant::now();
        let confirmed = self.last_exit_request_at.take().is_some_and(|previous| {
            now.saturating_duration_since(previous) <= super::EXIT_CONFIRM_WINDOW
        });
        if confirmed {
            self.should_quit = true;
        } else {
            self.last_exit_request_at = Some(now);
        }
    }

    /// 返回是否正在等待第二次 Ctrl+C 确认退出。
    pub(crate) fn exit_confirmation_pending(&self) -> bool {
        self.last_exit_request_at
            .is_some_and(|requested| requested.elapsed() <= super::EXIT_CONFIRM_WINDOW)
    }

    /// 处理"回到底部"提示上的左键点击，返回事件是否已被热区消费。
    ///
    /// 必须在文本拖选之前调用：否则点击会先被当作一次选择起点，热区永远收不到事件。
    pub(crate) fn handle_jump_hint_click(&mut self, mouse: &MouseEvent) -> bool {
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(crossterm::event::MouseButton::Left)
        ) {
            return false;
        }
        let Some(area) = self.jump_hint_area else {
            return false;
        };
        if !area.contains(Position::new(mouse.column, mouse.row)) {
            return false;
        }
        self.dismiss_text_selection();
        self.tool_click_candidate = None;
        self.scroll = None;
        true
    }

    /// 处理滚动条上的按下与拖拽，返回事件是否已被热区消费。
    ///
    /// 必须排在文本拖选之前：滚动条只占右侧一列，若不先截获，按下会被当作选择起点，
    /// 拖动则会变成选区而永远滚不动。拖到最底部时恢复自动跟随，与键盘滚动语义一致。
    pub(crate) fn handle_scrollbar_drag(&mut self, mouse: &MouseEvent) -> bool {
        use crossterm::event::MouseButton;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(area) = self.scrollbar_area else {
                    return false;
                };
                if !area.contains(Position::new(mouse.column, mouse.row)) {
                    return false;
                }
                self.dismiss_text_selection();
                self.tool_click_candidate = None;
                self.scrollbar_drag = true;
                self.scrub_to_row(mouse.row);
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.scrollbar_drag {
                    return false;
                }
                self.scrub_to_row(mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => std::mem::take(&mut self.scrollbar_drag),
            _ => false,
        }
    }

    /// 把滚动条上的行号映射为滚动位置；行号越界时按端点处理。
    fn scrub_to_row(&mut self, row: u16) {
        let Some(area) = self.scrollbar_area else {
            return;
        };
        if area.height == 0 || self.last_max_scroll == 0 {
            return;
        }
        let offset = row.saturating_sub(area.y).min(area.height - 1);
        // 轨道首行对应顶部、末行对应底部；两端都能精确命中，避免滑块"够不到"边界。
        let span = u32::from(area.height.saturating_sub(1)).max(1);
        let scaled = u32::from(offset) * u32::from(self.last_max_scroll);
        let position = u16::try_from((scaled + span / 2) / span).unwrap_or(u16::MAX);
        if position >= self.last_max_scroll {
            self.scroll = None;
        } else {
            self.scroll = Some(position);
        }
    }

    /// 向上滚动 n 行，进入手动滚动模式。
    pub(crate) fn scroll_up(&mut self, n: u16) {
        self.dismiss_text_selection();
        self.tool_click_candidate = None;
        let current = self.scroll.unwrap_or(self.last_max_scroll);
        self.scroll = Some(current.saturating_sub(n));
    }

    /// 向下滚动 n 行；到达底部时恢复自动跟随。
    pub(crate) fn scroll_down(&mut self, n: u16) {
        self.dismiss_text_selection();
        self.tool_click_candidate = None;
        if let Some(current) = self.scroll {
            let next = current.saturating_add(n);
            if next >= self.last_max_scroll {
                self.scroll = None;
            } else {
                self.scroll = Some(next);
            }
        }
    }

    /// 在当前光标处插入文本，并退出历史回溯态、同步附件状态。
    fn insert_input_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.finish_input_edit();
    }

    /// 完成一次输入内容修改后的统一清理。
    fn finish_input_edit(&mut self) {
        self.input_history_cursor = None;
        self.last_escape_at = None;
        self.prune_attachments();
    }

    /// 召回更早的一条输入；空历史保持当前编辑器不变。
    fn recall_older_input(&mut self) {
        let Some(index) = self.input_history_cursor.map_or_else(
            || self.input_history.len().checked_sub(1),
            |current| current.checked_sub(1),
        ) else {
            return;
        };
        self.input_history_cursor = Some(index);
        self.input.clone_from(&self.input_history[index]);
        self.cursor = self.input.len();
        self.prune_attachments();
    }

    /// 向较新的输入回溯；越过最新记录时恢复空编辑器。
    fn recall_newer_input(&mut self) {
        let Some(current) = self.input_history_cursor else {
            return;
        };
        if current + 1 < self.input_history.len() {
            let index = current + 1;
            self.input_history_cursor = Some(index);
            self.input.clone_from(&self.input_history[index]);
        } else {
            self.input_history_cursor = None;
            self.input.clear();
        }
        self.cursor = self.input.len();
        self.prune_attachments();
    }

    /// 记录成功提交的非空输入，忽略连续重复并只保留最近五十条。
    fn remember_input(&mut self, input: &str) {
        if input.is_empty() || self.input_history.last().is_some_and(|last| last == input) {
            self.input_history_cursor = None;
            return;
        }
        if self.input_history.len() == 50 {
            self.input_history.remove(0);
        }
        self.input_history.push(input.to_string());
        self.input_history_cursor = None;
    }

    /// 处理终端粘贴：文件路径转为附件，其余文本折叠为引用标签并保留原文待发送。
    pub(crate) fn handle_paste(&mut self, pasted: &str) {
        self.handle_paste_with(pasted, clipboard_file_path);
    }

    /// [`App::handle_paste`] 的可测试实现。
    ///
    /// `clipboard` 注入剪贴板文件引用的读取方式：Finder 复制文件时剪贴板文本只有
    /// 文件名，必须回查文件引用才能定位；测试传入固定结果，避免依赖本机剪贴板状态。
    pub(crate) fn handle_paste_with(
        &mut self,
        pasted: &str,
        clipboard: impl FnOnce() -> Option<std::path::PathBuf>,
    ) {
        let cwd = self.workspace.cwd.clone();
        if let Some(path) = pasted_file_path(pasted, &cwd) {
            self.attach_file(&path);
            return;
        }
        // 文本看起来是附件路径但按工作目录找不到：回查剪贴板文件引用（Finder 复制场景）。
        if looks_like_file_path(pasted) {
            match clipboard() {
                Some(path) => {
                    self.attach_file(&path);
                }
                None => self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!(
                        "无法定位附件 `{}`：请粘贴绝对路径，或重新从文件管理器复制该文件。",
                        pasted.trim()
                    ),
                )),
            }
            return;
        }
        // 统一平台换行，制表符仍降级为空格以避免不同终端产生不可见差异。
        let text = pasted
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', " ");
        if text.is_empty() {
            return;
        }
        if let Err(message) =
            self.validate_attachment_modality(&crate::config::model_metadata::ModelModality::Text)
        {
            self.messages.push(Msg::new(MsgKind::Error, message));
            return;
        }
        let label = self.next_pasted_text_label();
        self.attachments.push(PendingAttachment {
            label: label.clone(),
            name: "粘贴的文本".to_string(),
            media_type: "text/plain".to_string(),
            data: String::new(),
            is_image: false,
            pasted_text: Some(text),
        });
        let clip = format!("{label} ");
        self.input.insert_str(self.cursor, &clip);
        self.cursor += clip.len();
        self.finish_input_edit();
    }

    /// 读取文件并加入待发送附件，同时在光标处插入引用标签。
    ///
    /// 读取失败或超过 [`MAX_ATTACHMENT_BYTES`] 时追加错误消息，不修改输入。
    pub(crate) fn attach_file(&mut self, path: &Path) {
        use base64::Engine;

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("Failed to read attachment ({}): {error}", path.display()),
                ));
                return;
            }
        };
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!(
                    "Attachment exceeds the {} MiB limit: {}",
                    MAX_ATTACHMENT_BYTES / 1024 / 1024,
                    path.display()
                ),
            ));
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        let (media_type, is_image) = attachment_media_type(path, &bytes);
        let modality = attachment_modality(&media_type);
        if let Err(message) = self.validate_attachment_modality(&modality) {
            self.messages.push(Msg::new(MsgKind::Error, message));
            return;
        }
        let label = self.next_attachment_label(&modality);
        self.attachments.push(PendingAttachment {
            label: label.clone(),
            name,
            media_type,
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            is_image,
            pasted_text: None,
        });
        let clip = format!("{label} ");
        self.input.insert_str(self.cursor, &clip);
        self.cursor += clip.len();
        self.finish_input_edit();
    }

    /// 校验附件所需的模型输入模态，并隔离 Core 尚未实现的音视频协议。
    fn validate_attachment_modality(
        &self,
        modality: &crate::config::model_metadata::ModelModality,
    ) -> std::result::Result<(), String> {
        use crate::config::model_metadata::ModelModality;

        if let Some(supported) = self.model_input_modalities.as_ref() {
            if !supported.contains(modality) {
                return Err(format!(
                    "当前模型不支持{}输入；能力来自内置 models.dev 快照",
                    modality.label()
                ));
            }
        }
        if matches!(modality, ModelModality::Audio | ModelModality::Video) {
            return Err(format!(
                "当前模型支持{}输入，但 Lucia 当前模型协议尚未实现该内容块",
                modality.label()
            ));
        }
        Ok(())
    }

    /// 按附件模态生成不冲突的递增编号标签。
    pub(crate) fn next_attachment_label(
        &self,
        modality: &crate::config::model_metadata::ModelModality,
    ) -> String {
        use crate::config::model_metadata::ModelModality;

        let prefix = match modality {
            ModelModality::Image => "IMAGE",
            ModelModality::Pdf => "PDF",
            _ => "FILE",
        };
        let mut index = 1;
        loop {
            let label = format!("[{prefix}#{index}]");
            if !self
                .attachments
                .iter()
                .any(|attachment| attachment.label == label)
            {
                return label;
            }
            index += 1;
        }
    }

    /// 生成粘贴文本标签；首项使用固定文案，后续项追加序号以保持可独立删除。
    fn next_pasted_text_label(&self) -> String {
        let mut index = 1;
        loop {
            let label = if index == 1 {
                "[粘贴的文本]".to_string()
            } else {
                format!("[粘贴的文本#{index}]")
            };
            if !self
                .attachments
                .iter()
                .any(|attachment| attachment.label == label)
            {
                return label;
            }
            index += 1;
        }
    }

    /// 丢弃引用标签已不在输入文本中的附件，保持标签与附件一一对应。
    pub(crate) fn prune_attachments(&mut self) {
        if self.attachments.is_empty() {
            return;
        }
        let input = &self.input;
        self.attachments.retain(|a| input.contains(&a.label));
    }

    /// 光标紧跟附件引用标签时整体删除标签及对应附件，返回是否命中。
    pub(crate) fn remove_attachment_before_cursor(&mut self) -> bool {
        let prefix = &self.input[..self.cursor];
        let Some(index) = self
            .attachments
            .iter()
            .position(|a| prefix.ends_with(&a.label))
        else {
            return false;
        };
        let start = self.cursor - self.attachments[index].label.len();
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.attachments.remove(index);
        true
    }

    /// 复制最近一条助手回复原文（Markdown）到系统剪贴板。
    pub(crate) fn copy_last_assistant_message(&mut self) {
        let Some(text) = self
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.kind, MsgKind::Assistant) && !m.text.is_empty())
            .map(|m| m.text.clone())
        else {
            self.messages
                .push(Msg::new(MsgKind::Info, "No assistant reply to copy"));
            return;
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self.messages.push(Msg::new(
                MsgKind::Info,
                "Copied the latest reply to the clipboard",
            )),
            Err(error) => self
                .messages
                .push(Msg::new(MsgKind::Error, format!("Copy failed: {error}"))),
        }
    }

    /// 切换鼠标捕获：暂停后终端可用原生选择复制任意消息文本，恢复后支持滚轮滚动。
    pub(crate) fn toggle_mouse_capture(&mut self) {
        let result = if self.mouse_capture {
            crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
        } else {
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
        };
        if result.is_err() {
            return;
        }
        self.mouse_capture = !self.mouse_capture;
        self.dismiss_text_selection();
        let notice = if self.mouse_capture {
            "Mouse capture enabled; press Ctrl+T to restore text selection"
        } else {
            "Text selection restored; press Ctrl+T to enable mouse interaction"
        };
        self.messages.push(Msg::new(MsgKind::Info, notice));
    }

    /// 更新应用内文本选择，左键释放后只保留高亮范围，不主动修改系统剪贴板。
    ///
    /// 单击只重置选择；发生拖动后，选择按终端的线性行顺序展开。选区必须再由
    /// `Ctrl+C` 显式复制，避免鼠标释放意外覆盖用户已有剪贴板内容。
    pub(crate) fn handle_text_selection(&mut self, mouse: &MouseEvent, buffer: &Buffer) {
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.dismiss_text_selection();
                self.text_selection = Some(MouseTextSelection::new(position));
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                self.text_selection_content = None;
                if let Some(selection) = self.text_selection.as_mut() {
                    selection.update(position);
                }
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                let Some(selection) = self.text_selection.as_mut() else {
                    return;
                };
                selection.update(position);
                let text = if selection.dragged {
                    selection.extract(buffer)
                } else {
                    String::new()
                };
                self.text_selection_content = (!text.is_empty()).then_some(text);
            }
            _ => {}
        }
    }

    /// 处理工具消息的左键点击，并在未发生文本拖选时切换详情展开状态。
    ///
    /// 按下与释放必须命中同一条实时消息；滚动、拖动、审批快照或消息失效都会取消候选，
    /// 避免把复制手势和覆盖层点击误判为展开操作。返回 `true` 表示释放事件已被消费。
    pub(crate) fn handle_tool_message_click(&mut self, mouse: &MouseEvent) -> bool {
        #[cfg(feature = "plugins")]
        if self.main_event_snapshot.is_some() {
            self.tool_click_candidate = None;
            return false;
        }

        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.tool_click_candidate = self
                    .tool_message_hit_regions
                    .iter()
                    .find(|region| region.area.contains(position))
                    .map(|region| region.message_index);
                false
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                self.tool_click_candidate = None;
                false
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                let Some(message_index) = self.tool_click_candidate.take() else {
                    return false;
                };
                if self
                    .text_selection
                    .is_some_and(|selection| selection.dragged)
                {
                    return false;
                }
                let still_inside = self.tool_message_hit_regions.iter().any(|region| {
                    region.message_index == message_index && region.area.contains(position)
                });
                if !still_inside {
                    return false;
                }
                let Some(message) = self.messages.get_mut(message_index) else {
                    return false;
                };
                // 内容没有被藏起来时不做任何切换，点击继续交给文本选择处理。
                if !message.is_tool_expandable() {
                    return false;
                }
                message.tool_expanded = !message.tool_expanded;
                self.dismiss_text_selection();
                true
            }
            _ => false,
        }
    }

    /// 将左键释放时冻结的非空选区复制到系统剪贴板，并报告按键是否应被选区消费。
    ///
    /// 没有有效拖选时返回 `false`，调用方可继续执行 `Ctrl+C` 的退出行为；存在选区时
    /// 即使剪贴板写入失败也返回 `true`，并保留选区供重试，避免一次失败意外退出。
    pub(crate) fn copy_text_selection(&mut self) -> bool {
        let Some(text) = self.text_selection_content.clone() else {
            return false;
        };
        if let Err(error) = copy_to_clipboard(&text) {
            self.messages
                .push(Msg::new(MsgKind::Error, format!("Copy failed: {error}")));
        } else {
            self.dismiss_text_selection();
        }
        true
    }

    /// 返回当前是否存在已释放、可由 `Ctrl+C` 复制的文本选区。
    pub(crate) fn has_copyable_text_selection(&self) -> bool {
        self.text_selection_content.is_some()
    }

    /// 清除应用内拖选坐标与冻结文本；滚动、继续操作或复制成功后调用。
    pub(crate) fn dismiss_text_selection(&mut self) {
        self.text_selection = None;
        self.text_selection_content = None;
    }

    /// 在当前帧缓冲区中反色显示应用内鼠标选择，不改变字符内容。
    pub(crate) fn render_text_selection(&self, buffer: &mut Buffer) {
        let Some(selection) = self.text_selection.filter(|selection| selection.dragged) else {
            return;
        };
        selection.for_each_position(buffer.area, |position| {
            if let Some(cell) = buffer.cell_mut(position) {
                cell.modifier.insert(Modifier::REVERSED);
            }
        });
    }

    /// 取出并清空当前编辑器内容；空白输入返回 `None`。
    ///
    /// steering 等纯文本路径不携带附件，残留附件一并清除，
    /// 避免引用标签消失后附件悬挂。
    pub(crate) fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.attachments.clear();
        self.cursor = 0;
        self.last_escape_at = None;
        if input.is_empty() {
            return None;
        }
        self.remember_input(&input);
        Some(input)
    }

    /// 取出并清空当前输入与附件；文本和附件都为空时返回 `None`。
    pub(crate) fn take_submission(&mut self) -> Option<UserSubmission> {
        if !submission_text(&self.input, &self.attachments).is_empty() {
            if let Err(message) = self
                .validate_attachment_modality(&crate::config::model_metadata::ModelModality::Text)
            {
                self.messages.push(Msg::new(MsgKind::Error, message));
                return None;
            }
        }
        let text = self.input.trim().to_string();
        let attachments = std::mem::take(&mut self.attachments);
        self.input.clear();
        self.cursor = 0;
        self.last_escape_at = None;
        if text.is_empty() && attachments.is_empty() {
            return None;
        }
        self.remember_input(&text);
        Some(UserSubmission { text, attachments })
    }
}

/// 判断字符是否为按词移动与删除使用的分隔符。
fn is_word_separator(ch: char) -> bool {
    ch.is_whitespace() || ch.is_ascii_punctuation()
}

/// 返回光标左侧一个词的起始字节位置。
fn previous_word_boundary(input: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while let Some(ch) = input[..position].chars().next_back() {
        if !is_word_separator(ch) {
            break;
        }
        position -= ch.len_utf8();
    }
    while let Some(ch) = input[..position].chars().next_back() {
        if is_word_separator(ch) {
            break;
        }
        position -= ch.len_utf8();
    }
    position
}

/// 返回光标右侧下一个词的起始字节位置。
fn next_word_boundary(input: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while let Some(ch) = input[position..].chars().next() {
        if is_word_separator(ch) {
            break;
        }
        position += ch.len_utf8();
    }
    while let Some(ch) = input[position..].chars().next() {
        if !is_word_separator(ch) {
            break;
        }
        position += ch.len_utf8();
    }
    position
}

/// 在目标逻辑行中找到不超过指定显示列的字节位置。
fn byte_at_display_column(line: &str, target: usize) -> usize {
    let mut width = 0;
    let mut offset = 0;
    for ch in line.chars() {
        let next = width + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if next > target {
            break;
        }
        width = next;
        offset += ch.len_utf8();
    }
    offset
}

/// 按显示列在相邻逻辑行间移动光标；到达首行或末行时返回 `false`。
fn move_cursor_vertically(input: &str, cursor: &mut usize, upward: bool) -> bool {
    let line_start = input[..*cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_end = input[*cursor..]
        .find('\n')
        .map_or(input.len(), |index| *cursor + index);
    let column = unicode_width::UnicodeWidthStr::width(&input[line_start..*cursor]);
    let (target_start, target_end) = if upward {
        if line_start == 0 {
            return false;
        }
        let end = line_start - 1;
        let start = input[..end].rfind('\n').map_or(0, |index| index + 1);
        (start, end)
    } else {
        if line_end == input.len() {
            return false;
        }
        let start = line_end + 1;
        let end = input[start..]
            .find('\n')
            .map_or(input.len(), |index| start + index);
        (start, end)
    };
    *cursor = target_start + byte_at_display_column(&input[target_start..target_end], column);
    true
}
