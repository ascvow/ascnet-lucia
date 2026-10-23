# Plan 插件

Plan 是 Lucia 官方独立插件，可通过 `lucia plugin install plan` 由用户显式安装。开发目录中也可以通过 `--plugin-manifest examples/plugins/plan-plugin/plugin.toml` 临时加载。

Plan 插件向 Agent 提供两个工具：

- `update_plan`：整体替换当前计划，允许附带本次调整说明。
- `get_plan`：读取当前计划及修订号。

计划项状态为 `pending`、`in_progress` 或 `completed`。插件会拒绝空步骤、重复步骤以及同时存在多个 `in_progress` 项；传入空计划可清空当前计划。计划保存在插件实例状态中，卸载插件后不会写入磁盘。

插件不注入常驻 developer 提示，使用时机由 `update_plan` 的模型可见工具描述与插件准入策略共同约束：计划只服务困难或较大的任务，默认不要创建。满足以下任一条才值得建立计划：改动跨多个模块或多个文件且顺序有依赖；任务能拆成 3 个以上可独立验证的阶段；预计多轮长时执行、可能中断或需要持续汇报进度；用户明确要求维护计划。直接问答、代码阅读、单文件小改动、单条命令或单次测试、格式化与重命名、窄范围修复等一次连续执行即可完成的任务不开计划。

准入策略是行为约束而不只是提示：任何非空计划少于 3 个步骤都会被拒绝，工具结果会提示直接完成任务，不要把同一个动作拆成多步凑数。该检查只作用于新的计划更新，升级前保存的小计划仍可恢复、查看和完成，不会被判为损坏状态。

插件在输入框上方声明一个紧凑进度架，最多显示 5 行，超出的任务以 `+N Task` 汇总，已完成任务自动排在未完成任务之后。聚焦后按 `Enter` 或点击可进入完整计划子视图，查看调整说明和全部步骤；计划完成或清空后，进度架自动隐藏。

## 验证

```bash
cargo test --offline --manifest-path examples/plugins/plan-plugin/Cargo.toml
bun run build:plugin:plan
```

构建完成后，可通过 `examples/plugins/plan-plugin/plugin.toml` 加载生成的 WASM component。
