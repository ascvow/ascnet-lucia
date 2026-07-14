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
use std::{collections::BTreeSet, sync::Arc};

/// 工具访问范围。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "tools", rename_all = "snake_case")]
pub enum ToolAccess {
    /// 继承父节点当前允许的全部工具，不代表绕过父节点限制。
    #[default]
    All,
    /// 只允许集合中列出的工具。
    Allowlist(BTreeSet<String>),
}

impl ToolAccess {
    /// 创建一个工具 allowlist。
    pub fn allowlist<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Allowlist(names.into_iter().map(Into::into).collect())
    }

    /// 判断当前范围是否允许指定工具。
    pub fn permits(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allowlist(names) => names.contains(name),
        }
    }

    /// 在当前范围内应用下一层限制。
    ///
    /// 返回值只可能保持或收缩当前权限，子节点请求 `All` 也不会恢复父节点已移除的工具。
    pub fn restrict(&self, requested: &Self) -> Self {
        match (self, requested) {
            (Self::All, next) => next.clone(),
            (current @ Self::Allowlist(_), Self::All) => current.clone(),
            (Self::Allowlist(current), Self::Allowlist(requested)) => {
                Self::Allowlist(current.intersection(requested).cloned().collect())
            }
        }
    }
}

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

    async fn list_tools(&self) -> AnyResult<Vec<ToolSpec>> {
        Ok(self
            .inner
            .list_tools()
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

    async fn before_tool(&self, call: &ToolCall) -> AnyResult<ToolDecision> {
        if !self.access.permits(&call.name) {
            return Ok(ToolDecision::Block {
                reason: format!("工具不在当前 Agent 的 allowlist 中：{}", call.name),
            });
        }

        match self.inner.before_tool(call).await? {
            ToolDecision::Rewrite { call } if !self.access.permits(&call.name) => {
                Ok(ToolDecision::Block {
                    reason: format!("重写后的工具不在当前 Agent 的 allowlist 中：{}", call.name),
                })
            }
            decision => Ok(decision),
        }
    }

    async fn after_tool(&self, result: &ToolResult) -> AnyResult<()> {
        self.inner.after_tool(result).await
    }

    async fn on_event(&self, event: &AgentEvent) -> AnyResult<()> {
        self.inner.on_event(event).await
    }

    async fn drain_events(&self) -> AnyResult<Vec<Value>> {
        self.inner.drain_events().await
    }
}
