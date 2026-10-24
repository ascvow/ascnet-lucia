# Teammate 协作插件

该插件在 Agent Runtime 控制面之上实现独立的 teammate 业务协议。Runtime 仍只负责可信身份、派生、续跑、实时事件、steering、状态和取消，不保存成员角色或邮箱。

## 行为边界

- `worker` profile 由应用注册并授权，Guest 不能选择模型、凭证或扩大工具权限。
- 首次派生返回的 Agent ID 是稳定成员地址；续跑产生的新 Agent ID 仅作为该成员的当前执行句柄。
- 工具调用的发送者固定为 Host 注入的 controller Agent ID；服务调用的发送者固定为 Host 注入的 `caller_id`，请求不能自行声明发送者。
- 每个 owner 最多创建 16 个成员，每个邮箱最多保留 64 条未确认消息，单条 JSON payload 最大 64 KiB。
- `dispatch` 失败时保留消息并累计尝试次数，最多尝试 5 次；成功后自动确认消息并把当前执行句柄更新为续跑句柄。
- 消息不设置时间过期；确认、取消成员或插件卸载前一直保留。当前版本只使用实例内存，不提供跨重启持久化。
- 版本化服务名为 `teammate.mailbox`，版本为 `1.0.0`。不同 `caller_id` 的成员和邮箱相互隔离。
- 插件不注入常驻 developer 提示，使用时机由 `teammate_spawn` 的模型可见工具描述约束：仅在复杂项目存在互不阻塞的并行子任务，或独立审查确有收益时创建成员。
- 已完成的成员在所属 controller 运行结束时自动回收：只回收状态已终态、所属运行已经结束且邮箱没有未读消息的成员，仍排队或运行中的成员继续保留。回收后 Runtime 句柄、成员目录项和邮箱一起释放，终态结果移入有界回收台账（最多 8 条），`teammate_result` 仍可读取，`teammate_list` 会同时返回 `retired_members`。运行中晚于 controller 结束的成员会在后续运行结束时补收。通过 `teammate.mailbox` 服务创建的成员没有 controller 运行身份，保留到调用方显式 `remove`；需要跨轮继续同一成员的工作时应在当轮内 dispatch，否则重新 spawn。

## 工具

- `teammate_spawn`：为复杂项目中确有并行收益的独立子任务创建带角色的成员并异步执行首个任务；普通单点需求和单一路径审查不应派生。设置 `captain: true` 可将该独立成员指定为队长，每个 owner 最多一个队长，主 Agent 不能作为队长。派生输入会要求文件工具使用工作区相对路径，并在 Host 提供根目录时转换任务中的当前工作区绝对路径。
- `teammate_list`：列出当前 owner 的活跃成员、已回收成员及其当前执行句柄。
- `teammate_status`、`teammate_result`、`teammate_cancel`：操作成员的当前执行句柄。成员被自动回收后，`teammate_result` 从回收台账返回同样的终态结果。
- `teammate_remove`：取消并删除成员及其未确认消息，释放成员配额；对已回收成员只清理台账记录。正常流程不需要手动调用，完成态成员会在运行结束时自动回收。
- `teammate_send`：向成员地址投递消息。
- `teammate_inbox`：列出未确认消息。
- `teammate_ack`：显式确认并删除消息。
- `teammate_dispatch`：把指定消息转换为成功会话的新增输入；调用立即返回新句柄，不等待模型完成。

服务 payload 使用相同操作名：`spawn`、`list`、`status`、`result`、`cancel`、`remove`、`send`、`inbox`、`ack`、`dispatch`。`operation` 之外的字段与对应工具参数一致。

## 团队界面

进入成员视图时，每次刷新最多回放 4096 条已到达事件，并合并同一回复的连续文本增量，避免逐 token 事件挤掉任务正文。初始派生与成功 dispatch 的实际 prompt 都作为用户消息展示；未 dispatch 的邮箱消息不会伪装成已发送 prompt。上下方向键和 PageUp/PageDown 回看，Home 跳到保留历史开头，End 返回最新内容。视图仍受有界历史限制，不替代持久会话存储。

插件声明一个 36 列宽的右侧“团队”摘要视图、一个全屏“团队工作台”和一个成员 Agent 子视图。右侧视图与主输入框底部对齐，使用独立的统计、队长和成员区域展示运行状态；队长来自显式派生的 teammate 成员，并以洋红色区别于普通成员，未创建时显示 `No captain`，不会把主 Agent 显示成队长。点击面板边框、留白或内容均可聚焦，聚焦后按 `Enter` 进入团队工作台。工作台选择成员后按 `Enter` 进入其主界面，视图约每秒拉取一次 Runtime 状态与事件，通过 Guest SDK 的通用 Agent 视图展示可见模型输出、工具调用和运行状态，不展示隐藏推理内容。

成员会话将事件区保持在上方，并在底部固定展示带边框的输入框，支持正常文本编辑和 `Enter` 发送。成员正在排队或运行时，消息通过 Runtime steering 注入当前私有会话；成员成功结束后，消息通过 `continue_agent` 创建后续运行并继续显示在同一成员会话视图。失败或取消的运行没有可复用成功会话，因此会明确拒绝继续发送。

安装插件版 TUI 后，显式安装 Teammate 插件：

```bash
bun run install:tui:plugins
lucia plugin install teammate
lucia
```

启动后按 `Tab` 将焦点切换到右侧“团队”视图，再按 `Enter` 进入团队工作台。开发目录也可以先构建插件，再通过 `--plugin-manifest examples/plugins/teammate-plugin/plugin.toml` 显式加载；TUI 会为它注入受限 Runtime 和 `worker` profile。

## 构建与验证

```bash
cargo test --offline
cargo build --offline --target wasm32-wasip2 --release
cargo test --offline --manifest-path smoke-tests/Cargo.toml
```

应用加载插件前必须向 `PluginHostServices` 注入 Agent Runtime，并注册 controller profile 与 `worker` 派生配置。插件不会在同步 WASM 调用中等待后台 Agent 完成；调用方应轮询 `teammate_result`。
