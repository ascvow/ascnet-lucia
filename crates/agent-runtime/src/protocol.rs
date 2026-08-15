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
    /// 按执行策略的资源上限收紧运行时限额。
    ///
    /// 逐维度取较小值，策略未设该维度上限时保持原值，因此结果不会放宽任何一项。
    /// `max_children_per_agent` 与 `max_concurrent_agents` 至少保留 1，以维持
    /// [`RuntimeLimits::validate`] 要求的正数不变量；真正阻断派生依靠 `max_depth`。
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
    /// 从 Runtime 内部订阅通道创建事件流，不向外暴露接收端控制权。
    pub(crate) fn new(receiver: mpsc::UnboundedReceiver<AgentEvent>) -> Self {
        Self { receiver }
    }

    /// 创建立即结束的空事件流，供无历史的 Runtime 适配器返回。
    pub fn empty() -> Self {
        let (_sender, receiver) = mpsc::unbounded_channel();
        Self { receiver }
    }

    /// 取出下一条事件；目标终态且缓冲耗尽后返回 `None`。
    pub async fn next(&mut self) -> Option<AgentEvent> {
        self.receiver.recv().await
    }

    /// 非阻塞取出一条已缓冲事件；当前没有可用事件时返回 `None`。
    pub fn try_next(&mut self) -> Option<AgentEvent> {
        self.receiver.try_recv().ok()
    }
}

/// 把 Core Agent 事件转发给当前订阅者的事件 sink。
///
/// 发送失败（订阅方已放弃接收）的通道会被移除，不影响其余订阅者。
pub(crate) struct SubscriberEventSink {
    pub(crate) subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>>,
    pub(crate) history: Arc<Mutex<VecDeque<AgentEvent>>>,
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

    /// 从自身或后代 Agent 的成功终态会话创建后续运行，并立即返回新句柄。
    ///
    /// Runtime 只复用目标的私有会话、运行模板和有效权限，不向调用方返回原始会话，
    /// 新运行的权限也不会超过目标 Agent 已经持有的范围。
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
    /// 先回放 Runtime 保留的最近事件，再投递实时事件；目标已处于终态时在历史耗尽后结束。
    /// 实时事件通道不限量缓冲，订阅方应及时消费，避免长时间滞留占用内存。
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
