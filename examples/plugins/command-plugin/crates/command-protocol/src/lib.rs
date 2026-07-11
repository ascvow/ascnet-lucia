//! Command 插件公开的数据协议。
//!
//! 本 crate 不依赖 Lucia Host，第三方插件和 TUI 可以只依赖这些稳定的
//! JSON 数据类型完成命令注册、快照缓存、执行准备和会话选择界面对接。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, error::Error, fmt};

/// Command Provider 的固定插件 ID。
pub const PROVIDER_PLUGIN_ID: &str = "command";
/// Command 协议当前语义化版本。
pub const PROTOCOL_VERSION: &str = "1.0.0";
/// 注册或替换命令的服务名。
pub const REGISTER_SERVICE: &str = "command.register";
/// 注销当前调用方拥有命令的服务名。
pub const UNREGISTER_SERVICE: &str = "command.unregister";
/// 获取只读命令注册表快照的服务名。
pub const SNAPSHOT_SERVICE: &str = "command.snapshot";
/// 显式解析当前参数并生成候选计划的服务名。
///
/// 该服务不会由 Provider 在逐键输入热路径中自动调用，宿主应只在 Tab
/// 或自行节流后的候选请求中使用它。
pub const PREPARE_COMPLETION_SERVICE: &str = "command.prepare-completion";
/// 解析命令并生成执行计划的服务名。
pub const PREPARE_EXECUTE_SERVICE: &str = "command.prepare-execute";
/// 向会话选择界面注入异步数据的服务名。
pub const SURFACE_UPDATE_SERVICE: &str = "command.surface.update";
/// 轮询并清空会话选择界面待处理动作的服务名。
pub const SURFACE_POLL_EFFECTS_SERVICE: &str = "command.surface.poll-effects";
/// SDK 默认注册的命令回调服务名。
pub const CALLBACK_SERVICE: &str = "command.callback";
/// Command 插件声明的会话选择器视图 ID。
pub const SESSION_DIALOG_VIEW: &str = "command-session-dialog";
/// 宿主会话摘要候选的数据源名称。
pub const SESSION_COMPLETION_SOURCE: &str = "sessions";
/// 未显式指定时单次补全返回的候选上限。
pub const DEFAULT_COMPLETION_LIMIT: u16 = 20;
/// Provider、SDK 和宿主共同接受的单次候选硬上限。
pub const MAX_COMPLETION_LIMIT: u16 = 100;

/// 一个可注册的斜杠命令定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    /// 不含前导斜杠的规范命令名。
    pub name: String,
    /// 可触发同一命令的其他名称。
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 在候选列表中展示的一行摘要。
    pub summary: String,
    /// 在帮助和详细预览中展示的完整说明。
    pub description: String,
    /// 可选的显式用法；为空时由参数定义生成。
    #[serde(default)]
    pub usage: String,
    /// 按输入顺序排列的位置参数。
    #[serde(default)]
    pub arguments: Vec<ArgumentSpec>,
    /// 命令允许执行的应用状态。
    #[serde(default)]
    pub availability: CommandAvailability,
    /// 第三方命令的回调目标；官方内置命令可以为空。
    pub handler: Option<CommandHandlerRef>,
}

impl CommandSpec {
    /// 创建一个不含参数和别名的命令定义。
    pub fn new(
        name: impl Into<String>,
        summary: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            summary: summary.into(),
            description: description.into(),
            usage: String::new(),
            arguments: Vec::new(),
            availability: CommandAvailability::Always,
            handler: None,
        }
    }

    /// 追加一个命令别名。
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// 追加一个位置参数定义。
    pub fn with_argument(mut self, argument: ArgumentSpec) -> Self {
        self.arguments.push(argument);
        self
    }

    /// 设置显式用法文本。
    pub fn with_usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = usage.into();
        self
    }

    /// 返回用于预览的完整命令用法。
    pub fn display_usage(&self) -> String {
        if !self.usage.trim().is_empty() {
            return self.usage.clone();
        }
        let mut usage = format!("/{}", self.name);
        for argument in &self.arguments {
            usage.push(' ');
            let suffix = if argument.variadic { "..." } else { "" };
            if argument.required {
                usage.push_str(&format!("<{}{}>", argument.name, suffix));
            } else {
                usage.push_str(&format!("[{}{}]", argument.name, suffix));
            }
        }
        usage
    }
}

/// 命令的一个位置参数定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArgumentSpec {
    /// 参数在执行结果中的稳定名称。
    pub name: String,
    /// 参数预览中展示的说明。
    pub description: String,
    /// 参数值的解析类型。
    pub kind: ArgumentKind,
    /// 缺少该参数时是否拒绝执行。
    #[serde(default)]
    pub required: bool,
    /// 是否消费剩余全部输入；仅允许用于最后一个参数。
    #[serde(default)]
    pub variadic: bool,
    /// 参数候选值的来源。
    #[serde(default)]
    pub completion: CompletionSource,
}

impl ArgumentSpec {
    /// 创建一个必填参数定义。
    pub fn required(
        name: impl Into<String>,
        description: impl Into<String>,
        kind: ArgumentKind,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind,
            required: true,
            variadic: false,
            completion: CompletionSource::None,
        }
    }

    /// 创建一个可选参数定义。
    pub fn optional(
        name: impl Into<String>,
        description: impl Into<String>,
        kind: ArgumentKind,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind,
            required: false,
            variadic: false,
            completion: CompletionSource::None,
        }
    }

    /// 将参数设置为消费剩余输入的可变参数。
    pub fn variadic(mut self) -> Self {
        self.variadic = true;
        self
    }

    /// 设置参数候选值来源。
    pub fn with_completion(mut self, completion: CompletionSource) -> Self {
        self.completion = completion;
        self
    }
}

/// 命令参数支持的基础类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArgumentKind {
    /// 任意非空字符串。
    String,
    /// 可解析为有符号 64 位整数的字符串。
    Integer,
    /// `true`、`false`、`yes`、`no`、`1` 或 `0`。
    Boolean,
    /// 必须匹配给定值之一。
    Choice {
        /// 允许输入的规范值。
        values: Vec<String>,
    },
    /// 由宿主会话仓库解析的会话标识。
    Session,
}

/// 参数候选值的来源。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompletionSource {
    /// 不提供候选值。
    #[default]
    None,
    /// 候选值随命令快照一起下发，可由 TUI 本地过滤。
    Static {
        /// 完整静态候选列表。
        items: Vec<CompletionItem>,
    },
    /// 通过命令 owner 的 SDK 回调动态获取候选值。
    Callback,
    /// 由 TUI 对接的宿主数据源提供候选值。
    Surface {
        /// 数据源的稳定名称。
        source: String,
    },
}

/// 一个可展示和插入的补全候选。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionItem {
    /// 候选列表中展示的主标签。
    pub label: String,
    /// 候选代表的参数值；Provider 或 SDK 会在返回前编码成单个命令行 token。
    pub insert_text: String,
    /// 候选列表中展示的可选说明。
    pub description: Option<String>,
}

/// 命令允许执行的应用状态。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandAvailability {
    /// 空闲或 Agent 运行时都可以执行。
    #[default]
    Always,
    /// 仅允许在 Agent 空闲时执行。
    IdleOnly,
}

/// 第三方命令的回调服务位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandHandlerRef {
    /// owner 插件内公开的回调服务名。
    pub service: String,
    /// 同一个服务内区分处理器的稳定标识。
    pub handler_id: String,
}

/// 注册或替换一个命令的请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterCommandRequest {
    /// 要注册的完整命令定义。
    pub spec: CommandSpec,
}

/// 注册成功后返回的规范名称和注册表代次。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterCommandResponse {
    /// 注册后的规范命令名。
    pub name: String,
    /// 变更后的注册表代次。
    pub generation: u64,
}

/// 注销当前调用方拥有命令的请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnregisterCommandRequest {
    /// 要注销的规范名称或别名。
    pub name: String,
}

/// 注销请求的结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnregisterCommandResponse {
    /// 是否实际移除了命令。
    pub removed: bool,
    /// 操作完成后的注册表代次。
    pub generation: u64,
}

/// 获取完整命令快照的请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotRequest {}

/// 可在 TUI 输入热路径中缓存的只读命令快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSnapshot {
    /// 每次注册表变更后单调递增的代次。
    pub generation: u64,
    /// 按规范名称排序的命令定义。
    pub commands: Vec<CommandSpec>,
}

/// 请求 Provider 识别当前正在输入的命令参数。
///
/// 该请求只生成候选计划，不会直接调用第三方插件或宿主数据源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepareCompletionRequest {
    /// 包含前导斜杠的完整输入框文本。
    pub input: String,
    /// UTF-8 字节光标；为空时使用输入末尾。
    #[serde(default)]
    pub cursor: Option<u32>,
    /// 本次最多需要的候选数；零值按默认上限处理。
    #[serde(default = "default_completion_limit")]
    pub limit: u16,
}

impl PrepareCompletionRequest {
    /// 创建一个在输入末尾请求默认数量候选的请求。
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            cursor: None,
            limit: DEFAULT_COMPLETION_LIMIT,
        }
    }
}

/// Provider 已识别的参数位置和候选替换范围。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionContext {
    /// 解析别名后的规范命令名。
    pub command: String,
    /// 当前参数在 `CommandSpec` 中的稳定名称。
    pub argument: String,
    /// 当前参数在 `CommandSpec.arguments` 中的零基索引。
    pub argument_index: u16,
    /// 已按命令行转义规则解码的当前输入前缀。
    pub prefix: String,
    /// TUI 接受候选时应替换的 UTF-8 字节起点。
    pub replacement_start: u32,
    /// TUI 接受候选时应替换的 UTF-8 字节终点，不包含该位置。
    pub replacement_end: u32,
}

/// 交给宿主原生数据源执行的类型化补全请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceCompletionRequest {
    /// 宿主数据源的稳定名称，例如 `sessions`。
    pub source: String,
    /// 数据源查询所需的命令参数上下文。
    pub request: CommandCompletionRequest,
}

/// Provider 为一次显式参数候选请求生成的受控计划。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrepareCompletionResponse {
    /// Provider 已本地过滤静态或 Choice 候选。
    Candidates {
        /// 候选对应的参数位置和替换范围。
        context: CompletionContext,
        /// 不超过请求上限的候选列表。
        items: Vec<CompletionItem>,
    },
    /// TUI 应以 `caller_id=command` 调用可信 owner 的动态补全服务。
    Callback {
        /// 候选对应的参数位置和替换范围。
        context: CompletionContext,
        /// 由 Host 注入并由 Provider 注册表保存的 owner 插件 ID。
        owner_plugin_id: String,
        /// owner 插件公开的回调服务名。
        service: String,
        /// 已绑定 handler 的类型化动态补全请求。
        request: CommandCallbackRequest,
    },
    /// TUI 应把请求交给受支持的宿主原生数据源。
    Surface {
        /// 候选对应的参数位置和替换范围。
        context: CompletionContext,
        /// 不包含插件 owner 信息的宿主数据源请求。
        request: SurfaceCompletionRequest,
    },
    /// 当前输入尚未落在具有候选来源的参数上。
    NoMatch,
    /// 输入或光标无效，未生成任何外部调用计划。
    Error {
        /// 面向用户且不包含内部 owner 信息的错误说明。
        message: String,
    },
}

/// 请求解析并准备执行一条斜杠命令。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepareExecuteRequest {
    /// 包含前导斜杠的原始输入。
    pub input: String,
    /// Agent 当前是否空闲；缺失时按非空闲处理，避免绕过 `idle_only`。
    #[serde(default)]
    pub agent_idle: bool,
}

/// 命令准备阶段的结构化结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrepareExecuteResponse {
    /// 第三方插件命令的安全回调计划。
    Callback {
        /// 由 Host 注入并由注册表保存的 owner 插件 ID。
        owner_plugin_id: String,
        /// owner 插件内的回调服务名。
        service: String,
        /// 发送给回调服务的结构化请求。
        request: CommandCallbackRequest,
    },
    /// 请求 TUI 执行受控的应用级动作。
    SurfaceAction {
        /// 仅官方内置命令能够产生的动作。
        action: SurfaceAction,
    },
    /// 插件已经打开自己的声明式界面。
    SurfaceOpened {
        /// 需要 TUI 聚焦的插件视图 ID。
        view_id: String,
    },
    /// 直接写入对话事件列表的文本输出。
    Output {
        /// 命令生成的可展示文本。
        content: String,
    },
    /// 解析或参数校验失败，未产生任何副作用。
    Error {
        /// 面向用户的错误说明。
        message: String,
        /// 可用于纠正输入的命令用法。
        usage: Option<String>,
    },
}

/// 仅官方 Command 插件可以请求的应用级动作。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SurfaceAction {
    /// 结束当前会话并进入新的空白草稿。
    NewSession,
    /// 清空当前会话上下文并进入新的空白草稿。
    ClearSession,
    /// 请求 TUI 立即压缩并持久化当前会话上下文。
    CompactSession,
    /// 请求 TUI 正常退出应用。
    ExitApplication,
}

/// 已通过 Command 插件解析和校验的命令调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandInvocation {
    /// 命令的规范名称，不是用户输入的别名。
    pub command: String,
    /// 用户提交的原始命令行。
    pub input: String,
    /// 按参数定义名称保存的一个或多个值。
    pub arguments: BTreeMap<String, Vec<String>>,
}

/// Command 插件调用第三方 SDK 回调服务的请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandCallbackRequest {
    /// 执行已经完成参数校验的命令。
    Execute {
        /// SDK 路由到具体处理器所需的标识。
        handler_id: String,
        /// 已解析的命令调用。
        invocation: CommandInvocation,
    },
    /// 动态计算当前参数的候选值。
    Complete {
        /// SDK 路由到具体处理器所需的标识。
        handler_id: String,
        /// 补全所需的输入上下文。
        request: CommandCompletionRequest,
    },
}

/// 请求第三方命令动态补全时提供的上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandCompletionRequest {
    /// 命令的规范名称。
    pub command: String,
    /// 当前正在补全的参数名称。
    pub argument: String,
    /// 用户当前输入的候选前缀。
    pub prefix: String,
    /// 完整原始输入，便于实现上下文相关补全。
    pub input: String,
    /// 本次最多返回的候选数；SDK 会再次应用硬上限。
    #[serde(default = "default_completion_limit")]
    pub limit: u16,
}

/// 为 Serde 缺省字段提供稳定的候选数量。
fn default_completion_limit() -> u16 {
    DEFAULT_COMPLETION_LIMIT
}

/// 第三方 SDK 回调服务返回的统一信封。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandCallbackResponse {
    /// 命令执行成功后的自由格式 JSON 结果。
    Executed {
        /// 由命令 owner 定义的数据。
        result: Value,
    },
    /// 动态补全成功后的有界候选列表。
    Completed {
        /// 建议由 SDK 和 TUI 一起限制到较小数量。
        items: Vec<CompletionItem>,
    },
}

/// 会话选择界面的一条轻量摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    /// 当前项目内稳定且唯一的会话 ID。
    pub id: String,
    /// 用于列表展示的会话标题。
    pub title: String,
    /// 可选的一行内容预览。
    #[serde(default)]
    pub preview: String,
    /// 会话当前保存的消息数量。
    pub message_count: u64,
    /// 最近更新时间的 Unix 毫秒时间戳。
    pub updated_at_ms: u64,
    /// 由 TUI 根据当前时间生成的短标签，例如“10 分钟前”。
    #[serde(default)]
    pub updated_label: String,
    /// 用于恢复前并发校验的会话修订号。
    pub revision: u64,
    /// 是否已被当前宿主标记为活动状态，活动会话不能重复恢复。
    #[serde(default)]
    pub active: bool,
}

/// 会话选择界面的工作模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionSurfaceMode {
    /// 允许选中会话并请求恢复。
    Resume,
    /// 只浏览当前项目的会话列表。
    Browse,
}

/// TUI 向 Command 插件注入的一次会话查询结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceUpdateRequest {
    /// 必须匹配插件最近一次查询 effect 的请求 ID。
    pub request_id: u64,
    /// 异步会话列表的当前状态。
    pub status: SessionListStatus,
}

/// 会话列表的异步加载状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionListStatus {
    /// 宿主正在读取会话摘要。
    Loading,
    /// 已取得一页会话摘要。
    Ready {
        /// 当前页会话摘要。
        items: Vec<SessionSummary>,
        /// 下一页游标；为空表示没有更多结果。
        next_cursor: Option<String>,
    },
    /// 当前查询没有匹配会话。
    Empty,
    /// 查询失败但插件仍保持界面可交互。
    Error {
        /// 面向用户的错误说明。
        message: String,
    },
}

/// Command 插件请求 TUI 执行的会话界面副作用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SurfaceEffect {
    /// 查询当前 `cwd` 下的一页会话摘要。
    QuerySessions {
        /// 用于丢弃过期异步响应的单调请求 ID。
        request_id: u64,
        /// 不区分大小写的用户过滤文本。
        query: String,
        /// 可选分页游标。
        cursor: Option<String>,
        /// 本次查询最多返回的摘要数量。
        limit: u16,
    },
    /// 恢复用户确认选中的当前项目会话。
    ResumeSession {
        /// 待恢复会话 ID。
        session_id: String,
        /// 选择时看到的修订号，供 TUI 进行并发校验。
        revision: u64,
    },
    /// 关闭并取消聚焦插件对话框。
    CloseSurface,
}

/// TUI 轮询会话选择界面动作时取得的响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceEffectsResponse {
    /// 按插件产生顺序排列的待处理动作。
    pub effects: Vec<SurfaceEffect>,
}

/// 已拆分的斜杠命令行。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedCommandLine {
    /// 不含前导斜杠的用户命令名。
    pub name: String,
    /// 经过引号和反斜杠处理的位置参数。
    pub arguments: Vec<String>,
}

impl ParsedCommandLine {
    /// 解析支持单双引号和反斜杠转义的斜杠命令行。
    pub fn parse(input: &str) -> Result<Self, CommandLineError> {
        let trimmed = input.trim();
        let body = trimmed
            .strip_prefix('/')
            .ok_or_else(|| CommandLineError::new("命令必须以 `/` 开头"))?;
        let tokens = split_tokens(body)?;
        let (name, arguments) = tokens
            .split_first()
            .ok_or_else(|| CommandLineError::new("命令名不能为空"))?;
        Ok(Self {
            name: name.clone(),
            arguments: arguments.to_vec(),
        })
    }
}

/// 命令行词法解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLineError {
    message: String,
}

impl CommandLineError {
    /// 创建一个带用户可读说明的解析错误。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回不包含内部实现信息的错误说明。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CommandLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CommandLineError {}

/// 规范化并校验一个不含前导斜杠的命令名。
pub fn canonical_command_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.starts_with('/')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

/// 将一个参数值稳定编码成现有命令行解析器可识别的单个 token。
///
/// 不含语法字符的普通文本保持原样；空值、空白、引号或反斜杠会使用双引号包裹，
/// 并转义双引号和反斜杠。返回文本拼接到斜杠命令后，解析结果始终只包含原始值。
pub fn encode_command_token(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| !character.is_whitespace() && !matches!(character, '\'' | '"' | '\\'))
    {
        return value.into();
    }

    let mut encoded = String::with_capacity(value.len().saturating_add(2));
    encoded.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    encoded.push('"');
    encoded
}

/// 将命令正文拆成参数，同时保留空引号表达的空字符串。
fn split_tokens(input: &str) -> Result<Vec<String>, CommandLineError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Single,
        Double,
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;

    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, character) {
            (Some(Quote::Single), '\'') => quote = None,
            (Some(Quote::Single), _) => current.push(character),
            (Some(Quote::Double), '"') => quote = None,
            (Some(Quote::Double), '\\') => escaped = true,
            (Some(Quote::Double), _) => current.push(character),
            (None, '\'') => {
                quote = Some(Quote::Single);
                started = true;
            }
            (None, '"') => {
                quote = Some(Quote::Double);
                started = true;
            }
            (None, '\\') => {
                escaped = true;
                started = true;
            }
            (None, value) if value.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, value) => {
                current.push(value);
                started = true;
            }
        }
    }

    if escaped {
        return Err(CommandLineError::new("命令末尾存在未完成的转义"));
    }
    if quote.is_some() {
        return Err(CommandLineError::new("命令中存在未闭合的引号"));
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证命令行解析保留引号文本和空参数。
    #[test]
    fn parses_quotes_and_escapes() {
        let parsed = ParsedCommandLine::parse(r#"/deploy "hello world" '' escaped\ value"#)
            .expect("命令应成功解析");
        assert_eq!(parsed.name, "deploy");
        assert_eq!(parsed.arguments, ["hello world", "", "escaped value"]);
    }

    /// 验证补全参数编码能稳定往返空值、空白、单双引号和反斜杠。
    #[test]
    fn completion_token_encoding_round_trips_as_one_argument() {
        for value in [
            "plain",
            "",
            "hello world",
            "single'quote",
            "double\"quote",
            r"back\slash",
            "all 'three' \\\" forms",
        ] {
            let input = format!("/echo {}", encode_command_token(value));
            let parsed = ParsedCommandLine::parse(&input).expect("编码结果应能被命令行解析");
            assert_eq!(parsed.arguments, [value], "未能往返参数：{value:?}");
        }
    }

    /// 验证未闭合引号返回稳定错误而不是接受部分输入。
    #[test]
    fn rejects_unclosed_quote() {
        let error = ParsedCommandLine::parse("/help \"resume").expect_err("未闭合引号必须被拒绝");
        assert_eq!(error.message(), "命令中存在未闭合的引号");
    }

    /// 验证命令名只接受可跨平台传输的 ASCII 字符。
    #[test]
    fn normalizes_command_name() {
        assert_eq!(canonical_command_name(" ReSuMe "), Some("resume".into()));
        assert_eq!(canonical_command_name("bad/name"), None);
        assert_eq!(canonical_command_name("/resume"), None);
    }

    /// 验证没有显式用法时会根据参数定义生成预览文本。
    #[test]
    fn derives_usage_from_arguments() {
        let spec = CommandSpec::new("hello", "问候", "发送问候")
            .with_argument(ArgumentSpec::required("name", "名字", ArgumentKind::String))
            .with_argument(
                ArgumentSpec::optional("extra", "补充文本", ArgumentKind::String).variadic(),
            );
        assert_eq!(spec.display_usage(), "/hello <name> [extra...]");
    }

    /// 验证补全请求缺省值和版本化服务名保持稳定。
    #[test]
    fn completion_request_uses_stable_defaults() {
        let request: PrepareCompletionRequest = serde_json::from_value(serde_json::json!({
            "input": "/deploy pro"
        }))
        .expect("请求应可反序列化");
        assert_eq!(request.cursor, None);
        assert_eq!(request.limit, DEFAULT_COMPLETION_LIMIT);
        assert_eq!(PREPARE_COMPLETION_SERVICE, "command.prepare-completion");
        assert_eq!(PROTOCOL_VERSION, "1.0.0");
    }

    /// 验证可信回调计划在 JSON 中保留替换范围和补全上限。
    #[test]
    fn serializes_typed_callback_completion_plan() {
        let response = PrepareCompletionResponse::Callback {
            context: CompletionContext {
                command: "deploy".into(),
                argument: "target".into(),
                argument_index: 0,
                prefix: "pro".into(),
                replacement_start: 8,
                replacement_end: 11,
            },
            owner_plugin_id: "deploy-plugin".into(),
            service: CALLBACK_SERVICE.into(),
            request: CommandCallbackRequest::Complete {
                handler_id: "deploy-handler".into(),
                request: CommandCompletionRequest {
                    command: "deploy".into(),
                    argument: "target".into(),
                    prefix: "pro".into(),
                    input: "/deploy pro".into(),
                    limit: 12,
                },
            },
        };
        let value = serde_json::to_value(response).expect("响应应可序列化");
        assert_eq!(value["type"], "callback");
        assert_eq!(value["context"]["replacement_start"], 8);
        assert_eq!(value["request"]["request"]["limit"], 12);
    }
}
