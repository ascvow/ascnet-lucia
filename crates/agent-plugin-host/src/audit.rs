//! 插件 manifest、WASM Component 接口、能力 owner 与服务调用的中立审计证据。
//!
//! 本模块只描述 Plugin Host 已经掌握的事实，不依赖演进、评测或具体插件协议。调用方可
//! 将这些 DTO 交给独立策略层判定，但不能借此改变 Host 的 manifest 鉴权或 owner 路由。

use crate::manifest::{PluginManifest, ProvidedCapabilityMode, ResolvedPluginCapabilities};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, sync::Mutex};

#[cfg(feature = "wasm")]
use anyhow::Context;
#[cfg(feature = "wasm")]
use std::path::Path;
#[cfg(feature = "wasm")]
use wasmtime::{
    component::{types::ComponentItem, Component},
    Config, Engine,
};

/// 当前 Component 类型图扫描规则的固定修订号。
///
/// 修订号变化表示路径命名或遍历语义发生变化，与插件 ABI、manifest 版本相互独立。
pub const COMPONENT_INTERFACE_SCANNER_REVISION: &str = "wasmtime-component-types-v1";

/// 编译后的 Component 不保留源码 world 名称时使用的规范根名称。
pub const COMPONENT_ROOT_WORLD: &str = "component-root";

const MAX_SERVICE_METHOD_CHARS: usize = 256;
#[cfg(feature = "wasm")]
const MAX_COMPONENT_INTERFACE_DEPTH: usize = 64;
#[cfg(feature = "wasm")]
const MAX_COMPONENT_INTERFACE_ITEMS: usize = 16_384;

/// manifest 中一次能力请求的规范快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestCapabilityRequest {
    /// Host 定义的稳定能力 ID。
    pub capability_id: String,
    /// manifest 对该能力声明的路径、profile 或其他受限范围。
    pub scopes: Vec<String>,
}

/// manifest 中一次通用能力提供声明的规范快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestProvidedCapability {
    /// `[[provides]].id` 声明的能力 ID，不是插件 owner ID。
    pub capability_id: String,
    /// 提供方声明的能力协议版本。
    pub version: String,
    /// 同一能力允许的提供者基数。
    pub mode: ProvidedCapabilityMode,
}

/// Host 已校验 manifest 的能力证据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestCapabilitySnapshot {
    /// manifest 中的可信插件 ID。
    pub plugin_id: String,
    /// 按能力 ID 与 scope 字节序排列的能力请求。
    pub requested: Vec<ManifestCapabilityRequest>,
    /// 按能力 ID、版本和模式字节序排列的提供声明。
    pub provided: Vec<ManifestProvidedCapability>,
}

/// Component 类型图中的接口条目种类。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ComponentInterfaceItemKind {
    /// Component 函数。
    ComponentFunction,
    /// Core Wasm 函数。
    CoreFunction,
    /// Core Wasm 模块。
    Module,
    /// 嵌套 Component。
    Component,
    /// Component 接口实例。
    Instance,
    /// 接口类型。
    Type,
    /// 资源类型。
    Resource,
}

/// 从真实 Component 类型图扫描到的一个 import 或 export。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentInterfaceItemSnapshot {
    /// 规范路径；根条目使用原名，实例成员使用 `接口名#成员名` 递归连接。
    pub path: String,
    /// 类型图报告的条目种类。
    pub kind: ComponentInterfaceItemKind,
    /// Component Model 的可选 `implements` 标注。
    pub implements: Option<String>,
}

/// 真实 WASM Component 的 world/import/export 类型图快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentInterfaceSnapshot {
    /// 固定扫描器修订号，用于区分路径命名规则。
    pub scanner_revision: String,
    /// 编译产物的规范根 world 名称。
    pub world: String,
    /// 按 UTF-8 字节序去重排列的根 imports 及其实例成员。
    pub imports: Vec<ComponentInterfaceItemSnapshot>,
    /// 按 UTF-8 字节序去重排列的根 exports 及其实例成员。
    pub exports: Vec<ComponentInterfaceItemSnapshot>,
}

/// manifest 请求与真实 Component Host import 的可达性复核结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityImportCheck {
    /// manifest 请求的能力 ID。
    pub capability_id: String,
    /// Host 为该能力定义的全部相关根 import。
    pub mapped_host_imports: Vec<String>,
    /// 当前 Component 类型图中实际存在的相关根 import。
    pub reachable_imports: Vec<String>,
    /// 该 manifest 请求是否至少映射到一个真实可达的 Host import。
    pub satisfied: bool,
}

/// Host 冲突解析后的能力 owner 证据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedCapabilityOwnerSnapshot {
    /// `[[provides]].id` 对应的能力 ID。
    pub capability_id: String,
    /// Host 解析时使用的提供者基数。
    pub mode: ProvidedCapabilityMode,
    /// Host 最终接受的 owner 插件 ID，按 UTF-8 字节序排列。
    pub owner_plugin_ids: Vec<String>,
}

/// JSON 服务结果的脱敏形态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JsonValueKind {
    /// JSON null。
    Null,
    /// JSON 布尔值。
    Boolean,
    /// JSON 数字。
    Number,
    /// JSON 字符串。
    String,
    /// JSON 数组。
    Array,
    /// JSON 对象。
    Object,
}

/// 一次 Host 服务调用的脱敏结果摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostServiceCallResult {
    /// 目标服务成功返回，只保留 JSON 值种类。
    Succeeded {
        /// 返回值的 JSON 形态，不包含返回正文。
        value_kind: JsonValueKind,
    },
    /// 路由或目标服务返回错误。
    Failed {
        /// 稳定错误类别，不包含目标插件返回的原始错误正文。
        error: String,
    },
}

/// Host 实际执行的一次插件服务调用证据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostServiceCallObservation {
    /// Host 注入或应用提交的调用方 ID；Guest 不能覆盖自身实例的该值。
    pub caller_id: String,
    /// 服务注册表实际寻址的目标 owner 插件 ID。
    pub target_owner_id: String,
    /// 目标 owner 内的稳定服务名。
    pub service: String,
    /// 请求 payload 中显式提供的 `method` 或 `operation`；缺失时为 `None`。
    pub method: Option<String>,
    /// 不含原始 payload 和返回正文的结果摘要。
    pub result: HostServiceCallResult,
}

impl HostServiceCallObservation {
    /// 从真实服务路由输入与结果构造脱敏观察记录。
    pub(crate) fn from_result(
        call: &crate::service::PluginServiceCall,
        result: &Result<Value>,
    ) -> Self {
        let method = call
            .payload
            .get("method")
            .or_else(|| call.payload.get("operation"))
            .and_then(Value::as_str)
            .map(|method| truncate_chars(method, MAX_SERVICE_METHOD_CHARS));
        let result = match result {
            Ok(value) => HostServiceCallResult::Succeeded {
                value_kind: json_value_kind(value),
            },
            Err(error) => HostServiceCallResult::Failed {
                error: classify_service_error(&error.to_string()).into(),
            },
        };
        Self {
            caller_id: call.caller_id.clone(),
            target_owner_id: call.plugin_id.clone(),
            service: call.name.clone(),
            method,
            result,
        }
    }
}

/// Host 服务调用的旁路审计观察器。
///
/// 回调在服务调用完成后同步执行，不得阻塞、重入 Host 或抛出 panic。Host 会隔离观察器
/// panic，审计故障不会改变原服务调用结果。
pub trait HostServiceCallObserver: Send + Sync {
    /// 接收一条已经脱敏的真实路由记录。
    fn observe(&self, observation: HostServiceCallObservation);
}

/// 线程安全的内存服务调用观察器，适合测试和短生命周期审计运行。
#[derive(Default)]
pub struct InMemoryHostServiceCallObserver {
    observations: Mutex<Vec<HostServiceCallObservation>>,
}

impl InMemoryHostServiceCallObserver {
    /// 创建空观察器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 返回当前记录的稳定快照；不会清空内部记录。
    pub fn snapshot(&self) -> Vec<HostServiceCallObservation> {
        self.observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 取出当前全部记录并清空观察器。
    pub fn drain(&self) -> Vec<HostServiceCallObservation> {
        std::mem::take(
            &mut *self
                .observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl HostServiceCallObserver for InMemoryHostServiceCallObserver {
    fn observe(&self, observation: HostServiceCallObservation) {
        self.observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(observation);
    }
}

/// Host 对单个插件聚合的协议中立审计证据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginAuditEvidence {
    /// 已校验 manifest 的请求与提供声明。
    pub manifest: ManifestCapabilitySnapshot,
    /// 从真实 WASM Component 扫描的接口类型图。
    pub component: ComponentInterfaceSnapshot,
    /// manifest 请求与真实 Host imports 的复核记录。
    pub capability_import_checks: Vec<CapabilityImportCheck>,
    /// Host 对当前插件集合解析出的能力 owner。
    pub resolved_capability_owners: Vec<ResolvedCapabilityOwnerSnapshot>,
    /// 调用方明确纳入本次审计范围的真实 Host 服务调用观察。
    pub observed_host_service_calls: Vec<HostServiceCallObservation>,
}

/// 从已校验 manifest 生成稳定能力快照。
///
/// # Errors
///
/// manifest 身份、版本、依赖或能力声明无效时返回错误。
pub fn snapshot_manifest_capabilities(
    manifest: &PluginManifest,
) -> Result<ManifestCapabilitySnapshot> {
    manifest.validate()?;
    let capabilities = &manifest.capabilities;
    let mut requested = Vec::new();
    if capabilities.agent.spawn {
        requested.push(ManifestCapabilityRequest {
            capability_id: "agent.spawn".into(),
            scopes: sorted_unique(capabilities.agent.profiles.clone()),
        });
    }
    if capabilities.agent.observe {
        requested.push(capability_request("agent.observe"));
    }
    if capabilities.agent.cancel {
        requested.push(capability_request("agent.cancel"));
    }
    if capabilities.model_completion {
        requested.push(capability_request("model_completion"));
    }
    if capabilities.surface_actions {
        requested.push(capability_request("surface_actions"));
    }
    if capabilities.process_exec {
        requested.push(capability_request("process_exec"));
    }
    if capabilities.http {
        requested.push(capability_request("http"));
    }
    if capabilities.secrets {
        requested.push(capability_request("secrets"));
    }
    if !capabilities.fs_read.is_empty() {
        requested.push(ManifestCapabilityRequest {
            capability_id: "fs_read".into(),
            scopes: sorted_unique(capabilities.fs_read.clone()),
        });
    }
    if !capabilities.fs_write.is_empty() {
        requested.push(ManifestCapabilityRequest {
            capability_id: "fs_write".into(),
            scopes: sorted_unique(capabilities.fs_write.clone()),
        });
    }
    requested.sort_by(|left, right| byte_cmp(&left.capability_id, &right.capability_id));

    let mut provided = manifest
        .provides
        .iter()
        .map(|provided| ManifestProvidedCapability {
            capability_id: provided.id.clone(),
            version: provided.version.clone(),
            mode: provided.mode,
        })
        .collect::<Vec<_>>();
    provided.sort_by(|left, right| {
        byte_cmp(&left.capability_id, &right.capability_id)
            .then_with(|| byte_cmp(&left.version, &right.version))
            .then_with(|| left.mode.cmp(&right.mode))
    });

    Ok(ManifestCapabilitySnapshot {
        plugin_id: manifest.plugin.id.clone(),
        requested,
        provided,
    })
}

/// 将 Host 已解析的全部能力 owner 转换为稳定快照。
pub fn snapshot_resolved_capability_owners(
    resolved: &ResolvedPluginCapabilities,
) -> Vec<ResolvedCapabilityOwnerSnapshot> {
    let mut snapshots = resolved
        .resolved_owners()
        .map(
            |(capability_id, mode, owners)| ResolvedCapabilityOwnerSnapshot {
                capability_id: capability_id.to_string(),
                mode,
                owner_plugin_ids: sorted_unique(owners.to_vec()),
            },
        )
        .collect::<Vec<_>>();
    snapshots.sort_by(|left, right| byte_cmp(&left.capability_id, &right.capability_id));
    snapshots
}

/// 复核 manifest 能力请求在真实 Component 根 imports 中是否可达。
///
/// 未声明的 Host imports 仍由运行期 manifest 鉴权阻断，本函数不会把静态可达性误报为
/// 已授权能力。没有 Host import 映射的能力会得到未满足记录，而不是被静默忽略。
pub fn check_manifest_import_reachability(
    manifest: &ManifestCapabilitySnapshot,
    component: &ComponentInterfaceSnapshot,
) -> Vec<CapabilityImportCheck> {
    let root_imports = component
        .imports
        .iter()
        .filter(|item| !item.path.contains('#'))
        .map(|item| item.path.as_str())
        .collect::<HashSet<_>>();
    manifest
        .requested
        .iter()
        .map(|request| {
            let mapped_host_imports = mapped_host_imports(&request.capability_id)
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>();
            let reachable_imports = mapped_host_imports
                .iter()
                .filter(|name| root_imports.contains(name.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let satisfied = !mapped_host_imports.is_empty() && !reachable_imports.is_empty();
            CapabilityImportCheck {
                capability_id: request.capability_id.clone(),
                mapped_host_imports,
                reachable_imports,
                satisfied,
            }
        })
        .collect()
}

/// 扫描真实 WASM Component 并聚合 Host 审计证据。
///
/// `observed_host_service_calls` 必须来自 [`HostServiceCallObserver`]，调用方负责裁剪时间窗；
/// 本函数不会把服务调用混入 WIT imports 或 manifest capability。
///
/// # Errors
///
/// manifest 无效、Component 文件不可读或 Wasmtime 无法编译该 Component 时返回错误。
#[cfg(feature = "wasm")]
pub fn audit_plugin_component(
    manifest: &PluginManifest,
    component_path: impl AsRef<Path>,
    resolved: &ResolvedPluginCapabilities,
    observed_host_service_calls: Vec<HostServiceCallObservation>,
) -> Result<PluginAuditEvidence> {
    let manifest = snapshot_manifest_capabilities(manifest)?;
    let component = scan_component_interfaces(component_path)?;
    let capability_import_checks = check_manifest_import_reachability(&manifest, &component);
    Ok(PluginAuditEvidence {
        manifest,
        component,
        capability_import_checks,
        resolved_capability_owners: snapshot_resolved_capability_owners(resolved),
        observed_host_service_calls,
    })
}

/// 从真实 WASM Component 文件扫描根 world 的 imports 与 exports。
///
/// 路径按 UTF-8 字节序排序并去重。实例成员使用 `#` 连接，嵌套 Component 的 import
/// 与 export 分别使用 `#import:` 和 `#export:`；运行期服务调用不会出现在该快照中。
///
/// # Errors
///
/// Wasmtime Engine 初始化失败，或文件不是可编译的 Component 时返回错误。
#[cfg(feature = "wasm")]
pub fn scan_component_interfaces(path: impl AsRef<Path>) -> Result<ComponentInterfaceSnapshot> {
    let path = path.as_ref();
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)
        .map_err(|error| anyhow::anyhow!("{error:?}"))
        .context("创建 Component 审计 Engine 失败")?;
    let component = Component::from_file(&engine, path)
        .map_err(|error| anyhow::anyhow!("{error:?}"))
        .with_context(|| format!("编译待审计 Component 失败：{}", path.display()))?;
    scan_compiled_component(&engine, &component)
}

/// 扫描已经由同一 Wasmtime Engine 编译的 Component 类型图。
///
/// # Errors
///
/// 类型图嵌套超过 64 层或条目超过 16384 个时返回错误，避免不受信 Component 通过
/// 审计路径消耗无界栈或内存。
#[cfg(feature = "wasm")]
pub fn scan_compiled_component(
    engine: &Engine,
    component: &Component,
) -> Result<ComponentInterfaceSnapshot> {
    let component_type = component.component_type();
    let mut imports = Vec::new();
    for (name, item) in component_type.imports(engine) {
        collect_component_item(
            engine,
            name.to_string(),
            item.ty,
            item.implements,
            &mut imports,
            0,
        )?;
    }
    let mut exports = Vec::new();
    for (name, item) in component_type.exports(engine) {
        collect_component_item(
            engine,
            name.to_string(),
            item.ty,
            item.implements,
            &mut exports,
            0,
        )?;
    }
    sort_and_deduplicate_interfaces(&mut imports);
    sort_and_deduplicate_interfaces(&mut exports);
    Ok(ComponentInterfaceSnapshot {
        scanner_revision: COMPONENT_INTERFACE_SCANNER_REVISION.into(),
        world: COMPONENT_ROOT_WORLD.into(),
        imports,
        exports,
    })
}

#[cfg(feature = "wasm")]
fn collect_component_item(
    engine: &Engine,
    path: String,
    item: ComponentItem,
    implements: Option<&str>,
    output: &mut Vec<ComponentInterfaceItemSnapshot>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_COMPONENT_INTERFACE_DEPTH {
        return Err(anyhow::anyhow!(
            "Component 接口类型图嵌套超过 {MAX_COMPONENT_INTERFACE_DEPTH} 层"
        ));
    }
    if output.len() >= MAX_COMPONENT_INTERFACE_ITEMS {
        return Err(anyhow::anyhow!(
            "Component 接口类型图超过 {MAX_COMPONENT_INTERFACE_ITEMS} 个条目"
        ));
    }
    let kind = component_item_kind(&item);
    output.push(ComponentInterfaceItemSnapshot {
        path: path.clone(),
        kind,
        implements: implements.map(str::to_string),
    });
    match item {
        ComponentItem::ComponentInstance(instance) => {
            for (name, child) in instance.exports(engine) {
                collect_component_item(
                    engine,
                    format!("{path}#{name}"),
                    child.ty,
                    child.implements,
                    output,
                    depth + 1,
                )?;
            }
        }
        ComponentItem::Component(component) => {
            for (name, child) in component.imports(engine) {
                collect_component_item(
                    engine,
                    format!("{path}#import:{name}"),
                    child.ty,
                    child.implements,
                    output,
                    depth + 1,
                )?;
            }
            for (name, child) in component.exports(engine) {
                collect_component_item(
                    engine,
                    format!("{path}#export:{name}"),
                    child.ty,
                    child.implements,
                    output,
                    depth + 1,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(feature = "wasm")]
fn component_item_kind(item: &ComponentItem) -> ComponentInterfaceItemKind {
    match item {
        ComponentItem::ComponentFunc(_) => ComponentInterfaceItemKind::ComponentFunction,
        ComponentItem::CoreFunc(_) => ComponentInterfaceItemKind::CoreFunction,
        ComponentItem::Module(_) => ComponentInterfaceItemKind::Module,
        ComponentItem::Component(_) => ComponentInterfaceItemKind::Component,
        ComponentItem::ComponentInstance(_) => ComponentInterfaceItemKind::Instance,
        ComponentItem::Type(_) => ComponentInterfaceItemKind::Type,
        ComponentItem::Resource(_) => ComponentInterfaceItemKind::Resource,
    }
}

#[cfg(feature = "wasm")]
fn sort_and_deduplicate_interfaces(items: &mut Vec<ComponentInterfaceItemSnapshot>) {
    items.sort_by(|left, right| {
        byte_cmp(&left.path, &right.path)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.implements.cmp(&right.implements))
    });
    items.dedup();
}

fn capability_request(capability_id: &str) -> ManifestCapabilityRequest {
    ManifestCapabilityRequest {
        capability_id: capability_id.into(),
        scopes: Vec::new(),
    }
}

fn mapped_host_imports(capability_id: &str) -> &'static [&'static str] {
    match capability_id {
        "agent.spawn" | "agent.observe" | "agent.cancel" => &["host-agent-runtime-call"],
        "model_completion" => &["host-model-complete"],
        "surface_actions" => &["host-agent-emit-event"],
        "process_exec" => &[
            "host-process-kill",
            "host-process-read-line",
            "host-process-spawn",
            "host-process-write",
        ],
        "fs_read" => &["host-fs-list", "host-fs-read"],
        _ => &[],
    }
}

fn sorted_unique(mut values: Vec<String>) -> Vec<String> {
    values.sort_by(|left, right| byte_cmp(left, right));
    values.dedup();
    values
}

fn byte_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    left.as_bytes().cmp(right.as_bytes())
}

fn json_value_kind(value: &Value) -> JsonValueKind {
    match value {
        Value::Null => JsonValueKind::Null,
        Value::Bool(_) => JsonValueKind::Boolean,
        Value::Number(_) => JsonValueKind::Number,
        Value::String(_) => JsonValueKind::String,
        Value::Array(_) => JsonValueKind::Array,
        Value::Object(_) => JsonValueKind::Object,
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn classify_service_error(error: &str) -> &'static str {
    if error.contains("未注册服务") {
        "service_not_registered"
    } else if error.contains("当前不可调用") || error.contains("已卸载") {
        "owner_unavailable"
    } else if error.contains("同步循环调用") {
        "synchronous_cycle"
    } else {
        "service_failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{resolve_plugin_capabilities, PluginManifest};
    use std::{collections::HashMap, sync::Arc};

    /// 从最小 TOML 构造经过正式校验的测试 manifest。
    fn manifest_from_toml(input: &str) -> PluginManifest {
        let manifest: PluginManifest = toml::from_str(input).expect("测试 manifest 应可解析");
        manifest.validate().expect("测试 manifest 应通过校验");
        manifest
    }

    /// manifest 请求必须稳定排序，并保留文件路径与 Agent profile 范围。
    #[test]
    fn manifest_snapshot_preserves_requested_scopes() {
        let manifest = manifest_from_toml(
            r#"
                [plugin]
                id = "consumer"
                name = "Consumer"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "consumer.wasm"

                [capabilities]
                fs_read = ["z", "a"]
                model_completion = true

                [capabilities.agent]
                spawn = true
                profiles = ["reviewer", "builder"]
            "#,
        );

        let snapshot = snapshot_manifest_capabilities(&manifest).expect("生成 manifest 快照");
        assert_eq!(
            snapshot
                .requested
                .iter()
                .map(|request| request.capability_id.as_str())
                .collect::<Vec<_>>(),
            vec!["agent.spawn", "fs_read", "model_completion"]
        );
        assert_eq!(snapshot.requested[0].scopes, ["builder", "reviewer"]);
        assert_eq!(snapshot.requested[1].scopes, ["a", "z"]);
    }

    /// owner 快照必须使用 `provides.id`，并保留 Host 已解析的全部 owner。
    #[test]
    fn owner_snapshot_uses_provided_capability_id() {
        let first = manifest_from_toml(
            r#"
                [plugin]
                id = "provider-b"
                name = "Provider B"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "provider-b.wasm"

                [[provides]]
                id = "demo.capability"
                version = "1.0.0"
                mode = "multi"
            "#,
        );
        let second = manifest_from_toml(
            r#"
                [plugin]
                id = "provider-a"
                name = "Provider A"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "provider-a.wasm"

                [[provides]]
                id = "demo.capability"
                version = "1.0.0"
                mode = "multi"
            "#,
        );
        let resolved = resolve_plugin_capabilities(&[first, second], &HashMap::new())
            .expect("解析多 owner 能力");

        let owners = snapshot_resolved_capability_owners(&resolved);
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].capability_id, "demo.capability");
        assert_eq!(owners[0].owner_plugin_ids, ["provider-a", "provider-b"]);
    }

    /// 可达性检查必须区分 manifest 授权与 Component 静态 import。
    #[test]
    fn import_reachability_checks_only_requested_capabilities() {
        let manifest = ManifestCapabilitySnapshot {
            plugin_id: "consumer".into(),
            requested: vec![
                capability_request("fs_read"),
                capability_request("process_exec"),
                capability_request("http"),
            ],
            provided: Vec::new(),
        };
        let component = ComponentInterfaceSnapshot {
            scanner_revision: COMPONENT_INTERFACE_SCANNER_REVISION.into(),
            world: COMPONENT_ROOT_WORLD.into(),
            imports: vec![
                interface_item("host-fs-list"),
                interface_item("host-fs-read"),
                interface_item("host-process-spawn"),
                interface_item("host-service-call"),
            ],
            exports: Vec::new(),
        };

        let checks = check_manifest_import_reachability(&manifest, &component);
        assert!(checks[0].satisfied);
        assert!(checks[1].satisfied);
        assert!(!checks[2].satisfied);
        assert!(!checks.iter().any(|check| check.capability_id == "service"));
    }

    /// 构造根 Component 函数条目。
    fn interface_item(path: &str) -> ComponentInterfaceItemSnapshot {
        ComponentInterfaceItemSnapshot {
            path: path.into(),
            kind: ComponentInterfaceItemKind::ComponentFunction,
            implements: None,
        }
    }

    /// 内存观察器不得保留原始服务返回正文。
    #[test]
    fn service_observation_keeps_only_result_shape() {
        let observer = Arc::new(InMemoryHostServiceCallObserver::new());
        observer.observe(HostServiceCallObservation::from_result(
            &crate::service::PluginServiceCall {
                caller_id: "consumer".into(),
                plugin_id: "provider".into(),
                name: "demo.service".into(),
                payload: serde_json::json!({"method": "lookup", "secret": "request-secret"}),
            },
            &Ok(serde_json::json!({"secret": "response-secret"})),
        ));

        let observations = observer.snapshot();
        assert_eq!(observations[0].method.as_deref(), Some("lookup"));
        assert_eq!(
            observations[0].result,
            HostServiceCallResult::Succeeded {
                value_kind: JsonValueKind::Object
            }
        );
        let encoded = serde_json::to_string(&observations).expect("序列化观察记录");
        assert!(!encoded.contains("request-secret"));
        assert!(!encoded.contains("response-secret"));
    }

    /// 服务失败只允许保留稳定错误类别，不得泄露目标返回的原始错误正文。
    #[test]
    fn service_failure_observation_redacts_error_message() {
        let observation = HostServiceCallObservation::from_result(
            &crate::service::PluginServiceCall {
                caller_id: "consumer".into(),
                plugin_id: "provider".into(),
                name: "demo.service".into(),
                payload: Value::Null,
            },
            &Err(anyhow::anyhow!("目标返回 secret-value")),
        );

        assert_eq!(
            observation.result,
            HostServiceCallResult::Failed {
                error: "service_failed".into()
            }
        );
        assert!(!serde_json::to_string(&observation)
            .expect("序列化失败观察")
            .contains("secret-value"));
    }
}
