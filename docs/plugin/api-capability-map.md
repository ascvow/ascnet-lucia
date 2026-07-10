# 开发者 API 能力地图

Host API 的目标不是预先实现所有插件，而是提供可复用、可授权、可限额和可撤销的机制。具体协议与业务规则继续由插件负责。

## 当前能力

| 领域 | 已提供的通用 API | 典型插件 |
| --- | --- | --- |
| 身份与生命周期 | 可信 plugin ID、activate、deactivate、owner 注入 | 全部插件 |
| Agent Runtime | profile 派生、私有会话续跑、状态、结果、取消、有界消息 | sub-agent、multi-agent、workflow、teammate |
| Agent 贡献 | 工具、developer prompt、结构化事件 | MCP、Skill、命令 |
| 上下文 | 每次模型请求前完整替换上下文 | 压缩、检索、记忆 |
| UI 与输入 | 四向插槽、Dialog、frame、样式、键鼠输入 | 监控、表单、交互工具 |
| 插件协作 | 依赖、能力声明、版本化 JSON 服务 | command provider、公共基础插件 |
| 实例状态 | component 实例内 JSON KV | 计数、短期连接状态 |
| 文件与进程 | 受控只读文件、无 shell 子进程 stdio | Skill 扫描、MCP client |
| 资源限制 | fuel、线性内存、进程数、超时、消息与 Agent 限额 | 全部插件 |
| 会话持久化 | 原生 `SessionStore` trait、内存与文件实现 | 应用、原生扩展 |

## 能力分层

新增需求先归入以下平面，再决定是否需要扩展 ABI：

| 平面 | Host 应提供 | 插件应提供 |
| --- | --- | --- |
| 贡献 | 注册、删除、owner 路由、冲突检测 | 工具、提示、事件、UI 和服务内容 |
| 执行 | 受控 I/O、任务 handle、Agent Runtime | MCP、检索、workflow 等协议与调度规则 |
| 状态 | 隔离、持久化、revision、quota | key、schema、迁移与业务数据含义 |
| 治理 | 身份、授权、限额、撤销、审计 | 最小 capability 声明和错误处理 |

例如 MCP 只需要文件、进程、动态工具与 Secret 引用；Host 不应增加 `register_mcp`。Skill 只需要文件扫描、动态提示与可选 watcher；Host 不应解析 `SKILL.md`。同理，Agent Runtime 提供派生与传输，但不增加 `create_teammate` 或 workflow DSL。

## 后续优先项

| 优先级 | 能力 | 推荐契约 | 主要约束 |
| --- | --- | --- | --- |
| P0 | 持久插件 KV | namespace + revision/CAS | quota、schema、卸载保留策略 |
| P0 | 配置与 Secret 引用 | schema + typed value + opaque secret ref | 版本迁移、最小权限、明文不进入 Guest |
| P0 | HTTP client | Host 执行的结构化请求 | 域名 allowlist、响应上限、超时与重定向策略 |
| P0 | 后台任务 | start/status/result/cancel handle | 卸载撤销、并发、deadline、结果上限 |
| P1 | 受控 Session handle | load/checkpoint/fork/continue，不返回存储对象 | 会话 owner、敏感内容、CAS 冲突 |
| 已完成 | Agent 可恢复运行 | 成功终态私有 Session + continue handle | 不自动定义 teammate 协议、每次运行可取消 |
| P1 | 日志、追踪与指标 | structured log/span/counter/histogram | 字段脱敏、速率限制、owner 注入 |
| P1 | 用量与预算 | token、时间、并发和任务预算快照 | 只读、父子聚合、超限取消 |
| P1 | 文件写入 | Host 分配的临时或数据目录 | quota、原子写、禁止任意主机路径 |
| P1 | 命令与快捷键 | 声明、owner 路由、冲突检测 | 用户可发现、可重新绑定、焦点规则 |
| P2 | 定时器与调度 | timer/job handle | 卸载取消、数量上限、休眠恢复语义 |
| P2 | 流式通道 | bounded stream handle + poll | 背压、帧大小、关闭与取消 |

P0 表示大量插件无法可靠自行实现且必须由 Host 掌握权限边界；P1 表示现有原语可绕行，但统一契约能避免生态分裂；P2 应先通过版本化插件服务试验，确认通用语义后再进入 ABI。

当前 Agent 消息是可信、有界的传输原语，不会自动进入派生 Agent 的模型上下文。一次性 sub-agent、workflow fan-out 和由插件显式驱动的长期 teammate 续跑均可实现；Runtime 不会自动把邮箱消息注入模型上下文，也不会在 Core 中硬编码 teammate 角色。

## API 设计规则

- 暴露稳定 handle 和 DTO，不暴露具体运行时对象。
- plugin ID、principal、sender 和 owner 一律由 Host 注入。
- 所有队列、消息、文件、响应和并发都必须有上限。
- 长任务使用 start/status/result/cancel，不在同步 WASM import 中等待完成。
- manifest 只声明请求范围；Host 服务注册表和应用配置决定实际可用范围。
- 子 Agent 权限只能收缩，不能扩大父模板权限。
- 插件卸载或激活失败必须撤销进程、Agent 和其他 owner 资源。
- JSON ABI 新字段使用默认值；破坏性变更提升 ABI 版本。

## API 准入检查

一个候选能力只有同时满足以下条件才进入 Host API：

1. 至少两类互不相关的插件需要同一机制，而不是同一业务协议。
2. 插件无法在现有文件、进程、服务或 Agent API 上安全实现。
3. Host 能为它定义 owner、capability、quota、cancel 和 unload 清理语义。
4. 契约可以使用稳定 DTO 或 handle 表达，不暴露具体 Rust 运行时对象。
5. 能提供无权限、超限、撤销、并发和 ABI 兼容测试。

未通过准入检查的需求先作为独立插件或版本化插件服务发布。达到复用证据后，再把最小公共部分提升为 Host API。

## 不直接暴露

以下对象不会进入 Guest API：

- `Agent`、`ModelGateway`、`ToolRegistry`、`SessionStore` 原始对象。
- API key、provider 凭证和 Secret 明文。
- Tokio runtime、channel、任务句柄和数据库连接。
- Wasmtime Store、Linker、Instance 和线性内存。
- Ratatui Frame、终端句柄、原始 socket 和任意主机路径。
- 其他插件的内存，以及由 Guest 自报的 caller 或 owner 身份。

遇到新的插件需求时，先判断它需要的是表中的通用机制，还是某个插件自己的协议。只有前者进入 Host API。
