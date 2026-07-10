# Agent Runtime 能力展示插件

该插件展示 WASM Guest 如何通过 `PluginHostApi` 的类型化 Agent Runtime API 实现以下能力：

- 读取当前插件 controller 的可信身份；
- 使用 Host 注册并授权的 `worker` profile 派生 Agent；
- 从成功终态 Agent 的私有会话创建后续运行；
- 查询派生 Agent 的状态和终态结果；
- 级联取消派生任务。

## 构建

```bash
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
```

构建产物位于：

```text
target/wasm32-wasip2/release/agent_runtime_plugin.wasm
```

## Host 前置配置

应用必须先为 Plugin Host 注入 Agent Runtime，并注册 controller profile 与名为 `worker` 的派生策略。插件 manifest 只申请 `worker`，因此 Guest 不能选择模型、服务商参数或扩大工具权限；最终权限由 manifest 请求与 Host 注册表共同约束。

## 异步执行约定

`agent_runtime_spawn` 和 `agent_runtime_continue` 只等待任务入队并立即返回 Agent ID，不等待模型执行完成。`continue` 只接收目标 ID 与新增输入，原始 Session、存储对象和模型凭证不会进入 Guest。调用方应保存新 ID，随后通过 `agent_runtime_status` 查询状态，并通过 `agent_runtime_result` 轮询终态结果；不再需要任务时可调用 `agent_runtime_cancel`。

WASM Guest 的工具入口是同步接口，插件内禁止循环阻塞等待 Agent 完成。长时间等待会占用组件实例和 Host 调用线程，也会放大并发任务的排队延迟。轮询频率与超时策略应由 Agent、workflow 或上层应用决定。

## 工具

- `agent_runtime_identity`：返回 controller Agent ID。
- `agent_runtime_spawn`：固定使用 `worker` profile 启动派生 Agent。
- `agent_runtime_continue`：基于成功终态的私有会话启动后续运行。
- `agent_runtime_status`：查询 controller 或其后代的状态与权限快照。
- `agent_runtime_result`：读取幂等终态结果；未完成时返回 `completed = false`。
- `agent_runtime_cancel`：级联取消指定后代任务。

本插件只展示 Agent Runtime 控制面，不实现 teammate 邮箱。teammate 插件应自行定义消息结构、队列、投递、重试和消息注入上下文的规则，并可通过通用插件 service 对外暴露这些能力。

运行真实 WASM 端到端测试：

```bash
bun run test:plugin:agent-runtime
```

测试会注入离线固定模型和 Runtime，加载真实 component，并验证 spawn、result、continue 与卸载撤销链路，不访问外部网络。
