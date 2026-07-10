//! Provider-neutral model layer.
//! 与服务商无关的模型层。

pub mod adapter;
pub mod gateway;
pub mod ir;
pub mod stream;
pub mod transform;
#[cfg(any(feature = "openai", feature = "anthropic"))]
pub(crate) mod util;

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "openai")]
pub mod openai;

pub use adapter::{ChatModel, ProviderAdapter};
pub use gateway::{ModelGateway, ModelProviderConfig, OpenAiProtocol, ProviderKind};
pub use ir::{
    ContentBlock, FinishReason, MessageRole, ModelMessage, ModelRequest, ModelResponse,
    ProviderBilling, ReasoningLevel, TokenUsage, ToolChoice,
};
pub use stream::{ModelEventSender, ModelEventStream, ModelStreamEvent};
