# Lucia 开发文档

<div class="lucia-intro">
Lucia 是一个边界清晰的 Rust Agent 运行时。Core 只处理模型、上下文、工具、事件和 ReAct 循环；Plugin Host 只处理 ABI、权限与路由；MCP、Skill、上下文压缩等能力由独立 WASM 插件实现。
</div>

<div class="status-line">
  <span>WASM ABI <code>0.6.0</code></span>
  <span>Rust edition <code>2021</code></span>
  <span>插件目标 <code>wasm32-wasip2</code></span>
  <span>JS 工具链 <code>Bun</code></span>
</div>

## 从哪里开始

<div class="api-grid">
  <a href="/guide/quick-start"><strong>嵌入 Agent</strong><span>配置模型、注册工具并运行第一次会话。</span></a>
  <a href="/guide/distribution"><strong>打包 TUI</strong><span>分别构建纯 Core 与完整插件版本。</span></a>
  <a href="/agent/context-loader"><strong>控制上下文</strong><span>裁剪、摘要或完全替换模型实际看到的消息。</span></a>
  <a href="/host/overview"><strong>嵌入 Plugin Host</strong><span>加载 component、管理实例并按 owner 路由工具。</span></a>
  <a href="/plugin/quick-start"><strong>开发 WASM 插件</strong><span>从 Cargo crate、manifest 到 component 构建。</span></a>
  <a href="/plugin/dependencies-services"><strong>复用插件能力</strong><span>声明 SemVer 依赖，通过通用服务 API 组合插件。</span></a>
  <a href="/plugin/tui"><strong>插入主 TUI</strong><span>声明四向插槽、Dialog、样式和输入事件。</span></a>
  <a href="/examples/mcp"><strong>MCP 示例</strong><span>扫描配置、启动 stdio Server 并动态注册工具。</span></a>
  <a href="/guide/plugin-management"><strong>管理插件</strong><span>安装 bundle、校验依赖并选择独占能力 owner。</span></a>
  <a href="/guide/performance"><strong>分析插件性能</strong><span>测量 Host 路由开销和真实 WASM p95。</span></a>
  <a href="/guide/live-testing"><strong>测试真实模型</strong><span>从最小响应逐层验证 ReAct、复杂工具链和插件。</span></a>
</div>

## 一次请求经过哪些层

<div class="arch-flow">应用输入
  -> Agent Core: Session + ContextLoader + ModelGateway
  -> 模型返回 ToolCall
  -> Plugin Host: public tool name -> owner -> local tool id
  -> WASM 插件: 协议或业务实现
  -> ToolResult -> Agent Core -> 模型继续生成</div>

Core 不加载插件，Host 不解析 MCP，插件也不能直接持有 Ratatui `Frame`。每层只通过公开 API 协作，详见[架构边界](/guide/architecture)。

## 五分钟运行

```bash
cargo check --workspace
cargo run -p lucia -- --demo
```

开发 WASM 插件前，先安装 Rust 的 `wasm32-wasip2` target，然后阅读[创建插件](/plugin/quick-start)。
