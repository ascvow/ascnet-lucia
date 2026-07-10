# Agent API

## 构造与模型

| API | 用途 |
| --- | --- |
| `Agent::new(gateway, options)` | 使用调用方组装的网关和运行选项 |
| `Agent::from_model_config(config)` | 从单个 provider 配置创建 |
| `set_model_config` | 替换 provider 和模型选择 |
| `upsert_model_provider` | 增加或替换网关中的 provider |
| `set_model_route` | 切换逻辑 provider 与模型 |
| `gateway` / `gateway_mut` | 高级网关查询与注册 |
| `options` / `options_mut` | 查询或修改运行选项 |

`AgentOptions` 公开 provider、model、system prompt、最大步数、token 上限、温度、推理等级、工具策略和 provider options。

## 组件访问

```rust
agent.set_tools(tool_registry);
agent.tools_mut().register(my_tool)?;

agent.set_extension(extension);
let extension = agent.extension();

agent.set_event_sink(event_sink);
let sink = agent.event_sink();

agent.set_context_loader(context_loader);
let loader = agent.context_loader();
```

这些 API 暴露稳定组件，不暴露内部队列锁、ReAct 中间状态或 provider 私有请求实现。

## 运行入口

| API | 行为 |
| --- | --- |
| `run(input)` | 创建新 Session，设置默认 system，追加 user 消息 |
| `run_continue(session, input)` | 在已有 Session 上追加 user 消息并继续 |
| `run_session(session)` | 原样运行调用方构造的 Session |
| `tool_specs()` | 返回原生工具与扩展工具的去重快照 |

`AgentRun` 返回 `run_id`、最终文本、步数、累计用量和最终 Session。

## 独立控制句柄

`AgentControl` 可以跨任务持有，不需要把整个 Agent 交给输入线程：

```rust
let control = agent.control();

control.steer("停止后续工具，先处理这条消息");
control.follow_up("当前任务结束后再生成变更摘要");

println!("{}", control.pending_steering());
control.clear_follow_ups();
```

Steering 在当前工具完成后注入，并跳过本轮剩余工具。Follow-up 在当前任务正常结束后注入。

需要稳定派生、父子生命周期、私有会话续跑和限额时，使用独立的 [Agent Runtime](/agent/agent-runtime)。teammate 邮箱和编排规则由插件实现，不加入 Core 或 Runtime。

## 外部事件

调用方可以通过 `dispatch_event(event)` 将自定义事件写入当前 sink，并通知扩展观察。扩展随后发布的事件会被刷新为 `AgentEventKind::Extension`，不会再次回调扩展。
