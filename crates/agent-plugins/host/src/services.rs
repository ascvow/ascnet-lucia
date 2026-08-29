//! 插件宿主对外提供的受信服务装配。

#[cfg(feature = "wasm")]
use crate::audit;
use crate::host::PluginHost;
#[cfg(feature = "wasm")]
use agent_core::model::ModelGateway;
#[cfg(feature = "wasm")]
use agent_runtime::{AgentDeriveConfig, AgentProfileId, AgentRuntimeProvisioner};
use agent_tool::ExecutionPolicy;
#[cfg(feature = "wasm")]
use anyhow::{anyhow, Result};
use std::collections::HashMap;
#[cfg(feature = "wasm")]
use std::path::PathBuf;
use std::sync::Arc;
/// 应用固定路由的受限模型完成服务。
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub(crate) struct ModelCompletionHostServices {
    pub(crate) gateway: ModelGateway,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) max_output_tokens: u32,
    pub(crate) stream: bool,
}

/// Plugin Host 可注入 WASM 实例的通用宿主服务集合。
///
/// 字段保持私有，后续增加日志、secret 或网络服务时不会扩张 loader 参数列表。默认值不
/// 提供 Agent Runtime，因此现有 loader 和不申请相关权限的插件保持原行为。
#[derive(Clone, Default)]
pub struct PluginHostServices {
    #[cfg(feature = "wasm")]
    pub(crate) agent_runtime: Option<AgentRuntimeHostServices>,
    #[cfg(feature = "wasm")]
    pub(crate) model_completion: Option<ModelCompletionHostServices>,
    #[cfg(feature = "wasm")]
    pub(crate) activation_metadata: HashMap<String, HashMap<String, String>>,
    #[cfg(feature = "wasm")]
    service_call_observer: Option<Arc<dyn audit::HostServiceCallObserver>>,
    #[cfg(feature = "wasm")]
    pub(crate) persistent_state_dir: Option<PathBuf>,
    /// Host 持有且只能通过 `restrict` 收紧的运行平面策略。
    pub(crate) execution_policy: ExecutionPolicy,
}

/// Agent Runtime provisioner 与 Host 管理的派生策略注册表。
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub(crate) struct AgentRuntimeHostServices {
    pub(crate) provisioner: Arc<dyn AgentRuntimeProvisioner>,
    pub(crate) controller_profile: AgentProfileId,
    spawn_profiles: Arc<HashMap<String, AgentDeriveConfig>>,
}

impl PluginHostServices {
    /// 创建不提供额外宿主服务的默认集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 使用可信执行策略收紧插件宿主能力。
    ///
    /// 每次调用都与当前策略逐字段取更严格的一方，因此后续传入 Serve 策略也无法恢复
    /// 已收紧的字段。当前 Plugin Host 在进程能力入口使用该策略；其他能力可在获得对应
    /// 可信门禁后复用同一策略。
    pub fn restrict_execution_policy(mut self, requested: &ExecutionPolicy) -> Self {
        self.execution_policy = self.execution_policy.restrict(requested);
        self
    }

    /// 安装真实 Host 服务路由的旁路审计观察器。
    ///
    /// 观察器只接收脱敏结果，不得改变服务调用结果。该观察器会被同一次多插件加载共享，
    /// 因而可以捕获 Guest 发起的跨插件调用以及应用通过同一服务注册表发起的调用。
    #[cfg(feature = "wasm")]
    pub fn with_service_call_observer(
        mut self,
        observer: Arc<dyn audit::HostServiceCallObserver>,
    ) -> Self {
        self.service_call_observer = Some(observer);
        self
    }

    /// 注入应用拥有的插件持久状态目录。
    ///
    /// 状态仍通过现有 `get_state`、`set_state` 和 `remove_state` ABI 按插件 ID 隔离；未调用
    /// 本方法时保持进程内状态。应用不应把生产目录注入 Evaluation 或 Mutation 平面。
    ///
    /// # Errors
    ///
    /// 路径为空时返回错误；该调用本身不创建目录。
    #[cfg(feature = "wasm")]
    pub fn with_persistent_state_dir(mut self, path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(anyhow!("插件持久状态目录不能为空"));
        }
        self.persistent_state_dir = Some(path);
        Ok(self)
    }

    /// 为指定插件注入 Host 可信的激活元数据。
    ///
    /// 元数据只会传给 `plugin_id` 对应的实例，并覆盖 manifest 中的同名键。Host 不解释
    /// 值的业务含义；空插件 ID 或空键会返回错误，避免产生无法稳定寻址的配置。
    ///
    /// # Errors
    ///
    /// `plugin_id` 为空，或 `metadata` 包含空键时返回错误。
    #[cfg(feature = "wasm")]
    pub fn with_activation_metadata(
        mut self,
        plugin_id: impl Into<String>,
        metadata: HashMap<String, String>,
    ) -> Result<Self> {
        let plugin_id = plugin_id.into();
        if plugin_id.trim().is_empty() {
            return Err(anyhow!("激活元数据 plugin_id 不能为空"));
        }
        if metadata.keys().any(|key| key.trim().is_empty()) {
            return Err(anyhow!("插件 `{plugin_id}` 的激活元数据键不能为空"));
        }
        self.activation_metadata.insert(plugin_id, metadata);
        Ok(self)
    }

    /// 注入 Agent Runtime provisioner、controller 基础 profile 和 Guest 可请求的派生策略。
    ///
    /// `controller_profile` 选择应用注册的基础 `AgentTemplate`；`spawn_profiles` 只提供
    /// 每次派生的 `AgentDeriveConfig`。插件 manifest 仍需逐项声明允许使用的策略。
    #[cfg(feature = "wasm")]
    pub fn with_agent_runtime(
        mut self,
        provisioner: Arc<dyn AgentRuntimeProvisioner>,
        controller_profile: AgentProfileId,
        spawn_profiles: HashMap<String, AgentDeriveConfig>,
    ) -> Result<Self> {
        for profile in spawn_profiles.keys() {
            AgentProfileId::new(profile.clone())
                .map_err(|error| anyhow!("Agent spawn profile `{profile}` 无效：{error}"))?;
        }
        self.agent_runtime = Some(AgentRuntimeHostServices {
            provisioner,
            controller_profile,
            spawn_profiles: Arc::new(spawn_profiles),
        });
        Ok(self)
    }

    /// 注入插件可请求的固定模型路由和最大输出预算。
    ///
    /// Guest 只能提交 system、messages 和更小的输出上限；provider、model、流式模式、
    /// 工具和 provider options 均由 Host 控制。插件 manifest 还必须声明
    /// `model_completion`。
    #[cfg(feature = "wasm")]
    pub fn with_model_completion(
        mut self,
        gateway: ModelGateway,
        provider: impl Into<String>,
        model: impl Into<String>,
        max_output_tokens: u32,
        stream: bool,
    ) -> Result<Self> {
        let provider = provider.into();
        let model = model.into();
        if provider.trim().is_empty() {
            return Err(anyhow!("模型完成 provider 不能为空"));
        }
        if model.trim().is_empty() {
            return Err(anyhow!("模型完成 model 不能为空"));
        }
        if !gateway.contains(&provider) {
            return Err(anyhow!("模型完成 provider `{provider}` 未注册"));
        }
        if max_output_tokens == 0 {
            return Err(anyhow!("模型完成最大输出 token 必须大于零"));
        }
        self.model_completion = Some(ModelCompletionHostServices {
            gateway,
            provider,
            model,
            max_output_tokens,
            stream,
        });
        Ok(self)
    }

    /// 返回 Host 内部使用的 Agent Runtime 服务快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn agent_runtime(&self) -> Option<AgentRuntimeHostServices> {
        self.agent_runtime.clone()
    }

    /// 返回 Host 内部使用的固定模型完成服务快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn model_completion(&self) -> Option<ModelCompletionHostServices> {
        self.model_completion.clone()
    }

    /// 返回指定插件的 Host 可信激活元数据；未注入时返回空映射。
    #[cfg(feature = "wasm")]
    pub(crate) fn activation_metadata(&self, plugin_id: &str) -> HashMap<String, String> {
        self.activation_metadata
            .get(plugin_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 返回应用注入的持久状态目录；未注入时保持实例内存状态。
    #[cfg(feature = "wasm")]
    pub(crate) fn persistent_state_dir(&self) -> Option<PathBuf> {
        self.persistent_state_dir.clone()
    }

    /// 返回 Host 内部持有的可信执行策略快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn execution_policy(&self) -> ExecutionPolicy {
        self.execution_policy.clone()
    }

    /// 返回 Host 服务注册表使用的旁路审计观察器。
    #[cfg(feature = "wasm")]
    pub(crate) fn service_call_observer(&self) -> Option<Arc<dyn audit::HostServiceCallObserver>> {
        self.service_call_observer.clone()
    }
}

#[cfg(feature = "wasm")]
impl AgentRuntimeHostServices {
    /// 返回应用注册的派生策略；不存在时表示应用没有授权该名称。
    pub(crate) fn spawn_profile(&self, profile: &str) -> Option<AgentDeriveConfig> {
        self.spawn_profiles.get(profile).cloned()
    }
}

/// 按可信插件 ID 和插件内视图 ID 两级索引的 UI owner 快照。
pub(crate) type UiRoutes = HashMap<String, HashMap<String, Arc<dyn PluginHost>>>;
