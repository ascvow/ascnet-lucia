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

## 数据处理与终态

Recorder 在 `RunFinished` 时自动收敛：事件流先写入 SHA-256 CAS，随后只追加 Episode
Header。默认 `EpisodeDataPolicy` 为 `NotEligible` 且丢弃工具结果正文；模型隐藏思考
增量永不持久化。其余 JSON 字符串经过确定性脱敏，Episode 记录实际规则版本。

Recorder 只把脱敏并按数据策略收窄后的公开载荷交给 `RunSupervisor`。Supervisor 生成的
Event Envelope、Incident 和初始 Outcome Revision 分别进入 CAS，其引用保存在 Episode
Header 的 `supervision` 字段中，进程重启后仍可从同一 Episode 找回完整监督证据。

在线运行没有可信 Verifier 时，正常完成默认记为 `Unverifiable`，不能推断为任务成功。
取消记为 `Cancelled`。模型服务、工具环境或存储导致运行未产生 `RunFinished` 时，应用层
应调用 `recorder.finish(Outcome::InfrastructureFailure)`，避免把基础设施问题统计为候选
能力失败。

## 存储不变量

- `FileArtifactStore` 按 SHA-256 内容寻址，读取时重新验证摘要，提交不覆盖已有内容。
- `FileEpisodeStore` 使用 `create_new` 语义，只追加而不更新历史 Episode。
- Episode 查询支持按 `Outcome` 和 `session_id` 过滤。
- 原始工具正文只有在数据分级允许且策略显式设为 `StoreRaw` 时才会进入事件制品。
- `FileOutcomeRevisionStore` 按 Episode 保存单调序号记录；新修订必须通过 `supersedes`
  指向最新修订，并发竞争同一后继时只允许一个写入者提交。
- `FileEvolutionOutbox` 的 JSON 记录不可变；消费状态写入独立 `.consumed` 标记，不覆盖
  原始记录，并拒绝路径逃逸和符号链接制品。

## Protocol Replay

`ProtocolReplay` 读取 Episode 引用的 NDJSON 制品，先验证摘要、长度、事件数、Run ID、
事件 ID 唯一性、时间与 step 单调性，以及 `run_started` / `run_finished` 终态顺序；存在
监督引用时，还会逐条验证 Envelope 的 sequence、Episode、Run、Genome、Event ID 和
脱敏载荷。全部通过后才把事件交给 `ReplayEventSink`。该过程不调用真实模型或工具，
因此可用于确定性状态机、插件 Hook 与持久化回归。
