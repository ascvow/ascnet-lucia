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

`FileGenomeResolver` 同时支持精确 Revision 与只读 Stable lineage。Stable 引用位于
`stable/<sha256(lineage)>.json`，解析时会拒绝符号链接、校验引用版本和 lineage，并再次复核
目标 Revision 的 ID 与行为摘要。Resolver 不提供写 Stable 接口，Promotion 与 Rollback 的
写边界仍由可信 Release Controller 持有。

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
`<evidence-root>/genomes/<revision-id>.json` 读取并验证不可变 Revision，或把 Stable lineage
解析为精确 Revision。普通配置只继续提供模型凭据；provider 类型、端点、协议、模型参数、
Prompt、原生工具、插件 bundle、独占能力 owner 和执行策略都由 Genome 装配。Genome 必须
按顺序引用至少一个包含完整系统提示的 UTF-8 Prompt CAS 制品；空 Prompt 不会隐式采用普通
配置或 Core 默认提示。Prompt 与 Provider Options 从 Artifact CAS 按摘要读取，插件 bundle
使用 Plugin Manager 的同一摘要算法复核。额外发现的插件不会进入 Evidence 组合，任一固定
插件未 Ready 或加载失败时禁止开始 Run。

启动还会把 Revision 的包版本、Git 提交、dirty 状态、目标三元组和 TUI feature 与编译产物
逐项核对。源码归档构建使用显式 `unknown` 提交标记且按 dirty 构建处理；由于它无法唯一证明
内核版本，普通 Serve 可以运行，但 Evidence 会拒绝启动。

当前 TUI 没有跨插件的 Context、Planning 与 Skill 独立快照服务，因此包含这些非空字段的
Genome 会明确拒绝运行，不会只记录字段却继续采用另一份真实配置。插件内部配置和 Skill
文件仍可随整个 bundle 被摘要固定；以后开放独立变异表面时，需要由对应插件提供版本化快照
服务。任一真实主会话在
用户输入成功写入 Session Store 后预登记 Recorder，再把同一个 Run ID 传入 Core。正常
`RunFinished`、取消、步骤预算耗尽和基础设施错误都会显式收敛并释放路由。证据写入失败会
报告为运行完成错误，不会被静默忽略。

新 Session 在首次保存前写入 `agent_genome/<revision-id>` 行为绑定。已持久化 Session 只能在
绑定完全相同时恢复；旧记录缺少绑定或绑定不同 Revision 时拒绝恢复，避免 Stable 更新后让
长会话静默切换行为版本。

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

## Evolution Scorecard 与历史分析

可信评测平面使用 `EvaluationReport` schema v1 记录 Parent/Candidate 环境、Dataset 版本、
TaskCase metadata、逐次 Attempt、Verifier、安全、资源、Gate、Release 与继承证据。
历史分析新增的 `lineage`、`parent_generation` 和 `candidate_generation` 均为可选加法字段，
因此旧报告仍可读取；缺少这些字段时只显示 `N/A`，不会猜测 Lineage 或代数。

`agent-evolution` 从单份报告派生 `EvolutionScorecard` schema v1。TaskCase 内先按有效 Repeat
计算分数，再对 TaskCase 等权聚合；基础设施故障单独统计，Candidate 导致的超预算或超时仍算
行为失败。Capability Score 只用于展示，Promotion 仍由可信 Gate 决定。安全证据缺失、完整性
未知或 Hidden 隔离未知会得到 `INCONCLUSIVE`，不会按零失败处理。只有泛化、Retention、
安全、Gate、Release、重启、新 Session、旧 Session 保留、Stable 引用与摘要验证全部满足策略，
首屏才显示 `EVOLVED`。

`EvolutionCertificate` schema v1 绑定源 Episode、Issue、Mutation、允许差异、Candidate CAS
制品、四类 Dataset、EvaluationReport、Scorecard、Release 和继承验证。Certificate 自身使用
SHA-256 摘要；`--verify` 还会读取 CAS，逐项校验引用制品摘要与长度。Rollback 不删除证明包，
只生成生命周期为 `RolledBack` 的新归档视图。

默认数据根为 `$LUCIA_HOME/evolution`，也可用全局 `--root` 指定。不可变报告位于
`reports/`，Parent/Candidate 最近报告索引位于 `comparisons/`，派生评分卡位于 `scorecards/`，
证明包位于 `certificates/`，内容寻址制品位于 `artifacts/`。读取历史时任一 JSON、Schema 或
摘要损坏都会返回错误，不会静默跳过后继续展示成功。

常用只读命令如下：

```bash
lucia evolution genome inspect --revision <revision-id> [--format json]
lucia evolution genome inspect --stable stable/general [--format json]
lucia evolution genome verify --revision <revision-id>
lucia evolution genome diff --parent <revision-id> --candidate <revision-id>
lucia evolution genome diff --parent <revision-id> --candidate <revision-id> --allow task-strategy-prompt
lucia evolution compare --report <evaluation-report.json> --format table
lucia evolution compare --parent <parent-revision> --candidate <candidate-revision> --format json
lucia evolution dashboard [--lineage stable/general]
lucia evolution dashboard --tui [--lineage stable/general]
lucia evolution history [--lineage stable/general] [--format json]
lucia evolution lineage [stable/general] [--format json]
lucia evolution capability-map [--lineage stable/general] [--format json]
lucia evolution funnel [--lineage stable/general] [--format json]
lucia evolution certificate <release-id> --verify [--format json]
```

Genome 子命令全部只读。`diff --allow` 由可信实现逐字段计算差异，不接受 Candidate 自报的
变更列表；任一变化落在允许表面之外时命令失败。

四页 Ratatui Dashboard 通过 `Tab`、左右方向键或数字 `1` 至 `4` 切换 Overview、
Capability Map、Lineage 与 Evidence，Evidence 页使用上下方向键下钻，`q` 或 `Esc` 退出。
小终端会降级为仍包含 Verdict 与 Safety 的紧凑视图；无数据与损坏数据分别显示明确空状态和
错误状态。Evidence 只展示可信 ID、结构化计数和 CAS 引用，不读取或显示 Hidden TaskCase
正文、Secret、未脱敏 ToolResult，也不接受 Candidate 提供的最终评分。

历史 schema v1 按 Hidden Dataset 版本分段趋势，以后续 Regression TaskCase 的真实结果计算
Fix Survival；Candidate Yield 排除不可比较运行，Rollback Rate 使用 Promotion 数作为分母。
当前 EvaluationReport 没有 Episode 到 Candidate 生成阶段的完整漏斗计数，因此这些上游阶段
保持 `N/A`，不伪造为零。Schema v1 均为初始版本，不需要数据迁移；未来删除、改名或收紧字段
语义时必须升级对应版本。
