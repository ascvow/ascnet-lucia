//! 与模型服务商无关的契约、类型、路由和流式接口。

pub mod contract;
pub mod gateway;
mod provider;
pub mod stream;
pub mod transform;
pub mod types;

#[doc(hidden)]
pub use contract as adapter;
pub use contract::{ChatModel, ProviderAdapter};
pub use gateway::{ModelGateway, ModelProviderConfig, OpenAiProtocol, ProviderKind};
#[cfg(feature = "anthropic")]
pub use provider::anthropic;
#[cfg(feature = "openai")]
pub use provider::openai;
pub use stream::{ModelEventSender, ModelEventStream, ModelStreamEvent};
#[doc(hidden)]
pub use types as ir;
pub use types::{
    ContentBlock, FinishReason, MessageRole, ModelMessage, ModelRequest, ModelResponse,
    ProviderBilling, ReasoningLevel, TokenUsage, ToolChoice,
};
