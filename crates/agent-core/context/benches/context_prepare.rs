//! 上下文准备与消息转换的轻量性能基准。
//!
//! 与 `agent-plugin-host` 的基准保持一致：不依赖 Criterion，输出逐行 JSON，方便 CI
//! 保存结果后交给外部系统比较。性能结果只用于观测，不在进程内设置失败阈值，避免
//! 共享 CI 机器的调度抖动造成误报。
//!
//! 覆盖三处随会话历史线性增长的热点：
//!
//! - `context_load_passthrough`：`estimate_context_tokens` 的逐消息字节估算；
//! - `context_load_incremental`：缓存命中轮的摘要续算与历史拼接；
//! - `transform_messages`：每次模型请求前的整段消息清洗。

use agent_context::NativeContextLoader;
use agent_core::{
    model::transform::transform_messages_for_route, ChatModel, ContextLoadRequest, ContextLoader,
    MessageRole, ModelGateway, ModelMessage, ModelRequest, ModelResponse, NoopEventSink,
    ProviderAdapter,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::{
    env,
    future::Future,
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

const DEFAULT_WARMUP_ITERATIONS: usize = 500;
const DEFAULT_ITERATIONS: usize = 2_000;
/// 逐项基准使用的历史规模；递增以观察随消息数的增长曲线。
const MESSAGE_COUNTS: [usize; 3] = [64, 256, 1_024];
/// 每条消息的正文长度，接近含工具摘要的普通历史消息。
const MESSAGE_BODY_CHARS: usize = 400;
/// 增量场景使用的窗口：取值足够小，使压缩水位落在几千 token，便于稳定触发缓存。
const INCREMENTAL_CONTEXT_WINDOW: u64 = 8_000;
/// 增量场景建立缓存时使用的初始历史长度。
const INCREMENTAL_PRIME_MESSAGES: usize = 512;
/// 增量场景每轮追加的消息数，模拟模型回复后累积尚未压缩的历史。
const INCREMENTAL_APPEND_PER_ROUND: usize = 1;
/// 增量场景的计时轮数上限；该场景每轮都会追加历史，不限制会让预测期无限增长。
const INCREMENTAL_MAX_ITERATIONS: usize = 200;

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

/// 一组迭代的总耗时，保留总量以减少过早舍入带来的误差。
#[derive(Debug, Clone, Copy)]
struct Measurement {
    total: Duration,
    iterations: usize,
}

impl Measurement {
    /// 计算平均每次操作的纳秒数。
    fn ns_per_op(self) -> f64 {
        self.total.as_secs_f64() * 1_000_000_000.0 / self.iterations as f64
    }
}

/// 返回固定摘要的离线 provider，避免基准触碰真实网络。
struct FixedSummaryModel;

#[async_trait]
impl ChatModel for FixedSummaryModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text("<summary>基准摘要</summary>"))
    }
}

impl ProviderAdapter for FixedSummaryModel {
    fn name(&self) -> &'static str {
        "bench-fixed-summary"
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = BenchmarkConfig::from_env()?;
    print_config(config);

    bench_passthrough(config).await?;
    bench_incremental(config).await?;
    bench_transform(config)?;
    bench_decompose(config)?;

    Ok(())
}

/// 场景一：低于压缩水位的透传加载，度量逐消息 token 估算与消息转换。
async fn bench_passthrough(config: BenchmarkConfig) -> Result<()> {
    for message_count in MESSAGE_COUNTS {
        let history = history(message_count);
        let measurement = measure_async(config, || {
            let loader = build_loader();
            let messages = history.clone();
            async move { Ok(loader.load(request(messages, 1)).await?.messages.len()) }
        })
        .await?;
        report(
            config,
            "context_load_passthrough",
            "context",
            message_count,
            measurement,
        );
    }
    Ok(())
}

/// 场景二：先触发一次完整压缩建立缓存，再逐轮追加消息，度量缓存命中轮的开销。
///
/// 每轮追加历史使前缀不断变长，因此该场景度量的是"命中缓存但仍需续算摘要"的路径；
/// 平均开销会随轮数缓慢上升，输出同时给出归一化到消息数的数值便于比较趋势。
async fn bench_incremental(config: BenchmarkConfig) -> Result<()> {
    let iterations = config.iterations.min(INCREMENTAL_MAX_ITERATIONS);
    let warmup_iterations = config.warmup_iterations.min(INCREMENTAL_MAX_ITERATIONS);

    // 建立缓存本身不计入测量：它只在真实会话中最先发生一次。
    let mut primed = IncrementalState::new();
    primed.prime().await?;

    for _ in 0..warmup_iterations {
        black_box(primed.round().await?);
    }

    let started_at = Instant::now();
    for _ in 0..iterations {
        black_box(primed.round().await?);
    }
    report(
        config,
        "context_load_incremental",
        "context",
        primed.messages(),
        Measurement {
            total: started_at.elapsed(),
            iterations,
        },
    );
    Ok(())
}

/// 场景三：每次模型请求前的整段消息清洗。
fn bench_transform(config: BenchmarkConfig) -> Result<()> {
    for message_count in MESSAGE_COUNTS {
        let history = history(message_count);
        let measurement = measure_sync(config, || {
            Ok(transform_messages_for_route(
                &history,
                "bench-provider",
                "bench-model",
            ))
        })?;
        report(
            config,
            "transform_messages",
            "transform",
            message_count,
            measurement,
        );
    }
    Ok(())
}

/// 增量场景的可复用状态：一个已建立缓存的加载器与不断增长的原始历史。
struct IncrementalState {
    loader: NativeContextLoader,
    messages: Vec<ModelMessage>,
    step: usize,
}

impl IncrementalState {
    /// 构造空的增量状态；缓存由 [`Self::prime`] 建立。
    fn new() -> Self {
        Self {
            loader: build_loader(),
            messages: history(INCREMENTAL_PRIME_MESSAGES),
            step: 0,
        }
    }

    /// 执行一次超过压缩水位的加载，使加载器写入压缩缓存。
    async fn prime(&mut self) -> Result<()> {
        let messages = std::mem::take(&mut self.messages);
        let loaded = self.loader.load(request(messages.clone(), 0)).await?;
        black_box(loaded.messages.len());
        // 压缩后的上下文由加载器缓存，这里保留原始历史作为下一轮的前缀来源。
        self.messages = messages;
        Ok(())
    }

    /// 追加若干消息后执行一次缓存命中的加载。
    async fn round(&mut self) -> Result<usize> {
        self.messages.extend(history(INCREMENTAL_APPEND_PER_ROUND));
        self.step += 1;
        let messages = self.messages.clone();
        Ok(self
            .loader
            .load(request(messages, self.step))
            .await?
            .messages
            .len())
    }

    /// 当前原始历史的消息数，用于归一化输出。
    fn messages(&self) -> usize {
        self.messages.len()
    }
}

/// 构造启用缓存、注册了离线 provider 的上下文加载器。
fn build_loader() -> NativeContextLoader {
    let gateway = ModelGateway::new();
    gateway
        .register(
            "bench-provider",
            Arc::new(FixedSummaryModel) as Arc<dyn ProviderAdapter>,
        )
        .expect("注册基准 provider");
    NativeContextLoader::new(
        gateway,
        "bench-provider",
        "bench-model",
        false,
        8_192,
        Arc::new(NoopEventSink),
    )
    .expect("构造上下文加载器")
    .with_context_window(Some(INCREMENTAL_CONTEXT_WINDOW))
}

/// 构造一次自动（非用户发起）上下文加载请求。
fn request(messages: Vec<ModelMessage>, step: usize) -> ContextLoadRequest {
    ContextLoadRequest {
        run_id: "bench-run".to_string(),
        step,
        provider: "bench-provider".to_string(),
        model: "bench-model".to_string(),
        system: Some("基准系统提示".to_string()),
        messages,
        user_initiated: false,
    }
}

/// 构造指定条数、正文长度稳定的历史消息。
fn history(message_count: usize) -> Vec<ModelMessage> {
    let body = "基准历史内容 ".repeat(MESSAGE_BODY_CHARS / 8);
    (0..message_count)
        .map(|index| {
            let role = if index % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            ModelMessage::text(role, body.as_str())
        })
        .collect()
}

/// 临时：分解透传加载各阶段的成本。
fn bench_decompose(config: BenchmarkConfig) -> Result<()> {
    for message_count in MESSAGE_COUNTS {
        let history = history(message_count);

        let measurement = measure_sync(config, || {
            let loader = build_loader();
            let messages = history.clone();
            std::hint::black_box(&loader);
            Ok(messages.len())
        })?;
        report(
            config,
            "stage_setup",
            "decompose",
            message_count,
            measurement,
        );

        let measurement = measure_sync(config, || {
            let values: Vec<serde_json::Value> = history
                .iter()
                .map(|m| serde_json::to_value(m).expect("encode"))
                .collect();
            Ok(values.len())
        })?;
        report(
            config,
            "stage_encode",
            "decompose",
            message_count,
            measurement,
        );

        let values: Vec<serde_json::Value> = history
            .iter()
            .map(|m| serde_json::to_value(m).expect("encode"))
            .collect();

        let measurement = measure_sync(config, || {
            let mut total = 0usize;
            for value in &values {
                let mut writer = agent_context_bench_counter::Counter::default();
                serde_json::to_writer(&mut writer, value).expect("serialize");
                total += writer.tokens();
            }
            Ok(total)
        })?;
        report(
            config,
            "stage_estimate",
            "decompose",
            message_count,
            measurement,
        );

        let measurement = measure_sync(config, || {
            let decoded: Vec<agent_core::ModelMessage> = values
                .iter()
                .map(|v| serde_json::from_value(v.clone()).expect("decode"))
                .collect();
            Ok(decoded.len())
        })?;
        report(
            config,
            "stage_decode",
            "decompose",
            message_count,
            measurement,
        );
    }
    Ok(())
}

/// 与原生加载器相同的字节折算写入器，仅用于分解成本。
mod agent_context_bench_counter {
    use std::io::{self, Write};

    #[derive(Default)]
    pub struct Counter {
        ascii: usize,
        non_ascii: usize,
    }

    impl Counter {
        pub fn tokens(&self) -> usize {
            self.ascii.div_ceil(4) + self.non_ascii.div_ceil(3)
        }
    }

    impl Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let non_ascii = buffer.iter().filter(|b| !b.is_ascii()).count();
            self.non_ascii += non_ascii;
            self.ascii += buffer.len() - non_ascii;
            Ok(buffer.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

/// 执行预热和正式异步迭代；闭包每次返回一个待执行的 future。
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
        iterations: config.iterations,
    })
}

/// 执行预热和正式同步迭代。
fn measure_sync<F, T>(config: BenchmarkConfig, mut operation: F) -> Result<Measurement>
where
    F: FnMut() -> Result<T>,
{
    for _ in 0..config.warmup_iterations {
        black_box(operation()?);
    }

    let started_at = Instant::now();
    for _ in 0..config.iterations {
        black_box(operation()?);
    }
    Ok(Measurement {
        total: started_at.elapsed(),
        iterations: config.iterations,
    })
}

/// 输出基准元信息；该行声明结果只用于观测，不由本程序自动判定回归。
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
            "incremental_context_window": INCREMENTAL_CONTEXT_WINDOW,
            "incremental_prime_messages": INCREMENTAL_PRIME_MESSAGES,
            "incremental_append_per_round": INCREMENTAL_APPEND_PER_ROUND,
            "incremental_max_iterations": INCREMENTAL_MAX_ITERATIONS,
            "regression_policy": "informational_only",
            "automatic_regression_failure": false,
            "note": "共享 CI 的性能抖动较大，应保存历史分位数并由外部系统判断趋势"
        })
    );
}

/// 输出单项性能结果，并按消息数给出归一化开销以暴露增长曲线。
fn report(
    config: BenchmarkConfig,
    name: &str,
    group: &str,
    message_count: usize,
    measurement: Measurement,
) {
    let ns_per_op = measurement.ns_per_op();
    println!(
        "{}",
        json!({
            "type": "benchmark_result",
            "name": name,
            "group": group,
            "message_count": message_count,
            "warmup_iterations": config.warmup_iterations,
            "iterations": measurement.iterations,
            "total_ns": measurement.total.as_nanos(),
            "ns_per_op": ns_per_op,
            "ns_per_message": ns_per_op / message_count as f64,
            "automatic_regression_failure": false
        })
    );
}

/// 读取必须大于零的迭代次数，避免零除和没有样本的无效结果。
fn read_positive_usize(name: &str, default: usize) -> Result<usize> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value
        .to_str()
        .with_context(|| format!("环境变量 {name} 不是有效 UTF-8"))?;
    let parsed = value
        .parse::<usize>()
        .with_context(|| format!("环境变量 {name} 必须是正整数"))?;
    if parsed == 0 {
        bail!("环境变量 {name} 必须大于零");
    }
    Ok(parsed)
}
