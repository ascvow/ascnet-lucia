//! 插件宿主 trait 与空实现。

use crate::manifest::EventSubscription;
use crate::service::{PluginService, PluginServiceCall};
use crate::ui::{
    ToolRenderRequest, ToolRendererContribution, UiDeclaration, UiFrame, UiInput, UiRenderRequest,
};
use agent_core::AgentExtension;
use agent_core::ContextLoadRequest;
use agent_core::EventContext;
use agent_core::LoadedContext;
use agent_runtime::AgentRuntimeApi;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
/// 插件宿主在通用 Agent 扩展能力之上提供的 UI 接口。
#[async_trait]
pub trait PluginHost: AgentExtension {
    /// 返回宿主实例的稳定 ID；无独立身份的组合或空宿主返回 `None`。
    fn id(&self) -> Option<&str> {
        None
    }

    /// 返回指定插件激活实例绑定的 Agent Runtime API。
    ///
    /// 应用用它渲染插件名下 Agent 的原生会话视图。API 由 Host 按该插件自己的可信
    /// principal 创建，调用方只能访问该插件名下的 Agent；插件未声明 Agent 能力时返回
    /// `None`。
    fn agent_runtime_api(&self, _plugin_id: &str) -> Option<Arc<dyn AgentRuntimeApi>> {
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

    /// 返回插件为自有工具提供的消息列表渲染器。
    async fn tool_renderers(&self) -> Result<Vec<ToolRendererContribution>> {
        Ok(Vec::new())
    }

    /// 请求插件为指定尺寸渲染一帧；目标不属于当前插件时返回 `None`。
    async fn render_ui(&self, _request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        Ok(None)
    }

    /// 请求插件渲染一次自有工具调用；目标不属于当前插件时返回 `None`。
    async fn render_tool(&self, _request: &ToolRenderRequest) -> Result<Option<UiFrame>> {
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

    /// 返回插件在 manifest 中声明的事件订阅。
    ///
    /// 订阅是投递权限边界：未声明的事件不会送达该插件，宿主也不会为它在总线上登记
    /// 任何监听。组合宿主自身不订阅事件，只由具体插件宿主返回。
    fn event_subscriptions(&self) -> &[EventSubscription] {
        &[]
    }

    /// 向插件投递一次事件分发。
    ///
    /// 实现方把 `ctx` 的快照交给 Guest，并按 Guest 应答在同一个 `ctx` 上执行白名单
    /// 合并与取消。Guest 只能改写目标事件显式开放的字段。
    ///
    /// # Errors
    ///
    /// Guest 调用失败、应答不符合协议或超出等待预算时返回错误；错误后果由订阅声明的
    /// 故障模式决定，不会中断其余监听者。
    async fn dispatch_event(&self, _ctx: &mut EventContext<'_>) -> Result<()> {
        Ok(())
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
