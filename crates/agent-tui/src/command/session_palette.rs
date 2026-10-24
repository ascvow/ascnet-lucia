//! 原生会话选择面板的打开、加载、过滤与按键处理。
//!
//! 面板复用命令补全的位置与行几何：候选画在输入框上方，输入框本身就是搜索框。
//! 查询词因此始终是 [`App::input`]，退格、粘贴、按词移动都直接沿用主编辑器行为，
//! 面板只额外接管导航与确认键。

use crate::app::App;
use crate::command::{
    NativeSessionEntry, NativeSessionPalette, NativeSessionPaletteMode, NativeSessionPaletteStatus,
};
use crate::event::UiEvent;
use agent_session::{SessionStoreError, SessionSummary};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

/// 会话面板一次最多展示的候选行数。
///
/// 面板紧贴输入框，候选超过一屏就会把对话区挤成一条缝；剩下的靠搜索词收窄，而不是
/// 靠滚动硬看。它与命令补全的可见行数保持一致，两块面板的视觉高度因此不会互相打架。
pub(super) const MAX_VISIBLE_SESSION_ROWS: usize = 12;

/// 一次过滤的结果。
///
/// 命中数可能远大于可见行数：`indices` 只保留前 [`MAX_VISIBLE_SESSION_ROWS`] 项，
/// `total` 保留真实命中总数，供标题里的计数提示使用。
pub(super) struct SessionMatch {
    /// 命中项在 `Ready` 列表中的下标，已截断到可见行数。
    pub(super) indices: Vec<usize>,
    /// 命中总数。
    pub(super) total: usize,
    /// 实际参与匹配的检索词；渲染侧据此把命中位置标出来，解释"为什么这条被留下"。
    pub(super) tokens: Vec<String>,
}

impl SessionMatch {
    /// 构造一个空结果；加载中、加载失败与空输入共用这一形态。
    fn empty() -> Self {
        Self {
            indices: Vec::new(),
            total: 0,
            tokens: Vec::new(),
        }
    }
}

impl NativeSessionEntry {
    /// 用一条摘要构造候选，并预先算好检索文本。
    ///
    /// 过滤逐帧执行，每次都对标题重新小写化等于每帧分配一批等长字符串；这里在加载时
    /// 算一次，之后只做子串比较。
    pub(crate) fn new(summary: SessionSummary) -> Self {
        let mut haystack = String::with_capacity(
            summary.title.as_deref().map_or(0, str::len) + summary.id.as_str().len() + 1,
        );
        if let Some(title) = summary.title.as_deref() {
            haystack.push_str(title);
            haystack.push(' ');
        }
        haystack.push_str(summary.id.as_str());
        Self {
            summary,
            haystack: haystack.to_lowercase(),
        }
    }
}

/// 把搜索词切成小写检索词。
///
/// 空白分隔的多个词必须同时命中：`设计 评审` 定位的是标题里同时出现这两个词的会话，
/// 而不是把整串当成一个字面量去比对。
fn query_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|token| !token.is_empty())
        .collect()
}

/// 按搜索词过滤候选，返回可见下标与命中总数。
///
/// 顺序沿用加载时的时间降序：搜索只收窄集合，不改变"最近用过的在前"这一直觉。
/// 标题与稳定 ID 都参与匹配——标题可能被截断或缺失，ID 是唯一能精确定位的字段。
pub(super) fn session_matches(entries: &[NativeSessionEntry], query: &str) -> SessionMatch {
    let tokens = query_tokens(query);
    let mut indices = Vec::with_capacity(MAX_VISIBLE_SESSION_ROWS.min(entries.len()));
    let mut total = 0;
    for (index, entry) in entries.iter().enumerate() {
        if !tokens
            .iter()
            .all(|token| entry.haystack.contains(token.as_str()))
        {
            continue;
        }
        total += 1;
        if indices.len() < MAX_VISIBLE_SESSION_ROWS {
            indices.push(index);
        }
    }
    SessionMatch {
        indices,
        total,
        tokens,
    }
}

impl App {
    /// 打开会话选择面板，并异步读取当前项目的轻量摘要。
    ///
    /// 只加载摘要：完整会话在用户确认后才按 revision 校验并加载，避免浏览动作把整份
    /// 历史读进内存。
    pub(crate) fn open_native_sessions(&mut self, mode: NativeSessionPaletteMode) {
        self.native_command.session_palette = Some(NativeSessionPalette {
            mode,
            selected: 0,
            status: NativeSessionPaletteStatus::Loading,
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
                // 不截断：搜索要能命中更早的会话，可见行数由渲染侧限制。
                summaries
                    .into_iter()
                    .map(NativeSessionEntry::new)
                    .collect::<Vec<_>>()
            });
            let _ = tx.send(UiEvent::NativeSessionsLoaded(result));
        });
    }

    /// 把异步摘要结果提交到仍然打开的面板。
    pub(crate) fn handle_native_sessions_loaded(
        &mut self,
        result: Result<Vec<NativeSessionEntry>, SessionStoreError>,
    ) {
        let Some(palette) = self.native_command.session_palette.as_mut() else {
            return;
        };
        palette.selected = 0;
        palette.status = match result {
            Ok(entries) => NativeSessionPaletteStatus::Ready(entries),
            Err(error) => NativeSessionPaletteStatus::Error(error.to_string()),
        };
    }

    /// 返回会话面板当前应展示的候选行数；无候选时返回零（面板仍会画一行状态文本）。
    pub(crate) fn session_palette_rows(&self) -> usize {
        let Some(palette) = self.native_command.session_palette.as_ref() else {
            return 0;
        };
        match &palette.status {
            NativeSessionPaletteStatus::Ready(entries) => {
                session_matches(entries, &self.input).indices.len()
            }
            NativeSessionPaletteStatus::Loading | NativeSessionPaletteStatus::Error(_) => 0,
        }
    }

    /// 处理会话面板的键盘交互。
    ///
    /// 只消费导航与确认键；其余按键返回 `false` 交回主输入区，搜索词因此直接写进
    /// [`App::input`]。`Enter` 在没有命中时也一并吞掉——否则搜索词会变成一条发给
    /// 模型的消息，把用户正在找的东西发出去。
    pub(crate) fn handle_native_session_palette_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        if crate::app::is_copy_shortcut(code, modifiers) {
            return false;
        }
        let Some(palette) = self.native_command.session_palette.as_ref() else {
            return false;
        };
        let selected = palette.selected;
        let matched = match &palette.status {
            NativeSessionPaletteStatus::Ready(entries) => session_matches(entries, &self.input),
            NativeSessionPaletteStatus::Loading | NativeSessionPaletteStatus::Error(_) => {
                SessionMatch::empty()
            }
        };
        let resume_target = if matches!(code, KeyCode::Enter) {
            matched
                .indices
                .get(selected.min(matched.indices.len().saturating_sub(1)))
                .and_then(|index| match &palette.status {
                    NativeSessionPaletteStatus::Ready(entries) => entries.get(*index),
                    NativeSessionPaletteStatus::Loading | NativeSessionPaletteStatus::Error(_) => {
                        None
                    }
                })
                .map(|entry| entry.summary.clone())
        } else {
            None
        };
        match code {
            KeyCode::Esc => self.close_native_session_palette(),
            KeyCode::Up | KeyCode::Down => {
                let count = matched.indices.len();
                if let Some(palette) = self.native_command.session_palette.as_mut() {
                    if count > 0 {
                        // 先夹住再移动：搜索词变化后选中项可能停在已消失的下标上，
                        // 直接用旧值加减会跳到一个同样不存在的邻居。
                        let current = palette.selected.min(count - 1);
                        palette.selected = if matches!(code, KeyCode::Up) {
                            current.checked_sub(1).unwrap_or(count - 1)
                        } else {
                            (current + 1) % count
                        };
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(summary) = resume_target {
                    self.close_native_session_palette();
                    let _ = self.tx.send(UiEvent::NativeResumeRequested {
                        session_id: summary.id,
                        revision: summary.revision,
                    });
                }
            }
            _ => return false,
        }
        true
    }

    /// 关闭会话面板，并丢弃搜索词。
    ///
    /// 面板打开期间输入框就是搜索框，退出时不清空会让残留的搜索词在下一次回车时变成
    /// 一条发给模型的消息。
    fn close_native_session_palette(&mut self) {
        self.native_command.session_palette = None;
        self.input.clear();
        self.cursor = 0;
    }
}
