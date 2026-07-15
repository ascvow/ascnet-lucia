//! Guest-side WASM plugin SDK for ascnet-lucia.
//!
//! 插件侧 SDK：插件作者实现 [`AgentPlugin`]，然后调用 [`export_plugin!`] 即可导出 WIT world。
//! Guest SDK: implement [`AgentPlugin`] and invoke [`export_plugin!`] to export the WIT world.

#![deny(missing_docs)]

pub use agent_tool::{
    JsonSchema, ToolCall, ToolDecision, ToolDecisionStatus, ToolResult, ToolSpec,
};
pub use anyhow::{anyhow, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};

mod agent_view;

pub use agent_view::AgentViewSession;

#[doc(hidden)]
pub use serde_json as __serde_json;

/// WIT contract used by both host and guest.
/// 宿主和 guest 共用的 WIT 契约。
pub const PLUGIN_WIT: &str = r#"
package ascnet:lucia-plugin@0.7.0;

world plugin {
  import host-agent-upsert-tool: func(request-json: string) -> string;
  import host-agent-remove-tool: func(request-json: string) -> string;
  import host-agent-upsert-prompt: func(request-json: string) -> string;
  import host-agent-remove-prompt: func(request-json: string) -> string;
  import host-agent-emit-event: func(request-json: string) -> string;
  import host-state-get: func(request-json: string) -> string;
  import host-state-set: func(request-json: string) -> string;
  import host-state-remove: func(request-json: string) -> string;
  import host-service-upsert: func(request-json: string) -> string;
  import host-service-remove: func(request-json: string) -> string;
  import host-service-list: func(request-json: string) -> string;
  import host-service-call: func(request-json: string) -> string;
  import host-fs-read: func(request-json: string) -> string;
  import host-fs-list: func(request-json: string) -> string;
  import host-process-spawn: func(request-json: string) -> string;
  import host-process-write: func(request-json: string) -> string;
  import host-process-read-line: func(request-json: string) -> string;
  import host-process-kill: func(request-json: string) -> string;
  import host-model-complete: func(request-json: string) -> string;
  import host-agent-runtime-call: func(request-json: string) -> string;
  export activate: func(context-json: string) -> string;
  export deactivate: func() -> string;
  export handle-service: func(call-json: string) -> string;
  export list-tools: func() -> string;
  export call-tool: func(call-json: string) -> string;
  export before-tool: func(call-json: string) -> string;
  export after-tool: func(result-json: string);
  export on-event: func(event-json: string);
  export load-context: func(request-json: string) -> string;
  export describe-ui: func() -> string;
  export render-ui: func(request-json: string) -> string;
  export on-ui-input: func(input-json: string);
}
"#;

/// Host、Guest SDK 与应用共享的宿主无关 UI 协议。
pub use agent_plugin_protocol::*;

/// Events emitted by ascnet-lucia core.
/// ascnet-lucia core 发出的事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    /// Unique event id.
    /// 唯一事件 ID。
    #[serde(default)]
    pub id: String,

    /// Stable id shared by all events in one agent run.
    /// 同一次 agent run 内所有事件共享的稳定 ID。
    #[serde(default)]
    pub run_id: String,

    /// Unix timestamp in milliseconds.
    /// Unix 毫秒时间戳。
    #[serde(default)]
    pub timestamp_ms: u64,

    /// 事件的稳定语义类型，用于 Guest 选择处理分支。
    pub kind: AgentEventKind,
    /// 事件产生时的 ReAct 步数；run 级事件使用当前或最终步数。
    pub step: usize,
    /// 与事件类型对应的 provider-neutral JSON 载荷。
    pub payload: serde_json::Value,
}

/// Event kinds mirrored from core.
/// 与 core 对齐的事件类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    /// Agent 运行开始。
    RunStarted,
    /// 通用扩展发布的结构化事件。
    Extension,
    /// 单轮模型与工具处理开始。
    TurnStarted,
    /// 模型请求开始。
    ModelRequest,
    /// 模型文本输出增量。
    ModelTextDelta,
    /// 模型推理输出增量。
    ModelThinkingDelta,
    /// 模型响应完成。
    ModelResponse,
    /// 服务商用量或计费信息。
    BillingUsage,
    /// 工具执行开始。
    ToolStarted,
    /// 工具执行完成。
    ToolFinished,
    /// 工具因插话而跳过。
    ToolSkipped,
    /// 单轮模型与工具处理完成。
    TurnFinished,
    /// 插话消息已注入。
    SteeringInjected,
    /// 追加任务消息已注入。
    FollowUpInjected,
    /// Agent 运行完成。
    RunFinished,
    /// Agent 达到最大步数。
    StepLimitReached,
}

/// 插件启动时由宿主提供的只读上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivationContext {
    /// 当前实例可信的插件 ID，来源于 manifest。
    pub plugin_id: String,
    /// manifest 中的自由格式元数据。
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// 官方插件界面支持的语言集合；未知 locale 按英文处理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UiLanguage {
    /// 英文界面，也是缺失或未知 locale 的稳定回退。
    #[default]
    English,
    /// 简体中文界面，兼容 `zh`、`zh-CN` 与 `zh-Hans` 标签。
    SimplifiedChinese,
}

impl UiLanguage {
    /// 从 BCP 47 或 POSIX locale 标签解析支持的界面语言。
    pub fn from_locale(locale: &str) -> Self {
        let normalized = locale.trim().replace('_', "-").to_ascii_lowercase();
        if normalized == "zh" || normalized.starts_with("zh-") {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    /// 在英文与简体中文文案之间选择当前语言对应的值。
    pub fn select<'a>(self, english: &'a str, simplified_chinese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::SimplifiedChinese => simplified_chinese,
        }
    }
}

impl ActivationContext {
    /// 读取 Host 注入的界面语言；旧 Host 未提供时稳定回退到英文。
    pub fn ui_language(&self) -> UiLanguage {
        self.metadata
            .get(HOST_LOCALE_METADATA_KEY)
            .map(|locale| UiLanguage::from_locale(locale))
            .unwrap_or_default()
    }
}

/// 单次模型请求交给上下文插件的 provider-neutral 数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextLoadRequest {
    /// 当前 Agent run 的稳定 ID。
    pub run_id: String,
    /// 当前 ReAct step，从零开始。
    pub step: usize,
    /// 当前逻辑 provider 名称。
    pub provider: String,
    /// 当前模型 ID。
    pub model: String,
    /// 会话顶层 system 提示。
    pub system: Option<String>,
    /// 扩展提示与会话消息组成的 provider-neutral JSON 消息。
    pub messages: Vec<serde_json::Value>,
    /// 是否由用户显式发起（而非模型请求前的自动加载）。
    /// 上下文插件可据此跳过水位判断，无条件执行完整处理。
    #[serde(default)]
    pub user_initiated: bool,
}

/// 上下文插件返回的完整模型输入替换结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LoadedContext {
    /// 实际发送给模型的顶层 system 提示。
    pub system: Option<String>,
    /// 实际发送给模型的全部 provider-neutral JSON 消息。
    pub messages: Vec<serde_json::Value>,
}

/// WASM 上下文导出的稳定响应信封。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContextLoadResponse {
    /// 插件提供的完整替换上下文；`None` 表示本轮显式透传原始上下文。
    pub context: Option<LoadedContext>,
    /// 插件执行失败时返回的错误文本。
    pub error: Option<String>,
}

/// 注入 Agent 请求的 developer 提示贡献。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptContribution {
    /// 插件内部稳定且唯一的提示 ID。
    pub id: String,
    /// 提示正文。
    pub content: String,
    /// 数值较小的提示排在前面。
    #[serde(default)]
    pub priority: i32,
}

/// 插件发布给 Agent 事件流的结构化事件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtensionEvent {
    /// 插件内稳定的事件名称。
    pub name: String,
    /// 与事件关联的协议无关 JSON 数据。
    #[serde(default)]
    pub data: serde_json::Value,
    /// 可选的 UI 展示提示；无界面消费者可以忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<EventPresentation>,
}

impl ExtensionEvent {
    /// Creates a subview navigation event without application-specific semantics.
    /// 创建一条不携带业务语义的子视图导航事件。
    pub fn view_navigation(request: UiNavigationRequest) -> Result<Self> {
        Ok(Self {
            name: UI_NAVIGATION_EVENT.to_string(),
            data: serde_json::to_value(request)?,
            presentation: None,
        })
    }

    /// 创建一条宿主应用级动作事件；需要 manifest 声明 `surface_actions` 能力。
    pub fn host_action(request: UiHostActionRequest) -> Result<Self> {
        Ok(Self {
            name: UI_HOST_ACTION_EVENT.to_string(),
            data: serde_json::to_value(request)?,
            presentation: None,
        })
    }
}

/// 扩展事件希望展示的位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventPresentationTarget {
    /// Lucia 主事件列表。
    MainEventList,
}

/// 扩展事件的展示形态。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventPresentationVariant {
    /// 普通事件文本。
    #[default]
    Text,
    /// 带左右分隔线的事件文本。
    Divider,
}

/// 扩展事件的语义色调。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventPresentationTone {
    /// 普通信息。
    #[default]
    Info,
    /// 成功状态。
    Success,
    /// 警告状态。
    Warning,
    /// 错误状态。
    Error,
    /// 弱化信息。
    Muted,
}

/// 无 UI 框架依赖的扩展事件展示提示。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventPresentation {
    /// 展示目标。
    pub target: EventPresentationTarget,
    /// 展示形态。
    #[serde(default)]
    pub variant: EventPresentationVariant,
    /// 语义色调。
    #[serde(default)]
    pub tone: EventPresentationTone,
    /// 面向用户的事件文本。
    pub text: String,
}

impl EventPresentation {
    /// 创建主事件列表中的分隔线展示。
    pub fn divider(text: impl Into<String>, tone: EventPresentationTone) -> Self {
        Self {
            target: EventPresentationTarget::MainEventList,
            variant: EventPresentationVariant::Divider,
            tone,
            text: text.into(),
        }
    }
}

/// 当前插件公开的协议无关服务声明。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceSpec {
    /// 插件内稳定且唯一的服务名。
    pub name: String,
    /// 服务契约的语义化版本。
    pub version: String,
    /// 面向插件作者的可选说明。
    pub description: Option<String>,
}

/// Host 服务目录返回的服务描述。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceDescriptor {
    /// 提供服务的可信插件 ID。
    pub plugin_id: String,
    /// 提供方插件内的服务名。
    pub name: String,
    /// 服务契约的语义化版本。
    pub version: String,
    /// 面向插件作者的可选说明。
    pub description: Option<String>,
}

/// Host 路由给服务提供方的一次调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceCall {
    /// 调用方插件 ID，由 Host 注入。
    pub caller_id: String,
    /// 当前插件内的服务名。
    pub name: String,
    /// 服务自行定义的 JSON 请求。
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// 通过宿主启动长驻子进程的参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessSpec {
    /// 可执行文件名或绝对路径，不经过 shell 解析。
    pub command: String,
    /// 按原样传递给子进程的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 子进程环境变量；宿主只额外保留少量运行时基础变量。
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 工作目录；相对路径以插件目录为基准。
    pub cwd: Option<String>,
    /// 是否把子进程 stderr 连接到 Lucia 的 stderr。
    #[serde(default)]
    pub inherit_stderr: bool,
}

/// 插件发起的一次受限模型完成请求。
///
/// Provider 和模型由应用注入的 Host 服务固定，插件只能提供上下文与输出上限；Host
/// 不向该请求暴露工具，避免模型摘要等内部调用产生外部副作用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCompletionRequest {
    /// 本轮独立模型调用使用的 system 提示。
    pub system: Option<String>,
    /// 发送给模型的 provider-neutral JSON 消息。
    pub messages: Vec<serde_json::Value>,
    /// 请求的最大输出 token 数；Host 会收窄到应用配置上限。
    pub max_tokens: Option<u32>,
}

/// 宿主模型完成调用返回的文本与用量。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCompletionResponse {
    /// 模型返回的全部文本内容。
    pub text: String,
    /// Provider 返回的可选 token 用量；字段结构与 Core `TokenUsage` 一致。
    pub usage: Option<serde_json::Value>,
}

/// 宿主目录扫描返回的一项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    /// 相对于插件目录的路径；宿主无法相对化时可能为绝对路径。
    pub path: String,
    /// 该路径是否为目录。
    pub is_dir: bool,
}

/// Agent Runtime 返回的稳定 Agent 标识。
///
/// 标识由 Runtime 生成；插件只能保存和回传，不能借此伪造发送者或 owner。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    /// 从外部持久化值恢复标识；Host 会在实际调用时继续校验 UUID 格式和访问权限。
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(anyhow!("Agent ID 不能为空"));
        }
        Ok(Self(value))
    }

    /// 返回不透明标识字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Agent 在当前派生树中的父子关系。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLineage {
    /// 直接父节点；controller 根节点为 `None`。
    pub parent: Option<AgentId>,
    /// 当前派生树的 controller 根节点。
    pub root: AgentId,
    /// 当前节点深度；controller 根节点为零。
    pub depth: usize,
}

/// 派生 Agent 实际持有的工具访问范围。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", content = "tools", rename_all = "snake_case")]
pub enum AgentToolAccess {
    /// 继承 controller profile 允许的全部工具。
    #[default]
    All,
    /// 只允许集合中列出的工具。
    Allowlist(BTreeSet<String>),
}

/// 派生 Agent 实际生效的权限快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPermissions {
    /// 模型可见且可实际执行的工具范围。
    #[serde(default)]
    pub tools: AgentToolAccess,
}

/// 插件启动派生 Agent 的受限请求。
///
/// `profile` 只能引用 manifest 和应用注册表共同授权的派生策略。模型、服务商、
/// provider options 和工具范围不会直接穿过插件 ABI。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpawnRequest {
    /// 应用注册的派生策略名称。
    pub profile: String,
    /// 交给派生 Agent 的首次用户输入。
    pub input: String,
}

impl AgentSpawnRequest {
    /// 创建一条使用指定策略的派生请求。
    pub fn new(profile: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            input: input.into(),
        }
    }
}

/// 插件基于成功终态会话启动后续 Agent 运行的受限请求。
///
/// Host 只接收目标句柄与新增用户输入；原始 Session、存储实现和模型凭证不会进入 Guest。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentContinueRequest {
    /// 当前 controller 有权管理且已成功结束的 Agent 身份。
    pub target: AgentId,
    /// 追加到目标私有会话的新用户输入。
    pub input: String,
}

impl AgentContinueRequest {
    /// 创建一条基于目标成功会话的后续运行请求。
    pub fn new(target: AgentId, input: impl Into<String>) -> Self {
        Self {
            target,
            input: input.into(),
        }
    }
}

/// 已启动 Agent 的稳定句柄。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentHandle {
    /// Agent 身份，也是状态、结果和取消操作的查询键。
    pub id: AgentId,
    /// Agent 的父子谱系。
    pub lineage: AgentLineage,
}

/// Agent 的执行状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// controller 已就绪且没有后台执行任务。
    Ready,
    /// 派生任务正在等待并发许可。
    Queued,
    /// Core Agent 循环正在执行。
    Running,
    /// 执行成功。
    Succeeded,
    /// 执行失败。
    Failed,
    /// 已由管理方取消。
    Cancelled,
}

impl AgentStatus {
    /// 判断状态是否已经进入不可覆盖的终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// 派生 Agent 的 token 用量摘要。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTokenUsage {
    /// 输入 token 数；服务商未返回时为 `None`。
    pub input_tokens: Option<u64>,
    /// 输出 token 数；服务商未返回时为 `None`。
    pub output_tokens: Option<u64>,
    /// 总 token 数；服务商未返回时为 `None`。
    pub total_tokens: Option<u64>,
}

/// 派生 Agent 成功执行后的可序列化摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentExecutionResult {
    /// Core Agent 生成的运行 ID。
    pub run_id: String,
    /// 最终可见文本。
    pub final_text: String,
    /// 实际使用的 ReAct 步数。
    pub steps_used: usize,
    /// 服务商返回的 token 用量。
    pub usage: AgentTokenUsage,
}

/// 派生 Agent 的幂等终态结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentOutcome {
    /// Core Agent 正常完成。
    Succeeded {
        /// 可序列化执行摘要。
        result: AgentExecutionResult,
    },
    /// Core Agent 返回错误或后台任务发生 panic。
    Failed {
        /// 供诊断和展示的错误信息。
        error: String,
    },
    /// 执行由管理方取消。
    Cancelled,
}

/// Agent 状态查询快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSnapshot {
    /// Agent 身份。
    pub id: AgentId,
    /// Agent 的父子谱系。
    pub lineage: AgentLineage,
    /// 查询时的执行状态。
    pub status: AgentStatus,
    /// 当前实际生效的权限。
    pub permissions: AgentPermissions,
}

/// 插件可调用的协议无关宿主 API。
pub trait PluginHostApi {
    /// 注册或替换工具，返回暴露给模型的公开工具名。
    fn upsert_tool(&self, local_name: &str, spec: &ToolSpec) -> Result<String>;

    /// 按公开工具名删除工具；不存在时保持幂等。
    fn remove_tool(&self, public_name: &str) -> Result<()>;

    /// 注册或替换一条 developer 提示贡献。
    fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String>;

    /// 按插件内部提示 ID 删除贡献；不存在时保持幂等。
    fn remove_prompt(&self, id: &str) -> Result<()>;

    /// 发布一条结构化扩展事件。
    fn emit_event(&self, event: &ExtensionEvent) -> Result<()>;

    /// Requests one idempotent navigation operation for a plugin subview.
    /// 请求应用对插件子视图执行一次幂等导航。
    fn navigate_view(&self, request: UiNavigationRequest) -> Result<()> {
        self.emit_event(&ExtensionEvent::view_navigation(request)?)
    }

    /// 读取插件实例内存状态；实例卸载后状态不会保留。
    fn get_state(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// 写入插件实例内存状态。
    fn set_state(&self, key: &str, value: &serde_json::Value) -> Result<()>;

    /// 删除插件实例内存状态并返回旧值。
    fn remove_state(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// 注册或替换当前插件拥有的服务。
    fn upsert_service(&self, service: &ServiceSpec) -> Result<()>;

    /// 删除当前插件拥有的服务；不存在时保持幂等。
    fn remove_service(&self, name: &str) -> Result<()>;

    /// 查询全部服务或指定插件的服务。
    fn list_services(&self, plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>>;

    /// 调用目标插件服务并返回服务自行定义的 JSON。
    fn call_service(
        &self,
        plugin_id: &str,
        name: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value>;

    /// 读取 manifest `fs_read` 允许范围内的 UTF-8 文件。
    fn read_file(&self, path: &str) -> Result<String>;

    /// 列出 manifest `fs_read` 允许范围内的一层目录项。
    fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>>;

    /// 启动长驻子进程并返回实例内有效的句柄。
    fn spawn_process(&self, spec: &ProcessSpec) -> Result<u64>;

    /// 向子进程 stdin 原样写入数据。
    fn write_process(&self, handle: u64, data: &str) -> Result<()>;

    /// 在超时内读取一行 stdout；进程关闭 stdout 时返回 `None`。
    fn read_process_line(&self, handle: u64, timeout_ms: u64) -> Result<Option<String>>;

    /// 终止并释放子进程句柄。
    fn kill_process(&self, handle: u64) -> Result<()>;

    /// 使用应用固定的 provider 和模型执行一次无工具模型完成。
    fn complete_model(&self, _request: &ModelCompletionRequest) -> Result<ModelCompletionResponse> {
        Err(anyhow!("宿主未提供模型完成 API"))
    }

    /// 返回分配给当前插件激活实例的 controller Agent 身份。
    fn agent_identity(&self) -> Result<AgentId> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 使用 manifest 与应用共同授权的 profile 启动派生 Agent。
    ///
    /// 该调用只等待任务入队并立即返回句柄，不等待模型运行结束。
    fn spawn_agent(&self, _request: &AgentSpawnRequest) -> Result<AgentHandle> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 从当前 controller 可管理的成功终态会话启动后续运行。
    ///
    /// 该调用只创建后台运行并返回新句柄，不返回原始会话，也不等待模型执行结束。
    fn continue_agent(&self, _request: &AgentContinueRequest) -> Result<AgentHandle> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 向排队或运行中的 Agent 注入实时用户消息。
    fn steer_agent(&self, _target: &AgentId, _input: &str) -> Result<()> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 查询当前 controller 或其后代 Agent 的状态。
    fn agent_status(&self, _target: &AgentId) -> Result<AgentSnapshot> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 查询派生 Agent 的终态结果；尚未结束时返回 `None`。
    fn agent_result(&self, _target: &AgentId) -> Result<Option<AgentOutcome>> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 非阻塞读取派生 Agent 的历史回放和后续实时事件。
    fn agent_events(&self, _target: &AgentId, _limit: usize) -> Result<Vec<AgentEvent>> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }

    /// 级联取消当前 controller 的指定后代 Agent。
    fn cancel_agent(&self, _target: &AgentId) -> Result<bool> {
        Err(anyhow!("宿主未提供 Agent Runtime API"))
    }
}

/// Trait implemented by a WASM plugin.
/// WASM 插件需要实现的 trait。
///
/// 这个 trait 对插件保持同步；文件、进程等异步工作由 [`PluginHostApi`] 在宿主侧完成。
pub trait AgentPlugin: Default + Send + 'static {
    /// 插件实例创建后的启动钩子。
    ///
    /// 动态扫描、提示贡献和工具注册应在这里完成。返回错误会阻止插件加载。
    fn activate(&mut self, _host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        Ok(())
    }

    /// 插件实例卸载前的清理钩子。
    ///
    /// 插件应在这里终止通过宿主启动的长驻任务并移除临时贡献。
    fn deactivate(&mut self, _host: &dyn PluginHostApi) -> Result<()> {
        Ok(())
    }

    /// Return tools implemented by this plugin.
    /// 返回该插件实现的工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// Execute a tool call.
    /// 执行工具调用；纯上下文或纯 UI 插件默认返回未知工具错误。
    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        Ok(ToolResult::error(call.id, call.name, "插件未实现工具调用"))
    }

    /// 使用宿主能力执行工具调用。
    ///
    /// 默认调用兼容旧插件的 [`AgentPlugin::call_tool`]；需要文件或子进程能力的插件覆盖此方法。
    fn call_tool_with_host(
        &mut self,
        _host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        self.call_tool(call)
    }

    /// 处理其他插件通过 Host 路由过来的服务调用。
    ///
    /// 只有先通过 [`PluginHostApi::upsert_service`] 注册的服务才会到达这里。
    fn handle_service(
        &mut self,
        _host: &dyn PluginHostApi,
        call: ServiceCall,
    ) -> Result<serde_json::Value> {
        Err(anyhow!("未实现插件服务 `{}`", call.name))
    }

    /// Hook before any tool is executed.
    /// 任意工具执行前 hook。
    ///
    /// 返回 `Pending` 时 Host 会稍后重新调用；等待原因、交互和持久化协议由插件自行维护。
    fn before_tool(&mut self, _call: ToolCall) -> ToolDecisionStatus {
        ToolDecision::Allow.into()
    }

    /// Hook after any tool is executed.
    /// 任意工具执行后 hook。
    fn after_tool(&mut self, _result: ToolResult) {}

    /// Receive core lifecycle events.
    /// 接收 core 生命周期事件。
    fn on_event(&mut self, _event: AgentEvent) {}

    /// 为一次模型请求返回完整替换上下文；默认不参与上下文加载。
    ///
    /// 返回 `None` 表示本轮显式透传：宿主继续使用原始完整历史，
    /// 无需把未修改的上下文跨 WASM 边界回传。
    fn load_context(
        &mut self,
        _host: &dyn PluginHostApi,
        _request: ContextLoadRequest,
    ) -> Result<Option<LoadedContext>> {
        Ok(None)
    }

    /// 声明插件提供的终端视图；默认不提供界面。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        Vec::new()
    }

    /// 声明插件自有工具在消息列表中的渲染器；默认使用宿主通用工具样式。
    fn describe_tool_renderers(&self) -> Vec<ToolRendererContribution> {
        Vec::new()
    }

    /// 根据宿主分配的尺寸渲染指定视图；返回 `None` 表示该帧不更新。
    fn render_ui(&mut self, _request: UiRenderRequest) -> Option<UiFrame> {
        None
    }

    /// 使用宿主 API 渲染视图；默认转发给 [`AgentPlugin::render_ui`]。
    fn render_ui_with_host(
        &mut self,
        _host: &dyn PluginHostApi,
        request: UiRenderRequest,
    ) -> Option<UiFrame> {
        self.render_ui(request)
    }

    /// 渲染插件自有工具在消息列表中的内容；默认交给宿主通用工具样式。
    fn render_tool(&mut self, _request: ToolRenderRequest) -> Option<UiFrame> {
        None
    }

    /// 使用宿主 API 渲染工具消息；默认转发给 [`AgentPlugin::render_tool`]。
    fn render_tool_with_host(
        &mut self,
        _host: &dyn PluginHostApi,
        request: ToolRenderRequest,
    ) -> Option<UiFrame> {
        self.render_tool(request)
    }

    /// 处理宿主路由给焦点视图的输入事件。
    fn on_ui_input(&mut self, _input: UiInput) {}

    /// Handles view input with Host APIs; the default forwards to [`AgentPlugin::on_ui_input`].
    /// 使用宿主 API 处理视图输入；默认转发给旧的 [`AgentPlugin::on_ui_input`]。
    fn on_ui_input_with_host(&mut self, _host: &dyn PluginHostApi, input: UiInput) {
        self.on_ui_input(input);
    }
}

/// Serialize a value to a JSON string for the WIT ABI.
/// 将值序列化为 WIT ABI 使用的 JSON 字符串。
pub fn to_json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| {
        json!({
            "type": "block",
            "reason": format!("failed to serialize plugin response: {err}")
        })
        .to_string()
    })
}

/// Deserialize a value from a JSON string crossing the WIT ABI.
/// 从跨越 WIT ABI 的 JSON 字符串反序列化。
pub fn from_json_string<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|err| anyhow!("invalid plugin ABI JSON: {err}"))
}

/// 解码宿主能力 API 返回的 JSON 信封。
#[doc(hidden)]
pub fn decode_host_response<T: DeserializeOwned>(text: &str) -> Result<T> {
    #[derive(Deserialize)]
    struct HostResponse {
        ok: bool,
        #[serde(default)]
        value: serde_json::Value,
        error: Option<String>,
    }

    let response: HostResponse = from_json_string(text)?;
    if !response.ok {
        return Err(anyhow!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "宿主能力调用失败".to_string())
        ));
    }
    serde_json::from_value(response.value)
        .map_err(|error| anyhow!("宿主能力返回值类型错误：{error}"))
}

/// Export an [`AgentPlugin`] implementation as a WASM Component Model world.
/// 将 [`AgentPlugin`] 实现导出为 WASM Component Model world。
///
/// Usage / 用法：
///
/// ```ignore
/// use agent_plugin::{export_plugin, AgentPlugin};
///
/// #[derive(Default)]
/// struct MyPlugin;
///
/// impl AgentPlugin for MyPlugin { /* ... */ }
///
/// export_plugin!(MyPlugin);
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($plugin_ty:ty) => {
        mod __ascnet_lucia_component_export {
            use super::*;

            wit_bindgen::generate!({
                path: [],
                inline: r#"
package ascnet:lucia-plugin@0.7.0;

world plugin {
  import host-agent-upsert-tool: func(request-json: string) -> string;
  import host-agent-remove-tool: func(request-json: string) -> string;
  import host-agent-upsert-prompt: func(request-json: string) -> string;
  import host-agent-remove-prompt: func(request-json: string) -> string;
  import host-agent-emit-event: func(request-json: string) -> string;
  import host-state-get: func(request-json: string) -> string;
  import host-state-set: func(request-json: string) -> string;
  import host-state-remove: func(request-json: string) -> string;
  import host-service-upsert: func(request-json: string) -> string;
  import host-service-remove: func(request-json: string) -> string;
  import host-service-list: func(request-json: string) -> string;
  import host-service-call: func(request-json: string) -> string;
  import host-fs-read: func(request-json: string) -> string;
  import host-fs-list: func(request-json: string) -> string;
  import host-process-spawn: func(request-json: string) -> string;
  import host-process-write: func(request-json: string) -> string;
  import host-process-read-line: func(request-json: string) -> string;
  import host-process-kill: func(request-json: string) -> string;
  import host-model-complete: func(request-json: string) -> string;
  import host-agent-runtime-call: func(request-json: string) -> string;
  export activate: func(context-json: string) -> string;
  export deactivate: func() -> string;
  export handle-service: func(call-json: string) -> string;
  export list-tools: func() -> string;
  export call-tool: func(call-json: string) -> string;
  export before-tool: func(call-json: string) -> string;
  export after-tool: func(result-json: string);
  export on-event: func(event-json: string);
  export load-context: func(request-json: string) -> string;
  export describe-ui: func() -> string;
  export render-ui: func(request-json: string) -> string;
  export on-ui-input: func(input-json: string);
}
"#,
                world: "plugin",
            });

            struct Component;

            struct ComponentHostApi;

            impl $crate::PluginHostApi for ComponentHostApi {
                fn upsert_tool(
                    &self,
                    local_name: &str,
                    spec: &$crate::ToolSpec,
                ) -> $crate::Result<String> {
                    let request = $crate::__serde_json::json!({
                        "local_name": local_name,
                        "spec": spec,
                    });
                    $crate::decode_host_response(&host_agent_upsert_tool(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn remove_tool(&self, public_name: &str) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({"name": public_name});
                    $crate::decode_host_response(&host_agent_remove_tool(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn upsert_prompt(
                    &self,
                    prompt: &$crate::PromptContribution,
                ) -> $crate::Result<String> {
                    $crate::decode_host_response(&host_agent_upsert_prompt(
                        &$crate::to_json_string(prompt),
                    ))
                }

                fn remove_prompt(&self, id: &str) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({"id": id});
                    $crate::decode_host_response(&host_agent_remove_prompt(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn emit_event(&self, event: &$crate::ExtensionEvent) -> $crate::Result<()> {
                    $crate::decode_host_response(&host_agent_emit_event(
                        &$crate::to_json_string(event),
                    ))
                }

                fn get_state(
                    &self,
                    key: &str,
                ) -> $crate::Result<Option<$crate::__serde_json::Value>> {
                    let request = $crate::__serde_json::json!({"key": key});
                    $crate::decode_host_response(&host_state_get(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn set_state(
                    &self,
                    key: &str,
                    value: &$crate::__serde_json::Value,
                ) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({
                        "key": key,
                        "value": value,
                    });
                    $crate::decode_host_response(&host_state_set(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn remove_state(
                    &self,
                    key: &str,
                ) -> $crate::Result<Option<$crate::__serde_json::Value>> {
                    let request = $crate::__serde_json::json!({"key": key});
                    $crate::decode_host_response(&host_state_remove(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn upsert_service(&self, service: &$crate::ServiceSpec) -> $crate::Result<()> {
                    $crate::decode_host_response(&host_service_upsert(
                        &$crate::to_json_string(service),
                    ))
                }

                fn remove_service(&self, name: &str) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({"name": name});
                    $crate::decode_host_response(&host_service_remove(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn list_services(
                    &self,
                    plugin_id: Option<&str>,
                ) -> $crate::Result<Vec<$crate::ServiceDescriptor>> {
                    let request = $crate::__serde_json::json!({"plugin_id": plugin_id});
                    $crate::decode_host_response(&host_service_list(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn call_service(
                    &self,
                    plugin_id: &str,
                    name: &str,
                    payload: &$crate::__serde_json::Value,
                ) -> $crate::Result<$crate::__serde_json::Value> {
                    let request = $crate::__serde_json::json!({
                        "plugin_id": plugin_id,
                        "name": name,
                        "payload": payload,
                    });
                    $crate::decode_host_response(&host_service_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn read_file(&self, path: &str) -> $crate::Result<String> {
                    let request = $crate::__serde_json::json!({"path": path});
                    $crate::decode_host_response(&host_fs_read(&$crate::to_json_string(&request)))
                }

                fn list_dir(&self, path: &str) -> $crate::Result<Vec<$crate::FileEntry>> {
                    let request = $crate::__serde_json::json!({"path": path});
                    $crate::decode_host_response(&host_fs_list(&$crate::to_json_string(&request)))
                }

                fn spawn_process(&self, spec: &$crate::ProcessSpec) -> $crate::Result<u64> {
                    $crate::decode_host_response(&host_process_spawn(
                        &$crate::to_json_string(spec),
                    ))
                }

                fn write_process(&self, handle: u64, data: &str) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({
                        "handle": handle,
                        "data": data,
                    });
                    $crate::decode_host_response(&host_process_write(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn read_process_line(
                    &self,
                    handle: u64,
                    timeout_ms: u64,
                ) -> $crate::Result<Option<String>> {
                    let request = $crate::__serde_json::json!({
                        "handle": handle,
                        "timeout_ms": timeout_ms,
                    });
                    $crate::decode_host_response(&host_process_read_line(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn kill_process(&self, handle: u64) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({"handle": handle});
                    $crate::decode_host_response(&host_process_kill(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn complete_model(
                    &self,
                    request: &$crate::ModelCompletionRequest,
                ) -> $crate::Result<$crate::ModelCompletionResponse> {
                    $crate::decode_host_response(&host_model_complete(
                        &$crate::to_json_string(request),
                    ))
                }

                fn agent_identity(&self) -> $crate::Result<$crate::AgentId> {
                    let request = $crate::__serde_json::json!({
                        "operation": "identity",
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn spawn_agent(
                    &self,
                    request: &$crate::AgentSpawnRequest,
                ) -> $crate::Result<$crate::AgentHandle> {
                    let request = $crate::__serde_json::json!({
                        "operation": "spawn",
                        "request": request,
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn continue_agent(
                    &self,
                    request: &$crate::AgentContinueRequest,
                ) -> $crate::Result<$crate::AgentHandle> {
                    let request = $crate::__serde_json::json!({
                        "operation": "continue",
                        "request": request,
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn steer_agent(
                    &self,
                    target: &$crate::AgentId,
                    input: &str,
                ) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({
                        "operation": "steer",
                        "request": {"target": target, "input": input},
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn agent_status(
                    &self,
                    target: &$crate::AgentId,
                ) -> $crate::Result<$crate::AgentSnapshot> {
                    let request = $crate::__serde_json::json!({
                        "operation": "status",
                        "request": {"target": target},
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn agent_result(
                    &self,
                    target: &$crate::AgentId,
                ) -> $crate::Result<Option<$crate::AgentOutcome>> {
                    let request = $crate::__serde_json::json!({
                        "operation": "result",
                        "request": {"target": target},
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn agent_events(
                    &self,
                    target: &$crate::AgentId,
                    limit: usize,
                ) -> $crate::Result<Vec<$crate::AgentEvent>> {
                    let request = $crate::__serde_json::json!({
                        "operation": "events",
                        "request": {"target": target, "limit": limit},
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn cancel_agent(&self, target: &$crate::AgentId) -> $crate::Result<bool> {
                    let request = $crate::__serde_json::json!({
                        "operation": "cancel",
                        "request": {"target": target},
                    });
                    $crate::decode_host_response(&host_agent_runtime_call(
                        &$crate::to_json_string(&request),
                    ))
                }

            }

            static PLUGIN: std::sync::OnceLock<std::sync::Mutex<$plugin_ty>> =
                std::sync::OnceLock::new();

            fn with_plugin<R>(f: impl FnOnce(&mut $plugin_ty) -> R) -> R {
                // Keep one plugin instance alive across calls.
                // 在多次调用之间保留同一个插件实例。
                let lock = PLUGIN.get_or_init(|| std::sync::Mutex::new(<$plugin_ty>::default()));
                let mut plugin = lock.lock().expect("ascnet-lucia plugin mutex poisoned");
                f(&mut *plugin)
            }

            impl Guest for Component {
                fn activate(context_json: String) -> String {
                    let context: $crate::ActivationContext =
                        match $crate::from_json_string(&context_json) {
                            Ok(context) => context,
                            Err(error) => return error.to_string(),
                        };
                    with_plugin(|plugin| plugin.activate(&ComponentHostApi, context))
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default()
                }

                fn deactivate() -> String {
                    with_plugin(|plugin| plugin.deactivate(&ComponentHostApi))
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default()
                }

                fn handle_service(call_json: String) -> String {
                    let call: $crate::ServiceCall = match $crate::from_json_string(&call_json) {
                        Ok(call) => call,
                        Err(error) => {
                            return $crate::__serde_json::json!({
                                "ok": false,
                                "error": error.to_string(),
                            })
                            .to_string();
                        }
                    };
                    match with_plugin(|plugin| {
                        plugin.handle_service(&ComponentHostApi, call)
                    }) {
                        Ok(value) => $crate::__serde_json::json!({
                            "ok": true,
                            "value": value,
                        })
                        .to_string(),
                        Err(error) => $crate::__serde_json::json!({
                            "ok": false,
                            "error": format!("{error:#}"),
                        })
                        .to_string(),
                    }
                }

                fn list_tools() -> String {
                    with_plugin(|plugin| $crate::to_json_string(&plugin.list_tools()))
                }

                fn call_tool(call_json: String) -> String {
                    let call: $crate::ToolCall = match $crate::from_json_string(&call_json) {
                        Ok(call) => call,
                        Err(err) => {
                            return $crate::to_json_string(&$crate::ToolResult::error(
                                "invalid-call",
                                "invalid-tool",
                                err.to_string(),
                            ));
                        }
                    };

                    let fallback_id = call.id.clone();
                    let fallback_name = call.name.clone();
                    let result = with_plugin(|plugin| {
                        plugin.call_tool_with_host(&ComponentHostApi, call)
                    });
                    match result {
                        Ok(result) => $crate::to_json_string(&result),
                        Err(err) => $crate::to_json_string(&$crate::ToolResult::error(
                            fallback_id,
                            fallback_name,
                            err.to_string(),
                        )),
                    }
                }

                fn before_tool(call_json: String) -> String {
                    let call: $crate::ToolCall = match $crate::from_json_string(&call_json) {
                        Ok(call) => call,
                        Err(err) => {
                            let decision: $crate::ToolDecisionStatus = $crate::ToolDecision::Block {
                                reason: err.to_string(),
                            }
                            .into();
                            return $crate::to_json_string(&decision);
                        }
                    };
                    with_plugin(|plugin| $crate::to_json_string(&plugin.before_tool(call)))
                }

                fn after_tool(result_json: String) {
                    if let Ok(result) = $crate::from_json_string::<$crate::ToolResult>(&result_json)
                    {
                        with_plugin(|plugin| plugin.after_tool(result));
                    }
                }

                fn on_event(event_json: String) {
                    if let Ok(event) = $crate::from_json_string::<$crate::AgentEvent>(&event_json) {
                        with_plugin(|plugin| plugin.on_event(event));
                    }
                }

                fn load_context(request_json: String) -> String {
                    let request = match $crate::from_json_string::<$crate::ContextLoadRequest>(
                        &request_json,
                    ) {
                        Ok(request) => request,
                        Err(error) => {
                            return $crate::to_json_string(&$crate::ContextLoadResponse {
                                context: None,
                                error: Some(error.to_string()),
                            });
                        }
                    };
                    let response = match with_plugin(|plugin| {
                        plugin.load_context(&ComponentHostApi, request)
                    }) {
                        Ok(context) => $crate::ContextLoadResponse {
                            context,
                            error: None,
                        },
                        Err(error) => $crate::ContextLoadResponse {
                            context: None,
                            error: Some(format!("{error:#}")),
                        },
                    };
                    $crate::to_json_string(&response)
                }

                fn describe_ui() -> String {
                    with_plugin(|plugin| {
                        let mut contributions = plugin
                            .describe_ui()
                            .into_iter()
                            .map($crate::UiContribution::View)
                            .collect::<Vec<_>>();
                        contributions.extend(
                            plugin
                                .describe_tool_renderers()
                                .into_iter()
                                .map($crate::UiContribution::ToolRenderer),
                        );
                        $crate::to_json_string(&contributions)
                    })
                }

                fn render_ui(request_json: String) -> String {
                    let request: $crate::UiRenderRequestEnvelope =
                        match $crate::from_json_string(&request_json) {
                            Ok(request) => request,
                            Err(_) => return String::new(),
                        };
                    with_plugin(|plugin| {
                        let frame = match request {
                            $crate::UiRenderRequestEnvelope::View(request) => {
                                plugin.render_ui_with_host(&ComponentHostApi, request)
                            }
                            $crate::UiRenderRequestEnvelope::Tool(request) => {
                                plugin.render_tool_with_host(&ComponentHostApi, *request)
                            }
                        };
                        frame
                            .map(|frame| $crate::to_json_string(&frame))
                            .unwrap_or_default()
                    })
                }

                fn on_ui_input(input_json: String) {
                    if let Ok(input) = $crate::from_json_string::<$crate::UiInput>(&input_json) {
                        with_plugin(|plugin| {
                            plugin.on_ui_input_with_host(&ComponentHostApi, input)
                        });
                    }
                }
            }

            export!(Component);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The navigation helper emits a stable name and request data without presentation effects.
    /// 导航便捷构造器应生成稳定事件名和无展示副作用的请求数据。
    #[test]
    fn view_navigation_event_uses_stable_protocol() {
        let event = ExtensionEvent::view_navigation(UiNavigationRequest {
            request_id: "open-1".into(),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: "agent-detail".into(),
                    instance_id: "agent-1".into(),
                    title: None,
                },
            },
        })
        .expect("创建导航事件");

        assert_eq!(event.name, UI_NAVIGATION_EVENT);
        assert_eq!(event.data["action"]["action"], "push");
        assert_eq!(event.data["action"]["view"]["instance_id"], "agent-1");
        assert_eq!(event.presentation, None);
    }

    /// 激活上下文应识别常见中文标签，缺失或不支持的语言稳定回退到英文。
    #[test]
    fn activation_context_selects_supported_ui_language() {
        let mut context = ActivationContext {
            plugin_id: "ui-plugin".into(),
            metadata: HashMap::new(),
        };
        assert_eq!(context.ui_language(), UiLanguage::English);

        for locale in ["zh", "zh_CN.UTF-8", "zh-Hans-CN", "zh-TW"] {
            context
                .metadata
                .insert(HOST_LOCALE_METADATA_KEY.into(), locale.into());
            assert_eq!(context.ui_language(), UiLanguage::SimplifiedChinese);
        }

        context
            .metadata
            .insert(HOST_LOCALE_METADATA_KEY.into(), "ja-JP".into());
        assert_eq!(context.ui_language(), UiLanguage::English);
    }
}
