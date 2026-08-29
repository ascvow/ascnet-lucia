//! 插件执行的燃料预算与续注。

use crate::wasm::engine::{IntoAnyhow, WasmPluginLimits};
use crate::wasm::WasmPluginState;
use anyhow::{Context, Result};
/// 上下文加载每个序列化输入字节追加的 WASM fuel，覆盖 JSON 遍历与压缩成本。
pub(crate) const CONTEXT_FUEL_PER_INPUT_BYTE: u64 = 512;

/// 单次上下文加载允许使用的最高 fuel，避免超大请求解除计算资源上限。
pub(crate) const MAX_CONTEXT_FUEL: u64 = 500_000_000;

pub(crate) fn refill_fuel(state: &mut WasmPluginState) -> Result<()> {
    set_fuel(state, state.limits.fuel)
}

/// 按单次上下文输入大小分配 fuel，并保留显式的最大计算预算。
pub(crate) fn context_fuel_budget(limits: &WasmPluginLimits, input_bytes: usize) -> u64 {
    let extra = u64::try_from(input_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(CONTEXT_FUEL_PER_INPUT_BYTE);
    limits
        .fuel
        .saturating_add(extra)
        .min(MAX_CONTEXT_FUEL.max(limits.fuel))
}

/// 将指定 fuel 重置到单插件 store。
pub(crate) fn set_fuel(state: &mut WasmPluginState, fuel: u64) -> Result<()> {
    state
        .store
        .set_fuel(fuel)
        .into_anyhow()
        .context("failed to refill plugin fuel")
}
