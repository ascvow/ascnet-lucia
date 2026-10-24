//! 静态组合多个插件宿主的调度实现。

use crate::host::PluginHost;
use crate::manifest;
use crate::service::{PluginService, PluginServiceCall};
use crate::services::UiRoutes;
use crate::ui::{
    ToolRenderContext, ToolRenderRequest, ToolRenderState, ToolRendererContribution, UiDeclaration,
    UiFrame, UiInput, UiRenderRequest,
};
use agent_core::model::ModelMessage;
use agent_core::AgentExtension;
use agent_core::ContextLoadRequest;
use agent_core::ContextLoader;
use agent_core::LoadedContext;
use agent_tool::ToolSpec;
use agent_tool::{ToolCall, ToolPermissionScope};
use agent_tool::{ToolErrorKind, ToolResult};
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
    /// 按公开工具名索引的可信权限 scope，与工具 owner 路由在同一次快照中更新。
    tool_permission_scopes: Arc<RwLock<HashMap<String, ToolPermissionScope>>>,
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
            tool_permission_scopes: Arc::new(RwLock::new(HashMap::new())),
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
        let context = redact_permission_denial_for_guest(context);
        host.render_tool(&ToolRenderRequest {
            plugin_id: renderer.plugin_id,
            renderer_id: renderer.renderer_id,
            context,
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
        self.tool_permission_scopes
            .write()
            .expect("插件工具权限 scope 锁不应中毒")
            .clear();
    }

    /// 以最近一次可信 ToolSpec 快照覆盖调用 scope；改名到未知或原生工具时保守使用 unknown。
    ///
    /// 每个插件改写工具调用后都必须重新绑定：后续插件与最终的可信门禁看到的
    /// 必须是改写目标本身的 scope，而不是被改写前那个工具的 scope。
    pub(crate) fn bind_tool_permission_scope(
        &self,
        previous: Option<&ToolCall>,
        mut call: ToolCall,
    ) -> Result<ToolCall> {
        let registered = self
            .tool_permission_scopes
            .read()
            .map_err(|_| anyhow!("插件工具权限 scope 锁已中毒"))?
            .get(&call.name)
            .copied();
        let permission_scope = registered.unwrap_or_else(|| match previous {
            Some(previous) if previous.name == call.name => previous.permission_scope,
            Some(_) => ToolPermissionScope::Unknown,
            // 原生工具不在插件路由中，初始 scope 必须由 Core 注册表提前绑定。
            None => call.permission_scope,
        });
        call.bind_permission_scope(permission_scope);
        if let Some(previous) = previous {
            // Guest 只能改写目标和参数；调用关联键与运行身份始终继承可信输入。
            call.id.clone_from(&previous.id);
            if let Some(run_id) = previous.run_id.as_deref() {
                call.bind_run_id(run_id);
            }
        }
        Ok(call)
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

/// 为 Guest 工具 renderer 隐去可信执行层保存的审批详情。
///
/// TUI 仍持有原始上下文用于默认展示；这里只构造跨越 Host/Guest 边界的副本，避免工具
/// owner 通过自定义 renderer 读取审批模型理由或其他仅面向用户的权限诊断。
fn redact_permission_denial_for_guest(context: &ToolRenderContext) -> ToolRenderContext {
    let mut redacted = context.clone();
    let ToolRenderState::Finished { result } = &context.state else {
        return redacted;
    };
    if result.error_kind != Some(ToolErrorKind::PermissionDenied) {
        return redacted;
    }
    redacted.state = ToolRenderState::Finished {
        result: ToolResult::error_with_kind(
            result.call_id.clone(),
            result.name.clone(),
            ToolErrorKind::PermissionDenied,
            "工具调用未获批准",
        ),
    };
    redacted
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

    async fn prompt_messages_for_run(&self, run_id: &str) -> Result<Vec<ModelMessage>> {
        let mut messages = Vec::new();
        for host in &self.hosts {
            messages.extend(host.prompt_messages_for_run(run_id).await?);
        }
        Ok(messages)
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        let mut routes = HashMap::new();
        let mut permission_scopes = HashMap::new();
        for host in &self.hosts {
            for tool in host.list_tools().await? {
                tool.validate_name()?;
                if routes.insert(tool.name.clone(), host.clone()).is_some() {
                    return Err(anyhow!("插件公开了重复工具：{}", tool.name));
                }
                permission_scopes.insert(tool.name.clone(), tool.permission_scope);
                tools.push(tool);
            }
        }
        *self
            .tool_routes
            .write()
            .map_err(|_| anyhow!("插件工具路由锁已中毒"))? = routes;
        *self
            .tool_permission_scopes
            .write()
            .map_err(|_| anyhow!("插件工具权限 scope 锁已中毒"))? = permission_scopes;
        Ok(tools)
    }

    async fn list_tools_for_run(&self, run_id: &str) -> Result<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        let mut routes = HashMap::new();
        let mut permission_scopes = HashMap::new();
        for host in &self.hosts {
            for tool in host.list_tools_for_run(run_id).await? {
                tool.validate_name()?;
                if routes.insert(tool.name.clone(), host.clone()).is_some() {
                    return Err(anyhow!("插件公开了重复工具：{}", tool.name));
                }
                permission_scopes.insert(tool.name.clone(), tool.permission_scope);
                tools.push(tool);
            }
        }
        *self
            .tool_routes
            .write()
            .map_err(|_| anyhow!("插件工具路由锁已中毒"))? = routes;
        *self
            .tool_permission_scopes
            .write()
            .map_err(|_| anyhow!("插件工具权限 scope 锁已中毒"))? = permission_scopes;
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

    async fn call_tool_for_run(&self, run_id: &str, call: ToolCall) -> Result<Option<ToolResult>> {
        let host = self
            .tool_routes
            .read()
            .map_err(|_| anyhow!("插件工具路由锁已中毒"))?
            .get(&call.name)
            .cloned();
        match host {
            Some(host) => host.call_tool_for_run(run_id, call).await,
            None => Ok(None),
        }
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        let mut events = Vec::new();
        for host in &self.hosts {
            events.extend(host.drain_events().await?);
        }
        Ok(events)
    }

    async fn drain_events_for_run(&self, run_id: &str) -> Result<Vec<serde_json::Value>> {
        let mut events = Vec::new();
        for host in &self.hosts {
            events.extend(host.drain_events_for_run(run_id).await?);
        }
        Ok(events)
    }

    async fn finish_run(&self, run_id: &str) -> Result<()> {
        let mut errors = Vec::new();
        for host in &self.hosts {
            if let Err(error) = host.finish_run(run_id).await {
                let id = host.id().unwrap_or("<anonymous>");
                errors.push(format!("插件 `{id}`：{error:#}"));
            }
        }
        if !errors.is_empty() {
            return Err(anyhow!("插件运行清理失败：{}", errors.join("；")));
        }
        Ok(())
    }
}

#[async_trait]
impl PluginHost for CompositePluginHost {
    fn agent_runtime_api(
        &self,
        plugin_id: &str,
    ) -> Option<Arc<dyn agent_runtime::AgentRuntimeApi>> {
        self.get(plugin_id)?.agent_runtime_api(plugin_id)
    }

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
