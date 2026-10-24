//! 插件 manifest 合并与容错启动。

use crate::event::UiEvent;
#[cfg(feature = "plugins")]
use crate::plugin::config::{
    load_plugin_dir_config, resolve_plugin_settings, PluginRouteDefaults, PluginSettings,
};
use crate::plugin::sessions::TuiPluginSessionSource;
use agent_core::model::ModelProviderConfig;
use agent_core::AgentModelConfig;
use agent_plugin_host::manifest::PluginManifest;
use agent_plugin_host::wasm::load_wasm_plugins_progressively_with_selection_and_services;
use agent_plugin_host::{LivePluginHost, PluginHostServices};
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate,
    RuntimeLimits, ToolAccess,
};
use agent_session::SessionStore;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
#[cfg(feature = "plugins")]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 插件可请求的默认 worker 只读工具集合；新增原生工具不会自动进入派生 Agent。
const WORKER_READ_ONLY_TOOLS: [&str; 4] = [
    "list_directory",
    "search_files",
    "read_file",
    agent_skill::SKILL_READ_TOOL_NAME,
];

/// 构造 TUI 授予插件 worker profile 的显式只读派生策略。
///
/// 只收缩工具权限，不覆盖步数：worker 与主会话一样允许长时间运行，交互式主会话的
/// `max_steps = 0`（不设上限）会原样继承，队员因此可以一直跑到任务完成。
fn worker_agent_derive_config() -> AgentDeriveConfig {
    AgentDeriveConfig {
        permissions: AgentPermissions {
            tools: ToolAccess::allowlist(WORKER_READ_ONLY_TOOLS),
        },
        ..AgentDeriveConfig::default()
    }
}

/// 插件启动期间由可信装配层固定的执行约束。
///
/// `execution_policy` 同时收紧插件 Host、派生 Agent 和模型输出预算；`run_observer` 供可信
/// 装配层观察派生 Run，调用方不能通过本上下文绕过或替换。
#[cfg(feature = "plugins")]
pub(crate) struct PluginExecutionContext {
    /// Host 与 Runtime 共同执行的可信策略上限。
    pub(crate) execution_policy: agent_tool::ExecutionPolicy,
    /// 应用持有控制句柄的 Agent Runtime；Esc 可通过共享实例取消插件派生任务。
    pub(crate) runtime: AgentRuntime,
    /// 插件无工具模型完成使用的固定模型 ID；缺省时沿用主模型。
    pub(crate) model: Option<String>,
    /// 应用默认路由使用的服务商配置；插件未配置专属路由时沿用它。
    pub(crate) model_provider: ModelProviderConfig,
    /// Serve 平面使用的应用插件状态目录；受限平面必须保持为 `None`。
    pub(crate) persistent_state_dir: Option<PathBuf>,
}

#[cfg(feature = "plugins")]
impl PluginExecutionContext {
    /// 按可信执行策略创建插件 Runtime 与 Host 装配上下文。
    ///
    /// `run_observer` 只供应用可信观察派生 Run；模型路由与状态目录均由应用注入。
    /// Runtime 限额无效或初始化失败时返回错误，不启动任何插件。
    pub(crate) fn new(
        execution_policy: agent_tool::ExecutionPolicy,
        run_observer: Option<Arc<dyn agent_runtime::RuntimeRunObserver>>,
        model: Option<String>,
        model_provider: ModelProviderConfig,
        persistent_state_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let runtime_limits = plugin_runtime_limits(&execution_policy);
        let runtime = match run_observer {
            Some(observer) => AgentRuntime::new_with_run_observer(runtime_limits, observer),
            None => AgentRuntime::new(runtime_limits),
        }
        .context("创建 TUI Agent Runtime 失败")?;
        Ok(Self {
            execution_policy,
            runtime,
            model,
            model_provider,
            persistent_state_dir,
        })
    }

    /// 返回应用控制插件派生任务使用的共享 Runtime 句柄。
    pub(crate) fn runtime(&self) -> &AgentRuntime {
        &self.runtime
    }
}

/// 把可信执行策略的派生拓扑限制映射到 Plugin Host 使用的 Agent Runtime。
#[cfg(feature = "plugins")]
fn plugin_runtime_limits(policy: &agent_tool::ExecutionPolicy) -> RuntimeLimits {
    RuntimeLimits::default().clamped_by(&policy.limits)
}

/// 返回插件专属模型路由在网关中注册使用的 provider 名称。
///
/// 加前缀是为了与用户配置的服务商名称隔离，避免插件 ID 与 `model.name` 撞名后互相
/// 覆盖路由。
#[cfg(feature = "plugins")]
fn plugin_provider_name(plugin_id: &str) -> String {
    format!("plugin:{plugin_id}")
}

/// 按 manifest 逐个解析插件配置，合并插件目录默认值与主配置段。
///
/// 返回值按插件 ID 索引；manifest 无法解析时跳过该条目，由后续加载流程统一报告失败。
///
/// # Errors
///
/// 插件配置文件不合法，或模型路由字段缺少凭据、类型不符时返回错误，使问题在启动期而不是
/// 首次调用模型时暴露。
#[cfg(feature = "plugins")]
fn resolve_all_plugin_settings(
    manifests: &[PathBuf],
    plugin_configs: &HashMap<String, toml::Value>,
    defaults: &PluginRouteDefaults,
) -> Result<HashMap<String, PluginSettings>> {
    let mut resolved = HashMap::new();
    for path in manifests {
        let Ok(manifest) = PluginManifest::load(path) else {
            continue;
        };
        let plugin_id = manifest.plugin.id;
        let plugin_dir = path.parent().unwrap_or(Path::new("."));
        let dir_config = load_plugin_dir_config(plugin_dir)
            .with_context(|| format!("读取插件 `{plugin_id}` 的配置失败"))?;
        let settings = resolve_plugin_settings(
            &plugin_id,
            &plugin_provider_name(&plugin_id),
            dir_config,
            plugin_configs.get(&plugin_id),
            defaults,
        )
        .with_context(|| format!("解析插件 `{plugin_id}` 的配置失败"))?;
        resolved.insert(plugin_id, settings);
    }
    Ok(resolved)
}

/// 返回插件模型完成服务的有效输出上限，不能超过 Host 固定上限或执行策略上限。
#[cfg(feature = "plugins")]
fn plugin_model_output_limit(policy: &agent_tool::ExecutionPolicy) -> u32 {
    policy
        .limits
        .max_tokens
        .map_or(20_000, |limit| limit.min(20_000))
}

/// Builds load-order-preserving summaries from plugin activation events.
///
/// 根据插件激活事件生成保持加载顺序的启动摘要；没有事件文本的插件仅显示 ID。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_startup_details(plugin_ids: &[String], events: &[Value]) -> Vec<String> {
    let mut status_by_id = HashMap::new();
    for event in events {
        if event
            .pointer("/presentation/target")
            .and_then(Value::as_str)
            == Some("footer")
        {
            continue;
        }
        let Some(plugin_id) = event.pointer("/source/id").and_then(Value::as_str) else {
            continue;
        };
        let text = event
            .pointer("/presentation/text")
            .and_then(Value::as_str)
            .or_else(|| event.pointer("/data/text").and_then(Value::as_str))
            .or_else(|| event.get("name").and_then(Value::as_str));
        if let Some(text) = text {
            status_by_id.insert(plugin_id, text);
        }
    }
    plugin_ids
        .iter()
        .map(|plugin_id| {
            status_by_id
                .get(plugin_id.as_str())
                .map(|text| format!("{plugin_id}: {text}"))
                .unwrap_or_else(|| plugin_id.clone())
        })
        .collect()
}

/// 合并另一组插件 manifest，并保留先出现的同 ID 声明。
///
/// 调用方按优先级传入来源；无法解析的 manifest 保留给后台容错加载器报告。
#[cfg(feature = "plugins")]
pub(crate) fn merge_plugin_manifests(manifests: &mut Vec<PathBuf>, incoming: Vec<PathBuf>) {
    let mut plugin_ids = manifests
        .iter()
        .map(PluginManifest::load)
        .filter_map(Result::ok)
        .map(|manifest| manifest.plugin.id)
        .collect::<HashSet<_>>();
    for path in incoming {
        let should_append = PluginManifest::load(&path)
            .map(|manifest| plugin_ids.insert(manifest.plugin.id))
            // Keep invalid manifests for the background resilient loader to report after first paint.
            // 保留无效 manifest，由后台容错加载器报告，不在 TUI 首帧前中断。
            .unwrap_or(true);
        if should_append {
            manifests.push(path);
        }
    }
}

/// 扫描插件根目录下一层的独立 bundle，并按目录名稳定返回 manifest。
///
/// 每个直接子目录必须包含普通文件 `plugin.toml`；符号链接、普通散落文件和更深层目录
/// 不会被自动加载。根目录不存在时返回空列表，其他读取错误会返回给调用方。
#[cfg(feature = "plugins")]
pub(crate) fn discover_plugin_manifests(root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取插件目录失败：{}", root.display()));
        }
    };
    let mut bundles = entries
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("遍历插件目录失败：{}", root.display()))?;
    bundles.sort_by_key(|entry| entry.file_name());

    let mut manifests = Vec::new();
    for bundle in bundles {
        let file_type = bundle
            .file_type()
            .with_context(|| format!("读取插件目录项失败：{}", bundle.path().display()))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let manifest = bundle.path().join("plugin.toml");
        let metadata = match fs::symlink_metadata(&manifest) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取插件 manifest 失败：{}", manifest.display()));
            }
        };
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            manifests.push(manifest);
        }
    }
    Ok(manifests)
}

/// Removes manifests whose plugin ID appears in the user's disabled list.
///
/// 按用户配置的禁用插件 ID 剔除 manifest；对受管理插件和显式声明同样生效。
/// 无法解析的 manifest 保留给后台容错加载器报告，不在这里静默丢弃。
#[cfg(feature = "plugins")]
pub(crate) fn remove_disabled_plugin_manifests(manifests: &mut Vec<PathBuf>, disabled: &[String]) {
    if disabled.is_empty() {
        return;
    }
    manifests.retain(|path| {
        PluginManifest::load(path)
            .map(|manifest| !disabled.contains(&manifest.plugin.id))
            .unwrap_or(true)
    });
}

/// Reads stable plugin IDs for the loading footer before components are activated.
///
/// 在 component 激活前读取稳定插件 ID，供加载中的底栏展示。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_manifest_ids(manifests: &[PathBuf]) -> Vec<String> {
    manifests
        .iter()
        .map(|path| {
            PluginManifest::load(path)
                .map(|manifest| manifest.plugin.id)
                .unwrap_or_else(|_| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| path.display().to_string())
                })
        })
        .collect()
}

/// 没有插件需要加载时立即结束能力规划。
///
/// 返回 `true` 表示调用方可以直接收尾。Host 的工具准入守卫在能力规划完成前一律拒绝
/// 调用；没有插件时不存在「等待策略 owner Ready」的加载窗口，因此这一步必须显式完成，
/// 否则「不装插件」会表现为「所有工具都没有权限」，而界面上没有任何插件报错可看。
#[cfg(feature = "plugins")]
fn finish_planning_when_no_plugins(
    manifests: &[PathBuf],
    live_host: &LivePluginHost,
) -> Result<bool> {
    if !manifests.is_empty() {
        return Ok(false);
    }
    live_host.finish_capability_planning()?;
    Ok(true)
}

/// 在 TUI 事件循环之外复核并渐进加载插件，把每个 Ready/Failed 状态立即发回主循环。
///
/// 全局 manifest、依赖或能力规划错误会终止加载；单插件激活失败由 Host 转换为
/// `PluginLoadUpdate`，不会撤销��前已经发布的无关插件。
#[cfg(feature = "plugins")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_plugins_for_tui(
    manifests: Vec<PathBuf>,
    capability_selection: HashMap<String, String>,
    plugin_configs: HashMap<String, toml::Value>,
    agent_template: AgentTemplate,
    execution: PluginExecutionContext,
    workspace_scope: String,
    workspace_root: PathBuf,
    session_store: Arc<dyn SessionStore>,
    live_host: Arc<LivePluginHost>,
    tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    if finish_planning_when_no_plugins(&manifests, live_host.as_ref())? {
        return Ok(());
    }
    let prepared_manifests = manifests;
    let PluginExecutionContext {
        execution_policy,
        runtime,
        model,
        model_provider,
        persistent_state_dir,
    } = execution;
    let model_gateway = agent_template.gateway().clone();
    let model_name = model
        .clone()
        .unwrap_or_else(|| agent_template.options().model.clone());
    // 插件模型调用与主 Agent 使用相同的流式开关：部分代理会强制断开
    // 长时间无数据的非流式连接，摘要这类长请求必须跟随配置的传输方式。
    let model_stream = agent_template.options().stream;
    // 插件补全的缓存策略跟随主 Agent 配置；缓存键由 Host 按插件 ID 生成。
    let model_cache_retention = agent_template.options().cache_retention;
    let workspace_root_path = workspace_root;
    let workspace_root = workspace_root_path.to_string_lossy().into_owned();
    let controller_profile =
        AgentProfileId::new("tui-controller").context("创建 TUI controller profile 失败")?;
    runtime
        .register_profile(
            controller_profile.clone(),
            agent_template,
            AgentPermissions::default(),
        )
        .await
        .context("注册 TUI controller profile 失败")?;
    let model_output_limit = plugin_model_output_limit(&execution_policy);
    // 插件配置在 Host 侧解析：模型路由由宿主消费并注册成专属 provider，其余键值作为激活
    // 元数据下发，密钥不进入 Guest。
    let route_defaults = PluginRouteDefaults {
        stream: model_stream,
        cache_retention: model_cache_retention,
    };
    let plugin_settings =
        resolve_all_plugin_settings(&prepared_manifests, &plugin_configs, &route_defaults)?;
    for (plugin_id, settings) in &plugin_settings {
        let Some(route) = settings.route.as_ref() else {
            continue;
        };
        model_gateway
            .register_from_config(route.provider.clone())
            .with_context(|| format!("注册插件 `{plugin_id}` 的专属模型路由失败"))?;
    }
    let mut host_services = PluginHostServices::new().restrict_execution_policy(&execution_policy);
    let mut activation_metadata = plugin_settings
        .iter()
        .map(|(plugin_id, settings)| (plugin_id.clone(), settings.metadata.clone()))
        .collect::<HashMap<_, _>>();
    // 宿主注入的可信项排在插件配置之后写入，插件不得用配置伪造工作区身份。
    //
    // 工作区身份对全部插件注入，而不是按插件 ID 白名单：它与可信 plugin_id 属于同一类
    // 宿主事实，任何需要按项目隔离自身数据的插件都要用它，按名单发放只会让每个新插件
    // 都得回来改这里。注意这里给出的只是标识与路径，读取仓库内容仍需 manifest 的
    // 文件能力单独授权。
    for metadata in activation_metadata.values_mut() {
        metadata.extend([
            ("workspace_scope".to_string(), workspace_scope.clone()),
            ("workspace_root".to_string(), workspace_root.clone()),
        ]);
    }
    // 配置了 HTTP 端点的插件额外拿到该端点的非敏感摘要。插件用它判定「端点是否换过」
    // ——换端点意味着嵌入空间失效、语义索引必须重建。下发的是摘要而不是地址与密钥。
    for (plugin_id, settings) in &plugin_settings {
        let Some(http) = settings.http.as_ref() else {
            continue;
        };
        if let Some(metadata) = activation_metadata.get_mut(plugin_id) {
            metadata.insert("http_endpoint_id".to_string(), http.endpoint_id());
        }
    }
    for (plugin_id, metadata) in activation_metadata {
        host_services = host_services.with_activation_metadata(plugin_id, metadata)?;
    }
    // 插件持久存储按项目隔离，作用域由宿主注入，不经过 manifest 或 Guest。
    host_services = host_services.with_workspace_scope(workspace_scope)?;
    // 工作区读取边界使用 Host 持有的规范化路径，不能依赖可被插件配置覆盖的激活元数据。
    host_services = host_services.with_workspace_root(workspace_root_path)?;
    // 只读会话来源同样由宿主注入；传入的端口不含写入方法。
    host_services =
        host_services.with_session_source(Arc::new(TuiPluginSessionSource::new(session_store)));
    if let Some(state_dir) = persistent_state_dir {
        host_services = host_services.with_persistent_state_dir(state_dir)?;
    }
    // 应用默认路由沿用主 Agent 的服务商，只替换模型与调用参数。
    let mut default_route = AgentModelConfig::new(model_provider, model_name);
    default_route.stream = model_stream;
    default_route.cache_retention = model_cache_retention;
    host_services = host_services.with_model_completion(
        model_gateway.clone(),
        default_route,
        model_output_limit,
    )?;
    // 配置了专属路由的插件覆盖应用默认值；输出上限仍受执行策略约束。
    for (plugin_id, settings) in &plugin_settings {
        let Some(route) = settings.route.as_ref() else {
            continue;
        };
        host_services = host_services.with_plugin_model_completion(
            plugin_id,
            model_gateway.clone(),
            route.clone(),
            model_output_limit,
        )?;
    }
    // 配置了 HTTP 端点的插件获得受限网络能力；凭证留在 Host 侧，不进入激活元数据。
    // 实际能否发请求还要看 manifest 的 allow_hosts 是否覆盖该 base URL。
    for (plugin_id, settings) in &plugin_settings {
        let Some(http) = settings.http.as_ref() else {
            continue;
        };
        host_services = host_services.with_plugin_http(
            plugin_id,
            &http.base_url,
            http.credential_headers.clone(),
            http.max_request_bytes,
            http.max_response_bytes,
            http.timeout_ms,
        )?;
    }
    let host_services = host_services.with_agent_runtime(
        Arc::new(runtime),
        controller_profile,
        HashMap::from([("worker".to_string(), worker_agent_derive_config())]),
    )?;
    load_wasm_plugins_progressively_with_selection_and_services(
        &prepared_manifests,
        &capability_selection,
        host_services,
        live_host.as_ref(),
        |update| {
            let _ = tx.send(UiEvent::PluginLoadUpdate(update));
        },
    )
    .await?;
    Ok(())
}

#[cfg(all(test, feature = "plugins"))]
mod tests {
    use super::*;

    /// Evaluation 执行策略必须同步收紧插件派生拓扑与模型输出预算。
    #[test]
    fn evaluation_policy_clamps_plugin_runtime_and_model_budget() {
        let policy = agent_tool::ExecutionPolicy::evaluation("/tmp/lucia-fixture");
        let limits = plugin_runtime_limits(&policy);

        assert_eq!(limits.max_depth, 2);
        assert_eq!(limits.max_children_per_agent, 4);
        assert_eq!(limits.max_concurrent_agents, 2);
        assert_eq!(plugin_model_output_limit(&policy), 4096);
    }

    /// Serve 平面仍保留既有 Host 上限，不因新增接线扩大资源。
    #[test]
    fn serve_policy_keeps_existing_plugin_limits() {
        assert_eq!(
            plugin_runtime_limits(&agent_tool::ExecutionPolicy::serve()),
            RuntimeLimits::default()
        );
        assert_eq!(
            plugin_model_output_limit(&agent_tool::ExecutionPolicy::serve()),
            20_000
        );
    }

    /// worker profile 必须保持显式只读，不能因默认 `All` 或未来新增工具获得副作用能力。
    #[test]
    fn worker_profile_exposes_only_explicit_read_tools() {
        let config = worker_agent_derive_config();

        for allowed in WORKER_READ_ONLY_TOOLS {
            assert!(
                config.permissions.tools.permits(allowed),
                "应允许 {allowed}"
            );
        }
        for denied in [
            "write_file",
            "shell",
            "process_exec",
            "future_dangerous_tool",
        ] {
            assert!(
                !config.permissions.tools.permits(denied),
                "worker 不得获得 {denied}"
            );
        }
    }

    /// 队员允许长期运行，worker 不得被注入步数上限。
    ///
    /// 交互主会话可以配置 `max_steps = 0`（不设上限）；worker 若被覆盖成有限步数，
    /// 大任务会在中途被截断，与「队员一直跑到完成」的产品语义冲突。
    #[test]
    fn worker_inherits_unbounded_step_budget() {
        let config = worker_agent_derive_config();

        assert_eq!(
            config.options.max_steps, None,
            "worker 不得覆盖主会话的步数配置"
        );
    }

    /// 不装任何插件时工具必须照常放行。
    ///
    /// 工具准入守卫挂在总线上，规划未完成就拒绝一切调用；如果「没有 manifest」这条
    /// 早退路径忘了收尾，用户看到的就是「所有工具都没有权限」，而没有任何插件报错。
    #[tokio::test]
    async fn empty_plugin_set_still_admits_tool_calls() {
        use agent_core::{AgentEvent, AgentEventKind, AgentRecordEvent, EventBus, ToolCallEvent};
        use agent_tool::ToolCall;
        use serde_json::json;

        let live = LivePluginHost::new();
        let bus = EventBus::new();
        live.attach_bus(bus.clone()).expect("挂载事件总线");
        assert!(
            finish_planning_when_no_plugins(&[], &live).expect("没有插件时应直接结束规划"),
            "空 manifest 集合必须由该分支收尾"
        );

        bus.dispatch(AgentRecordEvent {
            record: AgentEvent::new("no-plugin-run", AgentEventKind::RunStarted, 0, json!({})),
        })
        .await;

        let mut call = ToolCall::new("native-1", "read_file", json!({ "path": "AGENTS.md" }));
        call.bind_run_id("no-plugin-run");
        let dispatched = bus
            .dispatch(ToolCallEvent::new("no-plugin-run", 0, call))
            .await;

        assert!(
            !dispatched.is_cancelled(),
            "没有插件时不应拒绝工具：{:?}",
            dispatched.event.block_reason
        );
    }

    /// 有 manifest 时该分支必须保持沉默，把收尾交给真实加载流程。
    #[test]
    fn plugin_manifest_set_is_not_finished_early() {
        let live = LivePluginHost::new();
        assert!(
            !finish_planning_when_no_plugins(&[PathBuf::from("plugin.toml")], &live)
                .expect("非空 manifest 不应报错"),
            "有插件时不能在加载前提前结束规划"
        );
    }
}
