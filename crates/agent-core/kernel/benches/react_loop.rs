//! ReAct 循环每轮固定开销的性能基准。
//!
//! 上下文准备类基准（`agent-context` 的 `context_prepare`）只覆盖加载与消息转换，
//! 覆盖不到运行循环自身：请求构造、`state.session` 回写、事件分发、工具准入。
//! 本基准用一个固定响应的模型把 provider 延迟排除在外，从而把循环开销单独暴露出来。
//!
//! 与仓库其他基准保持一致：不依赖 Criterion，输出逐行 JSON，结果只用于观测，
//! 不在进程内判定回归。

use agent_core::{
    Agent, AgentOptions, ChatModel, ModelGateway, ModelRequest, ModelResponse, ProviderAdapter,
    Session,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::{
    env,
    future::Future,
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

const DEFAULT_WARMUP_ITERATIONS: usize = 200;
const DEFAULT_ITERATIONS: usize = 1_000;
/// 与 `agent-context` 基准保持同一组历史规模，便于横向比较。
const MESSAGE_COUNTS: [usize; 4] = [0, 64, 256, 1_024];
/// 单条消息正文长度，与 `agent-context` 基准一致。
const MESSAGE_BODY_CHARS: usize = 400;

/// 每次都立即返回固定文本的模型。
///
/// 不产生工具调用，因此一次 `run_session` 只走一轮：测到的差异来自循环的固定开销，
/// 而不是 provider 往返或工具执行。
struct FixedModel;

#[async_trait]
impl ChatModel for FixedModel {
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text("基准响应"))
    }
}

#[async_trait]
impl ProviderAdapter for FixedModel {
    fn name(&self) -> &'static str {
        "bench-fixed"
    }
}

/// 单次基准运行使用的迭代配置。
#[derive(Debug, Clone, Copy)]
struct BenchmarkConfig {
    warmup_iterations: usize,
    iterations: usize,
}

impl BenchmarkConfig {
    /// 从环境变量读取配置；未设置时采用适合本地和 CI 的保守默认值。
    fn from_env() -> Result<Self> {
        Ok(Self {
            warmup_iterations: read_positive_usize(
                "LUCIA_BENCH_WARMUP",
                DEFAULT_WARMUP_ITERATIONS,
            )?,
            iterations: read_positive_usize("LUCIA_BENCH_ITERATIONS", DEFAULT_ITERATIONS)?,
        })
    }
}

/// 一组迭代的总耗时。
#[derive(Debug, Clone, Copy)]
struct Measurement {
    total: Duration,
}

impl Measurement {
    /// 计算平均每次操作的纳秒数。
    fn ns_per_op(self, iterations: usize) -> f64 {
        self.total.as_secs_f64() * 1_000_000_000.0 / iterations as f64
    }
}

/// 读取正整数的环境变量配置。
fn read_positive_usize(name: &str, default: usize) -> Result<usize> {
    match env::var(name) {
        Ok(raw) => {
            let value = raw
                .trim()
                .parse::<usize>()
                .with_context(|| format!("{name} 必须是正整数，实际为 {raw:?}"))?;
            if value == 0 {
                anyhow::bail!("{name} 必须大于零");
            }
            Ok(value)
        }
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(err).with_context(|| format!("读取 {name} 失败")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = BenchmarkConfig::from_env()?;
    print_config(config);
    bench_run_session(config).await?;
    Ok(())
}

/// 按历史规模分组度量一次完整运行的固定开销。
///
/// 每轮迭代都克隆一次预置会话用于 `run_session`，该克隆会计入测量值；它对所有
/// 版本都相同，因此不影响同一规模下的前后对比，但会稀释循环内部差异。
async fn bench_run_session(config: BenchmarkConfig) -> Result<()> {
    let gateway = ModelGateway::new();
    gateway
        .register("bench", Arc::new(FixedModel))
        .context("注册基准 provider")?;
    let agent = Arc::new(Agent::new(
        gateway,
        AgentOptions::default().with_model_route("bench", "bench-model"),
    ));

    for message_count in MESSAGE_COUNTS {
        let template = Arc::new(session_with_history(message_count));
        let measurement = measure_async(config, || {
            let agent = Arc::clone(&agent);
            let session = (*template).clone();
            async move {
                let run = agent.run_session(session).await?;
                Ok(black_box(run))
            }
        })
        .await?;
        report(config, "react_run_single_turn", message_count, measurement);
    }
    Ok(())
}

/// 构造指定条数历史的会话；正文长度固定，保证不同规模的差异只来自条数。
fn session_with_history(message_count: usize) -> Session {
    let body = "x".repeat(MESSAGE_BODY_CHARS);
    let mut session = Session::new();
    for index in 0..message_count {
        if index % 2 == 0 {
            session.push_user(format!("历史问题 {index} {body}"));
        } else {
            session.push_assistant_text(format!("历史回答 {index} {body}"));
        }
    }
    session
}

/// 执行预热与正式异步迭代，计时范围包含返回值释放以反映实际调用成本。
async fn measure_async<F, Fut, T>(config: BenchmarkConfig, mut operation: F) -> Result<Measurement>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for _ in 0..config.warmup_iterations {
        black_box(operation().await?);
    }

    let started_at = Instant::now();
    for _ in 0..config.iterations {
        black_box(operation().await?);
    }
    Ok(Measurement {
        total: started_at.elapsed(),
    })
}

/// 输出基准元信息；声明结果只用于观测，不由本程序自动判定回归。
fn print_config(config: BenchmarkConfig) {
    println!(
        "{}",
        json!({
            "type": "benchmark_config",
            "schema_version": 1,
            "warmup_iterations": config.warmup_iterations,
            "iterations": config.iterations,
            "message_counts": MESSAGE_COUNTS,
            "message_body_chars": MESSAGE_BODY_CHARS,
            "regression_policy": "informational_only",
            "automatic_regression_failure": false,
            "note": "模型为固定响应，测的是循环固定开销而非 provider 延迟"
        })
    );
}

/// 输出单项性能结果，并给出归一化到消息数的数值便于比较趋势。
fn report(config: BenchmarkConfig, name: &str, message_count: usize, measurement: Measurement) {
    let ns_per_op = measurement.ns_per_op(config.iterations);
    let ns_per_message = if message_count == 0 {
        0.0
    } else {
        ns_per_op / message_count as f64
    };
    println!(
        "{}",
        json!({
            "type": "benchmark_result",
            "name": name,
            "group": "react",
            "message_count": message_count,
            "warmup_iterations": config.warmup_iterations,
            "iterations": config.iterations,
            "total_ns": measurement.total.as_nanos(),
            "ns_per_op": ns_per_op,
            "ns_per_message": ns_per_message,
            "automatic_regression_failure": false
        })
    );
}
