//! 模型输出事件流。
//!
//! 参考 pi-ai 的 EventStream 设计：流是一等公民，`complete()` 等价于
//! `stream().result()`。生产者通过 [`ModelEventSender`] push 事件，
//! 消费者逐个消费 [`ModelStreamEvent`] 或直接等待最终 [`ModelResponse`]。

use super::types::ModelResponse;
use agent_tool::ToolCall;
use anyhow::{anyhow, Result};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// 只限制尚未消费的事件，不限制 Agent 运行轮数或生命周期累计输出。
const MAX_PENDING_EVENTS: usize = 4096;
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct StreamBudget {
    events: usize,
    bytes: usize,
    terminal: bool,
    overflowed: bool,
    task: Option<tokio::task::AbortHandle>,
}

/// 模型流式输出的事件。
///
/// `index` 为内容块在响应中的序号，同一个块的 Delta/End 事件共享同一 index。
#[derive(Debug, Clone)]
pub enum ModelStreamEvent {
    /// 响应开始。
    Start,

    /// 文本增量。
    TextDelta {
        /// 目标文本块在本次响应中的稳定序号。
        index: usize,
        /// 本次新增的文本片段。
        delta: String,
    },

    /// 文本块结束，携带完整文本。
    TextEnd {
        /// 已完成文本块的稳定序号。
        index: usize,
        /// 将此前增量合并后的完整文本。
        text: String,
    },

    /// 思维链增量。
    ThinkingDelta {
        /// 目标推理块在本次响应中的稳定序号。
        index: usize,
        /// 本次新增的推理文本片段。
        delta: String,
    },

    /// 思维链块结束，携带完整推理内容。
    ThinkingEnd {
        /// 已完成推理块的稳定序号。
        index: usize,
        /// 将此前增量合并后的完整推理文本。
        thinking: String,
    },

    /// 工具调用参数增量（原始 JSON 片段）。
    ToolCallDelta {
        /// 目标工具调用块在本次响应中的稳定序号。
        index: usize,
        /// 原始参数 JSON 片段，单个片段不保证可独立解析。
        delta: String,
    },

    /// 工具调用块结束，携带解析后的完整调用。
    ToolCallEnd {
        /// 已完成工具调用块的稳定序号。
        index: usize,
        /// 适配器解析并校验后的完整工具调用。
        call: ToolCall,
    },

    /// 响应完成，携带最终响应。
    Done {
        /// 合并所有流式块后的最终 provider-neutral 响应。
        response: ModelResponse,
    },

    /// 响应失败，携带错误描述。
    Error {
        /// 可供调用方诊断的错误描述；流在此事件后终止。
        message: String,
    },
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
    tx: mpsc::UnboundedSender<(ModelStreamEvent, usize)>,
    budget: Arc<Mutex<StreamBudget>>,
}

impl ModelEventSender {
    /// 发送一个事件；积压超限时以一个明确的 Error 终止并取消网络任务，不静默丢正文。
    pub fn send(&self, event: ModelStreamEvent) {
        if self.tx.is_closed() {
            return;
        }
        let bytes = event_bytes(&event);
        let mut budget = self
            .budget
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if budget.terminal {
            return;
        }
        if budget.events >= MAX_PENDING_EVENTS
            || bytes > MAX_PENDING_BYTES.saturating_sub(budget.bytes)
        {
            budget.terminal = true;
            budget.overflowed = true;
            let _ = self.tx.send((
                ModelStreamEvent::Error {
                    message: "model stream pending events exceeded resource limit".into(),
                },
                0,
            ));
            if let Some(task) = &budget.task {
                task.abort();
            }
            return;
        }
        budget.terminal = event.is_terminal();
        budget.events += 1;
        budget.bytes += bytes;
        let _ = self.tx.send((event, bytes));
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

/// 文本直接计字节，结构化终态用有界 writer 计量，不为排队再分配整份 JSON。
fn event_bytes(event: &ModelStreamEvent) -> usize {
    fn json_bytes(value: &impl serde::Serialize) -> usize {
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > MAX_PENDING_BYTES {
                    return Err(std::io::Error::other("stream event too large"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut count = Counter(0);
        if serde_json::to_writer(&mut count, value).is_err() {
            MAX_PENDING_BYTES + 1
        } else {
            count.0
        }
    }
    let bytes = match event {
        ModelStreamEvent::Start => 0,
        ModelStreamEvent::TextDelta { delta, .. }
        | ModelStreamEvent::ThinkingDelta { delta, .. }
        | ModelStreamEvent::ToolCallDelta { delta, .. } => delta.len(),
        ModelStreamEvent::TextEnd { text, .. } => text.len(),
        ModelStreamEvent::ThinkingEnd { thinking, .. } => thinking.len(),
        ModelStreamEvent::Error { message } => message.len(),
        ModelStreamEvent::ToolCallEnd { call, .. } => json_bytes(call),
        ModelStreamEvent::Done { response } => json_bytes(response),
    };
    bytes.saturating_add(std::mem::size_of::<ModelStreamEvent>())
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
    rx: mpsc::UnboundedReceiver<(ModelStreamEvent, usize)>,
    budget: Arc<Mutex<StreamBudget>>,
    /// 缓存的终止事件产物；`next` 消费到 Done/Error 时填充。
    final_result: Option<Result<ModelResponse>>,
    /// 可选的模型请求后台任务；消费者提前丢弃流时立即终止网络请求。
    request_task: Option<tokio::task::JoinHandle<()>>,
}

impl ModelEventStream {
    /// 创建一对生产者/消费者。
    pub fn channel() -> (ModelEventSender, ModelEventStream) {
        let (tx, rx) = mpsc::unbounded_channel();
        let budget = Arc::new(Mutex::new(StreamBudget::default()));
        (
            ModelEventSender {
                tx,
                budget: Arc::clone(&budget),
            },
            ModelEventStream {
                rx,
                budget,
                final_result: None,
                request_task: None,
            },
        )
    }

    /// 绑定实际执行模型请求的后台任务。
    ///
    /// 流被取消或提前丢弃时会中止该任务，避免 HTTP 请求继续占用连接与 token；正常消费到
    /// 终态后，已完成任务的中止操作没有副作用。
    pub(crate) fn with_request_task(mut self, task: tokio::task::JoinHandle<()>) -> Self {
        {
            let mut budget = self
                .budget
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            budget.task = Some(task.abort_handle());
            if budget.overflowed {
                task.abort();
            }
        }
        self.request_task = Some(task);
        self
    }

    /// 取下一个事件；流结束后返回 None。
    ///
    /// 终止事件（Done/Error）也会正常返回给调用方，同时缓存其结果供
    /// [`result`](Self::result) 使用。
    pub async fn next(&mut self) -> Option<ModelStreamEvent> {
        if self.final_result.is_some() {
            return None;
        }
        let (event, bytes) = self.rx.recv().await?;
        {
            let mut budget = self
                .budget
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            budget.events = budget.events.saturating_sub(1);
            budget.bytes = budget.bytes.saturating_sub(bytes);
        }
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
            .take()
            .unwrap_or_else(|| Err(anyhow!("model stream ended without a terminal event")))
    }
}

impl Drop for ModelEventStream {
    fn drop(&mut self) {
        if let Some(task) = self.request_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 慢消费者积压超限时必须收到终止错误，并取消已绑定的生产任务。
    #[tokio::test]
    async fn pending_queue_overflow_is_terminal_and_cancels_request() {
        let (tx, stream) = ModelEventStream::channel();
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let stream = stream.with_request_task(task);
        for _ in 0..=MAX_PENDING_EVENTS {
            tx.send(ModelStreamEvent::Start);
        }
        tx.done(ModelResponse::text("must not mask overflow"));
        assert!(stream
            .result()
            .await
            .unwrap_err()
            .to_string()
            .contains("resource limit"));
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    /// 消费后返还名额，长流不是生命周期累计限额。
    #[tokio::test]
    async fn consuming_events_releases_pending_budget() {
        let (tx, mut stream) = ModelEventStream::channel();
        for _ in 0..MAX_PENDING_EVENTS * 2 {
            tx.send(ModelStreamEvent::TextDelta {
                index: 0,
                delta: "文本".into(),
            });
            assert!(matches!(
                stream.next().await,
                Some(ModelStreamEvent::TextDelta { .. })
            ));
        }
        tx.done(ModelResponse::text("ok"));
        assert_eq!(stream.result().await.unwrap().text_content(), "ok");
    }

    /// 超大单事件同样被明确拒绝，不会绕过字节限额。
    #[tokio::test]
    async fn oversized_event_is_rejected() {
        let (tx, stream) = ModelEventStream::channel();
        tx.send(ModelStreamEvent::TextDelta {
            index: 0,
            delta: "x".repeat(MAX_PENDING_BYTES + 1),
        });
        assert!(stream
            .result()
            .await
            .unwrap_err()
            .to_string()
            .contains("resource limit"));
    }

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
