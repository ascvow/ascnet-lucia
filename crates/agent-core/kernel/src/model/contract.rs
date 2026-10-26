//! Provider-neutral 模型调用契约。

use super::stream::{ModelEventStream, ModelStreamEvent};
use super::{ModelRequest, ModelResponse};
use anyhow::Result;
use async_trait::async_trait;

/// 模型服务的临时故障；适配器可用此错误明确允许 Agent 重试当前请求。
///
/// 不用于鉴权、请求校验、权限或资源门禁拒绝。消息仅用于诊断，重试判断不解析其文本。
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransientModelError {
    message: String,
}

impl TransientModelError {
    /// 创建可重试故障，不发起请求或执行等待；实际次数与间隔由 Agent 运行配置决定。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 只识别类型化的临时故障，不从远程正文猜测重试条件，避免鉴权或本地错误被盲目重放。
pub(crate) fn is_retryable_model_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if cause.is::<TransientModelError>() {
            return true;
        }
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            );
        }
        #[cfg(any(
            feature = "openai",
            feature = "anthropic",
            feature = "google",
            feature = "bedrock"
        ))]
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            return error.is_timeout()
                || error.is_connect()
                || error.is_body()
                || error.status().is_some_and(|status| {
                    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
                });
        }
        false
    })
}

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
            Err(err) => tx.fail(err),
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

    /// 使用 Gateway 已校验的逻辑 provider 完成一次模型调用。
    ///
    /// 默认实现忽略逻辑名称并调用 [`ChatModel::complete`]；需要把注册别名纳入私有
    /// 协议路由的适配器应覆写本方法。模型请求失败时原样返回适配器错误。
    async fn complete_for_provider(
        &self,
        logical_provider: &str,
        req: ModelRequest,
    ) -> Result<ModelResponse> {
        let _ = logical_provider;
        self.complete(req).await
    }

    /// 使用 Gateway 已校验的逻辑 provider 发起流式模型调用。
    ///
    /// 默认实现保留适配器自身的流式能力；需要把注册别名纳入私有协议路由的适配器
    /// 应覆写本方法。协议错误通过返回的事件流传递。
    async fn stream_for_provider(
        &self,
        logical_provider: &str,
        req: ModelRequest,
    ) -> ModelEventStream {
        let _ = logical_provider;
        self.stream(req).await
    }
}
