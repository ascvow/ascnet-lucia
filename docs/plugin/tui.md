# TUI 与事件展示

插件不接触 Ratatui `Frame`、终端句柄或 ANSI 序列。它返回声明式 `UiFrame`，应用负责布局和渲染。

## 插槽

`UiPlacement` 支持：

- `Top`
- `Right`
- `Bottom`
- `Left`
- `Dialog`

```rust
fn describe_ui(&self) -> Vec<UiDeclaration> {
    vec![UiDeclaration {
        plugin_id: String::new(),
        view_id: "status".into(),
        title: "服务状态".into(),
        placement: UiPlacement::Right,
        size: UiSize { width: Some(32), height: None },
        initially_visible: true,
        focusable: true,
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

## 启动插件状态

插件版 TUI 会在 Agent 首次运行前消费插件激活阶段产生的事件，并在底部信息栏右侧一次性显示按加载顺序排列的插件状态，例如 `mcp: MCP 插件等待配置` 和 `skill: 已加载 1 个 Skill`。数秒后该区域收敛为 `◈ 2 plugins`，持续显示当前已加载插件数量，不额外占用对话区高度。

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

完整示例见 `examples/plugins/ui-showcase-plugin`。
