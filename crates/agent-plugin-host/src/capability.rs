//! 插件文件与子进程能力的宿主实现。

use crate::{
    contribution::{ContributionRegistry, PromptContribution, ToolRegistrationRequest},
    manifest::{AgentCapabilitySection, CapabilitySection},
    service::{PluginService, PluginServiceCall, ServiceRegistry},
    AgentRuntimeHostServices, ModelCompletionHostServices,
};
use agent_core::{AgentEvent, ModelMessage, ModelRequest, ReasoningLevel, ToolChoice};
use agent_runtime::{
    AgentEventStream, AgentId, AgentRuntimeApi, AgentSpawnRequest, RuntimePrincipal,
};
use agent_tool::{ExecutionPolicy, ExecutionProfile};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

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
    ) -> Self {
        Self {
            agent_runtime,
            model_completion,
            execution_policy,
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

/// 单个插件激活实例可使用的身份绑定 Agent Runtime。
#[derive(Clone)]
pub(crate) struct AgentRuntimeBinding {
    principal: RuntimePrincipal,
    api: Arc<dyn AgentRuntimeApi>,
    host_services: AgentRuntimeHostServices,
    event_streams: Arc<tokio::sync::Mutex<HashMap<AgentId, AgentEventStream>>>,
}

struct ManagedProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    /// Unix 下由子进程 PID 建立的独立进程组，用于回收命令启动的全部后代。
    #[cfg(unix)]
    process_group: u32,
}

impl ManagedProcess {
    /// 请求终止插件进程及其派生的整个进程树。
    fn start_kill_tree(&mut self) {
        #[cfg(unix)]
        {
            // 子进程启动时已成为同 PID 的进程组 leader，负 PID 表示向整组发送信号。
            if let Ok(group) = i32::try_from(self.process_group) {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.start_kill();
    }
}

/// 文件列表返回给插件的稳定结构。
#[derive(Serialize)]
struct FileEntry {
    path: String,
    is_dir: bool,
}

#[derive(Deserialize)]
struct NameRequest {
    name: String,
}

#[derive(Deserialize)]
struct IdRequest {
    id: String,
}

#[derive(Deserialize)]
struct KeyRequest {
    key: String,
}

#[derive(Deserialize)]
struct StateSetRequest {
    key: String,
    value: Value,
}

#[derive(Deserialize)]
struct ServiceSpecRequest {
    name: String,
    version: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct ServiceListRequest {
    plugin_id: Option<String>,
}

#[derive(Deserialize)]
struct ServiceCallRequest {
    plugin_id: String,
    name: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct ExtensionEventRequest {
    name: String,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    presentation: Option<Value>,
}

/// Guest 可提交的受限模型完成请求；真实路由与工具策略由 Host 注入。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestModelCompletionRequest {
    system: Option<String>,
    messages: Vec<ModelMessage>,
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct PathRequest {
    path: String,
}

#[derive(Deserialize)]
struct ProcessSpawnRequest {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    cwd: Option<String>,
    #[serde(default)]
    inherit_stderr: bool,
}

#[derive(Deserialize)]
struct ProcessWriteRequest {
    handle: u64,
    data: String,
}

#[derive(Deserialize)]
struct ProcessReadRequest {
    handle: u64,
    #[serde(default = "default_read_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
struct ProcessHandleRequest {
    handle: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRuntimeCallRequest {
    operation: String,
    #[serde(default)]
    request: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestAgentSpawnRequest {
    profile: String,
    input: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestAgentContinueRequest {
    target: String,
    input: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTargetRequest {
    target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentEventsRequest {
    target: String,
    #[serde(default = "default_agent_event_limit")]
    limit: usize,
}

fn default_agent_event_limit() -> usize {
    128
}

impl CapabilityState {
    /// 创建绑定到插件目录、manifest 请求和 Host 可信执行策略的能力状态。
    ///
    /// `host_context` 必须由 Host 应用组装层提供；构造后所有原生进程入口同时要求其中
    /// 的执行策略允许与 manifest 显式声明，拒绝 Guest 通过提示或工具名称扩大权限。
    pub(crate) fn new(
        plugin_id: String,
        plugin_dir: PathBuf,
        permissions: CapabilitySection,
        contributions: Arc<ContributionRegistry>,
        services: Arc<ServiceRegistry>,
        host_context: CapabilityHostContext,
    ) -> Self {
        Self {
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
            state: HashMap::new(),
        }
    }

    /// 克隆 Agent Runtime 调度所需的权限和绑定，避免跨 `await` 借用 Wasmtime store。
    pub(crate) fn agent_runtime_context(
        &self,
    ) -> (AgentCapabilitySection, Option<AgentRuntimeBinding>) {
        (self.permissions.agent.clone(), self.agent_runtime.clone())
    }

    /// 克隆模型完成所需的 manifest 授权和应用绑定，避免跨 `await` 借用 store。
    pub(crate) fn model_completion_context(&self) -> (bool, Option<ModelCompletionHostServices>) {
        (
            self.permissions.model_completion,
            self.model_completion.clone(),
        )
    }

    /// 校验并执行一次固定路由、禁用工具的模型完成调用。
    pub(crate) async fn complete_model_with(
        allowed: bool,
        binding: Option<ModelCompletionHostServices>,
        request_json: &str,
    ) -> Result<Value> {
        if !allowed {
            return Err(anyhow!("插件 manifest 未授权 model_completion"));
        }
        if request_json.len() > MAX_MODEL_COMPLETION_REQUEST_BYTES {
            return Err(anyhow!(
                "模型完成请求超过 {} 字节限制",
                MAX_MODEL_COMPLETION_REQUEST_BYTES
            ));
        }
        let request: GuestModelCompletionRequest = parse_request(request_json)?;
        if request.messages.is_empty() {
            return Err(anyhow!("模型完成消息不能为空"));
        }
        let binding = binding.ok_or_else(|| anyhow!("应用未注入模型完成服务"))?;

        let mut model_request = ModelRequest::new(binding.model, request.messages);
        model_request.system = request.system;
        model_request.tool_choice = ToolChoice::None;
        model_request.max_tokens = Some(
            request
                .max_tokens
                .unwrap_or(binding.max_output_tokens)
                .clamp(1, binding.max_output_tokens),
        );
        model_request.reasoning = ReasoningLevel::Off;
        let response = if binding.stream {
            binding
                .gateway
                .stream(&binding.provider, model_request)
                .await
                .context("启动插件模型完成流失败")?
                .result()
                .await
                .context("插件模型完成调用失败")?
        } else {
            binding
                .gateway
                .complete(&binding.provider, model_request)
                .await
                .context("插件模型非流式完成调用失败")?
        };
        if !response.tool_calls.is_empty() {
            return Err(anyhow!("模型完成服务返回了未授权工具调用"));
        }
        let text = response.text_content();
        if text.trim().is_empty() {
            return Err(anyhow!("模型完成服务返回了空文本"));
        }
        Ok(json!({
            "text": text,
            "usage": response.usage,
        }))
    }

    /// 解析、鉴权并委托一次 Agent Runtime 短控制面调用。
    pub(crate) async fn call_agent_runtime_with(
        permissions: AgentCapabilitySection,
        binding: Option<AgentRuntimeBinding>,
        request_json: &str,
    ) -> Result<Value> {
        if request_json.len() > MAX_AGENT_RUNTIME_REQUEST_BYTES {
            return Err(anyhow!(
                "Agent Runtime 请求超过 {} 字节限制",
                MAX_AGENT_RUNTIME_REQUEST_BYTES
            ));
        }
        let request: AgentRuntimeCallRequest = parse_request(request_json)?;
        let binding = binding.ok_or_else(|| anyhow!("应用未注入 Agent Runtime 服务"))?;
        match request.operation.as_str() {
            "identity" => {
                require_empty_agent_request(&request.request)?;
                permissions.require_any()?;
                Ok(serde_json::to_value(binding.api.identity())?)
            }
            "spawn" => {
                permissions.require_spawn()?;
                let request: GuestAgentSpawnRequest =
                    serde_json::from_value(request.request).context("解析 Agent spawn 请求失败")?;
                if request.input.trim().is_empty() {
                    return Err(anyhow!("Agent spawn 输入不能为空"));
                }
                if !permissions.allows_profile(&request.profile) {
                    return Err(anyhow!(
                        "插件 manifest 未授权 Agent spawn profile `{}`",
                        request.profile
                    ));
                }
                let derive = binding
                    .host_services
                    .spawn_profile(&request.profile)
                    .ok_or_else(|| {
                        anyhow!("应用未注册 Agent spawn profile `{}`", request.profile)
                    })?;
                let handle = binding
                    .api
                    .spawn(AgentSpawnRequest {
                        input: request.input,
                        derive,
                    })
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(handle)?)
            }
            "continue" => {
                permissions.require_spawn()?;
                let request: GuestAgentContinueRequest = serde_json::from_value(request.request)
                    .context("解析 Agent continue 请求失败")?;
                if request.input.trim().is_empty() {
                    return Err(anyhow!("Agent continue 输入不能为空"));
                }
                let target =
                    AgentId::from_str(&request.target).context("解析 Agent continue 目标失败")?;
                let handle = binding
                    .api
                    .continue_agent(&target, request.input)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(handle)?)
            }
            "steer" => {
                permissions.require_spawn()?;
                let request: GuestAgentContinueRequest =
                    serde_json::from_value(request.request).context("解析 Agent steer 请求失败")?;
                if request.input.trim().is_empty() {
                    return Err(anyhow!("Agent steer 输入不能为空"));
                }
                let target =
                    AgentId::from_str(&request.target).context("解析 Agent steer 目标失败")?;
                binding
                    .api
                    .steer(&target, request.input)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(Value::Null)
            }
            "status" => {
                permissions.require_observe()?;
                let target = parse_agent_target(request.request)?;
                let snapshot = binding
                    .api
                    .status(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(json!({
                    "id": snapshot.id,
                    "lineage": snapshot.lineage,
                    "status": snapshot.status,
                    "permissions": snapshot.permissions,
                }))
            }
            "result" => {
                permissions.require_observe()?;
                let target = parse_agent_target(request.request)?;
                let result = binding
                    .api
                    .result(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(result)?)
            }
            "events" => {
                permissions.require_observe()?;
                let request: AgentEventsRequest = serde_json::from_value(request.request)
                    .context("解析 Agent events 请求失败")?;
                let target =
                    AgentId::from_str(&request.target).context("解析 Agent events 目标失败")?;
                let events = binding
                    .poll_events(&target, request.limit.clamp(1, 512))
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(serde_json::to_value(events)?)
            }
            "cancel" => {
                permissions.require_cancel()?;
                let target = parse_agent_target(request.request)?;
                let cancelled = binding
                    .api
                    .cancel(&target)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(json!(cancelled))
            }
            operation => Err(anyhow!("未知 Agent Runtime 操作：`{operation}`")),
        }
    }

    /// 处理动态工具注册请求。
    pub(crate) fn upsert_tool(&mut self, request_json: &str) -> Result<Value> {
        let request: ToolRegistrationRequest = parse_request(request_json)?;
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
        if request.name == crate::ui::UI_HOST_ACTION_EVENT && !self.permissions.surface_actions {
            return Err(anyhow!("插件 manifest 未声明 surface_actions 能力"));
        }
        self.contributions.emit_event(json!({
            "source": {
                "type": "plugin",
                "id": self.plugin_id,
            },
            "name": request.name,
            "data": request.data,
            "presentation": request.presentation,
        }))?;
        Ok(Value::Null)
    }

    /// 读取当前插件实例的内存状态。
    pub(crate) fn get_state(&mut self, request_json: &str) -> Result<Value> {
        let request: KeyRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        Ok(serde_json::to_value(self.state.get(&request.key))?)
    }

    /// 写入当前插件实例的内存状态。
    pub(crate) fn set_state(&mut self, request_json: &str) -> Result<Value> {
        let request: StateSetRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        self.state.insert(request.key, request.value);
        Ok(Value::Null)
    }

    /// 删除当前插件实例的内存状态。
    pub(crate) fn remove_state(&mut self, request_json: &str) -> Result<Value> {
        let request: KeyRequest = parse_request(request_json)?;
        validate_state_key(&request.key)?;
        Ok(self.state.remove(&request.key).unwrap_or(Value::Null))
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
        Ok(serde_json::to_value(
            self.services.list(request.plugin_id.as_deref())?,
        )?)
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

    /// 启动一个带管道 stdin/stdout 的长驻子进程。
    pub(crate) fn spawn_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        if self.processes.len() >= MAX_PLUGIN_PROCESSES {
            return Err(anyhow!(
                "单个插件最多可同时运行 {MAX_PLUGIN_PROCESSES} 个进程"
            ));
        }

        let request: ProcessSpawnRequest = parse_request(request_json)?;
        validate_process_spawn_request(&request)?;

        let cwd = self.resolve_process_cwd(request.cwd.as_deref())?;
        let mut command = Command::new(&request.command);
        #[cfg(unix)]
        command.process_group(0);
        command
            .args(&request.args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if request.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        copy_safe_process_environment(&mut command);
        command.envs(request.env);

        let mut child = command
            .spawn()
            .with_context(|| format!("启动插件进程失败：{}", request.command))?;
        #[cfg(unix)]
        let process_group = child.id().context("插件子进程缺少 PID")?;
        let stdin = child.stdin.take().context("插件进程缺少 stdin 管道")?;
        let stdout = child.stdout.take().context("插件进程缺少 stdout 管道")?;
        let handle = self.next_process_handle;
        self.next_process_handle = self.next_process_handle.saturating_add(1);
        self.processes.insert(
            handle,
            ManagedProcess {
                child,
                stdin,
                stdout: BufReader::new(stdout).lines(),
                #[cfg(unix)]
                process_group,
            },
        );
        Ok(json!(handle))
    }

    /// 向指定子进程原样写入字节字符串。
    pub(crate) async fn write_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessWriteRequest = parse_request(request_json)?;
        if request.data.len() > MAX_PROCESS_WRITE_BYTES {
            return Err(anyhow!(
                "插件进程单次 stdin 写入超过 {MAX_PROCESS_WRITE_BYTES} 字节限制"
            ));
        }
        let process = self
            .processes
            .get_mut(&request.handle)
            .ok_or_else(|| anyhow!("未知插件进程句柄：{}", request.handle))?;
        process
            .stdin
            .write_all(request.data.as_bytes())
            .await
            .context("写入插件进程 stdin 失败")?;
        process
            .stdin
            .flush()
            .await
            .context("刷新插件进程 stdin 失败")?;
        Ok(Value::Null)
    }

    /// 从指定子进程读取一行，EOF 返回 `null`。
    pub(crate) async fn read_process_line(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessReadRequest = parse_request(request_json)?;
        let process = self
            .processes
            .get_mut(&request.handle)
            .ok_or_else(|| anyhow!("未知插件进程句柄：{}", request.handle))?;
        let timeout_ms = request.timeout_ms.clamp(1, MAX_READ_TIMEOUT_MS);
        let line = timeout(
            Duration::from_millis(timeout_ms),
            process.stdout.next_line(),
        )
        .await
        .map_err(|_| anyhow!("读取插件进程 stdout 超时：{timeout_ms}ms"))?
        .context("读取插件进程 stdout 失败")?;
        let Some(line) = line else {
            return Ok(Value::Null);
        };
        if line.len() > MAX_PROCESS_LINE_BYTES {
            return Err(anyhow!(
                "插件进程单行输出超过 {} 字节限制",
                MAX_PROCESS_LINE_BYTES
            ));
        }
        Ok(json!(line))
    }

    /// 终止并移除指定子进程。
    pub(crate) async fn kill_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessHandleRequest = parse_request(request_json)?;
        let mut process = self
            .processes
            .remove(&request.handle)
            .ok_or_else(|| anyhow!("未知插件进程句柄：{}", request.handle))?;
        process.start_kill_tree();
        process.child.wait().await.context("等待插件进程终止失败")?;
        Ok(Value::Null)
    }

    fn require_process_exec(&self) -> Result<()> {
        if self.execution_policy.profile() != ExecutionProfile::Serve
            || !self.execution_policy.allow_process
        {
            return Err(anyhow!("Host ExecutionPolicy 禁止插件进程执行"));
        }
        if !self.permissions.process_exec {
            return Err(anyhow!("插件 manifest 未声明 process_exec 能力"));
        }
        Ok(())
    }

    fn resolve_read_path(&self, requested: &str) -> Result<PathBuf> {
        if self.permissions.fs_read.is_empty() {
            return Err(anyhow!("插件 manifest 未声明 fs_read 能力"));
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
        Err(anyhow!("插件无权读取路径：{}", requested.display()))
    }

    fn resolve_process_cwd(&self, requested: Option<&str>) -> Result<PathBuf> {
        let path = requested
            .map(|path| resolve_from(&self.plugin_dir, path))
            .unwrap_or_else(|| self.plugin_dir.clone());
        let canonical = path
            .canonicalize()
            .with_context(|| format!("插件进程工作目录不存在：{}", path.display()))?;
        if !canonical.is_dir() {
            return Err(anyhow!("插件进程工作目录不是目录：{}", canonical.display()));
        }
        Ok(canonical)
    }
}

impl AgentRuntimeBinding {
    /// 创建与可信 principal、controller API 和 Host 策略注册表绑定的运行时上下文。
    pub(crate) fn new(
        principal: RuntimePrincipal,
        api: Arc<dyn AgentRuntimeApi>,
        host_services: AgentRuntimeHostServices,
    ) -> Self {
        Self {
            principal,
            api,
            host_services,
            event_streams: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// 复用目标订阅流并非阻塞取出一批已到达事件。
    async fn poll_events(
        &self,
        target: &AgentId,
        limit: usize,
    ) -> agent_runtime::RuntimeResult<Vec<AgentEvent>> {
        let needs_subscription = !self.event_streams.lock().await.contains_key(target);
        if needs_subscription {
            let stream = self.api.subscribe(target).await?;
            self.event_streams
                .lock()
                .await
                .insert(target.clone(), stream);
        }
        let mut streams = self.event_streams.lock().await;
        let stream = streams
            .get_mut(target)
            .expect("已创建的 Agent 事件订阅必须存在");
        Ok((0..limit).filter_map(|_| stream.try_next()).collect())
    }

    /// 撤销当前插件激活 principal，并取消、清理其 controller 与全部后代。
    pub(crate) async fn revoke(&self) -> usize {
        self.host_services.provisioner.revoke(&self.principal).await
    }
}

trait AgentCapabilityChecks {
    fn require_any(&self) -> Result<()>;
    fn require_spawn(&self) -> Result<()>;
    fn require_observe(&self) -> Result<()>;
    fn require_cancel(&self) -> Result<()>;
}

impl AgentCapabilityChecks for AgentCapabilitySection {
    fn require_any(&self) -> Result<()> {
        if self.spawn || self.observe || self.cancel {
            Ok(())
        } else {
            Err(anyhow!("插件 manifest 未声明 Agent Runtime 能力"))
        }
    }

    fn require_spawn(&self) -> Result<()> {
        if self.spawn {
            Ok(())
        } else {
            Err(anyhow!("插件 manifest 未声明 capabilities.agent.spawn"))
        }
    }

    fn require_observe(&self) -> Result<()> {
        if self.observe {
            Ok(())
        } else {
            Err(anyhow!("插件 manifest 未声明 capabilities.agent.observe"))
        }
    }

    fn require_cancel(&self) -> Result<()> {
        if self.cancel {
            Ok(())
        } else {
            Err(anyhow!("插件 manifest 未声明 capabilities.agent.cancel"))
        }
    }
}

fn parse_agent_target(value: Value) -> Result<AgentId> {
    let request: AgentTargetRequest =
        serde_json::from_value(value).context("解析 Agent 目标请求失败")?;
    AgentId::from_str(&request.target).context("Agent 目标 ID 无效")
}

fn require_empty_agent_request(value: &Value) -> Result<()> {
    if value.is_null() {
        Ok(())
    } else {
        Err(anyhow!("该 Agent Runtime 操作不接受 request 字段"))
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

fn parse_request<T: for<'de> Deserialize<'de>>(request_json: &str) -> Result<T> {
    if request_json.len() > MAX_HOST_CAPABILITY_REQUEST_BYTES {
        return Err(anyhow!(
            "插件宿主能力请求超过 {MAX_HOST_CAPABILITY_REQUEST_BYTES} 字节限制"
        ));
    }
    serde_json::from_str(request_json).context("解析插件宿主能力请求失败")
}

/// 校验原生进程启动请求的结构上限，避免高权限能力接收无界输入。
fn validate_process_spawn_request(request: &ProcessSpawnRequest) -> Result<()> {
    if request.command.trim().is_empty() {
        return Err(anyhow!("插件进程命令不能为空"));
    }
    validate_process_text("命令", &request.command, MAX_PROCESS_COMMAND_BYTES)?;
    if request.args.len() > MAX_PROCESS_ARGUMENTS {
        return Err(anyhow!(
            "插件进程参数数量超过 {MAX_PROCESS_ARGUMENTS} 项限制"
        ));
    }
    for argument in &request.args {
        validate_process_text("参数", argument, MAX_PROCESS_ARGUMENT_BYTES)?;
    }
    if request.env.len() > MAX_PROCESS_ENV_ENTRIES {
        return Err(anyhow!(
            "插件进程环境变量数量超过 {MAX_PROCESS_ENV_ENTRIES} 项限制"
        ));
    }
    let mut environment_bytes = 0usize;
    for (key, value) in &request.env {
        if key.is_empty() || key.contains('=') {
            return Err(anyhow!("插件进程环境变量名称无效：`{key}`"));
        }
        validate_process_text("环境变量名称", key, MAX_PROCESS_ENV_BYTES)?;
        validate_process_text("环境变量值", value, MAX_PROCESS_ENV_BYTES)?;
        environment_bytes = environment_bytes
            .saturating_add(key.len())
            .saturating_add(value.len());
    }
    if environment_bytes > MAX_PROCESS_ENV_BYTES {
        return Err(anyhow!(
            "插件进程环境变量总大小超过 {MAX_PROCESS_ENV_BYTES} 字节限制"
        ));
    }
    if let Some(cwd) = &request.cwd {
        validate_process_text("工作目录", cwd, MAX_PROCESS_CWD_BYTES)?;
    }
    Ok(())
}

/// 校验传给操作系统进程 API 的单个字符串字段。
fn validate_process_text(label: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.len() > max_bytes {
        return Err(anyhow!("插件进程{label}超过 {max_bytes} 字节限制"));
    }
    if value.contains('\0') {
        return Err(anyhow!("插件进程{label}不能包含 NUL 字节"));
    }
    Ok(())
}

fn resolve_from(base: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn copy_safe_process_environment(command: &mut Command) {
    for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn default_read_timeout_ms() -> u64 {
    DEFAULT_READ_TIMEOUT_MS
}

fn validate_state_key(key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(anyhow!("插件状态键不能为空"));
    }
    if key.len() > 256 {
        return Err(anyhow!("插件状态键不能超过 256 字节"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ServiceHandler;
    use agent_core::{model::ModelEventStream, ChatModel, ModelResponse, ProviderAdapter};
    use agent_runtime::{
        AgentDeriveConfig, AgentHandle, AgentLineage, AgentOutcome, AgentPermissions,
        AgentProfileId, AgentRuntimeError, AgentRuntimeProvisioner, AgentSnapshot, AgentStatus,
        ProvisionedAgentRuntime, RuntimeResult,
    };
    use agent_tool::ToolAccess;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// 捕获 Host 最终模型请求的测试适配器。
    struct CapturingCompletionModel {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
        expect_stream: bool,
    }

    #[async_trait]
    impl ChatModel for CapturingCompletionModel {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            assert!(!self.expect_stream, "Host 应使用配置的非流式调用路径");
            self.requests.lock().expect("锁定模型请求").push(request);
            Ok(ModelResponse::text("受控模型摘要"))
        }

        async fn stream(&self, request: ModelRequest) -> ModelEventStream {
            assert!(self.expect_stream, "Host 应使用配置的流式调用路径");
            self.requests.lock().expect("锁定模型请求").push(request);
            let (sender, stream) = ModelEventStream::channel();
            sender.done(ModelResponse::text("受控模型摘要"));
            stream
        }
    }

    impl ProviderAdapter for CapturingCompletionModel {
        fn name(&self) -> &'static str {
            "capturing-completion"
        }
    }

    /// 回显 Host 最终注入调用方的测试服务。
    struct CallerEchoService;

    #[async_trait]
    impl ServiceHandler for CallerEchoService {
        async fn handle(&self, call: PluginServiceCall) -> Result<Value> {
            Ok(json!({"caller_id": call.caller_id}))
        }
    }

    struct MockAgentRuntime {
        principal: RuntimePrincipal,
        controller: AgentId,
        child: AgentId,
    }

    #[async_trait]
    impl AgentRuntimeApi for MockAgentRuntime {
        fn principal(&self) -> RuntimePrincipal {
            self.principal.clone()
        }

        fn identity(&self) -> AgentId {
            self.controller.clone()
        }

        async fn spawn(&self, _request: AgentSpawnRequest) -> RuntimeResult<AgentHandle> {
            Ok(AgentHandle {
                id: self.child.clone(),
                lineage: AgentLineage {
                    parent: Some(self.controller.clone()),
                    root: self.controller.clone(),
                    depth: 1,
                },
            })
        }

        async fn continue_agent(
            &self,
            target: &AgentId,
            _input: String,
        ) -> RuntimeResult<AgentHandle> {
            Ok(AgentHandle {
                id: self.child.clone(),
                lineage: AgentLineage {
                    parent: Some(target.clone()),
                    root: self.controller.clone(),
                    depth: 2,
                },
            })
        }

        async fn steer(&self, _target: &AgentId, _input: String) -> RuntimeResult<()> {
            Ok(())
        }

        async fn status(&self, target: &AgentId) -> RuntimeResult<AgentSnapshot> {
            Ok(AgentSnapshot {
                id: target.clone(),
                lineage: AgentLineage {
                    parent: Some(self.controller.clone()),
                    root: self.controller.clone(),
                    depth: 1,
                },
                status: AgentStatus::Running,
                permissions: AgentPermissions::default(),
            })
        }

        async fn result(&self, _target: &AgentId) -> RuntimeResult<Option<AgentOutcome>> {
            Ok(Some(AgentOutcome::Cancelled))
        }

        async fn wait(&self, _target: &AgentId) -> RuntimeResult<AgentOutcome> {
            panic!("WASM dispatcher 不得调用长期等待式 wait")
        }

        async fn cancel(&self, _target: &AgentId) -> RuntimeResult<bool> {
            Ok(true)
        }

        async fn subscribe(
            &self,
            _target: &AgentId,
        ) -> RuntimeResult<agent_runtime::AgentEventStream> {
            Ok(agent_runtime::AgentEventStream::empty())
        }
    }

    struct NoopProvisioner;

    #[async_trait]
    impl AgentRuntimeProvisioner for NoopProvisioner {
        async fn grant_profile(
            &self,
            _principal: RuntimePrincipal,
            _profile: &AgentProfileId,
        ) -> RuntimeResult<()> {
            Ok(())
        }

        async fn provision(
            &self,
            _principal: RuntimePrincipal,
            profile: &AgentProfileId,
        ) -> RuntimeResult<ProvisionedAgentRuntime> {
            Err(AgentRuntimeError::ProfileNotFound(profile.clone()))
        }

        async fn revoke_profile_grant(
            &self,
            _principal: &RuntimePrincipal,
            _profile: &AgentProfileId,
        ) -> bool {
            true
        }

        async fn revoke(&self, _principal: &RuntimePrincipal) -> usize {
            0
        }
    }

    fn agent_runtime_fixture() -> (
        AgentCapabilitySection,
        AgentRuntimeBinding,
        AgentId,
        AgentId,
    ) {
        let principal = RuntimePrincipal::new("plugin:test:activation").expect("创建 principal");
        let controller = AgentId::new();
        let child = AgentId::new();
        let mut spawn_profiles = HashMap::new();
        spawn_profiles.insert("reviewer".into(), AgentDeriveConfig::default());
        let host_services = crate::PluginHostServices::new()
            .with_agent_runtime(
                Arc::new(NoopProvisioner),
                AgentProfileId::new("controller").expect("创建 controller profile"),
                spawn_profiles,
            )
            .expect("创建 Host Services")
            .agent_runtime()
            .expect("读取 Runtime 服务");
        let binding = AgentRuntimeBinding::new(
            principal.clone(),
            Arc::new(MockAgentRuntime {
                principal,
                controller: controller.clone(),
                child: child.clone(),
            }),
            host_services,
        );
        (
            AgentCapabilitySection {
                spawn: true,
                observe: true,
                cancel: true,
                profiles: vec!["reviewer".into()],
            },
            binding,
            controller,
            child,
        )
    }

    fn test_state() -> CapabilityState {
        CapabilityState::new(
            "test-plugin".into(),
            PathBuf::from("."),
            CapabilitySection::default(),
            Arc::new(ContributionRegistry::default()),
            Arc::new(ServiceRegistry::default()),
            CapabilityHostContext::default(),
        )
    }

    /// 创建带指定 manifest 进程声明和可信执行策略的测试状态。
    fn process_state(process_exec: bool, execution_policy: ExecutionPolicy) -> CapabilityState {
        CapabilityState::new(
            "process-test-plugin".into(),
            PathBuf::from("."),
            CapabilitySection {
                process_exec,
                ..CapabilitySection::default()
            },
            Arc::new(ContributionRegistry::default()),
            Arc::new(ServiceRegistry::default()),
            CapabilityHostContext::new(None, None, execution_policy),
        )
    }

    /// 插件实例状态应支持读取、替换和删除。
    #[test]
    fn instance_state_round_trip() {
        let mut state = test_state();
        state
            .set_state(r#"{"key":"counter","value":2}"#)
            .expect("写入状态应成功");
        assert_eq!(
            state
                .get_state(r#"{"key":"counter"}"#)
                .expect("读取状态应成功"),
            json!(2)
        );
        assert_eq!(
            state
                .remove_state(r#"{"key":"counter"}"#)
                .expect("删除状态应成功"),
            json!(2)
        );
        assert_eq!(
            state
                .get_state(r#"{"key":"counter"}"#)
                .expect("读取空状态应成功"),
            Value::Null
        );
    }

    /// 发布事件时 Host 必须覆盖可信的插件来源。
    #[test]
    fn emitted_event_contains_trusted_source() {
        let contributions = Arc::new(ContributionRegistry::default());
        let mut state = CapabilityState::new(
            "trusted-id".into(),
            PathBuf::from("."),
            CapabilitySection::default(),
            contributions.clone(),
            Arc::new(ServiceRegistry::default()),
            CapabilityHostContext::default(),
        );
        state
            .emit_event(r#"{"name":"demo.ready","data":{"ok":true}}"#)
            .expect("发布事件应成功");
        let events = contributions.drain_events().expect("读取事件应成功");
        assert_eq!(events[0]["source"]["id"], "trusted-id");
        assert_eq!(events[0]["name"], "demo.ready");
    }

    /// 宿主响应必须携带稳定版本，且成功和失败信封保持同一结构入口。
    #[test]
    fn host_response_includes_schema_version() {
        let success: Value =
            serde_json::from_str(&encode_host_response(Ok(json!(7)))).expect("解析成功响应");
        let failure = anyhow!("底层失败").context("外层失败");
        let failure: Value =
            serde_json::from_str(&encode_host_response(Err(failure))).expect("解析失败响应");

        assert_eq!(success["schema_version"], HOST_RESPONSE_SCHEMA_VERSION);
        assert_eq!(success["value"], 7);
        assert_eq!(failure["schema_version"], HOST_RESPONSE_SCHEMA_VERSION);
        assert_eq!(failure["error"], "外层失败: 底层失败");
    }

    /// 进程能力必须在调用操作系统前拒绝无界或无效字段。
    #[test]
    fn process_spawn_request_has_structural_limits() {
        let valid = ProcessSpawnRequest {
            command: "bun".into(),
            args: vec!["run".into(), "server.ts".into()],
            env: HashMap::from([("MODE".into(), "stdio".into())]),
            cwd: Some("config".into()),
            inherit_stderr: false,
        };
        assert!(validate_process_spawn_request(&valid).is_ok());

        let mut invalid = valid;
        invalid.args = vec!["x".into(); MAX_PROCESS_ARGUMENTS + 1];
        assert!(validate_process_spawn_request(&invalid).is_err());
        invalid.args.clear();
        invalid.env = HashMap::from([("BAD=KEY".into(), "value".into())]);
        assert!(validate_process_spawn_request(&invalid).is_err());
    }

    /// manifest 声明不得覆盖 Evaluation 或 Mutation 平面的可信进程禁令。
    #[test]
    fn process_manifest_cannot_override_restricted_plane_policy() {
        for mut policy in [
            ExecutionPolicy::evaluation("."),
            ExecutionPolicy::mutation(),
        ] {
            // 即使调用方错误地改开布尔位，可信平面身份也必须保持最终否决权。
            policy.allow_process = true;
            let mut state = process_state(true, policy);
            let error = state
                .spawn_process(r#"{"command":"command-must-not-run"}"#)
                .expect_err("受限平面必须在调用操作系统前拒绝进程");

            assert!(error.to_string().contains("ExecutionPolicy 禁止"));
            assert!(state.processes.is_empty());
        }
    }

    /// Serve 平面允许进程时仍必须获得 manifest 的显式声明。
    #[test]
    fn serve_process_policy_still_requires_manifest_permission() {
        let denied = process_state(false, ExecutionPolicy::serve());
        let error = denied
            .require_process_exec()
            .expect_err("缺少 manifest 声明必须拒绝");
        assert!(error.to_string().contains("manifest 未声明 process_exec"));

        let allowed = process_state(true, ExecutionPolicy::serve());
        allowed
            .require_process_exec()
            .expect("Serve 策略和 manifest 同时允许时门禁应通过");
    }

    /// 插件即使把工具伪装成普通名称，也不能绕过实际进程能力入口。
    #[test]
    fn plugin_tool_alias_cannot_bypass_process_policy() {
        let mut policy = ExecutionPolicy::evaluation(".");
        policy.tools = ToolAccess::allowlist(["innocent_plugin_tool"]);
        assert!(policy.permits_tool("innocent_plugin_tool"));

        let mut state = process_state(true, policy);
        let error = state
            .spawn_process(r#"{"command":"command-must-not-run"}"#)
            .expect_err("Host 进程门禁不得信任插件工具名称");

        assert!(error.to_string().contains("ExecutionPolicy 禁止"));
        assert!(state.processes.is_empty());
    }

    /// 所有 JSON 宿主能力入口必须共享请求大小上限。
    #[test]
    fn host_capability_request_size_is_bounded() {
        let oversized = "x".repeat(MAX_HOST_CAPABILITY_REQUEST_BYTES + 1);
        let error = parse_request::<Value>(&oversized).expect_err("超大请求必须被拒绝");

        assert!(error.to_string().contains("请求超过"));
    }

    /// 模型完成能力必须同时具备 manifest 授权和应用侧服务绑定。
    #[tokio::test]
    async fn model_completion_requires_manifest_permission_and_binding() {
        let request = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"摘要"}]}]}"#;
        let unauthorized = CapabilityState::complete_model_with(false, None, request)
            .await
            .expect_err("未授权请求必须失败");
        assert!(unauthorized.to_string().contains("manifest 未授权"));

        let missing_binding = CapabilityState::complete_model_with(true, None, request)
            .await
            .expect_err("未绑定模型服务必须失败");
        assert!(missing_binding.to_string().contains("未注入模型完成服务"));
    }

    /// Guest 不得通过请求字段覆盖 Host 注入的模型路由。
    #[tokio::test]
    async fn model_completion_rejects_forged_route_fields() {
        let error =
            CapabilityState::complete_model_with(true, None, r#"{"model":"forged","messages":[]}"#)
                .await
                .expect_err("伪造 model 字段必须失败");

        assert!(error.to_string().contains("解析插件宿主能力请求失败"));
    }

    /// Host 必须固定 provider、model、工具策略和最大输出预算。
    #[tokio::test]
    async fn model_completion_uses_trusted_route_and_limits() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = agent_core::ModelGateway::new();
        gateway
            .register(
                "trusted-provider",
                Arc::new(CapturingCompletionModel {
                    requests: requests.clone(),
                    expect_stream: true,
                }),
            )
            .expect("注册测试模型");
        let binding = ModelCompletionHostServices {
            gateway,
            provider: "trusted-provider".into(),
            model: "trusted-model".into(),
            max_output_tokens: 128,
            stream: true,
        };

        let response = CapabilityState::complete_model_with(
            true,
            Some(binding),
            r#"{"system":"只生成摘要","messages":[{"role":"user","content":[{"type":"text","text":"旧上下文"}]}],"max_tokens":999}"#,
        )
        .await
        .expect("受控模型请求应成功");

        assert_eq!(response["text"], "受控模型摘要");
        let captured = requests.lock().expect("锁定模型请求");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].model, "trusted-model");
        assert_eq!(captured[0].max_tokens, Some(128));
        assert_eq!(captured[0].tool_choice, ToolChoice::None);
        assert!(captured[0].tools.is_empty());
        assert_eq!(captured[0].reasoning, ReasoningLevel::Off);
    }

    /// Host 关闭流式模式后必须改用模型的非流式完成接口。
    #[tokio::test]
    async fn model_completion_can_use_non_streaming_route() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = agent_core::ModelGateway::new();
        gateway
            .register(
                "trusted-provider",
                Arc::new(CapturingCompletionModel {
                    requests: requests.clone(),
                    expect_stream: false,
                }),
            )
            .expect("注册非流式测试模型");
        let binding = ModelCompletionHostServices {
            gateway,
            provider: "trusted-provider".into(),
            model: "trusted-model".into(),
            max_output_tokens: 20_000,
            stream: false,
        };

        CapabilityState::complete_model_with(
            true,
            Some(binding),
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"旧上下文"}]}]}"#,
        )
        .await
        .expect("非流式模型完成应成功");

        assert_eq!(requests.lock().expect("锁定模型请求").len(), 1);
    }

    /// Dispatcher 只暴露短控制面操作，并返回可跨 ABI 使用的脱敏结构。
    #[tokio::test]
    async fn agent_runtime_dispatches_short_operations() {
        let (permissions, binding, controller, child) = agent_runtime_fixture();

        let identity = CapabilityState::call_agent_runtime_with(
            permissions.clone(),
            Some(binding.clone()),
            r#"{"operation":"identity"}"#,
        )
        .await
        .expect("读取 controller 身份");
        assert_eq!(identity, json!(controller));

        let spawned = CapabilityState::call_agent_runtime_with(
            permissions.clone(),
            Some(binding.clone()),
            r#"{"operation":"spawn","request":{"profile":"reviewer","input":"检查代码"}}"#,
        )
        .await
        .expect("启动派生 Agent");
        assert_eq!(spawned["id"], json!(child));

        let continued_request = json!({
            "operation": "continue",
            "request": {"target": child.to_string(), "input": "继续检查"},
        })
        .to_string();
        let continued = CapabilityState::call_agent_runtime_with(
            permissions.clone(),
            Some(binding.clone()),
            &continued_request,
        )
        .await
        .expect("继续派生 Agent");
        assert_eq!(continued["lineage"]["parent"], json!(child));

        let status_request = json!({
            "operation": "status",
            "request": {"target": child.to_string()},
        })
        .to_string();
        let status = CapabilityState::call_agent_runtime_with(
            permissions.clone(),
            Some(binding.clone()),
            &status_request,
        )
        .await
        .expect("查询状态");
        assert_eq!(status["status"], "running");
        assert!(status.get("owner").is_none());

        CapabilityState::call_agent_runtime_with(
            permissions.clone(),
            Some(binding.clone()),
            &json!({
                "operation": "steer",
                "request": {"target": child.to_string(), "input": "实时补充"},
            })
            .to_string(),
        )
        .await
        .expect("向运行中 Agent 注入消息");
        let events = CapabilityState::call_agent_runtime_with(
            permissions.clone(),
            Some(binding.clone()),
            &json!({
                "operation": "events",
                "request": {"target": child.to_string(), "limit": 32},
            })
            .to_string(),
        )
        .await
        .expect("轮询 Agent 事件");
        assert_eq!(events, json!([]));

        let cancelled = CapabilityState::call_agent_runtime_with(
            permissions,
            Some(binding),
            &json!({
                "operation": "cancel",
                "request": {"target": child.to_string()},
            })
            .to_string(),
        )
        .await
        .expect("取消派生 Agent");
        assert_eq!(cancelled, json!(true));
    }

    /// Manifest 未授权的 profile 必须在调用 Runtime 前被 Host 拒绝。
    #[tokio::test]
    async fn agent_runtime_rejects_ungranted_spawn_profile() {
        let (permissions, binding, _, _) = agent_runtime_fixture();
        let error = CapabilityState::call_agent_runtime_with(
            permissions,
            Some(binding),
            r#"{"operation":"spawn","request":{"profile":"admin","input":"任务"}}"#,
        )
        .await
        .expect_err("未授权 profile 必须失败");

        assert!(error.to_string().contains("manifest 未授权"));
    }

    /// Runtime 调用信封不得接受 Guest 伪造的 principal 或 caller 字段。
    #[tokio::test]
    async fn agent_runtime_rejects_forged_identity_fields() {
        let (permissions, binding, _, _) = agent_runtime_fixture();
        let error = CapabilityState::call_agent_runtime_with(
            permissions,
            Some(binding),
            r#"{"operation":"identity","principal":"forged"}"#,
        )
        .await
        .expect_err("伪造 principal 必须失败");

        assert!(error.to_string().contains("解析插件宿主能力请求失败"));
    }

    /// Guest 请求体中的 caller_id 不得覆盖 Host 从当前 Store 注入的可信插件 ID。
    #[tokio::test]
    async fn service_call_ignores_forged_caller_id() {
        let services = Arc::new(ServiceRegistry::default());
        services
            .register_handler("provider", Arc::new(CallerEchoService))
            .expect("注册测试服务处理器");
        services
            .upsert(
                "provider",
                PluginService {
                    plugin_id: String::new(),
                    name: "identity.echo".into(),
                    version: "1.0.0".into(),
                    description: None,
                },
            )
            .expect("注册测试服务");

        let response = CapabilityState::call_service_with(
            "trusted-consumer".into(),
            services,
            r#"{"plugin_id":"provider","name":"identity.echo","caller_id":"forged"}"#,
        )
        .await
        .expect("服务调用应成功");

        assert_eq!(response["caller_id"], "trusted-consumer");
    }
}
