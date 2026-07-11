//! 多插件依赖规划、能力选择与容错加载。

use super::*;

/// 将多个 WASM 插件 manifest 加载为一个组合宿主。
pub async fn load_wasm_plugins<P: AsRef<Path>>(paths: &[P]) -> Result<CompositePluginHost> {
    load_wasm_plugins_with_selection(paths, &HashMap::new()).await
}

/// 使用可扩展宿主服务加载多个 WASM 插件。
pub async fn load_wasm_plugins_with_services<P: AsRef<Path>>(
    paths: &[P],
    host_services: PluginHostServices,
) -> Result<CompositePluginHost> {
    load_wasm_plugins_with_selection_and_services(paths, &HashMap::new(), host_services).await
}

/// 使用应用显式选择解析独占能力并加载多个 WASM 插件。
pub async fn load_wasm_plugins_with_selection<P: AsRef<Path>>(
    paths: &[P],
    capability_selection: &HashMap<String, String>,
) -> Result<CompositePluginHost> {
    load_wasm_plugins_with_selection_and_services(
        paths,
        capability_selection,
        PluginHostServices::default(),
    )
    .await
}

/// 使用独占能力选择和可扩展宿主服务加载多个 WASM 插件。
pub async fn load_wasm_plugins_with_selection_and_services<P: AsRef<Path>>(
    paths: &[P],
    capability_selection: &HashMap<String, String>,
    host_services: PluginHostServices,
) -> Result<CompositePluginHost> {
    let mut pending = Vec::with_capacity(paths.len());
    for path in paths {
        let manifest_path = path.as_ref();
        let manifest = PluginManifest::load(manifest_path)?;
        let plugin_dir = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let wasm_path = plugin_dir.join(&manifest.plugin.wasm);
        pending.push((manifest, wasm_path, plugin_dir));
    }
    let manifests = pending
        .iter()
        .map(|(manifest, _, _)| manifest.clone())
        .collect::<Vec<_>>();
    let resolved_capabilities = resolve_plugin_capabilities(&manifests, capability_selection)?;
    let order = resolve_plugin_load_order(&manifests)?;
    let services = Arc::new(ServiceRegistry::default());
    let mut composite = CompositePluginHost::new();
    if let Some(owner) = resolved_capabilities.exclusive_owner(CONTEXT_LOADER_CAPABILITY) {
        composite.set_capability_owner(CONTEXT_LOADER_CAPABILITY, owner);
    }
    if let Some(owner) = resolved_capabilities.exclusive_owner(TOOL_POLICY_CAPABILITY) {
        composite.set_capability_owner(TOOL_POLICY_CAPABILITY, owner);
    }
    for index in order {
        let (manifest, wasm_path, plugin_dir) = pending[index].clone();
        let loading = WasmPluginHost::load_with_limits_in_dir(
            manifest,
            wasm_path,
            plugin_dir,
            WasmPluginLimits::default(),
            services.clone(),
            host_services.clone(),
        )
        .await;
        let host = match loading {
            Ok(host) => host,
            Err(error) => {
                let _ = composite.shutdown().await;
                return Err(error);
            }
        };
        composite.push(Arc::new(host));
    }
    Ok(composite)
}

/// Loads plugins independently and retains unrelated successes after activation failures.
///
/// 独立加载插件；激活失败后保留无关的成功插件，并按依赖关系跳过必选依赖方。
pub async fn load_wasm_plugins_resilient<P: AsRef<Path>>(paths: &[P]) -> Result<PluginLoadReport> {
    load_wasm_plugins_resilient_with_selection(paths, &HashMap::new()).await
}

/// Loads plugins resiliently with explicit exclusive-capability selections.
///
/// 使用显式独占能力选择进行容错加载。
pub async fn load_wasm_plugins_resilient_with_selection<P: AsRef<Path>>(
    paths: &[P],
    capability_selection: &HashMap<String, String>,
) -> Result<PluginLoadReport> {
    load_wasm_plugins_resilient_with_selection_and_services(
        paths,
        capability_selection,
        PluginHostServices::default(),
    )
    .await
}

/// Returns failed required dependencies while ignoring optional ones.
///
/// 返回加载失败的必选依赖，并忽略可选依赖。
pub(super) fn failed_required_dependencies(
    manifest: &PluginManifest,
    failed_ids: &HashSet<String>,
) -> Vec<String> {
    manifest
        .dependencies
        .iter()
        .filter(|dependency| !dependency.optional && failed_ids.contains(&dependency.id))
        .map(|dependency| dependency.id.clone())
        .collect()
}

/// Builds a partial dependency plan and isolates invalid required dependency closures.
///
/// 生成容错依赖计划，仅隔离必选依赖无效的插件及其依赖闭包。
///
/// Missing optional dependencies and optional dependencies whose provider was excluded do not
/// block a plugin. An installed optional dependency with an incompatible version keeps the strict
/// manifest semantics and excludes the dependent plugin.
///
/// 缺失的可选依赖、或已被隔离的可选依赖不会阻止插件；已安装但版本不兼容的
/// 可选依赖仍遵循严格 manifest 语义，剔除对应依赖方。
pub(super) fn resilient_dependency_plan(
    manifests: &[PluginManifest],
) -> Result<(Vec<usize>, Vec<PluginLoadFailure>)> {
    let mut by_id = HashMap::new();
    for (index, manifest) in manifests.iter().enumerate() {
        if by_id.insert(manifest.plugin.id.as_str(), index).is_some() {
            return Err(anyhow!("插件 ID 重复：`{}`", manifest.plugin.id));
        }
    }

    let mut failed = vec![None; manifests.len()];
    for (index, manifest) in manifests.iter().enumerate() {
        for dependency in &manifest.dependencies {
            let Some(&provider_index) = by_id.get(dependency.id.as_str()) else {
                if !dependency.optional {
                    failed[index] = Some(PluginLoadFailure {
                        plugin_id: manifest.plugin.id.clone(),
                        reason: format!("缺少必选依赖 `{}`", dependency.id),
                        blocked_by: vec![dependency.id.clone()],
                    });
                    break;
                }
                continue;
            };
            let requirement = VersionReq::parse(&dependency.version)?;
            let provider_version = Version::parse(&manifests[provider_index].plugin.version)?;
            if !requirement.matches(&provider_version) {
                failed[index] = Some(PluginLoadFailure {
                    plugin_id: manifest.plugin.id.clone(),
                    reason: format!(
                        "依赖 `{}` 需要版本 `{}`，当前为 `{provider_version}`",
                        dependency.id, dependency.version
                    ),
                    blocked_by: if dependency.optional {
                        Vec::new()
                    } else {
                        vec![dependency.id.clone()]
                    },
                });
                break;
            }
        }
    }

    // Propagate required failures layer by layer while preserving optional dependents.
    // 逐层传播必选依赖失败，保留只声明了可选依赖的插件。
    loop {
        let failed_ids = failed
            .iter()
            .filter_map(|failure| failure.as_ref().map(|failure| failure.plugin_id.clone()))
            .collect::<HashSet<_>>();
        let mut changed = false;
        for (index, manifest) in manifests.iter().enumerate() {
            if failed[index].is_some() {
                continue;
            }
            let blocked_by = manifest
                .dependencies
                .iter()
                .filter(|dependency| {
                    !dependency.optional && failed_ids.contains(dependency.id.as_str())
                })
                .map(|dependency| dependency.id.clone())
                .collect::<Vec<_>>();
            if blocked_by.is_empty() {
                continue;
            }
            failed[index] = Some(PluginLoadFailure {
                plugin_id: manifest.plugin.id.clone(),
                reason: format!("必选依赖加载失败：{}", blocked_by.join("、")),
                blocked_by,
            });
            changed = true;
        }
        if !changed {
            break;
        }
    }

    let mut outgoing = vec![Vec::new(); manifests.len()];
    let mut indegree = vec![0usize; manifests.len()];
    for (dependent_index, manifest) in manifests.iter().enumerate() {
        if failed[dependent_index].is_some() {
            continue;
        }
        for dependency in manifest
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
        {
            let provider_index = by_id[dependency.id.as_str()];
            if failed[provider_index].is_none() {
                outgoing[provider_index].push(dependent_index);
                indegree[dependent_index] += 1;
            }
        }
    }

    let required_order = topological_order(&outgoing, &indegree, &failed);
    let expected = failed.iter().filter(|failure| failure.is_none()).count();
    if required_order.len() != expected {
        let ordered = required_order.iter().copied().collect::<HashSet<_>>();
        let blocked = failed
            .iter()
            .enumerate()
            .filter_map(|(index, failure)| {
                (failure.is_none() && !ordered.contains(&index)).then_some(index)
            })
            .collect::<Vec<_>>();
        let blocked_ids = blocked
            .iter()
            .map(|index| manifests[*index].plugin.id.as_str())
            .collect::<Vec<_>>();
        let reason = format!("必选依赖链存在循环：{}", blocked_ids.join("、"));
        let blocked_set = blocked.iter().copied().collect::<HashSet<_>>();
        for index in blocked {
            let blocked_by = manifests[index]
                .dependencies
                .iter()
                .filter(|dependency| !dependency.optional)
                .filter_map(|dependency| by_id.get(dependency.id.as_str()).copied())
                .filter(|provider| blocked_set.contains(provider))
                .map(|provider| manifests[provider].plugin.id.clone())
                .collect();
            failed[index] = Some(PluginLoadFailure {
                plugin_id: manifests[index].plugin.id.clone(),
                reason: reason.clone(),
                blocked_by,
            });
        }
    }

    // Add optional edges only when they preserve the required DAG, preferring provider-first load.
    // 必选图无环后，尽量加入不会制造循环的可选依赖边，让可选 provider 优先加载。
    let mut outgoing = vec![Vec::new(); manifests.len()];
    let mut indegree = vec![0usize; manifests.len()];
    for (dependent_index, manifest) in manifests.iter().enumerate() {
        if failed[dependent_index].is_some() {
            continue;
        }
        for dependency in manifest
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
        {
            let provider_index = by_id[dependency.id.as_str()];
            if failed[provider_index].is_some() {
                continue;
            }
            outgoing[provider_index].push(dependent_index);
            indegree[dependent_index] += 1;
        }
    }
    for (dependent_index, manifest) in manifests.iter().enumerate() {
        if failed[dependent_index].is_some() {
            continue;
        }
        for dependency in manifest
            .dependencies
            .iter()
            .filter(|dependency| dependency.optional)
        {
            let Some(&provider_index) = by_id.get(dependency.id.as_str()) else {
                continue;
            };
            if failed[provider_index].is_some()
                || path_exists(&outgoing, dependent_index, provider_index)
            {
                continue;
            }
            outgoing[provider_index].push(dependent_index);
            indegree[dependent_index] += 1;
        }
    }

    let order = topological_order(&outgoing, &indegree, &failed);
    let failures = failed.into_iter().flatten().collect();
    Ok((order, failures))
}

/// Returns a deterministic topological order for nodes that have not failed.
///
/// 返回未失败节点的稳定拓扑顺序。
fn topological_order(
    outgoing: &[Vec<usize>],
    indegree: &[usize],
    failed: &[Option<PluginLoadFailure>],
) -> Vec<usize> {
    let mut indegree = indegree.to_vec();
    let mut ready = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (failed[index].is_none() && *degree == 0).then_some(index))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::new();
    while let Some(index) = ready.pop_first() {
        order.push(index);
        for &dependent in &outgoing[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }
    order
}

/// Returns whether the current directed graph contains a path between two nodes.
///
/// 判断当前有向图中两个节点之间是否已存在路径。
fn path_exists(outgoing: &[Vec<usize>], start: usize, target: usize) -> bool {
    let mut stack = vec![start];
    let mut visited = HashSet::new();
    while let Some(index) = stack.pop() {
        if index == target {
            return true;
        }
        if visited.insert(index) {
            stack.extend(outgoing[index].iter().copied());
        }
    }
    false
}

/// Loads plugins resiliently with capability selections and application host services.
///
/// 使用独占能力选择和应用宿主服务进行容错加载。
///
/// Invalid manifests, dependency incompatibilities, dependency cycles, and runtime activation
/// failures only remove the affected plugin and plugins whose required dependencies are
/// unavailable. Optional dependents and unrelated plugins continue loading. Duplicate stable IDs
/// and invalid capability selections remain global configuration errors because ownership would
/// otherwise be ambiguous.
///
/// 无效 manifest、依赖不兼容、依赖循环和运行时激活失败，只会剔除受影响插件及必选依赖
/// 不可用的依赖方；可选依赖方和无关插件继续加载。重复稳定 ID 和无效能力选择仍是
/// 全局配置错误，因为宿主无法安全确定 owner。
pub async fn load_wasm_plugins_resilient_with_selection_and_services<P: AsRef<Path>>(
    paths: &[P],
    capability_selection: &HashMap<String, String>,
    host_services: PluginHostServices,
) -> Result<PluginLoadReport> {
    let mut pending = Vec::with_capacity(paths.len());
    let mut failures = Vec::new();
    for path in paths {
        let manifest_path = path.as_ref();
        let manifest = match PluginManifest::load(manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                let plugin_id = manifest_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| manifest_path.display().to_string());
                failures.push(PluginLoadFailure {
                    plugin_id,
                    reason: error.to_string(),
                    blocked_by: Vec::new(),
                });
                continue;
            }
        };
        let plugin_dir = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let wasm_path = plugin_dir.join(&manifest.plugin.wasm);
        pending.push((manifest, wasm_path, plugin_dir));
    }
    let manifests = pending
        .iter()
        .map(|(manifest, _, _)| manifest.clone())
        .collect::<Vec<_>>();
    let (order, dependency_failures) = resilient_dependency_plan(&manifests)?;
    let eligible_manifests = order
        .iter()
        .map(|index| manifests[*index].clone())
        .collect::<Vec<_>>();
    let resolved_capabilities =
        resolve_plugin_capabilities(&eligible_manifests, capability_selection)?;
    let selected_context_owner = resolved_capabilities
        .exclusive_owner(CONTEXT_LOADER_CAPABILITY)
        .map(str::to_string);
    let selected_tool_policy_owner = resolved_capabilities
        .exclusive_owner(TOOL_POLICY_CAPABILITY)
        .map(str::to_string);
    let services = Arc::new(ServiceRegistry::default());
    let mut composite = CompositePluginHost::new();
    failures.extend(dependency_failures);
    let mut failed_ids = failures
        .iter()
        .map(|failure| failure.plugin_id.clone())
        .collect::<HashSet<_>>();

    for index in order {
        let (manifest, wasm_path, plugin_dir) = pending[index].clone();
        let plugin_id = manifest.plugin.id.clone();
        let blocked_by = failed_required_dependencies(&manifest, &failed_ids);
        if !blocked_by.is_empty() {
            failed_ids.insert(plugin_id.clone());
            failures.push(PluginLoadFailure {
                plugin_id,
                reason: format!("必选依赖加载失败：{}", blocked_by.join("、")),
                blocked_by,
            });
            continue;
        }

        let loading = WasmPluginHost::load_with_limits_in_dir(
            manifest,
            wasm_path,
            plugin_dir,
            WasmPluginLimits::default(),
            services.clone(),
            host_services.clone(),
        )
        .await;
        let host = match loading {
            Ok(host) => host,
            Err(error) => {
                failed_ids.insert(plugin_id.clone());
                failures.push(PluginLoadFailure {
                    plugin_id,
                    reason: error.to_string(),
                    blocked_by: Vec::new(),
                });
                continue;
            }
        };
        composite.push(Arc::new(host));
    }

    if let Some(owner) = selected_context_owner {
        if composite.get(&owner).is_some() {
            composite.set_capability_owner(CONTEXT_LOADER_CAPABILITY, owner);
        }
    }
    if let Some(owner) = selected_tool_policy_owner {
        if composite.get(&owner).is_some() {
            composite.set_capability_owner(TOOL_POLICY_CAPABILITY, owner);
        }
    }
    Ok(PluginLoadReport {
        host: composite,
        failures,
    })
}
