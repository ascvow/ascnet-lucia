//! 模型重连的物理请求边界：间隔、配额、取消与工具副作用不重放。

use super::*;
use crate::model::TransientModelError;
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

enum Attempt {
    Retryable,
    Permanent,
    Interrupted,
    Response(Box<ModelResponse>),
}

struct RetryModel {
    attempts: Mutex<VecDeque<Attempt>>,
    requests: Mutex<Vec<(Instant, ModelRequest)>>,
}

impl RetryModel {
    fn new(attempts: Vec<Attempt>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(attempts.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn next(&self, request: ModelRequest) -> Attempt {
        self.requests
            .lock()
            .unwrap()
            .push((Instant::now(), request));
        self.attempts
            .lock()
            .unwrap()
            .pop_front()
            .expect("不能发出脚本之外的请求")
    }
}

#[async_trait]
impl ChatModel for RetryModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        match self.next(request) {
            Attempt::Retryable => Err(TransientModelError::new("connection reset").into()),
            Attempt::Permanent => Err(anyhow!("permission denied")),
            Attempt::Response(response) => Ok(*response),
            Attempt::Interrupted => panic!("非流式请求不能返回中断流"),
        }
    }

    async fn stream(&self, request: ModelRequest) -> ModelEventStream {
        let (sender, stream) = ModelEventStream::channel();
        match self.next(request) {
            Attempt::Retryable => sender.fail(TransientModelError::new("connection reset").into()),
            Attempt::Permanent => sender.error("permission denied"),
            Attempt::Interrupted => {
                sender.send(ModelStreamEvent::TextDelta {
                    index: 0,
                    delta: "未完成的旧文本".into(),
                });
                sender.send(ModelStreamEvent::ThinkingDelta {
                    index: 1,
                    delta: "未完成的旧思考".into(),
                });
                // EOF 未带终态时必须重连，不能把增量误当完整响应。
            }
            Attempt::Response(response) => sender.done(*response),
        }
        stream
    }
}

impl ProviderAdapter for RetryModel {
    fn name(&self) -> &'static str {
        "retry-test"
    }
}

fn retry_agent(
    model: Arc<RetryModel>,
    stream: bool,
    max_retries: usize,
    interval_ms: u64,
) -> Agent {
    let gateway = ModelGateway::new();
    gateway.register("retry-test", model).unwrap();
    Agent::new(
        gateway,
        AgentOptions {
            stream,
            max_retries,
            retry_interval_ms: interval_ms,
            ..AgentOptions::default().with_model_route("retry-test", "test-model")
        },
    )
}

#[tokio::test]
async fn retry_success_waits_between_attempts_without_consuming_steps() {
    for stream in [false, true] {
        let model = RetryModel::new(vec![
            Attempt::Retryable,
            Attempt::Retryable,
            Attempt::Response(Box::new(ModelResponse::text("恢复成功"))),
        ]);
        let sink = Arc::new(InMemoryEventSink::new());
        let mut agent = retry_agent(model.clone(), stream, 5, 20).with_event_sink(sink.clone());
        agent.options.max_steps = 1;
        let run = agent.run("继续任务").await.unwrap();
        assert_eq!(run.final_text, "恢复成功");
        // 在异步读取事件之前释放同步锁；请求对比不需要跨 await 持有守卫。
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            for pair in requests.windows(2) {
                assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(20));
                assert_eq!(
                    serde_json::to_value(&pair[0].1).unwrap(),
                    serde_json::to_value(&pair[1].1).unwrap()
                );
            }
        }
        let events = sink.events().await;
        let attempts: Vec<_> = events
            .iter()
            .filter(|e| e.kind == AgentEventKind::ModelRequest)
            .collect();
        assert_eq!(attempts.len(), 3);
        for (index, event) in attempts.iter().enumerate() {
            assert_eq!(event.step, 0);
            assert_eq!(event.payload["retry_attempt"], index);
            assert_eq!(
                event.payload["retry_interval_ms"],
                if index == 0 { 0 } else { 20 }
            );
        }
    }
}

#[tokio::test]
async fn retry_limit_is_five_additional_attempts_and_zero_disables_it() {
    for stream in [false, true] {
        for max_retries in [0, 2, 5] {
            let model = RetryModel::new((0..=max_retries).map(|_| Attempt::Retryable).collect());
            let error = retry_agent(model.clone(), stream, max_retries, 0)
                .run("测试上限")
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains(&format!("已重试 {max_retries} 次")));
            assert_eq!(model.requests.lock().unwrap().len(), max_retries + 1);
        }
        let model = RetryModel::new(vec![Attempt::Permanent]);
        assert!(retry_agent(model.clone(), stream, 5, 0)
            .run("不可重试")
            .await
            .is_err());
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn interrupted_stream_discards_partial_text_and_never_replays_tools() {
    let call = ToolCall::new("retry-tool", "count_once", json!({}));
    let response = ModelResponse::tool_calls(vec![call]);
    let model = RetryModel::new(vec![
        Attempt::Response(Box::new(response)),
        Attempt::Interrupted,
        Attempt::Response(Box::new(ModelResponse::text("最终结果"))),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut tools = ToolRegistry::new();
    tools
        .register(JsonTool::new(
            ToolSpec::new("count_once", "计数", json!({"type":"object"})),
            move |_| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"ok":true}))
                }
            },
        ))
        .unwrap();
    let agent = retry_agent(model.clone(), true, 5, 0).with_tools(tools);
    let run = agent.run("运行工具后断线").await.unwrap();
    assert_eq!(run.final_text, "最终结果");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let messages = serde_json::to_string(run.session.messages()).unwrap();
    assert!(!messages.contains("未完成的旧"));
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        serde_json::to_value(&requests[1].1).unwrap(),
        serde_json::to_value(&requests[2].1).unwrap()
    );
}

struct RecordingGate {
    requests: AtomicUsize,
    allowed: usize,
    settlements: Mutex<Vec<Option<u64>>>,
}

impl ModelRequestGate for RecordingGate {
    fn before_request(&self, _: &ModelRequest) -> Result<()> {
        if self.requests.fetch_add(1, Ordering::SeqCst) >= self.allowed {
            return Err(anyhow!("request quota exhausted"));
        }
        Ok(())
    }
    fn settle_request(&self, _: usize, tokens: Option<u64>) {
        self.settlements.lock().unwrap().push(tokens);
    }
}

#[tokio::test]
async fn retries_reenter_trusted_gate_and_settle_each_attempt() {
    for allowed in [1, 2] {
        let model = RetryModel::new(vec![
            Attempt::Retryable,
            Attempt::Response(Box::new(ModelResponse {
                usage: Some(TokenUsage {
                    output_tokens: Some(7),
                    ..TokenUsage::default()
                }),
                ..ModelResponse::text("成功")
            })),
        ]);
        let gate = Arc::new(RecordingGate {
            requests: AtomicUsize::new(0),
            allowed,
            settlements: Mutex::new(Vec::new()),
        });
        let mut agent = retry_agent(model.clone(), true, 5, 0);
        agent.set_model_request_gate(gate.clone());
        assert_eq!(agent.run("配额检查").await.is_ok(), allowed == 2);
        assert_eq!(model.requests.lock().unwrap().len(), allowed);
        assert_eq!(gate.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            *gate.settlements.lock().unwrap(),
            if allowed == 1 {
                vec![None]
            } else {
                vec![None, Some(7)]
            }
        );
    }
}

#[tokio::test]
async fn cancellation_during_retry_delay_returns_without_another_request() {
    let model = RetryModel::new(vec![Attempt::Retryable]);
    let sink = Arc::new(InMemoryEventSink::new());
    let gate = Arc::new(RecordingGate {
        requests: AtomicUsize::new(0),
        allowed: 5,
        settlements: Mutex::new(Vec::new()),
    });
    let mut agent = retry_agent(model.clone(), true, 5, 60_000).with_event_sink(sink.clone());
    agent.set_model_request_gate(gate.clone());
    let agent = Arc::new(agent);
    let running = agent.clone();
    let task = tokio::spawn(async move { running.run("等待重连时取消").await });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if sink
                .events()
                .await
                .iter()
                .any(|event| event.payload["retry_attempt"] == 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent.control().cancel();
    let run = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(run.cancelled);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1);
    assert_eq!(*gate.settlements.lock().unwrap(), vec![None]);
}

/// 固定间隔仍受 Agent 墙钟上限约束，超时不得预支下一次请求。
#[tokio::test]
async fn retry_delay_respects_wall_clock_limit() {
    let model = RetryModel::new(vec![Attempt::Retryable]);
    let gate = Arc::new(RecordingGate {
        requests: AtomicUsize::new(0),
        allowed: 5,
        settlements: Mutex::new(Vec::new()),
    });
    let mut agent = retry_agent(model.clone(), true, 5, 60_000);
    agent.options.execution_policy.limits.wall_clock_ms = Some(30);
    agent.set_model_request_gate(gate.clone());
    let error = tokio::time::timeout(Duration::from_secs(1), agent.run("墙钟限制"))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("墙钟"));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1);
    assert_eq!(*gate.settlements.lock().unwrap(), vec![None]);
}

/// 一次运行中的多个模型步骤各有独立重试额度，不把临时失败累积成后续步骤的额度。
#[tokio::test]
async fn retry_budget_resets_for_each_model_step() {
    let model = RetryModel::new(vec![
        Attempt::Retryable,
        Attempt::Response(Box::new(ModelResponse::tool_calls(vec![ToolCall::new(
            "once",
            "count_once",
            json!({}),
        )]))),
        Attempt::Retryable,
        Attempt::Response(Box::new(ModelResponse::text("完成"))),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut tools = ToolRegistry::new();
    tools
        .register(JsonTool::new(
            ToolSpec::new("count_once", "计数", json!({"type":"object"})),
            move |_| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"ok":true}))
                }
            },
        ))
        .unwrap();
    let agent = retry_agent(model.clone(), true, 1, 0).with_tools(tools);
    assert_eq!(agent.run("逐步重连").await.unwrap().final_text, "完成");
    assert_eq!(model.requests.lock().unwrap().len(), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
