//! Plugin Host 公开 API 的轻量性能基准。
//!
//! 本基准不依赖 Criterion，输出为逐行 JSON，方便 CI 保存结果后交给外部系统比较。
//! 性能结果只用于观测，不在进程内设置失败阈值，避免共享 CI 机器的调度抖动造成误报。

use agent_core::{
    AgentExtension, ContextLoadRequest, ContextLoader, LoadedContext, MessageRole, ModelMessage,
    NoopAgentExtension, PassthroughContextLoader,
};
use agent_plugin_host::{
    manifest::CONTEXT_LOADER_CAPABILITY,
    ui::{UiDeclaration, UiFrame, UiInput, UiInputEvent, UiPlacement, UiRenderRequest, UiSize},
    CompositePluginHost, PluginHost,
};
use agent_tool::{ToolCall, ToolResult, ToolSpec};
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

const DEFAULT_WARMUP_ITERATIONS: usize = 2_000;
const DEFAULT_ITERATIONS: usize = 20_000;
const PLUGIN_COUNT: usize = 8;

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
}

impl Measurement {
    /// 计算平均每次操作的纳秒数。
    fn ns_per_op(self, iterations: usize) -> f64 {
        self.total.as_secs_f64() * 1_000_000_000.0 / iterations as f64
    }
}

/// 模拟具有提示、工具和可选上下文替换能力的最小插件宿主。
struct BenchmarkPlugin {
    id: String,
    prompt: ModelMessage,
    tool: ToolSpec,
    view_id: String,
    summary: Option<ModelMessage>,
}

impl BenchmarkPlugin {
    /// 创建一个提供唯一提示和唯一工具的测试插件。
    fn new(index: usize) -> Self {
        Self {
            id: format!("benchmark-plugin-{index}"),
            prompt: ModelMessage::text(MessageRole::Developer, format!("基准插件 {index} 的提示")),
            tool: ToolSpec::new(
                format!("benchmark_tool_{index}"),
                "用于测量插件工具路由开销",
                ToolSpec::empty_object_schema(),
            ),
            view_id: format!("benchmark-view-{index}"),
            summary: None,
        }
    }

    /// 创建一个只返回固定摘要的上下文插件。
    fn context_owner() -> Self {
        Self {
            id: "benchmark-context-owner".to_string(),
            prompt: ModelMessage::text(MessageRole::Developer, "上下文插件提示"),
            tool: ToolSpec::new(
                "benchmark_context_tool",
                "上下文插件占位工具",
                ToolSpec::empty_object_schema(),
            ),
            view_id: "benchmark-context-view".to_string(),
            summary: Some(ModelMessage::text(
                MessageRole::Developer,
                "插件生成的压缩上下文摘要",
            )),
        }
    }
}

#[async_trait]
impl AgentExtension for BenchmarkPlugin {
    /// 返回单条稳定提示，覆盖组合宿主的提示聚合路径。
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        Ok(vec![self.prompt.clone()])
    }

    /// 返回单个唯一工具，覆盖列表聚合和路由表重建路径。
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![self.tool.clone()])
    }

    /// 模拟工具 owner 执行成功，返回固定的小型 JSON 结果。
    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        if call.name != self.tool.name {
            return Ok(None);
        }
        Ok(Some(ToolResult::success(
            call.id,
            call.name,
            json!({"plugin_id": self.id}),
        )))
    }
}

#[async_trait]
impl PluginHost for BenchmarkPlugin {
    /// 返回工具路由和能力路由使用的稳定插件 ID。
    fn id(&self) -> Option<&str> {
        Some(&self.id)
    }

    /// 上下文 owner 返回完整替换上下文，普通测试插件不处理该请求。
    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        Ok(self
            .summary
            .clone()
            .map(|summary| LoadedContext::new(request.system.clone(), vec![summary])))
    }

    /// 返回单个稳定视图，覆盖组合宿主的 UI 路由表构建路径。
    async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
        Ok(vec![UiDeclaration {
            plugin_id: String::new(),
            view_id: self.view_id.clone(),
            title: "插件性能视图".to_string(),
            placement: UiPlacement::Right,
            size: UiSize::default(),
            focusable: true,
        }])
    }

    /// 返回最小空帧，测量 owner 自身调用与组合路由之间的差值。
    async fn render_ui(&self, request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        Ok((request.view_id == self.view_id).then(|| UiFrame {
            view_id: self.view_id.clone(),
            visible: true,
            lines: Vec::new(),
        }))
    }

    /// 接收属于当前视图的输入，基准中不执行额外业务逻辑。
    async fn on_ui_input(&self, _input: &UiInput) -> Result<()> {
        Ok(())
    }
}

/// 运行全部性能场景并逐行打印机器可读 JSON。
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = BenchmarkConfig::from_env()?;
    print_config(config);

    let noop_extension = NoopAgentExtension;
    let empty_composite = CompositePluginHost::new();
    let plugins = (0..PLUGIN_COUNT)
        .map(|index| Arc::new(BenchmarkPlugin::new(index)))
        .collect::<Vec<_>>();
    let mut populated_composite = CompositePluginHost::new();
    for plugin in &plugins {
        populated_composite.push(plugin.clone());
    }

    let noop_prompt =
        measure_async(config, || AgentExtension::prompt_messages(&noop_extension)).await?;
    report(
        config,
        "noop_agent_extension_prompt",
        "prompt_messages",
        "noop_agent_extension_prompt",
        noop_prompt,
        noop_prompt,
        0,
    );

    let empty_prompt =
        measure_async(config, || AgentExtension::prompt_messages(&empty_composite)).await?;
    report(
        config,
        "empty_composite_prompt",
        "prompt_messages",
        "noop_agent_extension_prompt",
        empty_prompt,
        noop_prompt,
        0,
    );

    let populated_prompt = measure_async(config, || {
        AgentExtension::prompt_messages(&populated_composite)
    })
    .await?;
    report(
        config,
        "eight_plugin_composite_prompt",
        "prompt_messages",
        "empty_composite_prompt",
        populated_prompt,
        empty_prompt,
        PLUGIN_COUNT,
    );

    let empty_tool_list =
        measure_async(config, || AgentExtension::list_tools(&empty_composite)).await?;
    report(
        config,
        "empty_composite_list_tools",
        "list_tools",
        "empty_composite_list_tools",
        empty_tool_list,
        empty_tool_list,
        0,
    );

    let populated_tool_list =
        measure_async(config, || AgentExtension::list_tools(&populated_composite)).await?;
    report(
        config,
        "eight_plugin_composite_list_tools",
        "list_tools",
        "empty_composite_list_tools",
        populated_tool_list,
        empty_tool_list,
        PLUGIN_COUNT,
    );

    // 在工具派发计时前显式建立快照，避免把路由表初始化混入单次调用开销。
    black_box(AgentExtension::list_tools(&populated_composite).await?);
    let routed_plugin = plugins.last().context("工具路由基准至少需要一个插件")?;
    let tool_call = ToolCall::new("benchmark-call", routed_plugin.tool.name.clone(), json!({}));
    let direct_tool_call = measure_async(config, || {
        AgentExtension::call_tool(routed_plugin.as_ref(), tool_call.clone())
    })
    .await?;
    report(
        config,
        "direct_plugin_call_tool",
        "call_tool",
        "direct_plugin_call_tool",
        direct_tool_call,
        direct_tool_call,
        1,
    );

    let routed_tool_call = measure_async(config, || {
        AgentExtension::call_tool(&populated_composite, tool_call.clone())
    })
    .await?;
    report(
        config,
        "eight_plugin_composite_call_tool",
        "call_tool",
        "direct_plugin_call_tool",
        routed_tool_call,
        direct_tool_call,
        PLUGIN_COUNT,
    );

    let missing_owner = measure_sync(config, || empty_composite.tool_owner("missing_tool"))?;
    report(
        config,
        "empty_composite_tool_owner_miss",
        "tool_owner",
        "empty_composite_tool_owner_miss",
        missing_owner,
        missing_owner,
        0,
    );

    let routed_tool_name = routed_plugin.tool.name.as_str();
    let owner_hit = measure_sync(config, || populated_composite.tool_owner(routed_tool_name))?;
    report(
        config,
        "eight_plugin_composite_tool_owner_hit",
        "tool_owner",
        "empty_composite_tool_owner_miss",
        owner_hit,
        missing_owner,
        PLUGIN_COUNT,
    );

    // 在 UI 派发计时前显式建立快照，隔离声明聚合成本和热路径路由成本。
    black_box(PluginHost::ui_declarations(&populated_composite).await?);
    let ui_request = UiRenderRequest {
        plugin_id: routed_plugin.id.clone(),
        view_id: routed_plugin.view_id.clone(),
        instance_id: None,
        width: 40,
        height: 12,
        focused: true,
        frame: 1,
    };
    let direct_ui_render = measure_async(config, || {
        PluginHost::render_ui(routed_plugin.as_ref(), &ui_request)
    })
    .await?;
    report(
        config,
        "direct_plugin_render_ui",
        "render_ui",
        "direct_plugin_render_ui",
        direct_ui_render,
        direct_ui_render,
        1,
    );

    let routed_ui_render = measure_async(config, || {
        PluginHost::render_ui(&populated_composite, &ui_request)
    })
    .await?;
    report(
        config,
        "eight_plugin_composite_render_ui",
        "render_ui",
        "direct_plugin_render_ui",
        routed_ui_render,
        direct_ui_render,
        PLUGIN_COUNT,
    );

    let ui_input = UiInput {
        plugin_id: routed_plugin.id.clone(),
        view_id: routed_plugin.view_id.clone(),
        instance_id: None,
        event: UiInputEvent::Key {
            code: "enter".to_string(),
            modifiers: Vec::new(),
        },
    };
    let direct_ui_input = measure_async(config, || {
        PluginHost::on_ui_input(routed_plugin.as_ref(), &ui_input)
    })
    .await?;
    report(
        config,
        "direct_plugin_ui_input",
        "on_ui_input",
        "direct_plugin_ui_input",
        direct_ui_input,
        direct_ui_input,
        1,
    );

    let routed_ui_input = measure_async(config, || {
        PluginHost::on_ui_input(&populated_composite, &ui_input)
    })
    .await?;
    report(
        config,
        "eight_plugin_composite_ui_input",
        "on_ui_input",
        "direct_plugin_ui_input",
        routed_ui_input,
        direct_ui_input,
        PLUGIN_COUNT,
    );

    let context_owner = Arc::new(BenchmarkPlugin::context_owner());
    let mut context_composite = CompositePluginHost::new();
    context_composite.push(context_owner.clone());
    context_composite.set_capability_owner(CONTEXT_LOADER_CAPABILITY, context_owner.id.clone());
    let context_request = context_request();

    let direct_context = measure_async(config, || {
        PluginHost::load_context(context_owner.as_ref(), &context_request)
    })
    .await?;
    report(
        config,
        "direct_plugin_context_replacement",
        "context_owner_dispatch",
        "direct_plugin_context_replacement",
        direct_context,
        direct_context,
        1,
    );

    let routed_context = measure_async(config, || {
        PluginHost::load_context(&context_composite, &context_request)
    })
    .await?;
    report(
        config,
        "composite_context_owner_dispatch",
        "context_owner_dispatch",
        "direct_plugin_context_replacement",
        routed_context,
        direct_context,
        1,
    );

    let passthrough_loader = PassthroughContextLoader;
    let passthrough_context = measure_async(config, || {
        ContextLoader::load(&passthrough_loader, context_request.clone())
    })
    .await?;
    report(
        config,
        "passthrough_context_loader",
        "context_loader",
        "passthrough_context_loader",
        passthrough_context,
        passthrough_context,
        0,
    );

    let empty_composite_context = measure_async(config, || {
        ContextLoader::load(&empty_composite, context_request.clone())
    })
    .await?;
    report(
        config,
        "empty_composite_context_loader",
        "context_loader",
        "passthrough_context_loader",
        empty_composite_context,
        passthrough_context,
        0,
    );

    let replacement_context = measure_async(config, || {
        ContextLoader::load(&context_composite, context_request.clone())
    })
    .await?;
    report(
        config,
        "plugin_context_replacement_loader",
        "context_loader",
        "empty_composite_context_loader",
        replacement_context,
        empty_composite_context,
        1,
    );

    Ok(())
}

/// 执行预热和正式异步迭代，计时范围包含返回值释放以反映实际调用成本。
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

/// 执行预热和正式同步迭代，用于不经过异步边界的 owner 查询。
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
    })
}

/// 构造包含 64 条历史消息的稳定上下文请求。
fn context_request() -> ContextLoadRequest {
    ContextLoadRequest {
        run_id: "benchmark-run".to_string(),
        step: 3,
        provider: "benchmark-provider".to_string(),
        model: "benchmark-model".to_string(),
        system: Some("Lucia 性能基准系统提示".to_string()),
        messages: (0..64)
            .map(|index| ModelMessage::text(MessageRole::User, format!("历史消息 {index}")))
            .collect(),
    }
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
            "plugin_count": PLUGIN_COUNT,
            "regression_policy": "informational_only",
            "automatic_regression_failure": false,
            "note": "共享 CI 的性能抖动较大，应保存历史分位数并由外部系统判断趋势"
        })
    );
}

/// 输出单项性能结果，并以同组基线计算相对倍率。
#[allow(clippy::too_many_arguments)]
fn report(
    config: BenchmarkConfig,
    name: &str,
    group: &str,
    baseline_name: &str,
    measurement: Measurement,
    baseline: Measurement,
    plugin_count: usize,
) {
    let ns_per_op = measurement.ns_per_op(config.iterations);
    let baseline_ns_per_op = baseline.ns_per_op(config.iterations);
    let relative_to_baseline = if baseline_ns_per_op > 0.0 {
        ns_per_op / baseline_ns_per_op
    } else {
        0.0
    };
    println!(
        "{}",
        json!({
            "type": "benchmark_result",
            "name": name,
            "group": group,
            "baseline": baseline_name,
            "warmup_iterations": config.warmup_iterations,
            "iterations": config.iterations,
            "plugin_count": plugin_count,
            "total_ns": measurement.total.as_nanos(),
            "ns_per_op": ns_per_op,
            "relative_to_baseline": relative_to_baseline,
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
