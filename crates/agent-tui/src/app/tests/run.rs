//! 运行编排、流式增量与会话持久化的回归测试。

use super::*;

/// 队列与立即发送共用的受控模型：首个请求保持挂起，后续请求立即返回，用通知而非睡眠确定时序。
#[derive(Default)]
struct QueueBlockingModel {
    requests: std::sync::Mutex<Vec<agent_core::ModelRequest>>,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl agent_core::ChatModel for QueueBlockingModel {
    async fn complete(
        &self,
        request: agent_core::ModelRequest,
    ) -> Result<agent_core::ModelResponse> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() == 1
        };
        self.entered.notify_one();
        if first {
            std::future::pending().await
        } else {
            Ok(agent_core::ModelResponse::text("处理完成"))
        }
    }
}

impl agent_core::ProviderAdapter for QueueBlockingModel {
    fn name(&self) -> &'static str {
        "queue-test"
    }
}

/// 构造不产生额外模型/工具事件的 Agent；完成事件仍经真实运行与持久化路径回投。
fn queue_test_agent(model: Arc<QueueBlockingModel>) -> Arc<Agent> {
    let gateway = agent_core::ModelGateway::new();
    gateway.register("queue-test", model).unwrap();
    let mut options = agent_core::AgentOptions::default().with_model_route("queue-test", "test");
    options.stream = false;
    Arc::new(Agent::new(gateway, options))
}

/// 等待单次真实提交收尾，超时说明取消或回投链路失效。
async fn queue_completion(rx: &mut mpsc::UnboundedReceiver<UiEvent>) -> AgentCompletion {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(UiEvent::AgentDone(result)) = rx.recv().await {
                return *result;
            }
        }
    })
    .await
    .expect("提交必须及时收尾")
}

/// 只取用户可见消息，检查同一提交在运行中、取消后与重启恢复后的身份保持一致。
fn visible_user_texts(app: &App) -> Vec<&str> {
    app.messages
        .iter()
        .filter(|message| matches!(message.kind, MsgKind::User))
        .map(|message| message.text.as_str())
        .collect()
}

/// 队首开始发送就显示标准用户气泡；Esc 后仅已发送条目落盘，其他队列不得渗入恢复上下文。
#[tokio::test]
async fn queued_cancel_restart_keeps_context_and_user_bubbles_in_sync() {
    let root = std::env::temp_dir().join(format!("lucia-queue-restart-{}", SessionId::generate()));
    let store = Arc::new(agent_session::FileSessionStore::open(&root).await.unwrap());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    app.session_store = store.clone();
    app.queued_inputs = VecDeque::from([
        "已发送的队首".into(),
        "仍在排队一".into(),
        "仍在排队二".into(),
    ]);
    let model = Arc::new(QueueBlockingModel::default());
    let agent = queue_test_agent(model.clone());
    app.run_next_queued(&agent);
    assert_eq!(visible_user_texts(&app), ["已发送的队首"]);
    assert_eq!(
        app.messages[0].to_lines(false, 60),
        Msg::new(MsgKind::User, "已发送的队首").to_lines(false, 60)
    );
    model.entered.notified().await;
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));
    assert!(!app.handle_agent_done(queue_completion(&mut rx).await));
    assert_eq!(visible_user_texts(&app), ["已发送的队首"]);
    assert_eq!(
        app.queued_inputs
            .iter()
            .map(|input| input.text.as_str())
            .collect::<Vec<_>>(),
        ["仍在排队一", "仍在排队二"]
    );
    let id = app.session_record.id.clone();
    drop(app);
    drop(store);
    let reopened = Arc::new(agent_session::FileSessionStore::open(&root).await.unwrap());
    let record = reopened.load(&id).await.unwrap().unwrap();
    assert_eq!(
        record
            .session
            .messages()
            .iter()
            .filter(|message| message.role == agent_core::MessageRole::User)
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["已发送的队首"]
    );
    let (tx, _) = mpsc::unbounded_channel();
    let restored = App::new(tx, "test".into()).with_persistent_session(reopened, record);
    assert_eq!(visible_user_texts(&restored), ["已发送的队首"]);
    assert!(restored.queued_inputs.is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

/// Ctrl+Enter 打断挂起的模型请求，完整携带附件/引用并在旧记录提交后插队；未选条目不进请求。
#[tokio::test]
async fn immediate_queue_send_interrupts_and_preserves_payload_and_order() {
    use crate::session::conversation::{PendingRef, UserSubmission};
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    let model = Arc::new(QueueBlockingModel::default());
    let agent = queue_test_agent(model.clone());
    app.input = "原始任务".into();
    app.submit(&agent);
    model.entered.notified().await;
    let priority = UserSubmission {
        text: "优先处理 [IMAGE#1] [REF#1]".into(),
        attachments: vec![PendingAttachment {
            label: "[IMAGE#1]".into(),
            name: "test.png".into(),
            media_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
            is_image: true,
            pasted_text: None,
            source_path: None,
        }],
        refs: vec![PendingRef {
            label: "[REF#1]".into(),
            provider: "test".into(),
            id: "ref".into(),
            title: "来源".into(),
            body: "完整引用正文".into(),
        }],
    };
    app.queued_inputs = VecDeque::from(["等待一".into(), priority.clone(), "等待二".into()]);
    app.queue_focus = Some(1);
    app.input = "不应覆盖的草稿".into();
    app.handle_queue_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));
    assert!(app.run_stream_cancelled);
    assert_eq!(app.immediate_input, Some(priority));
    assert_eq!(app.input, "不应覆盖的草稿");
    assert_eq!(visible_user_texts(&app), ["原始任务"]);
    let completion = queue_completion(&mut rx).await;
    assert!(completion.run.as_ref().unwrap().cancelled);
    assert!(app.handle_agent_done(completion));
    app.run_next_queued(&agent);
    assert_eq!(
        visible_user_texts(&app),
        ["原始任务", "优先处理 [IMAGE#1] [REF#1]"]
    );
    assert!(!app.handle_agent_done(queue_completion(&mut rx).await));
    assert!(app.queue_paused, "插队仅发送指定项，普通队列不能自动恢复");
    app.run_next_queued(&agent);
    assert!(!app.running);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let latest = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == agent_core::MessageRole::User)
        .unwrap();
    assert!(latest.text_content().contains("完整引用正文"));
    assert!(latest
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { data, .. } if data == "aW1hZ2U=")));
    assert!(!format!("{:?}", requests[1].messages).contains("等待一"));
    assert_eq!(
        app.queued_inputs
            .iter()
            .map(|input| input.text.as_str())
            .collect::<Vec<_>>(),
        ["等待一", "等待二"]
    );
    assert_eq!(app.input, "不应覆盖的草稿");
    assert_eq!(agent.control().pending_steering(), 0);
}

/// 指定修订的保存屏障；区分首存与最终保存，验证取消不打断 CAS 协调。
#[derive(Default)]
struct QueuePausedStore {
    inner: MemorySessionStore,
    pause_revision: u64,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl SessionStore for QueuePausedStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        self.inner.load(id).await
    }
    async fn save(
        &self,
        record: SessionRecord,
        expected: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        if record.revision == self.pause_revision {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.save(record, expected).await
    }
    async fn delete(&self, id: &SessionId, expected: u64) -> Result<(), SessionStoreError> {
        self.inner.delete(id, expected).await
    }
    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        self.inner.list().await
    }
}

/// Esc 在保存阶段到达也不能被 Core 的运行初始化覆盖，更不能继续派发后续队列。
#[tokio::test]
async fn cancel_during_initial_save_does_not_start_model_or_drain_queue() {
    let store = Arc::new(QueuePausedStore::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    app.session_store = store.clone();
    app.queued_inputs = VecDeque::from(["保存中的输入".into(), "尚未发送".into()]);
    let model = Arc::new(QueueBlockingModel::default());
    let agent = queue_test_agent(model.clone());
    app.run_next_queued(&agent);
    store.entered.notified().await;
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));
    store.release.notify_one();
    let completion = queue_completion(&mut rx).await;
    assert!(completion.input_committed);
    assert!(completion.run.is_none());
    assert!(!app.handle_agent_done(completion));
    assert!(model.requests.lock().unwrap().is_empty());
    assert_eq!(visible_user_texts(&app), ["保存中的输入"]);
    assert_eq!(app.queued_inputs.front().unwrap().text, "尚未发送");
    let saved = store.load(&app.session_record.id).await.unwrap().unwrap();
    assert_eq!(
        restore_session_messages(&saved.session)
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["保存中的输入"]
    );
}

/// 优先发送必须等待旧轮最终 CAS 完成；不能在 Core 已停止、保存仍阻塞时抢用旧 revision。
#[tokio::test]
async fn immediate_send_waits_for_previous_final_save() {
    let store = Arc::new(QueuePausedStore {
        pause_revision: 1,
        ..QueuePausedStore::default()
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    app.session_store = store.clone();
    let model = Arc::new(QueueBlockingModel::default());
    let agent = queue_test_agent(model.clone());
    app.input = "旧任务".into();
    app.submit(&agent);
    model.entered.notified().await;
    app.input = "优先消息".into();
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));
    store.entered.notified().await;
    assert!(app.running);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let before = store.load(&app.session_record.id).await.unwrap().unwrap();
    assert_eq!(before.revision, 1);
    assert!(!format!("{:?}", before.session).contains("优先消息"));
    assert!(rx.try_recv().is_err());
    store.release.notify_one();
    assert!(app.handle_agent_done(queue_completion(&mut rx).await));
    assert_eq!(app.session_record.revision, 2);
    app.run_next_queued(&agent);
    assert!(!app.handle_agent_done(queue_completion(&mut rx).await));
    assert_eq!(app.session_record.revision, 4);
    assert_eq!(
        store.list().await.unwrap().len(),
        1,
        "不能因抢先预写产生分叉"
    );
    assert_eq!(visible_user_texts(&app), ["旧任务", "优先消息"]);
}

/// 插队等待收尾期间再次 Esc 会把优先消息退回队列；连续 Ctrl+Enter 不覆盖第一条或新草稿。
#[tokio::test]
async fn cancelling_immediate_send_pauses_it_without_hidden_context() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    let model = Arc::new(QueueBlockingModel::default());
    let agent = queue_test_agent(model.clone());
    app.input = "原始任务".into();
    app.submit(&agent);
    model.entered.notified().await;
    app.queued_inputs.push_back("普通排队".into());
    app.input = "优先任务".into();
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));
    app.input = "继续编辑的草稿".into();
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));
    assert_eq!(app.input, "继续编辑的草稿");
    assert_eq!(app.immediate_input.as_ref().unwrap().text, "优先任务");
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));
    assert!(app.immediate_input.is_none());
    assert!(!app.handle_agent_done(queue_completion(&mut rx).await));
    assert_eq!(
        app.queued_inputs
            .iter()
            .map(|input| input.text.as_str())
            .collect::<Vec<_>>(),
        ["优先任务", "普通排队"]
    );
    assert_eq!(visible_user_texts(&app), ["原始任务"]);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert!(!format!("{:?}", app.session_record.session).contains("优先任务"));
}

/// 首存失败只撤销本次用户气泡，队列与期间编辑的新草稿都不能丢失。
#[tokio::test]
async fn failed_queue_submission_rolls_back_its_bubble_only() {
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    for from_queue in [true, false] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "test".into());
        app.session_store = Arc::new(ScriptedSaveStore::new(vec![1], ScriptedSaveFailure::Io));
        app.messages.push(Msg::new(MsgKind::User, "已有消息"));
        app.queued_inputs.push_back("发送失败".into());
        if from_queue {
            app.run_next_queued(&agent);
        } else {
            app.queue_focus = Some(0);
            app.send_selected_queued_immediate(Some(&agent));
        }
        app.input = "后来编辑的草稿".into();
        assert_eq!(visible_user_texts(&app), ["已有消息", "发送失败"]);
        assert!(!app.handle_agent_done(queue_completion(&mut rx).await));
        assert_eq!(visible_user_texts(&app), ["已有消息"]);
        assert_eq!(app.queued_inputs.front().unwrap().text, "发送失败");
        assert_eq!(app.input, "后来编辑的草稿");
    }
}

/// 选中正在发送的队首不能重复插队；模型未就绪、已有优先消息时队列和草稿保持原样。
#[test]
fn immediate_queue_send_rejects_unavailable_or_active_targets() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.running = true;
    app.queued_run_active = true;
    app.queued_inputs = VecDeque::from(["正在发送".into(), "待发送".into()]);
    app.input = "草稿".into();
    for (index, available) in [(0, true), (1, false)] {
        app.queue_focus = Some(index);
        app.handle_queue_key(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
            available.then_some(&agent),
        );
        assert!(app.immediate_input.is_none());
        assert!(!app.run_stream_cancelled);
        assert_eq!(app.queued_inputs.len(), 2);
        assert_eq!(app.input, "草稿");
    }
    app.queue_focus = Some(1);
    app.handle_queue_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));
    assert_eq!(app.queued_inputs.front().unwrap().text, "正在发送");
    assert_eq!(app.immediate_input.as_ref().unwrap().text, "待发送");
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&agent));
    assert!(app.immediate_input.is_none());
    assert_eq!(
        app.queued_inputs
            .iter()
            .map(|input| input.text.as_str())
            .collect::<Vec<_>>(),
        ["正在发送", "待发送"]
    );
}

/// 已取消的普通队列不会被任意后续完成事件恢复；用户显式 Enter 才恢复 FIFO。
#[tokio::test]
async fn paused_queue_resumes_only_after_explicit_normal_send() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "test".into());
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.queued_inputs = VecDeque::from(["等待一".into(), "等待二".into()]);
    app.queue_paused = true;
    app.run_next_queued(&agent);
    assert!(!app.running);
    app.queue_focus = Some(1);
    app.handle_queue_key(KeyCode::Enter, KeyModifiers::CONTROL, Some(&agent));
    assert!(!app.handle_agent_done(queue_completion(&mut rx).await));
    assert!(app.queue_paused, "空闲时 Ctrl+Enter 也只发送选中项");
    assert_eq!(app.queued_inputs.front().unwrap().text, "等待一");
    app.queued_inputs.push_back("等待三".into());
    app.queue_focus = Some(0);
    app.handle_queue_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));
    assert!(!app.queue_paused);
    assert!(app.handle_agent_done(queue_completion(&mut rx).await));
    app.run_next_queued(&agent);
    assert_eq!(visible_user_texts(&app), ["等待二", "等待一", "等待三"]);
    app.handle_agent_done(queue_completion(&mut rx).await);
    assert!(app.queued_inputs.is_empty());
}

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
    assert!(
        app.messages
            .iter()
            .all(|message| !matches!(message.kind, MsgKind::User)),
        "待发送消息只显示在输入框上方的排队面板"
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

/// 运行完成后不再向对话插入 token 统计，避免打断正文阅读。
#[test]
fn completed_run_does_not_display_prompt_cache_usage() {
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

    assert!(!app.messages.iter().any(|message| {
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
    let (sink, model_deltas, _thinking_deltas, _tool_deltas) = ChannelEventSink::new(tx);
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

/// UI 延迟消费时，不同模型轮次必须保持正文—工具—正文顺序，不能合进同一缓冲批次。
#[tokio::test]
async fn channel_event_sink_separates_text_at_model_and_tool_boundaries() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, model_deltas, _, _) = ChannelEventSink::new(tx);
    let call = ToolCall::new("edit-boundary", "edit_file", json!({"path":"a.rs"}));
    for (kind, step, payload) in [
        (AgentEventKind::ModelRequest, 0, json!({})),
        (
            AgentEventKind::ModelTextDelta,
            0,
            json!({"delta":"先编辑。"}),
        ),
        (
            AgentEventKind::ToolStarted,
            0,
            serde_json::to_value(&call).unwrap(),
        ),
        (AgentEventKind::ModelRequest, 1, json!({})),
        (
            AgentEventKind::ModelTextDelta,
            1,
            json!({"delta":"```rust\nfn done() {}\n```"}),
        ),
    ] {
        sink.record(&AgentEvent::new("batched", kind, step, payload))
            .await
            .unwrap();
    }
    let (tx, _) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    while let Ok(event) = rx.try_recv() {
        match event {
            UiEvent::ModelStarted => app.start_model_response(),
            UiEvent::ModelTextReady => app.queue_model_delta(&model_deltas.lock().unwrap().take()),
            UiEvent::ToolStarted(call) => {
                app.start_model_response();
                assert!(app.streaming_message.is_none());
                app.messages.push(Msg::tool_started(call));
            }
            _ => {}
        }
    }
    app.flush_pending_stream();
    assert_eq!(app.messages.len(), 3);
    assert_eq!(app.messages[0].text, "先编辑。");
    assert!(app.messages[1].is_tool());
    assert_eq!(app.messages[2].text, "```rust\nfn done() {}\n```");
}

/// 慢速消费者不能在第二条工具消息创建前提前取走它的输出。
#[tokio::test]
async fn tool_output_batches_do_not_cross_call_start_events() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _, _, output) = ChannelEventSink::new(tx);
    for id in ["first", "second"] {
        let call = ToolCall::new(id, "shell", json!({"command":"build"}));
        sink.record(&AgentEvent::new(
            "tools",
            AgentEventKind::ToolStarted,
            0,
            serde_json::to_value(call).unwrap(),
        ))
        .await
        .unwrap();
        sink.record(&AgentEvent::new(
            "tools",
            AgentEventKind::ToolOutputDelta,
            0,
            json!({"call_id":id,"stream":"stdout","delta":format!("{id}\n")}),
        ))
        .await
        .unwrap();
    }
    let mut messages: Vec<Msg> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            UiEvent::ToolStarted(call) => messages.push(Msg::tool_started(call)),
            UiEvent::ToolOutputReady => {
                for delta in output.lock().unwrap().take() {
                    let message = messages
                        .iter_mut()
                        .find(|msg| msg.tool_call_id() == Some(delta.call_id.as_str()))
                        .expect("输出不得提前于工具开始事件");
                    message.append_tool_stream(delta.stream, &delta.delta);
                }
            }
            _ => {}
        }
    }
    assert_eq!(messages[0].tool_output, "first\n");
    assert_eq!(messages[1].tool_output, "second\n");
}

/// 显式无展示的观察事件不产生额外时间线行，旧事件和新增字段仍保持兼容。
#[tokio::test]
async fn extension_without_presentation_does_not_duplicate_tool_component() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _, _, _) = ChannelEventSink::new(tx);
    sink.record(&AgentEvent::new(
        "extension",
        AgentEventKind::Extension,
        0,
        json!({
            "source":{"type":"plugin","id":"example"},
            "name":"example.message.sent", "data":{"text":"已由工具组件显示"},
            "presentation":null, "future_field":true
        }),
    ))
    .await
    .unwrap();
    assert!(rx.try_recv().is_err(), "观察事件不能重复显示正文或协议名");

    sink.record(&AgentEvent::new(
        "legacy",
        AgentEventKind::Extension,
        0,
        json!({"name":"legacy.notice"}),
    ))
    .await
    .unwrap();
    assert!(
        matches!(rx.try_recv(), Ok(UiEvent::Extension { text, .. }) if text == "legacy.notice")
    );

    sink.record(&AgentEvent::new(
        "extension",
        AgentEventKind::Extension,
        0,
        json!({
            "name":"example.notice", "presentation":{
                "target":"main_event_list", "text":"显式展示", "future_field":true
            }
        }),
    ))
    .await
    .unwrap();
    assert!(matches!(rx.try_recv(), Ok(UiEvent::Extension { text, .. }) if text == "显式展示"));
}

/// 扩展展示只读取通用 presentation，不解析插件 payload；多行与终端控制序列安全呈现。
#[tokio::test]
async fn extension_presentation_preserves_multiline_text() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _, _, _) = ChannelEventSink::new(tx);
    sink.record(&AgentEvent::new("extension", AgentEventKind::Extension, 0, json!({
        "name":"example.message.sent", "data":{"text":"不应显示"},
        "presentation":{"target":"main_event_list","text":"@审查员: 第一行\n第二行 👩‍💻\n\u{001b}[36m第三行\u{001b}[0m"}
    }))).await.unwrap();
    let Some(UiEvent::Extension {
        text,
        color,
        divider,
    }) = rx.recv().await
    else {
        panic!("缺少展示事件");
    };
    let msg = Msg::extension(text, color, divider);
    let lines = msg.to_lines(false, 40);
    assert_eq!(lines[0].to_string(), "  @审查员: 第一行");
    assert_eq!(lines[1].to_string(), "  第二行 👩‍💻");
    assert_eq!(lines[2].to_string(), "  第三行");
    for width in [12, 40, 80] {
        let mut layout = crate::ui::transcript_layout::TranscriptLayout::default();
        let height = layout.sync(std::slice::from_ref(&msg), None, width);
        assert!(height >= 4);
        let buffer = layout.render_range(
            ratatui::layout::Rect::new(0, 0, width, 20),
            std::slice::from_ref(&msg),
            None,
            0,
            20,
        );
        assert!(buffer
            .content
            .iter()
            .all(|cell| !cell.symbol().chars().any(char::is_control)));
    }
}

/// 新请求的分类快照通过事件边界传递，缺少该字段的旧请求仍可启动显示。
#[tokio::test]
async fn channel_event_sink_accepts_context_snapshot_and_legacy_requests() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _, _, _) = ChannelEventSink::new(tx);
    let event = AgentEvent::new(
        "context",
        AgentEventKind::ModelRequest,
        0,
        json!({"context": {"tokens": {"system": 25, "tools": 10},
            "attachments": 0, "messages": 2, "tools": 1}}),
    );
    sink.record(&event).await.unwrap();
    let Some(UiEvent::ContextSnapshot(snapshot)) = rx.try_recv().ok() else {
        panic!("模型请求应提供上下文快照");
    };
    assert_eq!(snapshot.tokens["system"], 25);
    assert!(matches!(rx.try_recv(), Ok(UiEvent::ModelStarted)));
    let legacy = AgentEvent::new("context", AgentEventKind::ModelRequest, 0, json!({}));
    sink.record(&legacy).await.unwrap();
    assert!(matches!(rx.try_recv(), Ok(UiEvent::ModelStarted)));
}

/// 模型用量必须按分项转发，供底栏分别计算上下文规模与缓存命中率。
#[tokio::test]
async fn channel_event_sink_forwards_prompt_cache_usage_breakdown() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _model_deltas, _thinking_deltas, _tool_deltas) = ChannelEventSink::new(tx);
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
    let (sink, model_deltas, thinking_deltas, _tool_deltas) = ChannelEventSink::new(tx);
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

/// 工具输出应按共享缓冲区合批，一个 chunk 不再单独触发一次整帧重绘。
///
/// 命令狂打日志时事件率可到每秒数千；逐个转发等于逐个整帧重绘，是能把渲染线程打满的
/// 唯一路径。这里断言：连发三段只产生**一次**就绪通知，且取出后顺序与内容完整。
#[tokio::test]
async fn channel_event_sink_batches_tool_output_deltas() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _model_deltas, _thinking_deltas, tool_deltas) = ChannelEventSink::new(tx);
    let outputs = [
        ToolOutputDelta {
            call_id: "call-live".into(),
            stream: agent_tool::ToolOutputStream::Stderr,
            delta: "Compiling agent-core\n".into(),
        },
        ToolOutputDelta {
            call_id: "call-live".into(),
            stream: agent_tool::ToolOutputStream::Stderr,
            delta: "Compiling lucia\n".into(),
        },
        ToolOutputDelta {
            call_id: "call-other".into(),
            stream: agent_tool::ToolOutputStream::Stdout,
            delta: "done\n".into(),
        },
    ];

    for output in &outputs {
        sink.record(&AgentEvent::new(
            "run-test",
            AgentEventKind::ToolOutputDelta,
            0,
            serde_json::to_value(output).expect("应序列化工具输出"),
        ))
        .await
        .expect("应记录工具输出");
    }

    let Some(UiEvent::ToolOutputReady) = rx.recv().await else {
        panic!("应收到工具输出就绪事件");
    };
    assert!(rx.try_recv().is_err(), "同一次合批不应产生第二个就绪事件");

    let received = tool_deltas.lock().expect("工具输出缓冲区锁不应中毒").take();
    assert_eq!(received, outputs, "合批必须保持到达顺序与内容完整");

    // 取走之后，下一段输出会重新触发就绪通知。
    sink.record(&AgentEvent::new(
        "run-test",
        AgentEventKind::ToolOutputDelta,
        0,
        serde_json::to_value(&outputs[0]).expect("应序列化工具输出"),
    ))
    .await
    .expect("应记录工具输出");
    assert!(matches!(rx.recv().await, Some(UiEvent::ToolOutputReady)));
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

    let first_completion = run_and_persist(&agent, &store, record, "第一轮", None).await;
    assert!(first_completion.error.is_none());
    assert!(first_completion.run.is_some());
    let first = first_completion.session_record;
    assert_eq!(first.revision, 2);
    assert_eq!(first.title.as_deref(), Some("第一轮"));

    let second_completion = run_and_persist(&agent, &store, first.clone(), "第二轮", None).await;
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

    let completion = run_and_persist(&agent, &store, record, "首次提交需要协调", None).await;

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

    let completion = run_and_persist(&agent, &store, record, "最终提交需要协调", None).await;

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

    let completion = run_and_persist(&agent, &store, record, "需要完整回复", None).await;

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

    let completion = run_and_persist(&agent, &store, record, "保留这次回复", None).await;
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

/// 用虚拟时钟推进一帧揭示。
///
/// 揭示节奏由墙钟决定，测试无法等真实时间，因此每次显式给出"现在是第几帧"，
/// 相邻两帧间隔正好一个 [`crate::theme::REVEAL_FRAME_INTERVAL_MS`]。
fn reveal_frame(app: &mut App, frame: u64) -> usize {
    let now = std::time::Instant::now()
        + std::time::Duration::from_millis(frame * crate::theme::REVEAL_FRAME_INTERVAL_MS);
    app.reveal_pending_stream_at(now)
}

/// 小段增量（正常逐 token 流式）也走统一节拍：每帧一两个字，而不是一次全出。
///
/// 这是"打字机"而不是"喷射"的关键：到达量再小，屏幕上也按帧匀速吐字。
#[test]
fn trickle_deltas_are_revealed_at_the_base_pace() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    let payload = "正在读取配置文件…";

    app.queue_model_delta(payload);
    let mut per_frame = Vec::new();
    for frame in 1..=32 {
        if !app.has_pending_stream() {
            break;
        }
        per_frame.push(reveal_frame(&mut app, frame));
    }

    assert!(!app.has_pending_stream(), "小段文本应在若干帧内铺完");
    assert!(
        per_frame.iter().all(|chars| *chars <= 2),
        "单帧揭示量必须是一两个字，不能整段蹦出：{per_frame:?}"
    );
    assert!(
        per_frame.iter().sum::<usize>() == payload.chars().count(),
        "逐帧揭示量之和必须等于总字符数：{per_frame:?}"
    );
}

/// 网络一次推来的整段文本会被摊平成匀速吐字，而不是一帧整块蹦出。
///
/// 断言的重点不是"分了多少帧"，而是**相邻帧之间没有突跳**：喷射感来自忽大忽小的单帧
/// 跳跃（一次几十个字、下一帧又是零），而不是来自整体偏快。
#[test]
fn burst_text_is_revealed_at_a_uniform_pace() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    let payload = "字".repeat(500);

    app.queue_model_delta(&payload);
    let mut per_frame = Vec::new();
    for frame in 1..=600 {
        if !app.has_pending_stream() {
            break;
        }
        per_frame.push(reveal_frame(&mut app, frame));
    }

    assert!(
        per_frame.len() > 4,
        "整段突发不应在一两帧内全部出现：{per_frame:?}"
    );
    let largest = *per_frame.iter().max().expect("应有节拍帧");
    assert!(
        largest <= 32,
        "单帧不应揭示超过上限速率的字符数：{per_frame:?}"
    );
    // 积压排空时速率先快后慢是允许的（那仍然是连续吐字）；不允许的是相邻帧突变。
    for pair in per_frame.windows(2) {
        assert!(
            pair[0] <= pair[1] + 4,
            "相邻帧揭示量不允许突跳，否则观感仍是喷射：{per_frame:?}"
        );
    }
    let shown = app
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<String>();
    assert_eq!(
        shown.chars().count(),
        payload.chars().count(),
        "字符不能丢失"
    );
}

/// 积压超过背压上限时立即清空超出部分，模型结束时不会还差半屏没显示。
#[test]
fn backlog_beyond_limit_is_flushed_immediately() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    let oversized = "a".repeat(crate::theme::REVEAL_BACKLOG_LIMIT * 2);

    app.queue_model_delta(&oversized);
    let revealed = app.reveal_pending_stream();

    assert!(
        revealed >= crate::theme::REVEAL_BACKLOG_LIMIT,
        "超出背压上限的部分应一次性上屏：{revealed}"
    );
    assert!(
        app.pending_stream.len() <= crate::theme::REVEAL_BACKLOG_LIMIT,
        "剩余积压应回落到上限以内：{}",
        app.pending_stream.len()
    );
}

/// 揭示只能在字符边界切分：切坏多字节字符会渲染成乱码。
#[test]
fn reveal_never_splits_a_multibyte_character() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    // 每个字符占 3 字节，节拍把单帧配额落在字符中间时最容易切坏。
    let payload = "汉".repeat(97);

    app.queue_model_delta(&payload);
    for frame in 1..=64 {
        if !app.has_pending_stream() {
            break;
        }
        reveal_frame(&mut app, frame);
        let shown: String = app.messages.iter().map(|m| m.text.as_str()).collect();
        assert!(
            payload.starts_with(&shown),
            "已显示的文本必须是原文的完整前缀（不允许半个字符）：{shown:?}"
        );
    }
    assert!(!app.has_pending_stream(), "97 个汉字应在几十帧内铺完");
}

/// 工具开始前必须把待揭示文本补完，否则后续揭示的文字会跑到工具卡片上方。
#[test]
fn flush_completes_text_before_the_next_boundary() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    let payload = "字".repeat(400);

    app.queue_model_delta(&payload);
    app.flush_pending_stream();

    assert!(!app.has_pending_stream());
    let shown = app
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<String>();
    assert_eq!(shown, payload);
}

/// 运行已结束时残留缓冲会被丢弃，不会写进下一条消息。
#[test]
fn pending_text_is_dropped_after_the_run_ends() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.queue_model_delta("迟到正文");
    app.running = false;

    assert_eq!(app.reveal_pending_stream(), 0);
    assert!(!app.has_pending_stream());
    assert!(app.messages.is_empty(), "取消后的迟到正文不应上屏");
}

/// 有待揭示文本时 tick 必须重绘，否则打字机停在当前字符等下一个网络事件。
#[test]
fn tick_requires_redraw_while_text_is_pending() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    assert!(app.tick_requires_redraw(), "运行中需要推进动画");

    app.queue_model_delta("待揭示");
    // 运行已结束但缓冲仍有残余：这是 Done 与最后一批揭示之间的防御性状态，
    // 此时必须继续出帧，否则残余文本永远不会上屏。
    app.running = false;
    assert!(app.has_pending_stream());
    assert!(app.tick_requires_redraw(), "有待揭示文本时必须继续出帧");

    // 边界补完会一次清空缓冲（揭示节拍只负责常态，边界必须立即追上）。
    app.flush_pending_stream();
    assert!(!app.tick_requires_redraw(), "残余清空后回到纯事件驱动");
}

// TEMP-BENCH-REMOVE
/// 临时测量：整段 Markdown 在每帧重排的成本。
#[test]
fn tmp_bench_markdown_frame_cost() {
    let mut body = String::new();
    for index in 0..400 {
        body.push_str(&format!(
            "第 {index} 段：流式输出里的一行正文，包含 `code` 与 **粗体**，用来逼近真实回答的排版密度。\n\n"
        ));
    }
    for chars in [2_000usize, 8_000, 20_000, 40_000] {
        let text: String = body.chars().take(chars).collect();
        let started = std::time::Instant::now();
        let rounds = 20;
        let mut lines = 0;
        for _ in 0..rounds {
            lines = markdown_lines(&text, true, 160).len();
        }
        let per = started.elapsed() / rounds;
        eprintln!(
            "BENCH chars={} lines={} per_render={:?}",
            text.chars().count(),
            lines,
            per
        );
    }
}
