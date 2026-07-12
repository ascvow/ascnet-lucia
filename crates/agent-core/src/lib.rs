//! ascnet-lucia core.
//!
//! Core principle / 核心原则：
//! - ReAct loop belongs to core. / ReAct 循环属于 core。
//! - Model wire protocols are converted at the boundary. / 模型网络协议只在边界做转换。
//! - 外部能力属于工具或调用方提供的扩展。
//! - No embedded HTTP proxy server. / 不内置独立 HTTP proxy server。
//! - Runtime model config and event persistence are caller-owned. / 运行时模型配置和事件持久化由调用方拥有。
//! - Billing data comes from provider responses; core does not keep local price tables.
//!   计费数据来自服务商响应；core 不维护本地价格表。

#![deny(missing_docs)]

pub mod agent;
pub mod config;
pub mod context;
pub mod event;
pub mod extension;
pub mod model;
pub mod session;
pub mod state;

pub use agent::{Agent, AgentControl, AgentModelConfig, AgentOptions, AgentRun};
pub use config::{AgentConfig, AgentRootConfig, ModelConfig};
pub use context::{
    ContextLoadRequest, ContextLoader, ContextTransform, LoadedContext, PassthroughContextLoader,
    TransformContextLoader,
};
pub use event::{
    AgentEvent, AgentEventKind, BillingUsage, CompositeEventSink, EventSink, InMemoryEventSink,
    JsonlEventSink, NoopEventSink,
};
pub use extension::{AgentExtension, CompositeAgentExtension, NoopAgentExtension, ToolDecision};
pub use model::{
    ChatModel, ContentBlock, FinishReason, MessageRole, ModelGateway, ModelMessage,
    ModelProviderConfig, ModelRequest, ModelResponse, OpenAiProtocol, ProviderAdapter,
    ProviderBilling, ProviderKind, ReasoningLevel, TokenUsage, ToolChoice,
};
pub use session::Session;
pub use state::{AgentPhase, AgentState, AgentToolCallState, AgentToolCallStatus};
