# Agent 核心能力

本目录集中存放默认运行时所需的原生 crate，Cargo 包名和公开 API 保持不变。

- `kernel`：通用消息、模型网关、ReAct、事件和扩展契约，对应 `agent-core`。
- `tool`：工具类型、执行策略和原生注册表，对应 `agent-tool`。
- `session`：版本化会话、CAS 和存储，对应 `agent-session`。
- `runtime`：Agent 身份、派生、生命周期、权限收缩和资源限额，对应 `agent-runtime`。
- `context`：默认上下文水位、微压缩和模型摘要，对应 `agent-context`。
- `skill`：默认 Skill 发现、索引和正文读取工具，对应 `agent-skill`。

目录归组不改变依赖方向：Kernel 只依赖 Tool，Session 与 Runtime 依赖 Kernel；Context 和 Skill 作为应用装配的原生能力，不把业务规则回灌到 Kernel。
