//! Lucia 的通用 Agent 派生与协作运行时。
//!
//! 本 crate 只提供身份、权限、生命周期、限额和消息通道等机制，不定义
//! sub-agent、workflow、multi-agent 或 teammate 等业务策略。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use agent_core::{
    Agent, AgentEvent, AgentExtension, AgentOptions, AgentRun, CompositeEventSink, ContextLoader,
    EventSink, ModelGateway, ModelMessage, ReasoningLevel, Session, TokenUsage, ToolChoice,
    ToolDecision,
};
use agent_tool::{ToolCall, ToolRegistry, ToolResult, ToolSpec};
use anyhow::Result as AnyResult;
use async_trait::async_trait;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
    panic::AssertUnwindSafe,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
};
use thiserror::Error;
use tokio::{
    sync::{mpsc, Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock, Semaphore},
    task::AbortHandle,
};
use uuid::Uuid;

/// Agent Runtime 操作结果。
pub type RuntimeResult<T> = Result<T, AgentRuntimeError>;

/// Agent Runtime 返回的稳定错误类型。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentRuntimeError {
    /// 运行时限额配置无效。
    #[error("运行时限额无效：{0}")]
    InvalidLimits(String),

    /// 指定的 Agent 不存在。
    #[error("Agent 不存在：{0}")]
    AgentNotFound(AgentId),

    /// Runtime principal 为空或格式无效。
    #[error("Runtime principal 无效：{0}")]
    InvalidPrincipal(String),

    /// Runtime principal 已被撤销。
    #[error("Runtime principal 已撤销：{0}")]
    PrincipalRevoked(RuntimePrincipal),

    /// Runtime principal 不是目标 Agent 的 owner。
    #[error("Runtime principal {principal} 不是 Agent {agent} 的 owner")]
    OwnerMismatch {
        /// Host 注入的可信 principal。
        principal: RuntimePrincipal,
        /// 被访问的 Agent。
        agent: AgentId,
    },

    /// 命名 profile 标识无效。
    #[error("Agent profile 标识无效：{0}")]
    InvalidProfileId(String),

    /// 命名 profile 已注册。
    #[error("Agent profile 已注册：{0}")]
    ProfileAlreadyExists(AgentProfileId),

    /// 命名 profile 不存在。
    #[error("Agent profile 不存在：{0}")]
    ProfileNotFound(AgentProfileId),

    /// principal 未获准使用指定 profile。
    #[error("Runtime principal {principal} 未获准使用 Agent profile {profile}")]
    ProfileDenied {
        /// Host 注入的可信 principal。
        principal: RuntimePrincipal,
        /// 被拒绝的 profile。
        profile: AgentProfileId,
    },

    /// 调用者无权管理目标 Agent。
    #[error("Agent {caller} 无权管理 Agent {target}")]
    PermissionDenied {
        /// 可信调用者身份。
        caller: AgentId,
        /// 被访问的目标身份。
        target: AgentId,
    },

    /// 调用者已经处于终态，不能再派生子节点。
    #[error("Agent 已结束，不能继续派生：{0}")]
    AgentInactive(AgentId),

    /// 派生深度超过运行时上限。
    #[error("Agent 派生深度超过上限 {limit}")]
    MaxDepthExceeded {
        /// 允许的最大深度，根节点深度为零。
        limit: usize,
    },

    /// 单个父节点的累计子节点数量超过上限。
    #[error("Agent 子节点数量超过上限 {limit}")]
    MaxChildrenExceeded {
        /// 单个父节点允许创建的累计子节点数量。
        limit: usize,
    },

    /// 派生 Agent 的构造失败。
    #[error("Agent 构造失败：{0}")]
    AgentBuild(String),

    /// 根节点没有后台执行任务，不能等待结果。
    #[error("Agent 没有可等待的执行任务：{0}")]
    NotRunnable(AgentId),

    /// 目标 Agent 尚未成功结束，或没有可供后续运行使用的私有会话。
    #[error("Agent 没有可继续的成功会话：{0}")]
    SessionUnavailable(AgentId),
}

/// Runtime 内稳定且不可伪造的 Agent 身份。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(Uuid);

impl AgentId {
    /// 生成一个随机 Agent 身份。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 返回底层 UUID。
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for AgentId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Host 为一次受信任组件生命周期分配的 owner principal。
///
/// Runtime 不解释命名空间或业务含义；Host 可使用插件 ID、租户 ID 或其他稳定主体，
/// 并建议为每次激活附加唯一代次，避免撤销后的 principal 被复用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimePrincipal(String);

impl RuntimePrincipal {
    /// 创建通用 owner principal。
    ///
    /// 空字符串、首尾空白或超过 256 字节的值会被拒绝。
    pub fn new(value: impl Into<String>) -> RuntimeResult<Self> {
        let value = value.into();
        if value.is_empty() || value.trim() != value || value.len() > 256 {
            return Err(AgentRuntimeError::InvalidPrincipal(value));
        }
        Ok(Self(value))
    }

    /// 返回供仅有一个宿主 owner 的原生集成使用的默认 principal。
    pub fn host() -> Self {
        Self("host".to_string())
    }

    /// 返回 principal 的不透明字符串值。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuntimePrincipal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Host 注册的命名 Agent 派生 profile 标识。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentProfileId(String);

impl AgentProfileId {
    /// 创建命名 profile 标识。
    ///
    /// 标识只允许 ASCII 字母、数字、点、下划线和连字符，长度为 1 到 128 字节。
    pub fn new(value: impl Into<String>) -> RuntimeResult<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid {
            return Err(AgentRuntimeError::InvalidProfileId(value));
        }
        Ok(Self(value))
    }

    /// 返回 profile 的稳定字符串值。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Agent 的父子谱系信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLineage {
    /// 直接父节点；根节点为 `None`。
    pub parent: Option<AgentId>,
    /// 整棵派生树的根节点。
    pub root: AgentId,
    /// 当前节点深度；根节点为零。
    pub depth: usize,
}

/// 工具访问范围。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "tools", rename_all = "snake_case")]
pub enum ToolAccess {
    /// 继承父节点当前允许的全部工具，不代表绕过父节点限制。
    #[default]
    All,
    /// 只允许集合中列出的工具。
    Allowlist(BTreeSet<String>),
}

impl ToolAccess {
    /// 创建一个工具 allowlist。
    pub fn allowlist<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Allowlist(names.into_iter().map(Into::into).collect())
    }

    /// 判断当前范围是否允许指定工具。
    pub fn permits(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allowlist(names) => names.contains(name),
        }
    }

    /// 在当前范围内应用下一层限制。
    ///
    /// 返回值只可能保持或收缩当前权限，子节点请求 `All` 也不会恢复父节点已移除的工具。
    pub fn restrict(&self, requested: &Self) -> Self {
        match (self, requested) {
            (Self::All, next) => next.clone(),
            (current @ Self::Allowlist(_), Self::All) => current.clone(),
            (Self::Allowlist(current), Self::Allowlist(requested)) => {
                Self::Allowlist(current.intersection(requested).cloned().collect())
            }
        }
    }
}

/// Agent 可继承和收缩的权限集合。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermissions {
    /// 模型可见且可实际执行的工具范围。
    #[serde(default)]
    pub tools: ToolAccess,
}

impl AgentPermissions {
    /// 在当前权限上应用子节点请求并返回有效权限。
    pub fn restrict(&self, requested: &Self) -> Self {
        Self {
            tools: self.tools.restrict(&requested.tools),
        }
    }
}

/// 对 Core [`AgentOptions`] 的可序列化增量覆盖。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentOptionsPatch {
    /// 覆盖逻辑服务商名称。
    pub provider: Option<String>,
    /// 覆盖模型名称。
    pub model: Option<String>,
    /// 覆盖最大 ReAct 步数。
    pub max_steps: Option<usize>,
    /// 覆盖系统提示词。
    pub system_prompt: Option<String>,
    /// 覆盖工具选择模式。
    pub tool_choice: Option<ToolChoice>,
    /// 覆盖最大输出 token 数。
    pub max_tokens: Option<u32>,
    /// 覆盖采样温度。
    pub temperature: Option<f32>,
    /// 覆盖推理级别。
    pub reasoning: Option<ReasoningLevel>,
    /// 覆盖服务商专属请求选项。
    pub provider_options: Option<Value>,
}

impl AgentOptionsPatch {
    /// 将非空字段应用到现有 Core 运行选项。
    pub fn apply_to(&self, options: &mut AgentOptions) {
        if let Some(value) = &self.provider {
            options.provider = value.clone();
        }
        if let Some(value) = &self.model {
            options.model = value.clone();
        }
        if let Some(value) = self.max_steps {
            options.max_steps = value;
        }
        if let Some(value) = &self.system_prompt {
            options.system_prompt = value.clone();
        }
        if let Some(value) = &self.tool_choice {
            options.tool_choice = value.clone();
        }
        if let Some(value) = self.max_tokens {
            options.max_tokens = Some(value);
        }
        if let Some(value) = self.temperature {
            options.temperature = Some(value);
        }
        if let Some(value) = self.reasoning {
            options.reasoning = value;
        }
        if let Some(value) = &self.provider_options {
            options.provider_options = value.clone();
        }
    }
}

/// 一次 Agent 派生的可序列化配置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentDeriveConfig {
    /// 对父模板运行选项的增量覆盖。
    #[serde(default)]
    pub options: AgentOptionsPatch,
    /// 子节点请求的权限；Runtime 会与父节点有效权限取交集。
    #[serde(default)]
    pub permissions: AgentPermissions,
}

/// 从现有 Core Agent 捕获的可复用派生模板。
///
/// 模型网关、工具实例和钩子通过 `Arc` 或内部共享句柄复用；每次实例化都会创建独立
/// Core Agent，因此运行控制队列不会在并发任务之间共享。
#[derive(Clone)]
pub struct AgentTemplate {
    gateway: ModelGateway,
    tools: ToolRegistry,
    extension: Arc<dyn AgentExtension>,
    event_sink: Arc<dyn EventSink>,
    context_loader: Arc<dyn ContextLoader>,
    options: AgentOptions,
}

impl AgentTemplate {
    /// 从现有 Core Agent 捕获派生模板，不取得该 Agent 的运行控制队列。
    pub fn from_agent(agent: &Agent) -> Self {
        Self {
            gateway: agent.gateway().clone(),
            tools: agent.tools().clone(),
            extension: agent.extension(),
            event_sink: agent.event_sink(),
            context_loader: agent.context_loader(),
            options: agent.options().clone(),
        }
    }

    /// 返回模板的基础运行选项。
    pub fn options(&self) -> &AgentOptions {
        &self.options
    }

    /// 根据父权限和派生配置创建独立 Core Agent。
    ///
    /// 返回值同时包含已经与父权限取交集的有效权限。工具 allowlist 会同时过滤模型
    /// 可见定义和实际执行入口；构造工具子集失败时返回 [`AgentRuntimeError::AgentBuild`]。
    pub fn instantiate(
        &self,
        parent_permissions: &AgentPermissions,
        config: &AgentDeriveConfig,
    ) -> RuntimeResult<(Agent, AgentPermissions)> {
        let permissions = parent_permissions.restrict(&config.permissions);
        let mut options = self.options.clone();
        config.options.apply_to(&mut options);

        let tools = match &permissions.tools {
            ToolAccess::All => self.tools.clone(),
            ToolAccess::Allowlist(names) => {
                let native_names = self
                    .tools
                    .specs()
                    .into_iter()
                    .map(|spec| spec.name)
                    .filter(|name| names.contains(name))
                    .collect::<Vec<_>>();
                self.tools
                    .subset(&native_names)
                    .map_err(|error| AgentRuntimeError::AgentBuild(error.to_string()))?
            }
        };

        let extension: Arc<dyn AgentExtension> = match &permissions.tools {
            ToolAccess::All => self.extension.clone(),
            access => Arc::new(RestrictedExtension {
                inner: self.extension.clone(),
                access: access.clone(),
            }),
        };

        let agent = Agent::new(self.gateway.clone(), options)
            .with_tools(tools)
            .with_extension(extension)
            .with_event_sink(self.event_sink.clone())
            .with_context_loader(self.context_loader.clone());
        Ok((agent, permissions))
    }
}

/// 为扩展工具应用与原生工具相同的 allowlist。
struct RestrictedExtension {
    inner: Arc<dyn AgentExtension>,
    access: ToolAccess,
}

#[async_trait]
impl AgentExtension for RestrictedExtension {
    async fn prompt_messages(&self) -> AnyResult<Vec<ModelMessage>> {
        self.inner.prompt_messages().await
    }

    async fn list_tools(&self) -> AnyResult<Vec<ToolSpec>> {
        Ok(self
            .inner
            .list_tools()
            .await?
            .into_iter()
            .filter(|spec| self.access.permits(&spec.name))
            .collect())
    }

    async fn call_tool(&self, call: ToolCall) -> AnyResult<Option<ToolResult>> {
        if self.access.permits(&call.name) {
            self.inner.call_tool(call).await
        } else {
            Ok(None)
        }
    }

    async fn before_tool(&self, call: &ToolCall) -> AnyResult<ToolDecision> {
        if !self.access.permits(&call.name) {
            return Ok(ToolDecision::Block {
                reason: format!("工具不在当前 Agent 的 allowlist 中：{}", call.name),
            });
        }

        match self.inner.before_tool(call).await? {
            ToolDecision::Rewrite { call } if !self.access.permits(&call.name) => {
                Ok(ToolDecision::Block {
                    reason: format!("重写后的工具不在当前 Agent 的 allowlist 中：{}", call.name),
                })
            }
            decision => Ok(decision),
        }
    }

    async fn after_tool(&self, result: &ToolResult) -> AnyResult<()> {
        self.inner.after_tool(result).await
    }

    async fn on_event(&self, event: &AgentEvent) -> AnyResult<()> {
        self.inner.on_event(event).await
    }

    async fn drain_events(&self) -> AnyResult<Vec<Value>> {
        self.inner.drain_events().await
    }
}

/// Agent Runtime 的资源与拓扑限额。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeLimits {
    /// 根节点以下允许的最大派生深度。
    pub max_depth: usize,
    /// 单个父节点在其生命周期内允许创建的累计子节点数量。
    pub max_children_per_agent: usize,
    /// 同时执行模型循环的最大 Agent 数量；额外任务保持排队状态。
    pub max_concurrent_agents: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_children_per_agent: 16,
            max_concurrent_agents: 8,
        }
    }
}

impl RuntimeLimits {
    /// 校验不能为零的运行时限额。
    pub fn validate(&self) -> RuntimeResult<()> {
        if self.max_children_per_agent == 0 {
            return Err(AgentRuntimeError::InvalidLimits(
                "max_children_per_agent 必须大于零".to_string(),
            ));
        }
        if self.max_concurrent_agents == 0 {
            return Err(AgentRuntimeError::InvalidLimits(
                "max_concurrent_agents 必须大于零".to_string(),
            ));
        }
        Ok(())
    }
}

/// 派生 Agent 的启动请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSpawnRequest {
    /// 交给派生 Agent 的首次用户输入。
    pub input: String,
    /// 运行选项和权限的派生配置。
    #[serde(default)]
    pub derive: AgentDeriveConfig,
}

impl AgentSpawnRequest {
    /// 使用默认派生配置创建启动请求。
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            derive: AgentDeriveConfig::default(),
        }
    }
}

/// 已创建 Agent 的稳定句柄。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHandle {
    /// 新 Agent 的稳定身份，也是本次独立执行任务的查询键。
    pub id: AgentId,
    /// 新 Agent 的父子谱系。
    pub lineage: AgentLineage,
}

/// Agent 执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// 已挂载的根身份，当前没有后台执行任务。
    Ready,
    /// 已创建并等待并发许可。
    Queued,
    /// 正在执行 Core Agent 循环。
    Running,
    /// 执行成功。
    Succeeded,
    /// 执行失败或发生 panic。
    Failed,
    /// 已由管理方取消。
    Cancelled,
}

impl AgentStatus {
    /// 判断状态是否为不可覆盖的终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// 可跨 JSON ABI 返回的 Agent 成功结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExecutionResult {
    /// Core Agent 生成的运行 ID。
    pub run_id: String,
    /// 最终可见文本。
    pub final_text: String,
    /// 实际使用的 ReAct 步数。
    pub steps_used: usize,
    /// 服务商返回的 token 用量。
    pub usage: TokenUsage,
}

impl From<AgentRun> for AgentExecutionResult {
    fn from(run: AgentRun) -> Self {
        Self {
            run_id: run.run_id,
            final_text: run.final_text,
            steps_used: run.steps_used,
            usage: run.usage,
        }
    }
}

/// Agent 的幂等终态结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentOutcome {
    /// Core Agent 正常完成。
    Succeeded {
        /// 可序列化的执行摘要。
        result: AgentExecutionResult,
    },
    /// Core Agent 返回错误或运行任务发生 panic。
    Failed {
        /// 供诊断和展示的错误信息。
        error: String,
    },
    /// 执行由管理方取消。
    Cancelled,
}

impl AgentOutcome {
    /// 返回终态对应的状态枚举。
    pub fn status(&self) -> AgentStatus {
        match self {
            Self::Succeeded { .. } => AgentStatus::Succeeded,
            Self::Failed { .. } => AgentStatus::Failed,
            Self::Cancelled => AgentStatus::Cancelled,
        }
    }
}

/// Agent 状态查询快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Agent 身份。
    pub id: AgentId,
    /// 父子谱系。
    pub lineage: AgentLineage,
    /// 查询时的执行状态。
    pub status: AgentStatus,
    /// 当前有效权限。
    pub permissions: AgentPermissions,
}

/// 订阅单个 Agent 生命周期事件的流句柄。
///
/// 事件在订阅之后开始投递：订阅前已发出的事件不会补发。目标进入终态并且
/// 缓冲事件全部取出后，[`next`](Self::next) 返回 `None`，流自然结束。
#[derive(Debug)]
pub struct AgentEventStream {
    receiver: mpsc::UnboundedReceiver<AgentEvent>,
}

impl AgentEventStream {
    /// 取出下一条事件；目标终态且缓冲耗尽后返回 `None`。
    pub async fn next(&mut self) -> Option<AgentEvent> {
        self.receiver.recv().await
    }
}

/// 把 Core Agent 事件转发给当前订阅者的事件 sink。
///
/// 发送失败（订阅方已放弃接收）的通道会被移除，不影响其余订阅者。
struct SubscriberEventSink {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>>,
}

#[async_trait]
impl EventSink for SubscriberEventSink {
    async fn record(&self, event: &AgentEvent) -> AnyResult<()> {
        self.subscribers
            .lock()
            .expect("事件订阅者锁不应中毒")
            .retain(|sender| sender.send(event.clone()).is_ok());
        Ok(())
    }
}

/// 可由 Host 注入的身份绑定 Agent Runtime API。
///
/// `spawn`、`continue_agent`、查询和取消是通用控制面调用。teammate 邮箱、消息主题、
/// 投递和重试属于插件协议，不由 Runtime 提供。Host 的同步插件 import 不应调用会长期
/// 等待的 [`wait`](Self::wait)，以免插件工具回调同一实例时形成锁等待。
#[async_trait]
pub trait AgentRuntimeApi: Send + Sync {
    /// 返回此 API 绑定的可信 owner principal。
    fn principal(&self) -> RuntimePrincipal;

    /// 返回此 API 绑定的可信 Agent 身份。
    fn identity(&self) -> AgentId;

    /// 启动一个独立派生 Agent 并立即返回句柄，不等待模型运行完成。
    async fn spawn(&self, request: AgentSpawnRequest) -> RuntimeResult<AgentHandle>;

    /// 从自身或后代 Agent 的成功终态会话创建后续运行，并立即返回新句柄。
    ///
    /// Runtime 只复用目标的私有会话、运行模板和有效权限，不向调用方返回原始会话，
    /// 新运行的权限也不会超过目标 Agent 已经持有的范围。
    async fn continue_agent(&self, target: &AgentId, input: String) -> RuntimeResult<AgentHandle>;

    /// 查询自身或后代 Agent 的状态。
    async fn status(&self, target: &AgentId) -> RuntimeResult<AgentSnapshot>;

    /// 查询自身或后代 Agent 的终态结果；尚未结束时返回 `None`。
    async fn result(&self, target: &AgentId) -> RuntimeResult<Option<AgentOutcome>>;

    /// 等待自身或后代 Agent 进入终态。
    ///
    /// 该方法用于原生异步调用方，不应直接暴露为持有插件互斥锁的同步 Host import。
    async fn wait(&self, target: &AgentId) -> RuntimeResult<AgentOutcome>;

    /// 取消自身或后代 Agent。
    ///
    /// 目标及其全部后代会级联取消。至少一个节点首次进入取消状态时返回 `true`；
    /// 重复取消且没有新增变化时返回 `false`。
    async fn cancel(&self, target: &AgentId) -> RuntimeResult<bool>;

    /// 订阅自身或后代 Agent 的生命周期事件流。
    ///
    /// 只投递订阅之后发出的事件；目标已处于终态时返回立即结束的空流。
    /// 事件通道不限量缓冲，订阅方应及时消费，避免长时间滞留占用内存。
    async fn subscribe(&self, target: &AgentId) -> RuntimeResult<AgentEventStream>;
}

/// Host provisioner 创建的独立 controller 与身份绑定 API。
pub struct ProvisionedAgentRuntime {
    /// 此 principal 独占的 controller 根身份。
    pub controller: AgentHandle,
    /// 可注入受限组件的身份绑定 Runtime API。
    pub api: Arc<dyn AgentRuntimeApi>,
}

/// Host 用于安全创建和撤销 controller 的通用 provisioner。
///
/// Host 先注册命名 profile，再按可信 principal 授权；受限组件的请求体不接触
/// principal、模板或授权表。
#[async_trait]
pub trait AgentRuntimeProvisioner: Send + Sync {
    /// 由可信 Host 为一次组件生命周期授予 profile。
    async fn grant_profile(
        &self,
        principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<()>;

    /// 按已授权 profile 创建独立 controller 和绑定 API。
    async fn provision(
        &self,
        principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<ProvisionedAgentRuntime>;

    /// 撤销 principal 对 profile 的后续 provision 权限。
    async fn revoke_profile_grant(
        &self,
        principal: &RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> bool;

    /// 撤销 principal，取消并清理其全部 controller 和派生任务。
    async fn revoke(&self, principal: &RuntimePrincipal) -> usize;
}

/// 通用 Agent Runtime。
///
/// Runtime 只实现机制；调用方或插件自行定义派生拓扑、调度策略、工作流协议和消息语义。
#[derive(Clone)]
pub struct AgentRuntime {
    inner: Arc<RuntimeInner>,
}

impl AgentRuntime {
    /// 使用指定限额创建空 Runtime。
    pub fn new(limits: RuntimeLimits) -> RuntimeResult<Self> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                semaphore: Arc::new(Semaphore::new(limits.max_concurrent_agents)),
                limits,
                agents: AsyncRwLock::new(HashMap::new()),
                revoked_principals: AsyncRwLock::new(HashSet::new()),
                profiles: AsyncRwLock::new(HashMap::new()),
                profile_grants: AsyncRwLock::new(HashMap::new()),
                lifecycle: AsyncMutex::new(()),
            }),
        })
    }

    /// 挂载一个 Host 已持有的根 Agent 身份。
    ///
    /// 根节点只作为模板、权限和通信主体，不会自动运行。返回后可通过 [`api`](Self::api)
    /// 获取身份绑定 API，再由策略层派生独立 Agent。
    pub async fn attach_root(
        &self,
        template: AgentTemplate,
        permissions: AgentPermissions,
    ) -> RuntimeResult<AgentHandle> {
        self.attach_root_for(RuntimePrincipal::host(), template, permissions)
            .await
    }

    /// 为指定可信 principal 挂载一个 Host 已持有的根 Agent 身份。
    pub async fn attach_root_for(
        &self,
        owner: RuntimePrincipal,
        template: AgentTemplate,
        permissions: AgentPermissions,
    ) -> RuntimeResult<AgentHandle> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner.ensure_principal_active(&owner).await?;
        let id = AgentId::new();
        let lineage = AgentLineage {
            parent: None,
            root: id.clone(),
            depth: 0,
        };
        let entry = AgentEntry::new(
            id.clone(),
            lineage.clone(),
            owner,
            template,
            permissions,
            AgentOptionsPatch::default(),
            AgentStatus::Ready,
        );
        self.inner.agents.write().await.insert(id.clone(), entry);
        Ok(AgentHandle { id, lineage })
    }

    /// 为已知 Agent 创建身份绑定的可注入 API。
    pub async fn api(&self, identity: &AgentId) -> RuntimeResult<Arc<dyn AgentRuntimeApi>> {
        self.api_for(RuntimePrincipal::host(), identity).await
    }

    /// 为 Host 提供的 principal 和其拥有的 Agent 创建身份绑定 API。
    ///
    /// principal 只在该 Host 入口传入，后续 Guest 请求不再携带 owner、caller 或 sender。
    pub async fn api_for(
        &self,
        principal: RuntimePrincipal,
        identity: &AgentId,
    ) -> RuntimeResult<Arc<dyn AgentRuntimeApi>> {
        self.inner.ensure_owned(&principal, identity).await?;
        Ok(Arc::new(BoundAgentRuntime {
            runtime: self.clone(),
            principal,
            identity: identity.clone(),
        }))
    }

    /// 撤销 principal，并取消其仍未进入终态的全部 Agent。
    ///
    /// 返回本次新取消的 Agent 数量。重复撤销返回零；已完成的终态不会被覆盖。
    pub async fn revoke_principal(&self, principal: &RuntimePrincipal) -> usize {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let inserted = self
            .inner
            .revoked_principals
            .write()
            .await
            .insert(principal.clone());
        if !inserted {
            return 0;
        }

        self.inner.profile_grants.write().await.remove(principal);
        let entries = {
            let mut agents = self.inner.agents.write().await;
            let ids = agents
                .values()
                .filter(|entry| &entry.owner == principal)
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| agents.remove(&id))
                .collect::<Vec<_>>()
        };
        let mut cancelled = 0;
        for entry in entries {
            if entry.finish(AgentOutcome::Cancelled) {
                entry.abort();
                cancelled += 1;
            }
        }
        cancelled
    }

    /// 返回当前运行时限额。
    pub fn limits(&self) -> &RuntimeLimits {
        &self.inner.limits
    }

    /// 注册一个供 Host 授权的命名 Agent profile。
    pub async fn register_profile(
        &self,
        id: AgentProfileId,
        template: AgentTemplate,
        permissions: AgentPermissions,
    ) -> RuntimeResult<()> {
        let mut profiles = self.inner.profiles.write().await;
        if profiles.contains_key(&id) {
            return Err(AgentRuntimeError::ProfileAlreadyExists(id));
        }
        profiles.insert(
            id,
            AgentProfile {
                template,
                permissions,
            },
        );
        Ok(())
    }

    /// 移除一个命名 profile；已 provision 的 controller 不受影响。
    pub async fn remove_profile(&self, id: &AgentProfileId) -> bool {
        let removed = self.inner.profiles.write().await.remove(id).is_some();
        if removed {
            for grants in self.inner.profile_grants.write().await.values_mut() {
                grants.remove(id);
            }
        }
        removed
    }

    /// 授予 principal 使用指定 profile 的权限。
    pub async fn grant_profile(
        &self,
        principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<()> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner.ensure_principal_active(&principal).await?;
        if !self.inner.profiles.read().await.contains_key(profile) {
            return Err(AgentRuntimeError::ProfileNotFound(profile.clone()));
        }
        self.inner
            .profile_grants
            .write()
            .await
            .entry(principal)
            .or_default()
            .insert(profile.clone());
        Ok(())
    }

    /// 撤销 principal 对指定 profile 的后续 provision 权限。
    pub async fn revoke_profile_grant(
        &self,
        principal: &RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> bool {
        self.inner
            .profile_grants
            .write()
            .await
            .get_mut(principal)
            .is_some_and(|profiles| profiles.remove(profile))
    }

    /// 按 Host 已授予的命名 profile 创建独立 controller。
    pub async fn provision(
        &self,
        principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<ProvisionedAgentRuntime> {
        self.inner.ensure_principal_active(&principal).await?;
        let allowed = self
            .inner
            .profile_grants
            .read()
            .await
            .get(&principal)
            .is_some_and(|profiles| profiles.contains(profile));
        if !allowed {
            return Err(AgentRuntimeError::ProfileDenied {
                principal,
                profile: profile.clone(),
            });
        }
        let selected = self
            .inner
            .profiles
            .read()
            .await
            .get(profile)
            .cloned()
            .ok_or_else(|| AgentRuntimeError::ProfileNotFound(profile.clone()))?;
        let controller = self
            .attach_root_for(principal.clone(), selected.template, selected.permissions)
            .await?;
        let api = self.api_for(principal, &controller.id).await?;
        Ok(ProvisionedAgentRuntime { controller, api })
    }

    async fn spawn_from(
        &self,
        principal: &RuntimePrincipal,
        parent_id: &AgentId,
        request: AgentSpawnRequest,
    ) -> RuntimeResult<AgentHandle> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let parent = self.inner.ensure_owned(principal, parent_id).await?;
        if parent.status().is_terminal() {
            return Err(AgentRuntimeError::AgentInactive(parent_id.clone()));
        }

        let depth = parent.lineage.depth + 1;
        if depth > self.inner.limits.max_depth {
            return Err(AgentRuntimeError::MaxDepthExceeded {
                limit: self.inner.limits.max_depth,
            });
        }
        parent.reserve_child(self.inner.limits.max_children_per_agent)?;

        let permissions = parent.permissions.restrict(&request.derive.permissions);
        let id = AgentId::new();
        let lineage = AgentLineage {
            parent: Some(parent_id.clone()),
            root: parent.lineage.root.clone(),
            depth,
        };
        let entry = AgentEntry::new(
            id.clone(),
            lineage.clone(),
            principal.clone(),
            parent.template.clone(),
            permissions,
            request.derive.options.clone(),
            AgentStatus::Queued,
        );
        self.inner
            .agents
            .write()
            .await
            .insert(id.clone(), entry.clone());

        let inner = self.inner.clone();
        let task_entry = entry.clone();
        let task = tokio::spawn(async move {
            let future = run_agent_task(inner, task_entry.clone(), request.input, None);
            match AssertUnwindSafe(future).catch_unwind().await {
                Ok(completion) => {
                    task_entry.finish_with_session(completion.outcome, completion.session);
                }
                Err(payload) => {
                    task_entry.finish(AgentOutcome::Failed {
                        error: format!("Agent 运行任务 panic：{}", panic_message(payload)),
                    });
                }
            }
        });
        entry.set_abort_handle(task.abort_handle());

        Ok(AgentHandle { id, lineage })
    }

    async fn continue_from(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
        input: String,
    ) -> RuntimeResult<AgentHandle> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        let source = self.inner.entry(target).await?;
        let session = source
            .continuation_session()
            .ok_or_else(|| AgentRuntimeError::SessionUnavailable(target.clone()))?;

        let depth = source.lineage.depth + 1;
        if depth > self.inner.limits.max_depth {
            return Err(AgentRuntimeError::MaxDepthExceeded {
                limit: self.inner.limits.max_depth,
            });
        }
        source.reserve_child(self.inner.limits.max_children_per_agent)?;

        let id = AgentId::new();
        let lineage = AgentLineage {
            parent: Some(target.clone()),
            root: source.lineage.root.clone(),
            depth,
        };
        let entry = AgentEntry::new(
            id.clone(),
            lineage.clone(),
            principal.clone(),
            source.template.clone(),
            source.permissions.clone(),
            source.run_options.clone(),
            AgentStatus::Queued,
        );
        self.inner
            .agents
            .write()
            .await
            .insert(id.clone(), entry.clone());

        let inner = self.inner.clone();
        let task_entry = entry.clone();
        let task = tokio::spawn(async move {
            let future = run_agent_task(inner, task_entry.clone(), input, Some(session));
            match AssertUnwindSafe(future).catch_unwind().await {
                Ok(completion) => {
                    task_entry.finish_with_session(completion.outcome, completion.session);
                }
                Err(payload) => {
                    task_entry.finish(AgentOutcome::Failed {
                        error: format!("Agent 后续运行任务 panic：{}", panic_message(payload)),
                    });
                }
            }
        });
        entry.set_abort_handle(task.abort_handle());

        Ok(AgentHandle { id, lineage })
    }

    async fn snapshot_for(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
    ) -> RuntimeResult<AgentSnapshot> {
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        let entry = self.inner.entry(target).await?;
        Ok(entry.snapshot())
    }

    async fn result_for(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
    ) -> RuntimeResult<Option<AgentOutcome>> {
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        Ok(self.inner.entry(target).await?.outcome())
    }

    async fn wait_for(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
    ) -> RuntimeResult<AgentOutcome> {
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        let entry = self.inner.entry(target).await?;
        if entry.status() == AgentStatus::Ready {
            return Err(AgentRuntimeError::NotRunnable(target.clone()));
        }
        loop {
            let notified = entry.finished.notified();
            if let Some(outcome) = entry.outcome() {
                return Ok(outcome);
            }
            notified.await;
        }
    }

    async fn subscribe_for(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
    ) -> RuntimeResult<AgentEventStream> {
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        Ok(self.inner.entry(target).await?.subscribe())
    }

    async fn cancel_for(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
    ) -> RuntimeResult<bool> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        let entries = self.inner.descendants_including(target).await?;
        let mut changed = false;
        for entry in entries {
            if entry.finish(AgentOutcome::Cancelled) {
                entry.abort();
                changed = true;
            }
        }
        Ok(changed)
    }
}

/// 身份绑定 API 的私有实现，阻止调用方自行填写发送者或父节点。
struct BoundAgentRuntime {
    runtime: AgentRuntime,
    principal: RuntimePrincipal,
    identity: AgentId,
}

#[async_trait]
impl AgentRuntimeApi for BoundAgentRuntime {
    fn principal(&self) -> RuntimePrincipal {
        self.principal.clone()
    }

    fn identity(&self) -> AgentId {
        self.identity.clone()
    }

    async fn spawn(&self, request: AgentSpawnRequest) -> RuntimeResult<AgentHandle> {
        self.runtime
            .spawn_from(&self.principal, &self.identity, request)
            .await
    }

    async fn continue_agent(&self, target: &AgentId, input: String) -> RuntimeResult<AgentHandle> {
        self.runtime
            .continue_from(&self.principal, &self.identity, target, input)
            .await
    }

    async fn status(&self, target: &AgentId) -> RuntimeResult<AgentSnapshot> {
        self.runtime
            .snapshot_for(&self.principal, &self.identity, target)
            .await
    }

    async fn result(&self, target: &AgentId) -> RuntimeResult<Option<AgentOutcome>> {
        self.runtime
            .result_for(&self.principal, &self.identity, target)
            .await
    }

    async fn wait(&self, target: &AgentId) -> RuntimeResult<AgentOutcome> {
        self.runtime
            .wait_for(&self.principal, &self.identity, target)
            .await
    }

    async fn cancel(&self, target: &AgentId) -> RuntimeResult<bool> {
        self.runtime
            .cancel_for(&self.principal, &self.identity, target)
            .await
    }

    async fn subscribe(&self, target: &AgentId) -> RuntimeResult<AgentEventStream> {
        self.runtime
            .subscribe_for(&self.principal, &self.identity, target)
            .await
    }
}

#[async_trait]
impl AgentRuntimeProvisioner for AgentRuntime {
    async fn grant_profile(
        &self,
        principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<()> {
        AgentRuntime::grant_profile(self, principal, profile).await
    }

    async fn provision(
        &self,
        principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<ProvisionedAgentRuntime> {
        AgentRuntime::provision(self, principal, profile).await
    }

    async fn revoke_profile_grant(
        &self,
        principal: &RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> bool {
        AgentRuntime::revoke_profile_grant(self, principal, profile).await
    }

    async fn revoke(&self, principal: &RuntimePrincipal) -> usize {
        self.revoke_principal(principal).await
    }
}

/// Runtime 的共享内部状态。
struct RuntimeInner {
    limits: RuntimeLimits,
    semaphore: Arc<Semaphore>,
    agents: AsyncRwLock<HashMap<AgentId, Arc<AgentEntry>>>,
    revoked_principals: AsyncRwLock<HashSet<RuntimePrincipal>>,
    profiles: AsyncRwLock<HashMap<AgentProfileId, AgentProfile>>,
    profile_grants: AsyncRwLock<HashMap<RuntimePrincipal, BTreeSet<AgentProfileId>>>,
    lifecycle: AsyncMutex<()>,
}

/// Host 注册的模板和初始权限。
#[derive(Clone)]
struct AgentProfile {
    template: AgentTemplate,
    permissions: AgentPermissions,
}

impl RuntimeInner {
    async fn ensure_principal_active(&self, principal: &RuntimePrincipal) -> RuntimeResult<()> {
        if self.revoked_principals.read().await.contains(principal) {
            return Err(AgentRuntimeError::PrincipalRevoked(principal.clone()));
        }
        Ok(())
    }

    async fn entry(&self, id: &AgentId) -> RuntimeResult<Arc<AgentEntry>> {
        self.agents
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| AgentRuntimeError::AgentNotFound(id.clone()))
    }

    async fn ensure_owned(
        &self,
        principal: &RuntimePrincipal,
        id: &AgentId,
    ) -> RuntimeResult<Arc<AgentEntry>> {
        self.ensure_principal_active(principal).await?;
        let entry = self.entry(id).await?;
        if &entry.owner != principal {
            return Err(AgentRuntimeError::OwnerMismatch {
                principal: principal.clone(),
                agent: id.clone(),
            });
        }
        Ok(entry)
    }

    async fn ensure_manageable(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
    ) -> RuntimeResult<()> {
        self.ensure_owned(principal, caller).await?;
        let agents = self.agents.read().await;
        if !agents.contains_key(caller) {
            return Err(AgentRuntimeError::AgentNotFound(caller.clone()));
        }
        let mut current = agents
            .get(target)
            .ok_or_else(|| AgentRuntimeError::AgentNotFound(target.clone()))?;
        loop {
            if &current.id == caller {
                if &current.owner != principal {
                    return Err(AgentRuntimeError::OwnerMismatch {
                        principal: principal.clone(),
                        agent: current.id.clone(),
                    });
                }
                return Ok(());
            }
            let Some(parent) = &current.lineage.parent else {
                return Err(AgentRuntimeError::PermissionDenied {
                    caller: caller.clone(),
                    target: target.clone(),
                });
            };
            current = agents
                .get(parent)
                .ok_or_else(|| AgentRuntimeError::AgentNotFound(parent.clone()))?;
        }
    }

    async fn descendants_including(&self, root: &AgentId) -> RuntimeResult<Vec<Arc<AgentEntry>>> {
        let agents = self.agents.read().await;
        if !agents.contains_key(root) {
            return Err(AgentRuntimeError::AgentNotFound(root.clone()));
        }
        Ok(agents
            .values()
            .filter(|entry| {
                let mut current = Some(entry.id.clone());
                while let Some(id) = current {
                    if &id == root {
                        return true;
                    }
                    current = agents
                        .get(&id)
                        .and_then(|candidate| candidate.lineage.parent.clone());
                }
                false
            })
            .cloned()
            .collect())
    }
}

/// 一个已登记 Agent 的状态和派生上下文。
struct AgentEntry {
    id: AgentId,
    lineage: AgentLineage,
    owner: RuntimePrincipal,
    template: AgentTemplate,
    permissions: AgentPermissions,
    run_options: AgentOptionsPatch,
    status: RwLock<AgentStatus>,
    outcome: RwLock<Option<AgentOutcome>>,
    session: RwLock<Option<Session>>,
    finished: Notify,
    abort_handle: Mutex<Option<AbortHandle>>,
    child_count: AtomicUsize,
    /// 当前事件订阅者；运行任务通过 [`SubscriberEventSink`] 共享此列表，
    /// 终态时清空以结束所有订阅流。
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>>,
}

impl AgentEntry {
    fn new(
        id: AgentId,
        lineage: AgentLineage,
        owner: RuntimePrincipal,
        template: AgentTemplate,
        permissions: AgentPermissions,
        run_options: AgentOptionsPatch,
        status: AgentStatus,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            lineage,
            owner,
            template,
            permissions,
            run_options,
            status: RwLock::new(status),
            outcome: RwLock::new(None),
            session: RwLock::new(None),
            finished: Notify::new(),
            abort_handle: Mutex::new(None),
            child_count: AtomicUsize::new(0),
            subscribers: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn status(&self) -> AgentStatus {
        *self.status.read().expect("Agent 状态锁不应中毒")
    }

    fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            id: self.id.clone(),
            lineage: self.lineage.clone(),
            status: self.status(),
            permissions: self.permissions.clone(),
        }
    }

    fn reserve_child(&self, limit: usize) -> RuntimeResult<()> {
        self.child_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < limit).then_some(count + 1)
            })
            .map(|_| ())
            .map_err(|_| AgentRuntimeError::MaxChildrenExceeded { limit })
    }

    fn mark_running(&self) -> bool {
        let mut status = self.status.write().expect("Agent 状态锁不应中毒");
        if *status != AgentStatus::Queued {
            return false;
        }
        *status = AgentStatus::Running;
        true
    }

    fn outcome(&self) -> Option<AgentOutcome> {
        self.outcome.read().expect("Agent 结果锁不应中毒").clone()
    }

    fn continuation_session(&self) -> Option<Session> {
        if self.status() != AgentStatus::Succeeded {
            return None;
        }
        self.session
            .read()
            .expect("Agent 私有会话锁不应中毒")
            .clone()
    }

    fn finish(&self, outcome: AgentOutcome) -> bool {
        self.finish_with_session(outcome, None)
    }

    fn finish_with_session(&self, outcome: AgentOutcome, session: Option<Session>) -> bool {
        let mut status = self.status.write().expect("Agent 状态锁不应中毒");
        if status.is_terminal() {
            return false;
        }
        *self.session.write().expect("Agent 私有会话锁不应中毒") = session;
        *self.outcome.write().expect("Agent 结果锁不应中毒") = Some(outcome.clone());
        *status = outcome.status();
        drop(status);
        self.finished.notify_waiters();
        // 丢弃所有订阅发送端，让事件流在缓冲耗尽后自然结束。
        self.subscribers
            .lock()
            .expect("事件订阅者锁不应中毒")
            .clear();
        true
    }

    /// 创建一个新的事件订阅流；目标已处于终态时返回立即结束的空流。
    fn subscribe(&self) -> AgentEventStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut subscribers = self.subscribers.lock().expect("事件订阅者锁不应中毒");
        // 在持有订阅者锁的前提下检查终态，避免与 finish 清空订阅者竞争。
        if !self.status().is_terminal() {
            subscribers.push(sender);
        }
        AgentEventStream { receiver }
    }

    fn set_abort_handle(&self, handle: AbortHandle) {
        *self.abort_handle.lock().expect("Agent 取消句柄锁不应中毒") = Some(handle);
    }

    fn abort(&self) {
        if let Some(handle) = self
            .abort_handle
            .lock()
            .expect("Agent 取消句柄锁不应中毒")
            .as_ref()
        {
            handle.abort();
        }
    }
}

/// 单次后台运行返回的公开终态与 Runtime 私有会话。
struct AgentTaskCompletion {
    outcome: AgentOutcome,
    session: Option<Session>,
}

/// 执行一个排队任务，并把所有错误转换为稳定终态。
async fn run_agent_task(
    inner: Arc<RuntimeInner>,
    entry: Arc<AgentEntry>,
    input: String,
    session: Option<Session>,
) -> AgentTaskCompletion {
    let permit = match inner.semaphore.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => {
            return AgentTaskCompletion {
                outcome: AgentOutcome::Failed {
                    error: format!("运行时并发控制已关闭：{error}"),
                },
                session: None,
            };
        }
    };
    if !entry.mark_running() {
        return AgentTaskCompletion {
            outcome: entry.outcome().unwrap_or(AgentOutcome::Cancelled),
            session: None,
        };
    }

    let (mut agent, _) = match entry.template.instantiate(
        &entry.permissions,
        &AgentDeriveConfig {
            options: entry.run_options.clone(),
            permissions: AgentPermissions::default(),
        },
    ) {
        Ok(value) => value,
        Err(error) => {
            return AgentTaskCompletion {
                outcome: AgentOutcome::Failed {
                    error: error.to_string(),
                },
                session: None,
            };
        }
    };

    // 在模板 sink 之外叠加订阅转发，让 subscribe 拿到本 Agent 的事件流。
    let mut sink = CompositeEventSink::new();
    sink.push(agent.event_sink());
    sink.push(Arc::new(SubscriberEventSink {
        subscribers: entry.subscribers.clone(),
    }));
    agent.set_event_sink(Arc::new(sink));

    let result = match session {
        Some(session) => agent.run_continue(session, input).await,
        None => agent.run(input).await,
    };
    drop(permit);
    match result {
        // Core 层优雅取消映射为 Runtime 的取消终态，不保留续跑会话。
        Ok(run) if run.cancelled => AgentTaskCompletion {
            outcome: AgentOutcome::Cancelled,
            session: None,
        },
        Ok(run) => AgentTaskCompletion {
            session: Some(run.session.clone()),
            outcome: AgentOutcome::Succeeded { result: run.into() },
        },
        Err(error) => AgentTaskCompletion {
            outcome: AgentOutcome::Failed {
                error: error.to_string(),
            },
            session: None,
        },
    }
}

/// 将 panic 载荷转换为可诊断文本。
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "未知 panic 载荷".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ChatModel, ModelRequest, ModelResponse, ProviderAdapter};
    use agent_tool::JsonTool;
    use anyhow::Result;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 返回固定文本的测试模型。
    struct FixedModel;

    #[async_trait]
    impl ChatModel for FixedModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse::text("完成"))
        }
    }

    #[async_trait]
    impl ProviderAdapter for FixedModel {
        fn name(&self) -> &'static str {
            "fixed"
        }
    }

    /// 等待通知的测试模型，用于验证取消语义。
    struct BlockingModel {
        entered: Arc<AtomicBool>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl ChatModel for BlockingModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            self.entered.store(true, Ordering::Release);
            self.release.notified().await;
            Ok(ModelResponse::text("不应完成"))
        }
    }

    #[async_trait]
    impl ProviderAdapter for BlockingModel {
        fn name(&self) -> &'static str {
            "blocking"
        }
    }

    /// 由测试信号逐次放行的模型，用于验证全局并发上限。
    struct ConcurrencyModel {
        current: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
    }

    #[async_trait]
    impl ChatModel for ConcurrencyModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            let current = self.current.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum.fetch_max(current, Ordering::AcqRel);
            let permit = self.release.acquire().await.expect("测试信号不应关闭");
            permit.forget();
            self.current.fetch_sub(1, Ordering::AcqRel);
            Ok(ModelResponse::text("完成"))
        }
    }

    #[async_trait]
    impl ProviderAdapter for ConcurrencyModel {
        fn name(&self) -> &'static str {
            "concurrency"
        }
    }

    /// 使用指定模型和工具名称构造测试模板。
    fn template(
        adapter: Arc<dyn ProviderAdapter>,
        provider: &str,
        tools: &[&str],
    ) -> AgentTemplate {
        let mut gateway = ModelGateway::new();
        gateway
            .register(provider, adapter)
            .expect("测试模型应成功注册");
        let mut registry = ToolRegistry::new();
        for name in tools {
            registry
                .register(JsonTool::new(
                    ToolSpec::new(*name, "测试工具", ToolSpec::empty_object_schema()),
                    |_| async { Ok(json!({"ok": true})) },
                ))
                .expect("测试工具应成功注册");
        }
        let agent = Agent::new(
            gateway,
            AgentOptions::default().with_model_route(provider, "test-model"),
        )
        .with_tools(registry);
        AgentTemplate::from_agent(&agent)
    }

    /// allowlist 必须同时收缩有效权限和模型可见工具。
    #[tokio::test]
    async fn derivation_filters_tools_and_cannot_expand_parent_permission() {
        let template = template(Arc::new(FixedModel), "fixed", &["read", "write"]);
        let parent = AgentPermissions {
            tools: ToolAccess::allowlist(["read"]),
        };
        let config = AgentDeriveConfig {
            permissions: AgentPermissions {
                tools: ToolAccess::All,
            },
            ..AgentDeriveConfig::default()
        };
        let (agent, effective) = template
            .instantiate(&parent, &config)
            .expect("派生 Agent 应成功");

        assert_eq!(effective, parent);
        let specs = agent.tool_specs().await.expect("读取工具定义应成功");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "read");
    }

    /// 派生 API 应立即返回，并提供完整父子谱系和成功终态。
    #[tokio::test]
    async fn spawn_returns_handle_and_reaches_success_terminal_state() {
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(Arc::new(FixedModel), "fixed", &[]),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let api = runtime.api(&root.id).await.expect("绑定根 API");
        let child = api
            .spawn(AgentSpawnRequest::new("执行任务"))
            .await
            .expect("派生 Agent");

        assert_eq!(child.lineage.parent.as_ref(), Some(&root.id));
        assert_eq!(child.lineage.root, root.id);
        assert_eq!(child.lineage.depth, 1);
        let outcome = api.wait(&child.id).await.expect("等待子 Agent");
        assert!(matches!(
            outcome,
            AgentOutcome::Succeeded {
                result: AgentExecutionResult { final_text, .. }
            } if final_text == "完成"
        ));
        assert!(!api.cancel(&child.id).await.expect("重复终态操作应成功"));
    }

    /// 成功终态应保留私有会话，并允许有权 controller 创建权限不扩大的后续运行。
    #[tokio::test]
    async fn continue_agent_reuses_private_session_and_preserves_permissions() {
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let permissions = AgentPermissions {
            tools: ToolAccess::allowlist(["read"]),
        };
        let root = runtime
            .attach_root(
                template(Arc::new(FixedModel), "fixed", &["read", "write"]),
                permissions.clone(),
            )
            .await
            .expect("挂载根 Agent");
        let api = runtime.api(&root.id).await.expect("绑定根 API");
        let child = api
            .spawn(AgentSpawnRequest::new("首次任务"))
            .await
            .expect("派生 Agent");
        api.wait(&child.id).await.expect("等待首次运行");

        let continued = api
            .continue_agent(&child.id, "后续任务".to_string())
            .await
            .expect("创建后续运行");
        assert_eq!(continued.lineage.parent.as_ref(), Some(&child.id));
        assert_eq!(continued.lineage.depth, child.lineage.depth + 1);
        let snapshot = api.status(&continued.id).await.expect("查询后续运行");
        assert_eq!(snapshot.permissions, permissions);
        assert!(matches!(
            api.wait(&continued.id).await.expect("等待后续运行"),
            AgentOutcome::Succeeded { .. }
        ));
    }

    /// 深度和累计子节点限制必须在启动模型任务前生效。
    #[tokio::test]
    async fn topology_limits_reject_excess_children_and_depth() {
        let limits = RuntimeLimits {
            max_depth: 1,
            max_children_per_agent: 1,
            ..RuntimeLimits::default()
        };
        let runtime = AgentRuntime::new(limits).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(Arc::new(FixedModel), "fixed", &[]),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let root_api = runtime.api(&root.id).await.expect("绑定根 API");
        let child = root_api
            .spawn(AgentSpawnRequest::new("第一个"))
            .await
            .expect("第一个子节点应成功");
        let error = root_api
            .spawn(AgentSpawnRequest::new("第二个"))
            .await
            .expect_err("第二个子节点应被拒绝");
        assert!(matches!(
            error,
            AgentRuntimeError::MaxChildrenExceeded { limit: 1 }
        ));

        let child_api = runtime.api(&child.id).await.expect("绑定子 API");
        let error = child_api
            .spawn(AgentSpawnRequest::new("孙节点"))
            .await
            .expect_err("孙节点应超过深度限制");
        assert!(matches!(
            error,
            AgentRuntimeError::MaxDepthExceeded { limit: 1 }
        ));
    }

    /// 全局并发限制必须让额外任务保持排队，且每个任务仍使用独立 Agent。
    #[tokio::test]
    async fn concurrency_limit_keeps_excess_agents_queued() {
        let current = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let runtime = AgentRuntime::new(RuntimeLimits {
            max_concurrent_agents: 1,
            ..RuntimeLimits::default()
        })
        .expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(
                    Arc::new(ConcurrencyModel {
                        current: current.clone(),
                        maximum: maximum.clone(),
                        release: release.clone(),
                    }),
                    "concurrency",
                    &[],
                ),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let api = runtime.api(&root.id).await.expect("绑定根 API");
        let first = api
            .spawn(AgentSpawnRequest::new("一"))
            .await
            .expect("派生第一个");
        let second = api
            .spawn(AgentSpawnRequest::new("二"))
            .await
            .expect("派生第二个");

        for _ in 0..100 {
            if current.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(current.load(Ordering::Acquire), 1);
        let statuses = [
            api.status(&first.id).await.expect("读取第一个状态").status,
            api.status(&second.id).await.expect("读取第二个状态").status,
        ];
        assert!(statuses.contains(&AgentStatus::Running));
        assert!(statuses.contains(&AgentStatus::Queued));

        release.add_permits(2);
        api.wait(&first.id).await.expect("第一个应完成");
        api.wait(&second.id).await.expect("第二个应完成");
        assert_eq!(maximum.load(Ordering::Acquire), 1);
    }

    /// Provisioner 必须覆盖授权、独立 controller 创建和 principal 清理生命周期。
    #[tokio::test]
    async fn provisioner_grants_profiles_and_revoke_cleans_owned_agents() {
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let profile = AgentProfileId::new("default-agent").expect("创建 profile ID");
        runtime
            .register_profile(
                profile.clone(),
                template(Arc::new(FixedModel), "fixed", &[]),
                AgentPermissions::default(),
            )
            .await
            .expect("注册 profile");
        let principal =
            RuntimePrincipal::new("component:test:activation-1").expect("创建激活 principal");
        let provisioner: Arc<dyn AgentRuntimeProvisioner> = Arc::new(runtime.clone());

        let denied = provisioner.provision(principal.clone(), &profile).await;
        assert!(matches!(
            denied,
            Err(AgentRuntimeError::ProfileDenied { .. })
        ));
        provisioner
            .grant_profile(principal.clone(), &profile)
            .await
            .expect("授予 profile");
        let provisioned = provisioner
            .provision(principal.clone(), &profile)
            .await
            .expect("创建 controller");
        assert_eq!(provisioned.api.principal(), principal);
        assert_eq!(provisioned.api.identity(), provisioned.controller.id);

        let wrong_principal =
            RuntimePrincipal::new("component:other:activation-1").expect("创建另一 principal");
        let wrong_binding = runtime
            .api_for(wrong_principal, &provisioned.controller.id)
            .await;
        assert!(matches!(
            wrong_binding,
            Err(AgentRuntimeError::OwnerMismatch { .. })
        ));

        assert_eq!(provisioner.revoke(&principal).await, 1);
        assert_eq!(provisioner.revoke(&principal).await, 0);
        assert!(!runtime
            .inner
            .agents
            .read()
            .await
            .contains_key(&provisioned.controller.id));
        let error = provisioned
            .api
            .spawn(AgentSpawnRequest::new("撤销后不得运行"))
            .await
            .expect_err("撤销后的 API 应失效");
        assert_eq!(error, AgentRuntimeError::PrincipalRevoked(principal));
    }

    /// 取消必须写入不可覆盖的终态，并中止阻塞中的模型调用。
    #[tokio::test]
    async fn cancellation_is_idempotent_and_terminal() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(
                    Arc::new(BlockingModel {
                        entered: entered.clone(),
                        release,
                    }),
                    "blocking",
                    &[],
                ),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let api = runtime.api(&root.id).await.expect("绑定根 API");
        let child = api
            .spawn(AgentSpawnRequest::new("阻塞任务"))
            .await
            .expect("派生阻塞 Agent");

        for _ in 0..100 {
            if entered.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(entered.load(Ordering::Acquire));
        let child_api = runtime.api(&child.id).await.expect("绑定子 Agent API");
        let grandchild = child_api
            .spawn(AgentSpawnRequest::new("后代阻塞任务"))
            .await
            .expect("派生后代 Agent");
        assert!(api.cancel(&child.id).await.expect("首次取消"));
        assert!(!api.cancel(&child.id).await.expect("重复取消"));
        assert_eq!(
            api.wait(&child.id).await.expect("读取取消终态"),
            AgentOutcome::Cancelled
        );
        assert_eq!(
            api.wait(&grandchild.id).await.expect("读取后代取消终态"),
            AgentOutcome::Cancelled
        );
        assert_eq!(
            api.status(&child.id).await.expect("读取状态").status,
            AgentStatus::Cancelled
        );
    }

    /// 兄弟节点不能查询或取消彼此，但仍可通过已知身份发送消息。
    #[tokio::test]
    async fn management_is_descendant_scoped() {
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(Arc::new(FixedModel), "fixed", &[]),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let root_api = runtime.api(&root.id).await.expect("绑定根 API");
        let first = root_api
            .spawn(AgentSpawnRequest::new("一"))
            .await
            .expect("派生第一个");
        let second = root_api
            .spawn(AgentSpawnRequest::new("二"))
            .await
            .expect("派生第二个");
        let first_api = runtime.api(&first.id).await.expect("绑定第一个 API");

        let error = first_api
            .status(&second.id)
            .await
            .expect_err("兄弟节点不能读取状态");
        assert!(matches!(error, AgentRuntimeError::PermissionDenied { .. }));
    }

    /// 订阅应收到订阅之后的事件，并在目标进入终态后自然结束。
    #[tokio::test]
    async fn subscribe_streams_events_until_terminal() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(
                    Arc::new(BlockingModel {
                        entered: entered.clone(),
                        release: release.clone(),
                    }),
                    "blocking",
                    &[],
                ),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let api = runtime.api(&root.id).await.expect("绑定根 API");
        let child = api
            .spawn(AgentSpawnRequest::new("阻塞任务"))
            .await
            .expect("派生阻塞 Agent");

        let mut stream = api.subscribe(&child.id).await.expect("订阅子 Agent 事件");
        for _ in 0..100 {
            if entered.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(entered.load(Ordering::Acquire));
        release.notify_one();

        let mut kinds = Vec::new();
        while let Some(event) = stream.next().await {
            kinds.push(event.kind);
        }
        assert!(kinds.contains(&agent_core::AgentEventKind::RunFinished));
        assert_eq!(
            api.wait(&child.id).await.expect("读取终态").status(),
            AgentStatus::Succeeded
        );
    }

    /// 目标已处于终态时订阅返回立即结束的空流。
    #[tokio::test]
    async fn subscribe_after_terminal_returns_ended_stream() {
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(Arc::new(FixedModel), "fixed", &[]),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let api = runtime.api(&root.id).await.expect("绑定根 API");
        let child = api
            .spawn(AgentSpawnRequest::new("执行任务"))
            .await
            .expect("派生 Agent");
        api.wait(&child.id).await.expect("等待终态");

        let mut stream = api.subscribe(&child.id).await.expect("终态后订阅");
        assert!(stream.next().await.is_none());
    }

    /// 订阅受后代范围限制：兄弟节点不能订阅彼此的事件。
    #[tokio::test]
    async fn subscribe_is_descendant_scoped() {
        let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
        let root = runtime
            .attach_root(
                template(Arc::new(FixedModel), "fixed", &[]),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载根 Agent");
        let root_api = runtime.api(&root.id).await.expect("绑定根 API");
        let first = root_api
            .spawn(AgentSpawnRequest::new("一"))
            .await
            .expect("派生第一个");
        let second = root_api
            .spawn(AgentSpawnRequest::new("二"))
            .await
            .expect("派生第二个");
        let first_api = runtime.api(&first.id).await.expect("绑定第一个 API");

        let error = first_api
            .subscribe(&second.id)
            .await
            .expect_err("兄弟节点不能订阅事件");
        assert!(matches!(error, AgentRuntimeError::PermissionDenied { .. }));
    }
}
