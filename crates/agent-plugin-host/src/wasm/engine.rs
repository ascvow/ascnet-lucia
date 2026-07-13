//! 进程共享 Engine、WASI Store 状态与单插件资源限额。

use crate::capability::CapabilityState;
use anyhow::{anyhow, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, OnceLock},
};
use wasmtime::component::ResourceTable;
use wasmtime::{Cache, CacheConfig, Config, Engine, StoreLimits};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

const DEFAULT_FUEL_PER_PLUGIN: u64 = 50_000_000;
const DEFAULT_FUEL_YIELD_INTERVAL: u64 = 250_000;
/// 单个插件线性内存的默认上限。
const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// 应用在首次创建 Engine 前注入的持久化编译缓存目录。
static WASM_CACHE_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

/// 配置进程级 Wasmtime 编译缓存目录。
///
/// 必须在首次加载 component 前调用；重复设置相同目录是幂等操作，不同目录会返回错误。
pub fn configure_wasm_cache_directory(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref().to_path_buf();
    if let Some(configured) = WASM_CACHE_DIRECTORY.get() {
        if configured == &path {
            return Ok(());
        }
        return Err(anyhow!(
            "Wasmtime 缓存目录已经配置为 {}",
            configured.display()
        ));
    }
    WASM_CACHE_DIRECTORY
        .set(path)
        .map_err(|_| anyhow!("Wasmtime 缓存目录配置失败"))
}

/// 进程内所有 WASM 插件共享的 Wasmtime Engine。
///
/// Engine 的克隆只会增加内部引用计数；共享实例可复用编译器、类型注册表和代码缓存，
/// 同时每个插件仍持有独立的 Store、燃料和内存限制。
static SHARED_WASM_ENGINE: LazyLock<std::result::Result<Engine, String>> = LazyLock::new(|| {
    create_wasm_engine(WASM_CACHE_DIRECTORY.get().map(PathBuf::as_path))
        .map_err(|error| format!("{error:#}"))
});

/// 按可选持久化缓存目录创建共享配置一致的 Wasmtime Engine。
fn create_wasm_engine(cache_directory: Option<&Path>) -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    // 缓存不可用时保持无缓存启动，避免本地目录权限问题使插件系统整体失败。
    if let Some(directory) = cache_directory {
        let cache = fs::create_dir_all(directory).ok().and_then(|_| {
            let mut cache_config = CacheConfig::new();
            cache_config.with_directory(directory);
            Cache::new(cache_config).ok()
        });
        if let Some(cache) = cache {
            config.cache(Some(cache));
        }
    }
    Engine::new(&config).map_err(|error| anyhow!("{error:?}"))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 显式缓存目录必须被 Wasmtime 验证并创建。
    #[test]
    fn explicit_cache_directory_is_created() {
        let directory =
            std::env::temp_dir().join(format!("lucia-wasmtime-cache-{}", uuid::Uuid::new_v4()));

        let engine = create_wasm_engine(Some(&directory)).expect("创建带缓存的 Engine");
        assert!(directory.is_dir());

        drop(engine);
        let _ = fs::remove_dir_all(directory);
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
