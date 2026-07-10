# UI 展示插件

该测试插件同时展示以下能力：

- `top`、`right`、`bottom`、`left` 四个停靠插槽。
- 默认隐藏、按需打开的 `dialog` 模态层。
- 多颜色、粗体、斜体、下划线和反色文本。
- Tab 焦点、方向键、Enter、字符键和鼠标点击输入。
- Agent 事件、工具调用和 TUI 输入共享插件状态。
- `ui_showcase_control` 工具从 Agent 侧打开对话框、调整计数或修改消息。

构建 WASM 组件：

```bash
cargo build --offline \
  --manifest-path examples/plugins/ui-showcase-plugin/Cargo.toml \
  --release \
  --target wasm32-wasip2
```

启动展示：

```bash
cargo run -p lucia -- --demo \
  --plugin-manifest examples/plugins/ui-showcase-plugin/plugin.toml
```

启动后按 `Tab` 在主输入区和四个面板间切换。左侧面板使用方向键调整计数，右侧面板按 `d` 或 Enter 打开对话框，底部面板按 `r` 重置状态；对话框按 Esc 或 Enter 关闭。
