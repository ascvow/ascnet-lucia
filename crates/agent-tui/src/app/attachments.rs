//! 编辑器附件的有界后台准备与代次校验；文件 I/O 不持有 App 或修改草稿。

use super::App;
use crate::event::UiEvent;
use crate::session::attachment::{self, PastedPathKind, MAX_AUTO_TEXT_ATTACHMENT_BYTES};
use crate::session::conversation::{Msg, MsgKind, PendingAttachment};
use agent_core::Agent;
use agent_session::SessionId;
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
};

const MAX_BATCH_BYTES: usize = 32 * 1024 * 1024;
const MAX_BATCH_FILES: usize = 16;
static SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

/// 已授权的编辑意图；不能由后台决定提交到哪个会话或 Agent。
#[derive(Clone)]
pub(crate) enum AttachmentIntent {
    Files(Vec<PathBuf>),
    Command(String),
    Paste(String),
    Submit {
        agent: Option<Arc<Agent>>,
        steering: bool,
    },
}

/// 单次后台任务对应的编辑器快照；完成时匹配失败直接丢弃。
pub(crate) struct AttachmentJob {
    id: u64,
    generation: u64,
    session: SessionId,
    input: String,
    cursor: usize,
    model: String,
    submit_after: Option<(Option<Arc<Agent>>, bool)>,
    intent: AttachmentIntent,
}

/// 只含数据的准备结果，标签在 UI 接受结果时才分配。
pub(crate) struct PreparedAttachments {
    // 许可随完成事件持有，UI 消费前不能堆积更多大结果。
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
    items: Vec<PreparedItem>,
    grants: Vec<PathBuf>,
}
enum PreparedItem {
    File {
        attachment: PendingAttachment,
        insert_label: bool,
    },
    Path(PathBuf, String),
    Text(String),
    Error(String),
}

impl App {
    /// 启用后台附件；同步嵌入/单测仍可使用相同文件准备规则。
    pub(crate) fn with_async_attachments(mut self) -> Self {
        self.async_attachments = true;
        self
    }

    /// 发起一个编辑批次，最多两个真实阻塞任务；被取消的 OS 读取仍持有许可直到完成。
    pub(crate) fn schedule_attachments(&mut self, intent: AttachmentIntent) {
        if let Some(job) = self.attachment_job.as_ref() {
            if job.session != self.session_record.id
                || job.input != self.input
                || job.cursor != self.cursor
                || job.model != self.model_name
            {
                self.invalidate_attachment_job();
            }
        }
        if let AttachmentIntent::Paste(text) = &intent {
            if text.len() > MAX_BATCH_BYTES {
                self.messages
                    .push(Msg::new(MsgKind::Error, "粘贴超过 32 MiB 预算"));
                return;
            }
        }
        if self.attachment_job.is_some() {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "附件仍在准备，请稍候；编辑草稿可取消本次结果。",
            ));
            return;
        }
        let Ok(permit) = SLOTS.clone().try_acquire_owned() else {
            self.messages.push(Msg::new(
                MsgKind::Info,
                "附件读取任务仍在收尾，请稍后重试。",
            ));
            return;
        };
        self.attachment_sequence = self.attachment_sequence.wrapping_add(1);
        let id = self.attachment_sequence;
        let input = self.input.clone();
        let base = self.workspace.cwd.clone();
        let existing = self
            .attachments
            .iter()
            .filter_map(|a| a.source_path.clone())
            .collect::<Vec<_>>();
        let budget = MAX_BATCH_BYTES.saturating_sub(
            self.attachments
                .iter()
                .map(|a| a.data.len() + a.pasted_text.as_ref().map_or(0, String::len))
                .sum::<usize>(),
        );
        self.attachment_job = Some(AttachmentJob {
            id,
            generation: self.editor_generation,
            session: self.session_record.id.clone(),
            input: input.clone(),
            cursor: self.cursor,
            model: self.model_name.clone(),
            submit_after: None,
            intent: intent.clone(),
        });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut prepared = prepare(intent, input, base, existing, budget);
                prepared._permit = Some(permit);
                prepared
            })
            .await
            .map_err(|error| format!("附件准备任务失败：{error}"));
            let _ = tx.send(UiEvent::AttachmentsPrepared { id, result });
        });
    }

    /// 编辑、会话替换或取走草稿使已有结果失效，不 abort 已进入 OS 的阻塞工作。
    pub(crate) fn invalidate_attachment_job(&mut self) {
        self.editor_generation = self.editor_generation.wrapping_add(1);
        self.attachment_job = None;
        self.mentioned_attachments_ready = false;
    }

    /// 提交前先准备正文中的附件；返回 true 表示已延后或正等待准备。
    pub(crate) fn defer_attachment_submission(
        &mut self,
        agent: Option<Arc<Agent>>,
        steering: bool,
    ) -> bool {
        if !self.async_attachments || self.mentioned_attachments_ready {
            return false;
        }
        if let Some(job) = self.attachment_job.as_mut() {
            job.submit_after = Some((agent, steering));
            return true;
        }
        self.schedule_attachments(AttachmentIntent::Submit { agent, steering });
        true
    }

    /// 完成事件只在原会话和原编辑代次提交；模型模态在 UI 端重新校验。
    pub(crate) fn apply_prepared_attachments(
        &mut self,
        id: u64,
        result: Result<PreparedAttachments, String>,
    ) {
        if self.attachment_job.as_ref().is_none_or(|job| job.id != id) {
            return;
        }
        let job = self.attachment_job.take().expect("已验证任务存在");
        if job.generation != self.editor_generation
            || job.session != self.session_record.id
            || job.input != self.input
            || job.cursor != self.cursor
            || job.model != self.model_name
        {
            return;
        }
        let prepared = match result {
            Ok(value) => value,
            Err(error) => {
                self.messages.push(Msg::new(MsgKind::Error, error));
                return;
            }
        };
        self.input_history_cursor = None;
        self.last_escape_at = None;
        let completion_permit = prepared._permit;
        for path in prepared.grants {
            let _ = self.read_path_grants.grant_canonical(path);
        }
        for item in prepared.items {
            match item {
                PreparedItem::File {
                    mut attachment,
                    insert_label,
                } => {
                    let modality = attachment::attachment_modality(&attachment.media_type);
                    if let Err(error) = self.validate_attachment_modality(&modality) {
                        self.messages.push(Msg::new(MsgKind::Error, error));
                        continue;
                    }
                    attachment.label = self.next_attachment_label(&modality);
                    if insert_label {
                        if self.cursor > 0
                            && !self.input[..self.cursor]
                                .chars()
                                .next_back()
                                .is_some_and(char::is_whitespace)
                        {
                            self.input.insert(self.cursor, ' ');
                            self.cursor += 1;
                        }
                        let clip = format!("{} ", attachment.label);
                        self.input.insert_str(self.cursor, &clip);
                        self.cursor += clip.len();
                    }
                    self.attachments.push(attachment);
                }
                PreparedItem::Path(path, reason) => {
                    self.insert_pasted_path_text(&super::input::pasted_path_display(&path));
                    self.messages.push(Msg::new(
                        MsgKind::Info,
                        format!("未附加 {}：{reason}", path.display()),
                    ));
                }
                PreparedItem::Text(text) => {
                    let used = self
                        .attachments
                        .iter()
                        .map(|a| a.data.len() + a.pasted_text.as_ref().map_or(0, String::len))
                        .sum::<usize>();
                    if text.len() > MAX_BATCH_BYTES.saturating_sub(used) {
                        self.messages
                            .push(Msg::new(MsgKind::Error, "粘贴正文超过 32 MiB 预算"));
                    } else {
                        self.insert_plain_paste(&text);
                    }
                }
                PreparedItem::Error(error) => self.messages.push(Msg::new(MsgKind::Error, error)),
            }
        }
        drop(completion_permit);
        if let AttachmentIntent::Submit { agent, steering } = job.intent {
            self.mentioned_attachments_ready = true;
            match agent {
                Some(agent)
                    if steering
                        && self.running
                        && self.attachments.is_empty()
                        && self.refs.is_empty() =>
                {
                    self.submit_steering(&agent)
                }
                Some(agent) if !self.running && self.pending_reload.is_none() => {
                    self.submit(&agent)
                }
                _ => self.queue_input_until_ready(),
            }
        } else {
            self.finish_input_edit();
            if let Some((agent, steering)) = job.submit_after {
                if !steering
                    && self.handle_native_command_key(
                        crossterm::event::KeyCode::Enter,
                        crossterm::event::KeyModifiers::NONE,
                        agent.as_ref(),
                    )
                {
                    return;
                }
                self.defer_attachment_submission(agent, steering);
            }
        }
    }
}

fn prepare(
    intent: AttachmentIntent,
    input: String,
    base: PathBuf,
    existing: Vec<PathBuf>,
    mut budget: usize,
) -> PreparedAttachments {
    let mut result = PreparedAttachments {
        _permit: None,
        items: Vec::new(),
        grants: Vec::new(),
    };
    let (paths, automatic, insert_label) = match intent {
        AttachmentIntent::Files(paths) => (paths, false, true),
        AttachmentIntent::Command(argument) => {
            if let Some(path) = attachment::pasted_file_path(&argument, &base) {
                (vec![path], false, true)
            } else {
                let paths = argument
                    .split_whitespace()
                    .filter_map(|candidate| {
                        let path = attachment::pasted_file_path(candidate, &base);
                        if path.is_none() {
                            result.items.push(PreparedItem::Error(format!(
                                "找不到可读取的文件 `{candidate}`"
                            )));
                        }
                        path
                    })
                    .collect();
                (paths, false, true)
            }
        }
        AttachmentIntent::Paste(text) => {
            let files = if text.is_empty() || attachment::may_be_clipboard_file_names(&text) {
                attachment::clipboard_files()
            } else {
                Vec::new()
            };
            let normalized = text
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .replace('\t', " ");
            let paths = if !files.is_empty()
                && (text.is_empty() || attachment::is_clipboard_file_names(&text, &files))
            {
                Some(files)
            } else {
                attachment::pasted_file_uris(&normalized)
                    .or_else(|| attachment::pasted_path(&normalized, &base).map(|path| vec![path]))
                    .or_else(|| attachment::pasted_paths(&normalized, &base))
            };
            match paths {
                Some(paths) => (paths, true, true),
                None => {
                    result.items.push(PreparedItem::Text(normalized));
                    return result;
                }
            }
        }
        AttachmentIntent::Submit { .. } => {
            let mut text_budget = MAX_AUTO_TEXT_ATTACHMENT_BYTES;
            let paths = attachment::mentioned_paths(&input, &base)
                .into_iter()
                .filter_map(|(span, path)| {
                    if let Ok(canonical) = path.canonicalize() {
                        result.grants.push(canonical);
                    }
                    if existing.contains(&path) {
                        return None;
                    }
                    let kind = attachment::pasted_path_kind(&path).ok()?;
                    if kind == PastedPathKind::Media {
                        return Some(path);
                    }
                    let size = std::fs::metadata(&path).ok()?.len();
                    if kind == PastedPathKind::Text
                        && size <= text_budget
                        && super::input::path_is_standalone(&input, &span)
                    {
                        text_budget -= size;
                        Some(path)
                    } else {
                        None
                    }
                })
                .collect();
            (paths, false, false)
        }
    };
    for (index, path) in paths.into_iter().enumerate() {
        if index >= MAX_BATCH_FILES {
            result.items.push(PreparedItem::Error(
                "单次附件最多 16 个，剩余文件未读取".into(),
            ));
            break;
        }
        let prepared = (|| {
            if automatic {
                let kind =
                    attachment::pasted_path_kind(&path).map_err(|error| error.to_string())?;
                match kind {
                    PastedPathKind::Media => {}
                    PastedPathKind::Text
                        if std::fs::metadata(&path)
                            .map_err(|error| error.to_string())?
                            .len()
                            <= MAX_AUTO_TEXT_ATTACHMENT_BYTES => {}
                    _ => return Err("目录、二进制或大文本请交给文件工具读取".into()),
                }
            }
            let attachment = attachment::prepare_file_attachment(&path)?;
            if attachment.data.len() > budget {
                return Err("待发送附件超过 32 MiB 编码预算".into());
            }
            budget -= attachment.data.len();
            Ok(attachment)
        })();
        match prepared {
            Ok(attachment) => result.items.push(PreparedItem::File {
                attachment,
                insert_label,
            }),
            Err(reason) if automatic => result.items.push(PreparedItem::Path(path, reason)),
            Err(reason) => result.items.push(PreparedItem::Error(reason)),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    async fn receive(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    ) -> (u64, Result<PreparedAttachments, String>) {
        loop {
            if let UiEvent::AttachmentsPrepared { id, result } =
                tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            {
                return (id, result);
            }
        }
    }

    /// 一个真实任务覆盖顺序编号、修改/切会话丢弃和提交延迟，避免全局许可测试相互争抢。
    #[tokio::test]
    async fn async_files_preserve_order_and_reject_stale_editor_results() {
        let root =
            std::env::temp_dir().join(format!("lucia-async-attachments-{}", SessionId::generate()));
        std::fs::create_dir(&root).unwrap();
        let first = root.join("a.txt");
        let second = root.join("b.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, "test".into()).with_async_attachments();
        app.schedule_attachments(AttachmentIntent::Files(vec![first.clone(), second.clone()]));
        assert!(app.attachments.is_empty(), "返回时只登记意图，不阻塞读取");
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        assert_eq!(
            app.attachments
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "b.txt"]
        );
        assert_eq!(app.input, "[FILE#1] [FILE#2] ");

        app.schedule_attachments(AttachmentIntent::Files(vec![first.clone()]));
        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE, None);
        let edited = app.input.clone();
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        assert_eq!(app.input, edited);
        assert_eq!(app.attachments.len(), 2);

        app.schedule_attachments(AttachmentIntent::Files(vec![first.clone()]));
        let fresh =
            agent_session::SessionRecord::new(SessionId::generate(), agent_core::Session::new())
                .unwrap();
        app.replace_session(fresh, None);
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        assert!(app.input.is_empty() && app.attachments.is_empty());

        app.input = first.to_string_lossy().into_owned();
        app.cursor = app.input.len();
        app.queue_input_until_ready();
        assert!(app.queued_inputs.is_empty());
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        assert_eq!(app.queued_inputs.len(), 1);
        assert_eq!(app.queued_inputs[0].attachments.len(), 1);
        assert!(app.input.is_empty());
        app.handle_paste("纯粘贴正文");
        app.queue_input_until_ready();
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        assert_eq!(
            app.queued_inputs.len(),
            2,
            "粘贴后立即 Enter 应等待准备而非丢提交"
        );
        assert_eq!(app.queued_inputs[1].text, "纯粘贴正文");
        app.handle_paste("/tui default");
        app.queue_input_until_ready();
        let (id, result) = receive(&mut rx).await;
        app.apply_prepared_attachments(id, result);
        assert!(!app.fullscreen, "异步粘贴后立即 Enter 仍须先经过命令路由");
        assert_eq!(app.queued_inputs.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 超预算在加入结果前拒绝，后台不生成最终标签。
    #[test]
    fn prepared_files_are_bounded_and_unlabelled() {
        let root =
            std::env::temp_dir().join(format!("lucia-attachment-budget-{}", SessionId::generate()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "data").unwrap();
        let result = prepare(
            AttachmentIntent::Files(vec![path.clone()]),
            String::new(),
            root.clone(),
            Vec::new(),
            1,
        );
        assert!(matches!(&result.items[0], PreparedItem::Error(_)));
        let result = prepare(
            AttachmentIntent::Files(vec![path]),
            String::new(),
            root.clone(),
            Vec::new(),
            1024,
        );
        match &result.items[0] {
            PreparedItem::File { attachment, .. } => assert!(attachment.label.is_empty()),
            _ => panic!("应准备文件"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
