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
    active_run: Option<ActivePluginRun>,
    capability_plan_ready: bool,
    plugin_order: HashMap<String, usize>,
}

#[derive(Clone)]
struct ActivePluginRun {
    run_id: String,
    host: CompositePluginHost,
    tools: Vec<ToolSpec>,
    prompts: Vec<ModelMessage>,
    capability_plan_ready: bool,
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

    fn active_snapshot(&self) -> Result<Option<ActivePluginRun>> {
        Ok(self.read_state()?.active_run.clone())
    }

    fn host_for_run(&self) -> Result<CompositePluginHost> {
        let state = self.read_state()?;
        Ok(state
            .active_run
            .as_ref()
            .map(|run| run.host.clone())
            .unwrap_or_else(|| state.current.clone()))
    }
}

#[async_trait]
impl AgentExtension for LivePluginHost {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        if let Some(run) = self.active_snapshot()? {
            return Ok(run.prompts);
        }
        AgentExtension::prompt_messages(&self.current_snapshot()?).await
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        if let Some(run) = self.active_snapshot()? {
            return Ok(run.tools);
        }
        AgentExtension::list_tools(&self.current_snapshot()?).await
    }

    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        AgentExtension::call_tool(&self.host_for_run()?, call).await
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        let plan_ready = self
            .active_snapshot()?
            .map(|run| run.capability_plan_ready)
            .unwrap_or(self.read_state()?.capability_plan_ready);
        if !plan_ready {
            return Ok(ToolDecision::Block {
                reason: "插件能力归属仍在解析，本轮暂不执行工具".into(),
            });
        }
        AgentExtension::before_tool(&self.host_for_run()?, call).await
    }

    async fn after_tool(&self, result: &ToolResult) -> Result<()> {
        AgentExtension::after_tool(&self.host_for_run()?, result).await
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        if event.kind == AgentEventKind::RunStarted {
            let (snapshot, capability_plan_ready) = {
                let state = self.read_state()?;
                (
                    state.current.detached_snapshot(),
                    state.capability_plan_ready,
                )
            };
            let tools = AgentExtension::list_tools(&snapshot).await?;
            let prompts = AgentExtension::prompt_messages(&snapshot).await?;
            self.write_state()?.active_run = Some(ActivePluginRun {
                run_id: event.run_id.clone(),
                host: snapshot.clone(),
                tools,
                prompts,
                capability_plan_ready,
            });
            return AgentExtension::on_event(&snapshot, event).await;
        }

        let active = self.active_snapshot()?;
        let host = active
            .as_ref()
            .map(|run| run.host.clone())
            .unwrap_or(self.current_snapshot()?);
        AgentExtension::on_event(&host, event).await?;
        if event.kind == AgentEventKind::RunFinished {
            let mut state = self.write_state()?;
            if state
                .active_run
                .as_ref()
                .is_some_and(|run| run.run_id == event.run_id)
            {
                state.active_run = None;
            }
        }
        Ok(())
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        // UI 事件始终来自最新 Ready 集合；运行快照只冻结模型可见能力。
        AgentExtension::drain_events(&self.current_snapshot()?).await
    }
}

#[async_trait]
impl PluginHost for LivePluginHost {
    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        PluginHost::load_context(&self.host_for_run()?, request).await
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
