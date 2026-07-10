//! 插件文件与子进程能力的宿主实现。

use crate::{
    contribution::{ContributionRegistry, PromptContribution, ToolRegistrationRequest},
    manifest::{AgentCapabilitySection, CapabilitySection},
    service::{PluginService, PluginServiceCall, ServiceRegistry},
    AgentRuntimeHostServices,
};
use agent_runtime::{
    AgentId, AgentMessageRequest, AgentRuntimeApi, AgentSpawnRequest, RuntimePrincipal,
};
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
const DEFAULT_READ_TIMEOUT_MS: u64 = 30_000;
const MAX_READ_TIMEOUT_MS: u64 = 120_000;
const MAX_AGENT_RUNTIME_REQUEST_BYTES: usize = 1024 * 1024;

/// 单个插件实例可访问的受控宿主能力状态。
pub(crate) struct CapabilityState {
    plugin_id: String,
    plugin_dir: PathBuf,
    permissions: CapabilitySection,
    contributions: Arc<ContributionRegistry>,
    services: Arc<ServiceRegistry>,
    agent_runtime: Option<AgentRuntimeBinding>,
    processes: HashMap<u64, ManagedProcess>,
    next_process_handle: u64,
    state: HashMap<String, Value>,
}

/// 单个插件激活实例可使用的身份绑定 Agent Runtime。
#[derive(Clone)]
pub(crate) struct AgentRuntimeBinding {
    principal: RuntimePrincipal,
    api: Arc<dyn AgentRuntimeApi>,
    host_services: AgentRuntimeHostServices,
}

struct ManagedProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
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
struct GuestAgentMessageRequest {
    recipient: String,
    topic: String,
    #[serde(default)]
    payload: Value,
}

impl CapabilityState {
    /// 创建绑定到插件目录和 manifest 权限的能力状态。
    pub(crate) fn new(
        plugin_id: String,
        plugin_dir: PathBuf,
        permissions: CapabilitySection,
        contributions: Arc<ContributionRegistry>,
        services: Arc<ServiceRegistry>,
        agent_runtime: Option<AgentRuntimeBinding>,
    ) -> Self {
        Self {
            plugin_id,
            plugin_dir,
            permissions,
            contributions,
            services,
            agent_runtime,
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
            "send" => {
                permissions.require_message()?;
                let request: GuestAgentMessageRequest =
                    serde_json::from_value(request.request).context("解析 Agent 消息请求失败")?;
                let recipient =
                    AgentId::from_str(&request.recipient).context("Agent 消息接收者 ID 无效")?;
                let id = binding
                    .api
                    .send(AgentMessageRequest {
                        recipient,
                        topic: request.topic,
                        payload: request.payload,
                    })
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(json!(id.to_string()))
            }
            "try_receive" => {
                require_empty_agent_request(&request.request)?;
                permissions.require_message()?;
                let message = binding
                    .api
                    .try_receive()
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(match message {
                    Some(message) => json!({
                        "id": message.id.to_string(),
                        "sender": message.sender,
                        "recipient": message.recipient,
                        "topic": message.topic,
                        "payload": message.payload,
                        "sent_at_ms": message.sent_at_ms,
                    }),
                    None => Value::Null,
                })
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
        if request.command.trim().is_empty() {
            return Err(anyhow!("插件进程命令不能为空"));
        }

        let cwd = self.resolve_process_cwd(request.cwd.as_deref())?;
        let mut command = Command::new(&request.command);
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
            },
        );
        Ok(json!(handle))
    }

    /// 向指定子进程原样写入字节字符串。
    pub(crate) async fn write_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessWriteRequest = parse_request(request_json)?;
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
        process.child.kill().await.context("终止插件进程失败")?;
        Ok(Value::Null)
    }

    fn require_process_exec(&self) -> Result<()> {
        if self.permissions.process_exec {
            Ok(())
        } else {
            Err(anyhow!("插件 manifest 未声明 process_exec 能力"))
        }
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
        }
    }

    /// 撤销当前插件激活 principal，并取消、清理其 controller 与全部后代。
    pub(crate) async fn revoke(&self) -> usize {
        self.host_services.provisioner.revoke(&self.principal).await
    }
}

trait AgentCapabilityChecks {
    fn require_any(&self) -> Result<()>;
    fn require_spawn(&self) -> Result<()>;
    fn require_message(&self) -> Result<()>;
    fn require_observe(&self) -> Result<()>;
    fn require_cancel(&self) -> Result<()>;
}

impl AgentCapabilityChecks for AgentCapabilitySection {
    fn require_any(&self) -> Result<()> {
        if self.spawn || self.message || self.observe || self.cancel {
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

    fn require_message(&self) -> Result<()> {
        if self.message {
            Ok(())
        } else {
            Err(anyhow!("插件 manifest 未声明 capabilities.agent.message"))
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
        Ok(value) => json!({"ok": true, "value": value}).to_string(),
        Err(error) => json!({"ok": false, "error": error.to_string()}).to_string(),
    }
}

fn parse_request<T: for<'de> Deserialize<'de>>(request_json: &str) -> Result<T> {
    serde_json::from_str(request_json).context("解析插件宿主能力请求失败")
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
    use agent_runtime::{
        AgentDeriveConfig, AgentHandle, AgentLineage, AgentMessage, AgentOutcome, AgentPermissions,
        AgentProfileId, AgentRuntimeError, AgentRuntimeProvisioner, AgentSnapshot, AgentStatus,
        ProvisionedAgentRuntime, RuntimeResult,
    };
    use async_trait::async_trait;
    use uuid::Uuid;

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

        async fn send(&self, _request: AgentMessageRequest) -> RuntimeResult<Uuid> {
            Ok(Uuid::nil())
        }

        async fn try_receive(&self) -> RuntimeResult<Option<AgentMessage>> {
            Ok(Some(AgentMessage {
                id: Uuid::nil(),
                sender: self.child.clone(),
                sender_principal: self.principal.clone(),
                recipient: self.controller.clone(),
                topic: "task.done".into(),
                payload: json!({"ok": true}),
                sent_at_ms: 1,
            }))
        }

        async fn receive(&self) -> RuntimeResult<AgentMessage> {
            panic!("WASM dispatcher 不得调用长期等待式 receive")
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
                message: true,
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
            None,
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
            None,
        );
        state
            .emit_event(r#"{"name":"demo.ready","data":{"ok":true}}"#)
            .expect("发布事件应成功");
        let events = contributions.drain_events().expect("读取事件应成功");
        assert_eq!(events[0]["source"]["id"], "trusted-id");
        assert_eq!(events[0]["name"], "demo.ready");
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

        let received = CapabilityState::call_agent_runtime_with(
            permissions,
            Some(binding),
            r#"{"operation":"try_receive"}"#,
        )
        .await
        .expect("非阻塞读取消息");
        assert_eq!(received["topic"], "task.done");
        assert!(received.get("sender_principal").is_none());
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
}
