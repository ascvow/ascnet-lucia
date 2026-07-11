//! 模型输出事件流。
//!
//! 参考 pi-ai 的 EventStream 设计：流是一等公民，`complete()` 等价于
//! `stream().result()`。生产者通过 [`ModelEventSender`] push 事件，
//! 消费者逐个消费 [`ModelStreamEvent`] 或直接等待最终 [`ModelResponse`]。

use super::ir::ModelResponse;
use agent_tool::ToolCall;
use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

/// 模型流式输出的事件。
///
/// `index` 为内容块在响应中的序号，同一个块的 Delta/End 事件共享同一 index。
#[derive(Debug, Clone)]
pub enum ModelStreamEvent {
    /// 响应开始。
    Start,

    /// 文本增量。
    TextDelta { index: usize, delta: String },

    /// 文本块结束，携带完整文本。
    TextEnd { index: usize, text: String },

    /// 思维链增量。
    ThinkingDelta { index: usize, delta: String },

    /// 思维链块结束，携带完整推理内容。
    ThinkingEnd { index: usize, thinking: String },

    /// 工具调用参数增量（原始 JSON 片段）。
    ToolCallDelta { index: usize, delta: String },

    /// 工具调用块结束，携带解析后的完整调用。
    ToolCallEnd { index: usize, call: ToolCall },

    /// 响应完成，携带最终响应。
    Done { response: ModelResponse },

    /// 响应失败，携带错误描述。
    Error { message: String },
}

impl ModelStreamEvent {
    /// 是否为终止事件（Done 或 Error）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}

/// 事件流的生产者端。
///
/// 适配器在解析 SSE 等流式响应时通过它 push 事件。
/// 所有 send 方法在消费端已关闭时静默丢弃事件。
#[derive(Clone)]
pub struct ModelEventSender {
    tx: mpsc::UnboundedSender<ModelStreamEvent>,
}

impl ModelEventSender {
    /// 发送一个事件。
    pub fn send(&self, event: ModelStreamEvent) {
        let _ = self.tx.send(event);
    }

    /// 发送终止事件 Done。
    pub fn done(&self, response: ModelResponse) {
        self.send(ModelStreamEvent::Done { response });
    }

    /// 发送终止事件 Error。
    pub fn error(&self, message: impl Into<String>) {
        self.send(ModelStreamEvent::Error {
            message: message.into(),
        });
    }
}

/// 事件流的消费者端。
///
/// 两种消费方式：
/// 1. 循环调用 [`next`](Self::next) 逐事件处理（实时 UI）
/// 2. 直接调用 [`result`](Self::result) 等待最终响应（批式调用）
///
/// 混合消费也可以：先 `next` 处理若干事件，再 `result` 拿最终响应；
/// 终止事件被 `next` 返回后仍会缓存，`result` 不会丢失它。
pub struct ModelEventStream {
    rx: mpsc::UnboundedReceiver<ModelStreamEvent>,
    /// 缓存的终止事件产物；`next` 消费到 Done/Error 时填充。
    final_result: Option<Result<ModelResponse>>,
}

impl ModelEventStream {
    /// 创建一对生产者/消费者。
    pub fn channel() -> (ModelEventSender, ModelEventStream) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            ModelEventSender { tx },
            ModelEventStream {
                rx,
                final_result: None,
            },
        )
    }

    /// 取下一个事件；流结束后返回 None。
    ///
    /// 终止事件（Done/Error）也会正常返回给调用方，同时缓存其结果供
    /// [`result`](Self::result) 使用。
    pub async fn next(&mut self) -> Option<ModelStreamEvent> {
        if self.final_result.is_some() {
            return None;
        }
        let event = self.rx.recv().await?;
        match &event {
            ModelStreamEvent::Done { response } => {
                self.final_result = Some(Ok(response.clone()));
            }
            ModelStreamEvent::Error { message } => {
                self.final_result = Some(Err(anyhow!("{message}")));
            }
            _ => {}
        }
        Some(event)
    }

    /// 消费剩余事件并返回最终响应。
    ///
    /// 错误：模型返回 Error 事件，或生产者在发出终止事件前断开。
    pub async fn result(mut self) -> Result<ModelResponse> {
        while self.final_result.is_none() {
            if self.next().await.is_none() {
                break;
            }
        }
        self.final_result
            .unwrap_or_else(|| Err(anyhow!("model stream ended without a terminal event")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// result() 直接等待最终响应。
    #[tokio::test]
    async fn result_waits_for_done() {
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::Start);
        tx.send(ModelStreamEvent::TextDelta {
            index: 0,
            delta: "he".into(),
        });
        tx.done(ModelResponse::text("hello"));

        let response = stream.result().await.expect("应该拿到最终响应");
        assert_eq!(response.text_content(), "hello");
    }

    /// 混合消费：先逐事件处理，终止后 result() 仍能取到响应。
    #[tokio::test]
    async fn mixed_consumption_keeps_final_result() {
        let (tx, mut stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::TextDelta {
            index: 0,
            delta: "hi".into(),
        });
        tx.done(ModelResponse::text("hi"));

        let mut count = 0;
        while let Some(_event) = stream.next().await {
            count += 1;
        }
        assert_eq!(count, 2);

        let response = stream.result().await.expect("终止事件应被缓存");
        assert_eq!(response.text_content(), "hi");
    }

    /// Error 事件让 result() 返回错误。
    #[tokio::test]
    async fn error_event_propagates() {
        let (tx, stream) = ModelEventStream::channel();
        tx.error("boom");

        let err = stream.result().await.expect_err("应该返回错误");
        assert!(err.to_string().contains("boom"));
    }

    /// 生产者提前断开时 result() 报缺少终止事件。
    #[tokio::test]
    async fn dropped_sender_yields_error() {
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::Start);
        drop(tx);

        let err = stream.result().await.expect_err("应该返回错误");
        assert!(err.to_string().contains("without a terminal event"));
    }
}
