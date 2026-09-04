//! Guest 侧宿主能力请求的稳定结构与边界校验。

use crate::capability::{
    DEFAULT_READ_TIMEOUT_MS, MAX_HOST_CAPABILITY_REQUEST_BYTES, MAX_PROCESS_ARGUMENTS,
    MAX_PROCESS_ARGUMENT_BYTES, MAX_PROCESS_COMMAND_BYTES, MAX_PROCESS_CWD_BYTES,
    MAX_PROCESS_ENV_BYTES, MAX_PROCESS_ENV_ENTRIES,
};
use agent_core::model::ModelMessage;
use agent_plugin_protocol::PluginStoreScope;
use agent_runtime::AgentId;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::process::Command;
/// 文件列表返回给插件的稳定结构。
#[derive(Serialize)]
pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
}

#[derive(Deserialize)]
pub(crate) struct NameRequest {
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct IdRequest {
    pub(crate) id: String,
}

#[derive(Deserialize)]
pub(crate) struct KeyRequest {
    pub(crate) key: String,
}

#[derive(Deserialize)]
pub(crate) struct StateSetRequest {
    pub(crate) key: String,
    pub(crate) value: Value,
}

#[derive(Deserialize)]
pub(crate) struct ServiceSpecRequest {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ServiceListRequest {
    pub(crate) plugin_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ServiceCallRequest {
    pub(crate) plugin_id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) payload: Value,
}

#[derive(Deserialize)]
pub(crate) struct ExtensionEventRequest {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) data: Value,
    #[serde(default)]
    pub(crate) presentation: Option<Value>,
}

/// Guest 可提交的受限模型完成请求；真实路由与工具策略由 Host 注入。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestModelCompletionRequest {
    pub(crate) system: Option<String>,
    pub(crate) messages: Vec<ModelMessage>,
    pub(crate) max_tokens: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct PathRequest {
    pub(crate) path: String,
}

#[derive(Deserialize)]
pub(crate) struct ProcessSpawnRequest {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: HashMap<String, String>,
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) inherit_stderr: bool,
}

#[derive(Deserialize)]
pub(crate) struct ProcessWriteRequest {
    pub(crate) handle: u64,
    pub(crate) data: String,
}

#[derive(Deserialize)]
pub(crate) struct ProcessReadRequest {
    pub(crate) handle: u64,
    #[serde(default = "default_read_timeout_ms")]
    pub(crate) timeout_ms: u64,
}

#[derive(Deserialize)]
pub(crate) struct ProcessHandleRequest {
    pub(crate) handle: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentRuntimeCallRequest {
    pub(crate) operation: String,
    #[serde(default)]
    pub(crate) request: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestAgentSpawnRequest {
    pub(crate) profile: String,
    pub(crate) input: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestAgentContinueRequest {
    pub(crate) target: String,
    pub(crate) input: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTargetRequest {
    pub(crate) target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentEventsRequest {
    pub(crate) target: String,
    #[serde(default = "default_agent_event_limit")]
    pub(crate) limit: usize,
}

fn default_agent_event_limit() -> usize {
    128
}

pub(crate) fn parse_agent_target(value: Value) -> Result<AgentId> {
    let request: AgentTargetRequest =
        serde_json::from_value(value).context("解析 Agent 目标请求失败")?;
    AgentId::from_str(&request.target).context("Agent 目标 ID 无效")
}

pub(crate) fn require_empty_agent_request(value: &Value) -> Result<()> {
    if value.is_null() {
        Ok(())
    } else {
        Err(anyhow!("该 Agent Runtime 操作不接受 request 字段"))
    }
}

pub(crate) fn parse_request<T: for<'de> Deserialize<'de>>(request_json: &str) -> Result<T> {
    if request_json.len() > MAX_HOST_CAPABILITY_REQUEST_BYTES {
        return Err(anyhow!(
            "插件宿主能力请求超过 {MAX_HOST_CAPABILITY_REQUEST_BYTES} 字节限制"
        ));
    }
    serde_json::from_str(request_json).context("解析插件宿主能力请求失败")
}

/// 校验原生进程启动请求的结构上限，避免高权限能力接收无界输入。
pub(crate) fn validate_process_spawn_request(request: &ProcessSpawnRequest) -> Result<()> {
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

pub(crate) fn resolve_from(base: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

pub(crate) fn copy_safe_process_environment(command: &mut Command) {
    for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn default_read_timeout_ms() -> u64 {
    DEFAULT_READ_TIMEOUT_MS
}

pub(crate) fn validate_state_key(key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(anyhow!("插件状态键不能为空"));
    }
    if key.len() > 256 {
        return Err(anyhow!("插件状态键不能超过 256 字节"));
    }
    Ok(())
}

/// 指定持久存储作用域的请求。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StoreScopeRequest {
    /// 目标作用域。
    pub(crate) scope: PluginStoreScope,
}

/// 指定持久存储作用域与键的请求。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StoreKeyRequest {
    /// 目标作用域。
    pub(crate) scope: PluginStoreScope,
    /// 目标键。
    pub(crate) key: String,
}

/// 写入持久存储的请求。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StoreSetRequest {
    /// 目标作用域。
    pub(crate) scope: PluginStoreScope,
    /// 目标键。
    pub(crate) key: String,
    /// 待写入的值。
    pub(crate) value: Value,
}

/// 不携带任何字段的请求体。
///
/// 保留为对象而不是空字符串，使后续新增可选字段不必再改 ABI。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoreScopelessRequest {}

/// 分页读取会话消息的请求。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionReadRequest {
    /// 目标会话标识。
    pub(crate) id: String,
    /// 起始消息序号，从零开始。
    #[serde(default)]
    pub(crate) offset: usize,
    /// 本次最多返回的消息数；缺省时使用宿主上限。
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}
