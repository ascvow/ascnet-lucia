//! 进程共享 Engine、WASI Store 状态与单插件资源限额。

use crate::capability::CapabilityState;
use anyhow::{anyhow, Result};
use std::sync::LazyLock;
use wasmtime::component::ResourceTable;
use wasmtime::{Cache, CacheConfig, Config, Engine, StoreLimits};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

const DEFAULT_FUEL_PER_PLUGIN: u64 = 50_000_000;
const DEFAULT_FUEL_YIELD_INTERVAL: u64 = 250_000;
/// 单个插件线性内存的默认上限。
const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// 进程内所有 WASM 插件共享的 Wasmtime Engine。
///
/// Engine 的克隆只会增加内部引用计数；共享实例可复用编译器、类型注册表和代码缓存，
/// 同时每个插件仍持有独立的 Store、燃料和内存限制。
static SHARED_WASM_ENGINE: LazyLock<std::result::Result<Engine, String>> = LazyLock::new(|| {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    // 缓存不可用时保持无缓存启动，避免本地目录权限问题使插件系统整体失败。
    if let Ok(cache) = Cache::new(CacheConfig::new()) {
        config.cache(Some(cache));
    }
    Engine::new(&config).map_err(|error| format!("{error:?}"))
});

/// 将 wasmtime 结果转换为 anyhow 结果。
/// wasmtime 46 起使用自有 Error 类型，不再实现 std Error，无法直接配合 anyhow。
pub(super) trait IntoAnyhow<T> {
    fn into_anyhow(self) -> Result<T>;
}

impl<T> IntoAnyhow<T> for std::result::Result<T, wasmtime::Error> {
    fn into_anyhow(self) -> Result<T> {
        self.map_err(|err| anyhow::anyhow!("{err:?}"))
    }
}

/// 获取进程级共享 Engine 的浅克隆。
///
/// 初始化失败会被缓存，避免并发插件加载重复执行昂贵且注定失败的初始化。
pub(super) fn shared_wasm_engine() -> Result<Engine> {
    match &*SHARED_WASM_ENGINE {
        Ok(engine) => Ok(engine.clone()),
        Err(error) => Err(anyhow!("failed to create Wasmtime engine: {error}")),
    }
}

/// WASI Preview 2 所需的宿主状态。
pub(super) struct PluginWasiState {
    wasi: WasiCtx,
    table: ResourceTable,
    pub(super) capabilities: CapabilityState,
    /// Wasmtime 在实例化和内存增长时应用的资源上限。
    pub(super) store_limits: StoreLimits,
}

impl PluginWasiState {
    /// 创建单 component 独占的 WASI 状态和资源表。
    pub(super) fn new(
        wasi: WasiCtx,
        capabilities: CapabilityState,
        store_limits: StoreLimits,
    ) -> Self {
        Self {
            wasi,
            table: ResourceTable::new(),
            capabilities,
            store_limits,
        }
    }
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
