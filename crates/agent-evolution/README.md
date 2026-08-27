# Agent 进化系统

本目录集中存放可信进化、评测和稳定协议 crate，Cargo 包名和公开 API 保持不变。

- `protocol`：Genome、Episode、候选、回执和版本化数据契约，对应 `agent-evolution-protocol`。
- `engine`：候选构建、证据归档、晋升、回滚和进化编排，对应 `agent-evolution`。
- `evaluation`：独立评测、Commit Gate、部署验证和可信报告，对应 `agent-evaluation`。

协议层不依赖 Engine 或 Evaluation；Evaluation 通过稳定协议和受控存储验证真实运行证据，不把判定逻辑放回 TUI。
