//! 一次用户提交的排队、运行、流式增量与完成处理。

use super::App;
use crate::app::reasoning_level_label;
use crate::config::persist_reasoning_level;
use crate::event::{AgentCompletion, SessionReloadOutcome, UiEvent};
use crate::session::conversation::{Msg, MsgKind, UserSubmission};
use crate::session::{restore_session_messages, run_and_persist};
use agent_core::model::ReasoningLevel;
use agent_core::{Agent, TokenUsage};
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
    pub(crate) fn start_model_response(&mut self) {
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
            if !user_cancelled && !run.usage.is_empty() {
                self.messages.push(Msg::new(
                    MsgKind::Info,
                    format_run_usage(&run.usage, run.steps_used),
                ));
            }
        }
        if let Some(error) = error {
            self.messages
                .push(Msg::new(MsgKind::Error, format!("{error:#}")));
        }
        queue_may_advance && !user_cancelled
    }
}

/// 格式化运行用量，并在服务商提供时显示提示缓存读取与写入量。
fn format_run_usage(usage: &TokenUsage, steps_used: usize) -> String {
    let mut cache = String::new();
    if usage.cache_read_tokens.is_some() || usage.cache_write_tokens.is_some() {
        cache = format!(
            " cache read {} write {}",
            usage.cache_read_tokens.unwrap_or(0),
            usage.cache_write_tokens.unwrap_or(0),
        );
    }
    format!(
        "↑{} ↓{}{} Σ{} tokens · {} steps",
        usage.input_tokens.unwrap_or(0),
        usage.output_tokens.unwrap_or(0),
        cache,
        usage.total_tokens.unwrap_or(0),
        steps_used,
    )
}
