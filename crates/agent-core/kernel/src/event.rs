//! Agent events and event sinks.
//! Agent 事件与事件保存接口。
//!
//! Core emits structured events but does not persist configuration or history by itself.
//! Callers can attach an [`EventSink`] to save JSONL, collect events in memory, or forward them
//! to an external system.
//!
//! core 会产出结构化事件，但不会自己持久化配置或历史。
//! 调用方可以挂载 [`EventSink`]，把事件保存为 JSONL、收集到内存，或转发到外部系统。

use crate::model::{ProviderBilling, TokenUsage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

/// Events emitted by the core.
/// core 发出的事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    /// Unique event id.
    /// 唯一事件 ID。
    pub id: String,

    /// Stable id shared by all events in one agent run.
    /// 同一次 agent run 内所有事件共享的稳定 ID。
    pub run_id: String,

    /// Unix timestamp in milliseconds.
    /// Unix 毫秒时间戳。
    pub timestamp_ms: u64,

    /// Event kind.
    /// 事件类型。
    pub kind: AgentEventKind,

    /// ReAct step index. Run-level events use the current or final step.
    /// ReAct 步数索引。run 级事件使用当前或最终 step。
    pub step: usize,

    /// Provider-neutral structured payload.
    /// 与服务商无关的结构化载荷。
    pub payload: Value,
}

impl AgentEvent {
    /// Create an event for a run.
    /// 为一次 run 创建事件。
    pub fn new(
        run_id: impl Into<String>,
        kind: AgentEventKind,
        step: usize,
        payload: Value,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.into(),
            timestamp_ms: unix_timestamp_ms(),
            kind,
            step,
            payload,
        }
    }
}

/// Event kinds.
/// 事件类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    /// 一次 Agent run 已创建，尚未开始首轮模型请求。
    RunStarted,

    /// 通用扩展发布的结构化事件。
    Extension,

    /// 一轮（一次模型调用 + 其工具执行）开始。
    TurnStarted,

    /// 即将把 provider-neutral 请求发送给模型网关。
    ModelRequest,

    /// 模型输出的文本增量，可用于实时界面渲染。
    ModelTextDelta,

    /// 模型输出的推理增量，可用于实时状态展示。
    ModelThinkingDelta,

    /// 模型已返回完整响应；流式增量事件会在它之前发布。
    ModelResponse,

    /// Billing data event emitted when a provider returns usage or cost fields.
    /// 当服务商返回 usage 或费用字段时发出的计费数据事件。
    BillingUsage,

    /// 工具调用已通过前置检查并开始执行。
    ToolStarted,
    /// 工具运行期间产生增量输出；载荷为共享的 `ToolOutputDelta`。
    ToolOutputDelta,
    /// 工具执行结束，载荷包含成功结果或结构化错误。
    ToolFinished,

    /// 工具因 steering 消息插入而被跳过。
    ToolSkipped,

    /// 一轮结束。
    TurnFinished,

    /// steering 消息被注入会话。
    SteeringInjected,

    /// follow-up 消息被注入会话。
    FollowUpInjected,

    /// Agent run 正常结束并产生最终文本。
    RunFinished,
    /// ReAct 循环达到最大步数，未继续请求模型。
    StepLimitReached,
}

/// Billing data payload for event consumers.
/// 供事件消费者使用的计费数据载荷。
///
/// This payload is based only on provider-returned fields. The core does not keep
/// a local price table and does not estimate costs.
/// 该载荷只基于服务商接口返回的字段。core 不维护本地价格表，也不估算费用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BillingUsage {
    /// Logical provider selected by the caller.
    /// 调用方选择的逻辑服务商名称。
    pub provider: String,

    /// Model id used for this request.
    /// 本次请求使用的模型 ID。
    pub model: String,

    /// Token usage returned by the provider, if present.
    /// 服务商返回的 token 用量，如果存在则填充。
    pub usage: Option<TokenUsage>,

    /// Provider-reported billing or cost fields, if present.
    /// 服务商返回的计费或费用字段，如果存在则填充。
    pub provider_billing: Option<ProviderBilling>,
}

impl BillingUsage {
    /// Build a billing payload from provider-returned usage and billing data.
    /// 根据服务商返回的 usage 和 billing 数据创建计费载荷。
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        usage: Option<TokenUsage>,
        provider_billing: Option<ProviderBilling>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            usage,
            provider_billing,
        }
    }

    /// Return true when no usage or provider billing data exists.
    /// 当没有 usage 或 provider billing 数据时返回 true。
    pub fn is_empty(&self) -> bool {
        self.usage.is_none()
            && self
                .provider_billing
                .as_ref()
                .is_none_or(ProviderBilling::is_empty)
    }
}

/// Event persistence or forwarding interface.
/// 事件持久化或转发接口。
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Record one event.
    /// 记录一个事件。
    async fn record(&self, event: &AgentEvent) -> Result<()>;
}

/// Event sink that drops everything.
/// 丢弃所有事件的 sink。
#[derive(Debug, Clone, Default)]
pub struct NoopEventSink;

#[async_trait]
impl EventSink for NoopEventSink {
    async fn record(&self, _event: &AgentEvent) -> Result<()> {
        Ok(())
    }
}

/// Event sink that dispatches to multiple sinks.
/// 将事件分发给多个 sink 的组合 sink。
#[derive(Clone, Default)]
pub struct CompositeEventSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl CompositeEventSink {
    /// Create an empty composite sink.
    /// 创建空组合 sink。
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one child sink.
    /// 添加一个子 sink。
    pub fn push(&mut self, sink: Arc<dyn EventSink>) -> &mut Self {
        self.sinks.push(sink);
        self
    }

    /// Number of child sinks.
    /// 子 sink 数量。
    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    /// Whether no child sink is registered.
    /// 是否没有注册任何子 sink。
    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }
}

#[async_trait]
impl EventSink for CompositeEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        for sink in &self.sinks {
            sink.record(event).await?;
        }
        Ok(())
    }
}

/// In-memory event sink useful for tests or embedding.
/// 内存事件 sink，适合测试或嵌入式调用。
#[derive(Clone, Default)]
pub struct InMemoryEventSink {
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl InMemoryEventSink {
    /// Create an empty in-memory sink.
    /// 创建空内存 sink。
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of recorded events.
    /// 返回已记录事件的快照。
    pub async fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().await.clone()
    }

    /// Remove all recorded events.
    /// 清空所有已记录事件。
    pub async fn clear(&self) {
        self.events.lock().await.clear();
    }
}

#[async_trait]
impl EventSink for InMemoryEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }
}

/// JSON Lines event sink.
/// JSON Lines 事件 sink。
///
/// Each event is appended as one JSON object per line. This is intentionally simple and
/// caller-owned: rotate, upload, or index the file in your application layer.
///
/// 每个事件会作为一行 JSON 追加写入。这里故意保持简单且由调用方拥有：
/// 日志轮转、上传、索引应放在应用层完成。
#[derive(Clone)]
pub struct JsonlEventSink {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl JsonlEventSink {
    /// Create a JSONL sink for a path.
    /// 为指定路径创建 JSONL sink。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Return the output path.
    /// 返回输出路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl EventSink for JsonlEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        let _guard = self.lock.lock().await;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create event directory: {}", parent.display())
                })?;
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open event jsonl: {}", self.path.display()))?;
        let line = serde_json::to_string(event).context("failed to serialize agent event")?;
        writeln!(file, "{line}")
            .with_context(|| format!("failed to write event jsonl: {}", self.path.display()))?;
        Ok(())
    }
}

fn unix_timestamp_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    millis.min(u64::MAX as u128) as u64
}
