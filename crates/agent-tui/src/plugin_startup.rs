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

/// 在 TUI 事件循环之外渐进加载插件，并把每个 Ready/Failed 状态立即发回主循环。
///
/// 全局 manifest、依赖或能力规划错误会终止加载；单插件激活失败由 Host 转换为
/// `PluginLoadUpdate`，不会撤销此前已经发布的无关插件。
#[cfg(feature = "plugins")]
pub(crate) async fn load_plugins_for_tui(
    manifests: Vec<PathBuf>,
    capability_selection: HashMap<String, String>,
    agent_template: AgentTemplate,
    live_host: Arc<LivePluginHost>,
    tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    let model_gateway = agent_template.gateway().clone();
    let model_provider = agent_template.options().provider.clone();
    let model_name = agent_template.options().model.clone();
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
    let host_services = PluginHostServices::new()
        .with_model_completion(model_gateway, model_provider, model_name, 20_000, false)?
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
