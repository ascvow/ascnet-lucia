use super::*;
use crate::{AgentExecutionResult, RuntimeRunFinalizer, RuntimeRunObservation, ToolAccess};
use agent_core::{
    Agent, AgentExtension, AgentOptions, ChatModel, ContextLoadRequest, ContextLoader,
    InMemoryEventSink, LoadedContext, ModelGateway, ModelRequest, ModelResponse, ProviderAdapter,
    TokenUsage,
};
use agent_tool::{JsonTool, ToolRegistry, ToolSpec};
use anyhow::Result;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Barrier;

/// 记录 Runtime 上下文、Core 事件和收敛终态的可信测试观察器。
struct RecordingRunObserver {
    contexts: Arc<Mutex<Vec<RuntimeRunContext>>>,
    terminations: Arc<Mutex<Vec<RuntimeRunTermination>>>,
    events: Arc<InMemoryEventSink>,
    fail_finish: bool,
}

/// 把一次 Runtime 终态写入测试观察器。
struct RecordingRunFinalizer {
    terminations: Arc<Mutex<Vec<RuntimeRunTermination>>>,
    fail_finish: bool,
}

#[async_trait]
impl RuntimeRunFinalizer for RecordingRunFinalizer {
    async fn finish(&self, termination: RuntimeRunTermination) -> RuntimeResult<()> {
        self.terminations
            .lock()
            .expect("观察终态锁不应中毒")
            .push(termination);
        if self.fail_finish {
            Err(AgentRuntimeError::RunObservation("模拟证据收敛失败".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl RuntimeRunObserver for RecordingRunObserver {
    async fn begin(&self, context: RuntimeRunContext) -> RuntimeResult<RuntimeRunObservation> {
        let run_id = format!("observed-{}", context.agent_id);
        self.contexts
            .lock()
            .expect("观察上下文锁不应中毒")
            .push(context);
        RuntimeRunObservation::new(
            run_id,
            self.events.clone(),
            Arc::new(RecordingRunFinalizer {
                terminations: Arc::clone(&self.terminations),
                fail_finish: self.fail_finish,
            }),
        )
    }
}

/// 返回固定文本的测试模型。
struct FixedModel;

#[async_trait]
impl ChatModel for FixedModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text("完成"))
    }
}

#[async_trait]
impl ProviderAdapter for FixedModel {
    fn name(&self) -> &'static str {
        "fixed"
    }
}

/// 记录 provider 调用次数与最终输出上限的测试模型。
struct RecordingModel {
    calls: Arc<AtomicUsize>,
    max_tokens: Arc<Mutex<Vec<Option<u32>>>>,
}

#[async_trait]
impl ChatModel for RecordingModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.max_tokens
            .lock()
            .expect("模型请求记录锁不应中毒")
            .push(request.max_tokens);
        Ok(ModelResponse::text("完成"))
    }
}

#[async_trait]
impl ProviderAdapter for RecordingModel {
    fn name(&self) -> &'static str {
        "recording"
    }
}

/// 每次调用都报告"实际用掉等于预留额度"的模型。
///
/// 结算语义下，只有实际用量占住预算，累计输出预算才会持续被消耗。并发准入测试要
/// 验证的是"预算只放行其中一次请求"，与调度顺序无关，因此模型必须如实报告用量。
struct BudgetConsumingModel {
    calls: Arc<AtomicUsize>,
    max_tokens: Arc<Mutex<Vec<Option<u32>>>>,
}

#[async_trait]
impl ChatModel for BudgetConsumingModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let limit = request.max_tokens;
        self.max_tokens
            .lock()
            .expect("模型请求记录锁不应中毒")
            .push(limit);
        let mut response = ModelResponse::text("完成");
        response.usage = Some(TokenUsage {
            output_tokens: limit.map(u64::from),
            ..TokenUsage::default()
        });
        Ok(response)
    }
}

#[async_trait]
impl ProviderAdapter for BudgetConsumingModel {
    fn name(&self) -> &'static str {
        "budget-consuming"
    }
}

/// 每次只报告少量实际输出的模型，用于验证预算按用量结算而不是按预留计量。
struct SmallOutputModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ChatModel for SmallOutputModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let mut response = ModelResponse::text("完成");
        response.usage = Some(TokenUsage {
            output_tokens: Some(5),
            ..TokenUsage::default()
        });
        Ok(response)
    }
}

#[async_trait]
impl ProviderAdapter for SmallOutputModel {
    fn name(&self) -> &'static str {
        "small-output"
    }
}

/// 每次都返回错误并记录调用次数的测试模型。
struct FailingModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ChatModel for FailingModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(anyhow::anyhow!("模拟 provider 失败"))
    }
}

#[async_trait]
impl ProviderAdapter for FailingModel {
    fn name(&self) -> &'static str {
        "failing"
    }
}

/// 让两个 Agent 在 provider 门禁前同时完成上下文加载，用于验证并发原子预留。
struct BarrierContextLoader {
    barrier: Arc<Barrier>,
}

#[async_trait]
impl ContextLoader for BarrierContextLoader {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        self.barrier.wait().await;
        Ok(LoadedContext::new(request.system, request.messages))
    }
}

/// 等待通知的测试模型，用于验证取消语义。
struct BlockingModel {
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
}

/// 记录每个 run 扩展清理次数，用于验证 Runtime 外部取消不会丢弃生命周期收尾。
struct FinishRecordingExtension {
    finished_runs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl AgentExtension for FinishRecordingExtension {
    async fn finish_run(&self, run_id: &str) -> Result<()> {
        self.finished_runs
            .lock()
            .expect("扩展清理记录锁不应中毒")
            .push(run_id.to_string());
        Ok(())
    }
}

/// 首轮等待交互、第二轮验证 steering 已进入模型上下文的测试模型。
struct InteractiveModel {
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    saw_message: Arc<AtomicBool>,
}

#[async_trait]
impl ChatModel for InteractiveModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            self.entered.store(true, Ordering::Release);
            self.release.notified().await;
            return Ok(ModelResponse::text("首轮完成"));
        }
        self.saw_message.store(
            request
                .messages
                .iter()
                .any(|message| message.text_content().contains("实时补充")),
            Ordering::Release,
        );
        Ok(ModelResponse::text("互动完成"))
    }
}

#[async_trait]
impl ProviderAdapter for InteractiveModel {
    fn name(&self) -> &'static str {
        "interactive"
    }
}

#[async_trait]
impl ChatModel for BlockingModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.entered.store(true, Ordering::Release);
        self.release.notified().await;
        Ok(ModelResponse::text("不应完成"))
    }
}

#[async_trait]
impl ProviderAdapter for BlockingModel {
    fn name(&self) -> &'static str {
        "blocking"
    }
}

/// 由测试信号逐次放行的模型，用于验证全局并发上限。
struct ConcurrencyModel {
    current: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl ChatModel for ConcurrencyModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let current = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        self.maximum.fetch_max(current, Ordering::AcqRel);
        let permit = self.release.acquire().await.expect("测试信号不应关闭");
        permit.forget();
        self.current.fetch_sub(1, Ordering::AcqRel);
        Ok(ModelResponse::text("完成"))
    }
}

#[async_trait]
impl ProviderAdapter for ConcurrencyModel {
    fn name(&self) -> &'static str {
        "concurrency"
    }
}

/// 使用指定模型和工具名称构造测试模板。
fn template(adapter: Arc<dyn ProviderAdapter>, provider: &str, tools: &[&str]) -> AgentTemplate {
    let gateway = ModelGateway::new();
    gateway
        .register(provider, adapter)
        .expect("测试模型应成功注册");
    let mut registry = ToolRegistry::new();
    for name in tools {
        registry
            .register(JsonTool::new(
                ToolSpec::new(*name, "测试工具", ToolSpec::empty_object_schema()),
                |_| async { Ok(json!({"ok": true})) },
            ))
            .expect("测试工具应成功注册");
    }
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route(provider, "test-model"),
    )
    .with_tools(registry);
    AgentTemplate::from_agent(&agent)
}

/// allowlist 必须同时收缩有效权限和模型可见工具。
#[tokio::test]
async fn derivation_filters_tools_and_cannot_expand_parent_permission() {
    let template = template(Arc::new(FixedModel), "fixed", &["read", "write"]);
    let parent = AgentPermissions {
        tools: ToolAccess::allowlist(["read"]),
    };
    let config = AgentDeriveConfig {
        permissions: AgentPermissions {
            tools: ToolAccess::All,
        },
        ..AgentDeriveConfig::default()
    };
    let (agent, effective) = template
        .instantiate(&parent, &config)
        .expect("派生 Agent 应成功");

    assert_eq!(effective, parent);
    let specs = agent.tool_specs().await.expect("读取工具定义应成功");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "read");
}

/// 派生 API 应立即返回，并提供完整父子谱系和成功终态。
#[tokio::test]
async fn spawn_returns_handle_and_reaches_success_terminal_state() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("执行任务"))
        .await
        .expect("派生 Agent");

    assert_eq!(child.lineage.parent.as_ref(), Some(&root.id));
    assert_eq!(child.lineage.root, root.id);
    assert_eq!(child.lineage.depth, 1);
    let outcome = api.wait(&child.id).await.expect("等待子 Agent");
    assert!(matches!(
        outcome,
        AgentOutcome::Succeeded {
            result: AgentExecutionResult { final_text, .. }
        } if final_text == "完成"
    ));
    assert!(!api.cancel(&child.id).await.expect("重复终态操作应成功"));
}

/// Host 观察器必须在 Core 启动前固定 Run ID，并接收可信身份、事件和完成终态。
#[tokio::test]
async fn trusted_run_observer_binds_runtime_execution() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let terminations = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(InMemoryEventSink::new());
    let runtime = AgentRuntime::new_with_run_observer(
        RuntimeLimits::default(),
        Arc::new(RecordingRunObserver {
            contexts: Arc::clone(&contexts),
            terminations: Arc::clone(&terminations),
            events: Arc::clone(&events),
            fail_finish: false,
        }),
    )
    .expect("创建带观察器的 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("执行证据任务"))
        .await
        .expect("派生 Agent");

    assert!(matches!(
        api.wait(&child.id).await.expect("等待子 Agent"),
        AgentOutcome::Succeeded { .. }
    ));
    {
        let contexts = contexts.lock().expect("观察上下文锁不应中毒");
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].agent_id, child.id);
        assert_eq!(contexts[0].lineage, child.lineage);
    }
    assert_eq!(
        *terminations.lock().expect("观察终态锁不应中毒"),
        vec![RuntimeRunTermination::Completed]
    );
    let recorded = events.events().await;
    assert!(recorded
        .iter()
        .any(|event| event.kind == agent_core::AgentEventKind::RunStarted));
    assert!(recorded
        .iter()
        .any(|event| event.kind == agent_core::AgentEventKind::RunFinished));
    assert!(recorded
        .iter()
        .all(|event| event.run_id == format!("observed-{}", child.id)));
}

/// 证据收敛失败必须把 Runtime 运行降为失败，不能保留可续跑的成功 Session。
#[tokio::test]
async fn run_observation_failure_rejects_success_terminal_state() {
    let runtime = AgentRuntime::new_with_run_observer(
        RuntimeLimits::default(),
        Arc::new(RecordingRunObserver {
            contexts: Arc::new(Mutex::new(Vec::new())),
            terminations: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(InMemoryEventSink::new()),
            fail_finish: true,
        }),
    )
    .expect("创建带失败观察器的 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("执行失败证据任务"))
        .await
        .expect("派生 Agent");

    assert!(matches!(
        api.wait(&child.id).await.expect("等待子 Agent"),
        AgentOutcome::Failed { error } if error.contains("模拟证据收敛失败")
    ));
    assert!(matches!(
        api.continue_agent(&child.id, "不得续跑".into()).await,
        Err(AgentRuntimeError::SessionUnavailable(_))
    ));
}

/// principal 撤销必须中断阻塞中的 Core，但要等待可信观察器完成取消证据收敛后再返回。
#[tokio::test]
async fn revoke_principal_waits_for_observed_run_finalization() {
    let entered = Arc::new(AtomicBool::new(false));
    let terminations = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(InMemoryEventSink::new());
    let runtime = AgentRuntime::new_with_run_observer(
        RuntimeLimits::default(),
        Arc::new(RecordingRunObserver {
            contexts: Arc::new(Mutex::new(Vec::new())),
            terminations: Arc::clone(&terminations),
            events: Arc::clone(&events),
            fail_finish: false,
        }),
    )
    .expect("创建带观察器的 Runtime");
    let principal =
        RuntimePrincipal::new("component:test:observed-revoke").expect("创建撤销测试 principal");
    let root = runtime
        .attach_root_for(
            principal.clone(),
            template(
                Arc::new(BlockingModel {
                    entered: Arc::clone(&entered),
                    release: Arc::new(Notify::new()),
                }),
                "blocking",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载撤销测试根 Agent");
    let api = runtime
        .api_for(principal.clone(), &root.id)
        .await
        .expect("绑定撤销测试 API");
    let child = api
        .spawn(AgentSpawnRequest::new("阻塞直到 principal 撤销"))
        .await
        .expect("派生阻塞 Agent");

    for _ in 0..100 {
        if entered.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(entered.load(Ordering::Acquire));
    assert_eq!(runtime.revoke_principal(&principal).await, 2);
    assert_eq!(
        *terminations.lock().expect("观察终态锁不应中毒"),
        vec![RuntimeRunTermination::Cancelled]
    );
    let recorded = events.events().await;
    assert!(recorded
        .iter()
        .any(|event| event.kind == agent_core::AgentEventKind::RunStarted));
    assert!(recorded
        .iter()
        .all(|event| event.run_id == format!("observed-{}", child.id)));
}

/// 成功终态应保留私有会话，并允许有权 controller 创建权限不扩大的后续运行。
#[tokio::test]
async fn continue_agent_reuses_private_session_and_preserves_permissions() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let permissions = AgentPermissions {
        tools: ToolAccess::allowlist(["read"]),
    };
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &["read", "write"]),
            permissions.clone(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("首次任务"))
        .await
        .expect("派生 Agent");
    api.wait(&child.id).await.expect("等待首次运行");

    let continued = api
        .continue_agent(&child.id, "后续任务".to_string())
        .await
        .expect("创建后续运行");
    // 续跑复用同一身份与谱系：队员是长期存在的 Agent，不是每次派活都新建节点。
    assert_eq!(continued.id, child.id);
    assert_eq!(continued.lineage, child.lineage);
    let snapshot = api.status(&continued.id).await.expect("查询后续运行");
    assert_eq!(snapshot.permissions, permissions);
    assert!(matches!(
        api.wait(&continued.id).await.expect("等待后续运行"),
        AgentOutcome::Succeeded { .. }
    ));
}

/// 队员可以一直接活：续跑不登记新 Agent，也不受派生配额限制。
///
/// 队长反复派活时，成员身份与谱系必须保持不变；登记数量只随真实身份增长，
/// 否则长会话会在某个不可见的时间点开始拒绝新任务。
#[tokio::test]
async fn continuation_reuses_identity_without_consuming_agent_slots() {
    let runtime = AgentRuntime::new(RuntimeLimits {
        // 只允许 controller 与一个成员：如果续跑登记新 Agent，第二轮就会失败。
        max_agents_per_principal: 2,
        max_children_per_agent: 1,
        max_depth: 1,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let member = api
        .spawn(AgentSpawnRequest::new("首次任务"))
        .await
        .expect("派生成员");
    api.wait(&member.id).await.expect("等待首次任务");

    let mut current = member.clone();
    for round in 0..32 {
        current = api
            .continue_agent(&current.id, format!("第 {round} 轮任务"))
            .await
            .expect("成员应能一直接活");
        assert_eq!(current.id, member.id, "续跑必须复用同一成员身份");
        assert_eq!(current.lineage, member.lineage, "续跑不得改变谱系");
        assert!(matches!(
            api.wait(&current.id).await.expect("等待后续任务"),
            AgentOutcome::Succeeded { .. }
        ));
    }
}

/// 订阅只能看到当前这次运行的已保留事件。
///
/// 续跑会清空上一轮的事件历史：否则每次派活后重新订阅都会重放旧事件，成员视图会
/// 不断出现重复条目。
#[tokio::test]
async fn continuation_event_history_is_scoped_to_current_run() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let member = api
        .spawn(AgentSpawnRequest::new("首次任务"))
        .await
        .expect("派生成员");
    api.wait(&member.id).await.expect("等待首次任务");

    let mut first_runs = Vec::new();
    let mut stream = api.subscribe(&member.id).await.expect("首次订阅");
    while let Some(event) = stream.next().await {
        if !first_runs.contains(&event.run_id) {
            first_runs.push(event.run_id.clone());
        }
    }
    assert_eq!(first_runs.len(), 1, "一次运行只应有一个 run_id");

    api.continue_agent(&member.id, "第二轮任务".to_string())
        .await
        .expect("续跑成员");
    api.wait(&member.id).await.expect("等待第二轮任务");

    let mut second_runs = Vec::new();
    let mut stream = api.subscribe(&member.id).await.expect("续跑后订阅");
    while let Some(event) = stream.next().await {
        if !second_runs.contains(&event.run_id) {
            second_runs.push(event.run_id.clone());
        }
    }
    assert_eq!(second_runs.len(), 1, "续跑后只应回放本轮事件");
    assert_ne!(
        second_runs[0], first_runs[0],
        "续跑必须开启新的 run，而不是重放上一轮事件"
    );
}

/// 会话续跑不能消耗派生深度，否则长期协作的队员在若干轮后会永久失去后续任务。
#[tokio::test]
async fn continue_agent_does_not_grow_derivation_depth() {
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_depth: 1,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let mut current = api
        .spawn(AgentSpawnRequest::new("首次任务"))
        .await
        .expect("派生 Agent");
    api.wait(&current.id).await.expect("等待首次任务");

    // 深度上限为 1 时仍应能连续续跑：续跑沿用深度，不再派生新的层级。
    for round in 0..8 {
        current = api
            .continue_agent(&current.id, format!("第 {round} 轮后续任务"))
            .await
            .expect("续跑不应受派生深度限制");
        assert_eq!(current.lineage.depth, 1, "续跑必须保持源节点深度");
        api.wait(&current.id).await.expect("等待后续运行");
    }
}

/// 深度和累计子节点限制必须在启动模型任务前生效。
#[tokio::test]
async fn topology_limits_reject_excess_children_and_depth() {
    let limits = RuntimeLimits {
        max_depth: 1,
        max_children_per_agent: 1,
        ..RuntimeLimits::default()
    };
    let runtime = AgentRuntime::new(limits).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let root_api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = root_api
        .spawn(AgentSpawnRequest::new("第一个"))
        .await
        .expect("第一个子节点应成功");
    let error = root_api
        .spawn(AgentSpawnRequest::new("第二个"))
        .await
        .expect_err("第二个子节点应被拒绝");
    assert!(matches!(
        error,
        AgentRuntimeError::MaxChildrenExceeded { limit: 1 }
    ));

    let child_api = runtime.api(&child.id).await.expect("绑定子 API");
    let error = child_api
        .spawn(AgentSpawnRequest::new("孙节点"))
        .await
        .expect_err("孙节点应超过深度限制");
    assert!(matches!(
        error,
        AgentRuntimeError::MaxDepthExceeded { limit: 1 }
    ));
}

/// 全局并发限制必须让额外任务保持排队，且每个任务仍使用独立 Agent。
#[tokio::test]
async fn concurrency_limit_keeps_excess_agents_queued() {
    let current = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_concurrent_agents: 1,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(ConcurrencyModel {
                    current: current.clone(),
                    maximum: maximum.clone(),
                    release: release.clone(),
                }),
                "concurrency",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = api
        .spawn(AgentSpawnRequest::new("一"))
        .await
        .expect("派生第一个");
    let second = api
        .spawn(AgentSpawnRequest::new("二"))
        .await
        .expect("派生第二个");

    for _ in 0..100 {
        if current.load(Ordering::Acquire) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(current.load(Ordering::Acquire), 1);
    let statuses = [
        api.status(&first.id).await.expect("读取第一个状态").status,
        api.status(&second.id).await.expect("读取第二个状态").status,
    ];
    assert!(statuses.contains(&AgentStatus::Running));
    assert!(statuses.contains(&AgentStatus::Queued));

    release.add_permits(2);
    api.wait(&first.id).await.expect("第一个应完成");
    api.wait(&second.id).await.expect("第二个应完成");
    assert_eq!(maximum.load(Ordering::Acquire), 1);
}

/// Provisioner 必须覆盖授权、独立 controller 创建和 principal 清理生命周期。
#[tokio::test]
async fn provisioner_grants_profiles_and_revoke_cleans_owned_agents() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let profile = AgentProfileId::new("default-agent").expect("创建 profile ID");
    runtime
        .register_profile(
            profile.clone(),
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("注册 profile");
    let principal =
        RuntimePrincipal::new("component:test:activation-1").expect("创建激活 principal");
    let provisioner: Arc<dyn AgentRuntimeProvisioner> = Arc::new(runtime.clone());

    let denied = provisioner.provision(principal.clone(), &profile).await;
    assert!(matches!(
        denied,
        Err(AgentRuntimeError::ProfileDenied { .. })
    ));
    provisioner
        .grant_profile(principal.clone(), &profile)
        .await
        .expect("授予 profile");
    let provisioned = provisioner
        .provision(principal.clone(), &profile)
        .await
        .expect("创建 controller");
    assert_eq!(provisioned.api.principal(), principal);
    assert_eq!(provisioned.api.identity(), provisioned.controller.id);

    let wrong_principal =
        RuntimePrincipal::new("component:other:activation-1").expect("创建另一 principal");
    let wrong_binding = runtime
        .api_for(wrong_principal, &provisioned.controller.id)
        .await;
    assert!(matches!(
        wrong_binding,
        Err(AgentRuntimeError::OwnerMismatch { .. })
    ));

    assert_eq!(provisioner.revoke(&principal).await, 1);
    assert_eq!(provisioner.revoke(&principal).await, 0);
    assert!(!runtime
        .inner
        .agents
        .read()
        .await
        .contains_key(&provisioned.controller.id));
    let error = provisioned
        .api
        .spawn(AgentSpawnRequest::new("撤销后不得运行"))
        .await
        .expect_err("撤销后的 API 应失效");
    assert_eq!(error, AgentRuntimeError::PrincipalRevoked(principal));
}

/// 取消必须写入不可覆盖的终态，并中止阻塞中的模型调用。
#[tokio::test]
async fn cancellation_is_idempotent_and_terminal() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(BlockingModel {
                    entered: entered.clone(),
                    release,
                }),
                "blocking",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("阻塞任务"))
        .await
        .expect("派生阻塞 Agent");

    for _ in 0..100 {
        if entered.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(entered.load(Ordering::Acquire));
    let child_api = runtime.api(&child.id).await.expect("绑定子 Agent API");
    let grandchild = child_api
        .spawn(AgentSpawnRequest::new("后代阻塞任务"))
        .await
        .expect("派生后代 Agent");
    assert!(api.cancel(&child.id).await.expect("首次取消"));
    assert!(!api.cancel(&child.id).await.expect("重复取消"));
    assert_eq!(
        api.wait(&child.id).await.expect("读取取消终态"),
        AgentOutcome::Cancelled
    );
    assert_eq!(
        api.wait(&grandchild.id).await.expect("读取后代取消终态"),
        AgentOutcome::Cancelled
    );
    assert_eq!(
        api.status(&child.id).await.expect("读取状态").status,
        AgentStatus::Cancelled
    );
}

/// Runtime 外部取消必须等待 Core 调用 finish_run，不能直接丢弃仍在阻塞的运行 future。
#[tokio::test]
async fn external_cancellation_finishes_run_extension_lifecycle() {
    let entered = Arc::new(AtomicBool::new(false));
    let finished_runs = Arc::new(Mutex::new(Vec::new()));
    let gateway = ModelGateway::new();
    gateway
        .register(
            "blocking-cleanup",
            Arc::new(BlockingModel {
                entered: Arc::clone(&entered),
                release: Arc::new(Notify::new()),
            }),
        )
        .expect("注册阻塞测试模型");
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("blocking-cleanup", "test-model"),
    )
    .with_extension(Arc::new(FinishRecordingExtension {
        finished_runs: Arc::clone(&finished_runs),
    }));
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            AgentTemplate::from_agent(&agent),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载清理测试根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定清理测试 API");
    let child = api
        .spawn(AgentSpawnRequest::new("等待取消并清理扩展"))
        .await
        .expect("派生清理测试 Agent");

    for _ in 0..100 {
        if entered.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(entered.load(Ordering::Acquire));
    assert!(api.cancel(&child.id).await.expect("取消阻塞 Agent"));
    assert_eq!(
        api.wait(&child.id).await.expect("等待取消终态"),
        AgentOutcome::Cancelled
    );

    let finished_runs = finished_runs.lock().expect("扩展清理记录锁不应中毒");
    assert_eq!(finished_runs.len(), 1);
    assert!(!finished_runs[0].is_empty());
}

/// 应用级取消必须终止全部活动任务，但保留 controller 以便后续再次派生。
#[tokio::test]
async fn cancel_active_runs_preserves_ready_controllers() {
    let entered = Arc::new(AtomicBool::new(false));
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(BlockingModel {
                    entered: entered.clone(),
                    release: Arc::new(Notify::new()),
                }),
                "blocking",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = api
        .spawn(AgentSpawnRequest::new("首次后台任务"))
        .await
        .expect("派生首次后台任务");

    for _ in 0..100 {
        if entered.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(runtime.cancel_active_runs().await, 1);
    assert_eq!(
        api.wait(&first.id).await.expect("等待首次取消终态"),
        AgentOutcome::Cancelled
    );
    assert_eq!(
        api.status(&root.id)
            .await
            .expect("读取 controller 状态")
            .status,
        AgentStatus::Ready
    );

    let second = api
        .spawn(AgentSpawnRequest::new("再次后台任务"))
        .await
        .expect("保留的 controller 应可再次派生");
    assert_eq!(runtime.cancel_active_runs().await, 1);
    assert_eq!(
        api.wait(&second.id).await.expect("等待再次取消终态"),
        AgentOutcome::Cancelled
    );
}

/// controller 可以向运行中的后代注入消息，且消息进入同一私有会话的下一轮。
#[tokio::test]
async fn steer_injects_message_into_running_descendant() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_message = Arc::new(AtomicBool::new(false));
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(InteractiveModel {
                    entered: entered.clone(),
                    release: release.clone(),
                    calls: calls.clone(),
                    saw_message: saw_message.clone(),
                }),
                "interactive",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("开始任务"))
        .await
        .expect("派生 Agent");
    for _ in 0..100 {
        if entered.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }

    api.steer(&child.id, "实时补充检查".into())
        .await
        .expect("运行中成员应接收消息");
    release.notify_one();
    let outcome = api.wait(&child.id).await.expect("等待互动完成");

    assert!(matches!(
        outcome,
        AgentOutcome::Succeeded {
            result: AgentExecutionResult { final_text, .. }
        } if final_text == "互动完成"
    ));
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert!(saw_message.load(Ordering::Acquire));
}

/// 兄弟节点不能查询或取消彼此，但仍可通过已知身份发送消息。
#[tokio::test]
async fn management_is_descendant_scoped() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let root_api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = root_api
        .spawn(AgentSpawnRequest::new("一"))
        .await
        .expect("派生第一个");
    let second = root_api
        .spawn(AgentSpawnRequest::new("二"))
        .await
        .expect("派生第二个");
    let first_api = runtime.api(&first.id).await.expect("绑定第一个 API");

    let error = first_api
        .status(&second.id)
        .await
        .expect_err("兄弟节点不能读取状态");
    assert!(matches!(error, AgentRuntimeError::PermissionDenied { .. }));
}

/// 订阅应收到订阅之后的事件，并在目标进入终态后自然结束。
#[tokio::test]
async fn subscribe_streams_events_until_terminal() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(BlockingModel {
                    entered: entered.clone(),
                    release: release.clone(),
                }),
                "blocking",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("阻塞任务"))
        .await
        .expect("派生阻塞 Agent");

    let mut stream = api.subscribe(&child.id).await.expect("订阅子 Agent 事件");
    for _ in 0..100 {
        if entered.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(entered.load(Ordering::Acquire));
    release.notify_one();

    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        kinds.push(event.kind);
    }
    assert!(kinds.contains(&agent_core::AgentEventKind::RunFinished));
    assert_eq!(
        api.wait(&child.id).await.expect("读取终态").status(),
        AgentStatus::Succeeded
    );
}

/// 目标已处于终态时订阅先回放有界历史，耗尽后自然结束。
#[tokio::test]
async fn subscribe_after_terminal_replays_history_then_ends() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("执行任务"))
        .await
        .expect("派生 Agent");
    api.wait(&child.id).await.expect("等待终态");

    let mut stream = api.subscribe(&child.id).await.expect("终态后订阅");
    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        kinds.push(event.kind);
    }
    assert!(kinds.contains(&agent_core::AgentEventKind::RunStarted));
    assert!(kinds.contains(&agent_core::AgentEventKind::RunFinished));
    assert!(stream.next().await.is_none());
}

/// 订阅受后代范围限制：兄弟节点不能订阅彼此的事件。
#[tokio::test]
async fn subscribe_is_descendant_scoped() {
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let root_api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = root_api
        .spawn(AgentSpawnRequest::new("一"))
        .await
        .expect("派生第一个");
    let second = root_api
        .spawn(AgentSpawnRequest::new("二"))
        .await
        .expect("派生第二个");
    let first_api = runtime.api(&first.id).await.expect("绑定第一个 API");

    let error = first_api
        .subscribe(&second.id)
        .await
        .expect_err("兄弟节点不能订阅事件");
    assert!(matches!(error, AgentRuntimeError::PermissionDenied { .. }));
}

/// controller 与成员身份共同消耗 principal 名额，续跑不额外登记。
#[tokio::test]
async fn principal_agent_limit_counts_identities_not_continuations() {
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agents_per_principal: 3,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("首次任务"))
        .await
        .expect("派生 Agent");
    api.wait(&child.id).await.expect("等待首次任务");
    // 续跑复用成员身份，因此不占用名额，成员可以反复接活。
    for round in 0..4 {
        let continued = api
            .continue_agent(&child.id, format!("后续任务 {round}"))
            .await
            .expect("续跑不应消耗 principal 名额");
        assert_eq!(continued.id, child.id);
        api.wait(&continued.id).await.expect("等待后续运行");
    }

    // 第二个成员占用最后的名额，第三个真实身份才被拒绝。
    let second = api
        .spawn(AgentSpawnRequest::new("第二个成员"))
        .await
        .expect("第二个成员应成功");
    api.wait(&second.id).await.expect("等待第二个成员");
    let error = api
        .spawn(AgentSpawnRequest::new("超过累计名额"))
        .await
        .expect_err("第四个真实身份应被拒绝");
    assert!(matches!(
        error,
        AgentRuntimeError::MaxAgentsPerPrincipalExceeded { limit: 3 }
    ));
}

/// 不同 principal 必须拥有彼此隔离的累计 Agent 账本。
#[tokio::test]
async fn principal_agent_limits_are_isolated() {
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agents_per_principal: 1,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let first = RuntimePrincipal::new("plugin:first").expect("创建第一 principal");
    let second = RuntimePrincipal::new("plugin:second").expect("创建第二 principal");

    runtime
        .attach_root_for(
            first.clone(),
            template(Arc::new(FixedModel), "fixed-first", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("第一 principal 应获得自己的名额");
    let error = runtime
        .attach_root_for(
            first,
            template(Arc::new(FixedModel), "fixed-first-extra", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect_err("同一 principal 的第二个 controller 应被拒绝");
    assert!(matches!(
        error,
        AgentRuntimeError::MaxAgentsPerPrincipalExceeded { limit: 1 }
    ));
    runtime
        .attach_root_for(
            second,
            template(Arc::new(FixedModel), "fixed-second", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("另一 principal 不应受前者用量影响");
}

/// 父节点子数门禁失败时必须回滚 principal 预留，后续合法派生仍可使用该名额。
#[tokio::test]
async fn child_limit_failure_rolls_back_principal_reservation() {
    let release = Arc::new(Semaphore::new(0));
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_depth: 2,
        max_children_per_agent: 1,
        max_agents_per_principal: 3,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(ConcurrencyModel {
                    current: Arc::new(AtomicUsize::new(0)),
                    maximum: Arc::new(AtomicUsize::new(0)),
                    release: Arc::clone(&release),
                }),
                "rollback",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let root_api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = root_api
        .spawn(AgentSpawnRequest::new("第一个子节点"))
        .await
        .expect("第一个子节点应成功");
    let error = root_api
        .spawn(AgentSpawnRequest::new("超过父节点上限"))
        .await
        .expect_err("第二个直属子节点应被拒绝");
    assert!(matches!(
        error,
        AgentRuntimeError::MaxChildrenExceeded { limit: 1 }
    ));

    let child_api = runtime.api(&child.id).await.expect("绑定子节点 API");
    let grandchild = child_api
        .spawn(AgentSpawnRequest::new("合法孙节点"))
        .await
        .expect("回滚后应仍有一个 principal 名额");
    release.add_permits(2);
    child_api.wait(&grandchild.id).await.expect("等待孙节点");
    root_api.wait(&child.id).await.expect("等待子节点");
}

/// 多个 Agent 必须共享 principal 的模型请求次数账本，超额请求不能到达 provider。
#[tokio::test]
async fn model_request_limit_is_shared_across_agents() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Arc::new(RecordingModel {
        calls: Arc::clone(&calls),
        max_tokens: Arc::new(Mutex::new(Vec::new())),
    });
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agent_model_requests_per_principal: Some(1),
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(model, "request-limit", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = api
        .spawn(AgentSpawnRequest::new("第一请求"))
        .await
        .expect("派生第一 Agent");
    assert_eq!(
        api.wait(&first.id).await.expect("等待第一 Agent").status(),
        AgentStatus::Succeeded
    );
    let second = api
        .spawn(AgentSpawnRequest::new("第二请求"))
        .await
        .expect("派生第二 Agent");
    let outcome = api.wait(&second.id).await.expect("等待第二 Agent");

    assert_eq!(outcome.status(), AgentStatus::Failed);
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

/// provider 失败后已预留的请求额度不得返还，后续重试仍应在 provider 前被拒绝。
#[tokio::test]
async fn failed_provider_call_does_not_refund_model_request_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agent_model_requests_per_principal: Some(1),
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(FailingModel {
                    calls: Arc::clone(&calls),
                }),
                "failing",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = api
        .spawn(AgentSpawnRequest::new("第一次调用"))
        .await
        .expect("派生第一 Agent");
    assert_eq!(
        api.wait(&first.id).await.expect("等待第一 Agent").status(),
        AgentStatus::Failed
    );
    let second = api
        .spawn(AgentSpawnRequest::new("失败后重试"))
        .await
        .expect("派生第二 Agent");
    assert_eq!(
        api.wait(&second.id).await.expect("等待第二 Agent").status(),
        AgentStatus::Failed
    );

    assert_eq!(calls.load(Ordering::Acquire), 1);
}

/// 请求序列化字节预算不足时必须在 provider 调用前失败。
#[tokio::test]
async fn model_request_bytes_limit_rejects_before_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agent_model_request_bytes_per_principal: Some(1),
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(RecordingModel {
                    calls: Arc::clone(&calls),
                    max_tokens: Arc::new(Mutex::new(Vec::new())),
                }),
                "byte-limit",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("必然超过一个字节"))
        .await
        .expect("派生 Agent");
    let outcome = api.wait(&child.id).await.expect("等待 Agent");

    assert_eq!(outcome.status(), AgentStatus::Failed);
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

/// 派生 Agent 未声明 max_tokens 时，Runtime 必须在最终请求中注入有限单次上限。
#[tokio::test]
async fn missing_max_tokens_gets_safe_runtime_limit() {
    let max_tokens = Arc::new(Mutex::new(Vec::new()));
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(RecordingModel {
                    calls: Arc::new(AtomicUsize::new(0)),
                    max_tokens: Arc::clone(&max_tokens),
                }),
                "missing-max-tokens",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("检查输出上限"))
        .await
        .expect("派生 Agent");
    assert_eq!(
        api.wait(&child.id).await.expect("等待 Agent").status(),
        AgentStatus::Succeeded
    );

    assert_eq!(
        max_tokens
            .lock()
            .expect("模型请求记录锁不应中毒")
            .as_slice(),
        &[Some(DEFAULT_DERIVED_AGENT_MAX_TOKENS)]
    );
}

/// 显式零输出上限不能作为零成本预留绕过 provider 前门禁。
#[tokio::test]
async fn zero_max_tokens_is_rejected_before_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(RecordingModel {
                    calls: Arc::clone(&calls),
                    max_tokens: Arc::new(Mutex::new(Vec::new())),
                }),
                "zero-max-tokens",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let mut request = AgentSpawnRequest::new("检查零输出上限");
    request.derive.options.max_tokens = Some(0);
    let child = api.spawn(request).await.expect("派生 Agent");
    let outcome = api.wait(&child.id).await.expect("等待 Agent");

    assert_eq!(outcome.status(), AgentStatus::Failed);
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

/// 并发 Agent 的输出 token 预留必须原子执行，累计预算只能放行其中一次请求。
#[tokio::test]
async fn concurrent_model_output_token_reservation_is_atomic() {
    let calls = Arc::new(AtomicUsize::new(0));
    let max_tokens = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(BudgetConsumingModel {
        calls: Arc::clone(&calls),
        max_tokens: Arc::clone(&max_tokens),
    });
    let gateway = ModelGateway::new();
    gateway
        .register("output-budget", model)
        .expect("注册记录模型应成功");
    let mut options = AgentOptions::default().with_model_route("output-budget", "test-model");
    options.max_tokens = Some(64);
    let agent = Agent::new(gateway, options).with_context_loader(Arc::new(BarrierContextLoader {
        barrier: Arc::new(Barrier::new(2)),
    }));
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agent_model_requests_per_principal: Some(2),
        max_agent_model_output_tokens_per_principal: Some(64),
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            AgentTemplate::from_agent(&agent),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let first = api
        .spawn(AgentSpawnRequest::new("并发一"))
        .await
        .expect("派生第一 Agent");
    let second = api
        .spawn(AgentSpawnRequest::new("并发二"))
        .await
        .expect("派生第二 Agent");
    let (first_outcome, second_outcome) = tokio::join!(api.wait(&first.id), api.wait(&second.id));
    let outcomes = [
        first_outcome.expect("等待第一 Agent"),
        second_outcome.expect("等待第二 Agent"),
    ];

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.status() == AgentStatus::Succeeded)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.status() == AgentStatus::Failed)
            .count(),
        1
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(
        max_tokens
            .lock()
            .expect("模型请求记录锁不应中毒")
            .as_slice(),
        &[Some(64)]
    );
}

/// 运行阶段的 Core steering 队列同样最多保留 32 条消息。
#[tokio::test]
async fn running_steering_queue_rejects_the_thirty_third_message() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let runtime = AgentRuntime::new(RuntimeLimits::default()).expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(InteractiveModel {
                    entered: Arc::clone(&entered),
                    release: Arc::clone(&release),
                    calls: Arc::new(AtomicUsize::new(0)),
                    saw_message: Arc::new(AtomicBool::new(false)),
                }),
                "bounded-steering",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("等待实时消息"))
        .await
        .expect("派生 Agent");
    while !entered.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    for index in 0..STEERING_QUEUE_LIMIT {
        api.steer(&child.id, format!("实时补充 {index}"))
            .await
            .expect("前 32 条 steering 应成功入队");
    }
    let error = api
        .steer(&child.id, "第 33 条".to_string())
        .await
        .expect_err("第 33 条 steering 应被拒绝");
    assert!(matches!(
        error,
        AgentRuntimeError::PendingInteractionsExceeded {
            limit: STEERING_QUEUE_LIMIT,
            ..
        }
    ));

    release.notify_waiters();
    assert_eq!(
        api.wait(&child.id).await.expect("等待 Agent").status(),
        AgentStatus::Succeeded
    );
}

/// 排队阶段的 Runtime steering 缓冲同样最多保留 32 条消息。
#[tokio::test]
async fn queued_steering_buffer_rejects_the_thirty_third_message() {
    let current = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_concurrent_agents: 1,
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            template(
                Arc::new(ConcurrencyModel {
                    current: Arc::clone(&current),
                    maximum: Arc::new(AtomicUsize::new(0)),
                    release: Arc::clone(&release),
                }),
                "queued-steering",
                &[],
            ),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let running = api
        .spawn(AgentSpawnRequest::new("占用并发许可"))
        .await
        .expect("派生运行 Agent");
    while current.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    let queued = api
        .spawn(AgentSpawnRequest::new("保持排队"))
        .await
        .expect("派生排队 Agent");

    for index in 0..STEERING_QUEUE_LIMIT {
        api.steer(&queued.id, format!("排队消息 {index}"))
            .await
            .expect("前 32 条 steering 应成功暂存");
    }
    let error = api
        .steer(&queued.id, "第 33 条".to_string())
        .await
        .expect_err("第 33 条 steering 应被拒绝");
    assert!(matches!(
        error,
        AgentRuntimeError::PendingInteractionsExceeded {
            limit: STEERING_QUEUE_LIMIT,
            ..
        }
    ));

    // 排队消息会逐条开启后续模型轮次，因此一次性放行全部测试调用。
    release.add_permits(64);
    api.wait(&running.id).await.expect("等待运行 Agent");
    api.wait(&queued.id).await.expect("等待排队 Agent");
}

/// 门禁拒绝的终态必须带上真实原因，而不是只剩一层外层 context。
///
/// `error.to_string()` 只打印 anyhow 的最外层，会把"为什么被拒绝"（次数/字节数/
/// 输出预算超限）整条丢掉，让派生 Agent 的失败无法自行诊断。这里把请求次数上限
/// 设为 0 强制触发拒绝，断言终态里同时出现外层说明与真实原因。
#[tokio::test]
async fn gate_rejection_terminal_state_explains_the_reason() {
    let limits = RuntimeLimits {
        max_agent_model_requests_per_principal: Some(0),
        ..RuntimeLimits::default()
    };
    let runtime = AgentRuntime::new(limits).expect("创建零请求额度的 Runtime");
    let root = runtime
        .attach_root(
            template(Arc::new(FixedModel), "fixed", &[]),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");
    let child = api
        .spawn(AgentSpawnRequest::new("触发门禁拒绝"))
        .await
        .expect("派生 Agent");

    let outcome = api.wait(&child.id).await.expect("等待子 Agent");
    let AgentOutcome::Failed { error } = outcome else {
        panic!("门禁拒绝必须以失败终态收敛，实际为 {outcome:?}");
    };
    assert!(
        error.contains("可信门禁拒绝"),
        "终态应保留外层说明，实际为：{error}"
    );
    assert!(
        error.contains("模型请求累计次数超过上限 0"),
        "终态必须说明真实原因，实际为：{error}"
    );
}

/// 输出预算必须按实际用量结算，否则一次请求就会吃掉整份上限。
///
/// 模型声明的输出上限（`max_tokens`）显著大于真实回答长度：models.dev 同步后可达
/// 数十万 token，而一次回答通常只有一千上下。若只在发起请求时按上限预留、从不结算，
/// 默认预算会在几十次请求后耗尽，把"允许 8192 次请求"变成"允许几十次"，派生 Agent
/// 会在没有日志解释的情况下全部失败。
///
/// 这里让每次请求预留 400、实际只用 5：只预留不结算时第 3 次必然超限，按用量结算
/// 则 8 次请求都应落在 1000 的预算内。
#[tokio::test]
async fn output_budget_settles_actual_usage_instead_of_reservation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gateway = ModelGateway::new();
    gateway
        .register(
            "settle",
            Arc::new(SmallOutputModel {
                calls: Arc::clone(&calls),
            }),
        )
        .expect("注册小输出模型应成功");
    let mut options = AgentOptions::default().with_model_route("settle", "test-model");
    options.max_tokens = Some(400);
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agent_model_requests_per_principal: Some(8),
        max_agent_model_output_tokens_per_principal: Some(1_000),
        ..RuntimeLimits::default()
    })
    .expect("创建 Runtime");
    let root = runtime
        .attach_root(
            AgentTemplate::from_agent(&Agent::new(gateway, options)),
            AgentPermissions::default(),
        )
        .await
        .expect("挂载根 Agent");
    let api = runtime.api(&root.id).await.expect("绑定根 API");

    for index in 0..8 {
        let child = api
            .spawn(AgentSpawnRequest::new(format!("第 {index} 次请求")))
            .await
            .expect("派生 Agent");
        let outcome = api.wait(&child.id).await.expect("等待 Agent");
        assert_eq!(
            outcome.status(),
            AgentStatus::Succeeded,
            "第 {} 次请求不应因输出预算被拒",
            index + 1
        );
    }
    assert_eq!(calls.load(Ordering::Acquire), 8, "8 次请求都应到达模型");
}
