# Eden 编辑器上下文插件

通过 Eden 编辑器的 Unix domain socket 让 Agent 知道「用户此刻在编辑器里看着什么」。

| 工具 | 作用 |
| --- | --- |
| `eden_selection` | 读取选区文本、起止行与光标行 |
| `eden_editors` | 列出所有已打开文件 |
| `eden_context` | 读取会话所在 tab 的标题、窗格、`cwd` 与文件树根 |
| `eden_reveal` | 把已打开文件的某一行显示出来 |

## 链路

```
Lucia TUI (Guest) ──host-socket-*──▶ Host ──AF_UNIX──▶ Eden agent socket
       │                                                │
       └── EDEN_SESSION / EDEN_SOCKET / EDEN_TOKEN_FILE ┘
```

- `src/protocol.rs`：NDJSON 分帧、报文与错误码，纯逻辑、可脱离宿主测试。
- `src/client.rs`：读取令牌、连接、握手、按 `id` 配对结果、超时与重连。
- `src/lib.rs`：四个工具的参数校验与结果整形。

## 权限

```toml
[capabilities]
env_read = ["EDEN_SESSION", "EDEN_SOCKET", "EDEN_TOKEN_FILE"]
fs_read = ["~/Library/Application Support/Eden/agent.token"]

[capabilities.socket]
allow_paths = ["~/Library/Application Support/Eden/agent.sock"]
```

若通过 `EDEN_SOCKET` / `EDEN_TOKEN_FILE` 把 Eden 的路径改到别处，需要同步修改这里的声明，
否则 Host 会拒绝连接并说明原因。`fs_read` 与 `socket` 叠加后，本插件全部工具的权限下限是
`unknown`，会进入当前审批模式。

## 构建与验证

```bash
bun run build:plugin:eden
bun run test:plugin:eden
```

`test:plugin:eden` 会运行原生单元测试、构建 component，再跑真实 WASM Host 冒烟测试——最后一步
会启动一个最小 Eden 服务端，验证握手、环境变量注入、allowlist 与焦点仲裁都在同一条路径上。
