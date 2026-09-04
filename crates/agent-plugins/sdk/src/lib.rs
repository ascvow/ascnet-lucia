//! Guest-side WASM plugin SDK for ascnet-lucia.
//!
//! 插件侧 SDK：插件作者实现 [`AgentPlugin`]，然后调用 [`export_plugin!`] 即可导出 WIT world。
//! Guest SDK: implement [`AgentPlugin`] and invoke [`export_plugin!`] to export the WIT world.

#![deny(missing_docs)]

pub use agent_tool::{
    is_sensitive_path, JsonSchema, ToolCall, ToolPermissionScope, ToolResult, ToolSpec,
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
package ascnet:lucia-plugin@0.10.0;

world plugin {
  import host-agent-upsert-tool: func(request-json: string) -> string;
  import host-agent-remove-tool: func(request-json: string) -> string;
  import host-agent-upsert-prompt: func(request-json: string) -> string;
  import host-agent-remove-prompt: func(request-json: string) -> string;
  import host-agent-emit-event: func(request-json: string) -> string;
  import host-state-get: func(request-json: string) -> string;
  import host-state-set: func(request-json: string) -> string;
  import host-state-remove: func(request-json: string) -> string;
  import host-store-get: func(request-json: string) -> string;
  import host-store-set: func(request-json: string) -> string;
  import host-store-remove: func(request-json: string) -> string;
  import host-store-list: func(request-json: string) -> string;
  import host-vector-open: func(request-json: string) -> string;
  import host-vector-sync: func(request-json: string) -> string;
  import host-vector-upsert: func(request-json: string) -> string;
  import host-vector-search: func(request-json: string) -> string;
  import host-vector-remove: func(request-json: string) -> string;
  import host-session-list: func(request-json: string) -> string;
  import host-session-read: func(request-json: string) -> string;
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
  import host-http-request: func(request-json: string) -> string;
  import host-model-complete: func(request-json: string) -> string;
  import host-agent-runtime-call: func(request-json: string) -> string;
  export activate: func(context-json: string) -> string;
  export deactivate: func() -> string;
  export handle-service: func(call-json: string) -> string;
  export list-tools: func() -> string;
  export call-tool: func(call-json: string) -> string;
  export on-event: func(envelope-json: string) -> string;
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
    /// 模型推理输出增量；为协议兼容保留，Host 不向第三方 Guest 下发此敏感事件。
    ModelThinkingDelta,
    /// 模型响应完成。
    ModelResponse,
    /// 服务商用量或计费信息。
    BillingUsage,
    /// 工具执行开始。
    ToolStarted,
    /// 工具运行期间产生增量输出。
    ToolOutputDelta,
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
    /// 扩展提示与会话消息组成的 provider-neutral JSON 消息；Host 已移除服务商私有推理块。
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
    /// 实际发送给模型的全部 provider-neutral JSON 消息；Host 会再次移除私有推理块。
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

    /// 创建一条宿主应用级动作事件；需要 manifest 在 `surface_action_allowlist` 授权该动作。
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
    /// 应用底部状态栏；同一可信插件来源的新状态会覆盖旧状态。
    Footer,
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

    /// 创建应用底部状态栏中的持久状态展示。
    ///
    /// 宿主按可信插件来源保存最近一条状态；不支持底栏的消费者可以安全忽略该提示。
    pub fn footer(text: impl Into<String>, tone: EventPresentationTone) -> Self {
        Self {
            target: EventPresentationTarget::Footer,
            variant: EventPresentationVariant::Text,
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

/// 插件提交给宿主的一次受限 HTTP 请求。
///
/// 请求只能携带相对路径和非敏感 header。目标主机、认证 header、超时和响应上限由 Host
/// 根据 manifest 与应用配置注入，Guest 不能借此访问任意地址或读取 API 密钥。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpRequest {
    /// HTTP 方法，例如 `GET` 或 `POST`。
    pub method: String,
    /// 相对于 Host 配置 base URL 的路径，可带查询字符串。
    pub path: String,
    /// 由插件提交的非敏感请求 header。
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// 可选 UTF-8 请求正文；二进制协议应在插件层自行编码。
    #[serde(default)]
    pub body: Option<String>,
}

/// 宿主执行受限 HTTP 请求后返回的响应快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP 状态码；非 2xx 状态由调用方按具体协议解释。
    pub status: u16,
    /// 经 Host 清理后的响应 header。
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// UTF-8 响应正文；超过 Host 上限的响应会在读取前失败。
    #[serde(default)]
    pub body: String,
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

/// 向量库中的一条记录。
///
/// `path` 与 `revision` 是宿主唯一理解的两个附加字段：前者供检索时按前缀过滤，后者供对账
/// 判断向量是否已经过期。插件不需要其中某一项时留空即可。
#[derive(Debug, Clone, PartialEq)]
pub struct VectorRecord {
    /// 插件自定义的稳定记录 ID。
    pub record_id: String,
    /// 供前缀过滤的路径；不需要过滤时为空串。
    pub path: String,
    /// 记录版本标识，通常是内容摘要。
    pub revision: Option<String>,
    /// 向量分量；长度必须与已绑定的维度一致。
    pub vector: Vec<f32>,
}

/// 一条向量检索命中。
#[derive(Debug, Clone, PartialEq)]
pub struct VectorHit {
    /// 命中的记录 ID。
    pub record_id: String,
    /// 记录路径。
    pub path: String,
    /// 余弦相似度，越大越相关。
    pub score: f32,
}

/// 对账中提交的一条记录标识。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorSyncEntry {
    /// 记录 ID。
    pub record_id: String,
    /// 期望的记录版本；`None` 表示只要存在即视为最新。
    pub revision: Option<String>,
}

/// 一次对账的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorSyncOutcome {
    /// 缺失或版本不符、需要重新计算向量的记录 ID。
    pub stale: Vec<String>,
    /// 本次对账收尾时删除的过期记录数；非收尾批次为 0。
    pub removed: u64,
}

/// 把向量编码为 ABI 使用的 base64 小端 f32 序列。
///
/// ABI 用 base64 而不是 JSON 数字数组：1024 维向量的数字数组约 10 KiB，base64 只要 5.5 KiB，
/// 且没有浮点文本往返的精度损失。
#[doc(hidden)]
pub fn encode_vector(vector: &[f32]) -> String {
    use base64::Engine;

    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
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

    /// 读取插件状态；应用未注入持久目录时状态仅在当前实例内保留。
    fn get_state(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// 写入插件状态；应用注入持久目录时 Host 会在成功返回前原子保存。
    fn set_state(&self, key: &str, value: &serde_json::Value) -> Result<()>;

    /// 删除插件状态并返回旧值；持久保存失败时 Host 保留原值并返回错误。
    fn remove_state(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// 读取本插件私有持久存储中的一个键。
    ///
    /// 与 [`PluginHostApi::get_state`] 的区别是**生命周期**而非权限范围：两者都只能访问
    /// 本插件自己的命名空间，但 state 在插件代码变化时清空，store 跨版本保留。记忆、
    /// 索引一类的用户数据必须用 store，否则一次插件升级就会清空用户资产。
    ///
    /// 需要 manifest 声明 `storage = true`。
    fn store_get(&self, scope: PluginStoreScope, key: &str) -> Result<Option<serde_json::Value>> {
        let _ = (scope, key);
        Err(anyhow!("当前宿主未提供插件持久存储能力"))
    }

    /// 写入本插件私有持久存储。
    fn store_set(
        &self,
        scope: PluginStoreScope,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let _ = (scope, key, value);
        Err(anyhow!("当前宿主未提供插件持久存储能力"))
    }

    /// 删除本插件私有持久存储中的一个键，返回被删除的值。
    fn store_remove(
        &self,
        scope: PluginStoreScope,
        key: &str,
    ) -> Result<Option<serde_json::Value>> {
        let _ = (scope, key);
        Err(anyhow!("当前宿主未提供插件持久存储能力"))
    }

    /// 列出指定作用域下的全部键，按字典序稳定排列。
    fn store_list(&self, scope: PluginStoreScope) -> Result<Vec<String>> {
        let _ = scope;
        Err(anyhow!("当前宿主未提供插件持久存储能力"))
    }

    /// 打开本插件私有向量库并绑定一个向量空间，返回库中现有记录数。
    ///
    /// 与 [`PluginHostApi::store_set`] 的区别是**形态**而非权限范围：store 是重写全量的小
    /// JSON，装不下成千上万条高维向量；向量库按记录增量写入，检索在宿主侧完成。
    ///
    /// `space_id` 或 `dimension` 与库中不符时整库清空重建——混用不同嵌入空间的向量会让
    /// 检索结果长期缓慢地不正确。
    ///
    /// 需要 manifest 声明 `vector_store = true`。
    fn vector_open(&self, space_id: &str, dimension: u32) -> Result<u64> {
        let _ = (space_id, dimension);
        Err(anyhow!("当前宿主未提供插件向量库能力"))
    }

    /// 分批对账当前记录清单，返回其中需要重新计算向量的记录 ID。
    ///
    /// `first` 开启一次对账会话，`last` 让宿主删除本次会话未出现的全部记录。它同时承担
    /// "哪些需要重算"与"哪些已失效"两件事，调用方不必把整库 ID 拉回本地比对。
    fn vector_sync(
        &self,
        entries: &[VectorSyncEntry],
        first: bool,
        last: bool,
    ) -> Result<VectorSyncOutcome> {
        let _ = (entries, first, last);
        Err(anyhow!("当前宿主未提供插件向量库能力"))
    }

    /// 批量写入或替换向量记录，返回实际写入条数。
    fn vector_upsert(&self, records: &[VectorRecord]) -> Result<u64> {
        let _ = records;
        Err(anyhow!("当前宿主未提供插件向量库能力"))
    }

    /// 执行一次余弦近邻检索，返回按相似度降序排列的命中。
    fn vector_search(
        &self,
        vector: &[f32],
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<VectorHit>> {
        let _ = (vector, limit, path_prefix);
        Err(anyhow!("当前宿主未提供插件向量库能力"))
    }

    /// 按记录 ID 与路径前缀删除向量记录，返回实际删除条数。
    fn vector_remove(&self, record_ids: &[String], path_prefixes: &[String]) -> Result<u64> {
        let _ = (record_ids, path_prefixes);
        Err(anyhow!("当前宿主未提供插件向量库能力"))
    }

    /// 列出当前项目的历史会话摘要。
    ///
    /// 需要 manifest 声明 `sessions = true`。该能力会把插件全部公开工具的权限下限
    /// 抬到 `unknown`：会话内容是 Agent 历史上见过的一切的并集，涵盖文件正文、
    /// shell 输出与网络返回。
    fn session_list(&self) -> Result<Vec<PluginSessionSummary>> {
        Err(anyhow!("当前宿主未提供只读会话访问能力"))
    }

    /// 分页读取一个会话的消息投影；会话不存在时返回 `None`。
    ///
    /// 返回内容不含模型私有推理与图片字节，超长正文按宿主上限截断。`limit` 缺省时
    /// 使用宿主上限；宿主也可能因单页字节预算提前收尾，调用方应按
    /// [`PluginSessionPage::message_count`] 判断是否还有后续页。
    fn session_read(
        &self,
        id: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> Result<Option<PluginSessionPage>> {
        let _ = (id, offset, limit);
        Err(anyhow!("当前宿主未提供只读会话访问能力"))
    }

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

    /// 执行一次由 manifest 允许的结构化 HTTP 请求。
    ///
    /// Host 会注入可信 base URL 与认证信息，并在返回前执行主机 allowlist、网络策略、
    /// 请求/响应大小和超时校验。默认实现返回能力不可用，便于旧宿主与单元测试明确失败。
    fn http_request(&self, _request: &HttpRequest) -> Result<HttpResponse> {
        Err(anyhow!("宿主未提供 HTTP 请求能力"))
    }

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
    ///
    /// Host 不返回私有推理增量，并会把插件策略的完整审批拒绝原因替换为稳定文案。
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

    /// 处理一次事件总线分发。
    ///
    /// 插件只会收到 manifest `[[events]]` 中声明订阅的事件；未声明即收不到，也无法对其
    /// 表态。通过 [`PluginEvent::cancel`] 取消事件，通过 [`PluginEvent::set`] 提交改写；
    /// 宿主只采纳目标事件白名单内的字段，调用 ID、运行 ID 与权限 scope 无法被改写。
    ///
    /// 返回 [`EventOutcome::pending`] 可以让宿主稍后重新投递同一事件，用于人工审批等
    /// 需要等待外部输入的场景；宿主对累计等待时长设有上限，超时按拒绝处理。
    fn on_event(&mut self, _host: &dyn PluginHostApi, _event: &mut PluginEvent) -> EventOutcome {
        EventOutcome::ready()
    }

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
package ascnet:lucia-plugin@0.10.0;

world plugin {
  import host-agent-upsert-tool: func(request-json: string) -> string;
  import host-agent-remove-tool: func(request-json: string) -> string;
  import host-agent-upsert-prompt: func(request-json: string) -> string;
  import host-agent-remove-prompt: func(request-json: string) -> string;
  import host-agent-emit-event: func(request-json: string) -> string;
  import host-state-get: func(request-json: string) -> string;
  import host-state-set: func(request-json: string) -> string;
  import host-state-remove: func(request-json: string) -> string;
  import host-store-get: func(request-json: string) -> string;
  import host-store-set: func(request-json: string) -> string;
  import host-store-remove: func(request-json: string) -> string;
  import host-store-list: func(request-json: string) -> string;
  import host-vector-open: func(request-json: string) -> string;
  import host-vector-sync: func(request-json: string) -> string;
  import host-vector-upsert: func(request-json: string) -> string;
  import host-vector-search: func(request-json: string) -> string;
  import host-vector-remove: func(request-json: string) -> string;
  import host-session-list: func(request-json: string) -> string;
  import host-session-read: func(request-json: string) -> string;
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
  import host-http-request: func(request-json: string) -> string;
  import host-model-complete: func(request-json: string) -> string;
  import host-agent-runtime-call: func(request-json: string) -> string;
  export activate: func(context-json: string) -> string;
  export deactivate: func() -> string;
  export handle-service: func(call-json: string) -> string;
  export list-tools: func() -> string;
  export call-tool: func(call-json: string) -> string;
  export on-event: func(envelope-json: string) -> string;
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

                fn store_get(
                    &self,
                    scope: $crate::PluginStoreScope,
                    key: &str,
                ) -> $crate::Result<Option<$crate::__serde_json::Value>> {
                    let request = $crate::__serde_json::json!({"scope": scope, "key": key});
                    $crate::decode_host_response(&host_store_get(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn store_set(
                    &self,
                    scope: $crate::PluginStoreScope,
                    key: &str,
                    value: &$crate::__serde_json::Value,
                ) -> $crate::Result<()> {
                    let request = $crate::__serde_json::json!({
                        "scope": scope,
                        "key": key,
                        "value": value,
                    });
                    $crate::decode_host_response(&host_store_set(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn store_remove(
                    &self,
                    scope: $crate::PluginStoreScope,
                    key: &str,
                ) -> $crate::Result<Option<$crate::__serde_json::Value>> {
                    let request = $crate::__serde_json::json!({"scope": scope, "key": key});
                    $crate::decode_host_response(&host_store_remove(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn store_list(
                    &self,
                    scope: $crate::PluginStoreScope,
                ) -> $crate::Result<Vec<String>> {
                    let request = $crate::__serde_json::json!({"scope": scope});
                    $crate::decode_host_response(&host_store_list(
                        &$crate::to_json_string(&request),
                    ))
                }

                fn vector_open(&self, space_id: &str, dimension: u32) -> $crate::Result<u64> {
                    let request = $crate::__serde_json::json!({
                        "space_id": space_id,
                        "dimension": dimension,
                    });
                    let response: $crate::__serde_json::Value =
                        $crate::decode_host_response(&host_vector_open(
                            &$crate::to_json_string(&request),
                        ))?;
                    Ok(response
                        .get("count")
                        .and_then($crate::__serde_json::Value::as_u64)
                        .unwrap_or(0))
                }

                fn vector_sync(
                    &self,
                    entries: &[$crate::VectorSyncEntry],
                    first: bool,
                    last: bool,
                ) -> $crate::Result<$crate::VectorSyncOutcome> {
                    let entries: Vec<$crate::__serde_json::Value> = entries
                        .iter()
                        .map(|entry| {
                            $crate::__serde_json::json!({
                                "record_id": entry.record_id,
                                "revision": entry.revision,
                            })
                        })
                        .collect();
                    let request = $crate::__serde_json::json!({
                        "entries": entries,
                        "first": first,
                        "last": last,
                    });
                    let response: $crate::__serde_json::Value =
                        $crate::decode_host_response(&host_vector_sync(
                            &$crate::to_json_string(&request),
                        ))?;
                    let stale = response
                        .get("stale")
                        .and_then($crate::__serde_json::Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map($crate::__serde_json::Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok($crate::VectorSyncOutcome {
                        stale,
                        removed: response
                            .get("removed")
                            .and_then($crate::__serde_json::Value::as_u64)
                            .unwrap_or(0),
                    })
                }

                fn vector_upsert(
                    &self,
                    records: &[$crate::VectorRecord],
                ) -> $crate::Result<u64> {
                    let records: Vec<$crate::__serde_json::Value> = records
                        .iter()
                        .map(|record| {
                            $crate::__serde_json::json!({
                                "record_id": record.record_id,
                                "path": record.path,
                                "revision": record.revision,
                                "vector": $crate::encode_vector(&record.vector),
                            })
                        })
                        .collect();
                    let request = $crate::__serde_json::json!({"records": records});
                    let response: $crate::__serde_json::Value =
                        $crate::decode_host_response(&host_vector_upsert(
                            &$crate::to_json_string(&request),
                        ))?;
                    Ok(response
                        .get("written")
                        .and_then($crate::__serde_json::Value::as_u64)
                        .unwrap_or(0))
                }

                fn vector_search(
                    &self,
                    vector: &[f32],
                    limit: usize,
                    path_prefix: Option<&str>,
                ) -> $crate::Result<Vec<$crate::VectorHit>> {
                    let request = $crate::__serde_json::json!({
                        "vector": $crate::encode_vector(vector),
                        "limit": limit,
                        "path_prefix": path_prefix,
                    });
                    let response: $crate::__serde_json::Value =
                        $crate::decode_host_response(&host_vector_search(
                            &$crate::to_json_string(&request),
                        ))?;
                    let hits = response
                        .get("hits")
                        .and_then($crate::__serde_json::Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| {
                                    Some($crate::VectorHit {
                                        record_id: item
                                            .get("record_id")?
                                            .as_str()?
                                            .to_string(),
                                        path: item
                                            .get("path")
                                            .and_then($crate::__serde_json::Value::as_str)
                                            .unwrap_or_default()
                                            .to_string(),
                                        score: item
                                            .get("score")
                                            .and_then($crate::__serde_json::Value::as_f64)
                                            .unwrap_or(0.0)
                                            as f32,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok(hits)
                }

                fn vector_remove(
                    &self,
                    record_ids: &[String],
                    path_prefixes: &[String],
                ) -> $crate::Result<u64> {
                    let request = $crate::__serde_json::json!({
                        "record_ids": record_ids,
                        "path_prefixes": path_prefixes,
                    });
                    let response: $crate::__serde_json::Value =
                        $crate::decode_host_response(&host_vector_remove(
                            &$crate::to_json_string(&request),
                        ))?;
                    Ok(response
                        .get("removed")
                        .and_then($crate::__serde_json::Value::as_u64)
                        .unwrap_or(0))
                }

                fn session_list(
                    &self,
                ) -> $crate::Result<Vec<$crate::PluginSessionSummary>> {
                    $crate::decode_host_response(&host_session_list(
                        &$crate::to_json_string(&$crate::__serde_json::json!({})),
                    ))
                }

                fn session_read(
                    &self,
                    id: &str,
                    offset: usize,
                    limit: Option<usize>,
                ) -> $crate::Result<Option<$crate::PluginSessionPage>> {
                    let request = $crate::__serde_json::json!({
                        "id": id,
                        "offset": offset,
                        "limit": limit,
                    });
                    $crate::decode_host_response(&host_session_read(
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

                fn http_request(
                    &self,
                    request: &$crate::HttpRequest,
                ) -> $crate::Result<$crate::HttpResponse> {
                    $crate::decode_host_response(&host_http_request(
                        &$crate::to_json_string(request),
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

                fn on_event(envelope_json: String) -> String {
                    let envelope: $crate::PluginEventEnvelope =
                        match $crate::from_json_string(&envelope_json) {
                            Ok(envelope) => envelope,
                            // 信封无法解析说明 Host/Guest 协议不一致；对安全事件
                            // fail-closed 取消，对通告事件取消不产生副作用。
                            Err(_) => {
                                return $crate::to_json_string(
                                    &$crate::PluginEventResponse::cancel(),
                                )
                            }
                        };
                    let mut event = $crate::PluginEvent::new(envelope);
                    let outcome =
                        with_plugin(|plugin| plugin.on_event(&ComponentHostApi, &mut event));
                    $crate::to_json_string(&event.into_response(outcome))
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

    /// 底栏展示构造器必须序列化为加法目标，不改变扩展事件的稳定 JSON 外壳。
    #[test]
    fn footer_presentation_uses_stable_target() {
        let presentation =
            EventPresentation::footer("Permission: review", EventPresentationTone::Muted);
        let encoded = serde_json::to_value(presentation).expect("序列化底栏展示");

        assert_eq!(encoded["target"], "footer");
        assert_eq!(encoded["variant"], "text");
        assert_eq!(encoded["tone"], "muted");
        assert_eq!(encoded["text"], "Permission: review");
    }
}

/// 插件在一次事件分发中看到的可取消上下文。
///
/// 插件拿到的是事件的 JSON 快照。取消与改写都先记录在本地，分发结束后一次性交给宿主，
/// 由宿主按目标事件的白名单合并——因此 `set` 写入白名单之外的字段不会生效，也不会报错。
pub struct PluginEvent {
    envelope: PluginEventEnvelope,
    patch: serde_json::Map<String, serde_json::Value>,
    cancel: bool,
}

impl PluginEvent {
    /// 由宿主信封构造事件上下文；由 `export_plugin!` 调用。
    pub fn new(envelope: PluginEventEnvelope) -> Self {
        Self {
            envelope,
            patch: serde_json::Map::new(),
            cancel: false,
        }
    }

    /// 事件名，例如 `core.tool.call`。
    pub fn name(&self) -> &str {
        &self.envelope.event
    }

    /// 事件在投递给本插件时是否已被更早的监听者取消。
    pub fn is_cancelled(&self) -> bool {
        self.envelope.cancelled || self.cancel
    }

    /// 首个取消者身份；未取消时为 `None`。
    ///
    /// 取消归属由首次取消决定，本插件之后再取消也不会改变它。
    pub fn canceller(&self) -> Option<&str> {
        self.envelope.canceller.as_deref()
    }

    /// 本插件在总线上的订阅身份，也是它取消事件时被记录的取消者。
    pub fn subscriber(&self) -> &str {
        &self.envelope.subscriber
    }

    /// 事件负载的原始 JSON 快照。
    pub fn data(&self) -> &serde_json::Value {
        &self.envelope.data
    }

    /// 按具体类型解析事件负载。
    ///
    /// # Errors
    ///
    /// 负载形状与目标类型不匹配时返回错误。
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.envelope.data.clone())
            .map_err(|error| anyhow!("解析事件 `{}` 负载失败：{error}", self.envelope.event))
    }

    /// 提交一个字段改写；宿主只采纳目标事件白名单内的字段。
    pub fn set(&mut self, field: impl Into<String>, value: serde_json::Value) -> &mut Self {
        self.patch.insert(field.into(), value);
        self
    }

    /// 取消事件。
    ///
    /// 对 `core.tool.call` 表示拒绝执行该工具；对其他可取消事件的后果见宿主文档。
    pub fn cancel(&mut self) -> &mut Self {
        self.cancel = true;
        self
    }

    /// 拒绝当前工具调用，并给出面向界面的原因与面向模型的一次性指导。
    ///
    /// `reason` 只进入可信事件流供界面展示；`agent_guidance` 只注入下一次模型请求，
    /// 不写入会话历史。仅在处理 `core.tool.call` 时有意义。
    pub fn deny_tool(
        &mut self,
        reason: impl Into<String>,
        agent_guidance: Option<String>,
    ) -> &mut Self {
        self.set("block_reason", json!(reason.into()));
        if let Some(guidance) = agent_guidance {
            self.set("agent_guidance", json!(guidance));
        }
        self.cancel()
    }

    /// 改写当前工具调用的名称与参数。
    ///
    /// 调用 ID、运行 ID 与权限 scope 由宿主绑定，无法通过本方法改写；宿主会在改写后
    /// 按目标工具重新绑定权限 scope。仅在处理 `core.tool.call` 时有意义。
    pub fn rewrite_tool(
        &mut self,
        name: Option<&str>,
        args: Option<serde_json::Value>,
    ) -> &mut Self {
        let mut call = serde_json::Map::new();
        if let Some(name) = name {
            call.insert("name".into(), json!(name));
        }
        if let Some(args) = args {
            call.insert("args".into(), args);
        }
        self.set("call", serde_json::Value::Object(call))
    }

    /// 把本次分发收敛为交给宿主的应答；由 `export_plugin!` 调用。
    pub fn into_response(self, outcome: EventOutcome) -> PluginEventResponse {
        match outcome {
            EventOutcome::Pending { retry_after_ms } => {
                PluginEventResponse::pending(retry_after_ms)
            }
            EventOutcome::Ready => PluginEventResponse::Ready {
                cancel: self.cancel,
                patch: if self.patch.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(self.patch)
                },
            },
        }
    }
}

/// `core.agent.event` 的事件负载。
///
/// 承载 Core 写入记录流的完整事件，供只需要观察模型增量、计费用量、工具输出等细粒度
/// 记录的插件使用；该事件只作通告，取消不产生任何后果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecordEventData {
    /// 已写入记录流的事件。
    pub record: AgentEvent,
}

/// 插件对一次事件分发的处理结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    /// 已给出结论，宿主按当前取消状态与补丁继续分发。
    Ready,
    /// 尚未形成结论，请宿主稍后重新投递同一事件。
    Pending {
        /// 宿主重新投递前的等待毫秒数。
        retry_after_ms: u64,
    },
}

impl EventOutcome {
    /// 构造已完成的结论。
    pub fn ready() -> Self {
        Self::Ready
    }

    /// 构造等待结论，请求宿主在指定毫秒后重投。
    pub fn pending(retry_after_ms: u64) -> Self {
        Self::Pending { retry_after_ms }
    }
}

/// 不连接宿主的 [`PluginHostApi`] 空实现，供插件单元测试使用。
///
/// 所有写入型能力都是无副作用的成功返回，所有读取型能力返回空结果。插件在测试中
/// 验证事件处理逻辑时不需要真实宿主，也就不必为每个插件重复编写替身。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHostApi;

impl PluginHostApi for NoopHostApi {
    fn upsert_tool(&self, local_name: &str, _spec: &ToolSpec) -> Result<String> {
        Ok(local_name.to_string())
    }

    fn remove_tool(&self, _public_name: &str) -> Result<()> {
        Ok(())
    }

    fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String> {
        Ok(prompt.id.clone())
    }

    fn remove_prompt(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    fn emit_event(&self, _event: &ExtensionEvent) -> Result<()> {
        Ok(())
    }

    fn get_state(&self, _key: &str) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    fn set_state(&self, _key: &str, _value: &serde_json::Value) -> Result<()> {
        Ok(())
    }

    fn remove_state(&self, _key: &str) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    fn store_get(&self, _scope: PluginStoreScope, _key: &str) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    fn store_set(
        &self,
        _scope: PluginStoreScope,
        _key: &str,
        _value: &serde_json::Value,
    ) -> Result<()> {
        Ok(())
    }

    fn store_remove(
        &self,
        _scope: PluginStoreScope,
        _key: &str,
    ) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    fn store_list(&self, _scope: PluginStoreScope) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn session_list(&self) -> Result<Vec<PluginSessionSummary>> {
        Ok(Vec::new())
    }

    fn session_read(
        &self,
        _id: &str,
        _offset: usize,
        _limit: Option<usize>,
    ) -> Result<Option<PluginSessionPage>> {
        Ok(None)
    }

    fn upsert_service(&self, _service: &ServiceSpec) -> Result<()> {
        Ok(())
    }

    fn remove_service(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn list_services(&self, _plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
        Ok(Vec::new())
    }

    fn call_service(
        &self,
        _plugin_id: &str,
        _name: &str,
        _payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        Err(anyhow!("测试替身不提供插件服务调用"))
    }

    fn read_file(&self, _path: &str) -> Result<String> {
        Err(anyhow!("测试替身不提供文件读取"))
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<FileEntry>> {
        Ok(Vec::new())
    }

    fn spawn_process(&self, _spec: &ProcessSpec) -> Result<u64> {
        Err(anyhow!("测试替身不提供子进程能力"))
    }

    fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
        Err(anyhow!("测试替身不提供子进程能力"))
    }

    fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
        Ok(None)
    }

    fn kill_process(&self, _handle: u64) -> Result<()> {
        Ok(())
    }
}
