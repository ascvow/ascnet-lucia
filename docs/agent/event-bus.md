# 事件总线

Lucia 的解耦点是一条可取消事件总线。Core 只发布事件并遵守取消结果，不知道谁在监听；
界面、Runtime 权限门禁和 WASM 插件都以同一套监听者契约接入，因此新增一类扩展不需要
在 Core 里加钩子。

## 分层

总线原语在 `agent-event` 中定义，不含任何业务语义。各层各自定义事件类型，只共享事件名
和 JSON 形状：

| 层 | 前缀 | 定义位置 |
| --- | --- | --- |
| Agent Core | `core.` | `agent_core::bus` |
| TUI | `tui.` | `agent-tui` 的 `bus` 模块 |
| 插件 | 只订阅，不定义 | manifest `[[events]]` |

## 取消语义

事件的取消状态是 `cancelled` 加 `canceller`：

```json
{ "cancelled": true, "canceller": "permission" }
```

取消是**单调**的：一旦被取消就无法恢复，`canceller` 只记录首个取消者且此后不可修改。
这条规则是安全前提——如果允许撤销，低优先级监听者就能推翻高优先级安全监听者做出的
拦截决定，取消语义也不再能作为权限边界使用。

每个事件被取消时的后果由发布方定义，监听者只负责表达"取消"这一个动作：

| 事件 | 取消后果 |
| --- | --- |
| `core.run.start` | 运行不进入循环，直接以取消终态收尾 |
| `core.turn.start` | 当前轮不再请求模型，运行以取消终态收尾 |
| `core.model.request` | 本次请求不发往服务商，运行以取消终态收尾 |
| `core.tool.call` | 工具不执行，返回权限拒绝结果；`canceller` 即拒绝者 |
| `core.message.inject` | 该条 steering 或 follow-up 不注入会话 |
| `tui.input.key` | 该按键被吞掉，不再路由给命令、插件视图或主输入区 |
| `tui.input.paste` | 该次粘贴被丢弃 |

`core.run.finish`、`core.turn.finish`、`core.model.response`、`core.tool.result`、
`core.agent.event` 与 `tui.run.finish` 是既成事实的通告，发布方忽略它们的取消状态。

## 优先级

一次分发中，监听者按优先级从 `lowest` 到 `highest` 依次执行，同优先级按登记顺序：

| 优先级 | 用途 |
| --- | --- |
| `lowest` / `low` | 最先执行，最容易被后续监听者推翻 |
| `normal` | 默认 |
| `high` / `highest` | 最后仍可改写事件，适合安全拦截 |
| `monitor` | 在全部改写完成后执行的只读观察级，改写与取消都不生效 |

订阅还可以声明 `ignore_cancelled`：事件已被更早的监听者取消时跳过本监听者。

独占的工具策略 owner 插件对 `core.tool.call` 的订阅会被宿主固定提升为 `highest`，
其余插件在该事件上最高只能到 `high`，保证最终裁决者始终最后执行。

## 故障处置

监听者报错不会中断分发，也不会打断 Agent 主循环——这是"单个插件故障不应拖垮整个
会话"的实现点。错误后果由订阅时声明的故障模式决定：

- **观察型**：记录错误后继续分发，事件语义不变。
- **fail-closed**：记录错误并立即以该监听者为 `canceller` 取消事件。工具准入这类安全
  决策点使用该模式，插件崩溃或超时不会被当作放行。

## 信任边界

进程内可信监听者按具体类型直接读写事件字段；不可信监听者（WASM 插件）只能看到 JSON
快照，并通过补丁提交改写。补丁由事件类型自己的白名单合并函数处理，**默认拒绝一切改写**，
因此新增事件在没有主动开放字段前是安全的。

工具调用 ID、运行 ID 与权限 scope 不在任何事件的白名单里。即使白名单出现疏漏，Core 仍会
在分发返回后重新绑定这些可信字段，插件因此无法把结果关联到其他调用或声明未获授予的权限。

## 派生 Agent

派生 Agent 运行在父总线的**子总线**上：父总线的监听者继续参与子 Agent 的决策，两侧监听者
按同一套优先级统一排序；子总线上新增的订阅（如该 Agent 自己的工具 allowlist 门禁）不会
回流影响父 Agent。

## 相关页面

- [工具与事件](/agent/tools-events)
- [插件生命周期](/plugin/lifecycle)
- [WIT API 0.8](/reference/wit)
