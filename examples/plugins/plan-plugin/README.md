# Plan 插件

Plan 是 Lucia 官方独立插件，可通过 `lucia plugin install plan` 由用户显式安装。开发目录中也可以通过 `--plugin-manifest examples/plugins/plan-plugin/plugin.toml` 临时加载。

Plan 插件向 Agent 提供两个工具：

- `update_plan`：整体替换当前计划，允许附带本次调整说明。
- `get_plan`：读取当前计划及修订号。

计划项状态为 `pending`、`in_progress` 或 `completed`。插件会拒绝空步骤、重复步骤以及同时存在多个 `in_progress` 项；传入空计划可清空当前计划。计划保存在插件实例状态中，卸载插件后不会写入磁盘。

插件激活时会注入独立的 developer 提示：仅在多步骤、长期或需要呈现进度的任务中维护计划；原生 Skill 能力无需另行安装。

插件在输入框上方声明一个紧凑进度架，最多显示 5 行，超出的任务以 `+N Task` 汇总，已完成任务自动排在未完成任务之后。聚焦后按 `Enter` 或点击可进入完整计划子视图，查看调整说明和全部步骤；计划完成或清空后，进度架自动隐藏。

## 验证

```bash
cargo test --offline --manifest-path examples/plugins/plan-plugin/Cargo.toml
bun run build:plugin:plan
```

构建完成后，可通过 `examples/plugins/plan-plugin/plugin.toml` 加载生成的 WASM component。
