//! 一次用户提交的排队、运行、流式增量与完成处理。

use super::App;
use crate::app::reasoning_level_label;
use crate::config::persist_reasoning_level;
use crate::event::{AgentCompletion, SessionReloadOutcome, UiEvent};
use crate::session::conversation::{Msg, MsgKind, UserSubmission};
use crate::session::{restore_session_messages, run_and_persist};
use crate::theme::{
    REVEAL_BACKLOG_LIMIT, REVEAL_DRAIN_MS, REVEAL_FRAME_INTERVAL_MS, REVEAL_MAX_CHARS_PER_SEC,
    REVEAL_MIN_CHARS_PER_SEC,
};
use agent_core::model::ReasoningLevel;
use agent_core::Agent;
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 打字机节拍器：把"网络到达节奏"换算成"每帧吐字量"。
///
/// 关键在于结算口径是**真实流逝时间**，而不是"被调用了几次"。事件循环在收到网络事件后
/// 会立刻再走一轮揭示，如果按次数结算，吐字速度就重新等于到达速度（一次突发 = 一次
/// 整段上屏），这正是"喷射感"的来源。这里改成按墙钟累计"字符配额"，不足一个字符的
/// 余量留到下一帧，于是屏幕节奏只由帧时钟决定。
#[derive(Default)]
pub(crate) struct RevealPacer {
    /// 上一次结算配额的墙钟时间点；`None` 表示缓冲刚被清空，下一次结算只给一帧配额。
    last_settle: Option<Instant>,
    /// 尚未满一个字符的剩余配额，单位为千分之一字符。
    credit_milli: u32,
}

impl RevealPacer {
    /// 按当前积压量结算本帧应揭示的字符数。
    ///
    /// `backlog_chars` 是缓冲中待显示的字符数，`now` 是本次揭示的墙钟时间点。
    /// 返回值不会超过 `backlog_chars`：不会揭示不存在的文本。
    ///
    /// 停滞过久（审批弹窗、插件对话框阻塞事件循环）后不补偿欠账：一次补上几十帧的配额
    /// 会让屏幕整块蹦字，宁可让那一瞬间丢掉的节拍白丢。
    fn settle(&mut self, backlog_chars: usize, now: Instant) -> usize {
        if backlog_chars == 0 {
            self.reset();
            return 0;
        }
        let elapsed = match self.last_settle {
            Some(previous) => now.saturating_duration_since(previous),
            // 首次结算只看作一帧：运行开始前的等待时间不属于这段文本。
            None => Duration::from_millis(REVEAL_FRAME_INTERVAL_MS),
        };
        self.last_settle = Some(now);
        let elapsed = elapsed.min(Duration::from_millis(REVEAL_FRAME_INTERVAL_MS * 3));
        // 速率（字符/秒）× 毫秒 = 千分之一字符，全程整数运算，没有逐帧取整误差。
        let gain = reveal_rate_chars_per_sec(backlog_chars) as u64 * elapsed.as_millis() as u64;
        self.credit_milli = self
            .credit_milli
            .saturating_add(u32::try_from(gain).unwrap_or(u32::MAX));
        let quota = usize::try_from(self.credit_milli / 1000).unwrap_or(usize::MAX);
        self.credit_milli %= 1000;
        quota.min(backlog_chars)
    }

    /// 按共享节拍取出一帧文本，超限积压先按 UTF-8 边界释放；主/子会话各自持有缓冲。
    pub(super) fn take_chunk(&mut self, pending: &mut String, now: Instant) -> String {
        if pending.is_empty() {
            self.reset();
            return String::new();
        }
        let mut forced = pending.len().saturating_sub(REVEAL_BACKLOG_LIMIT);
        while !pending.is_char_boundary(forced) {
            forced -= 1;
        }
        let remaining = &pending[forced..];
        let quota = self.settle(remaining.chars().count(), now);
        let paced = remaining
            .char_indices()
            .nth(quota)
            .map_or(remaining.len(), |(index, _)| index);
        let chunk = pending.drain(..forced + paced).collect();
        if pending.is_empty() {
            self.reset();
        }
        chunk
    }

    /// 清空节拍状态：缓冲排空或越过一次显式补完的边界后调用。
    pub(super) fn reset(&mut self) {
        self.last_settle = None;
        self.credit_milli = 0;
    }
}

/// 按当前积压量决定揭示速率（字符/秒）。
///
/// 取"在 [`REVEAL_DRAIN_MS`] 内排空积压"的反推值：积压越多吐得越快，积压清空则回落到
/// 最低速率。稳态下积压恒为 `到达速率 × REVEAL_DRAIN_MS`，因此每帧吐出的字符数接近
/// 常数——这就是"匀速打字机"的来源；网络突发只会让它短暂加速，不会让它一步跳完整段。
fn reveal_rate_chars_per_sec(backlog_chars: usize) -> usize {
    let drain_rate = backlog_chars.saturating_mul(1000) / REVEAL_DRAIN_MS as usize;
    drain_rate.clamp(REVEAL_MIN_CHARS_PER_SEC, REVEAL_MAX_CHARS_PER_SEC)
}

impl App {
    /// 处理后台会话上下文重载结果：替换会话时保留正在编辑的输入。
    ///
    /// 处理说明由原生加载器发布的展示事件提供，这里只负责会话状态切换。
    pub(crate) fn handle_session_context_reloaded(&mut self, result: Result<SessionReloadOutcome>) {
        self.pending_reload = None;
        match result {
            Ok(SessionReloadOutcome::Replaced(saved)) => {
                // 重载在后台完成，用户可能仍在编辑输入，替换会话后原样恢复。
                let input = std::mem::take(&mut self.input);
                let cursor = self.cursor;
                let attachments = std::mem::take(&mut self.attachments);
                let refs = std::mem::take(&mut self.refs);
                self.replace_session(saved, None);
                self.input = input;
                self.cursor = cursor;
                self.attachments = attachments;
                self.refs = refs;
            }
            Ok(SessionReloadOutcome::Unchanged) => {}
            Err(error) => self.messages.push(Msg::new(
                MsgKind::Error,
                format!("重新加载会话上下文失败：{error}"),
            )),
        }
    }

    /// 循环切换 models.dev 声明的推理等级，并更新共享 Agent 的后续请求参数。
    pub(crate) fn cycle_reasoning_level(&mut self, agent: Option<&Arc<Agent>>) {
        let Some(agent) = agent else {
            self.messages
                .push(Msg::new(MsgKind::Info, "模型尚未就绪，无法切换思考等级"));
            return;
        };
        if self.reasoning_levels.len() <= 1 {
            self.messages
                .push(Msg::new(MsgKind::Info, "当前模型没有可切换的思考等级"));
            return;
        }
        let current = self
            .reasoning_levels
            .iter()
            .position(|level| *level == self.reasoning_level)
            .unwrap_or(0);
        let level = self.reasoning_levels[(current + 1) % self.reasoning_levels.len()];
        self.set_reasoning_level(level, Some(agent));
    }

    /// 设置 models.dev 声明支持的推理等级，并更新共享 Agent 的后续请求参数。
    pub(crate) fn set_reasoning_level(
        &mut self,
        level: ReasoningLevel,
        agent: Option<&Arc<Agent>>,
    ) {
        let Some(agent) = agent else {
            self.messages
                .push(Msg::new(MsgKind::Info, "模型尚未就绪，无法切换思考等级"));
            return;
        };
        if !self.reasoning_levels.contains(&level) {
            self.messages.push(Msg::new(
                MsgKind::Error,
                format!("当前模型不支持思考等级 {}", reasoning_level_label(level)),
            ));
            return;
        }
        if let Some(path) = self.config_path.as_deref() {
            if let Err(error) = persist_reasoning_level(path, level) {
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("保存思考等级失败，当前设置未改变：{error:#}"),
                ));
                return;
            }
        }
        self.reasoning_level = level;
        agent.set_reasoning_level(level);
        self.messages.push(Msg::new(
            MsgKind::Info,
            format!("思考等级已切换为 {}", reasoning_level_label(level)),
        ));
    }

    /// Queues one complete input while plugin loading keeps the Agent unavailable.
    ///
    /// 插件加载导致 Agent 尚不可用时，将一条完整输入加入 FIFO 队列。待发送消息只显示
    /// 在输入框上方的排队面板里；开始发送时移出队列并进入正文，首存失败时再回退。
    pub(crate) fn queue_input_until_ready(&mut self) {
        if self.defer_attachment_submission(None, false) {
            return;
        }
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.queued_inputs.push_back(submission);
        self.scroll = None;
    }

    /// 把队列中的一条消息取回输入框继续编辑；返回是否成功。
    ///
    /// 输入框已有草稿时先按正常提交规则把草稿排到队尾，保证两边内容都不丢。
    /// 已开始发送的输入不再位于队列中，因此队列内的每一项都可以取回。
    pub(crate) fn edit_queued_input(&mut self, index: usize) -> bool {
        if index >= self.queued_inputs.len() {
            return false;
        }
        let has_draft =
            !self.input.trim().is_empty() || !self.attachments.is_empty() || !self.refs.is_empty();
        if has_draft {
            let Some(draft) = self.take_submission() else {
                return false;
            };
            // 追加到队尾不影响待取回条目的下标。
            self.queued_inputs.push_back(draft);
        }
        let Some(queued) = self.queued_inputs.get(index).cloned() else {
            return false;
        };
        self.queued_inputs.remove(index);
        // 发送时自动附加的附件（正文里点名的图片等）没有对应的输入标签；取回编辑时
        // 补在正文末尾，否则任何一次编辑都会按"标签已删除"把它回收掉。
        let mut text = queued.text;
        for attachment in &queued.attachments {
            if !text.contains(&attachment.label) {
                text.push(' ');
                text.push_str(&attachment.label);
            }
        }
        for entry in &queued.refs {
            if !text.contains(&entry.label) {
                text.push(' ');
                text.push_str(&entry.label);
            }
        }
        self.input = text;
        self.cursor = self.input.len();
        self.attachments = queued.attachments;
        self.refs = queued.refs;
        self.queue_focus = None;
        self.finish_input_edit();
        self.scroll = None;
        self.messages.push(Msg::new(
            MsgKind::Info,
            "已取回排队消息；修改后按 Enter 重新入队，清空输入即取消",
        ));
        true
    }

    /// 把键盘焦点移入排队面板；队列为空时返回 `false`。
    pub(crate) fn focus_queue(&mut self) -> bool {
        let len = self.queued_inputs.len();
        if len == 0 {
            self.queue_focus = None;
            return false;
        }
        self.queue_focus = Some(self.queue_focus.unwrap_or(0).min(len - 1));
        true
    }

    /// 在排队面板内移动选择；越界时停在队首或队尾。
    pub(crate) fn move_queue_selection(&mut self, delta: isize) {
        let len = self.queued_inputs.len();
        if len == 0 {
            self.queue_focus = None;
            return;
        }
        let current = self.queue_focus.unwrap_or(0).min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        self.queue_focus = Some(next as usize);
    }

    /// 发送排队面板中选中的消息。
    ///
    /// 空闲时立即作为新一轮发送并移出队列，剩余队列在该轮结束后继续 FIFO；运行中
    /// 不打断当前轮次，只把选中项移到待发送队首，等本轮结束后发送。
    pub(crate) fn send_selected_queued(&mut self, agent: Option<&Arc<Agent>>) {
        let Some(index) = self.queue_focus else {
            return;
        };
        let len = self.queued_inputs.len();
        if len == 0 {
            self.queue_focus = None;
            return;
        }
        let index = index.min(len - 1);
        if self.pending_reload.is_some() {
            self.messages
                .push(Msg::new(MsgKind::Info, "上下文压缩仍在进行中，请稍候"));
            return;
        }
        if self.running {
            let Some(submission) = self.queued_inputs.remove(index) else {
                return;
            };
            self.queued_inputs.push_front(submission);
            self.queue_focus = Some(0);
            self.messages
                .push(Msg::new(MsgKind::Info, "已置顶，本轮结束后发送"));
            return;
        }
        let Some(agent) = agent else {
            self.messages
                .push(Msg::new(MsgKind::Info, "模型尚未就绪，排队消息保持不动"));
            return;
        };
        let Some(submission) = self.queued_inputs.remove(index) else {
            return;
        };
        self.queue_focus = if self.queued_inputs.is_empty() {
            None
        } else {
            Some(index.min(self.queued_inputs.len() - 1))
        };
        self.queued_run_active = false;
        self.start_input_run(agent, submission);
    }

    /// 丢弃排队面板中选中的待发送消息，不影响已经出队的当前运行。
    pub(crate) fn remove_selected_queued(&mut self) {
        let Some(index) = self.queue_focus else {
            return;
        };
        let len = self.queued_inputs.len();
        if len == 0 {
            self.queue_focus = None;
            return;
        }
        let index = index.min(len - 1);
        if self.queued_inputs.remove(index).is_some() {
            self.messages
                .push(Msg::new(MsgKind::Info, "已丢弃排队消息"));
        }
        self.queue_focus = if self.queued_inputs.is_empty() {
            None
        } else {
            Some(index.min(self.queued_inputs.len() - 1))
        };
    }

    /// Ctrl+Enter 提交完整输入：取消旧运行并在其持久化收尾后优先启动，不使用延迟 steering。
    pub(crate) fn submit_immediate(&mut self, agent: &Arc<Agent>) {
        if self.input.trim().is_empty()
            && self.attachments.is_empty()
            && self.refs.is_empty()
            && self.attachment_job.is_none()
        {
            return;
        }
        if self.immediate_input.is_some() {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "已有立即发送的消息在等待收尾；当前草稿保持不变",
            ));
            return;
        }
        if self.pending_reload.is_some() {
            self.messages
                .push(Msg::new(MsgKind::Info, "上下文压缩仍在进行中，请稍候"));
            return;
        }
        if self.defer_attachment_submission(Some(Arc::clone(agent)), true) {
            // 接受插队意图时就停当前轮，不能等文件读完再误取消下一条自动派发的任务。
            if self.attachment_job.is_some() && self.running {
                self.interrupt_run(Some(agent));
            }
            return;
        }
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.send_immediate_submission(agent, submission);
    }

    /// 运行中只登记优先输入，绝不在旧 Session 尚未保存时并发启动新 Agent 轮次。
    /// 优先消息实际启动时恢复 FIFO，使其完成后继续处理其余待发送消息。
    fn send_immediate_submission(&mut self, agent: &Arc<Agent>, submission: UserSubmission) {
        if self.running {
            self.interrupt_run(Some(agent));
            self.immediate_input = Some(submission);
            self.messages.push(Msg::new(
                MsgKind::Info,
                "正在停止当前轮次，随后立即发送；Esc 可取消优先发送",
            ));
        } else {
            self.queued_run_active = false;
            self.start_input_run(agent, submission);
        }
    }

    /// 对已排队的选中消息执行 Ctrl+Enter；保留其他条目的顺序和当前编辑器草稿。
    pub(crate) fn send_selected_queued_immediate(&mut self, agent: Option<&Arc<Agent>>) {
        let Some(index) = self
            .queue_focus
            .filter(|index| *index < self.queued_inputs.len())
        else {
            return;
        };
        if self.immediate_input.is_some() || self.pending_reload.is_some() {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "正在等待上一条发送或上下文操作完成；队列保持不变",
            ));
            return;
        }
        let Some(agent) = agent else {
            self.messages
                .push(Msg::new(MsgKind::Info, "模型尚未就绪，排队消息保持不动"));
            return;
        };
        let submission = self.queued_inputs.remove(index).expect("已校验队列下标");
        self.queue_focus = None;
        self.send_immediate_submission(agent, submission);
    }

    /// 显式取消或保存失败时，把尚未发送的优先输入放回待发送队列，不写入上下文。
    pub(super) fn pause_immediate_input(&mut self) {
        if let Some(submission) = self.immediate_input.take() {
            self.queued_inputs.push_front(submission);
            self.queue_focus = self.queue_focus.map(|index| index + 1);
        }
    }

    /// Submits the current editor value immediately to a ready Agent.
    ///
    /// 将当前编辑器内容立即提交给已就绪的 Agent。
    pub(crate) fn submit(&mut self, agent: &Arc<Agent>) {
        if self.defer_attachment_submission(Some(Arc::clone(agent)), false) {
            return;
        }
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.queued_run_active = false;
        self.start_input_run(agent, submission);
    }

    /// 启动一次完整用户提交；手动发送、FIFO 与优先发送统一展示用户气泡并恢复队列。
    pub(crate) fn start_input_run(&mut self, agent: &Arc<Agent>, submission: UserSubmission) {
        self.flush_pending_stream();
        // 插队借用取消路径只是为了等旧轮安全收尾；新提交已获准启动，暂停不能泄漏到其后。
        // 不在完成回调中无条件恢复，避免 Esc 或保存失败后的迟到事件意外重启队列。
        self.queue_paused = false;
        self.active_input_message = Some(self.messages.len());
        self.messages
            .push(Msg::new(MsgKind::User, submission.text.clone()));
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        self.run_cancellation = Some(cancel);
        self.running = true;
        self.run_started_at = Some(std::time::Instant::now());
        self.streaming_message = None;
        self.run_stream_cancelled = false;
        self.model_is_thinking = false;
        self.scroll = None;

        let agent = Arc::clone(agent);
        let tx = self.tx.clone();
        let session_store = Arc::clone(&self.session_store);
        let session_record = self.session_record.clone();
        // 会话 ID 作为服务商侧的提示缓存路由键；每次启动运行时同步，使 /resume、fork
        // 等切换后的下一轮请求立刻改用新会话的键，而不是继续复用上一个会话的缓存分区。
        agent.set_session_id(Some(session_record.id.to_string()));

        tokio::spawn(async move {
            let result = run_and_persist(
                agent.as_ref(),
                session_store.as_ref(),
                session_record,
                submission,
                Some(cancellation),
            )
            .await;
            let _ = tx.send(UiEvent::AgentDone(Box::new(result)));
        });
    }

    /// 旧运行已持久化收尾后先处理显式优先输入，否则按 FIFO 启动下一条。
    pub(crate) fn run_next_queued(&mut self, agent: &Arc<Agent>) {
        if self.running || self.pending_reload.is_some() {
            return;
        }
        if let Some(submission) = self.immediate_input.take() {
            self.queued_run_active = false;
            self.start_input_run(agent, submission);
        } else if !self.queue_paused {
            if let Some(submission) = self.queued_inputs.pop_front() {
                // 开始发送即离开待发送队列；运行持有完整输入，首存失败时由完成事件恢复。
                self.queue_focus = self.queue_focus.and_then(|index| {
                    (!self.queued_inputs.is_empty())
                        .then(|| index.saturating_sub(1).min(self.queued_inputs.len() - 1))
                });
                self.queued_run_active = true;
                self.start_input_run(agent, submission);
            }
        }
    }

    /// 开始一个新的模型响应轮次，后续增量会写入新的助手消息。
    ///
    /// 先把上一段待揭示文本补完：新轮次会把 `streaming_message` 归零，残留缓冲的内容
    /// 会失去归属消息。
    pub(crate) fn start_model_response(&mut self) {
        self.flush_pending_stream();
        self.streaming_message = None;
        self.model_is_thinking = false;
    }

    /// 标记模型进入思考阶段；推理摘要文本不会进入界面、消息或持久会话。
    pub(crate) fn mark_model_thinking(&mut self) {
        if self.accepts_run_stream_output() {
            self.model_is_thinking = true;
        }
    }

    /// 标记模型离开思考阶段，正文生成和工具执行都会调用该状态切换。
    pub(crate) fn finish_model_thinking(&mut self) {
        self.model_is_thinking = false;
    }

    /// 将文本增量追加到当前助手消息；不改动滚动位置，用户回看历史时不被拉回底部。
    ///
    /// 这是"写入消息"的原语：事件循环不直接调用它，而是先经 [`Self::queue_model_delta`]
    /// 进入揭示缓冲，再由每帧的 [`Self::reveal_pending_stream`] 调用本方法上屏。
    pub(crate) fn append_model_delta(&mut self, delta: &str) {
        if delta.is_empty() || !self.accepts_run_stream_output() {
            return;
        }
        self.finish_model_thinking();
        let index = self.streaming_message.unwrap_or_else(|| {
            self.messages
                .push(Msg::new(MsgKind::Assistant, String::new()));
            let index = self.messages.len() - 1;
            self.streaming_message = Some(index);
            index
        });
        if let Some(message) = self.messages.get_mut(index) {
            message.text.push_str(delta);
        }
    }

    /// 把事件循环取到的模型增量放进揭示缓冲，等待逐帧上屏。
    ///
    /// 与 [`Self::append_model_delta`] 的差别只在时机：本方法不做任何渲染决策，也不创建
    /// 消息，因此可以在一个事件里接收任意大的突发文本而不引发一次整段重排。
    pub(crate) fn queue_model_delta(&mut self, delta: &str) {
        if delta.is_empty() || !self.accepts_run_stream_output() {
            return;
        }
        self.pending_stream.push_str(delta);
    }

    /// 是否还有待揭示的模型文本。
    ///
    /// 主循环据此决定要不要启用帧时钟：没有待揭示文本时保持纯事件驱动，不空转。
    pub(crate) fn has_pending_stream(&self) -> bool {
        #[cfg(feature = "plugins")]
        if self
            .active_agent_session_view()
            .is_some_and(|view| view.has_pending_stream())
        {
            return true;
        }
        !self.pending_stream.is_empty()
    }

    /// 按本帧配额把待揭示文本铺到屏幕上，返回是否改动了消息内容。
    ///
    /// 两条路径决定本帧吐多少字：
    ///
    /// - **节拍路径**（常态）：由 [`RevealPacer`] 按真实流逝时间结算配额。积压越多速率
    ///   越高（[`REVEAL_DRAIN_MS`] 内排空），积压清空则回落到每帧一个字符。因为配额按
    ///   时间累计，网络一秒钟推来一大块与一秒钟推来十小块，屏幕上都是匀速吐字。
    /// - **背压路径**（兜底）：积压超过 [`REVEAL_BACKLOG_LIMIT`] 时，超出部分一次性上屏，
    ///   保证模型结束时屏幕上不会还差半屏没显示。
    ///
    /// 返回本帧真正上屏的字符数：主循环用它决定是否需要重绘，诊断探针用它统计吞吐。
    pub(crate) fn reveal_pending_stream(&mut self) -> usize {
        self.reveal_pending_stream_at(Instant::now())
    }

    /// 按指定墙钟时间揭示一帧。
    ///
    /// 与 [`Self::reveal_pending_stream`] 的唯一差别是时间来源：测试用虚拟时钟驱动，
    /// 才能断言"每帧吐几个字符"这种与真实时间相关的行为。
    pub(crate) fn reveal_pending_stream_at(&mut self, now: Instant) -> usize {
        #[cfg(feature = "plugins")]
        if self.active_agent_session_route().is_some() {
            // 主会话在后台继续归档，不为不可见内容逐帧重绘。
            self.flush_pending_stream();
            return self
                .active_agent_session_view_mut()
                .map_or(0, |view| view.reveal_pending_stream_at(now));
        }
        if self.pending_stream.is_empty() {
            self.reveal_pacer.reset();
            return 0;
        }
        // 运行已结束或已被取消：缓冲里的文本不会再有归属消息，直接丢弃。
        if !self.accepts_run_stream_output() {
            self.pending_stream.clear();
            self.reveal_pacer.reset();
            return 0;
        }
        let chunk = self.reveal_pacer.take_chunk(&mut self.pending_stream, now);
        let revealed = chunk.chars().count();
        self.append_model_delta(&chunk);
        revealed
    }

    /// 立即揭示全部待显示文本。
    ///
    /// 在工具开始、运行结束、用户取消与新一次模型响应这些边界上调用：这些时刻"助手正文"
    /// 的归属会发生变化（后面要插入工具消息，或整份列表会被 Session 重建），把剩余文本
    /// 补完才能保证显示顺序与最终内容一致。
    ///
    /// 补完同时清空节拍状态：残余额度不该被下一次响应继承，否则新回答的第一帧会一次
    /// 蹦出好几个字。
    pub(crate) fn flush_pending_stream(&mut self) {
        self.reveal_pacer.reset();
        if self.pending_stream.is_empty() {
            return;
        }
        if !self.accepts_run_stream_output() {
            self.pending_stream.clear();
            return;
        }
        let chunk = std::mem::take(&mut self.pending_stream);
        self.append_model_delta(&chunk);
    }

    /// 返回当前是否允许本次运行继续接收流式增量。
    ///
    /// 用户请求取消后立即拒绝迟到增量。审批期间仍接收后台事件，由主事件区快照负责冻结
    /// 可见内容，审批结束后可以完整追上而不丢失并行工具结果。
    pub(crate) fn accepts_run_stream_output(&self) -> bool {
        self.running && !self.run_stream_cancelled
    }

    /// 完成 Agent 运行，以完整 Session 校准界面，并返回是否可自动推进 FIFO。
    ///
    /// 失败信息使用完整错误链，确保插件、WASM 或协议层的根因可直接在主事件列表中查看。
    pub(crate) fn handle_agent_done(&mut self, completion: AgentCompletion) -> bool {
        // 结束处理会用 Session 重建整份消息列表，残留的待揭示文本必须先补完。
        self.flush_pending_stream();
        let AgentCompletion {
            run,
            session_record,
            error,
            input_committed,
            queue_may_advance,
            input,
        } = completion;
        let user_cancelled = self.run_stream_cancelled;
        // 用户取消后保留 Esc 时冻结的界面；Session 仍更新到持久完成态，但不能让迟到内容
        // 在 AgentDone 阶段重新跳回消息列表。其他失败仅在没有完成态 Session 时保留界面。
        let preserve_visible_history = user_cancelled || (run.is_none() && error.is_some());
        // 只有真正跑完并留下回答的轮次才值得起标题：取消、失败或没有完成态会话时，
        // 首问首答不完整，生成出来的标题只会是猜测。
        let run_succeeded =
            error.is_none() && input_committed && run.as_ref().is_some_and(|run| !run.cancelled);
        self.running = false;
        self.run_cancellation = None;
        let active_input_message = self.active_input_message.take();
        self.run_started_at = None;
        self.model_is_thinking = false;
        let queued_run = std::mem::take(&mut self.queued_run_active);

        if input_committed {
            self.session_record = session_record;
            if !preserve_visible_history {
                // 尚未发送的队列项只存在于输入框上方的排队面板，不写入会话正文。
                self.messages = restore_session_messages(&self.session_record.session);
            }
        } else {
            // 自动派发的输入已出队；首存失败时恢复到队首，既不丢消息也不覆盖新草稿。
            if queued_run
                || !self.input.is_empty()
                || !self.attachments.is_empty()
                || !self.refs.is_empty()
            {
                self.queued_inputs.push_front(input.clone());
                self.queue_focus = self.queue_focus.map(|index| index + 1);
            } else {
                self.input = input.text.clone();
                self.cursor = self.input.len();
                self.attachments = input.attachments.clone();
                self.refs = input.refs.clone();
            }
            if let Some(index) = active_input_message.filter(|index| {
                self.messages.get(*index).is_some_and(|message| {
                    matches!(message.kind, MsgKind::User) && message.text == input.text
                })
            }) {
                self.messages.remove(index);
            }
        }

        self.streaming_message = None;
        self.run_stream_cancelled = false;
        if !queue_may_advance {
            self.pause_immediate_input();
        }
        if user_cancelled {
            let queued = self.queued_inputs.len();
            if let Some(message) = self.messages.iter_mut().rev().find(|message| {
                matches!(message.kind, MsgKind::Info)
                    && message.text == "Cancelling the current run..."
            }) {
                message.text = if self.immediate_input.is_some() && queue_may_advance {
                    "Run stopped; sending the priority message".into()
                } else if queued == 0 {
                    "Run cancelled; visible output was frozen".into()
                } else {
                    format!(
                        "Run cancelled; {queued} queued message(s) paused — Ctrl+O to send or edit"
                    )
                };
            }
        } else if run.as_ref().is_some_and(|run| run.cancelled) {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "Run cancelled; generated content was preserved",
            ));
        }
        if let Some(error) = error {
            self.messages
                .push(Msg::new(MsgKind::Error, format!("{error:#}")));
        }
        // 只有真正产生了回答才值得起标题：被取消或失败的轮次没有可供概括的内容。
        if run_succeeded && !user_cancelled {
            self.schedule_session_title();
        }
        if user_cancelled || run.as_ref().is_some_and(|run| run.cancelled) || !queue_may_advance {
            self.queue_paused = true;
        }
        queue_may_advance && (self.immediate_input.is_some() || !self.queue_paused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 揭示速率必须随积压单调上升，并收在"每帧一个字符"到"每帧三十个字符"之间。
    ///
    /// 单调性保证屏幕节奏随网络平滑变化；上下限保证既不长时间静止，也不整屏喷射。
    #[test]
    fn reveal_rate_is_monotonic_and_bounded() {
        assert_eq!(
            reveal_rate_chars_per_sec(0),
            crate::theme::REVEAL_MIN_CHARS_PER_SEC,
            "空积压不应低于基础节拍"
        );
        assert_eq!(
            reveal_rate_chars_per_sec(1),
            crate::theme::REVEAL_MIN_CHARS_PER_SEC,
            "极小积压仍按基础节拍吐字"
        );
        let huge = reveal_rate_chars_per_sec(1_000_000);
        assert_eq!(
            huge,
            crate::theme::REVEAL_MAX_CHARS_PER_SEC,
            "超大积压也必须收在上限内"
        );
        let mut previous = 0;
        for backlog in [1usize, 4, 16, 64, 256, 1024, 4096] {
            let rate = reveal_rate_chars_per_sec(backlog);
            assert!(rate >= previous, "速率必须随积压单调不减：{backlog}");
            assert!(rate <= huge, "速率不得超过上限：{backlog}");
            previous = rate;
        }
    }

    /// 配额按流逝时间累计：不足一个字符的余量留到下一帧，长期平均等于设定速率。
    ///
    /// 60 字符/秒在 16ms 帧下是 0.96 字符，逐帧取整会丢掉 4%，长期会明显偏慢；
    /// 余量结转后 10 帧正好吐出 9~10 个字符。
    #[test]
    fn pacer_carries_fractional_characters_between_frames() {
        let mut pacer = RevealPacer::default();
        let base = Instant::now();
        let mut total = 0;
        for frame in 1..=10u64 {
            let now = base + Duration::from_millis(frame * REVEAL_FRAME_INTERVAL_MS);
            // 积压只给 10 个字符，速率因此始终停在下限，才能单独验证取整余量。
            total += pacer.settle(10, now);
        }
        // 10 帧 × 16ms = 160ms，60 字符/秒 → 9.6 字符。
        assert!(
            (9..=10).contains(&total),
            "长期吐字量应贴合设定速率：{total}"
        );
    }

    /// 停滞很久之后的第一次结算只补一帧配额，绝不把几十帧的欠账一次补上。
    #[test]
    fn pacer_does_not_replay_a_long_stall() {
        let mut pacer = RevealPacer::default();
        let base = Instant::now();
        pacer.settle(4096, base + Duration::from_millis(REVEAL_FRAME_INTERVAL_MS));
        let quota = pacer.settle(4096, base + Duration::from_secs(5));

        let three_frames =
            crate::theme::REVEAL_MAX_CHARS_PER_SEC as u64 * REVEAL_FRAME_INTERVAL_MS * 3 / 1000;
        assert!(
            quota as u64 <= three_frames + 1,
            "停滞 5 秒后也不应一次吐出多帧的量：{quota}"
        );
        assert!(quota > 0, "停滞之后仍要立刻继续吐字：{quota}");
    }

    /// 缓冲排空后节拍状态归零：下一段文本重新按基础节拍起步。
    #[test]
    fn pacer_resets_after_the_buffer_drains() {
        let mut pacer = RevealPacer::default();
        let base = Instant::now();
        pacer.settle(500, base + Duration::from_millis(REVEAL_FRAME_INTERVAL_MS));
        assert_eq!(
            pacer.credit_milli, 800,
            "28.8 个字符应留下 800 千分之一余量"
        );

        pacer.settle(
            0,
            base + Duration::from_millis(2 * REVEAL_FRAME_INTERVAL_MS),
        );

        assert_eq!(pacer.credit_milli, 0);
        assert!(pacer.last_settle.is_none());
    }
}
