# 动态工作流插件

该插件在 WASM Guest 内维护可动态扩展的 DAG，并通过 `PluginHostApi` 使用 Host 授权的 `worker` profile 派生 Agent。工作流协议、依赖调度和失败传播都归插件所有；模型、服务商、工具权限与全局资源上限仍由 Host 控制。

## 生命周期

1. 使用 `workflow_create` 创建开放工作流，可同时提供初始节点。
2. 使用 `workflow_add_node` 动态追加只依赖既有节点的新节点。
3. 重复调用 `workflow_tick`，每次调用同步终态并启动当前并行度预算内的就绪节点。
4. 使用 `workflow_seal` 封存工作流；全部节点收敛后，工作流进入 `succeeded` 或 `failed`。

`workflow_tick` 不会等待 Agent 完成。调用方应按自己的轮询和超时策略继续推进，避免同步 component 调用长期占用 Host 线程。

## 失败策略

- `stop`：首次节点失败或取消后停止工作流，取消运行节点并跳过等待节点。
- `continue`：跳过依赖失败节点的下游节点，继续调度独立分支；最终状态仍为 `failed`。

工作流状态只保存在当前插件组件实例内，卸载后不会恢复。需要跨进程恢复时，应由应用接入插件持久 KV 或版本化存储服务。

## 构建与测试

```bash
cargo build --offline --manifest-path examples/plugins/workflow-plugin/Cargo.toml --release --target wasm32-wasip2
cargo test --offline --manifest-path examples/plugins/workflow-plugin/smoke-tests/Cargo.toml
```
