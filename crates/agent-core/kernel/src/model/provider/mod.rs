//! 具体模型服务商协议实现及其共享 HTTP 支持。
//!
//! Provider-neutral 契约、类型、路由和流协议保留在父模块；本模块只承载服务商适配。

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(any(feature = "openai", feature = "anthropic"))]
pub(crate) mod support;
