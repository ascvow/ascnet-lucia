//! 支持渐进加载与热插拔的动态插件宿主。

use crate::composite::CompositePluginHost;
use crate::host::PluginHost;
use crate::service::{PluginService, PluginServiceCall};
use crate::ui::{
    ToolRenderContext, ToolRenderRequest, ToolRendererContribution, UiDeclaration, UiFrame,
    UiInput, UiRenderRequest,
};
use agent_core::model::ModelMessage;
use agent_core::AgentEvent;
use agent_core::AgentEventKind;
use agent_core::AgentExtension;
use agent_core::ContextLoadRequest;
use agent_core::ContextLoader;
use agent_core::LoadedContext;
use agent_core::ToolDecision;
use agent_tool::ToolCall;
use agent_tool::ToolResult;
use agent_tool::ToolSpec;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
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
}

#[derive(Clone)]
struct ActivePluginRun {
    host: CompositePluginHost,
    tools: Vec<ToolSpec>,
    prompts: Vec<ModelMessage>,
    capability_plan_ready: bool,
    state: PluginRunState,
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
        Ok(())
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

    /// 按能力规划状态执行一次工具前置检查。
    async fn before_tool_with_route(
        host: CompositePluginHost,
        capability_plan_ready: bool,
        call: &ToolCall,
    ) -> Result<ToolDecision> {
        if !capability_plan_ready {
            return Ok(ToolDecision::block_with_guidance(
                "插件能力归属仍在解析，本轮暂不执行工具",
                "工具策略尚未就绪，当前调用未执行；请先继续处理无需该工具的部分，必要时稍后再尝试。",
            ));
        }
        AgentExtension::before_tool(&host, call).await
    }

    /// 按事件运行身份创建、路由或结束冻结快照。
    async fn route_event(&self, event: &AgentEvent) -> Result<()> {
        if event.run_id.is_empty() {
            return Err(anyhow!("插件事件缺少 run_id"));
        }
        if event.kind == AgentEventKind::RunStarted {
            {
                let state = self.read_state()?;
                if state.active_runs.contains_key(&event.run_id) {
                    return Err(anyhow!("插件运行快照重复创建：`{}`", event.run_id));
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
            {
                let mut state = self.write_state()?;
                if state.active_runs.contains_key(&event.run_id) {
                    return Err(anyhow!("插件运行快照重复创建：`{}`", event.run_id));
                }
                state.active_runs.insert(
                    event.run_id.clone(),
                    ActivePluginRun {
                        host: snapshot.clone(),
                        tools,
                        prompts,
                        capability_plan_ready,
                        state: PluginRunState::Active,
                    },
                );
            }
            let result = AgentExtension::on_event_for_run(&snapshot, &event.run_id, event).await;
            if result.is_err() {
                if let Some(run) = self.write_state()?.active_runs.get_mut(&event.run_id) {
                    run.state = PluginRunState::CleanupPending;
                }
                return result;
            }
            return Ok(());
        }

        let run = self.run_snapshot(&event.run_id)?;
        let result = AgentExtension::on_event_for_run(&run.host, &event.run_id, event).await;
        if event.kind == AgentEventKind::RunFinished && result.is_ok() {
            let mut state = self.write_state()?;
            let active = state
                .active_runs
                .get_mut(&event.run_id)
                .ok_or_else(|| anyhow!("插件运行快照在结束回调期间消失：`{}`", event.run_id))?;
            if active.state != PluginRunState::Active {
                return Err(anyhow!(
                    "插件运行快照在结束回调期间发生并发终态变更：`{}`",
                    event.run_id
                ));
            }
            active.state = PluginRunState::FinishEventDelivered;
        }
        result
    }
}

#[async_trait]
impl AgentExtension for LivePluginHost {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        if let Some(run) = self.legacy_run_snapshot()? {
            return Ok(run.prompts);
        }
        AgentExtension::prompt_messages(&self.current_snapshot()?).await
    }

    async fn prompt_messages_for_run(&self, run_id: &str) -> Result<Vec<ModelMessage>> {
        Ok(self.run_snapshot(run_id)?.prompts)
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        if let Some(run) = self.legacy_run_snapshot()? {
            return Ok(run.tools);
        }
        AgentExtension::list_tools(&self.current_snapshot()?).await
    }

    async fn list_tools_for_run(&self, run_id: &str) -> Result<Vec<ToolSpec>> {
        Ok(self.run_snapshot(run_id)?.tools)
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

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        if let Some(run_id) = call.run_id.as_deref() {
            return self.before_tool_for_run(run_id, call).await;
        }
        let (host, plan_ready) = self.legacy_route()?;
        Self::before_tool_with_route(host, plan_ready, call).await
    }

    async fn before_tool_for_run(&self, run_id: &str, call: &ToolCall) -> Result<ToolDecision> {
        Self::validate_call_run_id(call, run_id)?;
        let run = self.run_snapshot(run_id)?;
        if !run.capability_plan_ready {
            return Self::before_tool_with_route(run.host, false, call).await;
        }
        AgentExtension::before_tool_for_run(&run.host, run_id, call).await
    }

    async fn after_tool(&self, result: &ToolResult) -> Result<()> {
        AgentExtension::after_tool(&self.legacy_route()?.0, result).await
    }

    async fn after_tool_for_run(&self, run_id: &str, result: &ToolResult) -> Result<()> {
        let host = self.run_snapshot(run_id)?.host;
        AgentExtension::after_tool_for_run(&host, run_id, result).await
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        self.route_event(event).await
    }

    async fn on_event_for_run(&self, run_id: &str, event: &AgentEvent) -> Result<()> {
        if event.run_id != run_id {
            return Err(anyhow!(
                "插件事件 run_id 与可信运行不一致：期望 `{run_id}`，实际 `{}`",
                event.run_id
            ));
        }
        self.route_event(event).await
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
