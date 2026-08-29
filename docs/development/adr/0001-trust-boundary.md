# ADR-0001 执行信任边界

- 状态：已接受
- 日期：2026-08-15

本 ADR 定义 Lucia 的执行信任边界：哪些组件构成可信强制点、插件能收紧到什么程度、原生工具的文件与进程边界如何强制。

## 决策摘要

1. 工具授权必须由 Host 强制，插件只能进一步收紧，不能成为唯一强制点。
2. 权限只��单向收缩，任何组件都不得放宽上层已移除的能力。
3. 强制点缺失或不可用时 fail-closed，拒绝执行而不是放行。

## 组件分类

分类是本 ADR 的核心约束。任何新增组件都必须落入其中一类。

### Trusted Immutable

安全语义的强制点，只能由人类通过常规评审改动：

- `agent-core`：ReAct 循环、工具调用、事件派发。
- `agent-tool`：`ToolSpec`、`ToolCall`、`ToolDecision` 契约与原生工具实现。
- `agent-runtime`：`ToolAccess` / `AgentPermissions` 的单向收缩语义。
- `agent-plugin-host`：ABI、生命周期、能力 owner 路由、UI 契约校验。
- Execution Profile 的定义与强制点。

### Ordinary Mutable

由人类正常开发流程修改：文档、TUI 布局、示例插件、构建脚本、Prompt 与 Skill 内容。

## Permission 插件与 Host Enforcement

### 背景

本节记录该决策做出时的状态：**授权完全由插件承担，Host 没有独立兜底。**

`CompositePluginHost::before_tool`（`crates/agent-plugins/host/src/lib.rs:431`）的实际行为是：

1. 依次调用非 policy owner 插件的 `before_tool`，任一返回 `Block` / `CancelRun` 即短路。
2. 最后调用 `TOOL_POLICY_CAPABILITY` 的 owner 插件做最终裁决。
3. 若该 owner 已声明但尚未 Ready，则阻止本轮工具执行（fail-closed，符合预期）。
4. 若**没有任何插件**声明该能力，`policy_owner` 为 `None`，流程直接落到 `Ok(ToolDecision::Allow)`。

因此移除 permission 插件等于移除全部**工具授权**（哪些工具可调用）。

> 该小节描述的是执行策略与工作区守卫落地之前的状态。原生文件工具彼时不接收 workspace
> root、不做 canonicalize、不拒绝 `..` 逃逸，`ShellTool` 完整继承宿主环境变量。
> 这些缺口已由后文的落点关闭；工具授权本身仍由插件裁决。

唯一已经存在的可信强制点是 `agent-runtime` 的 `RestrictedExtension`（`crates/agent-core/runtime/src/permissions.rs:251`）。它对派生 Agent 同时过滤 `list_tools`、拦截 `call_tool`，并且**重新校验插件 `Rewrite` 后的工具名**，因此插件无法借 `Rewrite` 提权。但它只作用于派生 Agent 的 allowlist，不构成根 Agent 的文件系统或进程边界。

### 决策

- 授权判定**必须**下沉为 Host 责任。插件可以收紧结果、提供交互式审批 UI，但不能成为唯一的强制点。
- 权限只能单向收缩。任何组件都不得把 `Evaluation` 提升为 `Serve`，也不得恢复上层已移除的工具。
- Host 必须 fail-closed：策略组件缺失或不可用时拒绝执行，而非放行。

### 执行策略落点

`ExecutionPolicy` 定义在 `agent-tool`（依赖叶子），因此 `agent-core`、`agent-runtime` 和
`agent-plugin-host` 共用同一份定义，无需反向依赖。强制点在 `agent-core`：

- `Agent::tool_specs` 过滤策略拒绝的工具，模型看不到它们。
- `Agent::execute_tool_with_hooks` 在插件钩子**之前**和**重写之后**各校验一次，
  因此插件既不能放行被拒工具，也不能借 `Rewrite` 换成被拒工具。
- `RuntimeLimits::clamped_by` 按策略收紧派生深度、子 Agent 数与并发数。

这三处都位于可信 Rust 代码内，WASM 插件经 JSON ABI 通信，不接触 `ExecutionPolicy`
任何类型，因此无法提升自身权限。`AgentOptions::with_execution_policy` 应用
`restrict` 而非覆盖，重复调用只会越来越严格。

### 工作区守卫落点

`WorkspaceGuard`（`agent-tool`）把 `FilesystemScope` 从声明变成强制：

- 所有原生文件工具的路径都先经 `resolve_existing` / `resolve_new` 解析，
  再使用返回的规范路径，而不是模型给出的原始字符串。
- 逃逸防护依赖 `canonicalize`，它同时展开 `..` 与 symlink，因此指向工作区外的
  链接在包含性检查之前就已还原为真实路径。
- 新建路径逐级上溯到最近的已存在祖先再拼回，其中出现 `..` 直接拒绝。
- 相对路径针对工作区根解析，不随进程 cwd 漂移。
- 读、写、创建、删除是四项独立能力；`search_files` 递归时逐条目校验真实路径。

Shell 与进程：工作目录固定在工作区内，`env_clear` 后按白名单重新注入
（Secret 默认不进入子进程），输出按流截断，超时后按进程组回收整棵进程树。

TUI 的工作区固定为启动目录。这是相对既有行为的**收紧**：原生工具不再能读写
启动目录之外的路径。

**残留限制（重要）**：`shell` 一旦获准执行，其内部命令不受工作区约束——
`sh -c "cat /etc/passwd"` 仍然可行。固定 cwd 只影响相对路径解析，不构成沙箱。
真正的进程隔离需要 OS 级手段（容器、seccomp、sandbox-exec），不在当前范围。
因此对不可信内容的防线是 **受限执行策略直接关闭进程类工具**，而不是约束
shell 的行为。

`ExecutionPolicy::permits_network_access`、`permits_secret_access` 与
`permits_process_execution` 同时校验私有 Profile 和公开请求字段，受限平面
无法通过篡改布尔位打开真实能力。当前 Host 尚未提供 HTTP 与 Secret API，
插件 manifest 一旦声明 `http` 或 `secrets`，会在 component 实例化前直接拒绝；
原生 Shell 也在启动操作系统进程前复核同一进程门禁，并清空非白名单环境变量。
每个 WASM 插件使用独立且有限的 Store fuel，真实无限循环 component 的调用会以
`Trap::OutOfFuel` 终止，不会无限占用 Host 执行线程。

Serve 平面仍未提供跨完整用户 Run 的统一墙钟上限；Host 也尚未提供 Secret API。以后
新增网络或凭据入口时，必须复用上述最终门禁，不能只读取公开布尔字段。

## 后果

- 授权和文件系统边界的最终判定必须留在可信 Rust 代码内，WASM 插件只能收紧。
- 新增能力入口时必须复用既有门禁，否则隔离承诺失效。

## 参考

- [架构边界](/guide/architecture)
- [离线检查](/development/checks)
