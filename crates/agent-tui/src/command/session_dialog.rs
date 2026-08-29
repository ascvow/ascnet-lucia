//! 原生会话对话框的打开、加载与按键处理。

use crate::app::App;
use crate::command::{
    NativeSessionDialog, NativeSessionDialogMode, NativeSessionDialogStatus, MAX_VISIBLE_SESSIONS,
};
use crate::event::UiEvent;
use agent_session::{SessionStoreError, SessionSummary};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

impl App {
    /// 打开原生会话 Dialog，并异步读取当前项目的轻量摘要。
    pub(crate) fn open_native_sessions(&mut self, mode: NativeSessionDialogMode) {
        self.native_command.dialog = Some(NativeSessionDialog {
            mode,
            selected: 0,
            status: NativeSessionDialogStatus::Loading,
        });
        let session_store = Arc::clone(&self.session_store);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session_store.list_summaries().await.map(|mut summaries| {
                summaries.sort_by(|left, right| {
                    right
                        .updated_at_ms
                        .cmp(&left.updated_at_ms)
                        .then_with(|| left.id.cmp(&right.id))
                });
                summaries.truncate(MAX_VISIBLE_SESSIONS);
                summaries
            });
            let _ = tx.send(UiEvent::NativeSessionsLoaded(result));
        });
    }

    /// 把异步摘要结果提交到仍然打开的原生会话 Dialog。
    pub(crate) fn handle_native_sessions_loaded(
        &mut self,
        result: Result<Vec<SessionSummary>, SessionStoreError>,
    ) {
        let Some(dialog) = self.native_command.dialog.as_mut() else {
            return;
        };
        dialog.selected = 0;
        dialog.status = match result {
            Ok(summaries) => NativeSessionDialogStatus::Ready(summaries),
            Err(error) => NativeSessionDialogStatus::Error(error.to_string()),
        };
    }

    /// 处理会话 Dialog 的稳定键盘交互。
    pub(crate) fn handle_native_session_dialog_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return false;
        }
        let Some(dialog) = self.native_command.dialog.as_mut() else {
            return false;
        };
        let item_count = match &dialog.status {
            NativeSessionDialogStatus::Ready(items) => items.len(),
            NativeSessionDialogStatus::Loading | NativeSessionDialogStatus::Error(_) => 0,
        };
        match code {
            KeyCode::Esc => {
                self.native_command.dialog = None;
            }
            KeyCode::Up if item_count > 0 => {
                dialog.selected = dialog.selected.checked_sub(1).unwrap_or(item_count - 1);
            }
            KeyCode::Down if item_count > 0 => {
                dialog.selected = (dialog.selected + 1) % item_count;
            }
            KeyCode::Enter if item_count > 0 => {
                let NativeSessionDialogStatus::Ready(items) = &dialog.status else {
                    unreachable!("会话数量只会来自 Ready 状态");
                };
                let summary = items[dialog.selected.min(items.len() - 1)].clone();
                self.native_command.dialog = None;
                let _ = self.tx.send(UiEvent::NativeResumeRequested {
                    session_id: summary.id,
                    revision: summary.revision,
                });
            }
            _ => {}
        }
        true
    }
}
