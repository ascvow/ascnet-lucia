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
use crate::manifest::PluginManifest;
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
use agent_core::ContextLoadRequest;
use agent_core::LoadedContext;
use agent_core::ToolDecision;
use agent_core::{AgentEvent, AgentEventKind};
use agent_runtime::RuntimePrincipal;
use agent_tool::ToolCall;
use agent_tool::ToolDecisionStatus;
use agent_tool::ToolResult;
use agent_tool::ToolSpec;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::Mutex;
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
    state: Arc<Mutex<WasmPluginState>>,
}

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
    before_tool: TypedFunc<(String,), (String,)>,
    after_tool: TypedFunc<(String,), ()>,
    on_event: TypedFunc<(String,), ()>,
    load_context: TypedFunc<(String,), (String,)>,
    handle_service: TypedFunc<(String,), (String,)>,
    deactivate: TypedFunc<(), (String,)>,
    render_ui: TypedFunc<(String,), (String,)>,
    on_ui_input: TypedFunc<(String,), ()>,
    limits: WasmPluginLimits,
}

/// 服务目录持有的弱引用端点，避免服务注册表与 WASM store 形成引用环。
struct WasmServiceEndpoint {
    plugin_id: String,
    state: Weak<Mutex<WasmPluginState>>,
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
        let mut state = state.lock().await;
        refill_fuel(&mut state)?;
        let handle_service = state.handle_service;
        let request = serde_json::to_string(&serde_json::json!({
            "caller_id": call.caller_id,
            "name": call.name,
            "payload": call.payload,
        }))?;
        let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
        let (response_json,) = handle_service
            .call_async(&mut state.store, (request,))
            .await
            .into_anyhow()
            .with_context(|| format!("插件 `{}` 服务调用失败", self.plugin_id))?;
        drop(run_scope);
        let response: GuestServiceResponse = serde_json::from_str(&response_json)
            .with_context(|| format!("插件 `{}` 返回了无效服务响应", self.plugin_id))?;
        if response.ok {
            Ok(response.value)
        } else {
            Err(anyhow!(
                "{}",
                response.error.unwrap_or_else(|| "插件服务调用失败".into())
            ))
        }
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

        let engine = shared_wasm_engine()?;
        let wasm_path = wasm_path.as_ref();
        let canonical_wasm_path = wasm_path
            .canonicalize()
            .with_context(|| format!("无法解析插件 Component 路径：{}", wasm_path.display()))?;
        let component_bytes = fs::read(wasm_path)
            .with_context(|| format!("无法读取插件 Component：{}", wasm_path.display()))?;
        let state_identity =
            derive_plugin_state_identity(&manifest, &canonical_wasm_path, &component_bytes)?;
        let component = Component::new(&engine, &component_bytes)
            .into_anyhow()
            .with_context(|| {
                format!("failed to compile wasm component: {}", wasm_path.display())
            })?;

        let mut linker = Linker::<PluginWasiState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .into_anyhow()
            .context("failed to add WASI Preview 2 imports to linker")?;
        add_plugin_host_imports(&mut linker)?;

        let agent_runtime =
            provision_agent_runtime(&manifest, host_services.agent_runtime()).await?;
        let mut cleanup_agent_runtime = AgentRuntimeLoadGuard::new(agent_runtime.clone());
        let loading: Result<Self> = async {
            let mut wasi = WasiCtxBuilder::new();
            // 刻意保持最小 WASI：不继承环境变量、不预打开目录、不继承 stdio。
            wasi.arg(&manifest.plugin.id);

            let contributions = Arc::new(ContributionRegistry::default());
            let store_limits = StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
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
                            host_services.model_completion(),
                            host_services.execution_policy(),
                            host_services.persistent_state_dir(),
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
            if let Some(interval) = limits.fuel_yield_interval {
                store
                    .fuel_async_yield_interval(Some(interval))
                    .into_anyhow()
                    .context("failed to configure plugin fuel yield interval")?;
            }

            let instance = linker
                .instantiate_async(&mut store, &component)
                .await
                .into_anyhow()
                .context("failed to instantiate wasm component")?;

            let list_tools =
                get_required_func::<(), (String,)>(&instance, &mut store, "list-tools")?;
            let call_tool =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "call-tool")?;
            let before_tool =
                get_required_func::<(String,), (String,)>(&instance, &mut store, "before-tool")?;
            let after_tool =
                get_required_func::<(String,), ()>(&instance, &mut store, "after-tool")?;
            let on_event = get_required_func::<(String,), ()>(&instance, &mut store, "on-event")?;
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

            let (tools_json,) = list_tools
                .call_async(&mut store, ())
                .await
                .into_anyhow()
                .context("plugin `list-tools` failed")?;
            let mut static_tools: Vec<ToolSpec> = serde_json::from_str(&tools_json)
                .with_context(|| "plugin `list-tools` returned invalid ToolSpec JSON")?;
            for tool in &mut static_tools {
                manifest.capabilities.bind_tool_scope(tool)?;
            }
            contributions.upsert_static_tools(static_tools)?;

            let plugin_id = manifest.plugin.id.clone();
            let state = Arc::new(Mutex::new(WasmPluginState {
                store,
                instance,
                call_tool,
                before_tool,
                after_tool,
                on_event,
                load_context,
                handle_service,
                deactivate,
                render_ui,
                on_ui_input,
                limits,
            }));
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

            let initialization = async {
                let mut state = state.lock().await;
                refill_fuel(&mut state)?;
                let mut activation_metadata = manifest.metadata.clone();
                activation_metadata.extend(host_services.activation_metadata(&plugin_id));
                let context_json = serde_json::to_string(&serde_json::json!({
                    "plugin_id": &plugin_id,
                    "metadata": activation_metadata,
                }))?;
                let activation_scope = state.store.data().capabilities.enter_active_run_scope(None);
                let (activation_error,) = activate
                    .call_async(&mut state.store, (context_json,))
                    .await
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
                let (declarations_json,) = describe_ui
                    .call_async(&mut state.store, ())
                    .await
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
            let (known_ui, known_tool_renderers) = match initialization {
                Ok(contributions) => contributions,
                Err(error) => {
                    services.unregister_plugin(&plugin_id)?;
                    return Err(error);
                }
            };

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
            if let Some(binding) = cleanup_agent_runtime.take() {
                binding.revoke().await;
            }
        } else {
            cleanup_agent_runtime.disarm();
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
        let mut state = self.state.lock().await;
        let deactivation = async {
            let deactivate = state.deactivate;
            refill_fuel(&mut state)?;
            let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
            let (deactivation_error,) = deactivate
                .call_async(&mut state.store, ())
                .await
                .into_anyhow()
                .with_context(|| format!("plugin `{}` deactivate failed", self.id()))?;
            drop(run_scope);
            if deactivation_error.is_empty() {
                Ok(())
            } else {
                Err(anyhow!(
                    "plugin `{}` deactivation failed: {deactivation_error}",
                    self.id()
                ))
            }
        }
        .await;
        drop(state);
        let unregister = self.services.unregister_plugin(self.id());
        if let Some(binding) = &self.agent_runtime {
            binding.revoke().await;
        }
        deactivation.and(unregister)
    }

    /// 调用 Guest 后置钩子，并在能力导入期间绑定可信 Agent run。
    async fn after_tool_scoped(&self, run_id: Option<&str>, result: &ToolResult) -> Result<()> {
        let result_json = serde_json::to_string(result)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let run_scope = state
            .store
            .data()
            .capabilities
            .enter_active_run_scope(run_id);
        let after_tool = state.after_tool;
        let invocation = after_tool
            .call_async(&mut state.store, (result_json,))
            .await
            .into_anyhow();
        drop(run_scope);
        invocation.with_context(|| format!("plugin `{}` after-tool failed", self.id()))?;
        Ok(())
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
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
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
                let failure =
                    capability_failure.unwrap_or_else(|| classify_wasm_plugin_failure(&error));
                return Ok(Some(plugin_host_failure_result(
                    &call,
                    &public_name,
                    self.id(),
                    failure,
                )));
            }
        };
        if let Some(failure) = capability_failure {
            return Ok(Some(plugin_host_failure_result(
                &call,
                &public_name,
                self.id(),
                failure,
            )));
        }
        let mut result = match serde_json::from_str::<ToolResult>(&result_json) {
            Ok(result) => result,
            Err(_) => {
                return Ok(Some(plugin_host_failure_result(
                    &call,
                    &public_name,
                    self.id(),
                    PluginHostFailureKind::ContractViolation,
                )));
            }
        };
        bind_guest_tool_result(&mut result, &call, &public_name);
        Ok(Some(result))
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        let call_json = serde_json::to_string(call)?;
        loop {
            let decision = {
                let mut state = self.state.lock().await;
                refill_fuel(&mut state)?;
                let run_scope = state
                    .store
                    .data()
                    .capabilities
                    .enter_active_run_scope(call.run_id.as_deref());
                let before_tool = state.before_tool;
                let invocation = before_tool
                    .call_async(&mut state.store, (call_json.clone(),))
                    .await
                    .into_anyhow();
                drop(run_scope);
                let (decision_json,) = invocation
                    .with_context(|| format!("plugin `{}` before-tool failed", self.id()))?;
                serde_json::from_str::<ToolDecisionStatus>(&decision_json).with_context(|| {
                    format!(
                        "plugin `{}` returned invalid ToolDecisionStatus JSON",
                        self.id()
                    )
                })?
            };
            match decision {
                ToolDecisionStatus::Pending { retry_after_ms } => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        retry_after_ms.clamp(50, 1_000),
                    ))
                    .await;
                }
                ToolDecisionStatus::Ready { decision } => return Ok(decision),
            }
        }
    }

    async fn after_tool(&self, result: &ToolResult) -> Result<()> {
        self.after_tool_scoped(None, result).await
    }

    async fn after_tool_for_run(&self, run_id: &str, result: &ToolResult) -> Result<()> {
        self.after_tool_scoped(Some(run_id), result).await
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        // 私有推理增量只面向可信应用 Recorder；WASM 边界再次 fail-closed。
        if event.kind == AgentEventKind::ModelThinkingDelta {
            return Ok(());
        }
        let event_json = serde_json::to_string(event)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let run_scope = state
            .store
            .data()
            .capabilities
            .enter_active_run_scope(Some(&event.run_id));
        let on_event = state.on_event;
        let invocation = on_event
            .call_async(&mut state.store, (event_json,))
            .await
            .into_anyhow();
        drop(run_scope);
        invocation.with_context(|| format!("plugin `{}` on-event failed", self.id()))?;
        Ok(())
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
        let callback = self.on_event(&cleanup).await;
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

    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        let mut guest_request = request.clone();
        redact_guest_context_messages(&mut guest_request.messages);
        let request_json = serde_json::to_string(&guest_request)?;
        let mut state = self.state.lock().await;
        let fuel = context_fuel_budget(&state.limits, request_json.len());
        set_fuel(&mut state, fuel)?;
        let run_scope = state
            .store
            .data()
            .capabilities
            .enter_active_run_scope(Some(&request.run_id));
        let load_context = state.load_context;
        let invocation = load_context
            .call_async(&mut state.store, (request_json,))
            .await
            .into_anyhow();
        drop(run_scope);
        let (response_json,) =
            invocation.with_context(|| format!("plugin `{}` load-context failed", self.id()))?;
        let response: GuestContextLoadResponse = serde_json::from_str(&response_json)
            .with_context(|| format!("plugin `{}` returned invalid context response", self.id()))?;
        if let Some(error) = response.error {
            return Err(anyhow!(
                "plugin `{}` context load failed: {error}",
                self.id()
            ));
        }
        let mut context = response.context;
        if let Some(context) = context.as_mut() {
            redact_guest_context_messages(&mut context.messages);
        }
        Ok(context)
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
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
        let render_ui = state.render_ui;
        let (frame_json,) = render_ui
            .call_async(&mut state.store, (request_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` render-ui failed", self.id()))?;
        drop(run_scope);
        if frame_json.is_empty() {
            return Ok(None);
        }
        let frame = serde_json::from_str::<UiFrame>(&frame_json)
            .with_context(|| format!("plugin `{}` returned invalid UiFrame JSON", self.id()))?;
        if frame.view_id != request.view_id {
            return Err(anyhow!(
                "plugin `{}` rendered view `{}` for request `{}`",
                self.id(),
                frame.view_id,
                request.view_id
            ));
        }
        Ok(Some(frame))
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
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
        let render_ui = state.render_ui;
        let (frame_json,) = render_ui
            .call_async(&mut state.store, (request_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` tool renderer failed", self.id()))?;
        drop(run_scope);
        if frame_json.is_empty() {
            return Ok(None);
        }
        let frame = serde_json::from_str::<UiFrame>(&frame_json)
            .with_context(|| format!("plugin `{}` returned invalid UiFrame JSON", self.id()))?;
        if frame.view_id != renderer.renderer_id {
            return Err(anyhow!(
                "plugin `{}` rendered view `{}` for tool renderer `{}`",
                self.id(),
                frame.view_id,
                renderer.renderer_id
            ));
        }
        if frame.lines.len() > usize::from(request.context.max_height) {
            return Err(anyhow!(
                "plugin `{}` tool renderer `{}` returned {} lines above limit {}",
                self.id(),
                renderer.renderer_id,
                frame.lines.len(),
                request.context.max_height
            ));
        }
        Ok(Some(frame))
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
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let run_scope = state.store.data().capabilities.enter_active_run_scope(None);
        let on_ui_input = state.on_ui_input;
        let invocation = on_ui_input
            .call_async(&mut state.store, (input_json,))
            .await
            .into_anyhow();
        drop(run_scope);
        invocation.with_context(|| format!("plugin `{}` on-ui-input failed", self.id()))?;
        Ok(())
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
