//! 插件文件、状态、服务与进程能力的宿主实现。

use crate::capability::agent::AgentCapabilityChecks;
use crate::capability::process::ManagedProcess;
use crate::capability::requests::parse_request;
use crate::capability::requests::resolve_from;
use crate::capability::requests::{
    validate_state_key, StoreKeyRequest, StoreScopeRequest, StoreSetRequest,
};
use crate::capability::requests::ExtensionEventRequest;
use crate::capability::requests::FileEntry;
use crate::capability::requests::IdRequest;
use crate::capability::requests::KeyRequest;
use crate::capability::requests::NameRequest;
use crate::capability::requests::PathRequest;
use crate::capability::requests::ServiceCallRequest;
use crate::capability::requests::ServiceListRequest;
use crate::capability::requests::ServiceSpecRequest;
use crate::capability::requests::StateSetRequest;
use crate::contribution::ContributionRegistry;
use crate::contribution::PromptContribution;
use crate::contribution::ToolRegistrationRequest;
use crate::manifest::AgentCapabilitySection;
use crate::manifest::CapabilitySection;
use crate::service::PluginService;
use crate::service::PluginServiceCall;
use crate::service::ServiceRegistry;
use crate::services::ModelCompletionHostServices;
use crate::state::PluginStateStore;
use agent_plugin_protocol::PluginStoreScope;
use crate::ui::UiHostActionRequest;
pub(crate) use agent::AgentRuntimeBinding;
use agent_tool::ExecutionPolicy;
use anyhow::anyhow;
use anyhow::{Context, Result};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
mod agent;
mod process;
mod requests;
#[cfg(test)]
mod tests;

const MAX_PLUGIN_PROCESSES: usize = 16;

const MAX_PROCESS_LINE_BYTES: usize = 4 * 1024 * 1024;

const MAX_PROCESS_WRITE_BYTES: usize = 4 * 1024 * 1024;

const MAX_PROCESS_COMMAND_BYTES: usize = 4 * 1024;

const MAX_PROCESS_ARGUMENTS: usize = 256;

const MAX_PROCESS_ARGUMENT_BYTES: usize = 64 * 1024;

const MAX_PROCESS_ENV_ENTRIES: usize = 128;

const MAX_PROCESS_ENV_BYTES: usize = 256 * 1024;

const MAX_PROCESS_CWD_BYTES: usize = 4 * 1024;

const MAX_HOST_CAPABILITY_REQUEST_BYTES: usize = 8 * 1024 * 1024;

const DEFAULT_READ_TIMEOUT_MS: u64 = 30_000;

const MAX_READ_TIMEOUT_MS: u64 = 120_000;

const MAX_AGENT_RUNTIME_REQUEST_BYTES: usize = 1024 * 1024;

const MAX_MODEL_COMPLETION_REQUEST_BYTES: usize = 8 * 1024 * 1024;

const HOST_RESPONSE_SCHEMA_VERSION: u32 = 1;

/// 单个插件实例可访问的受控宿主能力状态。
pub(crate) struct CapabilityState {
    plugin_id: String,
    plugin_dir: PathBuf,
    permissions: CapabilitySection,
    /// 来自 Host 运行平面的可信策略，不接受 manifest 或 Guest 输入。
    execution_policy: ExecutionPolicy,
    contributions: Arc<ContributionRegistry>,
    services: Arc<ServiceRegistry>,
    agent_runtime: Option<AgentRuntimeBinding>,
    model_completion: Option<ModelCompletionHostServices>,
    processes: HashMap<u64, ManagedProcess>,
    next_process_handle: u64,
    state: HashMap<String, Value>,
    /// 按作用域缓存的持久存储句柄与已加载内容。
    stores: HashMap<PluginStoreScope, PluginStoreSlot>,
    /// 当前 Guest 回调所属的可信 Agent run；独立 RAII 绑定保证异步导出被取消时也会恢复。
    active_run_id: ActiveRunBinding,
    /// 应用可选注入的持久存储；缺失时保留实例内存语义。
    state_store: Option<PluginStateStore>,
}

/// Host 在真实 Manifest 或执行策略边界拒绝插件能力时使用的类型化错误。
#[derive(Debug)]
struct PluginCapabilityDenied(String);

/// 可跨 `await` 持有的 Guest 回调运行绑定；同步锁只保护一个短生命周期字符串替换。
#[derive(Clone, Default)]
struct ActiveRunBinding(Arc<Mutex<Option<String>>>);

impl ActiveRunBinding {
    /// 进入一个 Guest 导出作用域并保存旧值；返回守卫被丢弃时恢复，覆盖取消与提前返回。
    fn enter(&self, run_id: Option<&str>) -> ActiveRunScope {
        let mut active = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::mem::replace(&mut *active, run_id.map(str::to_string));
        ActiveRunScope {
            binding: self.clone(),
            previous,
        }
    }

    /// 克隆当前可信运行 ID，避免在贡献注册表调用期间持有同步锁。
    fn current(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Guest 导出调用的运行作用域；Drop 路径是异步 Future 被取消时的清理保证。
pub(crate) struct ActiveRunScope {
    binding: ActiveRunBinding,
    previous: Option<String>,
}

impl Drop for ActiveRunScope {
    fn drop(&mut self) {
        let mut active = self
            .binding
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active = self.previous.take();
    }
}

impl std::fmt::Display for PluginCapabilityDenied {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PluginCapabilityDenied {}

/// 构造可由 WASM Host 可信识别的能力拒绝错误。
fn capability_denied(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(PluginCapabilityDenied(message.into()))
}

/// 判断错误链是否包含 Host 真实执行的能力拒绝。
pub(crate) fn is_capability_denied(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|source| source.is::<PluginCapabilityDenied>())
}

/// Host 注入单个插件实例的可信服务与运行平面策略。
///
/// 该上下文不包含 manifest 或 Guest 可控字段，避免服务绑定与可信策略在构造阶段被混入
/// 插件声明。
#[derive(Clone, Default)]
pub(crate) struct CapabilityHostContext {
    /// 已绑定可信 principal 的 Agent Runtime；缺失时关闭控制面能力。
    agent_runtime: Option<AgentRuntimeBinding>,
    /// Host 固定模型路由；缺失时关闭模型完成能力。
    model_completion: Option<ModelCompletionHostServices>,
    /// Host 运行平面的可信策略，默认 Serve 以保持既有加载行为。
    execution_policy: ExecutionPolicy,
    /// 应用拥有的持久状态目录；Guest 与 manifest 均不能声明或覆盖。
    persistent_state_dir: Option<PathBuf>,
    /// 当前项目的可信作用域标识，用于按项目隔离插件持久存储。
    ///
    /// 由 Host 应用层从工作区身份生成；缺失时按项目隔离的存储不可用，只保留用户级存储。
    workspace_scope: Option<String>,
}

impl CapabilityHostContext {
    /// 组合 Host 已完成身份绑定的服务与只能收紧的执行策略。
    ///
    /// `agent_runtime` 和 `model_completion` 缺失时，对应能力保持关闭；
    /// `execution_policy` 决定原生进程等高权限能力能否进入操作系统。
    pub(crate) fn new(
        agent_runtime: Option<AgentRuntimeBinding>,
        model_completion: Option<ModelCompletionHostServices>,
        execution_policy: ExecutionPolicy,
        persistent_state_dir: Option<PathBuf>,
        workspace_scope: Option<String>,
    ) -> Self {
        Self {
            agent_runtime,
            model_completion,
            execution_policy,
            persistent_state_dir,
            workspace_scope,
        }
    }
}

impl Drop for CapabilityState {
    fn drop(&mut self) {
        // 加载 future 被取消时无法执行 Guest deactivate，必须由 Host 兜底终止子进程。
        for process in self.processes.values_mut() {
            process.start_kill_tree();
        }
    }
}

impl AgentCapabilityChecks for AgentCapabilitySection {
    fn require_any(&self) -> Result<()> {
        if self.spawn || self.observe || self.cancel {
            Ok(())
        } else {
            Err(capability_denied("插件 manifest 未声明 Agent Runtime 能力"))
        }
    }

    fn require_spawn(&self) -> Result<()> {
        if self.spawn {
            Ok(())
        } else {
            Err(capability_denied(
                "插件 manifest 未声明 capabilities.agent.spawn",
            ))
        }
    }

    fn require_observe(&self) -> Result<()> {
        if self.observe {
            Ok(())
        } else {
            Err(capability_denied(
                "插件 manifest 未声明 capabilities.agent.observe",
            ))
        }
    }

    fn require_cancel(&self) -> Result<()> {
        if self.cancel {
            Ok(())
        } else {
            Err(capability_denied(
                "插件 manifest 未声明 capabilities.agent.cancel",
            ))
        }
    }
}

/// 把宿主能力调用结果编码成稳定的 JSON 信封。
pub(crate) fn encode_host_response(result: Result<Value>) -> String {
    match result {
        Ok(value) => json!({
            "schema_version": HOST_RESPONSE_SCHEMA_VERSION,
            "ok": true,
            "value": value,
        })
        .to_string(),
        Err(error) => json!({
            "schema_version": HOST_RESPONSE_SCHEMA_VERSION,
            "ok": false,
            "error": format!("{error:#}"),
        })
        .to_string(),
    }
}

impl CapabilityState {
    /// 创建绑定到插件目录、manifest 请求和 Host 可信执行策略的能力状态。
    ///
    /// `state_identity` 必须由 Host 根据真实 Bundle 和 capability profile 生成；
    /// `host_context` 必须由 Host 应用组装层提供。构造后所有原生进程入口同时要求其中的
    /// 执行策略允许与 manifest 显式声明，拒绝 Guest 通过提示或工具名称扩大权限。
    pub(crate) fn new(
        plugin_id: String,
        state_identity: String,
        plugin_dir: PathBuf,
        permissions: CapabilitySection,
        contributions: Arc<ContributionRegistry>,
        services: Arc<ServiceRegistry>,
        host_context: CapabilityHostContext,
    ) -> Result<Self> {
        let state_store = host_context
            .persistent_state_dir
            .clone()
            .map(|root| PluginStateStore::new(root, &plugin_id, &state_identity))
            .transpose()?;
        let state = state_store
            .as_ref()
            .map(PluginStateStore::load)
            .transpose()?
            .unwrap_or_default();
        // 持久存储只在 manifest 显式申请且应用注入了存储目录时建立。
        let mut stores = HashMap::new();
        if permissions.storage {
            if let Some(root) = host_context.persistent_state_dir.as_ref() {
                for scope in [PluginStoreScope::User, PluginStoreScope::Workspace] {
                    let Some(scope_key) =
                        store_scope_key(scope, host_context.workspace_scope.as_deref())
                    else {
                        continue;
                    };
                    let store = PluginStateStore::durable(root.clone(), &plugin_id, &scope_key)?;
                    let values = store.load()?;
                    stores.insert(scope, PluginStoreSlot { store, values });
                }
            }
        }
        Ok(Self {
            plugin_id,
            plugin_dir,
            permissions,
            execution_policy: host_context.execution_policy,
            contributions,
            services,
            agent_runtime: host_context.agent_runtime,
            model_completion: host_context.model_completion,
            processes: HashMap::new(),
            next_process_handle: 1,
            state,
            stores,
            active_run_id: ActiveRunBinding::default(),
            state_store,
        })
    }

    /// 绑定当前 Guest 导出调用所属的可信运行；`None` 用于启动、服务与 UI 控制面。
    pub(crate) fn enter_active_run_scope(&self, run_id: Option<&str>) -> ActiveRunScope {
        self.active_run_id.enter(run_id)
    }

    /// 处理动态工具注册请求。
    pub(crate) fn upsert_tool(&mut self, request_json: &str) -> Result<Value> {
        let mut request: ToolRegistrationRequest = parse_request(request_json)?;
        self.permissions.bind_tool_scope(&mut request.spec)?;
        Ok(json!(self.contributions.upsert_tool(request)?))
    }

    /// 处理动态工具删除请求。
    pub(crate) fn remove_tool(&mut self, request_json: &str) -> Result<Value> {
        let request: NameRequest = parse_request(request_json)?;
        self.contributions.remove_tool(&request.name)?;
        Ok(Value::Null)
    }

    /// 处理提示贡献注册请求。
    pub(crate) fn upsert_prompt(&mut self, request_json: &str) -> Result<Value> {
        let request: PromptContribution = parse_request(request_json)?;
        Ok(json!(self.contributions.upsert_prompt(request)?))
    }

    /// 处理提示贡献删除请求。
    pub(crate) fn remove_prompt(&mut self, request_json: &str) -> Result<Value> {
        let request: IdRequest = parse_request(request_json)?;
        self.contributions.remove_prompt(&request.id)?;
        Ok(Value::Null)
    }

    /// 处理结构化扩展事件发布请求。
    pub(crate) fn emit_event(&mut self, request_json: &str) -> Result<Value> {
        let request: ExtensionEventRequest = parse_request(request_json)?;
        if request.name.trim().is_empty() {
            return Err(anyhow!("扩展事件名称不能为空"));
        }
        if request.name == crate::ui::UI_HOST_ACTION_EVENT {
            let action: UiHostActionRequest =
                serde_json::from_value(request.data.clone()).context("解析插件宿主界面动作失败")?;
            if !self.permissions.allows_surface_action(action.action.kind()) {
                return Err(capability_denied(format!(
                    "插件 manifest 未授权 surface action `{}`",
                    action.action.kind().as_str()
                )));
            }
        }
        let active_run_id = self.active_run_id.current();
        self.contributions.emit_event(
            active_run_id.as_deref(),
            json!({
                "source": {
                    "type": "plugin",
                    "id": self.plugin_id,
                },
                "name": request.name,
                "data": request.data,
                "presentation": request.presentation,
            }),
        )?;
        Ok(Value::Null)
    }

    /// 按作用域取出持久存储槽位，并把缺失原因转换为可读的能力拒绝。
    fn store_slot_mut(&mut self, scope: PluginStoreScope) -> Result<&mut PluginStoreSlot> {
        if !self.permissions.storage {
            return Err(capability_denied("插件 manifest 未声明 storage 能力"));
        }
        self.stores.get_mut(&scope).ok_or_else(|| {
            capability_denied(match scope {
                PluginStoreScope::Workspace => "当前宿主未提供项目作用域，按项目隔离的存储不可用",
                PluginStoreScope::User => "当前宿主未启用插件持久存储目录",
            })
        })
    }

    /// 读取本插件持久存储中的一个键。
    pub(crate) fn store_get(&mut self, request_json: &str) -> Result<Value> {
        let request: StoreKeyRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        let slot = self.store_slot_mut(request.scope)?;
        Ok(serde_json::to_value(slot.values.get(&request.key))?)
    }

    /// 事务性写入本插件持久存储；落盘失败时保留原内存值。
    pub(crate) fn store_set(&mut self, request_json: &str) -> Result<Value> {
        let request: StoreSetRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        let slot = self.store_slot_mut(request.scope)?;
        let mut values = slot.values.clone();
        values.insert(request.key, request.value);
        slot.store.save(&values)?;
        slot.values = values;
        Ok(Value::Null)
    }

    /// 事务性删除本插件持久存储中的一个键；落盘失败时保留原内存值。
    pub(crate) fn store_remove(&mut self, request_json: &str) -> Result<Value> {
        let request: StoreKeyRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        let slot = self.store_slot_mut(request.scope)?;
        let mut values = slot.values.clone();
        let Some(removed) = values.remove(&request.key) else {
            return Ok(Value::Null);
        };
        slot.store.save(&values)?;
        slot.values = values;
        Ok(removed)
    }

    /// 列出本插件持久存储中的全部键，按字典序稳定排列。
    pub(crate) fn store_list(&mut self, request_json: &str) -> Result<Value> {
        let request: StoreScopeRequest = parse_request(request_json)?;
        let slot = self.store_slot_mut(request.scope)?;
        let mut keys = slot.values.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        Ok(serde_json::to_value(keys)?)
    }

    /// 读取当前插件实例状态；应用注入持久目录时该映射已在激活前恢复。
    pub(crate) fn get_state(&mut self, request_json: &str) -> Result<Value> {
        let request: KeyRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        Ok(serde_json::to_value(self.state.get(&request.key))?)
    }

    /// 事务性写入当前插件状态；持久保存失败时保留原内存值。
    pub(crate) fn set_state(&mut self, request_json: &str) -> Result<Value> {
        let request: StateSetRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        let mut state = self.state.clone();
        state.insert(request.key, request.value);
        if let Some(store) = self.state_store.as_ref() {
            store.save(&state)?;
        }
        self.state = state;
        Ok(Value::Null)
    }

    /// 事务性删除当前插件状态；持久保存失败时保留原内存值。
    pub(crate) fn remove_state(&mut self, request_json: &str) -> Result<Value> {
        let request: KeyRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        let mut state = self.state.clone();
        let Some(removed) = state.remove(&request.key) else {
            return Ok(Value::Null);
        };
        if let Some(store) = self.state_store.as_ref() {
            store.save(&state)?;
        }
        self.state = state;
        Ok(removed)
    }

    /// 注册或替换当前插件拥有的服务。
    pub(crate) fn upsert_service(&mut self, request_json: &str) -> Result<Value> {
        let request: ServiceSpecRequest = parse_request(request_json)?;
        self.services.upsert(
            &self.plugin_id,
            PluginService {
                plugin_id: String::new(),
                name: request.name,
                version: request.version,
                description: request.description,
            },
        )?;
        Ok(Value::Null)
    }

    /// 删除当前插件拥有的服务。
    pub(crate) fn remove_service(&mut self, request_json: &str) -> Result<Value> {
        let request: NameRequest = parse_request(request_json)?;
        self.services.remove(&self.plugin_id, &request.name)?;
        Ok(Value::Null)
    }

    /// 查询当前服务目录。
    pub(crate) fn list_services(&mut self, request_json: &str) -> Result<Value> {
        let request: ServiceListRequest = parse_request(request_json)?;
        Ok(serde_json::to_value(self.services.list_for_plugin(
            &self.plugin_id,
            request.plugin_id.as_deref(),
        )?)?)
    }

    /// 克隆服务调用所需的可信调用方和共享路由器。
    pub(crate) fn service_context(&self) -> (String, Arc<ServiceRegistry>) {
        (self.plugin_id.clone(), self.services.clone())
    }

    /// 解析并调用另一个插件公开的服务。
    pub(crate) async fn call_service_with(
        caller_id: String,
        services: Arc<ServiceRegistry>,
        request_json: &str,
    ) -> Result<Value> {
        let request: ServiceCallRequest = parse_request(request_json)?;
        services
            .call(PluginServiceCall {
                caller_id,
                plugin_id: request.plugin_id,
                name: request.name,
                payload: request.payload,
            })
            .await
    }

    /// 在 manifest 允许的目录中读取 UTF-8 文件。
    pub(crate) fn read_file(&mut self, request_json: &str) -> Result<Value> {
        let request: PathRequest = parse_request(request_json)?;
        let path = self.resolve_read_path(&request.path)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("读取插件文件失败：{}", path.display()))?;
        Ok(json!(content))
    }

    /// 在 manifest 允许的目录中列出一层目录项。
    pub(crate) fn list_dir(&mut self, request_json: &str) -> Result<Value> {
        let request: PathRequest = parse_request(request_json)?;
        let path = self.resolve_read_path(&request.path)?;
        if !path.is_dir() {
            return Err(anyhow!("插件请求的路径不是目录：{}", path.display()));
        }

        let mut entries = std::fs::read_dir(&path)
            .with_context(|| format!("列出插件目录失败：{}", path.display()))?
            .map(|entry| {
                let entry = entry.context("读取插件目录项失败")?;
                let entry_path = entry.path();
                let relative = entry_path
                    .strip_prefix(&self.plugin_dir)
                    .unwrap_or(&entry_path)
                    .to_string_lossy()
                    .into_owned();
                Ok(FileEntry {
                    path: relative,
                    is_dir: entry
                        .file_type()
                        .context("读取插件目录项类型失败")?
                        .is_dir(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(serde_json::to_value(entries)?)
    }

    fn resolve_read_path(&self, requested: &str) -> Result<PathBuf> {
        if self.permissions.fs_read.is_empty() {
            return Err(capability_denied("插件 manifest 未声明 fs_read 能力"));
        }
        let requested = resolve_from(&self.plugin_dir, requested)
            .canonicalize()
            .with_context(|| format!("插件读取路径不存在：{requested}"))?;
        for allowed in &self.permissions.fs_read {
            let allowed = resolve_from(&self.plugin_dir, allowed)
                .canonicalize()
                .with_context(|| format!("manifest 中的 fs_read 路径不存在：{allowed}"))?;
            if requested == allowed || (allowed.is_dir() && requested.starts_with(&allowed)) {
                return Ok(requested);
            }
        }
        Err(capability_denied(format!(
            "插件无权读取路径：{}",
            requested.display()
        )))
    }
}

/// 一个作用域下已加载的持久存储。
struct PluginStoreSlot {
    /// 该作用域对应的磁盘地址。
    store: PluginStateStore,
    /// 已加载的键值映射；写入成功后才更新，保证与磁盘一致。
    values: HashMap<String, Value>,
}

/// 返回持久存储作用域的可信命名空间标识。
///
/// 按项目隔离需要宿主提供工作区标识；缺失时返回 `None`，对应作用域直接不可用，
/// 而不是退化成跨项目共享——那会让一个项目的数据泄漏到另一个项目。
fn store_scope_key(scope: PluginStoreScope, workspace_scope: Option<&str>) -> Option<String> {
    match scope {
        PluginStoreScope::User => Some("user".to_string()),
        PluginStoreScope::Workspace => workspace_scope
            .filter(|scope| !scope.trim().is_empty())
            .map(|scope| format!("workspace:{scope}")),
    }
}
