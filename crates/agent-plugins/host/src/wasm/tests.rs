//! WASM 宿主导入、燃料预算与 Guest 返回值校验的回归测试。

use crate::manifest::PluginManifest;
use crate::ui::UiDeclaration;
use crate::ui::UiPlacement;
use crate::wasm::engine::shared_wasm_engine;
use crate::wasm::fuel::context_fuel_budget;
use crate::wasm::fuel::CONTEXT_FUEL_PER_INPUT_BYTE;
use crate::wasm::fuel::MAX_CONTEXT_FUEL;
use crate::wasm::guard::bind_guest_tool_result;
use crate::wasm::guard::sanitize_guest_tool_result;
use crate::wasm::guard::validate_ui_instance;
use crate::wasm::linker::PLUGIN_HOST_FAILURE_DETAIL_KEY;
use agent_tool::ToolCall;
use agent_tool::ToolErrorKind;
use agent_tool::ToolResult;
use anyhow::Result;
use loader::{
    failed_required_dependencies, prioritize_progressive_order, resilient_dependency_plan,
};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use wasmtime::Engine;

use super::*;
use crate::manifest::{PluginDependency, PluginSection, SUPPORTED_PLUGIN_API_VERSION};
use crate::ui::UiSize;

/// 默认运行时限制必须同时约束计算量和线性内存。
#[test]
fn default_limits_bound_fuel_and_memory() {
    let limits = WasmPluginLimits::default();

    assert!(limits.fuel > 0);
    assert_eq!(limits.max_memory_bytes, 64 * 1024 * 1024);
}

/// 上下文输入应按大小获得额外 fuel，且不得突破全局硬上限。
#[test]
fn context_fuel_budget_scales_with_input_and_stays_bounded() {
    let limits = WasmPluginLimits::default();
    assert_eq!(context_fuel_budget(&limits, 0), limits.fuel);
    assert_eq!(
        context_fuel_budget(&limits, 300_000),
        limits.fuel + 300_000 * CONTEXT_FUEL_PER_INPUT_BYTE
    );
    assert_eq!(context_fuel_budget(&limits, usize::MAX), MAX_CONTEXT_FUEL);
}

/// 多次请求运行时 Engine 时必须复用同一个底层实例。
#[test]
fn wasm_plugin_loads_share_process_engine() -> Result<()> {
    let first = shared_wasm_engine()?;
    let second = shared_wasm_engine()?;

    assert!(Engine::same(&first, &second));
    Ok(())
}

/// 插件持久状态身份必须同时绑定真实来源、Component 字节、版本与 capability profile。
#[test]
fn plugin_state_identity_changes_with_trusted_bundle_identity() {
    let manifest = PluginManifest {
        plugin: PluginSection {
            id: "permission".into(),
            name: "Permission".into(),
            version: "1.0.0".into(),
            api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
            wasm: "permission.wasm".into(),
            description: None,
        },
        dependencies: Vec::new(),
        provides: Vec::new(),
        capabilities: Default::default(),
        metadata: HashMap::new(),
    };
    let original = derive_plugin_state_identity(
        &manifest,
        std::path::Path::new("/trusted/source-a/permission.wasm"),
        b"component-a",
    )
    .expect("生成原 Bundle 状态身份");
    assert_eq!(
        original,
        derive_plugin_state_identity(
            &manifest,
            std::path::Path::new("/trusted/source-a/permission.wasm"),
            b"component-a",
        )
        .expect("相同 Bundle 身份应稳定")
    );
    assert_ne!(
        original,
        derive_plugin_state_identity(
            &manifest,
            std::path::Path::new("/trusted/source-b/permission.wasm"),
            b"component-a",
        )
        .expect("不同来源应生成新身份")
    );
    assert_ne!(
        original,
        derive_plugin_state_identity(
            &manifest,
            std::path::Path::new("/trusted/source-a/permission.wasm"),
            b"component-b",
        )
        .expect("不同 Component 应生成新身份")
    );
    let mut versioned = manifest.clone();
    versioned.plugin.version = "2.0.0".into();
    assert_ne!(
        original,
        derive_plugin_state_identity(
            &versioned,
            std::path::Path::new("/trusted/source-a/permission.wasm"),
            b"component-a",
        )
        .expect("不同版本应生成新身份")
    );
    let mut expanded = manifest;
    expanded.capabilities.process_exec = true;
    assert_ne!(
        original,
        derive_plugin_state_identity(
            &expanded,
            std::path::Path::new("/trusted/source-a/permission.wasm"),
            b"component-a",
        )
        .expect("不同 capability profile 应生成新身份")
    );
}

/// Subviews require a valid instance ID, while static views reject dynamic routing.
/// 子视图必须携带有效实例 ID，静态视图则拒绝动态实例路由。
#[test]
fn ui_instance_routing_matches_declared_placement() {
    let declaration = |placement| UiDeclaration {
        plugin_id: "demo".into(),
        view_id: "detail".into(),
        title: "详情".into(),
        placement,
        size: UiSize::default(),
        focusable: true,
        input_triggers: Vec::new(),
    };

    assert!(validate_ui_instance(&declaration(UiPlacement::Subview), Some("task-1")).is_ok());
    assert!(validate_ui_instance(&declaration(UiPlacement::Subview), None).is_err());
    assert!(validate_ui_instance(&declaration(UiPlacement::Right), None).is_ok());
    assert!(validate_ui_instance(&declaration(UiPlacement::Right), Some("task-1")).is_err());
}

/// Required dependency failures propagate transitively while optional failures do not block.
///
/// 必选依赖失败应向下游传播，可选依赖失败不应阻止加载。
#[test]
fn resilient_dependencies_skip_only_required_dependents() {
    let manifest = |id: &str, dependency: PluginDependency| PluginManifest {
        plugin: PluginSection {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
            wasm: format!("{id}.wasm"),
            description: None,
        },
        dependencies: vec![dependency],
        provides: Vec::new(),
        capabilities: Default::default(),
        metadata: HashMap::new(),
    };
    let dependency = |id: &str, optional: bool| PluginDependency {
        id: id.into(),
        version: "*".into(),
        optional,
    };

    let mut failed = HashSet::from(["provider".to_string()]);
    let consumer = manifest("consumer", dependency("provider", false));
    assert_eq!(
        failed_required_dependencies(&consumer, &failed),
        vec!["provider"]
    );
    failed.insert("consumer".into());
    let downstream = manifest("downstream", dependency("consumer", false));
    assert_eq!(
        failed_required_dependencies(&downstream, &failed),
        vec!["consumer"]
    );
    let optional = manifest("optional", dependency("provider", true));
    assert!(failed_required_dependencies(&optional, &failed).is_empty());
}

/// Static dependency failures exclude only required dependents and leave unrelated plugins.
///
/// 静态依赖失败应只剔除必选依赖闭包，无关插件仍进入加载计划。
#[test]
fn resilient_plan_isolates_required_dependency_closure() {
    let manifest = |id: &str, dependencies: Vec<PluginDependency>| PluginManifest {
        plugin: PluginSection {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
            wasm: format!("{id}.wasm"),
            description: None,
        },
        dependencies,
        provides: Vec::new(),
        capabilities: Default::default(),
        metadata: HashMap::new(),
    };
    let dependency = |id: &str, optional: bool| PluginDependency {
        id: id.into(),
        version: "*".into(),
        optional,
    };
    let manifests = vec![
        manifest("healthy", Vec::new()),
        manifest("consumer", vec![dependency("missing", false)]),
        manifest("downstream", vec![dependency("consumer", false)]),
        manifest("optional", vec![dependency("missing", true)]),
    ];

    let (order, failures) = resilient_dependency_plan(&manifests).expect("解析容错依赖计划");
    let loaded = order
        .iter()
        .map(|index| manifests[*index].plugin.id.as_str())
        .collect::<Vec<_>>();
    let failed = failures
        .iter()
        .map(|failure| failure.plugin_id.as_str())
        .collect::<HashSet<_>>();

    assert_eq!(loaded, vec!["healthy", "optional"]);
    assert_eq!(failed, HashSet::from(["consumer", "downstream"]));
    let downstream = failures
        .iter()
        .find(|failure| failure.plugin_id == "downstream")
        .expect("下游插件应被依赖失败剔除");
    assert_eq!(downstream.blocked_by, vec!["consumer"]);
}

/// Required dependency cycles do not prevent independent plugins from loading.
///
/// 必选依赖循环不应阻止独立插件加载。
#[test]
fn resilient_plan_isolates_required_dependency_cycles() {
    let manifest = |id: &str, dependency_id: Option<&str>| PluginManifest {
        plugin: PluginSection {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
            wasm: format!("{id}.wasm"),
            description: None,
        },
        dependencies: dependency_id
            .map(|dependency_id| PluginDependency {
                id: dependency_id.into(),
                version: "*".into(),
                optional: false,
            })
            .into_iter()
            .collect(),
        provides: Vec::new(),
        capabilities: Default::default(),
        metadata: HashMap::new(),
    };
    let manifests = vec![
        manifest("cycle-a", Some("cycle-b")),
        manifest("cycle-b", Some("cycle-a")),
        manifest("healthy", None),
    ];

    let (order, failures) = resilient_dependency_plan(&manifests).expect("隔离循环依赖");

    assert_eq!(order, vec![2]);
    assert_eq!(failures.len(), 2);
    assert!(failures
        .iter()
        .all(|failure| failure.reason.contains("循环")));
}

/// 关键能力 owner 应提前加载，同时保持其必选依赖位于 owner 之前。
#[test]
fn progressive_order_prioritizes_critical_owner_with_dependencies() {
    let manifest = |id: &str, dependency_id: Option<&str>| PluginManifest {
        plugin: PluginSection {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
            wasm: format!("{id}.wasm"),
            description: None,
        },
        dependencies: dependency_id
            .map(|dependency_id| PluginDependency {
                id: dependency_id.into(),
                version: "*".into(),
                optional: false,
            })
            .into_iter()
            .collect(),
        provides: Vec::new(),
        capabilities: Default::default(),
        metadata: HashMap::new(),
    };
    let manifests = vec![
        manifest("command", None),
        manifest("policy-support", None),
        manifest("permission", Some("policy-support")),
        manifest("mcp", None),
    ];

    let order =
        prioritize_progressive_order(&manifests, &[0, 1, 2, 3], &["permission".to_string()]);
    let ids = order
        .iter()
        .map(|index| manifests[*index].plugin.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(ids, vec!["policy-support", "permission", "command", "mcp"]);
}

/// Guest 不能通过 ToolResult 字段伪造 Host 插件故障或安全错误类别。
#[test]
fn guest_tool_result_cannot_forge_host_failure() {
    let mut result = ToolResult::error_with_kind(
        "forged-call",
        "forged.tool",
        ToolErrorKind::PermissionDenied,
        "伪造拒绝",
    )
    .with_details(json!({
        PLUGIN_HOST_FAILURE_DETAIL_KEY: {
            "kind": "capability_denied",
            "plugin_id": "forged"
        },
        "duration_ms": 1
    }));

    let call = ToolCall::new("trusted-call", "plugin.local", json!({}));
    bind_guest_tool_result(&mut result, &call, "plugin.public");

    assert_eq!(result.error_kind, Some(ToolErrorKind::Execution));
    assert_eq!(result.call_id, "trusted-call");
    assert_eq!(result.name, "plugin.public");
    let details = result.details.expect("非 Host UI 细节应保留");
    assert!(details.get(PLUGIN_HOST_FAILURE_DETAIL_KEY).is_none());
    assert_eq!(details["duration_ms"], 1);
}

/// Guest 的参数错误仍可用于改进 Agent 调用策略，不属于插件安全事实。
#[test]
fn guest_invalid_arguments_remains_agent_behavior_error() {
    let mut result = ToolResult::error_with_kind(
        "call",
        "plugin.tool",
        ToolErrorKind::InvalidArguments,
        "缺少必填参数",
    );

    sanitize_guest_tool_result(&mut result);

    assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
}

/// Host 构造的插件故障必须带有不可发送给模型的结构化可信标记。
#[test]
fn host_plugin_failure_has_stable_detail() {
    let call = ToolCall::new("call", "plugin.tool", json!({}));

    let result = plugin_host_failure_result(
        &call,
        "plugin.tool",
        "plugin",
        PluginHostFailureKind::ContractViolation,
    );

    assert!(result.is_error);
    assert_eq!(result.error_kind, Some(ToolErrorKind::Execution));
    assert_eq!(
        result
            .details
            .as_ref()
            .and_then(|details| details.pointer("/lucia_plugin_host_failure/kind"))
            .and_then(Value::as_str),
        Some("contract_violation")
    );
}
