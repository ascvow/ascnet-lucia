//! 工作区、会话存储与启动装配的回归测试。

use super::*;

/// 验证空模型密钥触发演示模式，而非空明文密钥允许构建真实模型运行时。
#[test]
fn model_key_availability_rejects_empty_values() {
    let without_key: AgentRootConfig = serde_json::from_str(
        r#"{ "model": { "provider": "open-ai", "model": "test-model", "api_key": "   " } }"#,
    )
    .expect("解析无密钥测试配置");
    assert!(!configured_model_key_is_available(&without_key));

    let with_key: AgentRootConfig = serde_json::from_str(
        r#"{ "model": { "provider": "open-ai", "model": "test-model", "api_key": "test-key" } }"#,
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

/// 分支探测兼容工作区子目录、worktree 指针重定向、detached HEAD 和仓库创建/移除。
#[test]
fn git_branch_detection_follows_repository_and_worktree_changes() {
    use crate::session::detect_git_branch;
    let root = std::env::temp_dir().join(format!("lucia-git-detect-{}", SessionId::generate()));
    let repo = root.join("repo");
    let nested = repo.join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(detect_git_branch(&nested), None);

    std::fs::create_dir(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/master\n").unwrap();
    assert_eq!(detect_git_branch(&nested).as_deref(), Some("master"));
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/功能/刷新\n").unwrap();
    assert_eq!(detect_git_branch(&nested).as_deref(), Some("功能/刷新"));
    std::fs::write(
        repo.join(".git/HEAD"),
        "abcdef0123456789abcdef0123456789abcdef01\n",
    )
    .unwrap();
    assert_eq!(detect_git_branch(&nested).as_deref(), Some("abcdef01"));
    std::fs::write(repo.join(".git/HEAD"), "\n").unwrap();
    assert_eq!(detect_git_branch(&nested), None);
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    assert_eq!(detect_git_branch(&nested), None);

    let gitdir = root.join("metadata/worktrees/example");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/worktree-branch\n").unwrap();
    std::fs::write(repo.join(".git"), "gitdir: ../metadata/worktrees/example\n").unwrap();
    assert_eq!(
        detect_git_branch(&nested).as_deref(),
        Some("worktree-branch")
    );
    let moved = root.join("moved-metadata");
    std::fs::create_dir(&moved).unwrap();
    std::fs::write(moved.join("HEAD"), "ref: refs/heads/moved-branch\n").unwrap();
    std::fs::write(repo.join(".git"), format!("gitdir: {}\n", moved.display())).unwrap();
    assert_eq!(detect_git_branch(&nested).as_deref(), Some("moved-branch"));
    std::fs::remove_file(repo.join(".git")).unwrap();
    assert_eq!(detect_git_branch(&nested), None);
    std::fs::remove_dir_all(root).unwrap();
}

/// 后台刷新不依赖用户按键或 Agent 执行，空闲时只在变化后发事件并更新可见底栏。
#[tokio::test]
async fn git_branch_watcher_updates_idle_footer_and_stops_with_receiver() {
    let root = std::env::temp_dir().join(format!("lucia-git-watch-{}", SessionId::generate()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join(".git/HEAD"),
        "ref: refs/heads/feature/stale-footer\n",
    )
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx.clone(), "test".into());
    app.workspace.cwd = root.clone();
    app.workspace.git_branch = Some("feature/stale-footer".into());
    let identity = (
        app.workspace.cwd.clone(),
        app.workspace.project_id.clone(),
        app.session_record.id.clone(),
    );
    let mut terminal = Terminal::new(TestBackend::new(240, 20)).unwrap();
    terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
    let screen = |terminal: &Terminal<TestBackend>| {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    assert!(screen(&terminal).contains("feature/stale-footer"));
    let task = crate::session::watch_git_branch(&app.workspace, tx);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1_100), rx.recv())
            .await
            .is_err(),
        "未变化时不应重复触发重绘"
    );

    for (head, expected) in [
        (Some("ref: refs/heads/master\n"), Some("master")),
        (
            Some("deadbeef0123456789abcdef0123456789abcdef01\n"),
            Some("deadbeef"),
        ),
        (None, None),
        (Some("ref: refs/heads/restored\n"), Some("restored")),
    ] {
        if let Some(head) = head {
            std::fs::write(root.join(".git/HEAD.next"), head).unwrap();
            std::fs::rename(root.join(".git/HEAD.next"), root.join(".git/HEAD")).unwrap();
        } else {
            std::fs::remove_file(root.join(".git/HEAD")).unwrap();
        }
        let event = tokio::time::timeout(std::time::Duration::from_secs(4), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let UiEvent::GitBranchChanged(branch) = event else {
            panic!("只应收到分支变化事件")
        };
        assert_eq!(branch.as_deref(), expected);
        app.workspace.git_branch = branch;
        terminal.draw(|frame| render_root(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(!text.contains("feature/stale-footer"));
        if let Some(branch) = expected {
            assert!(text.contains(&format!("⎇ {branch}")), "{text}");
        } else {
            assert!(!text.contains('⎇'));
        }
        assert_eq!(
            (
                &app.workspace.cwd,
                &app.workspace.project_id,
                &app.session_record.id
            ),
            (&identity.0, &identity.1, &identity.2)
        );
    }
    drop(rx);
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
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
