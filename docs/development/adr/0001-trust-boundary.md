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

- `agent-core`：ReAct 循环、工具调用、事件派发与 `core.*` 事件的取消语义。
- `agent-event`：取消状态的单调性、优先级排序与不可信监听者的白名单合并边界。
- `agent-tool`：`ToolSpec`、`ToolCall` 契约与原生工具实现。
- `agent-runtime`：`ToolAccess` / `AgentPermissions` 的单向收缩语义。
- `agent-plugin-host`：ABI、生命周期、能力 owner 路由、UI 契约校验。
- Execution Profile 的定义与强制点。

### Ordinary Mutable

由人类正常开发流程修改：文档、TUI 布局、示例插件、构建脚本、Prompt 与 Skill 内容。

## Permission 插件与 Host Enforcement

### 背景

本节记录该决策做出时的状态：**授权完全由插件承担，Host 没有独立兜底。**

工具准入现在是 `core.tool.call` 事件的一次总线分发（`crates/agent-plugins/host/src/live.rs`
中的 `PluginFanout`），实际行为是：

1. 无条件登记的守卫分组最先执行：能力归属仍在解析，或已声明的 `TOOL_POLICY_CAPABILITY`
   owner 尚未进入本次运行快照时，直接取消事件（fail-closed，符合预期）。
2. 按订阅优先级依次把事件扇出给非 policy owner 插件；任一插件取消即记录 `canceller`，
   取消不可被后续插件撤销。
3. policy owner 插件的 `core.tool.call` 订阅被固定提升为 `highest`，因此它在全部普通插件
   之后做最终裁决；其余插件在该事件上最高只能到 `high`。
4. 若**没有任何插件**声明该能力，也没有插件取消事件，则调用放行。

因此移除 permission 插件等于移除全部**工具授权**（哪些工具可调用）。

事件总线本身不改变这一结论：它把"谁在什么顺序上拥有否决权"变成可审计的声明
（manifest `[[events]]` 与优先级），但没有引入 Host 侧的独立授权兜底。

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

## TUI worker 的开发工具授权

默认 `worker` 由 TUI 可信装配层明确授予七个工具：`list_directory`、`search_files`、`read_file`、
`write_file`、`edit_file`、`shell`、`skill_read`。这修正了旧版只读 profile 导致队员无法落盘或验证、
只能让主 Agent 代写的问题；不是把 `ToolAccess` 改成 `All`，MCP、递归派生及新增工具不会自动继承。

这是应用授予的**真实副作用能力扩展**，不是提示词承诺，也不是 Guest 自行声明授权：

- Runtime 仍与父 Agent 权限取交集；子节点不能恢复父级删除的工具。
- Core 的执行策略门禁在模型可见定义和实际调用处仍生效；Evaluation/Mutation 不能执行 Shell。
- 原生工具实例与父模板共享，文件工具继续校验工作区与符号链接边界；不扩大文件能力或路径授权。
- 同一父总线上的 Host 启动期门禁、选定策略 owner 和其他审批规则继续作用于队员调用。
- Shell 拥有当前用户的完整原生进程权限，**固定 cwd 不是沙箱**，也不能宣称脚本只能读写工作区。
  子进程环境仍清空后按既有白名单注入，输出/超时限制与共享实例的进程名额不变；不增加
  Guest `process_exec` 授权、不绕过 manifest 的 Agent profile 检查。
- 并行队员共享真实工作区，不提供事务或自动合并。控制方必须分配不重叠的文件所有权并核验结果，
  不能因“队员隔离”而假定写冲突或命令副作用可回滚。

回归必须既验证生产 TUI profile 的允许集合/父权限交集/Host 拒绝链，也通过真实 TUI → WASM
Teammate → Runtime 队员执行写入、编辑、Shell 验证和续跑；只测试 `AgentDeriveConfig::default()`
会掩盖 TUI 的错误裁剪。已运行的旧进程和旧成员不会热扩权，更新后二次派生必须从新 TUI 进程开始。

## 后果

- 授权和文件系统边界的最终判定必须留在可信 Rust 代码内，WASM 插件只能收紧。
- 新增能力入口时必须复用既有门禁，否则隔离承诺失效。

## 参考

- [架构边界](/guide/architecture)
- [离线检查](/development/checks)
