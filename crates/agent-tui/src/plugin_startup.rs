//! 官方插件发现、manifest 合并与容错启动。

use super::*;

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

/// Appends default official plugins while preserving explicit manifests with the same ID.
///
/// 将默认官方插件补充到显式插件列表，并让同 ID 的显式声明优先。
#[cfg(feature = "plugins")]
pub(crate) fn merge_official_plugin_manifests(
    manifests: &mut Vec<PathBuf>,
    official_manifests: Vec<PathBuf>,
) {
    let mut plugin_ids = manifests
        .iter()
        .map(PluginManifest::load)
        .filter_map(Result::ok)
        .map(|manifest| manifest.plugin.id)
        .collect::<HashSet<_>>();
    for path in official_manifests {
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

/// Loads and activates plugins away from the TUI event loop.
///
/// 在 TUI 事件循环之外加载并激活插件；后续准备失败时会主动关闭已创建的宿主。
#[cfg(feature = "plugins")]
pub(crate) async fn load_plugins_for_tui(
    manifests: Vec<PathBuf>,
    capability_selection: HashMap<String, String>,
    agent_template: AgentTemplate,
) -> Result<LoadedPlugins> {
    let runtime =
        AgentRuntime::new(RuntimeLimits::default()).context("创建 TUI Agent Runtime 失败")?;
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
    let host_services = PluginHostServices::new().with_agent_runtime(
        Arc::new(runtime),
        controller_profile,
        HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
    )?;
    let report = load_wasm_plugins_resilient_with_selection_and_services(
        &manifests,
        &capability_selection,
        host_services,
    )
    .await?;
    let host = Arc::new(report.host);
    let failures = report.failures;
    let prepared = async {
        let plugin_ids = host
            .host_ids()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let plugin_views = host.ui_declarations().await?;
        let startup_events = host.drain_events().await?;
        Ok(LoadedPlugins {
            host: host.clone(),
            plugin_ids,
            plugin_views,
            startup_events,
            failures,
        })
    }
    .await;
    if prepared.is_err() {
        let _ = host.shutdown().await;
    }
    prepared
}
