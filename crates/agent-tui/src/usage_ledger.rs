//! 主会话逐次模型请求的用量台账。
//!
//! 每次模型响应追加一行 JSON：服务商回报的用量分项、请求前的上下文估算，以及与上一次请求
//! 相比前缀从哪条消息开始变化；扩展事件各自另起一行。台账只依赖通用事件字段，不解析任何
//! 扩展事件的业务含义：宿主原生事件（没有可信 `source`）保留名称与有界数据，插件事件只保留
//! 名称与来源，含义交给离线分析脚本解释。
//!
//! 台账只挂在主 Agent 的事件出口上，队员请求不会进入，与 `/context` 的统计口径一致。它是
//! 诊断旁路：写入失败只通知一次界面并停止本进程的后续写入，绝不中断用户的 Agent 运行。

use crate::event::UiEvent;
use agent_core::event::{AgentEvent, AgentEventKind, EventSink};
use agent_session::SessionId;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc;

/// 台账行格式版本；字段只做加法，删除或改变含义时递增。
const LEDGER_SCHEMA_VERSION: u64 = 1;
/// 单条宿主原生扩展事件数据写入台账的最大序列化字节数，超出时只记录字节数。
const MAX_EXTENSION_DATA_BYTES: usize = 4 * 1024;

/// Core 在 `ModelRequest` 事件中发布的前缀指纹。
#[derive(Debug, Clone, Deserialize)]
struct RequestDigests {
    settings: String,
    system: String,
    tools: String,
    messages: Vec<String>,
    roles: String,
}

/// 台账的可变状态；同一时刻只服务一个会话。
#[derive(Default)]
struct LedgerState {
    /// 当前写入的会话；为 `None` 时不记录任何事件。
    session_id: Option<SessionId>,
    /// 当前会话上一次请求的前缀指纹，作为下一次请求的比较基线。
    previous: Option<RequestDigests>,
    /// 已发出请求、尚未收到响应的记录，键为 `(run_id, step)`；重试覆盖同一步的旧记录。
    pending: HashMap<(String, usize), Map<String, Value>>,
}

/// 把主 Agent 事件转换为按会话分文件的用量台账。
pub(crate) struct UsageLedger {
    /// 台账目录，每个会话一个 `<会话 ID>.jsonl` 文件。
    directory: PathBuf,
    state: Mutex<LedgerState>,
    /// 保证同一进程内的行按事件顺序落盘。
    write_order: Arc<tokio::sync::Mutex<()>>,
    /// 首次写入失败后置位，本进程不再尝试写入。
    disabled: AtomicBool,
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl UsageLedger {
    /// 创建写入 `directory` 的台账；目录在首次写入时创建。
    ///
    /// `tx` 只用于在首次写入失败时向界面发送一次提示。
    pub(crate) fn new(directory: PathBuf, tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            directory,
            state: Mutex::new(LedgerState::default()),
            write_order: Arc::new(tokio::sync::Mutex::new(())),
            disabled: AtomicBool::new(false),
            tx,
        }
    }

    /// 切换台账当前归属的会话。
    ///
    /// 必须在该会话的运行或上下文重载开始之前调用；切换到不同会话时清空比较基线与
    /// 待写记录，避免把上一个会话的请求当作前缀。
    pub(crate) fn set_session(&self, session_id: &SessionId) {
        let mut state = self.lock_state();
        if state.session_id.as_ref() == Some(session_id) {
            return;
        }
        *state = LedgerState {
            session_id: Some(session_id.clone()),
            ..LedgerState::default()
        };
    }

    /// 状态只存放可重建的诊断数据，锁中毒时继续使用内部值即可。
    fn lock_state(&self) -> MutexGuard<'_, LedgerState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// 根据事件更新内部状态，并返回需要落盘的会话与行；无需写入时返回 `None`。
    fn ledger_line(&self, event: &AgentEvent) -> Option<(SessionId, Value)> {
        let mut state = self.lock_state();
        let session_id = state.session_id.clone()?;
        let line = match event.kind {
            AgentEventKind::ModelRequest => {
                // 主会话的运行串行执行：新请求到达时，其他运行遗留的待写记录不会再有响应。
                state
                    .pending
                    .retain(|(run_id, _), _| run_id == &event.run_id);
                let record = request_record(event, &mut state.previous);
                state
                    .pending
                    .insert((event.run_id.clone(), event.step), record);
                return None;
            }
            AgentEventKind::ModelResponse => {
                let request = state
                    .pending
                    .remove(&(event.run_id.clone(), event.step))
                    .unwrap_or_default();
                response_line(event, &session_id, request)
            }
            AgentEventKind::Extension => extension_line(event, &session_id)?,
            _ => return None,
        };
        Some((session_id, line))
    }

    /// 在阻塞线程中把一行追加到会话台账，返回前完成写入。
    async fn append(&self, session_id: &SessionId, line: &Value) -> Result<()> {
        let path = self.directory.join(format!("{session_id}.jsonl"));
        let mut bytes = serde_json::to_vec(line).context("序列化用量台账行失败")?;
        bytes.push(b'\n');
        let guard = Arc::clone(&self.write_order).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            append_line(&path, &bytes)
        })
        .await
        .context("用量台账写入任务失败")?
    }
}

#[async_trait]
impl EventSink for UsageLedger {
    async fn record(&self, event: &AgentEvent) -> Result<()> {
        if self.disabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some((session_id, line)) = self.ledger_line(event) else {
            return Ok(());
        };
        if let Err(error) = self.append(&session_id, &line).await {
            if !self.disabled.swap(true, Ordering::Relaxed) {
                let _ = self
                    .tx
                    .send(UiEvent::UsageLedgerFailed(format!("{error:#}")));
            }
        }
        Ok(())
    }
}

/// 从 `ModelRequest` 事件提取请求侧字段，并以 `previous` 为基线计算前缀变化。
fn request_record(event: &AgentEvent, previous: &mut Option<RequestDigests>) -> Map<String, Value> {
    let payload = &event.payload;
    let digests = payload
        .get("digests")
        .cloned()
        .and_then(|value| serde_json::from_value::<RequestDigests>(value).ok());
    let prefix = digests.as_ref().map_or(Value::Null, |current| {
        prefix_change(previous.as_ref(), current)
    });
    if let Some(current) = digests {
        *previous = Some(current);
    }
    let mut record = Map::new();
    record.insert("ts_ms".into(), json!(event.timestamp_ms));
    for key in [
        "retry_attempt",
        "provider",
        "model",
        "message_count",
        "tool_count",
    ] {
        record.insert(key.into(), field(payload, key));
    }
    record.insert("context".into(), context_summary(payload.get("context")));
    record.insert("prefix".into(), prefix);
    record
}

/// 合并请求侧记录与响应用量，生成一条 `request` 行；服务商未回报的用量分项保留为 `null`。
fn response_line(event: &AgentEvent, session_id: &SessionId, request: Map<String, Value>) -> Value {
    let request_field = |key: &str| request.get(key).cloned().unwrap_or(Value::Null);
    json!({
        "schema": LEDGER_SCHEMA_VERSION,
        "type": "request",
        "session_id": session_id.as_str(),
        "run_id": event.run_id,
        "step": event.step,
        "retry_attempt": request_field("retry_attempt"),
        "ts_ms": request_field("ts_ms"),
        "response_ts_ms": event.timestamp_ms,
        "provider": request_field("provider"),
        "model": request_field("model"),
        "usage": field(&event.payload, "usage"),
        "finish_reason": field(&event.payload, "finish_reason"),
        "message_count": request_field("message_count"),
        "tool_count": request_field("tool_count"),
        "context": request_field("context"),
        "prefix": request_field("prefix"),
    })
}

/// 把扩展事件转换为 `extension` 行；没有名称的事件无法解释，直接跳过。
fn extension_line(event: &AgentEvent, session_id: &SessionId) -> Option<Value> {
    let payload = &event.payload;
    let name = payload.get("name").and_then(Value::as_str)?;
    let source = payload.pointer("/source/id").and_then(Value::as_str);
    // 插件数据由插件定义，可能很大且与用量无关，只保留名称与 Host 注入的可信来源。
    let data = match source {
        Some(_) => Value::Null,
        None => bounded_data(payload.get("data")),
    };
    Some(json!({
        "schema": LEDGER_SCHEMA_VERSION,
        "type": "extension",
        "session_id": session_id.as_str(),
        "run_id": event.run_id,
        "step": event.step,
        "ts_ms": event.timestamp_ms,
        "name": name,
        "source": source,
        "data": data,
    }))
}

/// 与上一次请求逐条比较消息指纹，给出稳定前缀长度和第一处变化。
///
/// `rewritten` 为真表示上一次请求已发送的消息里有内容被改写或删除，服务商缓存只能命中
/// 到 `stable_messages` 之前；为假时本次请求只是在末尾追加。
fn prefix_change(previous: Option<&RequestDigests>, current: &RequestDigests) -> Value {
    let Some(previous) = previous else {
        return json!({
            "previous_messages": null,
            "messages": current.messages.len(),
        });
    };
    let stable = previous
        .messages
        .iter()
        .zip(&current.messages)
        .take_while(|(before, after)| before == after)
        .count();
    let rewritten = stable < previous.messages.len();
    json!({
        "previous_messages": previous.messages.len(),
        "messages": current.messages.len(),
        "stable_messages": stable,
        "rewritten": rewritten,
        "changed_role": role_at(&current.roles, stable).filter(|_| rewritten),
        "previous_role": role_at(&previous.roles, stable).filter(|_| rewritten),
        "settings_changed": previous.settings != current.settings,
        "system_changed": previous.system != current.system,
        "tools_changed": previous.tools != current.tools,
    })
}

/// 把 Core 的单字符角色编码还原为角色名；越界或未知编码返回 `None`。
fn role_at(roles: &str, index: usize) -> Option<&'static str> {
    match roles.chars().nth(index)? {
        's' => Some("system"),
        'd' => Some("developer"),
        'u' => Some("user"),
        'a' => Some("assistant"),
        't' => Some("tool"),
        _ => None,
    }
}

/// 汇总 Core 的上下文分类估算，额外给出总量便于直接比较。
fn context_summary(context: Option<&Value>) -> Value {
    let Some(context) = context else {
        return Value::Null;
    };
    let tokens = field(context, "tokens");
    let estimated_tokens = tokens.as_object().map_or(0, |categories| {
        categories.values().filter_map(Value::as_u64).sum::<u64>()
    });
    json!({
        "estimated_tokens": estimated_tokens,
        "tokens": tokens,
        "attachments": field(context, "attachments"),
    })
}

/// 原样保留不超过上限的数据，超出时只记录字节数。
fn bounded_data(data: Option<&Value>) -> Value {
    let Some(data) = data else {
        return Value::Null;
    };
    match serde_json::to_vec(data) {
        Ok(bytes) if bytes.len() <= MAX_EXTENSION_DATA_BYTES => data.clone(),
        Ok(bytes) => json!({ "omitted_bytes": bytes.len() }),
        Err(_) => Value::Null,
    }
}

/// 读取对象字段的副本；缺失时返回 `null`。
fn field(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

/// 追加一行到台账文件；目录不存在时创建，Unix 新文件权限为 0600。
fn append_line(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建用量台账目录失败：{}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("打开用量台账失败：{}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("写入用量台账失败：{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造带前缀指纹的请求事件。
    fn request_event(run_id: &str, step: usize, messages: &[&str], roles: &str) -> AgentEvent {
        AgentEvent::new(
            run_id,
            AgentEventKind::ModelRequest,
            step,
            json!({
                "provider": "main",
                "model": "test-model",
                "message_count": messages.len(),
                "tool_count": 1,
                "context": {"tokens": {"system": 10, "results": 30}, "attachments": 0},
                "digests": {
                    "settings": "s", "system": "y", "tools": "t",
                    "messages": messages, "roles": roles,
                },
                "retry_attempt": 0,
            }),
        )
    }

    /// 台账按会话落盘：缺失的用量保持 `null`，改写历史时给出第一处变化，
    /// 原生扩展事件保留数据而插件事件只保留来源。
    #[tokio::test]
    async fn ledger_records_usage_prefix_and_extensions() {
        let directory =
            std::env::temp_dir().join(format!("lucia-usage-ledger-{}", SessionId::generate()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let ledger = UsageLedger::new(directory.clone(), tx);
        let session = SessionId::new("ledger-session").unwrap();
        ledger.set_session(&session);

        let usage = json!({"input_tokens": 120, "output_tokens": 8, "cache_read_tokens": 4608,
            "cache_write_tokens": null, "total_tokens": null});
        ledger
            .record(&request_event("run-1", 0, &["p1", "u1"], "du"))
            .await
            .unwrap();
        ledger
            .record(&AgentEvent::new(
                "run-1",
                AgentEventKind::ModelResponse,
                0,
                json!({"finish_reason": "stop", "usage": usage}),
            ))
            .await
            .unwrap();
        ledger
            .record(&AgentEvent::new(
                "run-2",
                AgentEventKind::Extension,
                0,
                json!({"name": "native.event", "data": {"before": 9, "after": 3}}),
            ))
            .await
            .unwrap();
        ledger
            .record(&AgentEvent::new(
                "run-2",
                AgentEventKind::Extension,
                0,
                json!({"name": "plugin.event", "data": {"secret": "x"}, "source": {"id": "demo"}}),
            ))
            .await
            .unwrap();
        ledger
            .record(&request_event(
                "run-2",
                0,
                &["p2", "u1", "a1", "u2"],
                "duau",
            ))
            .await
            .unwrap();
        ledger
            .record(&AgentEvent::new(
                "run-2",
                AgentEventKind::ModelResponse,
                0,
                json!({"finish_reason": "stop", "usage": null}),
            ))
            .await
            .unwrap();

        let lines = std::fs::read_to_string(directory.join("ledger-session.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["type"], "request");
        assert_eq!(lines[0]["usage"]["cache_read_tokens"], 4608);
        assert!(lines[0]["usage"]["cache_write_tokens"].is_null());
        assert!(lines[0]["prefix"]["previous_messages"].is_null());
        assert_eq!(lines[0]["context"]["estimated_tokens"], 40);
        assert_eq!(lines[1]["data"]["after"], 3);
        assert_eq!(lines[2]["source"], "demo");
        assert!(lines[2]["data"].is_null());
        assert_eq!(lines[3]["prefix"]["stable_messages"], 0);
        assert_eq!(lines[3]["prefix"]["rewritten"], true);
        assert_eq!(lines[3]["prefix"]["changed_role"], "developer");
        assert!(lines[3]["usage"].is_null());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
