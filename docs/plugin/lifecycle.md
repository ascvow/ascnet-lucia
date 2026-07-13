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
不等待后续无关插件。Agent 在 `RunStarted` 时冻结当时 Ready 的工具、提示、路由和能力
owner；运行中新增的插件从下一轮生效。独占工具策略 owner 尚未 Ready 时，Host 会阻止
工具调用，避免加载窗口弱化权限。

## 工具方法

- `list_tools`：注册初始静态工具。仅使用动态注册的插件返回空数组。
- `call_tool`：不需要 Host I/O 的旧式执行入口。
- `call_tool_with_host`：需要文件、状态或子进程 API 时覆盖；默认调用 `call_tool`。
- `before_tool`：观察所有工具调用，可允许、阻止或重写。
- `after_tool`：观察最终结果。

Host 在调用 component 前把公开工具名替换为注册时的本地 ID。

## 事件与 UI

- `on_event` 接收 Core 生命周期事件。
- `describe_ui` 返回静态视图声明。
- `render_ui` 根据 Host 分配尺寸渲染一帧；需要轮询宿主状态时覆盖 `render_ui_with_host`，默认仍转发到纯渲染方法。
- `on_ui_input` 接收焦点视图的键盘或鼠标事件。

## 插件服务

- `handle_service` 接收 Host 已按 owner 路由的调用。
- 调用方 ID 由 Host 注入，不能由 Guest 伪造。
- 服务注册、发现和调用见[依赖与服务](/plugin/dependencies-services)。

## deactivate

应用调用 `PluginHost::shutdown` 或 `WasmPluginHost::deactivate` 时触发。插件应终止长驻任务并清理临时贡献。

当前 ABI 要求 component 导出 `deactivate`。组合宿主按加载顺序的反向关闭，使依赖方先于 provider 清理。应用单独移除宿主时不会自动 shutdown，因为调用方可能需要自定义错误策略。
