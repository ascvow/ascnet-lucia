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
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
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

    /// 模型输出的推理增量，仅发送给可信应用 EventSink 用于实时状态展示。
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
    /// 输出目录是否已确认创建。
    ///
    /// 逐 token 的增量事件会让 `record` 非常密集，每次都调用 `create_dir_all` 等于为
    /// 每个事件额外引入一串系统调用。首次成功后置位跳过；若目录随后被外部删除，
    /// 打开失败时会复位该标志，让下一次事件重新创建。
    directory_ready: Arc<AtomicBool>,
}

impl JsonlEventSink {
    /// Create a JSONL sink for a path.
    /// 为指定路径创建 JSONL sink。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
            directory_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return the output path.
    /// 返回输出路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 确认输出目录存在；成功后不再重复调用 `create_dir_all`。
    fn ensure_directory(&self) -> Result<()> {
        if self.directory_ready.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create event directory: {}", parent.display())
                })?;
            }
        }
        self.directory_ready.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// 调用者先持有顺序锁；每次重新打开以继续支持应用侧日志轮转。
    fn append_line(&self, line: &[u8]) -> Result<()> {
        self.ensure_directory()?;
        let mut file = match self.open_append() {
            Ok(file) => file,
            Err(_) => {
                self.directory_ready.store(false, Ordering::Relaxed);
                self.ensure_directory()?;
                self.open_append().with_context(|| {
                    format!("failed to open event jsonl: {}", self.path.display())
                })?
            }
        };
        file.write_all(line)
            .with_context(|| format!("failed to write event jsonl: {}", self.path.display()))
    }

    /// 以追加方式打开事件文件；文件不存在时创建。
    fn open_append(&self) -> std::io::Result<std::fs::File> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&self.path)
    }
}

#[async_trait]
impl EventSink for JsonlEventSink {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        // 先取得顺序锁再提交，避免 blocking 池积压无限写任务；取消等待不撤销已提交写入。
        let guard = Arc::clone(&self.lock).lock_owned().await;
        let mut line = serde_json::to_vec(event).context("failed to serialize agent event")?;
        line.push(b'\n');
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return self.append_line(&line);
        };
        let sink = self.clone();
        runtime
            .spawn_blocking(move || {
                let _guard = guard;
                sink.append_line(&line)
            })
            .await
            .context("event jsonl blocking task failed")?
    }
}

fn unix_timestamp_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    millis.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent, AgentEventKind, EventSink, JsonlEventSink};
    use serde_json::json;

    /// 写入完成才返回，轮转后不会继续写旧 inode。
    #[tokio::test]
    async fn jsonl_sink_preserves_order_and_rotation() {
        let root = std::env::temp_dir().join(format!("lucia-jsonl-{}", uuid::Uuid::new_v4()));
        let path = root.join("events.jsonl");
        let sink = JsonlEventSink::new(&path);
        for step in 0..3 {
            sink.record(&AgentEvent::new(
                "run",
                AgentEventKind::TurnStarted,
                step,
                json!({}),
            ))
            .await
            .unwrap();
        }
        let data = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            data.lines()
                .map(|line| serde_json::from_str::<AgentEvent>(line).unwrap().step)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        std::fs::rename(&path, root.join("old.jsonl")).unwrap();
        sink.record(&AgentEvent::new(
            "run",
            AgentEventKind::RunFinished,
            3,
            json!({}),
        ))
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 事件按 JSON Lines 追加；目录被外部删除后必须自动重建而不是永久失败。
    ///
    /// 目录创建被缓存成一次性动作，这里验证缓存不会把"目录消失"变成不可恢复的错误，
    /// 否则日志会在长时间运行中静默失效。
    #[tokio::test]
    async fn jsonl_sink_appends_and_recovers_directory() {
        let root = std::env::temp_dir().join(format!("lucia-event-sink-{}", uuid::Uuid::new_v4()));
        let path = root.join("nested").join("events.jsonl");
        let sink = JsonlEventSink::new(path.clone());

        let first = AgentEvent::new("run-1", AgentEventKind::TurnStarted, 0, json!({}));
        sink.record(&first)
            .await
            .expect("首次写入应成功并创建输出目录");

        std::fs::remove_dir_all(&root).expect("删除测试目录");

        let second = AgentEvent::new("run-1", AgentEventKind::TurnFinished, 0, json!({}));
        sink.record(&second)
            .await
            .expect("目录被删除后应重新创建并写入");

        let content = std::fs::read_to_string(&path).expect("读取事件文件");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "重建后的文件只应包含删除之后写入的事件");
        let parsed: AgentEvent = serde_json::from_str(lines[0]).expect("每行必须是合法事件 JSON");
        assert_eq!(parsed.kind, AgentEventKind::TurnFinished);
        assert_eq!(parsed.run_id, "run-1");

        let _ = std::fs::remove_dir_all(&root);
    }
}
