//! Eden 客户端：连接、握手、按 id 配对调用结果。
//!
//! # 职责边界
//!
//! 这一层只处理「怎么和 Eden 说话」：连接参数解析、握手、id 配对、超时与断连。它不判断
//! 哪个工具该读什么，也不决定错误怎么呈现给模型——那些属于 `lib` 的工具层。
//!
//! # 超时语义
//!
//! 读取超时表示「Eden 没在时限内回应」，不是连接断开：连接保留，下一次调用仍然可用。
//! 只有读到 EOF、写入失败或帧非法才丢弃连接，下次调用重新握手。宿主对单次 `recv` 的等待
//! 有硬上限，因此这里按切片循环，把总时限留在客户端自己手里。

use crate::protocol::{self, EdenError, Welcome};
use agent_plugin::{PluginHostApi, SocketRecv};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Eden socket 的默认路径；`~/` 由宿主展开，Guest 不需要知道用户 home。
pub(crate) const DEFAULT_SOCKET_PATH: &str = "~/Library/Application Support/Eden/agent.sock";

/// 握手令牌的默认路径。
pub(crate) const DEFAULT_TOKEN_PATH: &str = "~/Library/Application Support/Eden/agent.token";

/// 单次读取响应的总时限；超过即视为「Eden 没响应」。
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// 单次向宿主请求读取时最多等待的毫秒数。
///
/// 取得比总时限小：宿主对单次等待有硬上限，切片循环既能满足它，也能让客户端在每一片
/// 结束后重新检查总时限。
const RECV_SLICE_MS: u64 = 500;

/// 单次向宿主请求读取的最大字节数。
const RECV_CHUNK_BYTES: usize = 64 * 1024;

/// 连接超时毫秒数。
const CONNECT_TIMEOUT_MS: u64 = 5_000;

/// 单次调用允许跳过的无关帧上限，用于防止对端持续推送未知帧时空转。
const MAX_SKIPPED_FRAMES: usize = 4096;

/// 一次工具调用所需的连接参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdenConfig {
    /// socket 路径。
    pub(crate) socket_path: String,
    /// 令牌文件路径。
    pub(crate) token_path: String,
    /// 可选会话 id；缺失时服务端退回 active 作用域。
    pub(crate) session: Option<String>,
}

impl EdenConfig {
    /// 从 Eden 注入的环境变量推导连接参数，缺失时使用协议默认路径。
    pub(crate) fn from_env() -> Self {
        Self {
            socket_path: env_value("EDEN_SOCKET")
                .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string()),
            token_path: env_value("EDEN_TOKEN_FILE")
                .unwrap_or_else(|| DEFAULT_TOKEN_PATH.to_string()),
            session: env_value("EDEN_SESSION"),
        }
    }
}

/// 读取一个非空环境变量。
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 一次 Eden 交互的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EdenFailure {
    /// Eden 明确返回的结构化错误。
    Protocol(EdenError),
    /// 连接、读写、分帧或版本层面的本地失败。
    Transport(String),
}

impl std::fmt::Display for EdenFailure {
    /// 输出面向模型的单行说明。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => formatter.write_str(&error.describe()),
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EdenFailure {}

/// 一条已建立的连接。
struct Connection {
    /// 宿主分配的 socket 句柄。
    handle: u64,
    /// 尚未切分成行的接收缓冲；一次读取可能是半行或多行。
    buffer: Vec<u8>,
}

/// 长驻 Eden 客户端；一个插件实例复用同一条连接。
pub(crate) struct EdenClient {
    /// 连接参数。
    config: EdenConfig,
    /// 当前连接；`None` 表示需要重新握手。
    connection: Option<Connection>,
    /// 已协商的握手结果。
    welcome: Option<Welcome>,
    /// 下一个调用 id；客户端生成、服务端原样回填。
    next_id: u64,
    /// 单次响应等待时限；测试可缩短。
    read_timeout: Duration,
}

impl EdenClient {
    /// 用给定连接参数创建客户端；不会立刻连接。
    pub(crate) fn new(config: EdenConfig) -> Self {
        Self {
            config,
            connection: None,
            welcome: None,
            next_id: 1,
            read_timeout: READ_TIMEOUT,
        }
    }

    /// 覆盖响应等待时限，供单元测试使用。
    #[cfg(test)]
    pub(crate) fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// 返回握手结果；尚未连接时为 `None`。
    pub(crate) fn welcome(&self) -> Option<&Welcome> {
        self.welcome.as_ref()
    }

    /// 确保已握手；必要时重新连接。
    pub(crate) fn ensure_connected(&mut self, host: &dyn PluginHostApi) -> Result<(), EdenFailure> {
        if self.connection.is_some() {
            return Ok(());
        }
        self.connect(host)
    }

    /// 连接 Eden 并完成 v1 握手。
    ///
    /// 令牌每次连接都重新读取：Eden 每次启动都会重新生成令牌，缓存旧值只会让插件在
    /// 编辑器重启后持续收到 `unauthorized`。
    pub(crate) fn connect(&mut self, host: &dyn PluginHostApi) -> Result<(), EdenFailure> {
        self.reset(host);
        let loaded = host.read_file(&self.config.token_path).map_err(|error| {
            EdenFailure::Transport(format!(
                "读取 Eden 令牌失败（{}）：{error:#}。请确认 Eden 正在运行，且 manifest 的 fs_read \
                 已声明该路径。",
                self.config.token_path
            ))
        })?;
        let token = loaded.trim().to_string();
        if token.is_empty() {
            return Err(EdenFailure::Transport(format!(
                "Eden 令牌文件为空：{}",
                self.config.token_path
            )));
        }

        let handle = host
            .socket_connect(&self.config.socket_path, CONNECT_TIMEOUT_MS)
            .map_err(|error| {
                EdenFailure::Transport(format!(
                    "连接 Eden socket 失败（{}）：{error:#}。请确认 Eden 正在运行，且 manifest 的 \
                     capabilities.socket.allow_paths 已声明该路径。",
                    self.config.socket_path
                ))
            })?;
        self.connection = Some(Connection {
            handle,
            buffer: Vec::new(),
        });

        let mut hello = serde_json::Map::new();
        hello.insert("op".to_string(), json!("hello"));
        hello.insert("v".to_string(), json!(protocol::PROTOCOL_VERSION));
        // 协议要求用不到的字段省略而不是写 `null`：`session` 只在本进程确实从 Eden 的终端
        // 窗格启动时才有意义，缺失时服务端会退回 `active` 作用域。
        if let Some(session) = self.config.session.as_ref() {
            hello.insert("session".to_string(), json!(session));
        }
        hello.insert("token".to_string(), json!(token));
        hello.insert(
            "client".to_string(),
            json!({
                "name": "lucia-eden",
                "version": env!("CARGO_PKG_VERSION"),
                // Guest 没有进程号；协议只把它当自述信息，不做校验。
                "pid": 0,
            }),
        );
        if let Err(error) = self.send_frame(host, &Value::Object(hello)) {
            self.reset(host);
            return Err(error);
        }

        let frame = match self.read_frame(host) {
            Ok(frame) => frame,
            Err(error) => {
                self.reset(host);
                return Err(error);
            }
        };
        match Welcome::parse(&frame) {
            Ok(welcome) => {
                self.welcome = Some(welcome);
                Ok(())
            }
            Err(error) => {
                self.reset(host);
                Err(EdenFailure::Transport(format!("{error:#}")))
            }
        }
    }

    /// 调用一个 Eden 工具并返回成功负载。
    ///
    /// 失败结果区分两类：Eden 明确给出的结构化错误原样上抛，传输失败则丢弃连接，让下一次
    /// 调用重新握手——半途断掉的连接继续复用只会读到 EOF。
    pub(crate) fn call(
        &mut self,
        host: &dyn PluginHostApi,
        tool: &str,
        args: Value,
    ) -> Result<Value, EdenFailure> {
        self.ensure_connected(host)?;
        if let Some(welcome) = self.welcome.as_ref() {
            if !welcome.supports(tool) {
                return Err(EdenFailure::Protocol(EdenError {
                    code: "unknown_tool".to_string(),
                    message: format!("Eden 未声明能力 `{tool}`"),
                    hint: Some(format!(
                        "服务端可用能力：{}",
                        welcome.capabilities.join("、")
                    )),
                }));
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({ "op": "call", "id": id, "tool": tool, "args": args });
        let result = self
            .send_frame(host, &request)
            .and_then(|()| self.read_result(host, id));
        match &result {
            // 传输失败说明连接已经不可用。
            Err(EdenFailure::Transport(_)) => self.reset(host),
            // `unauthorized` 说明 Eden 重启后换了令牌。只有丢弃连接，下一次调用才会重新
            // 读令牌文件并重新握手；否则后续每次调用都会带着旧令牌一直失败。
            Err(EdenFailure::Protocol(error)) if error.code == "unauthorized" => self.reset(host),
            _ => {}
        }
        result
    }

    /// 读取与 `id` 配对的 result 帧。
    ///
    /// 响应顺序不保证与请求顺序一致，因此必须按 id 配对；收到其他 id 的帧说明它属于更早
    /// 的调用，直接丢弃。v1 未启用事件推送，`event` 一类的帧同样跳过。
    fn read_result(&mut self, host: &dyn PluginHostApi, id: u64) -> Result<Value, EdenFailure> {
        for _ in 0..MAX_SKIPPED_FRAMES {
            let frame = self.read_frame(host)?;
            match frame.get("op").and_then(Value::as_str).unwrap_or_default() {
                "result" => {
                    if frame.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    let ok = frame.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    if ok {
                        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
                    }
                    let error = EdenError::from_value(&frame).unwrap_or_else(|| EdenError {
                        code: "unknown".to_string(),
                        message: "Eden 返回了失败结果但没有 error 对象".to_string(),
                        hint: None,
                    });
                    return Err(EdenFailure::Protocol(error));
                }
                "error" => {
                    let error = EdenError::from_value(&frame).unwrap_or_else(|| EdenError {
                        code: "bad_request".to_string(),
                        message: "Eden 返回了没有 error 对象的帧级错误".to_string(),
                        hint: None,
                    });
                    return Err(EdenFailure::Protocol(error));
                }
                _ => continue,
            }
        }
        Err(EdenFailure::Transport(format!(
            "Eden 连续返回超过 {MAX_SKIPPED_FRAMES} 个无法匹配的帧"
        )))
    }

    /// 发送一条报文。
    fn send_frame(&mut self, host: &dyn PluginHostApi, message: &Value) -> Result<(), EdenFailure> {
        let Some(connection) = self.connection.as_ref() else {
            return Err(EdenFailure::Transport("Eden 连接尚未建立".to_string()));
        };
        let handle = connection.handle;
        let line = protocol::encode_line(message);
        host.socket_send(handle, line.as_bytes())
            .map_err(|error| EdenFailure::Transport(format!("写入 Eden socket 失败：{error:#}")))
    }

    /// 读取下一条完整报文。
    fn read_frame(&mut self, host: &dyn PluginHostApi) -> Result<Value, EdenFailure> {
        let deadline = Instant::now() + self.read_timeout;
        let read_timeout = self.read_timeout;
        loop {
            let Some(connection) = self.connection.as_mut() else {
                return Err(EdenFailure::Transport("Eden 连接尚未建立".to_string()));
            };
            let line = protocol::take_line(&mut connection.buffer)
                .map_err(|error| EdenFailure::Transport(format!("{error:#}")))?;
            if let Some(line) = line {
                if line.trim().is_empty() {
                    continue;
                }
                return protocol::parse_message(&line)
                    .map_err(|error| EdenFailure::Transport(format!("{error:#}")));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(EdenFailure::Transport(format!(
                    "Eden 未在 {} 秒内响应",
                    read_timeout.as_secs()
                )));
            }
            let slice_ms = u64::try_from(remaining.as_millis())
                .unwrap_or(RECV_SLICE_MS)
                .clamp(1, RECV_SLICE_MS);
            match host.socket_recv(connection.handle, RECV_CHUNK_BYTES, slice_ms) {
                Ok(SocketRecv::Data(bytes)) => {
                    if let Some(connection) = self.connection.as_mut() {
                        connection.buffer.extend_from_slice(&bytes);
                    }
                }
                // 切片到期不是失败：继续等，直到总时限耗尽。
                Ok(SocketRecv::Timeout) => continue,
                Ok(SocketRecv::Closed) => {
                    return Err(EdenFailure::Transport("Eden 已关闭连接".to_string()))
                }
                Err(error) => {
                    return Err(EdenFailure::Transport(format!(
                        "读取 Eden socket 失败：{error:#}"
                    )))
                }
            }
        }
    }

    /// 放弃当前连接并释放句柄，保留配置以便下次重连。
    fn reset(&mut self, host: &dyn PluginHostApi) {
        if let Some(connection) = self.connection.take() {
            let _ = host.socket_close(connection.handle);
        }
        self.welcome = None;
    }

    /// 发送 `bye` 并关闭连接；失败不影响卸载流程。
    pub(crate) fn close(&mut self, host: &dyn PluginHostApi) {
        if self.connection.is_some() {
            let _ = self.send_frame(host, &json!({ "op": "bye" }));
        }
        self.reset(host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin::{
        AgentContinueRequest, AgentId, AgentOutcome, AgentSnapshot, AgentSpawnRequest, FileEntry,
        HttpRequest, HttpResponse, ModelCompletionRequest, ModelCompletionResponse, ProcessSpec,
        ServiceDescriptor, ServiceSpec,
    };
    use anyhow::Result;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// 记录调用并按队列返回读取结果的假宿主。
    #[derive(Default)]
    struct FakeHost {
        /// 令牌文件内容。
        token: String,
        /// 待返回给客户端的读取结果。
        incoming: RefCell<VecDeque<SocketRecv>>,
        /// 客户端写入的全部字节。
        sent: RefCell<Vec<u8>>,
        /// 已建立的连接数。
        connects: RefCell<usize>,
        /// 已关闭的句柄。
        closed: RefCell<Vec<u64>>,
        /// 令牌文件被读取的次数；用于证明重连会重新读令牌。
        token_reads: RefCell<usize>,
    }

    impl FakeHost {
        /// 用给定令牌和预置响应创建假宿主。
        fn new(token: &str, incoming: Vec<SocketRecv>) -> Self {
            Self {
                token: token.to_string(),
                incoming: RefCell::new(incoming.into()),
                ..Self::default()
            }
        }

        /// 取出并清空客户端写入的全部字节。
        fn take_sent(&self) -> String {
            let bytes = self.sent.borrow_mut().drain(..).collect::<Vec<_>>();
            String::from_utf8(bytes).expect("客户端写入应为 UTF-8")
        }
    }

    impl PluginHostApi for FakeHost {
        fn upsert_tool(&self, _local_name: &str, _spec: &agent_plugin::ToolSpec) -> Result<String> {
            unreachable!("测试不注册工具")
        }

        fn remove_tool(&self, _public_name: &str) -> Result<()> {
            unreachable!("测试不删除工具")
        }

        fn upsert_prompt(&self, _prompt: &agent_plugin::PromptContribution) -> Result<String> {
            unreachable!("测试不注册提示")
        }

        fn remove_prompt(&self, _id: &str) -> Result<()> {
            unreachable!("测试不删除提示")
        }

        fn emit_event(&self, _event: &agent_plugin::ExtensionEvent) -> Result<()> {
            unreachable!("测试不发布事件")
        }

        fn get_state(&self, _key: &str) -> Result<Option<Value>> {
            Ok(None)
        }

        fn set_state(&self, _key: &str, _value: &Value) -> Result<()> {
            Ok(())
        }

        fn remove_state(&self, _key: &str) -> Result<Option<Value>> {
            Ok(None)
        }

        fn upsert_service(&self, _service: &ServiceSpec) -> Result<()> {
            unreachable!("测试不注册服务")
        }

        fn remove_service(&self, _name: &str) -> Result<()> {
            unreachable!("测试不删除服务")
        }

        fn list_services(&self, _plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
            Ok(Vec::new())
        }

        fn call_service(&self, _plugin_id: &str, _name: &str, _payload: &Value) -> Result<Value> {
            unreachable!("测试不调用服务")
        }

        fn read_file(&self, _path: &str) -> Result<String> {
            *self.token_reads.borrow_mut() += 1;
            Ok(self.token.clone())
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<FileEntry>> {
            Ok(Vec::new())
        }

        fn spawn_process(&self, _spec: &ProcessSpec) -> Result<u64> {
            unreachable!("测试不启动进程")
        }

        fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
            unreachable!("测试不写进程")
        }

        fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
            unreachable!("测试不读进程")
        }

        fn kill_process(&self, _handle: u64) -> Result<()> {
            unreachable!("测试不终止进程")
        }

        fn http_request(&self, _request: &HttpRequest) -> Result<HttpResponse> {
            unreachable!("测试不发 HTTP 请求")
        }

        fn complete_model(
            &self,
            _request: &ModelCompletionRequest,
        ) -> Result<ModelCompletionResponse> {
            unreachable!("测试不调用模型")
        }

        fn agent_identity(&self) -> Result<AgentId> {
            unreachable!("测试不查询身份")
        }

        fn spawn_agent(&self, _request: &AgentSpawnRequest) -> Result<agent_plugin::AgentHandle> {
            unreachable!("测试不派生 Agent")
        }

        fn continue_agent(
            &self,
            _request: &AgentContinueRequest,
        ) -> Result<agent_plugin::AgentHandle> {
            unreachable!("测试不续跑 Agent")
        }

        fn steer_agent(&self, _target: &AgentId, _input: &str) -> Result<()> {
            unreachable!("测试不注入消息")
        }

        fn agent_status(&self, _target: &AgentId) -> Result<AgentSnapshot> {
            unreachable!("测试不查询状态")
        }

        fn agent_result(&self, _target: &AgentId) -> Result<Option<AgentOutcome>> {
            unreachable!("测试不查询结果")
        }

        fn agent_events(
            &self,
            _target: &AgentId,
            _limit: usize,
        ) -> Result<Vec<agent_plugin::AgentEvent>> {
            unreachable!("测试不订阅事件")
        }

        fn cancel_agent(&self, _target: &AgentId) -> Result<bool> {
            unreachable!("测试不取消 Agent")
        }

        fn socket_connect(&self, _path: &str, _timeout_ms: u64) -> Result<u64> {
            let mut connects = self.connects.borrow_mut();
            *connects += 1;
            Ok(*connects as u64)
        }

        fn socket_send(&self, _handle: u64, data: &[u8]) -> Result<()> {
            self.sent.borrow_mut().extend_from_slice(data);
            Ok(())
        }

        fn socket_recv(
            &self,
            _handle: u64,
            _max_bytes: usize,
            _timeout_ms: u64,
        ) -> Result<SocketRecv> {
            Ok(self
                .incoming
                .borrow_mut()
                .pop_front()
                .unwrap_or(SocketRecv::Timeout))
        }

        fn socket_close(&self, handle: u64) -> Result<()> {
            self.closed.borrow_mut().push(handle);
            Ok(())
        }
    }

    /// 构造一个把给定 JSON 报文排队返回的读取结果序列。
    fn frames(lines: &[Value]) -> Vec<SocketRecv> {
        lines
            .iter()
            .map(|line| SocketRecv::Data(protocol::encode_line(line).into_bytes()))
            .collect()
    }

    /// 测试用连接参数。
    fn config() -> EdenConfig {
        EdenConfig {
            socket_path: "/tmp/eden-test.sock".to_string(),
            token_path: "~/eden.token".to_string(),
            session: Some("session-1".to_string()),
        }
    }

    /// 拿不到 `EDEN_SESSION` 时的连接参数；服务端据此退回 `active` 作用域。
    fn config_without_session() -> EdenConfig {
        EdenConfig {
            session: None,
            ..config()
        }
    }

    /// 没有会话时 `hello` 必须省略 `session` 字段，而不是写 `null`。
    ///
    /// 协议要求「用不到的字段省略」，服务端按存在与否区分「这是从某个窗格发起的」和
    /// 「拿不到会话」，多一个 `null` 会让这一步变成解析差异。
    #[test]
    fn hello_omits_session_when_absent() {
        let host = FakeHost::new(
            "token",
            frames(&[json!({ "op": "welcome", "v": 1, "scope": { "resolved": false } })]),
        );
        let mut client = EdenClient::new(config_without_session());
        client.ensure_connected(&host).expect("握手应成功");

        let sent = host.take_sent();
        let hello: Value = serde_json::from_str(sent.trim()).expect("hello 应是 JSON");
        assert!(
            hello.get("session").is_none(),
            "缺少会话时不应写 session 字段：{hello}"
        );
        assert_eq!(hello["token"], "token");
    }

    /// `unauthorized` 必须丢弃连接，下一次调用重新读令牌并重新握手。
    ///
    /// Eden 每次启动都会生成新令牌，缓存旧令牌会让插件在编辑器重启后持续失败；传输层
    /// 无法区分这两种失败，所以这里单独覆盖协议错误的分支。
    #[test]
    fn unauthorized_reconnects_and_rereads_token() {
        let host = FakeHost::new(
            "rotated-token",
            frames(&[
                json!({ "op": "welcome", "v": 1, "scope": { "focused": true } }),
                json!({
                    "op": "result",
                    "id": 1,
                    "ok": false,
                    "error": {
                        "code": "unauthorized",
                        "message": "bad token",
                        "hint": "重新读取令牌后重连"
                    }
                }),
                json!({ "op": "welcome", "v": 1, "scope": { "focused": true } }),
                json!({ "op": "result", "id": 2, "ok": true, "result": { "path": "/tmp/a.swift" } }),
            ]),
        );
        let mut client = EdenClient::new(config());

        let error = client
            .call(&host, "context.get", json!({}))
            .expect_err("旧令牌应被拒绝");
        assert!(error.to_string().contains("重新读取令牌"), "{error}");
        assert_eq!(*host.connects.borrow(), 1);
        assert_eq!(host.closed.borrow().as_slice(), &[1], "应释放旧连接");

        let result = client
            .call(&host, "context.get", json!({}))
            .expect("应重新握手后成功");
        assert_eq!(result["path"], "/tmp/a.swift");
        assert_eq!(*host.connects.borrow(), 2);
        assert_eq!(*host.token_reads.borrow(), 2, "重连必须重新读令牌文件");
    }

    /// 握手必须发送带令牌与会话的 hello，并记录 welcome 的能力与作用域。
    #[test]
    fn handshake_sends_hello_and_records_welcome() {
        let host = FakeHost::new(
            "3f9a\n",
            frames(&[json!({
                "op": "welcome",
                "v": 1,
                "session": "session-1",
                "capabilities": ["selection.get", "context.get"],
                "scope": { "resolved": true, "focused": true }
            })]),
        );
        let mut client = EdenClient::new(config());
        client.ensure_connected(&host).expect("握手应成功");

        let sent = host.take_sent();
        let hello: Value = serde_json::from_str(sent.trim()).expect("hello 应是 JSON");
        assert_eq!(hello["op"], "hello");
        assert_eq!(hello["v"], 1);
        assert_eq!(hello["token"], "3f9a");
        assert_eq!(hello["session"], "session-1");

        let welcome = client.welcome().expect("应记录 welcome");
        assert!(welcome.supports("selection.get"));
        assert_eq!(welcome.focused(), Some(true));
    }

    /// 结果必须按 id 配对：先到的旧 id 帧要被跳过。
    #[test]
    fn results_are_matched_by_id() {
        let host = FakeHost::new(
            "token",
            frames(&[
                json!({
                    "op": "welcome",
                    "v": 1,
                    "capabilities": ["selection.get"],
                    "scope": { "focused": true }
                }),
                json!({ "op": "result", "id": 99, "ok": true, "result": { "stale": true } }),
                json!({ "op": "event", "seq": 1, "data": {} }),
                json!({
                    "op": "result",
                    "id": 1,
                    "ok": true,
                    "result": { "path": "/tmp/a.swift", "caretLine": 7 }
                }),
            ]),
        );
        let mut client = EdenClient::new(config());
        let result = client
            .call(&host, "selection.get", json!({ "scope": "session" }))
            .expect("调用应成功");

        assert_eq!(result["caretLine"], 7);
        let sent = host.take_sent();
        let call: Value = serde_json::from_str(sent.lines().last().expect("应有 call 帧"))
            .expect("call 应是 JSON");
        assert_eq!(call["id"], 1);
        assert_eq!(call["tool"], "selection.get");
    }

    /// Eden 的结构化错误必须带上 hint 上抛，且连接保持可用。
    #[test]
    fn protocol_errors_keep_hint_and_connection() {
        let host = FakeHost::new(
            "token",
            frames(&[
                json!({ "op": "welcome", "v": 1, "capabilities": ["editor.reveal"] }),
                json!({
                    "op": "result",
                    "id": 1,
                    "ok": false,
                    "error": {
                        "code": "user_in_another_tab",
                        "message": "用户正在看别的 tab",
                        "hint": "改成文字告知"
                    }
                }),
            ]),
        );
        let mut client = EdenClient::new(config());
        let error = client
            .call(
                &host,
                "editor.reveal",
                json!({ "path": "/tmp/a.swift", "line": 3 }),
            )
            .expect_err("应返回结构化错误");

        assert_eq!(
            error,
            EdenFailure::Protocol(EdenError {
                code: "user_in_another_tab".to_string(),
                message: "用户正在看别的 tab".to_string(),
                hint: Some("改成文字告知".to_string()),
            })
        );
        assert!(error.to_string().contains("改成文字告知"));
        assert!(host.closed.borrow().is_empty(), "协议错误不应断开连接");
    }

    /// 未声明的能力必须在本地拦下，避免把必然失败的调用发出去。
    #[test]
    fn unsupported_capability_fails_before_send() {
        let host = FakeHost::new(
            "token",
            frames(&[json!({ "op": "welcome", "v": 1, "capabilities": ["selection.get"] })]),
        );
        let mut client = EdenClient::new(config());
        client.ensure_connected(&host).expect("握手应成功");
        let _ = host.take_sent();

        let error = client
            .call(&host, "context.get", json!({}))
            .expect_err("未声明能力应失败");
        assert!(matches!(error, EdenFailure::Protocol(_)));
        assert!(host.take_sent().is_empty(), "不应发送未授权的调用");
    }

    /// 读取超时必须报「Eden 没响应」，而不是当成成功或永久挂起。
    #[test]
    fn read_timeout_is_reported() {
        let host = FakeHost::new("token", Vec::new());
        let mut client = EdenClient::new(config()).with_read_timeout(Duration::from_millis(30));
        let error = client.ensure_connected(&host).expect_err("无响应应失败");

        assert!(matches!(error, EdenFailure::Transport(_)));
        assert!(error.to_string().contains("未在"), "{error}");
    }

    /// 对端关闭后必须释放句柄，下一次调用重新握手。
    #[test]
    fn closed_connection_is_reset_and_reconnected() {
        let host = FakeHost::new(
            "token",
            vec![
                SocketRecv::Data(
                    protocol::encode_line(&json!({ "op": "welcome", "v": 1 })).into_bytes(),
                ),
                SocketRecv::Closed,
                SocketRecv::Data(
                    protocol::encode_line(&json!({ "op": "welcome", "v": 1 })).into_bytes(),
                ),
                SocketRecv::Data(
                    protocol::encode_line(
                        &json!({ "op": "result", "id": 2, "ok": true, "result": {} }),
                    )
                    .into_bytes(),
                ),
            ],
        );
        let mut client = EdenClient::new(config());
        let error = client
            .call(&host, "context.get", json!({}))
            .expect_err("连接已关闭");
        assert!(error.to_string().contains("关闭"), "{error}");
        assert_eq!(*host.connects.borrow(), 1);

        let result = client
            .call(&host, "context.get", json!({}))
            .expect("应自动重连");
        assert!(result.is_object());
        assert_eq!(*host.connects.borrow(), 2);
    }

    /// `bye` 之后必须关闭句柄，避免连接泄漏。
    #[test]
    fn close_sends_bye_and_releases_handle() {
        let host = FakeHost::new("token", frames(&[json!({ "op": "welcome", "v": 1 })]));
        let mut client = EdenClient::new(config());
        client.ensure_connected(&host).expect("握手应成功");
        let _ = host.take_sent();

        client.close(&host);
        assert!(host.take_sent().contains("\"op\":\"bye\""));
        assert_eq!(host.closed.borrow().as_slice(), &[1]);
    }
}
