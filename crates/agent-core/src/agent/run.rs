//! Core Agent 的 ReAct 循环、工具执行、状态收敛和事件发布。

use super::*;

/// 把工具同步输出回调转发到当前异步运行循环。
struct ToolOutputChannel(tokio::sync::mpsc::UnboundedSender<ToolOutputDelta>);

impl ToolOutputSink for ToolOutputChannel {
    fn emit(&self, output: ToolOutputDelta) {
        let _ = self.0.send(output);
    }
}

impl Agent {
    /// 消费一次取消请求；运行循环在检查点调用。
    fn take_cancelled(&self) -> bool {
        self.cancelled
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// 取出下一条 steering 消息。
    fn pop_steering(&self) -> Option<String> {
        let mut queue = self.steering.lock().expect("steering lock poisoned");
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    /// 取出下一条 follow-up 消息。
    fn pop_follow_up(&self) -> Option<String> {
        let mut queue = self.follow_ups.lock().expect("follow_ups lock poisoned");
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    /// 为一次用户输入准备可先行持久化的会话。
    ///
    /// 空会话会补充当前 Agent 的 system 提示，随后只追加一次用户消息。应用层可先保存
    /// 返回值，再调用 [`Agent::run_session`]，从而保证模型请求失败时仍保留用户输入。
    pub fn prepare_session(&self, mut session: Session, input: impl Into<String>) -> Session {
        if session.system().is_none() {
            session.set_system(self.options.system_prompt.clone());
        }
        session.push_user(input.into());
        session
    }

    /// 为一次多内容块的用户输入准备可先行持久化的会话。
    ///
    /// 行为与 [`Agent::prepare_session`] 一致，但允许调用方提供文本、图片、
    /// 文件附件等任意用户内容块组合；`content` 为空时不追加用户消息。
    pub fn prepare_session_blocks(
        &self,
        mut session: Session,
        content: Vec<crate::model::ContentBlock>,
    ) -> Session {
        if session.system().is_none() {
            session.set_system(self.options.system_prompt.clone());
        }
        session.push_user_blocks(content);
        session
    }

    /// Run the ReAct loop for one user input.
    /// 对一次用户输入运行 ReAct 循环。
    ///
    /// # Errors
    ///
    /// 同一 Agent 已在运行，或上下文加载、模型、工具、扩展及事件 sink 任一环节失败时
    /// 返回错误。运行开始后的错误会写入 [`AgentState::error`] 并收敛为失败状态。
    pub async fn run(&self, input: impl Into<String>) -> Result<AgentRun> {
        let session = self.prepare_session(Session::new(), input);
        self.run_session(session).await
    }

    /// 在已有会话上继续运行 ReAct 循环。
    /// Continue the ReAct loop on an existing session.
    ///
    /// # Errors
    ///
    /// 错误条件与 [`Agent::run_session`] 一致；用户输入会在运行前追加到返回状态的会话。
    pub async fn run_continue(
        &self,
        session: Session,
        input: impl Into<String>,
    ) -> Result<AgentRun> {
        let session = self.prepare_session(session, input);
        self.run_session(session).await
    }

    /// 直接运行调用方构造的会话，不自动追加用户消息或替换 system 提示。
    ///
    /// 收到取消请求（[`AgentControl::cancel`]）时在最近的检查点优雅收尾，
    /// 返回 `cancelled = true` 的 [`AgentRun`]，会话保留已完成的内容。
    ///
    /// # Errors
    ///
    /// 同一 Agent 已在运行，或上下文加载、模型、工具、扩展及事件 sink 任一环节失败时
    /// 返回错误。成功、取消和失败都会更新 [`Agent::state`] 返回的最近运行快照。
    pub async fn run_session(&self, session: Session) -> Result<AgentRun> {
        let run_id = uuid::Uuid::new_v4().to_string();
        self.begin_run(&run_id, &session)?;

        let result = self.run_session_inner(run_id, session).await;
        match &result {
            Ok(run) => self.finish_state(run),
            Err(error) => self.fail_state(error),
        }
        result
    }

    /// 执行已取得唯一运行槽位的 ReAct 循环，所有退出由外层统一收敛状态。
    async fn run_session_inner(&self, run_id: String, mut session: Session) -> Result<AgentRun> {
        let mut total_usage = TokenUsage::default();
        // 取消只作用于当前运行：清除上一次运行结束后残留的取消请求。
        self.cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);

        self.emit(
            &run_id,
            AgentEventKind::RunStarted,
            0,
            json!({
                "provider": &self.options.provider,
                "model": &self.options.model,
            }),
        )
        .await?;

        let mut step = 0;
        let mut steps_since_user_input = 0;
        while self.options.max_steps == 0 || steps_since_user_input < self.options.max_steps {
            self.update_state(|state| {
                state.phase = AgentPhase::Preparing;
                state.step = step;
                state.session = session.clone();
                state.streamed_text.clear();
                state.thinking_text.clear();
                state.tool_calls.clear();
            });
            // 检查点：follow-up 续跑或上一轮收尾期间到达的取消请求。
            if self.take_cancelled() {
                return self
                    .finish_cancelled(&run_id, step, step, total_usage, session)
                    .await;
            }
            self.emit(&run_id, AgentEventKind::TurnStarted, step, json!({}))
                .await?;

            let tools = self.tool_specs().await?;
            // 扩展提示不会写入会话，但必须参与每次请求的上下文加载和清洗。
            let mut source_messages = self.extension.prompt_messages().await?;
            source_messages.extend(session.model_messages());
            let loaded_context = self
                .context_loader
                .load(ContextLoadRequest {
                    run_id: run_id.clone(),
                    step,
                    provider: self.options.provider.clone(),
                    model: self.options.model.clone(),
                    system: session.system().cloned(),
                    messages: source_messages,
                    user_initiated: false,
                })
                .await
                .context("上下文加载失败")?;
            let req = ModelRequest {
                model: self.options.model.clone(),
                system: loaded_context.system,
                messages: crate::model::transform::transform_messages(&loaded_context.messages),
                tools,
                tool_choice: self.options.tool_choice.clone(),
                max_tokens: self.options.max_tokens,
                temperature: self.options.temperature,
                reasoning: self.options.reasoning,
                provider_options: self.options.provider_options.clone(),
            };

            self.update_state(|state| state.phase = AgentPhase::RequestingModel);

            self.emit(
                &run_id,
                AgentEventKind::ModelRequest,
                step,
                json!({
                    "provider": &self.options.provider,
                    "model": &self.options.model,
                    "message_count": req.messages.len(),
                    "tool_count": req.tools.len(),
                }),
            )
            .await?;

            let mut model_stream = if self.options.stream {
                let stream = self.gateway.stream(&self.options.provider, req).await?;
                self.update_state(|state| state.phase = AgentPhase::StreamingModel);
                stream
            } else {
                let response = self.gateway.complete(&self.options.provider, req).await?;
                let (sender, stream) = crate::model::ModelEventStream::channel();
                sender.done(response);
                stream
            };
            // 累积文本增量：取消发生在流中途时，把已生成的部分文本保留进会话。
            let mut streamed_text = String::new();
            while let Some(event) = model_stream.next().await {
                // 检查点：流事件之间响应取消；丢弃流即中止本次模型请求。
                if self.take_cancelled() {
                    drop(model_stream);
                    if !streamed_text.is_empty() {
                        session.push_assistant_blocks(vec![crate::model::ContentBlock::Text {
                            text: streamed_text,
                        }]);
                    }
                    return self
                        .finish_cancelled(&run_id, step, step + 1, total_usage, session)
                        .await;
                }
                match event {
                    ModelStreamEvent::TextDelta { index, delta } => {
                        streamed_text.push_str(&delta);
                        self.update_state(|state| state.streamed_text.push_str(&delta));
                        self.emit(
                            &run_id,
                            AgentEventKind::ModelTextDelta,
                            step,
                            json!({ "index": index, "delta": delta }),
                        )
                        .await?;
                    }
                    ModelStreamEvent::ThinkingDelta { index, delta } => {
                        self.update_state(|state| state.thinking_text.push_str(&delta));
                        self.emit(
                            &run_id,
                            AgentEventKind::ModelThinkingDelta,
                            step,
                            json!({ "index": index, "delta": delta }),
                        )
                        .await?;
                    }
                    event if event.is_terminal() => break,
                    _ => {}
                }
            }
            let response = model_stream.result().await?;
            let response_text = response.text_content();
            let tool_calls = response.tool_calls.clone();
            let usage = response.usage.clone();
            let provider_billing = response.billing.clone();
            session.push_assistant_blocks(response.content.clone());
            self.update_state(|state| {
                state.session = session.clone();
                state.streamed_text.clear();
                state.tool_calls = tool_calls
                    .iter()
                    .cloned()
                    .map(AgentToolCallState::pending)
                    .collect();
            });

            self.emit(
                &run_id,
                AgentEventKind::ModelResponse,
                step,
                json!({
                    "finish_reason": response.finish_reason,
                    "tool_call_count": tool_calls.len(),
                    "text_len": response_text.len(),
                    "usage": &usage,
                    "provider_billing": &provider_billing,
                }),
            )
            .await?;

            if let Some(usage) = &usage {
                total_usage.add_assign(usage);
                self.update_state(|state| state.usage = total_usage.clone());
            }

            let billing = BillingUsage::new(
                self.options.provider.clone(),
                self.options.model.clone(),
                usage,
                provider_billing,
            );
            if !billing.is_empty() {
                self.emit(
                    &run_id,
                    AgentEventKind::BillingUsage,
                    step,
                    serde_json::to_value(&billing)?,
                )
                .await?;
            }

            if tool_calls.is_empty() {
                let final_text = if response_text.is_empty() {
                    session.last_assistant_text()
                } else {
                    response_text
                };
                self.emit(&run_id, AgentEventKind::TurnFinished, step, json!({}))
                    .await?;

                // 模型直接返回文本时也必须消费运行期间到达的 steering，避免新指令静默遗留。
                if let Some(message) = self.pop_steering() {
                    self.emit(
                        &run_id,
                        AgentEventKind::SteeringInjected,
                        step,
                        json!({ "text_len": message.len() }),
                    )
                    .await?;
                    session.push_user(message);
                    self.update_state(|state| state.session = session.clone());
                    step += 1;
                    steps_since_user_input = 0;
                    continue;
                }

                // 任务完成前检查 follow-up 队列；有消息则注入并继续循环。
                if let Some(follow_up) = self.pop_follow_up() {
                    self.emit(
                        &run_id,
                        AgentEventKind::FollowUpInjected,
                        step,
                        json!({ "text_len": follow_up.len() }),
                    )
                    .await?;
                    session.push_user(follow_up);
                    self.update_state(|state| state.session = session.clone());
                    step += 1;
                    steps_since_user_input = 0;
                    continue;
                }

                self.emit(
                    &run_id,
                    AgentEventKind::RunFinished,
                    step,
                    json!({ "steps_used": step + 1, "usage": &total_usage }),
                )
                .await?;
                return Ok(AgentRun {
                    run_id,
                    final_text,
                    steps_used: step + 1,
                    usage: total_usage,
                    session,
                    cancelled: false,
                });
            }

            // 逐个执行工具；每个工具执行前检查取消，完成后检查 steering 队列。
            self.update_state(|state| state.phase = AgentPhase::ExecutingTools);
            let mut results = Vec::new();
            let mut steering_message = None;
            let mut run_cancelled = false;
            for (index, call) in tool_calls.iter().enumerate() {
                // 检查点：取消优先于 steering，跳过所有尚未执行的工具。
                if self.take_cancelled() {
                    for skipped in &tool_calls[index..] {
                        self.emit(
                            &run_id,
                            AgentEventKind::ToolSkipped,
                            step,
                            json!({
                                "call": skipped,
                                "reason": "Skipped due to cancelled run",
                            }),
                        )
                        .await?;
                        results.push(ToolResult::error(
                            skipped.id.clone(),
                            skipped.name.clone(),
                            "Skipped due to cancelled run",
                        ));
                        self.mark_tool_skipped(&skipped.id);
                    }
                    run_cancelled = true;
                    break;
                }

                let result = self
                    .execute_tool_with_hooks(&run_id, call.clone(), step)
                    .await?;
                results.push(result);

                // 工具前置策略可以取消当前运行；立即跳过同一批剩余工具。
                if self.take_cancelled() {
                    for skipped in &tool_calls[index + 1..] {
                        self.emit(
                            &run_id,
                            AgentEventKind::ToolSkipped,
                            step,
                            json!({
                                "call": skipped,
                                "reason": "Skipped due to cancelled run",
                            }),
                        )
                        .await?;
                        results.push(ToolResult::error(
                            skipped.id.clone(),
                            skipped.name.clone(),
                            "Skipped due to cancelled run",
                        ));
                        self.mark_tool_skipped(&skipped.id);
                    }
                    run_cancelled = true;
                    break;
                }

                if let Some(message) = self.pop_steering() {
                    // 剩余工具标记为 Skipped，让模型知道它们没有执行。
                    for skipped in &tool_calls[index + 1..] {
                        self.emit(
                            &run_id,
                            AgentEventKind::ToolSkipped,
                            step,
                            json!({
                                "call": skipped,
                                "reason": "Skipped due to queued user message",
                            }),
                        )
                        .await?;
                        results.push(ToolResult::error(
                            skipped.id.clone(),
                            skipped.name.clone(),
                            "Skipped due to queued user message",
                        ));
                        self.mark_tool_skipped(&skipped.id);
                    }
                    steering_message = Some(message);
                    break;
                }
            }
            session.push_tool_results(results);
            self.update_state(|state| state.session = session.clone());

            if run_cancelled {
                return self
                    .finish_cancelled(&run_id, step, step + 1, total_usage, session)
                    .await;
            }

            if let Some(message) = steering_message {
                self.emit(
                    &run_id,
                    AgentEventKind::SteeringInjected,
                    step,
                    json!({ "text_len": message.len() }),
                )
                .await?;
                session.push_user(message);
                self.update_state(|state| state.session = session.clone());
                steps_since_user_input = 0;
            } else {
                steps_since_user_input += 1;
            }

            self.emit(&run_id, AgentEventKind::TurnFinished, step, json!({}))
                .await?;
            step += 1;
        }

        self.emit(
            &run_id,
            AgentEventKind::StepLimitReached,
            step,
            json!({
                "max_steps": self.options.max_steps,
                "steps_used": step,
                "usage": &total_usage
            }),
        )
        .await?;
        Err(anyhow!(
            "max ReAct steps reached: {}",
            self.options.max_steps
        ))
    }

    /// 以取消终态收尾当前运行：发出带 `cancelled: true` 的 RunFinished 事件，
    /// 并返回保留已完成内容的 [`AgentRun`]。
    async fn finish_cancelled(
        &self,
        run_id: &str,
        step: usize,
        steps_used: usize,
        total_usage: TokenUsage,
        session: Session,
    ) -> Result<AgentRun> {
        self.emit(
            run_id,
            AgentEventKind::RunFinished,
            step,
            json!({ "steps_used": steps_used, "usage": &total_usage, "cancelled": true }),
        )
        .await?;
        Ok(AgentRun {
            run_id: run_id.to_string(),
            final_text: session.last_assistant_text(),
            steps_used,
            usage: total_usage,
            session,
            cancelled: true,
        })
    }

    async fn execute_tool_with_hooks(
        &self,
        run_id: &str,
        call: ToolCall,
        step: usize,
    ) -> Result<ToolResult> {
        let original_call = call.clone();
        let decision = loop {
            if self.control().cancel_requested() {
                break ToolDecision::CancelRun {
                    reason: "工具执行前检查已取消".to_string(),
                };
            }
            tokio::select! {
                decision = self.extension.before_tool(&call) => break decision?,
                _ = self.cancel_notify.notified() => {}
            }
        };
        let call = match decision {
            ToolDecision::Allow => call,
            ToolDecision::Rewrite { call } => call,
            ToolDecision::Block { reason } => {
                let result = ToolResult::error(original_call.id, original_call.name, reason);
                self.extension.after_tool(&result).await?;
                self.finish_tool_state(&result);
                return Ok(result);
            }
            ToolDecision::CancelRun { reason } => {
                self.control().cancel();
                let result = ToolResult::error(original_call.id, original_call.name, reason);
                self.extension.after_tool(&result).await?;
                self.finish_tool_state(&result);
                return Ok(result);
            }
        };

        self.update_state(|state| {
            if let Some(tool) = state
                .tool_calls
                .iter_mut()
                .find(|tool| tool.call.id == call.id)
            {
                tool.call = call.clone();
                tool.status = AgentToolCallStatus::Running;
            }
        });

        self.emit(
            run_id,
            AgentEventKind::ToolStarted,
            step,
            // 内建工具事件直接使用共享工具类型，避免跨层转换丢失调用身份或参数。
            serde_json::to_value(&call)?,
        )
        .await?;

        let result = if self.tools.contains(&call.name) {
            self.execute_native_tool_with_output(run_id, step, call)
                .await?
        } else if let Some(result) = self.extension.call_tool(call.clone()).await? {
            result
        } else {
            ToolResult::error(
                call.id.clone(),
                call.name.clone(),
                format!("unknown tool: {}", call.name),
            )
        };

        self.extension.after_tool(&result).await?;
        self.finish_tool_state(&result);
        self.emit(
            run_id,
            AgentEventKind::ToolFinished,
            step,
            // 完整 ToolResult 包含 UI 专用 details，截断和展示策略由应用层决定。
            serde_json::to_value(&result)?,
        )
        .await?;
        Ok(result)
    }

    /// 执行原生工具，并在最终结果到达前按顺序发布运行期输出事件。
    async fn execute_native_tool_with_output(
        &self,
        run_id: &str,
        step: usize,
        call: ToolCall,
    ) -> Result<ToolResult> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let output: Arc<dyn ToolOutputSink> = Arc::new(ToolOutputChannel(tx));
        let mut execution = Box::pin(self.tools.call_with_output(call, output));
        loop {
            tokio::select! {
                result = &mut execution => {
                    while let Ok(output) = rx.try_recv() {
                        self.record_tool_output(run_id, step, output).await?;
                    }
                    return result;
                }
                Some(output) = rx.recv() => {
                    self.record_tool_output(run_id, step, output).await?;
                }
            }
        }
    }

    /// 只向事件 sink 写入高频工具输出，不逐块调用扩展 hook 或排空插件事件。
    async fn record_tool_output(
        &self,
        run_id: &str,
        step: usize,
        output: ToolOutputDelta,
    ) -> Result<()> {
        let event = AgentEvent::new(
            run_id.to_string(),
            AgentEventKind::ToolOutputDelta,
            step,
            serde_json::to_value(output)?,
        );
        self.events.record(&event).await
    }

    /// 取得唯一运行槽位并用输入会话初始化完整状态。
    fn begin_run(&self, run_id: &str, session: &Session) -> Result<()> {
        let mut state = self.state.lock().expect("Agent 状态锁不应中毒");
        if state.phase.is_running() {
            return Err(anyhow!("agent is already running"));
        }
        *state = AgentState {
            phase: AgentPhase::Preparing,
            run_id: Some(run_id.to_string()),
            session: session.clone(),
            ..AgentState::default()
        };
        Ok(())
    }

    /// 将成功或取消结果收敛为稳定终态快照。
    fn finish_state(&self, run: &AgentRun) {
        self.update_state(|state| {
            state.phase = if run.cancelled {
                AgentPhase::Cancelled
            } else {
                AgentPhase::Succeeded
            };
            state.run_id = Some(run.run_id.clone());
            state.step = run.steps_used.saturating_sub(1);
            state.session = run.session.clone();
            state.streamed_text.clear();
            state.thinking_text.clear();
            state.usage = run.usage.clone();
            state.error = None;
        });
    }

    /// 将任意 ReAct 错误收敛为失败终态，同时保留最后已确认的运行上下文。
    fn fail_state(&self, error: &anyhow::Error) {
        self.update_state(|state| {
            state.phase = AgentPhase::Failed;
            state.error = Some(format!("{error:#}"));
        });
    }

    /// 在内部锁保护下执行一次不可观察的原子状态更新。
    fn update_state(&self, update: impl FnOnce(&mut AgentState)) {
        update(&mut self.state.lock().expect("Agent 状态锁不应中毒"));
    }

    /// 将工具结果写入对应调用状态；未知调用不会扩张模型声明的调用集合。
    fn finish_tool_state(&self, result: &ToolResult) {
        self.update_state(|state| {
            if let Some(tool) = state
                .tool_calls
                .iter_mut()
                .find(|tool| tool.call.id == result.call_id)
            {
                tool.status = if result.is_error {
                    AgentToolCallStatus::Failed
                } else {
                    AgentToolCallStatus::Succeeded
                };
                tool.result = Some(result.clone());
            }
        });
    }

    /// 将未执行的工具调用标记为跳过，并保留其原始参数供诊断。
    fn mark_tool_skipped(&self, call_id: &str) {
        self.update_state(|state| {
            if let Some(tool) = state
                .tool_calls
                .iter_mut()
                .find(|tool| tool.call.id == call_id)
            {
                tool.status = AgentToolCallStatus::Skipped;
            }
        });
    }

    /// 返回当前实际暴露给模型的原生工具与扩展工具定义。
    ///
    /// # Errors
    ///
    /// 扩展无法列出工具、工具名称非法，或原生工具与扩展工具存在重名时返回错误。
    pub async fn tool_specs(&self) -> Result<Vec<ToolSpec>> {
        let mut specs = self.tools.specs();
        specs.extend(self.extension.list_tools().await?);

        let mut names = HashSet::new();
        for spec in &specs {
            spec.validate_name()?;
            if !names.insert(spec.name.clone()) {
                return Err(anyhow!("duplicated tool exposed to model: {}", spec.name));
            }
        }
        Ok(specs)
    }

    /// 将调用方构造的事件写入 sink，并通知扩展观察该事件。
    ///
    /// 随后会刷新扩展发布的结构化事件。扩展事件只写入 sink，不再次回调扩展。
    ///
    /// # Errors
    ///
    /// 事件 sink 写入、扩展事件回调或扩展事件排空失败时返回错误。已经完成的前序写入
    /// 不会回滚。
    pub async fn dispatch_event(&self, event: AgentEvent) -> Result<()> {
        let run_id = event.run_id.clone();
        let step = event.step;
        self.events.record(&event).await?;
        self.extension.on_event(&event).await?;
        self.flush_extension_events(&run_id, step).await
    }

    async fn flush_extension_events(&self, run_id: &str, step: usize) -> Result<()> {
        for payload in self.extension.drain_events().await? {
            let event =
                AgentEvent::new(run_id.to_string(), AgentEventKind::Extension, step, payload);
            self.events.record(&event).await?;
        }
        Ok(())
    }

    async fn emit(
        &self,
        run_id: &str,
        kind: AgentEventKind,
        step: usize,
        payload: Value,
    ) -> Result<()> {
        let event = AgentEvent::new(run_id.to_string(), kind, step, payload);
        self.dispatch_event(event).await
    }
}
