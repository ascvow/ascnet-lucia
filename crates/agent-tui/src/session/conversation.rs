//! 对话消息、Markdown 渲染、附件引用与剪贴板处理。

use crate::theme::{COLOR_DANGER, COLOR_DANGER_BRIGHT, COLOR_MUTED, COLOR_SUCCESS, COLOR_WARNING};
use crate::ui::highlight::CodeLanguage;
use crate::ui::markdown::{conversation_lines, markdown_lines, truncate_line, user_lines};
use crate::ui::transcript::{
    output_preview_lines, preview_language, preview_lines, render_preview, tool_summary,
    tool_target, tool_title_spans, PreviewLine,
};
use agent_core::model::ContentBlock;
#[cfg(feature = "plugins")]
use agent_plugin_protocol::UiFrame as PluginUiFrame;
use agent_tool::{ToolCall, ToolResult};
use ratatui::prelude::*;
use std::path::PathBuf;

/// 输入框中等待随下一条消息发送的附件。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingAttachment {
    /// 输入框中展示的引用标签，如 `[IMAGE#1]`、`[FILE#1]`、`[PDF#1]`。
    pub(crate) label: String,
    /// 文件名（不含路径）。
    pub(crate) name: String,
    /// MIME 类型。
    pub(crate) media_type: String,
    /// base64 编码的文件内容；粘贴文本使用空字符串并由 `pasted_text` 保存原文。
    pub(crate) data: String,
    /// 是否图片附件。
    pub(crate) is_image: bool,
    /// 从剪贴板折叠展示的原始文本；文件附件为 `None`。
    pub(crate) pasted_text: Option<String>,
    /// 文件附件的来源路径；粘贴文本与历史恢复的附件为 `None`。
    ///
    /// 同一条消息可能既通过 `/attach` 附加过文件，正文里又写出了它的路径；
    /// 按来源路径判重可以避免同一个文件被发送两次。
    pub(crate) source_path: Option<PathBuf>,
}

/// 输入框中等待随下一条消息发送的上下文引用。
///
/// 与 [`PendingAttachment`] 的区别是内容形态：附件是文件字节，引用是 provider 已经
/// 生成好的一段正文。宿主不解释正文语义，只负责边界校验、标签展示与随消息发送。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingRef {
    /// 输入框中展示的引用标签，如 `[REF#1]`。
    pub(crate) label: String,
    /// 提供该引用的插件 ID；不同 provider 的 ID 空间互不相干。
    pub(crate) provider: String,
    /// provider 给出的稳定引用 ID；宿主用它判重与更新。
    pub(crate) id: String,
    /// 面向用户的一行标题，例如 `App.swift:12-20`。
    pub(crate) title: String,
    /// 随消息注入模型的正文。
    pub(crate) body: String,
}

impl PendingRef {
    /// 引用在输入框中占据的片段（标签加一个尾随空格）。
    ///
    /// 只有插件宿主存在时才可能自动插入引用，因此纯 Core 构建不需要该辅助方法。
    #[cfg(feature = "plugins")]
    pub(crate) fn clip(&self) -> String {
        format!("{} ", self.label)
    }
}

/// 一次用户提交：含引用标签的文本、附件与上下文引用。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UserSubmission {
    /// 含附件与引用标签的输入文本。
    pub(crate) text: String,
    /// 与文本中引用标签对应的附件，按插入顺序排列。
    pub(crate) attachments: Vec<PendingAttachment>,
    /// 随本条消息发送的上下文引用；标签被用户删掉的条目不会出现在这里。
    pub(crate) refs: Vec<PendingRef>,
}

impl UserSubmission {
    /// 转换为一次用户消息的内容块：正文在前，引用其次，附件最后。
    pub(crate) fn blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::with_capacity(self.attachments.len() + self.refs.len() + 1);
        let visible_text = submission_text(&self.text, &self.attachments, &self.refs);
        if !visible_text.is_empty() {
            blocks.push(ContentBlock::Text { text: visible_text });
        }
        for entry in &self.refs {
            blocks.push(ContentBlock::Text {
                text: entry.body.clone(),
            });
        }
        for attachment in &self.attachments {
            if attachment.pasted_text.is_some() {
                continue;
            }
            blocks.push(if attachment.is_image {
                ContentBlock::Image {
                    media_type: attachment.media_type.clone(),
                    data: attachment.data.clone(),
                }
            } else {
                ContentBlock::File {
                    name: attachment.name.clone(),
                    media_type: attachment.media_type.clone(),
                    data: attachment.data.clone(),
                }
            });
        }
        blocks
    }
}

/// 按输入文本过滤引用：标签已被删除的条目不再发送。
pub(crate) fn retained_refs(text: &str, refs: &[PendingRef]) -> Vec<PendingRef> {
    refs.iter()
        .filter(|entry| text.contains(entry.label.as_str()))
        .cloned()
        .collect()
}

/// 展开粘贴文本并移除文件与引用标签，返回真正发送给模型的正文。
pub(crate) fn submission_text(
    text: &str,
    attachments: &[PendingAttachment],
    refs: &[PendingRef],
) -> String {
    let mut body = text.to_string();
    for attachment in attachments {
        let replacement = attachment.pasted_text.as_deref().unwrap_or_default();
        body = body.replace(&attachment.label, replacement);
    }
    for entry in refs {
        body = body.replace(&entry.label, "");
    }
    body.trim().to_string()
}

impl From<&str> for UserSubmission {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            attachments: Vec::new(),
            refs: Vec::new(),
        }
    }
}

impl From<String> for UserSubmission {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
            refs: Vec::new(),
        }
    }
}

// ─── 聊天消息 ───

/// 主事件区消息的展示类别，决定普通文本与工具状态的渲染方式。
#[derive(Clone)]
pub(crate) enum MsgKind {
    User,
    Assistant,
    /// 工具运行中。
    ToolRunning,
    /// 工具成功完成。
    ToolOk,
    /// 工具失败。
    ToolError,
    /// 工具因 steering 被跳过。
    ToolSkipped,
    Error,
    Info,
    Extension,
}

/// 主事件区的一条可渲染消息，保留工具调用身份、结果和插件帧。
#[derive(Clone)]
pub(crate) struct Msg {
    pub(crate) kind: MsgKind,
    pub(crate) text: String,
    /// 工具消息关联的完整调用；普通消息为 `None`。
    pub(crate) tool_call: Option<ToolCall>,
    /// 工具消息关联的完整结果；运行中或普通消息为 `None`。
    pub(crate) tool_result: Option<ToolResult>,
    /// 工具运行期间已收到的增量输出；完成后由结构化结果接管展示。
    pub(crate) tool_output: String,
    /// 工具未执行时的原因；其他消息为 `None`。
    pub(crate) skip_reason: Option<String>,
    /// 工具详情是否展开；新消息默认收起，只保留标题与结果摘要。
    pub(crate) tool_expanded: bool,
    /// 插件为当前工具消息返回的声明式帧。
    #[cfg(feature = "plugins")]
    pub(crate) tool_frame: Option<PluginUiFrame>,
    /// 当前插件帧对应的消息区宽度。
    #[cfg(feature = "plugins")]
    pub(crate) tool_frame_width: Option<u16>,
    /// 最新一次工具消息渲染请求的版本号。
    #[cfg(feature = "plugins")]
    pub(crate) tool_render_revision: u64,
    /// 当前仍在运行的渲染请求所使用的消息区宽度。
    #[cfg(feature = "plugins")]
    pub(crate) tool_render_pending_width: Option<u16>,
    /// 扩展事件使用的强调色。
    pub(crate) accent: Option<Color>,
    /// 是否以分隔线形式展示扩展事件。
    pub(crate) divider: bool,
}

impl Msg {
    /// 创建普通消息，工具专用字段留空。
    pub(crate) fn new(kind: MsgKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            tool_call: None,
            tool_result: None,
            tool_output: String::new(),
            skip_reason: None,
            tool_expanded: false,
            #[cfg(feature = "plugins")]
            tool_frame: None,
            #[cfg(feature = "plugins")]
            tool_frame_width: None,
            #[cfg(feature = "plugins")]
            tool_render_revision: 0,
            #[cfg(feature = "plugins")]
            tool_render_pending_width: None,
            accent: None,
            divider: false,
        }
    }

    /// 创建运行中的工具消息，并以调用 ID 作为后续状态更新依据。
    pub(crate) fn tool_started(call: ToolCall) -> Self {
        let mut message = Self::new(MsgKind::ToolRunning, call.name.clone());
        message.tool_call = Some(call);
        message
    }

    /// 创建只有最终结果的工具消息，用于事件缺失时的安全恢复。
    pub(crate) fn tool_finished(result: ToolResult) -> Self {
        let kind = if result.is_error {
            MsgKind::ToolError
        } else {
            MsgKind::ToolOk
        };
        let mut message = Self::new(kind, result.name.clone());
        message.tool_result = Some(result);
        message
    }

    /// 返回工具消息的稳定调用 ID。
    pub(crate) fn tool_call_id(&self) -> Option<&str> {
        self.tool_call
            .as_ref()
            .map(|call| call.id.as_str())
            .or_else(|| {
                self.tool_result
                    .as_ref()
                    .map(|result| result.call_id.as_str())
            })
    }

    /// 返回当前消息是否属于工具生命周期，供消息分组和点击热区共用。
    pub(crate) fn is_tool(&self) -> bool {
        matches!(
            self.kind,
            MsgKind::ToolRunning | MsgKind::ToolOk | MsgKind::ToolError | MsgKind::ToolSkipped
        )
    }

    /// 返回展开当前工具消息是否真的能多看到内容。
    ///
    /// 判定基准是**收起态**的行数上限：只有默认视图确实藏起了内容，展开才有意义。
    /// 结果本来就短于上限时既不显示操作提示，也不注册点击热区，避免出现点击后
    /// 内容毫无变化的假交互。
    pub(crate) fn is_tool_expandable(&self) -> bool {
        if !self.is_tool() || self.skip_reason.is_some() {
            return false;
        }
        #[cfg(feature = "plugins")]
        if let Some(frame) = self
            .tool_frame
            .as_ref()
            .filter(|frame| frame.visible && !frame.lines.is_empty())
        {
            return frame.lines.len() > TOOL_RESULT_PREVIEW_LINES;
        }
        let preview = match self.tool_result.as_ref() {
            Some(result) => {
                preview_lines(self.tool_call.as_ref(), result, TOOL_RESULT_PREVIEW_LINES)
            }
            None if self.tool_output.is_empty() => return false,
            None => output_preview_lines(&self.tool_output, TOOL_RESULT_PREVIEW_LINES),
        };
        preview.iter().any(PreviewLine::is_elided)
    }

    /// 向运行中的工具消息追加输出；调用 ID 的匹配由上层状态机负责。
    pub(crate) fn append_tool_output(&mut self, delta: &str) {
        self.tool_output.push_str(delta);
        if self.tool_output.len() > TOOL_LIVE_OUTPUT_BYTES {
            let mut start = self.tool_output.len() - TOOL_LIVE_OUTPUT_BYTES;
            while !self.tool_output.is_char_boundary(start) {
                start += 1;
            }
            self.tool_output = format!("… [较早输出已省略]\n{}", &self.tool_output[start..]);
        }
    }

    /// 创建由扩展事件驱动的主事件列表消息。
    pub(crate) fn extension(text: impl Into<String>, color: Color, divider: bool) -> Self {
        Self {
            kind: MsgKind::Extension,
            text: text.into(),
            tool_call: None,
            tool_result: None,
            tool_output: String::new(),
            skip_reason: None,
            tool_expanded: false,
            #[cfg(feature = "plugins")]
            tool_frame: None,
            #[cfg(feature = "plugins")]
            tool_frame_width: None,
            #[cfg(feature = "plugins")]
            tool_render_revision: 0,
            #[cfg(feature = "plugins")]
            tool_render_pending_width: None,
            accent: Some(color),
            divider,
        }
    }

    /// 将消息转换为行首标记加正文的紧凑样式，并标记正在流式生成的消息。
    ///
    /// `width` 为消息区可用显示宽度，用户消息按它预换行以保持竖条连续。
    pub(crate) fn to_lines(&self, streaming: bool, width: u16) -> Vec<Line<'_>> {
        match self.kind {
            MsgKind::User => user_lines(&self.text, width),
            MsgKind::Assistant => markdown_lines(&self.text, streaming, width),
            MsgKind::ToolRunning => self.tool_lines("", COLOR_WARNING, width),
            MsgKind::ToolOk => self.tool_lines("", COLOR_SUCCESS, width),
            MsgKind::ToolError => self.tool_lines("failed", COLOR_DANGER, width),
            MsgKind::ToolSkipped => self.tool_lines("skipped", COLOR_MUTED, width),
            MsgKind::Error => {
                // 错误标记用更亮的一档红：失败信息读漏的代价最高。
                conversation_lines("✗", &self.text, COLOR_DANGER_BRIGHT, COLOR_DANGER, false)
            }
            // Info 行作为块结束标记（token 统计、插话提示），追加空行与后续消息分隔。
            MsgKind::Info => vec![
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(self.text.as_str(), Style::new().fg(COLOR_MUTED)),
                ]),
                Line::default(),
            ],
            MsgKind::Extension => {
                let color = self.accent.unwrap_or(COLOR_MUTED);
                let text = if self.divider {
                    format!("── {} ──", self.text)
                } else {
                    self.text.clone()
                };
                vec![
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(text, Style::new().fg(color).bold()),
                    ]),
                    Line::default(),
                ]
            }
        }
    }

    /// 构造工具调用块：标题显示 `● 名称(目标)` 与结果摘要，可展开结果在详情底部
    /// 显示展开或收起提示；圆点以状态色区分，diff 行整行铺背景色。
    ///
    /// 摘要与预览行按 `width` 显示宽度截断，避免窄终端下自动换行
    /// 把续行顶到行首、破坏缩进对齐。
    pub(crate) fn tool_lines(&self, note: &str, color: Color, width: u16) -> Vec<Line<'static>> {
        #[cfg(feature = "plugins")]
        if let Some(frame) = self
            .tool_frame
            .as_ref()
            .filter(|frame| frame.visible && !frame.lines.is_empty())
        {
            let frame_lines = crate::ui::plugin_frame_lines(frame);
            let expandable = self.is_tool_expandable();
            let visible_lines = if self.tool_expanded {
                TOOL_RESULT_DETAIL_LINES
            } else {
                TOOL_RESULT_PREVIEW_LINES
            };
            let omitted = frame_lines.len().saturating_sub(visible_lines);
            let mut lines = frame_lines
                .into_iter()
                .take(visible_lines)
                .map(|line| {
                    // 插件帧借用自帧数据，这里克隆为独立行以统一返回生命周期。
                    Line::from(
                        line.spans
                            .into_iter()
                            .map(|span| Span::styled(span.content.into_owned(), span.style))
                            .collect::<Vec<_>>(),
                    )
                    .style(line.style)
                })
                .collect::<Vec<_>>();
            if omitted > 0 {
                lines.push(Line::from(Span::styled(
                    format!("  … +{omitted} 行"),
                    Style::new().fg(COLOR_MUTED),
                )));
            }
            if expandable {
                lines.push(tool_toggle_hint(self.tool_expanded, width));
            }
            return lines;
        }

        // 摘要行预留缩进与截断省略号，窄终端下至少保留一个正文列。
        let summary_width = usize::from(width).saturating_sub(6).max(1);
        let is_error = matches!(self.kind, MsgKind::ToolError);
        let call = self.tool_call.as_ref();
        let target = call.and_then(tool_target);

        // 有结果就直接展示：先按当前上限构造详情，再据此决定是否需要操作提示。
        let limit = if self.tool_expanded {
            TOOL_RESULT_DETAIL_LINES
        } else {
            TOOL_RESULT_PREVIEW_LINES
        };
        let (preview, language) = if self.skip_reason.is_some() {
            (Vec::new(), CodeLanguage::Plain)
        } else {
            match self.tool_result.as_ref() {
                Some(result) => (
                    preview_lines(call, result, limit),
                    preview_language(call, result),
                ),
                None => (
                    output_preview_lines(&self.tool_output, limit),
                    CodeLanguage::Plain,
                ),
            }
        };
        let mut first = Vec::new();
        first.push(Span::styled("● ", Style::new().fg(color)));
        first.extend(tool_title_spans(&self.text, target.as_deref()));
        if !note.is_empty() {
            first.push(Span::styled(format!("  {note}"), Style::new().fg(color)));
        }
        let mut lines = vec![Line::from(first)];

        // 跳过的工具没有结果可展开，直接说明原因。
        if let Some(reason) = self.skip_reason.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("  └ ", Style::new().fg(COLOR_MUTED)),
                Span::styled(
                    truncate_line(reason, summary_width),
                    Style::new().fg(COLOR_MUTED),
                ),
            ]));
            return lines;
        }

        let summary = self
            .tool_result
            .as_ref()
            .and_then(|result| tool_summary(call, result));

        if let Some(summary) = summary {
            // 失败摘要用亮红：这是唯一必须被读到的工具结果。
            let summary_color = if is_error {
                COLOR_DANGER_BRIGHT
            } else {
                COLOR_MUTED
            };
            lines.push(Line::from(vec![
                Span::styled("  └ ", Style::new().fg(COLOR_MUTED)),
                Span::styled(
                    truncate_line(&summary, summary_width),
                    Style::new().fg(summary_color),
                ),
            ]));
        }
        lines.extend(render_preview(&preview, width, language, is_error));
        if self.is_tool_expandable() {
            lines.push(tool_toggle_hint(self.tool_expanded, width));
        }
        lines
    }
}

/// 构造可展开工具结果末尾的操作提示。
///
/// `expanded` 决定提示用户展开还是收起，`width` 用于限制窄终端中的文本宽度；
/// 返回的独立行使用弱化颜色，不产生外部副作用。
fn tool_toggle_hint(expanded: bool, width: u16) -> Line<'static> {
    let text = if expanded {
        "Click to collapse"
    } else {
        "Click to expand"
    };
    // 与工具正文同列（两列缩进），提示不额外右移，窄终端里也不会先被截断。
    let available = usize::from(width).saturating_sub(2).max(1);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(truncate_line(text, available), Style::new().fg(COLOR_MUTED)),
    ])
}

/// 工具消息默认展示的详情行数。
///
/// 有结果就直接展示，不必先点开——但必须有上限，否则一次 `read_file` 会把整屏刷满、
/// 把上下文冲掉。超出部分折叠为 `… +N 行`，点击工具消息展开到
/// [`TOOL_RESULT_DETAIL_LINES`]。
pub(crate) const TOOL_RESULT_PREVIEW_LINES: usize = 8;

/// 单条工具消息展开后最多展示的详情行数，避免超大结果拖慢每帧布局。
pub(crate) const TOOL_RESULT_DETAIL_LINES: usize = 40;
/// 单个运行中工具在 TUI 内保留的最大文本量，防止第三方工具无限推送。
const TOOL_LIVE_OUTPUT_BYTES: usize = 200 * 1024;
