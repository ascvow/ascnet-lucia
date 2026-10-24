//! WASM Component 插件宿主。
//!
//! WIT world 故意在 ABI 边界使用 JSON 字符串。这样第一版 ABI 更稳定，
//! Rust 内部类型可以继续通过 serde 演进。

use crate::audit::snapshot_manifest_capabilities;
use crate::capability::AgentRuntimeBinding;
use crate::capability::CapabilityHostContext;
use crate::capability::CapabilityState;
use crate::composite::CompositePluginHost;
use crate::contribution::ContributionRegistry;
use crate::host::PluginHost;
use crate::manifest::resolve_plugin_load_order;
use crate::manifest::EventSubscription;
use crate::manifest::PluginManifest;
use crate::service::inherit_service_call_stack;
use crate::service::PluginService;
use crate::service::PluginServiceCall;
use crate::service::ServiceHandler;
use crate::service::ServiceRegistry;
use crate::services::AgentRuntimeHostServices;
use crate::services::PluginHostServices;
use crate::ui::ToolRenderRequest;
use crate::ui::ToolRendererContribution;
use crate::ui::UiContribution;
use crate::ui::UiDeclaration;
use crate::ui::UiFrame;
use crate::ui::UiInput;
use crate::ui::UiRenderRequest;
use crate::wasm::engine::shared_wasm_engine;
use crate::wasm::engine::IntoAnyhow;
use crate::wasm::engine::PluginWasiState;
use crate::wasm::fuel::context_fuel_budget;
use crate::wasm::fuel::refill_fuel;
use crate::wasm::fuel::set_fuel;
use crate::wasm::guard::validate_ui_contributions;
use crate::wasm::guard::validate_ui_instance;
use crate::wasm::guard::{bind_guest_tool_result, redact_guest_context_messages};
use crate::wasm::linker::classify_wasm_plugin_failure;
use crate::wasm::linker::get_required_func;
use agent_core::model::ModelMessage;
use agent_core::AgentExtension;
use agent_core::AgentRecordEvent;
use agent_core::ContextLoadRequest;
use agent_core::Event;
use agent_core::EventContext;
use agent_core::LoadedContext;
use agent_core::{AgentEvent, AgentEventKind};
use agent_plugin_protocol::{PluginEventEnvelope, PluginEventResponse};
use agent_runtime::RuntimePrincipal;
use agent_tool::ToolCall;
use agent_tool::ToolResult;
use agent_tool::ToolSpec;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;
use wasmtime::component::Component;
use wasmtime::component::Instance;
use wasmtime::component::Linker;
use wasmtime::component::TypedFunc;
use wasmtime::Store;
use wasmtime::StoreLimitsBuilder;
use wasmtime_wasi::WasiCtxBuilder;
mod engine;
mod fuel;
mod guard;
mod linker;
mod loader;

use linker::{add_plugin_host_imports, plugin_host_failure_result, PluginHostFailureKind};

pub use engine::{configure_wasm_cache_directory, WasmPluginLimits};
pub use loader::{
    load_wasm_plugins, load_wasm_plugins_progressively_with_selection_and_services,
    load_wasm_plugins_resilient, load_wasm_plugins_resilient_with_selection,
    load_wasm_plugins_resilient_with_selection_and_services, load_wasm_plugins_with_selection,
    load_wasm_plugins_with_selection_and_services, load_wasm_plugins_with_services,
    ProgressivePluginLoadUpdate,
};

/// 从 Host 实际加载的来源、Component 字节和规范能力快照生成持久状态身份。
///
/// 该身份不接受 Guest 或 manifest metadata 自报值。即使插件 ID 相同，加载来源、代码字节、
/// 版本或 capability profile 任一变化都会进入新的状态命名空间，禁止继承旧插件私有状态。
fn derive_plugin_state_identity(
    manifest: &PluginManifest,
    component_path: &Path,
    component_bytes: &[u8],
) -> Result<String> {
    let capability_snapshot = snapshot_manifest_capabilities(manifest)?;
    let capability_bytes = serde_json::to_vec(&capability_snapshot)
        .context("序列化插件可信 capability profile 失败")?;
    let mut digest = Sha256::new();
    digest.update(b"lucia-plugin-state-identity-v1\0");
    update_state_identity_field(&mut digest, manifest.plugin.id.as_bytes());
    update_state_identity_field(&mut digest, manifest.plugin.version.as_bytes());
    update_state_identity_field(&mut digest, manifest.plugin.api_version.as_bytes());
    update_state_identity_field(&mut digest, component_path.to_string_lossy().as_bytes());
    update_state_identity_field(&mut digest, &capability_bytes);
    update_state_identity_field(&mut digest, component_bytes);
    Ok(format!("{:x}", digest.finalize()))
}

/// 使用长度前缀写入身份字段，避免相邻可变长字段产生拼接歧义。
fn update_state_identity_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

/// 解析插件私有数据目录；未申请该能力或应用未注入存储根时返回 `None`。
///
/// 数据目录按项目作用域隔离——索引天然属于某个仓库，用户级作用域会让不同项目互相污染。
/// 应用没有提供项目身份时退回用户级，保证单仓库以外的场景依然可用。
///
/// # Errors
///
/// 目录创建失败时返回错误：申请了该能力却拿不到目录，属于必须让加载失败的配置问题，
/// 静默降级只会让插件在运行时才发现自己无处落盘。
fn plugin_data_dir_for(
    manifest: &PluginManifest,
    host_services: &PluginHostServices,
) -> Result<Option<PathBuf>> {
    if !manifest.capabilities.data_dir {
        return Ok(None);
    }
    let Some(root) = host_services.persistent_state_dir() else {
        return Ok(None);
    };
    let scope_key = host_services
        .workspace_scope()
        .filter(|scope| !scope.trim().is_empty())
        .map_or_else(|| "user".to_string(), |scope| format!("workspace:{scope}"));
    crate::state::plugin_data_dir(&root, &manifest.plugin.id, &scope_key).map(Some)
}

/// 为一次插件激活创建独占 controller，并把 manifest profile 请求与应用注册表求交集。
async fn provision_agent_runtime(
    manifest: &PluginManifest,
    host_services: Option<AgentRuntimeHostServices>,
) -> Result<Option<AgentRuntimeBinding>> {
    let permissions = &manifest.capabilities.agent;
    if !permissions.is_requested() {
        return Ok(None);
    }
    let host_services = host_services.ok_or_else(|| {
        anyhow!(
            "插件 `{}` 请求 Agent Runtime 能力，但应用未注入对应服务",
            manifest.plugin.id
        )
    })?;
    for profile in &permissions.profiles {
        if host_services.spawn_profile(profile).is_none() {
            return Err(anyhow!(
                "插件 `{}` 请求的 Agent spawn profile `{profile}` 未在应用注册",
                manifest.plugin.id
            ));
        }
    }

    let principal =
        RuntimePrincipal::new(format!("plugin:{}:{}", manifest.plugin.id, Uuid::new_v4()))
            .map_err(|error| anyhow!(error.to_string()))?;
    if let Err(error) = host_services
        .provisioner
        .grant_profile(principal.clone(), &host_services.controller_profile)
        .await
    {
        host_services.provisioner.revoke(&principal).await;
        return Err(anyhow!(error.to_string()));
    }
    let provisioned = match host_services
        .provisioner
        .provision(principal.clone(), &host_services.controller_profile)
        .await
    {
        Ok(provisioned) => provisioned,
        Err(error) => {
            host_services.provisioner.revoke(&principal).await;
            return Err(anyhow!(error.to_string()));
        }
    };
    if provisioned.api.principal() != principal
        || provisioned.api.identity() != provisioned.controller.id
    {
        host_services.provisioner.revoke(&principal).await;
        return Err(anyhow!(
            "Agent Runtime provisioner 返回了与可信 principal 或 controller 不一致的 API"
        ));
    }
    Ok(Some(AgentRuntimeBinding::new(
        principal,
        provisioned.api,
        host_services,
    )))
}

/// One plugin excluded from a resilient load attempt.
///
/// 容错加载中被剔除的单个插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLoadFailure {
    /// Stable plugin ID, or the manifest file name when parsing failed.
    /// 稳定插件 ID；manifest 解析失败时使用文件名。
    pub plugin_id: String,
    /// Human-readable activation or dependency failure. 面向用户的激活或依赖失败原因。
    pub reason: String,
    /// Required plugins that failed earlier and blocked this plugin. 导致当前插件被跳过的失败必选依赖。
    pub blocked_by: Vec<String>,
}

/// Partial-success result returned by resilient multi-plugin loading.
///
/// 多插件容错加载返回的部分成功结果。
pub struct PluginLoadReport {
    /// Composite host containing only successfully loaded plugins. 仅包含成功插件的组合宿主。
    pub host: CompositePluginHost,
    /// Failures in dependency-resolved processing order. 按依赖处理顺序排列的失败记录。
    pub failures: Vec<PluginLoadFailure>,
}

/// 已加载的 WASM 插件。
pub struct WasmPluginHost {
    manifest: PluginManifest,
    contributions: Arc<ContributionRegistry>,
    services: Arc<ServiceRegistry>,
    known_ui: Vec<UiDeclaration>,
    known_tool_renderers: Vec<ToolRendererContribution>,
    agent_runtime: Option<AgentRuntimeBinding>,
    state: Arc<Mutex<Option<WasmPluginState>>>,
}

/// 单个插件在一次事件分发上允许累计等待的最长毫秒数。
///
/// 人工审批等交互式插件通过 `Pending` 分片等待，这里给出统一上限：超时后 Host 判定
/// Guest 失约并报错，由订阅声明的故障模式决定放行还是拒绝，防止插件永久挂起 Agent。
const MAX_EVENT_PENDING_TOTAL_MS: u64 = 600_000;

/// 插件加载被取消时异步撤销尚未转交给宿主实例的 Agent Runtime principal。
struct AgentRuntimeLoadGuard {
    binding: Option<AgentRuntimeBinding>,
}

impl AgentRuntimeLoadGuard {
    /// 创建持有可选临时绑定的加载守卫。
    fn new(binding: Option<AgentRuntimeBinding>) -> Self {
        Self { binding }
    }

    /// 正常加载失败时取出绑定，由当前 future 等待完整撤销。
    fn take(&mut self) -> Option<AgentRuntimeBinding> {
        self.binding.take()
    }

    /// 宿主实例已经接管绑定后解除取消兜底。
    fn disarm(&mut self) {
        self.binding = None;
    }
}

impl Drop for AgentRuntimeLoadGuard {
    fn drop(&mut self) {
        let Some(binding) = self.binding.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                binding.revoke().await;
            });
        }
    }
}

struct WasmPluginState {
    store: Store<PluginWasiState>,
    #[allow(dead_code)]
    instance: Instance,
    call_tool: TypedFunc<(String,), (String,)>,
    on_event: TypedFunc<(String,), (String,)>,
    load_context: TypedFunc<(String,), (String,)>,
    handle_service: TypedFunc<(String,), (String,)>,
    deactivate: TypedFunc<(), (String,)>,
    render_ui: TypedFunc<(String,), (String,)>,
    on_ui_input: TypedFunc<(String,), ()>,
    limits: WasmPluginLimits,
}

/// UI 不应被业务调用的长等待拖住；排队超时不销毁健康实例。
#[derive(Clone, Copy)]
struct ComponentCallBudget {
    wait: Duration,
    execute: Duration,
    retire: bool,
}
tokio::task_local! {
    /// 嵌套服务必须继承父导出的截止时间，不能通过独立任务延长父调用的授权窗口。
    static COMPONENT_DEADLINE: tokio::time::Instant;
}
const UI_CALL_BUDGET: ComponentCallBudget = ComponentCallBudget {
    wait: Duration::from_millis(250),
    execute: Duration::from_secs(2),
    retire: false,
};
const WORK_CALL_BUDGET: ComponentCallBudget = ComponentCallBudget {
    wait: Duration::from_secs(1800),
    execute: Duration::from_secs(1800),
    retire: false,
};
const DEACTIVATE_CALL_BUDGET: ComponentCallBudget = ComponentCallBudget {
    wait: Duration::from_secs(1800),
    execute: Duration::from_secs(60),
    retire: true,
};
const INITIALIZATION_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 协作式 deadline，不能抢占阻塞系统调用；超时后调用者必须销毁而非复用 Store。
async fn component_deadline<F: Future>(
    operation: &str,
    duration: Duration,
    future: F,
) -> Result<F::Output> {
    let deadline = tokio::time::Instant::now() + duration;
    let deadline = COMPONENT_DEADLINE
        .try_with(|parent| deadline.min(*parent))
        .unwrap_or(deadline);
    tokio::time::timeout_at(deadline, COMPONENT_DEADLINE.scope(deadline, future))
        .await
        .map_err(|_| anyhow!("{operation} deadline 超时（{}ms）", duration.as_millis()))
}

/// 调用期间从共享槽取走 Store，panic/超时只销毁，不允许进入被取消过的实例。
struct ComponentCallLease {
    slot: OwnedMutexGuard<Option<WasmPluginState>>,
    active: Option<WasmPluginState>,
}
impl ComponentCallLease {
    fn restore(&mut self) {
        *self.slot = self.active.take();
    }
}
impl Drop for ComponentCallLease {
    fn drop(&mut self) {
        if let Some(state) = self.active.take() {
            let capabilities = &state.store.data().capabilities;
            let (id, services) = capabilities.service_context();
            let (_, binding) = capabilities.agent_runtime_context();
            let _ = services.unregister_plugin(&id);
            drop(state);
            if let Some(binding) = binding {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        binding.revoke().await;
                    });
                }
            }
        }
    }
}

/// 独立任务持锁执行并负责 deadline；外部取消只解除等待，不会遗留可重入的半执行 Store。
async fn complete_component_call<T, F>(
    state: &Arc<Mutex<Option<WasmPluginState>>>,
    operation: String,
    budget: ComponentCallBudget,
    call: F,
) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut WasmPluginState) -> BoxFuture<'a, Result<T>> + Send + 'static,
{
    let parent_deadline = COMPONENT_DEADLINE.try_with(|deadline| *deadline).ok();
    let mut slot = component_deadline(
        &format!("{operation} 等待实例锁"),
        budget.wait,
        Arc::clone(state).lock_owned(),
    )
    .await?;
    let active = slot
        .take()
        .ok_or_else(|| anyhow!("{operation} Store 已隔离，必须重新加载"))?;
    let mut lease = ComponentCallLease {
        slot,
        active: Some(active),
    };
    let task_operation = operation.clone();
    let execution_deadline = tokio::time::Instant::now() + budget.execute;
    let execution_deadline =
        parent_deadline.map_or(execution_deadline, |parent| parent.min(execution_deadline));
    let task = tokio::spawn(inherit_service_call_stack(COMPONENT_DEADLINE.scope(
        execution_deadline,
        async move {
            let outcome = component_deadline(
                &task_operation,
                budget.execute,
                call(lease.active.as_mut().expect("调用守卫持有实例")),
            )
            .await;
            if outcome.is_ok() && !budget.retire {
                lease.restore();
            }
            outcome?
        },
    )));
    task.await
        .with_context(|| format!("{operation} 的 Component 调用任务异常结束"))?
}

/// 服务目录持有的弱引用端点，避免服务注册表与 WASM store 形成引用环。
struct WasmServiceEndpoint {
    plugin_id: String,
    state: Weak<Mutex<Option<WasmPluginState>>>,
}

#[derive(Deserialize)]
struct GuestServiceResponse {
    ok: bool,
    #[serde(default)]
    value: Value,
    error: Option<String>,
}

#[derive(Deserialize)]
struct GuestContextLoadResponse {
    context: Option<LoadedContext>,
    error: Option<String>,
}

#[async_trait]
impl ServiceHandler for WasmServiceEndpoint {
    async fn handle(&self, call: PluginServiceCall) -> Result<Value> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| anyhow!("插件 `{}` 已卸载", self.plugin_id))?;
        let request = serde_json::to_string(&serde_json::json!({
            "caller_id": call.caller_id,
            "name": call.name,
            "payload": call.payload,
        }))?;
        let plugin_id = self.plugin_id.clone();
        complete_component_call(
            &state,
            format!("插件 `{plugin_id}` 服务"),
            WORK_CALL_BUDGET,
            move |state| {
                Box::pin(async move {
                    refill_fuel(state)?;
                    let handle_service = state.handle_service;
                    let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
                    let (response_json,) = handle_service
                        .call_async(&mut state.store, (request,))
                        .await
                        .into_anyhow()
                        .with_context(|| format!("插件 `{plugin_id}` 服务调用失败"))?;
                    drop(run_scope);
                    let response: GuestServiceResponse = serde_json::from_str(&response_json)
                        .with_context(|| format!("插件 `{plugin_id}` 返回了无效服务响应"))?;
                    if response.ok {
                        Ok(response.value)
                    } else {
                        Err(anyhow!(
                            "{}",
                            response.error.unwrap_or_else(|| "插件服务调用失败".into())
                        ))
                    }
                })
            },
        )
        .await
    }
}

impl WasmPluginHost {
    /// 从 manifest 文件加载 component 插件。
    pub async fn load_from_manifest(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_from_manifest_with_limits(path, WasmPluginLimits::default()).await
    }

    /// 使用运行时限制从 manifest 文件加载 component 插件。
    pub async fn load_from_manifest_with_limits(
        path: impl AsRef<Path>,
        limits: WasmPluginLimits,
    ) -> Result<Self> {
        Self::load_from_manifest_with_limits_and_services(
            path,
            limits,
            PluginHostServices::default(),
        )
        .await
    }

    /// 使用可扩展宿主服务从 manifest 文件加载 component 插件。
    pub async fn load_from_manifest_with_services(
        path: impl AsRef<Path>,
        host_services: PluginHostServices,
    ) -> Result<Self> {
        Self::load_from_manifest_with_limits_and_services(
            path,
            WasmPluginLimits::default(),
            host_services,
        )
        .await
    }

    /// 使用运行时限制和可扩展宿主服务加载 component 插件。
    pub async fn load_from_manifest_with_limits_and_services(
        path: impl AsRef<Path>,
        limits: WasmPluginLimits,
        host_services: PluginHostServices,
    ) -> Result<Self> {
        let manifest_path = path.as_ref();
        let manifest = PluginManifest::load(manifest_path)?;
        resolve_plugin_load_order(std::slice::from_ref(&manifest))?;
        let base_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let wasm_path = base_dir.join(&manifest.plugin.wasm);
        let services = Arc::new(ServiceRegistry::new(host_services.service_call_observer()));
        Self::load_with_limits_in_dir(
            manifest,
            wasm_path,
            base_dir.to_path_buf(),
            limits,
            services,
            host_services,
        )
        .await
    }

    /// 根据 manifest 和 wasm 路径加载 component 插件。
    pub async fn load(manifest: PluginManifest, wasm_path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with_limits(manifest, wasm_path, WasmPluginLimits::default()).await
    }

    /// 根据 manifest、wasm 路径和限制加载 component 插件。
    pub async fn load_with_limits(
        manifest: PluginManifest,
        wasm_path: impl AsRef<Path>,
        limits: WasmPluginLimits,
    ) -> Result<Self> {
        Self::load_with_limits_and_services(
            manifest,
            wasm_path,
            limits,
            PluginHostServices::default(),
        )
        .await
    }

    /// 根据 manifest、WASM 路径和可扩展宿主服务加载 component 插件。
    pub async fn load_with_services(
        manifest: PluginManifest,
        wasm_path: impl AsRef<Path>,
        host_services: PluginHostServices,
    ) -> Result<Self> {
        Self::load_with_limits_and_services(
            manifest,
            wasm_path,
            WasmPluginLimits::default(),
            host_services,
        )
        .await
    }

    /// 根据 manifest、WASM 路径、限制和可扩展宿主服务加载 component 插件。
    pub async fn load_with_limits_and_services(
        manifest: PluginManifest,
        wasm_path: impl AsRef<Path>,
        limits: WasmPluginLimits,
        host_services: PluginHostServices,
    ) -> Result<Self> {
        resolve_plugin_load_order(std::slice::from_ref(&manifest))?;
        let plugin_dir = wasm_path
            .as_ref()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let services = Arc::new(ServiceRegistry::new(host_services.service_call_observer()));
        Self::load_with_limits_in_dir(
            manifest,
            wasm_path,
            plugin_dir,
            limits,
            services,
            host_services,
        )
        .await
    }

    async fn load_with_limits_in_dir(
        manifest: PluginManifest,
        wasm_path: impl AsRef<Path>,
        plugin_dir: PathBuf,
        limits: WasmPluginLimits,
        services: Arc<ServiceRegistry>,
        host_services: PluginHostServices,
    ) -> Result<Self> {
        manifest.validate()?;

        let wasm_path = wasm_path.as_ref().to_path_buf();
        let identity_manifest = manifest.clone();
        // 许可由实际 blocking 任务持有；等待方取消不会使残留编译失去并发约束。
        static COMPILE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);
        let permit = COMPILE_SLOTS.acquire().await.context("插件编译槽已关闭")?;
        let (engine, component, state_identity) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let engine = shared_wasm_engine()?;
            let canonical = wasm_path
                .canonicalize()
                .context("无法解析插件 Component 路径")?;
            let bytes = fs::read(&wasm_path).context("无法读取插件 Component")?;
            let identity = derive_plugin_state_identity(&identity_manifest, &canonical, &bytes)?;
            let component = Component::new(&engine, &bytes)
                .into_anyhow()
                .context("failed to compile wasm component")?;
            Ok::<_, anyhow::Error>((engine, component, identity))
        })
        .await
        .context("插件编译任务失败")??;

        let mut linker = Linker::<PluginWasiState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .into_anyhow()
            .context("failed to add WASI Preview 2 imports to linker")?;
        add_plugin_host_imports(&mut linker)?;

        let mut registration = services.begin_plugin_load(
            &manifest.plugin.id,
            manifest
                .dependencies
                .iter()
                .map(|dependency| dependency.id.clone())
                .collect(),
        )?;
        let agent_runtime =
            provision_agent_runtime(&manifest, host_services.agent_runtime()).await?;
        let mut cleanup_agent_runtime = AgentRuntimeLoadGuard::new(agent_runtime.clone());
        let loading: Result<Self> = async {
            let mut wasi = WasiCtxBuilder::new();
            // 刻意保持最小 WASI：不继承环境变量、不继承 stdio，也不预打开工作区或任何
            // 系统目录。唯一的例外是插件自己申请的私有数据目录，见下方。
            wasi.arg(&manifest.plugin.id);
            // manifest 逐个列出的变量才会进入 Guest。默认不继承是刻意的：宿主进程的环境
            // 里通常就有模型 API 密钥，整体继承等于把凭证直接交给插件。
            for name in &manifest.capabilities.env_read {
                // WASI 环境表只承载 UTF-8 文本；非 UTF-8 值无法表达，按不可见处理。
                if let Ok(value) = std::env::var(name) {
                    wasi.env(name, value);
                }
            }
            // 声明 `data_dir` 的插件获得一个 preopen 成 `/data` 的私有目录。
            //
            // 这里刻意**不设容量上限**：索引、向量库这类数据的体积由用户仓库规模决定，
            // 替插件猜一个字节上限只会让功能在超限后静默退化。需要约束的是访问边界——
            // 目录按插件与项目作用域隔离，插件看不到 `/data` 以外的任何路径。
            let data_dir = plugin_data_dir_for(&manifest, &host_services)?;
            if let Some(path) = data_dir.as_ref() {
                wasi.preopened_dir(
                    path,
                    "/data",
                    wasmtime_wasi::DirPerms::all(),
                    wasmtime_wasi::FilePerms::all(),
                )
                .into_anyhow()
                .with_context(|| format!("预打开插件数据目录 `{}` 失败", path.display()))?;
            }

            let contributions = Arc::new(ContributionRegistry::default());
            let store_limits = StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .memories(4)
                .instances(64)
                .tables(16)
                .table_elements(100_000)
                .trap_on_grow_failure(true)
                .build();
            let mut store = Store::new(
                &engine,
                PluginWasiState::new(
                    wasi.build(),
                    CapabilityState::new(
                        manifest.plugin.id.clone(),
                        state_identity,
                        plugin_dir,
                        manifest.capabilities.clone(),
                        contributions.clone(),
                        services.clone(),
                        CapabilityHostContext::new(
                            agent_runtime.clone(),
                            host_services.model_completion_for(&manifest.plugin.id),
                            host_services.execution_policy(),
                            host_services.persistent_state_dir(),
                            host_services.workspace_scope(),
                            host_services.session_source(),
                            host_services.workspace_root(),
                        )
                        .with_http(host_services.http_for(&manifest.plugin.id))
                        .with_call_observer(host_services.service_call_observer())
                        .with_provided_capabilities(
                            manifest
                                .provides
                                .iter()
                                .map(|provided| provided.id.clone())
                                .collect(),
                        ),
                    )?,
                    store_limits,
                ),
            );
            store.limiter(|state| &mut state.store_limits);
            store
                .set_fuel(limits.fuel)
                .into_anyhow()
                .context("failed to set plugin fuel")?;
            store
                .fuel_async_yield_interval(Some(
                    limits
                        .fuel_yield_interval
                        .unwrap_or(250_000)
                        .clamp(1, 250_000),
                ))
                .into_anyhow()
                .context("failed to configure plugin fuel yield interval")?;

            let instance = component_deadline(
                "plugin instantiate",
                INITIALIZATION_CALL_TIMEOUT,
                linker.instantiate_async(&mut store, &component),
            )
            .await?
            .into_anyhow()
            .context("failed to instantiate wasm component")?;

            let list_tools =
                get_required_func::<(), (String,)>(&instance, &mut store, "list-tools")?;
            let call_tool =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "call-tool")?;
            let on_event =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "on-event")?;
            let load_context =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "load-context")?;
            let activate =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "activate")?;
            let deactivate =
                get_required_func::<(), (String,)>(&instance, &mut store, "deactivate")?;
            let handle_service =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "handle-service")?;
            let describe_ui =
                get_required_func::<(), (String,)>(&instance, &mut store, "describe-ui")?;
            let render_ui =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "render-ui")?;
            let on_ui_input =
                get_required_func::<(String,), ()>(&instance, &mut store, "on-ui-input")?;

            let (tools_json,) = component_deadline(
                "plugin list-tools",
                INITIALIZATION_CALL_TIMEOUT,
                list_tools.call_async(&mut store, ()),
            )
            .await?
            .into_anyhow()
            .context("plugin `list-tools` failed")?;
            let mut static_tools: Vec<ToolSpec> = serde_json::from_str(&tools_json)
                .with_context(|| "plugin `list-tools` returned invalid ToolSpec JSON")?;
            for tool in &mut static_tools {
                manifest.capabilities.bind_tool_scope(tool)?;
            }
            contributions.upsert_static_tools(static_tools)?;

            let plugin_id = manifest.plugin.id.clone();
            let mut state = WasmPluginState {
                store,
                instance,
                call_tool,
                on_event,
                load_context,
                handle_service,
                deactivate,
                render_ui,
                on_ui_input,
                limits,
            };

            let initialization = async {
                refill_fuel(&mut state)?;
                let mut activation_metadata = manifest.metadata.clone();
                activation_metadata.extend(host_services.activation_metadata(&plugin_id));
                let context_json = serde_json::to_string(&serde_json::json!({
                    "plugin_id": &plugin_id,
                    "metadata": activation_metadata,
                }))?;
                let activation_scope = state.store.data().capabilities.enter_active_run_scope(None);
                let (activation_error,) = component_deadline(
                    "plugin activate",
                    WORK_CALL_BUDGET.execute,
                    activate.call_async(&mut state.store, (context_json,)),
                )
                .await?
                .into_anyhow()
                .with_context(|| format!("plugin `{plugin_id}` activate failed"))?;
                drop(activation_scope);
                if !activation_error.is_empty() {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` activation failed: {activation_error}"
                    ));
                }

                refill_fuel(&mut state)?;
                let describe_scope = state.store.data().capabilities.enter_active_run_scope(None);
                let (declarations_json,) = component_deadline(
                    "plugin describe-ui",
                    INITIALIZATION_CALL_TIMEOUT,
                    describe_ui.call_async(&mut state.store, ()),
                )
                .await?
                .into_anyhow()
                .context("plugin `describe-ui` failed")?;
                drop(describe_scope);
                let ui_contributions: Vec<UiContribution> =
                    serde_json::from_str(&declarations_json).with_context(|| {
                        format!("plugin `{plugin_id}` returned invalid UI contribution JSON")
                    })?;
                let owned_tools = contributions.tools()?;
                let (declarations, tool_renderers) =
                    validate_ui_contributions(&plugin_id, ui_contributions, &owned_tools)?;
                Ok((declarations, tool_renderers))
            }
            .await;
            let (known_ui, known_tool_renderers) = initialization?;
            let state = Arc::new(Mutex::new(Some(state)));
            services.register_handler(
                plugin_id.clone(),
                manifest
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.id.clone())
                    .collect(),
                Arc::new(WasmServiceEndpoint {
                    plugin_id: plugin_id.clone(),
                    state: Arc::downgrade(&state),
                }),
            )?;

            Ok(Self {
                manifest,
                contributions,
                services,
                known_ui,
                known_tool_renderers,
                agent_runtime,
                state,
            })
        }
        .await;
        if loading.is_err() {
            drop(registration);
            if let Some(binding) = cleanup_agent_runtime.take() {
                // 取消等待不能取消撤销；JoinHandle 超时后仅 detach，任务继续清理。
                let task = tokio::spawn(async move {
                    binding.revoke().await;
                });
                let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
            }
        } else {
            cleanup_agent_runtime.disarm();
            registration.disarm();
        }
        loading
    }

    /// manifest 中的插件 ID。
    pub fn id(&self) -> &str {
        &self.manifest.plugin.id
    }

    /// 插件 manifest。
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// 调用 component 的卸载钩子并撤销其全部宿主资源。
    pub async fn deactivate(&self) -> Result<()> {
        let plugin_id = self.id().to_string();
        let state = Arc::clone(&self.state);
        let services = Arc::clone(&self.services);
        let binding = self.agent_runtime.clone();
        let cleanup_id = plugin_id.clone();
        let task = tokio::spawn(async move {
            let deactivation = complete_component_call(
                &state,
                format!("plugin `{plugin_id}` deactivate"),
                DEACTIVATE_CALL_BUDGET,
                move |state| {
                    Box::pin(async move {
                        let deactivate = state.deactivate;
                        refill_fuel(state)?;
                        let run_scope =
                            state.store.data().capabilities.enter_active_run_scope(None);
                        let (deactivation_error,) = deactivate
                            .call_async(&mut state.store, ())
                            .await
                            .into_anyhow()
                            .with_context(|| format!("plugin `{plugin_id}` deactivate failed"))?;
                        drop(run_scope);
                        if deactivation_error.is_empty() {
                            Ok(())
                        } else {
                            Err(anyhow!(
                                "plugin `{plugin_id}` deactivation failed: {deactivation_error}"
                            ))
                        }
                    })
                },
            )
            .await;
            let unregister = services.unregister_plugin(&cleanup_id);
            if let Some(binding) = binding {
                binding.revoke().await;
            }
            deactivation.and(unregister)
        });
        task.await.context("插件卸载任务异常结束")?
    }

    /// 向 Guest 投递一次事件分发，并在能力导入期间绑定可信 Agent run。
    ///
    /// Guest 返回 `Pending` 时按其请求的间隔重试，供人工审批一类需要外部输入的插件
    /// 使用；累计等待超过 [`MAX_EVENT_PENDING_TOTAL_MS`] 视为 Guest 失约并返回错误，
    /// 避免一个卡住的插件永久挂起整个 Agent。
    async fn deliver_event(
        &self,
        envelope: &PluginEventEnvelope,
        run_id: Option<&str>,
    ) -> Result<PluginEventResponse> {
        let envelope_json = serde_json::to_string(envelope)?;
        let mut waited_ms = 0u64;
        loop {
            let plugin_id = self.id().to_string();
            let event_name = envelope.event.clone();
            let event_json = envelope_json.clone();
            let active_run_id = run_id.map(str::to_owned);
            let response = complete_component_call(
                &self.state,
                format!("plugin `{plugin_id}` on-event `{event_name}`"),
                WORK_CALL_BUDGET,
            move |state| Box::pin(async move {
                    refill_fuel(state)?;
                    let run_scope = state
                        .store
                        .data()
                        .capabilities
                        .enter_active_run_scope(active_run_id.as_deref());
                    let on_event = state.on_event;
                    let invocation = on_event
                        .call_async(&mut state.store, (event_json,))
                        .await
                        .into_anyhow();
                    drop(run_scope);
                    let (response_json,) = invocation.with_context(|| {
                        format!("plugin `{plugin_id}` on-event failed for `{event_name}`")
                    })?;
                // 空字符串是"放行且不改写"的简写，允许纯观察型插件省略应答构造。
                    if response_json.trim().is_empty() {
                        return Ok(PluginEventResponse::allow());
                    }
                    serde_json::from_str::<PluginEventResponse>(&response_json).with_context(|| {
                        format!(
                            "plugin `{plugin_id}` returned invalid PluginEventResponse JSON for `{event_name}`"
                        )
                    })
                }),
            )
            .await?;

            match response {
                PluginEventResponse::Pending { retry_after_ms } => {
                    let delay = retry_after_ms.clamp(50, 1_000);
                    waited_ms = waited_ms.saturating_add(delay);
                    if waited_ms > MAX_EVENT_PENDING_TOTAL_MS {
                        return Err(anyhow::anyhow!(
                            "插件 `{}` 在事件 `{}` 上等待超过 {}ms 仍未给出结论",
                            self.id(),
                            envelope.event,
                            MAX_EVENT_PENDING_TOTAL_MS
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                ready => return Ok(ready),
            }
        }
    }
}

#[async_trait]
impl AgentExtension for WasmPluginHost {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        self.contributions.prompt_messages()
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        self.contributions.tools()
    }

    async fn call_tool(&self, mut call: ToolCall) -> Result<Option<ToolResult>> {
        let run_id = call.run_id.clone();
        let public_name = call.name.clone();
        let Some(local_name) = self.contributions.local_tool_name(&public_name)? else {
            return Ok(None);
        };
        call.name = local_name;

        let call_json = serde_json::to_string(&call)?;
        let plugin_id = self.id().to_string();
        complete_component_call(
            &self.state,
            format!("plugin `{plugin_id}` call-tool"),
            WORK_CALL_BUDGET,
            move |state| {
                Box::pin(async move {
                    refill_fuel(state)?;
                    // 前一个 Hook 的故障不能污染本次工具调用；每个导出调用只消费自己的 Host 观察。
                    state.store.data_mut().take_plugin_failure();
                    let run_scope = state
                        .store
                        .data()
                        .capabilities
                        .enter_active_run_scope(run_id.as_deref());
                    let call_tool = state.call_tool;
                    let invocation = call_tool.call_async(&mut state.store, (call_json,)).await;
                    drop(run_scope);
                    let capability_failure = state.store.data_mut().take_plugin_failure();
                    let (result_json,) = match invocation {
                        Ok(value) => value,
                        Err(error) => {
                            let failure = capability_failure
                                .unwrap_or_else(|| classify_wasm_plugin_failure(&error));
                            return Ok(Some(plugin_host_failure_result(
                                &call,
                                &public_name,
                                &plugin_id,
                                failure,
                            )));
                        }
                    };
                    if let Some(failure) = capability_failure {
                        return Ok(Some(plugin_host_failure_result(
                            &call,
                            &public_name,
                            &plugin_id,
                            failure,
                        )));
                    }
                    let mut result = match serde_json::from_str::<ToolResult>(&result_json) {
                        Ok(result) => result,
                        Err(_) => {
                            return Ok(Some(plugin_host_failure_result(
                                &call,
                                &public_name,
                                &plugin_id,
                                PluginHostFailureKind::ContractViolation,
                            )));
                        }
                    };
                    bind_guest_tool_result(&mut result, &call, &public_name);
                    Ok(Some(result))
                })
            },
        )
        .await
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        self.contributions.drain_events()
    }

    async fn drain_events_for_run(&self, run_id: &str) -> Result<Vec<serde_json::Value>> {
        self.contributions.drain_events_for_run(run_id)
    }

    async fn finish_run(&self, run_id: &str) -> Result<()> {
        // 异常、取消和墙钟超时可能没有常规 RunFinished；通过既有稳定事件 ABI
        // 通知 Guest 清理该 run 的瞬时状态，不把这条内部收尾事件写入应用 EventSink。
        let cleanup = AgentEvent::new(
            run_id,
            AgentEventKind::RunFinished,
            0,
            serde_json::json!({"aborted": true, "host_cleanup": true}),
        );
        let callback = match serde_json::to_value(AgentRecordEvent { record: cleanup }) {
            Ok(data) => self
                .deliver_event(
                    &PluginEventEnvelope {
                        event: AgentRecordEvent::NAME.to_string(),
                        cancelled: false,
                        canceller: None,
                        subscriber: self.id().to_string(),
                        data,
                    },
                    Some(run_id),
                )
                .await
                .map(|_| ()),
            Err(error) => Err(error.into()),
        };
        let drained = self.contributions.drain_events_for_run(run_id);
        match (callback, drained) {
            (Err(error), Err(drain_error)) => {
                Err(error.context(format!("清理插件运行事件队列同时失败：{drain_error:#}")))
            }
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(_)) => Ok(()),
        }
    }
}

#[async_trait]
impl PluginHost for WasmPluginHost {
    fn id(&self) -> Option<&str> {
        Some(WasmPluginHost::id(self))
    }

    fn agent_runtime_api(
        &self,
        plugin_id: &str,
    ) -> Option<Arc<dyn agent_runtime::AgentRuntimeApi>> {
        (plugin_id == WasmPluginHost::id(self))
            .then(|| self.agent_runtime.as_ref().map(|binding| binding.api()))
            .flatten()
    }

    fn event_subscriptions(&self) -> &[EventSubscription] {
        &self.manifest.events
    }

    async fn dispatch_event(&self, ctx: &mut EventContext<'_>) -> Result<()> {
        let data = ctx.snapshot()?;
        let envelope = PluginEventEnvelope {
            event: ctx.name().to_string(),
            cancelled: ctx.is_cancelled(),
            canceller: ctx.canceller().map(str::to_string),
            // 订阅身份由 Host 注入的可信插件 ID 决定，Guest 无法自行声明。
            subscriber: WasmPluginHost::id(self).to_string(),
            data,
        };
        let run_id = snapshot_run_id(&envelope.data).map(str::to_string);

        let response = self.deliver_event(&envelope, run_id.as_deref()).await?;
        let PluginEventResponse::Ready { cancel, patch } = response else {
            unreachable!("deliver_event 只返回 Ready 应答")
        };
        // Monitor 级订阅只观察最终结果，Guest 的改写与取消一律不生效。
        if ctx.is_read_only() {
            return Ok(());
        }
        if !patch.is_null() {
            ctx.merge(&patch)?;
        }
        if cancel {
            ctx.cancel();
        }
        Ok(())
    }

    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        let mut guest_request = request.clone();
        redact_guest_context_messages(&mut guest_request.messages);
        let request_json = serde_json::to_string(&guest_request)?;
        let run_id = request.run_id.clone();
        let plugin_id = self.id().to_string();
        complete_component_call(
            &self.state,
            format!("plugin `{plugin_id}` load-context"),
            WORK_CALL_BUDGET,
            move |state| {
                Box::pin(async move {
                    let fuel = context_fuel_budget(&state.limits, request_json.len());
                    set_fuel(state, fuel)?;
                    let run_scope = state
                        .store
                        .data()
                        .capabilities
                        .enter_active_run_scope(Some(&run_id));
                    let load_context = state.load_context;
                    let invocation = load_context
                        .call_async(&mut state.store, (request_json,))
                        .await
                        .into_anyhow();
                    drop(run_scope);
                    let (response_json,) = invocation
                        .with_context(|| format!("plugin `{plugin_id}` load-context failed"))?;
                    let response: GuestContextLoadResponse = serde_json::from_str(&response_json)
                        .with_context(|| {
                        format!("plugin `{plugin_id}` returned invalid context response")
                    })?;
                    if let Some(error) = response.error {
                        return Err(anyhow!("plugin `{plugin_id}` context load failed: {error}"));
                    }
                    let mut context = response.context;
                    if let Some(context) = context.as_mut() {
                        redact_guest_context_messages(&mut context.messages);
                    }
                    Ok(context)
                })
            },
        )
        .await
    }

    async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
        Ok(self.known_ui.clone())
    }

    async fn tool_renderers(&self) -> Result<Vec<ToolRendererContribution>> {
        Ok(self.known_tool_renderers.clone())
    }

    async fn render_ui(&self, request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        if request.plugin_id != self.id() {
            return Ok(None);
        }
        let Some(declaration) = self
            .known_ui
            .iter()
            .find(|declaration| declaration.view_id == request.view_id)
        else {
            return Ok(None);
        };
        validate_ui_instance(declaration, request.instance_id.as_deref())?;

        let request_json = serde_json::to_string(request)?;
        let view_id = request.view_id.clone();
        let plugin_id = self.id().to_string();
        complete_component_call(
            &self.state,
            format!("plugin `{plugin_id}` render-ui"),
            UI_CALL_BUDGET,
            move |state| {
                Box::pin(async move {
                    refill_fuel(state)?;
                    let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
                    let render_ui = state.render_ui;
                    let (frame_json,) = render_ui
                        .call_async(&mut state.store, (request_json,))
                        .await
                        .into_anyhow()
                        .with_context(|| format!("plugin `{plugin_id}` render-ui failed"))?;
                    drop(run_scope);
                    if frame_json.is_empty() {
                        return Ok(None);
                    }
                    let frame = guard::decode_ui_frame(&frame_json).with_context(|| {
                        format!("plugin `{plugin_id}` returned invalid UiFrame JSON")
                    })?;
                    if frame.view_id != view_id {
                        return Err(anyhow!(
                            "plugin `{plugin_id}` rendered view `{}` for request `{view_id}`",
                            frame.view_id
                        ));
                    }
                    Ok(Some(frame))
                })
            },
        )
        .await
    }

    async fn render_tool(&self, request: &ToolRenderRequest) -> Result<Option<UiFrame>> {
        if request.plugin_id != self.id() {
            return Ok(None);
        }
        let Some(renderer) = self.known_tool_renderers.iter().find(|renderer| {
            renderer.renderer_id == request.renderer_id
                && renderer.tool_name == request.context.call.name
        }) else {
            return Ok(None);
        };

        let request_json = serde_json::to_string(request)?;
        let renderer_id = renderer.renderer_id.clone();
        let max_height = request.context.max_height;
        let plugin_id = self.id().to_string();
        complete_component_call(
            &self.state,
            format!("plugin `{plugin_id}` tool renderer"),
            UI_CALL_BUDGET,
            move |state| Box::pin(async move {
                refill_fuel(state)?;
                let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
                let render_ui = state.render_ui;
                let (frame_json,) = render_ui
                    .call_async(&mut state.store, (request_json,))
                    .await
                    .into_anyhow()
                    .with_context(|| format!("plugin `{plugin_id}` tool renderer failed"))?;
                drop(run_scope);
                if frame_json.is_empty() {
                    return Ok(None);
                }
                let frame = guard::decode_ui_frame(&frame_json).with_context(|| {
                    format!("plugin `{plugin_id}` returned invalid UiFrame JSON")
                })?;
                if frame.view_id != renderer_id {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` rendered view `{}` for tool renderer `{renderer_id}`",
                        frame.view_id
                    ));
                }
                if frame.lines.len() > usize::from(max_height) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` tool renderer `{renderer_id}` returned {} lines above limit {max_height}",
                        frame.lines.len()
                    ));
                }
                Ok(Some(frame))
            }),
        )
        .await
    }

    async fn on_ui_input(&self, input: &UiInput) -> Result<()> {
        if input.plugin_id != self.id() {
            return Ok(());
        }
        let Some(declaration) = self
            .known_ui
            .iter()
            .find(|declaration| declaration.view_id == input.view_id)
        else {
            return Ok(());
        };
        validate_ui_instance(declaration, input.instance_id.as_deref())?;

        let input_json = serde_json::to_string(input)?;
        let plugin_id = self.id().to_string();
        complete_component_call(
            &self.state,
            format!("plugin `{plugin_id}` on-ui-input"),
            UI_CALL_BUDGET,
            move |state| {
                Box::pin(async move {
                    refill_fuel(state)?;
                    let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
                    let on_ui_input = state.on_ui_input;
                    let invocation = on_ui_input
                        .call_async(&mut state.store, (input_json,))
                        .await
                        .into_anyhow();
                    drop(run_scope);
                    invocation
                        .with_context(|| format!("plugin `{plugin_id}` on-ui-input failed"))?;
                    Ok(())
                })
            },
        )
        .await
    }

    async fn services(&self) -> Result<Vec<PluginService>> {
        self.services.list(Some(self.id()))
    }

    async fn call_service(&self, call: &PluginServiceCall) -> Result<Option<Value>> {
        if call.plugin_id != self.id() {
            return Ok(None);
        }
        Ok(Some(self.services.call(call.clone()).await?))
    }

    async fn shutdown(&self) -> Result<()> {
        self.deactivate().await
    }
}

#[cfg(test)]
mod tests;

/// 从事件快照中读取 Core 绑定的运行 ID。
///
/// 事件负载由 Core 构造，因此这里读到的 `run_id` 是可信的；Guest 无法通过改写补丁
/// 影响它，能力导入的运行作用域也就不会被指向其他运行。
fn snapshot_run_id(data: &Value) -> Option<&str> {
    data.get("run_id")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/record/run_id").and_then(Value::as_str))
        .filter(|run_id| !run_id.is_empty())
}
