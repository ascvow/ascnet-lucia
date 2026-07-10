//! Model adapter traits.
//! 模型适配器 trait。

use super::stream::{ModelEventStream, ModelStreamEvent};
use super::{ModelRequest, ModelResponse};
use anyhow::Result;
use async_trait::async_trait;

/// The minimal interface the ReAct loop needs.
/// ReAct loop 需要的最小模型接口。
#[async_trait]
pub trait ChatModel: Send + Sync {
    /// Complete one model turn.
    /// 完成一次模型调用。
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse>;

    /// 流式完成一次模型调用。
    ///
    /// 默认实现：调用 [`complete`](Self::complete)，把最终响应包装为
    /// Start → Done（或 Error）两个事件。原生支持 SSE 的适配器应覆写本方法。
    async fn stream(&self, req: ModelRequest) -> ModelEventStream {
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::Start);
        match self.complete(req).await {
            Ok(response) => tx.done(response),
            Err(err) => tx.error(format!("{err:#}")),
        }
        stream
    }
}

/// A provider adapter that can be registered inside ModelGateway.
/// 可以注册到 ModelGateway 的服务商适配器。
#[async_trait]
pub trait ProviderAdapter: ChatModel {
    /// Stable provider adapter name.
    /// 稳定的服务商适配器名称。
    fn name(&self) -> &'static str;
}
