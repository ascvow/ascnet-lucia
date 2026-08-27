use super::*;
use crate::{AgentExecutionResult, RuntimeRunFinalizer, RuntimeRunObservation, ToolAccess};
use agent_core::{
    Agent, AgentOptions, ChatModel, InMemoryEventSink, ModelGateway, ModelRequest, ModelResponse,
    ProviderAdapter,
};
use agent_tool::{JsonTool, ToolRegistry, ToolSpec};
use anyhow::Result;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// 等待通知的测试模型，用于验证取消语义。
struct BlockingModel {
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
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
    let mut gateway = ModelGateway::new();
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
    assert_eq!(continued.lineage.parent.as_ref(), Some(&child.id));
    assert_eq!(continued.lineage.depth, child.lineage.depth + 1);
    let snapshot = api.status(&continued.id).await.expect("查询后续运行");
    assert_eq!(snapshot.permissions, permissions);
    assert!(matches!(
        api.wait(&continued.id).await.expect("等待后续运行"),
        AgentOutcome::Succeeded { .. }
    ));
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
