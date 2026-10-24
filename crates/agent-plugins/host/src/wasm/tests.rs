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

/// Guest 即使忽略未授权进程结果，真实 linker 仍记录 Denied 且不含参数。
#[tokio::test]
async fn component_capability_audit_is_host_owned() -> Result<()> {
    #[derive(Default)]
    struct Probe(std::sync::Mutex<Vec<crate::audit::HostCapabilityCallObservation>>);
    impl crate::audit::HostServiceCallObserver for Probe {
        fn observe(&self, _: crate::audit::HostServiceCallObservation) {}
        fn observe_capability(&self, event: crate::audit::HostCapabilityCallObservation) {
            self.0.lock().unwrap().push(event);
        }
    }
    let observer = Arc::new(Probe::default());
    let engine = shared_wasm_engine()?;
    let component = Component::new(&engine, r#"(component
        (import "host-process-spawn" (func $spawn (param "s" string) (result string)))
        (core module $mem
            (memory (export "memory") 1)
            (data (i32.const 0) "{}")
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
        (core instance $mem (instantiate $mem))
        (core func $spawn (canon lower (func $spawn) (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
        (core module $guest
            (import "h" "spawn" (func $spawn (param i32 i32 i32)))
            (func (export "run") i32.const 0 i32.const 2 i32.const 2048 call $spawn))
        (core instance $guest (instantiate $guest (with "h" (instance (export "spawn" (func $spawn))))))
        (func (export "run") (canon lift (core func $guest "run"))))"#).into_anyhow()?;
    let capabilities = CapabilityState::new(
        "trusted".into(),
        "audit".into(),
        PathBuf::from("."),
        Default::default(),
        Arc::new(ContributionRegistry::default()),
        Arc::new(ServiceRegistry::default()),
        CapabilityHostContext::default().with_call_observer(Some(observer.clone())),
    )?;
    let mut store = Store::new(
        &engine,
        PluginWasiState::new(
            WasiCtxBuilder::new().build(),
            capabilities,
            StoreLimitsBuilder::new().build(),
        ),
    );
    store.set_fuel(1_000_000).into_anyhow()?;
    let mut linker = Linker::new(&engine);
    add_plugin_host_imports(&mut linker)?;
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .into_anyhow()?;
    instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .into_anyhow()?
        .call_async(&mut store, ())
        .await
        .into_anyhow()?;
    let events = observer.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].plugin_id, "trusted");
    assert_eq!(events[0].operation, "host-process-spawn");
    assert_eq!(
        events[0].result,
        crate::audit::HostCapabilityCallResult::Denied
    );
    Ok(())
}

/// 真实 Component 在首次导出期间发起服务调用，必须走 Guest ACL 而非应用入口。
#[tokio::test]
async fn component_import_enforces_initialization_service_acl() -> Result<()> {
    struct Echo;
    #[async_trait]
    impl ServiceHandler for Echo {
        async fn handle(&self, _call: PluginServiceCall) -> Result<Value> {
            Ok(json!("allowed"))
        }
    }
    let services = Arc::new(ServiceRegistry::default());
    services.register_handler("provider", HashSet::new(), Arc::new(Echo))?;
    services.upsert(
        "provider",
        PluginService {
            plugin_id: String::new(),
            name: "echo".into(),
            version: "1.0.0".into(),
            description: None,
        },
    )?;
    let request = r#"{"plugin_id":"provider","name":"echo","payload":null}"#;
    let encoded = request
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let engine = shared_wasm_engine()?;
    let component = Component::new(&engine, format!(r#"
        (component
          (import "host-service-call" (func $service (param "request" string) (result string)))
          (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 0) "{encoded}")
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
          (core instance $mem (instantiate $memory))
          (core func $call (canon lower (func $service) (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
          (core module $guest
            (import "host" "call" (func $call (param i32 i32 i32)))
            (func (export "run") (result i32)
              i32.const 0 i32.const {length} i32.const 2048 call $call i32.const 2048))
          (core instance $guest (instantiate $guest (with "host" (instance (export "call" (func $call))))))
          (func (export "run") (result string) (canon lift (core func $guest "run") (memory (core memory $mem "memory")))))
    "#, length = request.len())).into_anyhow()?;
    for authorized in [false, true] {
        let dependencies = if authorized {
            HashSet::from(["provider".into()])
        } else {
            HashSet::new()
        };
        let _registration = services.begin_plugin_load("loading-guest", dependencies)?;
        let capabilities = CapabilityState::new(
            "loading-guest".into(),
            "fixture".into(),
            std::path::PathBuf::from("."),
            Default::default(),
            Arc::new(ContributionRegistry::default()),
            services.clone(),
            CapabilityHostContext::new(
                None,
                None,
                agent_tool::ExecutionPolicy::serve(),
                None,
                None,
                None,
                None,
            ),
        )?;
        let mut store = Store::new(
            &engine,
            PluginWasiState::new(
                WasiCtxBuilder::new().build(),
                capabilities,
                StoreLimitsBuilder::new().build(),
            ),
        );
        store.set_fuel(1_000_000).into_anyhow()?;
        let mut linker = Linker::new(&engine);
        add_plugin_host_imports(&mut linker)?;
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .into_anyhow()?;
        let run = instance
            .get_typed_func::<(), (String,)>(&mut store, "run")
            .into_anyhow()?;
        let (response,) = run.call_async(&mut store, ()).await.into_anyhow()?;
        let response: Value = serde_json::from_str(&response)?;
        assert_eq!(response["ok"], authorized, "{response}");
    }
    Ok(())
}

/// 真实异步 Host import 超时后销毁 Store，不能把半执行实例放回共享槽。
#[tokio::test]
async fn component_deadline_retires_pending_store() -> Result<()> {
    let engine = shared_wasm_engine()?;
    let component = Component::new(&engine, r#"
      (component
        (import "wait" (func $wait))
        (core func $wait (canon lower (func $wait)))
        (core module $m
          (import "h" "wait" (func $wait))
          (memory (export "memory") 1)
          (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 16)
          (func (export "run") call $wait)
          (func (export "json") (param i32 i32) (result i32) i32.const 0)
          (func (export "empty") (result i32) i32.const 0)
          (func (export "input") (param i32 i32)))
        (core instance $i (instantiate $m (with "h" (instance (export "wait" (func $wait))))))
        (func (export "run") (canon lift (core func $i "run")))
        (func (export "json") (param "s" string) (result string) (canon lift (core func $i "json") (memory (core memory $i "memory")) (realloc (core func $i "realloc"))))
        (func (export "empty") (result string) (canon lift (core func $i "empty") (memory (core memory $i "memory"))))
        (func (export "input") (param "s" string) (canon lift (core func $i "input") (memory (core memory $i "memory")) (realloc (core func $i "realloc")))))
    "#).into_anyhow()?;
    let services = Arc::new(ServiceRegistry::default());
    let capabilities = CapabilityState::new(
        "deadline".into(),
        "test".into(),
        PathBuf::from("."),
        Default::default(),
        Arc::new(ContributionRegistry::default()),
        services,
        CapabilityHostContext::new(
            None,
            None,
            agent_tool::ExecutionPolicy::serve(),
            None,
            None,
            None,
            None,
        ),
    )?;
    let mut store = Store::new(
        &engine,
        PluginWasiState::new(
            WasiCtxBuilder::new().build(),
            capabilities,
            StoreLimitsBuilder::new().build(),
        ),
    );
    store.set_fuel(1_000_000).into_anyhow()?;
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap_async(
            "wait",
            |_caller: wasmtime::StoreContextMut<'_, PluginWasiState>, (): ()| {
                Box::new(async {
                    std::future::pending::<()>().await;
                    Ok(())
                })
            },
        )
        .into_anyhow()?;
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .into_anyhow()?;
    let json = instance
        .get_typed_func::<(String,), (String,)>(&mut store, "json")
        .into_anyhow()?;
    let empty = instance
        .get_typed_func::<(), (String,)>(&mut store, "empty")
        .into_anyhow()?;
    let input = instance
        .get_typed_func::<(String,), ()>(&mut store, "input")
        .into_anyhow()?;
    let state = Arc::new(Mutex::new(Some(WasmPluginState {
        store,
        instance,
        call_tool: json,
        on_event: json,
        load_context: json,
        handle_service: json,
        deactivate: empty,
        render_ui: json,
        on_ui_input: input,
        limits: WasmPluginLimits::default(),
    })));
    let budget = ComponentCallBudget {
        wait: Duration::from_millis(20),
        execute: Duration::from_millis(40),
        retire: false,
    };
    let lock = state.lock().await;
    assert!(
        complete_component_call(&state, "queue".into(), budget, |_| Box::pin(async {
            Ok(())
        }))
        .await
        .is_err()
    );
    assert!(lock.is_some(), "排队超时不能销毁健康实例");
    drop(lock);
    let caller_state = state.clone();
    let caller = tokio::spawn(async move {
        COMPONENT_DEADLINE
            .scope(
                tokio::time::Instant::now() + Duration::from_millis(40),
                complete_component_call(
                    &caller_state,
                    "pending".into(),
                    WORK_CALL_BUDGET,
                    |state| {
                        Box::pin(async move {
                            let run = state
                                .instance
                                .get_typed_func::<(), ()>(&mut state.store, "run")
                                .into_anyhow()?;
                            run.call_async(&mut state.store, ()).await.into_anyhow()
                        })
                    },
                ),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    caller.abort();
    let _ = caller.await;
    let lock = tokio::time::timeout(Duration::from_secs(2), state.lock()).await?;
    assert!(lock.is_none(), "调用者取消后 deadline 仍须完成隔离");
    drop(lock);
    assert!(
        complete_component_call(&state, "retry".into(), budget, |_| Box::pin(async {
            Ok(())
        }))
        .await
        .unwrap_err()
        .to_string()
        .contains("隔离")
    );
    Ok(())
}

/// 多线性内存 component 不能把单内存上限倍增为无界总量。
#[test]
fn store_rejects_excess_memory_count() -> Result<()> {
    let engine = shared_wasm_engine()?;
    let component = Component::new(
        &engine,
        r#"(component
        (core module $m (memory 1) (memory 1) (memory 1) (memory 1) (memory 1))
        (core instance $i (instantiate $m)))"#,
    )
    .into_anyhow()?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new()
            .memory_size(64 * 1024 * 1024)
            .memories(4)
            .build(),
    );
    store.limiter(|limits| limits);
    store.set_fuel(1_000_000).into_anyhow()?;
    let linker = Linker::new(&engine);
    assert!(linker.instantiate(&mut store, &component).is_err());
    Ok(())
}

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
        events: Vec::new(),
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
        events: Vec::new(),
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
        events: Vec::new(),
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
        events: Vec::new(),
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
        events: Vec::new(),
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
