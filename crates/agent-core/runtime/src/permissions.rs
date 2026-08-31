//! Agent 派生模板、选项覆盖与单向权限收缩。

use crate::{AgentRuntimeError, RuntimeResult};
use agent_core::{
    Agent, AgentEvent, AgentExtension, AgentOptions, ContextLoader, EventSink, ModelGateway,
    ModelMessage, ReasoningLevel, ToolChoice, ToolDecision,
};
use agent_tool::{ToolCall, ToolRegistry, ToolResult, ToolSpec};
use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// 工具访问范围。
///
/// 定义已上移到 `agent-tool`，使派生 Agent 的 allowlist 与 Execution Profile 的
/// 工具门禁共用同一套收缩语义。此处重导出以保持既有调用路径不变。
pub use agent_tool::ToolAccess;

/// Agent 可继承和收缩的权限集合。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermissions {
    /// 模型可见且可实际执行的工具范围。
    #[serde(default)]
    pub tools: ToolAccess,
}

impl AgentPermissions {
    /// 在当前权限上应用子节点请求并返回有效权限。
    pub fn restrict(&self, requested: &Self) -> Self {
        Self {
            tools: self.tools.restrict(&requested.tools),
        }
    }
}

/// 对 Core [`AgentOptions`] 的可序列化增量覆盖。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentOptionsPatch {
    /// 覆盖逻辑服务商名称。
    pub provider: Option<String>,
    /// 覆盖模型名称。
    pub model: Option<String>,
    /// 覆盖最大 ReAct 步数；`0` 表示不设置总步数上限。
    pub max_steps: Option<usize>,
    /// 覆盖系统提示词。
    pub system_prompt: Option<String>,
    /// 覆盖工具选择模式。
    pub tool_choice: Option<ToolChoice>,
    /// 覆盖最大输出 token 数。
    pub max_tokens: Option<u32>,
    /// 覆盖是否使用模型流式接口。
    pub stream: Option<bool>,
    /// 覆盖采样温度。
    pub temperature: Option<f32>,
    /// 覆盖推理级别。
    pub reasoning: Option<ReasoningLevel>,
    /// 覆盖服务商专属请求选项。
    pub provider_options: Option<Value>,
}

impl AgentOptionsPatch {
    /// 将非空字段应用到现有 Core 运行选项。
    pub fn apply_to(&self, options: &mut AgentOptions) {
        if let Some(value) = &self.provider {
            options.provider = value.clone();
        }
        if let Some(value) = &self.model {
            options.model = value.clone();
        }
        if let Some(value) = self.max_steps {
            options.max_steps = value;
        }
        if let Some(value) = &self.system_prompt {
            options.system_prompt = value.clone();
        }
        if let Some(value) = &self.tool_choice {
            options.tool_choice = value.clone();
        }
        if let Some(value) = self.max_tokens {
            options.max_tokens = Some(value);
        }
        if let Some(value) = self.stream {
            options.stream = value;
        }
        if let Some(value) = self.temperature {
            options.temperature = Some(value);
        }
        if let Some(value) = self.reasoning {
            options.reasoning = value;
        }
        if let Some(value) = &self.provider_options {
            options.provider_options = value.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeLimits;
    use agent_tool::ExecutionPolicy;

    /// 执行策略应收紧 Runtime 限额，且不会放宽默认值。
    #[test]
    fn runtime_limits_are_clamped_by_execution_policy() {
        let defaults = RuntimeLimits::default();
        let evaluation = ExecutionPolicy::evaluation("/tmp/fixture");
        let clamped = defaults.clamped_by(&evaluation.limits);

        assert_eq!(clamped.max_depth, 2);
        assert_eq!(clamped.max_children_per_agent, 4);
        assert_eq!(clamped.max_concurrent_agents, 2);
        assert_eq!(clamped.max_agents_per_principal, 16);
        assert_eq!(clamped.max_agent_model_requests_per_principal, 128);
        assert_eq!(
            clamped.max_agent_model_request_bytes_per_principal,
            16 * 1024 * 1024
        );
        assert_eq!(
            clamped.max_agent_model_output_tokens_per_principal,
            128 * 4096
        );
        clamped.validate().expect("收紧后的限额仍应合法");
    }

    /// Mutation 策略应完全阻断派生，同时保持正数不变量。
    #[test]
    fn mutation_policy_blocks_derivation_but_stays_valid() {
        let clamped = RuntimeLimits::default().clamped_by(&ExecutionPolicy::mutation().limits);

        assert_eq!(clamped.max_depth, 0);
        assert_eq!(clamped.max_children_per_agent, 1);
        assert_eq!(clamped.max_agents_per_principal, 1);
        clamped.validate().expect("限额必须通过校验");
    }

    /// 策略未声明某维度上限时保持 Runtime 原值。
    #[test]
    fn serve_policy_leaves_runtime_limits_untouched() {
        let defaults = RuntimeLimits::default();
        let clamped = defaults.clamped_by(&ExecutionPolicy::serve().limits);

        assert_eq!(clamped, defaults);
    }

    /// 旧版 JSON 缺少累计资源字段时必须补入有限默认值，不能退化为无限制。
    #[test]
    fn legacy_runtime_limits_json_uses_safe_resource_defaults() {
        let decoded: RuntimeLimits = serde_json::from_value(serde_json::json!({
            "max_depth": 3,
            "max_children_per_agent": 4,
            "max_concurrent_agents": 2
        }))
        .expect("旧版 RuntimeLimits JSON 应保持兼容");
        let defaults = RuntimeLimits::default();

        assert_eq!(
            decoded.max_agents_per_principal,
            defaults.max_agents_per_principal
        );
        assert_eq!(
            decoded.max_agent_model_requests_per_principal,
            defaults.max_agent_model_requests_per_principal
        );
        assert_eq!(
            decoded.max_agent_model_request_bytes_per_principal,
            defaults.max_agent_model_request_bytes_per_principal
        );
        assert_eq!(
            decoded.max_agent_model_output_tokens_per_principal,
            defaults.max_agent_model_output_tokens_per_principal
        );
    }

    /// 派生配置应能显式关闭基础 Agent 默认启用的流式模式。
    #[test]
    fn options_patch_overrides_streaming_mode() {
        let mut options = AgentOptions::default();
        assert!(options.stream);

        AgentOptionsPatch {
            stream: Some(false),
            ..AgentOptionsPatch::default()
        }
        .apply_to(&mut options);

        assert!(!options.stream);
    }

    /// Runtime allowlist 的拒绝必须携带一次性指导，详细工具名只留给事件消费者。
    #[tokio::test]
    async fn restricted_extension_denial_uses_one_time_guidance() {
        let extension = RestrictedExtension {
            inner: Arc::new(agent_core::NoopAgentExtension),
            access: ToolAccess::allowlist(["read_file"]),
        };

        let decision = extension
            .before_tool(&ToolCall::new(
                "denied-shell",
                "shell",
                serde_json::json!({}),
            ))
            .await
            .expect("allowlist 拒绝应形成决策");

        assert!(matches!(
            decision,
            ToolDecision::Block {
                reason,
                agent_guidance: Some(guidance),
            } if reason.contains("shell")
                && !guidance.contains("shell")
                && guidance.contains("权限范围内")
        ));
    }
}

/// 一次 Agent 派生的可序列化配置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentDeriveConfig {
    /// 对父模板运行选项的增量覆盖。
    #[serde(default)]
    pub options: AgentOptionsPatch,
    /// 子节点请求的权限；Runtime 会与父节点有效权限取交集。
    #[serde(default)]
    pub permissions: AgentPermissions,
}

/// 从现有 Core Agent 捕获的可复用派生模板。
///
/// 模型网关、工具实例和钩子通过 `Arc` 或内部共享句柄复用；每次实例化都会创建独立
/// Core Agent，因此运行控制队列不会在并发任务之间共享。
#[derive(Clone)]
pub struct AgentTemplate {
    gateway: ModelGateway,
    tools: ToolRegistry,
    extension: Arc<dyn AgentExtension>,
    event_sink: Arc<dyn EventSink>,
    context_loader: Arc<dyn ContextLoader>,
    options: AgentOptions,
}

impl AgentTemplate {
    /// 从现有 Core Agent 捕获派生模板，不取得该 Agent 的运行控制队列。
    pub fn from_agent(agent: &Agent) -> Self {
        Self {
            gateway: agent.gateway().clone(),
            tools: agent.tools().clone(),
            extension: agent.extension(),
            event_sink: agent.event_sink(),
            context_loader: agent.context_loader(),
            options: agent.options().clone(),
        }
    }

    /// 返回模板的基础运行选项。
    pub fn options(&self) -> &AgentOptions {
        &self.options
    }

    /// 返回模板复用的模型网关，供应用注册受控的模型宿主能力。
    pub fn gateway(&self) -> &ModelGateway {
        &self.gateway
    }

    /// 根据父权限和派生配置创建独立 Core Agent。
    ///
    /// 返回值同时包含已经与父权限取交集的有效权限。工具 allowlist 会同时过滤模型
    /// 可见定义和实际执行入口；构造工具子集失败时返回 [`AgentRuntimeError::AgentBuild`]。
    pub fn instantiate(
        &self,
        parent_permissions: &AgentPermissions,
        config: &AgentDeriveConfig,
    ) -> RuntimeResult<(Agent, AgentPermissions)> {
        let permissions = parent_permissions.restrict(&config.permissions);
        let mut options = self.options.clone();
        config.options.apply_to(&mut options);

        let tools = match &permissions.tools {
            ToolAccess::All => self.tools.clone(),
            ToolAccess::Allowlist(names) => {
                let native_names = self
                    .tools
                    .specs()
                    .into_iter()
                    .map(|spec| spec.name)
                    .filter(|name| names.contains(name))
                    .collect::<Vec<_>>();
                self.tools
                    .subset(&native_names)
                    .map_err(|error| AgentRuntimeError::AgentBuild(error.to_string()))?
            }
        };

        let extension: Arc<dyn AgentExtension> = match &permissions.tools {
            ToolAccess::All => self.extension.clone(),
            access => Arc::new(RestrictedExtension {
                inner: self.extension.clone(),
                access: access.clone(),
            }),
        };

        let agent = Agent::new(self.gateway.clone(), options)
            .with_tools(tools)
            .with_extension(extension)
            .with_event_sink(self.event_sink.clone())
            .with_context_loader(self.context_loader.clone());
        Ok((agent, permissions))
    }
}

/// 为扩展工具应用与原生工具相同的 allowlist。
struct RestrictedExtension {
    inner: Arc<dyn AgentExtension>,
    access: ToolAccess,
}

#[async_trait]
impl AgentExtension for RestrictedExtension {
    async fn prompt_messages(&self) -> AnyResult<Vec<ModelMessage>> {
        self.inner.prompt_messages().await
    }

    async fn prompt_messages_for_run(&self, run_id: &str) -> AnyResult<Vec<ModelMessage>> {
        self.inner.prompt_messages_for_run(run_id).await
    }

    async fn list_tools(&self) -> AnyResult<Vec<ToolSpec>> {
        Ok(self
            .inner
            .list_tools()
            .await?
            .into_iter()
            .filter(|spec| self.access.permits(&spec.name))
            .collect())
    }

    async fn list_tools_for_run(&self, run_id: &str) -> AnyResult<Vec<ToolSpec>> {
        Ok(self
            .inner
            .list_tools_for_run(run_id)
            .await?
            .into_iter()
            .filter(|spec| self.access.permits(&spec.name))
            .collect())
    }

    async fn call_tool(&self, call: ToolCall) -> AnyResult<Option<ToolResult>> {
        if self.access.permits(&call.name) {
            self.inner.call_tool(call).await
        } else {
            Ok(None)
        }
    }

    async fn call_tool_for_run(
        &self,
        run_id: &str,
        call: ToolCall,
    ) -> AnyResult<Option<ToolResult>> {
        if self.access.permits(&call.name) {
            self.inner.call_tool_for_run(run_id, call).await
        } else {
            Ok(None)
        }
    }

    async fn before_tool(&self, call: &ToolCall) -> AnyResult<ToolDecision> {
        if !self.access.permits(&call.name) {
            return Ok(ToolDecision::block_with_guidance(
                format!("工具不在当前 Agent 的 allowlist 中：{}", call.name),
                "当前 Agent 无权使用该工具，调用未执行；不要重复相同调用，请改用权限范围内的工具继续。",
            ));
        }

        match self.inner.before_tool(call).await? {
            ToolDecision::Rewrite { call } if !self.access.permits(&call.name) => {
                Ok(ToolDecision::block_with_guidance(
                    format!("重写后的工具不在当前 Agent 的 allowlist 中：{}", call.name),
                    "工具重写后的目标超出当前 Agent 权限，调用未执行；请改用权限范围内的工具继续。",
                ))
            }
            decision => Ok(decision),
        }
    }

    async fn before_tool_for_run(&self, run_id: &str, call: &ToolCall) -> AnyResult<ToolDecision> {
        if !self.access.permits(&call.name) {
            return Ok(ToolDecision::block_with_guidance(
                format!("工具不在当前 Agent 的 allowlist 中：{}", call.name),
                "当前 Agent 无权使用该工具，调用未执行；不要重复相同调用，请改用权限范围内的工具继续。",
            ));
        }

        match self.inner.before_tool_for_run(run_id, call).await? {
            ToolDecision::Rewrite { call } if !self.access.permits(&call.name) => {
                Ok(ToolDecision::block_with_guidance(
                    format!("重写后的工具不在当前 Agent 的 allowlist 中：{}", call.name),
                    "工具重写后的目标超出当前 Agent 权限，调用未执行；请改用权限范围内的工具继续。",
                ))
            }
            decision => Ok(decision),
        }
    }

    async fn after_tool(&self, result: &ToolResult) -> AnyResult<()> {
        self.inner.after_tool(result).await
    }

    async fn after_tool_for_run(&self, run_id: &str, result: &ToolResult) -> AnyResult<()> {
        self.inner.after_tool_for_run(run_id, result).await
    }

    async fn on_event(&self, event: &AgentEvent) -> AnyResult<()> {
        self.inner.on_event(event).await
    }

    async fn on_event_for_run(&self, run_id: &str, event: &AgentEvent) -> AnyResult<()> {
        self.inner.on_event_for_run(run_id, event).await
    }

    async fn drain_events(&self) -> AnyResult<Vec<Value>> {
        self.inner.drain_events().await
    }

    async fn drain_events_for_run(&self, run_id: &str) -> AnyResult<Vec<Value>> {
        self.inner.drain_events_for_run(run_id).await
    }

    async fn finish_run(&self, run_id: &str) -> AnyResult<()> {
        self.inner.finish_run(run_id).await
    }
}
