# 插件生命周期

## 调用顺序

<div class="arch-flow">instantiate component
  -> list-tools（初始静态工具）
  -> activate(context)
  -> describe-ui
  -> prompt/list/call/event/ui hooks
  -> deactivate()</div>

## activate

`activate` 收到 `ActivationContext`：可信插件 ID 和 manifest metadata。适合：

- 扫描插件配置。
- 启动长驻子进程。
- 动态注册工具和提示。
- 注册供其他插件复用的服务。
- 初始化实例状态。
- 发布 ready 事件。

返回错误会阻止插件加载，不会产生半初始化实例。

TUI 使用渐进生命周期：每个插件完成 `activate` 与 `describe-ui` 后即原子发布为 Ready，
不等待后续无关插件。Host 先加载独占工具策略 owner 及其依赖，再以最多三个并发任务
加载依赖已经完成的插件；实际完成顺序不会改变计划中的工具和提示顺序。Agent 在
`RunStarted` 时冻结当时 Ready 的工具、提示、路由和能力 owner；运行中新增的插件从
下一轮生效。独占工具策略 owner 尚未 Ready 时，Host 会阻止工具调用，避免加载窗口
弱化权限。

## 工具方法

- `list_tools`：注册初始静态工具。仅使用动态注册的插件返回空数组。
- `call_tool`：不需要 Host I/O 的旧式执行入口。
- `call_tool_with_host`：需要文件、状态或子进程 API 时覆盖；默认调用 `call_tool`。
Host 在调用 component 前把公开工具名替换为注册时的本地 ID。

## 事件与 UI

- `on_event` 是插件参与决策与观察的唯一入口，见下节。
- `describe_ui` 返回静态视图声明。
- `render_ui` 根据 Host 分配尺寸渲染一帧；需要轮询宿主状态时覆盖 `render_ui_with_host`，默认仍转发到纯渲染方法。
- `on_ui_input` 接收焦点视图的键盘或鼠标事件。

## 事件总线

插件通过 `on_event` 参与宿主事件总线的分发，取代 0.7 的 `before_tool` / `after_tool` /
`on_event` 三个专用钩子。

订阅在 `plugin.toml` 中声明，未声明的事件不会投递给插件——订阅同时是投递权限边界：

```toml
[[events]]
name = "core.tool.call"
priority = "highest"      # lowest | low | normal | high | highest | monitor
ignore_cancelled = false  # 事件已被更早的监听者取消时是否跳过本插件
```

一次分发中，监听者按优先级从 `lowest` 到 `highest` 依次执行，同优先级按加载顺序；
`monitor` 最后执行且只读，它的改写与取消都不会生效。插件在 `on_event` 中可以：

- **取消事件**：`event.cancel()`。取消是单调的，首个取消者记录在 `canceller` 中且不可
  被后续监听者改写，因此低优先级插件无法撤销高优先级插件做出的安全拦截。
  取消 `core.tool.call` 即拒绝执行该工具，可用 `event.deny_tool(reason, guidance)` 一步完成。
- **改写事件**：`event.set(field, value)`，或对工具调用使用 `event.rewrite_tool(name, args)`。
  Host 只采纳目标事件白名单内的字段：调用 ID、运行 ID 与权限 scope 由宿主绑定，插件改不动。
- **请求等待**：返回 `EventOutcome::pending(ms)`，Host 会在指定间隔后重新投递同一事件。
  人工审批插件用它在不阻塞 Agent 主循环的前提下等待用户操作；Host 对累计等待设有上限。

Core 事件见 `agent_plugin_protocol::core_events`：`core.run.start`、`core.run.finish`、
`core.turn.start`、`core.turn.finish`、`core.model.request`、`core.model.response`、
`core.tool.call`、`core.tool.result`、`core.message.inject` 与承载全部记录流的
`core.agent.event`。界面事件由 TUI 定义，前缀为 `tui.`。

## 插件服务

- `handle_service` 接收 Host 已按 owner 路由的调用。
- 调用方 ID 由 Host 注入，不能由 Guest 伪造。
- 服务注册、发现和调用见[依赖与服务](/plugin/dependencies-services)。

## deactivate

应用调用 `PluginHost::shutdown` 或 `WasmPluginHost::deactivate` 时触发。插件应终止长驻任务并清理临时贡献。

当前 ABI 要求 component 导出 `deactivate`。组合宿主按加载顺序的反向关闭，使依赖方先于 provider 清理。应用单独移除宿主时不会自动 shutdown，因为调用方可能需要自定义错误策略。
