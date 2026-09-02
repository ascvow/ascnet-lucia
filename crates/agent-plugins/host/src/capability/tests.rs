//! 宿主能力边界、进程策略与 Runtime 调度的回归测试。

use super::*;
use crate::audit::{HostServiceCallResult, InMemoryHostServiceCallObserver, JsonValueKind};
use crate::capability::agent::AgentRuntimeBinding;
use crate::capability::requests::{
    parse_request, validate_process_spawn_request, ProcessSpawnRequest,
};
use crate::contribution::ContributionRegistry;
use crate::manifest::{AgentCapabilitySection, CapabilitySection};
use crate::service::{PluginService, PluginServiceCall, ServiceHandler, ServiceRegistry};
use crate::PluginHostServices;
use agent_core::{model::ModelEventStream, ChatModel, ModelResponse, ProviderAdapter};
use agent_core::{ModelRequest, ReasoningLevel, ToolChoice};
use agent_runtime::{
    AgentDeriveConfig, AgentHandle, AgentLineage, AgentOutcome, AgentPermissions, AgentProfileId,
    AgentRuntimeError, AgentRuntimeProvisioner, AgentSnapshot, AgentStatus,
    ProvisionedAgentRuntime, RuntimeResult,
};
use agent_runtime::{AgentId, AgentRuntimeApi, AgentSpawnRequest, RuntimePrincipal};
use agent_tool::ExecutionPolicy;
use agent_tool::ToolAccess;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// Agent Runtime 的 Guest 订阅出口不得暴露服务商私有推理增量。
#[test]
fn agent_runtime_guest_events_hide_private_thinking() {
    let event = agent_core::AgentEvent::new(
        "run-private",
        agent_core::AgentEventKind::ModelThinkingDelta,
        0,
        json!({"delta": "私有推理"}),
    );

    assert!(super::agent::sanitize_guest_visible_agent_event(event).is_none());
}

/// `agent.observe` 不得绕过 Core 的审批事件脱敏读取完整拒绝原因。
#[test]
fn agent_runtime_guest_events_hide_approval_reason() {
    let event = agent_core::AgentEvent::new(
        "run-denied",
        agent_core::AgentEventKind::ToolFinished,
        0,
        json!({
            "call_id": "blocked-call",
            "name": "shell",
            "content": {"error": "审批模型拒绝：命令包含敏感宿主路径"},
            "is_error": true,
            "error_kind": "permission_denied",
            "details": {"private": "仅可信 UI 可见"},
            "runtime_origin": "plugin_policy"
        }),
    );

    let sanitized =
        super::agent::sanitize_guest_visible_agent_event(event).expect("工具终态仍应向 Guest 可见");

    assert_eq!(sanitized.payload["call_id"], "blocked-call");
    assert_eq!(sanitized.payload["error_kind"], "permission_denied");
    assert_eq!(
        sanitized.payload["content"]["error"],
        "工具调用未获批准，未执行。"
    );
    assert!(sanitized.payload.get("details").is_none());
    assert!(!sanitized.payload.to_string().contains("敏感宿主路径"));
}

/// 捕获 Host 最终模型请求的测试适配器。
struct CapturingCompletionModel {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    expect_stream: bool,
}

#[async_trait]
impl ChatModel for CapturingCompletionModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        assert!(!self.expect_stream, "Host 应使用配置的非流式调用路径");
        self.requests.lock().expect("锁定模型请求").push(request);
        Ok(ModelResponse::text("受控模型摘要"))
    }

    async fn stream(&self, request: ModelRequest) -> ModelEventStream {
        assert!(self.expect_stream, "Host 应使用配置的流式调用路径");
        self.requests.lock().expect("锁定模型请求").push(request);
        let (sender, stream) = ModelEventStream::channel();
        sender.done(ModelResponse::text("受控模型摘要"));
        stream
    }
}

impl ProviderAdapter for CapturingCompletionModel {
    fn name(&self) -> &'static str {
        "capturing-completion"
    }
}

/// 回显 Host 最终注入调用方的测试服务。
struct CallerEchoService;

#[async_trait]
impl ServiceHandler for CallerEchoService {
    async fn handle(&self, call: PluginServiceCall) -> Result<Value> {
        Ok(json!({"caller_id": call.caller_id}))
    }
}

struct MockAgentRuntime {
    principal: RuntimePrincipal,
    controller: AgentId,
    child: AgentId,
}

#[async_trait]
impl AgentRuntimeApi for MockAgentRuntime {
    fn principal(&self) -> RuntimePrincipal {
        self.principal.clone()
    }

    fn identity(&self) -> AgentId {
        self.controller.clone()
    }

    async fn spawn(&self, _request: AgentSpawnRequest) -> RuntimeResult<AgentHandle> {
        Ok(AgentHandle {
            id: self.child.clone(),
            lineage: AgentLineage {
                parent: Some(self.controller.clone()),
                root: self.controller.clone(),
                depth: 1,
            },
        })
    }

    async fn continue_agent(&self, target: &AgentId, _input: String) -> RuntimeResult<AgentHandle> {
        Ok(AgentHandle {
            id: self.child.clone(),
            lineage: AgentLineage {
                parent: Some(target.clone()),
                root: self.controller.clone(),
                depth: 2,
            },
        })
    }

    async fn steer(&self, _target: &AgentId, _input: String) -> RuntimeResult<()> {
        Ok(())
    }

    async fn status(&self, target: &AgentId) -> RuntimeResult<AgentSnapshot> {
        Ok(AgentSnapshot {
            id: target.clone(),
            lineage: AgentLineage {
                parent: Some(self.controller.clone()),
                root: self.controller.clone(),
                depth: 1,
            },
            status: AgentStatus::Running,
            permissions: AgentPermissions::default(),
        })
    }

    async fn result(&self, _target: &AgentId) -> RuntimeResult<Option<AgentOutcome>> {
        Ok(Some(AgentOutcome::Cancelled))
    }

    async fn wait(&self, _target: &AgentId) -> RuntimeResult<AgentOutcome> {
        panic!("WASM dispatcher 不得调用长期等待式 wait")
    }

    async fn cancel(&self, _target: &AgentId) -> RuntimeResult<bool> {
        Ok(true)
    }

    async fn subscribe(&self, _target: &AgentId) -> RuntimeResult<agent_runtime::AgentEventStream> {
        Ok(agent_runtime::AgentEventStream::empty())
    }
}

struct NoopProvisioner;

#[async_trait]
impl AgentRuntimeProvisioner for NoopProvisioner {
    async fn grant_profile(
        &self,
        _principal: RuntimePrincipal,
        _profile: &AgentProfileId,
    ) -> RuntimeResult<()> {
        Ok(())
    }

    async fn provision(
        &self,
        _principal: RuntimePrincipal,
        profile: &AgentProfileId,
    ) -> RuntimeResult<ProvisionedAgentRuntime> {
        Err(AgentRuntimeError::ProfileNotFound(profile.clone()))
    }

    async fn revoke_profile_grant(
        &self,
        _principal: &RuntimePrincipal,
        _profile: &AgentProfileId,
    ) -> bool {
        true
    }

    async fn revoke(&self, _principal: &RuntimePrincipal) -> usize {
        0
    }
}

fn agent_runtime_fixture() -> (
    AgentCapabilitySection,
    AgentRuntimeBinding,
    AgentId,
    AgentId,
) {
    let principal = RuntimePrincipal::new("plugin:test:activation").expect("创建 principal");
    let controller = AgentId::new();
    let child = AgentId::new();
    let mut spawn_profiles = HashMap::new();
    spawn_profiles.insert("reviewer".into(), AgentDeriveConfig::default());
    let host_services = crate::PluginHostServices::new()
        .with_agent_runtime(
            Arc::new(NoopProvisioner),
            AgentProfileId::new("controller").expect("创建 controller profile"),
            spawn_profiles,
        )
        .expect("创建 Host Services")
        .agent_runtime()
        .expect("读取 Runtime 服务");
    let binding = AgentRuntimeBinding::new(
        principal.clone(),
        Arc::new(MockAgentRuntime {
            principal,
            controller: controller.clone(),
            child: child.clone(),
        }),
        host_services,
    );
    (
        AgentCapabilitySection {
            spawn: true,
            observe: true,
            cancel: true,
            profiles: vec!["reviewer".into()],
        },
        binding,
        controller,
        child,
    )
}

fn test_state() -> CapabilityState {
    CapabilityState::new(
        "test-plugin".into(),
        "test-bundle".into(),
        PathBuf::from("."),
        CapabilitySection::default(),
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::default(),
    )
    .expect("创建测试能力状态")
}

/// 创建带指定 manifest 进程声明和可信执行策略的测试状态。
fn process_state(process_exec: bool, execution_policy: ExecutionPolicy) -> CapabilityState {
    CapabilityState::new(
        "process-test-plugin".into(),
        "process-test-bundle".into(),
        PathBuf::from("."),
        CapabilitySection {
            process_exec,
            ..CapabilitySection::default()
        },
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::new(None, None, execution_policy, None, None, None),
    )
    .expect("创建进程测试能力状态")
}

/// 插件实例状态应支持读取、替换和删除。
#[test]
fn instance_state_round_trip() {
    let mut state = test_state();
    state
        .set_state(r#"{"key":"counter","value":2}"#)
        .expect("写入状态应成功");
    assert_eq!(
        state
            .get_state(r#"{"key":"counter"}"#)
            .expect("读取状态应成功"),
        json!(2)
    );
    assert_eq!(
        state
            .remove_state(r#"{"key":"counter"}"#)
            .expect("删除状态应成功"),
        json!(2)
    );
    assert_eq!(
        state
            .get_state(r#"{"key":"counter"}"#)
            .expect("读取空状态应成功"),
        Value::Null
    );
    state
        .set_state(r#"{"key":"nullable","value":null}"#)
        .expect("写入空值状态应成功");
    assert_eq!(
        state
            .remove_state(r#"{"key":"nullable"}"#)
            .expect("删除空值状态应成功"),
        Value::Null
    );
    assert!(!state.state.contains_key("nullable"));
}

/// 应用注入持久目录后，新 CapabilityState 必须在插件激活前恢复同一插件的状态。
#[test]
fn persistent_instance_state_survives_recreation() {
    let root = std::env::temp_dir().join(format!(
        "lucia-capability-state-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let create_state = || {
        CapabilityState::new(
            "permission".into(),
            "permission-bundle-a".into(),
            PathBuf::from("."),
            CapabilitySection::default(),
            Arc::new(ContributionRegistry::default()),
            Arc::new(ServiceRegistry::default()),
            CapabilityHostContext::new(None, None, ExecutionPolicy::serve(), Some(root.clone()), Some("test-workspace".into()), None),
        )
        .expect("创建持久能力状态")
    };
    let mut first = create_state();
    first
        .set_state(r#"{"key":"permission_mode","value":"allow_all"}"#)
        .expect("保存权限模式");

    let mut reopened = create_state();
    assert_eq!(
        reopened
            .get_state(r#"{"key":"permission_mode"}"#)
            .expect("恢复权限模式"),
        json!("allow_all")
    );
    std::fs::remove_dir_all(root).expect("清理持久能力状态测试目录");
}

/// 同 ID 的不同可信 Bundle 不得读取旧权限插件保存的 AllowAll。
#[test]
fn persistent_permission_state_is_isolated_by_bundle_identity() {
    let root = std::env::temp_dir().join(format!(
        "lucia-capability-state-identity-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let create_state = |state_identity: &str| {
        CapabilityState::new(
            "permission".into(),
            state_identity.into(),
            PathBuf::from("."),
            CapabilitySection::default(),
            Arc::new(ContributionRegistry::default()),
            Arc::new(ServiceRegistry::default()),
            CapabilityHostContext::new(None, None, ExecutionPolicy::serve(), Some(root.clone()), Some("test-workspace".into()), None),
        )
        .expect("创建持久能力状态")
    };
    let mut original = create_state("permission-bundle-a");
    original
        .set_state(r#"{"key":"permission_mode:project-a","value":"allow_all"}"#)
        .expect("保存旧 Bundle 权限模式");

    let mut replacement = create_state("permission-bundle-b");
    assert_eq!(
        replacement
            .get_state(r#"{"key":"permission_mode:project-a"}"#)
            .expect("读取新 Bundle 权限模式"),
        Value::Null
    );
    let mut reopened = create_state("permission-bundle-a");
    assert_eq!(
        reopened
            .get_state(r#"{"key":"permission_mode:project-a"}"#)
            .expect("恢复原 Bundle 权限模式"),
        json!("allow_all")
    );
    std::fs::remove_dir_all(root).expect("清理身份隔离状态测试目录");
}

/// 发布事件时 Host 必须覆盖可信的插件来源。
#[test]
fn emitted_event_contains_trusted_source() {
    let contributions = Arc::new(ContributionRegistry::default());
    let mut state = CapabilityState::new(
        "trusted-id".into(),
        "trusted-bundle".into(),
        PathBuf::from("."),
        CapabilitySection::default(),
        contributions.clone(),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::default(),
    )
    .expect("创建事件测试能力状态");
    state
        .emit_event(r#"{"name":"demo.ready","data":{"ok":true}}"#)
        .expect("发布事件应成功");
    let events = contributions.drain_events().expect("读取事件应成功");
    assert_eq!(events[0]["source"]["id"], "trusted-id");
    assert_eq!(events[0]["name"], "demo.ready");
}

/// Guest 导出 Future 被丢弃等价于作用域守卫 Drop，后续控制面事件不得继承旧 run。
#[test]
fn active_run_scope_restores_previous_binding_on_drop() {
    let contributions = Arc::new(ContributionRegistry::default());
    let mut state = CapabilityState::new(
        "trusted-id".into(),
        "trusted-bundle".into(),
        PathBuf::from("."),
        CapabilitySection::default(),
        contributions.clone(),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::default(),
    )
    .expect("创建运行作用域测试能力状态");

    {
        let _scope = state.enter_active_run_scope(Some("run-a"));
        state
            .emit_event(r#"{"name":"run.event","data":{}}"#)
            .expect("运行内事件应成功");
    }
    state
        .emit_event(r#"{"name":"ui.event","data":{}}"#)
        .expect("作用域结束后的控制面事件应成功");

    let run_events = contributions
        .drain_events_for_run("run-a")
        .expect("读取运行事件应成功");
    let control_events = contributions.drain_events().expect("读取控制面事件应成功");
    assert_eq!(run_events[0]["name"], "run.event");
    assert_eq!(control_events[0]["name"], "ui.event");
}

/// 异步 Guest 导出被取消时必须立即 Drop 守卫，并恢复进入导出前的运行绑定。
#[tokio::test]
async fn active_run_scope_restores_binding_when_future_is_cancelled() {
    let binding = ActiveRunBinding::default();
    let outer_scope = binding.enter(Some("run-outer"));
    let worker_binding = binding.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _inner_scope = worker_binding.enter(Some("run-cancelled"));
        entered_tx.send(()).expect("通知测试任务已进入运行作用域");
        std::future::pending::<()>().await;
    });

    entered_rx.await.expect("测试任务应进入运行作用域");
    assert_eq!(binding.current().as_deref(), Some("run-cancelled"));
    task.abort();
    let cancelled = task.await.expect_err("测试任务应被取消");
    assert!(cancelled.is_cancelled());
    assert_eq!(binding.current().as_deref(), Some("run-outer"));

    drop(outer_scope);
    assert_eq!(binding.current(), None);
}

/// 宿主界面动作必须逐项授权，permission 插件需要的 SetInput 不得隐式获得 Exit。
#[test]
fn surface_action_allowlist_is_enforced_per_action() {
    let mut state = CapabilityState::new(
        "permission".into(),
        "permission-bundle".into(),
        PathBuf::from("."),
        CapabilitySection {
            surface_action_allowlist: vec![crate::ui::UiHostActionKind::SetInput],
            ..CapabilitySection::default()
        },
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::default(),
    )
    .expect("创建细粒度界面动作状态");

    state
        .emit_event(
            &json!({
                "name": crate::ui::UI_HOST_ACTION_EVENT,
                "data": {
                    "request_id": "set-input-1",
                    "action": {"action": "set_input", "text": "", "cursor": 0}
                }
            })
            .to_string(),
        )
        .expect("已授权 SetInput 应成功");
    let error = state
        .emit_event(
            &json!({
                "name": crate::ui::UI_HOST_ACTION_EVENT,
                "data": {
                    "request_id": "exit-1",
                    "action": {"action": "exit"}
                }
            })
            .to_string(),
        )
        .expect_err("未授权 Exit 必须拒绝");
    assert!(error.to_string().contains("未授权 surface action `exit`"));
    assert!(is_capability_denied(&error));
}

/// 宿主响应必须携带稳定版本，且成功和失败信封保持同一结构入口。
#[test]
fn host_response_includes_schema_version() {
    let success: Value =
        serde_json::from_str(&encode_host_response(Ok(json!(7)))).expect("解析成功响应");
    let failure = anyhow!("底层失败").context("外层失败");
    let failure: Value =
        serde_json::from_str(&encode_host_response(Err(failure))).expect("解析失败响应");

    assert_eq!(success["schema_version"], HOST_RESPONSE_SCHEMA_VERSION);
    assert_eq!(success["value"], 7);
    assert_eq!(failure["schema_version"], HOST_RESPONSE_SCHEMA_VERSION);
    assert_eq!(failure["error"], "外层失败: 底层失败");
}

/// 进程能力必须在调用操作系统前拒绝无界或无效字段。
#[test]
fn process_spawn_request_has_structural_limits() {
    let valid = ProcessSpawnRequest {
        command: "bun".into(),
        args: vec!["run".into(), "server.ts".into()],
        env: HashMap::from([("MODE".into(), "stdio".into())]),
        cwd: Some("config".into()),
        inherit_stderr: false,
    };
    assert!(validate_process_spawn_request(&valid).is_ok());

    let mut invalid = valid;
    invalid.args = vec!["x".into(); MAX_PROCESS_ARGUMENTS + 1];
    assert!(validate_process_spawn_request(&invalid).is_err());
    invalid.args.clear();
    invalid.env = HashMap::from([("BAD=KEY".into(), "value".into())]);
    assert!(validate_process_spawn_request(&invalid).is_err());
}

/// manifest 声明不得覆盖 Evaluation 或 Mutation 平面的可信进程禁令。
#[test]
fn process_manifest_cannot_override_restricted_plane_policy() {
    for mut policy in [
        ExecutionPolicy::evaluation("."),
        ExecutionPolicy::mutation(),
    ] {
        // 即使调用方错误地改开布尔位，可信平面身份也必须保持最终否决权。
        policy.allow_process = true;
        let mut state = process_state(true, policy);
        let error = state
            .spawn_process(r#"{"command":"command-must-not-run"}"#)
            .expect_err("受限平面必须在调用操作系统前拒绝进程");

        assert!(error.to_string().contains("ExecutionPolicy 禁止"));
        assert!(state.processes.is_empty());
    }
}

/// Serve 平面允许进程时仍必须获得 manifest 的显式声明。
#[test]
fn serve_process_policy_still_requires_manifest_permission() {
    let denied = process_state(false, ExecutionPolicy::serve());
    let error = denied
        .require_process_exec()
        .expect_err("缺少 manifest 声明必须拒绝");
    assert!(error.to_string().contains("manifest 未声明 process_exec"));

    let allowed = process_state(true, ExecutionPolicy::serve());
    allowed
        .require_process_exec()
        .expect("Serve 策略和 manifest 同时允许时门禁应通过");
}

/// Serve 平面仍必须执行 ResourceLimits 的进程数上限，`0` 在触碰操作系统前拒绝。
#[test]
fn plugin_process_count_follows_execution_policy_limit() {
    let mut policy = ExecutionPolicy::serve();
    policy.limits.max_processes = Some(0);
    let mut state = process_state(true, policy);

    let error = state
        .spawn_process(r#"{"command":"command-must-not-run"}"#)
        .expect_err("进程上限为 0 时必须拒绝");

    assert!(error.to_string().contains("进程数上限为 0"));
    assert!(state.processes.is_empty());
}

/// 插件状态被释放时必须回收受管进程组，不能让后台后代脱离 Host 生命周期。
#[cfg(unix)]
#[tokio::test]
async fn dropping_plugin_state_kills_managed_process_group() {
    let heartbeat = std::env::temp_dir().join(format!(
        "lucia-plugin-process-heartbeat-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut state = process_state(true, ExecutionPolicy::serve());
    let handle = state
        .spawn_process(
            &json!({
                "command": "sh",
                "args": ["-c", "printf 'ready\\n'; while :; do printf x >> \"$HEARTBEAT\"; sleep 0.02; done & wait"],
                "env": {"HEARTBEAT": heartbeat.to_string_lossy()}
            })
            .to_string(),
        )
        .expect("应启动插件测试进程")
        .as_u64()
        .expect("进程句柄应为整数");
    let ready = state
        .read_process_line(&json!({"handle": handle, "timeout_ms": 2_000}).to_string())
        .await
        .expect("应读取插件进程就绪信号")
        .as_str()
        .expect("就绪信号应为文本")
        .to_string();
    assert_eq!(ready, "ready");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::fs::metadata(&heartbeat).is_ok_and(|metadata| metadata.len() > 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("后台后代应开始产生心跳");

    drop(state);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let settled_size = std::fs::metadata(&heartbeat).expect("心跳文件应存在").len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        std::fs::metadata(&heartbeat).expect("心跳文件应保留").len(),
        settled_size,
        "插件状态释放后后台后代必须停止运行"
    );
    std::fs::remove_file(heartbeat).expect("清理插件进程心跳文件");
}

/// 插件即使把工具伪装成普通名称，也不能绕过实际进程能力入口。
#[test]
fn plugin_tool_alias_cannot_bypass_process_policy() {
    let mut policy = ExecutionPolicy::evaluation(".");
    policy.tools = ToolAccess::allowlist(["innocent_plugin_tool"]);
    assert!(policy.permits_tool("innocent_plugin_tool"));

    let mut state = process_state(true, policy);
    let error = state
        .spawn_process(r#"{"command":"command-must-not-run"}"#)
        .expect_err("Host 进程门禁不得信任插件工具名称");

    assert!(error.to_string().contains("ExecutionPolicy 禁止"));
    assert!(state.processes.is_empty());
}

/// 所有 JSON 宿主能力入口必须共享请求大小上限。
#[test]
fn host_capability_request_size_is_bounded() {
    let oversized = "x".repeat(MAX_HOST_CAPABILITY_REQUEST_BYTES + 1);
    let error = parse_request::<Value>(&oversized).expect_err("超大请求必须被拒绝");

    assert!(error.to_string().contains("请求超过"));
}

/// 模型完成能力必须同时具备 manifest 授权和应用侧服务绑定。
#[tokio::test]
async fn model_completion_requires_manifest_permission_and_binding() {
    let request = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"摘要"}]}]}"#;
    let unauthorized = CapabilityState::complete_model_with(false, None, request)
        .await
        .expect_err("未授权请求必须失败");
    assert!(unauthorized.to_string().contains("manifest 未授权"));

    let missing_binding = CapabilityState::complete_model_with(true, None, request)
        .await
        .expect_err("未绑定模型服务必须失败");
    assert!(missing_binding.to_string().contains("未注入模型完成服务"));
}

/// Guest 不得通过请求字段覆盖 Host 注入的模型路由。
#[tokio::test]
async fn model_completion_rejects_forged_route_fields() {
    let error =
        CapabilityState::complete_model_with(true, None, r#"{"model":"forged","messages":[]}"#)
            .await
            .expect_err("伪造 model 字段必须失败");

    assert!(error.to_string().contains("解析插件宿主能力请求失败"));
}

/// Host 必须固定 provider、model、工具策略和最大输出预算。
#[tokio::test]
async fn model_completion_uses_trusted_route_and_limits() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = agent_core::ModelGateway::new();
    gateway
        .register(
            "trusted-provider",
            Arc::new(CapturingCompletionModel {
                requests: requests.clone(),
                expect_stream: true,
            }),
        )
        .expect("注册测试模型");
    let binding = PluginHostServices::new()
        .with_model_completion(gateway, "trusted-provider", "trusted-model", 128, true)
        .expect("注入流式模型服务")
        .model_completion()
        .expect("模型服务绑定应存在");

    let response = CapabilityState::complete_model_with(
        true,
        Some(binding),
        r#"{"system":"只生成摘要","messages":[{"role":"user","content":[{"type":"text","text":"旧上下文"}]}],"max_tokens":999}"#,
    )
    .await
    .expect("受控模型请求应成功");

    assert_eq!(response["text"], "受控模型摘要");
    let captured = requests.lock().expect("锁定模型请求");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].model, "trusted-model");
    assert_eq!(captured[0].max_tokens, Some(128));
    assert_eq!(captured[0].tool_choice, ToolChoice::None);
    assert!(captured[0].tools.is_empty());
    assert_eq!(captured[0].reasoning, ReasoningLevel::Off);
}

/// Host 关闭流式模式后必须改用模型的非流式完成接口。
#[tokio::test]
async fn model_completion_can_use_non_streaming_route() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = agent_core::ModelGateway::new();
    gateway
        .register(
            "trusted-provider",
            Arc::new(CapturingCompletionModel {
                requests: requests.clone(),
                expect_stream: false,
            }),
        )
        .expect("注册非流式测试模型");
    let binding = PluginHostServices::new()
        .with_model_completion(gateway, "trusted-provider", "trusted-model", 20_000, false)
        .expect("注入非流式模型服务")
        .model_completion()
        .expect("模型服务绑定应存在");

    CapabilityState::complete_model_with(
        true,
        Some(binding),
        r#"{"messages":[{"role":"user","content":[{"type":"text","text":"旧上下文"}]}]}"#,
    )
    .await
    .expect("非流式模型完成应成功");

    assert_eq!(requests.lock().expect("锁定模型请求").len(), 1);
}

/// 插件模型完成必须执行累计调用预算，防止插件通过重复审核形成无界费用。
#[tokio::test]
async fn model_completion_enforces_execution_policy_budget() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = agent_core::ModelGateway::new();
    gateway
        .register(
            "trusted-provider",
            Arc::new(CapturingCompletionModel {
                requests: requests.clone(),
                expect_stream: false,
            }),
        )
        .expect("注册预算测试模型");
    let mut policy = ExecutionPolicy::serve();
    policy.limits.max_plugin_model_completions = Some(1);
    let binding = PluginHostServices::new()
        .restrict_execution_policy(&policy)
        .with_model_completion(gateway, "trusted-provider", "trusted-model", 128, false)
        .expect("注入预算模型服务")
        .model_completion()
        .expect("模型服务绑定应存在");
    let request = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"审核"}]}]}"#;

    CapabilityState::complete_model_with(true, Some(binding.clone()), request)
        .await
        .expect("第一次调用应在预算内");
    let error = CapabilityState::complete_model_with(true, Some(binding), request)
        .await
        .expect_err("第二次调用必须被预算拒绝");

    assert!(error.to_string().contains("调用次数超过策略上限 1 次"));
    assert!(is_capability_denied(&error));
    assert_eq!(requests.lock().expect("锁定模型请求").len(), 1);
}

/// 不同插件激活必须拥有独立预算，而同一激活内的服务 clone 继续共享计数器。
#[tokio::test]
async fn model_completion_budget_is_isolated_per_plugin_activation() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = agent_core::ModelGateway::new();
    gateway
        .register(
            "trusted-provider",
            Arc::new(CapturingCompletionModel {
                requests: requests.clone(),
                expect_stream: false,
            }),
        )
        .expect("注册预算隔离测试模型");
    let mut policy = ExecutionPolicy::serve();
    policy.limits.max_plugin_model_completions = Some(1);
    let services = PluginHostServices::new()
        .restrict_execution_policy(&policy)
        .with_model_completion(gateway, "trusted-provider", "trusted-model", 128, false)
        .expect("注入预算模型服务");
    let first_activation = services
        .model_completion()
        .expect("第一插件激活应获得模型服务");
    let second_activation = services
        .model_completion()
        .expect("第二插件激活应获得模型服务");
    let request = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"审核"}]}]}"#;

    CapabilityState::complete_model_with(true, Some(first_activation.clone()), request)
        .await
        .expect("第一插件首次调用应成功");
    let error = CapabilityState::complete_model_with(true, Some(first_activation), request)
        .await
        .expect_err("同一插件通过 clone 发起第二次调用仍应被拒绝");
    assert!(error.to_string().contains("调用次数超过策略上限 1 次"));
    CapabilityState::complete_model_with(true, Some(second_activation), request)
        .await
        .expect("第二插件应拥有独立预算");

    assert_eq!(requests.lock().expect("锁定模型请求").len(), 2);
}

/// Dispatcher 只暴露短控制面操作，并返回可跨 ABI 使用的脱敏结构。
#[tokio::test]
async fn agent_runtime_dispatches_short_operations() {
    let (permissions, binding, controller, child) = agent_runtime_fixture();

    let identity = CapabilityState::call_agent_runtime_with(
        permissions.clone(),
        Some(binding.clone()),
        r#"{"operation":"identity"}"#,
    )
    .await
    .expect("读取 controller 身份");
    assert_eq!(identity, json!(controller));

    let spawned = CapabilityState::call_agent_runtime_with(
        permissions.clone(),
        Some(binding.clone()),
        r#"{"operation":"spawn","request":{"profile":"reviewer","input":"检查代码"}}"#,
    )
    .await
    .expect("启动派生 Agent");
    assert_eq!(spawned["id"], json!(child));

    let continued_request = json!({
        "operation": "continue",
        "request": {"target": child.to_string(), "input": "继续检查"},
    })
    .to_string();
    let continued = CapabilityState::call_agent_runtime_with(
        permissions.clone(),
        Some(binding.clone()),
        &continued_request,
    )
    .await
    .expect("继续派生 Agent");
    assert_eq!(continued["lineage"]["parent"], json!(child));

    let status_request = json!({
        "operation": "status",
        "request": {"target": child.to_string()},
    })
    .to_string();
    let status = CapabilityState::call_agent_runtime_with(
        permissions.clone(),
        Some(binding.clone()),
        &status_request,
    )
    .await
    .expect("查询状态");
    assert_eq!(status["status"], "running");
    assert!(status.get("owner").is_none());

    CapabilityState::call_agent_runtime_with(
        permissions.clone(),
        Some(binding.clone()),
        &json!({
            "operation": "steer",
            "request": {"target": child.to_string(), "input": "实时补充"},
        })
        .to_string(),
    )
    .await
    .expect("向运行中 Agent 注入消息");
    let events = CapabilityState::call_agent_runtime_with(
        permissions.clone(),
        Some(binding.clone()),
        &json!({
            "operation": "events",
            "request": {"target": child.to_string(), "limit": 32},
        })
        .to_string(),
    )
    .await
    .expect("轮询 Agent 事件");
    assert_eq!(events, json!([]));

    let cancelled = CapabilityState::call_agent_runtime_with(
        permissions,
        Some(binding),
        &json!({
            "operation": "cancel",
            "request": {"target": child.to_string()},
        })
        .to_string(),
    )
    .await
    .expect("取消派生 Agent");
    assert_eq!(cancelled, json!(true));
}

/// Manifest 未授权的 profile 必须在调用 Runtime 前被 Host 拒绝。
#[tokio::test]
async fn agent_runtime_rejects_ungranted_spawn_profile() {
    let (permissions, binding, _, _) = agent_runtime_fixture();
    let error = CapabilityState::call_agent_runtime_with(
        permissions,
        Some(binding),
        r#"{"operation":"spawn","request":{"profile":"admin","input":"任务"}}"#,
    )
    .await
    .expect_err("未授权 profile 必须失败");

    assert!(error.to_string().contains("manifest 未授权"));
    assert!(is_capability_denied(&error));
}

/// Runtime 调用信封不得接受 Guest 伪造的 principal 或 caller 字段。
#[tokio::test]
async fn agent_runtime_rejects_forged_identity_fields() {
    let (permissions, binding, _, _) = agent_runtime_fixture();
    let error = CapabilityState::call_agent_runtime_with(
        permissions,
        Some(binding),
        r#"{"operation":"identity","principal":"forged"}"#,
    )
    .await
    .expect_err("伪造 principal 必须失败");

    assert!(error.to_string().contains("解析插件宿主能力请求失败"));
}

/// Guest 请求体中的 caller_id 不得覆盖 Host 从当前 Store 注入的可信插件 ID。
#[tokio::test]
async fn service_call_ignores_forged_caller_id() {
    let observer = Arc::new(InMemoryHostServiceCallObserver::new());
    let services = Arc::new(ServiceRegistry::new(Some(observer.clone())));
    services
        .register_handler(
            "provider",
            std::collections::HashSet::new(),
            Arc::new(CallerEchoService),
        )
        .expect("注册测试服务处理器");
    services
        .register_handler(
            "trusted-consumer",
            std::collections::HashSet::from(["provider".to_string()]),
            Arc::new(CallerEchoService),
        )
        .expect("注册测试调用方处理器");
    services
        .upsert(
            "provider",
            PluginService {
                plugin_id: String::new(),
                name: "identity.echo".into(),
                version: "1.0.0".into(),
                description: None,
            },
        )
        .expect("注册测试服务");

    let response = CapabilityState::call_service_with(
        "trusted-consumer".into(),
        services,
        r#"{"plugin_id":"provider","name":"identity.echo","caller_id":"forged"}"#,
    )
    .await
    .expect("服务调用应成功");

    assert_eq!(response["caller_id"], "trusted-consumer");
    let observations = observer.snapshot();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].caller_id, "trusted-consumer");
    assert_eq!(observations[0].target_owner_id, "provider");
    assert_eq!(observations[0].service, "identity.echo");
    assert_eq!(
        observations[0].result,
        HostServiceCallResult::Succeeded {
            value_kind: JsonValueKind::Object
        }
    );
}

/// 构造申请了持久存储能力的测试用能力状态。
fn storage_state(
    root: &std::path::Path,
    plugin_id: &str,
    state_identity: &str,
    workspace: Option<&str>,
) -> CapabilityState {
    CapabilityState::new(
        plugin_id.into(),
        state_identity.into(),
        PathBuf::from("."),
        CapabilitySection {
            storage: true,
            ..CapabilitySection::default()
        },
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::new(
            None,
            None,
            ExecutionPolicy::serve(),
            Some(root.to_path_buf()),
            workspace.map(str::to_string),
            None,
        ),
    )
    .expect("创建持久存储能力状态")
}

/// 创建一次性测试目录。
fn storage_test_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lucia-plugin-store-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

/// 持久存储必须跨 Bundle 身份保留——插件升级不能清空用户数据。
///
/// 这正是它与私有 state 的分界：state 换代码即换命名空间，store 不换。
#[test]
fn durable_store_survives_bundle_identity_change() {
    let root = storage_test_root("upgrade");

    let mut original = storage_state(&root, "memory", "bundle-a", Some("project-a"));
    original
        .store_set(r#"{"scope":"workspace","key":"fact-1","value":"用户偏好 Bun"}"#)
        .expect("写入持久存储");

    // 代码字节变化会产生新的 Bundle 身份，私有 state 会因此清空。
    let mut upgraded = storage_state(&root, "memory", "bundle-b", Some("project-a"));
    assert_eq!(
        upgraded
            .store_get(r#"{"scope":"workspace","key":"fact-1"}"#)
            .expect("读取持久存储"),
        json!("用户偏好 Bun")
    );

    std::fs::remove_dir_all(root).expect("清理持久存储测试目录");
}

/// 不同项目的工作区存储必须互相不可见。
#[test]
fn workspace_store_is_isolated_by_project() {
    let root = storage_test_root("workspace");

    let mut first = storage_state(&root, "memory", "bundle-a", Some("project-a"));
    first
        .store_set(r#"{"scope":"workspace","key":"note","value":"甲项目"}"#)
        .expect("写入甲项目存储");

    let mut second = storage_state(&root, "memory", "bundle-a", Some("project-b"));
    assert_eq!(
        second
            .store_get(r#"{"scope":"workspace","key":"note"}"#)
            .expect("读取乙项目存储"),
        Value::Null
    );

    std::fs::remove_dir_all(root).expect("清理持久存储测试目录");
}

/// 不同插件的持久存储必须互相不可见。
#[test]
fn durable_store_is_isolated_by_plugin() {
    let root = storage_test_root("plugin");

    let mut owner = storage_state(&root, "memory", "bundle-a", Some("project-a"));
    owner
        .store_set(r#"{"scope":"user","key":"secret","value":"仅记忆插件可见"}"#)
        .expect("写入本插件存储");

    let mut other = storage_state(&root, "workflow", "bundle-a", Some("project-a"));
    assert_eq!(
        other
            .store_get(r#"{"scope":"user","key":"secret"}"#)
            .expect("读取其他插件存储"),
        Value::Null
    );

    std::fs::remove_dir_all(root).expect("清理持久存储测试目录");
}

/// 未声明 storage 能力的插件不得访问持久存储。
#[test]
fn store_requires_declared_capability() {
    let root = storage_test_root("denied");
    let mut state = CapabilityState::new(
        "memory".into(),
        "bundle-a".into(),
        PathBuf::from("."),
        CapabilitySection::default(),
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::new(
            None,
            None,
            ExecutionPolicy::serve(),
            Some(root.clone()),
            Some("project-a".into()),
            None,
        ),
    )
    .expect("创建未声明存储能力的状态");

    let error = state
        .store_get(r#"{"scope":"user","key":"any"}"#)
        .expect_err("未声明能力必须拒绝");
    assert!(error.to_string().contains("storage"), "{error}");
}

/// 宿主未提供项目标识时，按项目隔离的存储必须不可用而不是退化为跨项目共享。
#[test]
fn workspace_store_is_unavailable_without_project_scope() {
    let root = storage_test_root("no-scope");
    let mut state = storage_state(&root, "memory", "bundle-a", None);

    let error = state
        .store_set(r#"{"scope":"workspace","key":"note","value":"x"}"#)
        .expect_err("缺少项目作用域必须拒绝");
    assert!(error.to_string().contains("项目作用域"), "{error}");

    // 用户级存储仍然可用。
    state
        .store_set(r#"{"scope":"user","key":"note","value":"x"}"#)
        .expect("用户级存储应可用");

    std::fs::remove_dir_all(root).expect("清理持久存储测试目录");
}

/// 列键按字典序稳定返回，删除后不再出现。
#[test]
fn store_list_is_stable_and_reflects_removals() {
    let root = storage_test_root("list");
    let mut state = storage_state(&root, "memory", "bundle-a", Some("project-a"));
    for key in ["gamma", "alpha", "beta"] {
        state
            .store_set(&format!(r#"{{"scope":"user","key":"{key}","value":1}}"#))
            .expect("写入持久存储");
    }

    assert_eq!(
        state
            .store_list(r#"{"scope":"user"}"#)
            .expect("列出持久存储键"),
        json!(["alpha", "beta", "gamma"])
    );

    state
        .store_remove(r#"{"scope":"user","key":"beta"}"#)
        .expect("删除持久存储键");
    assert_eq!(
        state
            .store_list(r#"{"scope":"user"}"#)
            .expect("列出持久存储键"),
        json!(["alpha", "gamma"])
    );

    std::fs::remove_dir_all(root).expect("清理持久存储测试目录");
}

/// 返回固定会话的测试用只读来源。
struct StubSessionSource {
    id: String,
}

#[async_trait::async_trait]
impl crate::session::PluginSessionSource for StubSessionSource {
    async fn list_sessions(&self) -> anyhow::Result<Vec<crate::session::PluginSessionDescriptor>> {
        Ok(vec![crate::session::PluginSessionDescriptor {
            id: self.id.clone(),
            title: Some("历史会话".into()),
            created_at_ms: 1,
            updated_at_ms: 2,
            revision: 3,
            message_count: 1,
        }])
    }

    async fn load_session(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<agent_core::session::Session>> {
        if id != self.id {
            return Ok(None);
        }
        Ok(Some(agent_core::session::Session::from_parts(
            None,
            vec![agent_core::model::ModelMessage::text(
                agent_core::model::MessageRole::User,
                "历史提问",
            )],
        )))
    }
}

/// 构造声明了 sessions 能力的测试用能力状态。
fn session_state(sessions: bool) -> CapabilityState {
    CapabilityState::new(
        "knowledge".into(),
        "bundle-a".into(),
        PathBuf::from("."),
        CapabilitySection {
            sessions,
            ..CapabilitySection::default()
        },
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::new(
            None,
            None,
            ExecutionPolicy::serve(),
            None,
            None,
            Some(Arc::new(StubSessionSource {
                id: "session-a".into(),
            })),
        ),
    )
    .expect("创建会话能力状态")
}

/// 未声明 sessions 能力的插件不得读取任何历史会话。
#[tokio::test]
async fn session_access_requires_declared_capability() {
    let state = session_state(false);
    let (allowed, source) = state.session_context();

    let listed = CapabilityState::list_sessions_with(allowed, source.clone(), "{}")
        .await
        .expect_err("未声明能力必须拒绝列举");
    assert!(listed.to_string().contains("sessions"), "{listed}");

    let read = CapabilityState::read_session_with(allowed, source, r#"{"id":"session-a"}"#)
        .await
        .expect_err("未声明能力必须拒绝读取");
    assert!(read.to_string().contains("sessions"), "{read}");
}

/// 声明能力后可以列举摘要并按标识读取消息投影。
#[tokio::test]
async fn declared_capability_lists_and_reads_sessions() {
    let state = session_state(true);
    let (allowed, source) = state.session_context();

    let listed = CapabilityState::list_sessions_with(allowed, source.clone(), "{}")
        .await
        .expect("列举会话");
    assert_eq!(listed[0]["id"], "session-a");
    assert_eq!(listed[0]["message_count"], 1);

    let page = CapabilityState::read_session_with(allowed, source, r#"{"id":"session-a"}"#)
        .await
        .expect("读取会话");
    assert_eq!(page["messages"][0]["text"], "历史提问");
}

/// 未知会话返回 null 而不是错误，便于插件把"已删除"当作正常控制流处理。
#[tokio::test]
async fn reading_unknown_session_returns_null() {
    let state = session_state(true);
    let (allowed, source) = state.session_context();

    let page = CapabilityState::read_session_with(allowed, source, r#"{"id":"missing"}"#)
        .await
        .expect("未知会话不应报错");
    assert!(page.is_null());
}

/// 应用未注入会话来源时，即便 manifest 声明了能力也必须失败而不是返回空列表。
///
/// 返回空列表会让插件把"宿主没接线"误读成"这个项目没有历史会话"，
/// 从而静默建出一个空知识库。
#[tokio::test]
async fn missing_session_source_fails_instead_of_reporting_empty() {
    let error = CapabilityState::list_sessions_with(true, None, "{}")
        .await
        .expect_err("未注入来源必须失败");

    assert!(error.to_string().contains("未注入"), "{error}");
}
