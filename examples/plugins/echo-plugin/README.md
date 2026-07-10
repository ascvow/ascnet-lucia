# Echo WASM Plugin / Echo WASM 插件

Build / 构建：

```bash
cd examples/plugins/echo-plugin
cargo build --release --target wasm32-wasip2
```

Run with the demo model / 使用 demo 模型运行：

```bash
cd ../../..
cargo run -p agent-basic-cli -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml \
  "hello from wasm"
```

该示例还声明了一个可聚焦的右侧 TUI 插槽，用于显示工具调用数和 Agent 事件数。通过配置文件加载插件并启动 `lucia` 后，可以按 `Tab` 聚焦该面板，再按 `Enter` 切换帮助内容。

```bash
cargo run -p lucia -- --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml
```
