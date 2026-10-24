//! TUI 事件循环在通道上传递的宿主事件与运行结果。

use crate::session::conversation::UserSubmission;
use agent_core::model::TokenUsage;
use agent_core::AgentRun;
use agent_session::{SessionId, SessionRecord, SessionStoreError};
use agent_tool::{ToolCall, ToolResult};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::style::Color;
#[cfg(feature = "plugins")]
use {
    crate::command::NativePluginDialogData,
    crate::plugin::PluginManagementOutcome,
    crate::ui::PluginRenderResult,
    agent_plugin_host::wasm::ProgressivePluginLoadUpdate,
    agent_plugin_protocol::{UiFrame as PluginUiFrame, UiNavigationRequest, UiSessionsReply},
};

pub(crate) enum UiEvent {
    Input(Event),
    Tick,
    ModelStarted,
    /// 共享文本缓冲区已有尚未渲染的模型增量。
    ModelTextReady,
    /// 共享推理摘要缓冲区已有尚未渲染的模型增量。
    ModelThinkingReady,
    /// 工具开始执行，携带未经裁剪的调用数据。
    ToolStarted(ToolCall),
    /// 共享工具输出缓冲区已有尚未展示的运行期输出。
    ///
    /// 命令可以每秒产生上千个 chunk；逐个转发等于逐个整帧重绘，会把渲染线程打满。
    /// 与模型文本一样按缓冲区合并，UI 一次取走本帧积累的全部 chunk。
    ToolOutputReady,
    /// 工具完成执行，携带未经裁剪的结果与 UI 细节。
    ToolFinished(ToolResult),
    /// 工具未执行，保留原始调用和跳过原因。
    ToolSkipped {
        call: ToolCall,
        reason: String,
    },
    /// 插件工具 renderer 完成一次消息帧渲染。
    #[cfg(feature = "plugins")]
    ToolFrameLoaded {
        call_id: String,
        revision: u64,
        width: u16,
        result: Result<Option<PluginUiFrame>>,
    },
    SteeringInjected,
    FollowUpInjected,
    /// 扩展发布到主事件列表的结构化展示事件。
    Extension {
        text: String,
        color: Color,
        divider: bool,
    },
    /// 插件发布的常驻底栏状态；插件 ID 来自 Host 注入的可信事件来源。
    #[cfg(feature = "plugins")]
    PluginFooterStatus {
        plugin_id: String,
        text: String,
        color: Color,
    },
    /// 插件请求主应用更新子视图导航栈。
    #[cfg(feature = "plugins")]
    ViewNavigation {
        plugin_id: String,
        request: UiNavigationRequest,
    },
    /// 最近一次模型响应的完整用量分项。
    ///
    /// 底栏用输入合计显示当前上下文规模，缓存读写分项用于累计会话命中率。
    ModelUsage(TokenUsage),
    /// 压缩和工具筛选后，实际模型请求的上下文分类快照。
    ContextSnapshot(crate::command::ContextSnapshot),
    /// 后台附件读取完成；仅由 App 复核请求与编辑代次后提交。
    AttachmentsPrepared {
        id: u64,
        result: Result<crate::app::PreparedAttachments, String>,
    },
    /// Agent 运行结束，携带至少一次持久化后的会话状态。
    AgentDone(Box<AgentCompletion>),
    /// 原生会话搜索面板的摘要查询完成。
    ///
    /// 载荷已经是可逐帧过滤的候选条目：检索文本在加载时算好一次，避免每次输入都
    /// 重新小写化整个列表。
    NativeSessionsLoaded(Result<Vec<crate::command::NativeSessionEntry>, SessionStoreError>),
    /// 会话标题的模型生成结果。
    ///
    /// 失败只携带原因字符串：标题是界面元数据，失败一律静默保留兜底标题，不进入消息流。
    SessionTitleReady {
        /// 请求标题的会话；与当前会话不一致时结果被丢弃。
        session_id: SessionId,
        /// 清洗后的单行标题。
        result: Result<String, String>,
    },
    /// 用户在原生会话 Dialog 中确认恢复一份精确修订。
    NativeResumeRequested {
        /// 待恢复的稳定会话 ID。
        session_id: SessionId,
        /// 列表加载时观察到的修订号，用于阻止竞态覆盖。
        revision: u64,
    },
    /// 原生 `/compact` 请求通过当前 Context Loader 重载会话。
    NativeCompactRequested,
    /// 原生插件管理页完成 Manager 与 Stable 环境读取。
    #[cfg(feature = "plugins")]
    NativePluginsLoaded(Result<NativePluginDialogData, String>),
    /// 原生插件管理页完成人工写操作和 Stable 基线提交。
    #[cfg(feature = "plugins")]
    NativePluginActionFinished(Result<PluginManagementOutcome, String>),
    /// 后台会话上下文重载完成。
    SessionContextReloaded(Box<Result<SessionReloadOutcome>>),
    /// 后台会话摘要查询完成，等待回送发起插件。
    #[cfg(feature = "plugins")]
    SessionsQueryDone {
        /// 发起查询的可信插件 ID。
        plugin_id: String,
        /// 接收应答的插件服务名。
        reply_service: String,
        /// 回送插件的查询应答。
        reply: Box<UiSessionsReply>,
    },
    /// 单个插件的渐进加载状态已经提交到动态宿主。
    #[cfg(feature = "plugins")]
    PluginLoadUpdate(ProgressivePluginLoadUpdate),
    /// 后台插件视图批量渲染完成。
    #[cfg(feature = "plugins")]
    PluginFramesLoaded(Vec<PluginRenderResult>),
    /// 后台上下文引用快照拉取完成。
    #[cfg(feature = "plugins")]
    ContextRefsLoaded(Vec<ContextRefOutcome>),
    /// 原生 Agent 会话视图收到一批 Runtime 事件。
    #[cfg(feature = "plugins")]
    AgentViewEvents {
        /// 发起导航的可信插件 ID。
        plugin_id: String,
        /// 插件内成员实例 ID。
        instance_id: String,
        /// 这批事件的来源句柄；与视图当前目标不一致时整批丢弃。
        target: agent_runtime::AgentId,
        /// 本批事件，按到达顺序排列。
        events: Vec<agent_core::AgentEvent>,
    },
    /// 原生 Agent 会话视图的事件流已经结束。
    #[cfg(feature = "plugins")]
    AgentViewClosed {
        /// 发起导航的可信插件 ID。
        plugin_id: String,
        /// 插件内成员实例 ID。
        instance_id: String,
        /// 结束的句柄；与视图当前目标不一致时忽略。
        target: agent_runtime::AgentId,
    },
    /// 后台单个视图已完成，立即展示而不等待整批结束。
    #[cfg(feature = "plugins")]
    PluginFrameLoaded(PluginRenderResult),
    /// 后台渐进加载全部结束，或全局规划失败。
    #[cfg(feature = "plugins")]
    PluginsLoaded(Box<Result<()>>),
}

/// 单个上下文引用 provider 的一次快照结果。
#[cfg(feature = "plugins")]
pub(crate) struct ContextRefOutcome {
    /// 提供快照的可信插件 ID。
    pub(crate) provider: String,
    /// 通过契约校验的引用；失败时携带面向用户的说明。
    pub(crate) refs: Result<Vec<agent_plugin_protocol::ContextRef>, String>,
}

/// 一次用户输入从先行持久化到模型运行结束的完整结果。
pub(crate) struct AgentCompletion {
    /// 模型已成功完成时携带运行结果。
    pub(crate) run: Option<AgentRun>,
    /// 下一轮必须使用的完整记录；最终保存失败时可能是尚未落盘的 dirty 完成态。
    pub(crate) session_record: SessionRecord,
    /// 保存或模型运行失败时的错误。
    pub(crate) error: Option<anyhow::Error>,
    /// 用户输入是否已经写入会话存储。
    pub(crate) input_committed: bool,
    /// 当前完成态是否允许自动执行下一条 FIFO 输入。
    pub(crate) queue_may_advance: bool,
    /// 用于保存失败时恢复编辑器内容的原始输入。
    pub(crate) input: UserSubmission,
}

/// 一次后台会话上下文重载的会话级结果。
pub(crate) enum SessionReloadOutcome {
    /// 加载器返回了新上下文，已持久化为该会话记录。
    Replaced(SessionRecord),
    /// 加载器返回的上下文与当前会话一致，无需替换。
    Unchanged,
}
