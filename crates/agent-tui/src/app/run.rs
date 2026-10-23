//! 一次用户提交的排队、运行、流式增量与完成处理。

use super::App;
use crate::app::reasoning_level_label;
use crate::config::persist_reasoning_level;
use crate::event::{AgentCompletion, SessionReloadOutcome, UiEvent};
use crate::session::conversation::{Msg, MsgKind, UserSubmission};
use crate::session::{restore_session_messages, run_and_persist};
use crate::theme::{REVEAL_BACKLOG_LIMIT, REVEAL_CHARS_PER_FRAME, REVEAL_SPREAD_FRAMES};
use agent_core::model::ReasoningLevel;
use agent_core::Agent;
use anyhow::Result;
use std::sync::Arc;

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
                self.replace_session(saved, None);
                self.input = input;
                self.cursor = cursor;
                self.attachments = attachments;
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
    /// 插件加载导致 Agent 尚不可用时，将一条完整输入加入 FIFO 队列。
    pub(crate) fn queue_input_until_ready(&mut self) {
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.messages
            .push(Msg::new(MsgKind::User, submission.text.clone()));
        self.queued_inputs.push_back(submission);
        self.scroll = None;
    }

    /// 运行中提交 steering 插话：跳过剩余工具，让模型立即响应新指令。
    pub(crate) fn submit_steering(&mut self, agent: &Arc<Agent>) {
        let Some(input) = self.take_input() else {
            return;
        };
        agent.steer(input.clone());
        self.messages.push(Msg::new(MsgKind::User, input));
        self.messages.push(Msg::new(
            MsgKind::Info,
            "Steering queued; it will be applied after the current tool finishes",
        ));
    }

    /// Submits the current editor value immediately to a ready Agent.
    ///
    /// 将当前编辑器内容立即提交给已就绪的 Agent。
    pub(crate) fn submit(&mut self, agent: &Arc<Agent>) {
        let Some(submission) = self.take_submission() else {
            return;
        };
        self.queued_run_active = false;
        self.start_input_run(agent, submission, true);
    }

    /// Starts one Agent run and optionally appends the user message to the visible history.
    ///
    /// 启动一次 Agent 运行，并按需把用户消息追加到可见历史。
    pub(crate) fn start_input_run(
        &mut self,
        agent: &Arc<Agent>,
        submission: UserSubmission,
        show_user_message: bool,
    ) {
        if show_user_message {
            self.messages
                .push(Msg::new(MsgKind::User, submission.text.clone()));
        }
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
            )
            .await;
            let _ = tx.send(UiEvent::AgentDone(Box::new(result)));
        });
    }

    /// Starts the next pre-ready input after the Agent becomes idle.
    ///
    /// Agent 就绪且空闲后启动下一条预加载输入。
    pub(crate) fn run_next_queued(&mut self, agent: &Arc<Agent>) {
        if self.running {
            return;
        }
        if let Some(submission) = self.queued_inputs.front().cloned() {
            self.queued_run_active = true;
            self.start_input_run(agent, submission, false);
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
        !self.pending_stream.is_empty()
    }

    /// 按本帧预算把待揭示文本铺到屏幕上，返回是否改动了消息内容。
    ///
    /// 预算的目标是"网络快时不拖慢、网络抖时不整块蹦"：
    ///
    /// - 待揭示量在正常流式范围内（几十字符）时，预算远大于它，因此**一次性全部上屏**，
    ///   不引入任何额外延迟；
    /// - 网络一次推来几百字符时，按 [`REVEAL_SPREAD_FRAMES`] 摊平到若干帧，观感是连续
    ///   吐字而不是整段出现；
    /// - 积压超过 [`REVEAL_BACKLOG_LIMIT`] 时直接清空积压部分，保证模型结束时屏幕上不会
    ///   还差半屏没显示。
    ///
    /// 返回本帧真正上屏的字符数：主循环用它决定是否需要重绘，诊断探针用它统计吞吐。
    pub(crate) fn reveal_pending_stream(&mut self) -> usize {
        if self.pending_stream.is_empty() {
            return 0;
        }
        // 运行已结束或已被取消：缓冲里的文本不会再有归属消息，直接丢弃。
        if !self.accepts_run_stream_output() {
            self.pending_stream.clear();
            return 0;
        }
        let pending_bytes = self.pending_stream.len();
        // 两条路径的"预算"单位不同，分开算清楚，避免字节与字符混用导致中文只按三分之一
        // 速度揭示：
        // - 背压路径：按字节清空超出上限的积压，再回退到字符边界；
        // - 常规路径：按字符数决定本帧揭示多少，再换算成字节偏移。
        let take = if pending_bytes > REVEAL_BACKLOG_LIMIT {
            let mut end = pending_bytes - REVEAL_BACKLOG_LIMIT;
            while end > 0 && !self.pending_stream.is_char_boundary(end) {
                end -= 1;
            }
            end
        } else {
            let pending_chars = self.pending_stream.chars().count();
            let budget = (pending_chars / REVEAL_SPREAD_FRAMES).max(REVEAL_CHARS_PER_FRAME);
            // char_indices 的第 budget 个偏移正好是"前 budget 个字符"的结束位置，
            // 因此不存在切开多字节字符的可能。
            self.pending_stream
                .char_indices()
                .nth(budget)
                .map_or(pending_bytes, |(index, _)| index)
        };
        if take == 0 {
            return 0;
        }
        let chunk: String = self.pending_stream.drain(..take).collect();
        let revealed = chunk.chars().count();
        self.append_model_delta(&chunk);
        revealed
    }

    /// 立即揭示全部待显示文本。
    ///
    /// 在工具开始、运行结束、用户取消与新一次模型响应这些边界上调用：这些时刻"助手正文"
    /// 的归属会发生变化（后面要插入工具消息，或整份列表会被 Session 重建），把剩余文本
    /// 补完才能保证显示顺序与最终内容一致。
    pub(crate) fn flush_pending_stream(&mut self) {
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
        self.running = false;
        self.run_started_at = None;
        self.model_is_thinking = false;
        let queued_run = std::mem::take(&mut self.queued_run_active);
        if queued_run && input_committed {
            let committed_input = self
                .queued_inputs
                .pop_front()
                .expect("队列运行必须对应一个 FIFO 输入");
            debug_assert_eq!(committed_input, input);
        }

        if input_committed {
            self.session_record = session_record;
            if !preserve_visible_history {
                self.messages = restore_session_messages(&self.session_record.session);
                self.messages.extend(
                    self.queued_inputs
                        .iter()
                        .map(|pending| Msg::new(MsgKind::User, pending.text.clone())),
                );
            }
        } else {
            // 队列首存失败时保留原队列和已展示顺序，等待后续重试同一队首。
            if !queued_run && self.input.is_empty() && self.attachments.is_empty() {
                self.input = input.text.clone();
                self.cursor = self.input.len();
                self.attachments = input.attachments.clone();
            }
            if !queued_run
                && self.messages.last().is_some_and(|message| {
                    matches!(message.kind, MsgKind::User) && message.text == input.text
                })
            {
                self.messages.pop();
            }
        }

        self.streaming_message = None;
        self.run_stream_cancelled = false;
        if let Some(run) = run {
            if run.cancelled {
                if user_cancelled {
                    if let Some(message) = self.messages.iter_mut().rev().find(|message| {
                        matches!(message.kind, MsgKind::Info)
                            && message.text == "Cancelling the current run..."
                    }) {
                        message.text = "Run cancelled; visible output was frozen".into();
                    }
                } else {
                    self.messages.push(Msg::new(
                        MsgKind::Info,
                        "Run cancelled; generated content was preserved",
                    ));
                }
            }
        }
        if let Some(error) = error {
            self.messages
                .push(Msg::new(MsgKind::Error, format!("{error:#}")));
        }
        queue_may_advance && !user_cancelled
    }
}
