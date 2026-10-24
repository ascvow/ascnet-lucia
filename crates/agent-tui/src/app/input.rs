//! 主输入区的按键编辑、附件引用、滚动与文本选择。

use super::App;
use crate::app::{is_command_modifier, MouseTextSelection, ESC_DOUBLE_PRESS_WINDOW};
use crate::session::attachment::{
    attachment_media_type, attachment_modality, clipboard_files, copy_to_clipboard,
    is_clipboard_file_names, looks_like_single_pasted_path, may_be_clipboard_file_names,
    mentioned_paths, pasted_file_uris, pasted_path, pasted_path_kind, pasted_paths, PastedPathKind,
    MAX_AUTO_TEXT_ATTACHMENT_BYTES,
};
use crate::session::conversation::{
    retained_refs, submission_text, Msg, MsgKind, PendingAttachment, UserSubmission,
};
use crate::theme::MAX_ATTACHMENT_BYTES;
use crate::ui::chat::{scrollbar_thumb, ScrollbarThumb};
use agent_core::Agent;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use std::path::Path;
use std::sync::Arc;

/// 拖动选区越过内容区边界时，单次拖动事件最多滚动的行数。
///
/// 上限的作用是让靠近边界时的拖动保持精细：贴边一格只滚一行，拖到屏幕外很远才加速。
const AUTOSCROLL_MAX_STEP: i32 = 5;

/// 单行粘贴折叠为引用标签的显示宽度门槛。
///
/// 输入盒最多长到六行，这个宽度以内的单行内容在常见终端里只占一两行，仍能看清、能就地
/// 改字；超过它或本身就是多行时，直接铺进输入框只会把内容挤成一团，才收成
/// `[粘贴的文本]` 标签。
pub(crate) const PASTE_TAG_COLUMNS: usize = 120;

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
        // raw mode 让终端不再把 Ctrl+Z 变成 SIGTSTP，这里登记意图：由主循环先恢复
        // 终端状态再真正挂起，否则外层 shell 会一直收到鼠标上报序列。
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('z')) {
            self.pending_suspend = true;
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
        // Ctrl+O 进入排队面板；点击排队行也会进入，方向键选择后按 Enter 发送。
        if is_command_modifier(modifiers) && matches!(code, KeyCode::Char('o')) {
            self.focus_queue();
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
                        // 正文里点名的图片/PDF 同样要先变成附件，排队后才能整条发送。
                        self.attach_mentioned_paths();
                        // 默认排队：消息显示在输入框上方，可在本轮结束前继续修改。
                        // Ctrl+Enter 才是立即 steering；附件与引用无法 steering，仍排队。
                        let steer = is_command_modifier(modifiers)
                            && self.attachments.is_empty()
                            && self.refs.is_empty();
                        if steer {
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
                        self.attachments.clear();
                        self.refs.clear();
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

    /// 排队面板获得键盘焦点时的按键处理。
    ///
    /// 面板聚焦期间吞掉所有按键，避免字符落进输入框：`↑`/`↓` 选择，`Enter` 发送，
    /// `Ctrl+O` 取回编辑，`Delete` 丢弃，`Esc` 返回输入框。
    pub(crate) fn handle_queue_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        agent: Option<&Arc<Agent>>,
    ) {
        if self.queue_focus.is_none() {
            return;
        }
        match code {
            KeyCode::Esc => self.queue_focus = None,
            KeyCode::Up => self.move_queue_selection(-1),
            KeyCode::Down => self.move_queue_selection(1),
            KeyCode::Enter => self.send_selected_queued(agent),
            KeyCode::Delete => self.remove_selected_queued(),
            KeyCode::Char('o') if is_command_modifier(modifiers) => {
                if let Some(index) = self.queue_focus {
                    self.edit_queued_input(index);
                }
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
        // 取消前先把已收到的文本补完：否则屏幕会永远停在"差几个字"的状态。
        self.flush_pending_stream();
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
            self.refs.clear();
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

    /// 处理底栏上下文芯片上的左键点击，返回事件是否已被热区消费。
    ///
    /// 芯片是 `/context` 的鼠标入口：点开最近一次请求的分类详情，再点一次收起，
    /// 效果与命令面板里的 `/context` 完全一致。与其他热区一样必须先于文本拖选判定，
    /// 否则按下会被当成一次选区起点。
    pub(crate) fn handle_context_chip_click(&mut self, mouse: &MouseEvent) -> bool {
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(crossterm::event::MouseButton::Left)
        ) {
            return false;
        }
        let Some(area) = self.context_chip_area else {
            return false;
        };
        if !area.contains(Position::new(mouse.column, mouse.row)) {
            return false;
        }
        self.dismiss_text_selection();
        self.tool_click_candidate = None;
        if self.native_command.context_open {
            self.native_command.context_open = false;
        } else {
            self.native_command.context_open = true;
            self.native_command.context_scroll = 0;
        }
        true
    }

    /// 处理排队消息面板上的左键点击：进入面板并选中被点中的消息。
    ///
    /// 只有主界面且没有对话框或插件焦点时生效；命中行消费这次点击，避免落到下方的
    /// 文本选择逻辑。点击面板以外的地方会退出排队焦点。返回事件是否已被消费。
    pub(crate) fn handle_queued_input_click(&mut self, mouse: &MouseEvent) -> bool {
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(crossterm::event::MouseButton::Left)
        ) {
            return false;
        }
        if self.native_dialog_active() {
            return false;
        }
        #[cfg(feature = "plugins")]
        if !self.view_stack.is_main()
            || self.plugin_focus.is_some()
            || self.active_dialog_index().is_some()
        {
            return false;
        }
        let position = Position::new(mouse.column, mouse.row);
        let Some(index) = self
            .queued_input_areas
            .iter()
            .find(|(_, area)| area.contains(position))
            .map(|(index, _)| *index)
        else {
            // 点击面板以外的地方视为离开排队区，把焦点还给输入框。
            self.queue_focus = None;
            return false;
        };
        self.dismiss_text_selection();
        self.tool_click_candidate = None;
        self.queue_focus = Some(index);
        true
    }

    /// 更新底栏热点（滚动条、上下文芯片）的悬停态，返回状态是否发生变化。
    ///
    /// 只有真正的指针移动（[`MouseEventKind::Moved`]）才会改变悬停态：按下、滚轮、拖拽
    /// 携带的坐标是"事件发生点"，用它点亮高亮会让鼠标没动时滚动条无故变宽。判定范围就是
    /// 渲染层上一帧发布的 [`App::scrollbar_area`] 与 [`App::context_chip_area`]，因此
    /// 悬停区与显示区始终是同一块几何。
    ///
    /// 拖拽期间终端上报的是 `Drag` 而不是 `Moved`，于是按住滑块横向甩出时高亮会自然保留，
    /// 直到松手后的下一次移动才重新判定。
    pub(crate) fn handle_mouse_event(&mut self, mouse: &MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Moved) {
            return false;
        }
        let position = Position::new(mouse.column, mouse.row);
        let scrollbar_hovered = self
            .scrollbar_area
            .is_some_and(|area| area.contains(position));
        let chip_hovered = self
            .context_chip_area
            .is_some_and(|area| area.contains(position));
        let previous = (self.is_scrollbar_hovered, self.is_context_chip_hovered);
        self.is_scrollbar_hovered = scrollbar_hovered;
        self.is_context_chip_hovered = chip_hovered;
        previous != (scrollbar_hovered, chip_hovered)
    }

    /// 处理滚动条上的按下与拖拽，返回事件是否已被热区消费。
    ///
    /// 必须排在文本拖选之前：滚动条只占右侧一列，若不先截获，按下会被当作选择起点，
    /// 拖动则会变成选区而永远滚不动。拖到最底部时恢复自动跟随，与键盘滚动语义一致。
    ///
    /// 按在滑块内时只记录抓取点、不改变滚动位置；按在轨道空白处才把滑块顶端对齐到
    /// 指针。拖动阶段按指针位移平移滑块，抓住滑块的任何位置都不会在按下瞬间跳动。
    pub(crate) fn handle_scrollbar_drag(&mut self, mouse: &MouseEvent) -> bool {
        use crossterm::event::MouseButton;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((area, thumb)) = self.scrollbar_geometry() else {
                    return false;
                };
                if !area.contains(Position::new(mouse.column, mouse.row)) {
                    return false;
                }
                self.dismiss_text_selection();
                self.tool_click_candidate = None;
                self.scrollbar_drag = true;
                let first = area.y.saturating_add(thumb.first_row());
                let last = area.y.saturating_add(thumb.last_row());
                if (first..=last).contains(&mouse.row) {
                    // 抓住滑块：保留指针相对滑块顶端的半行偏移，按下时刻不滚动。
                    let grab =
                        2 * i32::from(mouse.row.saturating_sub(area.y)) - i32::from(thumb.top_half);
                    self.scrollbar_grab_half = Some(grab);
                } else {
                    // 点击轨道空白：滑块顶端对齐指针，随后从零偏移开始拖动。
                    self.scrollbar_grab_half = Some(0);
                    self.scrub_to_top_half(2 * i32::from(mouse.row.saturating_sub(area.y)));
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.scrollbar_drag {
                    return false;
                }
                let Some(area) = self.scrollbar_area else {
                    return false;
                };
                let grab = self.scrollbar_grab_half.unwrap_or(0);
                let top_half = 2 * i32::from(mouse.row.saturating_sub(area.y)) - grab;
                self.scrub_to_top_half(top_half);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.scrollbar_grab_half = None;
                std::mem::take(&mut self.scrollbar_drag)
            }
            _ => false,
        }
    }

    /// 计算主滚动条当前帧的轨道与滑块几何。
    ///
    /// 渲染层用 `scrollbar_thumb` 画滑块；输入侧用同一函数和相同的滚动几何重算一次，
    /// 不额外保存屏幕坐标，拖动与显示就不会出现两套判断。内容未溢出或轨道尚未发布时
    /// 返回 `None`。
    fn scrollbar_geometry(&self) -> Option<(Rect, ScrollbarThumb)> {
        let area = self.scrollbar_area?;
        let viewport = self.last_viewport.max(1);
        let total = self.last_max_scroll.saturating_add(viewport);
        if area.height == 0 || total <= viewport {
            return None;
        }
        let scroll = self.scroll.unwrap_or(self.last_max_scroll);
        let thumb = scrollbar_thumb(area.height, total, viewport, scroll);
        Some((area, thumb))
    }

    /// 把滑块顶端的半行位置映射为滚动偏移；越界时按轨道端点处理。
    ///
    /// 可动区间取「轨道半行数 - 滑块半行高」，与渲染完全一致；因此拖到轨道首行或末行
    /// 时滑块顶端正好贴住端点，不会被滑块自身高度截短而"够不到"底边。
    fn scrub_to_top_half(&mut self, top_half: i32) {
        let Some((area, thumb)) = self.scrollbar_geometry() else {
            return;
        };
        let span = u32::from(area.height)
            .saturating_mul(2)
            .saturating_sub(u32::from(thumb.height_half));
        if span == 0 || self.last_max_scroll == 0 {
            return;
        }
        let top = u32::try_from(top_half.max(0)).unwrap_or(u32::MAX).min(span);
        let scaled = top.saturating_mul(u32::from(self.last_max_scroll));
        let position = u16::try_from((scaled + span / 2) / span).unwrap_or(u16::MAX);
        if top >= span || position >= self.last_max_scroll {
            self.scroll = None;
        } else {
            self.scroll = Some(position);
        }
    }

    /// 向上滚动 n 行，进入手动滚动模式。
    ///
    /// 不取消文本选区：选区以内容行号为基准，滚动只改变视口偏移，高亮仍贴在原来的
    /// 文字上。这正是"滚动着选中一屏以上内容再复制"的前提。
    pub(crate) fn scroll_up(&mut self, n: u16) {
        self.tool_click_candidate = None;
        let current = self.scroll.unwrap_or(self.last_max_scroll);
        self.scroll = Some(current.saturating_sub(n));
    }

    /// 向下滚动 n 行；到达底部时恢复自动跟随。
    ///
    /// 与 [`Self::scroll_up`] 相同，保留现有选区。
    pub(crate) fn scroll_down(&mut self, n: u16) {
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

    /// 按符号滚动若干行：正数向上（查看更早内容），负数向下。
    ///
    /// 拖动自动滚动用这个入口，逻辑与滚轮、键盘完全一致，避免出现"拖到边界时的滚动"
    /// 和"滚轮滚动"两套行为。
    fn scroll_selection_view(&mut self, delta: i32) {
        match delta {
            delta if delta > 0 => self.scroll_up(u16::try_from(delta).unwrap_or(u16::MAX)),
            delta if delta < 0 => {
                self.scroll_down(delta.unsigned_abs().try_into().unwrap_or(u16::MAX))
            }
            _ => {}
        }
    }

    /// 在当前光标处插入文本，并退出历史回溯态、同步附件状态。
    fn insert_input_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.finish_input_edit();
    }

    /// 完成一次输入内容修改后的统一清理。
    ///
    /// 供命令路由在改写输入后复用（例如把路径输入转成附件标签），
    /// 保证历史游标、Esc 计时与附件回收与普通编辑一致。
    pub(crate) fn finish_input_edit(&mut self) {
        self.input_history_cursor = None;
        self.last_escape_at = None;
        self.prune_attachments();
        self.prune_refs();
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
        self.prune_refs();
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

    /// 处理终端粘贴：剪贴板持有文件引用时转为附件，粘贴的是单个本地路径时保留路径并附加
    /// 该文件，长文本折叠为引用标签，短文本直接落进输入框。
    pub(crate) fn handle_paste(&mut self, pasted: &str) {
        self.handle_paste_with(pasted, clipboard_files);
    }

    /// [`App::handle_paste`] 的可测试实现。
    ///
    /// `clipboard` 注入剪贴板文件引用的读取方式：文件管理器复制文件时剪贴板的文本
    /// 表示只有文件名，必须回查文件引用才能定位；测试传入固定结果，避免依赖本机
    /// 剪贴板状态。只有纯文件名形态才会调用它——路径与长文本一定不是文件管理器给出
    /// 的名字表示，不值得为它们等待一个系统进程。
    ///
    /// 三种手势区别对待：
    ///
    /// - **复制文件后粘贴**：剪贴板文本与文件引用一一对应，任何类型都按附件发送；
    /// - **粘贴单条或多条路径**：能附加的路径只留引用标签，避免输入框同时出现
    ///   路径与标签；无法附加的路径（目录、超限文本、二进制等）保留路径文本，
    ///   同时说明原因；
    /// - **粘贴普通文本**：短文本内联，长到会撑坏输入盒时折叠成标签。
    pub(crate) fn handle_paste_with(
        &mut self,
        pasted: &str,
        clipboard: impl FnOnce() -> Vec<std::path::PathBuf>,
    ) {
        // 剪贴板只有文件引用、没有文本表示时，终端可能送来一次空粘贴；此时唯一可用的
        // 线索就是文件引用本身。
        if pasted.is_empty() {
            let files = clipboard();
            if files.is_empty() {
                return;
            }
            self.attach_pasted_paths(&files);
            return;
        }
        if may_be_clipboard_file_names(pasted) {
            let files = clipboard();
            if is_clipboard_file_names(pasted, &files) {
                // 文件管理器多选复制：按剪贴板顺序逐个附加，标签顺序与用户的选择一致。
                self.attach_pasted_paths(&files);
                return;
            }
        }
        // 统一平台换行，制表符仍降级为空格以避免不同终端产生不可见差异。
        let text = pasted
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', " ");
        if text.is_empty() {
            return;
        }
        // 单条路径：保留路径文本，再把可消费的文件内容附加成附件。路径文字不改写成
        // 标签——同一段文字可能是一条待执行的命令，也可能是模型定位文件所需的线索。
        // 终端拖入文件有时给的是 `file://` URI 列表：它不是可读的路径文本，直接换成
        // 附件标签，用户看到的就是"这些文件已附加"。
        if let Some(paths) = pasted_file_uris(&text) {
            self.attach_pasted_paths(&paths);
            return;
        }
        // 纯路径粘贴：能附加时输入框只留引用标签，不重复保留路径文本；无法附加
        // （目录、超限文本、二进制等）才插入原路径并说明原因，供 Agent 用工具访问。
        if let Some(path) = pasted_path(&text, &self.workspace.cwd) {
            if let Some(reason) = self.attach_pasted_path(&path) {
                self.insert_pasted_path_text(text.trim_end());
                self.messages.push(Msg::new(
                    MsgKind::Info,
                    format!("已保留路径，未作为附件发送：{}（{reason}）", path.display()),
                ));
            }
            return;
        }
        // 一次粘贴多条路径：文件管理器多选「拷贝为路径名」按行给出路径，终端拖入多个
        // 文件则用空白分隔并转义空格。附加成功的只留标签，无法附加的保留路径文本。
        if let Some(paths) = pasted_paths(&text, &self.workspace.cwd) {
            self.attach_pasted_paths(&paths);
            return;
        }
        // 明确指向本地路径却解析不到文件时给出原因：否则用户只会看到路径变成普通
        // 文本，无法判断是文件不存在还是附件识别没有生效。
        if looks_like_single_pasted_path(&text) {
            self.insert_pasted_path_text(text.trim_end());
            self.messages.push(Msg::new(
                MsgKind::Info,
                format!("未找到本地文件，已按文本插入：{}", text.trim()),
            ));
            return;
        }
        // 短粘贴直接落进输入框：用户能看清原文、能像打字一样继续改。行尾换行去掉——从
        // 终端或编辑器复制一行常带结尾换行，留着会把光标推到下一行空行。
        if !paste_needs_label(&text) {
            let inline = text.trim_end_matches('\n');
            if !inline.is_empty() {
                self.insert_input_text(inline);
            }
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
            source_path: None,
        });
        let clip = format!("{label} ");
        self.input.insert_str(self.cursor, &clip);
        self.cursor += clip.len();
        self.finish_input_edit();
    }

    /// 把一段路径文本插入光标处，必要时与前后文字隔开一列空格。
    ///
    /// 路径是模型定位文件的唯一线索，粘在相邻词上会变成另一个路径（`foo/Users/...`）。
    fn insert_pasted_path_text(&mut self, text: &str) {
        if self.needs_separating_space() {
            self.insert_input_text(" ");
        }
        self.insert_input_text(text);
    }

    /// 判断在光标处插入内容前是否需要补一列空格。
    fn needs_separating_space(&self) -> bool {
        !self.input.is_empty()
            && !self
                .input
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
    }

    /// 逐个附加粘贴来源给出的路径，并把跳过原因合并成一条说明。
    ///
    /// 附加成功的路径只留引用标签；无法附加的路径插入原路径文本（必要时加引号），
    /// 让 Agent 仍能用 `read_file` / `list_directory` 按路径访问。
    fn attach_pasted_paths(&mut self, files: &[std::path::PathBuf]) {
        let mut skipped = Vec::new();
        for path in files {
            if let Some(reason) = self.attach_pasted_path(path) {
                self.insert_pasted_path_text(&pasted_path_display(path));
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                skipped.push(format!("{name}（{reason}）"));
            }
        }
        if !skipped.is_empty() {
            self.messages.push(Msg::new(
                MsgKind::Info,
                format!("以下文件未作为附件发送：{}", skipped.join("、")),
            ));
        }
    }

    /// 按内容类型决定一个粘贴路径能否变成附件，返回跳过原因。
    ///
    /// 只有模型真正能消费的内容才自动附加：图片与 PDF 走原生二进制输入，文本按内容
    /// 内联（受 [`MAX_AUTO_TEXT_ATTACHMENT_BYTES`] 约束）。目录、音视频与其他二进制
    /// 附加后模型只会看到一条"内容已省略"的占位说明，因此保留路径交给工具处理。
    fn attach_pasted_path(&mut self, path: &Path) -> Option<String> {
        let kind = match pasted_path_kind(path) {
            Ok(kind) => kind,
            Err(error) => return Some(format!("无法读取：{error}")),
        };
        let too_large = std::fs::metadata(path)
            .map(|metadata| metadata.len() > MAX_AUTO_TEXT_ATTACHMENT_BYTES)
            .unwrap_or(false);
        let reason = match kind {
            PastedPathKind::Media => None,
            PastedPathKind::Text if too_large => Some(format!(
                "超过 {} KiB 的文本会占满上下文；工作区内的文件可让 Agent 用 read_file 分段读取",
                MAX_AUTO_TEXT_ATTACHMENT_BYTES / 1024
            )),
            PastedPathKind::Text => None,
            PastedPathKind::AudioVideo => Some("当前模型协议尚未支持音频/视频附件".to_string()),
            PastedPathKind::Binary => Some("二进制文件无法内联给模型".to_string()),
            PastedPathKind::Directory => Some(
                "目录不能作为附件；工作区内的目录可让 Agent 用 list_directory 查看".to_string(),
            ),
        };
        if let Some(reason) = reason {
            return Some(reason);
        }
        // 标签前补一列空格：路径粘贴后紧跟 `[FILE#1]` 会读成路径本身的一部分。空输入
        // 不需要前导空格，否则标签会缩进一列。
        let before = self.input.len();
        if self.needs_separating_space() {
            self.insert_input_text(" ");
        }
        match self.try_attach_file(path) {
            Ok(()) => None,
            Err(message) => {
                // 附加失败时不留下多余空格，输入保持粘贴前的样子。
                self.input.truncate(before);
                self.cursor = self.cursor.min(self.input.len());
                self.finish_input_edit();
                Some(message)
            }
        }
    }

    /// 读取文件并加入待发送附件，同时在光标处插入引用标签。
    ///
    /// 读取失败或超过 [`MAX_ATTACHMENT_BYTES`] 时追加错误消息，不修改输入。
    pub(crate) fn attach_file(&mut self, path: &Path) {
        if let Err(message) = self.try_attach_file(path) {
            self.messages.push(Msg::new(MsgKind::Error, message));
        }
    }

    /// 尝试附加一个文件，失败时把原因交回调用方决定如何展示。
    ///
    /// 只有成功后才改动输入与附件列表；失败时编辑器保持原样，调用方可以选择把路径
    /// 文本留在输入框里继续发送。
    fn try_attach_file(&mut self, path: &Path) -> std::result::Result<(), String> {
        let attachment = self.load_attachment(path)?;
        let clip = format!("{} ", attachment.label);
        self.attachments.push(attachment);
        self.input.insert_str(self.cursor, &clip);
        self.cursor += clip.len();
        self.finish_input_edit();
        Ok(())
    }

    /// 把正文里点名的本地路径登记为只读授权，并按需附加内容。
    ///
    /// 用户在消息里写「看这张图 /Users/me/a.png」时，模型只拿到一段路径文本：图片
    /// 内容永远进不了上下文，模型只能退回 OCR 或猜测。[`mentioned_paths`] 找出真实
    /// 存在的文件与目录后在这里处理：
    ///
    /// - 图片与 PDF 直接读成附件，出现在句子任意位置都附加；
    /// - 文本文件只在路径独立成行时内联，并与其他自动附加的文本共享
    ///   [`MAX_AUTO_TEXT_ATTACHMENT_BYTES`] 预算——正文里随手提到的源码每轮都塞进
    ///   上下文代价太高；
    /// - 其余路径（大文本、目录、二进制、音视频）登记为只读授权，让 Agent 用
    ///   `read_file` / `list_directory` 按需访问，工作区外文件因此也能被工具读取。
    ///
    /// 路径文本保留原样，不做原位替换：同一段文字可能是一条待执行的命令（
    /// `cp a.png b.png`），把路径改写成引用标签会损坏用户的消息。附加结果以一条
    /// 提示告知用户；单个文件读取失败或当前模型不支持该模态时只提示，消息照常发送。
    pub(crate) fn attach_mentioned_paths(&mut self) {
        let base = self.workspace.cwd.clone();
        let mut attached: Vec<String> = Vec::new();
        let mut text_budget = MAX_AUTO_TEXT_ATTACHMENT_BYTES;
        for (span, path) in mentioned_paths(&self.input, &base) {
            // 用户点名的路径一律登记只读授权；工作区内的路径由守卫天然允许。
            let _ = self.read_path_grants.grant(&path);
            if self
                .attachments
                .iter()
                .any(|attachment| attachment.source_path.as_deref() == Some(path.as_path()))
            {
                continue;
            }
            let kind = match pasted_path_kind(&path) {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            let size = std::fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let text_attachable = matches!(kind, PastedPathKind::Text)
                && size <= text_budget
                && path_is_standalone(&self.input, &span);
            if !matches!(kind, PastedPathKind::Media) && !text_attachable {
                continue;
            }
            match self.load_attachment(&path) {
                Ok(attachment) => {
                    if text_attachable {
                        text_budget = text_budget.saturating_sub(size);
                    }
                    attached.push(format!("{} {}", attachment.label, attachment.name));
                    self.attachments.push(attachment);
                }
                Err(message) => self.messages.push(Msg::new(MsgKind::Error, message)),
            }
        }
        if !attached.is_empty() {
            self.messages.push(Msg::new(
                MsgKind::Info,
                format!("正文中的本地文件已附带发送：{}", attached.join("、")),
            ));
        }
    }

    /// 读取并校验一个本地文件，构造待发送附件。
    ///
    /// 错误以面向用户的文案返回：`/attach` 会把它当作命令失败展示，正文自动识别
    /// 则只提示后继续发送。标签编号基于当前附件列表，因此调用期间不得并发改动。
    fn load_attachment(&self, path: &Path) -> std::result::Result<PendingAttachment, String> {
        use base64::Engine;

        let bytes = std::fs::read(path)
            .map_err(|error| format!("Failed to read attachment ({}): {error}", path.display()))?;
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "Attachment exceeds the {} MiB limit: {}",
                MAX_ATTACHMENT_BYTES / 1024 / 1024,
                path.display()
            ));
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        let (media_type, is_image) = attachment_media_type(path, &bytes);
        let modality = attachment_modality(&media_type);
        self.validate_attachment_modality(&modality)?;
        Ok(PendingAttachment {
            label: self.next_attachment_label(&modality),
            name,
            media_type,
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            is_image,
            pasted_text: None,
            source_path: Some(path.to_path_buf()),
        })
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

    /// 丢弃引用标签已不在输入文本中的上下文引用。
    ///
    /// 用户删掉标签就是在说「这段上下文不要带」，因此记入丢弃集合：轮询即使再次
    /// 看到同一段选区，也不会把标签补回去。只有选区变化产生新 ID 才会重新出现。
    pub(crate) fn prune_refs(&mut self) {
        if self.refs.is_empty() {
            return;
        }
        let input = &self.input;
        let (kept, dropped): (Vec<_>, Vec<_>) = self
            .refs
            .iter()
            .cloned()
            .partition(|entry| input.contains(&entry.label));
        self.refs = kept;
        for entry in dropped {
            self.dismissed_refs.insert(entry.id);
        }
        // 丢弃集合只用于判重，不需要无限增长；超出后整体清空，最多让已删除的旧标签
        // 在选区未变时再出现一次，代价远低于长期增长。
        if self.dismissed_refs.len() > 64 {
            self.dismissed_refs.clear();
        }
    }

    /// 光标紧跟附件或引用标签时整体删除标签及对应条目，返回是否命中。
    pub(crate) fn remove_attachment_before_cursor(&mut self) -> bool {
        let prefix = &self.input[..self.cursor];
        if let Some(index) = self
            .attachments
            .iter()
            .position(|a| prefix.ends_with(&a.label))
        {
            let start = self.cursor - self.attachments[index].label.len();
            self.input.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.attachments.remove(index);
            return true;
        }
        let Some(index) = self.refs.iter().position(|r| prefix.ends_with(&r.label)) else {
            return false;
        };
        let start = self.cursor - self.refs[index].label.len();
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
        let removed = self.refs.remove(index);
        self.dismissed_refs.insert(removed.id);
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
        // 挂起处理器靠这份状态决定是否发送禁用序列，切换后必须同步。
        crate::terminal::set_mouse_capture(self.mouse_capture);
        // 关掉捕获后不会再有指针移动事件，悬停高亮必须立刻收掉，否则会一直亮着。
        self.is_scrollbar_hovered = false;
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
    ///
    /// 选区以内容行号为基准，因此：
    /// - 拖动越过内容区上下边界时自动滚动，可以选中一屏以上的内容；
    /// - 滚轮滚动不会取消选区，高亮继续贴在原来的文字上。
    pub(crate) fn handle_text_selection(&mut self, mouse: &MouseEvent, buffer: &Buffer) {
        let Some(area) = self.content_area.filter(|area| !area.is_empty()) else {
            // 本帧没有可选择的正文区域（例如审批覆盖层），不建立选区。
            self.dismiss_text_selection();
            return;
        };
        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.dismiss_text_selection();
                self.text_selection =
                    Some(MouseTextSelection::new(self.content_position(area, mouse)));
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                self.text_selection_content = None;
                if self.text_selection.is_none() {
                    return;
                }
                // 先滚动再取坐标：滚动量改变后，同一个鼠标行对应不同的内容行，
                // 顺序反了选区就会落后一帧、拖不到屏幕外。
                self.autoscroll_selection(mouse.row, area);
                let position = self.content_position(area, mouse);
                if let Some(selection) = self.text_selection.as_mut() {
                    selection.update(position);
                }
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                let Some(mut selection) = self.text_selection else {
                    return;
                };
                selection.update(self.content_position(area, mouse));
                let text = if selection.dragged {
                    self.extract_selection_text(selection, buffer)
                } else {
                    String::new()
                };
                self.text_selection = Some(selection);
                self.text_selection_content = (!text.is_empty()).then_some(text);
            }
            _ => {}
        }
    }

    /// 把鼠标屏幕坐标换算成选区使用的**内容坐标**。
    ///
    /// 横向夹取在内容区的左右边界内（内容不越出该区域），纵向用未裁剪的行号，
    /// 这样拖到屏幕外仍能表达"继续向外扩展"的意图，再由调用方滚动把它变成可见。
    ///
    /// 滚动偏移取**当前**值而不是上一帧渲染时记下的 `content_scroll`：拖动自动滚动会
    /// 在两次绘制之间改变偏移，用旧值会让选区永远落后一步、拖不出屏幕。
    fn content_position(&self, area: Rect, mouse: &MouseEvent) -> Position {
        let column = mouse
            .column
            .clamp(area.left(), area.right().saturating_sub(1));
        let scroll = i32::from(self.scroll.unwrap_or(self.last_max_scroll));
        let row = i32::from(mouse.row) - i32::from(area.y) + scroll;
        // 首屏没有消息内容（`content_rows == 0`）：此时可选范围就是消息区本身，
        // 按内容行数夹取会把整块区域压成一行。
        let last_row = if self.content_rows == 0 {
            i32::from(area.height.saturating_sub(1))
        } else {
            i32::from(self.content_rows.saturating_sub(1))
        };
        let row = row.clamp(0, last_row.max(0));
        Position::new(column, u16::try_from(row).unwrap_or(0))
    }

    /// 拖动越过内容区上下边界时滚动视图，让选区可以延伸到一屏之外。
    ///
    /// 每次拖动事件按越界距离滚动若干行：拖得越远滚得越快，靠近边界时保持精细控制。
    /// 终端在鼠标静止时不产生拖动事件，因此"按住不动"不会持续滚动；这是明确的取舍，
    /// 换来的是没有额外的定时器与重绘。
    fn autoscroll_selection(&mut self, row: u16, area: Rect) {
        let step = if row < area.top() {
            i32::from(area.top() - row).min(AUTOSCROLL_MAX_STEP)
        } else if row >= area.bottom() {
            -i32::from(row - area.bottom() + 1).max(-AUTOSCROLL_MAX_STEP)
        } else {
            return;
        };
        self.scroll_selection_view(step);
    }

    /// 提取选区覆盖的文本，支持跨越一屏以上的范围。
    ///
    /// 首屏（没有消息内容）没有滚动，选区行号相对内容区顶部，映射回屏幕坐标后直接从
    /// 当前帧缓冲区取；正常运行中用离屏缓冲区重新渲染选中的行范围，因此滚出屏幕的
    /// 历史同样能被复制。
    fn extract_selection_text(&mut self, selection: MouseTextSelection, buffer: &Buffer) -> String {
        if self.content_rows == 0 {
            let offset = self.content_area.map_or(0, |area| area.y);
            let translated = MouseTextSelection {
                anchor: Position::new(
                    selection.anchor.x,
                    selection.anchor.y.saturating_add(offset),
                ),
                current: Position::new(
                    selection.current.x,
                    selection.current.y.saturating_add(offset),
                ),
                dragged: selection.dragged,
            };
            return translated.extract(buffer);
        }
        let Some(area) = self.content_area else {
            return String::new();
        };
        let (start, end) = selection.row_span();
        let end = end.min(self.content_rows.saturating_sub(1));
        let height = end.saturating_sub(start).saturating_add(1);
        if height == 0 {
            return String::new();
        }
        let (messages, streaming) = crate::app::timeline_messages(
            #[cfg(feature = "plugins")]
            &self.main_event_snapshot,
            &self.messages,
            self.streaming_message,
        );
        let rendered = self
            .transcript_layout
            .render_range(area, messages, streaming, start, height);
        selection.extract(&rendered)
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
    ///
    /// 选区存的是内容行号，这里换算成屏幕行：滚动之后高亮仍贴在原来的文字上，
    /// 滚出屏幕的部分自然不绘制。
    pub(crate) fn render_text_selection(&self, buffer: &mut Buffer) {
        let Some(selection) = self.text_selection.filter(|selection| selection.dragged) else {
            return;
        };
        let Some(area) = self.content_area else {
            return;
        };
        if area.is_empty() {
            return;
        }
        // 内容行 0 对应屏幕上的 area.y - content_scroll：滚动越大，内容越靠上。
        let content_bounds = Rect::new(area.x, 0, area.width, self.content_rows.max(area.height));
        let scroll = i32::from(self.content_scroll);
        selection.for_each_position(content_bounds, |position| {
            let screen_y = i32::from(position.y) - scroll + i32::from(area.y);
            let Ok(screen_y) = u16::try_from(screen_y) else {
                return;
            };
            if screen_y < area.top() || screen_y >= area.bottom() {
                return;
            }
            if let Some(cell) = buffer.cell_mut(Position::new(position.x, screen_y)) {
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
        self.refs.clear();
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
        // 正文里点名了本地图片/PDF 时先把它读成附件：模型拿不到图像内容就只能
        // 退化成 OCR 或猜测，而用户写下路径的本意就是让模型看到那个文件。
        self.attach_mentioned_paths();
        // 标签被删掉的引用视为用户主动丢弃，不能因为条目还在列表里就照旧发送。
        let refs = retained_refs(&self.input, &self.refs);
        let visible = submission_text(&self.input, &self.attachments, &refs);
        if !visible.is_empty() {
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
        self.refs.clear();
        self.cursor = 0;
        self.last_escape_at = None;
        if text.is_empty() && attachments.is_empty() && refs.is_empty() {
            return None;
        }
        self.remember_input(&text);
        Some(UserSubmission {
            text,
            attachments,
            refs,
        })
    }
}

/// 把无法附加的路径还原成可再次解析的输入文本。
///
/// 含空白的路径用引号包住，确保发送时的正文扫描仍把它当成同一条路径；优先单引号，
/// 路径本身含单引号时改用双引号，两者都含时退回空格转义。
fn pasted_path_display(path: &Path) -> String {
    let text = path.display().to_string();
    if !text.chars().any(char::is_whitespace) {
        return text;
    }
    if !text.contains('\'') {
        format!("'{text}'")
    } else if !text.contains('"') {
        format!("\"{text}\"")
    } else {
        text.replace(' ', "\\ ")
    }
}

/// 判断路径片段是否独占一行（或整条输入）。
///
/// 只有独立成行的路径才自动内联文本内容：嵌在句子或命令里的路径可能只是参数或
/// 随手提及，把整份文件附加进去会让后续每一轮请求都背上它的体积。引号、句读与
/// 不成对的括号属于句子结构，不影响"独占一行"的判定。
fn path_is_standalone(input: &str, span: &std::ops::Range<usize>) -> bool {
    let line_start = input[..span.start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = input[span.end..]
        .find('\n')
        .map_or(input.len(), |index| span.end + index);
    input[line_start..span.start]
        .trim_matches(crate::session::attachment::is_path_boundary_char)
        .is_empty()
        && input[span.end..line_end]
            .trim_matches(crate::session::attachment::is_path_boundary_char)
            .is_empty()
}

/// 判断一段粘贴文本是否长到应该折叠成引用标签。
///
/// 判据是"会不会把输入盒撑坏"：多行必然增加逻辑行数；单行按显示宽度算，宽字符占两列。
/// 短内容返回 `false`，由调用方按普通输入插入。
fn paste_needs_label(text: &str) -> bool {
    text.lines().count() > 1 || unicode_width::UnicodeWidthStr::width(text) > PASTE_TAG_COLUMNS
}

/// 判断字符是否为按词移动与删除使用的分隔符。
fn is_word_separator(ch: char) -> bool {
    ch.is_whitespace() || ch.is_ascii_punctuation()
}

/// 返回光标左侧一个词的起始字节位置。
pub(super) fn previous_word_boundary(input: &str, cursor: usize) -> usize {
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
pub(super) fn move_cursor_vertically(input: &str, cursor: &mut usize, upward: bool) -> bool {
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
