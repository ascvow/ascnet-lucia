//! 运行编排、流式增量与会话持久化的回归测试。

use super::*;

/// Agent 引用确实不可用时，输入保持 FIFO 顺序并显示在加载底栏。
///
/// 该路径用于启动装配失败等兜底场景；渐进插件加载的正常路径始终提供 Agent。
#[cfg(feature = "plugins")]
#[test]
fn unavailable_agent_queues_inputs_in_fifo_order() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app =
        App::new(tx, "测试模型".into()).with_loading_plugins(vec!["mcp".into(), "skill".into()]);

    for input in ["第一条任务", "第二条任务"] {
        app.input = input.into();
        app.cursor = app.input.len();
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
    }

    assert_eq!(
        app.queued_inputs
            .iter()
            .map(|submission| submission.text.as_str())
            .collect::<Vec<_>>(),
        vec!["第一条任务", "第二条任务"]
    );
    assert_eq!(
        app.messages
            .iter()
            .filter(|message| matches!(message.kind, MsgKind::User))
            .count(),
        2
    );
    let (_, status) = app.plugin_status_content();
    assert!(status.contains("queued 2"), "{status}");

    // 触发前缀尚未由插件声明时，斜杠文本没有特殊语义，按普通输入排队。
    app.input = "/clear".into();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
    assert!(app.input.is_empty());
    assert_eq!(app.queued_inputs.len(), 3);
}

/// Queued startup inputs execute sequentially and persist into one continuing session.
///
/// 启动队列中的输入应逐条执行，并持久化到同一个连续 Session。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn ready_agent_drains_startup_queue_in_fifo_order() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into()).with_loading_plugins(vec!["skill".into()]);
    for input in ["第一条任务", "第二条任务"] {
        app.input = input.into();
        app.cursor = app.input.len();
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
    }

    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.run_next_queued(&agent);
    for expected_revision in [2, 4] {
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(UiEvent::AgentDone(result)) = rx.recv().await {
                    break *result;
                }
            }
        })
        .await
        .expect("等待排队任务完成不应超时");
        let queue_may_advance = app.handle_agent_done(result);
        assert_eq!(app.session_record.revision, expected_revision);
        if queue_may_advance {
            app.run_next_queued(&agent);
        }
    }

    assert!(app.queued_inputs.is_empty());
    assert!(!app.running);
    let user_messages = app
        .session_record
        .session
        .messages()
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .map(|message| message.text_content())
        .collect::<Vec<_>>();
    assert_eq!(user_messages, vec!["第一条任务", "第二条任务"]);
}

/// 验证持久化 Session 会恢复用户、助手和已完成工具事件，而不展示系统提示词。
#[test]
fn persisted_session_hydrates_main_event_list() {
    let mut session = Session::new();
    session.set_system("不应显示的系统提示词");
    session.push_user("读取项目配置");
    session.push_assistant_blocks(vec![ContentBlock::ToolCall {
        call: ToolCall::new("call-1", "read_file", json!({"path": "config.toml"})),
    }]);
    session.push_tool_result(agent_tool::ToolResult::success(
        "call-1",
        "read_file",
        json!({"content": "配置内容"}),
    ));
    session.push_assistant_text("配置已经读取");

    let messages = restore_session_messages(&session);

    assert_eq!(messages.len(), 3);
    assert!(matches!(messages[0].kind, MsgKind::User));
    assert_eq!(messages[0].text, "读取项目配置");
    assert!(matches!(messages[1].kind, MsgKind::ToolOk));
    assert_eq!(messages[1].text, "read_file");
    assert_eq!(messages[1].tool_call_id(), Some("call-1"));
    assert_eq!(
        messages[1].tool_call.as_ref().map(|call| &call.args),
        Some(&json!({"path": "config.toml"}))
    );
    assert!(messages[1]
        .tool_result
        .as_ref()
        .is_some_and(|result| result.content_text().contains("配置内容")));
    assert!(matches!(messages[2].kind, MsgKind::Assistant));
    assert_eq!(messages[2].text, "配置已经读取");
}

/// 同名工具乱序返回时必须按调用 ID 恢复各自结果。
#[test]
fn persisted_session_matches_same_name_tools_by_call_id() {
    let mut session = Session::new();
    session.push_assistant_blocks(vec![
        ContentBlock::ToolCall {
            call: ToolCall::new("call-1", "read_file", json!({"path": "one"})),
        },
        ContentBlock::ToolCall {
            call: ToolCall::new("call-2", "read_file", json!({"path": "two"})),
        },
    ]);
    session.push_tool_result(ToolResult::success(
        "call-2",
        "read_file",
        json!({"content": "second"}),
    ));
    session.push_tool_result(ToolResult::success(
        "call-1",
        "read_file",
        json!({"content": "first"}),
    ));

    let messages = restore_session_messages(&session);

    let first = messages
        .iter()
        .find(|message| message.tool_call_id() == Some("call-1"))
        .expect("应恢复第一个工具调用");
    let second = messages
        .iter()
        .find(|message| message.tool_call_id() == Some("call-2"))
        .expect("应恢复第二个工具调用");
    assert_eq!(
        first.tool_result.as_ref().map(ToolResult::content_text),
        Some(r#"{"content":"first"}"#.to_string())
    );
    assert_eq!(
        second.tool_result.as_ref().map(ToolResult::content_text),
        Some(r#"{"content":"second"}"#.to_string())
    );
}

/// 验证普通空白启动不会创建项目会话目录，首次保存才创建。
#[tokio::test]
async fn lazy_session_store_creates_directory_on_first_save() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("生成测试时间戳")
        .as_nanos();
    let parent =
        std::env::temp_dir().join(format!("lucia-lazy-session-{}-{nonce}", std::process::id()));
    let root = parent.join("project").join("sessions");
    let store = LazyFileSessionStore::new(root.clone());

    assert!(store.list().await.expect("列出空存储").is_empty());
    assert!(!root.exists());

    let record = SessionRecord::new(
        SessionId::new("lazy-session").expect("创建测试会话标识"),
        Session::new(),
    )
    .expect("创建测试会话");
    store.save(record, None).await.expect("首次保存会话");
    assert!(root.is_dir());

    std::fs::remove_dir_all(parent).expect("清理惰性会话测试目录");
}

/// 验证多个文本增量只更新一条助手消息，最终结果不会重复追加。
#[test]
fn streamed_deltas_update_one_assistant_message() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.start_model_response();
    app.append_model_delta("你");
    app.append_model_delta("好");

    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].text, "你好");

    let mut completed_session = Session::new();
    completed_session.push_assistant_text("你好！");
    let mut saved_record = app.session_record.clone();
    saved_record.session = completed_session.clone();
    app.handle_agent_done(AgentCompletion {
        run: Some(AgentRun {
            run_id: "run-test".into(),
            final_text: "你好！".into(),
            steps_used: 1,
            usage: Default::default(),
            session: completed_session,
            cancelled: false,
        }),
        session_record: saved_record,
        error: None,
        input_committed: true,
        queue_may_advance: true,
        input: "测试".into(),
    });

    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].text, "你好！");
    assert!(app.streaming_message.is_none());
}

/// 运行完成提示必须展示服务商返回的缓存读取与写入 token。
#[test]
fn completed_run_displays_prompt_cache_usage() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut completed_session = Session::new();
    completed_session.push_assistant_text("完成");
    let mut saved_record = app.session_record.clone();
    saved_record.session = completed_session.clone();

    app.handle_agent_done(AgentCompletion {
        run: Some(AgentRun {
            run_id: "cache-usage".into(),
            final_text: "完成".into(),
            steps_used: 2,
            usage: TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                total_tokens: Some(1_000),
                cache_read_tokens: Some(700),
                cache_write_tokens: Some(180),
            },
            session: completed_session,
            cancelled: false,
        }),
        session_record: saved_record,
        error: None,
        input_committed: true,
        queue_may_advance: true,
        input: "测试缓存".into(),
    });

    assert!(app.messages.iter().any(|message| {
        matches!(message.kind, MsgKind::Info)
            && message.text == "↑100 ↓20 cache read 700 write 180 Σ1000 tokens · 2 steps"
    }));
}

/// 推理增量只切换运行阶段，不保存摘要文本，并在正文、下一轮或运行结束时复位。
#[test]
fn thinking_deltas_only_toggle_run_phase() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.mark_model_thinking();
    app.mark_model_thinking();

    assert!(app.model_is_thinking);
    assert!(app.messages.is_empty());
    app.start_model_response();
    assert!(!app.model_is_thinking);

    app.mark_model_thinking();
    app.handle_agent_done(AgentCompletion {
        run: None,
        session_record: app.session_record.clone(),
        error: None,
        input_committed: false,
        queue_may_advance: true,
        input: String::new().into(),
    });
    assert!(!app.model_is_thinking);
    assert!(app.messages.is_empty());

    app.start_model_response();
    assert!(!app.model_is_thinking);
}

/// 高频模型增量只排队一个 UI 通知，消费后才能创建下一次通知。
#[tokio::test]
async fn channel_event_sink_coalesces_model_text_notifications() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, model_deltas, _thinking_deltas) = ChannelEventSink::new(tx);
    let first = AgentEvent::new(
        "run-test",
        AgentEventKind::ModelTextDelta,
        0,
        json!({ "delta": "你" }),
    );
    let second = AgentEvent::new(
        "run-test",
        AgentEventKind::ModelTextDelta,
        0,
        json!({ "delta": "好" }),
    );

    sink.record(&first).await.expect("记录首个模型增量");
    sink.record(&second).await.expect("记录第二个模型增量");

    assert!(matches!(rx.recv().await, Some(UiEvent::ModelTextReady)));
    assert!(rx.try_recv().is_err());
    assert_eq!(
        model_deltas
            .lock()
            .expect("模型增量缓冲区锁不应中毒")
            .take(),
        "你好"
    );

    sink.record(&first).await.expect("消费后应允许再次通知");
    assert!(matches!(rx.recv().await, Some(UiEvent::ModelTextReady)));
}

/// 模型用量必须按分项转发，供底栏分别计算上下文规模与缓存命中率。
#[tokio::test]
async fn channel_event_sink_forwards_prompt_cache_usage_breakdown() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _model_deltas, _thinking_deltas) = ChannelEventSink::new(tx);
    let event = AgentEvent::new(
        "run-cache",
        AgentEventKind::ModelResponse,
        0,
        json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_tokens": 700,
                "cache_write_tokens": 180
            }
        }),
    );

    sink.record(&event).await.expect("应转发缓存用量分项");

    let Some(UiEvent::ModelUsage(usage)) = rx.recv().await else {
        panic!("应收到模型用量事件");
    };
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.cache_read_tokens, Some(700));
    assert_eq!(usage.cache_write_tokens, Some(180));
}

/// 缓存命中率按「缓存读取 ÷（缓存读取 + 未缓存输入）」计算，缓存写入不计入分母。
///
/// 写入是建立缓存的一次性成本；计进分母会让刚建立缓存的会话显示成低命中。
#[test]
fn cache_hit_rate_excludes_cache_writes_from_denominator() {
    let usage = agent_core::model::TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(20),
        total_tokens: None,
        cache_read_tokens: Some(900),
        cache_write_tokens: Some(500),
    };

    let (label, _color) =
        crate::ui::chat::cache_hit_status(&usage).expect("有缓存读取时应给出命中率");

    assert!(label.starts_with("↺ 90%"), "{label}");
}

/// 没有缓存读取记录时不展示命中率，避免把"尚未命中"显示成 0%。
#[test]
fn cache_hit_rate_is_hidden_without_cache_reads() {
    let usage = agent_core::model::TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(20),
        total_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: Some(500),
    };

    assert!(crate::ui::chat::cache_hit_status(&usage).is_none());
}

/// 高频推理摘要增量必须与回答文本分开合并，避免写入助手消息缓冲区。
#[tokio::test]
async fn channel_event_sink_coalesces_thinking_notifications_separately() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, model_deltas, thinking_deltas) = ChannelEventSink::new(tx);
    for delta in ["检查", "边界"] {
        sink.record(&AgentEvent::new(
            "run-test",
            AgentEventKind::ModelThinkingDelta,
            0,
            json!({ "delta": delta }),
        ))
        .await
        .expect("记录推理摘要增量");
    }

    assert!(matches!(rx.recv().await, Some(UiEvent::ModelThinkingReady)));
    assert!(rx.try_recv().is_err());
    assert!(model_deltas
        .lock()
        .expect("回答增量缓冲区锁不应中毒")
        .take()
        .is_empty());
    assert_eq!(
        thinking_deltas
            .lock()
            .expect("推理摘要增量缓冲区锁不应中毒")
            .take(),
        "检查边界"
    );
}

/// 工具输出事件应保留调用 ID、输出流和文本，供消息状态机增量更新。
#[tokio::test]
async fn channel_event_sink_forwards_tool_output_delta() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _model_deltas, _thinking_deltas) = ChannelEventSink::new(tx);
    let output = ToolOutputDelta {
        call_id: "call-live".into(),
        stream: agent_tool::ToolOutputStream::Stderr,
        delta: "Compiling agent-core\n".into(),
    };

    sink.record(&AgentEvent::new(
        "run-test",
        AgentEventKind::ToolOutputDelta,
        0,
        serde_json::to_value(&output).expect("应序列化工具输出"),
    ))
    .await
    .expect("应记录工具输出");

    let Some(UiEvent::ToolOutputDelta(received)) = rx.recv().await else {
        panic!("应收到工具输出 UI 事件");
    };
    assert_eq!(received, output);
}

/// 验证成功轮次创建并更新同一稳定会话，且每次保存都会推进 revision。
#[tokio::test]
async fn successful_runs_persist_with_cas_revision() {
    let (gateway, options) = build_demo_gateway();
    let agent = Agent::new(gateway, options);
    let store = MemorySessionStore::new();
    let record = SessionRecord::new(
        SessionId::new("stable-session").expect("创建稳定测试会话标识"),
        Session::new(),
    )
    .expect("创建测试会话记录");

    let first_completion = run_and_persist(&agent, &store, record, "第一轮").await;
    assert!(first_completion.error.is_none());
    assert!(first_completion.run.is_some());
    let first = first_completion.session_record;
    assert_eq!(first.revision, 2);
    assert_eq!(first.title.as_deref(), Some("第一轮"));

    let second_completion = run_and_persist(&agent, &store, first.clone(), "第二轮").await;
    assert!(second_completion.error.is_none());
    assert!(second_completion.run.is_some());
    let second = second_completion.session_record;
    assert_eq!(second.id, first.id);
    assert_eq!(second.revision, 4);
    assert_eq!(second.title.as_deref(), first.title.as_deref());
    assert_eq!(
        store.load(&second.id).await.expect("读取测试会话"),
        Some(second)
    );
}

/// 首次 save 已写入但返回错误时，应通过回读协调后继续模型运行。
#[tokio::test]
async fn reconciles_indeterminate_initial_save_before_running() {
    let (gateway, options) = build_demo_gateway();
    let agent = Agent::new(gateway, options);
    let store = ScriptedSaveStore::new(vec![1], ScriptedSaveFailure::IoAfterCommit);
    let record = SessionRecord::new(
        SessionId::new("initial-after-commit").expect("创建首次协调会话标识"),
        Session::new(),
    )
    .expect("创建首次协调测试会话");

    let completion = run_and_persist(&agent, &store, record, "首次提交需要协调").await;

    assert!(completion.error.is_none());
    assert!(completion.run.is_some());
    assert!(completion.input_committed);
    assert!(completion.queue_may_advance);
    assert_eq!(completion.session_record.revision, 2);
    assert_eq!(
        store
            .load(&completion.session_record.id)
            .await
            .expect("读取首次协调后的会话"),
        Some(completion.session_record)
    );
}

/// 最终 save 已写入但返回错误时，应采用原会话回读结果而不是创建分叉。
#[tokio::test]
async fn reconciles_indeterminate_final_save_without_forking() {
    let (gateway, options) = build_demo_gateway();
    let agent = Agent::new(gateway, options);
    let store = ScriptedSaveStore::new(vec![2], ScriptedSaveFailure::IoAfterCommit);
    let record = SessionRecord::new(
        SessionId::new("final-after-commit").expect("创建最终协调会话标识"),
        Session::new(),
    )
    .expect("创建最终协调测试会话");
    let original_id = record.id.clone();

    let completion = run_and_persist(&agent, &store, record, "最终提交需要协调").await;

    assert!(completion.error.is_none());
    assert!(completion.run.is_some());
    assert_eq!(completion.session_record.id, original_id);
    assert_eq!(completion.session_record.revision, 2);
    assert_eq!(store.list().await.expect("列出协调后的会话").len(), 1);
    assert!(restore_session_messages(&completion.session_record.session)
        .iter()
        .any(|message| matches!(message.kind, MsgKind::Assistant)));
}

/// 最终 CAS 冲突时应把完整回复保存为新会话，并切换到分叉记录。
#[tokio::test]
async fn final_save_conflict_forks_completed_session() {
    let (gateway, options) = build_demo_gateway();
    let agent = Agent::new(gateway, options);
    let store = ScriptedSaveStore::new(vec![2], ScriptedSaveFailure::RevisionConflict);
    let mut record = SessionRecord::new(
        SessionId::new("conflicted-session").expect("创建冲突会话标识"),
        Session::new(),
    )
    .expect("创建冲突测试会话");
    record
        .metadata
        .insert("lucia.project_id".into(), Value::String("project-a".into()));
    let original_id = record.id.clone();

    let completion = run_and_persist(&agent, &store, record, "需要完整回复").await;

    assert!(completion.run.is_some());
    assert!(completion.input_committed);
    assert!(completion.queue_may_advance);
    assert_ne!(completion.session_record.id, original_id);
    assert_eq!(completion.session_record.revision, 1);
    assert_eq!(
        completion.session_record.metadata.get("lucia.project_id"),
        Some(&Value::String("project-a".into()))
    );
    assert!(restore_session_messages(&completion.session_record.session)
        .iter()
        .any(|message| matches!(message.kind, MsgKind::Assistant)));
    assert_eq!(
        store
            .load(&completion.session_record.id)
            .await
            .expect("读取分叉会话"),
        Some(completion.session_record.clone())
    );
    assert!(completion
        .error
        .as_ref()
        .is_some_and(|error| error.to_string().contains("已分叉保存")));
}

/// 最终保存与分叉均失败时应保留 dirty 完成态，并以该 Session 重建界面。
#[tokio::test]
async fn failed_final_and_fork_saves_keep_dirty_completed_session() {
    let (gateway, options) = build_demo_gateway();
    let agent = Agent::new(gateway, options);
    let store = ScriptedSaveStore::new(vec![2, 3], ScriptedSaveFailure::Io);
    let record = SessionRecord::new(
        SessionId::new("dirty-session").expect("创建 dirty 会话标识"),
        Session::new(),
    )
    .expect("创建 dirty 测试会话");

    let completion = run_and_persist(&agent, &store, record, "保留这次回复").await;
    let dirty_record = completion.session_record.clone();
    let expected_assistant = restore_session_messages(&dirty_record.session)
        .into_iter()
        .filter(|message| matches!(message.kind, MsgKind::Assistant))
        .map(|message| message.text)
        .collect::<Vec<_>>();
    assert!(!expected_assistant.is_empty());
    assert!(completion.run.is_some());
    assert!(completion.input_committed);
    assert!(!completion.queue_may_advance);

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.append_model_delta("尚未校准的流式文本");
    let queue_may_advance = app.handle_agent_done(completion);
    let actual_assistant = app
        .messages
        .iter()
        .filter(|message| matches!(message.kind, MsgKind::Assistant))
        .map(|message| message.text.clone())
        .collect::<Vec<_>>();

    assert!(!queue_may_advance);
    assert_eq!(app.session_record, dirty_record);
    assert_eq!(actual_assistant, expected_assistant);
    assert!(app.messages.iter().any(|message| {
        matches!(message.kind, MsgKind::Error)
            && message.text.contains("完整回复已保留在当前内存会话中")
    }));
}

/// FIFO 队首预写失败时不得出队或自动推进，所有待处理输入保持原顺序。
#[test]
fn queued_initial_save_failure_preserves_fifo_order() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let original_record = app.session_record.clone();
    app.queued_inputs = VecDeque::from(["第一条".into(), "第二条".into()]);
    app.messages.extend([
        Msg::new(MsgKind::User, "第一条"),
        Msg::new(MsgKind::User, "第二条"),
    ]);
    app.queued_run_active = true;
    app.running = true;
    let mut attempted_record = original_record.clone();
    attempted_record.session.push_user("第一条");

    let queue_may_advance = app.handle_agent_done(AgentCompletion {
        run: None,
        session_record: attempted_record,
        error: Some(anyhow!("模拟首次保存失败")),
        input_committed: false,
        queue_may_advance: false,
        input: "第一条".into(),
    });

    assert!(!queue_may_advance);
    assert_eq!(app.session_record, original_record);
    assert_eq!(
        app.queued_inputs
            .iter()
            .map(|submission| submission.text.as_str())
            .collect::<Vec<_>>(),
        vec!["第一条", "第二条"]
    );
    assert_eq!(
        app.messages
            .iter()
            .filter(|message| matches!(message.kind, MsgKind::User))
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec!["第一条", "第二条"]
    );
    assert!(app.input.is_empty());
    assert!(!app.queued_run_active);
}

/// 验证运行或保存错误不会替换应用持有的原会话记录。
#[test]
fn failed_completion_preserves_confirmed_session() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let original = app.session_record.clone();

    app.handle_agent_done(AgentCompletion {
        run: None,
        session_record: original.clone(),
        error: Some(anyhow!("模拟运行失败")),
        input_committed: false,
        queue_may_advance: false,
        input: "失败输入".into(),
    });

    assert_eq!(app.session_record, original);
}

/// 原生上下文加载失败必须向界面保留完整错误链。
#[test]
fn failed_completion_shows_context_loader_root_cause() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let original = app.session_record.clone();
    let error = anyhow!("原生摘要模型请求失败").context("上下文加载失败");

    app.handle_agent_done(AgentCompletion {
        run: None,
        session_record: original,
        error: Some(error),
        input_committed: true,
        queue_may_advance: false,
        input: "继续任务".into(),
    });

    let displayed = app
        .messages
        .iter()
        .find(|message| matches!(message.kind, MsgKind::Error))
        .expect("上下文加载失败应显示在界面中");
    assert!(displayed.text.contains("上下文加载失败"));
    assert!(displayed.text.contains("原生摘要模型请求失败"));
}

/// 已提交运行失败时应保留界面分析历史，诊断文本不得写入下一次模型使用的 Session。
#[test]
fn failed_completion_preserves_visible_history_without_persisting_diagnostic() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut persisted = app.session_record.clone();
    persisted.session.push_assistant_text("已完成架构分析");
    app.messages = restore_session_messages(&persisted.session);
    let diagnostic = "上下文加载失败：WASM fuel 耗尽";

    app.handle_agent_done(AgentCompletion {
        run: None,
        session_record: persisted,
        error: Some(anyhow!(diagnostic)),
        input_committed: true,
        queue_may_advance: false,
        input: "继续分析".into(),
    });

    assert!(app
        .messages
        .iter()
        .any(|message| message.text == "已完成架构分析"));
    assert!(app
        .messages
        .iter()
        .any(|message| matches!(message.kind, MsgKind::Error) && message.text == diagnostic));
    assert!(app
        .session_record
        .session
        .messages()
        .iter()
        .all(|message| !message.text_content().contains(diagnostic)));
}

/// 手动编辑拆散引用标签时，失去标签的附件被同步丢弃。
#[test]
fn broken_ref_label_prunes_attachment() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("pic.png", b"png");
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    // 在标签中间插入字符，标签失效
    app.cursor = app.input.len() - 2;
    app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE, None);

    assert!(app.attachments.is_empty());
}

/// Agent 运行中单按 Esc 必须优先请求取消，并保留尚未发送的输入。
#[test]
fn escape_interrupts_running_agent_without_clearing_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.running = true;
    app.input = "尚未发送的引导".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));

    assert!(agent.state().cancel_requested);
    assert!(app.run_stream_cancelled);
    assert_eq!(app.input, "尚未发送的引导");
    assert!(!app.should_quit);
}

/// 用户取消完成时必须保留 Esc 瞬间的可见文本，不得用含迟到内容的 Session 重建消息区。
#[test]
fn cancelled_completion_keeps_frozen_visible_history() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.append_model_delta("取消前可见");
    app.run_stream_cancelled = true;
    app.messages
        .push(Msg::new(MsgKind::Info, "Cancelling the current run..."));
    let mut completed = app.session_record.clone();
    completed.session.push_assistant_text("取消后的迟到正文");

    let queue_may_advance = app.handle_agent_done(AgentCompletion {
        run: Some(AgentRun {
            run_id: "cancelled-run".into(),
            final_text: "取消后的迟到正文".into(),
            steps_used: 1,
            usage: Default::default(),
            session: completed.session.clone(),
            cancelled: true,
        }),
        session_record: completed,
        error: None,
        input_committed: true,
        queue_may_advance: true,
        input: "测试取消".into(),
    });

    assert!(!queue_may_advance);
    assert!(app
        .messages
        .iter()
        .any(|message| message.text == "取消前可见"));
    assert!(app
        .messages
        .iter()
        .all(|message| message.text != "取消后的迟到正文"));
    assert!(app
        .messages
        .iter()
        .any(|message| message.text == "Run cancelled; visible output was frozen"));
}
