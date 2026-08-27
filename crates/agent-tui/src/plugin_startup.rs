//! 插件 manifest 合并与容错启动。

use super::*;
#[cfg(feature = "plugins")]
use std::fs;

/// 插件启动期间由可信装配层固定的执行约束。
///
/// `execution_policy` 同时收紧插件 Host、派生 Agent 和模型输出预算；`run_observer` 把子 Agent
/// Run 接入 Evidence；`require_complete_genome` 要求 Genome 固定的插件组合全部进入 Ready；
/// `activation_metadata` 只向对应插件实例传递可信装配数据。
#[cfg(feature = "plugins")]
pub(crate) struct PluginExecutionContext {
    /// Host 与 Runtime 共同执行的可信策略上限。
    pub(crate) execution_policy: agent_tool::ExecutionPolicy,
    /// 可选的可信 Runtime Run 观察器。
    pub(crate) run_observer: Option<Arc<dyn agent_runtime::RuntimeRunObserver>>,
    /// 是否拒绝任何不完整的 Genome 插件组合。
    pub(crate) require_complete_genome: bool,
    /// 按插件 ID 隔离的 Host 可信激活元数据。
    pub(crate) activation_metadata: HashMap<String, HashMap<String, String>>,
}

/// 把可信执行策略的派生拓扑限制映射到 Plugin Host 使用的 Agent Runtime。
#[cfg(feature = "plugins")]
fn plugin_runtime_limits(policy: &agent_tool::ExecutionPolicy) -> RuntimeLimits {
    RuntimeLimits::default().clamped_by(&policy.limits)
}

/// 返回插件模型完成服务的有效输出上限，不能超过 Host 固定上限或 Genome 策略上限。
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

/// 在 TUI 事件循环之外渐进加载插件，并把每个 Ready/Failed 状态立即发回主循环。
///
/// 全局 manifest、依赖或能力规划错误会终止加载；单插件激活失败由 Host 转换为
/// `PluginLoadUpdate`，不会撤销此前已经发布的无关插件。`require_complete_genome` 为真时，
/// 任一单插件失败也会让整体结果失败，调用方必须阻止不完整组合开始 Evidence Run。
#[cfg(feature = "plugins")]
pub(crate) async fn load_plugins_for_tui(
    manifests: Vec<PathBuf>,
    capability_selection: HashMap<String, String>,
    agent_template: AgentTemplate,
    execution: PluginExecutionContext,
    live_host: Arc<LivePluginHost>,
    tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    if manifests.is_empty() {
        return Ok(());
    }
    let model_gateway = agent_template.gateway().clone();
    let model_provider = agent_template.options().provider.clone();
    let model_name = agent_template.options().model.clone();
    // 插件模型调用与主 Agent 使用相同的流式开关：部分代理会强制断开
    // 长时间无数据的非流式连接，摘要这类长请求必须跟随配置的传输方式。
    let model_stream = agent_template.options().stream;
    let runtime_limits = plugin_runtime_limits(&execution.execution_policy);
    let runtime = match execution.run_observer {
        Some(observer) => AgentRuntime::new_with_run_observer(runtime_limits, observer),
        None => AgentRuntime::new(runtime_limits),
    }
    .context("创建 TUI Agent Runtime 失败")?;
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
    let model_output_limit = plugin_model_output_limit(&execution.execution_policy);
    let mut host_services =
        PluginHostServices::new().restrict_execution_policy(&execution.execution_policy);
    for (plugin_id, metadata) in execution.activation_metadata {
        host_services = host_services.with_activation_metadata(plugin_id, metadata)?;
    }
    let host_services = host_services
        .with_model_completion(
            model_gateway,
            model_provider,
            model_name,
            model_output_limit,
            model_stream,
        )?
        .with_agent_runtime(
            Arc::new(runtime),
            controller_profile,
            HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
        )?;
    let failures = load_wasm_plugins_progressively_with_selection_and_services(
        &manifests,
        &capability_selection,
        host_services,
        live_host.as_ref(),
        |update| {
            let _ = tx.send(UiEvent::PluginLoadUpdate(update));
        },
    )
    .await?;
    if execution.require_complete_genome && !failures.is_empty() {
        let details = failures
            .iter()
            .map(|failure| format!("{}: {}", failure.plugin_id, failure.reason))
            .collect::<Vec<_>>()
            .join("；");
        return Err(anyhow!("Genome 插件组合未完整加载：{details}"));
    }
    Ok(())
}

#[cfg(all(test, feature = "plugins"))]
mod tests {
    use super::*;

    /// Evaluation Genome 必须同步收紧插件派生拓扑与模型输出预算。
    #[test]
    fn evidence_policy_clamps_plugin_runtime_and_model_budget() {
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
}
