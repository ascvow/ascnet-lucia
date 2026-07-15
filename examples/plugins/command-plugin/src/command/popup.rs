//! 命令补全弹层的状态机与声明式渲染。
//!
//! 弹层完全由插件维护：宿主只转发主输入快照与手势键，并按帧可见性渲染。

use super::*;

/// 弹层当前展示的参数候选状态。
pub(super) struct PopupCompletion {
    /// 候选对应的参数位置与替换范围，基于生成时的输入快照。
    pub(super) context: CompletionContext,
    /// 生成候选时的完整输入，输入变化后候选失效。
    pub(super) source_input: String,
    /// 已过滤并编码的候选列表。
    pub(super) items: Vec<CompletionItem>,
}

/// 等待宿主会话数据源应答的参数候选请求。
pub(super) struct PendingSessionCompletion {
    /// 与宿主应答匹配的查询 ID。
    pub(super) query_id: u64,
    /// 候选对应的参数位置与替换范围。
    pub(super) context: CompletionContext,
    /// 发起请求时的完整输入。
    pub(super) source_input: String,
}

/// 命令弹层的输入快照、匹配与候选状态。
#[derive(Default)]
pub(super) struct CommandPopup {
    /// 最近一次主输入快照。
    pub(super) input: String,
    /// 主输入的 UTF-8 字节光标。
    pub(super) cursor: usize,
    /// 用户按 Esc 后临时隐藏，输入变化时复位。
    pub(super) hidden: bool,
    /// 当前选中行。
    pub(super) selection: usize,
    /// 激活的参数候选；存在时优先于命令名匹配展示。
    pub(super) completion: Option<PopupCompletion>,
    /// 等待宿主会话数据源应答的候选请求。
    pub(super) pending: Option<PendingSessionCompletion>,
}

impl CommandPopup {
    /// 同步主输入快照；内容变化时复位隐藏、选中与候选状态。
    pub(super) fn sync(&mut self, text: String, cursor: usize) {
        if text != self.input {
            self.hidden = false;
            self.selection = 0;
            self.completion = None;
            self.pending = None;
        }
        self.input = text;
        self.cursor = cursor;
    }

    /// 返回与当前输入前缀匹配的命令定义，最多六条。
    pub(super) fn matches(&self, registry: &CommandRegistry) -> Vec<CommandSpec> {
        let Some(body) = self.input.trim_start().strip_prefix('/') else {
            return Vec::new();
        };
        let prefix = body.split_whitespace().next().unwrap_or_default();
        registry
            .commands
            .values()
            .filter(|entry| {
                entry.spec.name.starts_with(prefix)
                    || entry
                        .spec
                        .aliases
                        .iter()
                        .any(|alias| alias.starts_with(prefix))
            })
            .take(6)
            .map(|entry| entry.spec.clone())
            .collect()
    }

    /// 输入是否仍处在命令名阶段（尚未输入参数分隔空白）。
    pub(super) fn in_name_stage(&self) -> bool {
        self.input
            .trim_start()
            .strip_prefix('/')
            .is_some_and(|body| !body.chars().any(char::is_whitespace))
    }

    /// 弹层是否有可展示内容。
    pub(super) fn visible(&self, registry: &CommandRegistry) -> bool {
        if self.hidden {
            return false;
        }
        self.completion.is_some() || self.pending.is_some() || !self.matches(registry).is_empty()
    }

    /// 上移选中行。
    pub(super) fn select_previous(&mut self) {
        self.selection = self.selection.saturating_sub(1);
    }

    /// 下移选中行，长度由当前展示模式决定。
    pub(super) fn select_next(&mut self, len: usize) {
        self.selection = (self.selection + 1).min(len.saturating_sub(1));
    }

    /// 隐藏弹层并清空瞬时候选状态。
    pub(super) fn dismiss(&mut self) {
        self.hidden = true;
        self.completion = None;
        self.pending = None;
        self.selection = 0;
    }

    /// 将选中的参数候选写入替换区间；返回新的输入与光标。
    ///
    /// 输入或替换区间失效时返回 `None`，调用方应清空候选状态。
    pub(super) fn apply_selected(&self) -> Option<(String, usize)> {
        let completion = self.completion.as_ref()?;
        if completion.source_input != self.input {
            return None;
        }
        let start = usize::try_from(completion.context.replacement_start).ok()?;
        let end = usize::try_from(completion.context.replacement_end).ok()?;
        let selected = self.selection.min(completion.items.len().saturating_sub(1));
        let item = completion.items.get(selected)?;
        if start > end
            || end > self.input.len()
            || !self.input.is_char_boundary(start)
            || !self.input.is_char_boundary(end)
        {
            return None;
        }
        let mut text = self.input.clone();
        text.replace_range(start..end, &item.insert_text);
        Some((text, start + item.insert_text.len()))
    }

    /// 渲染弹层内容行；宿主在触发未激活时不会展示这些行。
    pub(super) fn render(
        &self,
        registry: &CommandRegistry,
        agent_idle: bool,
        width: u16,
    ) -> Vec<UiLine> {
        let content_width = usize::from(width.saturating_sub(2)).max(1);
        if self.hidden {
            return Vec::new();
        }
        if let Some(completion) = &self.completion {
            let mut lines = completion
                .items
                .iter()
                .take(6)
                .enumerate()
                .map(|(index, item)| {
                    let selected = index == self.selection.min(completion.items.len() - 1);
                    candidate_line(
                        &item.label,
                        item.description.as_deref().unwrap_or_default(),
                        selected,
                    )
                })
                .collect::<Vec<_>>();
            let detail = registry
                .commands
                .get(&completion.context.command)
                .and_then(|entry| {
                    entry
                        .spec
                        .arguments
                        .get(usize::from(completion.context.argument_index))
                })
                .map(|argument| argument.description.as_str())
                .unwrap_or_default();
            lines.push(line(vec![
                styled(
                    &format!(
                        "/{} · {}",
                        completion.context.command, completion.context.argument
                    ),
                    UiColor::Cyan,
                    false,
                    false,
                ),
                plain("  "),
                styled(&clip(detail, content_width), UiColor::Gray, false, false),
            ]));
            return lines;
        }
        if self.pending.is_some() {
            return vec![line(vec![styled(
                "候选加载中...",
                UiColor::Yellow,
                false,
                false,
            )])];
        }
        let matches = self.matches(registry);
        if matches.is_empty() {
            return Vec::new();
        }
        let selected_index = self.selection.min(matches.len() - 1);
        let mut lines = matches
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                candidate_line(
                    &spec.display_usage(),
                    &spec.summary,
                    index == selected_index,
                )
            })
            .collect::<Vec<_>>();
        let selected = &matches[selected_index];
        let availability = if !agent_idle && selected.availability == CommandAvailability::IdleOnly
        {
            "  Agent 运行结束后可用"
        } else {
            ""
        };
        lines.push(line(vec![
            styled(
                &clip(&selected.description, content_width),
                UiColor::Gray,
                false,
                false,
            ),
            styled(availability, UiColor::Yellow, false, false),
        ]));
        lines
    }
}

/// 渲染一行带选中标记的候选。
fn candidate_line(label: &str, description: &str, selected: bool) -> UiLine {
    line(vec![
        styled(
            if selected { "› " } else { "  " },
            if selected {
                UiColor::Cyan
            } else {
                UiColor::Gray
            },
            false,
            false,
        ),
        styled(
            label,
            if selected {
                UiColor::White
            } else {
                UiColor::Gray
            },
            true,
            false,
        ),
        plain("  "),
        styled(description, UiColor::Gray, false, false),
    ])
}
