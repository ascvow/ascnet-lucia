---
name: lucia-plugin-development
description: 开发或修改 Lucia WASM 插件、manifest 和插件测试时使用。
---

# Lucia 插件开发

保持 Agent Core、Plugin Host 和业务插件之间的职责边界。插件业务必须位于独立 WASM crate，通过 `PluginHostApi` 注册工具、提示、事件或服务。

修改插件后应构建 `wasm32-wasip2` component，并通过真实 Plugin Host 路由测试验证行为。
