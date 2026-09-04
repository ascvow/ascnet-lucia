//! 插件文件、状态、服务与进程能力的宿主实现。

use crate::capability::agent::AgentCapabilityChecks;
use crate::capability::process::ManagedProcess;
use crate::capability::requests::parse_request;
use crate::capability::requests::resolve_from;
use crate::capability::requests::ExtensionEventRequest;
use crate::capability::requests::FileEntry;
use crate::capability::requests::IdRequest;
use crate::capability::requests::KeyRequest;
use crate::capability::requests::NameRequest;
use crate::capability::requests::PathRequest;
use crate::capability::requests::ServiceCallRequest;
use crate::capability::requests::ServiceListRequest;
use crate::capability::requests::ServiceSpecRequest;
use crate::capability::requests::SessionReadRequest;
use crate::capability::requests::StateSetRequest;
use crate::capability::requests::StoreScopelessRequest;
#[cfg(feature = "vector-store")]
use crate::capability::requests::{
    decode_vector, VectorOpenRequest, VectorRemoveRequest, VectorSearchRequest, VectorSyncRequest,
    VectorUpsertRequest,
};
use crate::capability::requests::{
    validate_state_key, StoreKeyRequest, StoreScopeRequest, StoreSetRequest,
};
use crate::contribution::ContributionRegistry;
use crate::contribution::PromptContribution;
use crate::contribution::ToolRegistrationRequest;
use crate::manifest::AgentCapabilitySection;
use crate::manifest::CapabilitySection;
use crate::service::PluginService;
use crate::service::PluginServiceCall;
use crate::service::ServiceRegistry;
use crate::services::{HttpHostServices, ModelCompletionHostServices};
use crate::session::{project_session, PluginSessionSource, MAX_SESSION_PAGE_MESSAGES};
use crate::state::PluginStateStore;
use crate::ui::UiHostActionRequest;
pub(crate) use agent::AgentRuntimeBinding;
use agent_plugin_protocol::PluginSessionSummary;
use agent_plugin_protocol::PluginStoreScope;
use agent_tool::ExecutionPolicy;
use anyhow::anyhow;
use anyhow::{Context, Result};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
mod agent;
pub(crate) mod http;
mod process;
mod requests;
#[cfg(test)]
mod tests;
#[cfg(feature = "vector-store")]
mod vector;

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
    /// 应用注入的受限 HTTP 端点；缺失时 `http` 能力不可用。
    http: Option<HttpHostServices>,
    session_source: Option<Arc<dyn PluginSessionSource>>,
    processes: HashMap<u64, ManagedProcess>,
    next_process_handle: u64,
    state: HashMap<String, Value>,
    /// 按作用域缓存的持久存储句柄与已加载内容。
    stores: HashMap<PluginStoreScope, PluginStoreSlot>,
    /// 当前 Guest 回调所属的可信 Agent run；独立 RAII 绑定保证异步导出被取消时也会恢复。
    active_run_id: ActiveRunBinding,
    /// 应用可选注入的持久存储；缺失时保留实例内存语义。
    state_store: Option<PluginStateStore>,
    /// Host 注入的可信规范化工作区根；只供 `workspace_read` 路径校验使用。
    workspace_root: Option<PathBuf>,
    /// 向量库的落盘目录与作用域；manifest 未申请或应用未注入存储目录时为 `None`。
    ///
    /// 只保留寻址信息而不是打开的句柄：向量库要付出建库与加载扩展的代价，而多数插件
    /// 一次也不会用到它，惰性打开让这份代价只落在真正使用的插件上。
    #[cfg(feature = "vector-store")]
    vector_location: Option<(PathBuf, String)>,
    /// 已打开的向量库；首次调用 `host-vector-*` 时建立。
    #[cfg(feature = "vector-store")]
    vectors: Option<vector::PluginVectorStore>,
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
    /// Host 注入的受限 HTTP 端点；缺失时关闭 HTTP 能力。
    http: Option<HttpHostServices>,
    /// 应用注入的只读会话来源；缺失时关闭会话读取能力。
    session_source: Option<Arc<dyn PluginSessionSource>>,
    /// Host 运行平面的可信策略，默认 Serve 以保持既有加载行为。
    execution_policy: ExecutionPolicy,
    /// 应用拥有的持久状态目录；Guest 与 manifest 均不能声明或覆盖。
    persistent_state_dir: Option<PathBuf>,
    /// 当前项目的可信作用域标识，用于按项目隔离插件持久存储。
    ///
    /// 由 Host 应用层从工作区身份生成；缺失时按项目隔离的存储不可用，只保留用户级存储。
    workspace_scope: Option<String>,
    /// 当前项目的可信规范化根目录；缺失时 `workspace_read` 调用必须失败。
    workspace_root: Option<PathBuf>,
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
        session_source: Option<Arc<dyn PluginSessionSource>>,
        workspace_root: Option<PathBuf>,
    ) -> Self {
        Self {
            agent_runtime,
            model_completion,
            http: None,
            execution_policy,
            persistent_state_dir,
            workspace_scope,
            session_source,
            workspace_root,
        }
    }

    /// 注入该插件的受限 HTTP 端点。
    ///
    /// 用 builder 而不是再加一个构造参数：参数表已有七项，继续扩张会让调用点的位置实参
    /// 难以核对，而这正是把服务绑定接错插件的典型来源。
    pub(crate) fn with_http(mut self, http: Option<HttpHostServices>) -> Self {
        self.http = http;
        self
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
        // 向量库按项目隔离：索引天然属于某个仓库，用户级作用域会让不同项目互相污染。
        // 没有项目作用域时退回用户级，保证单仓库以外的使用场景依然可用。
        #[cfg(feature = "vector-store")]
        let vector_location = permissions
            .vector_store
            .then(|| {
                let root = host_context.persistent_state_dir.as_ref()?;
                let scope_key = store_scope_key(
                    PluginStoreScope::Workspace,
                    host_context.workspace_scope.as_deref(),
                )
                .or_else(|| store_scope_key(PluginStoreScope::User, None))?;
                Some((root.clone(), scope_key))
            })
            .flatten();

        Ok(Self {
            plugin_id,
            plugin_dir,
            permissions,
            execution_policy: host_context.execution_policy,
            contributions,
            services,
            agent_runtime: host_context.agent_runtime,
            model_completion: host_context.model_completion,
            http: host_context.http,
            session_source: host_context.session_source,
            processes: HashMap::new(),
            next_process_handle: 1,
            state,
            stores,
            active_run_id: ActiveRunBinding::default(),
            state_store,
            workspace_root: host_context.workspace_root,
            #[cfg(feature = "vector-store")]
            vector_location,
            #[cfg(feature = "vector-store")]
            vectors: None,
        })
    }

    /// 返回已打开的向量库，必要时惰性建立。
    ///
    /// # Errors
    ///
    /// manifest 未申请 `vector_store`、应用未注入持久存储目录，或打开数据库失败时返回错误。
    #[cfg(feature = "vector-store")]
    fn vector_store(&mut self) -> Result<&mut vector::PluginVectorStore> {
        if self.vectors.is_none() {
            let Some((root, scope_key)) = self.vector_location.clone() else {
                return Err(capability_denied(if self.permissions.vector_store {
                    "当前宿主未启用插件持久存储目录，向量库不可用"
                } else {
                    "插件 manifest 未申请 vector_store 能力"
                }));
            };
            self.vectors = Some(vector::PluginVectorStore::open(
                &root,
                &self.plugin_id,
                &scope_key,
            )?);
        }
        self.vectors
            .as_mut()
            .ok_or_else(|| anyhow!("插件向量库未能建立"))
    }

    /// 处理绑定向量空间的请求。
    #[cfg(feature = "vector-store")]
    pub(crate) fn vector_open(&mut self, request_json: &str) -> Result<Value> {
        let request: VectorOpenRequest = parse_request(request_json)?;
        let count = self
            .vector_store()?
            .bind_space(&request.space_id, request.dimension)?;
        Ok(json!({ "count": count }))
    }

    /// 处理分批对账请求。
    #[cfg(feature = "vector-store")]
    pub(crate) fn vector_sync(&mut self, request_json: &str) -> Result<Value> {
        let request: VectorSyncRequest = parse_request(request_json)?;
        request.validate()?;
        let entries: Vec<vector::SyncEntry> = request
            .entries
            .into_iter()
            .map(|entry| vector::SyncEntry {
                record_id: entry.record_id,
                revision: entry.revision,
            })
            .collect();
        let (stale, removed) = self
            .vector_store()?
            .sync(&entries, request.first, request.last)?;
        Ok(json!({ "stale": stale, "removed": removed }))
    }

    /// 处理批量写入向量的请求。
    #[cfg(feature = "vector-store")]
    pub(crate) fn vector_upsert(&mut self, request_json: &str) -> Result<Value> {
        let request: VectorUpsertRequest = parse_request(request_json)?;
        request.validate()?;
        let mut records = Vec::with_capacity(request.records.len());
        for record in request.records {
            records.push(vector::VectorRecord {
                record_id: record.record_id,
                path: record.path,
                revision: record.revision,
                vector: decode_vector(&record.vector)?,
            });
        }
        let written = self.vector_store()?.upsert(&records)?;
        Ok(json!({ "written": written }))
    }

    /// 处理向量检索请求。
    #[cfg(feature = "vector-store")]
    pub(crate) fn vector_search(&mut self, request_json: &str) -> Result<Value> {
        let request: VectorSearchRequest = parse_request(request_json)?;
        let query = decode_vector(&request.vector)?;
        let limit = request.effective_limit();
        let prefix = request
            .path_prefix
            .as_deref()
            .filter(|prefix| !prefix.is_empty());
        let hits = self.vector_store()?.search(&query, limit, prefix)?;
        Ok(json!({
            "hits": hits
                .into_iter()
                .map(|hit| json!({
                    "record_id": hit.record_id,
                    "path": hit.path,
                    "score": hit.score,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// 处理删除向量记录的请求。
    #[cfg(feature = "vector-store")]
    pub(crate) fn vector_remove(&mut self, request_json: &str) -> Result<Value> {
        let request: VectorRemoveRequest = parse_request(request_json)?;
        request.validate()?;
        let removed = self
            .vector_store()?
            .remove(&request.record_ids, &request.path_prefixes)?;
        Ok(json!({ "removed": removed }))
    }

    /// 关闭 `vector-store` feature 时的占位实现，保证 ABI 形状不随构建配置变化。
    #[cfg(not(feature = "vector-store"))]
    pub(crate) fn vector_unavailable(&mut self, _request_json: &str) -> Result<Value> {
        Err(capability_denied(
            "当前宿主构建未启用 vector-store，插件向量库不可用",
        ))
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

    /// 克隆会话读取所需的 manifest 授权与应用绑定，避免跨 `await` 借用 store。
    pub(crate) fn session_context(&self) -> (bool, Option<Arc<dyn PluginSessionSource>>) {
        (self.permissions.sessions, self.session_source.clone())
    }

    /// 列出当前项目的全部会话摘要。
    pub(crate) async fn list_sessions_with(
        allowed: bool,
        source: Option<Arc<dyn PluginSessionSource>>,
        request_json: &str,
    ) -> Result<Value> {
        let source = Self::session_source_for(allowed, source)?;
        // 请求体保留为对象以便后续演进，当前不接受任何字段。
        let _: StoreScopelessRequest = parse_request(request_json)?;
        let descriptors = source.list_sessions().await.context("列出会话失败")?;
        let summaries: Vec<PluginSessionSummary> =
            descriptors.iter().map(PluginSessionSummary::from).collect();
        Ok(serde_json::to_value(summaries)?)
    }

    /// 按标识分页读取会话消息；会话不存在时返回 `null`。
    pub(crate) async fn read_session_with(
        allowed: bool,
        source: Option<Arc<dyn PluginSessionSource>>,
        request_json: &str,
    ) -> Result<Value> {
        let source = Self::session_source_for(allowed, source)?;
        let request: SessionReadRequest = parse_request(request_json)?;
        if request.id.trim().is_empty() {
            return Err(anyhow!("会话标识不能为空"));
        }
        let Some(session) = source
            .load_session(&request.id)
            .await
            .with_context(|| format!("读取会话 `{}` 失败", request.id))?
        else {
            return Ok(Value::Null);
        };
        let page = project_session(
            &request.id,
            &session,
            request.offset,
            request.limit.unwrap_or(MAX_SESSION_PAGE_MESSAGES),
        );
        Ok(serde_json::to_value(page)?)
    }

    /// 校验会话读取授权并取出应用注入的来源。
    fn session_source_for(
        allowed: bool,
        source: Option<Arc<dyn PluginSessionSource>>,
    ) -> Result<Arc<dyn PluginSessionSource>> {
        if !allowed {
            return Err(capability_denied("插件 manifest 未授权 sessions"));
        }
        source.ok_or_else(|| anyhow!("应用未注入只读会话来源"))
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
        let resolved = self.resolve_read_path(&request.path)?;
        let content = std::fs::read_to_string(&resolved.path)
            .with_context(|| format!("读取插件文件失败：{}", resolved.path.display()))?;
        Ok(json!(content))
    }

    /// 在 manifest 允许的目录中列出一层目录项。
    pub(crate) fn list_dir(&mut self, request_json: &str) -> Result<Value> {
        let request: PathRequest = parse_request(request_json)?;
        let resolved = self.resolve_read_path(&request.path)?;
        if !resolved.path.is_dir() {
            return Err(anyhow!(
                "插件请求的路径不是目录：{}",
                resolved.path.display()
            ));
        }

        let mut entries = std::fs::read_dir(&resolved.path)
            .with_context(|| format!("列出插件目录失败：{}", resolved.path.display()))?
            .map(|entry| {
                let entry = entry.context("读取插件目录项失败")?;
                let entry_path = entry.path();
                let relative = entry_path
                    .strip_prefix(&resolved.display_root)
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

    /// 将 Guest 路径解析到 manifest 声明的插件文件范围或 Host 注入的当前工作区范围。
    ///
    /// 两个候选都在规范化后做前缀校验，因此 `..` 与指向工作区外部的符号链接无法越界。
    /// 返回值同时携带目录列表使用的展示根，确保工作区条目只暴露仓库相对路径。
    fn resolve_read_path(&self, requested: &str) -> Result<ResolvedReadPath> {
        if !self.permissions.workspace_read && self.permissions.fs_read.is_empty() {
            return Err(capability_denied(
                "插件 manifest 未声明 workspace_read 或 fs_read 能力",
            ));
        }

        if self.permissions.workspace_read {
            let Some(workspace_root) = self.workspace_root.as_ref() else {
                if self.permissions.fs_read.is_empty() {
                    return Err(capability_denied(
                        "宿主未注入 workspace_read 所需的工作区根目录",
                    ));
                }
                return self.resolve_plugin_read_path(requested);
            };
            if let Ok(path) = resolve_from(workspace_root, requested).canonicalize() {
                if path == *workspace_root || path.starts_with(workspace_root) {
                    return Ok(ResolvedReadPath {
                        path,
                        display_root: workspace_root.clone(),
                    });
                }
            }
        }

        self.resolve_plugin_read_path(requested)
    }

    /// 按 manifest 的插件目录相对 `fs_read` allowlist 解析路径。
    fn resolve_plugin_read_path(&self, requested: &str) -> Result<ResolvedReadPath> {
        if self.permissions.fs_read.is_empty() {
            return Err(capability_denied(format!(
                "插件无权读取当前工作区之外的路径：{requested}"
            )));
        }
        let path = resolve_from(&self.plugin_dir, requested)
            .canonicalize()
            .with_context(|| format!("插件读取路径不存在：{requested}"))?;
        for allowed in &self.permissions.fs_read {
            let allowed = resolve_from(&self.plugin_dir, allowed)
                .canonicalize()
                .with_context(|| format!("manifest 中的 fs_read 路径不存在：{allowed}"))?;
            if path == allowed || (allowed.is_dir() && path.starts_with(&allowed)) {
                return Ok(ResolvedReadPath {
                    path,
                    display_root: self.plugin_dir.clone(),
                });
            }
        }
        Err(capability_denied(format!(
            "插件无权读取路径：{}",
            path.display()
        )))
    }
}

/// 已通过授权边界校验的读取路径及目录列表展示根。
struct ResolvedReadPath {
    /// Host 实际访问的规范化路径。
    path: PathBuf,
    /// `list_dir` 返回相对路径时使用的可信根。
    display_root: PathBuf,
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
