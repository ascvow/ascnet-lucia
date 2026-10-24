//! Agent 队列调度、生命周期、会话续跑和 principal 资源管理。

use crate::protocol::SubscriberEventSink;
use crate::{
    AgentDeriveConfig, AgentEventStream, AgentHandle, AgentId, AgentLineage, AgentOptionsPatch,
    AgentOutcome, AgentPermissions, AgentProfileId, AgentRuntimeApi, AgentRuntimeError,
    AgentRuntimeProvisioner, AgentSnapshot, AgentSpawnRequest, AgentStatus, AgentTemplate,
    ProvisionedAgentRuntime, RuntimeLimits, RuntimePrincipal, RuntimeResult, RuntimeRunContext,
    RuntimeRunObserver, RuntimeRunTermination,
};
use agent_core::{
    AgentControl, AgentEvent, AgentExtension, CompositeEventSink, ModelRequest, ModelRequestGate,
    Session,
};
use async_trait::async_trait;
use futures_util::FutureExt;
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    io::{Error as IoError, Write},
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock, Semaphore};

/// 单个排队或运行中 Agent 允许暂存的 steering 消息数量。
const STEERING_QUEUE_LIMIT: usize = 32;

/// 派生 Agent 未显式设置输出上限时使用的安全单次默认值。
const DEFAULT_DERIVED_AGENT_MAX_TOKENS: u32 = 20_000;

/// Runtime 发出 Core 取消后等待其发布终态并执行扩展清理的最长时间。
const CORE_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Core 被强制丢弃或 panic 后，Runtime 等待扩展兜底清理的最长时间。
const ABORTED_RUN_CLEANUP_PERIOD: Duration = Duration::from_secs(2);

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
        Self::build(limits, None)
    }

    /// 使用指定限额和 Host 可信运行观察器创建 Runtime。
    ///
    /// 观察器只接收 Runtime 维护的身份与 lineage，可为每次 Core Run 提供固定 Run ID、
    /// 事件 sink 和终态收敛器；它不改变 Agent 权限或调度语义。
    ///
    /// # Errors
    ///
    /// Runtime 限额无效时返回错误。
    pub fn new_with_run_observer(
        limits: RuntimeLimits,
        observer: Arc<dyn RuntimeRunObserver>,
    ) -> RuntimeResult<Self> {
        Self::build(limits, Some(observer))
    }

    /// 创建共享内部状态，确保不同构造入口使用同一组限额校验与默认值。
    fn build(
        limits: RuntimeLimits,
        run_observer: Option<Arc<dyn RuntimeRunObserver>>,
    ) -> RuntimeResult<Self> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                semaphore: Arc::new(Semaphore::new(limits.max_concurrent_agents)),
                limits,
                agents: AsyncRwLock::new(HashMap::new()),
                principal_usage: Mutex::new(HashMap::new()),
                revoked_principals: AsyncRwLock::new(HashSet::new()),
                profiles: AsyncRwLock::new(HashMap::new()),
                profile_grants: AsyncRwLock::new(HashMap::new()),
                lifecycle: AsyncMutex::new(()),
                run_observer,
            }),
        })
    }

    /// 取消当前 Runtime 中全部排队或运行中的 Agent，并保留 Ready controller 供后续派生。
    ///
    /// 应用层在用户中断一次顶层交互时调用该方法，可跨插件 principal 收敛本 Runtime
    /// 已派生的后台工作。返回本次首次收到取消请求的 Agent 数量；已终止任务和未运行的
    /// controller 不计入结果，也不会被撤销。
    pub async fn cancel_active_runs(&self) -> usize {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let entries = self
            .inner
            .agents
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        entries
            .into_iter()
            .filter(|entry| matches!(entry.status(), AgentStatus::Queued | AgentStatus::Running))
            .filter(|entry| entry.request_cancel())
            .count()
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
        // 先跨过最后一个取消点，再同步完成预留与登记，避免 future 被 abort 时泄漏累计名额。
        let mut agents = self.inner.agents.write().await;
        let principal_usage = self.inner.reserve_principal_agent(&owner)?;
        let entry = AgentEntry::new(
            id.clone(),
            lineage.clone(),
            owner,
            principal_usage,
            template,
            permissions,
            AgentOptionsPatch::default(),
            AgentStatus::Ready,
        );
        agents.insert(id.clone(), entry);
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
        self.inner
            .principal_usage
            .lock()
            .expect("principal 资源账本锁不应中毒")
            .remove(principal);
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
        let mut cancelled = Vec::new();
        for entry in &entries {
            if entry.request_cancel() {
                cancelled.push(Arc::clone(entry));
            }
        }
        // principal 已进入不可逆撤销态且 Agent 已从注册表移除，此后无需继续持有生命周期锁。
        // 等待后台监督任务完成，确保 Runtime 观察器在组件卸载返回前已经收敛。
        drop(_lifecycle);
        for entry in &cancelled {
            entry.wait_until_finished().await;
        }
        cancelled.len()
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
        let permissions = parent.permissions.restrict(&request.derive.permissions);
        let id = AgentId::new();
        let lineage = AgentLineage {
            parent: Some(parent_id.clone()),
            root: parent.lineage.root.clone(),
            depth,
        };
        // 预留 principal 与父节点名额后不再 await，保证取消不能留下未登记的累计用量。
        let mut agents = self.inner.agents.write().await;
        if parent.status().is_terminal() {
            return Err(AgentRuntimeError::AgentInactive(parent_id.clone()));
        }
        let principal_usage = self.inner.reserve_principal_agent(principal)?;
        if let Err(error) = parent.reserve_child(self.inner.limits.max_children_per_agent) {
            RuntimeInner::release_principal_agent_reservation(&principal_usage);
            return Err(error);
        }
        let entry = AgentEntry::new(
            id.clone(),
            lineage.clone(),
            principal.clone(),
            principal_usage,
            parent.template.clone(),
            permissions,
            request.derive.options.clone(),
            AgentStatus::Queued,
        );
        agents.insert(id.clone(), entry.clone());
        drop(agents);

        let inner = self.inner.clone();
        let task_entry = entry.clone();
        tokio::spawn(async move {
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

        // 会话续跑是同一个 Agent 的后续运行，不是新的派生层：`continue` 沿用源节点的
        // 深度，也不消耗父节点的累计子配额。否则长期协作的队员会在若干次 dispatch
        // 之后撞到派生深度或子配额上限，表现为「跑一段时间后再也接不了新任务」。
        // 登记的 Agent 总数仍计入 principal 账本，作为内存与结果保留的硬边界。
        let depth = source.lineage.depth;
        let id = AgentId::new();
        let lineage = AgentLineage {
            parent: Some(target.clone()),
            root: source.lineage.root.clone(),
            depth,
        };
        // 预留 principal 名额后不再 await，保证取消不能留下未登记的累计用量。
        let mut agents = self.inner.agents.write().await;
        let principal_usage = self.inner.reserve_principal_agent(principal)?;
        let entry = AgentEntry::new(
            id.clone(),
            lineage.clone(),
            principal.clone(),
            principal_usage,
            source.template.clone(),
            source.permissions.clone(),
            source.run_options.clone(),
            AgentStatus::Queued,
        );
        agents.insert(id.clone(), entry.clone());
        drop(agents);

        let inner = self.inner.clone();
        let task_entry = entry.clone();
        tokio::spawn(async move {
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

    async fn steer_for(
        &self,
        principal: &RuntimePrincipal,
        caller: &AgentId,
        target: &AgentId,
        input: String,
    ) -> RuntimeResult<()> {
        self.inner
            .ensure_manageable(principal, caller, target)
            .await?;
        self.inner.entry(target).await?.steer(input)
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
            if entry.request_cancel() {
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

    async fn steer(&self, target: &AgentId, input: String) -> RuntimeResult<()> {
        self.runtime
            .steer_for(&self.principal, &self.identity, target, input)
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
    /// 每个可信 principal 在一次激活生命周期内共享的累计资源账本。
    principal_usage: Mutex<HashMap<RuntimePrincipal, Arc<Mutex<PrincipalUsage>>>>,
    revoked_principals: AsyncRwLock<HashSet<RuntimePrincipal>>,
    profiles: AsyncRwLock<HashMap<AgentProfileId, AgentProfile>>,
    profile_grants: AsyncRwLock<HashMap<RuntimePrincipal, BTreeSet<AgentProfileId>>>,
    lifecycle: AsyncMutex<()>,
    /// Host 可信运行观察器；Guest 无法读取、替换或绕过。
    run_observer: Option<Arc<dyn RuntimeRunObserver>>,
}

/// 单个 principal 在一次激活生命周期内的累计资源用量。
#[derive(Default)]
struct PrincipalUsage {
    registered_agents: usize,
    model_requests: usize,
    model_request_bytes: usize,
    model_output_tokens: usize,
}

/// Core provider 调用前执行的 principal 模型预算门禁。
///
/// 每一项都是可选上限：`None` 表示该维度不设累计配额，交互式 Serve 平面默认全部为
/// `None`，长期运行的队员不会因为会话累计用量而被拒绝。受限平面显式设置为 `Some`。
struct PrincipalModelRequestGate {
    usage: Arc<Mutex<PrincipalUsage>>,
    max_requests: Option<usize>,
    max_request_bytes: Option<usize>,
    max_output_tokens: Option<usize>,
}

/// 只统计序列化字节且在达到剩余预算时中止，避免为门禁复制完整请求缓冲。
struct BoundedRequestSizeCounter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl BoundedRequestSizeCounter {
    /// 使用当前 principal 尚可提交的字节数创建计数器。
    fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for BoundedRequestSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self.bytes.checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(IoError::other("模型请求字节计数溢出"));
        };
        if next > self.limit {
            self.exceeded = true;
            return Err(IoError::other("模型请求超过剩余字节预算"));
        }
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ModelRequestGate for PrincipalModelRequestGate {
    fn before_request(&self, request: &ModelRequest) -> anyhow::Result<()> {
        let requested_output_tokens = request
            .max_tokens
            .filter(|limit| *limit > 0)
            .ok_or(AgentRuntimeError::AgentModelRequestMaxTokensRequired)?
            as usize;
        let mut usage = self.usage.lock().expect("principal 资源账本锁不应中毒");
        if let Some(max_requests) = self.max_requests {
            if usage.model_requests >= max_requests {
                return Err(
                    AgentRuntimeError::MaxAgentModelRequestsPerPrincipalExceeded {
                        limit: max_requests,
                    }
                    .into(),
                );
            }
        }
        // 不设字节上限时跳过序列化：门禁不应为不生效的配额复制整份请求。
        let request_bytes = match self.max_request_bytes {
            Some(max_request_bytes) => {
                let remaining_request_bytes =
                    max_request_bytes.saturating_sub(usage.model_request_bytes);
                let mut request_size = BoundedRequestSizeCounter::new(remaining_request_bytes);
                if let Err(error) = serde_json::to_writer(&mut request_size, request) {
                    if request_size.exceeded {
                        return Err(
                            AgentRuntimeError::MaxAgentModelRequestBytesPerPrincipalExceeded {
                                limit: max_request_bytes,
                            }
                            .into(),
                        );
                    }
                    return Err(anyhow::anyhow!("序列化模型请求以执行资源门禁失败：{error}"));
                }
                let Some(next_bytes) = usage.model_request_bytes.checked_add(request_size.bytes)
                else {
                    return Err(
                        AgentRuntimeError::MaxAgentModelRequestBytesPerPrincipalExceeded {
                            limit: max_request_bytes,
                        }
                        .into(),
                    );
                };
                if next_bytes > max_request_bytes {
                    return Err(
                        AgentRuntimeError::MaxAgentModelRequestBytesPerPrincipalExceeded {
                            limit: max_request_bytes,
                        }
                        .into(),
                    );
                }
                Some(next_bytes)
            }
            None => None,
        };
        if let Some(max_output_tokens) = self.max_output_tokens {
            let Some(next_output_tokens) = usage
                .model_output_tokens
                .checked_add(requested_output_tokens)
            else {
                return Err(
                    AgentRuntimeError::MaxAgentModelOutputTokensPerPrincipalExceeded {
                        limit: max_output_tokens,
                    }
                    .into(),
                );
            };
            if next_output_tokens > max_output_tokens {
                return Err(
                    AgentRuntimeError::MaxAgentModelOutputTokensPerPrincipalExceeded {
                        limit: max_output_tokens,
                    }
                    .into(),
                );
            }
        }
        usage.model_requests += 1;
        if let Some(next_bytes) = request_bytes {
            usage.model_request_bytes = next_bytes;
        }
        // 预留量始终记账，结算时才能按实际用量替换；未设上限时该值只用于对账。
        usage.model_output_tokens = usage
            .model_output_tokens
            .saturating_add(requested_output_tokens);
        Ok(())
    }

    /// 退还预留与实际用量的差额，让账本记录真实消耗。
    ///
    /// `before_request` 按 `max_tokens` 预留，而实际输出通常远小于这个上限（模型声明
    /// 的上限可达数十万 token，一次回答只有一千上下）。设置输出上限时若只预留不结算，
    /// 预算会在几十次请求后被耗尽，把配额变成远小于声明值的实际请求数。本方法把预留
    /// 替换为实际用量，使输出上限只约束真实成本。
    fn settle_request(&self, reserved_output_tokens: usize, actual_output_tokens: Option<u64>) {
        let actual = actual_output_tokens.unwrap_or(0).min(usize::MAX as u64) as usize;
        let mut usage = self.usage.lock().expect("principal 资源账本锁不应中毒");
        // 预留额度一定仍记在账上（未结算前不会被他人扣减），因此这里先减后加是安全的。
        usage.model_output_tokens = usage
            .model_output_tokens
            .saturating_sub(reserved_output_tokens)
            .saturating_add(actual);
    }
}

/// Host 注册的模板和初始权限。
#[derive(Clone)]
struct AgentProfile {
    template: AgentTemplate,
    permissions: AgentPermissions,
}

impl RuntimeInner {
    /// 为 principal 原子预留一个累计 Agent 名额，并返回其共享资源账本。
    fn reserve_principal_agent(
        &self,
        principal: &RuntimePrincipal,
    ) -> RuntimeResult<Arc<Mutex<PrincipalUsage>>> {
        let usage = self
            .principal_usage
            .lock()
            .expect("principal 资源账本锁不应中毒")
            .entry(principal.clone())
            .or_insert_with(|| Arc::new(Mutex::new(PrincipalUsage::default())))
            .clone();
        let mut current = usage.lock().expect("principal 用量锁不应中毒");
        if current.registered_agents >= self.limits.max_agents_per_principal {
            return Err(AgentRuntimeError::MaxAgentsPerPrincipalExceeded {
                limit: self.limits.max_agents_per_principal,
            });
        }
        current.registered_agents += 1;
        drop(current);
        Ok(usage)
    }

    /// 回滚尚未形成 Agent 的 principal 名额预留；已登记 Agent 的终态不会调用本方法。
    fn release_principal_agent_reservation(usage: &Arc<Mutex<PrincipalUsage>>) {
        let mut current = usage.lock().expect("principal 用量锁不应中毒");
        current.registered_agents = current
            .registered_agents
            .checked_sub(1)
            .expect("仅能回滚已成功预留的 principal Agent 名额");
    }

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
    /// owner 在当前激活生命周期内共享的累计资源账本。
    principal_usage: Arc<Mutex<PrincipalUsage>>,
    template: AgentTemplate,
    permissions: AgentPermissions,
    run_options: AgentOptionsPatch,
    status: RwLock<AgentStatus>,
    outcome: RwLock<Option<AgentOutcome>>,
    session: RwLock<Option<Session>>,
    finished: Notify,
    /// Runtime 监督任务消费的一次性取消请求；不能直接 abort 监督任务，否则会跳过证据收敛。
    cancel_requested: AtomicBool,
    cancel_requested_notify: Notify,
    control: Mutex<Option<AgentControl>>,
    pending_steering: Mutex<VecDeque<String>>,
    child_count: AtomicUsize,
    /// 当前事件订阅者；运行任务通过 [`SubscriberEventSink`] 共享此列表，
    /// 终态时清空以结束所有订阅流。
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>>,
    /// 供晚加入观察者回放的有界事件历史。
    event_history: Arc<Mutex<VecDeque<AgentEvent>>>,
}

impl AgentEntry {
    /// 创建 Agent 条目。
    ///
    /// 参数一一对应条目的不可变身份与初始状态，且只由 Runtime 内部的派生路径构造；
    /// 为它们再包一层参数对象只会把同一组字段搬一次，因此保留逐个参数。
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: AgentId,
        lineage: AgentLineage,
        owner: RuntimePrincipal,
        principal_usage: Arc<Mutex<PrincipalUsage>>,
        template: AgentTemplate,
        permissions: AgentPermissions,
        run_options: AgentOptionsPatch,
        status: AgentStatus,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            lineage,
            owner,
            principal_usage,
            template,
            permissions,
            run_options,
            status: RwLock::new(status),
            outcome: RwLock::new(None),
            session: RwLock::new(None),
            finished: Notify::new(),
            cancel_requested: AtomicBool::new(false),
            cancel_requested_notify: Notify::new(),
            control: Mutex::new(None),
            pending_steering: Mutex::new(VecDeque::new()),
            child_count: AtomicUsize::new(0),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            event_history: Arc::new(Mutex::new(VecDeque::new())),
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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
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
        *self.control.lock().expect("Agent 控制句柄锁不应中毒") = None;
        self.pending_steering
            .lock()
            .expect("Agent 待处理消息锁不应中毒")
            .clear();
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

    /// 创建事件订阅流，先回放有界历史，再持续发送实时事件直到目标结束。
    fn subscribe(&self) -> AgentEventStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        let history = self.event_history.lock().expect("Agent 事件历史锁不应中毒");
        for event in history.iter().cloned() {
            if sender.send(event).is_err() {
                break;
            }
        }
        let mut subscribers = self.subscribers.lock().expect("事件订阅者锁不应中毒");
        // 在持有订阅者锁的前提下检查终态，避免与 finish 清空订阅者竞争。
        if !self.status().is_terminal() {
            subscribers.push(sender);
        }
        AgentEventStream::new(receiver)
    }

    /// 向当前运行注入 steering；尚在排队时有界暂存，启动后按顺序提交。
    fn steer(&self, input: String) -> RuntimeResult<()> {
        let status = self.status.read().expect("Agent 状态锁不应中毒");
        if status.is_terminal() {
            return Err(AgentRuntimeError::InteractionUnavailable(self.id.clone()));
        }
        let control = self.control.lock().expect("Agent 控制句柄锁不应中毒");
        if let Some(control) = control.as_ref() {
            if control.try_steer_with_limit(input, STEERING_QUEUE_LIMIT) {
                return Ok(());
            }
            return Err(AgentRuntimeError::PendingInteractionsExceeded {
                agent: self.id.clone(),
                limit: STEERING_QUEUE_LIMIT,
            });
        }
        let mut pending = self
            .pending_steering
            .lock()
            .expect("Agent 待处理消息锁不应中毒");
        if pending.len() >= STEERING_QUEUE_LIMIT {
            return Err(AgentRuntimeError::PendingInteractionsExceeded {
                agent: self.id.clone(),
                limit: STEERING_QUEUE_LIMIT,
            });
        }
        pending.push_back(input);
        Ok(())
    }

    /// 绑定本次 Core 运行的控制句柄，并提交排队阶段积累的交互消息。
    fn set_control(&self, control: AgentControl) {
        let mut slot = self.control.lock().expect("Agent 控制句柄锁不应中毒");
        for input in self
            .pending_steering
            .lock()
            .expect("Agent 待处理消息锁不应中毒")
            .drain(..)
        {
            control.steer(input);
        }
        *slot = Some(control);
    }

    /// 请求取消一次排队或运行中的任务；Ready 根节点没有监督任务，因此直接进入终态。
    fn request_cancel(&self) -> bool {
        let status = self.status();
        if status.is_terminal() {
            return false;
        }
        if status == AgentStatus::Ready {
            return self.finish(AgentOutcome::Cancelled);
        }
        if self.cancel_requested.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.cancel_requested_notify.notify_waiters();
        true
    }

    /// 等待已登记的取消请求；先登记通知再复核标志，避免丢失并发通知。
    async fn cancelled(&self) {
        loop {
            let notified = self.cancel_requested_notify.notified();
            if self.cancel_requested.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// 等待 Runtime 监督任务写入最终结果。
    async fn wait_until_finished(&self) {
        loop {
            let notified = self.finished.notified();
            if self.outcome().is_some() {
                return;
            }
            notified.await;
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
    let permit = tokio::select! {
        biased;
        _ = entry.cancelled() => {
            return AgentTaskCompletion {
                outcome: AgentOutcome::Cancelled,
                session: None,
            };
        }
        permit = inner.semaphore.clone().acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(error) => {
                return AgentTaskCompletion {
                    outcome: AgentOutcome::Failed {
                        error: format!("运行时并发控制已关闭：{error}"),
                    },
                    session: None,
                };
            }
        }
    };
    if !entry.mark_running() {
        return AgentTaskCompletion {
            outcome: entry.outcome().unwrap_or(AgentOutcome::Cancelled),
            session: None,
        };
    }
    if entry.cancel_requested.load(Ordering::Acquire) {
        return AgentTaskCompletion {
            outcome: AgentOutcome::Cancelled,
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
                    // 用 `{error:#}` 保留整条 anyhow 上下文链。`to_string()` 只打印
                    // 最外层 context，会把"为什么被拒绝"这类关键原因全部丢掉。
                    error: format!("{error:#}"),
                },
                session: None,
            };
        }
    };

    if agent
        .options()
        .execution_policy
        .limits
        .clamp_tokens(agent.options().max_tokens)
        .is_none()
    {
        agent.options_mut().max_tokens = Some(DEFAULT_DERIVED_AGENT_MAX_TOKENS);
    }
    agent.set_model_request_gate(Arc::new(PrincipalModelRequestGate {
        usage: Arc::clone(&entry.principal_usage),
        max_requests: inner.limits.max_agent_model_requests_per_principal,
        max_request_bytes: inner.limits.max_agent_model_request_bytes_per_principal,
        max_output_tokens: inner.limits.max_agent_model_output_tokens_per_principal,
    }));

    let observation = match inner.run_observer.as_ref() {
        Some(observer) => match observer
            .begin(RuntimeRunContext {
                agent_id: entry.id.clone(),
                lineage: entry.lineage.clone(),
            })
            .await
        {
            Ok(observation) => Some(observation.into_parts()),
            Err(error) => {
                return AgentTaskCompletion {
                    outcome: AgentOutcome::Failed {
                        error: format!("{error:#}"),
                    },
                    session: None,
                };
            }
        },
        None => None,
    };

    // 可信观察 sink 放在模板和订阅 sink 之前：即使展示或诊断 sink 失败，可信观察仍能
    // 看到导致 Core 退出的最后一条事件，并由 finalizer 收敛为显式失败终态。
    let mut sink = CompositeEventSink::new();
    if let Some((_, observation_sink, _)) = &observation {
        sink.push(Arc::clone(observation_sink));
    }
    sink.push(agent.event_sink());
    sink.push(Arc::new(SubscriberEventSink {
        subscribers: entry.subscribers.clone(),
        history: entry.event_history.clone(),
    }));
    agent.set_event_sink(Arc::new(sink));
    let core_control = agent.control();
    entry.set_control(core_control.clone());
    // Runtime 始终固定 Core run_id，外层监督任务才能在 future 被强制丢弃后精确清理
    // 对应的插件快照；观察器提供 ID 时继续以观察器为唯一来源。
    let run_id = observation
        .as_ref()
        .map(|(run_id, _, _)| run_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let observation_finalizer = observation
        .as_ref()
        .map(|(_, _, finalizer)| Arc::clone(finalizer));
    drop(observation);

    let extension = agent.extension();
    let core_run_id = run_id.clone();
    let core_future = AssertUnwindSafe(async move {
        let session = agent.prepare_session(session.unwrap_or_default(), input);
        agent.run_session_with_id(session, core_run_id).await
    })
    .catch_unwind();
    // 使用拥有 future 的 Pin<Box<_>>，超时分支可立即 drop Core future，再执行独立清理。
    let mut core_future = Box::pin(core_future);
    let execution = tokio::select! {
        biased;
        _ = entry.cancelled() => {
            // Runtime 取消先传入 Core，让模型、工具和审批钩子从既有检查点优雅退出；
            // 只有超过独立宽限期时才强制丢弃 future，并按可信 run_id 兜底清理。
            core_control.cancel();
            match tokio::time::timeout(CORE_CANCEL_GRACE_PERIOD, &mut core_future).await {
                Ok(Ok(Ok(_))) => RuntimeCoreCompletion::Cancelled { error: None },
                // Core 正常返回错误前已经执行 finish_run，不再重复调用任意第三方扩展。
                Ok(Ok(Err(error))) => RuntimeCoreCompletion::Cancelled {
                    error: Some(error),
                },
                Ok(Err(payload)) => {
                    let error = anyhow::anyhow!(
                        "Agent Core 取消收尾时 panic：{}",
                        panic_message(payload)
                    );
                    let cleanup = cleanup_aborted_run(&extension, &run_id).await;
                    RuntimeCoreCompletion::Cancelled {
                        error: Some(merge_cleanup_error(error, cleanup)),
                    }
                }
                Err(_) => {
                    drop(core_future);
                    RuntimeCoreCompletion::Cancelled {
                        error: cleanup_aborted_run(&extension, &run_id).await.err(),
                    }
                }
            }
        },
        core_result = &mut core_future => {
            match core_result {
                Ok(result) if entry.cancel_requested.load(Ordering::Acquire) => {
                    RuntimeCoreCompletion::Cancelled {
                        // Core 已正常返回时会自行调用 finish_run；仅保留其显式失败。
                        error: result.err(),
                    }
                }
                Ok(result) => RuntimeCoreCompletion::Finished(result),
                Err(payload) => {
                    let error = anyhow::anyhow!(
                        "Agent Core 运行 panic：{}",
                        panic_message(payload)
                    );
                    let cleanup = cleanup_aborted_run(&extension, &run_id).await;
                    RuntimeCoreCompletion::Finished(Err(merge_cleanup_error(error, cleanup)))
                }
            }
        }
    };
    drop(permit);
    let termination = match &execution {
        RuntimeCoreCompletion::Cancelled { .. } => RuntimeRunTermination::Cancelled,
        RuntimeCoreCompletion::Finished(Ok(run)) if run.cancelled => {
            RuntimeRunTermination::Cancelled
        }
        RuntimeCoreCompletion::Finished(Ok(_)) => RuntimeRunTermination::Completed,
        RuntimeCoreCompletion::Finished(Err(_)) => RuntimeRunTermination::Failed,
    };
    let observation_error = match observation_finalizer {
        Some(finalizer) => finalizer.finish(termination).await.err(),
        None => None,
    };
    match (execution, observation_error) {
        (
            RuntimeCoreCompletion::Cancelled {
                error: cleanup_error,
            },
            observation_error,
        ) => match (cleanup_error, observation_error) {
            (None, None) => AgentTaskCompletion {
                outcome: AgentOutcome::Cancelled,
                session: None,
            },
            (cleanup_error, observation_error) => AgentTaskCompletion {
                outcome: AgentOutcome::Failed {
                    error: match (cleanup_error, observation_error) {
                        (Some(cleanup_error), Some(observation_error)) => {
                            format!("{cleanup_error:#}；同时发生运行观察错误：{observation_error}")
                        }
                        (Some(cleanup_error), None) => format!("{cleanup_error:#}"),
                        (None, Some(observation_error)) => format!("{observation_error:#}"),
                        (None, None) => unreachable!("无错误的取消分支已提前处理"),
                    },
                },
                session: None,
            },
        },
        // Core 层优雅取消映射为 Runtime 的取消终态，不保留续跑会话。
        (RuntimeCoreCompletion::Finished(Ok(run)), None) if run.cancelled => AgentTaskCompletion {
            outcome: AgentOutcome::Cancelled,
            session: None,
        },
        (RuntimeCoreCompletion::Finished(Ok(run)), None) => AgentTaskCompletion {
            session: Some(run.session.clone()),
            outcome: AgentOutcome::Succeeded { result: run.into() },
        },
        (RuntimeCoreCompletion::Finished(Ok(_)), Some(error)) => AgentTaskCompletion {
            outcome: AgentOutcome::Failed {
                error: format!("{error:#}"),
            },
            session: None,
        },
        (RuntimeCoreCompletion::Finished(Err(error)), observation_error) => AgentTaskCompletion {
            outcome: AgentOutcome::Failed {
                error: match observation_error {
                    Some(observation_error) => {
                        format!("{error:#}；同时发生运行观察错误：{observation_error}")
                    }
                    None => format!("{error:#}"),
                },
            },
            session: None,
        },
    }
}

/// Runtime 监督层区分 Core 返回与外部取消，以便无论哪条路径都先收敛可信观察器。
enum RuntimeCoreCompletion {
    Finished(anyhow::Result<agent_core::AgentRun>),
    Cancelled { error: Option<anyhow::Error> },
}

/// 在 Core future 被强制丢弃或 panic 后，按可信 run_id 有界执行扩展兜底清理。
async fn cleanup_aborted_run(
    extension: &Arc<dyn AgentExtension>,
    run_id: &str,
) -> anyhow::Result<()> {
    match tokio::time::timeout(ABORTED_RUN_CLEANUP_PERIOD, extension.finish_run(run_id)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow::anyhow!(
            "强制结束运行 `{run_id}` 后清理扩展失败：{error:#}"
        )),
        Err(_) => Err(anyhow::anyhow!("强制结束运行 `{run_id}` 后扩展清理超时")),
    }
}

/// 保留 Core 主错误，并在扩展兜底清理也失败时追加安全生命周期诊断。
fn merge_cleanup_error(error: anyhow::Error, cleanup: anyhow::Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => anyhow::anyhow!("{error:#}；{cleanup_error:#}"),
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
mod tests;
