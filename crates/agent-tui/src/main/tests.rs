//! Lucia TUI 状态、输入、渲染和会话协调回归测试。

use super::*;
#[cfg(feature = "plugins")]
use agent_plugin_host::ui::{UiNavigationAction, UiViewInstance};
use ratatui::{backend::TestBackend, Terminal};
#[cfg(feature = "plugins")]
use std::{fs, time::SystemTime};

/// 将测试终端缓冲区转换为去除宽字符占位空格的纯文本。
fn render_text(width: u16, height: u16, running: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("创建测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = running;
    app.messages.extend([
        Msg::new(MsgKind::User, "测试消息"),
        Msg::new(MsgKind::Assistant, "测试回复"),
    ]);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染测试界面");
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// 测试存储在指定 save 调用上注入错误，其余操作委托给内存存储。
#[derive(Clone)]
struct ScriptedSaveStore {
    /// 保存成功时使用的真实内存存储。
    inner: MemorySessionStore,
    /// 从一开始计数的 save 调用次数。
    save_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// 需要失败的 save 调用序号。
    failing_calls: Arc<Vec<usize>>,
    /// 注入的错误类型。
    failure: ScriptedSaveFailure,
}

/// 测试所需的保存失败时序与错误类型。
#[derive(Clone, Copy)]
enum ScriptedSaveFailure {
    /// 模拟另一进程抢先更新记录。
    RevisionConflict,
    /// 模拟底层文件写入失败。
    Io,
    /// 模拟 payload 已写入，但提交确认阶段返回 I/O 错误。
    IoAfterCommit,
}

impl ScriptedSaveStore {
    /// 创建按调用序号失败的测试存储。
    fn new(failing_calls: Vec<usize>, failure: ScriptedSaveFailure) -> Self {
        Self {
            inner: MemorySessionStore::new(),
            save_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failing_calls: Arc::new(failing_calls),
            failure,
        }
    }

    /// 构造与当前保存记录匹配的模拟错误。
    fn failure_for(&self, record: &SessionRecord) -> SessionStoreError {
        match self.failure {
            ScriptedSaveFailure::RevisionConflict => SessionStoreError::RevisionConflict {
                id: record.id.clone(),
                expected: Some(record.revision),
                actual: Some(record.revision.saturating_add(1)),
            },
            ScriptedSaveFailure::Io | ScriptedSaveFailure::IoAfterCommit => SessionStoreError::Io {
                operation: "模拟保存会话",
                path: PathBuf::from("scripted-session.json"),
                source: std::io::Error::other("模拟写入失败"),
            },
        }
    }
}

#[async_trait]
impl SessionStore for ScriptedSaveStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        self.inner.load(id).await
    }

    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        let call = self
            .save_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self.failing_calls.contains(&call) {
            let error = self.failure_for(&record);
            if matches!(self.failure, ScriptedSaveFailure::IoAfterCommit) {
                self.inner.save(record, expected_revision).await?;
            }
            return Err(error);
        }
        self.inner.save(record, expected_revision).await
    }

    async fn delete(
        &self,
        id: &SessionId,
        expected_revision: u64,
    ) -> Result<(), SessionStoreError> {
        self.inner.delete(id, expected_revision).await
    }

    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        self.inner.list().await
    }
}

/// 验证常规尺寸下角色标记、输入提示和底部信息行均可见。
#[test]
fn render_shows_visual_hierarchy() {
    let text = render_text(100, 24, false);

    assert!(text.contains("测试模型"), "{text:?}");
    assert!(text.contains("▌测试消息"), "{text:?}");
    assert!(text.contains("●测试回复"), "{text:?}");
    assert!(text.contains("MessageLucia..."), "{text:?}");
    assert!(!text.contains("agentruntime"), "{text:?}");
    assert!(!text.contains("ReAct"), "{text:?}");
}

/// 输入提示应紧跟输入盒上边框，下边框之后立即进入状态栏。
#[test]
fn input_editor_starts_immediately_after_rule() {
    let width = 80;
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("创建输入间距测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染输入间距测试界面");
    let rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let rule_row = rows
        .iter()
        .position(|row| row.chars().filter(|character| *character == '─').count() > 20)
        .expect("输入盒上边框应存在");

    assert!(rows[rule_row + 1].contains("Message Lucia..."));
    // 编辑行之后是输入盒下边框，再往下一行是状态栏。
    assert!(
        rows[rule_row + 2]
            .chars()
            .filter(|character| *character == '─')
            .count()
            > 20
    );
    assert!(!rows[rule_row + 3].trim().is_empty());
}

/// 多行输入框最多展示六个逻辑行，并自动滚动到光标所在的末行。
#[test]
fn multiline_input_renders_six_visible_rows() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建多行输入测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = (1..=8)
        .map(|index| format!("input-line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.cursor = app.input.len();

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染多行输入界面");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(!text.contains("input-line-2"), "{text:?}");
    for index in 3..=8 {
        assert!(text.contains(&format!("input-line-{index}")), "{text:?}");
    }
}

/// Startup activation events render in the footer once, then collapse into a plugin count.
///
/// 启动激活事件应在底部信息栏右侧展示一次，随后收敛为插件数量。
#[cfg(feature = "plugins")]
#[test]
fn plugin_status_shows_startup_details_then_compact_count() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app = app.with_loading_plugins(vec!["mcp".into(), "skill".into()]);
    app.mark_plugin_ready(
        "mcp".into(),
        &[json!({
            "source": {"id": "mcp"},
            "data": {"text": "MCP 插件等待配置"}
        })],
    );
    app.mark_plugin_ready(
        "skill".into(),
        &[json!({
            "source": {"id": "skill"},
            "presentation": {"text": "已加载 1 个 Skill"}
        })],
    );
    app.finish_progressive_plugin_loading();
    assert_eq!(
        app.plugin_status_content(),
        (
            "✓",
            "插件加载完成 · mcp: MCP 插件等待配置 · skill: 已加载 1 个 Skill".into()
        )
    );

    let backend = TestBackend::new(100, 18);
    let mut terminal = Terminal::new(backend).expect("创建插件状态测试终端");
    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染插件启动状态");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(rendered.contains("插件加载完成"), "{rendered:?}");

    for _ in 0..PLUGIN_STATUS_DETAIL_TICKS {
        app.tick_plugin_status();
    }
    assert_eq!(app.plugin_status_content(), ("◈", "2 plugins".into()));
}

/// 渐进加载期间应同时展示剩余插件和已经可用的插件数量。
#[cfg(feature = "plugins")]
#[test]
fn plugin_status_reports_progressive_ready_count() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app =
        App::new(tx, "测试模型".into()).with_loading_plugins(vec!["command".into(), "mcp".into()]);

    app.mark_plugin_ready("command".into(), &[]);

    let (_, status) = app.plugin_status_content();
    assert!(status.contains("mcp"), "{status}");
    assert!(!status.contains("command"), "{status}");
    assert!(status.contains("已就绪 1"), "{status}");
}

/// Partial plugin failures retain successes and remain visible in the compact footer count.
///
/// 单插件失败应保留成功插件，并在紧凑底栏中持续显示失败数量。
#[cfg(feature = "plugins")]
#[test]
fn plugin_status_keeps_partial_successes() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app = app.with_loading_plugins(vec!["skill".into(), "mcp".into()]);
    app.mark_plugin_ready("skill".into(), &[]);
    app.mark_plugin_failed(PluginLoadFailure {
        plugin_id: "mcp".into(),
        reason: "初始化超时".into(),
        blocked_by: Vec::new(),
    });
    app.finish_progressive_plugin_loading();

    let (icon, status) = app.plugin_status_content();
    assert_eq!(icon, "!");
    assert!(status.contains("插件部分加载"), "{status}");
    assert!(status.contains("mcp: 加载失败"), "{status}");
    assert_eq!(app.plugin_status_color(), COLOR_WARNING);

    for _ in 0..PLUGIN_STATUS_DETAIL_TICKS {
        app.tick_plugin_status();
    }
    assert_eq!(app.plugin_status_content(), ("◈", "1 plugins · ✗ 1".into()));
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
    assert_eq!(
        app.messages
            .iter()
            .filter(|message| matches!(message.kind, MsgKind::User))
            .count(),
        2
    );
    let (_, status) = app.plugin_status_content();
    assert!(status.contains("queued 2"), "{status}");

    app.input = "/clear".into();
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, None);
    assert_eq!(app.input, "/clear");
    assert_eq!(app.queued_inputs.len(), 2);
    assert_eq!(
        app.messages
            .iter()
            .filter(|message| matches!(message.kind, MsgKind::User))
            .count(),
        2
    );
}

/// 插件仍在渐进加载时，普通输入应立即启动已有工具快照，不进入等待队列。
#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_loading_does_not_block_ready_agent_input() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app =
        App::new(tx, "测试模型".into()).with_loading_plugins(vec!["mcp".into(), "skill".into()]);
    let (gateway, options) = build_demo_gateway();
    let agent = Arc::new(Agent::new(gateway, options));
    app.input = "立即执行".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&agent));

    assert!(app.running);
    assert!(app.queued_inputs.is_empty());
    assert!(app.input.is_empty());
    assert!(app
        .messages
        .iter()
        .any(|message| matches!(message.kind, MsgKind::User) && message.text == "立即执行"));
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

/// 验证空模型密钥触发演示模式，而非空明文密钥允许构建真实模型运行时。
#[test]
fn model_key_availability_rejects_empty_values() {
    let without_key: AgentRootConfig = toml::from_str(
        r#"
            [model]
            provider = "open-ai"
            model = "test-model"
            api_key = "   "
        "#,
    )
    .expect("解析无密钥测试配置");
    assert!(!configured_model_key_is_available(&without_key));

    let with_key: AgentRootConfig = toml::from_str(
        r#"
            [model]
            provider = "open-ai"
            model = "test-model"
            api_key = "test-key"
        "#,
    )
    .expect("解析有密钥测试配置");
    assert!(configured_model_key_is_available(&with_key));
}

/// 验证运行状态使用 steering 文案，并在窄终端隐藏目录信息。
#[test]
fn render_adapts_to_running_state_and_narrow_width() {
    let text = render_text(60, 16, true);

    assert!(text.contains("Working..."), "{text:?}");
    assert!(text.contains("Steerthecurrentrun..."), "{text:?}");
    assert!(!text.contains("ascnet-lucia"), "{text:?}");
}

/// 验证工具行展示参数与返回内容摘要，且过长内容按显示宽度截断。
#[test]
fn tool_lines_show_args_and_truncated_result() {
    let mut msg = Msg::new(MsgKind::ToolOk, "read_file");
    msg.args = Some(summarize_json(&json!({ "path": "src/main.rs" }), 64));
    msg.result = Some(summarize_json(
        &json!({ "content": "很长的文件内容".repeat(30) }),
        24,
    ));

    let lines = msg.to_lines(false, 80);
    let text: String = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("● read_file(path: src/main.rs)"), "{text:?}");
    assert!(text.contains("⎿ content: 很长的文件内容"), "{text:?}");
    assert!(text.contains('…'), "{text:?}");
}

/// 验证嵌套 JSON 摘要折叠为计数而不是原始序列化。
#[test]
fn summarize_json_folds_nested_structures() {
    let value = json!({
        "path": "src",
        "entries": [{ "name": "lib.rs" }, { "name": "main.rs" }],
        "meta": { "hidden": false }
    });

    let summary = summarize_json(&value, 96);

    assert_eq!(summary, "path: src, entries: [2 项], meta: {…}");
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
    assert_eq!(messages[1].args.as_deref(), Some("path: config.toml"));
    assert!(messages[1]
        .result
        .as_deref()
        .is_some_and(|result| result.contains("配置内容")));
    assert!(matches!(messages[2].kind, MsgKind::Assistant));
    assert_eq!(messages[2].text, "配置已经读取");
}

/// 非 UTF-8 路径即使有损文本相同，也必须映射到不同项目空间。
#[cfg(unix)]
#[test]
fn workspace_id_hashes_raw_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let left = PathBuf::from(OsString::from_vec(vec![b'/', b'p', 0x80]));
    let right = PathBuf::from(OsString::from_vec(vec![b'/', b'p', 0x81]));
    assert_eq!(left.to_string_lossy(), right.to_string_lossy());
    assert_ne!(workspace_project_id(&left), workspace_project_id(&right));
}

/// 普通启动必须创建 Draft，显式参数仍可恢复最近或指定会话。
#[tokio::test]
async fn startup_session_respects_explicit_and_latest_priority() {
    let store = MemorySessionStore::new();
    let older = store
        .save(
            SessionRecord::new(
                SessionId::new("older").expect("创建旧会话 ID"),
                Session::new(),
            )
            .expect("创建旧会话"),
            None,
        )
        .await
        .expect("保存旧会话");
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let newer = store
        .save(
            SessionRecord::new(
                SessionId::new("newer").expect("创建新会话 ID"),
                Session::new(),
            )
            .expect("创建新会话"),
            None,
        )
        .await
        .expect("保存新会话");
    let workspace = WorkspaceContext::capture().expect("捕获测试工作目录");

    let draft = load_startup_session(&store, None, &workspace, false)
        .await
        .expect("创建启动 Draft");
    assert_ne!(draft.id, older.id);
    assert_ne!(draft.id, newer.id);
    assert_eq!(draft.revision, 0);

    let latest = load_startup_session(&store, None, &workspace, true)
        .await
        .expect("恢复最近会话");
    assert_eq!(latest.id, newer.id);

    let explicit = load_startup_session(&store, Some(older.id.as_str()), &workspace, true)
        .await
        .expect("恢复显式会话");
    assert_eq!(explicit.id, older.id);
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

/// 验证 Markdown 表格排版为对齐行：分隔行转为横线，中文列按显示宽度补齐。
#[test]
fn markdown_tables_render_aligned() {
    let lines = markdown_lines(
        "说明\n\n| 模块 | 作用 |\n|---|---|\n| core | Agent 循环 |\n| tool | 工具注册 |\n\n结尾",
        false,
    );
    let rows: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let text = rows.join("\n");

    assert!(text.contains("┼"), "{text:?}");
    assert!(!text.contains("|---"), "{text:?}");
    // 两列数据行应等宽对齐：core 与 tool 后的竖线位置一致。
    let core_row = rows
        .iter()
        .find(|row| row.contains("core"))
        .expect("core 行");
    let tool_row = rows
        .iter()
        .find(|row| row.contains("tool"))
        .expect("tool 行");
    assert_eq!(core_row.find('│'), tool_row.find('│'), "{text:?}");
    assert!(text.contains("结尾"), "{text:?}");
}

/// 验证 Markdown 渲染保留标题与行内代码的强调样式，且不产生任何背景色。
#[test]
fn markdown_renders_emphasis_without_background() {
    let lines = markdown_lines("# 标题\n\n**重点** 与 `代码` 内容", false);

    let no_background = lines.iter().all(|line| {
        line.style.bg.is_none() && line.spans.iter().all(|span| span.style.bg.is_none())
    });
    assert!(no_background);

    let heading = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.contains("标题")))
        .expect("应包含标题行");
    assert!(heading.style.add_modifier.contains(Modifier::BOLD));

    let bold_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("重点"))
        .expect("应包含加粗片段");
    assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
}

/// 验证围栏代码块隐藏围栏、保留语言标签，并且不干扰相邻表格。
#[test]
fn markdown_code_blocks_are_visually_separated() {
    let lines = markdown_lines(
        "```rust\nfn main() {}\n```\n| 列一 | 列二 |\n|---|---|\n| 值一 | 值二 |\n```txt\n未闭合",
        false,
    );
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<String>();

    assert!(!text.contains("```"), "{text:?}");
    assert!(text.contains("▎ fn main() {}  rust"), "{text:?}");
    assert!(text.contains('┼'), "{text:?}");
    assert!(text.contains("▎ 未闭合  txt"), "{text:?}");
}

/// 验证流式增量与运行结束不会打断用户的手动滚动位置。
#[test]
fn manual_scroll_survives_streaming_updates() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = true;
    app.last_max_scroll = 40;
    app.scroll_up(10);
    assert_eq!(app.scroll, Some(30));

    app.append_model_delta("新的输出");
    let saved_record = app.session_record.clone();
    app.handle_agent_done(AgentCompletion {
        run: Some(AgentRun {
            run_id: "run-scroll".into(),
            final_text: "完成".into(),
            steps_used: 1,
            usage: Default::default(),
            session: Session::new(),
            cancelled: false,
        }),
        session_record: saved_record,
        error: None,
        input_committed: true,
        queue_may_advance: true,
        input: "测试".into(),
    });

    assert_eq!(app.scroll, Some(30));
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

/// 高频模型增量只排队一个 UI 通知，消费后才能创建下一次通知。
#[tokio::test]
async fn channel_event_sink_coalesces_model_text_notifications() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, model_deltas) = ChannelEventSink::new(tx);
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

/// 上下文加载失败必须向界面保留插件或 WASM 层的完整错误链。
#[test]
fn failed_completion_shows_context_loader_root_cause() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let original = app.session_record.clone();
    let error = anyhow!("插件 `context` context load failed: guest trap").context("上下文加载失败");

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
    assert!(displayed.text.contains("guest trap"));
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

/// Explicit manifests override same-ID official plugins while retaining other defaults.
///
/// 显式插件应覆盖同 ID 官方插件，同时保留其他官方插件。
#[cfg(feature = "plugins")]
#[test]
fn explicit_plugin_manifest_overrides_official_manifest() {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("生成测试时间戳")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "lucia-official-plugin-merge-{}-{nonce}",
        std::process::id()
    ));
    let explicit = root.join("explicit.toml");
    let official_same = root.join("official-same.toml");
    let official_other = root.join("official-other.toml");
    fs::create_dir_all(&root).expect("创建插件合并测试目录");
    let manifest = |id: &str, name: &str| {
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.6.0\"\nwasm = \"plugin.wasm\"\n"
        )
    };
    fs::write(&explicit, manifest("mcp", "显式 MCP")).expect("写入显式插件 manifest");
    fs::write(&official_same, manifest("mcp", "官方 MCP")).expect("写入同 ID 官方插件 manifest");
    fs::write(&official_other, manifest("skill", "官方 Skill")).expect("写入其他官方插件 manifest");

    let mut manifests = vec![explicit.clone()];
    merge_official_plugin_manifests(&mut manifests, vec![official_same, official_other.clone()]);

    assert_eq!(manifests, vec![explicit, official_other]);
    fs::remove_dir_all(root).expect("清理插件合并测试目录");
}

/// Invalid manifests remain visible to background loading instead of blocking first paint.
///
/// 无效 manifest 应交给后台加载器报告，不应阻断 TUI 首帧。
#[cfg(feature = "plugins")]
#[test]
fn invalid_plugin_manifest_does_not_block_startup_labels() {
    let invalid = PathBuf::from("/tmp/lucia-invalid-plugin.toml");
    let labels = plugin_manifest_ids(std::slice::from_ref(&invalid));

    assert_eq!(labels, vec!["lucia-invalid-plugin.toml"]);
}

/// 创建测试插件视图，覆盖停靠、对话框和焦点路由测试。
#[cfg(feature = "plugins")]
fn test_plugin_view(placement: UiPlacement, title: &str) -> PluginViewState {
    PluginViewState {
        declaration: UiDeclaration {
            plugin_id: "test-plugin".into(),
            view_id: format!("{placement:?}").to_ascii_lowercase(),
            title: title.into(),
            placement,
            size: agent_plugin_host::ui::UiSize {
                width: Some(24),
                height: Some(8),
            },
            focusable: true,
        },
        frame: Some(PluginUiFrame {
            view_id: format!("{placement:?}").to_ascii_lowercase(),
            visible: true,
            lines: vec![UiLine {
                spans: vec![UiSpan {
                    text: "插件内容".into(),
                    style: UiStyle::default(),
                }],
            }],
        }),
        area: Rect::default(),
    }
}

/// 隐藏的 Command Dialog 只接受定向刷新，不参与周期性跨 WASM 轮询。
#[test]
#[cfg(feature = "plugins")]
fn hidden_command_dialog_is_excluded_from_periodic_refresh() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::Dialog, "会话");
    view.declaration.plugin_id = PROVIDER_PLUGIN_ID.into();
    view.declaration.view_id = SESSION_DIALOG_VIEW.into();
    view.frame.as_mut().expect("测试视图应包含帧").visible = false;
    app.plugin_views.push(view);

    assert!(app.periodic_plugin_render_requests().is_empty());
    assert_eq!(app.plugin_render_requests().len(), 1);
}

/// 验证右侧插槽与主界面可以同时渲染，且插件获得实际内容尺寸。
#[test]
#[cfg(feature = "plugins")]
fn plugin_dock_renders_inside_main_ui() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages.push(Msg::new(MsgKind::User, "主界面内容"));
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Right, "右侧插件"));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染带插件的界面");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    assert!(text.contains("主界面内容"), "{text:?}");
    assert!(text.contains("右侧插件"), "{text:?}");
    assert!(text.contains("插件内容"), "{text:?}");
    assert!(app.plugin_views[0].area.width > 0);
}

/// 验证可见对话框覆盖主界面并优先接收按键。
#[test]
#[cfg(feature = "plugins")]
fn plugin_dialog_is_modal() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Dialog, "插件对话框"));

    let route = app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE);
    let PluginKeyRoute::Input(input) = route else {
        panic!("对话框应优先接收输入");
    };

    assert_eq!(input.plugin_id, "test-plugin");
    assert_eq!(input.view_id, "dialog");
    assert!(matches!(
        input.event,
        UiInputEvent::Key { ref code, .. } if code == "enter"
    ));
}

/// A dynamic subview replaces the main view while preserving instance input routing.
/// 动态子视图应替换主视图，并保留实例化输入路由。
#[test]
#[cfg(feature = "plugins")]
fn plugin_subview_replaces_main_view_and_routes_instance_input() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("创建测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.messages
        .push(Msg::new(MsgKind::User, "main-view-content"));
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Subview, "Agent 详情"));
    app.apply_view_navigation(
        "test-plugin",
        UiNavigationRequest {
            request_id: "open-agent-1".into(),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: "subview".into(),
                    instance_id: "agent-1".into(),
                    title: Some("Reviewer Agent".into()),
                },
            },
        },
    )
    .expect("打开插件子视图");
    app.update_plugin_frame(
        "test-plugin",
        Some("agent-1"),
        PluginUiFrame {
            view_id: "subview".into(),
            visible: true,
            lines: vec![UiLine {
                spans: vec![UiSpan {
                    text: "subview-content".into(),
                    style: UiStyle::default(),
                }],
            }],
        },
    );

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染子视图");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Reviewer Agent"), "{rendered:?}");
    assert!(rendered.contains("subview-content"), "{rendered:?}");
    assert!(!rendered.contains("main-view-content"), "{rendered:?}");

    let PluginKeyRoute::Input(input) = app.route_plugin_key(KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("子视图应接收键盘输入");
    };
    assert_eq!(input.instance_id.as_deref(), Some("agent-1"));
    assert_eq!(input.view_id, "subview");

    assert!(matches!(
        app.route_plugin_key(KeyCode::Esc, KeyModifiers::NONE),
        PluginKeyRoute::Consumed
    ));
    assert!(app.view_stack.is_main());
}

/// 验证 Tab 在主输入区与可聚焦停靠视图之间循环。
#[test]
#[cfg(feature = "plugins")]
fn tab_cycles_plugin_focus() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.plugin_views
        .push(test_plugin_view(UiPlacement::Left, "左侧插件"));

    assert!(matches!(
        app.route_plugin_key(KeyCode::Tab, KeyModifiers::NONE),
        PluginKeyRoute::Consumed
    ));
    assert_eq!(app.plugin_focus, Some(0));
    assert!(matches!(
        app.route_plugin_key(KeyCode::Tab, KeyModifiers::NONE),
        PluginKeyRoute::Consumed
    ));
    assert_eq!(app.plugin_focus, None);
}

/// 验证参数候选使用 Provider 给出的 UTF-8 字节区间替换中文输入。
#[test]
#[cfg(feature = "plugins")]
fn command_completion_replaces_utf8_argument_range() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/deploy 北京".into();
    app.cursor = app.input.len();
    app.command_selection = 1;
    app.command_completion = Some(ResolvedCommandCompletion {
        source_input: app.input.clone(),
        source_cursor: app.cursor,
        context: CompletionContext {
            command: "deploy".into(),
            argument: "region".into(),
            argument_index: 0,
            replacement_start: 8,
            replacement_end: u32::try_from(app.input.len()).expect("输入长度应可转换"),
            prefix: "北京".into(),
        },
        items: vec![
            CompletionItem {
                label: "北京".into(),
                insert_text: "北京".into(),
                description: None,
            },
            CompletionItem {
                label: "上海".into(),
                insert_text: "上海".into(),
                description: Some("华东".into()),
            },
        ],
    });

    assert!(app.apply_selected_command_completion());
    assert_eq!(app.input, "/deploy 上海");
    assert_eq!(app.cursor, app.input.len());
    assert!(app.command_completion.is_none());
}

/// 验证输入斜杠时展示命令用法、摘要和详细说明。
#[test]
#[cfg(feature = "plugins")]
fn command_preview_renders_descriptive_snapshot() {
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).expect("创建命令预览测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "/res".into();
    app.cursor = app.input.len();
    app.set_command_snapshot(Some(CommandSnapshot {
        generation: 1,
        commands: vec![CommandSpec::new(
            "resume",
            "恢复历史会话",
            "打开当前项目的会话列表并选择恢复。",
        )],
    }));

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染命令预览");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(text.contains("/resume"), "{text}");
    assert!(text.contains("恢复历史会话"), "{text}");
    assert!(
        text.contains("打开当前项目的会话列表并选择恢复。"),
        "{text}"
    );
}

/// 验证 Session 参数候选只读取轻量摘要并按标题过滤。
#[tokio::test]
#[cfg(feature = "plugins")]
async fn session_completion_uses_summary_titles() {
    let store = MemorySessionStore::new();
    for (id, title) in [("alpha", "架构讨论"), ("beta", "发布计划")] {
        let mut record = SessionRecord::new(
            SessionId::new(id).expect("创建候选会话标识"),
            Session::new(),
        )
        .expect("创建候选会话");
        record.title = Some(title.into());
        store.save(record, None).await.expect("保存候选会话");
    }

    let items = session_completion_items(&store, "架构", 6)
        .await
        .expect("读取 Session 候选");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label, "架构讨论");
    assert_eq!(items[0].insert_text, "alpha");
}

/// 验证恢复会话前必须重新读取并严格匹配用户选择时看到的 revision。
#[tokio::test]
#[cfg(feature = "plugins")]
async fn resume_selection_rejects_stale_revision() {
    let store = MemorySessionStore::new();
    let mut session = Session::new();
    session.push_user("历史消息");
    let saved = store
        .save(
            SessionRecord::new(
                SessionId::new("resume-target").expect("创建恢复目标标识"),
                session,
            )
            .expect("创建恢复目标"),
            None,
        )
        .await
        .expect("保存恢复目标");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.session_store = Arc::new(store);
    let original_id = app.session_record.id.clone();

    let error = resume_selected_session(&mut app, saved.id.as_str(), 0)
        .await
        .expect_err("过期 revision 必须被拒绝");
    assert!(error.to_string().contains("已更新"));
    assert_eq!(app.session_record.id, original_id);

    resume_selected_session(&mut app, saved.id.as_str(), saved.revision)
        .await
        .expect("最新 revision 应恢复成功");
    assert_eq!(app.session_record.id, saved.id);
    assert!(app
        .messages
        .iter()
        .any(|message| message.text.contains("历史消息")));
}

/// 验证点击插件外的主界面会释放插件焦点，使字符重新进入主输入框。
#[test]
#[cfg(feature = "plugins")]
fn clicking_main_view_restores_input_focus() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let mut view = test_plugin_view(UiPlacement::Left, "左侧插件");
    view.area = Rect::new(0, 0, 10, 10);
    app.plugin_views.push(view);
    app.plugin_focus = Some(0);

    let routed = app.route_plugin_mouse(&MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 20,
        row: 20,
        modifiers: KeyModifiers::NONE,
    });

    assert!(routed.is_none());
    assert_eq!(app.plugin_focus, None);
    assert!(matches!(
        app.route_plugin_key(KeyCode::Char('a'), KeyModifiers::NONE),
        PluginKeyRoute::Main
    ));
}

/// 写入一个测试附件文件，返回路径；测试结束由调用方删除。
fn write_temp_attachment(name: &str, bytes: &[u8]) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间应晚于 UNIX 纪元")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "lucia-attach-test-{}-{unique}-{name}",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("写入测试附件");
    path
}

/// 拖入图片生成 [Image#N] 引用标签，提交时转换为文本加图片内容块。
#[test]
fn attached_image_becomes_ref_clip_and_blocks() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("图.png", b"\x89PNG\r\n\x1a\n");

    app.input = "看这张 ".to_string();
    app.cursor = app.input.len();
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    assert_eq!(app.input, "看这张 [Image#1] ");
    assert_eq!(app.attachments.len(), 1);
    assert!(app.attachments[0].is_image);
    assert_eq!(app.attachments[0].media_type, "image/png");

    let submission = app.take_submission().expect("应有提交内容");
    let blocks = submission.blocks();
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.contains("[Image#1]")));
    assert!(
        matches!(&blocks[1], ContentBlock::Image { media_type, .. } if media_type == "image/png")
    );
    assert!(app.attachments.is_empty());
    assert!(app.input.is_empty());
}

/// 非图片文件使用 [FILE#文件名] 标签，重名时追加序号后缀。
#[test]
fn attached_files_use_name_labels_with_dedup() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("note.txt", "内容".as_bytes());

    app.attach_file(&path);
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();

    let labels: Vec<&str> = app
        .attachments
        .iter()
        .map(|attachment| attachment.label.as_str())
        .collect();
    assert!(labels[0].starts_with("[FILE#"));
    assert_ne!(labels[0], labels[1]);
    assert!(app.input.contains(labels[0]));
    assert!(app.input.contains(labels[1]));
}

/// Backspace 紧跟引用标签时整体删除标签与对应附件。
#[test]
fn backspace_removes_whole_ref_clip() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    let path = write_temp_attachment("pic.png", b"png");
    app.attach_file(&path);
    std::fs::remove_file(&path).ok();
    // 删除标签后自动附加的空格，使光标紧跟标签结尾
    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE, None);

    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE, None);

    assert!(app.input.is_empty());
    assert!(app.attachments.is_empty());
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

/// 粘贴的引号包裹或转义路径解析为文件；普通文本返回 None。
#[test]
fn pasted_file_path_parses_dropped_paths() {
    let path = write_temp_attachment("带 空格.txt", b"x");
    let display = path.display().to_string();

    let quoted = format!("'{display}'");
    assert_eq!(pasted_file_path(&quoted), Some(path.clone()));

    let escaped = display.replace(' ', "\\ ");
    assert_eq!(pasted_file_path(&escaped), Some(path.clone()));

    std::fs::remove_file(&path).ok();
    assert_eq!(pasted_file_path("普通粘贴文本"), None);
    assert_eq!(pasted_file_path(&display), None);
}

/// 粘贴非路径文本时保留逻辑换行并统一平台换行符。
#[test]
fn paste_preserves_multiline_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());

    app.handle_paste("第一行\r\n第二行\t内容");

    assert_eq!(app.input, "第一行\n第二行 内容");
    assert_eq!(app.cursor, app.input.len());
}

/// 输入历史由 Ctrl+P 进入回溯，回溯态中方向键可继续导航，编辑后退出回溯态。
#[test]
fn input_history_recalls_recent_submissions() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    for input in ["第一条", "第一条", "第二条"] {
        app.input = input.into();
        app.cursor = app.input.len();
        assert!(app.take_input().is_some());
    }

    app.handle_key(KeyCode::Char('p'), KeyModifiers::CONTROL, None);
    assert_eq!(app.input, "第二条");
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.input, "第一条");
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.input, "第二条");
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert!(app.input.is_empty());

    app.handle_key(KeyCode::Char('p'), KeyModifiers::CONTROL, None);
    app.handle_key(KeyCode::Char('改'), KeyModifiers::NONE, None);
    assert_eq!(app.input, "第二条改");
    assert_eq!(app.input_history_cursor, None);
}

/// 空输入时方向键滚动消息区而不是进入历史回溯；滚轮在备用屏下映射为方向键。
#[test]
fn arrow_keys_keep_scrolling_message_history() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "已提交".into();
    app.cursor = app.input.len();
    assert!(app.take_input().is_some());
    app.last_max_scroll = 10;

    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.scroll, Some(9));
    assert!(app.input.is_empty());
    assert_eq!(app.input_history_cursor, None);

    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.scroll, None);
}

/// 换行手势（Shift+Enter / Alt+Enter / Ctrl+J）插入换行，Home/End 使用行内语义。
#[test]
fn multiline_newline_gestures_and_line_navigation() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Enter, KeyModifiers::SHIFT, None);
    app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Enter, KeyModifiers::ALT, None);
    app.handle_key(KeyCode::Char('j'), KeyModifiers::CONTROL, None);
    app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE, None);
    assert_eq!(app.input, "a\nb\n\nc");

    // Home/End 停留在当前逻辑行内，而不是整段首尾。
    app.handle_key(KeyCode::Home, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, 5);
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::End, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, 3);
}

/// 编辑快捷键按词边界与 UTF-8 字符边界移动和删除。
#[test]
fn input_editor_supports_word_and_forward_deletion() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "hello, 世界".into();
    app.cursor = app.input.len();

    app.handle_key(KeyCode::Left, KeyModifiers::ALT, None);
    assert_eq!(&app.input[app.cursor..], "世界");
    app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL, None);
    assert_eq!(app.input, "世界");
    app.cursor = 0;
    app.handle_key(KeyCode::Delete, KeyModifiers::NONE, None);
    assert_eq!(app.input, "界");
}

/// 多行输入支持插入换行和按显示列上下移动光标。
#[test]
fn multiline_input_moves_cursor_between_lines() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.input = "甲乙\nabc\n终".into();
    app.cursor = "甲乙\nab".len();

    app.handle_key(KeyCode::Up, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, "甲".len());
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, None);
    assert_eq!(app.cursor, "甲乙\nab".len());
    app.cursor = app.input.len();
    app.handle_key(KeyCode::Enter, KeyModifiers::SHIFT, None);
    assert!(app.input.ends_with('\n'));
}

/// PageUp 和 PageDown 使用最近视口高度减一作为整页步长，Ctrl+End 回到底部。
#[test]
fn page_scroll_uses_last_viewport() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.last_max_scroll = 80;
    app.last_viewport = 20;

    app.handle_key(KeyCode::PageUp, KeyModifiers::NONE, None);
    assert_eq!(app.scroll, Some(61));
    app.handle_key(KeyCode::PageDown, KeyModifiers::NONE, None);
    assert_eq!(app.scroll, None);
    app.handle_key(KeyCode::PageUp, KeyModifiers::NONE, None);
    app.handle_key(KeyCode::End, KeyModifiers::CONTROL, None);
    assert_eq!(app.scroll, None);
}

/// 上下文状态在配置窗口后展示占比，并按超过 80% 与 95% 的阈值着色。
#[test]
fn context_status_uses_configured_window_thresholds() {
    assert_eq!(
        tui::context_status(86_900, Some(200_000)),
        ("43% · 86.9k".into(), COLOR_MUTED)
    );
    assert_eq!(tui::context_status(160_000, Some(200_000)).1, COLOR_MUTED);
    assert_eq!(tui::context_status(160_001, Some(200_000)).1, COLOR_WARNING);
    assert_eq!(tui::context_status(190_001, Some(200_000)).1, COLOR_DANGER);
    assert_eq!(
        tui::context_status(5_300, None),
        ("5.3k".into(), COLOR_MUTED)
    );
}

/// MIME 推断：图片按扩展名，未知扩展按内容区分文本与二进制。
#[test]
fn attachment_media_type_detects_kinds() {
    let png = attachment_media_type(Path::new("a.PNG"), b"binary");
    assert_eq!(png, ("image/png".to_string(), true));

    let pdf = attachment_media_type(Path::new("b.pdf"), b"%PDF");
    assert_eq!(pdf, ("application/pdf".to_string(), false));

    let text = attachment_media_type(Path::new("c.rs"), "fn main() {}".as_bytes());
    assert_eq!(text, ("text/plain".to_string(), false));

    let binary = attachment_media_type(Path::new("d.bin"), &[0u8, 159, 146, 150]);
    assert_eq!(binary, ("application/octet-stream".to_string(), false));
}

/// 输入渲染将引用标签切分为独立的高亮片段。
#[test]
fn input_ref_spans_highlight_labels() {
    let attachments = vec![PendingAttachment {
        label: "[Image#1]".to_string(),
        name: "pic.png".to_string(),
        media_type: "image/png".to_string(),
        data: String::new(),
        is_image: true,
    }];

    let spans = input_ref_spans("看 [Image#1] 这个", &attachments);
    let contents: Vec<&str> = spans.iter().map(|span| span.content.as_ref()).collect();

    assert_eq!(contents, vec!["看 ", "[Image#1]", " 这个"]);
}
