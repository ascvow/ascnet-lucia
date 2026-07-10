//! 上下文替换插件的真实 WASM 性能探针。

use anyhow::{anyhow, Result};
use agent_core::{
    AgentExtension, ContextLoadRequest, ContextLoader, MessageRole, ModelMessage,
    PassthroughContextLoader,
};
use agent_plugin_host::wasm::load_wasm_plugins;
use std::{path::Path, time::Instant};

/// 单组延迟样本的汇总。
struct LatencySummary {
    iterations: usize,
    total_ns: u128,
    p50_ns: u128,
    p95_ns: u128,
    max_ns: u128,
}

impl LatencySummary {
    /// 从纳秒样本创建稳定分位数汇总。
    fn from_samples(mut samples: Vec<u128>) -> Self {
        samples.sort_unstable();
        let iterations = samples.len();
        let percentile = |numerator: usize| {
            let index = ((iterations - 1) * numerator) / 100;
            samples[index]
        };
        Self {
            iterations,
            total_ns: samples.iter().sum(),
            p50_ns: percentile(50),
            p95_ns: percentile(95),
            max_ns: *samples.last().expect("性能样本不能为空"),
        }
    }

    /// 输出便于性能平台解析的单行 JSON。
    fn print_json(&self, name: &str) {
        println!(
            "{{\"benchmark\":\"{name}\",\"iterations\":{},\"total_ns\":{},\"p50_ns\":{},\"p95_ns\":{},\"max_ns\":{}}}",
            self.iterations, self.total_ns, self.p50_ns, self.p95_ns, self.max_ns
        );
    }
}

/// 构造每轮性能测试使用的 provider-neutral 上下文请求。
fn context_request() -> ContextLoadRequest {
    ContextLoadRequest {
        run_id: "performance-run".into(),
        step: 0,
        provider: "benchmark".into(),
        model: "benchmark-model".into(),
        system: Some("性能测试系统提示".into()),
        messages: vec![ModelMessage::text(
            MessageRole::User,
            "需要被上下文插件替换的消息",
        )],
    }
}

/// 运行真实 component 冷启动与上下文调用性能探针。
#[tokio::main]
async fn main() -> Result<()> {
    let iterations = std::env::var("LUCIA_PERF_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200)
        .max(10);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let load_started = Instant::now();
    let plugin_host = load_wasm_plugins(&[manifest]).await?;
    let component_load_ns = load_started.elapsed().as_nanos();
    println!(
        "{{\"benchmark\":\"wasm_component_load\",\"iterations\":1,\"total_ns\":{component_load_ns},\"p50_ns\":{component_load_ns},\"p95_ns\":{component_load_ns},\"max_ns\":{component_load_ns}}}"
    );

    let passthrough = PassthroughContextLoader;
    let mut baseline_samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let _ = passthrough.load(context_request()).await?;
        baseline_samples.push(started.elapsed().as_nanos());
    }
    let baseline = LatencySummary::from_samples(baseline_samples);
    baseline.print_json("core_context_passthrough");

    for _ in 0..20 {
        let _ = ContextLoader::load(&plugin_host, context_request()).await?;
        let _ = plugin_host.drain_events().await?;
    }
    let mut plugin_samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let _ = ContextLoader::load(&plugin_host, context_request()).await?;
        let _ = plugin_host.drain_events().await?;
        plugin_samples.push(started.elapsed().as_nanos());
    }
    let plugin = LatencySummary::from_samples(plugin_samples);
    plugin.print_json("wasm_context_replacement");

    if std::env::var("LUCIA_PERF_ENFORCE").as_deref() == Ok("1") {
        let context_budget_us = std::env::var("LUCIA_PLUGIN_CONTEXT_P95_US")
            .ok()
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(500);
        if plugin.p95_ns > context_budget_us * 1_000 {
            return Err(anyhow!(
                "WASM 上下文替换 p95={}ns，超过预算 {}us",
                plugin.p95_ns,
                context_budget_us
            ));
        }

        let load_budget_ms = std::env::var("LUCIA_PLUGIN_LOAD_MAX_MS")
            .ok()
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(250);
        if component_load_ns > load_budget_ms * 1_000_000 {
            return Err(anyhow!(
                "WASM component 冷启动={}ns，超过预算 {}ms",
                component_load_ns,
                load_budget_ms
            ));
        }
    }
    Ok(())
}
