//! Host 与插件边界可使用的 Runtime 控制面协议。

use crate::{
    AgentDeriveConfig, AgentId, AgentLineage, AgentPermissions, AgentProfileId, AgentRuntimeError,
    RuntimePrincipal, RuntimeResult,
};
use agent_core::{AgentEvent, AgentRun, EventSink, TokenUsage};
use agent_tool::ResourceLimits;
use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

/// 单个 Agent 为实时观察者保留的最近事件数量。
const AGENT_EVENT_HISTORY_LIMIT: usize = 512;

/// 单 principal 默认允许登记的 Agent 累计总数。
const DEFAULT_MAX_AGENTS_PER_PRINCIPAL: usize = 128;

/// 为旧版 RuntimeLimits JSON 补充安全的单 principal Agent 总数默认值。
const fn default_max_agents_per_principal() -> usize {
    DEFAULT_MAX_AGENTS_PER_PRINCIPAL
}

/// 取两个可选累计限额中更严格的一个，`None` 视为不设上限。
///
/// 该组合满足「只能收缩」：策略给出上限时收紧，策略未设上限时保持当前值。
fn tighter_limit(current: Option<usize>, requested: Option<usize>) -> Option<usize> {
    match (current, requested) {
        (Some(current), Some(requested)) => Some(current.min(requested)),
        (Some(current), None) => Some(current),
        (None, requested) => requested,
    }
}

/// Agent Runtime 的资源与拓扑限额。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeLimits {
    /// 根节点以下允许的最大派生深度。
    pub max_depth: usize,
    /// 单个父节点在其生命周期内允许创建的累计子节点数量。
    ///
    /// 与 `max_agents_per_principal` 对齐：长期交互 controller 会在一生中顺序派生
    /// 多批成员，容量小于 principal 的 Agent 总数时，会话会在第 16 次派生后永久
    /// 拒绝新成员；真正限制并发扇出的是 `max_concurrent_agents`。需要更严格扇出的
    /// 部署应通过执行策略显式收紧该值。
    pub max_children_per_agent: usize,
    /// 同时执行模型循环的最大 Agent 数量；额外任务保持排队状态。
    pub max_concurrent_agents: usize,
    /// 单个可信 principal 在一次激活生命周期内允许登记的 Agent 累计总数。
    #[serde(default = "default_max_agents_per_principal")]
    pub max_agents_per_principal: usize,
    /// 单个可信 principal 的派生 Agent 允许发起的模型请求累计次数。
    ///
    /// `None` 表示不设累计上限。交互式 Serve 平面默认不设上限：队员可以一直运行到
    /// 任务完成，模型请求次数不应成为会话寿命配额。Evaluation 与 Mutation 等受限
    /// 平面必须显式设置该上限，用于阻断失控候选。
    #[serde(default)]
    pub max_agent_model_requests_per_principal: Option<usize>,
    /// 单个可信 principal 的派生 Agent 允许提交的模型请求累计序列化字节数。
    ///
    /// `None` 表示不设累计上限，此时 Runtime 也不为计费序列化请求，避免为不生效的
    /// 限额复制完整请求。受限平面用该维度限制超大上下文反复上传。
    #[serde(default)]
    pub max_agent_model_request_bytes_per_principal: Option<usize>,
    /// 单个可信 principal 的派生 Agent 允许累计消耗的模型输出 token 总数。
    ///
    /// `None` 表示不设累计上限。设置时按 `max_tokens` 预留，请求结束后按服务商回报的
    /// 实际用量结算并退还差额，因此约束的是真实消耗而不是请求次数乘以上限。
    #[serde(default)]
    pub max_agent_model_output_tokens_per_principal: Option<usize>,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_children_per_agent: 128,
            max_concurrent_agents: 8,
            max_agents_per_principal: DEFAULT_MAX_AGENTS_PER_PRINCIPAL,
            // 原生 Runtime 只提供机制；是否给模型成本设累计上限由应用策略决定。
            max_agent_model_requests_per_principal: None,
            max_agent_model_request_bytes_per_principal: None,
            max_agent_model_output_tokens_per_principal: None,
        }
    }
}

impl RuntimeLimits {
    /// 按执行策略的资源上限收紧运行时限额。
    ///
    /// 逐维度取较小值，策略未设该维度上限时保持原值，因此结果不会放宽任何一项。
    /// `max_children_per_agent`、`max_concurrent_agents` 与 `max_agents_per_principal`
    /// 至少保留 1，以维持 [`RuntimeLimits::validate`] 要求的正数不变量并容纳 controller；
    /// 真正阻断派生依靠 `max_depth`。三项模型成本预算的 `None` 表示不设上限，策略给出
    /// 上限时才会收紧。
    pub fn clamped_by(&self, limits: &ResourceLimits) -> Self {
        Self {
            max_depth: limits
                .max_depth
                .map_or(self.max_depth, |limit| self.max_depth.min(limit)),
            max_children_per_agent: limits
                .max_children_per_agent
                .map_or(self.max_children_per_agent, |limit| {
                    self.max_children_per_agent.min(limit).max(1)
                }),
            max_concurrent_agents: limits
                .max_concurrent_agents
                .map_or(self.max_concurrent_agents, |limit| {
                    self.max_concurrent_agents.min(limit).max(1)
                }),
            max_agents_per_principal: limits
                .max_agents_per_principal
                .map_or(self.max_agents_per_principal, |limit| {
                    self.max_agents_per_principal.min(limit).max(1)
                }),
            max_agent_model_requests_per_principal: tighter_limit(
                self.max_agent_model_requests_per_principal,
                limits.max_agent_model_requests_per_principal,
            ),
            max_agent_model_request_bytes_per_principal: tighter_limit(
                self.max_agent_model_request_bytes_per_principal,
                limits.max_agent_model_request_bytes_per_principal,
            ),
            max_agent_model_output_tokens_per_principal: tighter_limit(
                self.max_agent_model_output_tokens_per_principal,
                limits.max_agent_model_output_tokens_per_principal,
            ),
        }
    }

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
        if self.max_agents_per_principal == 0 {
            return Err(AgentRuntimeError::InvalidLimits(
                "max_agents_per_principal 必须大于零".to_string(),
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
    /// 结束探测已经取出但尚未交付的事件。
    pending: Option<AgentEvent>,
}

impl AgentEventStream {
    /// 从 Runtime 内部订阅通道创建事件流，不向外暴露接收端控制权。
    pub(crate) fn new(receiver: mpsc::UnboundedReceiver<AgentEvent>) -> Self {
        Self {
            receiver,
            pending: None,
        }
    }

    /// 创建立即结束的空事件流，供无历史的 Runtime 适配器返回。
    pub fn empty() -> Self {
        let (_sender, receiver) = mpsc::unbounded_channel();
        Self::new(receiver)
    }

    /// 取出下一条事件；目标终态且缓冲耗尽后返回 `None`。
    pub async fn next(&mut self) -> Option<AgentEvent> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        self.receiver.recv().await
    }

    /// 非阻塞取出一条已缓冲事件；当前没有可用事件时返回 `None`。
    pub fn try_next(&mut self) -> Option<AgentEvent> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        self.receiver.try_recv().ok()
    }

    /// 判断本次运行的流是否已经结束且没有待取事件。
    ///
    /// 同一 Agent 身份可以被反复续跑：一次运行结束后发送端全部丢弃，流随即结束，
    /// 观察者需要据此为下一次运行重新订阅。判断过程取到的事件会暂存，不会丢失。
    pub fn is_finished(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        match self.receiver.try_recv() {
            Ok(event) => {
                self.pending = Some(event);
                false
            }
            Err(mpsc::error::TryRecvError::Empty) => false,
            Err(mpsc::error::TryRecvError::Disconnected) => true,
        }
    }
}

/// 把 Core Agent 事件转发给当前订阅者的事件 sink。
///
/// 发送失败（订阅方已放弃接收）的通道会被移除，不影响其余订阅者。
pub(crate) struct SubscriberEventSink {
    pub(crate) subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>>,
    pub(crate) history: Arc<Mutex<VecDeque<AgentEvent>>>,
}

/// Runtime 即将启动一次 Core Run 时提供给可信 Host 观察器的上下文。
///
/// 本结构只描述当前 Runtime 身份和派生关系，不接受 Guest 提供的 owner 或 Run ID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRunContext {
    /// 即将运行的 Runtime Agent 身份。
    pub agent_id: AgentId,
    /// Runtime 内可信维护的父子派生关系。
    pub lineage: AgentLineage,
}

/// Runtime 观察到的 Core Run 终止形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeRunTermination {
    /// Core 正常完成且没有取消。
    Completed,
    /// Runtime 外部取消了 Core future，或 Core 自身通过取消路径优雅收尾。
    Cancelled,
    /// Core、事件 sink、模型、工具或扩展返回错误。
    Failed,
}

/// 一次 Runtime Run 结束后的可信收敛回调。
#[async_trait]
pub trait RuntimeRunFinalizer: Send + Sync {
    /// 使用 Runtime 判定的终止形态收敛外部证据。
    ///
    /// # Errors
    ///
    /// 证据或审计制品无法完整写入时返回错误；Runtime 会把该运行判定为失败。
    async fn finish(&self, termination: RuntimeRunTermination) -> RuntimeResult<()>;
}

/// Host 观察器为单次 Core Run 提供的固定 ID、事件 sink 和收敛器。
pub struct RuntimeRunObservation {
    run_id: String,
    event_sink: Arc<dyn EventSink>,
    finalizer: Arc<dyn RuntimeRunFinalizer>,
}

impl RuntimeRunObservation {
    /// 创建一次可信运行观察绑定。
    ///
    /// # Errors
    ///
    /// `run_id` 为空时返回 [`AgentRuntimeError::RunObservation`]。
    pub fn new(
        run_id: impl Into<String>,
        event_sink: Arc<dyn EventSink>,
        finalizer: Arc<dyn RuntimeRunFinalizer>,
    ) -> RuntimeResult<Self> {
        let run_id = run_id.into();
        if run_id.is_empty() {
            return Err(AgentRuntimeError::RunObservation(
                "运行观察器返回了空 Run ID".into(),
            ));
        }
        Ok(Self {
            run_id,
            event_sink,
            finalizer,
        })
    }

    /// 分解为 Runtime 执行路径使用的固定绑定。
    pub(crate) fn into_parts(self) -> (String, Arc<dyn EventSink>, Arc<dyn RuntimeRunFinalizer>) {
        (self.run_id, self.event_sink, self.finalizer)
    }
}

/// Host 可选注入的 Runtime Run 观察器。
///
/// 观察器运行在 Guest 和普通插件权限之外，可预登记 Run，但不能改变
/// Agent 权限、输入、执行策略或 Runtime lineage。
#[async_trait]
pub trait RuntimeRunObserver: Send + Sync {
    /// 在 Core Run 启动前创建固定观察绑定。
    ///
    /// # Errors
    ///
    /// 无法预登记证据或生成合法运行绑定时返回错误；Core 不会在失败后继续运行。
    async fn begin(&self, context: RuntimeRunContext) -> RuntimeResult<RuntimeRunObservation>;
}

#[async_trait]
impl EventSink for SubscriberEventSink {
    async fn record(&self, event: &AgentEvent) -> AnyResult<()> {
        let mut history = self.history.lock().expect("Agent 事件历史锁不应中毒");
        if history.len() >= AGENT_EVENT_HISTORY_LIMIT {
            history.pop_front();
        }
        history.push_back(event.clone());
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

    /// 从自身或后代 Agent 的成功终态会话创建后续运行，并立即返回同一身份的句柄。
    ///
    /// Runtime 只复用目标的私有会话、运行模板和有效权限，不向调用方返回原始会话，
    /// 新运行的权限也不会超过目标 Agent 已经持有的范围。续跑复用同一 `AgentId`、
    /// 谱系和子节点计数，不登记新 Agent：队长可以一直派活，队员可以一直接活。
    async fn continue_agent(&self, target: &AgentId, input: String) -> RuntimeResult<AgentHandle>;

    /// 向排队或运行中的自身/后代 Agent 注入一条用户消息。
    async fn steer(&self, target: &AgentId, input: String) -> RuntimeResult<()> {
        let _ = input;
        Err(AgentRuntimeError::InteractionUnavailable(target.clone()))
    }

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
    /// 每次运行先回放 Runtime 保留的最近事件，再投递实时事件；本次运行结束且历史
    /// 耗尽后流结束。续跑会清空上一轮历史并开启新的运行，观察者可重新订阅而不重复
    /// 收到旧事件。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认限额只约束拓扑，不把模型成本变成会话寿命配额。
    ///
    /// Runtime 只提供机制：应用未显式设置成本上限时，长期运行的队员不应在某次
    /// 请求后永久失败。受限平面必须自行通过 `clamped_by` 显式收紧。
    #[test]
    fn default_limits_have_no_lifetime_model_cost_quota() {
        let limits = RuntimeLimits::default();

        assert!(
            limits.max_children_per_agent >= limits.max_agents_per_principal,
            "父节点子配额不能先于 principal 的 Agent 总数耗尽"
        );
        assert_eq!(limits.max_agent_model_requests_per_principal, None);
        assert_eq!(limits.max_agent_model_request_bytes_per_principal, None);
        assert_eq!(limits.max_agent_model_output_tokens_per_principal, None);
    }

    /// 策略只能收紧可选成本上限，不能借 `None` 放宽已有硬上限。
    #[test]
    fn policy_clamps_optional_cost_limits_without_widening() {
        let current = RuntimeLimits::default();
        let restricted = ResourceLimits {
            max_agent_model_requests_per_principal: Some(8),
            max_agent_model_request_bytes_per_principal: Some(2048),
            max_agent_model_output_tokens_per_principal: Some(1024),
            ..ResourceLimits::default()
        };

        let clamped = current.clamped_by(&restricted);
        assert_eq!(clamped.max_agent_model_requests_per_principal, Some(8));
        assert_eq!(
            clamped.max_agent_model_request_bytes_per_principal,
            Some(2048)
        );
        assert_eq!(
            clamped.max_agent_model_output_tokens_per_principal,
            Some(1024)
        );

        // 反向组合：策略不设上限时必须保持调用方已有的硬上限。
        let already_limited = clamped.clone();
        let kept = already_limited.clamped_by(&ResourceLimits::default());
        assert_eq!(kept, already_limited);
    }
}
