# 测试与调试

## 三层测试

| 层级 | 验证内容 | 建议 |
| --- | --- | --- |
| 插件纯逻辑 | 配置解析、命名、协议转换 | 普通 Rust 单元测试 |
| component 编译 | WIT import/export 和 SDK 宏 | `cargo check --target wasm32-wasip2` |
| 端到端 | Host、权限、进程、动态路由 | 独立 smoke-test crate |

具体插件的端到端测试必须归插件所有，不能放进 Core 或 Plugin Host。

## 编译 component

```bash
cargo check --offline \
  --manifest-path examples/plugins/mcp-plugin/Cargo.toml \
  --lib \
  --target wasm32-wasip2

cargo build --offline \
  --manifest-path examples/plugins/mcp-plugin/Cargo.toml \
  --lib \
  --release \
  --target wasm32-wasip2
```

## 独立 smoke test

```bash
cargo test --offline \
  --manifest-path examples/plugins/mcp-plugin/smoke-tests/Cargo.toml \
  component_discovers_and_calls_stdio_tool \
  -- --ignored --nocapture
```

独立测试 crate 可以依赖 Plugin Host，但 Host 的自身测试不能反向引用具体插件。

Command 等关键官方插件必须在构建 component 后运行独立 smoke test，至少验证真实 Host 加载、服务注册、服务调用和 UI 路由。原生 workspace 单元测试或仅执行 `cargo check --target wasm32-wasip2` 不能替代该门禁。

## 常见加载错误

| 错误 | 检查 |
| --- | --- |
| ABI 版本不支持 | manifest `api_version` 是否为 `0.7.0` |
| 缺少导出 | 插件是否调用 `export_plugin!` |
| 文件无权限 | `fs_read` 是否包含真实路径 |
| 进程启动失败 | `process_exec`、PATH、command 与 cwd |
| 工具名非法 | 仅 ASCII 字母、数字、`_`、`-`，最大 64 字符 |
| 工具重名 | 不同 owner 是否使用了相同公开名称 |
| fuel 用尽 | 是否在 guest 内进行无界循环或重计算 |

上下文替换插件提供可重复运行的真实 component 测试：

```bash
bun run test:plugin:context
```

该命令先离线构建 WASM component，再运行独立 smoke-test，验证完整压缩先使用 Host 固定路由生成模型摘要，随后 Agent 实际模型请求包含摘要与近期原文，并检查扩展事件已经进入事件流。微压缩测试同时验证它不会触发摘要模型调用或展示文本。

子进程调试日志应写 stderr，并在 `ProcessSpec` 中启用 `inherit_stderr`，避免污染协议 stdout。
