# 上下文替换测试插件

该插件声明独占能力 `agent.context-loader`，并在每次模型请求前把完整消息列表替换为一条确定性摘要。它只验证 Lucia 的通用上下文桥接，不实现真实压缩算法。

## 构建

```bash
bun run build:plugin:context
```

## 加载

```bash
bun run build:tui:plugins
target/plugin-tui/release/lucia \
  --demo \
  --plugin-manifest examples/plugins/context-replacement-plugin/plugin.toml
```

运行后模型请求只会收到插件返回的摘要，主事件列表会显示“上下文压缩”分隔信息。
