//! 插件可用的受信模型补全与 Agent Runtime 调度。

use crate::capability::requests::{
    parse_agent_target, parse_request, require_empty_agent_request, AgentEventsRequest,
    AgentRuntimeCallRequest, GuestAgentContinueRequest, GuestAgentSpawnRequest,
    GuestModelCompletionRequest,
};
use crate::capability::{
    capability_denied, CapabilityState, MAX_AGENT_RUNTIME_REQUEST_BYTES,
    MAX_MODEL_COMPLETION_REQUEST_BYTES,
};
use crate::manifest::AgentCapabilitySection;
use crate::services::{AgentRuntimeHostServices, ModelCompletionHostServices};
use agent_core::{AgentEvent, AgentEventKind, ModelRequest, ToolChoice};
use agent_runtime::{
    AgentEventStream, AgentId, AgentRuntimeApi, AgentSpawnRequest, RuntimePrincipal,
};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

/// 单次 Guest 轮询最多检查的 Runtime 事件数，避免大量敏感增量造成长时间同步扫描。
const MAX_AGENT_EVENT_SCAN_PER_POLL: usize = 4_096;

/// Session 与扩展共享的稳定审批拒绝结果；完整原因只允许可信应用事件消费者读取。
const BLOCKED_TOOL_GUEST_MESSAGE: &str = "工具调用未获批准，未执行。";

/// 清洗允许跨 Guest 边界的 Runtime 事件。
///
/// 服务商私有推理增量会被直接丢弃；插件策略拒绝的工具结果保留调用身份与错误类别，
/// 但完整审批原因会替换为稳定文案，避免 `agent.observe` 绕过 Core 的扩展事件脱敏。
pub(super) fn sanitize_guest_visible_agent_event(mut event: AgentEvent) -> Option<AgentEvent> {
    if event.kind == AgentEventKind::ModelThinkingDelta {
        return None;
    }
    if event.kind == AgentEventKind::ToolFinished
        && event.payload["runtime_origin"] == "plugin_policy"
        && event.payload["error_kind"] == "permission_denied"
    {
        if let Some(payload) = event.payload.as_object_mut() {
            payload.insert(
                "content".into(),
                json!({ "error": BLOCKED_TOOL_GUEST_MESSAGE }),
            );
            payload.remove("details");
        }
    }
    Some(event)
}

/// 单个插件激活实例可使用的身份绑定 Agent Runtime。
#[derive(Clone)]
pub(crate) struct AgentRuntimeBinding {
    principal: RuntimePrincipal,
    api: Arc<dyn AgentRuntimeApi>,
    host_services: AgentRuntimeHostServices,
    event_streams: Arc<tokio::sync::Mutex<HashMap<AgentId, AgentEventStream>>>,
}

impl AgentRuntimeBinding {
    /// 创建与可信 principal、controller API 和 Host 策略注册表绑定的运行时上下文。
    pub(crate) fn new(
        principal: RuntimePrincipal,
        api: Arc<dyn AgentRuntimeApi>,
        host_services: AgentRuntimeHostServices,
    ) -> Self {
        Self {
            principal,
            api,
            host_services,
            event_streams: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// 复用目标订阅流并非阻塞取出一批已到达事件。
    async fn poll_events(
        &self,
        target: &AgentId,
        limit: usize,
    ) -> agent_runtime::RuntimeResult<Vec<AgentEvent>> {
        let needs_subscription = !self.event_streams.lock().await.contains_key(target);
        if needs_subscription {
            let stream = self.api.subscribe(target).await?;
            self.event_streams
                .lock()
                .await
                .insert(target.clone(), stream);
        }
        let mut streams = self.event_streams.lock().await;
        let stream = streams
            .get_mut(target)
            .expect("已创建的 Agent 事件订阅必须存在");
        let mut visible = Vec::with_capacity(limit);
        let scan_limit = limit
            .saturating_mul(16)
            .min(MAX_AGENT_EVENT_SCAN_PER_POLL)
            .max(limit);
        for _ in 0..scan_limit {
            if visible.len() >= limit {
                break;
            }
            let Some(event) = stream.try_next() else {
                break;
            };
            if let Some(event) = sanitize_guest_visible_agent_event(event) {
                visible.push(event);
            }
        }
        Ok(visible)
    }

    /// 返回与当前插件 principal 绑定的 Runtime API。
    ///
    /// 调用方只能凭该 API 访问本插件名下的 Agent；目标属于其他 owner 时 Runtime 会拒绝。
    pub(crate) fn api(&self) -> Arc<dyn AgentRuntimeApi> {
        Arc::clone(&self.api)
    }

    /// 撤销当前插件激活 principal，并取消、清理其 controller 与全部后代。
    pub(crate) async fn revoke(&self) -> usize {
        self.host_services.provisioner.revoke(&self.principal).await
    }
}

pub(crate) trait AgentCapabilityChecks {
    fn require_any(&self) -> Result<()>;
    fn require_spawn(&self) -> Result<()>;
    fn require_observe(&self) -> Result<()>;
    fn require_cancel(&self) -> Result<()>;
    fn require_collect(&self) -> Result<()>;
}

impl CapabilityState {
    /// 克隆 Agent Runtime 调度所需的权限和绑定，避免跨 `await` 借用 Wasmtime store。
    pub(crate) fn agent_runtime_context(
        &self,
    ) -> (AgentCapabilitySection, Option<AgentRuntimeBinding>) {
        (self.permissions.agent.clone(), self.agent_runtime.clone())
    }

    /// 克隆模型完成所需的 manifest 授权、应用绑定和调用方插件 ID，避免跨 `await` 借用 store。
    ///
    /// 插件 ID 参与缓存路由：同一插件的补全共用固定 system 前缀，按插件分组才能持续命中。
    pub(crate) fn model_completion_context(
        &self,
    ) -> (bool, Option<ModelCompletionHostServices>, String) {
        (
            self.permissions.model_completion,
            self.model_completion.clone(),
            self.plugin_id.clone(),
        )
    }

    /// 校验并执行一次固定路由、禁用工具的模型完成调用。
    pub(crate) async fn complete_model_with(
        allowed: bool,
        binding: Option<ModelCompletionHostServices>,
        plugin_id: &str,
        request_json: &str,
    ) -> Result<Value> {
        if !allowed {
            return Err(capability_denied("插件 manifest 未授权 model_completion"));
        }
        if request_json.len() > MAX_MODEL_COMPLETION_REQUEST_BYTES {
            return Err(anyhow!(
                "模型完成请求超过 {} 字节限制",
                MAX_MODEL_COMPLETION_REQUEST_BYTES
            ));
        }
        let request: GuestModelCompletionRequest = parse_request(request_json)?;
        if request.messages.is_empty() {
            return Err(anyhow!("模型完成消息不能为空"));
        }
        let binding = binding.ok_or_else(|| anyhow!("应用未注入模型完成服务"))?;
        binding
            .reserve(request_json.len())
            .map_err(capability_denied)?;

        // 路由的全部调用参数来自 Core 的模型配置；Host 只强制工具禁用与输出上限。
        let config = binding.config;
        let mut model_request = ModelRequest::new(config.model, request.messages);
        model_request.system = request.system;
        model_request.tool_choice = ToolChoice::None;
        model_request.max_tokens = Some(
            request
                .max_tokens
                .or(config.max_tokens)
                .unwrap_or(binding.max_output_tokens)
                .clamp(1, binding.max_output_tokens),
        );
        model_request.reasoning = config.reasoning;
        model_request.temperature = config.temperature;
        model_request.cache_retention = config.cache_retention;
        model_request.provider_options = config.provider_options;
        // 插件补全通常是"固定 system + 短小可变正文"的高频调用，前缀在多次调用之间完全
        // 相同，值得缓存。键按插件而不是按会话分组：同一插件的补全共享同一份 system 前缀
        // 缓存，同时与主会话的缓存分区分开，互不驱逐。
        model_request.session_id = Some(format!("plugin:{plugin_id}"));
        let response = if config.stream {
            binding
                .gateway
                .stream(&config.provider.name, model_request)
                .await
                .context("启动插件模型完成流失败")?
                .result()
                .await
                .context("插件模型完成调用失败")?
        } else {
            binding
                .gateway
                .complete(&config.provider.name, model_request)
                .await
                .context("插件模型非流式完成调用失败")?
        };
        if !response.tool_calls.is_empty() {
            return Err(capability_denied("模型完成服务返回了未授权工具调用"));
        }
        let text = response.text_content();
        if text.trim().is_empty() {
            return Err(anyhow!("模型完成服务返回了空文本"));
        }
        Ok(json!({
            "text": text,
            "usage": response.usage,
        }))
    }

    /// 解析、鉴权并委托一次 Agent Runtime 短控制面调用。
    pub(crate) async fn call_agent_runtime_with(
        permissions: AgentCapabilitySection,
        binding: Option<AgentRuntimeBinding>,
        request_json: &str,
    ) -> Result<Value> {
        if request_json.len() > MAX_AGENT_RUNTIME_REQUEST_BYTES {
            return Err(anyhow!(
                "Agent Runtime 请求超过 {} 字节限制",
                MAX_AGENT_RUNTIME_REQUEST_BYTES
            ));
        }
        let request: AgentRuntimeCallRequest = parse_request(request_json)?;
        let binding = binding.ok_or_else(|| anyhow!("应用未注入 Agent Runtime 服务"))?;
        match request.operation.as_str() {
            "identity" => {
                require_empty_agent_request(&request.request)?;
                permissions.require_any()?;
                Ok(serde_json::to_value(binding.api.identity())?)
            }
            "spawn" => {
                permissions.require_spawn()?;
                let request: GuestAgentSpawnRequest =
                    serde_json::from_value(request.request).context("解析 Agent spawn 请求失败")?;
                if request.input.trim().is_empty() {
                    return Err(anyhow!("Agent spawn 输入不能为空"));
                }
                if !permissions.allows_profile(&request.profile) {
                    return Err(capability_denied(format!(
                        "插件 manifest 未授权 Agent spawn profile `{}`",
                        request.profile
                    )));
                }
                let derive = binding
                    .host_services
                    .spawn_profile(&request.profile)
                    .ok_or_else(|| {
                        anyhow!("应用未注册 Agent spawn profile `{}`", request.profile)
                    })?;
                let handle = binding
                    .api
                    .spawn(AgentSpawnRequest {
                        input: request.input,
                        derive,
                    })
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(handle)?)
            }
            "continue" => {
                permissions.require_spawn()?;
                let request: GuestAgentContinueRequest = serde_json::from_value(request.request)
                    .context("解析 Agent continue 请求失败")?;
                if request.input.trim().is_empty() {
                    return Err(anyhow!("Agent continue 输入不能为空"));
                }
                let target =
                    AgentId::from_str(&request.target).context("解析 Agent continue 目标失败")?;
                let handle = binding
                    .api
                    .continue_agent(&target, request.input)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(handle)?)
            }
            "steer" => {
                permissions.require_spawn()?;
                let request: GuestAgentContinueRequest =
                    serde_json::from_value(request.request).context("解析 Agent steer 请求失败")?;
                if request.input.trim().is_empty() {
                    return Err(anyhow!("Agent steer 输入不能为空"));
                }
                let target =
                    AgentId::from_str(&request.target).context("解析 Agent steer 目标失败")?;
                binding
                    .api
                    .steer(&target, request.input)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(Value::Null)
            }
            "status" => {
                permissions.require_observe()?;
                let target = parse_agent_target(request.request)?;
                let snapshot = binding
                    .api
                    .status(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(json!({
                    "id": snapshot.id,
                    "lineage": snapshot.lineage,
                    "status": snapshot.status,
                    "permissions": snapshot.permissions,
                }))
            }
            "result" => {
                permissions.require_observe()?;
                let target = parse_agent_target(request.request)?;
                let result = binding
                    .api
                    .result(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(result)?)
            }
            "events" => {
                permissions.require_observe()?;
                let request: AgentEventsRequest = serde_json::from_value(request.request)
                    .context("解析 Agent events 请求失败")?;
                let target =
                    AgentId::from_str(&request.target).context("解析 Agent events 目标失败")?;
                let events = binding
                    .poll_events(&target, request.limit.clamp(1, 512))
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(events)?)
            }
            "cancel" => {
                permissions.require_cancel()?;
                let target = parse_agent_target(request.request)?;
                let cancelled = binding
                    .api
                    .cancel(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(json!(cancelled))
            }
            "collect" => {
                permissions.require_collect()?;
                let target = parse_agent_target(request.request)?;
                let collected = binding
                    .api
                    .collect(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                // 目标已回收，缓存的订阅流不再有意义；一并释放以免长期占用内存。
                binding.event_streams.lock().await.remove(&target);
                Ok(json!(collected))
            }
            operation => Err(anyhow!("未知 Agent Runtime 操作：`{operation}`")),
        }
    }
}
