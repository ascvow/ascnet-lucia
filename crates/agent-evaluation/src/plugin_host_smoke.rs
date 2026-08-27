//! M8 插件真实 WASM Host 激活、声明复核与审计证据绑定。

use crate::plugin_host_audit::{
    bind_plugin_host_audit, PluginHostAuditBinding, PluginHostAuditBindingError,
    TrustedHostCheckOutcome,
};
use agent_core::model::{MessageRole, ModelMessage};
use agent_evolution_protocol::{ArtifactDigest, PluginHostAuditEvidence};
use agent_plugin_host::audit::{
    audit_plugin_component, InMemoryHostServiceCallObserver, PluginAuditEvidence,
};
use agent_plugin_host::manifest::{
    PluginManifest, ProvidedCapabilityMode, ResolvedPluginCapabilities,
};
use agent_plugin_host::wasm::{WasmPluginHost, WasmPluginLimits};
use agent_plugin_host::{
    AgentExtension, PluginHost, PluginHostServices, PluginService, ToolRendererContribution,
    UiDeclaration,
};
use agent_tool::{ExecutionPolicy, ToolSpec};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Evaluation 平面允许单个插件使用的最高 fuel。
const MAX_EVALUATION_FUEL: u64 = 50_000_000;
/// Evaluation 平面允许单个插件使用的最高线性内存字节数。
const MAX_EVALUATION_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// 真实 Host smoke 固定执行的声明类别与身份边界检查数。
const BASE_DECLARATION_CHECK_COUNT: usize = 9;
/// 资源限额、Evaluation Profile 与卸载生命周期的固定检查数。
const RUNTIME_CHECK_COUNT: u32 = 9;

/// M8 真实 Host smoke 的不可变输入。
///
/// manifest 与 Component 必须是同一已构建 Bundle 中的真实文件；`binding` 提供 Candidate
/// 身份、期望接口和能力 Profile。调用方传入的 `binding.host_smoke` 与
/// `binding.runtime_audit` 不被信任，执行器会用本次真实运行结果覆盖它们。
#[derive(Debug)]
pub struct PluginHostSmokeInput<'a> {
    /// 已通过构建平面验证的 `plugin.toml` 路径。
    pub manifest_path: &'a Path,
    /// 已通过构建平面验证、且必须与 manifest `plugin.wasm` 指向同一文件的 Component 路径。
    pub component_path: &'a Path,
    /// Evaluation 策略唯一允许访问的 Fixture 根目录。
    pub fixture_root: &'a Path,
    /// Host 对当前插件集合解析出的真实能力 owner。
    pub resolved_capabilities: &'a ResolvedPluginCapabilities,
    /// 单插件 fuel、协作 yield 与线性内存上限。
    pub limits: WasmPluginLimits,
    /// 当前 M8 Candidate 的受信审计绑定。
    pub binding: PluginHostAuditBinding,
}

/// 从真实 Host 读取并通过 owner、重复项和 manifest 边界复核的声明快照。
#[derive(Debug, Clone, Serialize)]
pub struct PluginHostDeclarationSnapshot {
    /// Host 注入的可信插件 ID。
    pub plugin_id: String,
    /// activation 后动态注册的 developer prompt 消息。
    pub prompt_messages: Vec<ModelMessage>,
    /// activation 后插件实际公开的工具声明。
    pub tools: Vec<ToolSpec>,
    /// `describe-ui` 返回且由 Host 注入 owner 的 UI 声明。
    pub ui_declarations: Vec<UiDeclaration>,
    /// `describe-ui` 返回且绑定自有工具的 renderer 声明。
    pub tool_renderers: Vec<ToolRendererContribution>,
    /// activation 后插件实际注册的版本化服务声明。
    pub services: Vec<PluginService>,
}

/// M8 真实 Host smoke 的声明快照与最终协议证据。
#[derive(Debug)]
pub struct PluginHostSmokeOutput {
    /// 从已激活 Host 读取的完整声明快照。
    pub declarations: PluginHostDeclarationSnapshot,
    /// 完整声明快照的规范 SHA-256 摘要。
    pub declaration_report_digest: ArtifactDigest,
    /// 由真实 Component/Host 审计证据经受信绑定生成的 M8 证据。
    pub host_audit: PluginHostAuditEvidence,
}

/// M8 真实 Host smoke 的失败阶段。
#[derive(Debug, thiserror::Error)]
pub enum PluginHostSmokeError {
    /// manifest 读取、资源限额检查或真实 WASM activation 失败。
    #[error("M8 插件 Host activation 失败：{source}")]
    Activation {
        /// activation 阶段的底层错误。
        #[source]
        source: anyhow::Error,
    },
    /// activation 成功后读取或复核声明失败。
    #[error("M8 插件 Host 声明复核失败：{source}")]
    Declaration {
        /// 声明阶段的底层错误。
        #[source]
        source: anyhow::Error,
        /// 声明失败后尽力卸载时产生的附加错误。
        shutdown_error: Option<String>,
    },
    /// Component 扫描、真实文件绑定或 M8 协议适配失败。
    #[error("M8 插件 Host 审计失败：{source}")]
    Audit {
        /// 审计阶段的底层错误。
        #[source]
        source: anyhow::Error,
    },
    /// 声明复核完成后的 Guest 卸载或 Host 资源撤销失败。
    #[error("M8 插件 Host shutdown 失败：{source}")]
    Shutdown {
        /// shutdown 阶段的底层错误。
        #[source]
        source: anyhow::Error,
    },
}

impl PluginHostSmokeError {
    /// 返回主阶段失败后尽力 shutdown 产生的附加错误。
    ///
    /// 仅声明阶段可能同时保留主错误和 shutdown 错误；其他阶段返回 `None`。
    pub fn secondary_shutdown_error(&self) -> Option<&str> {
        match self {
            Self::Declaration { shutdown_error, .. } => shutdown_error.as_deref(),
            _ => None,
        }
    }
}

/// 使用真实 `WasmPluginHost` 执行 M8 activation、声明复核、shutdown 与证据绑定。
///
/// Host 服务先被 `ExecutionPolicy::evaluation` 收窄，且不会注入网络、Secret、原生进程、
/// Agent Runtime 或模型完成服务。函数会在 activation 成功后的所有失败路径尽力调用
/// [`PluginHost::shutdown`]；成功返回前也必须完成 shutdown。
///
/// # Errors
///
/// manifest/activation、声明读取与复核、Component 审计/绑定、shutdown 分别返回
/// [`PluginHostSmokeError`] 的对应变体。声明失败且 shutdown 也失败时，主错误保留在
/// `Declaration`，附加 shutdown 错误可通过
/// [`PluginHostSmokeError::secondary_shutdown_error`] 获取。
pub async fn run_plugin_host_smoke(
    mut input: PluginHostSmokeInput<'_>,
) -> std::result::Result<PluginHostSmokeOutput, PluginHostSmokeError> {
    let prepared =
        prepare_input(&input).map_err(|source| PluginHostSmokeError::Activation { source })?;
    verify_binding(&prepared, &input.binding)
        .map_err(|source| PluginHostSmokeError::Audit { source })?;

    let observer = Arc::new(InMemoryHostServiceCallObserver::default());
    let evaluation_policy = ExecutionPolicy::evaluation(prepared.fixture_root.clone());
    let host_services = PluginHostServices::new()
        .restrict_execution_policy(&evaluation_policy)
        .with_service_call_observer(observer.clone());
    let host = WasmPluginHost::load_with_limits_and_services(
        prepared.manifest.clone(),
        &prepared.component_path,
        input.limits.clone(),
        host_services,
    )
    .await
    .map_err(|source| PluginHostSmokeError::Activation { source })?;

    let declarations = match read_and_validate_declarations(
        &host,
        &prepared.manifest,
        input.resolved_capabilities,
    )
    .await
    {
        Ok(declarations) => declarations,
        Err(source) => {
            let shutdown_error = shutdown_error(&host).await;
            return Err(PluginHostSmokeError::Declaration {
                source,
                shutdown_error,
            });
        }
    };
    let declaration_report_digest =
        match canonical_digest("ascnet.lucia.m8.plugin-host-declarations.v1", &declarations) {
            Ok(digest) => digest,
            Err(source) => {
                let shutdown_error = shutdown_error(&host).await;
                return Err(PluginHostSmokeError::Declaration {
                    source,
                    shutdown_error,
                });
            }
        };

    PluginHost::shutdown(&host)
        .await
        .map_err(|source| PluginHostSmokeError::Shutdown { source })?;

    let host_evidence = audit_plugin_component(
        &prepared.manifest,
        &prepared.component_path,
        input.resolved_capabilities,
        observer.snapshot(),
    )
    .map_err(|source| PluginHostSmokeError::Audit { source })?;
    validate_audit_identity(&host_evidence, &prepared.manifest)
        .map_err(|source| PluginHostSmokeError::Audit { source })?;

    let declaration_check_count = declaration_check_count(&declarations)
        .map_err(|source| PluginHostSmokeError::Audit { source })?;
    input.binding.host_smoke = TrustedHostCheckOutcome {
        report_digest: declaration_report_digest.clone(),
        check_count: declaration_check_count,
        failure_count: 0,
    };
    input.binding.runtime_audit = TrustedHostCheckOutcome {
        report_digest: runtime_report_digest(&prepared, &input.limits)
            .map_err(|source| PluginHostSmokeError::Audit { source })?,
        check_count: RUNTIME_CHECK_COUNT,
        failure_count: 0,
    };
    let host_audit = bind_plugin_host_audit(&host_evidence, input.binding)
        .map_err(binding_error)
        .map_err(|source| PluginHostSmokeError::Audit { source })?;

    Ok(PluginHostSmokeOutput {
        declarations,
        declaration_report_digest,
        host_audit,
    })
}

/// 已规范化并完成 manifest/Component 边界检查的内部输入。
struct PreparedPluginHostSmoke {
    manifest: PluginManifest,
    manifest_path: PathBuf,
    manifest_digest: ArtifactDigest,
    component_path: PathBuf,
    component_digest: ArtifactDigest,
    fixture_root: PathBuf,
}

/// 读取 manifest、规范路径并验证显式 Evaluation 资源上限。
fn prepare_input(input: &PluginHostSmokeInput<'_>) -> Result<PreparedPluginHostSmoke> {
    validate_limits(&input.limits)?;
    let manifest_path = canonical_file(input.manifest_path, "插件 manifest")?;
    let component_path = canonical_file(input.component_path, "插件 Component")?;
    let fixture_root = fs::canonicalize(input.fixture_root).with_context(|| {
        format!(
            "规范化 Evaluation Fixture 根目录失败：{}",
            input.fixture_root.display()
        )
    })?;
    ensure!(fixture_root.is_dir(), "Evaluation Fixture 根路径不是目录");

    let manifest = PluginManifest::load(&manifest_path)?;
    validate_manifest_component_path(&manifest, &manifest_path, &component_path)?;
    let manifest_digest = digest_file(&manifest_path)?;
    let component_digest = digest_file(&component_path)?;
    Ok(PreparedPluginHostSmoke {
        manifest,
        manifest_path,
        manifest_digest,
        component_path,
        component_digest,
        fixture_root,
    })
}

/// 确认 M8 binding 与真实 manifest/Component 字节及插件身份一致。
fn verify_binding(
    prepared: &PreparedPluginHostSmoke,
    binding: &PluginHostAuditBinding,
) -> Result<()> {
    ensure!(
        binding.plugin_id == prepared.manifest.plugin.id,
        "M8 binding 插件 ID 与 manifest 不一致"
    );
    ensure!(
        binding.manifest_digest == prepared.manifest_digest,
        "M8 binding manifest 摘要与真实文件不一致：{}",
        prepared.manifest_path.display()
    );
    ensure!(
        binding.component_digest == prepared.component_digest,
        "M8 binding Component 摘要与真实文件不一致：{}",
        prepared.component_path.display()
    );
    ensure!(
        binding.expected_interface.plugin_id == binding.plugin_id
            && binding.expected_interface.component_digest == binding.component_digest,
        "M8 binding 期望接口未绑定当前插件或 Component"
    );
    Ok(())
}

/// 验证 manifest `plugin.wasm` 只能指向同一 Bundle 内的显式 Component。
fn validate_manifest_component_path(
    manifest: &PluginManifest,
    manifest_path: &Path,
    component_path: &Path,
) -> Result<()> {
    let declared = Path::new(&manifest.plugin.wasm);
    ensure!(
        !declared.is_absolute()
            && declared
                .components()
                .all(|component| { matches!(component, Component::Normal(_) | Component::CurDir) }),
        "manifest plugin.wasm 必须是 Bundle 内不含父级跳转的相对路径"
    );
    let bundle_root = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest 路径没有父目录"))?;
    let declared_component = fs::canonicalize(bundle_root.join(declared)).with_context(|| {
        format!(
            "规范化 manifest 声明的 Component 失败：{}",
            bundle_root.join(declared).display()
        )
    })?;
    ensure!(
        declared_component == component_path,
        "显式 Component 路径与 manifest plugin.wasm 不一致"
    );
    ensure!(
        declared_component.starts_with(bundle_root),
        "manifest Component 逃逸 Bundle 根目录"
    );
    Ok(())
}

/// 验证 smoke 使用非零且不超过 Evaluation 硬上限的显式资源限制。
fn validate_limits(limits: &WasmPluginLimits) -> Result<()> {
    ensure!(
        (1..=MAX_EVALUATION_FUEL).contains(&limits.fuel),
        "Evaluation WASM fuel 必须位于 1..={MAX_EVALUATION_FUEL}"
    );
    ensure!(
        matches!(limits.fuel_yield_interval, Some(interval) if interval > 0 && interval <= limits.fuel),
        "Evaluation WASM 必须配置不超过 fuel 的非零协作 yield 间隔"
    );
    ensure!(
        (1..=MAX_EVALUATION_MEMORY_BYTES).contains(&limits.max_memory_bytes),
        "Evaluation WASM 线性内存上限必须位于 1..={MAX_EVALUATION_MEMORY_BYTES}"
    );
    Ok(())
}

/// 从已激活 Host 读取五类声明并复核 owner、重复项及跨声明绑定。
async fn read_and_validate_declarations(
    host: &WasmPluginHost,
    manifest: &PluginManifest,
    resolved: &ResolvedPluginCapabilities,
) -> Result<PluginHostDeclarationSnapshot> {
    ensure!(
        host.id() == manifest.plugin.id,
        "Host 身份与 manifest 不一致"
    );
    let prompt_messages = host.prompt_messages().await?;
    let tools = host.list_tools().await?;
    let ui_declarations = host.ui_declarations().await?;
    let tool_renderers = host.tool_renderers().await?;
    let services = host.services().await?;
    validate_prompt_messages(&prompt_messages)?;
    validate_tools(&tools)?;
    validate_ui_and_renderers(
        &manifest.plugin.id,
        &tools,
        &ui_declarations,
        &tool_renderers,
    )?;
    validate_services(&manifest.plugin.id, &services)?;
    validate_capability_owners(manifest, resolved)?;
    Ok(PluginHostDeclarationSnapshot {
        plugin_id: manifest.plugin.id.clone(),
        prompt_messages,
        tools,
        ui_declarations,
        tool_renderers,
        services,
    })
}

/// 确认 Host 只返回非空 developer prompt，避免错误角色越过上下文边界。
fn validate_prompt_messages(messages: &[ModelMessage]) -> Result<()> {
    for message in messages {
        ensure!(
            message.role == MessageRole::Developer,
            "插件 prompt 必须由 Host 收窄为 developer 角色"
        );
        ensure!(
            !message.text_content().trim().is_empty(),
            "插件 prompt 内容不能为空"
        );
    }
    Ok(())
}

/// 校验工具名有效且在当前 Host 快照内不重复。
fn validate_tools(tools: &[ToolSpec]) -> Result<()> {
    let mut names = HashSet::new();
    for tool in tools {
        tool.validate_name()?;
        ensure!(
            names.insert(tool.name.as_str()),
            "插件工具声明重复：{}",
            tool.name
        );
    }
    Ok(())
}

/// 校验 UI/renderer owner、路由唯一性及 renderer 的工具所有权。
fn validate_ui_and_renderers(
    plugin_id: &str,
    tools: &[ToolSpec],
    declarations: &[UiDeclaration],
    renderers: &[ToolRendererContribution],
) -> Result<()> {
    let owned_tools = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<HashSet<_>>();
    let mut route_ids = HashSet::new();
    for declaration in declarations {
        ensure!(
            declaration.plugin_id == plugin_id,
            "UI 声明 owner 未由 Host 绑定到当前插件"
        );
        ensure!(
            !declaration.view_id.trim().is_empty(),
            "UI 声明 view_id 不能为空"
        );
        ensure!(
            route_ids.insert(declaration.view_id.as_str()),
            "插件 UI 路由重复：{}",
            declaration.view_id
        );
    }
    for renderer in renderers {
        ensure!(
            renderer.plugin_id == plugin_id,
            "工具 renderer owner 未由 Host 绑定到当前插件"
        );
        ensure!(
            !renderer.renderer_id.trim().is_empty(),
            "工具 renderer ID 不能为空"
        );
        ensure!(
            route_ids.insert(renderer.renderer_id.as_str()),
            "插件 UI/renderer 路由重复：{}",
            renderer.renderer_id
        );
        ensure!(
            owned_tools.contains(renderer.tool_name.as_str()),
            "工具 renderer 引用了非当前插件拥有的工具：{}",
            renderer.tool_name
        );
    }
    Ok(())
}

/// 校验服务 owner、名称和单插件服务路由唯一性。
fn validate_services(plugin_id: &str, services: &[PluginService]) -> Result<()> {
    let mut names = HashSet::new();
    for service in services {
        ensure!(
            service.plugin_id == plugin_id,
            "服务声明 owner 未由 Host 绑定到当前插件"
        );
        ensure!(!service.name.trim().is_empty(), "插件服务名不能为空");
        ensure!(
            !service.version.trim().is_empty(),
            "插件服务协议版本不能为空"
        );
        ensure!(
            names.insert(service.name.as_str()),
            "插件服务声明重复：{}",
            service.name
        );
    }
    Ok(())
}

/// 确认 manifest 的每项提供能力均由 Host 解析为当前插件所有。
fn validate_capability_owners(
    manifest: &PluginManifest,
    resolved: &ResolvedPluginCapabilities,
) -> Result<()> {
    for provided in &manifest.provides {
        let Some((_, mode, owners)) = resolved
            .resolved_owners()
            .find(|(capability_id, _, _)| *capability_id == provided.id)
        else {
            bail!("manifest 提供能力没有 Host owner：{}", provided.id);
        };
        ensure!(
            mode == provided.mode,
            "能力 owner 模式与 manifest 不一致：{}",
            provided.id
        );
        ensure!(
            owners.iter().any(|owner| owner == &manifest.plugin.id),
            "manifest 提供能力未解析到当前插件 owner：{}",
            provided.id
        );
        if provided.mode == ProvidedCapabilityMode::Exclusive {
            ensure!(
                owners.len() == 1 && owners[0] == manifest.plugin.id,
                "独占能力没有唯一绑定当前插件：{}",
                provided.id
            );
        }
    }
    Ok(())
}

/// 确认真实 Host 审计证据仍绑定刚刚激活的 manifest 身份。
fn validate_audit_identity(
    evidence: &PluginAuditEvidence,
    manifest: &PluginManifest,
) -> Result<()> {
    ensure!(
        evidence.manifest.plugin_id == manifest.plugin.id,
        "Host Component 审计身份与已激活插件不一致"
    );
    Ok(())
}

/// 计算声明报告实际覆盖的非零检查数，并拒绝整数溢出。
fn declaration_check_count(snapshot: &PluginHostDeclarationSnapshot) -> Result<u32> {
    let count = BASE_DECLARATION_CHECK_COUNT
        .checked_add(snapshot.prompt_messages.len())
        .and_then(|count| count.checked_add(snapshot.tools.len()))
        .and_then(|count| count.checked_add(snapshot.ui_declarations.len()))
        .and_then(|count| count.checked_add(snapshot.tool_renderers.len()))
        .and_then(|count| count.checked_add(snapshot.services.len()))
        .ok_or_else(|| anyhow!("Host 声明检查数溢出"))?;
    u32::try_from(count).context("Host 声明检查数超过 u32")
}

/// 生成 Evaluation 策略、资源限制和成功 shutdown 的不可变运行报告摘要。
fn runtime_report_digest(
    prepared: &PreparedPluginHostSmoke,
    limits: &WasmPluginLimits,
) -> Result<ArtifactDigest> {
    #[derive(Serialize)]
    struct RuntimeReport<'a> {
        profile: &'static str,
        fixture_root: &'a Path,
        allow_network: bool,
        allow_secrets: bool,
        allow_process: bool,
        fuel: u64,
        fuel_yield_interval: Option<u64>,
        max_memory_bytes: usize,
        shutdown_completed: bool,
    }

    canonical_digest(
        "ascnet.lucia.m8.plugin-host-runtime.v1",
        &RuntimeReport {
            profile: "evaluation",
            fixture_root: &prepared.fixture_root,
            allow_network: false,
            allow_secrets: false,
            allow_process: false,
            fuel: limits.fuel,
            fuel_yield_interval: limits.fuel_yield_interval,
            max_memory_bytes: limits.max_memory_bytes,
            shutdown_completed: true,
        },
    )
}

/// 规范化一个必须存在的普通文件路径。
fn canonical_file(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("规范化{label}路径失败：{}", path.display()))?;
    ensure!(canonical.is_file(), "{label}路径不是普通文件");
    Ok(canonical)
}

/// 读取真实文件并计算 M8 使用的 SHA-256 制品摘要。
fn digest_file(path: &Path) -> Result<ArtifactDigest> {
    let bytes = fs::read(path).with_context(|| format!("读取制品失败：{}", path.display()))?;
    digest_bytes(&bytes)
}

/// 计算字节内容的 SHA-256 制品摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| anyhow!("生成制品摘要失败：{error}"))
}

/// 为可序列化报告附加稳定域并计算规范摘要。
fn canonical_digest<T: Serialize>(domain: &'static str, value: &T) -> Result<ArtifactDigest> {
    let bytes = serde_json::to_vec(&(domain, value)).context("序列化 Host smoke 报告失败")?;
    digest_bytes(&bytes)
}

/// 在主阶段失败后尽力 shutdown，并只返回附加错误文本。
async fn shutdown_error(host: &WasmPluginHost) -> Option<String> {
    PluginHost::shutdown(host)
        .await
        .err()
        .map(|error| format!("{error:#}"))
}

/// 把强类型 M8 binding 错误保留为审计阶段错误源。
fn binding_error(error: PluginHostAuditBindingError) -> anyhow::Error {
    anyhow!(error)
}
