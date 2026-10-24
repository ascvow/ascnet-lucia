//! 把 Agent 事件流转换为 TUI 通道事件的宿主 Sink。

use crate::event::UiEvent;
use crate::stream_probe::StreamProbe;
use crate::theme::{COLOR_DANGER, COLOR_MUTED, COLOR_SUCCESS, COLOR_USER, COLOR_WARNING};
#[cfg(feature = "plugins")]
use crate::ui::plugin_bridge::plugin_footer_event;
use agent_core::event::{AgentEvent, AgentEventKind, EventSink};
use agent_core::model::TokenUsage;
#[cfg(feature = "plugins")]
use agent_plugin_protocol::{UiNavigationRequest, UI_NAVIGATION_EVENT};
use agent_tool::{ToolCall, ToolOutputDelta, ToolResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
/// 合并模型文本增量，避免慢速终端为每个 token 累积一个无界通道事件。
#[derive(Default)]
pub(crate) struct ModelDeltaBuffer {
    batches: std::collections::VecDeque<String>,
    notification_pending: bool,
}

impl ModelDeltaBuffer {
    /// 追加文本，并返回是否需要向 UI 发送新的就绪通知。
    fn push(&mut self, delta: &str) -> bool {
        let notify = !self.notification_pending;
        if notify {
            self.batches.push_back(String::new());
            self.notification_pending = true;
        }
        self.batches
            .back_mut()
            .expect("增量已有对应批次")
            .push_str(delta);
        notify
    }

    /// 关闭当前合批窗口；已排队通知仍只消费自己的批次，不能跨过结构事件读走未来正文。
    fn seal(&mut self) {
        self.notification_pending = false;
    }

    /// 按通知顺序取出一个批次；后续轮次即使已经到达，也必须等它自己的通知。
    pub(crate) fn take(&mut self) -> String {
        let text = self.batches.pop_front().unwrap_or_default();
        if self.batches.is_empty() {
            self.notification_pending = false;
        }
        text
    }
}

/// 合并工具运行期输出，避免每个 chunk 都触发一次整帧重绘。
///
/// 模型文本早就有同样的合批，工具输出却一直是"一个 chunk 一个事件"。命令狂打日志时
/// 事件率可以到每秒数千，而每个事件都要整帧重扫预览缓冲：这是唯一能把渲染线程打满的
/// 路径。这里按到达顺序保留全部 chunk，UI 一次取走并顺序拼接，保证输出不乱序、不丢失。
#[derive(Default)]
pub(crate) struct ToolOutputBuffer {
    batches: std::collections::VecDeque<Vec<ToolOutputDelta>>,
    notification_pending: bool,
}

impl ToolOutputBuffer {
    /// 追加一段输出，并返回是否需要向 UI 发送新的就绪通知。
    fn push(&mut self, delta: ToolOutputDelta) -> bool {
        let notify = !self.notification_pending;
        if notify {
            self.batches.push_back(Vec::new());
            self.notification_pending = true;
        }
        self.batches
            .back_mut()
            .expect("工具输出已有对应批次")
            .push(delta);
        notify
    }

    /// 工具生命周期事件关闭当前批次，防止消费时读到尚未创建消息的下一次调用输出。
    fn seal(&mut self) {
        self.notification_pending = false;
    }

    /// 按通知顺序取走一批输出，不能跨过工具开始或结束事件。
    pub(crate) fn take(&mut self) -> Vec<ToolOutputDelta> {
        let deltas = self.batches.pop_front().unwrap_or_default();
        if self.batches.is_empty() {
            self.notification_pending = false;
        }
        deltas
    }
}

/// 回答文本（含推理摘要）的共享增量缓冲区。
type SharedModelDeltas = Arc<std::sync::Mutex<ModelDeltaBuffer>>;

/// 工具运行期输出的共享增量缓冲区。
type SharedToolOutput = Arc<std::sync::Mutex<ToolOutputBuffer>>;

/// 将 Agent 事件转换为 TUI 事件；高频模型文本通过共享缓冲区合并。
pub(crate) struct ChannelEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
    model_deltas: SharedModelDeltas,
    thinking_deltas: SharedModelDeltas,
    tool_output: SharedToolOutput,
    /// 可选的流式诊断探针；只记录到达节奏，不参与 UI 决策。
    probe: Option<StreamProbe>,
}

impl ChannelEventSink {
    /// 绑定流式诊断探针；未启用诊断时传入 `None`，所有记录点都会短路。
    pub(crate) fn with_probe(mut self, probe: Option<StreamProbe>) -> Self {
        self.probe = probe;
        self
    }

    /// 创建事件 sink，并返回供 UI 消费回答文本、推理摘要与工具输出的共享缓冲区。
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<UiEvent>,
    ) -> (Self, SharedModelDeltas, SharedModelDeltas, SharedToolOutput) {
        let model_deltas = Arc::new(std::sync::Mutex::new(ModelDeltaBuffer::default()));
        let thinking_deltas = Arc::new(std::sync::Mutex::new(ModelDeltaBuffer::default()));
        let tool_output = Arc::new(std::sync::Mutex::new(ToolOutputBuffer::default()));
        (
            Self {
                tx,
                model_deltas: model_deltas.clone(),
                thinking_deltas: thinking_deltas.clone(),
                tool_output: tool_output.clone(),
                probe: None,
            },
            model_deltas,
            thinking_deltas,
            tool_output,
        )
    }
}

#[async_trait]
impl EventSink for ChannelEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        if matches!(
            event.kind,
            AgentEventKind::ToolStarted
                | AgentEventKind::ToolFinished
                | AgentEventKind::ToolSkipped
                | AgentEventKind::RunFinished
        ) {
            self.tool_output
                .lock()
                .expect("工具输出缓冲区锁不应中毒")
                .seal();
        }
        if matches!(
            event.kind,
            AgentEventKind::ModelRequest
                | AgentEventKind::ModelResponse
                | AgentEventKind::ToolStarted
                | AgentEventKind::ToolSkipped
                | AgentEventKind::RunFinished
        ) {
            self.model_deltas
                .lock()
                .expect("模型增量缓冲区锁不应中毒")
                .seal();
            self.thinking_deltas
                .lock()
                .expect("推理摘要增量缓冲区锁不应中毒")
                .seal();
        }
        match event.kind {
            AgentEventKind::ModelRequest => {
                if let Some(snapshot) = event
                    .payload
                    .get("context")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                {
                    let _ = self.tx.send(UiEvent::ContextSnapshot(snapshot));
                }
                let _ = self.tx.send(UiEvent::ModelStarted);
            }
            AgentEventKind::ModelResponse => {
                // 完整分项一并上送：底栏按输入合计显示上下文规模，缓存读写用于累计命中率。
                if let Some(usage) = model_usage(&event.payload) {
                    let _ = self.tx.send(UiEvent::ModelUsage(usage));
                }
            }
            AgentEventKind::ModelTextDelta => {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    if let Some(probe) = self.probe.as_ref() {
                        probe.record_model_delta(delta.len());
                    }
                    let should_notify = self
                        .model_deltas
                        .lock()
                        .expect("模型增量缓冲区锁不应中毒")
                        .push(delta);
                    if should_notify {
                        let _ = self.tx.send(UiEvent::ModelTextReady);
                    }
                }
            }
            AgentEventKind::ModelThinkingDelta => {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    let should_notify = self
                        .thinking_deltas
                        .lock()
                        .expect("推理摘要增量缓冲区锁不应中毒")
                        .push(delta);
                    if should_notify {
                        let _ = self.tx.send(UiEvent::ModelThinkingReady);
                    }
                }
            }
            AgentEventKind::ToolStarted => {
                let call = serde_json::from_value::<ToolCall>(event.payload.clone())?;
                let _ = self.tx.send(UiEvent::ToolStarted(call));
            }
            AgentEventKind::ToolOutputDelta => {
                if let Some(probe) = self.probe.as_ref() {
                    probe.record_tool_delta();
                }
                let output = serde_json::from_value::<ToolOutputDelta>(event.payload.clone())?;
                let should_notify = self
                    .tool_output
                    .lock()
                    .expect("工具输出缓冲区锁不应中毒")
                    .push(output);
                if should_notify {
                    let _ = self.tx.send(UiEvent::ToolOutputReady);
                }
            }
            AgentEventKind::ToolFinished => {
                let result = serde_json::from_value::<ToolResult>(event.payload.clone())?;
                let _ = self.tx.send(UiEvent::ToolFinished(result));
            }
            AgentEventKind::ToolSkipped => {
                let call = serde_json::from_value::<ToolCall>(
                    event.payload.get("call").cloned().unwrap_or(Value::Null),
                )?;
                let reason = event
                    .payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("工具跳过事件缺少原因"))?
                    .to_string();
                let _ = self.tx.send(UiEvent::ToolSkipped { call, reason });
            }
            AgentEventKind::SteeringInjected => {
                let _ = self.tx.send(UiEvent::SteeringInjected);
            }
            AgentEventKind::FollowUpInjected => {
                let _ = self.tx.send(UiEvent::FollowUpInjected);
            }
            AgentEventKind::Extension => {
                #[cfg(feature = "plugins")]
                if let Some((plugin_id, text, color)) = plugin_footer_event(&event.payload)? {
                    let _ = self.tx.send(UiEvent::PluginFooterStatus {
                        plugin_id,
                        text,
                        color,
                    });
                    return Ok(());
                }
                #[cfg(feature = "plugins")]
                if event.payload.get("name").and_then(Value::as_str) == Some(UI_NAVIGATION_EVENT) {
                    let plugin_id = event
                        .payload
                        .pointer("/source/id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("视图导航事件缺少可信插件来源"))?;
                    let request = serde_json::from_value::<UiNavigationRequest>(
                        event.payload.get("data").cloned().unwrap_or(Value::Null),
                    )?;
                    let _ = self.tx.send(UiEvent::ViewNavigation {
                        plugin_id: plugin_id.to_string(),
                        request,
                    });
                    return Ok(());
                }
                let presentation = event.payload.get("presentation");
                // Host 显式写入 null 表示仅供观察，不再把协议名当第二条可见消息。
                // 未携带该字段的旧事件仍保留名称回退，与插件 UI 事件消费保持一致。
                if presentation.is_some_and(Value::is_null) {
                    return Ok(());
                }
                let target = presentation
                    .and_then(|value| value.get("target"))
                    .and_then(Value::as_str)
                    .unwrap_or("main_event_list");
                if target == "main_event_list" {
                    let text = presentation
                        .and_then(|value| value.get("text"))
                        .and_then(Value::as_str)
                        .or_else(|| event.payload.pointer("/data/text").and_then(Value::as_str))
                        .or_else(|| event.payload.get("name").and_then(Value::as_str))
                        .unwrap_or("扩展事件")
                        .to_string();
                    let divider = presentation
                        .and_then(|value| value.get("variant"))
                        .and_then(Value::as_str)
                        == Some("divider");
                    let color = match presentation
                        .and_then(|value| value.get("tone"))
                        .and_then(Value::as_str)
                    {
                        Some("success") => COLOR_SUCCESS,
                        Some("warning") => COLOR_WARNING,
                        Some("error") => COLOR_DANGER,
                        Some("muted") => COLOR_MUTED,
                        _ => COLOR_USER,
                    };
                    let _ = self.tx.send(UiEvent::Extension {
                        text,
                        color,
                        divider,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// 解析模型响应中的用量分项；全部字段缺失时返回 `None`。
fn model_usage(payload: &Value) -> Option<TokenUsage> {
    let usage = payload.get("usage")?;
    let usage: TokenUsage = serde_json::from_value(usage.clone()).ok()?;
    (!usage.is_empty()).then_some(usage)
}
