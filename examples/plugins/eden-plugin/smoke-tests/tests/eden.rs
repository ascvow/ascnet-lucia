//! Eden 插件的真实 WASM Host 端到端测试。
//!
//! 这里不复用原生单元测试的假宿主：只有真正实例化 component、由 Host 建立 Unix socket
//! 连接、再让一个最小 Eden 服务端回话，才能证明 ABI 注册、manifest 能力声明、环境变量
//! 注入、服务路由与协议编码在同一路径上都是通的。原生单测与 component 编译都不能替代
//! 这一项。

use agent_core::AgentExtension;
use agent_plugin_host::wasm::WasmPluginHost;
use agent_plugin_host::{PluginHost, PluginServiceCall};
use agent_plugin_protocol::{ContextRefSnapshot, CONTEXT_REF_SERVICE, CONTEXT_REF_SERVICE_VERSION};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// 冒烟测试使用的会话 id。
const SESSION: &str = "smoke-session";

/// 一个最小 Eden 服务端：只实现 v1 的握手与选区回包。
struct MockEden {
    /// 存放 socket、令牌与 manifest 的临时目录。
    directory: PathBuf,
    /// socket 文件路径。
    socket: PathBuf,
    /// 令牌文件路径。
    token: PathBuf,
    /// 待接受的监听器。
    listener: UnixListener,
}

/// 服务端观察到的客户端行为。
struct Observed {
    /// 客户端发来的 hello。
    hello: Value,
    /// 客户端发来的全部 call 帧。
    calls: Vec<Value>,
}

impl MockEden {
    /// 创建临时目录、写入令牌并绑定 socket。
    ///
    /// 目录固定在 `/tmp` 并用短名：`sockaddr_un` 的路径上限约 100 字节，macOS 的 `TMPDIR`
    /// 本身就占掉大半，放进去会直接绑定失败。
    fn new(token: &str) -> Self {
        let directory = PathBuf::from("/tmp").join(format!(
            "lucia-eden-smoke-{}",
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        std::fs::create_dir_all(&directory).expect("创建临时目录");
        let socket = directory.join("agent.sock");
        let token_path = directory.join("agent.token");
        std::fs::write(&token_path, format!("{token}\n")).expect("写入令牌文件");
        let listener = UnixListener::bind(&socket).expect("绑定测试 socket");
        Self {
            directory,
            socket,
            token: token_path,
            listener,
        }
    }

    /// 返回 socket 路径字符串。
    fn socket_path(&self) -> String {
        self.socket.to_string_lossy().into_owned()
    }

    /// 返回令牌路径字符串。
    fn token_path(&self) -> String {
        self.token.to_string_lossy().into_owned()
    }

    /// 写出一份只允许本测试路径的 manifest。
    ///
    /// socket 与令牌路径通过环境变量传给插件，因此 allowlist 使用同样的绝对路径；
    /// 这正是用户覆盖 Eden 默认路径时的真实形态。
    fn write_manifest(&self, name: &str) -> PathBuf {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/wasm32-wasip2/release/eden_plugin.wasm");
        let manifest = self.directory.join(format!("{name}.toml"));
        let text = format!(
            r#"[plugin]
id = "eden"
name = "Eden smoke"
version = "0.2.0"
api_version = "0.10.0"
wasm = "{}"
description = "冒烟测试用 manifest。"

[[provides]]
id = "agent.context-ref"
version = "1.0.0"
mode = "multi"

[capabilities]
env_read = ["EDEN_SESSION", "EDEN_SOCKET", "EDEN_TOKEN_FILE"]
fs_read = ["{}"]

[capabilities.socket]
allow_paths = ["{}"]
"#,
            wasm.to_string_lossy(),
            self.token_path(),
            self.socket_path(),
        );
        std::fs::write(&manifest, text).expect("写出测试 manifest");
        manifest
    }

    /// 启动一个只回固定选区结果的服务端任务。
    fn serve(self, selection: Value) -> JoinHandle<Observed> {
        tokio::spawn(async move {
            let (stream, _) = self.listener.accept().await.expect("接受插件连接");
            observe(stream, selection).await
        })
    }
}

impl Drop for MockEden {
    /// 删除临时目录；socket 文件随目录一并移除。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// 处理一条连接：读 hello、回 welcome，然后逐条回 call 结果。
async fn observe(stream: UnixStream, selection: Value) -> Observed {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    let hello_line = lines
        .next_line()
        .await
        .expect("读取 hello")
        .expect("连接应先发 hello");
    let hello: Value = serde_json::from_str(&hello_line).expect("hello 应是 JSON");
    assert_eq!(hello["op"], "hello");
    assert_eq!(hello["v"], 1);

    write_line(
        &mut write,
        &json!({
            "op": "welcome",
            "v": 1,
            "session": SESSION,
            "capabilities": ["selection.get", "editors.list", "context.get", "editor.reveal", "ping"],
            "scope": {
                "resolved": true,
                "tab": "zsh ~/proj",
                "tabID": "A636878F",
                "focused": true,
                "hasEditor": true,
                "cwd": "/Users/me/proj",
                "pane": "terminal"
            }
        }),
    )
    .await;

    let mut calls = Vec::new();
    while let Some(line) = lines.next_line().await.expect("读取客户端帧") {
        let frame: Value = serde_json::from_str(&line).expect("客户端帧应是 JSON");
        match frame["op"].as_str() {
            Some("call") => {
                calls.push(frame.clone());
                write_line(
                    &mut write,
                    &json!({
                        "op": "result",
                        "id": frame["id"],
                        "ok": true,
                        "result": selection,
                    }),
                )
                .await;
            }
            Some("bye") => break,
            _ => continue,
        }
    }

    Observed { hello, calls }
}

/// 写入一行 JSON 并刷新。
async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, message: &Value) {
    let mut line = message.to_string();
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .expect("写入服务端帧");
    write.flush().await.expect("刷新服务端帧");
}

/// 把 Eden 注入的三个变量指向本测试的路径。
fn point_env_at(source: &MockEden) {
    std::env::set_var("EDEN_SESSION", SESSION);
    std::env::set_var("EDEN_SOCKET", source.socket_path());
    std::env::set_var("EDEN_TOKEN_FILE", source.token_path());
}

/// 真实 component 必须注册上下文引用服务、不暴露工具，并返回可用的选区引用。
///
/// 两个场景合并成一个测试并串行执行：连接参数通过进程环境变量注入，而环境变量是
/// 进程全局状态，并行跑两个测试会让彼此的 `EDEN_SOCKET` 互相覆盖，最终测得的是竞态
/// 而不是行为。
#[tokio::test]
async fn component_serves_context_refs_from_eden() {
    let eden = MockEden::new(&"a".repeat(64));
    point_env_at(&eden);
    let manifest = eden.write_manifest("context");
    let server = eden.serve(json!({
        "path": "/Users/me/proj/Sources/App.swift",
        "startLine": 12,
        "endLine": 20,
        "caretLine": 20,
        "text": "func run() {\n    print(1)\n}\n",
        "truncated": false,
        "dirty": true,
        "language": "swift",
        "tab": "zsh ~/proj",
        "inSessionTab": true,
        "focused": true
    }));

    let host = WasmPluginHost::load_from_manifest(&manifest)
        .await
        .expect("Eden component 应加载成功");

    // 选区不再是模型工具：工具表必须为空。
    let tools = host.list_tools().await.expect("读取工具列表");
    assert!(tools.is_empty(), "插件不应暴露工具：{tools:?}");

    let services = host.services().await.expect("读取服务目录");
    let service = services
        .iter()
        .find(|service| service.name == CONTEXT_REF_SERVICE)
        .expect("应注册 context.ref 服务");
    assert_eq!(service.plugin_id, "eden");
    assert_eq!(service.version, CONTEXT_REF_SERVICE_VERSION);

    let response = host
        .call_service(&PluginServiceCall {
            caller_id: "lucia-tui".into(),
            plugin_id: "eden".into(),
            name: CONTEXT_REF_SERVICE.into(),
            payload: json!({}),
        })
        .await
        .expect("服务调用不应失败")
        .expect("目标插件应处理该服务");
    let snapshot: ContextRefSnapshot =
        serde_json::from_value(response).expect("应返回上下文引用快照");
    snapshot.validate().expect("快照必须通过宿主契约校验");

    assert_eq!(snapshot.refs.len(), 1);
    let entry = &snapshot.refs[0];
    assert_eq!(entry.title, "App.swift:12-20");
    assert!(entry.body.contains("func run() {"), "{}", entry.body);
    assert!(entry.body.contains("lines=\"12-20\""), "{}", entry.body);
    assert!(entry.body.contains("language=\"swift\""), "{}", entry.body);

    drop(host);
    let observed = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("服务端任务不应挂住")
        .expect("服务端任务不应 panic");
    assert_eq!(observed.hello["token"], "a".repeat(64));
    assert_eq!(observed.hello["session"], SESSION);
    assert_eq!(observed.calls.len(), 1);
    assert_eq!(observed.calls[0]["tool"], "selection.get");
    assert_eq!(observed.calls[0]["args"]["scope"], "session");
    assert_eq!(observed.calls[0]["args"]["includeText"], true);

    // 场景二：用户没有选区时必须返回空快照，而不是退回光标位置。
    let eden = MockEden::new(&"b".repeat(64));
    point_env_at(&eden);
    let manifest = eden.write_manifest("no-selection");
    let server = eden.serve(json!({
        "path": "/Users/me/proj/Sources/App.swift",
        "caretLine": 7,
        "truncated": false,
        "dirty": false,
        "language": "swift",
        "tab": "zsh ~/proj",
        "inSessionTab": true,
        "focused": true
    }));

    let host = WasmPluginHost::load_from_manifest(&manifest)
        .await
        .expect("Eden component 应加载成功");
    let response = host
        .call_service(&PluginServiceCall {
            caller_id: "lucia-tui".into(),
            plugin_id: "eden".into(),
            name: CONTEXT_REF_SERVICE.into(),
            payload: json!({}),
        })
        .await
        .expect("服务调用不应失败")
        .expect("目标插件应处理该服务");
    let snapshot: ContextRefSnapshot =
        serde_json::from_value(response).expect("应返回上下文引用快照");
    assert!(snapshot.refs.is_empty(), "{snapshot:?}");

    drop(host);
    let observed = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("服务端任务不应挂住")
        .expect("服务端任务不应 panic");
    assert_eq!(observed.calls.len(), 1, "无选区时仍应只查询一次");
}
