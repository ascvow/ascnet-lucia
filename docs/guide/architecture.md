# 架构边界

## Crate 职责

| Crate | 负责 | 不负责 |
| --- | --- | --- |
| `agent-core` | Session、ContextLoader、ModelGateway、ReAct、工具调用、事件 | WASM、manifest、插件 UI、MCP、Skill |
| `agent-tool` | ToolSpec、ToolCall、ToolResult、原生 ToolRegistry | Agent 循环、插件加载 |
| `agent-session` | 版本化会话记录、CAS、内存与原子文件存储 | 模型配置、Agent 调度、插件状态 |
| `agent-runtime` | Agent 派生、身份、生命周期、私有会话续跑、权限收缩与限额 | workflow、multi-agent、teammate 的编排、邮箱与消息协议 |
| `agent-plugin-host` | ABI、生命周期、权限、贡献注册、owner 路由、UI 协议 | 具体扩展协议和业务规则 |
| `agent-plugin` | Guest SDK、WIT 绑定、导出宏 | component 加载、终端渲染 |
| 独立插件 crate | MCP、Skill、压缩、业务集成 | 修改 Core 或 Host 语义 |

## 依赖方向

<div class="arch-flow">application
  -> agent-core -> agent-tool
  -> agent-session -> agent-core
  -> agent-runtime -> agent-core
  -> agent-plugin-host -> agent-core
  -> agent-plugin-host -> agent-runtime
wasm guest plugin
  -> agent-plugin -> agent-tool</div>

Core 不依赖 Plugin Host。Guest SDK 不依赖 Host 实现。仓库内插件属于统一 Cargo workspace，但不属于默认成员；插件必须按包名和 `wasm32-wasip2` 目标单独构建，避免宿主 target 编译 component exports。

## 为什么使用 JSON ABI

WIT world 的参数使用 JSON 字符串，Rust 结构体通过 serde 演进。这样可以：

- 保持 component 函数签名稳定。
- 在不重写 WIT record 的情况下增加可选字段。
- 让非 Rust guest 复用同一契约。
- 在 Host 边界统一校验和返回错误信封。

JSON ABI 不意味着忽略类型。Rust Guest SDK 对外仍提供 `ToolSpec`、`ProcessSpec`、`ExtensionEvent` 等强类型接口。

## 扩展能力放在哪里

判断规则只有一个：Agent 执行任何扩展都需要的能力属于 Core/Host API；某类扩展如何工作的规则属于插件。

例如 Host 可以提供文件读取、子进程 stdio、动态工具和事件 API，但不能知道 `tools/list`、`SKILL.md` 或摘要格式。
