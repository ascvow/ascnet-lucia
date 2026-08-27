//! 官方上下文管理插件的真实 WASM 端到端测试。

use agent_core::{
    model::ModelEventStream, Agent, AgentEventKind, AgentOptions, ChatModel, ContentBlock,
    ContextLoadRequest, ContextLoader, InMemoryEventSink, MessageRole, ModelGateway, ModelMessage,
    ModelRequest, ModelResponse, ProviderAdapter, Session, TokenUsage, ToolChoice,
};
use agent_evaluation::evaluate_context_policy_candidate;
use agent_evolution::diff_genomes;
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ContextEvaluationObservationV1, GateDecision, GenomeMetadata,
    GenomeRevision, ModelGenome, MutationSurface, PluginGenome, PolicyRef, PromptGenome,
    RecallObservationV1, RuntimeIdentity, ToolProfileGenome,
    CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION, GENOME_SCHEMA_VERSION,
};
use agent_plugin_host::{
    wasm::{load_wasm_plugins, load_wasm_plugins_with_services},
    PluginHostServices,
};
use agent_tool::ToolResult;
use agent_tool::{ExecutionPolicy, ToolAccess};
use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

/// 捕获 Agent 实际发送给模型的请求。
#[derive(Default)]
struct CapturingModel {
    requests: std::sync::Mutex<Vec<ModelRequest>>,
    complete_calls: AtomicUsize,
    stream_calls: AtomicUsize,
}

/// 在摘要请求中返回带根因错误的测试模型。
struct FailingSummaryModel;
/// 返回普通摘要但故意省略结构化标记信封的测试模型。
struct MissingMarkersModel;

impl CapturingModel {
    /// 根据请求类型构造摘要或主 Agent 的确定性响应。
    fn response_for(request: &ModelRequest) -> ModelResponse {
        let is_summary = request
            .system
            .as_deref()
            .is_some_and(|system| system.contains("负责压缩长对话"));
        let mut response = if is_summary {
            let marker_envelope = request
                .messages
                .last()
                .map(ModelMessage::text_content)
                .and_then(|prompt| extract_marker_envelope(&prompt));
            ModelResponse::text(format!(
                "<analysis>不应进入主上下文</analysis><summary>模型生成摘要：已分析旧上下文并保留关键状态。</summary>{}",
                marker_envelope.unwrap_or_default()
            ))
        } else {
            ModelResponse::text("上下文插件测试完成")
        };
        response.usage = Some(TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            total_tokens: Some(120),
        });
        response
    }

    /// 保存请求，供测试检查摘要与主 Agent 的调用顺序。
    fn record(&self, request: ModelRequest) {
        self.requests
            .lock()
            .expect("模型请求锁不应中毒")
            .push(request);
    }
}

#[async_trait]
impl ChatModel for CapturingModel {
    /// 保存请求并返回确定性文本，避免测试访问网络。
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        let response = Self::response_for(&request);
        self.record(request);
        Ok(response)
    }

    /// 保存流式请求，并返回只包含终态的测试事件流。
    async fn stream(&self, request: ModelRequest) -> ModelEventStream {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let response = Self::response_for(&request);
        self.record(request);
        let (sender, stream) = ModelEventStream::channel();
        sender.done(response);
        stream
    }
}

#[async_trait]
impl ProviderAdapter for CapturingModel {
    /// 返回测试路由使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "capturing"
    }
}

#[async_trait]
impl ChatModel for FailingSummaryModel {
    /// 模拟第三方模型网关拒绝摘要请求。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(anyhow::anyhow!("上游模型拒绝摘要请求"))
    }
}

#[async_trait]
impl ProviderAdapter for FailingSummaryModel {
    /// 返回失败模型使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "failing-summary"
    }
}

#[async_trait]
impl ChatModel for MissingMarkersModel {
    /// 返回不含 Context Policy 标记信封的非空摘要。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text(
            "<summary>摘要正文存在，但没有返回任何结构化确认标记。</summary>",
        ))
    }
}

#[async_trait]
impl ProviderAdapter for MissingMarkersModel {
    /// 返回缺标记模型使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "missing-markers"
    }
}

/// 从摘要请求提示中提取模型必须逐字回传的标记信封。
fn extract_marker_envelope(prompt: &str) -> Option<String> {
    const OPEN: &str = "<context-policy-markers>";
    const CLOSE: &str = "</context-policy-markers>";
    let start = prompt.find(OPEN)?;
    let relative_end = prompt[start..].find(CLOSE)?;
    let end = start + relative_end + CLOSE.len();
    Some(prompt[start..end].to_string())
}

/// 构造可在小型 smoke 输入上触发真实完整压缩的规范 M6 策略 JSON。
fn configured_policy_json() -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "micro_compact_threshold_tokens": 4096,
        "full_compact_threshold_tokens": 8192,
        "recent_message_count": 2,
        "tool_result_retention": {
            "preserve_errors_and_recent": { "recent_successful_results": 1 }
        },
        "user_constraints": {
            "pinned_structured_and_summary": { "max_items": 8 }
        },
        "plan_snapshot": {
            "latest_snapshot_and_summary": { "max_items": 8 }
        },
        "summary_token_budget": 512,
        "post_summary_validation": {
            "structured_marker_coverage_v1": { "min_coverage_bps": 10000 }
        }
    }))
    .expect("测试策略应编码为 JSON")
}

/// 按插件使用的 CAS 文本格式计算策略 JSON 摘要。
fn policy_digest(policy_json: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(policy_json.as_bytes()))
}

/// 为 context 插件注入一对完整、匹配的 Host 可信策略元数据。
fn inject_policy(services: PluginHostServices, policy_json: &str) -> PluginHostServices {
    services
        .with_activation_metadata(
            "context",
            HashMap::from([
                ("context_policy_json".into(), policy_json.into()),
                ("context_policy_digest".into(), policy_digest(policy_json)),
            ]),
        )
        .expect("Host 应接受完整策略激活元数据")
}

/// 加载真实 component，并验证结构化摘要、近期消息保留和事件发布完整链路。
#[tokio::test]
async fn component_replaces_agent_context_and_emits_event() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let model = Arc::new(CapturingModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let services = PluginHostServices::new()
        .with_model_completion(
            gateway.clone(),
            "capturing",
            "trusted-summary-model",
            20_000,
            false,
        )
        .expect("Host 应接受固定模型完成服务");
    let plugin_host = Arc::new(
        load_wasm_plugins_with_services(&[manifest], services)
            .await
            .expect("上下文替换 component 应加载成功"),
    );
    let events = Arc::new(InMemoryEventSink::new());
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "test-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host.clone())
    .with_event_sink(events.clone());

    let recent_request = "继续处理最新请求";
    let session = Session::from_parts(
        Some("保持准确".into()),
        vec![
            ModelMessage::text(MessageRole::User, "分析上下文压缩方式"),
            ModelMessage::text(MessageRole::Assistant, "已定位上下文管理入口"),
            ModelMessage::text(MessageRole::Tool, "x".repeat(520_000)),
            ModelMessage::text(MessageRole::Assistant, "已完成旧历史分析"),
            ModelMessage::text(MessageRole::User, recent_request),
            ModelMessage::text(MessageRole::Assistant, "正在继续处理"),
        ],
    );

    agent
        .run_session(session)
        .await
        .expect("Agent 应使用插件上下文完成运行");

    {
        let requests = model.requests.lock().expect("模型请求锁不应中毒");
        assert_eq!(requests.len(), 2);
        assert_eq!(model.complete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.stream_calls.load(Ordering::SeqCst), 1);
        let summary_request = &requests[0];
        assert_eq!(summary_request.model, "trusted-summary-model");
        assert_eq!(summary_request.tool_choice, ToolChoice::None);
        assert!(summary_request.tools.is_empty());
        assert_eq!(summary_request.max_tokens, Some(20_000));
        assert!(summary_request
            .messages
            .last()
            .is_some_and(|message| message.text_content().contains("尚未完成的任务")));

        let main_request = &requests[1];
        assert_eq!(main_request.model, "test-model");
        assert!(main_request.messages.len() < 6);
        assert!(main_request.messages[0]
            .text_content()
            .contains("模型生成摘要"));
        assert!(!main_request.messages[0]
            .text_content()
            .contains("不应进入主上下文"));
        assert!(main_request
            .messages
            .iter()
            .any(|message| message.text_content() == recent_request));
    }

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.compaction.completed"
            && event.payload["presentation"]["text"] == "上下文压缩"
    }));
}

/// Genome/Host 策略必须穿过真实 WASM 激活并驱动压缩、固定区、摘要预算和事件指标。
#[tokio::test]
async fn configured_policy_drives_wasm_compaction_and_preserves_structured_state() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let policy_json = configured_policy_json();
    let digest = policy_digest(&policy_json);
    let model = Arc::new(CapturingModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let services = inject_policy(
        PluginHostServices::new()
            .with_model_completion(
                gateway.clone(),
                "capturing",
                "trusted-policy-summary-model",
                20_000,
                false,
            )
            .expect("Host 应接受固定模型完成服务"),
        &policy_json,
    );
    let plugin_host = Arc::new(
        load_wasm_plugins_with_services(&[manifest], services)
            .await
            .expect("注入策略的 context component 应加载成功"),
    );
    let events = Arc::new(InMemoryEventSink::new());
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "main-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host)
    .with_event_sink(events.clone());

    let constraint_text = serde_json::json!({
        "schema": "lucia.context/user-constraint/v1",
        "id": "constraint-no-push",
        "value": { "rule": "禁止推送远端" }
    })
    .to_string();
    let fact_text = serde_json::json!({
        "schema": "lucia.context/fact/v1",
        "id": "fact-worktree-dirty",
        "value": { "dirty": true }
    })
    .to_string();
    let plan_state = serde_json::json!({
        "schema_version": 1,
        "revision": 7,
        "explanation": "执行 M6 smoke",
        "plan": [
            { "step": "装配策略", "status": "completed" },
            { "step": "验证固定区", "status": "in_progress" }
        ]
    });
    let constraint_message = ModelMessage::text(MessageRole::User, constraint_text);
    let plan_message =
        ModelMessage::tool_result(ToolResult::success("plan-call", "update_plan", plan_state));
    let messages = vec![
        constraint_message.clone(),
        ModelMessage::text(MessageRole::User, fact_text),
        ModelMessage::assistant_tool_calls(vec![agent_tool::ToolCall::new(
            "plan-call",
            "update_plan",
            serde_json::json!({}),
        )]),
        plan_message.clone(),
        ModelMessage::text(MessageRole::Tool, "x".repeat(30_000)),
        ModelMessage::text(MessageRole::Assistant, "旧轮次处理完成"),
        ModelMessage::text(MessageRole::User, "继续执行最新步骤"),
    ];

    agent
        .run_session(Session::from_parts(None, messages))
        .await
        .expect("配置策略应完成真实摘要和主模型请求");

    let final_message_count = {
        let requests = model.requests.lock().expect("模型请求锁不应中毒");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].model, "trusted-policy-summary-model");
        assert_eq!(requests[0].max_tokens, Some(512));
        let marker_prompt = requests[0]
            .messages
            .last()
            .expect("摘要请求应包含标记要求")
            .text_content();
        assert!(marker_prompt.contains("constraint-no-push"));
        assert!(marker_prompt.contains("fact-worktree-dirty"));
        assert!(marker_prompt.contains("\"plan_revision\":7"));
        let main_request = &requests[1];
        assert!(main_request.messages.contains(&constraint_message));
        assert!(main_request.messages.contains(&plan_message));
        main_request.messages.len()
    };

    let recorded = events.events().await;
    let event = recorded
        .iter()
        .find(|event| {
            event.kind == AgentEventKind::Extension
                && event.payload["name"] == "context.compaction.completed"
        })
        .expect("应发布策略压缩事件");
    assert_eq!(event.payload["data"]["policy_digest"], digest);
    assert_eq!(event.payload["data"]["policy_verified"], true);
    assert_eq!(event.payload["data"]["preservation_verified"], true);
    assert_eq!(event.payload["data"]["preserved_user_constraints"], 1);
    assert_eq!(event.payload["data"]["preserved_plan_snapshots"], 1);
    assert_eq!(event.payload["data"]["summary_max_output_tokens"], 512);
    assert_eq!(event.payload["data"]["after_messages"], final_message_count);
}

/// 按其他插件 ID 注入的元数据不能泄漏给真实 context component。
#[tokio::test]
async fn activation_metadata_is_isolated_by_plugin_id_in_real_wasm() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let policy_json = configured_policy_json();
    let services = PluginHostServices::new()
        .with_activation_metadata(
            "other-plugin",
            HashMap::from([
                ("context_policy_json".into(), policy_json.clone()),
                ("context_policy_digest".into(), policy_digest(&policy_json)),
            ]),
        )
        .expect("Host 应接受其他插件的隔离元数据");
    let plugin_host = load_wasm_plugins_with_services(&[manifest], services)
        .await
        .expect("context component 不应收到其他插件配置");
    let source = vec![
        ModelMessage::text(MessageRole::User, "x".repeat(30_000)),
        ModelMessage::text(MessageRole::Assistant, "旧轮次"),
        ModelMessage::text(MessageRole::User, "当前请求"),
    ];
    let loaded = ContextLoader::load(
        &plugin_host,
        ContextLoadRequest {
            run_id: "metadata-isolation".into(),
            step: 0,
            provider: "test".into(),
            model: "test".into(),
            system: None,
            messages: source.clone(),
            user_initiated: false,
        },
    )
    .await
    .expect("未命中目标插件的策略不应改变默认透传");
    assert_eq!(loaded.messages, source);
}

/// Host 注入摘要被篡改时，Guest 激活必须在任何上下文请求前失败关闭。
#[tokio::test]
async fn component_rejects_tampered_policy_digest_during_activation() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let policy_json = configured_policy_json();
    let services = PluginHostServices::new()
        .with_activation_metadata(
            "context",
            HashMap::from([
                ("context_policy_json".into(), policy_json),
                (
                    "context_policy_digest".into(),
                    format!("sha256:{}", "0".repeat(64)),
                ),
            ]),
        )
        .expect("Host 通用层不解释业务摘要");
    let error = match load_wasm_plugins_with_services(&[manifest], services).await {
        Ok(_) => panic!("Guest 必须拒绝篡改的 Context Policy 摘要"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("Context Policy 摘要不匹配"));
}

/// 配置策略要求结构化验证时，摘要模型遗漏标记必须终止真实 WASM 上下文加载。
#[tokio::test]
async fn component_fails_closed_when_summary_markers_are_missing() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let policy_json = configured_policy_json();
    let mut gateway = ModelGateway::new();
    gateway
        .register("missing", Arc::new(MissingMarkersModel))
        .expect("缺标记模型应注册成功");
    let services = inject_policy(
        PluginHostServices::new()
            .with_model_completion(gateway, "missing", "missing-model", 20_000, false)
            .expect("Host 应接受缺标记测试模型"),
        &policy_json,
    );
    let plugin_host = load_wasm_plugins_with_services(&[manifest], services)
        .await
        .expect("component 激活应成功");
    let error = ContextLoader::load(
        &plugin_host,
        ContextLoadRequest {
            run_id: "missing-markers".into(),
            step: 0,
            provider: "missing".into(),
            model: "missing-model".into(),
            system: None,
            messages: vec![
                ModelMessage::text(MessageRole::User, "x".repeat(30_000)),
                ModelMessage::text(MessageRole::Assistant, "旧轮次"),
                ModelMessage::text(MessageRole::User, "当前请求"),
            ],
            user_initiated: false,
        },
    )
    .await
    .expect_err("缺少摘要标记必须失败关闭");
    assert!(format!("{error:#}").contains("缺少结构化标记信封"));
}

/// Gate E2E 使用的确定性摘要模型；事实值来自真实摘要输入，标记信封来自插件请求。
#[derive(Default)]
struct GateSummaryModel {
    /// Provider 实际返回的总 token 用量，由可信测试 Runner 读取为成本输入。
    usage_tokens: AtomicU64,
}

#[async_trait]
impl ChatModel for GateSummaryModel {
    /// 从请求内显式事实标记生成摘要，并逐字回传插件要求的验证信封。
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let facts = request
            .messages
            .iter()
            .flat_map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(ContentBlock::text)
                    .filter(|text| {
                        serde_json::from_str::<serde_json::Value>(text)
                            .ok()
                            .is_some_and(|value| {
                                value.get("schema").and_then(serde_json::Value::as_str)
                                    == Some("lucia.context/fact/v1")
                            })
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let marker_envelope = request
            .messages
            .last()
            .map(ModelMessage::text_content)
            .and_then(|prompt| extract_marker_envelope(&prompt))
            .ok_or_else(|| anyhow::anyhow!("Gate 摘要请求缺少标记信封"))?;
        let input_tokens = serde_json::to_vec(&request.messages)?.len().div_ceil(3) as u64;
        let output_tokens = 32u64;
        self.usage_tokens
            .fetch_add(input_tokens + output_tokens, Ordering::SeqCst);
        let mut response = ModelResponse::text(format!(
            "<summary>已验证的结构化事实：{}</summary>{marker_envelope}",
            facts.join("\n")
        ));
        response.usage = Some(TokenUsage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            total_tokens: Some(input_tokens + output_tokens),
        });
        Ok(response)
    }
}

#[async_trait]
impl ProviderAdapter for GateSummaryModel {
    /// 返回 M6 Gate smoke 使用的稳定适配器名称。
    fn name(&self) -> &'static str {
        "gate-summary"
    }
}

/// 同一长 fixture 中由可信 Verifier 逐值检查的必需状态。
struct GateFixture {
    source: Vec<ModelMessage>,
    fact_value: String,
    constraint: ModelMessage,
    tool_states: Vec<ModelMessage>,
    plan: ModelMessage,
    downstream: ModelMessage,
}

/// 构造包含显式事实、约束、工具状态、Plan 和下游任务的长上下文。
fn gate_fixture() -> GateFixture {
    let fact_value = serde_json::json!({
        "schema": "lucia.context/fact/v1",
        "id": "fact-config-path",
        "value": { "path": "src/config.rs", "exists": true }
    })
    .to_string();
    let constraint = ModelMessage::text(
        MessageRole::User,
        serde_json::json!({
            "schema": "lucia.context/user-constraint/v1",
            "id": "constraint-read-only",
            "value": { "mode": "read_only" }
        })
        .to_string(),
    );
    let read_call = ModelMessage::assistant_tool_calls(vec![agent_tool::ToolCall::new(
        "read-call",
        "read_file",
        serde_json::json!({ "path": "src/config.rs" }),
    )]);
    let read_result = ModelMessage::tool_result(ToolResult::success(
        "read-call",
        "read_file",
        serde_json::json!({ "path": "src/config.rs", "status": "verified" }),
    ));
    let plan_call = ModelMessage::assistant_tool_calls(vec![agent_tool::ToolCall::new(
        "plan-call",
        "update_plan",
        serde_json::json!({}),
    )]);
    let plan = ModelMessage::tool_result(ToolResult::success(
        "plan-call",
        "update_plan",
        serde_json::json!({
            "schema_version": 1,
            "revision": 9,
            "plan": [
                { "step": "读取配置", "status": "completed" },
                { "step": "验证结果", "status": "in_progress" }
            ]
        }),
    ));
    let downstream = ModelMessage::text(
        MessageRole::User,
        serde_json::json!({
            "schema": "lucia.context/downstream-task/v1",
            "id": "downstream-report",
            "value": { "action": "report_verified_path" }
        })
        .to_string(),
    );
    let source = vec![
        ModelMessage::text(MessageRole::User, fact_value.clone()),
        constraint.clone(),
        read_call.clone(),
        read_result.clone(),
        plan_call.clone(),
        plan.clone(),
        ModelMessage::text(MessageRole::Assistant, "已收集第一批证据"),
        ModelMessage::text(MessageRole::Tool, "a".repeat(30_000)),
        ModelMessage::text(MessageRole::Assistant, "已收集第二批证据"),
        ModelMessage::text(MessageRole::Tool, "b".repeat(30_000)),
        ModelMessage::text(MessageRole::Assistant, "开始整理报告"),
        ModelMessage::text(MessageRole::User, "保持所有验证状态"),
        ModelMessage::text(MessageRole::Assistant, "准备处理下游任务"),
        downstream.clone(),
    ];
    GateFixture {
        source,
        fact_value,
        constraint,
        tool_states: vec![read_result, plan.clone()],
        plan,
        downstream,
    }
}

/// 构造仅在近期消息数量上不同的合法 Parent/Candidate Context Policy。
fn gate_policy_json(recent_message_count: u16) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "micro_compact_threshold_tokens": 4096,
        "full_compact_threshold_tokens": 8192,
        "recent_message_count": recent_message_count,
        "tool_result_retention": "preserve_all",
        "user_constraints": {
            "pinned_structured_and_summary": { "max_items": 8 }
        },
        "plan_snapshot": {
            "latest_snapshot_and_summary": { "max_items": 8 }
        },
        "summary_token_budget": 512,
        "post_summary_validation": {
            "structured_marker_coverage_v1": { "min_coverage_bps": 10000 }
        }
    }))
    .expect("Gate 策略应编码为 JSON")
}

/// 把策略 JSON 摘要转换为 Genome 使用的强类型 ArtifactDigest。
fn artifact_digest_for_policy(policy_json: &str) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(policy_digest(policy_json).trim_start_matches("sha256:"))
        .expect("策略摘要应可进入 Genome")
}

/// 构造只允许 Context Policy 摘要变化的有效 Genome 修订对。
fn gate_genomes(parent_policy: &str, candidate_policy: &str) -> (GenomeRevision, GenomeRevision) {
    let genome = AgentGenome {
        schema_version: GENOME_SCHEMA_VERSION,
        runtime: RuntimeIdentity {
            package_version: "0.1.0".into(),
            git_commit: "m6-smoke".into(),
            git_dirty: false,
            target_triple: "aarch64-apple-darwin".into(),
            features: BTreeSet::from(["plugins".into()]),
        },
        model: ModelGenome {
            provider: "gate".into(),
            provider_kind: "fixture".into(),
            model: "gate-model".into(),
            base_url: None,
            protocol: None,
            max_tokens: Some(512),
            temperature: None,
            stream: false,
            provider_options_digest: None,
        },
        prompt: PromptGenome::default(),
        plugins: vec![PluginGenome {
            id: "context".into(),
            version: "0.1.0".into(),
            api_version: "0.7.0".into(),
            bundle: ArtifactDigest::from_sha256_hex("c".repeat(64))
                .expect("测试 bundle 摘要应合法"),
            config_digest: None,
        }],
        capability_owners: [("agent.context-loader".into(), "context".into())]
            .into_iter()
            .collect(),
        tools: ToolProfileGenome {
            native_tools: BTreeSet::new(),
            access: ToolAccess::All,
        },
        context_policy: Some(PolicyRef {
            id: "context".into(),
            config_digest: artifact_digest_for_policy(parent_policy),
        }),
        planning_policy: None,
        skills: Vec::new(),
        execution: ExecutionPolicy::serve(),
    };
    let parent =
        GenomeRevision::create(genome, GenomeMetadata::default()).expect("Parent Genome 应合法");
    let mut candidate_genome = parent.genome.clone();
    candidate_genome
        .context_policy
        .as_mut()
        .expect("Candidate 应保留 Context Policy")
        .config_digest = artifact_digest_for_policy(candidate_policy);
    let candidate = GenomeRevision::create(candidate_genome, GenomeMetadata::default())
        .expect("Candidate Genome 应合法");
    (parent, candidate)
}

/// 使用真实 WASM Host 运行一份策略，并从最终上下文和实际 Provider 观测构造 Gate 输入。
async fn run_gate_policy(
    policy_json: &str,
    fixture: &GateFixture,
) -> (ContextEvaluationObservationV1, Vec<ModelMessage>) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let model = Arc::new(GateSummaryModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("gate", model.clone())
        .expect("Gate 摘要模型应注册成功");
    let services = inject_policy(
        PluginHostServices::new()
            .with_model_completion(gateway, "gate", "gate-summary-model", 20_000, false)
            .expect("Host 应接受 Gate 摘要模型"),
        policy_json,
    );
    let plugin_host = load_wasm_plugins_with_services(&[manifest], services)
        .await
        .expect("Gate context component 应加载成功");
    let started = Instant::now();
    let loaded = ContextLoader::load(
        &plugin_host,
        ContextLoadRequest {
            run_id: format!("gate-{}", policy_digest(policy_json)),
            step: 0,
            provider: "gate".into(),
            model: "gate-model".into(),
            system: None,
            messages: fixture.source.clone(),
            user_initiated: false,
        },
    )
    .await
    .expect("Gate 策略应产生已验证上下文");
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let observation = verify_gate_context(
        fixture,
        &loaded.messages,
        model.usage_tokens.load(Ordering::SeqCst),
        elapsed_ms,
    );
    (observation, loaded.messages)
}

/// 可信测试 Verifier 只按最终上下文中的真实值生成召回与资源观察。
fn verify_gate_context(
    fixture: &GateFixture,
    loaded: &[ModelMessage],
    usage_tokens: u64,
    elapsed_ms: u64,
) -> ContextEvaluationObservationV1 {
    let exact_recall = |expected: &[ModelMessage]| RecallObservationV1 {
        expected: expected.len() as u64,
        recalled: expected
            .iter()
            .filter(|item| loaded.iter().any(|actual| actual == *item))
            .count() as u64,
    };
    let facts = RecallObservationV1 {
        expected: 1,
        recalled: u64::from(
            loaded
                .iter()
                .any(|message| message.text_content().contains(&fixture.fact_value)),
        ),
    };
    let constraints = exact_recall(std::slice::from_ref(&fixture.constraint));
    let tool_states = exact_recall(&fixture.tool_states);
    let plan_states = exact_recall(std::slice::from_ref(&fixture.plan));
    let downstream_tasks = exact_recall(std::slice::from_ref(&fixture.downstream));
    let tokens_before = serde_json::to_vec(&fixture.source)
        .expect("源上下文应可编码")
        .len()
        .div_ceil(3) as u64;
    let tokens_after = serde_json::to_vec(loaded)
        .expect("最终上下文应可编码")
        .len()
        .div_ceil(3) as u64;
    ContextEvaluationObservationV1 {
        schema_version: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
        facts,
        constraints,
        tool_states,
        plan_states,
        downstream_tasks,
        tokens_before,
        tokens_after,
        cost_microunits: usage_tokens,
        latency_ms: elapsed_ms,
    }
}

/// Parent/Candidate 必须通过真实 Context Host 形成观察，再由 Evaluation Gate 作最终判定。
#[tokio::test]
async fn parent_candidate_real_wasm_context_gate_passes() {
    let fixture = gate_fixture();
    let parent_policy = gate_policy_json(4);
    let candidate_policy = gate_policy_json(2);
    let (parent, candidate) = gate_genomes(&parent_policy, &candidate_policy);
    let changed = diff_genomes(&parent, &candidate)
        .expect("应计算真实 Genome Diff")
        .changed_surfaces;
    assert_eq!(changed, BTreeSet::from([MutationSurface::ContextPolicy]));

    let (parent_observation, parent_context) = run_gate_policy(&parent_policy, &fixture).await;
    let (candidate_observation, candidate_context) =
        run_gate_policy(&candidate_policy, &fixture).await;
    assert!(candidate_context.len() <= parent_context.len());
    assert!(candidate_observation.tokens_after <= parent_observation.tokens_after);

    let report = evaluate_context_policy_candidate(
        &parent,
        &candidate,
        &parent_observation,
        &candidate_observation,
    )
    .expect("真实 Parent/Candidate 观察应产生 M6 Gate 报告");
    assert_eq!(report.decision, GateDecision::Pass);
    assert!(report.failures.is_empty());
    assert_eq!(report.parent_metrics.fact_recall_bps, 10_000);
    assert_eq!(report.candidate_metrics.fact_recall_bps, 10_000);
    assert_eq!(report.candidate_metrics.constraint_recall_bps, 10_000);
    assert_eq!(report.candidate_metrics.tool_state_recall_bps, 10_000);
    assert_eq!(report.candidate_metrics.plan_state_recall_bps, 10_000);
    assert_eq!(report.candidate_metrics.downstream_task_success_bps, 10_000);
    assert!(
        report.candidate_metrics.token_reduction_bps >= report.parent_metrics.token_reduction_bps
    );
    assert_eq!(
        report.candidate_metrics.cost_microunits,
        candidate_observation.cost_microunits
    );
    assert_eq!(
        report.candidate_metrics.latency_ms,
        candidate_observation.latency_ms
    );
}

/// 摘要模型失败时，WASM 双边信封必须把底层 provider 原因传回 Core。
#[tokio::test]
async fn component_preserves_summary_model_error_chain() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let mut gateway = ModelGateway::new();
    gateway
        .register("failing", Arc::new(FailingSummaryModel))
        .expect("失败模型应注册成功");
    let services = PluginHostServices::new()
        .with_model_completion(gateway, "failing", "failing-model", 20_000, false)
        .expect("Host 应接受失败模型服务");
    let plugin_host = load_wasm_plugins_with_services(&[manifest], services)
        .await
        .expect("上下文 component 应加载成功");
    let session = Session::from_parts(
        None,
        vec![
            ModelMessage::text(MessageRole::User, "分析旧上下文"),
            ModelMessage::text(MessageRole::Assistant, "开始分析"),
            ModelMessage::text(MessageRole::Tool, "x".repeat(520_000)),
            ModelMessage::text(MessageRole::Assistant, "继续处理"),
            ModelMessage::text(MessageRole::User, "保留当前请求"),
        ],
    );
    let request = ContextLoadRequest {
        run_id: "failing-summary-run".into(),
        user_initiated: false,
        step: 0,
        provider: "failing".into(),
        model: "failing-model".into(),
        system: None,
        messages: session.model_messages(),
    };

    let error = ContextLoader::load(&plugin_host, request)
        .await
        .expect_err("摘要模型失败必须终止上下文加载");
    let error = format!("{error:#}");
    assert!(error.contains("调用模型生成上下文摘要失败"));
    assert!(error.contains("上游模型拒绝摘要请求"));
}

/// 短会话低于压缩水位时插件返回透传，Agent 必须继续使用原始上下文而不是报错。
#[tokio::test]
async fn component_passes_through_short_context_without_error() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("上下文透传 component 应加载成功"),
    );
    let model = Arc::new(CapturingModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "test-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host.clone());

    let session = Session::from_parts(
        Some("保持准确".into()),
        vec![ModelMessage::text(MessageRole::User, "你好")],
    );
    agent
        .run_session(session)
        .await
        .expect("短会话应透传原始上下文完成运行");

    let requests = model.requests.lock().expect("模型请求锁不应中毒");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].system.as_deref(), Some("保持准确"));
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[0].messages[0].text_content(), "你好");
}

/// 验证微压缩后的工具结果仍可通过真实 WASM 宿主进入模型请求。
#[tokio::test]
async fn component_micro_compacts_tool_results_without_breaking_model_request() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = Arc::new(
        load_wasm_plugins(&[manifest])
            .await
            .expect("上下文微压缩 component 应加载成功"),
    );
    let model = Arc::new(CapturingModel::default());
    let mut gateway = ModelGateway::new();
    gateway
        .register("capturing", model.clone())
        .expect("捕获模型应注册成功");
    let events = Arc::new(InMemoryEventSink::new());
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("capturing", "test-model"),
    )
    .with_extension(plugin_host.clone())
    .with_context_loader(plugin_host.clone())
    .with_event_sink(events.clone());

    let mut messages = vec![ModelMessage::text(MessageRole::User, "分析工具执行结果")];
    for index in 0..4 {
        messages.push(ModelMessage::assistant_tool_calls(vec![
            agent_tool::ToolCall::new(
                format!("call-{index}"),
                "read_file",
                serde_json::json!({ "path": format!("file-{index}.txt") }),
            ),
        ]));
        messages.push(ModelMessage::tool_result(ToolResult::success(
            format!("call-{index}"),
            "read_file",
            serde_json::json!("x".repeat(100_000)),
        )));
    }
    messages.push(ModelMessage::text(MessageRole::User, "继续处理当前任务"));

    agent
        .run_session(Session::from_parts(None, messages))
        .await
        .expect("微压缩后的上下文应能完成模型请求");

    {
        let requests = model.requests.lock().expect("模型请求锁不应中毒");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { result }
                    if result.content.as_str() == Some("[旧工具结果内容已清理]"))
            })
        }));
    }

    let recorded = events.events().await;
    assert!(recorded.iter().any(|event| {
        event.kind == AgentEventKind::Extension
            && event.payload["name"] == "context.micro_compaction.completed"
            && event.payload["presentation"].is_null()
    }));
}

/// 验证微压缩后的同一运行追加消息不会因比较大 JSON 前缀耗尽 WASM fuel。
#[tokio::test]
async fn component_reuses_micro_compacted_context_within_fuel_budget() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let plugin_host = load_wasm_plugins(&[manifest])
        .await
        .expect("上下文缓存 component 应加载成功");
    let mut messages = vec![ModelMessage::text(MessageRole::User, "分析工具执行结果")];
    for index in 0..4 {
        messages.push(ModelMessage::assistant_tool_calls(vec![
            agent_tool::ToolCall::new(
                format!("cache-call-{index}"),
                "read_file",
                serde_json::json!({ "path": format!("cache-{index}.txt") }),
            ),
        ]));
        messages.push(ModelMessage::tool_result(ToolResult::success(
            format!("cache-call-{index}"),
            "read_file",
            serde_json::json!("x".repeat(100_000)),
        )));
    }

    let first = ContextLoadRequest {
        run_id: "cached-micro-run".into(),
        step: 0,
        user_initiated: false,
        provider: "capturing".into(),
        model: "test-model".into(),
        system: None,
        messages: messages.clone(),
    };
    ContextLoader::load(&plugin_host, first)
        .await
        .expect("首次微压缩不应耗尽 fuel");

    messages.push(ModelMessage::text(MessageRole::User, "继续处理新消息"));
    let second = ContextLoadRequest {
        run_id: "cached-micro-run".into(),
        step: 1,
        user_initiated: false,
        provider: "capturing".into(),
        model: "test-model".into(),
        system: None,
        messages,
    };
    let loaded = ContextLoader::load(&plugin_host, second)
        .await
        .expect("复用微压缩前缀不应耗尽 fuel");
    assert!(loaded
        .messages
        .iter()
        .any(|message| message.text_content() == "继续处理新消息"));
}
