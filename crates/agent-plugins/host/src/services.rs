//! 插件宿主对外提供的受信服务装配。

#[cfg(feature = "wasm")]
use crate::audit;
use crate::host::PluginHost;
use crate::session::PluginSessionSource;
#[cfg(feature = "wasm")]
use agent_core::model::ModelGateway;
#[cfg(feature = "wasm")]
use agent_core::AgentModelConfig;
#[cfg(feature = "wasm")]
use agent_runtime::{AgentDeriveConfig, AgentProfileId, AgentRuntimeProvisioner};
use agent_tool::ExecutionPolicy;
#[cfg(feature = "wasm")]
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
#[cfg(feature = "wasm")]
use std::path::PathBuf;
#[cfg(feature = "wasm")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 应用注入的受限 HTTP 端点服务。
///
/// 承载 `adr-api-embedding.md` 定下的分工：`base_url` 与凭证 header 由应用配置提供，
/// Guest 只能给出相对路径。凭证保存在这里而不是下发给插件，因此**插件无法读回密钥**，
/// 也无法把请求发往 base URL 之外的地址。
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub(crate) struct HttpHostServices {
    /// 可信 base URL；Guest 的相对路径都拼接在它之后。
    base_url: url::Url,
    /// Host 注入的凭证与固定 header，Guest 既不可见也不可覆盖。
    injected_headers: Arc<Vec<(String, String)>>,
    /// 复用连接池的 HTTP 客户端。
    client: reqwest::Client,
    /// 单次请求正文的字节上限。
    pub(crate) max_request_bytes: usize,
    /// 单次响应正文的字节上限。
    max_response_bytes: usize,
    execution_policy: ExecutionPolicy,
    completed_calls: Arc<AtomicUsize>,
    submitted_input_bytes: Arc<AtomicUsize>,
}

#[cfg(feature = "wasm")]
impl HttpHostServices {
    /// 为单个插件激活创建独立预算账本；返回值自身的后续 clone 继续共享该账本。
    fn for_plugin_activation(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            injected_headers: self.injected_headers.clone(),
            client: self.client.clone(),
            max_request_bytes: self.max_request_bytes,
            max_response_bytes: self.max_response_bytes,
            execution_policy: self.execution_policy.clone(),
            completed_calls: Arc::new(AtomicUsize::new(0)),
            submitted_input_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 把 Guest 提交的相对路径解析为可信绝对 URL。
    pub(crate) fn resolve(&self, path: &str) -> Result<url::Url> {
        crate::capability::http::join_base_url(&self.base_url, path)
    }

    /// 预留一次插件 HTTP 调用预算；失败时不增加任何计数。
    pub(crate) fn reserve(&self, input_bytes: usize) -> std::result::Result<(), String> {
        // Profile 身份拥有最终否决权：Evaluation 与 Mutation 即使限额被写成正数也不放行。
        if !self.execution_policy.permits_network_access() {
            return Err("当前执行策略禁止插件发起网络请求".to_string());
        }
        let limits = &self.execution_policy.limits;
        if !try_reserve(
            &self.submitted_input_bytes,
            input_bytes,
            limits.max_plugin_http_request_bytes,
        ) {
            return Err(format!(
                "插件 HTTP 请求累计输入超过策略上限 {} 字节",
                limits.max_plugin_http_request_bytes.unwrap_or(0)
            ));
        }
        if !try_reserve(&self.completed_calls, 1, limits.max_plugin_http_requests) {
            self.submitted_input_bytes
                .fetch_sub(input_bytes, Ordering::AcqRel);
            return Err(format!(
                "插件 HTTP 请求次数超过策略上限 {} 次",
                limits.max_plugin_http_requests.unwrap_or(0)
            ));
        }
        Ok(())
    }

    /// 执行一次已通过全部校验的 HTTP 请求。
    ///
    /// 注入 header 在 Guest header 之后写入，因此 Guest 无法覆盖凭证；响应按字节上限
    /// 截断前就拒绝，避免把超大响应读进内存。
    pub(crate) async fn execute(
        &self,
        method: String,
        target: url::Url,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<serde_json::Value> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| anyhow!("HTTP 方法无效：{error}"))?;
        let mut request = self.client.request(method, target);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        // 凭证最后注入，Guest 的同名 header 已在能力层被拒绝，这里是第二重保证。
        for (name, value) in self.injected_headers.iter() {
            request = request.header(name.as_str(), value.as_str());
        }
        if !body.is_empty() {
            request = request.body(body);
        }
        let response = request.send().await.context("插件 HTTP 请求失败")?;
        let status = response.status().as_u16();
        let response_headers =
            crate::capability::http::sanitize_response_headers(response.headers());
        // 先看 Content-Length：能提前拒绝的超大响应就不必读进内存。
        if let Some(length) = response.content_length() {
            if length > self.max_response_bytes as u64 {
                return Err(anyhow!(
                    "HTTP 响应超过 {} 字节限制",
                    self.max_response_bytes
                ));
            }
        }
        let bytes = response.bytes().await.context("读取插件 HTTP 响应失败")?;
        // 分块响应没有 Content-Length，必须在读完后再校验一次。
        if bytes.len() > self.max_response_bytes {
            return Err(anyhow!(
                "HTTP 响应超过 {} 字节限制",
                self.max_response_bytes
            ));
        }
        let body = String::from_utf8(bytes.to_vec())
            .context("插件 HTTP 响应不是合法 UTF-8；二进制响应需由插件自行约定编码")?;
        Ok(crate::capability::http::encode_http_response(
            status,
            response_headers,
            body,
        ))
    }
}

/// 应用固定路由的受限模型完成服务。
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub(crate) struct ModelCompletionHostServices {
    pub(crate) gateway: ModelGateway,
    /// Core 的模型调用配置原语；模型、推理级别、缓存策略等全部由它描述。
    ///
    /// 直接复用 Core 类型而不是在 Host 侧另立一套字段：插件补全与主 Agent 支持同样的
    /// 调用参数，新增能力只需扩展 Core 的配置，不必在这一层同步。
    pub(crate) config: AgentModelConfig,
    /// Host 允许的单次输出上限；Guest 与配置都只能取更小值。
    pub(crate) max_output_tokens: u32,
    execution_policy: ExecutionPolicy,
    completed_calls: Arc<AtomicUsize>,
    submitted_input_bytes: Arc<AtomicUsize>,
}

#[cfg(feature = "wasm")]
impl ModelCompletionHostServices {
    /// 为单个插件激活创建独立预算账本；返回值自身的后续 clone 继续共享该账本。
    fn for_plugin_activation(&self) -> Self {
        Self {
            gateway: self.gateway.clone(),
            config: self.config.clone(),
            max_output_tokens: self.max_output_tokens,
            execution_policy: self.execution_policy.clone(),
            completed_calls: Arc::new(AtomicUsize::new(0)),
            submitted_input_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 预留一次插件模型调用预算；失败时不增加任何计数。
    pub(crate) fn reserve(&self, input_bytes: usize) -> std::result::Result<(), String> {
        let limits = &self.execution_policy.limits;
        if !try_reserve(
            &self.submitted_input_bytes,
            input_bytes,
            limits.max_plugin_model_input_bytes,
        ) {
            return Err(format!(
                "插件模型请求累计输入超过策略上限 {} 字节",
                limits.max_plugin_model_input_bytes.unwrap_or(0)
            ));
        }
        if !try_reserve(
            &self.completed_calls,
            1,
            limits.max_plugin_model_completions,
        ) {
            self.submitted_input_bytes
                .fetch_sub(input_bytes, Ordering::AcqRel);
            return Err(format!(
                "插件模型完成调用次数超过策略上限 {} 次",
                limits.max_plugin_model_completions.unwrap_or(0)
            ));
        }
        Ok(())
    }
}

/// 原子预留一个累计资源维度；`None` 表示不设上限但仍保留计数。
#[cfg(feature = "wasm")]
fn try_reserve(counter: &AtomicUsize, amount: usize, limit: Option<usize>) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            let next = current.checked_add(amount)?;
            limit.is_none_or(|limit| next <= limit).then_some(next)
        })
        .is_ok()
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
    /// 按插件 ID 覆盖的模型路由；未覆盖的插件沿用 `model_completion`。
    #[cfg(feature = "wasm")]
    pub(crate) plugin_model_completion: HashMap<String, ModelCompletionHostServices>,
    /// 按插件 ID 注入的 HTTP 端点；没有应用默认值。
    ///
    /// 与模型路由不同，HTTP 端点**刻意不提供全局默认**：一个插件的 base URL 与凭证
    /// 对另一个插件没有意义，共享默认值只会让密钥流向不该拿到它的插件。
    #[cfg(feature = "wasm")]
    pub(crate) plugin_http: HashMap<String, HttpHostServices>,
    /// 应用注入的只读会话来源；缺失时插件的 `sessions` 能力保持关闭。
    pub(crate) session_source: Option<Arc<dyn PluginSessionSource>>,
    #[cfg(feature = "wasm")]
    pub(crate) activation_metadata: HashMap<String, HashMap<String, String>>,
    #[cfg(feature = "wasm")]
    service_call_observer: Option<Arc<dyn audit::HostServiceCallObserver>>,
    #[cfg(feature = "wasm")]
    pub(crate) persistent_state_dir: Option<PathBuf>,
    /// 当前项目的可信作用域标识，用于按项目隔离插件持久存储。
    #[cfg(feature = "wasm")]
    pub(crate) workspace_scope: Option<String>,
    /// 当前项目的可信规范化根目录，用于约束 `workspace_read` 文件访问。
    #[cfg(feature = "wasm")]
    pub(crate) workspace_root: Option<PathBuf>,
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
        #[cfg(feature = "wasm")]
        {
            let policy = self.execution_policy.clone();
            // 按插件覆盖的路由同样受收紧后的策略约束，避免绕过预算上限。
            for binding in self
                .model_completion
                .iter_mut()
                .chain(self.plugin_model_completion.values_mut())
            {
                binding.execution_policy = binding.execution_policy.restrict(&policy);
            }
            // HTTP 端点同样受收紧后的策略约束，否则网络预算可被绕过。
            for binding in self.plugin_http.values_mut() {
                binding.execution_policy = binding.execution_policy.restrict(&policy);
            }
        }
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
    /// 状态仍通过现有 `get_state`、`set_state` 和 `remove_state` ABI 访问，但 Host 会按插件
    /// ID、真实加载来源、Component 字节、版本和 capability profile 共同隔离；未调用本方法
    /// 时保持进程内状态。应用不应把生产目录注入 Evaluation 或 Mutation 平面。
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

    /// 声明当前项目的可信作用域标识。
    ///
    /// 该标识只用于隔离插件持久存储的命名空间，不会传给 Guest。未声明时按项目隔离的
    /// 存储作用域不可用，插件只能使用用户级存储——不会退化为跨项目共享。
    ///
    /// # Errors
    ///
    /// 标识为空白时返回错误，避免产生无法稳定寻址的命名空间。
    #[cfg(feature = "wasm")]
    pub fn with_workspace_scope(mut self, scope: impl Into<String>) -> Result<Self> {
        let scope = scope.into();
        if scope.trim().is_empty() {
            return Err(anyhow!("插件持久存储的项目作用域不能为空"));
        }
        self.workspace_scope = Some(scope);
        Ok(self)
    }

    /// 注入当前项目的可信根目录，供声明 `workspace_read` 的插件只读访问。
    ///
    /// 根目录在注入时规范化，后续 Guest 路径仍会逐次规范化并校验前缀；这样符号链接和
    /// `..` 都不能把读取范围扩张到项目之外。该路径不会从 manifest 或激活元数据读取。
    ///
    /// # Errors
    ///
    /// 路径为空、不存在、无法规范化或不是目录时返回错误。
    #[cfg(feature = "wasm")]
    pub fn with_workspace_root(mut self, path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(anyhow!("插件工作区根目录不能为空"));
        }
        let path = path
            .canonicalize()
            .map_err(|error| anyhow!("无法解析插件工作区根目录 `{}`：{error}", path.display()))?;
        if !path.is_dir() {
            return Err(anyhow!("插件工作区根路径不是目录：{}", path.display()));
        }
        self.workspace_root = Some(path);
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
    /// 路由用 Core 的 [`AgentModelConfig`] 描述：模型、推理级别、缓存策略、采样温度和
    /// provider options 都由它承载，插件补全因此与主 Agent 支持同一套调用参数。Guest 只能
    /// 提交 system、messages 和更小的输出上限，其余全部由 Host 控制。插件 manifest 还必须
    /// 声明 `model_completion`。
    ///
    /// `max_output_tokens` 是 Host 强制上限，配置里更大的值会被收敛到它。
    ///
    /// # Errors
    ///
    /// model 为空、provider 未注册到网关或输出上限为零时返回错误。
    #[cfg(feature = "wasm")]
    pub fn with_model_completion(
        mut self,
        gateway: ModelGateway,
        config: AgentModelConfig,
        max_output_tokens: u32,
    ) -> Result<Self> {
        self.model_completion =
            Some(self.build_model_completion(None, gateway, config, max_output_tokens)?);
        Ok(self)
    }

    /// 为单个插件注入专属模型路由，覆盖 [`Self::with_model_completion`] 的应用默认值。
    ///
    /// 供应用把插件自己的配置（模型、地址、密钥、推理级别）解析成独立路由；密钥只存在于
    /// Host 侧，Guest 仍然只能提交 system、messages 和更小的输出上限。未覆盖的插件继续
    /// 使用默认路由；覆盖后的插件拥有独立的调用次数与输入字节预算账本。
    ///
    /// # Errors
    ///
    /// 插件 ID 或 model 为空、provider 未注册到网关、输出上限为零时返回错误。
    #[cfg(feature = "wasm")]
    pub fn with_plugin_model_completion(
        mut self,
        plugin_id: impl Into<String>,
        gateway: ModelGateway,
        config: AgentModelConfig,
        max_output_tokens: u32,
    ) -> Result<Self> {
        let plugin_id = plugin_id.into();
        if plugin_id.trim().is_empty() {
            return Err(anyhow!("插件模型路由的 plugin_id 不能为空"));
        }
        let binding =
            self.build_model_completion(Some(&plugin_id), gateway, config, max_output_tokens)?;
        self.plugin_model_completion.insert(plugin_id, binding);
        Ok(self)
    }

    /// 校验路由并按当前执行策略构造模型完成绑定。
    ///
    /// `plugin_id` 只用于错误信息，`None` 表示应用默认路由。配置里的输出上限会被收敛到
    /// Host 允许的上限。
    #[cfg(feature = "wasm")]
    fn build_model_completion(
        &self,
        plugin_id: Option<&str>,
        gateway: ModelGateway,
        mut config: AgentModelConfig,
        max_output_tokens: u32,
    ) -> Result<ModelCompletionHostServices> {
        let scope = plugin_id.map_or(String::new(), |id| format!("插件 `{id}` 的"));
        if config.model.trim().is_empty() {
            return Err(anyhow!("{scope}模型完成 model 不能为空"));
        }
        if !gateway.contains(&config.provider.name) {
            return Err(anyhow!(
                "{scope}模型完成 provider `{}` 未注册",
                config.provider.name
            ));
        }
        if max_output_tokens == 0 {
            return Err(anyhow!("{scope}模型完成最大输出 token 必须大于零"));
        }
        // 配置只能把输出上限调小；Host 上限始终生效。
        config.max_tokens = Some(
            config
                .max_tokens
                .unwrap_or(max_output_tokens)
                .min(max_output_tokens),
        );
        Ok(ModelCompletionHostServices {
            gateway,
            config,
            max_output_tokens,
            execution_policy: self.execution_policy.clone(),
            completed_calls: Arc::new(AtomicUsize::new(0)),
            submitted_input_bytes: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 为单个插件注入受限 HTTP 端点。
    ///
    /// `base_url` 与 `credential_headers` 来自应用配置（与 `[plugins.<id>]` 模型路由同一处），
    /// 凭证只保存在 Host 侧。插件 manifest 还必须声明 `[capabilities.http] allow_hosts`
    /// 且覆盖 `base_url` 的主机——两者不一致时请求会在能力层被拒绝，使 manifest 里写明的
    /// 授权边界始终是真实的。
    ///
    /// # Errors
    ///
    /// 插件 ID 为空、`base_url` 无法解析、scheme 不是 http/https、带有用户名密码、
    /// 凭证 header 名称非法或体积上限为零时返回错误。
    #[cfg(feature = "wasm")]
    pub fn with_plugin_http(
        mut self,
        plugin_id: impl Into<String>,
        base_url: &str,
        credential_headers: Vec<(String, String)>,
        max_request_bytes: usize,
        max_response_bytes: usize,
        timeout_ms: u64,
    ) -> Result<Self> {
        let plugin_id = plugin_id.into();
        if plugin_id.trim().is_empty() {
            return Err(anyhow!("插件 HTTP 端点的 plugin_id 不能为空"));
        }
        let mut base_url = url::Url::parse(base_url)
            .map_err(|error| anyhow!("插件 `{plugin_id}` 的 HTTP base_url 无效：{error}"))?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(anyhow!(
                "插件 `{plugin_id}` 的 HTTP base_url 只支持 http/https，实际为 `{}`",
                base_url.scheme()
            ));
        }
        // URL 内嵌凭证会随日志与错误信息外泄，且绕开 header 注入路径。
        if !base_url.username().is_empty() || base_url.password().is_some() {
            return Err(anyhow!(
                "插件 `{plugin_id}` 的 HTTP base_url 不得内嵌用户名或密码"
            ));
        }
        if base_url.host_str().is_none() {
            return Err(anyhow!("插件 `{plugin_id}` 的 HTTP base_url 缺少主机名"));
        }
        // 结尾不带斜杠时 `Url::join` 会丢掉最后一段路径，导致 base 前缀校验与用户预期不符。
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        if max_request_bytes == 0 || max_response_bytes == 0 {
            return Err(anyhow!(
                "插件 `{plugin_id}` 的 HTTP 请求与响应上限必须大于零"
            ));
        }
        for (name, _) in &credential_headers {
            if name.trim().is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err(anyhow!(
                    "插件 `{plugin_id}` 的 HTTP 凭证 header 名称无效：`{name}`"
                ));
            }
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            // 重定向会把请求带出 allowlist，使 manifest 的授权边界失效。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .with_context(|| format!("构造插件 `{plugin_id}` 的 HTTP 客户端失败"))?;
        self.plugin_http.insert(
            plugin_id,
            HttpHostServices {
                base_url,
                injected_headers: Arc::new(credential_headers),
                client,
                max_request_bytes,
                max_response_bytes,
                execution_policy: self.execution_policy.clone(),
                completed_calls: Arc::new(AtomicUsize::new(0)),
                submitted_input_bytes: Arc::new(AtomicUsize::new(0)),
            },
        );
        Ok(self)
    }

    /// 返回指定插件激活使用的 HTTP 端点快照。
    ///
    /// 每次激活创建独立预算账本，避免一个插件耗尽其他插件的额度；同一返回值的 clone
    /// 仍共享计数器，使该插件实例内的并发调用无法绕过累计上限。
    #[cfg(feature = "wasm")]
    pub(crate) fn http_for(&self, plugin_id: &str) -> Option<HttpHostServices> {
        self.plugin_http
            .get(plugin_id)
            .map(HttpHostServices::for_plugin_activation)
    }

    /// 注入插件可只读访问的历史会话来源。
    ///
    /// 传入的是只读端口而不是 `SessionStore`，因此插件路径不存在写入会话的可能。
    /// 插件 manifest 还必须声明 `sessions`；该能力会把插件全部公开工具的权限下限
    /// 抬到 `unknown`，因为会话内容涵盖 Agent 历史上见过的一切。
    #[cfg(feature = "wasm")]
    pub fn with_session_source(mut self, source: Arc<dyn PluginSessionSource>) -> Self {
        self.session_source = Some(source);
        self
    }

    /// 返回应用注入的只读会话来源快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn session_source(&self) -> Option<Arc<dyn PluginSessionSource>> {
        self.session_source.clone()
    }

    /// 返回 Host 内部使用的 Agent Runtime 服务快照。
    #[cfg(feature = "wasm")]
    pub(crate) fn agent_runtime(&self) -> Option<AgentRuntimeHostServices> {
        self.agent_runtime.clone()
    }

    /// 返回指定插件激活使用的模型完成服务快照。
    ///
    /// 优先返回该插件的专属路由，没有配置时回退到应用默认路由。每次调用创建独立预算
    /// 账本，避免一个插件耗尽其他插件的额度；同一返回值的 clone 仍共享计数器，使该插件
    /// 实例内的并发调用无法绕过累计上限。
    #[cfg(feature = "wasm")]
    pub(crate) fn model_completion_for(
        &self,
        plugin_id: &str,
    ) -> Option<ModelCompletionHostServices> {
        self.plugin_model_completion
            .get(plugin_id)
            .or(self.model_completion.as_ref())
            .map(ModelCompletionHostServices::for_plugin_activation)
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

    /// 返回应用注入的项目作用域标识；未注入时按项目隔离的存储不可用。
    #[cfg(feature = "wasm")]
    pub(crate) fn workspace_scope(&self) -> Option<String> {
        self.workspace_scope.clone()
    }

    /// 返回应用注入的可信规范化工作区根；未注入时 `workspace_read` 不可用。
    #[cfg(feature = "wasm")]
    pub(crate) fn workspace_root(&self) -> Option<PathBuf> {
        self.workspace_root.clone()
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
