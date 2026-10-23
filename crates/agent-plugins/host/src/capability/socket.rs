//! 插件 Unix domain socket 出站能力的宿主实现。
//!
//! # 为什么能力放在 Host 而不是交给 Guest
//!
//! WASI 沙箱没有 socket 访问；要让插件与本地编辑器、守护进程一类的服务通信，只能由 Host
//! 代为连接。代连这件事本身就把授权边界抬到了 Host：Guest 只能请求「连到某个路径」，
//! 具体能不能连由 manifest 与执行策略决定。
//!
//! # 三道闸门
//!
//! 1. **manifest allowlist**：`[capabilities.socket] allow_paths` 是用户与审计者能看到的
//!    授权边界，逐条精确匹配，不支持前缀或通配符。
//! 2. **执行策略**：`ExecutionPolicy::permits_network_access` 为假时整条能力关闭。
//! 3. **调用整形**：路径必须绝对（或 `~/` 开头）、规范化后不得逃逸、必须是 socket 文件；
//!    连接数、单次读写字节、单次等待时长和单连接累计读取量都有上限。
//!
//! # 只搬运字节
//!
//! Host 不解析任何上层协议。NDJSON、MCP、私有 RPC 都只是插件自己的事；这样新增一个本地
//! 服务客户端不需要改 Host，也不会把某个具体协议的规则漏进 ABI。

use crate::capability::requests::{parse_request, resolve_local_path};
use crate::capability::{
    capability_denied, CapabilityState, MAX_PLUGIN_SOCKETS, MAX_SOCKET_READ_BUDGET,
    MAX_SOCKET_RECV_TIMEOUT_MS, MAX_SOCKET_TRANSFER_BYTES, MAX_SOCKET_WAIT_TIMEOUT_MS,
};
use crate::manifest::is_pinned_local_path;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

/// 单个插件实例持有的一条受管 socket 连接。
pub(crate) struct ManagedSocket {
    /// 已建立的连接；句柄表 drop 时自动关闭文件描述符。
    stream: UnixStream,
    /// 该连接累计已读取字节数，用于防止插件把本地服务当无限数据源。
    bytes_read: usize,
}

/// Guest 提交的连接请求。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketConnectRequest {
    /// socket 路径，必须是绝对路径或 `~/` 开头。
    path: String,
    /// 连接超时毫秒数；Host 会夹到自身上限。
    #[serde(default)]
    timeout_ms: u64,
}

/// Guest 提交的写入请求。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketSendRequest {
    /// 连接句柄。
    handle: u64,
    /// 原始数据的 base64 文本。
    data_base64: String,
}

/// Guest 提交的读取请求。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketRecvRequest {
    /// 连接句柄。
    handle: u64,
    /// 单次最多读取的字节数。
    max_bytes: u32,
    /// 读取等待毫秒数；Host 会夹到自身上限。
    #[serde(default)]
    timeout_ms: u64,
}

/// Guest 提交的关闭请求。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketHandleRequest {
    /// 连接句柄。
    handle: u64,
}

impl CapabilityState {
    /// 校验 socket 能力授权与执行策略，并返回 manifest allowlist。
    fn require_socket(&self) -> Result<&crate::manifest::SocketCapabilitySection> {
        if !self.execution_policy.permits_network_access() {
            return Err(capability_denied(
                "Host ExecutionPolicy 禁止插件发起网络访问，socket 能力同样被关闭",
            ));
        }
        self.permissions
            .socket
            .as_ref()
            .ok_or_else(|| capability_denied("插件 manifest 未声明 capabilities.socket 能力"))
    }

    /// 把 Guest 请求的路径解析成已授权的规范化 socket 路径。
    ///
    /// 先规范化再精确比较 allowlist：允许前缀匹配等于把同目录下任意本地服务都授权出去，
    /// 而符号链接必须按真实目标比较，否则一条指向 `/etc/passwd` 的链接就能绕过声明。
    fn resolve_socket_path(&self, requested: &str) -> Result<PathBuf> {
        let allowlist = self.require_socket()?;
        if requested.contains('\0') || requested.starts_with('@') {
            return Err(anyhow!(
                "socket 路径不能包含 NUL 或使用抽象命名空间：`{requested}`"
            ));
        }
        if !is_pinned_local_path(requested) {
            return Err(anyhow!(
                "socket 路径必须是 `/` 或 `~/` 开头的绝对路径且不含 `..`：`{requested}`"
            ));
        }
        let candidate = resolve_local_path(&self.plugin_dir, requested)
            .with_context(|| format!("解析 socket 路径失败：`{requested}`"))?;
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("socket 路径不存在：{}", candidate.display()))?;
        let allowed = allowlist.allow_paths.iter().any(|entry| {
            let Ok(expanded) = resolve_local_path(&self.plugin_dir, entry) else {
                return false;
            };
            // 未声明的路径此刻不存在时直接跳过：服务未启动不应该让已声明的其他连接失败。
            expanded
                .canonicalize()
                .map(|path| path == canonical)
                .unwrap_or(false)
        });
        if !allowed {
            return Err(capability_denied(format!(
                "socket 路径不在 manifest allow_paths 内：{}",
                canonical.display()
            )));
        }
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("读取 socket 元数据失败：{}", canonical.display()))?;
        if !metadata.file_type().is_socket() {
            return Err(anyhow!(
                "路径不是 Unix domain socket：{}",
                canonical.display()
            ));
        }
        Ok(canonical)
    }

    /// 返回当前实例允许同时持有的连接数。
    ///
    /// socket 与子进程同属「原生句柄」资源，因此共用 `max_processes` 这一维度：策略把它
    /// 收紧到 0 就等于关闭全部原生句柄入口，收紧到 2 则两者都最多 2 条。执行策略可以
    /// 收紧，但永远不能放宽 Host 硬上限。
    fn socket_limit(&self) -> usize {
        self.execution_policy
            .limits
            .max_processes
            .map(|limit| limit.min(MAX_PLUGIN_SOCKETS))
            .unwrap_or(MAX_PLUGIN_SOCKETS)
    }

    /// 连接 manifest 允许的 Unix domain socket，返回实例内有效的句柄。
    pub(crate) async fn socket_connect(&mut self, request_json: &str) -> Result<Value> {
        self.require_socket()?;
        let request: SocketConnectRequest = parse_request(request_json)?;
        let path = self.resolve_socket_path(&request.path)?;
        let limit = self.socket_limit();
        if limit == 0 {
            return Err(capability_denied(
                "Host ExecutionPolicy 的原生句柄上限为 0，socket 能力同样被关闭",
            ));
        }
        if self.sockets.len() >= limit {
            return Err(anyhow!(
                "当前策略允许单个插件同时持有的 socket 连接上限为 {limit}"
            ));
        }
        let wait_ms = request.timeout_ms.clamp(1, MAX_SOCKET_WAIT_TIMEOUT_MS);
        let stream = timeout(Duration::from_millis(wait_ms), UnixStream::connect(&path))
            .await
            .map_err(|_| anyhow!("连接 socket 超时：{wait_ms}ms"))?
            .with_context(|| format!("连接 socket 失败：{}", path.display()))?;
        let handle = self.next_socket_handle;
        self.next_socket_handle = self.next_socket_handle.saturating_add(1);
        self.sockets.insert(
            handle,
            ManagedSocket {
                stream,
                bytes_read: 0,
            },
        );
        Ok(json!(handle))
    }

    /// 向指定连接写入全部字节。
    pub(crate) async fn socket_send(&mut self, request_json: &str) -> Result<Value> {
        self.require_socket()?;
        let request: SocketSendRequest = parse_request(request_json)?;
        let data = crate::capability::requests::decode_base64(&request.data_base64)?;
        if data.len() > MAX_SOCKET_TRANSFER_BYTES {
            return Err(anyhow!(
                "socket 单次写入超过 {MAX_SOCKET_TRANSFER_BYTES} 字节限制"
            ));
        }
        let socket = self
            .sockets
            .get_mut(&request.handle)
            .ok_or_else(|| anyhow!("未知 socket 句柄：{}", request.handle))?;
        // 写超时同样收窄：对端一直不读时，Guest 不能借写入把 Host 线程长期占住。
        timeout(
            Duration::from_millis(MAX_SOCKET_WAIT_TIMEOUT_MS),
            socket.stream.write_all(&data),
        )
        .await
        .map_err(|_| anyhow!("写入 socket 超时：{MAX_SOCKET_WAIT_TIMEOUT_MS}ms"))?
        .context("写入 socket 失败")?;
        Ok(Value::Null)
    }

    /// 从指定连接读取一段数据；超时与对端关闭通过稳定状态值返回。
    pub(crate) async fn socket_recv(&mut self, request_json: &str) -> Result<Value> {
        self.require_socket()?;
        let request: SocketRecvRequest = parse_request(request_json)?;
        let max_bytes = usize::try_from(request.max_bytes)
            .unwrap_or(MAX_SOCKET_TRANSFER_BYTES)
            .clamp(1, MAX_SOCKET_TRANSFER_BYTES);
        let wait_ms = request.timeout_ms.clamp(1, MAX_SOCKET_RECV_TIMEOUT_MS);
        let socket = self
            .sockets
            .get_mut(&request.handle)
            .ok_or_else(|| anyhow!("未知 socket 句柄：{}", request.handle))?;
        if socket.bytes_read >= MAX_SOCKET_READ_BUDGET {
            return Err(anyhow!(
                "socket 累计读取超过 {MAX_SOCKET_READ_BUDGET} 字节限制，请重新连接"
            ));
        }
        let mut buffer = vec![0u8; max_bytes];
        match timeout(
            Duration::from_millis(wait_ms),
            socket.stream.read(&mut buffer),
        )
        .await
        {
            // 超时不是连接断开：客户端应当保留连接重试，而不是重建。
            Err(_) => Ok(json!({ "state": "timeout" })),
            Ok(Ok(0)) => Ok(json!({ "state": "closed" })),
            Ok(Ok(read)) => {
                socket.bytes_read = socket.bytes_read.saturating_add(read);
                buffer.truncate(read);
                Ok(json!({
                    "state": "data",
                    "data_base64": crate::capability::requests::encode_base64(&buffer),
                }))
            }
            Ok(Err(error)) => Err(anyhow!("读取 socket 失败：{error}")),
        }
    }

    /// 关闭并释放指定连接；句柄不存在时保持幂等。
    pub(crate) async fn socket_close(&mut self, request_json: &str) -> Result<Value> {
        self.require_socket()?;
        let request: SocketHandleRequest = parse_request(request_json)?;
        if let Some(mut socket) = self.sockets.remove(&request.handle) {
            // 对端可能已经关闭；shutdown 失败不影响句柄回收。
            let _ = socket.stream.shutdown().await;
        }
        Ok(Value::Null)
    }
}
