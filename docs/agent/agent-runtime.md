# Agent Runtime

`agent-runtime` 是 Core 之上的机制层。Core 仍只运行单个 ReAct Agent；Runtime 负责安全派生、生命周期、限额、身份和消息，workflow、multi-agent、sub-agent 与 teammate 的具体规则仍由应用或插件实现。

## 现有 API 状态

| 需求 | 状态 | 说明 |
| --- | --- | --- |
| 在已有 Session 上继续 | 已满足 | Core 已提供 `run_continue` 和 `run_session` |
| 手工组装独立 Agent | 已满足 | Core 已公开 gateway、options、tools、extension、event sink 和 context loader |
| 稳定派生与权限收缩 | Runtime 新增 | `AgentTemplate` 和 `AgentDeriveConfig` |
| 生命周期与结果 | Runtime 新增 | `spawn`、`continue_agent`、`status`、`result`、`wait`、`cancel` |
| Agent 间通信 | Runtime 新增 | 有界邮箱、可信 sender、消息大小与派生树边界 |
| workflow / teammate 业务语义 | 插件负责 | Runtime 不定义步骤 DSL、角色协议或消息内容 |

## 派生模板

`AgentTemplate::from_agent` 复用模型网关、工具实例和通用钩子，但每次派生都会创建新的 Core `Agent`，因此运行控制队列不会在并发任务之间共享。

`AgentPermissions` 当前约束工具集合。子节点只能保持或缩小父节点权限，不能重新获得父节点已移除的工具。allowlist 同时过滤模型可见的工具定义和真实执行入口。

## 生命周期

派生任务使用以下状态：

```text
Queued -> Running -> Succeeded | Failed | Cancelled
```

终态不可覆盖。取消父节点会级联取消后代；重复取消保持幂等。`RuntimeLimits` 约束最大深度、每个父节点的累计子节点数、全局并发、邮箱容量和单条消息大小。

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

`wait` 和阻塞式 `receive` 只供原生异步调用方使用。WASM 插件持有 component store 锁时不能长期等待，因此 Guest API 只开放立即返回的 spawn、continue、status、result、cancel、send 和 `try_receive`。成功终态的 Session 只保存在 Runtime 内部；continue 只接受目标与新增输入，并继承目标的模板和有效权限。

## Principal 与 profile

Host 为每次受限组件激活创建唯一 `RuntimePrincipal`。principal 拥有独立 controller、邮箱和派生树；插件请求不能填写 owner、parent 或 sender。插件卸载时，Host 通过 provisioner 撤销 principal，取消并清理其全部 Runtime 资源。

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

## 消息边界

`AgentMessageRequest` 不包含 sender。Runtime 从身份绑定 API 注入可信 `AgentId` 和 principal，并只允许在同一 owner、同一派生树内投递。邮箱是有界 FIFO，满载时立即返回背压错误。

Runtime 只投递结构化消息，不决定消息如何进入模型上下文。teammate 或 workflow 插件可以把消息解释为任务、回复、控制指令或自定义协议，并自行决定何时启动下一次 Agent 运行。

当前派生 Agent 是一次性运行，成功结果只返回执行摘要。消息传输已可供原生 orchestrator 使用，但 WASM 插件不能把新消息自动注入一个已结束或正在执行的派生 Agent。长期 teammate 应在后续通过受控的 session/continue handle 实现；这属于 Runtime 机制扩展，不改变 Core 的单 Agent ReAct 边界。
