//! 支持渐进加载与热插拔的动态插件宿主。

use crate::composite::CompositePluginHost;
use crate::host::PluginHost;
use crate::manifest::TOOL_POLICY_CAPABILITY;
use crate::service::{PluginService, PluginServiceCall};
use crate::ui::{
    ToolRenderContext, ToolRenderRequest, ToolRendererContribution, UiDeclaration, UiFrame,
    UiInput, UiRenderRequest,
};
use agent_core::model::ModelMessage;
use agent_core::AgentEventKind;
use agent_core::AgentExtension;
use agent_core::AgentRecordEvent;
use agent_core::ContextLoadRequest;
use agent_core::ContextLoader;
use agent_core::Event;
use agent_core::EventBus;
use agent_core::EventContext;
use agent_core::EventListener;
use agent_core::ListenerFailure;
use agent_core::ListenerPriority;
use agent_core::LoadedContext;
use agent_core::Subscribe;
use agent_core::SubscriptionId;
use agent_core::ToolCallEvent;
use agent_tool::ToolCall;
use agent_tool::ToolResult;
use agent_tool::ToolSpec;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

/// Live Host 对瞬态运行清理故障执行的最大尝试次数。
const MAX_RUN_CLEANUP_ATTEMPTS: usize = 2;
/// 单次底层清理的最长等待时间；两次尝试总预算必须小于 Runtime 的兜底清理预算。
const RUN_CLEANUP_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(750);
/// 可在进程运行期间发布新插件、并为每次 Agent 运行冻结一致能力视图的宿主。
///
/// 后台加载完成的插件立即进入最新宿主集合；已开始的 Agent 运行继续使用
/// `RunStarted` 时冻结的工具、提示、路由和独占能力 owner，新插件从下一轮生效。
#[derive(Clone, Default)]
pub struct LivePluginHost {
    state: Arc<RwLock<LivePluginState>>,
}

#[derive(Default)]
struct LivePluginState {
    current: CompositePluginHost,
    active_runs: HashMap<String, ActivePluginRun>,
    capability_plan_ready: bool,
    plugin_order: HashMap<String, usize>,
    /// 已挂载的宿主事件总线；未挂载时插件不参与任何事件分发。
    bus: Option<EventBus>,
    /// 当前已在总线上登记的 (事件名, 优先级) 分组及其句柄。
    registrations: HashMap<(String, ListenerPriority), SubscriptionId>,
}

#[derive(Clone)]
struct ActivePluginRun {
    host: CompositePluginHost,
    /// 冻结贡献只在实际请求工具/提示时复制，逐 token 的事件扇出共享同一份内容。
    tools: Arc<Vec<ToolSpec>>,
    prompts: Arc<Vec<ModelMessage>>,
    capability_plan_ready: bool,
    state: PluginRunState,
    /// 本次运行的 `RunFinished` 是否有插件未成功接收。
    ///
    /// 只要有一个插件没收到结束通知，就不能走"已送达"的轻量排空路径，必须让
    /// `finish_run` 调用底层异常清理，确保 Guest 有机会释放该运行的瞬时状态。
    finish_delivery_failed: bool,
}

/// Live Host 中单次运行的终态推进阶段。
///
/// `RunFinished` Guest 回调与最后一次扩展事件排空是两个独立提交点；只有两者都成功后
/// 才能直接释放快照。清理 future 被取消时回到 `CleanupPending` 供同一可信 `run_id`
/// 重试，且不再开放工具、提示或上下文路由；有界重试持续失败后释放不可达快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginRunState {
    /// 运行仍可使用冻结插件能力。
    Active,
    /// Guest 已成功接收 `RunFinished`，等待 Core 排空该回调产生的最后事件。
    FinishEventDelivered,
    /// `RunFinished` 回调和最后事件排空均成功，可直接释放快照。
    Completed,
    /// 异常清理曾失败，快照仅保留给后续清理重试。
    CleanupPending,
    /// 一个清理调用正在执行，拒绝并发重复清理。
    Cleaning,
}

/// 清理阶段被取消时把运行恢复到可重试状态，避免永久停留在 `Cleaning`。
struct PluginRunCleanupLease {
    state: Arc<RwLock<LivePluginState>>,
    run_id: String,
    rollback_state: PluginRunState,
    armed: bool,
}

impl PluginRunCleanupLease {
    /// 创建清理租约；正常收敛后必须显式解除。
    fn new(
        state: Arc<RwLock<LivePluginState>>,
        run_id: &str,
        rollback_state: PluginRunState,
    ) -> Self {
        Self {
            state,
            run_id: run_id.to_string(),
            rollback_state,
            armed: true,
        }
    }

    /// 标记运行状态已经由成功或最终失败路径收敛。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PluginRunCleanupLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.state.write() {
            if let Some(run) = state.active_runs.get_mut(&self.run_id) {
                if run.state == PluginRunState::Cleaning {
                    run.state = self.rollback_state;
                }
            }
        }
    }
}

/// 单次 `finish_run` 所需执行的底层收尾动作。
enum PluginRunCleanupAction {
    /// `RunFinished` 尚未完整送达，需要调用底层异常清理。
    Abort(CompositePluginHost),
    /// `RunFinished` 已送达，只需补做最终事件排空，禁止重复通知 Guest。
    Drain(CompositePluginHost),
}

impl LivePluginHost {
    /// 创建尚未发布任何 Ready 插件的动态宿主。
    pub fn new() -> Self {
        Self::default()
    }

    /// 发布一个已完成实例化和激活的插件；同 ID 插件会被拒绝。
    pub fn publish(&self, host: Arc<dyn PluginHost>) -> Result<()> {
        let mut state = self.write_state()?;
        if let Some(id) = host.id() {
            if state.current.get(id).is_some() {
                return Err(anyhow!("插件 ID 重复发布：`{id}`"));
            }
        }
        let rank = host
            .id()
            .and_then(|id| state.plugin_order.get(id))
            .copied()
            .unwrap_or(usize::MAX);
        let index = state
            .current
            .hosts()
            .iter()
            .position(|current| {
                current
                    .id()
                    .and_then(|id| state.plugin_order.get(id))
                    .copied()
                    .unwrap_or(usize::MAX)
                    > rank
            })
            .unwrap_or_else(|| state.current.len());
        state.current.insert(index, host);
        drop(state);
        // 新插件的订阅必须在它可能收到事件之前登记完成。
        self.sync_subscriptions()
    }

    /// 在发布前记录插件的稳定计划顺序，避免并发完成顺序改变工具与提示顺序。
    pub(crate) fn set_plugin_order(&self, plugin_ids: &[String]) -> Result<()> {
        let mut state = self.write_state()?;
        if !state.current.is_empty() {
            return Err(anyhow!("插件发布后不能修改稳定加载顺序"));
        }
        state.plugin_order = plugin_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect();
        Ok(())
    }

    /// 预先记录独占能力 owner；owner 未 Ready 时按能力类型执行安全降级。
    pub fn set_capability_owner(
        &self,
        capability_id: impl Into<String>,
        plugin_id: impl Into<String>,
    ) -> Result<()> {
        self.write_state()?
            .current
            .set_capability_owner(capability_id, plugin_id);
        Ok(())
    }

    /// 标记 manifest 能力归属已经完成解析，后续运行可按解析结果执行工具策略。
    pub fn finish_capability_planning(&self) -> Result<()> {
        self.write_state()?.capability_plan_ready = true;
        Ok(())
    }

    /// 查询规划后的通用独占能力 owner；用于装配层显示，不作为运行授权决策。
    ///
    /// 未完成规划时返回错误；完成但未配置 owner 时返回 None。实际调用仍使用运行冻结快照。
    pub fn planned_capability_owner(&self, capability_id: &str) -> Result<Option<String>> {
        let state = self.read_state()?;
        if !state.capability_plan_ready {
            return Err(anyhow!("插件能力归属仍在解析"));
        }
        Ok(state
            .current
            .capability_owner(capability_id)
            .map(str::to_string))
    }

    /// 返回指定插件是否已经 Ready。
    pub fn is_ready(&self, plugin_id: &str) -> Result<bool> {
        Ok(self.read_state()?.current.get(plugin_id).is_some())
    }

    /// 使用最新 Ready 插件快照渲染工具消息，TUI 无需感知 owner 或 renderer ID。
    pub async fn render_tool_message(
        &self,
        context: &ToolRenderContext,
    ) -> Result<Option<UiFrame>> {
        self.current_snapshot()?.render_tool_message(context).await
    }

    fn read_state(&self) -> Result<std::sync::RwLockReadGuard<'_, LivePluginState>> {
        self.state
            .read()
            .map_err(|_| anyhow!("动态插件宿主读锁已中毒"))
    }

    fn write_state(&self) -> Result<std::sync::RwLockWriteGuard<'_, LivePluginState>> {
        self.state
            .write()
            .map_err(|_| anyhow!("动态插件宿主写锁已中毒"))
    }

    fn current_snapshot(&self) -> Result<CompositePluginHost> {
        Ok(self.read_state()?.current.clone())
    }

    /// 返回指定运行的冻结快照；未知或已结束运行不得退回最新插件集合。
    fn run_snapshot(&self, run_id: &str) -> Result<ActivePluginRun> {
        let state = self.read_state()?;
        let run = state
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow!("插件运行快照不存在：`{run_id}`"))?;
        if run.state != PluginRunState::Active {
            return Err(anyhow!("插件运行快照正在结束或已经结束：`{run_id}`"));
        }
        Ok(run.clone())
    }

    /// 返回事件排空阶段可用的快照；`RunFinished` 后由 Core 排空事件再统一释放。
    fn run_snapshot_for_drain(&self, run_id: &str) -> Result<ActivePluginRun> {
        let run = self
            .read_state()?
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow!("插件运行快照不存在：`{run_id}`"))?;
        if !matches!(
            run.state,
            PluginRunState::Active | PluginRunState::FinishEventDelivered
        ) {
            return Err(anyhow!("插件运行快照不再接受事件排空：`{run_id}`"));
        }
        Ok(run)
    }

    /// 为旧版无运行参数接口选择唯一活动快照；并发时拒绝猜测运行身份。
    fn legacy_run_snapshot(&self) -> Result<Option<ActivePluginRun>> {
        let state = self.read_state()?;
        let mut runs = state
            .active_runs
            .values()
            .filter(|run| run.state == PluginRunState::Active);
        let first = runs.next().cloned();
        if runs.next().is_some() {
            return Err(anyhow!(
                "存在多个并发 Agent 运行，必须使用带 run_id 的插件扩展接口"
            ));
        }
        Ok(first)
    }

    /// 为旧版无运行参数接口选择唯一运行或最新 Ready 集合。
    fn legacy_route(&self) -> Result<(CompositePluginHost, bool)> {
        if let Some(run) = self.legacy_run_snapshot()? {
            return Ok((run.host, run.capability_plan_ready));
        }
        let state = self.read_state()?;
        Ok((state.current.clone(), state.capability_plan_ready))
    }

    /// 校验 Core 绑定在工具调用上的运行身份没有被包装层改写或清除。
    fn validate_call_run_id(call: &ToolCall, run_id: &str) -> Result<()> {
        match call.run_id.as_deref() {
            Some(bound) if bound == run_id => Ok(()),
            Some(bound) => Err(anyhow!(
                "工具调用 run_id 与可信运行不一致：期望 `{run_id}`，实际 `{bound}`"
            )),
            None => Err(anyhow!("工具调用缺少可信 run_id：`{run_id}`")),
        }
    }

    /// 挂载宿主事件总线，并按当前插件集合完成一次订阅同步。
    ///
    /// 应用在装配阶段把同一条总线交给 Core、界面和本宿主；未挂载总线时插件不会收到
    /// 任何事件，也无法参与取消决策。
    pub fn attach_bus(&self, bus: EventBus) -> Result<()> {
        self.write_state()?.bus = Some(bus);
        self.sync_subscriptions()
    }

    /// 按当前已发布插件的 manifest 订阅重建总线登记。
    ///
    /// 每个 (事件名, 优先级) 分组只登记一个监听者，由 [`PluginFanout`] 在分发时按运行
    /// 快照扇出给该分组内的插件。这样跨插件与跨宿主的执行顺序由总线统一裁决，而每次
    /// 运行看到的插件集合仍是 `RunStarted` 时冻结的那一份。
    ///
    /// 插件发布或卸载后必须调用；重复调用是幂等的。
    pub fn sync_subscriptions(&self) -> Result<()> {
        let (bus, desired) = {
            let state = self.read_state()?;
            let Some(bus) = state.bus.clone() else {
                return Ok(());
            };
            let policy_owner = state
                .current
                .capability_owner(TOOL_POLICY_CAPABILITY)
                .map(str::to_string);
            let mut desired = BTreeSet::new();
            for host in state.current.hosts() {
                let is_policy_owner = host
                    .id()
                    .is_some_and(|id| Some(id) == policy_owner.as_deref());
                for subscription in host.event_subscriptions() {
                    desired.insert((
                        subscription.name.clone(),
                        effective_priority(
                            &subscription.name,
                            subscription.priority,
                            is_policy_owner,
                        ),
                    ));
                }
            }
            // 运行快照生命周期由 core.agent.event 驱动，必须始终订阅且最先执行。
            desired.insert((AgentRecordEvent::NAME.to_string(), ListenerPriority::Lowest));
            // 运行结束的状态推进必须排在全部插件分组之后，因此固定登记只读的收尾分组。
            desired.insert((
                AgentRecordEvent::NAME.to_string(),
                ListenerPriority::Monitor,
            ));
            // 工具准入守卫无条件登记且最先执行：策略 owner 尚未 Ready 或能力归属仍在
            // 解析时必须拒绝工具，不能因为"暂时没有插件订阅"而形成放行空窗。
            desired.insert((ToolCallEvent::NAME.to_string(), ListenerPriority::Lowest));
            (bus, desired)
        };

        let mut state = self.write_state()?;
        state.registrations.retain(|key, id| {
            let keep = desired.contains(key);
            if !keep {
                bus.unsubscribe(*id);
            }
            keep
        });
        for (event, priority) in desired {
            if state.registrations.contains_key(&(event.clone(), priority)) {
                continue;
            }
            // 工具准入是安全决策点：扇出失败必须等同拒绝，其余事件只记录错误。
            let failure = if event == ToolCallEvent::NAME {
                ListenerFailure::CancelEvent
            } else {
                ListenerFailure::Ignore
            };
            let id = bus.subscribe(
                Subscribe::new(event.clone(), PLUGIN_HOST_SUBSCRIBER)
                    .with_priority(priority)
                    .on_failure(failure),
                Arc::new(PluginFanout {
                    live: self.clone(),
                    priority,
                }),
            );
            state.registrations.insert((event, priority), id);
        }
        Ok(())
    }

    /// 创建一次运行的冻结插件快照。
    ///
    /// 快照包含该运行可用的插件集合、工具与提示；后续加载的插件不会进入已开始的运行。
    async fn open_run_snapshot(&self, run_id: &str) -> Result<()> {
        {
            let state = self.read_state()?;
            if state.active_runs.contains_key(run_id) {
                return Err(anyhow!("插件运行快照重复创建：`{run_id}`"));
            }
        }
        let (snapshot, capability_plan_ready) = {
            let state = self.read_state()?;
            (
                state.current.detached_snapshot(),
                state.capability_plan_ready,
            )
        };
        let tools = AgentExtension::list_tools(&snapshot).await?;
        let prompts = AgentExtension::prompt_messages(&snapshot).await?;

        let mut state = self.write_state()?;
        if state.active_runs.contains_key(run_id) {
            return Err(anyhow!("插件运行快照重复创建：`{run_id}`"));
        }
        state.active_runs.insert(
            run_id.to_string(),
            ActivePluginRun {
                host: snapshot,
                tools: Arc::new(tools),
                prompts: Arc::new(prompts),
                capability_plan_ready,
                state: PluginRunState::Active,
                finish_delivery_failed: false,
            },
        );
        Ok(())
    }

    /// 标记该运行的 `RunFinished` 已经送达全部插件，等待 Core 排空最后一批事件。
    ///
    /// 存在未送达的插件时保持 `Active`，由 `finish_run` 走异常清理路径补偿。
    fn mark_finish_delivered(&self, run_id: &str) -> Result<()> {
        let mut state = self.write_state()?;
        let Some(active) = state.active_runs.get_mut(run_id) else {
            return Ok(());
        };
        if active.state == PluginRunState::Active && !active.finish_delivery_failed {
            active.state = PluginRunState::FinishEventDelivered;
        }
        Ok(())
    }

    /// 记录本次运行的结束通知存在未送达插件。
    fn mark_finish_delivery_failed(&self, run_id: &str) -> Result<()> {
        if let Some(active) = self.write_state()?.active_runs.get_mut(run_id) {
            active.finish_delivery_failed = true;
        }
        Ok(())
    }

    /// 返回允许参与事件扇出的运行快照。
    ///
    /// 记录事件在 `RunFinished` 送达后仍需继续投递给尚未处理的分组，因此这里接受
    /// `FinishEventDelivered`；工具准入等决策事件仍走严格的 [`Self::run_snapshot`]。
    fn run_snapshot_for_fanout(&self, run_id: &str, is_record: bool) -> Result<ActivePluginRun> {
        if !is_record {
            return self.run_snapshot(run_id);
        }
        self.run_snapshot_for_drain(run_id)
    }
}

/// Live Host 在总线上使用的固定订阅者身份。
const PLUGIN_HOST_SUBSCRIBER: &str = "lucia.plugin-host";

/// 计算插件订阅在总线上的实际优先级。
///
/// 独占的工具策略 owner 必须在全部普通插件之后做最终裁决，因此它对
/// `core.tool.call` 的订阅固定提升为 `Highest`，其余插件在该事件上最高只能到 `High`。
/// 这保留了旧组合宿主"策略插件最后执行"的不变量，同时把排序交还给总线。
fn effective_priority(
    event: &str,
    declared: ListenerPriority,
    is_policy_owner: bool,
) -> ListenerPriority {
    if event != ToolCallEvent::NAME {
        return declared;
    }
    if is_policy_owner {
        ListenerPriority::Highest
    } else {
        declared.min(ListenerPriority::High)
    }
}

/// 判断当前运行是否处于必须拒绝全部工具的策略空窗，并返回可信侧的详细原因。
///
/// 两种空窗都会让"谁是最终裁决者"无法确定，此时放行等于绕过工具策略：
/// 能力归属尚未解析完成，或已声明的独占策略 owner 还没有进入本次运行的冻结快照。
fn tool_admission_block_reason(
    run: &ActivePluginRun,
    policy_owner: Option<&str>,
) -> Option<String> {
    if !run.capability_plan_ready {
        return Some("插件能力归属仍在解析，本轮暂不执行工具".to_string());
    }
    let owner = policy_owner?;
    if run.host.get(owner).is_some() {
        return None;
    }
    Some(format!("工具策略插件 `{owner}` 仍在加载，本轮暂不执行工具"))
}

/// 把一次总线分发扇出给运行快照中订阅了同一优先级分组的插件。
struct PluginFanout {
    live: LivePluginHost,
    priority: ListenerPriority,
}

impl PluginFanout {
    /// 读取事件快照中 Core 绑定的运行 ID。
    fn run_id(ctx: &EventContext<'_>) -> Result<String> {
        // 高频增量记录已有强类型身份，不为取一个 ID 序列化整份事件正文。
        if let Some(event) = ctx.event::<AgentRecordEvent>() {
            if !event.record.run_id.is_empty() {
                return Ok(event.record.run_id.clone());
            }
        }
        let data = ctx.snapshot()?;
        data.get("run_id")
            .and_then(|value| value.as_str())
            .or_else(|| {
                data.pointer("/record/run_id")
                    .and_then(|value| value.as_str())
            })
            .filter(|run_id| !run_id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("插件事件 `{}` 缺少可信 run_id", ctx.name()))
    }

    /// 在插件改写工具调用后重新绑定可信 scope 与运行身份。
    ///
    /// 后续插件和 Core 的最终门禁必须看到改写目标本身的权限 scope；调用 ID 与运行 ID
    /// 始终继承 Core 的可信输入，插件无法借改写把结果关联到其他调用。
    fn rebind_tool_call(
        host: &CompositePluginHost,
        ctx: &mut EventContext<'_>,
        trusted: &ToolCall,
    ) -> Result<()> {
        let Some(event) = ctx.event_mut::<ToolCallEvent>() else {
            return Ok(());
        };
        event.call = host.bind_tool_permission_scope(Some(trusted), event.call.clone())?;
        Ok(())
    }
}

#[async_trait]
impl EventListener for PluginFanout {
    async fn handle(&self, ctx: &mut EventContext<'_>) -> Result<()> {
        let run_id = Self::run_id(ctx)?;
        let is_record = ctx.name() == AgentRecordEvent::NAME;
        let record_kind = ctx
            .event::<AgentRecordEvent>()
            .map(|event| event.record.kind.clone());

        // 运行快照生命周期只由记录事件驱动，且必须先于任何插件投递完成创建。
        if is_record && record_kind == Some(AgentEventKind::RunStarted) {
            self.live.open_run_snapshot(&run_id).await?;
        }

        let run = self.live.run_snapshot_for_fanout(&run_id, is_record)?;
        let policy_owner = run
            .host
            .capability_owner(TOOL_POLICY_CAPABILITY)
            .map(str::to_string);

        // 准入守卫只在最先执行的分组里跑一次，保证任何插件看到调用之前就已否决。
        if ctx.name() == ToolCallEvent::NAME && self.priority == ListenerPriority::Lowest {
            if let Some(reason) = tool_admission_block_reason(&run, policy_owner.as_deref()) {
                if let Some(event) = ctx.event_mut::<ToolCallEvent>() {
                    event.block_reason = Some(reason);
                    // 一次性指导不暴露具体策略插件 ID，避免模型据此推断绕过路径。
                    event.agent_guidance = Some(
                        "工具策略尚未就绪，当前调用未执行；请先继续处理无需该工具的部分，必要时稍后再尝试。"
                            .into(),
                    );
                }
                ctx.cancel();
                return Ok(());
            }
            // 守卫通过后立刻用运行快照中的可信 ToolSpec 覆盖调用 scope，
            // 保证第一个插件看到的就是宿主认定的权限范围。
            if let Some(event) = ctx.event_mut::<ToolCallEvent>() {
                let trusted = event.call.clone();
                event.call = run
                    .host
                    .bind_tool_permission_scope(Some(&trusted), trusted.clone())?;
            }
        }
        let mut errors = Vec::new();
        for host in run.host.hosts() {
            let is_policy_owner = host
                .id()
                .is_some_and(|id| Some(id) == policy_owner.as_deref());
            let subscribed = host.event_subscriptions().iter().any(|subscription| {
                subscription.name == ctx.name()
                    && effective_priority(
                        &subscription.name,
                        subscription.priority,
                        is_policy_owner,
                    ) == self.priority
                    && !(subscription.ignore_cancelled && ctx.is_cancelled())
            });
            if !subscribed {
                continue;
            }
            let trusted_call = ctx.event::<ToolCallEvent>().map(|event| event.call.clone());
            if let Err(error) = host.dispatch_event(ctx).await {
                errors.push(format!(
                    "插件 `{}` 处理事件 `{}` 失败：{error:#}",
                    host.id().unwrap_or("<匿名>"),
                    ctx.name()
                ));
                continue;
            }
            if let Some(trusted_call) = trusted_call {
                Self::rebind_tool_call(&run.host, ctx, &trusted_call)?;
            }
        }

        if is_record && record_kind == Some(AgentEventKind::RunFinished) {
            if errors.is_empty() {
                // 只读收尾分组在全部插件分组之后执行，此时才允许推进终态。
                if self.priority == ListenerPriority::Monitor {
                    self.live.mark_finish_delivered(&run_id)?;
                }
            } else {
                self.live.mark_finish_delivery_failed(&run_id)?;
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("；")))
        }
    }
}

#[async_trait]
impl AgentExtension for LivePluginHost {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        if let Some(run) = self.legacy_run_snapshot()? {
            return Ok(run.prompts.as_ref().clone());
        }
        AgentExtension::prompt_messages(&self.current_snapshot()?).await
    }

    async fn prompt_messages_for_run(&self, run_id: &str) -> Result<Vec<ModelMessage>> {
        Ok(self.run_snapshot(run_id)?.prompts.as_ref().clone())
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        if let Some(run) = self.legacy_run_snapshot()? {
            return Ok(run.tools.as_ref().clone());
        }
        AgentExtension::list_tools(&self.current_snapshot()?).await
    }

    async fn list_tools_for_run(&self, run_id: &str) -> Result<Vec<ToolSpec>> {
        Ok(self.run_snapshot(run_id)?.tools.as_ref().clone())
    }

    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        if let Some(run_id) = call.run_id.clone() {
            return self.call_tool_for_run(&run_id, call).await;
        }
        AgentExtension::call_tool(&self.legacy_route()?.0, call).await
    }

    async fn call_tool_for_run(&self, run_id: &str, call: ToolCall) -> Result<Option<ToolResult>> {
        Self::validate_call_run_id(&call, run_id)?;
        let host = self.run_snapshot(run_id)?.host;
        AgentExtension::call_tool_for_run(&host, run_id, call).await
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        // UI 与启动事件不属于 Agent run，只从最新 Ready 集合排空未绑定运行的队列。
        AgentExtension::drain_events(&self.current_snapshot()?).await
    }

    async fn drain_events_for_run(&self, run_id: &str) -> Result<Vec<serde_json::Value>> {
        let run = self.run_snapshot_for_drain(run_id)?;
        let events = AgentExtension::drain_events_for_run(&run.host, run_id).await?;
        if run.state == PluginRunState::FinishEventDelivered {
            let mut state = self.write_state()?;
            let active = state
                .active_runs
                .get_mut(run_id)
                .ok_or_else(|| anyhow!("插件运行快照在最终事件排空期间消失：`{run_id}`"))?;
            if active.state != PluginRunState::FinishEventDelivered {
                return Err(anyhow!(
                    "插件运行快照在最终事件排空期间发生并发终态变更：`{run_id}`"
                ));
            }
            active.state = PluginRunState::Completed;
        }
        Ok(events)
    }

    async fn finish_run(&self, run_id: &str) -> Result<()> {
        // 正常终态只有在 Guest 回调和最后事件排空均成功后才能直接释放；其他路径通过
        // 底层异常清理收敛。Live Host 自身吸收一次瞬态失败，最终失败也释放路由快照，
        // 避免 Core 单次清理返回后永久保留不可达运行。
        let (action, rollback_state) = {
            let mut state = self.write_state()?;
            let Some(run) = state.active_runs.get_mut(run_id) else {
                return Ok(());
            };
            match run.state {
                PluginRunState::Completed => {
                    state.active_runs.remove(run_id);
                    return Ok(());
                }
                PluginRunState::Cleaning => {
                    return Err(anyhow!("插件运行快照正在清理：`{run_id}`"));
                }
                PluginRunState::FinishEventDelivered => {
                    let host = run.host.clone();
                    run.state = PluginRunState::Cleaning;
                    (
                        PluginRunCleanupAction::Drain(host),
                        PluginRunState::FinishEventDelivered,
                    )
                }
                PluginRunState::Active | PluginRunState::CleanupPending => {
                    let host = run.host.clone();
                    run.state = PluginRunState::Cleaning;
                    (
                        PluginRunCleanupAction::Abort(host),
                        PluginRunState::CleanupPending,
                    )
                }
            }
        };

        let mut lease = PluginRunCleanupLease::new(Arc::clone(&self.state), run_id, rollback_state);
        let mut last_error = None;
        for _ in 0..MAX_RUN_CLEANUP_ATTEMPTS {
            let result = tokio::time::timeout(RUN_CLEANUP_ATTEMPT_TIMEOUT, async {
                match &action {
                    PluginRunCleanupAction::Abort(host) => {
                        AgentExtension::finish_run(host, run_id).await
                    }
                    PluginRunCleanupAction::Drain(host) => {
                        AgentExtension::drain_events_for_run(host, run_id)
                            .await
                            .map(|_| ())
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                Err(anyhow!(
                    "插件运行清理单次尝试超过 {}ms",
                    RUN_CLEANUP_ATTEMPT_TIMEOUT.as_millis()
                ))
            });
            match result {
                Ok(()) => {
                    self.write_state()?.active_runs.remove(run_id);
                    lease.disarm();
                    return Ok(());
                }
                Err(error) => last_error = Some(error),
            }
        }

        self.write_state()?.active_runs.remove(run_id);
        lease.disarm();
        Err(last_error
            .expect("至少执行一次插件运行清理")
            .context(format!(
                "插件运行清理在 {MAX_RUN_CLEANUP_ATTEMPTS} 次有界尝试后仍失败"
            )))
    }
}

#[async_trait]
impl PluginHost for LivePluginHost {
    fn agent_runtime_api(
        &self,
        plugin_id: &str,
    ) -> Option<Arc<dyn agent_runtime::AgentRuntimeApi>> {
        self.current_snapshot().ok()?.agent_runtime_api(plugin_id)
    }

    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        let host = self.run_snapshot(&request.run_id)?.host;
        PluginHost::load_context(&host, request).await
    }

    async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
        PluginHost::ui_declarations(&self.current_snapshot()?).await
    }

    async fn tool_renderers(&self) -> Result<Vec<ToolRendererContribution>> {
        PluginHost::tool_renderers(&self.current_snapshot()?).await
    }

    async fn render_ui(&self, request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        PluginHost::render_ui(&self.current_snapshot()?, request).await
    }

    async fn render_tool(&self, request: &ToolRenderRequest) -> Result<Option<UiFrame>> {
        PluginHost::render_tool(&self.current_snapshot()?, request).await
    }

    async fn on_ui_input(&self, input: &UiInput) -> Result<()> {
        PluginHost::on_ui_input(&self.current_snapshot()?, input).await
    }

    async fn services(&self) -> Result<Vec<PluginService>> {
        PluginHost::services(&self.current_snapshot()?).await
    }

    async fn call_service(&self, call: &PluginServiceCall) -> Result<Option<serde_json::Value>> {
        PluginHost::call_service(&self.current_snapshot()?, call).await
    }

    async fn shutdown(&self) -> Result<()> {
        PluginHost::shutdown(&self.current_snapshot()?).await
    }
}

#[async_trait]
impl ContextLoader for LivePluginHost {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        match PluginHost::load_context(self, &request).await? {
            Some(context) => Ok(context),
            None => Ok(LoadedContext::passthrough(request)),
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// 未规划、未配置和 owner 尚未 Ready 必须区分，不以插件 ID 猜审批状态。
    #[test]
    fn planned_owner_distinguishes_loading_and_unconfigured() {
        let host = LivePluginHost::new();
        assert!(host.planned_capability_owner("agent.tool-policy").is_err());
        host.finish_capability_planning().unwrap();
        assert_eq!(
            host.planned_capability_owner("agent.tool-policy").unwrap(),
            None
        );
        host.set_capability_owner("agent.tool-policy", "custom-policy")
            .unwrap();
        assert_eq!(
            host.planned_capability_owner("agent.tool-policy")
                .unwrap()
                .as_deref(),
            Some("custom-policy")
        );
        assert!(!host.is_ready("custom-policy").unwrap());
    }

    /// 扇出快照的克隆必须共享冻结正文而不是按 token 深克隆。
    #[tokio::test]
    async fn fanout_snapshot_shares_frozen_contributions() {
        let host = LivePluginHost::new();
        host.open_run_snapshot("snapshot").await.unwrap();
        let first = host.run_snapshot("snapshot").unwrap();
        let second = host.run_snapshot_for_fanout("snapshot", true).unwrap();
        assert!(Arc::ptr_eq(&first.tools, &second.tools));
        assert!(Arc::ptr_eq(&first.prompts, &second.prompts));
    }
}
