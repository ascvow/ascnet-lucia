# 架构演进与修改指导

本文记录 Lucia 当前架构在继续演进时应优先遵循的修改方向。目标不是减少抽象层或追求文件行数，而是在保留既有边界的前提下，降低协议漂移、维护成本和端到端回归风险。

## 1. 保持现有职责边界

以下边界应继续严格维持：

- `agent-core`：仅处理服务商无关的消息、上下文、模型网关、ReAct、工具调用、事件和通用扩展契约。
- `agent-tool`：仅处理通用工具类型、原生工具和注册表。
- `agent-session`：仅处理会话记录、revision/CAS 与存储实现；不得持有模型配置、Agent 调度或插件业务状态。
- `agent-runtime`：仅处理 Agent 身份、派生、权限收缩、资源限额与私有会话续跑；workflow、teammate、邮箱和业务编排仍应属于插件。
- `agent-plugin-host`：仅处理 ABI、组件生命周期、能力鉴权、可信身份注入、贡献注册、owner 路由和宿主无关 UI 协议。
- `agent-plugin`：仅处理 Guest SDK、共享协议类型、WIT 绑定和导出宏；不得依赖 Host 实现或 Ratatui。
- `agent-tui`：仅处理应用组装、输入、渲染、会话操作和受控 surface effect；不得重新实现插件业务规则。
- 独立插件：实现 MCP、Skill、Command、上下文压缩及其他业务协议，并拥有对应端到端测试。

判断新增逻辑归属时，优先回答：**所有 Agent 扩展都会需要这个机制吗？**

- 若答案是“会”，它可能属于 Core 或 Host 的通用 API。
- 若答案是“仅某一类扩展需要”，它应属于对应插件或插件协议。

## 2. 优先治理 ABI 与协议副本

当前插件 UI、服务和生命周期契约分布在 WIT、Guest SDK、Host、TUI 消费逻辑及文档中。它们是后续最容易发生漂移的区域。

### 修改 ABI 时的最小检查清单

当修改以下任意内容时：

- `wit/plugin.wit`；
- `agent-plugin` 内嵌 WIT 或公共 JSON 类型；
- `agent-plugin-host` 的 JSON 解析、绑定或路由；
- UI、Service、Capability、Activation 相关公共 payload；

必须同时完成：

1. 明确这次变更是“加法兼容”还是“破坏性变更”。
2. 新增 JSON 字段时使用 `#[serde(default)]`，必要时配合 `skip_serializing_if`。
3. 不删除、改名或改变既有字段语义；若无法避免，升级对应协议版本并明确迁移策略。
4. 同步更新 WIT、Guest 类型、Host 类型、文档示例与测试 fixture。
5. 增加至少两类兼容测试：
   - 新 Host 解析旧请求；
   - 新 Host/Guest 忽略未知的加法字段。
6. 对新增 import/export 评估旧 Host 与旧 Guest 是否仍可互操作，不要把“JSON 字段可选”误当作“WIT world 自动兼容”。

### 建议的后续收敛方向

- 为 WIT ABI、插件 manifest `api_version`、插件服务协议版本、Session schema、Plugin Manager lock schema 分别维护兼容说明；这些版本维度不要混用。
- 在 CI 中增加 WIT 与 Guest 内嵌 `PLUGIN_WIT` 的一致性检查，避免人工复制后出现遗漏。
- 为关键 JSON payload 建立 golden fixture；Host、Guest 和真实 component smoke test 共用同一份 fixture。
- README 中展示的 WIT world 必须从真实 `wit/plugin.wit` 同步生成或在 CI 中校验，避免示例落后于实际 ABI。

## 3. 为关键官方插件补齐真实 Component 端到端测试

Command 插件已承载命令注册、补全、执行计划、Session Dialog 和受控 surface effect，是当前复杂度最高的官方插件之一。它不能只依赖 Rust 内部单元测试。

建议为 Command 增加独立 smoke test，至少覆盖：

1. 构建 `wasm32-wasip2` component。
2. 用真实 `PluginHost` 加载 `plugin.toml`。
3. 验证激活后七个 Command 服务可在 Host 服务目录中发现。
4. 验证 Host 注入的 `caller_id` 无法被 Guest 伪造。
5. 调用 `command.prepare-execute`，确认 `/resume` 产生受控 session 查询 effect。
6. 通过真实 WIT 调用 `render-ui`、`on-ui-input` 与 `command.surface.update`，确认 Dialog 状态正确变化。
7. 验证卸载后服务目录、回调路由和 UI 路由均被清理。

同样的原则适用于依赖 Host 服务、动态工具注册、ContextLoader 或 Agent Runtime 的官方插件：单元测试验证业务规则，真实 WASM smoke test 验证 ABI 与路由。

## 4. 控制大模块的认知范围

不要只因为文件较长而拆分；但当一个文件同时包含多种独立变化原因、多个协议边界或无法局部测试的状态机时，应按职责拆分。

当前优先关注的区域：

| 区域 | 推荐拆分依据 |
| --- | --- |
| `agent-tui/src/` | `application`、`app_state`、`session_coordination`、`command_surface`、`conversation` 与 `tui` 分别拥有启动循环、状态机和渲染职责 |
| `agent-plugin-host/src/wasm/` | `engine`、单 component 宿主与 `loader` 分别拥有 Store 初始化、WIT 路由和容错加载 |
| `agent-session/src/` | `protocol`、`memory`、`file_store`、`file_lock`、`summary_index` 与测试分别维护协议和存储状态机 |
| `examples/plugins/command-plugin/src/command/` | `registry` 与 `surface` 分别维护注册解析、内建命令和 Session Dialog 状态机 |
| `agent-runtime/src/` | `identity`、`permissions`、`protocol` 与 `runtime` 分别维护身份、派生授权、公共控制面和生命周期调度 |

拆分后应保持：

- 对外公共 API 尽量仍由原 crate 根模块统一 re-export；
- 不为拆分而新增跨 crate 依赖；
- 每个模块有清晰所有者和独立测试入口；
- 不把插件业务规则搬回 Host 或 TUI。

## 5. 继续强化会话存储语义

`agent-session` 的 Session ID 校验、CAS、原子替换、跨进程协作锁和摘要索引设计应保持不被削弱。

后续修改时重点保护：

- `SessionId` 的路径安全校验与反序列化校验；
- 同一锁周期内完成 revision 读取、CAS 验证、记录写入和索引更新；
- 临时文件与目标文件在同文件系统内，以维持 rename 原子性；
- 符号链接拒绝策略；
- 索引损坏、缺失和旧会话目录的可恢复重建；
- TUI 首次用户输入先持久化、最终保存冲突时分叉而非覆盖的语义。

若未来增加 SQLite、远端数据库或对象存储实现，必须保持 `SessionStore` 的 revision/CAS 契约，不应由不同后端各自定义冲突行为。

## 6. 保持可信调用链

对于跨插件服务、Command callback、surface effect、Agent Runtime 与插件 UI 导航，必须区分：

- Guest 或模型提供的业务数据；
- Host 注入的可信身份、owner、权限、资源上限和生命周期状态；
- TUI 可以执行的受控动作。

具体要求：

- owner、caller、profile、权限、实例归属等服务端已知字段必须由 Host 注入或收窄。
- TUI 不应根据插件输入直接选择 callback owner、service 或 handler。
- 插件只能替换、注销或关闭自身拥有的资源。
- `process_exec` 继续视为原生进程信任，不应被描述为 WASM 沙箱能力。
- 新增敏感能力时，必须同时更新 manifest 约束、结构限制、审计信息、威胁模型和回归测试。

## 7. 构建、发布与文档一致性

当前纯 Core 与插件版的分发边界应继续保持：

- 默认 TUI 包含 `plugins` feature；
- `--no-default-features` 构建纯 Core TUI；
- 纯 Core 依赖树不应出现 `agent-plugin-host`、`wasmtime` 或 `wasmtime-wasi`；
- 官方插件安装应先原子发布 WASM，最后发布 `plugin.toml`。

修改 feature、安装或分发行为时，至少验证：

```bash
cargo test -p lucia --no-default-features
cargo test -p lucia --features plugins
cargo tree -p lucia --no-default-features -e normal
```

同时检查：

- 文档中的默认 feature 与 Cargo 配置一致；
- 文档中的 crate path 与实际目录一致；
- README 中的 WIT、脚本命令和官方插件列表与实现一致；
- 插件版与纯 Core 版的产物名称、安装覆盖行为已明确说明。

## 8. 推荐的提交前验证矩阵

按修改范围选择最小充分验证，而不是每次盲目运行所有命令：

| 修改范围 | 至少验证 |
| --- | --- |
| Core / Tool | 对应 crate 测试，必要时 `cargo test --workspace` |
| Session | `cargo test -p agent-session`，重点检查 CAS、索引和文件锁测试 |
| Host / Guest SDK / WIT | `cargo test -p agent-plugin`、`cargo test -p agent-plugin-host`，以及对应真实 component smoke test |
| TUI | `cargo test -p lucia --no-default-features` 与 `cargo test -p lucia --features plugins` |
| Command 协议或插件 | `cargo test --manifest-path examples/plugins/command-plugin/Cargo.toml --workspace`，并构建 wasm32-wasip2 component |
| 官方插件 | 对应插件独立 workspace 测试、component 构建、smoke test |
| 文档 / 构建脚本 | `git diff --check`、相关构建命令及文档构建 |

最后，任何修改都应遵守一个核心原则：**不要通过把复杂度移动到错误层来换取局部代码变短。**
