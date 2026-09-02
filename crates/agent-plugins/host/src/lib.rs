//! ascnet-lucia 的插件宿主、组合调度和 UI 协议。
//!
//! 该 crate 依赖 core 的通用扩展点，并负责把 WASM 插件适配到 Agent 和 TUI。

#![deny(missing_docs)]

pub mod audit;
pub mod manifest;
pub mod service;
pub mod session;
pub mod ui;

mod composite;
mod contribution;
mod host;
mod live;
mod services;
#[cfg(test)]
mod tests;

#[cfg(feature = "wasm")]
mod capability;
#[cfg(feature = "wasm")]
mod state;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use agent_core::{AgentEvent, AgentEventKind, AgentExtension};
pub use composite::CompositePluginHost;
pub use host::{NoopPluginHost, PluginHost};
pub use live::LivePluginHost;
pub use service::{PluginService, PluginServiceCall};
pub use services::PluginHostServices;
pub use ui::{
    ToolRenderContext, ToolRenderRequest, ToolRendererContribution, UiDeclaration, UiFrame,
    UiInput, UiRenderRequest,
};
