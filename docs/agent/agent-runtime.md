# Agent Runtime

`agent-runtime` 是 Core 之上的机制层。Core 仍只运行单个 ReAct Agent；Runtime 负责安全派生、私有会话续跑、生命周期、限额和身份，workflow、multi-agent、sub-agent 与 teammate 的具体规则仍由应用或插件实现。

## 现有 API 状态

| 需求 | 状态 | 说明 |
| --- | --- | --- |
| 在已有 Session 上继续 | 已满足 | Core 已提供 `run_continue` 和 `run_session` |
| 手工组装独立 Agent | 已满足 | Core 已公开 gateway、options、tools、extension、event sink 和 context loader |
| 稳定派生与权限收缩 | Runtime 新增 | `AgentTemplate` 和 `AgentDeriveConfig` |
| 生命周期与结果 | Runtime 新增 | `spawn`、`continue_agent`、`steer`、`status`、`result`、`wait`、`cancel`、`subscribe` |
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

终态不可覆盖。取消父节点会级联取消后代；重复取消保持幂等。`RuntimeLimits` 约束最大深度、每个父节点的累计子节点数和全局模型运行并发。

`steer` 只接受排队或运行中的自身/后代 Agent。排队阶段最多暂存 32 条消息，Core Agent 启动后按顺序注入；目标进入终态后拒绝实时消息。`subscribe` 为晚加入观察者回放最近 512 条事件，再持续发送实时事件，终态且缓冲耗尽后自然结束。

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

Runtime 的 controller profile 由应用注册，决定基础模型、工具和权限。Plugin Host 还会把 Guest 可见的 spawn profile 映射为受控的 `AgentDeriveConfig`。Guest 只能提交 profile 名称和任务输入，不能直接传入 provider、API key、provider options 或工具权限。

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
