//! 静态组合多个插件宿主的调度实现。

use crate::host::PluginHost;
use crate::manifest;
use crate::service::{PluginService, PluginServiceCall};
use crate::services::UiRoutes;
use crate::ui::{
    ToolRenderContext, ToolRenderRequest, ToolRendererContribution, UiDeclaration, UiFrame,
    UiInput, UiRenderRequest,
};
use agent_core::model::ModelMessage;
use agent_core::AgentEvent;
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
/// 将请求分发给多个子宿主的组合插件宿主。
#[derive(Clone, Default)]
pub struct CompositePluginHost {
    hosts: Vec<Arc<dyn PluginHost>>,
    tool_routes: Arc<RwLock<HashMap<String, Arc<dyn PluginHost>>>>,
    /// 按可信插件 ID 和视图 ID 索引 UI owner，避免渲染和输入阶段遍历插件。
    ui_routes: Arc<RwLock<UiRoutes>>,
    /// 按公开工具名索引的可信 renderer 声明快照。
    tool_renderers: Arc<RwLock<HashMap<String, ToolRendererContribution>>>,
    capability_owners: HashMap<String, String>,
}

impl CompositePluginHost {
    /// 创建空组合宿主。
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建与当前宿主集合和能力 owner 一致、但路由缓存彼此隔离的快照。
    pub(crate) fn detached_snapshot(&self) -> Self {
        Self {
            hosts: self.hosts.clone(),
            tool_routes: Arc::new(RwLock::new(HashMap::new())),
            ui_routes: Arc::new(RwLock::new(UiRoutes::new())),
            tool_renderers: Arc::new(RwLock::new(HashMap::new())),
            capability_owners: self.capability_owners.clone(),
        }
    }

    /// 添加子宿主。
    pub fn push(&mut self, host: Arc<dyn PluginHost>) -> &mut Self {
        self.insert(self.hosts.len(), host);
        self
    }

    /// 在指定稳定顺序位置插入子宿主。
    pub(crate) fn insert(&mut self, index: usize, host: Arc<dyn PluginHost>) -> &mut Self {
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

    /// 根据可信贡献快照为工具消息选择 owner 并请求渲染。
    pub async fn render_tool_message(
        &self,
        context: &ToolRenderContext,
    ) -> Result<Option<UiFrame>> {
        let renderer = self
            .tool_renderers
            .read()
            .map_err(|_| anyhow!("插件工具 renderer 路由锁已中毒"))?
            .get(&context.call.name)
            .cloned();
        let Some(renderer) = renderer else {
            return Ok(None);
        };
        let Some(host) = self.get(&renderer.plugin_id) else {
            return Ok(None);
        };
        host.render_tool(&ToolRenderRequest {
            plugin_id: renderer.plugin_id,
            renderer_id: renderer.renderer_id,
            context: context.clone(),
        })
        .await
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
        self.tool_renderers
            .write()
            .expect("插件工具 renderer 路由锁不应中毒")
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
        self.tool_renderers
            .write()
            .map_err(|_| anyhow!("插件工具 renderer 路由锁已中毒"))?
            .clear();

        let mut declarations = Vec::new();
        let mut routes = UiRoutes::new();
        let mut tool_renderers = HashMap::new();
        for host in &self.hosts {
            let host_declarations = host.ui_declarations().await?;
            let host_renderers = host.tool_renderers().await?;
            if host_declarations.is_empty() && host_renderers.is_empty() {
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
            let owned_tools = host
                .list_tools()
                .await?
                .into_iter()
                .map(|tool| tool.name)
                .collect::<std::collections::HashSet<_>>();
            for mut renderer in host_renderers {
                renderer.plugin_id = plugin_id.to_string();
                if renderer.renderer_id.trim().is_empty() {
                    return Err(anyhow!("插件 `{plugin_id}` 公开了空工具 renderer ID"));
                }
                if !owned_tools.contains(&renderer.tool_name) {
                    return Err(anyhow!(
                        "插件 `{plugin_id}` 不能为非自有工具 `{}` 注册 renderer",
                        renderer.tool_name
                    ));
                }
                if tool_renderers
                    .insert(renderer.tool_name.clone(), renderer)
                    .is_some()
                {
                    return Err(anyhow!("同一工具不能注册多个消息 renderer"));
                }
            }
        }
        *self
            .ui_routes
            .write()
            .map_err(|_| anyhow!("插件 UI 路由锁已中毒"))? = routes;
        *self
            .tool_renderers
            .write()
            .map_err(|_| anyhow!("插件工具 renderer 路由锁已中毒"))? = tool_renderers;
        Ok(declarations)
    }

    async fn tool_renderers(&self) -> Result<Vec<ToolRendererContribution>> {
        let mut renderers = self
            .tool_renderers
            .read()
            .map_err(|_| anyhow!("插件工具 renderer 路由锁已中毒"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        renderers.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        Ok(renderers)
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

    async fn render_tool(&self, request: &ToolRenderRequest) -> Result<Option<UiFrame>> {
        let renderer = self
            .tool_renderers
            .read()
            .map_err(|_| anyhow!("插件工具 renderer 路由锁已中毒"))?
            .get(&request.context.call.name)
            .cloned();
        let Some(renderer) = renderer else {
            return Ok(None);
        };
        if renderer.plugin_id != request.plugin_id || renderer.renderer_id != request.renderer_id {
            return Ok(None);
        }
        let Some(host) = self.get(&renderer.plugin_id) else {
            return Ok(None);
        };
        host.render_tool(request).await
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

#[async_trait]
impl ContextLoader for CompositePluginHost {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        match PluginHost::load_context(self, &request).await? {
            Some(context) => Ok(context),
            None => Ok(LoadedContext::passthrough(request)),
        }
    }
}
