# TUI 与事件展示

插件不接触 Ratatui `Frame`、终端句柄或 ANSI 序列。它返回声明式 `UiFrame`，应用负责布局和渲染。

## 插槽

`UiPlacement` 支持：

- `Top`
- `Right`
- `Bottom`
- `Left`
- `Dialog`
- `Subview`
- `Input`
- `InputPanel`
- `ComposerShelf`

```rust
fn describe_ui(&self) -> Vec<UiDeclaration> {
    vec![UiDeclaration {
        plugin_id: String::new(),
        view_id: "status".into(),
        title: "服务状态".into(),
        placement: UiPlacement::Right,
        size: UiSize { width: Some(32), height: None },
        focusable: true,
        input_triggers: Vec::new(),
    }]
}
```

`plugin_id` 由 Host 注入，插件声明时留空。多个插件占同一插槽时按加载顺序堆叠。

## 帧与样式

`UiFrame` 由 `UiLine` 和 `UiSpan` 组成。`UiStyle` 支持：

- 前景色和背景色
- bold、italic、underlined、reversed
- 16 种便携终端颜色

Host 会验证返回的 `view_id` 与请求一致。空帧或错误只影响对应插件视图。

## 输入

焦点视图收到 `UiInputEvent`：

- key：规范化键名、字符和修饰键
- mouse：down、up、drag、move、scroll 与内容区相对坐标
- focus / blur

Dialog 可见时优先接收输入。普通停靠视图通过 Tab 与主输入区切换焦点。

## 主视图与子视图

Lucia 主视图是导航栈的固定根节点。插件可以声明 `UiPlacement::Subview` 视图类型，再通过 `PluginHostApi::navigate_view` 请求 `Push`、`Replace` 或 `Pop`。Host 只提供导航、所有权校验、幂等去重、渲染和输入路由，不理解 sub-agent、workflow、邮箱或其他业务语义。

`UiPlacement::Input` 用于短时、必须立即处理的交互。视图可见时替换主文本输入区并自动获取按键焦点，隐藏后恢复主输入；适合审批和确认，不适合承载长内容。插件仍只返回声明式文本与样式，具体快捷键语义由插件处理。

`UiPlacement::ComposerShelf` 用于输入框上方的常驻上下文摘要，多个可见视图按加载顺序堆叠。独占 `Input` 可见时隐藏全部输入上方内容；输入触发激活时，`InputPanel` 暂时替换全部 `ComposerShelf`，退出触发后自动恢复。宿主只实现这一布局优先级，不理解计划、工作流或其他插件语义。

```rust
fn describe_ui(&self) -> Vec<UiDeclaration> {
    vec![UiDeclaration {
        plugin_id: String::new(),
        view_id: "agent-detail".into(),
        title: "Agent 详情".into(),
        placement: UiPlacement::Subview,
        size: UiSize::default(),
        focusable: true,
    }]
}

host.navigate_view(UiNavigationRequest {
    request_id: "open-reviewer-1".into(),
    action: UiNavigationAction::Push {
        view: UiViewInstance {
            view_id: "agent-detail".into(),
            instance_id: "reviewer-1".into(),
            title: Some("Reviewer".into()),
        },
    },
})?;
```

`view_id` 标识静态声明的视图类型；`instance_id` 是插件拥有的不透明动态实例键，同一种视图可以同时对应多个实例。Host 会在后续 `UiRenderRequest` 和 `UiInput` 中原样回传 `instance_id`。插件可将它映射到 sub-agent、任务或任意内部对象，但这些映射不会进入 TUI 或 Plugin Host。

同一个插件重复提交相同 `request_id` 时不会重复导航。插件只能 `Replace` 或 `Pop` 自己当前激活的子视图；用户按 Esc 可以执行宿主级返回。重新 `Push` 已存在的同一实例会返回该实例并截断其后的导航层级。

## 输入触发与输入面板

`UiDeclaration.input_triggers` 声明主输入触发前缀。主输入去除前导空白后以任一前缀开头时该前缀激活：宿主把主输入快照（`UiInputEvent::MainInput`，包含完整文本与 UTF-8 字节光标）与无修饰的 Tab、Enter、方向键、Esc 手势转发给该视图，并把 `InputPanel` 视图渲染在输入区上方。触发退出激活后面板整体消失，不依赖插件端状态；无任何插件声明触发前缀时，这些字符没有特殊语义，宿主行为与无插件形态一致。

官方 Command 插件用这一机制实现斜杠命令：它声明触发前缀 `/` 的补全弹层，自己维护命令快照、逐键筛选、参数候选与选中状态；第三方命令的动态候选与执行回调由插件直接经 `host-service-call` 调用 owner 服务完成，不经过宿主中转。

## 宿主动作事件

声明 `capabilities.surface_actions` 的插件可以发布 `ui.host.action` 扩展事件，请求宿主执行基础动作：替换主输入（`set_input`）、新建或清空会话、重载上下文（`reload_context`）、退出应用（`exit`）、恢复会话（`resume_session`）与异步会话查询（`query_sessions`）。请求携带插件内幂等 `request_id`，宿主忽略重复交付；`query_sessions` 完成后宿主调用发起插件的 `reply_service` 服务回送 `UiSessionsReply`。

`/resume` 和 `/sessions` 会打开 Command 插件声明的 `command-session-dialog`。插件负责查询、加载、空结果、错误、选择和关闭状态；宿主只应答当前项目的分页会话摘要，并在收到 `resume_session` 动作后校验 revision、加载完整记录。这样插件无法直接读取会话正文，也不会把 Session 存储契约耦合进 Plugin Host。

## 启动插件状态

插件版 TUI 会先显示首帧并立即开放 Agent，再在后台按依赖顺序渐进加载插件。Host 优先完成工具策略 owner，随后有限并发加载其余插件。加载期间提交普通消息会使用该轮开始时已经 Ready 的工具、提示和路由；本轮执行期间新 Ready 的插件不会改变模型能力，从下一条用户消息起自动可见。触发前缀由插件声明，插件 Ready 前这些字符按普通文本处理。

单个插件的 manifest、依赖或激活失败不会关闭整个插件系统。Host 仅剔除该插件以及必选依赖它的下游插件；可选依赖方和无关插件继续加载。底栏会实时显示待加载、已就绪和失败数量。只有重复稳定 ID、无法确定独占能力 owner 等全局配置错误才会终止后续规划，并保留此前已经 Ready 的插件。独占工具策略 owner 未 Ready 时，Host 会阻止工具调用，禁止利用异步加载窗口绕过策略。

TUI 会在每个插件 Ready 时立即消费其激活事件，并在底部信息栏右侧按稳定计划顺序累积状态和本次加载耗时，例如 `mcp: MCP 插件等待配置 · 420 ms`。全部加载结束数秒后该区域收敛为 `◈ N plugins`，持续显示当前已加载插件数量，不额外占用对话区高度。周期性插件视图渲染在单个后台任务中执行；刷新期间到达的新请求会合并为下一批，不阻塞键盘事件，也不会无限堆积任务。

只有启动激活阶段的事件进入该状态行。工具调用、上下文压缩及其他运行期插件事件仍按 `presentation.target` 进入主事件列表，插件无需为此修改运行期事件协议。

## 主事件列表

插件不需要占用插槽也能发布事件：

```json
{
  "name": "context.compression.completed",
  "data": { "before": 200000, "after": 10000 },
  "presentation": {
    "target": "main_event_list",
    "variant": "divider",
    "tone": "info",
    "text": "上下文压缩"
  }
}
```

Lucia TUI 支持 `text` 与 `divider`，tone 支持 `info`、`success`、`warning`、`error`、`muted`。无界面消费者可以忽略 `presentation`，仍保留 name 和 data。

Rust 插件应使用 `EventPresentation`、`EventPresentationTarget`、`EventPresentationVariant` 和 `EventPresentationTone` 构造展示提示，避免手写容易漂移的 JSON 字段。

交互动作应通过插件视图的 `UiInputEvent` 返回，不把回调函数或 UI 框架对象放进事件 payload。

完整示例见 `examples/plugins/ui-showcase-plugin`；在右侧面板按 `s` 可以创建动态子视图实例。
