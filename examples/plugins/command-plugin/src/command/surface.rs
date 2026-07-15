//! Session Dialog 状态机、输入处理与声明式渲染。

use super::*;

/// 会话 Dialog 的全部交互和异步加载状态。
pub(super) struct SessionSurface {
    pub(super) visible: bool,
    pub(super) mode: SessionSurfaceMode,
    pub(super) query: String,
    pub(super) status: SessionListStatus,
    pub(super) selected: usize,
    pub(super) rendered_start: usize,
    pub(super) rendered_len: usize,
    pub(super) request_id: u64,
    pub(super) effects: VecDeque<SurfaceEffect>,
}

impl Default for SessionSurface {
    fn default() -> Self {
        Self {
            visible: false,
            mode: SessionSurfaceMode::Resume,
            query: String::new(),
            status: SessionListStatus::Empty,
            selected: 0,
            rendered_start: 0,
            rendered_len: 0,
            request_id: 0,
            effects: VecDeque::new(),
        }
    }
}

impl SessionSurface {
    /// 打开并重置界面，然后请求当前 `cwd` 的第一页会话摘要。
    ///
    /// `seq` 是插件级共享的查询 ID 计数器，避免与弹层候选查询串线。
    pub(super) fn open(&mut self, seq: &mut u64, mode: SessionSurfaceMode) {
        self.visible = true;
        self.mode = mode;
        self.query.clear();
        self.selected = 0;
        self.rendered_start = 0;
        self.rendered_len = 0;
        self.queue_query(seq, None);
    }

    /// 仅接受最近一次查询的响应，防止快速输入时旧结果覆盖新结果。
    pub(super) fn update(&mut self, request: SurfaceUpdateRequest) -> bool {
        if !self.visible || request.request_id != self.request_id {
            return false;
        }
        self.status = match request.status {
            SessionListStatus::Ready { items, .. } if items.is_empty() => SessionListStatus::Empty,
            status => status,
        };
        self.selected = self.selected.min(self.items().len().saturating_sub(1));
        self.rendered_start = 0;
        self.rendered_len = 0;
        true
    }

    /// 处理 Dialog 的稳定输入事件。
    pub(super) fn handle_input(&mut self, seq: &mut u64, event: UiInputEvent) {
        match event {
            UiInputEvent::Key { code, modifiers } => self.handle_key(seq, &code, &modifiers),
            UiInputEvent::Mouse { kind, y, .. } => self.handle_mouse(seq, &kind, y),
            UiInputEvent::MainInput { .. } => {}
        }
    }

    /// 处理导航、过滤、确认和关闭按键。
    pub(super) fn handle_key(&mut self, seq: &mut u64, code: &str, modifiers: &[String]) {
        match code {
            "escape" => self.close(),
            "up" => self.selected = self.selected.saturating_sub(1),
            "down" => {
                if self.selected + 1 >= self.items().len() {
                    if let Some(cursor) = self.next_cursor() {
                        self.selected = 0;
                        self.queue_query(seq, Some(cursor));
                        return;
                    }
                }
                self.selected = (self.selected + 1).min(self.items().len().saturating_sub(1));
            }
            "pageup" => self.selected = self.selected.saturating_sub(10),
            "pagedown" => {
                if self.selected + 10 >= self.items().len() {
                    if let Some(cursor) = self.next_cursor() {
                        self.selected = 0;
                        self.queue_query(seq, Some(cursor));
                        return;
                    }
                }
                self.selected = (self.selected + 10).min(self.items().len().saturating_sub(1));
            }
            "home" => self.selected = 0,
            "end" => self.selected = self.items().len().saturating_sub(1),
            "backspace" => {
                if self.query.pop().is_some() {
                    self.selected = 0;
                    self.queue_query(seq, None);
                }
            }
            "enter" => self.confirm_selection(),
            _ if is_printable_key(code, modifiers) => {
                self.query.push_str(code);
                self.selected = 0;
                self.queue_query(seq, None);
            }
            _ => {}
        }
    }

    /// 将鼠标滚轮和列表行点击映射为选择状态。
    pub(super) fn handle_mouse(&mut self, seq: &mut u64, kind: &str, y: u16) {
        match kind {
            "scroll_up" => self.selected = self.selected.saturating_sub(1),
            "scroll_down" => {
                if self.selected + 1 >= self.items().len() {
                    if let Some(cursor) = self.next_cursor() {
                        self.selected = 0;
                        self.queue_query(seq, Some(cursor));
                        return;
                    }
                }
                self.selected = (self.selected + 1).min(self.items().len().saturating_sub(1));
            }
            value if value.starts_with("down_") && y >= 3 => {
                let rendered_row = usize::from(y - 3);
                let index = self.rendered_start.saturating_add(rendered_row);
                if rendered_row < self.rendered_len && index < self.items().len() {
                    self.selected = index;
                }
            }
            _ => {}
        }
    }

    /// 在恢复模式中确认选中项，并立即隐藏对话框避免重复提交。
    pub(super) fn confirm_selection(&mut self) {
        if self.mode != SessionSurfaceMode::Resume {
            return;
        }
        let Some(item) = self.items().get(self.selected).cloned() else {
            return;
        };
        if item.active {
            return;
        }
        self.visible = false;
        self.effects.push_back(SurfaceEffect::ResumeSession {
            session_id: item.id,
            revision: item.revision,
        });
    }

    /// 隐藏界面并通知 TUI 取消 Dialog 焦点。
    pub(super) fn close(&mut self) {
        self.visible = false;
        self.effects.push_back(SurfaceEffect::CloseSurface);
    }

    /// 合并连续查询，只保留最新的轻量会话摘要请求。
    pub(super) fn queue_query(&mut self, seq: &mut u64, cursor: Option<String>) {
        *seq = seq.saturating_add(1).max(1);
        self.request_id = *seq;
        self.status = SessionListStatus::Loading;
        self.rendered_start = 0;
        self.rendered_len = 0;
        self.effects
            .retain(|effect| !matches!(effect, SurfaceEffect::QuerySessions { .. }));
        self.effects.push_back(SurfaceEffect::QuerySessions {
            request_id: self.request_id,
            query: self.query.clone(),
            cursor,
            limit: SESSION_PAGE_LIMIT,
        });
    }

    /// 返回当前可选择的会话摘要切片。
    pub(super) fn items(&self) -> &[SessionSummary] {
        match &self.status {
            SessionListStatus::Ready { items, .. } => items,
            _ => &[],
        }
    }

    /// 返回当前页的下一页游标，调用方可以安全地在状态变更前克隆它。
    pub(super) fn next_cursor(&self) -> Option<String> {
        match &self.status {
            SessionListStatus::Ready { next_cursor, .. } => next_cursor.clone(),
            _ => None,
        }
    }

    /// 根据宿主分配尺寸渲染稳定高度的 Dialog 内容。
    pub(super) fn render(&mut self, width: u16, height: u16, language: UiLanguage) -> Vec<UiLine> {
        if !self.visible {
            self.rendered_start = 0;
            self.rendered_len = 0;
            return Vec::new();
        }
        let content_width = usize::from(width.saturating_sub(2)).max(1);
        let title = match self.mode {
            SessionSurfaceMode::Resume => language.select("Resume session", "恢复会话"),
            SessionSurfaceMode::Browse => language.select("Project sessions", "项目会话"),
        };
        let mut lines = vec![
            line(vec![styled(title, UiColor::Cyan, true, false)]),
            line(vec![
                styled(
                    language.select("Search  ", "搜索  "),
                    UiColor::Gray,
                    false,
                    false,
                ),
                plain(if self.query.is_empty() {
                    language.select("Type to filter", "输入关键词过滤")
                } else {
                    &self.query
                }),
            ]),
            line(vec![plain("")]),
        ];

        let list_height = usize::from(height.saturating_sub(6)).max(1);
        self.rendered_start = visible_window_start(self.selected, self.items().len(), list_height);
        self.rendered_len = self
            .items()
            .len()
            .saturating_sub(self.rendered_start)
            .min(list_height);
        match &self.status {
            SessionListStatus::Loading => {
                lines.push(line(vec![styled(
                    language.select("Loading sessions...", "正在加载会话..."),
                    UiColor::Yellow,
                    false,
                    false,
                )]));
            }
            SessionListStatus::Empty => {
                lines.push(line(vec![styled(
                    language.select(
                        "No matching sessions in the current directory",
                        "当前工作目录没有匹配会话",
                    ),
                    UiColor::Gray,
                    false,
                    false,
                )]));
            }
            SessionListStatus::Error { message } => {
                lines.push(line(vec![styled(
                    &clip(message, content_width),
                    UiColor::Red,
                    false,
                    false,
                )]));
            }
            SessionListStatus::Ready { items, next_cursor } => {
                for (offset, item) in items
                    .iter()
                    .skip(self.rendered_start)
                    .take(list_height)
                    .enumerate()
                {
                    let index = self.rendered_start + offset;
                    lines.push(render_session_line(
                        item,
                        index == self.selected,
                        content_width,
                        language,
                    ));
                }
                if next_cursor.is_some() && items.len() < list_height {
                    lines.push(line(vec![styled(
                        language.select("More sessions available", "还有更多会话"),
                        UiColor::Gray,
                        false,
                        false,
                    )]));
                }
            }
        }

        while lines.len() < usize::from(height.saturating_sub(2)) {
            lines.push(line(vec![plain("")]));
        }
        lines.push(line(vec![plain("")]));
        lines.truncate(usize::from(height));
        lines
    }
}

/// 让选中项始终处在固定高度列表窗口内，并尽量保持窗口稳定靠前。
pub(super) fn visible_window_start(
    selected: usize,
    item_count: usize,
    list_height: usize,
) -> usize {
    if item_count <= list_height || selected < list_height {
        0
    } else {
        selected
            .saturating_add(1)
            .saturating_sub(list_height)
            .min(item_count.saturating_sub(list_height))
    }
}

/// 判断稳定键名是否表示可追加到搜索框的单个字符。
pub(super) fn is_printable_key(code: &str, modifiers: &[String]) -> bool {
    code.chars().count() == 1
        && modifiers
            .iter()
            .all(|modifier| modifier == "shift" || modifier.is_empty())
        && !code.chars().all(char::is_control)
}

/// 渲染包含标题、消息数、更新时间和占用状态的会话行。
pub(super) fn render_session_line(
    item: &SessionSummary,
    selected: bool,
    content_width: usize,
    language: UiLanguage,
) -> UiLine {
    let title = if item.title.trim().is_empty() {
        item.id.as_str()
    } else {
        item.title.as_str()
    };
    let active = if item.active {
        language.select(" · Active", " · 使用中")
    } else {
        ""
    };
    let updated = if item.updated_label.trim().is_empty() {
        item.updated_at_ms.to_string()
    } else {
        item.updated_label.clone()
    };
    let text = format!(
        "{}{} · {} {} · {}{}",
        if selected { "> " } else { "  " },
        title,
        item.message_count,
        language.select("messages", "条消息"),
        updated,
        active
    );
    line(vec![styled(
        &clip(&text, content_width),
        if item.active {
            UiColor::Gray
        } else if selected {
            UiColor::Black
        } else {
            UiColor::White
        },
        selected,
        selected,
    )])
}

/// 按 Unicode 字符边界裁剪一行，并在空间足够时添加省略号。
pub(super) fn clip(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.into();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut clipped = text.chars().take(width - 1).collect::<String>();
    clipped.push('…');
    clipped
}

/// 创建一行声明式终端内容。
pub(super) fn line(spans: Vec<UiSpan>) -> UiLine {
    UiLine { spans }
}

/// 创建没有额外样式的文本片段。
pub(super) fn plain(text: &str) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle::default(),
    }
}

/// 创建带颜色、粗体和可选反色背景的文本片段。
pub(super) fn styled(text: &str, foreground: UiColor, bold: bool, selected: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(foreground),
            background: selected.then_some(UiColor::Cyan),
            bold,
            reversed: false,
            ..UiStyle::default()
        },
    }
}
