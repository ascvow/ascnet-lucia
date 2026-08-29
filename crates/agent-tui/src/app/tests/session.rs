//! 工作区、会话存储与启动装配的回归测试。

use super::*;

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
