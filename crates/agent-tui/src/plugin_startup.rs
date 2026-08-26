//! 插件 manifest 合并与容错启动。

use super::*;
#[cfg(feature = "plugins")]
use std::fs;

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
/// `PluginLoadUpdate`，不会撤销此前已经发布的无关插件。
#[cfg(feature = "plugins")]
pub(crate) async fn load_plugins_for_tui(
    manifests: Vec<PathBuf>,
    capability_selection: HashMap<String, String>,
    agent_template: AgentTemplate,
    run_observer: Option<Arc<dyn agent_runtime::RuntimeRunObserver>>,
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
    let runtime = match run_observer {
        Some(observer) => AgentRuntime::new_with_run_observer(RuntimeLimits::default(), observer),
        None => AgentRuntime::new(RuntimeLimits::default()),
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
    let host_services = PluginHostServices::new()
        .with_model_completion(
            model_gateway,
            model_provider,
            model_name,
            20_000,
            model_stream,
        )?
        .with_agent_runtime(
            Arc::new(runtime),
            controller_profile,
            HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
        )?;
    let _failures = load_wasm_plugins_progressively_with_selection_and_services(
        &manifests,
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
