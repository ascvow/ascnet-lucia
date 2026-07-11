# Plan 插件

Plan 插件向 Agent 提供两个工具：

- `update_plan`：整体替换当前计划，允许附带本次调整说明。
- `get_plan`：读取当前计划及修订号。

计划项状态为 `pending`、`in_progress` 或 `completed`。插件会拒绝空步骤、重复步骤以及同时存在多个 `in_progress` 项；传入空计划可清空当前计划。计划保存在插件实例状态中，卸载插件后不会写入磁盘。

插件同时声明一个只读右侧面板，显示完成进度和每个步骤的状态。

## 验证

```bash
cargo test --manifest-path examples/plugins/plan-plugin/Cargo.toml
cargo build --manifest-path examples/plugins/plan-plugin/Cargo.toml \
  --release \
  --target wasm32-wasip2
```

构建完成后，可通过 `examples/plugins/plan-plugin/plugin.toml` 加载生成的 WASM component。
