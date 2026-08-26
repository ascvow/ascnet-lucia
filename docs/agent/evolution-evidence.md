# Evolution 可证据化运行

Goal A 的证据链由 `agent-evolution-protocol` 与 `agent-evolution` 共同提供：前者定义
稳定 Schema 与 Genome 行为摘要，后者实现不可变 Genome Store、本地 Artifact CAS、只追加
Episode Store、脱敏 Recorder、运行监督、失败归因和 Protocol Replay。Serve Core 不依赖
Evolution crate，未装配 Recorder 时原有运行方式保持不变。

## Genome 完整性

`AgentGenome::digest` 只序列化行为字段，并在计算 SHA-256 前校验 schema、排序、重复项、
Prompt 层级与 capability owner。修订 ID、父版本、变异来源、创建时间和描述位于
`GenomeMetadata`，不参与行为摘要；同一行为可以由不同 lineage 生成多个 Revision，但共享
同一个 `GenomeDigest`。

`FileGenomeStore` 使用 create-new 语义只追加 `GenomeRevision`。读取时会重新计算行为摘要并
与记录中的声明值比较，同时拒绝符号链接、ID 与文件名不一致或已存在修订覆盖。应用层只能
把已经通过 `GenomeStore::get` 验证的 Revision ID 交给 Recorder，不能临时生成一个 ID
冒充真实 Genome。

## 运行绑定

每次需要形成证据的运行必须先创建 `EpisodeRecorderConfig`。配置会预先生成唯一
`EpisodeId` 和 `RunId`，并要求调用方同时固定 `SessionId` 和 `GenomeRevisionId`：

```rust
use agent_core::{Agent, CompositeEventSink, Session};
use agent_evolution::{EpisodeRecorder, EpisodeRecorderConfig};
use agent_evolution_protocol::GenomeRevisionId;
use std::sync::Arc;

let config = EpisodeRecorderConfig::online(
    "session-1",
    GenomeRevisionId::new("grev_0123456789abcdef0123456789abcdef")?,
);
let run_id = config.run_id.clone();
let recorder = Arc::new(EpisodeRecorder::new(config, artifacts, episodes));
let mut sinks = CompositeEventSink::new();
sinks.push(recorder);
let agent = agent.with_event_sink(Arc::new(sinks));

let run = agent
    .run_session_with_id(Session::new(), run_id.to_string())
    .await?;
# Ok::<(), anyhow::Error>(())
```

`EpisodeRecorder` 会拒绝与预绑定 `RunId` 不同的首事件，也会拒绝把多个运行写进同一
Episode。Episode Header、Incident 和 Outcome Revision 共享预分配的 `EpisodeId`；公开
`EpisodeEvent`、对应 Event Envelope 及 Incident 证据共享 Recorder 分配的 `EventId`。
`run_session_with_id` 只是一项 Core 通用机制，不解析 Genome 或 Episode。

TUI 的 Evidence 装配默认关闭。启用时，启动阶段先从
`<evidence-root>/genomes/<revision-id>.json` 读取并验证不可变 Revision；任一真实主会话在
用户输入成功写入 Session Store 后预登记 Recorder，再把同一个 Run ID 传入 Core。正常
`RunFinished`、取消、步骤预算耗尽和基础设施错误都会显式收敛并释放路由。证据写入失败会
报告为运行完成错误，不会被静默忽略。

插件 Runtime 的子 Agent 不继承 TUI 主会话 Recorder。TUI 在创建 Runtime 时注入可信
`RuntimeRunObserver`；Runtime 在 Core 启动前向观察器提交 Host 维护的 `AgentId` 与
`AgentLineage`，取得固定 Run ID、独立事件 sink 和一次性 finalizer。每个子运行使用
`runtime-agent:<AgentId>` 作为独立 Session ID，并绑定启动时已经验证的同一 Genome
Revision。正常完成记为 `Unverifiable`，取消记为 `Cancelled`，失败则根据已记录事件推断
`BudgetFailure` 或 `InfrastructureFailure`。

Runtime 的取消和 principal 撤销不会直接 abort 持有 finalizer 的监督任务。取消请求会中断
Core future，监督任务随后先关闭 Episode、释放 Hub 路由，再写入 Runtime 终态；principal
撤销还会等待这些收敛完成后返回。Evidence 未启用时不注入观察器，原有 Runtime 行为和 API
保持兼容。

Execution Lineage 与 Genome Lineage 是两套独立语义：前者只描述当前进程中 Agent 的父子
身份、根节点和派生深度，用于把 Runtime Run 关联到执行主体；后者通过
`GenomeRevision.parent_revision_ids` 描述跨版本 Mutation、Evaluation、Promotion 和
Rollback。Episode 同时引用执行会话与 Genome Revision，但不得用任一 lineage 字段替代
另一套关系。

## 数据处理与终态

Recorder 默认在 `RunFinished` 时自动收敛：事件流先写入 SHA-256 CAS，随后只追加 Episode
Header。TUI 会关闭自动收敛并在 Core 返回后统一决定终态，避免后续 UI/JSONL sink 的错误
被提前记成正常完成。默认 `EpisodeDataPolicy` 为 `NotEligible` 且丢弃工具结果正文；模型隐藏
思考增量永不持久化。其余 JSON 字符串经过确定性脱敏，Episode 记录实际规则版本。

Recorder 只把脱敏并按数据策略收窄后的公开载荷交给 `RunSupervisor`。Supervisor 生成的
Event Envelope、Incident 和初始 Outcome Revision 分别进入 CAS，其引用保存在 Episode
Header 的 `supervision` 字段中，进程重启后仍可从同一 Episode 找回完整监督证据。

在线运行没有可信 Verifier 时，正常完成默认记为 `Unverifiable`，不能推断为任务成功。
取消记为 `Cancelled`。模型服务、工具环境或存储导致运行未产生 `RunFinished` 时，应用层
应调用 `recorder.finish(Outcome::InfrastructureFailure)`，避免把基础设施问题统计为候选
能力失败。

## 存储不变量

- `FileArtifactStore` 按 SHA-256 内容寻址，读取时重新验证摘要，提交不覆盖已有内容。
- `FileGenomeStore` 只追加通过行为摘要校验的 Revision，读取时重新验证 ID 与摘要。
- `FileEpisodeStore` 使用 `create_new` 语义，只追加而不更新历史 Episode。
- Episode 查询支持按 `Outcome` 和 `session_id` 过滤。
- 原始工具正文只有在数据分级允许且策略显式设为 `StoreRaw` 时才会进入事件制品。
- `FileOutcomeRevisionStore` 按 Episode 保存单调序号记录；新修订必须通过 `supersedes`
  指向最新修订，并发竞争同一后继时只允许一个写入者提交。
- `FileEvolutionOutbox` 的 JSON 记录不可变；消费状态写入独立 `.consumed` 标记，不覆盖
  原始记录，并拒绝路径逃逸和符号链接制品。

## 延迟反馈与 Outcome 修订

`FeedbackProcessor` 是 Trusted Evaluation/Evidence Plane 的应用服务，不作为 Agent Tool 或
普通插件服务暴露。调用方必须同时传入从 Host 身份或受信适配器得到的来源，处理器会拒绝
`FeedbackEvent.source` 与可信调用上下文不一致的请求，避免 Candidate 通过 JSON 自报为
`DeterministicCheck`。

处理器先读取只追加 Episode Header，校验 `related_episode_id` 与 `related_run_id`，再验证可选
脱敏证据确实存在于 Artifact CAS 且长度一致。Recorder 生成的初始 `OutcomeRevision` 位于
Episode 的监督 CAS 中；处理器会把该制品恢复为本地修订历史首项，然后用 `supersedes` 追加
反馈修订。Episode Header 永不覆盖，反馈修订以加法字段保存完整 `FeedbackEvent`，旧 JSON
记录缺少该字段时仍按 `None` 读取。

`Unverifiable` 可以被后续决定性反馈修订；已有明确终态只能由同等或更高可信来源覆盖。
`Note`、未知来源、Run 绑定错误、缺失或篡改的 CAS 制品均不能改变 Outcome。相同反馈重复
提交会返回已有修订，不会生成重复历史。延迟反馈本身不携带 Episode 内 Event ID，因此原因
未知的单次纠正不会伪造 Incident 或直接进入 Evolution Outbox。

## Protocol Replay

`ProtocolReplay` 读取 Episode 引用的 NDJSON 制品，先验证摘要、长度、事件数、Run ID、
事件 ID 唯一性、时间与 step 单调性，以及 `run_started` / `run_finished` 终态顺序；存在
监督引用时，还会逐条验证 Envelope 的 sequence、Episode、Run、Genome、Event ID 和
脱敏载荷。全部通过后才把事件交给 `ReplayEventSink`。该过程不调用真实模型或工具，
因此可用于确定性状态机、插件 Hook 与持久化回归。
