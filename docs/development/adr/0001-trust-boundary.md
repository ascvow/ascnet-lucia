# ADR-0001 自进化信任边界

- 状态：已接受
- 日期：2026-08-15
- 背景：受控 Prompt 自进化 MVP（M0-02）

本 ADR 定义 Lucia 引入自进化能力后的信任边界：哪些组件不可被自动变异、哪些可以在门禁下变异、变异器与评测器各自能触碰什么。后续所有进化相关改动都以本文为准。

## 决策摘要

1. 系统划分为 Serve、Evolution、Trusted Evaluation 三个平面，Evolution 平面被视为**不可信输入源**。
2. Mutator 一律不得修改安全策略、评测实现、数据集、发布控制器和目标函数。
3. Candidate 一律不得读取 Hidden Dataset 与 Hidden Verifier。
4. 当前正在运行的 Genome 不允许热变更；新 Genome 只在下一次运行生效。
5. 自动 Promote 只在本地显式开启时允许，生产环境默认只能 Recommend。

## 三个平面

```text
Serve 平面            正常为用户执行任务；唯一面向真实用户和真实副作用的平面
Evolution 平面        生成候选；输入是脱敏证据，输出只有数据制品
Trusted Evaluation 平面  判定候选优劣；独占隐藏数据集、Verifier 和 Commit Policy
```

三者的隔离规则：

- Evolution 平面**不能**链接或读取 Trusted Evaluation 的内部实现，只能通过进程边界请求评测并接收报告。
- Trusted Evaluation 平面**不能**被候选内容影响，候选产出的文本一律视为不可信数据，不得当作指令。
- Serve 平面**不能**加载 Mutator。TUI 默认不包含变异能力。

对应到计划中的产物：`lucia-evolve` 属于 Evolution 平面，`lucia-eval` 属于 Trusted Evaluation 平面，两者以进程接口通信。

## 组件分类

分类是本 ADR 的核心约束。任何新增组件都必须落入其中一类。

### Trusted Immutable

自动变异流程绝不能修改，只能由人类通过常规评审改动：

- `agent-core`：ReAct 循环、工具调用、事件派发。
- `agent-tool`：`ToolSpec`、`ToolCall`、`ToolDecision` 契约与原生工具实现。
- `agent-runtime`：`ToolAccess` / `AgentPermissions` 的单向收缩语义。
- `agent-plugin-host`：ABI、生命周期、能力 owner 路由、UI 契约校验。
- Execution Profile 的定义与强制点。
- Verifier 实现、Commit Policy、Release Controller、Audit 写入路径。
- Hidden Dataset 及其加载器。
- 目标函数与评分口径。

### Mutable but Gated

允许自动生成候选，但必须经过 Trusted Evaluation 与 Commit Gate 才能成为 stable：

| 表面 | 引入里程碑 | 说明 |
| --- | --- | --- |
| Task Strategy Prompt | M5 | MVP 阶段**唯一**开放的表面 |
| Context Policy 参数 | M6 | 只含数值与枚举，不含插件源码 |
| Skill 内容与选择规则 | M7 | 新 Skill 默认进入 Quarantine |

未列入本表的任何内容都不是合法变异表面。Candidate Builder 必须在构建期拒绝越界的 Patch，而不是等到评测阶段才发现。

### Ordinary Mutable

由人类正常开发流程修改，不参与自动进化：文档、TUI 布局、示例插件、构建脚本。

### Runtime Data

运行期产生、不属于代码的制品：Episode、Event Stream、CAS Artifact、Evaluation Report、Genome Revision、Release Record、Audit Chain。这些数据**只追加不覆盖**；Archive 中的历史记录不得被后续流程改写。

## Permission 插件与 Host Enforcement

### 现状（截至本 ADR）

必须如实记录：**当前授权完全由插件承担，Host 没有独立兜底。**

`CompositePluginHost::before_tool`（`crates/agent-plugins/host/src/lib.rs:431`）的实际行为是：

1. 依次调用非 policy owner 插件的 `before_tool`，任一返回 `Block` / `CancelRun` 即短路。
2. 最后调用 `TOOL_POLICY_CAPABILITY` 的 owner 插件做最终裁决。
3. 若该 owner 已声明但尚未 Ready，则阻止本轮工具执行（fail-closed，符合预期）。
4. 若**没有任何插件**声明该能力，`policy_owner` 为 `None`，流程直接落到 `Ok(ToolDecision::Allow)`。

因此移除 permission 插件等于移除全部**工具授权**（哪些工具可调用）。

> 该小节描述的是 M0-03 与 M0-04 之前的状态。原生文件工具彼时不接收 workspace
> root、不做 canonicalize、不拒绝 `..` 逃逸，`ShellTool` 完整继承宿主环境变量。
> 这些缺口已由后文的 M0-03、M0-04 落点关闭；工具授权本身仍由插件裁决。

唯一已经存在的可信强制点是 `agent-runtime` 的 `RestrictedExtension`（`crates/agent-core/runtime/src/permissions.rs:251`）。它对派生 Agent 同时过滤 `list_tools`、拦截 `call_tool`，并且**重新校验插件 `Rewrite` 后的工具名**，因此插件无法借 `Rewrite` 提权。但它只作用于派生 Agent 的 allowlist，不构成根 Agent 的文件系统或进程边界。

### 决策

- 授权判定**必须**下沉为 Host 责任。插件可以收紧结果、提供交互式审批 UI，但不能成为唯一的强制点。
- 权限只能单向收缩。任何组件都不得把 `Evaluation` 提升为 `Serve`，也不得恢复上层已移除的工具。
- Host 必须 fail-closed：策略组件缺失或不可用时拒绝执行，而非放行。
- 上述缺口由 M0-03（Execution Profile）与 M0-04（原生文件与进程工具收紧）关闭。在两者完成前，不得开启任何自动 Promote。

### M0-03 落点

`ExecutionPolicy` 定义在 `agent-tool`（依赖叶子），因此 `agent-core`、`agent-runtime` 和
`agent-plugin-host` 共用同一份定义，无需反向依赖。强制点在 `agent-core`：

- `Agent::tool_specs` 过滤策略拒绝的工具，模型看不到它们。
- `Agent::execute_tool_with_hooks` 在插件钩子**之前**和**重写之后**各校验一次，
  因此插件既不能放行被拒工具，也不能借 `Rewrite` 换成被拒工具。
- `RuntimeLimits::clamped_by` 按策略收紧派生深度、子 Agent 数与并发数。

这三处都位于可信 Rust 代码内，WASM 插件经 JSON ABI 通信，不接触 `ExecutionPolicy`
任何类型，因此无法提升自身权限。`AgentOptions::with_execution_policy` 应用
`restrict` 而非覆盖，重复调用只会越来越严格。

### M0-04 落点

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
因此对不可信内容的防线是 **Evaluation 平面直接关闭进程类工具**，而不是约束
shell 的行为。这一点决定了：在引入 OS 级隔离之前，任何 TaskCase 都不应为
Candidate 开放 `shell`。

`ExecutionPolicy::permits_network_access`、`permits_secret_access` 与
`permits_process_execution` 同时校验私有 Profile 和公开请求字段，Evaluation 与
Mutation 无法通过篡改布尔位打开真实能力。当前 Host 尚未提供 HTTP 与 Secret API，
插件 manifest 一旦声明 `http` 或 `secrets`，会在 component 实例化前直接拒绝；
原生 Shell 也在启动操作系统进程前复核同一进程门禁，并清空非白名单环境变量。
每个 WASM 插件使用独立且有限的 Store fuel，真实无限循环 component 的调用会以
`Trap::OutOfFuel` 终止，不会无限占用 Host 执行线程。

Evaluation Runner 已按每个 TaskCase 的 `wall_clock_ms` 包裹完整候选运行，并在超时后
以受信运行结果失败关闭。Serve 平面仍未提供跨完整用户 Run 的统一墙钟上限；Host 也
尚未提供 Secret API。以后由平台开发者新增网络或凭据入口时，必须复用上述最终门禁，
不能只读取公开布尔字段，更不能把能力扩张交给 Evolution Plane。

## Mutator、Evaluator 与 Commit Gate 的权限

| 能力 | Mutator | Evaluator | Commit Gate |
| --- | --- | --- | --- |
| 读脱敏 Episode | 允许 | 允许 | 否 |
| 读原始 Session / 用户工作区 | 否 | 否 | 否 |
| 读 Hidden Dataset | 否 | 允许 | 否 |
| 读 Hidden Verifier 实现 | 否 | 允许 | 否 |
| 写 Candidate Artifact Store | 允许 | 否 | 否 |
| 读取或写入插件源码 | 否 | 否 | 否 |
| 构建、签名或发布插件 Bundle | 否 | 否 | 否 |
| 调用 Plugin Manager 写操作 | 否 | 否 | 否 |
| 写 Dataset / Verifier | 否 | 否 | 否 |
| 修改 Commit Policy | 否 | 否 | 否 |
| 调用 Release Controller | 否 | 否 | 允许 |
| 写 Audit | 否 | 追加 | 追加 |
| 真实网络与生产 Secret | 否 | 否 | 否 |

明确禁止 Mutator 修改：安全策略、评测实现、数据集、发布流程、目标函数。这五项构成"不可自我优化"的核心——允许其中任意一项被变异，等同于允许系统重写自己的成功标准。

Commit Policy 本身是版本化只读制品。修改它属于人类评审动作，且 Policy Version 必须写入 Evaluation Report 与 Audit。

## Candidate 可访问的数据与工具

Candidate 在 Evaluation Profile 下运行，默认拒绝：真实网络、真实 Secret、Shell 与 `process_exec`、workspace 之外的文件、Dataset 目录。

允许的只有：TaskCase 显式声明的输入、Fixture Workspace 内的文件、Evaluation Policy 显式开放的工具、Mock 模型。

两条附加规则：

- Candidate 的输出（包括 Prompt 文本、工具参数、最终回答）在 Verifier 眼中永远是**数据**。Verifier 不得执行、不得解释其中的指令。
- Candidate 不得从评测错误信息中反推隐藏答案。Verifier 返回给 Candidate 侧的失败信息必须是不含期望值的粗粒度分类。

## Hidden Dataset 规则

- 存储在普通 Lucia TUI 二进制**不可达**的位置，不编译进 TUI，不随仓库分发。
- 只由 `lucia-eval` 进程加载，且加载路径与普通 Agent 的文件工具完全隔离。
- Held-out 集由维护者单独维护，或以独立评测包注入。
- 命中 Hidden Dataset 的读取尝试记为 Safety Violation，直接 Reject，并写入 Audit。
- Hidden 结果只以聚合指标形式出现在 Evaluation Report，不回传具体样例。

## Evolution 数据处理策略

由 `agent-evolution-protocol` 承载。该 crate 不依赖 `agent-core`，`agent-core`
也不依赖它，Serve 平面不会因为引入进化能力而链接变异逻辑。

**默认不可用**是贯穿全部默认值的原则：`DataClass` 默认 `Secret`，
`EvolutionEligibility` 默认 `NotEligible`，`RawToolResultPolicy` 默认 `Discard`。
新增字段或新增来源时的安全默认值总是最严格的那个，因此生产 Session **不会**
因为忘记标记就自动成为变异输入。

### 哪些 Session 可以进入 Episode Store

四档资格：

| 资格 | 含义 |
| --- | --- |
| `NotEligible` | 只能本地调试，不进入任何进化流程 |
| `EligibleAfterRedaction` | 脱敏完成前等同 `NotEligible` |
| `EligibleForLocalEvolution` | 可用于本机进化，不得离开本机 |
| `EligibleForSharedEvaluation` | 可用于共享评测，允许离开本机 |

`Sensitive` 与 `Secret` 级数据即使脱敏也不得共享——脱敏可能失败，而凭据外泄不可逆。

### 原始 ToolResult 与保留期

`StoreRaw` 只对 `Public` 与 `Internal` 开放；`Sensitive` 与 `Secret` 至少经过脱敏。
保留期随敏感度递减：`Public` 不限期，`Internal` 180 天，`Sensitive` 30 天，
`Secret` 0 天（即不保留）。

### Mutator 可读字段

Mutator 只看得到"发生了什么形态的失败"，看不到具体内容：可读 `Outcome`、
`FailureClass`、`ToolCallShape`、`PromptArtifactRef`、`RedactedToolResult`、
`Timing`、`Usage`；不可读 `RawToolResult`、`RawModelResponse`、`UserContent`。
这样它无法把用户数据或隐藏答案写进候选 Prompt。

### 隐藏推理不持久化

`HiddenReasoning` 既不可持久化也不可读。它不构成可验证证据，却显著扩大泄漏面。

### 脱敏

在**持久化之前**执行，因此原始凭据不会先落盘再被清理。规则按固定顺序应用，
相同输入必然得到相同输出与相同命中集合，且对已脱敏文本幂等。覆盖 URL 凭据、
`Authorization` 与 `Cookie` 头、键值对凭据、`Bearer` 令牌、服务商令牌字面量
（`sk-`、`ghp_`、`AKIA`、`xoxb-`）、JWT 与私有路径。

规则集带版本号，Episode 记录它所使用的版本。一处刻意的例外：`total_tokens`
之类的键名同样含 "token"，但纯数字值会被保留，否则 Usage 证据会被误删。

### 导出与删除

导出走 `lucia episode export --redacted`（M2-08），只输出脱敏后内容且要求
`permits_mutation_input()` 为真。删除按保留期执行：`RetentionPolicy::is_expired`
判定过期后连同 CAS 制品一并移除。Archive 中的评测记录不受此影响——它们不含
原始内容，只含引用与摘要。

### 强类型标识

全部标识都是 newtype 而非裸 `String`，把 `EpisodeId` 传给需要 `RunId` 的位置
会在编译期失败。两个家族：

- **带前缀标识**：`<prefix>_<8-64 位小写字母或数字>`，正文取自 UUID v4，
  不含时间戳、路径或用户名，标识本身不泄漏内容。
- **内容摘要**：`sha256:<64 位小写十六进制>`。

反序列化同样执行校验，非法值不会静默进入系统。各类型的 `PATTERN` 常量是
跨语言校验的唯一事实来源，`id_json_schema()` 据此生成
`schemas/evolution-ids.schema.json`。该文件已固化在仓库中供 TypeScript 引用，
并由测试比对防止漂移；变更后以 `UPDATE_SCHEMA=1 cargo test -p
agent-evolution-protocol` 重新生成。

## Promote 的两种模式

| | 本地 | 生产 |
| --- | --- | --- |
| 默认模式 | `RecommendOnly` | `RecommendOnly` |
| 可否自动 Promote | 显式开启 `AutoPromoteLocal` 后可以 | 否，必须人工批准 |
| Dirty 构建 | 允许运行，不得自动 Promote | 禁止 |
| 样本不足 | 返回 `Inconclusive` | 返回 `RequireApproval` |

生产环境 Canary 尚未开放；如后续由平台开发者实现，仍必须在 Gate C 全部满足后走独立
人工授权流程。插件不进入该流程：插件更新只属于人工 Plugin Management，不存在自动
Plugin Candidate 或插件 Evolution Canary。

## 运行中的 Genome 不可热变更

一次运行在启动时绑定 `GenomeRevisionId` 并在整个生命周期内固定。理由有三：

1. 运行中替换 Prompt 或 Policy 会使 Episode 失去归因价值——无法判断某个失败该归给哪一版。
2. 热变更是一条提权路径：允许运行中改 Genome，等于允许绕过 Commit Gate 生效。
3. Replay 要求确定性。Genome 变动会让同一 Episode 无法复现。

Promote 只切换 `stable` 别名，对已在运行的会话无效，新 Genome 从下一次运行开始生效。子 Agent 可以使用派生 Genome，但必须记录 Parent Genome，且派生只能收缩权限。插件环境同样在 Session 启动时固定；人工插件更新只建立新的人工基线，不会热替换活跃 Session。

## 后果

- `agent-core` 不得依赖 `agent-evolution`，否则 Serve 平面会被迫链接变异逻辑。
- `lucia-eval` 与 `lucia-evolve` 必须是两个二进制。合并它们会使隐藏数据集进入候选可达范围。
- 在 M0-03 与 M0-04 完成前，Evaluation Profile 的隔离承诺尚无强制手段，此期间只允许人工审阅候选。

## 参考

- [架构边界](/guide/architecture)
- [离线检查](/development/checks)
