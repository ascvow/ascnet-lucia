//! WASM Component 宿主导入函数与失败分类。

use crate::capability::encode_host_response;
use crate::capability::is_capability_denied;
use crate::capability::CapabilityState;
use crate::wasm::engine::IntoAnyhow;
use crate::wasm::engine::PluginWasiState;
use agent_tool::ToolCall;
use agent_tool::ToolErrorKind;
use agent_tool::ToolResult;
use anyhow::{Context, Result};
use serde_json::json;
use serde_json::Value;
use wasmtime::component::ComponentNamedList;
use wasmtime::component::Instance;
use wasmtime::component::Lift;
use wasmtime::component::Linker;
use wasmtime::component::Lower;
use wasmtime::component::TypedFunc;
use wasmtime::Store;
use wasmtime::StoreContextMut;
use wasmtime::Trap;
/// ToolResult.details 中只允许 Host 注入的插件故障键。
pub(crate) const PLUGIN_HOST_FAILURE_DETAIL_KEY: &str = "lucia_plugin_host_failure";

/// Host 从真实 WASM 执行边界观察到的插件故障类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginHostFailureKind {
    /// WASM 陷阱或其他执行崩溃。
    Trap,
    /// 单次调用耗尽 Host 分配的 fuel。
    FuelExhausted,
    /// 插件触发 Host 配置的线性内存上限。
    MemoryLimit,
    /// 插件调用了 Manifest 或 ExecutionPolicy 未授权的宿主能力。
    CapabilityDenied,
    /// Guest 返回值无法满足正式 ToolResult 契约。
    ContractViolation,
}

impl PluginHostFailureKind {
    /// 返回写入可信 ToolResult 细节的稳定协议名。
    const fn as_str(self) -> &'static str {
        match self {
            Self::Trap => "trap",
            Self::FuelExhausted => "fuel_exhausted",
            Self::MemoryLimit => "memory_limit",
            Self::CapabilityDenied => "capability_denied",
            Self::ContractViolation => "contract_violation",
        }
    }
}

/// 把协议无关的 Agent、文件和进程能力注册到 component 根命名空间。
pub(super) fn add_plugin_host_imports(linker: &mut Linker<PluginWasiState>) -> Result<()> {
    let mut root = linker.root();
    root.func_wrap(
        "host-agent-upsert-tool",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.upsert_tool(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-upsert-tool 失败")?;
    root.func_wrap(
        "host-agent-remove-tool",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_tool(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-remove-tool 失败")?;
    root.func_wrap(
        "host-agent-upsert-prompt",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.upsert_prompt(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-upsert-prompt 失败")?;
    root.func_wrap(
        "host-agent-remove-prompt",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_prompt(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-remove-prompt 失败")?;
    root.func_wrap(
        "host-agent-emit-event",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let result = caller.data_mut().capabilities.emit_event(&request);
            Ok((encode_host_response_with_failure(caller.data_mut(), result),))
        },
    )
    .into_anyhow()
    .context("注册 host-agent-emit-event 失败")?;
    root.func_wrap(
        "host-state-get",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.get_state(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-state-get 失败")?;
    root.func_wrap(
        "host-state-set",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.set_state(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-state-set 失败")?;
    root.func_wrap(
        "host-state-remove",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_state(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-state-remove 失败")?;
    for (name, handler) in [
        (
            "host-store-get",
            (|state: &mut PluginWasiState, request: &str| state.capabilities.store_get(request))
                as fn(&mut PluginWasiState, &str) -> anyhow::Result<serde_json::Value>,
        ),
        ("host-store-set", |state: &mut PluginWasiState, request| {
            state.capabilities.store_set(request)
        }),
        ("host-store-remove", |state: &mut PluginWasiState, request| {
            state.capabilities.store_remove(request)
        }),
        ("host-store-list", |state: &mut PluginWasiState, request| {
            state.capabilities.store_list(request)
        }),
    ] {
        root.func_wrap(
            name,
            move |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
                Ok((encode_host_response(handler(caller.data_mut(), &request)),))
            },
        )
        .into_anyhow()
        .with_context(|| format!("注册 {name} 失败"))?;
    }
    root.func_wrap(
        "host-service-upsert",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.upsert_service(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-service-upsert 失败")?;
    root.func_wrap(
        "host-service-remove",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.remove_service(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-service-remove 失败")?;
    root.func_wrap(
        "host-service-list",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Ok((encode_host_response(
                caller.data_mut().capabilities.list_services(&request),
            ),))
        },
    )
    .into_anyhow()
    .context("注册 host-service-list 失败")?;
    root.func_wrap_async(
        "host-service-call",
        |caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (caller_id, services) = caller.data().capabilities.service_context();
            Box::new(async move {
                let result =
                    CapabilityState::call_service_with(caller_id, services, &request).await;
                Ok((encode_host_response(result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-service-call 失败")?;
    root.func_wrap(
        "host-fs-read",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let result = caller.data_mut().capabilities.read_file(&request);
            Ok((encode_host_response_with_failure(caller.data_mut(), result),))
        },
    )
    .into_anyhow()
    .context("注册 host-fs-read 失败")?;
    root.func_wrap(
        "host-fs-list",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let result = caller.data_mut().capabilities.list_dir(&request);
            Ok((encode_host_response_with_failure(caller.data_mut(), result),))
        },
    )
    .into_anyhow()
    .context("注册 host-fs-list 失败")?;
    root.func_wrap(
        "host-process-spawn",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let result = caller.data_mut().capabilities.spawn_process(&request);
            Ok((encode_host_response_with_failure(caller.data_mut(), result),))
        },
    )
    .into_anyhow()
    .context("注册 host-process-spawn 失败")?;
    root.func_wrap_async(
        "host-process-write",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Box::new(async move {
                let result = caller.data_mut().capabilities.write_process(&request).await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-process-write 失败")?;
    root.func_wrap_async(
        "host-process-read-line",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Box::new(async move {
                let result = caller
                    .data_mut()
                    .capabilities
                    .read_process_line(&request)
                    .await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-process-read-line 失败")?;
    root.func_wrap_async(
        "host-process-kill",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            Box::new(async move {
                let result = caller.data_mut().capabilities.kill_process(&request).await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-process-kill 失败")?;
    root.func_wrap_async(
        "host-session-list",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (allowed, source) = caller.data().capabilities.session_context();
            Box::new(async move {
                let result = CapabilityState::list_sessions_with(allowed, source, &request).await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-session-list 失败")?;
    root.func_wrap_async(
        "host-session-read",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (allowed, source) = caller.data().capabilities.session_context();
            Box::new(async move {
                let result = CapabilityState::read_session_with(allowed, source, &request).await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-session-read 失败")?;
    root.func_wrap_async(
        "host-model-complete",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (allowed, binding) = caller.data().capabilities.model_completion_context();
            Box::new(async move {
                let result = CapabilityState::complete_model_with(allowed, binding, &request).await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-model-complete 失败")?;
    root.func_wrap_async(
        "host-agent-runtime-call",
        |mut caller: StoreContextMut<'_, PluginWasiState>, (request,): (String,)| {
            let (permissions, binding) = caller.data().capabilities.agent_runtime_context();
            Box::new(async move {
                let result =
                    CapabilityState::call_agent_runtime_with(permissions, binding, &request).await;
                Ok((encode_host_response_with_failure(caller.data_mut(), result),))
            })
        },
    )
    .into_anyhow()
    .context("注册 host-agent-runtime-call 失败")?;
    Ok(())
}

/// 编码宿主能力响应，并把真实权限拒绝锁存在当前 WASM Store 中供调用终态消费。
pub(crate) fn encode_host_response_with_failure(
    state: &mut PluginWasiState,
    result: Result<Value>,
) -> String {
    if result.as_ref().is_err_and(is_capability_denied) {
        state.record_plugin_failure(PluginHostFailureKind::CapabilityDenied);
    }
    encode_host_response(result)
}

/// 将 Host 观察到的真实 WASM 调用错误归入稳定插件故障类别。
pub(crate) fn classify_wasm_plugin_failure(error: &wasmtime::Error) -> PluginHostFailureKind {
    if matches!(error.downcast_ref::<Trap>(), Some(Trap::OutOfFuel)) {
        return PluginHostFailureKind::FuelExhausted;
    }
    let diagnostic = format!("{error:#?}").to_ascii_lowercase();
    if diagnostic.contains("forcing trap when growing memory")
        || diagnostic.contains("memory growth failure")
        || diagnostic.contains("memory limits")
    {
        return PluginHostFailureKind::MemoryLimit;
    }
    PluginHostFailureKind::Trap
}

/// 构造由 Host 注入、且会由 Core 重绑调用身份的插件故障结果。
pub(crate) fn plugin_host_failure_result(
    call: &ToolCall,
    public_name: &str,
    plugin_id: &str,
    failure: PluginHostFailureKind,
) -> ToolResult {
    ToolResult::error_with_kind(
        call.id.clone(),
        public_name,
        ToolErrorKind::Execution,
        format!("插件 `{plugin_id}` 运行失败：{}", failure.as_str()),
    )
    .with_details(json!({
        PLUGIN_HOST_FAILURE_DETAIL_KEY: {
            "kind": failure.as_str(),
            "plugin_id": plugin_id,
        }
    }))
}

pub(crate) fn get_required_func<Params, Results>(
    instance: &Instance,
    store: &mut Store<PluginWasiState>,
    name: &str,
) -> Result<TypedFunc<Params, Results>>
where
    Params: ComponentNamedList + Lower + Send + Sync + 'static,
    Results: ComponentNamedList + Lift + Send + Sync + 'static,
{
    instance
        .get_typed_func::<Params, Results>(store, name)
        .into_anyhow()
        .with_context(|| {
            format!("plugin missing export `{name}` or the export has incompatible type")
        })
}
