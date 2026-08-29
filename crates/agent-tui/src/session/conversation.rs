//! 对话消息、Markdown 渲染、附件引用与剪贴板处理。

use crate::theme::{COLOR_DANGER, COLOR_MUTED, COLOR_SUCCESS, COLOR_TEXT, COLOR_WARNING};
use crate::ui::markdown::{
    conversation_lines, markdown_lines, summarize_json, tool_output_preview_lines,
    tool_result_lines, truncate_line, user_lines,
};
use agent_core::model::ContentBlock;
#[cfg(feature = "plugins")]
use agent_plugin_protocol::UiFrame as PluginUiFrame;
use agent_tool::{ToolCall, ToolResult};
use ratatui::prelude::*;

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
}

/// 一次用户提交：含附件引用标签的文本与对应附件。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UserSubmission {
    /// 含附件引用标签的输入文本。
    pub(crate) text: String,
    /// 与文本中引用标签对应的附件，按插入顺序排列。
    pub(crate) attachments: Vec<PendingAttachment>,
}

impl UserSubmission {
    /// 转换为一次用户消息的内容块：文本在前，附件按加入顺序排在其后。
    pub(crate) fn blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::with_capacity(self.attachments.len() + 1);
        let visible_text = submission_text(&self.text, &self.attachments);
        if !visible_text.is_empty() {
            blocks.push(ContentBlock::Text { text: visible_text });
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

/// 展开粘贴文本并移除文件类展示标签，返回真正发送给模型的正文。
pub(crate) fn submission_text(text: &str, attachments: &[PendingAttachment]) -> String {
    let mut body = text.to_string();
    for attachment in attachments {
        let replacement = attachment.pasted_text.as_deref().unwrap_or_default();
        body = body.replace(&attachment.label, replacement);
    }
    body.trim().to_string()
}

impl From<&str> for UserSubmission {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            attachments: Vec::new(),
        }
    }
}

impl From<String> for UserSubmission {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
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
            MsgKind::Assistant => markdown_lines(&self.text, streaming),
            MsgKind::ToolRunning => self.tool_lines("", COLOR_WARNING, width),
            MsgKind::ToolOk => self.tool_lines("", COLOR_SUCCESS, width),
            MsgKind::ToolError => self.tool_lines("failed", COLOR_DANGER, width),
            MsgKind::ToolSkipped => self.tool_lines("skipped", COLOR_MUTED, width),
            MsgKind::Error => {
                conversation_lines("✗", &self.text, COLOR_DANGER, COLOR_DANGER, false)
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

    /// 构造工具调用块：`● 名称(参数)` 加 `└ 返回摘要` 与缩进的多行预览，
    /// 圆点以状态色区分，`+`/`-` 前缀的预览行按 diff 着色，块尾留空行。
    ///
    /// 摘要与预览行按 `width` 显示宽度截断，避免窄终端下自动换行
    /// 把续行顶到行首、破坏缩进对齐。
    pub(crate) fn tool_lines(&self, note: &str, color: Color, width: u16) -> Vec<Line<'_>> {
        #[cfg(feature = "plugins")]
        if let Some(frame) = self
            .tool_frame
            .as_ref()
            .filter(|frame| frame.visible && !frame.lines.is_empty())
        {
            let mut lines = crate::ui::plugin_frame_lines(frame);
            lines.push(Line::default());
            return lines;
        }

        // 前缀占 4 列，再留 1 列吸收截断省略号，保证整行不超宽。
        let preview_width = usize::from(width).saturating_sub(5).max(1);
        let mut first = vec![
            Span::styled("● ", Style::new().fg(color)),
            Span::styled(self.text.as_str(), Style::new().fg(COLOR_TEXT).bold()),
        ];
        if let Some(args) = self
            .tool_call
            .as_ref()
            .map(|call| summarize_json(&call.args, 64))
            .filter(|args| !args.is_empty())
        {
            first.push(Span::styled(
                format!("({args})"),
                Style::new().fg(COLOR_MUTED),
            ));
        }
        if !note.is_empty() {
            first.push(Span::styled(format!("  {note}"), Style::new().fg(color)));
        }
        let mut lines = vec![Line::from(first)];
        let result_color = if matches!(self.kind, MsgKind::ToolError) {
            COLOR_DANGER
        } else {
            COLOR_MUTED
        };
        let result_lines = if let Some(result) = self.tool_result.as_ref() {
            tool_result_lines(&result.content, TOOL_RESULT_PREVIEW_LINES, 96)
        } else if self.tool_output.is_empty() {
            Vec::new()
        } else {
            tool_output_preview_lines(&self.tool_output, TOOL_RESULT_PREVIEW_LINES, 96)
        };
        if let Some(result) = result_lines.first() {
            lines.push(Line::from(vec![
                Span::styled("  └ ", Style::new().fg(COLOR_MUTED)),
                Span::styled(
                    truncate_line(result, preview_width),
                    Style::new().fg(result_color),
                ),
            ]));
        }
        for detail in result_lines.iter().skip(1) {
            let detail_color = if matches!(self.kind, MsgKind::ToolError) {
                COLOR_DANGER
            } else if detail.starts_with('+') {
                COLOR_SUCCESS
            } else if detail.starts_with('-') {
                COLOR_DANGER
            } else {
                COLOR_MUTED
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    truncate_line(detail, preview_width),
                    Style::new().fg(detail_color),
                ),
            ]));
        }
        lines.push(Line::default());
        lines
    }
}

/// 工具结果预览保留的最大行数。
pub(crate) const TOOL_RESULT_PREVIEW_LINES: usize = 6;
/// 单个运行中工具在 TUI 内保留的最大文本量，防止第三方工具无限推送。
const TOOL_LIVE_OUTPUT_BYTES: usize = 200 * 1024;
