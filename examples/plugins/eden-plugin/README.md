# Eden 编辑器上下文插件

把 Eden 编辑器里「用户此刻选中的那段代码」自动带进下一条消息。

用户在 Eden 里选中一段代码，Lucia 输入框里就会多出一个 `[REF#1]` 标签；发送时标签展开为带
`path` / `lines` / `language` 属性的代码块，随这条消息一起进入模型上下文。删掉标签就是丢弃
这段引用，同一段选区在选区变化前不会被自动补回。

| 项 | 值 |
| --- | --- |
| 插件 ID | `eden` |
| 提供服务 | `context.ref@1.0.0`（`[[provides]] agent.context-ref`） |
| 模型工具 | 无 |
| 能力 | `socket`、`fs_read`（令牌）、`env_read`（三个 Eden 变量） |

## 链路

```
Eden ──AF_UNIX──▶ eden-plugin ──context.ref──▶ 宿主 ──[REF#1]──▶ 下一条用户消息
       ▲                  │
       └──── handshake ───┘
```

- `src/protocol.rs`：NDJSON 分帧、报文与错误码，纯逻辑、可脱离宿主测试。
- `src/client.rs`：读取令牌、连接、握手、按 `id` 配对结果、超时与重连。
- `src/lib.rs`：把 `selection.get` 响应转换成上下文引用，并处理 `context.ref` 服务请求。

插件自身不保留任何「推送」路径：Eden 协议 v1 没有事件通道，WASM 插件也没有后台线程。宿主按
固定节拍调用一次 `context.ref`，插件每次都现取一次选区。Eden 将来支持推送后，只需让插件在收到
事件时更新缓存，契约不变。

## 快照契约

```json
{
  "refs": [
    {
      "id": "selection-9f2c1a7d55e0b413-12-20",
      "title": "App.swift:12-20",
      "body": "<selection path=\"…\" lines=\"12-20\" language=\"swift\">\n…\n</selection>"
    }
  ]
}
```

`id` 只由路径与行号决定：用户继续编辑正文时标签保持同一个，选区移动到别的行才产生新标签。
没有选区时返回 `{"refs":[]}`——引用是增益信息，缺了它不该打扰用户。

## 权限

```toml
[[provides]]
id = "agent.context-ref"
version = "1.0.0"
mode = "multi"

[capabilities]
env_read = ["EDEN_SESSION", "EDEN_SOCKET", "EDEN_TOKEN_FILE"]
fs_read = ["~/Library/Application Support/Eden/agent.token"]

[capabilities.socket]
allow_paths = ["~/Library/Application Support/Eden/agent.sock"]
```

若通过 `EDEN_SOCKET` / `EDEN_TOKEN_FILE` 把 Eden 的路径改到别处，需要同步修改这里的声明，否则
Host 会拒绝连接并说明原因。`[[provides]]` 是注册 `context.ref` 的资格：这条服务的内容会被自动
带进用户下一条消息，不能由任意插件在运行时自行宣布。

## 构建与验证

```bash
bun run build:plugin:eden
bun run test:plugin:eden
```

`test:plugin:eden` 会运行原生单元测试、构建 component，再跑真实 WASM Host 冒烟测试——最后一步
启动一个最小 Eden 服务端，验证握手、环境变量注入、socket allowlist、服务注册与协议编码都在同一
条路径上。
