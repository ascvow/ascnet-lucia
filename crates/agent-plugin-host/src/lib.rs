//! ascnet-lucia 的插件宿主、组合调度和 UI 协议。
//!
//! 该 crate 依赖 core 的通用扩展点，并负责把 WASM 插件适配到 Agent 和 TUI。

#![deny(missing_docs)]

pub mod manifest;
pub mod service;
pub mod ui;

mod contribution;

#[cfg(feature = "wasm")]
mod capability;

#[cfg(feature = "wasm")]
pub mod wasm;

use agent_core::{
    model::{ModelGateway, ModelMessage},
    AgentExtension, ContextLoadRequest, ContextLoader, LoadedContext,
};
#[cfg(feature = "wasm")]
use agent_runtime::{AgentDeriveConfig, AgentProfileId, AgentRuntimeProvisioner};
use agent_tool::{ToolCall, ToolResult, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

pub use agent_core::{AgentEvent, AgentEventKind, ToolDecision};
pub use service::{PluginService, PluginServiceCall};
pub use ui::{UiDeclaration, UiFrame, UiInput, UiRenderRequest};

/// Plugin Host 可注入 WASM 实例的通用宿主服务集合。
///
/// 字段保持私有，后续增加日志、secret 或网络服务时不会扩张 loader 参数列表。默认值不
/// 提供 Agent Runtime，因此现有 loader 和不申请相关权限的插件保持原行为。
#[derive(Clone, Default)]
pub struct PluginHostServices {
    #[cfg(feature = "wasm")]
    agent_runtime: Option<AgentRuntimeHostServices>,
    #[cfg(feature = "wasm")]
    model_completion: Option<ModelCompletionHostServices>,
}

/// Agent Runtime provisioner 与 Host 管理的派生策略注册表。
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub(crate) struct AgentRuntimeHostServices {
    pub(crate) provisioner: Arc<dyn AgentRuntimeProvisioner>,
    pub(crate) controller_profile: AgentProfileId,
    spawn_profiles: Arc<HashMap<String, AgentDeriveConfig>>,
}

/// 应用固定路由的受限模型完成服务。
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub(crate) struct ModelCompletionHostServices {
    pub(crate) gateway: ModelGateway,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) max_output_tokens: u32,
}

impl PluginHostServices {
    /// 创建不提供额外宿主服务的默认集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入 Agent Runtime provisioner、controller 基础 profile 和 Guest 可请求的派生策略。
    ///
    /// `controller_profile` 选择应用注册的基础 `AgentTemplate`；`spawn_profiles` 只提供
    /// 每次派生的 `AgentDeriveConfig`。插件 manifest 仍需逐项声明允许使用的策略。
    #[cfg(feature = "wasm")]
    pub fn with_agent_runtime(
        mut self,
        provisioner: Arc<dyn AgentRuntimeProvisioner>,
        controller_profile: AgentProfileId,
        spawn_profiles: HashMap<String, AgentDeriveConfig>,
    ) -> Result<Self> {
        for profile in spawn_profiles.keys() {
            AgentProfileId::new(profile.clone())
                .map_err(|error| anyhow!("Agent spawn profile `{profile}` 无效：{error}"))?;
        }
        self.agent_runtime = Some(AgentRuntimeHostServices {
            provisioner,
            controller_profile,
            spawn_profiles: Arc::new(spawn_profiles),
        });
        Ok(self)
    }

    /// 注入插件可请求的固定模型路由和最大输出预算。
    ///
    /// Guest 只能提交 system、messages 和更小的输出上限；provider、model、工具和
    /// provider options 均由 Host 控制。插件 manifest 还必须声明 `model_completion`。
    #[cfg(feature = "wasm")]
    pub fn with_model_completion(
        mut self,
        gateway: ModelGateway,
        provider: impl Into<String>,
        model: impl Into<String>,
        max_output_tokens: u32,
    ) -> Result<Self> {
        let provider = provider.into();
        let model = model.into();
        if provider.trim().is_empty() {
            return Err(anyhow!("模型完成 provider 不能为空"));
        }
        if model.trim().is_empty() {
            return Err(anyhow!("模型完成 model 不能为空"));
        }
        if !gateway.contains(&provider) {
            return Err(anyhow!("模型完成 provider `{provider}` 未注册"));
        }
        if max_output_tokens == 0 {
            return Err(anyhow!("模型完成最大输出 token 必须大于零"));
        }
        self.model_completion = Some(ModelCompletionHostServices {
            gateway,
            provider,
            model,
            max_output_tokens,
        });
        Ok(self)
    }

    /// 返回 Host 内部使用的 Agent Runtime 服务快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn agent_runtime(&self) -> Option<AgentRuntimeHostServices> {
        self.agent_runtime.clone()
    }

    /// 返回 Host 内部使用的固定模型完成服务快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn model_completion(&self) -> Option<ModelCompletionHostServices> {
        self.model_completion.clone()
    }
}

#[cfg(feature = "wasm")]
impl AgentRuntimeHostServices {
    /// 返回应用注册的派生策略；不存在时表示应用没有授权该名称。
    pub(crate) fn spawn_profile(&self, profile: &str) -> Option<AgentDeriveConfig> {
        self.spawn_profiles.get(profile).cloned()
    }
}

/// 按可信插件 ID 和插件内视图 ID 两级索引的 UI owner 快照。
type UiRoutes = HashMap<String, HashMap<String, Arc<dyn PluginHost>>>;

/// 插件宿主在通用 Agent 扩展能力之上提供的 UI 接口。
#[async_trait]
pub trait PluginHost: AgentExtension {
    /// 返回宿主实例的稳定 ID；无独立身份的组合或空宿主返回 `None`。
    fn id(&self) -> Option<&str> {
        None
    }

    /// 请求插件为模型调用提供完整替换上下文；返回 `None` 表示本轮透传原始上下文。
    async fn load_context(&self, _request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        Ok(None)
    }

    /// 返回插件提供的终端视图声明。
    async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
        Ok(Vec::new())
    }

    /// 请求插件为指定尺寸渲染一帧；目标不属于当前插件时返回 `None`。
    async fn render_ui(&self, _request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        Ok(None)
    }

    /// 将焦点视图收到的输入路由给对应插件。
    async fn on_ui_input(&self, _input: &UiInput) -> Result<()> {
        Ok(())
    }

    /// 返回当前宿主公开的插件服务。
    async fn services(&self) -> Result<Vec<PluginService>> {
        Ok(Vec::new())
    }

    /// 调用当前宿主拥有的服务；目标不属于当前宿主时返回 `None`。
    async fn call_service(&self, _call: &PluginServiceCall) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    /// 请求宿主实例执行卸载清理。
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// 空插件宿主。
#[derive(Debug, Clone, Default)]
pub struct NoopPluginHost;

#[async_trait]
impl AgentExtension for NoopPluginHost {}

#[async_trait]
impl PluginHost for NoopPluginHost {}

/// 将请求分发给多个子宿主的组合插件宿主。
#[derive(Clone, Default)]
pub struct CompositePluginHost {
    hosts: Vec<Arc<dyn PluginHost>>,
    tool_routes: Arc<RwLock<HashMap<String, Arc<dyn PluginHost>>>>,
    /// 按可信插件 ID 和视图 ID 索引 UI owner，避免渲染和输入阶段遍历插件。
    ui_routes: Arc<RwLock<UiRoutes>>,
    capability_owners: HashMap<String, String>,
}

impl CompositePluginHost {
    /// 创建空组合宿主。
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建与当前宿主集合和能力 owner 一致、但路由缓存彼此隔离的快照。
    fn detached_snapshot(&self) -> Self {
        Self {
            hosts: self.hosts.clone(),
            tool_routes: Arc::new(RwLock::new(HashMap::new())),
            ui_routes: Arc::new(RwLock::new(UiRoutes::new())),
            capability_owners: self.capability_owners.clone(),
        }
    }

    /// 添加子宿主。
    pub fn push(&mut self, host: Arc<dyn PluginHost>) -> &mut Self {
        self.insert(self.hosts.len(), host);
        self
    }

    /// 在指定稳定顺序位置插入子宿主。
    fn insert(&mut self, index: usize, host: Arc<dyn PluginHost>) -> &mut Self {
        self.hosts.insert(index.min(self.hosts.len()), host);
        self.invalidate_tool_routes();
        self.invalidate_ui_routes();
        self
    }

    /// 记录独占能力最终选择的插件 owner。
    pub fn set_capability_owner(
        &mut self,
        capability_id: impl Into<String>,
        plugin_id: impl Into<String>,
    ) -> &mut Self {
        self.capability_owners
            .insert(capability_id.into(), plugin_id.into());
        self
    }

    /// 返回独占能力最终选择的插件 owner。
    pub fn capability_owner(&self, capability_id: &str) -> Option<&str> {
        self.capability_owners
            .get(capability_id)
            .map(String::as_str)
    }

    /// 返回按加载顺序排列的子宿主。
    pub fn hosts(&self) -> &[Arc<dyn PluginHost>] {
        &self.hosts
    }

    /// 返回所有具有稳定身份的子宿主 ID。
    pub fn host_ids(&self) -> Vec<&str> {
        self.hosts.iter().filter_map(|host| host.id()).collect()
    }

    /// 按稳定 ID 查询子宿主。
    pub fn get(&self, id: &str) -> Option<Arc<dyn PluginHost>> {
        self.hosts
            .iter()
            .find(|host| host.id() == Some(id))
            .cloned()
    }

    /// 按稳定 ID 移除并返回子宿主；调用方可随后执行 `shutdown`。
    pub fn remove(&mut self, id: &str) -> Option<Arc<dyn PluginHost>> {
        let index = self.hosts.iter().position(|host| host.id() == Some(id))?;
        let host = self.hosts.remove(index);
        self.capability_owners.retain(|_, owner| owner != id);
        self.invalidate_tool_routes();
        self.invalidate_ui_routes();
        Some(host)
    }

    /// 移除并返回全部子宿主；调用方可逐个执行 `shutdown`。
    pub fn clear(&mut self) -> Vec<Arc<dyn PluginHost>> {
        self.invalidate_tool_routes();
        self.invalidate_ui_routes();
        self.capability_owners.clear();
        std::mem::take(&mut self.hosts)
    }

    /// 返回最近一次工具快照中指定工具的 owner ID。
    pub fn tool_owner(&self, tool_name: &str) -> Result<Option<String>> {
        Ok(self
            .tool_routes
            .read()
            .map_err(|_| anyhow!("插件工具路由锁已中毒"))?
            .get(tool_name)
            .and_then(|host| host.id().map(str::to_string)))
    }

    /// 子宿主数量。
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    /// 是否没有子宿主。
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    fn invalidate_tool_routes(&self) {
        self.tool_routes
            .write()
            .expect("插件工具路由锁不应中毒")
            .clear();
    }

    /// 清空 UI owner 快照，确保宿主集合变化后不会调用旧 owner。
    fn invalidate_ui_routes(&self) {
        self.ui_routes
            .write()
            .expect("插件 UI 路由锁不应中毒")
            .clear();
    }
}

#[async_trait]
impl AgentExtension for CompositePluginHost {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        let mut messages = Vec::new();
        for host in &self.hosts {
            messages.extend(host.prompt_messages().await?);
        }
        Ok(messages)
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        let mut routes = HashMap::new();
        for host in &self.hosts {
            for tool in host.list_tools().await? {
                tool.validate_name()?;
                if routes.insert(tool.name.clone(), host.clone()).is_some() {
                    return Err(anyhow!("插件公开了重复工具：{}", tool.name));
                }
                tools.push(tool);
            }
        }
        *self
            .tool_routes
            .write()
            .map_err(|_| anyhow!("插件工具路由锁已中毒"))? = routes;
        Ok(tools)
    }

    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        let host = self
            .tool_routes
            .read()
            .map_err(|_| anyhow!("插件工具路由锁已中毒"))?
            .get(&call.name)
            .cloned();
        match host {
            Some(host) => host.call_tool(call).await,
            None => Ok(None),
        }
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        let mut current = call.clone();
        let policy_owner = self
            .capability_owner(manifest::TOOL_POLICY_CAPABILITY)
            .map(str::to_string);
        for host in &self.hosts {
            if host
                .id()
                .is_some_and(|id| Some(id) == policy_owner.as_deref())
            {
                continue;
            }
            match host.before_tool(&current).await? {
                ToolDecision::Allow => {}
                ToolDecision::Block { reason } => return Ok(ToolDecision::Block { reason }),
                ToolDecision::CancelRun { reason } => {
                    return Ok(ToolDecision::CancelRun { reason });
                }
                ToolDecision::Rewrite { call } => current = call,
                decision @ ToolDecision::RequireApproval { .. } => return Ok(decision),
            }
        }

        if let Some(owner) = policy_owner {
            let Some(policy) = self.get(&owner) else {
                return Ok(ToolDecision::Block {
                    reason: format!("工具策略插件 `{owner}` 仍在加载，本轮暂不执行工具"),
                });
            };
            match policy.before_tool(&current).await? {
                ToolDecision::Allow => {}
                ToolDecision::Block { reason } => return Ok(ToolDecision::Block { reason }),
                ToolDecision::CancelRun { reason } => {
                    return Ok(ToolDecision::CancelRun { reason });
                }
                ToolDecision::Rewrite { call } => current = call,
                decision @ ToolDecision::RequireApproval { .. } => return Ok(decision),
            }
        }

        if &current == call {
            Ok(ToolDecision::Allow)
        } else {
            Ok(ToolDecision::Rewrite { call: current })
        }
    }

    async fn after_tool(&self, result: &ToolResult) -> Result<()> {
        for host in &self.hosts {
            host.after_tool(result).await?;
        }
        Ok(())
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        for host in &self.hosts {
            host.on_event(event).await?;
        }
        Ok(())
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        let mut events = Vec::new();
        for host in &self.hosts {
            events.extend(host.drain_events().await?);
        }
        Ok(events)
    }
}

#[async_trait]
impl PluginHost for CompositePluginHost {
    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        let Some(owner) = self.capability_owner(manifest::CONTEXT_LOADER_CAPABILITY) else {
            return Ok(None);
        };
        let Some(host) = self.get(owner) else {
            // 渐进加载期间 owner 尚未 Ready，本轮保持原始上下文。
            return Ok(None);
        };
        host.load_context(request).await
    }

    async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
        // 刷新开始即清空旧快照；声明失败时保持无路由，避免继续调用过期 owner。
        self.ui_routes
            .write()
            .map_err(|_| anyhow!("插件 UI 路由锁已中毒"))?
            .clear();

        let mut declarations = Vec::new();
        let mut routes = UiRoutes::new();
        for host in &self.hosts {
            let host_declarations = host.ui_declarations().await?;
            if host_declarations.is_empty() {
                continue;
            }
            let plugin_id = host
                .id()
                .ok_or_else(|| anyhow!("公开 UI 的插件宿主缺少稳定 ID"))?;
            let plugin_routes = routes.entry(plugin_id.to_string()).or_default();
            for mut declaration in host_declarations {
                declaration.plugin_id = plugin_id.to_string();
                if declaration.view_id.trim().is_empty() {
                    return Err(anyhow!("插件 `{plugin_id}` 公开了空 UI 视图 ID"));
                }
                if plugin_routes
                    .insert(declaration.view_id.clone(), host.clone())
                    .is_some()
                {
                    return Err(anyhow!(
                        "插件公开了重复 UI 视图：{plugin_id}/{}",
                        declaration.view_id
                    ));
                }
                declarations.push(declaration);
            }
        }
        *self
            .ui_routes
            .write()
            .map_err(|_| anyhow!("插件 UI 路由锁已中毒"))? = routes;
        Ok(declarations)
    }

    async fn render_ui(&self, request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        let host = self
            .ui_routes
            .read()
            .map_err(|_| anyhow!("插件 UI 路由锁已中毒"))?
            .get(&request.plugin_id)
            .and_then(|routes| routes.get(&request.view_id))
            .cloned();
        match host {
            Some(host) => host.render_ui(request).await,
            None => Ok(None),
        }
    }

    async fn on_ui_input(&self, input: &UiInput) -> Result<()> {
        let host = self
            .ui_routes
            .read()
            .map_err(|_| anyhow!("插件 UI 路由锁已中毒"))?
            .get(&input.plugin_id)
            .and_then(|routes| routes.get(&input.view_id))
            .cloned();
        if let Some(host) = host {
            host.on_ui_input(input).await?;
        }
        Ok(())
    }

    async fn services(&self) -> Result<Vec<PluginService>> {
        let mut services = Vec::new();
        for host in &self.hosts {
            services.extend(host.services().await?);
        }
        Ok(services)
    }

    async fn call_service(&self, call: &PluginServiceCall) -> Result<Option<serde_json::Value>> {
        let Some(host) = self.get(&call.plugin_id) else {
            return Ok(None);
        };
        host.call_service(call).await
    }

    async fn shutdown(&self) -> Result<()> {
        // 依赖方后加载，因此必须先卸载，避免清理阶段访问已停止的 provider。
        for host in self.hosts.iter().rev() {
            host.shutdown().await?;
        }
        Ok(())
    }
}

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

    async fn render_ui(&self, request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        PluginHost::render_ui(&self.current_snapshot()?, request).await
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

#[async_trait]
impl ContextLoader for CompositePluginHost {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        match PluginHost::load_context(self, &request).await? {
            Some(context) => Ok(context),
            None => Ok(LoadedContext::passthrough(request)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{UiInputEvent, UiPlacement, UiSize};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 记录调用次数的测试插件宿主。
    struct CountingPluginHost {
        id: &'static str,
        tool: ToolSpec,
        calls: Arc<AtomicUsize>,
    }

    /// 将任意工具重写为指定名称的测试宿主。
    struct RewritePluginHost {
        id: &'static str,
        target: &'static str,
    }

    #[async_trait]
    impl AgentExtension for RewritePluginHost {
        async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
            let mut call = call.clone();
            call.name = self.target.into();
            Ok(ToolDecision::Rewrite { call })
        }
    }

    #[async_trait]
    impl PluginHost for RewritePluginHost {
        fn id(&self) -> Option<&str> {
            Some(self.id)
        }
    }

    /// 只允许看到最终工具名的测试策略宿主。
    struct FinalPolicyPluginHost;

    #[async_trait]
    impl AgentExtension for FinalPolicyPluginHost {
        async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
            if call.name == "rewritten" {
                Ok(ToolDecision::Block {
                    reason: "最终策略已检查重写调用".into(),
                })
            } else {
                Ok(ToolDecision::Allow)
            }
        }
    }

    #[async_trait]
    impl PluginHost for FinalPolicyPluginHost {
        fn id(&self) -> Option<&str> {
            Some("policy")
        }
    }

    #[async_trait]
    impl AgentExtension for CountingPluginHost {
        async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
            Ok(vec![self.tool.clone()])
        }

        async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(ToolResult::success(
                call.id,
                call.name,
                json!({"owner": self.tool.name}),
            )))
        }
    }

    #[async_trait]
    impl PluginHost for CountingPluginHost {
        fn id(&self) -> Option<&str> {
            Some(self.id)
        }
    }

    /// 公开一个视图并分别记录渲染与输入次数的测试插件宿主。
    struct CountingUiPluginHost {
        id: &'static str,
        declared_plugin_id: &'static str,
        view_id: &'static str,
        render_calls: Arc<AtomicUsize>,
        input_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentExtension for CountingUiPluginHost {}

    #[async_trait]
    impl PluginHost for CountingUiPluginHost {
        /// 返回组合宿主用于覆盖插件自报身份的可信 ID。
        fn id(&self) -> Option<&str> {
            Some(self.id)
        }

        /// 返回带有不可信插件 ID 的声明，用于验证宿主会覆盖该字段。
        async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
            Ok(vec![UiDeclaration {
                plugin_id: self.declared_plugin_id.to_string(),
                view_id: self.view_id.to_string(),
                title: format!("{} 测试视图", self.id),
                placement: UiPlacement::Right,
                size: UiSize::default(),
                focusable: true,
            }])
        }

        /// 记录一次 owner 渲染调用并返回最小可见帧。
        async fn render_ui(&self, _request: &UiRenderRequest) -> Result<Option<UiFrame>> {
            self.render_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(UiFrame {
                view_id: self.view_id.to_string(),
                visible: true,
                lines: Vec::new(),
            }))
        }

        /// 记录一次 owner 输入调用。
        async fn on_ui_input(&self, _input: &UiInput) -> Result<()> {
            self.input_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 构造指向指定插件视图的稳定渲染请求。
    fn ui_render_request(plugin_id: &str, view_id: &str) -> UiRenderRequest {
        UiRenderRequest {
            plugin_id: plugin_id.to_string(),
            view_id: view_id.to_string(),
            instance_id: None,
            width: 40,
            height: 12,
            focused: true,
            frame: 1,
        }
    }

    /// 构造指向指定插件视图的稳定按键输入。
    fn ui_input(plugin_id: &str, view_id: &str) -> UiInput {
        UiInput {
            plugin_id: plugin_id.to_string(),
            view_id: view_id.to_string(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".to_string(),
                modifiers: Vec::new(),
            },
        }
    }

    /// 返回固定摘要的测试上下文宿主。
    struct SummaryPluginHost {
        id: &'static str,
    }

    #[async_trait]
    impl AgentExtension for SummaryPluginHost {}

    #[async_trait]
    impl PluginHost for SummaryPluginHost {
        fn id(&self) -> Option<&str> {
            Some(self.id)
        }

        async fn load_context(
            &self,
            request: &ContextLoadRequest,
        ) -> Result<Option<LoadedContext>> {
            Ok(Some(LoadedContext::new(
                request.system.clone(),
                vec![ModelMessage::text(
                    agent_core::MessageRole::Developer,
                    "插件压缩后的摘要",
                )],
            )))
        }
    }

    /// 组合宿主应只把上下文请求派发给显式选择的独占 owner。
    #[tokio::test]
    async fn selected_context_owner_replaces_model_messages() {
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(SummaryPluginHost { id: "summary" }));
        host.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "summary");
        let request = ContextLoadRequest {
            run_id: "run-1".into(),
            step: 0,
            provider: "test".into(),
            model: "test-model".into(),
            system: Some("系统提示".into()),
            messages: vec![ModelMessage::text(
                agent_core::MessageRole::User,
                "不应发送的完整历史",
            )],
        };

        let loaded = ContextLoader::load(&host, request)
            .await
            .expect("上下文 owner 应完成替换");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text_content(), "插件压缩后的摘要");
    }

    /// 已选中的上下文 owner 返回 `None` 时必须显式透传本轮原始上下文。
    #[tokio::test]
    async fn selected_context_owner_can_pass_through_original_context() {
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(CountingPluginHost {
            id: "empty-context",
            tool: ToolSpec::new("unused", "未使用工具", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        host.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "empty-context");
        let request = ContextLoadRequest {
            run_id: "run-1".into(),
            step: 0,
            provider: "test".into(),
            model: "test-model".into(),
            system: Some("系统提示".into()),
            messages: vec![ModelMessage::text(
                agent_core::MessageRole::User,
                "需要透传的完整历史",
            )],
        };

        let loaded = ContextLoader::load(&host, request)
            .await
            .expect("空替换结果应透传原始上下文");
        assert_eq!(loaded.system.as_deref(), Some("系统提示"));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text_content(), "需要透传的完整历史");
    }

    /// 渐进加载期间上下文 owner 尚未 Ready 时，本轮必须保留原始上下文。
    #[tokio::test]
    async fn selected_context_owner_not_ready_passes_through_original_context() {
        let mut host = CompositePluginHost::new();
        host.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "loading-context");
        let request = ContextLoadRequest {
            run_id: "run-loading".into(),
            step: 0,
            provider: "test".into(),
            model: "test-model".into(),
            system: Some("系统提示".into()),
            messages: vec![ModelMessage::text(
                agent_core::MessageRole::User,
                "加载期间保留的历史",
            )],
        };

        let loaded = ContextLoader::load(&host, request)
            .await
            .expect("owner 未 Ready 时应透传原始上下文");
        assert_eq!(loaded.system.as_deref(), Some("系统提示"));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text_content(), "加载期间保留的历史");
    }

    /// 工具调用只能派发给注册该公开名称的 owner 插件。
    #[tokio::test]
    async fn tool_call_routes_only_to_owner() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(CountingPluginHost {
            id: "first",
            tool: ToolSpec::new("first_tool", "第一个工具", json!({"type": "object"})),
            calls: first_calls.clone(),
        }));
        host.push(Arc::new(CountingPluginHost {
            id: "second",
            tool: ToolSpec::new("second_tool", "第二个工具", json!({"type": "object"})),
            calls: second_calls.clone(),
        }));

        host.list_tools().await.expect("构建工具路由应成功");
        let result = host
            .call_tool(ToolCall::new("call-1", "second_tool", json!({})))
            .await
            .expect("调用不应失败")
            .expect("owner 应返回工具结果");

        assert_eq!(result.content["owner"], "second_tool");
        assert_eq!(first_calls.load(Ordering::SeqCst), 0);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }

    /// 独占工具策略 owner 必须在其他插件完成 Rewrite 后执行。
    #[tokio::test]
    async fn selected_tool_policy_checks_final_rewritten_call() {
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(FinalPolicyPluginHost));
        host.push(Arc::new(RewritePluginHost {
            id: "rewriter",
            target: "rewritten",
        }));
        host.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "policy");

        let decision = host
            .before_tool(&ToolCall::new("call", "original", json!({})))
            .await
            .expect("最终策略检查不应失败");

        assert!(matches!(decision, ToolDecision::Block { .. }));
    }

    /// UI 声明必须使用可信宿主 ID，渲染和输入只能调用对应 owner。
    #[tokio::test]
    async fn ui_calls_route_only_to_declared_owner() {
        let first_render_calls = Arc::new(AtomicUsize::new(0));
        let first_input_calls = Arc::new(AtomicUsize::new(0));
        let second_render_calls = Arc::new(AtomicUsize::new(0));
        let second_input_calls = Arc::new(AtomicUsize::new(0));
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(CountingUiPluginHost {
            id: "first",
            declared_plugin_id: "伪造身份",
            view_id: "panel",
            render_calls: first_render_calls.clone(),
            input_calls: first_input_calls.clone(),
        }));
        host.push(Arc::new(CountingUiPluginHost {
            id: "second",
            declared_plugin_id: "first",
            view_id: "panel",
            render_calls: second_render_calls.clone(),
            input_calls: second_input_calls.clone(),
        }));

        let declarations = host.ui_declarations().await.expect("建立 UI 路由应成功");
        assert_eq!(declarations[0].plugin_id, "first");
        assert_eq!(declarations[1].plugin_id, "second");

        let request = ui_render_request("second", "panel");
        assert!(host
            .render_ui(&request)
            .await
            .expect("owner 渲染不应失败")
            .is_some());
        host.on_ui_input(&ui_input("second", "panel"))
            .await
            .expect("owner 输入不应失败");

        assert_eq!(first_render_calls.load(Ordering::SeqCst), 0);
        assert_eq!(first_input_calls.load(Ordering::SeqCst), 0);
        assert_eq!(second_render_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_input_calls.load(Ordering::SeqCst), 1);
    }

    /// 同一可信插件 ID 下的重复视图必须失败，且不得保留半成品路由。
    #[tokio::test]
    async fn duplicate_ui_routes_are_rejected_without_partial_snapshot() {
        let render_calls = Arc::new(AtomicUsize::new(0));
        let input_calls = Arc::new(AtomicUsize::new(0));
        let mut host = CompositePluginHost::new();
        for declared_plugin_id in ["first-claim", "second-claim"] {
            host.push(Arc::new(CountingUiPluginHost {
                id: "duplicate",
                declared_plugin_id,
                view_id: "panel",
                render_calls: render_calls.clone(),
                input_calls: input_calls.clone(),
            }));
        }

        let error = host
            .ui_declarations()
            .await
            .expect_err("重复 UI 路由必须失败");
        assert!(error.to_string().contains("重复 UI 视图"));
        assert!(host
            .render_ui(&ui_render_request("duplicate", "panel"))
            .await
            .expect("重复声明后查询应安全返回")
            .is_none());
        host.on_ui_input(&ui_input("duplicate", "panel"))
            .await
            .expect("重复声明后输入应安全返回");
        assert_eq!(render_calls.load(Ordering::SeqCst), 0);
        assert_eq!(input_calls.load(Ordering::SeqCst), 0);
    }

    /// 空视图 ID 必须在建立组合路由前被拒绝。
    #[tokio::test]
    async fn empty_ui_view_id_is_rejected_without_snapshot() {
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(CountingUiPluginHost {
            id: "empty-view",
            declared_plugin_id: "ignored",
            view_id: " ",
            render_calls: Arc::new(AtomicUsize::new(0)),
            input_calls: Arc::new(AtomicUsize::new(0)),
        }));

        let error = host
            .ui_declarations()
            .await
            .expect_err("空视图 ID 必须失败");
        assert!(error.to_string().contains("空 UI 视图 ID"));
        assert!(host
            .render_ui(&ui_render_request("empty-view", " "))
            .await
            .expect("失败后路由查询应安全返回")
            .is_none());
    }

    /// 未建立及因宿主集合变化而失效的 UI 路由必须安全返回且不调用插件。
    #[tokio::test]
    async fn missing_and_invalidated_ui_routes_are_safe() {
        let render_calls = Arc::new(AtomicUsize::new(0));
        let input_calls = Arc::new(AtomicUsize::new(0));
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(CountingUiPluginHost {
            id: "owner",
            declared_plugin_id: "ignored",
            view_id: "panel",
            render_calls: render_calls.clone(),
            input_calls: input_calls.clone(),
        }));
        let request = ui_render_request("owner", "panel");
        let input = ui_input("owner", "panel");

        assert!(host
            .render_ui(&request)
            .await
            .expect("未建立路由时渲染应安全返回")
            .is_none());
        host.on_ui_input(&input)
            .await
            .expect("未建立路由时输入应安全返回");

        host.ui_declarations().await.expect("首次建立路由应成功");
        host.push(Arc::new(CountingUiPluginHost {
            id: "other",
            declared_plugin_id: "ignored",
            view_id: "other-panel",
            render_calls: Arc::new(AtomicUsize::new(0)),
            input_calls: Arc::new(AtomicUsize::new(0)),
        }));
        assert!(host
            .render_ui(&request)
            .await
            .expect("push 后旧路由应安全返回")
            .is_none());

        host.ui_declarations().await.expect("重建路由应成功");
        assert!(host.remove("other").is_some());
        host.on_ui_input(&input)
            .await
            .expect("remove 后旧路由应安全返回");

        host.ui_declarations().await.expect("再次建立路由应成功");
        assert_eq!(host.clear().len(), 1);
        assert!(host
            .render_ui(&request)
            .await
            .expect("clear 后旧路由应安全返回")
            .is_none());

        assert_eq!(render_calls.load(Ordering::SeqCst), 0);
        assert_eq!(input_calls.load(Ordering::SeqCst), 0);
    }

    /// 不同插件公开同名工具时必须在模型请求前失败。
    #[tokio::test]
    async fn duplicate_public_tool_names_are_rejected() {
        let mut host = CompositePluginHost::new();
        for _ in 0..2 {
            host.push(Arc::new(CountingPluginHost {
                id: "duplicate-owner",
                tool: ToolSpec::new("duplicate", "重复工具", json!({"type": "object"})),
                calls: Arc::new(AtomicUsize::new(0)),
            }));
        }

        let error = host
            .list_tools()
            .await
            .expect_err("重复公开工具名必须被拒绝");
        assert!(error.to_string().contains("重复工具"));
    }

    /// 组合宿主应公开身份查询、工具 owner 和移除接口。
    #[tokio::test]
    async fn composite_host_exposes_management_api() {
        let mut host = CompositePluginHost::new();
        host.push(Arc::new(CountingPluginHost {
            id: "managed",
            tool: ToolSpec::new("managed_tool", "受管工具", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }));

        host.list_tools().await.expect("构建路由应成功");
        assert_eq!(host.host_ids(), vec!["managed"]);
        assert!(host.get("managed").is_some());
        assert_eq!(
            host.tool_owner("managed_tool").expect("查询 owner 应成功"),
            Some("managed".into())
        );
        assert!(host.remove("managed").is_some());
        assert!(host.is_empty());
        assert_eq!(
            host.tool_owner("managed_tool").expect("查询空路由应成功"),
            None
        );
    }

    /// 运行开始后发布的新插件不得改变本轮工具定义或路由，下一轮才可见。
    #[tokio::test]
    async fn live_host_freezes_tools_until_next_run() {
        let live = LivePluginHost::new();
        let first_calls = Arc::new(AtomicUsize::new(0));
        live.publish(Arc::new(CountingPluginHost {
            id: "first",
            tool: ToolSpec::new("first_tool", "首个工具", json!({"type": "object"})),
            calls: first_calls.clone(),
        }))
        .expect("发布首个插件");

        let started = AgentEvent::new("run-1", AgentEventKind::RunStarted, 0, json!({}));
        live.on_event(&started).await.expect("冻结首轮插件快照");
        assert_eq!(
            live.list_tools()
                .await
                .expect("读取首轮工具")
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            vec!["first_tool"]
        );

        let second_calls = Arc::new(AtomicUsize::new(0));
        live.publish(Arc::new(CountingPluginHost {
            id: "second",
            tool: ToolSpec::new("second_tool", "后续工具", json!({"type": "object"})),
            calls: second_calls.clone(),
        }))
        .expect("运行中发布第二个插件");
        assert!(live.is_ready("second").expect("读取 Ready 状态"));
        assert_eq!(live.list_tools().await.expect("首轮工具保持冻结").len(), 1);
        let unavailable = live
            .call_tool(ToolCall::new("call-2", "second_tool", json!({})))
            .await
            .expect("未进入快照的工具应安全返回");
        assert!(unavailable.is_none());

        let finished = AgentEvent::new("run-1", AgentEventKind::RunFinished, 1, json!({}));
        live.on_event(&finished).await.expect("结束首轮快照");
        let next_started = AgentEvent::new("run-2", AgentEventKind::RunStarted, 0, json!({}));
        live.on_event(&next_started)
            .await
            .expect("冻结下一轮插件快照");
        assert_eq!(live.list_tools().await.expect("读取下一轮工具").len(), 2);
        live.call_tool(ToolCall::new("call-3", "second_tool", json!({})))
            .await
            .expect("下一轮调用新工具")
            .expect("新工具应存在");
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
        assert_eq!(first_calls.load(Ordering::SeqCst), 0);
    }

    /// 并发加载完成顺序不得改变计划中的工具顺序。
    #[tokio::test]
    async fn live_host_publishes_plugins_in_planned_order() {
        let live = LivePluginHost::new();
        live.set_plugin_order(&["first".into(), "second".into()])
            .expect("设置稳定计划顺序");
        live.publish(Arc::new(CountingPluginHost {
            id: "second",
            tool: ToolSpec::new("second_tool", "第二个工具", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .expect("先发布后完成插件");
        live.publish(Arc::new(CountingPluginHost {
            id: "first",
            tool: ToolSpec::new("first_tool", "第一个工具", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .expect("后发布先计划插件");

        let tools = live
            .list_tools()
            .await
            .expect("读取稳定工具顺序")
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(tools, vec!["first_tool", "second_tool"]);
    }

    /// 独占工具策略 owner 尚未 Ready 时必须阻止工具，不能在加载窗口绕过策略。
    #[tokio::test]
    async fn live_host_blocks_tools_until_selected_policy_is_ready() {
        let live = LivePluginHost::new();
        live.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "sandbox")
            .expect("设置策略 owner");
        live.finish_capability_planning().expect("完成能力规划");
        live.on_event(&AgentEvent::new(
            "run-policy",
            AgentEventKind::RunStarted,
            0,
            json!({}),
        ))
        .await
        .expect("冻结缺少策略插件的快照");

        let decision = live
            .before_tool(&ToolCall::new("native-1", "shell", json!({})))
            .await
            .expect("缺少策略时返回可解释阻止结果");
        assert!(matches!(decision, ToolDecision::Block { reason } if reason.contains("仍在加载")));
    }

    /// 独占能力归属尚未解析时必须默认阻止工具，避免启动竞态形成策略空窗。
    #[tokio::test]
    async fn live_host_blocks_tools_while_capability_planning_is_pending() {
        let live = LivePluginHost::new();
        live.on_event(&AgentEvent::new(
            "run-planning",
            AgentEventKind::RunStarted,
            0,
            json!({}),
        ))
        .await
        .expect("冻结能力规划中的插件快照");

        let decision = live
            .before_tool(&ToolCall::new("native-1", "shell", json!({})))
            .await
            .expect("能力规划中返回可解释阻止结果");
        assert!(
            matches!(decision, ToolDecision::Block { reason } if reason.contains("能力归属仍在解析"))
        );
    }
}
