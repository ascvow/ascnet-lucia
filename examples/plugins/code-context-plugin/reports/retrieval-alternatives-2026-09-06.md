# AST 实验之后的检索备选方案

日期：2026-09-06。运行算法仍为 `595e1c8`，本轮起点为文档提交 `2ec1c6b`。本轮只核对既有实验、当前源码和公开实现，没有修改检索算法、调用付费推理或运行新的 A/B。以下是待验证方案，不是准确率提升结论。

## 一、结论与当前证据

先做**同一次搜索的阶段记录与正文覆盖消融**；随后优先验证**索引侧生成块专属中文描述**。父子检索和调用关系扩展适合补齐已找到入口的实现，列表重排适合已有正确候选但排序不佳的查询。不要把这些方案一起上线再解释总体变化。

两轮全语言 AST 都没有证明提升业务质量：48 条新业务的 MRR 为 0.697421 → 0.627257 → 0.665451；第二轮对基线的双侧配对符号检验 p=0.581055，两次尝试校正 p=1。恢复 Vue 窗口只挽回部分退化；该实验已经完成，不重做。完整补丁仍在 [实验归档](./experiments/all-language-ast-2026-09-06.patch)。

当前链路有两个独立前缀限制：

- [嵌入文档](../src/lib.rs)：`MAX_SEMANTIC_BODY_CHARS=1200`，路径、签名、原注释、父符号和依赖已存在。
- [重排文档](../src/rerank.rs)：`MAX_BODY_CHARS=1800`，最多 150 个候选。
- [playground](../examples/search_playground.rs) 的 `--diagnostics` 会在真实工具结果之后再运行一次 `search_execution`；不能将额外调用的候选当作前一次工具调用的内部证据。

对同一份冻结目标做离线前缀检查：

| 查询组 | 查询数 | 至少一个目标关键行超出声明后 1200 字符 | 超出 1800 字符 |
| --- | ---: | ---: | ---: |
| 新业务 | 48 | 11 | 5 |
| 同义表达 | 24，属于 12 个业务族 | 10 | 6 |

统计按 [冻结数据](../target/all-language-ast-implementation-2026-09-06/baseline/frozen-cases.json) 的 `answer_groups[].targets[]` 回读 52 个目标源码文件，逐个核验 SHA-256 与冻结值一致；从声明起点到必需行末保留原文空白，以 Unicode 字符数计算去掉前导空白后的完整行尾位置。一个查询的任一目标越界即计一次。该统计只描述目标跨度，**不代表实际 chunk 起点、实际模型输入或真实丢失阶段**，也不把可替代答案组当作全部必需目标。已有重叠窗口可能覆盖目标后半段。

## 二、方案比较

| 优先级 | 方案 | 主要解决的问题 | 查询成本与限制 |
| --- | --- | --- | --- |
| 1 | 保留现有块身份，扩大有界嵌入正文 | 长块关键行为没有进入向量表示 | 不新增查询 API；需要重嵌受影响文档，仍须实测端到端延迟。不能修复所有跨函数缺失。 |
| 2 | 完整源码驱动的块专属中文描述 | 中文业务表述与英文代码缺少语义联系 | 生成成本移到索引侧；不新增查询生成调用。存在解释失真和缓存失效问题。 |
| 3 | 父子表示及有界关系扩展 | 已找到一个函数，但缺共同语境或下游实现 | 本地读取可不增加模型调用；送入重排的文本变大仍会增时。无法保证救回完全没有种子的区域。 |
| 4 | 一次列表重排替换现有重排 | 正确候选已在池内，入口、说明或相似实现排在前面 | 可以替换当前请求层，但 150 项大输入的延迟未知；不能缩为 OCE 默认 50 项。 |
| 储备 | 服务端 late chunking | 每块向量需要保留相邻代码的上下文 | API 确实存在；需要换提供商、分组批次和重建向量。 |
| 储备 | ColBERT / late interaction | 单向量压缩损失细粒度匹配信息 | API 可返回 token 多向量，但需要新索引与 MaxSim 检索，WASM 资源和时延未验证。 |

### 1. 正文覆盖：先做便宜、可解释的消融

[OCE 固定源码审计](../target/oce-research-2026-09-06/report.md) 已确认，其常规 chunk 在 6000 字符限制内通常以全文嵌入；Lucia 两轮 AST 实验均保留了 1200 字符限制。这是尚未检验的独立变量，不能由 AST 实验失败推断它也无效。

建议预先固定一档正文预算，例如 6000 字符，对照现有 1200。保留分块身份、短函数组、模型、向量维度、150 候选、BM25、融合和重排输入不变；不恢复被回退的新 AST。6000 是可复现的实验预算，不是已知最优值。模板变化必须参与缓存身份，使用隔离实验缓存，不删除已有 WAL/SHM。

先记录真实候选阶段：若困难查询的目标已进池，实际重排输入却缺关键正文，应另开“重排正文覆盖”单变量实验，不能指望只扩大嵌入文本解决。若长正文进入向量后造成主题稀释、最终 MRR 无显著改善，同样回退并记录。

### 2. 索引侧中文描述：第一项值得验证的新语义方案

[Anthropic 官方说明](https://www.anthropic.com/engineering/contextual-retrieval) 明确区分了两种做法：给每块贴相同的整文档摘要，其实验收益很小；读取全文后为当前块生成专属上下文，才是其 Contextual Retrieval 方案。

[官方代码库检索示例](https://github.com/anthropics/anthropic-cookbook/blob/a97b9a2dc300635f0c26b5e05d0b54bbe0279ee5/capabilities/contextual-embeddings/guide.ipynb) 的 `situate_context(doc, chunk)` 接收全文与块，`process_chunk` 将生成解释与原块拼接后嵌入；查询仍只嵌入原问。示例使用代码库与 golden chunk 评测，但并未验证我们的中文问题、多函数完整覆盖及现有重排链路，不能引用其数字预测 Lucia 的收益。

建议生成内容描述“执行什么操作、受什么条件触发、改变什么状态、与周围实现有什么关系”，使用中文并保留必要英文标识符。只从源码推导，不使用项目词表、不读取评测查询或真值，也不把整文件全部功能贴给每个函数。

第一轮保持单向量：仅在既有嵌入文档中追加描述，向量数量不变，不新增 RRF 通道，也不同时把描述加入 BM25 或重排。这样避免再次重复计算高度相关证据。生成文本只用于索引，结果仍返回原始源码。

[LlamaIndex 的 DocumentContextExtractor](https://github.com/run-llama/llama_index/blob/d2ac544a27c73d2a68e9c57efec4b2ac0ef99892/llama-index-core/llama_index/core/extractors/document_context.py#L175-L217) 提供全文与块并保存生成上下文，可借鉴其并发和失败处理；但 Lucia 的缓存必须包含全文摘要、块身份及正文、生成模型和提示词版本，不能只检查“已有描述”。超长文件需明确输入预算与降级，失败保留原文并支持续跑。

按基线 29,320 个向量、每项输出 100 token 粗估，单输出约 293 万 token；输入还包含重复读取的文件语境。这不是价格或吞吐实测。先从与查询无关的源码样本测量 token、耗时和成功率，再估全仓成本；不能把 Anthropic 的提示词缓存报价套到 SiliconFlow。

### 3. 父子检索与关系扩展：补完整实现

[LangChain ParentDocumentRetriever](https://github.com/langchain-ai/langchain/blob/eef3eaeac8ff2646c43b37ac2a755a5e7978e8b4/libs/langchain/langchain_classic/retrievers/parent_document_retriever.py#L99-L145) 将子块写入向量库、父正文写入文档库，命中后读取父正文。[LlamaIndex AutoMergingRetriever](https://github.com/run-llama/llama_index/blob/d2ac544a27c73d2a68e9c57efec4b2ac0ef99892/llama-index-core/llama_index/core/retrievers/auto_merging_retriever.py#L83-L121) 按命中子块占比提升为父节点。二者都不是“额外加一路父向量就自然提高召回”。

Lucia 可保留原子声明定位，另用同文件、同词法父级、源码相邻性和统一预算构造语义包，并维护成员到原文范围的映射。输出不能用模糊整文件覆盖替代精确声明，也不能借扩大返回篇幅抬高完整覆盖指标；同时记录输出 token 和无关正文占比。

对调用链，只扩展可从实际语法和作用域确认的有界引用；未解析或多义引用明确保留未知状态。[Aider repo map](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/repomap.py#L481-L545) 的同名引用连边和 PageRank 是近似关系，不能照搬为“所有语言的精确调用图”。该机制适用于通用代码关系，不能针对 Java/PHP 或某个业务名写特殊召回规则。

### 4. 列表重排：只在排序确实是瓶颈时试

[OCE 列表重排](https://github.com/oce-ai/oce/blob/a359272560bbbdb321055aaed6c16ba1f4e06887/src/oce/domain/services/llm/reranker.py#L59-L138) 通过一次聊天比较多个候选，但其默认只有 50 项。[SiliconFlow 聊天接口](https://docs.siliconflow.cn/docs/api/chat-completions-post) 提供思考开关、输出上限及结构化格式选项，具体支持取决于模型。

可尝试直接替换当前重排，严格校验返回编号、范围和去重，并保留角色与体量处理及失败回退。150 项每项 1600 字符就约 24 万字符正文，必须先核对实际 token 和模型上下文；一次请求不等于同样耗时。若只有 Top-20 叠加重排获得收益，不能据此宣称替换整个 150 候选排序也有效。

[RankLLM 的滑动窗口实现](https://github.com/castorini/rank_llm/blob/34af7f4e2454a388a77537901478cb85a97e5705/src/rank_llm/rerank/listwise/listwise_rankllm.py#L512-L557) 默认 20 窗口、10 步长，处理 150 项需 14 次依赖调用，不适合直接作为当前默认链路。

### 5. Late chunking 与多向量：存在真实 API，但不是改模型名

[Jina 官方 OpenAPI](https://api.jina.ai/openapi.json) 的 EmbeddingsV3Request/EmbeddingsV4Request 提供 `late_chunking`；[固定提交测试](https://github.com/jina-ai/late-chunking/blob/1d3bb02bf091becd0771455e4e7959463935e26c/tests/test_api.py#L16-L35) 确实将按序块列表交给服务端，并验证整篇 token 向量分段池化。

服务端会把同一请求输入串接为共同语境，因此不能继续跨无关文件随意拼批。需按文件或有界语境组处理，并在上下文变化时重算相关块。普通 pooled 向量不能在客户端还原这一能力。[SiliconFlow 当前嵌入接口](https://docs.siliconflow.cn/docs/api/embeddings-post) 未公开同类开关或 token 向量输出。

[ColBERT](https://github.com/stanford-futuredata/ColBERT#colbert-v2) 使用 token 向量矩阵与 MaxSim；Jina OpenAPI 也提供相应多向量输出。但当前 Lucia 是每块一个向量，需要新存储和检索机制。只在现有 150 项上用 MaxSim 无法补池外召回；全库多向量检索的容量、WASM fuel 和时延必须另验。

当前插件允许域名不含 `api.jina.ai`，也没有本轮可用的 Jina 凭据。这些方案需要正规提供商配置与能力声明，不绕过宿主注入密钥。公开接口证据核对日期为 2026-09-06，本轮未发送代码到这些服务。

## 三、真实搜索语义与结果：方案分别对应什么问题

以下摘自已完成的基线搜索，**不是新方案的实验结果**。完整 Top-10、原始代码和人工审计见 [既有逐查询报告](./all-language-ast-search-results-2026-09-06.md) 与 [冻结基线结果](../target/all-language-ast-implementation-2026-09-06/baseline/results.jsonl)。`search_code` 是展示记法，实际工具名仍为 `code_search`。

| 搜索语义 | 基线真实结果 | 对方案选择的意义 |
| --- | --- | --- |
| `search_code("课程视频到了互动点，哪里决定暂停并显示互动？")` | 第 1 条为 VideoSegment.vue 的 `maybeOpenInteraction` 75–89；第 4 条 1–73 才带出 `openInteraction` 69–73。完整冻结链路需读到第 4 条。 | 已有种子；父子语境或可靠引用扩展值得验证。 |
| `search_code("生成临时下载链接")` | 第 1 条 OssController.java 181–262；第 2 条 OssServiceImpl.java 227–282。目标 `generatePresignedUrl` 为 213–235，Top-10 缺完整构造逻辑。 | 目标声明本身仅 1032 字符，不能说所有失败都是函数过长；还需检查窗口起点、候选与最终输出。 |
| `search_code("修改已有贷款申请的证件照片时，如何删除选中的旧照片并保存新的正反面？")` | 第 1 条为照片编辑表单；第 4 条 Index.php 881–933 仅到目标开头。目标 `postIdimage` 927–1001 的删除和保存主体未完整返回。 | 既要区分编辑表单与操作实现，也要保留后半段行为；不能只算命中文件。 |
| `search_code("图片太大时怎么压缩再给文字识别用？")` | 第 1 条 ImagePreprocessingService.java 119–349；第 2 条同文件 1–58。目标入口 `preprocessImage` 为 70–143，Top-10 缺 70–118。 | 找到相关辅助代码不等于完整入口；需要定位实际丢失阶段。 |

## 四、下一轮如何验收

1. **先取得同次调用证据。** 沿用真实 `code_search` 路径，记录查询/源码版本、原子及语义包 ID、各路召回、融合池、模型输入摘要与可见范围、模型原始顺序、角色调整、上下文保留或剔除原因。不得为了诊断再调用一次模型；诊断开关不能改变结果。
2. **一次只改一个变量。** 首先验证正文覆盖；若进入描述实验，生成器只读取源码，先冻结模型、提示词和描述产物，再运行搜索。生成器不读取查询、标签或失败列表，也不只增强被标记为未命中的目标。
3. **复用现有评测，不另换口径。** 完整执行 48 条新业务和 12 家族的 24 条同义表达，保持每进程最多 6 条；逐查询及逐家族分别做配对符号检验，预先声明主指标和比较次数并校正。原 48 条已参与选方案，应作为回归集；参数冻结后还需用同一评分器冻结独立新业务组确认泛化。
4. **保留实际搜索语义与全部结果。** 同时报告 MRR、Recall@1/3/10、完整声明与关键正文、人工复核、索引 token/时间/向量数、输出体量及端到端中位/P95。A/B 随机交替，在索引预热完成且没有并发冷嵌入时测查询延迟；缓存命中与未命中分开。
5. **设停止条件。** 只在候选阶段改善、最终质量没有显著提升，不算通过；完整实现退化或延迟超过约 4.7 秒参考预算也不能默认合入。失败保留补丁与结论。禁止用少数成功样例或技术测试通过替代业务验收。

本轮没有新的准确率 A/B，因此没有新的 p 值；上述两轮 AST 的数字仅用于解释为何调整实验方向。

本轮重新执行用户指定的 Cargo 测试和 Clippy，附加 `--locked --offline`：443 项通过、11 项原有 ignored，Clippy 无警告；本地文档链接检查通过。运行代码、依赖和根锁未改动。这些检查不构成上述候选方案的业务验收。
