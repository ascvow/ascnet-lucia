# Agent Runtime

`agent-runtime` 是 Core 之上的机制层。Core 仍只运行单个 ReAct Agent；Runtime 负责安全派生、私有会话续跑、生命周期、限额和身份，workflow、multi-agent、sub-agent 与 teammate 的具体规则仍由应用或插件实现。

## 现有 API 状态

| 需求 | 状态 | 说明 |
| --- | --- | --- |
| 在已有 Session 上继续 | 已满足 | Core 已提供 `run_continue` 和 `run_session` |
| 手工组装独立 Agent | 已满足 | Core 已公开 gateway、options、tools、extension、event sink 和 context loader |
| 稳定派生与权限收缩 | Runtime 新增 | `AgentTemplate` 和 `AgentDeriveConfig` |
| 生命周期与结果 | Runtime 新增 | `spawn`、`continue_agent`、`collect`、`steer`、`status`、`result`、`wait`、`cancel`、`subscribe` |
| teammate 邮箱与通信 | 插件负责 | Runtime 不定义邮箱、消息主题、投递、重试或上下文注入 |
| workflow / teammate 业务语义 | 插件负责 | Runtime 不定义步骤 DSL、角色协议或协作规则 |

## 派生模板

`AgentTemplate::from_agent` 复用模型网关、工具实例和通用钩子，但每次派生都会创建新的 Core `Agent`，因此运行控制队列不会在并发任务之间共享。

`AgentPermissions` 当前约束工具集合。子节点只能保持或缩小父节点权限，不能重新获得父节点已移除的工具。allowlist 同时过滤模型可见的工具定义和真实执行入口。

## 生命周期

派生任务使用以下状态：

```text
Queued -> Running -> Succeeded | Failed | Cancelled
```

终态不可覆盖。取消父节点会级联取消后代；重复取消保持幂等。`RuntimeLimits` 约束最大深度、每个父节点的累计子节点数、全局模型运行并发，以及单个 principal 的累计 Agent 总数与可选的累计模型资源上限。`collect` 可以把已终态实例的名额归还 principal，因此累计总数只反映尚未回收的实例。

`steer` 只接受排队或运行中的自身/后代 Agent。排队阶段与 Core Agent 运行阶段的待注入队列都最多保留 32 条消息，启动后按顺序注入；目标进入终态后拒绝实时消息。`subscribe` 为晚加入观察者回放最近 512 条事件，再持续发送实时事件，终态且缓冲耗尽后自然结束。

## 原生调用

```rust
use agent_runtime::{
    AgentPermissions, AgentProfileId, AgentRuntime, AgentRuntimeApi,
    AgentRuntimeProvisioner, AgentSpawnRequest, AgentTemplate, RuntimeLimits,
    RuntimePrincipal,
};

let runtime = AgentRuntime::new(RuntimeLimits::default())?;
let profile = AgentProfileId::new("default-worker")?;
runtime
    .register_profile(
        profile.clone(),
        AgentTemplate::from_agent(&agent),
        AgentPermissions::default(),
    )
    .await?;

let principal = RuntimePrincipal::new("application:workflow-1")?;
runtime.grant_profile(principal.clone(), &profile).await?;
let provisioned = runtime.provision(principal, &profile).await?;

let child = provisioned
    .api
    .spawn(AgentSpawnRequest::new("分析这个任务"))
    .await?;
let outcome = provisioned.api.wait(&child.id).await?;
let follow_up = provisioned
    .api
    .continue_agent(&child.id, "继续检查边界条件".to_string())
    .await?;
```

`wait` 只供原生异步调用方使用。WASM 插件持有 component store 锁时不能长期等待，因此 Guest API 只开放立即返回或非阻塞的 spawn、continue、steer、status、result、events 和 cancel。成功终态的 Session 只保存在 Runtime 内部；continue 只接受目标与新增输入，并继承目标的模板和有效权限。

## Principal 与 profile

Host 为每次受限组件激活创建唯一 `RuntimePrincipal`。principal 拥有独立 controller 和派生树；插件请求不能填写 owner 或 parent。插件卸载时，Host 通过 provisioner 撤销 principal，取消并清理其全部 Runtime 资源。

每个 controller、首次派生和 `continue_agent` 创建的后续 Agent 都计入该 principal 的累计 Agent 总数。父节点深度或子节点数检查失败时，尚未形成 Agent 的临时预留会回滚。每个父节点的累计子节点配额与 principal 的 Agent 总数对齐（默认 128），否则长期交互 controller 会在顺序派生多批成员后永久失败；并发扇出仍由 `max_concurrent_agents` 限制，需要更严格扇出的部署可通过执行策略显式收紧子配额。

`collect` 是回收入口：目标进入终态后，调用方可请 Runtime 释放它的私有会话、终态结果、事件历史和运行选项，并把累计名额归还 principal。长期协作的成员按“每次派活投胎一个新实例、完成后回收”运行，因此累计名额只反映尚未回收的实例，不会成为派活次数的上限。回收不移除谱系锚点，后续派生仍能沿父指针追溯到 controller；回收后该实例不能再续跑，也读不到终态结果，调用方应先读取结果再回收。

派生 Agent 的每个最终 `ModelRequest` 在 provider 调用前可以原子预留三项累计资源：请求次数、provider-neutral 请求的 JSON 序列化字节数和 `max_tokens` 声明的输出 token。三项都是**可选上限**，`None` 表示不设累计配额；交互式 Serve 平面默认全部为 `None`，因为队员需要一直运行到任务完成，模型请求次数不应成为会话寿命上限。设置上限时，字节预算统计的是 adapter 转换前的可信请求，不等同于最终网络 body；计数使用有界 writer，超限时不会先复制完整请求。任一维度超限时 provider 不会收到请求；provider 随后失败也不返还预留，防止失败重试绕过预算。执行策略或 Agent 已设置的单次输出上限优先；两者都未设置时 Runtime 为派生 Agent 注入 20,000 token 的有限默认值。

Evaluation 与 Mutation 等受限平面必须显式设置这三项上限，用于阻断失控候选；Serve 平面的交互式长会话不应只靠调低累计预算来控制成本，成本控制应交给并发上限、单次输出上限与用户取消。

内置 OpenAI 与 Anthropic adapter 把这个 `max_tokens` 继续作为最终 wire 输出硬上限：合并 `provider_options` 后重新覆盖主 token 字段，把已出现的常见顶层 token 别名收窄到同一上限，并把多候选参数固定为一。Anthropic extended thinking 也必须在该总上限内分配思考与可见输出，预算不足时直接拒绝，不会隐式扩容。第三方 adapter 若要保留 Runtime 的累计输出保证，也必须把 `ModelRequest.max_tokens` 作为不可放宽的最终请求上限。

Runtime 的 controller profile 由应用注册，决定基础模型、工具和权限。Plugin Host 还会把 Guest 可见的 spawn profile 映射为受控的 `AgentDeriveConfig`。Guest 只能提交 profile 名称和任务输入，不能直接传入 provider、API key、provider options 或工具权限。

Lucia TUI 的 `worker` profile 使用显式开发工具 allowlist：`list_directory`、`search_files`、
`read_file`、`write_file`、`edit_file`、`shell` 和 `skill_read`（发现 Skill 后才注册）。
队员可以在共享工作区内独立修改并验证，不需要把代码交回主 Agent 代写；不会自动获得 MCP、
递归派生或未来新增工具。该集合仍与父权限取交集，继承主会话的 ExecutionPolicy、文件工具的
WorkspaceGuard，以及已启用的 Host 工具审批/启动门禁。`shell` 是完整原生进程信任，
工作目录限制不构成进程沙箱；环境白名单、超时和共享工具实例的进程并发限制保持不变。

worker 不覆盖主会话的 `max_steps`：主会话配置为 `0`（不设上限）时队员可以一直运行到任务完成。
`continue_agent` 不增加派生深度、不消耗父节点子配额，并继承原成员已经收窄的权限；队员因此可以
反复接收新任务。每次投胎的实例完成后由调用方 `collect`，上下文不足时由它继承的 Context Loader
自行压缩。旧进程和旧成员的权限不会热升级：更新 TUI 后须重启并新建成员。

应用通过可选宿主服务把 Runtime 注入 WASM loader：

```rust
use agent_runtime::AgentDeriveConfig;
use agent_plugin_host::{wasm::WasmPluginHost, PluginHostServices};
use std::{collections::HashMap, sync::Arc};

let services = PluginHostServices::new().with_agent_runtime(
    Arc::new(runtime.clone()),
    profile,
    HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
)?;

let plugin = WasmPluginHost::load_from_manifest_with_services(
    "examples/plugins/agent-runtime-plugin/plugin.toml",
    services,
)
.await?;
```

不调用 `with_agent_runtime` 时，原有 loader 保持纯插件宿主行为；申请 Agent 权限的插件会被明确拒绝，不会获得隐式默认 Agent。

## Teammate 边界

Runtime 的 `AgentId`、谱系和状态可作为 teammate 插件的成员地址与执行状态，但 Runtime 不创建邮箱。teammate 插件自行定义：

- 邮件 DTO、主题和角色身份；
- 队列容量、背压、确认、重试和过期策略；
- 消息的持久化、可见范围与权限；
- 何时把一条消息转换为 `continue_agent` 的新增输入；
- 邮箱事件如何显示在 TUI 或 Agent 事件列表。

插件可以先使用实例状态保存短期队列，并通过版本化 plugin service 提供 send/list/ack 等协议；需要跨重启时使用后续的持久插件 KV。Host 只注入可信插件 owner 和 service caller，不理解 teammate 消息内容。

这种拆分允许不同 teammate 插件实现 actor mailbox、共享频道、黑板或事件溯源等不同模型，同时保持 Core 与 Runtime 不依赖任何一种协作协议。
