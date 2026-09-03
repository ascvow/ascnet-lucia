//! 把 Agent 事件流转换为 TUI 通道事件的宿主 Sink。

use crate::event::UiEvent;
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
    text: String,
    notification_pending: bool,
}

impl ModelDeltaBuffer {
    /// 追加文本，并返回是否需要向 UI 发送新的就绪通知。
    fn push(&mut self, delta: &str) -> bool {
        self.text.push_str(delta);
        if self.notification_pending {
            false
        } else {
            self.notification_pending = true;
            true
        }
    }

    /// 取出当前全部文本并允许生产者发送下一次就绪通知。
    pub(crate) fn take(&mut self) -> String {
        self.notification_pending = false;
        std::mem::take(&mut self.text)
    }
}

/// 将 Agent 事件转换为 TUI 事件；高频模型文本通过共享缓冲区合并。
pub(crate) struct ChannelEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
    model_deltas: Arc<std::sync::Mutex<ModelDeltaBuffer>>,
    thinking_deltas: Arc<std::sync::Mutex<ModelDeltaBuffer>>,
}

impl ChannelEventSink {
    /// 创建事件 sink，并返回供 UI 分别消费回答文本和推理摘要的共享缓冲区。
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<UiEvent>,
    ) -> (
        Self,
        Arc<std::sync::Mutex<ModelDeltaBuffer>>,
        Arc<std::sync::Mutex<ModelDeltaBuffer>>,
    ) {
        let model_deltas = Arc::new(std::sync::Mutex::new(ModelDeltaBuffer::default()));
        let thinking_deltas = Arc::new(std::sync::Mutex::new(ModelDeltaBuffer::default()));
        (
            Self {
                tx,
                model_deltas: model_deltas.clone(),
                thinking_deltas: thinking_deltas.clone(),
            },
            model_deltas,
            thinking_deltas,
        )
    }
}

#[async_trait]
impl EventSink for ChannelEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        match event.kind {
            AgentEventKind::ModelRequest => {
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
                let output = serde_json::from_value::<ToolOutputDelta>(event.payload.clone())?;
                let _ = self.tx.send(UiEvent::ToolOutputDelta(output));
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
