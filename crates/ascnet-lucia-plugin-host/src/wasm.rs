//! WASM Component 插件宿主。
//!
//! WIT world 故意在 ABI 边界使用 JSON 字符串。这样第一版 ABI 更稳定，
//! Rust 内部类型可以继续通过 serde 演进。

use super::{
    capability::{encode_host_response, AgentRuntimeBinding, CapabilityState},
    contribution::ContributionRegistry,
    manifest::{
        resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest,
        CONTEXT_LOADER_CAPABILITY,
    },
    service::{PluginService, PluginServiceCall, ServiceHandler, ServiceRegistry},
    AgentEvent, AgentRuntimeHostServices, CompositePluginHost, PluginHost, PluginHostServices,
    ToolDecision, UiDeclaration, UiFrame, UiInput, UiRenderRequest,
};
use agent_core::{model::ModelMessage, AgentExtension, ContextLoadRequest, LoadedContext};
use agent_runtime::RuntimePrincipal;
use agent_tool::{ToolCall, ToolResult, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};
use tokio::sync::Mutex;
use uuid::Uuid;
use wasmtime::component::{
    Component, ComponentNamedList, Instance, Lift, Linker, Lower, ResourceTable, TypedFunc,
};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

const DEFAULT_FUEL_PER_PLUGIN: u64 = 50_000_000;
const DEFAULT_FUEL_YIELD_INTERVAL: u64 = 250_000;
/// 单个插件线性内存的默认上限。
const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// 将 wasmtime 结果转换为 anyhow 结果。
/// wasmtime 46 起使用自有 Error 类型，不再实现 std Error，无法直接配合 anyhow。
trait IntoAnyhow<T> {
    fn into_anyhow(self) -> Result<T>;
}

impl<T> IntoAnyhow<T> for std::result::Result<T, wasmtime::Error> {
    fn into_anyhow(self) -> Result<T> {
        self.map_err(|err| anyhow::anyhow!("{err:?}"))
    }
}

/// WASI Preview 2 所需的宿主状态。
struct PluginWasiState {
    wasi: WasiCtx,
    table: ResourceTable,
    capabilities: CapabilityState,
    /// Wasmtime 在实例化和内存增长时应用的资源上限。
    store_limits: StoreLimits,
}

impl WasiView for PluginWasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// WASM 插件运行时限制。
#[derive(Debug, Clone)]
pub struct WasmPluginLimits {
    /// 分配给插件 store 的 fuel。
    pub fuel: u64,

    /// 协作式 async yield 的 fuel 间隔。
    pub fuel_yield_interval: Option<u64>,

    /// 单个线性内存允许增长到的最大字节数。
    pub max_memory_bytes: usize,
}

impl Default for WasmPluginLimits {
    fn default() -> Self {
        Self {
            fuel: DEFAULT_FUEL_PER_PLUGIN,
            fuel_yield_interval: Some(DEFAULT_FUEL_YIELD_INTERVAL),
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
        }
    }
}

/// 已加载的 WASM 插件。
pub struct WasmPluginHost {
    manifest: PluginManifest,
    contributions: Arc<ContributionRegistry>,
    services: Arc<ServiceRegistry>,
    known_ui: Vec<UiDeclaration>,
    has_context_loader: bool,
    agent_runtime: Option<AgentRuntimeBinding>,
    state: Arc<Mutex<WasmPluginState>>,
}

struct WasmPluginState {
    store: Store<PluginWasiState>,
    #[allow(dead_code)]
    instance: Instance,
    call_tool: TypedFunc<(String,), (String,)>,
    before_tool: TypedFunc<(String,), (String,)>,
    after_tool: TypedFunc<(String,), ()>,
    on_event: TypedFunc<(String,), ()>,
    load_context: Option<TypedFunc<(String,), (String,)>>,
    handle_service: Option<TypedFunc<(String,), (String,)>>,
    deactivate: Option<TypedFunc<(), (String,)>>,
    render_ui: Option<TypedFunc<(String,), (String,)>>,
    on_ui_input: Option<TypedFunc<(String,), ()>>,
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
        let Some(handle_service) = state.handle_service else {
            return Err(anyhow!("插件 `{}` 未导出服务处理函数", self.plugin_id));
        };
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

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);

        let engine = Engine::new(&config)
            .into_anyhow()
            .context("failed to create Wasmtime engine")?;
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
        let cleanup_agent_runtime = agent_runtime.clone();
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
                PluginWasiState {
                    wasi: wasi.build(),
                    table: ResourceTable::new(),
                    capabilities: CapabilityState::new(
                        manifest.plugin.id.clone(),
                        plugin_dir,
                        manifest.capabilities.clone(),
                        contributions.clone(),
                        services.clone(),
                        agent_runtime.clone(),
                    ),
                    store_limits,
                },
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
                get_optional_func::<(String,), (String,)>(&instance, &mut store, "load-context")?;
            let activate =
                get_optional_func::<(String,), (String,)>(&instance, &mut store, "activate")?;
            let deactivate =
                get_optional_func::<(), (String,)>(&instance, &mut store, "deactivate")?;
            let handle_service =
                get_optional_func::<(String,), (String,)>(&instance, &mut store, "handle-service")?;
            let describe_ui =
                get_optional_func::<(), (String,)>(&instance, &mut store, "describe-ui")?;
            let (render_ui, on_ui_input) = if describe_ui.is_some() {
                (
                    Some(get_required_func::<(String,), (String,)>(
                        &instance,
                        &mut store,
                        "render-ui",
                    )?),
                    Some(get_required_func::<(String,), ()>(
                        &instance,
                        &mut store,
                        "on-ui-input",
                    )?),
                )
            } else {
                (None, None)
            };

            let (tools_json,) = list_tools
                .call_async(&mut store, ())
                .await
                .into_anyhow()
                .context("plugin `list-tools` failed")?;
            let legacy_tools: Vec<ToolSpec> = serde_json::from_str(&tools_json)
                .with_context(|| "plugin `list-tools` returned invalid ToolSpec JSON")?;
            contributions.upsert_legacy_tools(legacy_tools)?;

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
                if let Some(activate) = activate {
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
                }

                // UI 导出是可选能力，未实现 describe-ui 的旧插件继续作为纯工具插件加载。
                let Some(describe_ui) = describe_ui else {
                    return Ok(Vec::new());
                };
                refill_fuel(&mut state)?;
                let (declarations_json,) = describe_ui
                    .call_async(&mut state.store, ())
                    .await
                    .into_anyhow()
                    .context("plugin `describe-ui` failed")?;
                let mut declarations: Vec<UiDeclaration> = serde_json::from_str(&declarations_json)
                    .with_context(|| {
                        format!("plugin `{plugin_id}` returned invalid UiDeclaration JSON")
                    })?;
                validate_ui_declarations(&plugin_id, &mut declarations)?;
                Ok(declarations)
            }
            .await;
            let known_ui = match initialization {
                Ok(known_ui) => known_ui,
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
                has_context_loader: load_context.is_some(),
                agent_runtime,
                state,
            })
        }
        .await;
        if loading.is_err() {
            if let Some(binding) = cleanup_agent_runtime {
                binding.revoke().await;
            }
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

    /// 返回组件是否导出了上下文加载入口。
    pub fn supports_context_loading(&self) -> bool {
        self.has_context_loader
    }

    /// 调用 component 的可选卸载钩子。
    pub async fn deactivate(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        let deactivation = async {
            let Some(deactivate) = state.deactivate else {
                return Ok(());
            };
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
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let before_tool = state.before_tool;
        let (decision_json,) = before_tool
            .call_async(&mut state.store, (call_json,))
            .await
            .into_anyhow()
            .with_context(|| format!("plugin `{}` before-tool failed", self.id()))?;
        let decision = serde_json::from_str::<ToolDecision>(&decision_json).with_context(|| {
            format!("plugin `{}` returned invalid ToolDecision JSON", self.id())
        })?;
        Ok(decision)
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
        refill_fuel(&mut state)?;
        let Some(load_context) = state.load_context else {
            return Ok(None);
        };
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

    async fn render_ui(&self, request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        if request.plugin_id != self.id()
            || !self
                .known_ui
                .iter()
                .any(|declaration| declaration.view_id == request.view_id)
        {
            return Ok(None);
        }

        let request_json = serde_json::to_string(request)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let Some(render_ui) = state.render_ui else {
            return Ok(None);
        };
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
        if input.plugin_id != self.id()
            || !self
                .known_ui
                .iter()
                .any(|declaration| declaration.view_id == input.view_id)
        {
            return Ok(());
        }

        let input_json = serde_json::to_string(input)?;
        let mut state = self.state.lock().await;
        refill_fuel(&mut state)?;
        let Some(on_ui_input) = state.on_ui_input else {
            return Ok(());
        };
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

/// 将多个 WASM 插件 manifest 加载为一个组合宿主。
pub async fn load_wasm_plugins<P: AsRef<Path>>(paths: &[P]) -> Result<CompositePluginHost> {
    load_wasm_plugins_with_selection(paths, &HashMap::new()).await
}

/// 使用可扩展宿主服务加载多个 WASM 插件。
pub async fn load_wasm_plugins_with_services<P: AsRef<Path>>(
    paths: &[P],
    host_services: PluginHostServices,
) -> Result<CompositePluginHost> {
    load_wasm_plugins_with_selection_and_services(paths, &HashMap::new(), host_services).await
}

/// 使用应用显式选择解析独占能力并加载多个 WASM 插件。
pub async fn load_wasm_plugins_with_selection<P: AsRef<Path>>(
    paths: &[P],
    capability_selection: &HashMap<String, String>,
) -> Result<CompositePluginHost> {
    load_wasm_plugins_with_selection_and_services(
        paths,
        capability_selection,
        PluginHostServices::default(),
    )
    .await
}

/// 使用独占能力选择和可扩展宿主服务加载多个 WASM 插件。
pub async fn load_wasm_plugins_with_selection_and_services<P: AsRef<Path>>(
    paths: &[P],
    capability_selection: &HashMap<String, String>,
    host_services: PluginHostServices,
) -> Result<CompositePluginHost> {
    let mut pending = Vec::with_capacity(paths.len());
    for path in paths {
        let manifest_path = path.as_ref();
        let manifest = PluginManifest::load(manifest_path)?;
        let plugin_dir = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let wasm_path = plugin_dir.join(&manifest.plugin.wasm);
        pending.push((manifest, wasm_path, plugin_dir));
    }
    let manifests = pending
        .iter()
        .map(|(manifest, _, _)| manifest.clone())
        .collect::<Vec<_>>();
    let resolved_capabilities = resolve_plugin_capabilities(&manifests, capability_selection)?;
    let order = resolve_plugin_load_order(&manifests)?;
    let services = Arc::new(ServiceRegistry::default());
    let mut composite = CompositePluginHost::new();
    if let Some(owner) = resolved_capabilities.exclusive_owner(CONTEXT_LOADER_CAPABILITY) {
        composite.set_capability_owner(CONTEXT_LOADER_CAPABILITY, owner);
    }
    for index in order {
        let (manifest, wasm_path, plugin_dir) = pending[index].clone();
        let loading = WasmPluginHost::load_with_limits_in_dir(
            manifest,
            wasm_path,
            plugin_dir,
            WasmPluginLimits::default(),
            services.clone(),
            host_services.clone(),
        )
        .await;
        let host = match loading {
            Ok(host) => host,
            Err(error) => {
                let _ = composite.shutdown().await;
                return Err(error);
            }
        };
        if composite.capability_owner(CONTEXT_LOADER_CAPABILITY) == Some(host.id())
            && !host.supports_context_loading()
        {
            let _ = host.shutdown().await;
            let _ = composite.shutdown().await;
            return Err(anyhow!(
                "插件 `{}` 声明了 `{CONTEXT_LOADER_CAPABILITY}`，但未导出 load-context",
                host.id()
            ));
        }
        composite.push(Arc::new(host));
    }
    Ok(composite)
}

fn refill_fuel(state: &mut WasmPluginState) -> Result<()> {
    let fuel = state.limits.fuel;
    state
        .store
        .set_fuel(fuel)
        .into_anyhow()
        .context("failed to refill plugin fuel")
}

/// 校验插件内部视图 ID，并注入可信的 manifest 插件 ID。
fn validate_ui_declarations(plugin_id: &str, declarations: &mut [UiDeclaration]) -> Result<()> {
    let mut view_ids = HashSet::new();
    for declaration in declarations {
        if declaration.view_id.trim().is_empty() {
            return Err(anyhow!("plugin `{plugin_id}` has an empty UI view id"));
        }
        if !view_ids.insert(declaration.view_id.clone()) {
            return Err(anyhow!(
                "plugin `{plugin_id}` declares duplicate UI view `{}`",
                declaration.view_id
            ));
        }
        declaration.plugin_id = plugin_id.to_string();
    }
    Ok(())
}

/// 探测可选 component 导出；导出存在但类型不匹配时仍视为加载错误。
fn get_optional_func<Params, Results>(
    instance: &Instance,
    store: &mut Store<PluginWasiState>,
    name: &str,
) -> Result<Option<TypedFunc<Params, Results>>>
where
    Params: ComponentNamedList + Lower + Send + Sync + 'static,
    Results: ComponentNamedList + Lift + Send + Sync + 'static,
{
    if instance.get_func(&mut *store, name).is_none() {
        return Ok(None);
    }
    get_required_func(instance, store, name).map(Some)
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

    /// 默认运行时限制必须同时约束计算量和线性内存。
    #[test]
    fn default_limits_bound_fuel_and_memory() {
        let limits = WasmPluginLimits::default();

        assert!(limits.fuel > 0);
        assert_eq!(limits.max_memory_bytes, 64 * 1024 * 1024);
    }
}
