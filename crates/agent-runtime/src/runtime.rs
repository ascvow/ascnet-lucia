//! Agent 队列调度、生命周期、续跑和 principal 资源管理。

use crate::protocol::SubscriberEventSink;
use crate::{
    AgentDeriveConfig, AgentEventStream, AgentHandle, AgentId, AgentLineage, AgentOptionsPatch,
    AgentOutcome, AgentPermissions, AgentProfileId, AgentRuntimeApi, AgentRuntimeError,
    AgentRuntimeProvisioner, AgentSnapshot, AgentSpawnRequest, AgentStatus, AgentTemplate,
    ProvisionedAgentRuntime, RuntimeLimits, RuntimePrincipal, RuntimeResult,
};
use agent_core::{AgentControl, AgentEvent, CompositeEventSink, Session};
use async_trait::async_trait;
use futures_util::FutureExt;
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
};
use tokio::{
    sync::{mpsc, Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock, Semaphore},
    task::AbortHandle,
};

/// Agent 尚未开始运行时允许暂存的交互消息数量。
const PENDING_STEERING_LIMIT: usize = 32;

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
            control.steer(input);
            return Ok(());
        }
        let mut pending = self
            .pending_steering
            .lock()
            .expect("Agent 待处理消息锁不应中毒");
        if pending.len() >= PENDING_STEERING_LIMIT {
            return Err(AgentRuntimeError::PendingInteractionsExceeded {
                agent: self.id.clone(),
                limit: PENDING_STEERING_LIMIT,
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
        history: entry.event_history.clone(),
    }));
    agent.set_event_sink(Arc::new(sink));
    entry.set_control(agent.control());

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
mod tests;
