//! 工作区身份、会话存储、启动恢复与 CAS 持久化协调。

use super::*;

/// 启动目录派生出的稳定项目上下文。
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceContext {
    /// 规范化后的项目工作目录。
    pub(crate) cwd: PathBuf,
    /// 用于隔离项目会话空间的稳定摘要。
    pub(crate) project_id: String,
    /// 启动时探测到的 git 分支；非仓库为 `None`。
    pub(crate) git_branch: Option<String>,
}

impl WorkspaceContext {
    /// 捕获当前进程工作目录并生成稳定项目标识。
    pub(crate) fn capture() -> Result<Self> {
        let cwd = std::env::current_dir()?.canonicalize()?;
        let project_id = workspace_project_id(&cwd);
        let git_branch = detect_git_branch(&cwd);
        Ok(Self {
            cwd,
            project_id,
            git_branch,
        })
    }

    /// 创建尚未落盘的空白会话记录，并绑定当前项目标识。
    pub(crate) fn draft_record(&self) -> Result<SessionRecord> {
        self.record_with_id(SessionId::generate())
    }

    /// 使用指定标识创建绑定当前项目的空白会话记录。
    pub(crate) fn record_with_id(&self, id: SessionId) -> Result<SessionRecord> {
        let mut record = SessionRecord::new(id, Session::new())?;
        record.metadata.insert(
            "lucia.project_id".to_string(),
            Value::String(self.project_id.clone()),
        );
        Ok(record)
    }

    /// 返回当前项目的会话存储目录。
    pub(crate) fn sessions_dir(&self, root: &Path) -> PathBuf {
        root.join(&self.project_id).join("sessions")
    }
}

/// 读取工作目录所属 git 仓库的当前分支；非仓库或读取失败返回 `None`。
///
/// 自当前目录向上查找 `.git`，兼容 worktree（`.git` 为指向真实 gitdir 的
/// 文件）；detached HEAD 显示提交短哈希。仅在启动时读取一次。
pub(crate) fn detect_git_branch(cwd: &Path) -> Option<String> {
    let mut dir = cwd;
    let git_path = loop {
        let candidate = dir.join(".git");
        if candidate.exists() {
            break candidate;
        }
        dir = dir.parent()?;
    };
    let head_path = if git_path.is_dir() {
        git_path.join("HEAD")
    } else {
        let content = std::fs::read_to_string(&git_path).ok()?;
        let gitdir = PathBuf::from(content.strip_prefix("gitdir:")?.trim());
        let base = if gitdir.is_absolute() {
            gitdir
        } else {
            dir.join(gitdir)
        };
        base.join("HEAD")
    };
    let head = std::fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    Some(head.chars().take(8).collect())
}

/// 使用操作系统原始路径表示生成稳定项目标识，避免有损字符串转换造成目录碰撞。
pub(crate) fn workspace_project_id(cwd: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"lucia-project-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        digest.update(cwd.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in cwd.as_os_str().encode_wide() {
            digest.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    digest.update(cwd.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
}

/// 只在首次实际读取已有目录或保存消息时打开文件会话存储。
pub(crate) struct LazyFileSessionStore {
    /// 当前项目的最终会话目录。
    root: PathBuf,
    /// 首次实际打开后复用的文件存储实例。
    store: tokio::sync::OnceCell<FileSessionStore>,
}

impl LazyFileSessionStore {
    /// 创建不会触碰文件系统的惰性存储句柄。
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            store: tokio::sync::OnceCell::new(),
        }
    }

    /// 打开存储目录；只有保存路径会在目录不存在时调用本方法。
    pub(crate) async fn open(&self) -> Result<&FileSessionStore, SessionStoreError> {
        self.store
            .get_or_try_init(|| async { FileSessionStore::open(&self.root).await })
            .await
    }

    /// 目录不存在时保持惰性，已有目录则通过文件存储完成安全校验。
    pub(crate) async fn existing(&self) -> Result<Option<&FileSessionStore>, SessionStoreError> {
        if let Some(store) = self.store.get() {
            return Ok(Some(store));
        }
        match tokio::fs::symlink_metadata(&self.root).await {
            Ok(_) => self.open().await.map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SessionStoreError::Io {
                operation: "检查会话存储目录",
                path: self.root.clone(),
                source,
            }),
        }
    }
}

#[async_trait]
impl SessionStore for LazyFileSessionStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.load(id).await,
            None => Ok(None),
        }
    }

    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        self.open().await?.save(record, expected_revision).await
    }

    async fn delete(
        &self,
        id: &SessionId,
        expected_revision: u64,
    ) -> Result<(), SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.delete(id, expected_revision).await,
            None => Err(SessionStoreError::RevisionConflict {
                id: id.clone(),
                expected: Some(expected_revision),
                actual: None,
            }),
        }
    }

    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.list().await,
            None => Ok(Vec::new()),
        }
    }

    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        match self.existing().await? {
            Some(store) => store.list_summaries().await,
            None => Ok(Vec::new()),
        }
    }
}

/// 将持久化的 provider-neutral Session 恢复为主事件列表消息。
///
/// system、developer 和 thinking 内容不会直接展示；工具调用与后续结果会合并为一条
/// 工具事件，避免恢复后出现重复块。
pub(crate) fn restore_session_messages(session: &Session) -> Vec<Msg> {
    let mut messages = Vec::new();
    for message in session.messages() {
        match message.role {
            MessageRole::User => {
                let mut text = message.text_content();
                // TUI 提交的附件引用标签内嵌在文本中；其他来源的纯附件消息
                // 没有文本时补充占位标签，保证恢复后可见。
                if text.is_empty() {
                    let labels: Vec<String> = message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Image { .. } => Some("[Image]".to_string()),
                            ContentBlock::File { name, .. } => Some(format!("[FILE#{name}]")),
                            _ => None,
                        })
                        .collect();
                    text = labels.join(" ");
                }
                if !text.is_empty() {
                    messages.push(Msg::new(MsgKind::User, text));
                }
            }
            MessageRole::Assistant => {
                let text = message.text_content();
                if !text.is_empty() {
                    messages.push(Msg::new(MsgKind::Assistant, text));
                }
                for block in &message.content {
                    if let ContentBlock::ToolCall { call } = block {
                        messages.push(Msg::tool_started(call.clone()));
                    }
                }
            }
            MessageRole::Tool => {
                for block in &message.content {
                    let ContentBlock::ToolResult { result } = block else {
                        continue;
                    };
                    if let Some(restored) = messages.iter_mut().rev().find(|candidate| {
                        matches!(candidate.kind, MsgKind::ToolRunning)
                            && candidate.tool_call_id() == Some(result.call_id.as_str())
                    }) {
                        restored.kind = if result.is_error {
                            MsgKind::ToolError
                        } else {
                            MsgKind::ToolOk
                        };
                        restored.tool_result = Some(result.clone());
                    } else {
                        messages.push(Msg::tool_finished(result.clone()));
                    }
                }
            }
            MessageRole::System | MessageRole::Developer => {}
        }
    }
    messages
}

/// 从首次用户输入生成适合会话列表的短标题。
pub(crate) fn session_title(input: &str) -> Option<String> {
    let text = input.lines().find(|line| !line.trim().is_empty())?.trim();
    let mut chars = text.chars();
    let title = chars.by_ref().take(60).collect::<String>();
    Some(if chars.next().is_some() {
        format!("{title}…")
    } else {
        title
    })
}

// ─── 应用状态 ───

/// 判断回读记录是否包含一次 save 尝试的同一业务 payload。
///
/// 存储成功时会自行更新 revision 与更新时间，因此这两个规范化字段不参与比较。
pub(crate) fn session_record_payload_matches(
    stored: &SessionRecord,
    attempted: &SessionRecord,
) -> bool {
    stored.schema_version == attempted.schema_version
        && stored.id == attempted.id
        && stored.created_at_ms == attempted.created_at_ms
        && stored.title == attempted.title
        && stored.metadata == attempted.metadata
        && stored.session == attempted.session
}

/// 保存记录，并通过同 ID 回读协调“已写入但返回错误”的不确定提交。
pub(crate) async fn save_session_record_reconciled(
    session_store: &dyn SessionStore,
    record: SessionRecord,
    expected_revision: Option<u64>,
) -> Result<SessionRecord, SessionStoreError> {
    match session_store.save(record.clone(), expected_revision).await {
        Ok(saved) => Ok(saved),
        Err(error) => match session_store.load(&record.id).await {
            Ok(Some(stored)) if session_record_payload_matches(&stored, &record) => Ok(stored),
            _ => Err(error),
        },
    }
}

/// 先保存用户输入，再运行 Agent 并保存完整回复。
///
/// save 返回错误时先回读并协调可能已经落盘的不确定提交。确认未落盘后，最终保存失败
/// 才会分叉完整 Session；分叉也失败则返回 dirty 完成态，避免从旧记录继续。
pub(crate) async fn run_and_persist(
    agent: &Agent,
    session_store: &dyn SessionStore,
    mut session_record: SessionRecord,
    input: impl Into<UserSubmission>,
) -> AgentCompletion {
    let submission: UserSubmission = input.into();
    let expected_revision = (session_record.revision > 0).then_some(session_record.revision);
    session_record.session =
        agent.prepare_session_blocks(session_record.session.clone(), submission.blocks());
    if session_record.title.is_none() {
        // 纯附件输入没有可用文本时，用首个附件引用标签作为会话标题。
        session_record.title = session_title(&submission.text)
            .or_else(|| submission.attachments.first().map(|a| a.label.clone()));
    }
    let committed_record = match save_session_record_reconciled(
        session_store,
        session_record.clone(),
        expected_revision,
    )
    .await
    {
        Ok(record) => record,
        Err(error) => {
            return AgentCompletion {
                run: None,
                session_record,
                error: Some(error.into()),
                input_committed: false,
                queue_may_advance: false,
                input: submission,
            };
        }
    };

    let run = match agent.run_session(committed_record.session.clone()).await {
        Ok(run) => run,
        Err(error) => {
            return AgentCompletion {
                run: None,
                session_record: committed_record,
                error: Some(error),
                input_committed: true,
                queue_may_advance: true,
                input: submission,
            };
        }
    };

    let mut completed_record = committed_record.clone();
    completed_record.session = run.session.clone();
    match save_session_record_reconciled(
        session_store,
        completed_record.clone(),
        Some(committed_record.revision),
    )
    .await
    {
        Ok(saved_record) => AgentCompletion {
            run: Some(run),
            session_record: saved_record,
            error: None,
            input_committed: true,
            queue_may_advance: true,
            input: submission,
        },
        Err(save_error) => {
            let fork_result =
                match SessionRecord::new(SessionId::generate(), completed_record.session.clone()) {
                    Ok(mut fork_record) => {
                        fork_record.title = completed_record.title.clone();
                        fork_record.metadata = completed_record.metadata.clone();
                        save_session_record_reconciled(session_store, fork_record, None).await
                    }
                    Err(error) => Err(error),
                };
            match fork_result {
                Ok(fork_record) => AgentCompletion {
                    run: Some(run),
                    error: Some(anyhow!(
                        "原会话的模型回复保存失败（{save_error}），完整回复已分叉保存为会话 {}",
                        fork_record.id
                    )),
                    session_record: fork_record,
                    input_committed: true,
                    queue_may_advance: true,
                    input: submission,
                },
                Err(fork_error) => AgentCompletion {
                    run: Some(run),
                    session_record: completed_record,
                    error: Some(anyhow!(
                        "模型回复未能保存：{save_error}；分叉保存也失败：{fork_error}。完整回复已保留在当前内存会话中"
                    )),
                    input_committed: true,
                    queue_may_advance: false,
                    input: submission,
                },
            }
        }
    }
}

/// 解析 TUI 配置中的路径；CLI 路径保持相对当前工作目录的既有语义。
pub(crate) fn resolve_tui_path(
    cli_path: Option<&Path>,
    configured_path: Option<&Path>,
    config_path: &Path,
    fallback: PathBuf,
) -> PathBuf {
    if let Some(path) = cli_path {
        path.to_path_buf()
    } else if let Some(path) = configured_path {
        resolve_config_relative_path(config_path, path)
    } else {
        fallback
    }
}

/// 普通启动创建空白 Draft；只有显式 CLI 参数才恢复已有会话。
pub(crate) async fn load_startup_session(
    store: &dyn SessionStore,
    cli_session_id: Option<&str>,
    workspace: &WorkspaceContext,
    cli_resume_latest: bool,
) -> Result<SessionRecord> {
    if cli_session_id.is_none() && cli_resume_latest {
        let mut summaries = store.list_summaries().await?;
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some(summary) = summaries.into_iter().next() {
            if let Some(record) = store.load(&summary.id).await? {
                return Ok(record);
            }
        }
    }

    let Some(id) = cli_session_id else {
        return workspace.draft_record();
    };
    let id = SessionId::new(id)?;
    Ok(match store.load(&id).await? {
        Some(record) => record,
        None => workspace.record_with_id(id)?,
    })
}

/// 输出按最近更新时间排序的持久化会话摘要。
pub(crate) async fn print_persisted_sessions(store: &dyn SessionStore) -> Result<()> {
    let mut summaries = store.list_summaries().await?;
    summaries.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    if summaries.is_empty() {
        println!("没有持久化会话");
        return Ok(());
    }

    println!("SESSION\tREVISION\tMESSAGES\tUPDATED_MS\tTITLE");
    for summary in summaries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            summary.id,
            summary.revision,
            summary.message_count,
            summary.updated_at_ms,
            summary.title.as_deref().unwrap_or("")
        );
    }
    Ok(())
}
