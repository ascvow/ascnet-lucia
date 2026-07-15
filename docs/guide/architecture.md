# 架构边界

## Crate 职责

| Crate | 负责 | 不负责 |
| --- | --- | --- |
| `agent-core` | Session、ContextLoader、ModelGateway、ReAct、工具调用、事件 | WASM、manifest、插件 UI、MCP、Skill |
| `agent-tool` | ToolSpec、ToolCall、ToolResult、原生 ToolRegistry | Agent 循环、插件加载 |
| `agent-session` | 版本化会话记录、CAS、内存与原子文件存储 | 模型配置、Agent 调度、插件状态 |
| `agent-runtime` | Agent 派生、身份、生命周期、私有会话续跑、权限收缩与限额 | workflow、multi-agent、teammate 的编排、邮箱与消息协议 |
| `agent-plugin-protocol` | Host、Guest 与应用共享的宿主无关 UI 数据契约 | owner 路由、WASM 调用、终端渲染 |
| `agent-plugin-host` | ABI、生命周期、权限、贡献注册、owner 路由、UI 契约校验 | 具体扩展协议、业务规则和终端布局 |
| `agent-plugin` | Guest SDK、WIT 绑定、导出宏 | component 加载、终端渲染 |
| `agent-tui` | 应用组装、输入路由、通用声明式 UI 渲染 | 贡献归属、插件协议和具体插件业务规则 |
| 独立插件 crate | MCP、Skill、压缩、业务集成 | 修改 Core 或 Host 语义 |

## 依赖方向

<div class="arch-flow">application
  -> agent-core -> agent-tool
  -> agent-session -> agent-core
  -> agent-runtime -> agent-core
  -> agent-plugin-host -> agent-core
  -> agent-plugin-host -> agent-runtime
  -> agent-tui -> agent-plugin-protocol
wasm guest plugin
  -> agent-plugin -> agent-plugin-protocol -> agent-tool</div>

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

## Hook 边界

三层 hook 可以同时存在，但每层只处理自己的维度：

- Core hook 位于 ReAct 执行链，发布通用生命周期事件并调用扩展契约，不解释插件业务协议。
- Plugin hook 位于扩展实例，处理激活、工具策略、事件观察、服务和 UI 贡献；审批、工作流等完整状态协议由对应插件拥有。
- TUI hook 位于输入与渲染循环，只把宿主无关输入交给 Host，并渲染 Host 返回的通用帧，不读取插件贡献表或复制插件规则。

工具消息渲染遵循单一路径：TUI 提交完整工具调用、状态和可用尺寸；Host 根据可信贡献快照选择 owner 并注入插件与 renderer 身份；插件返回 `UiFrame`。TUI 不知道哪个插件拥有该工具，也不为具体工具写展示分支。
