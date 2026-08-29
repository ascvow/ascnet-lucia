//! 插件宿主 trait 与空实现。

use crate::service::{PluginService, PluginServiceCall};
use crate::ui::{
    ToolRenderRequest, ToolRendererContribution, UiDeclaration, UiFrame, UiInput, UiRenderRequest,
};
use agent_core::AgentExtension;
use agent_core::ContextLoadRequest;
use agent_core::LoadedContext;
use anyhow::Result;
use async_trait::async_trait;
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
