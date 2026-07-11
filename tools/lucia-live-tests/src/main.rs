//! Lucia 真实模型分级测试运行器。
//!
//! 运行器只把固定标记、工具事件摘要、用量和脱敏错误写入报告，不保存模型原文、
//! API key 或服务商原始响应。

use anyhow::Result;
use agent_core::{
    extension::AgentExtension, Agent, AgentEvent, AgentEventKind, AgentRun, InMemoryEventSink,
    AgentRootConfig, TokenUsage,
};
use agent_plugin_host::{
    manifest::load_plugin_runtime_config, wasm::load_wasm_plugins, PluginHost,
};
use agent_tool::{JsonTool, ToolRegistry, ToolSpec};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

const MINIMAL_MARKER: &str = "LUCIA_LIVE_MINIMAL_OK";
const REACT_MARKER: &str = "LUCIA_LIVE_REACT_OK:react-proof-7F9C2A";
const COMPLEX_MARKER: &str = "LUCIA_LIVE_COMPLEX_OK:lucia-plugin-api-verified";
const PLUGIN_MARKER: &str = "LUCIA_LIVE_PLUGIN_OK:plugin-proof-A13F";

/// 命令行参数。
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "运行 Lucia 真实模型分级测试并输出脱敏 JSON 报告"
)]
struct Args {
    /// Lucia 模型 TOML 配置路径。
    #[arg(long)]
    config: PathBuf,

    /// 要运行的测试级别。
    #[arg(long, value_enum, default_value_t = ScenarioSelection::All)]
    scenario: ScenarioSelection,

    /// 插件场景使用的 plugin.toml；未指定时读取配置中的第一个插件。
    #[arg(long = "plugin-manifest")]
    plugin_manifest: Option<PathBuf>,

    /// 可选的 JSON 报告输出路径；报告始终同时输出到标准输出。
    #[arg(long)]
    report: Option<PathBuf>,
}

/// 命令行支持的场景选择。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScenarioSelection {
    Minimal,
    React,
    Complex,
    Plugin,
    All,
}

impl ScenarioSelection {
    /// 展开为实际运行的场景，并保持由简单到复杂的顺序。
    fn scenarios(self) -> Vec<LiveScenario> {
        match self {
            Self::Minimal => vec![LiveScenario::Minimal],
            Self::React => vec![LiveScenario::React],
            Self::Complex => vec![LiveScenario::Complex],
            Self::Plugin => vec![LiveScenario::Plugin],
            Self::All => vec![
                LiveScenario::Minimal,
                LiveScenario::React,
                LiveScenario::Complex,
                LiveScenario::Plugin,
            ],
        }
    }
}

/// 报告中的稳定场景名称。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LiveScenario {
    Minimal,
    React,
    Complex,
    Plugin,
}

/// 一次完整测试套件的脱敏报告。
#[derive(Debug, Serialize)]
struct SuiteReport {
    schema_version: u32,
    passed: bool,
    duration_ms: u64,
    scenarios: Vec<ScenarioReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SuiteReport {
    /// 根据场景报告生成总结果，任一失败都会使总结果失败。
    fn new(duration_ms: u64, scenarios: Vec<ScenarioReport>) -> Self {
        Self {
            schema_version: 1,
            passed: scenarios.iter().all(|scenario| scenario.passed),
            duration_ms,
            scenarios,
            error: None,
        }
    }
}

/// 单个场景的脱敏报告。
#[derive(Debug, Serialize)]
struct ScenarioReport {
    scenario: LiveScenario,
    passed: bool,
    duration_ms: u64,
    steps_used: Option<usize>,
    tool_calls: Vec<ToolCallReport>,
    usage: TokenUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ScenarioReport {
    /// 创建尚未进入 Agent 运行阶段的失败报告。
    fn setup_failed(scenario: LiveScenario, started: Instant, error: impl Into<String>) -> Self {
        Self {
            scenario,
            passed: false,
            duration_ms: elapsed_ms(started),
            steps_used: None,
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            error: Some(error.into()),
        }
    }

    /// 在已有错误后追加一个不包含外部响应内容的诊断原因。
    fn append_error(&mut self, error: &str) {
        self.passed = false;
        match &mut self.error {
            Some(current) => {
                current.push('；');
                current.push_str(error);
            }
            None => self.error = Some(error.to_string()),
        }
    }
}

/// 报告中的单次工具调用摘要。
#[derive(Debug, Serialize, PartialEq, Eq)]
struct ToolCallReport {
    name: String,
    succeeded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

/// 仅在进程内使用的工具事件证据，工具结果不会直接写入报告。
#[derive(Debug)]
struct ObservedToolCall {
    call_id: String,
    name: String,
    started_at_ms: u64,
    is_error: Option<bool>,
    result: Option<Value>,
    duration_ms: Option<u64>,
}

impl ObservedToolCall {
    /// 转换为不带参数、结果和服务商调用 ID 的公开报告。
    fn to_report(&self) -> ToolCallReport {
        ToolCallReport {
            name: self.name.clone(),
            succeeded: self.is_error == Some(false),
            duration_ms: self.duration_ms,
        }
    }
}

/// 启动运行器并根据总结果设置退出码。
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let mut report = run_suite(&args).await;

    if let Some(path) = &args.report {
        if persist_report(&report, path).is_err() {
            report.passed = false;
            report.error = Some("无法写入指定的报告文件".to_string());
        }
    }

    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(_) => {
            println!("{{\"schema_version\":1,\"passed\":false,\"error\":\"无法序列化测试报告\"}}");
            std::process::exit(2);
        }
    }

    if !report.passed {
        std::process::exit(1);
    }
}

/// 顺序运行所选场景；单场景失败不会阻止后续场景。
async fn run_suite(args: &Args) -> SuiteReport {
    let suite_started = Instant::now();
    let selected = args.scenario.scenarios();
    let config = match AgentRootConfig::load(&args.config) {
        Ok(config) => config,
        Err(_) => {
            let reports = selected
                .into_iter()
                .map(|scenario| {
                    ScenarioReport::setup_failed(
                        scenario,
                        Instant::now(),
                        "无法加载模型配置；请检查 TOML、API key 环境变量和服务商字段",
                    )
                })
                .collect();
            return SuiteReport::new(elapsed_ms(suite_started), reports);
        }
    };

    let plugin_manifest = resolve_plugin_manifest(args);
    let mut reports = Vec::with_capacity(selected.len());
    for scenario in selected {
        let report = match scenario {
            LiveScenario::Minimal | LiveScenario::React | LiveScenario::Complex => {
                run_core_scenario(&config, scenario).await
            }
            LiveScenario::Plugin => match &plugin_manifest {
                Some(path) => run_plugin_scenario(&config, path).await,
                None => ScenarioReport::setup_failed(
                    scenario,
                    Instant::now(),
                    "插件场景缺少 plugin.toml；请传入 --plugin-manifest 或在配置中声明插件",
                ),
            },
        };
        reports.push(report);
    }

    SuiteReport::new(elapsed_ms(suite_started), reports)
}

/// 优先使用命令行 manifest，否则读取 Lucia 配置中的第一个插件。
fn resolve_plugin_manifest(args: &Args) -> Option<PathBuf> {
    args.plugin_manifest.clone().or_else(|| {
        load_plugin_runtime_config(&args.config)
            .ok()
            .and_then(|runtime| runtime.manifest_paths.into_iter().next())
    })
}

/// 运行不依赖插件的真实模型场景。
async fn run_core_scenario(config: &AgentRootConfig, scenario: LiveScenario) -> ScenarioReport {
    let started = Instant::now();
    let sink = Arc::new(InMemoryEventSink::new());
    let (tools, prompt, minimum_steps) = match scenario {
        LiveScenario::Minimal => (ToolRegistry::new(), minimal_prompt(), 2),
        LiveScenario::React => match react_tools() {
            Ok(tools) => (tools, react_prompt(), 4),
            Err(_) => {
                return ScenarioReport::setup_failed(scenario, started, "无法注册 ReAct 测试工具")
            }
        },
        LiveScenario::Complex => match complex_tools() {
            Ok(tools) => (tools, complex_prompt(), 8),
            Err(_) => {
                return ScenarioReport::setup_failed(scenario, started, "无法注册复杂场景测试工具")
            }
        },
        LiveScenario::Plugin => unreachable!("插件场景由专用入口运行"),
    };

    let agent = match build_agent(config, tools, sink.clone(), minimum_steps) {
        Ok(agent) => agent,
        Err(_) => {
            return ScenarioReport::setup_failed(
                scenario,
                started,
                "无法创建 Agent；请检查模型配置和 API key 环境变量",
            )
        }
    };

    let outcome = agent.run(prompt).await;
    let events = sink.events().await;
    report_from_run(scenario, started, outcome, &events)
}

/// 加载真实 WASM 插件并运行插件工具场景。
async fn run_plugin_scenario(config: &AgentRootConfig, manifest: &Path) -> ScenarioReport {
    let scenario = LiveScenario::Plugin;
    let started = Instant::now();
    let host = match load_wasm_plugins(&[manifest]).await {
        Ok(host) => Arc::new(host),
        Err(_) => {
            return ScenarioReport::setup_failed(
                scenario,
                started,
                "无法加载 WASM 插件；请检查 manifest、组件产物、ABI 和权限",
            )
        }
    };

    let exposed_tools = match host.list_tools().await {
        Ok(tools) => tools,
        Err(_) => {
            let _ = PluginHost::shutdown(host.as_ref()).await;
            return ScenarioReport::setup_failed(scenario, started, "无法读取插件工具定义");
        }
    };
    if !exposed_tools.iter().any(|tool| tool.name == "echo") {
        let _ = PluginHost::shutdown(host.as_ref()).await;
        return ScenarioReport::setup_failed(
            scenario,
            started,
            "插件场景要求真实 WASM 插件公开 echo 工具",
        );
    }

    let sink = Arc::new(InMemoryEventSink::new());
    let mut agent = match build_agent(config, ToolRegistry::new(), sink.clone(), 5) {
        Ok(agent) => agent,
        Err(_) => {
            let _ = PluginHost::shutdown(host.as_ref()).await;
            return ScenarioReport::setup_failed(
                scenario,
                started,
                "无法创建插件场景 Agent；请检查模型配置和 API key 环境变量",
            );
        }
    };
    agent
        .set_extension(host.clone())
        .set_context_loader(host.clone());

    let outcome = agent.run(plugin_prompt()).await;
    let events = sink.events().await;
    let mut report = report_from_run(scenario, started, outcome, &events);
    if PluginHost::shutdown(host.as_ref()).await.is_err() {
        report.append_error("插件卸载失败");
    }
    report
}

/// 从配置创建场景 Agent，并设置事件收集器与最低 ReAct 步数。
fn build_agent(
    config: &AgentRootConfig,
    tools: ToolRegistry,
    sink: Arc<InMemoryEventSink>,
    minimum_steps: usize,
) -> Result<Agent> {
    let mut agent = config.build_agent()?;
    agent.options_mut().max_steps = agent.options().max_steps.max(minimum_steps);
    agent.options_mut().system_prompt.push_str(
        "\n\nThis is an automated Lucia capability test. Follow the user's exact output and tool-use constraints. Do not fabricate tool results.",
    );
    agent.set_tools(tools).set_event_sink(sink);
    Ok(agent)
}

/// 把 Agent 运行结果和事件证据转换为脱敏场景报告。
fn report_from_run(
    scenario: LiveScenario,
    started: Instant,
    outcome: Result<AgentRun>,
    events: &[AgentEvent],
) -> ScenarioReport {
    let observations = observe_tool_calls(events);
    let tool_calls = observations
        .iter()
        .map(ObservedToolCall::to_report)
        .collect();

    match outcome {
        Ok(run) => {
            let error = validate_scenario(scenario, &run.final_text, &observations);
            ScenarioReport {
                scenario,
                passed: error.is_none(),
                duration_ms: elapsed_ms(started),
                steps_used: Some(run.steps_used),
                tool_calls,
                usage: run.usage,
                error,
            }
        }
        Err(_) => ScenarioReport {
            scenario,
            passed: false,
            duration_ms: elapsed_ms(started),
            steps_used: None,
            tool_calls,
            usage: usage_from_events(events),
            error: Some(
                "Agent 运行失败；请检查网络、服务商兼容性、工具调用格式和最大步数".to_string(),
            ),
        },
    }
}

/// 验证最终标记以及无法仅靠文本伪造的工具事件证据。
fn validate_scenario(
    scenario: LiveScenario,
    final_text: &str,
    observations: &[ObservedToolCall],
) -> Option<String> {
    let marker = marker_for(scenario);
    let mut failures = Vec::new();
    if final_text.trim() != marker {
        failures.push("最终回答没有精确匹配场景标记");
    }

    let expected_tools = expected_tools(scenario);
    if !expected_tools.is_empty() && !contains_valid_tool_sequence(scenario, observations) {
        failures.push("未观察到顺序正确且结果有效的工具调用事件");
    }

    if failures.is_empty() {
        None
    } else {
        Some(failures.join("；"))
    }
}

/// 返回场景要求模型精确输出的固定标记。
fn marker_for(scenario: LiveScenario) -> &'static str {
    match scenario {
        LiveScenario::Minimal => MINIMAL_MARKER,
        LiveScenario::React => REACT_MARKER,
        LiveScenario::Complex => COMPLEX_MARKER,
        LiveScenario::Plugin => PLUGIN_MARKER,
    }
}

/// 返回场景必须按顺序完成的工具名称。
fn expected_tools(scenario: LiveScenario) -> &'static [&'static str] {
    match scenario {
        LiveScenario::Minimal => &[],
        LiveScenario::React => &["lucia_test_nonce"],
        LiveScenario::Complex => &[
            "lucia_test_project",
            "lucia_test_requirement",
            "lucia_test_verify",
        ],
        LiveScenario::Plugin => &["echo"],
    }
}

/// 检查成功工具调用是否包含指定场景所需的有效有序子序列。
fn contains_valid_tool_sequence(scenario: LiveScenario, observations: &[ObservedToolCall]) -> bool {
    let expected = expected_tools(scenario);
    let mut expected_index = 0;
    for observation in observations {
        if expected_index >= expected.len() {
            break;
        }
        if observation.name == expected[expected_index]
            && observation.is_error == Some(false)
            && result_matches(scenario, expected_index, observation.result.as_ref())
        {
            expected_index += 1;
        }
    }
    expected_index == expected.len()
}

/// 校验确定性工具返回值，防止模型只调用同名工具或伪造完成状态。
fn result_matches(scenario: LiveScenario, index: usize, result: Option<&Value>) -> bool {
    let Some(result) = result else {
        return false;
    };
    match (scenario, index) {
        (LiveScenario::React, 0) => {
            result["valid"] == true && result["nonce"] == "react-proof-7F9C2A"
        }
        (LiveScenario::Complex, 0) => {
            result["project"] == "lucia" && result["requirement_id"] == "REQ-2048"
        }
        (LiveScenario::Complex, 1) => {
            result["valid"] == true
                && result["component"] == "plugin-api"
                && result["check_token"] == "CTX-91"
        }
        (LiveScenario::Complex, 2) => {
            result["valid"] == true && result["proof"] == "lucia-plugin-api-verified"
        }
        (LiveScenario::Plugin, 0) => {
            result["source"] == "wasm-plugin" && result["echo"] == "plugin-proof-A13F"
        }
        _ => false,
    }
}

/// 从 Agent 事件恢复工具调用顺序、成功状态和耗时。
fn observe_tool_calls(events: &[AgentEvent]) -> Vec<ObservedToolCall> {
    let mut observations = Vec::new();
    for event in events {
        match event.kind {
            AgentEventKind::ToolStarted => {
                let Some(call_id) = event.payload.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = event.payload.get("name").and_then(Value::as_str) else {
                    continue;
                };
                observations.push(ObservedToolCall {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    started_at_ms: event.timestamp_ms,
                    is_error: None,
                    result: None,
                    duration_ms: None,
                });
            }
            AgentEventKind::ToolFinished => {
                let Some(call_id) = event.payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(observation) = observations
                    .iter_mut()
                    .rev()
                    .find(|item| item.call_id == call_id && item.is_error.is_none())
                {
                    observation.is_error = event.payload.get("is_error").and_then(Value::as_bool);
                    observation.result = event.payload.get("result").cloned();
                    observation.duration_ms =
                        Some(event.timestamp_ms.saturating_sub(observation.started_at_ms));
                }
            }
            _ => {}
        }
    }
    observations
}

/// 从计费事件聚合失败运行中已经产生的 token 用量。
fn usage_from_events(events: &[AgentEvent]) -> TokenUsage {
    let mut total = TokenUsage::default();
    for event in events {
        if event.kind != AgentEventKind::BillingUsage {
            continue;
        }
        let Some(usage) = event.payload.get("usage") else {
            continue;
        };
        if let Ok(usage) = serde_json::from_value::<TokenUsage>(usage.clone()) {
            total.add_assign(&usage);
        }
    }
    total
}

/// 创建一次 ReAct 场景使用的确定性 nonce 工具。
fn react_tools() -> Result<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    tools.register(JsonTool::new(
        ToolSpec::new(
            "lucia_test_nonce",
            "Validate the challenge and return the nonce required by the Lucia live test.",
            json!({
                "type": "object",
                "properties": {
                    "challenge": { "type": "string" }
                },
                "required": ["challenge"],
                "additionalProperties": false
            }),
        ),
        |args| async move {
            if args.get("challenge").and_then(Value::as_str) == Some("react-2048") {
                Ok(json!({"valid": true, "nonce": "react-proof-7F9C2A"}))
            } else {
                Ok(json!({"valid": false}))
            }
        },
    ))?;
    Ok(tools)
}

/// 创建复杂场景使用的三段确定性数据链工具。
fn complex_tools() -> Result<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    tools.register(JsonTool::new(
        ToolSpec::new(
            "lucia_test_project",
            "Return the project and requirement id for the Lucia live test.",
            ToolSpec::empty_object_schema(),
        ),
        |_| async move { Ok(json!({"project": "lucia", "requirement_id": "REQ-2048"})) },
    ))?;
    tools.register(JsonTool::new(
        ToolSpec::new(
            "lucia_test_requirement",
            "Resolve a requirement id returned by lucia_test_project.",
            json!({
                "type": "object",
                "properties": {
                    "requirement_id": { "type": "string" }
                },
                "required": ["requirement_id"],
                "additionalProperties": false
            }),
        ),
        |args| async move {
            if args.get("requirement_id").and_then(Value::as_str) == Some("REQ-2048") {
                Ok(json!({
                    "valid": true,
                    "component": "plugin-api",
                    "check_token": "CTX-91"
                }))
            } else {
                Ok(json!({"valid": false}))
            }
        },
    ))?;
    tools.register(JsonTool::new(
        ToolSpec::new(
            "lucia_test_verify",
            "Verify the chained project, component, and check token.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "component": { "type": "string" },
                    "check_token": { "type": "string" }
                },
                "required": ["project", "component", "check_token"],
                "additionalProperties": false
            }),
        ),
        |args| async move {
            let valid = args.get("project").and_then(Value::as_str) == Some("lucia")
                && args.get("component").and_then(Value::as_str) == Some("plugin-api")
                && args.get("check_token").and_then(Value::as_str) == Some("CTX-91");
            if valid {
                Ok(json!({"valid": true, "proof": "lucia-plugin-api-verified"}))
            } else {
                Ok(json!({"valid": false}))
            }
        },
    ))?;
    Ok(tools)
}

/// 返回最小模型连通性场景提示。
fn minimal_prompt() -> &'static str {
    "Reply with exactly LUCIA_LIVE_MINIMAL_OK and no other text."
}

/// 返回要求真实工具事件的单步 ReAct 场景提示。
fn react_prompt() -> &'static str {
    "Call lucia_test_nonce exactly once with challenge set to react-2048. After reading its returned nonce, reply with exactly LUCIA_LIVE_REACT_OK:react-proof-7F9C2A and no other text. You must not answer before calling the tool."
}

/// 返回要求按数据依赖顺序调用三个工具的复杂场景提示。
fn complex_prompt() -> &'static str {
    "Complete this chained task using tools. First call lucia_test_project. Use its requirement_id to call lucia_test_requirement. Then use the project, component, and check_token returned by those tools to call lucia_test_verify. Do not guess values or call a later tool before receiving the prior result. After verification, reply with exactly LUCIA_LIVE_COMPLEX_OK:lucia-plugin-api-verified and no other text."
}

/// 返回真实 WASM echo 插件场景提示。
fn plugin_prompt() -> &'static str {
    "Call the echo tool exactly once with text set to plugin-proof-A13F. After reading the plugin result, reply with exactly LUCIA_LIVE_PLUGIN_OK:plugin-proof-A13F and no other text. You must not answer before calling the tool."
}

/// 持久化格式化 JSON 报告，并创建缺失的父目录。
fn persist_report(report: &SuiteReport, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

/// 把单调时钟耗时转换为不会溢出的毫秒数。
fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建带固定时间戳的测试事件。
    fn event(kind: AgentEventKind, timestamp_ms: u64, payload: Value) -> AgentEvent {
        let mut event = AgentEvent::new("test-run", kind, 0, payload);
        event.timestamp_ms = timestamp_ms;
        event
    }

    /// 创建一组成功工具调用事件。
    fn successful_call(id: &str, name: &str, result: Value, at: u64) -> Vec<AgentEvent> {
        vec![
            event(
                AgentEventKind::ToolStarted,
                at,
                json!({"id": id, "name": name, "args": {}}),
            ),
            event(
                AgentEventKind::ToolFinished,
                at + 7,
                json!({
                    "call_id": id,
                    "name": name,
                    "is_error": false,
                    "result": result
                }),
            ),
        ]
    }

    /// 最小场景只接受精确标记，不接受包裹文本。
    #[test]
    fn minimal_requires_exact_marker() {
        assert_eq!(
            validate_scenario(LiveScenario::Minimal, MINIMAL_MARKER, &[]),
            None
        );
        assert!(
            validate_scenario(LiveScenario::Minimal, "Result: LUCIA_LIVE_MINIMAL_OK", &[])
                .is_some()
        );
    }

    /// ReAct 场景必须同时具有正确工具结果和最终标记。
    #[test]
    fn react_requires_real_tool_evidence() {
        assert!(validate_scenario(LiveScenario::React, REACT_MARKER, &[]).is_some());

        let events = successful_call(
            "call-1",
            "lucia_test_nonce",
            json!({"valid": true, "nonce": "react-proof-7F9C2A"}),
            100,
        );
        let observations = observe_tool_calls(&events);
        assert_eq!(
            validate_scenario(LiveScenario::React, REACT_MARKER, &observations),
            None
        );
        assert_eq!(observations[0].to_report().duration_ms, Some(7));
    }

    /// 复杂场景拒绝顺序错误的多工具调用。
    #[test]
    fn complex_requires_ordered_valid_chain() {
        let mut events = Vec::new();
        events.extend(successful_call(
            "call-2",
            "lucia_test_requirement",
            json!({"valid": true, "component": "plugin-api", "check_token": "CTX-91"}),
            10,
        ));
        events.extend(successful_call(
            "call-1",
            "lucia_test_project",
            json!({"project": "lucia", "requirement_id": "REQ-2048"}),
            20,
        ));
        events.extend(successful_call(
            "call-3",
            "lucia_test_verify",
            json!({"valid": true, "proof": "lucia-plugin-api-verified"}),
            30,
        ));
        let observations = observe_tool_calls(&events);
        assert!(validate_scenario(LiveScenario::Complex, COMPLEX_MARKER, &observations).is_some());

        events.clear();
        events.extend(successful_call(
            "call-1",
            "lucia_test_project",
            json!({"project": "lucia", "requirement_id": "REQ-2048"}),
            10,
        ));
        events.extend(successful_call(
            "call-2",
            "lucia_test_requirement",
            json!({"valid": true, "component": "plugin-api", "check_token": "CTX-91"}),
            20,
        ));
        events.extend(successful_call(
            "call-3",
            "lucia_test_verify",
            json!({"valid": true, "proof": "lucia-plugin-api-verified"}),
            30,
        ));
        let observations = observe_tool_calls(&events);
        assert_eq!(
            validate_scenario(LiveScenario::Complex, COMPLEX_MARKER, &observations),
            None
        );
    }

    /// 总报告在任一场景失败时必须失败，且序列化结果不包含模型原文或密钥字段。
    #[test]
    fn suite_report_fails_without_sensitive_fields() {
        let reports = vec![
            ScenarioReport {
                scenario: LiveScenario::Minimal,
                passed: true,
                duration_ms: 2,
                steps_used: Some(1),
                tool_calls: Vec::new(),
                usage: TokenUsage::default(),
                error: None,
            },
            ScenarioReport::setup_failed(LiveScenario::Plugin, Instant::now(), "插件加载失败"),
        ];
        let report = SuiteReport::new(4, reports);
        let encoded = serde_json::to_string(&report).expect("报告应可序列化");

        assert!(!report.passed);
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("raw_provider_response"));
        assert!(!encoded.contains("final_text"));
    }

    /// 失败运行仍应从计费事件聚合已产生的 token 用量。
    #[test]
    fn failed_run_usage_is_aggregated_from_events() {
        let events = vec![event(
            AgentEventKind::BillingUsage,
            10,
            json!({
                "provider": "test",
                "model": "test",
                "usage": {"input_tokens": 10, "output_tokens": 3, "total_tokens": 13},
                "provider_billing": null
            }),
        )];
        let usage = usage_from_events(&events);
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(13));
    }
}
