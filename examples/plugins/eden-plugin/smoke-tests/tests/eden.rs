//! Eden 插件的真实 WASM Host 端到端测试。
//!
//! 这里不复用原生单元测试的假宿主：只有真正实例化 component、由 Host 建立 Unix socket
//! 连接、再让一个最小 Eden 服务端回话，才能证明 ABI 注册、manifest allowlist、环境变量
//! 注入和协议编码在同一路径上都是通的。原生单测与 component 编译都不能替代这一项。

use agent_core::AgentExtension;
use agent_plugin_host::wasm::WasmPluginHost;
use agent_tool::ToolCall;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// 冒烟测试使用的会话 id。
const SESSION: &str = "smoke-session";

/// 一个最小 Eden 服务端：只实现 v1 的握手与结果回包。
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
version = "0.1.0"
api_version = "0.10.0"
wasm = "{}"
description = "冒烟测试用 manifest。"

[capabilities]
env_read = ["EDEN_SESSION", "EDEN_SOCKET", "EDEN_TOKEN_FILE"]
fs_read = ["{}"]

[capabilities.socket]
allow_paths = ["{}"]

[capabilities.tool_scopes]
eden_selection = "network"
eden_editors = "network"
eden_context = "network"
eden_reveal = "network"
"#,
            wasm.to_string_lossy(),
            self.token_path(),
            self.socket_path(),
        );
        std::fs::write(&manifest, text).expect("写出测试 manifest");
        manifest
    }

    /// 启动一个只回一固定结果的服务端任务。
    fn serve(self, focused: bool, result: Value) -> JoinHandle<Observed> {
        tokio::spawn(async move {
            let (stream, _) = self.listener.accept().await.expect("接受插件连接");
            observe(stream, focused, result).await
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
async fn observe(stream: UnixStream, focused: bool, result: Value) -> Observed {
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

    let welcome = json!({
        "op": "welcome",
        "v": 1,
        "session": SESSION,
        "capabilities": ["selection.get", "editors.list", "context.get", "editor.reveal", "ping"],
        "scope": {
            "resolved": true,
            "tab": "zsh ~/proj",
            "tabID": "A636878F",
            "focused": focused,
            "hasEditor": true,
            "cwd": "/Users/me/proj",
            "pane": "terminal"
        }
    });
    write_line(&mut write, &welcome).await;

    let mut calls = Vec::new();
    while let Some(line) = lines.next_line().await.expect("读取客户端帧") {
        let frame: Value = serde_json::from_str(&line).expect("客户端帧应是 JSON");
        match frame["op"].as_str() {
            Some("call") => {
                calls.push(frame.clone());
                let reply = json!({
                    "op": "result",
                    "id": frame["id"],
                    "ok": true,
                    "result": result,
                });
                write_line(&mut write, &reply).await;
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

/// 真实 component 必须完成握手、读取上下文，并在焦点不在本 tab 时退化成文字结果。
///
/// 两个场景合并成一个测试并串行执行：连接参数通过进程环境变量注入，而环境变量是进程全局
/// 状态，并行跑两个测试会让彼此的 `EDEN_SOCKET` 互相覆盖，最终测得的是竞态而不是行为。
#[tokio::test]
async fn component_bridges_eden_protocol() {
    // 场景一：正常读取上下文。
    let first = MockEden::new(&"a".repeat(64));
    point_env_at(&first);
    let manifest = first.write_manifest("context");
    let server = first.serve(
        true,
        json!({
            "tab": "zsh ~/proj",
            "tabID": "A636878F",
            "focused": true,
            "activePane": "terminal",
            "panes": [{ "kind": "terminal", "title": "zsh" }],
            "cwd": "/Users/me/proj",
            "treeRoot": "/Users/me/proj",
            "session": SESSION
        }),
    );

    let host = WasmPluginHost::load_from_manifest(&manifest)
        .await
        .expect("Eden component 应加载成功");
    let tools = host.list_tools().await.expect("读取工具列表");
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "eden_context",
            "eden_editors",
            "eden_reveal",
            "eden_selection"
        ]
    );

    let result = host
        .call_tool(ToolCall::new("call-1", "eden_context", json!({})))
        .await
        .expect("工具路由不应失败")
        .expect("插件应处理自己的工具");
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content["tab"], "zsh ~/proj");
    assert_eq!(result.content["session"], SESSION);

    drop(host);
    let observed = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("服务端任务不应挂住")
        .expect("服务端任务不应 panic");
    assert_eq!(observed.hello["token"], "a".repeat(64));
    assert_eq!(observed.hello["session"], SESSION);
    assert_eq!(observed.calls.len(), 1);
    assert_eq!(observed.calls[0]["tool"], "context.get");

    // 场景二：用户在看别的 tab，reveal 必须退化成文字结果且不发出写请求。
    let second = MockEden::new(&"b".repeat(64));
    point_env_at(&second);
    let manifest = second.write_manifest("reveal");
    let server = second.serve(false, json!({}));

    let host = WasmPluginHost::load_from_manifest(&manifest)
        .await
        .expect("Eden component 应加载成功");
    let result = host
        .call_tool(ToolCall::new(
            "call-2",
            "eden_reveal",
            json!({ "path": "/Users/me/proj/Sources/App.swift", "line": 42 }),
        ))
        .await
        .expect("工具路由不应失败")
        .expect("插件应处理自己的工具");
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content["revealed"], false);
    assert_eq!(result.content["reason"], "user_in_another_tab");

    drop(host);
    let observed = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("服务端任务不应挂住")
        .expect("服务端任务不应 panic");
    assert!(
        observed.calls.is_empty(),
        "焦点在别处时不应发出 editor.reveal：{:?}",
        observed.calls
    );
}
