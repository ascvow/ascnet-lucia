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
use crate::services::ModelCompletionHostServices;
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
        PathBuf::from("."),
        CapabilitySection {
            process_exec,
            ..CapabilitySection::default()
        },
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::new(None, None, execution_policy, None),
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
            PathBuf::from("."),
            CapabilitySection::default(),
            Arc::new(ContributionRegistry::default()),
            Arc::new(ServiceRegistry::default()),
            CapabilityHostContext::new(None, None, ExecutionPolicy::serve(), Some(root.clone())),
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

/// 发布事件时 Host 必须覆盖可信的插件来源。
#[test]
fn emitted_event_contains_trusted_source() {
    let contributions = Arc::new(ContributionRegistry::default());
    let mut state = CapabilityState::new(
        "trusted-id".into(),
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
    let binding = ModelCompletionHostServices {
        gateway,
        provider: "trusted-provider".into(),
        model: "trusted-model".into(),
        max_output_tokens: 128,
        stream: true,
    };

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
    let binding = ModelCompletionHostServices {
        gateway,
        provider: "trusted-provider".into(),
        model: "trusted-model".into(),
        max_output_tokens: 20_000,
        stream: false,
    };

    CapabilityState::complete_model_with(
        true,
        Some(binding),
        r#"{"messages":[{"role":"user","content":[{"type":"text","text":"旧上下文"}]}]}"#,
    )
    .await
    .expect("非流式模型完成应成功");

    assert_eq!(requests.lock().expect("锁定模型请求").len(), 1);
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
        .register_handler("provider", Arc::new(CallerEchoService))
        .expect("注册测试服务处理器");
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
