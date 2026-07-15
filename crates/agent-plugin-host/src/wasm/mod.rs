//! WASM Component 插件宿主。
//!
//! WIT world 故意在 ABI 边界使用 JSON 字符串。这样第一版 ABI 更稳定，
//! Rust 内部类型可以继续通过 serde 演进。

use super::{
    capability::{encode_host_response, AgentRuntimeBinding, CapabilityState},
    contribution::ContributionRegistry,
    manifest::{
        resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest,
        CONTEXT_LOADER_CAPABILITY, TOOL_POLICY_CAPABILITY,
    },
    service::{PluginService, PluginServiceCall, ServiceHandler, ServiceRegistry},
    AgentEvent, AgentRuntimeHostServices, CompositePluginHost, LivePluginHost, PluginHost,
    PluginHostServices, ToolDecision, ToolRendererContribution, UiDeclaration, UiFrame, UiInput,
    UiRenderRequest,
};
use crate::ui::{UiContribution, UiPlacement};
use agent_core::{model::ModelMessage, AgentExtension, ContextLoadRequest, LoadedContext};
use agent_runtime::RuntimePrincipal;
use agent_tool::{ToolCall, ToolDecisionStatus, ToolResult, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use semver::{Version, VersionReq};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};
use tokio::sync::Mutex;
use uuid::Uuid;
use wasmtime::component::{
    Component, ComponentNamedList, Instance, Lift, Linker, Lower, TypedFunc,
};
#[cfg(test)]
use wasmtime::Engine;
use wasmtime::{Store, StoreContextMut, StoreLimitsBuilder};
use wasmtime_wasi::WasiCtxBuilder;

/// 上下文加载每个序列化输入字节追加的 WASM fuel，覆盖 JSON 遍历与压缩成本。
const CONTEXT_FUEL_PER_INPUT_BYTE: u64 = 512;
/// 单次上下文加载允许使用的最高 fuel，避免超大请求解除计算资源上限。
const MAX_CONTEXT_FUEL: u64 = 500_000_000;

mod engine;
mod loader;

pub use engine::{configure_wasm_cache_directory, WasmPluginLimits};
use engine::{shared_wasm_engine, IntoAnyhow, PluginWasiState};

#[cfg(test)]
use loader::{
    failed_required_dependencies, prioritize_progressive_order, resilient_dependency_plan,
};
pub use loader::{
    load_wasm_plugins, load_wasm_plugins_progressively_with_selection_and_services,
    load_wasm_plugins_resilient, load_wasm_plugins_resilient_with_selection,
    load_wasm_plugins_resilient_with_selection_and_services, load_wasm_plugins_with_selection,
    load_wasm_plugins_with_selection_and_services, load_wasm_plugins_with_services,
    ProgressivePluginLoadUpdate,
};

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
        let (response_json,) = handle_service
            .call_async(&mut state.store, (request,))
            .await
            .into_anyhow()
            .with_context(|| format!("插件 `{}` 服务调用失败", self.plugin_id))?;
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
        Self::load_with_limits_in_dir(
            manifest,
            wasm_path,
            base_dir.to_path_buf(),
            limits,
            Arc::new(ServiceRegistry::default()),
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
        Self::load_with_limits_in_dir(
            manifest,
            wasm_path,
            plugin_dir,
            limits,
            Arc::new(ServiceRegistry::default()),
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
        let component = Component::from_file(&engine, wasm_path.as_ref())
            .into_anyhow()
            .with_context(|| {
                format!(
                    "failed to compile wasm component: {}",
                    wasm_path.as_ref().display()
                )
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
                .build();
            let mut store = Store::new(
                &engine,
                PluginWasiState::new(
                    wasi.build(),
                    CapabilityState::new(
                        manifest.plugin.id.clone(),
                        plugin_dir,
                        manifest.capabilities.clone(),
                        contributions.clone(),
                        services.clone(),
                        agent_runtime.clone(),
                        host_services.model_completion(),
                    ),
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
            let static_tools: Vec<ToolSpec> = serde_json::from_str(&tools_json)
                .with_context(|| "plugin `list-tools` returned invalid ToolSpec JSON")?;
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
                Arc::new(WasmServiceEndpoint {
                    plugin_id: plugin_id.clone(),
                    state: Arc::downgrade(&state),
                }),
            )?;

            let initialization = async {
                let mut state = state.lock().await;
                refill_fuel(&mut state)?;
                let context_json = serde_json::to_string(&serde_json::json!({
                    "plugin_id": &plugin_id,
                    "metadata": &manifest.metadata,
                }))?;
                let (activation_error,) = activate
                    .call_async(&mut state.store, (context_json,))
                    .await
                    .into_anyhow()
                    .with_context(|| format!("plugin `{plugin_id}` activate failed"))?;
                if !activation_error.is_empty() {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` activation failed: {activation_error}"
                    ));
                }

                refill_fuel(&mut state)?;
                let (declarations_json,) = describe_ui
                    .call_async(&mut state.store, ())
                    .await
                    .into_anyhow()
                    .context("plugin `describe-ui` failed")?;
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
            let (deactivation_error,) = deactivate
                .call_async(&mut state.store, ())
                .await
                .into_anyhow()
                .with_context(|| format!("plugin `{}` deactivate failed", self.id()))?;
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
        let public_name = call.name.clone();
        let Some(local_name) = self.contributions.local_tool_name(&public_name)? else {
            return Ok(None);
        };
        call.name = local_name;

        let call_json = serde_json::to_string(&call)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let call_tool = state.call_tool;
        let (result_json,) = call_tool
            .call_async(&mut state.store, (call_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` call-tool failed", self.id()))?;
        let mut result = serde_json::from_str::<ToolResult>(&result_json)
            .with_context(|| format!("plugin `{}` returned invalid ToolResult JSON", self.id()))?;
        result.name = public_name;
        Ok(Some(result))
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        let call_json = serde_json::to_string(call)?;
        loop {
            let decision = {
                let mut state = self.state.lock().await;
                refill_fuel(&mut state)?;
                let before_tool = state.before_tool;
                let (decision_json,) = before_tool
                    .call_async(&mut state.store, (call_json.clone(),))
                    .await
                    .into_anyhow()
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
        let result_json = serde_json::to_string(result)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let after_tool = state.after_tool;
        after_tool
            .call_async(&mut state.store, (result_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` after-tool failed", self.id()))?;
        Ok(())
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        let event_json = serde_json::to_string(event)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let on_event = state.on_event;
        on_event
            .call_async(&mut state.store, (event_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` on-event failed", self.id()))?;
        Ok(())
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        self.contributions.drain_events()
    }
}

#[async_trait]
impl PluginHost for WasmPluginHost {
    fn id(&self) -> Option<&str> {
        Some(WasmPluginHost::id(self))
    }

    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        let request_json = serde_json::to_string(request)?;
        let mut state = self.state.lock().await;
        let fuel = context_fuel_budget(&state.limits, request_json.len());
        set_fuel(&mut state, fuel)?;
        let load_context = state.load_context;
        let (response_json,) = load_context
            .call_async(&mut state.store, (request_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` load-context failed", self.id()))?;
        let response: GuestContextLoadResponse = serde_json::from_str(&response_json)
            .with_context(|| format!("plugin `{}` returned invalid context response", self.id()))?;
        if let Some(error) = response.error {
            return Err(anyhow!(
                "plugin `{}` context load failed: {error}",
                self.id()
            ));
        }
        Ok(response.context)
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
        let declaration = self
            .known_ui
            .iter()
            .find(|declaration| declaration.view_id == request.view_id);
        let is_tool_renderer = self
            .known_tool_renderers
            .iter()
            .any(|renderer| renderer.renderer_id == request.view_id);
        if declaration.is_none() && !is_tool_renderer {
            return Ok(None);
        }
        if let Some(declaration) = declaration {
            validate_ui_instance(declaration, request.instance_id.as_deref())?;
        } else {
            validate_tool_renderer_instance(&request.view_id, request.instance_id.as_deref())?;
        }

        let request_json = serde_json::to_string(request)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let render_ui = state.render_ui;
        let (frame_json,) = render_ui
            .call_async(&mut state.store, (request_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` render-ui failed", self.id()))?;
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

    async fn on_ui_input(&self, input: &UiInput) -> Result<()> {
        if input.plugin_id != self.id() {
            return Ok(());
        }
        let declaration = self
            .known_ui
            .iter()
            .find(|declaration| declaration.view_id == input.view_id);
        let is_tool_renderer = self
            .known_tool_renderers
            .iter()
            .any(|renderer| renderer.renderer_id == input.view_id);
        if declaration.is_none() && !is_tool_renderer {
            return Ok(());
        }
        if let Some(declaration) = declaration {
            validate_ui_instance(declaration, input.instance_id.as_deref())?;
        } else {
            validate_tool_renderer_instance(&input.view_id, input.instance_id.as_deref())?;
        }

        let input_json = serde_json::to_string(input)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let on_ui_input = state.on_ui_input;
        on_ui_input
            .call_async(&mut state.store, (input_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` on-ui-input failed", self.id()))?;
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

/// 把协议无关的 Agent、文件和进程能力注册到 component 根命名空间。
fn add_plugin_host_imports(linker: &mut Linker<PluginWasiState>) -> Result<()> {
    let mut root = linker.root();
    root.func_wrap(
        "host-agent-upsert-tool",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.upsert_tool(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-upsert-tool 失败")?;
    root.func_wrap(
        "host-agent-remove-tool",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_tool(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-remove-tool 失败")?;
    root.func_wrap(
        "host-agent-upsert-prompt",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.upsert_prompt(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-upsert-prompt 失败")?;
    root.func_wrap(
        "host-agent-remove-prompt",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_prompt(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-remove-prompt 失败")?;
    root.func_wrap(
        "host-agent-emit-event",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.emit_event(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-emit-event 失败")?;
    root.func_wrap(
        "host-state-get",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.get_state(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-state-get 失败")?;
    root.func_wrap(
        "host-state-set",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.set_state(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-state-set 失败")?;
    root.func_wrap(
        "host-state-remove",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_state(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-state-remove 失败")?;
    root.func_wrap(
        "host-service-upsert",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.upsert_service(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-service-upsert 失败")?;
    root.func_wrap(
        "host-service-remove",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_service(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-service-remove 失败")?;
    root.func_wrap(
        "host-service-list",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.list_services(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-service-list 失败")?;
    root.func_wrap_async(
        "host-service-call",
        |caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (caller_id, services) = caller.data().capabilities.service_context();
            Box::new(async move {
                let result =
                    CapabilityState::call_service_with(caller_id, services, &request).await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-service-call 失败")?;
    root.func_wrap(
        "host-fs-read",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.read_file(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-fs-read 失败")?;
    root.func_wrap(
        "host-fs-list",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.list_dir(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-fs-list 失败")?;
    root.func_wrap(
        "host-process-spawn",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.spawn_process(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-process-spawn 失败")?;
    root.func_wrap_async(
        "host-process-write",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Box::new(async move {
                let result = caller.data_mut().capabilities.write_process(&request).await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-process-write 失败")?;
    root.func_wrap_async(
        "host-process-read-line",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Box::new(async move {
                let result = caller
                    .data_mut()
                    .capabilities
                    .read_process_line(&request)
                    .await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-process-read-line 失败")?;
    root.func_wrap_async(
        "host-process-kill",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Box::new(async move {
                let result = caller.data_mut().capabilities.kill_process(&request).await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-process-kill 失败")?;
    root.func_wrap_async(
        "host-model-complete",
        |caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (allowed, binding) = caller.data().capabilities.model_completion_context();
            Box::new(async move {
                let result = CapabilityState::complete_model_with(allowed, binding, &request).await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-model-complete 失败")?;
    root.func_wrap_async(
        "host-agent-runtime-call",
        |caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (permissions, binding) = caller.data().capabilities.agent_runtime_context();
            Box::new(async move {
                let result =
                    CapabilityState::call_agent_runtime_with(permissions, binding, &request).await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-agent-runtime-call 失败")?;
    Ok(())
}

fn refill_fuel(state: &mut WasmPluginState) -> Result<()> {
    set_fuel(state, state.limits.fuel)
}

/// 按单次上下文输入大小分配 fuel，并保留显式的最大计算预算。
fn context_fuel_budget(limits: &WasmPluginLimits, input_bytes: usize) -> u64 {
    let extra = u64::try_from(input_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(CONTEXT_FUEL_PER_INPUT_BYTE);
    limits
        .fuel
        .saturating_add(extra)
        .min(MAX_CONTEXT_FUEL.max(limits.fuel))
}

/// 将指定 fuel 重置到单插件 store。
fn set_fuel(state: &mut WasmPluginState, fuel: u64) -> Result<()> {
    state
        .store
        .set_fuel(fuel)
        .into_anyhow()
        .context("failed to refill plugin fuel")
}

/// 校验插件内部视图 ID，并注入可信的 manifest 插件 ID。
fn validate_ui_contributions(
    plugin_id: &str,
    contributions: Vec<UiContribution>,
    tools: &[ToolSpec],
) -> Result<(Vec<UiDeclaration>, Vec<ToolRendererContribution>)> {
    let mut view_ids = HashSet::new();
    let owned_tools = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<HashSet<_>>();
    let mut declarations = Vec::new();
    let mut tool_renderers = Vec::new();
    for contribution in contributions {
        match contribution {
            UiContribution::View(mut declaration) => {
                if declaration.view_id.trim().is_empty() {
                    return Err(anyhow!("plugin `{plugin_id}` has an empty UI view id"));
                }
                if !view_ids.insert(declaration.view_id.clone()) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` declares duplicate UI route `{}`",
                        declaration.view_id
                    ));
                }
                declaration.plugin_id = plugin_id.to_string();
                declarations.push(declaration);
            }
            UiContribution::ToolRenderer(mut renderer) => {
                if renderer.renderer_id.trim().is_empty() {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` has an empty tool renderer id"
                    ));
                }
                if !view_ids.insert(renderer.renderer_id.clone()) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` declares duplicate UI route `{}`",
                        renderer.renderer_id
                    ));
                }
                if !owned_tools.contains(renderer.tool_name.as_str()) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` cannot render unowned tool `{}`",
                        renderer.tool_name
                    ));
                }
                renderer.plugin_id = plugin_id.to_string();
                tool_renderers.push(renderer);
            }
        }
    }
    Ok((declarations, tool_renderers))
}

/// 工具 renderer 必须使用稳定调用 ID 作为实例路由键。
fn validate_tool_renderer_instance(renderer_id: &str, instance_id: Option<&str>) -> Result<()> {
    match instance_id {
        Some(instance_id)
            if !instance_id.is_empty()
                && instance_id.len() <= 256
                && !instance_id.chars().any(char::is_control) =>
        {
            Ok(())
        }
        _ => Err(anyhow!(
            "工具 renderer `{renderer_id}` 需要有效的工具调用 instance_id"
        )),
    }
}

/// 校验静态视图与动态子视图的实例路由是否匹配。
fn validate_ui_instance(declaration: &UiDeclaration, instance_id: Option<&str>) -> Result<()> {
    match (declaration.placement, instance_id) {
        (UiPlacement::Subview, Some(instance_id))
            if !instance_id.is_empty()
                && instance_id.len() <= 256
                && !instance_id.chars().any(char::is_control) =>
        {
            Ok(())
        }
        (UiPlacement::Subview, _) => Err(anyhow!(
            "subview `{}` 需要有效 instance_id",
            declaration.view_id
        )),
        (_, None) => Ok(()),
        (_, Some(_)) => Err(anyhow!(
            "非 subview `{}` 不允许携带 instance_id",
            declaration.view_id
        )),
    }
}

fn get_required_func<Params, Results>(
    instance: &Instance,
    store: &mut Store<PluginWasiState>,
    name: &str,
) -> Result<TypedFunc<Params, Results>>
where
    Params: ComponentNamedList + Lower + Send + Sync + 'static,
    Results: ComponentNamedList + Lift + Send + Sync + 'static,
{
    instance
        .get_typed_func::<Params, Results>(store, name)
        .into_anyhow()
        .with_context(|| {
            format!("plugin missing export `{name}` or the export has incompatible type")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PluginDependency, PluginSection, SUPPORTED_PLUGIN_API_VERSION};
    use crate::ui::UiSize;

    /// 默认运行时限制必须同时约束计算量和线性内存。
    #[test]
    fn default_limits_bound_fuel_and_memory() {
        let limits = WasmPluginLimits::default();

        assert!(limits.fuel > 0);
        assert_eq!(limits.max_memory_bytes, 64 * 1024 * 1024);
    }

    /// 上下文输入应按大小获得额外 fuel，且不得突破全局硬上限。
    #[test]
    fn context_fuel_budget_scales_with_input_and_stays_bounded() {
        let limits = WasmPluginLimits::default();
        assert_eq!(context_fuel_budget(&limits, 0), limits.fuel);
        assert_eq!(
            context_fuel_budget(&limits, 300_000),
            limits.fuel + 300_000 * CONTEXT_FUEL_PER_INPUT_BYTE
        );
        assert_eq!(context_fuel_budget(&limits, usize::MAX), MAX_CONTEXT_FUEL);
    }

    /// 多次请求运行时 Engine 时必须复用同一个底层实例。
    #[test]
    fn wasm_plugin_loads_share_process_engine() -> Result<()> {
        let first = shared_wasm_engine()?;
        let second = shared_wasm_engine()?;

        assert!(Engine::same(&first, &second));
        Ok(())
    }

    /// Subviews require a valid instance ID, while static views reject dynamic routing.
    /// 子视图必须携带有效实例 ID，静态视图则拒绝动态实例路由。
    #[test]
    fn ui_instance_routing_matches_declared_placement() {
        let declaration = |placement| UiDeclaration {
            plugin_id: "demo".into(),
            view_id: "detail".into(),
            title: "详情".into(),
            placement,
            size: UiSize::default(),
            focusable: true,
            input_triggers: Vec::new(),
        };

        assert!(validate_ui_instance(&declaration(UiPlacement::Subview), Some("task-1")).is_ok());
        assert!(validate_ui_instance(&declaration(UiPlacement::Subview), None).is_err());
        assert!(validate_ui_instance(&declaration(UiPlacement::Right), None).is_ok());
        assert!(validate_ui_instance(&declaration(UiPlacement::Right), Some("task-1")).is_err());
    }

    /// Required dependency failures propagate transitively while optional failures do not block.
    ///
    /// 必选依赖失败应向下游传播，可选依赖失败不应阻止加载。
    #[test]
    fn resilient_dependencies_skip_only_required_dependents() {
        let manifest = |id: &str, dependency: PluginDependency| PluginManifest {
            plugin: PluginSection {
                id: id.into(),
                name: id.into(),
                version: "1.0.0".into(),
                api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
                wasm: format!("{id}.wasm"),
                description: None,
            },
            dependencies: vec![dependency],
            provides: Vec::new(),
            capabilities: Default::default(),
            metadata: HashMap::new(),
        };
        let dependency = |id: &str, optional: bool| PluginDependency {
            id: id.into(),
            version: "*".into(),
            optional,
        };

        let mut failed = HashSet::from(["provider".to_string()]);
        let consumer = manifest("consumer", dependency("provider", false));
        assert_eq!(
            failed_required_dependencies(&consumer, &failed),
            vec!["provider"]
        );
        failed.insert("consumer".into());
        let downstream = manifest("downstream", dependency("consumer", false));
        assert_eq!(
            failed_required_dependencies(&downstream, &failed),
            vec!["consumer"]
        );
        let optional = manifest("optional", dependency("provider", true));
        assert!(failed_required_dependencies(&optional, &failed).is_empty());
    }

    /// Static dependency failures exclude only required dependents and leave unrelated plugins.
    ///
    /// 静态依赖失败应只剔除必选依赖闭包，无关插件仍进入加载计划。
    #[test]
    fn resilient_plan_isolates_required_dependency_closure() {
        let manifest = |id: &str, dependencies: Vec<PluginDependency>| PluginManifest {
            plugin: PluginSection {
                id: id.into(),
                name: id.into(),
                version: "1.0.0".into(),
                api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
                wasm: format!("{id}.wasm"),
                description: None,
            },
            dependencies,
            provides: Vec::new(),
            capabilities: Default::default(),
            metadata: HashMap::new(),
        };
        let dependency = |id: &str, optional: bool| PluginDependency {
            id: id.into(),
            version: "*".into(),
            optional,
        };
        let manifests = vec![
            manifest("healthy", Vec::new()),
            manifest("consumer", vec![dependency("missing", false)]),
            manifest("downstream", vec![dependency("consumer", false)]),
            manifest("optional", vec![dependency("missing", true)]),
        ];

        let (order, failures) = resilient_dependency_plan(&manifests).expect("解析容错依赖计划");
        let loaded = order
            .iter()
            .map(|index| manifests[*index].plugin.id.as_str())
            .collect::<Vec<_>>();
        let failed = failures
            .iter()
            .map(|failure| failure.plugin_id.as_str())
            .collect::<HashSet<_>>();

        assert_eq!(loaded, vec!["healthy", "optional"]);
        assert_eq!(failed, HashSet::from(["consumer", "downstream"]));
        let downstream = failures
            .iter()
            .find(|failure| failure.plugin_id == "downstream")
            .expect("下游插件应被依赖失败剔除");
        assert_eq!(downstream.blocked_by, vec!["consumer"]);
    }

    /// Required dependency cycles do not prevent independent plugins from loading.
    ///
    /// 必选依赖循环不应阻止独立插件加载。
    #[test]
    fn resilient_plan_isolates_required_dependency_cycles() {
        let manifest = |id: &str, dependency_id: Option<&str>| PluginManifest {
            plugin: PluginSection {
                id: id.into(),
                name: id.into(),
                version: "1.0.0".into(),
                api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
                wasm: format!("{id}.wasm"),
                description: None,
            },
            dependencies: dependency_id
                .map(|dependency_id| PluginDependency {
                    id: dependency_id.into(),
                    version: "*".into(),
                    optional: false,
                })
                .into_iter()
                .collect(),
            provides: Vec::new(),
            capabilities: Default::default(),
            metadata: HashMap::new(),
        };
        let manifests = vec![
            manifest("cycle-a", Some("cycle-b")),
            manifest("cycle-b", Some("cycle-a")),
            manifest("healthy", None),
        ];

        let (order, failures) = resilient_dependency_plan(&manifests).expect("隔离循环依赖");

        assert_eq!(order, vec![2]);
        assert_eq!(failures.len(), 2);
        assert!(failures
            .iter()
            .all(|failure| failure.reason.contains("循环")));
    }

    /// 关键能力 owner 应提前加载，同时保持其必选依赖位于 owner 之前。
    #[test]
    fn progressive_order_prioritizes_critical_owner_with_dependencies() {
        let manifest = |id: &str, dependency_id: Option<&str>| PluginManifest {
            plugin: PluginSection {
                id: id.into(),
                name: id.into(),
                version: "1.0.0".into(),
                api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
                wasm: format!("{id}.wasm"),
                description: None,
            },
            dependencies: dependency_id
                .map(|dependency_id| PluginDependency {
                    id: dependency_id.into(),
                    version: "*".into(),
                    optional: false,
                })
                .into_iter()
                .collect(),
            provides: Vec::new(),
            capabilities: Default::default(),
            metadata: HashMap::new(),
        };
        let manifests = vec![
            manifest("command", None),
            manifest("policy-support", None),
            manifest("sandbox", Some("policy-support")),
            manifest("mcp", None),
        ];

        let order =
            prioritize_progressive_order(&manifests, &[0, 1, 2, 3], &["sandbox".to_string()]);
        let ids = order
            .iter()
            .map(|index| manifests[*index].plugin.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["policy-support", "sandbox", "command", "mcp"]);
    }
}
