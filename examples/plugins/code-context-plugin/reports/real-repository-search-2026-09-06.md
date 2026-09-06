# Lucia 跨项目语义检索测试报告

日期：2026-09-06。以下逐条保留搜索语义、实际 Top-10 结果、文件行号和人工复核。

[完整报告（含实际返回的源码片段）](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/report.md>) · [完整结构化结果](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/report.json>) · [独立运行审计](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/runtime-audit.json>)

本轮固定 48 条自然语言查询，来自 8 个项目；有效完成 48 条。样本在检索前由源码核验，覆盖统计仅适用于本轮固定样本，不能代表两个父目录下全部项目的准确率。

以 search_code 展示搜索语义，实际调用工具 code_search。

自动统计冻结目标的声明与关键行覆盖；人工核验判断实际返回内容能否回答查询。两组结果并列保留：模板外层起点未返回或同义替代实现可能让自动统计低估语义命中。

本轮为单版本测试，仅报告固定样本覆盖，不外推为目录准确率，也不计算 A/B 显著性。

## 冻结目标的声明与关键行覆盖

| 项目 | 输入候选文件数 | 完成/样本 | 文件@10 | 声明/块@10 | 关键行@1/@3/@10 | 关键行 MRR | 查询中位秒 |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1623-mobile | 97 | 6/6 | 6 | 6 | 4/6/6 | 0.833 | 3.86 |
| 1321_frontend | 82 | 6/6 | 6 | 6 | 5/5/5 | 0.833 | 4.38 |
| 1305_pc | 155 | 6/6 | 6 | 5 | 4/5/5 | 0.750 | 4.14 |
| 1573-frontend | 39 | 6/6 | 6 | 5 | 4/5/5 | 0.722 | 3.58 |
| bid-review-app | 310 | 6/6 | 5 | 5 | 2/5/5 | 0.528 | 6.26 |
| teriri-api-hub | 49 | 6/6 | 6 | 6 | 6/6/6 | 1.000 | 4.41 |
| 1604-apps | 654 | 6/6 | 6 | 6 | 5/6/6 | 0.917 | 3.71 |
| 1086-backend | 2682 | 6/6 | 6 | 5 | 4/5/5 | 0.750 | 5.43 |

冻结目标的声明与关键行覆盖：Top-1 34/48，Top-3 43/48，Top-10 43/48。
真实工具耗时中位 4.34 秒，P95 7.50 秒。

## 人工语义核验

能由 Top-1/Top-3/Top-10 结果回答的样本数：36/48、45/48、47/48。人工判断来自返回的真实代码片段与源码核查，未覆盖或改写自动分数。

人工可回答不等于完整函数覆盖：贷款创建只返回函数后半段，虽然包含主单、身份证图片和附件的同一事务保存，但缺少函数入口及借款字段构造；另有替代实现与模板起点约束的差异。库存盘点保存仍缺必要的参数构造和实际提交正文，保持未能完整回答。

| 项目 | 已核验样本 | Top-1 可回答 | Top-3 可回答 | Top-10 可回答 |
|---|---:|---:|---:|---:|
| 1623-mobile | 6 | 4 | 6 | 6 |
| 1321_frontend | 6 | 5 | 5 | 5 |
| 1305_pc | 6 | 5 | 5 | 6 |
| 1573-frontend | 6 | 5 | 6 | 6 |
| bid-review-app | 6 | 2 | 6 | 6 |
| teriri-api-hub | 6 | 6 | 6 | 6 |
| 1604-apps | 6 | 5 | 6 | 6 |
| 1086-backend | 6 | 4 | 5 | 6 |

## 测试环境与索引

实际执行路径：当前构建的原生 search_playground → CodeContextPlugin::call_tool_with_host → code_search。本轮不包含 WASM Host 装载验收。

模型：BAAI/bge-m3（1024 维）+ Qwen/Qwen3-Reranker-4B；Top-10、8000 token；每批最多六条，420 秒超时加最多两次重试。查询用时不含启动和索引；持久缓存未清理 WAL/SHM。

自动判据：按前 K 条返回片段中同路径、同行文本的并集核验声明/块起始行及全部预定关键行。targets 为 OR，required_targets 为 AND。它不代表完整函数体覆盖；single_result_body_rank 另要求每个目标由单条结果覆盖。API 错误、降级与源码漂移不混为未命中。

输入候选文件数是示例按白名单读取的源码、文档和配置总数；实际进入索引的数量见下表。索引时间仅指该成功批次内的建立/加载时间，首次超时尝试另保存在失败日志，不能把该时间当成全量初次嵌入总耗时。

| 项目 | 实际索引文件数 | 块数 | 符号数 | 成功批次内索引秒 |
|---|---:|---:|---:|---:|
| 1623-mobile | 97 | 867 | 558 | 15.6 |
| 1321_frontend | 82 | 831 | 514 | 15.3 |
| 1305_pc | 154 | 8368 | 5257 | 154.7 |
| 1573-frontend | 39 | 482 | 355 | 6.3 |
| bid-review-app | 310 | 2315 | 711 | 43.6 |
| teriri-api-hub | 49 | 172 | 50 | 3.3 |
| 1604-apps | 654 | 6008 | 3825 | 112.7 |
| 1086-backend | 2682 | 13530 | 6611 | 241.1 |

运行终验：实际索引 4067 个文件、32573 个代码块、17881 个符号，持久保存 25577 条 1024 维向量。8 批均首次成功，8 个项目运行前后源码摘要一致；未清理已有缓存。自动评分逐项复算一致，8 项评测工具 smoke 检查通过。详情见 [runtime-audit.json](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/runtime-audit.json>)。

## 输入与原始证据

- 构建提交：6d5da16d26e52b973d680fd36ecee66f6bd094c5
- 数据集摘要：1c00158e7e3bd1dbd833af0141dd1ed0368f284958b622343f8eecc57bb0ef1e
- 二进制摘要：085993450da770aca9ed1c09a4617bfdd0afaca7d83d1d3410413e8f4ac5f00a
- 冻结案例与源码摘要：[frozen-cases.json](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/frozen-cases.json>)
- 完整结构化报告：[report.json](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/report.json>)
- 原始日志目录：/Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs
- 失败尝试数：0；失败后成功重试的查询只计成功结果一次。

## 1. 1623-mobile — 图片选取、异步上传和表单回填

```text
search_code("实习照片怎样从手机拍照或相册选取后上传并填回记录？")
```

**搜索语义**：实习照片怎样从手机拍照或相册选取后上传并填回记录？

**预期意图**：图片选取、异步上传和表单回填

**期待来源**：src/pages/internship/form.vue:130 选择一张相机或相册图片，调用上传接口，将返回地址写入实习照片字段并处理异常。

- 可替代目标：[src/pages/internship/form.vue:130](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:130>)；handleUpload；关键实现行 [131, 134, 140, 145]。选择一张相机或相册图片，调用上传接口，将返回地址写入实习照片字段并处理异常。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:114>)，对应 [src/pages/internship/form.vue:114-172](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:114>)。
- 判断依据：第 1 条虽然标作“文本块”，实际包含 handleUpload 的声明和完整函数：相册/相机选图、上传响应校验、照片地址回填及异常清理均可见。
- 结果标签说明：显示符号为文本块，不能据此判作函数未命中。
- 冻结目标：第 1 条；[src/pages/internship/form.vue:114-172](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:114>)；核验行 131、134、140、145。

**执行状态**：有效；真实工具 3.507 秒，批次含索引 43.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/internship/form.vue:114-172](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:114>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/pages/internship/form.vue:1-54](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) | 仅文件命中 |
| 3 | handleUpload | [src/pages/basic/index.vue:77-101](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:77>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/pages/internship/detail.vue:1-115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) | 未覆盖预定目标 |
| 5 | onChooseAvatar | [src/components/LoginWrapper/index.vue:204-212](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:204>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/internship/index.vue:26-34](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:26>) | 未覆盖预定目标 |
| 7 | handleSubmit | [src/pages/basic/internship.vue:97-107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/internship/index.vue:1-21](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/pages/basic/internship.vue:1-60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) | 未覆盖预定目标 |
| 10 | savePracticeRecord | [src/api/practice.js:80-86](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:80>) | 未覆盖预定目标 |



## 2. 1623-mobile — 按月份和学生保存完整实习报告

```text
search_code("学生填写某月的实习内容、心得和照片后，在哪里保存并返回列表？")
```

**搜索语义**：学生填写某月的实习内容、心得和照片后，在哪里保存并返回列表？

**预期意图**：按月份和学生保存完整实习报告

**期待来源**：src/pages/internship/form.vue:156 组装月份、学生、内容、心得和照片，保存成功后重启至实习列表。

- 可替代目标：[src/pages/internship/form.vue:156](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>)；handleSubmit；关键实现行 [158, 159, 160, 161, 162, 164, 170]。组装月份、学生、内容、心得和照片，保存成功后重启至实习列表。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>)，对应 [src/pages/internship/form.vue:156-171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>)。
- 判断依据：第 1 条直接返回保存实习报告的完整函数，月份、学生、内容、心得和照片均进入保存请求，成功后返回列表。
- 冻结目标：第 1 条；[src/pages/internship/form.vue:156-171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>)；核验行 158、159、160、161、162、164、170。

**执行状态**：有效；真实工具 3.681 秒，批次含索引 43.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | handleSubmit | [src/pages/internship/form.vue:156-171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) | 单条覆盖声明及关键行 |
| 2 | loadDetail | [src/pages/internship/form.vue:118-128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:118>) | 仅文件命中 |
| 3 | 文本块 | [src/pages/internship/detail.vue:1-115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/pages/internship/index.vue:1-55](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/pages/internship/students.vue:1-56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) | 未覆盖预定目标 |
| 6 | handleSubmit | [src/pages/basic/internship.vue:97-107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/pages/basic/internship.vue:1-60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/rating/index.vue:1-57](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/pages/rating/form.vue:1-51](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:1>) | 未覆盖预定目标 |
| 10 | savePracticeInfo | [src/api/practice.js:49-55](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:49>) | 未覆盖预定目标 |



## 3. 1623-mobile — 流式转写替换旧片段并有序回填表单

```text
search_code("语音识别服务修正前面识别错的片段时，输入框里的文字怎样重新合并？")
```

**搜索语义**：语音识别服务修正前面识别错的片段时，输入框里的文字怎样重新合并？

**预期意图**：流式转写替换旧片段并有序回填表单

**期待来源**：src/pages/internship/form.vue:422 处理识别服务的替换区间，删除旧片段并触发表单文本更新。

- 可替代目标：[src/pages/internship/form.vue:422](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>)；handleIatMessage；关键实现行 [446, 447, 448, 451, 452]。处理识别服务的替换区间，删除旧片段并触发表单文本更新。
- 必要目标：[src/pages/internship/form.vue:470](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>)；updateVoiceField；关键实现行 [472, 473, 474, 476]。按片段编号排序合并识别文本，追加到初始文本后回填当前语音字段。

**人工语义核验**：已定位，跨方法正文齐全

- 可回答范围：前 2 条结果；语义首次可回答名次 [第 2 名](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>)，对应 [src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>)。
- 判断依据：第 1 条负责按序合并和表单回填，第 2 条负责替换旧识别片段并调用回填；到第 2 条才同时满足两项目标。单看第 1 条不足以解释服务端修正片段。
- 跨目标核验：handleIatMessage 与 updateVoiceField 在前 2 条分别完整出现。
- 冻结目标：第 2 条；[src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>)；核验行 446、447、448、451、452。
- 冻结目标：第 1 条；[src/pages/internship/form.vue:470-477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>)；核验行 472、473、474、476。

**执行状态**：有效；真实工具 6.206 秒，批次含索引 43.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 2 / 2；单片段严格诊断名次：2。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | updateVoiceField | [src/pages/internship/form.vue:470-477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) | 单条覆盖声明及关键行 |
| 2 | handleIatMessage | [src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) | 单条覆盖声明及关键行 |
| 3 | default.methods.trimSpliter | [src/components/SkuSelector/index.vue:291-300](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:291>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/components/PracticeDesign/FormField.vue:48-100](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/FormField.vue:48>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/manifest.json:61-91](</Users/teriri/WebstormProjects/1623-mobile/src/manifest.json:61>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17-24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/pages/index/index.vue:75-80](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:75>) | 未覆盖预定目标 |
| 8 | processVueFiles.textMap | [scripts/generate-language.js:326-326](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:326>) | 未覆盖预定目标 |
| 9 | handleAgree | [src/pages/launch/index.vue:21-24](</Users/teriri/WebstormProjects/1623-mobile/src/pages/launch/index.vue:21>) | 未覆盖预定目标 |
| 10 | default.methods.combineAttr.result | [src/components/SkuSelector/index.vue:164-164](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:164>) | 未覆盖预定目标 |



## 4. 1623-mobile — 按学生和月份提交考勤及等级

```text
search_code("老师怎样保存某个学生当月的迟到请假次数、工作时长和实习评级？")
```

**搜索语义**：老师怎样保存某个学生当月的迟到请假次数、工作时长和实习评级？

**预期意图**：按学生和月份提交考勤及等级

**期待来源**：src/pages/rating/form.vue:91 保存学生月份、考勤、工时与等级，并返回该学生评级列表。

- 可替代目标：[src/pages/rating/form.vue:91](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:91>)；handleSubmit；关键实现行 [92, 93, 94, 95, 98, 99, 101, 108]。保存学生月份、考勤、工时与等级，并返回该学生评级列表。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:52>)，对应 [src/pages/rating/form.vue:52-110](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:52>)。
- 判断依据：第 1 条文本块包含实际保存考勤函数，学生、月份、迟到、请假、工时和等级均入参，并包含成功后的列表跳转。
- 结果标签说明：文本块同时含表单和回填逻辑，但完整保存函数也确实位于返回正文内。
- 冻结目标：第 1 条；[src/pages/rating/form.vue:52-110](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:52>)；核验行 92、93、94、95、98、99、101、108。

**执行状态**：有效；真实工具 4.035 秒，批次含索引 43.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/rating/form.vue:52-110](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:52>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/pages/internship/detail.vue:54-115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:54>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/pages/index/index.vue:1-49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) | 未覆盖预定目标 |
| 4 | handleSubmit | [src/pages/internship/form.vue:156-171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) | 未覆盖预定目标 |
| 5 | loadDetail | [src/pages/internship/form.vue:118-128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:118>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/index/index.vue:110-169](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:110>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/pages/rating/index.vue:1-57](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/internship/students.vue:1-56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) | 未覆盖预定目标 |
| 9 | saveAttendance | [src/api/practice.js:119-125](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:119>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/pages/basic/internship.vue:1-115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) | 未覆盖预定目标 |


## 5. 1623-mobile — 指导教师选择后回填关联资料

```text
search_code("选中指导老师后，怎样自动带出老师的姓名和联系电话？")
```

**搜索语义**：选中指导老师后，怎样自动带出老师的姓名和联系电话？

**预期意图**：指导教师选择后回填关联资料

**期待来源**：src/pages/basic/internship.vue:80 选择教师列表项后回填教师 ID、姓名和手机号，并处理空列表。

- 可替代目标：[src/pages/basic/internship.vue:80](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>)；handleTeacherSelect；关键实现行 [85, 86, 88, 90, 91, 92]。选择教师列表项后回填教师 ID、姓名和手机号，并处理空列表。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>)，对应 [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>)。
- 判断依据：第 1 条包含教师选择弹窗、选中项检查与教师姓名和手机号回填，满足完整动作。
- 冻结目标：第 1 条；[src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>)；核验行 85、86、88、90、91、92。

**执行状态**：有效；真实工具 3.441 秒，批次含索引 43.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | handleTeacherSelect | [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/pages/basic/internship.vue:1-60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) | 仅文件命中 |
| 3 | getTeacherList | [src/api/practice.js:57-62](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:57>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/pages/internship/index.vue:26-34](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:26>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/pages/basic/index.vue:1-49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/index/index.vue:1-49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/pages/index/index.vue:68-74](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) | 未覆盖预定目标 |
| 8 | handleSelect | [src/pages/rating/index.vue:36-42](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:36>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/pages/login/role.vue:53-58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:53>) | 未覆盖预定目标 |
| 10 | loadHomeData | [src/pages/index/index.vue:81-88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:81>) | 未覆盖预定目标 |



## 6. 1623-mobile — 带角色账号登录和会话保存

```text
search_code("用手机号密码登录时，怎样携带学生或老师身份并在成功后记住账号？")
```

**搜索语义**：用手机号密码登录时，怎样携带学生或老师身份并在成功后记住账号？

**预期意图**：带角色账号登录和会话保存

**期待来源**：src/pages/login/account.vue:104 登录请求携带角色，成功后保存令牌、用户资料与登录角色并进入首页。

- 可替代目标：[src/pages/login/account.vue:104](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>)；handleLogin；关键实现行 [113, 114, 115, 116, 123, 124, 125, 126]。登录请求携带角色，成功后保存令牌、用户资料与登录角色并进入首页。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 2 条结果；语义首次可回答名次 [第 2 名](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>)，对应 [src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>)。
- 判断依据：第 1 条只有手机号/密码表单及角色初值，尚无登录动作；第 2 条完整返回携带角色的登录请求、令牌和用户保存及首页跳转。
- 首条结果限制：只定位到了页面状态，不能单独证明提交时携带角色或登录后的会话保存。
- 冻结目标：第 2 条；[src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>)；核验行 113、114、115、116、123、124、125、126。

**执行状态**：有效；真实工具 6.044 秒，批次含索引 43.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 2 / 2；单片段严格诊断名次：2。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788661979769119000-40e118dc62f6-web-1623-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/login/account.vue:68-76](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:68>) | 仅文件命中 |
| 2 | handleLogin | [src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) | 单条覆盖声明及关键行 |
| 3 | loginByPhone | [src/components/LoginWrapper/index.vue:242-262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/pages/login/role.vue:53-58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:53>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/pages/index/index.vue:68-80](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/components/LoginWrapper/index.vue:58-115](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:58>) | 未覆盖预定目标 |
| 7 | getWechatMobile | [src/api/common.js:6-12](</Users/teriri/WebstormProjects/1623-mobile/src/api/common.js:6>) | 未覆盖预定目标 |
| 8 | handleTeacherSelect | [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 未覆盖预定目标 |
| 9 | handleEnter | [src/pages/login/role.vue:60-62](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:60>) | 未覆盖预定目标 |
| 10 | useWechatMobileLogin | [src/hooks/useWechatLogin.js:53-67](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatLogin.js:53>) | 未覆盖预定目标 |



## 7. 1321_frontend — 按日期汇总非零数量提交蛋糕预订

```text
search_code("预订蛋糕时怎样只提交数量大于零的品种，并带上选定的取货日期？")
```

**搜索语义**：预订蛋糕时怎样只提交数量大于零的品种，并带上选定的取货日期？

**预期意图**：按日期汇总非零数量提交蛋糕预订

**期待来源**：src/pages/book/index.vue:286 读取日历或周日期，仅收集正数量蛋糕并发送预订请求。

- 可替代目标：[src/pages/book/index.vue:286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:286>)；handleUpload；关键实现行 [297, 298, 299, 315, 316, 317, 318, 335, 336, 337]。读取日历或周日期，仅收集正数量蛋糕并发送预订请求。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>)，对应 [src/pages/book/index.vue:280-363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>)。
- 判断依据：第 1 条涵盖选定日期的优先级、正数量蛋糕筛选、提交对象、预订接口和成功刷新。函数前多带日历取消方法，不影响目标正文覆盖。
- 冻结目标：第 1 条；[src/pages/book/index.vue:280-363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>)；核验行 297、298、299、315、316、317、318、335、336、337。

**执行状态**：有效；真实工具 5.824 秒，批次含索引 42.7 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | handleUpload | [src/pages/book/index.vue:280-363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) | 单条覆盖声明及关键行 |
| 2 | handleUpload | [src/pages/item/index.vue:336-450](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:336>) | 未覆盖预定目标 |
| 3 | apiIndexBookingCake | [src/api/index.js:168-174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/pages/book/index.vue:45-96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) | 仅文件命中 |
| 5 | 文本块 | [src/pages/stock/index.vue:533-647](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:533>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/stock/index.vue:416-473](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:416>) | 未覆盖预定目标 |
| 7 | apiIndexBookingArticles | [src/api/index.js:249-255](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:249>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/delivery/index.vue:48-99](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:48>) | 未覆盖预定目标 |
| 9 | apiIndexCakeList | [src/api/index.js:138-144](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:138>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/language/zh-CN.js:1-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:1>) | 未覆盖预定目标 |



## 8. 1321_frontend — 损坏数量调整时限制可用库存

```text
search_code("盘点时新增一个损坏蛋糕，怎样扣减可用数量，并防止剩余库存变成负数？")
```

**搜索语义**：盘点时新增一个损坏蛋糕，怎样扣减可用数量，并防止剩余库存变成负数？

**预期意图**：损坏数量调整时限制可用库存

**期待来源**：src/pages/stock/index.vue:459 有剩余时减少剩余数量并同步库存；不足时撤销损坏数量递增。

- 可替代目标：[src/pages/stock/index.vue:459](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>)；handleDamageIncrease；关键实现行 [460, 461, 462, 470]。有剩余时减少剩余数量并同步库存；不足时撤销损坏数量递增。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>)，对应 [src/pages/stock/index.vue:459-472](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>)。
- 判断依据：第 1 条直接显示有库存时减一、同步库存，以及无剩余时撤销损坏数递增，足以回答扣减与防负数问题。
- 冻结目标：第 1 条；[src/pages/stock/index.vue:459-472](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>)；核验行 460、461、462、470。

**执行状态**：有效；真实工具 4.455 秒，批次含索引 42.7 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | handleDamageIncrease | [src/pages/stock/index.vue:459-472](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/pages/stock/index.vue:474-532](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) | 仅文件命中 |
| 3 | default.methods.handleChangeNumber | [src/components/SkuSelector/index.vue:707-716](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:707>) | 未覆盖预定目标 |
| 4 | apiIndexEditStock | [src/api/index.js:413-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) | 未覆盖预定目标 |
| 5 | decrease | [src/components/QuantityControl/index.vue:55-61](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:55>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/book/index.vue:45-104](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) | 未覆盖预定目标 |
| 7 | totalQuantity | [src/pages/book/index.vue:198-208](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:198>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/statistics/index.vue:54-225](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) | 未覆盖预定目标 |
| 9 | increase | [src/components/QuantityControl/index.vue:63-69](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:63>) | 未覆盖预定目标 |
| 10 | apiIndexBookingCake | [src/api/index.js:168-174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) | 未覆盖预定目标 |



## 9. 1321_frontend — 检测库存字段变化并批量保存

```text
search_code("库存盘点保存时怎样找出真正改动的蛋糕，并一并提交剩余、损坏和过期数量？")
```

**搜索语义**：库存盘点保存时怎样找出真正改动的蛋糕，并一并提交剩余、损坏和过期数量？

**预期意图**：检测库存字段变化并批量保存

**期待来源**：src/pages/stock/index.vue:575 对三类数量逐项检测变化，携带日期批量保存，成功后重新加载库存。

- 可替代目标：[src/pages/stock/index.vue:575](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:575>)；handleConfirmChanges；关键实现行 [583, 584, 585, 586, 589, 591, 592, 593, 624, 625, 626, 649]。对三类数量逐项检测变化，携带日期批量保存，成功后重新加载库存。

**人工语义核验**：已定位，部分正文

- 可回答范围：前 10 条结果不足以回答本次意图。
- 判断依据：目标文件第 1 条已找到，目标函数声明在第 3 条；该文本块到第 586 行的字段变化判断即结束。缺少三种数量组成提交对象和实际保存请求，不只是少了不重要的函数尾部。第 4 条 API 包装器仅转发 params，无法补齐页面的参数构造。
- 失败分类：返回正文覆盖不足
- 已观察边界：第 3 条元数据范围 533–587，正文仅到 586 行；587 为空行。
- 诊断范围：当前查询返回窗口不足；仅凭这次结果不能判定向量召回阶段丢失。另一条配送查询的第 4 条返回同函数 575–665 全文，确认源码实现本身完整。
- 冻结目标：第 3 条；[src/pages/stock/index.vue:533-587](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:533>)；核验行 583、584、585、586、589、591、592、593、624、625、626、649。

**执行状态**：有效；真实工具 4.296 秒，批次含索引 42.7 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 3 / None；单片段严格诊断名次：None。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/stock/index.vue:308-360](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:308>) | 仅文件命中 |
| 2 | isValueModified | [src/pages/stock/index.vue:512-520](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:512>) | 仅文件命中 |
| 3 | 文本块 | [src/pages/stock/index.vue:533-587](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:533>) | 声明/块起始覆盖 |
| 4 | apiIndexEditStock | [src/api/index.js:413-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/pages/delivery/index.vue:270-320](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:270>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/statistics/index.vue:54-167](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/pages/book/index.vue:45-96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/delivery/index.vue:100-129](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:100>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/language/zh-CN.js:61-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:61>) | 未覆盖预定目标 |
| 10 | apiIndexBookingCake | [src/api/index.js:168-174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) | 未覆盖预定目标 |




## 10. 1321_frontend — 配送核对保存差异数量

```text
search_code("核对送货数量后，怎样只保存修改过的商品并刷新当日送货清单？")
```

**搜索语义**：核对送货数量后，怎样只保存修改过的商品并刷新当日送货清单？

**预期意图**：配送核对保存差异数量

**期待来源**：src/pages/delivery/index.vue:306 比较原始数量与编辑数量，提交差异商品和日期，成功后重载配送数据。

- 可替代目标：[src/pages/delivery/index.vue:306](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>)；handleConfirmChanges；关键实现行 [312, 313, 314, 315, 332, 333, 334, 335, 345]。比较原始数量与编辑数量，提交差异商品和日期，成功后重载配送数据。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>)，对应 [src/pages/delivery/index.vue:306-377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>)。
- 判断依据：第 1 条包含原始数量比较、差异商品数组、日期转换、配送修改请求与成功后的异步刷新。
- 冻结目标：第 1 条；[src/pages/delivery/index.vue:306-377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>)；核验行 312、313、314、315、332、333、334、335、345。

**执行状态**：有效；真实工具 4.263 秒，批次含索引 42.7 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | handleConfirmChanges | [src/pages/delivery/index.vue:306-377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>) | 单条覆盖声明及关键行 |
| 2 | handleVerify | [src/pages/delivery/index.vue:271-286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:271>) | 仅文件命中 |
| 3 | loadDeliveryData | [src/pages/index/index.vue:59-133](</Users/teriri/WebstormProjects/1321_frontend/src/pages/index/index.vue:59>) | 未覆盖预定目标 |
| 4 | handleConfirmChanges | [src/pages/stock/index.vue:575-665](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:575>) | 未覆盖预定目标 |
| 5 | toggleItemDetails | [src/pages/stock/index.vue:437-448](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:437>) | 未覆盖预定目标 |
| 6 | apiIndexEditSupplyOrder | [src/api/index.js:357-363](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:357>) | 未覆盖预定目标 |
| 7 | apiIndexEditStock | [src/api/index.js:413-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) | 未覆盖预定目标 |
| 8 | apiIndexIndex | [src/api/index.js:112-118](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:112>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/language/en-US.js:61-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:61>) | 未覆盖预定目标 |
| 10 | handleUpload | [src/pages/item/index.vue:367-450](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) | 未覆盖预定目标 |



## 11. 1321_frontend — 本周快捷时间范围及刷新

```text
search_code("统计页点本周后，如何算出周一到周日的范围并自动重新查询？")
```

**搜索语义**：统计页点本周后，如何算出周一到周日的范围并自动重新查询？

**预期意图**：本周快捷时间范围及刷新

**期待来源**：src/pages/statistics/index.vue:180 从当天计算周一与周日，同步日历范围并刷新统计。

- 可替代目标：[src/pages/statistics/index.vue:180](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>)；selectQuickDate；关键实现行 [188, 189, 190, 193, 195, 199, 202]。从当天计算周一与周日，同步日历范围并刷新统计。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>)，对应 [src/pages/statistics/index.vue:180-203](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>)。
- 判断依据：第 1 条包括星期日特殊处理、周一及周日计算、日历同步与自动刷新统计，关键步骤齐全。
- 冻结目标：第 1 条；[src/pages/statistics/index.vue:180-203](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>)；核验行 188、189、190、193、195、199、202。

**执行状态**：有效；真实工具 3.918 秒，批次含索引 42.7 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | selectQuickDate | [src/pages/statistics/index.vue:180-203](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/pages/statistics/index.vue:284-341](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:284>) | 仅文件命中 |
| 3 | generateWeekDates | [src/pages/stock/index.vue:174-198](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:174>) | 未覆盖预定目标 |
| 4 | generateWeekDates | [src/pages/item/index.vue:164-188](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:164>) | 未覆盖预定目标 |
| 5 | generateWeekDates | [src/pages/book/index.vue:106-131](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:106>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/pages/stock/index.vue:201-207](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:201>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/pages/item/index.vue:191-197](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:191>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/book/index.vue:133-142](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:133>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/pages/index/index.vue:1-58](</Users/teriri/WebstormProjects/1321_frontend/src/pages/index/index.vue:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/pages/delivery/index.vue:130-136](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:130>) | 未覆盖预定目标 |



## 12. 1321_frontend — 数量输入失焦校验与边界约束

```text
search_code("手动输入商品数量后，怎样处理空值和非法数字，并把超出范围的值限制到上下限？")
```

**搜索语义**：手动输入商品数量后，怎样处理空值和非法数字，并把超出范围的值限制到上下限？

**预期意图**：数量输入失焦校验与边界约束

**期待来源**：src/components/QuantityControl/index.vue:81 失焦时恢复非法输入，对有效整数按组件上下限裁剪并向父组件发出更新。

- 可替代目标：[src/components/QuantityControl/index.vue:81](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>)；handleInputBlur；关键实现行 [84, 85, 89, 90, 91, 92, 93, 95]。失焦时恢复非法输入，对有效整数按组件上下限裁剪并向父组件发出更新。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>)，对应 [src/components/QuantityControl/index.vue:81-97](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>)。
- 判断依据：第 1 条完整返回失焦处理，空值或无法解析数字时恢复旧值，低于最小值或高于最大值时裁剪并发出更新。
- 源码行为说明：此处按原实现采用 parseInt；检索验收不额外声称其会拒绝所有混合字符形式。
- 冻结目标：第 1 条；[src/components/QuantityControl/index.vue:81-97](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>)；核验行 84、85、89、90、91、92、93、95。

**执行状态**：有效；真实工具 4.559 秒，批次含索引 42.7 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662022906440000-ea9b8b3f99ad-web-1321-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | handleInputBlur | [src/components/QuantityControl/index.vue:81-97](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/components/QuantityControl/index.vue:1-54](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:1>) | 仅文件命中 |
| 3 | default.methods.handleChangeNumber | [src/components/SkuSelector/index.vue:707-716](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:707>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/components/SkuSelector/index.vue:1-120](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:1>) | 未覆盖预定目标 |
| 5 | handleVerify | [src/pages/delivery/index.vue:271-286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:271>) | 未覆盖预定目标 |
| 6 | handleUpload | [src/pages/item/index.vue:367-444](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) | 未覆盖预定目标 |
| 7 | updateStockAfterQuantityChange | [src/pages/stock/index.vue:451-456](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:451>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/pages/book/index.vue:280-363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/pages/stock/index.vue:474-539](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/pages/item/index.vue:49-133](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:49>) | 未覆盖预定目标 |



## 13. 1305_pc — 预约创建的实体核验和服务端保存链路

```text
search_code("新增预约时，怎样先核验会员、服务和员工，再把预约时间提交给服务端？")
```

**搜索语义**：新增预约时，怎样先核验会员、服务和员工，再把预约时间提交给服务端？

**预期意图**：预约创建的实体核验和服务端保存链路

**期待来源**：src/views/appointment-list/index.vue:543 通过表单校验后调用预约创建接口并刷新预约资源。

- 可替代目标：[src/views/appointment-list/index.vue:543](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:543>)；submitForm；关键实现行 [544, 551, 552, 554, 559]。通过表单校验后调用预约创建接口并刷新预约资源。
- 必要目标：[src/views/appointment-list/index.vue:366](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:366>)；buildAppointmentCreateParams；关键实现行 [367, 368, 369, 373, 374, 375, 377, 378, 379, 382, 383]。检查会员、启用的服务和员工有效性，将实体 ID 与起止时间组装为创建参数。

**人工语义核验**：预设链路部分召回，替代实现完整

- 可回答范围：前 5 条结果；语义首次可回答名次 [第 5 名](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>)，对应 [src/views/appointment-board/index.vue:963-986](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>)。
- 判断依据：预设列表页只有校验与参数构造返回，submitForm 未进入前 10 条，因此保持冻结指标未齐。第 2 条和第 5 条却完整返回预约看板的等效链路：核验会员、启用服务和正常员工，随后携带日期及起止时间调用创建接口并刷新。原查询未限定列表页，该替代实现可以完整回答。
- 冻结目标说明：主目标缺失；required_targets 的参数构造完整返回
- 指标说明：仅添加人工语义核验；不增补或修改冻结真值，不将替代命中计入原自动覆盖率。
- 未返回的冻结目标：[src/views/appointment-list/index.vue:543-565](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:543>)；核验行 544、551、552、554、559。
- 冻结目标：第 1 条；[src/views/appointment-list/index.vue:302-412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:302>)；核验行 367、368、369、373、374、375、377、378、379、382、383。
- 替代实现：第 2 条；[src/views/appointment-board/index.vue:918-955](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:918>)；核验行 919、920、923、929、932、951、954。
- 替代实现：第 5 条；[src/views/appointment-board/index.vue:963-986](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>)；核验行 964、968、969、970、971、973、974、975、981。

**执行状态**：有效；真实工具 4.109 秒，批次含索引 179.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / None / None；单片段严格诊断名次：None。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/views/appointment-list/index.vue:302-412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:302>) | 单条覆盖声明及关键行 |
| 2 | resolveAppointmentCreateContext | [src/views/appointment-board/index.vue:882-955](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:882>) | 未覆盖预定目标 |
| 3 | useBusinessStore.createAppointment | [src/store/useBusinessStore.js:6071-6111](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6071>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/views/appointment-board/index.vue:184-243](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:184>) | 未覆盖预定目标 |
| 5 | submitAppointmentCreateApi | [src/views/appointment-board/index.vue:963-986](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/views/appointment-board/index.vue:302-361](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) | 未覆盖预定目标 |
| 7 | useBusinessStore.resolveAppointmentMemberContext | [src/store/useBusinessStore.js:6032-6041](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6032>) | 未覆盖预定目标 |
| 8 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121-6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/views/member-list/index.vue:853-907](</Users/teriri/WebstormProjects/1305_pc/src/views/member-list/index.vue:853>) | 未覆盖预定目标 |
| 10 | submitForm | [src/views/member-list/index.vue:924-945](</Users/teriri/WebstormProjects/1305_pc/src/views/member-list/index.vue:924>) | 未覆盖预定目标 |


## 14. 1305_pc — 确认取消预约、后端状态写入和刷新

```text
search_code("用户取消预约并确认后，怎样更新服务器状态并刷新预约列表？")
```

**搜索语义**：用户取消预约并确认后，怎样更新服务器状态并刷新预约列表？

**预期意图**：确认取消预约、后端状态写入和刷新

**期待来源**：src/views/appointment-list/index.vue:604 确认弹窗后根据远端 ID 发送取消状态并刷新列表；本地记录走现有状态更新。

- 可替代目标：[src/views/appointment-list/index.vue:604](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>)；cancelRow；关键实现行 [606, 607, 609, 610, 612]。确认弹窗后根据远端 ID 发送取消状态并刷新列表；本地记录走现有状态更新。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>)，对应 [src/views/appointment-list/index.vue:604-620](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>)。
- 判断依据：第 1 条包括取消确认、远端预约状态写入和预约资源刷新，也保留本地预约的分支处理。
- 冻结目标：第 1 条；[src/views/appointment-list/index.vue:604-620](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>)；核验行 606、607、609、610、612。

**执行状态**：有效；真实工具 3.512 秒，批次含索引 179.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | cancelRow | [src/views/appointment-list/index.vue:604-620](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>) | 单条覆盖声明及关键行 |
| 2 | submitAppointmentCancelApi | [src/views/appointment-board/index.vue:994-1003](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:994>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/views/appointment-list/index.vue:622-677](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:622>) | 仅文件命中 |
| 4 | 文本块 | [src/views/appointment-board/index.vue:1195-1201](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1195>) | 未覆盖预定目标 |
| 5 | toggleStatus | [src/views/cashier-advance/index.vue:490-509](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-advance/index.vue:490>) | 未覆盖预定目标 |
| 6 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121-6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) | 未覆盖预定目标 |
| 7 | toggleStatus | [src/views/document-consume/index.vue:636-648](</Users/teriri/WebstormProjects/1305_pc/src/views/document-consume/index.vue:636>) | 未覆盖预定目标 |
| 8 | toggleStatus | [src/views/document-package-sale/index.vue:635-647](</Users/teriri/WebstormProjects/1305_pc/src/views/document-package-sale/index.vue:635>) | 未覆盖预定目标 |
| 9 | toggleStatus | [src/views/document-card-refund/index.vue:514-526](</Users/teriri/WebstormProjects/1305_pc/src/views/document-card-refund/index.vue:514>) | 未覆盖预定目标 |
| 10 | toggleStatus | [src/views/document-return/index.vue:525-537](</Users/teriri/WebstormProjects/1305_pc/src/views/document-return/index.vue:525>) | 未覆盖预定目标 |



## 15. 1305_pc — 不同支付类型合并为服务端支付明细

```text
search_code("组合收款时，怎样把会员卡、次卡和普通支付统一转换成接口需要的支付配置和金额？")
```

**搜索语义**：组合收款时，怎样把会员卡、次卡和普通支付统一转换成接口需要的支付配置和金额？

**预期意图**：不同支付类型合并为服务端支付明细

**期待来源**：src/views/order-settlement/index.vue:864 合并卡余额支付、次卡扣次和普通支付，映射为支付配置 ID 与字符串金额。

- 可替代目标：[src/views/order-settlement/index.vue:864](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>)；buildOrderSettlementApiPayArr；关键实现行 [865, 866, 868, 869, 871, 873, 875, 876]。合并卡余额支付、次卡扣次和普通支付，映射为支付配置 ID 与字符串金额。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>)，对应 [src/views/order-settlement/index.vue:864-878](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>)。
- 判断依据：第 1 条完整合并会员卡、次卡和普通支付，并将每项映射为支付配置 ID 与金额，正是原查询要求的接口转换。
- 冻结目标：第 1 条；[src/views/order-settlement/index.vue:864-878](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>)；核验行 865、866、868、869、871、873、875、876。

**执行状态**：有效；真实工具 4.381 秒，批次含索引 179.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | buildOrderSettlementApiPayArr | [src/views/order-settlement/index.vue:864-878](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/views/order-settlement/index.vue:791-850](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:791>) | 仅文件命中 |
| 3 | buildCardOpenApiPayArr | [src/views/business-flow/index.vue:1586-1633](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:1586>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/views/business-flow/index.vue:2490-2496](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2490>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/views/business-flow/index.vue:2726-2788](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2726>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/views/cashier-counter/index.vue:587-635](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-counter/index.vue:587>) | 未覆盖预定目标 |
| 7 | useBusinessStore.normalizePackageBuyPaymentLines | [src/store/useBusinessStore.js:5288-5315](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:5288>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/views/cashier-counter/index.vue:513-532](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-counter/index.vue:513>) | 未覆盖预定目标 |
| 9 | useBusinessStore.settleOrder | [src/store/useBusinessStore.js:4550-4699](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4550>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/views/home-action/index.vue:2076-2132](</Users/teriri/WebstormProjects/1305_pc/src/views/home-action/index.vue:2076>) | 未覆盖预定目标 |



## 16. 1305_pc — 启用权限筛选及菜单映射

```text
search_code("员工接口返回的权限规则，怎样只保留已启用项并转换成桌面菜单能识别的权限？")
```

**搜索语义**：员工接口返回的权限规则，怎样只保留已启用项并转换成桌面菜单能识别的权限？

**预期意图**：启用权限筛选及菜单映射

**期待来源**：src/utils/pcAdminSession.js:308 保留始终可用权限，筛选启用规则，映射并去重为桌面菜单权限。

- 可替代目标：[src/utils/pcAdminSession.js:308](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>)；resolvePcPermissionKeys；关键实现行 [309, 311, 312, 313, 316]。保留始终可用权限，筛选启用规则，映射并去重为桌面菜单权限。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>)，对应 [src/utils/pcAdminSession.js:308-317](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>)。
- 判断依据：第 1 条展示启用规则筛选、规则名提取、菜单权限映射与集合去重；第 10 条还补充具体映射函数。
- 冻结目标：第 1 条；[src/utils/pcAdminSession.js:308-317](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>)；核验行 309、311、312、313、316。

**执行状态**：有效；真实工具 4.182 秒，批次含索引 179.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | resolvePcPermissionKeys | [src/utils/pcAdminSession.js:308-317](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/views/employee-permission/index.vue:117-219](</Users/teriri/WebstormProjects/1305_pc/src/views/employee-permission/index.vue:117>) | 未覆盖预定目标 |
| 3 | syncStaffPermissions | [src/utils/pcAdminSession.js:448-455](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:448>) | 仅文件命中 |
| 4 | employeeDefaultPermissions | [src/config/pcAdmin.js:41-179](</Users/teriri/WebstormProjects/1305_pc/src/config/pcAdmin.js:41>) | 未覆盖预定目标 |
| 5 | backendAuthorizedMenuKeys | [src/config/pcAdmin.js:17-25](</Users/teriri/WebstormProjects/1305_pc/src/config/pcAdmin.js:17>) | 未覆盖预定目标 |
| 6 | flattenMenuItems | [src/router/system.js:637-655](</Users/teriri/WebstormProjects/1305_pc/src/router/system.js:637>) | 未覆盖预定目标 |
| 7 | normalizeEmployeePermissions | [src/store/useAdminStore.js:74-76](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:74>) | 未覆盖预定目标 |
| 8 | useAdminStore.syncEmployeePermissionsFromApi | [src/store/useAdminStore.js:242-244](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:242>) | 未覆盖预定目标 |
| 9 | useAdminStore.setEmployeePermissions | [src/store/useAdminStore.js:233-235](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:233>) | 未覆盖预定目标 |
| 10 | resolvePcPermissionKeysByRuleName | [src/utils/pcAdminSession.js:297-300](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:297>) | 仅文件命中 |




## 17. 1305_pc — 会话资料读取中的缓存与并发请求合并

```text
search_code("多个页面同时读取登录后的门店资料时，怎样避免重复请求并复用短时间缓存？")
```

**搜索语义**：多个页面同时读取登录后的门店资料时，怎样避免重复请求并复用短时间缓存？

**预期意图**：会话资料读取中的缓存与并发请求合并

**期待来源**：src/utils/pcAdminSession.js:520 检查登录和缓存期限，共享进行中的同步 Promise 并在完成后释放。

- 可替代目标：[src/utils/pcAdminSession.js:520](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>)；ensurePcAdminSession；关键实现行 [524, 528, 529, 532, 533, 537, 538, 541]。检查登录和缓存期限，共享进行中的同步 Promise 并在完成后释放。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>)，对应 [src/utils/pcAdminSession.js:520-543](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>)。
- 判断依据：第 1 条展示登录检查、缓存时限判断、共享同步 Promise 及完成后的释放；缓存和并发合并两个条件均覆盖。
- 冻结目标：第 1 条；[src/utils/pcAdminSession.js:520-543](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>)；核验行 524、528、529、532、533、537、538、541。

**执行状态**：有效；真实工具 4.175 秒，批次含索引 179.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | ensurePcAdminSession | [src/utils/pcAdminSession.js:520-543](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>) | 单条覆盖声明及关键行 |
| 2 | syncPcBusinessResources | [src/utils/pcBusinessResources.js:2703-2728](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:2703>) | 未覆盖预定目标 |
| 3 | syncPcAdminSession | [src/utils/pcAdminSession.js:462-511](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:462>) | 仅文件命中 |
| 4 | pageDescription | [src/views/auth/AuthPage.vue:155-163](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:155>) | 未覆盖预定目标 |
| 5 | resetPcBusinessResourcesCache | [src/utils/pcBusinessResources.js:2755-2758](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:2755>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/views/auth/AuthPage.vue:292-299](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:292>) | 未覆盖预定目标 |
| 7 | resetPcAdminSessionCache | [src/utils/pcAdminSession.js:548-550](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:548>) | 仅文件命中 |
| 8 | 文本块 | [src/views/appointment-board/index.vue:1080-1117](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1080>) | 未覆盖预定目标 |
| 9 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:44-68](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:44>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/views/auth/AuthPage.vue:355-397](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:355>) | 未覆盖预定目标 |




## 18. 1305_pc — 从门店资料推导可用状态

```text
search_code("门店资料返回后，怎样区分未绑定、停用、已过期和可正常使用？")
```

**搜索语义**：门店资料返回后，怎样区分未绑定、停用、已过期和可正常使用？

**预期意图**：从门店资料推导可用状态

**期待来源**：src/utils/pcAdminSession.js:159 依次判断空资料、禁用标记及非永久会员到期时间，得到门店进入状态。

- 可替代目标：[src/utils/pcAdminSession.js:159](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>)；resolveStoreStatusFromPayload；关键实现行 [160, 163, 164, 167, 168, 169, 170, 173]。依次判断空资料、禁用标记及非永久会员到期时间，得到门店进入状态。

**人工语义核验**：已定位，完整正文；另有首位替代实现

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:305>)，对应 [src/views/auth/AuthPage.vue:305-354](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:305>)。
- 判断依据：冻结的资料状态推导函数在第 2 条完整返回。第 1 条 AuthPage 文本块中的 resolveStoreStatus 也完整判断未绑定、停用、过期和正常四种状态，已能回答原查询。两种实现对永久会员等边界不同，本查询只询问四态推导。
- 指标说明：冻结名次仍保留第 2 名，首位替代实现仅用于人工语义核验。
- 冻结目标：第 2 条；[src/utils/pcAdminSession.js:159-174](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>)；核验行 160、163、164、167、168、169、170、173。
- 替代实现：第 1 条；[src/views/auth/AuthPage.vue:330-353](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:330>)；核验行 331、333、334、338、343、346、347、348、349、352。

**执行状态**：有效；真实工具 4.114 秒，批次含索引 179.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：2 / 2 / 2；单片段严格诊断名次：2。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662065636289000-426e74ea9ee4-web-1305-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/views/auth/AuthPage.vue:305-354](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:305>) | 未覆盖预定目标 |
| 2 | resolveStoreStatusFromPayload | [src/utils/pcAdminSession.js:159-174](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>) | 单条覆盖声明及关键行 |
| 3 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:50-68](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:50>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/views/auth/StoreStatusPage.vue:30-43](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:30>) | 未覆盖预定目标 |
| 5 | resolveStoreStatusFromError | [src/utils/pcAdminSession.js:199-205](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:199>) | 仅文件命中 |
| 6 | useAdminStore.syncCurrentStoreFromApi | [src/store/useAdminStore.js:198-212](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:198>) | 未覆盖预定目标 |
| 7 | resolveStoreCardStatus.statusMap | [src/utils/pcBusinessResources.js:1980-1986](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1980>) | 未覆盖预定目标 |
| 8 | resolveTimeCardSaleStatus.statusMap | [src/utils/pcBusinessResources.js:1947-1953](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1947>) | 未覆盖预定目标 |
| 9 | resolveUserPackageCardStatus.statusMap | [src/utils/pcBusinessResources.js:732-739](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:732>) | 未覆盖预定目标 |
| 10 | isUsableMemberCard | [src/views/home-action/index.vue:1381-1391](</Users/teriri/WebstormProjects/1305_pc/src/views/home-action/index.vue:1381>) | 未覆盖预定目标 |




## 19. 1573-frontend — 视频播放阈值触发一次互动暂停

```text
search_code("课程视频播放到互动节点时，哪里判断是否该暂停视频并弹出互动内容？")
```

**搜索语义**：课程视频播放到互动节点时，哪里判断是否该暂停视频并弹出互动内容？

**预期意图**：视频播放阈值触发一次互动暂停

**期待来源**：apps/web/src/pages/VideoSegment.vue:75 依据视频时长、当前进度、互动状态与完成标记判断是否进入互动。

- 可替代目标：[apps/web/src/pages/VideoSegment.vue:75](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>)；maybeOpenInteraction；关键实现行 [79, 80, 81, 82, 83, 88]。依据视频时长、当前进度、互动状态与完成标记判断是否进入互动。
- 必要目标：[apps/web/src/pages/VideoSegment.vue:69](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:69>)；openInteraction；关键实现行 [70, 71, 72]。激活互动显示状态并暂停当前视频。

**人工语义核验**：已定位，跨方法正文齐全

- 可回答范围：前 3 条结果；语义首次可回答名次 [第 3 名](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>)，对应 [apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>)。
- 判断依据：第 1 条负责按进度和互动状态判定是否触发；第 3 条文本块包含激活互动并暂停视频的 openInteraction。前 3 条合起来才覆盖判断和暂停两项必要行为。
- 跨目标核验：maybeOpenInteraction 在第 1 条完整出现，openInteraction 在第 3 条文本块 69–73 行完整出现。
- 冻结目标：第 1 条；[apps/web/src/pages/VideoSegment.vue:75-89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>)；核验行 79、80、81、82、83、88。
- 冻结目标：第 3 条；[apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>)；核验行 70、71、72。

**执行状态**：有效；真实工具 6.473 秒，批次含索引 30.8 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 3 / 3；单片段严格诊断名次：3。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75-89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:111-226](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:111>) | 仅文件命中 |
| 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 单条覆盖声明及关键行 |
| 4 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖预定目标 |
| 5 | segments | [apps/web/src/pages/PackageDetail.vue:1-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [apps/web/src/pages/PackageList.vue:50-82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:50>) | 未覆盖预定目标 |
| 7 | 文本块 | [apps/web/src/pages/ModelViewer.vue:22-28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:22>) | 未覆盖预定目标 |
| 8 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97-98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) | 未覆盖预定目标 |
| 9 | createJointControls | [apps/web/src/pages/ModelViewer.vue:103-110](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:103>) | 未覆盖预定目标 |
| 10 | appendLog | [apps/web/src/pages/ModelViewer.vue:126-128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:126>) | 未覆盖预定目标 |




## 20. 1573-frontend — 视频进度条跳转和互动触发

```text
search_code("点击视频进度条时，如何把鼠标位置换算成播放时间并检查是否进入互动？")
```

**搜索语义**：点击视频进度条时，如何把鼠标位置换算成播放时间并检查是否进入互动？

**预期意图**：视频进度条跳转和互动触发

**期待来源**：apps/web/src/pages/VideoSegment.vue:185 用点击点相对进度条宽度的比例设置播放时间，并重新检查互动节点。

- 可替代目标：[apps/web/src/pages/VideoSegment.vue:185](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>)；seekVideo；关键实现行 [187, 191, 197, 198, 199, 202]。用点击点相对进度条宽度的比例设置播放时间，并重新检查互动节点。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>)，对应 [apps/web/src/pages/VideoSegment.vue:185-203](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>)。
- 判断依据：第 1 条用点击位置相对于进度条的比例计算播放时间，并校验视频状态和时长、更新当前时间及检查互动。
- 冻结目标：第 1 条；[apps/web/src/pages/VideoSegment.vue:185-203](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>)；核验行 187、191、197、198、199、202。

**执行状态**：有效；真实工具 4.356 秒，批次含索引 30.8 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | seekVideo | [apps/web/src/pages/VideoSegment.vue:185-203](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-110](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 仅文件命中 |
| 3 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132-140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) | 仅文件命中 |
| 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:280-334](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:280>) | 仅文件命中 |
| 5 | segments | [apps/web/src/pages/PackageDetail.vue:1-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97-98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) | 未覆盖预定目标 |
| 7 | initializeScene.handleCanvasClick | [apps/web/src/pages/ModelViewer.vue:592-612](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:592>) | 未覆盖预定目标 |
| 8 | handleMove | [apps/web/src/components/neural/SVGDraggableNumber.vue:37-47](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:37>) | 未覆盖预定目标 |
| 9 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:21-28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:21>) | 未覆盖预定目标 |
| 10 | handleMove | [apps/web/src/components/neural/DraggableNumber.vue:32-42](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>) | 未覆盖预定目标 |





## 21. 1573-frontend — 响应式神经网络前向计算及分类准确率

```text
search_code("拖动样本或调整权重以后，神经网络演示怎样重新计算分类结果和正确率？")
```

**搜索语义**：拖动样本或调整权重以后，神经网络演示怎样重新计算分类结果和正确率？

**预期意图**：响应式神经网络前向计算及分类准确率

**期待来源**：apps/web/src/components/NeuralGridDemo.vue:93 结合拖动偏移和权重计算两层线性输出，以符号判定类别并统计正确率。

- 可替代目标：[apps/web/src/components/NeuralGridDemo.vue:93](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:93>)；processed；关键实现行 [99, 100, 101, 104, 105, 106, 109, 111, 112, 132]。结合拖动偏移和权重计算两层线性输出，以符号判定类别并统计正确率。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>)，对应 [apps/web/src/components/NeuralGridDemo.vue:1-155](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>)。
- 判断依据：第 1 条虽返回较大的 1–155 行片段，但其中 93–134 行完整包含拖动偏移、权重前向计算、分类判断与正确率统计。
- 结果标签说明：符号名 processed 正确，返回窗口含较多类型和初始状态；该较大窗口未截断关键计算。
- 冻结目标：第 1 条；[apps/web/src/components/NeuralGridDemo.vue:1-155](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>)；核验行 99、100、101、104、105、106、109、111、112、132。

**执行状态**：有效；真实工具 3.542 秒，批次含索引 30.8 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | processed | [apps/web/src/components/NeuralGridDemo.vue:1-155](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:215-275](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:215>) | 仅文件命中 |
| 3 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:277-452](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:277>) | 仅文件命中 |
| 4 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:661-718](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:661>) | 仅文件命中 |
| 5 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:21-28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:21>) | 未覆盖预定目标 |
| 6 | 文本块 | [apps/web/src/components/neural/DraggableNumber.vue:17-23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:17>) | 未覆盖预定目标 |
| 7 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 未覆盖预定目标 |
| 8 | handleUp | [apps/web/src/components/neural/DraggableNumber.vue:44-71](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:44>) | 未覆盖预定目标 |
| 9 | handleUp | [apps/web/src/components/neural/SVGDraggableNumber.vue:49-59](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:49>) | 未覆盖预定目标 |
| 10 | handleDown | [apps/web/src/components/neural/SVGDraggableNumber.vue:30-35](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:30>) | 未覆盖预定目标 |





## 22. 1573-frontend — 浏览器认证状态恢复及损坏数据清理

```text
search_code("浏览器刷新后，怎样从本地恢复上次登录的账号和凭证，数据损坏时又怎么处理？")
```

**搜索语义**：浏览器刷新后，怎样从本地恢复上次登录的账号和凭证，数据损坏时又怎么处理？

**预期意图**：浏览器认证状态恢复及损坏数据清理

**期待来源**：apps/web/src/context/auth.ts:28 读取保存的令牌和用户，解析并恢复认证状态；解析异常时退出清理会话。

- 可替代目标：[apps/web/src/context/auth.ts:28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>)；initializeAuth；关键实现行 [29, 30, 33, 34, 35, 37, 40]。读取保存的令牌和用户，解析并恢复认证状态；解析异常时退出清理会话。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>)，对应 [apps/web/src/context/auth.ts:28-41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>)。
- 判断依据：第 1 条完整展示本地令牌与用户读取、JSON 解析、登录状态恢复以及异常时退出；第 3 条还给出清理存储的 logout。
- 冻结目标：第 1 条；[apps/web/src/context/auth.ts:28-41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>)；核验行 29、30、33、34、35、37、40。

**执行状态**：有效；真实工具 3.153 秒，批次含索引 30.8 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | initializeAuth | [apps/web/src/context/auth.ts:28-41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) | 单条覆盖声明及关键行 |
| 2 | saveSession | [apps/web/src/context/auth.ts:43-49](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:43>) | 仅文件命中 |
| 3 | logout | [apps/web/src/context/auth.ts:76-83](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:76>) | 仅文件命中 |
| 4 | 文本块 | [apps/web/src/pages/Login.vue:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [apps/web/src/pages/Login.vue:45-101](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:45>) | 未覆盖预定目标 |
| 6 | 文本块 | [apps/web/src/pages/Login.vue:13-14](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:13>) | 未覆盖预定目标 |
| 7 | 文本块 | [apps/api/src/app.ts:16-25](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:16>) | 未覆盖预定目标 |
| 8 | 文本块 | [apps/api/src/auth.ts:15-16](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:15>) | 未覆盖预定目标 |
| 9 | 文本块 | [apps/web/src/main.ts:1-9](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/main.ts:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [apps/api/src/auth.ts:60-66](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:60>) | 未覆盖预定目标 |




## 23. 1573-frontend — 令牌签名校验和过期拒绝

```text
search_code("服务端怎样检查登录令牌有没有被篡改或已经过期？")
```

**搜索语义**：服务端怎样检查登录令牌有没有被篡改或已经过期？

**预期意图**：令牌签名校验和过期拒绝

**期待来源**：apps/api/src/auth.ts:83 拆分令牌，计算并比较签名，解析并验证载荷与到期时间。

- 可替代目标：[apps/api/src/auth.ts:83](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>)；verifyJwt；关键实现行 [84, 85, 89, 90, 96, 101, 102, 105]。拆分令牌，计算并比较签名，解析并验证载荷与到期时间。

**人工语义核验**：已定位，完整正文

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>)，对应 [apps/api/src/auth.ts:83-106](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>)。
- 判断依据：第 1 条完整返回令牌签名比较、载荷解析和到期检查，篡改或过期均返回空值。
- 冻结目标：第 1 条；[apps/api/src/auth.ts:83-106](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>)；核验行 84、85、89、90、96、101、102、105。

**执行状态**：有效；真实工具 3.627 秒，批次含索引 30.8 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | verifyJwt | [apps/api/src/auth.ts:83-106](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>) | 单条覆盖声明及关键行 |
| 2 | getUserByToken | [apps/api/src/auth.ts:117-138](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:117>) | 仅文件命中 |
| 3 | 文本块 | [apps/api/src/auth.ts:8-11](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:8>) | 仅文件命中 |
| 4 | initializeAuth | [apps/web/src/context/auth.ts:28-41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) | 未覆盖预定目标 |
| 5 | AuthSession | [apps/web/src/context/auth.ts:8-11](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:8>) | 未覆盖预定目标 |
| 6 | saveSession | [apps/web/src/context/auth.ts:43-49](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:43>) | 未覆盖预定目标 |
| 7 | getWebSocketToken | [apps/api/src/app.ts:128-136](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:128>) | 未覆盖预定目标 |
| 8 | app | [apps/api/src/app.ts:138-306](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:138>) | 未覆盖预定目标 |
| 9 | env | [apps/api/src/env.ts:8-14](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/env.ts:8>) | 未覆盖预定目标 |
| 10 | 文本块 | [apps/api/src/app.ts:16-25](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:16>) | 未覆盖预定目标 |




## 24. 1573-frontend — 模板按课程锁定状态切换视觉与标识

```text
search_code("课程包尚未解锁时，界面在哪里把卡片变灰并显示锁定标志？")
```

**搜索语义**：课程包尚未解锁时，界面在哪里把卡片变灰并显示锁定标志？

**预期意图**：模板按课程锁定状态切换视觉与标识

**期待来源**：apps/web/src/pages/PackageList.vue:56 Vue 模板根据锁定状态添加灰度、降低透明度、锁图标和覆盖层。

- 可替代目标：[apps/web/src/pages/PackageList.vue:56](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:56>)；代码块；关键实现行 [88, 95, 97, 103, 119, 120, 122]。Vue 模板根据锁定状态添加灰度、降低透明度、锁图标和覆盖层。

**人工语义核验**：已定位，完整模板正文；冻结指标低估

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:83>)，对应 [apps/web/src/pages/PackageList.vue:83-128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:83>)。
- 判断依据：第 1 条 83–128 行覆盖全部冻结关键行：按锁定状态应用灰度和透明度、显示锁图标及文字、添加锁定覆盖层。外层 <template> 起点第 56 行未一并返回，导致本轮要求块起始行的严格声明和正文指标为空；这不是搜索语义或实际正文的未命中。
- 指标说明：保持本轮冻结判据及自动指标；人工核验按实际模板行为确认第 1 名完整回答。
- 实际模板正文：第 1 条；[apps/web/src/pages/PackageList.vue:83-128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:83>)；核验行 88、95、97、103、119、120、122。

**执行状态**：有效；真实工具 3.354 秒，批次含索引 30.8 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / None / None；单片段严格诊断名次：None。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662244869462000-9654beeb071b-web-1573-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [apps/web/src/pages/PackageList.vue:83-128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:83>) | 仅文件命中 |
| 2 | packages | [apps/web/src/pages/PackageList.vue:19-32](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:19>) | 仅文件命中 |
| 3 | 文本块 | [apps/web/src/pages/PackageDetail.vue:1-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) | 未覆盖预定目标 |
| 4 | 文本块 | [apps/web/src/pages/Login.vue:1-101](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [apps/web/src/pages/VideoSegment.vue:221-279](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:221>) | 未覆盖预定目标 |
| 6 | hoverOverlay.isDraggingThis | [apps/web/src/components/NeuralGridDemo.vue:156-156](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) | 未覆盖预定目标 |
| 7 | 文本块 | [apps/web/src/pages/VideoSegment.vue:7-34](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:7>) | 未覆盖预定目标 |
| 8 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:1-44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [apps/web/src/pages/VideoSegment.vue:42-44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:42>) | 未覆盖预定目标 |
| 10 | formatServerMessage | [apps/web/src/pages/ModelViewer.vue:644-654](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:644>) | 未覆盖预定目标 |



## 25. bid-review-app — 定位长文档切分时长度、边界及重叠区间的具体计算

```text
search_code("文档太长时怎样切成多段，并让相邻片段保留一部分相同内容？")
```

**搜索语义**：文档太长时怎样切成多段，并让相邻片段保留一部分相同内容？

**预期意图**：定位长文档切分时长度、边界及重叠区间的具体计算

**期待来源**：循环限定块长，选择适当边界，后续块从重叠起点截取，构造带重叠元数据的文本块。

- 可替代目标：[src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98>)；createChunks；关键实现行 [104, 108, 121, 126, 131]。循环限定块长，选择适当边界，后续块从重叠起点截取，构造带重叠元数据的文本块。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 2 条结果；语义首次可回答名次 [第 2 名](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>)，对应 [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1-167](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>)。
- 判断依据：Top1 只有另一分块器的说明与配置；Top2 已返回创建片段方法的长度限制、边界选择和重叠截取，足以回答。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 2 条；[src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1-167](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>)；核验行 104、108、121、126、131。循环限定块长，选择适当边界，后续块从重叠起点截取，构造带重叠元数据的文本块。

**执行状态**：有效；真实工具 7.499 秒，批次含索引 86.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：2 / 2 / 2；单片段严格诊断名次：2。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1-55](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) | 未覆盖预定目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1-167](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>) | 单条覆盖声明及关键行 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:112-228](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:112>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:172-225](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:172>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1-57](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455-512](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164-206](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:120-174](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:120>) | 未覆盖预定目标 |
| 10 | 文本块 | [docs/bid-review-architecture.md:210-262](</Users/teriri/IdeaProjects/bid-review-app/docs/bid-review-architecture.md:210>) | 未覆盖预定目标 |



## 26. bid-review-app — 定位结构化模型结果的递归清洗及去重实现

```text
search_code("模型抽取结果里重复的条目和空白内容在哪里被清理，同时不能把零和假值丢掉？")
```

**搜索语义**：模型抽取结果里重复的条目和空白内容在哪里被清理，同时不能把零和假值丢掉？

**预期意图**：定位结构化模型结果的递归清洗及去重实现

**期待来源**：字符串去空白，对象递归清理，数组按规范键保留首项，数值与布尔值直接保留。

- 可替代目标：[src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:310](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:310>)；cleanAndDedupe；关键实现行 [315, 323, 335, 339, 346]。字符串去空白，对象递归清理，数组按规范键保留首项，数值与布尔值直接保留。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289>)，对应 [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289-348](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289>)。
- 判断依据：Top1 包含递归清理完整函数，可见空白删除、数组规范键去重及数值/布尔值保留分支。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289-348](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289>)；核验行 315、323、335、339、346。字符串去空白，对象递归清理，数组按规范键保留首项，数值与布尔值直接保留。

**执行状态**：有效；真实工具 5.020 秒，批次含索引 86.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289-348](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:175-231](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:175>) | 仅文件命中 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:423-523](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:423>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:401-458](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:401>) | 仅文件命中 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/model/document/MarkdownExtractionResult.java:1-56](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/MarkdownExtractionResult.java:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/model/document/TextExtractionResult.java:1-44](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/TextExtractionResult.java:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/VllmOcrStrategy.java:232-304](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/VllmOcrStrategy.java:232>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/model/document/BatchTextExtractionResult.java:1-49](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/BatchTextExtractionResult.java:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextOptimizationService.java:118-290](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextOptimizationService.java:118>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:253-309](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:253>) | 未覆盖预定目标 |




## 27. bid-review-app — 定位复审请求内容的组装，包含已有结论、原文件及可选指示

```text
search_code("再次检查投标材料时，哪里把招标要求、第一次判断和原始标书一起交给模型？")
```

**搜索语义**：再次检查投标材料时，哪里把招标要求、第一次判断和原始标书一起交给模型？

**预期意图**：定位复审请求内容的组装，包含已有结论、原文件及可选指示

**期待来源**：组装招标要求、初审结论、原始投标文本与自定义提示，交给复审用户模板渲染。

- 可替代目标：[src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90>)；buildReReviewUserContent；关键实现行 [94, 95, 96, 100, 105]。组装招标要求、初审结论、原始投标文本与自定义提示，交给复审用户模板渲染。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 2 条结果；语义首次可回答名次 [第 2 名](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>)，对应 [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288-314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>)。
- 判断依据：冻结目标方法没有召回；Top2 是调用链上的另一完整组装方法，直接把招标要求、初审结论与标书文本放入参数，并渲染复审模板或调用已冻结目标，足以定位原问题。源码113行实际调用此方法。Top1 是上传的招标原文，不能作为代码实现命中。
- 冻结目标说明：冻结文件、声明、关键正文均未命中；保留自动指标 null
- 实际返回结果：第 2 条；[src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288-314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>)；核验行 296、301、302、303、306、312。buildSecondaryReviewUserContent 的完整正文把三项输入一起组装并选择复审模板；源码113行有活动调用。

**执行状态**：有效；真实工具 12.818 秒，批次含索引 86.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：None / None / None；单片段严格诊断名次：None。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) | 未覆盖预定目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288-314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>) | 未覆盖预定目标 |
| 3 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:692-751](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:692>) | 未覆盖预定目标 |
| 4 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-full.txt:181-240](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-full.txt:181>) | 未覆盖预定目标 |
| 5 | 文本块 | [uploads/bid-review/a3a83331-cebb-4d67-a86d-05b8dc0dee9a/requirement-original.txt:361-420](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/a3a83331-cebb-4d67-a86d-05b8dc0dee9a/requirement-original.txt:361>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/resources/template-config.yaml:1-24](</Users/teriri/IdeaProjects/bid-review-app/src/main/resources/template-config.yaml:1>) | 未覆盖预定目标 |


## 28. bid-review-app — 定位文档来源识别、提取分支、失败校验和文字合并

```text
search_code("给一个本地文档路径或远程链接，哪里会读取内容并提取可供审查的文字？")
```

**搜索语义**：给一个本地文档路径或远程链接，哪里会读取内容并提取可供审查的文字？

**预期意图**：定位文档来源识别、提取分支、失败校验和文字合并

**期待来源**：按 URL/文件路径选择对应提取方法，非成功状态抛错，最终合并文字页内容。

- 可替代目标：[src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27>)；extractTextContent；关键实现行 [31, 37, 40, 44, 48]。按 URL/文件路径选择对应提取方法，非成功状态抛错，最终合并文字页内容。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 3 条结果；语义首次可回答名次 [第 3 名](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>)，对应 [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>)。
- 判断依据：Top1 有提取入口但只接收文档对象/输入流；Top2 明确拒绝 FILE 类型，缺从本地路径打开文件的分支。Top3 同时展示本地路径读取与 URL 提取两条分支及结果处理，才完整回答。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 3 条；[src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>)；核验行 31、37、40、44、48。按 URL/文件路径选择对应提取方法，非成功状态抛错，最终合并文字页内容。

**执行状态**：有效；真实工具 5.007 秒，批次含索引 86.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：3 / 3 / 3；单片段严格诊断名次：3。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1-120](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1>) | 未覆盖预定目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181-234](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>) | 单条覆盖声明及关键行 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1-99](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1-54](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:583-616](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:583>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1-56](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1-30](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1>) | 未覆盖预定目标 |
| 9 | DocumentInfo | [bidreview-app-ui/src/types/index.ts:17-23](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/types/index.ts:17>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:546-656](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:546>) | 未覆盖预定目标 |



## 29. bid-review-app — 定位 OCR 提供商失败后的筛选、优先级切换和全部失败处理

```text
search_code("图片识字服务失败以后，系统在哪按顺序换其他可用厂商继续尝试？")
```

**搜索语义**：图片识字服务失败以后，系统在哪按顺序换其他可用厂商继续尝试？

**预期意图**：定位 OCR 提供商失败后的筛选、优先级切换和全部失败处理

**期待来源**：排除失败策略和禁用厂商，按优先级逐一识别，有缓存字节时重新构造输入流，全部失败抛出异常。

- 可替代目标：[src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330>)；tryFallbackStrategy；关键实现行 [336, 337, 338, 355, 371]。排除失败策略和禁用厂商，按优先级逐一识别，有缓存字节时重新构造输入流，全部失败抛出异常。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 3 条结果；语义首次可回答名次 [第 3 名](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>)，对应 [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228-399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>)。
- 判断依据：Top1 提供商列表排序与保留兼容的旧回退函数不能证明当前完整识别流程，Top2 活动循环停在获取提供商名称处。Top3 包含排除失败/禁用厂商、按优先级尝试识别与全部失败异常的完整实现。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 3 条；[src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228-399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>)；核验行 336、337、338、355、371。排除失败策略和禁用厂商，按优先级逐一识别，有缓存字节时重新构造输入流，全部失败抛出异常。

**执行状态**：有效；真实工具 4.498 秒，批次含索引 86.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：3 / 3 / 3；单片段严格诊断名次：3。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:175-290](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:175>) | 未覆盖预定目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61-114](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228-399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>) | 单条覆盖声明及关键行 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/SystemController.java:228-284](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/SystemController.java:228>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrConfig.java:1-58](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrConfig.java:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1-84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:106-216](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:106>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:342-390](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:342>) | 未覆盖预定目标 |
| 10 | 文本块 | [bidreview-app-ui/src/components/SystemTest.vue:156-207](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/SystemTest.vue:156>) | 未覆盖预定目标 |



## 30. bid-review-app — 定位删除审查任务时的文件与存储清理流程

```text
search_code("删除一次标书审查记录时，上传材料和缓存记录是在哪里一起清掉的？")
```

**搜索语义**：删除一次标书审查记录时，上传材料和缓存记录是在哪里一起清掉的？

**预期意图**：定位删除审查任务时的文件与存储清理流程

**期待来源**：先查任务，再逆序删除任务上传目录，随后删除 Redis 和内存记录；有返回成功/失败分支。

- 可替代目标：[src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:363](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:363>)；deleteBidReviewTask；关键实现行 [371, 377, 385]。先查任务，再逆序删除任务上传目录，随后删除 Redis 和内存记录；有返回成功/失败分支。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>)，对应 [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340-395](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>)。
- 判断依据：Top1 同时展示任务上传目录的逆序删除及存储清理调用，足以定位两类清理的业务入口；Top4 补充具体内存与Redis删除。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340-395](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>)；核验行 371、377、385。先查任务，再逆序删除任务上传目录，随后删除 Redis 和内存记录；有返回成功/失败分支。

**执行状态**：有效；真实工具 7.593 秒，批次含索引 86.1 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662275708160000-64e5e537c7b8-idea-bid-review-app-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340-395](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:59-116](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:59>) | 仅文件命中 |
| 3 | bidReviewApi | [bidreview-app-ui/src/api/bid-review.ts:13-112](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:13>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868-925](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868>) | 仅文件命中 |
| 5 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:212-271](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:212>) | 未覆盖预定目标 |
| 6 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:55-111](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:55>) | 未覆盖预定目标 |
| 8 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:452-511](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:452>) | 未覆盖预定目标 |
| 9 | clearHistory | [bidreview-app-ui/src/components/TaskList.vue:490-539](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskList.vue:490>) | 未覆盖预定目标 |
| 10 | 文本块 | [bidreview-app-ui/src/views/Dashboard.vue:1-56](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/views/Dashboard.vue:1>) | 未覆盖预定目标 |




## 31. teriri-api-hub — 定位聊天上下文复用、SSE 握手及后台发起推理的多步骤流程

```text
search_code("新的聊天请求如何复用此前上下文，并立即向网页建立持续接收回答的连接？")
```

**搜索语义**：新的聊天请求如何复用此前上下文，并立即向网页建立持续接收回答的连接？

**预期意图**：定位聊天上下文复用、SSE 握手及后台发起推理的多步骤流程

**期待来源**：检查聊天与模型参数，创建或复用上下文，发送 connected 事件，后台调用上下文推理并返回 SSE。

- 可替代目标：[src/main/java/com/teriri/teririapihub/service/ChatService.java:72](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:72>)；chatWithLLMStream；关键实现行 [88, 91, 105, 122, 135]。检查聊天与模型参数，创建或复用上下文，发送 connected 事件，后台调用上下文推理并返回 SSE。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>)，对应 [src/main/java/com/teriri/teririapihub/service/ChatService.java:1-178](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>)。
- 判断依据：新的聊天入口同时返回 SSE 与复用上下文，Top1 覆盖参数验证、上下文分支、connected 事件和后台推理调用。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/teriri/teririapihub/service/ChatService.java:1-178](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>)；核验行 88、91、105、122、135。检查聊天与模型参数，创建或复用上下文，发送 connected 事件，后台调用上下文推理并返回 SSE。

**执行状态**：有效；真实工具 4.393 秒，批次含索引 32.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:1-178](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:60-228](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:60>) | 未覆盖预定目标 |
| 3 | sendSSE | [llm-tester/src/api/index.js:3-26](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/api/index.js:3>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1-16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) | 未覆盖预定目标 |
| 5 | send | [llm-tester/src/App.vue:55-153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1-157](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1>) | 未覆盖预定目标 |
| 7 | connectAsrWebSocket | [llm-tester/src/App.vue:412-490](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:412>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/ChatController.java:1-32](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/ChatController.java:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/teriri/teririapihub/event/llm/LLMCompletionEvent.java:1-22](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/event/llm/LLMCompletionEvent.java:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55-102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) | 未覆盖预定目标 |


## 32. teriri-api-hub — 定位流式文本事件到 SSE 和语音合成两条输出的连接

```text
search_code("模型每生成一段回答，哪里把文字推送给前端并按开关送去转成声音？")
```

**搜索语义**：模型每生成一段回答，哪里把文字推送给前端并按开关送去转成声音？

**预期意图**：定位流式文本事件到 SSE 和语音合成两条输出的连接

**期待来源**：从事件构建带首尾标记的文本消息发送给 SSE，开启语音时把非空内容追加到对应 TTS 会话。

- 可替代目标：[src/main/java/com/teriri/teririapihub/service/ChatService.java:156](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:156>)；onLLMTextChunk；关键实现行 [179, 181, 186]。从事件构建带首尾标记的文本消息发送给 SSE，开启语音时把非空内容追加到对应 TTS 会话。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>)，对应 [src/main/java/com/teriri/teririapihub/service/ChatService.java:61-199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>)。
- 判断依据：Top1 覆盖文本事件组装、SSE 发送、语音开关与非空文本追加，包含原问题全部关键步骤。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/teriri/teririapihub/service/ChatService.java:61-199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>)；核验行 179、181、186。从事件构建带首尾标记的文本消息发送给 SSE，开启语音时把非空内容追加到对应 TTS 会话。

**执行状态**：有效；真实工具 5.319 秒，批次含索引 32.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:61-199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>) | 单条覆盖声明及关键行 |
| 2 | send | [llm-tester/src/App.vue:55-153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1-7](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1>) | 未覆盖预定目标 |
| 4 | sendSSE | [llm-tester/src/api/index.js:3-26](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/api/index.js:3>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55-102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151-203](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1-17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1-108](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1-16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1-70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) | 未覆盖预定目标 |


## 33. teriri-api-hub — 定位语音会话复用、容量上限和并发创建保护

```text
search_code("很多人同时请求语音合成时，哪里限制连接总数并防止重复建立同一个会话？")
```

**搜索语义**：很多人同时请求语音合成时，哪里限制连接总数并防止重复建立同一个会话？

**预期意图**：定位语音会话复用、容量上限和并发创建保护

**期待来源**：先复用已存在会话，限制总量，通过创建锁和锁内复查避免重复建立，再登记新客户端。

- 可替代目标：[src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:50](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:50>)；acquireSession；关键实现行 [56, 63, 68, 74, 81, 83]。先复用已存在会话，限制总量，通过创建锁和锁内复查避免重复建立，再登记新客户端。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1>)，对应 [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1-119](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1>)。
- 判断依据：Top1 整个会话池包含容量检查、已有会话复用、加锁及锁内二次检查，不需要依靠只有说明的结果。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1-119](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1>)；核验行 56、63、68、74、81、83。先复用已存在会话，限制总量，通过创建锁和锁内复查避免重复建立，再登记新客户端。

**执行状态**：有效；真实工具 4.421 秒，批次含索引 32.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1-119](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:92-201](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:92>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:1-102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:1>) | 未覆盖预定目标 |
| 4 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1-70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:511-569](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:511>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1-159](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:204-247](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:204>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1-57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/dto/tts/TTSOptional.java:1-60](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/dto/tts/TTSOptional.java:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:219-275](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:219>) | 未覆盖预定目标 |


## 34. teriri-api-hub — 定位分块编码音频解码后写入语音识别输入管道

```text
search_code("前端分批传来的编码音频在哪里还原成字节并接到实时识别的数据流？")
```

**搜索语义**：前端分批传来的编码音频在哪里还原成字节并接到实时识别的数据流？

**预期意图**：定位分块编码音频解码后写入语音识别输入管道

**期待来源**：校验追加流已就绪，Base64 解码后写入并刷新管道；无效编码和 IO 错误分别处理。

- 可替代目标：[src/main/java/com/teriri/teririapihub/client/ASRClient.java:142](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:142>)；appendAudioBase64Chunk；关键实现行 [143, 151, 156, 157, 165]。校验追加流已就绪，Base64 解码后写入并刷新管道；无效编码和 IO 错误分别处理。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>)，对应 [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1-169](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>)。
- 判断依据：Top1 包含音频管道准备状态校验、Base64 还原、写入与刷新，以及异常处理；可直接定位实际接入数据流的代码。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/teriri/teririapihub/client/ASRClient.java:1-169](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>)；核验行 143、151、156、157、165。校验追加流已就绪，Base64 解码后写入并刷新管道；无效编码和 IO 错误分别处理。

**执行状态**：有效；真实工具 4.191 秒，批次含索引 32.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1-169](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) | 单条覆盖声明及关键行 |
| 2 | send | [llm-tester/src/App.vue:83-153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:83>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1-57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1>) | 未覆盖预定目标 |
| 4 | setupMicrophone | [llm-tester/src/App.vue:263-312](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:263>) | 未覆盖预定目标 |
| 5 | base64ToUint8Array | [llm-tester/src/App.vue:44-53](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:44>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:158-214](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:158>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:164-219](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:164>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1-125](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:384-425](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:384>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1-21](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1>) | 未覆盖预定目标 |


## 35. teriri-api-hub — 定位识别客户端关闭时的资源释放、回调和状态清理

```text
search_code("语音识别连接退出时，哪里关闭网络和音频资源并通知调用方？")
```

**搜索语义**：语音识别连接退出时，哪里关闭网络和音频资源并通知调用方？

**预期意图**：定位识别客户端关闭时的资源释放、回调和状态清理

**期待来源**：获取关闭锁，关闭 WebSocket 和音频流，触发关闭回调并清空连接及会话状态。

- 可替代目标：[src/main/java/com/teriri/teririapihub/client/ASRClient.java:498](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:498>)；close；关键实现行 [515, 519, 523, 526, 527]。获取关闭锁，关闭 WebSocket 和音频流，触发关闭回调并清空连接及会话状态。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:492>)，对应 [src/main/java/com/teriri/teririapihub/client/ASRClient.java:492-538](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:492>)。
- 判断依据：Top1 是关闭方法完整正文，依次关闭网络、关闭音频流、发出回调并重置状态。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/teriri/teririapihub/client/ASRClient.java:492-538](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:492>)；核验行 515、519、523、526、527。获取关闭锁，关闭 WebSocket 和音频流，触发关闭回调并清空连接及会话状态。

**执行状态**：有效；真实工具 4.334 秒，批次含索引 32.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:492-538](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:492>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1-111](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) | 仅文件命中 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1-125](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) | 未覆盖预定目标 |
| 4 | 文本块 | [llm-tester/src/App.vue:492-544](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:492>) | 未覆盖预定目标 |
| 5 | stopRecordingAndCleanUp | [llm-tester/src/App.vue:345-390](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:345>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1-21](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:109-235](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:109>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1-17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:570-616](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:570>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1-57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1>) | 未覆盖预定目标 |



## 36. teriri-api-hub — 定位语音合成发送锁超时后的队列回退

```text
search_code("朗读内容发送时抢不到锁，哪里会把这段文字放回队列等待下次处理？")
```

**搜索语义**：朗读内容发送时抢不到锁，哪里会把这段文字放回队列等待下次处理？

**预期意图**：定位语音合成发送锁超时后的队列回退

**期待来源**：等待发送锁超时时重新入队，有锁才发送，并通过 finally 释放锁；中断时恢复线程中断标记。

- 可替代目标：[src/main/java/com/teriri/teririapihub/client/TTSClient.java:315](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:315>)；processTextFromQueue；关键实现行 [318, 320, 324, 326]。等待发送锁超时时重新入队，有锁才发送，并通过 finally 释放锁；中断时恢复线程中断标记。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:248>)，对应 [src/main/java/com/teriri/teririapihub/client/TTSClient.java:248-353](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:248>)。
- 判断依据：Top1 包含消费函数完整正文：等待发送锁超时后重新入队，有锁才发送并最终解锁。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[src/main/java/com/teriri/teririapihub/client/TTSClient.java:248-353](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:248>)；核验行 318、320、324、326。等待发送锁超时时重新入队，有锁才发送，并通过 finally 释放锁；中断时恢复线程中断标记。

**执行状态**：有效；真实工具 6.008 秒，批次含索引 32.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662361805327000-7779a25c5907-idea-teriri-api-hub-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:248-353](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:248>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [llm-tester/src/App.vue:55-114](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) | 未覆盖预定目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:32-91](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:32>) | 仅文件命中 |
| 4 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:61-199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>) | 未覆盖预定目标 |
| 5 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:248-295](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:248>) | 未覆盖预定目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1-159](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151-203](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) | 未覆盖预定目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55-102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) | 未覆盖预定目标 |
| 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/dto/tts/TextRequest.java:1-17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/dto/tts/TextRequest.java:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1-70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) | 未覆盖预定目标 |



## 37. 1604-apps — 定位发货创建与库存流水联动及失败补偿

```text
search_code("司机登记一笔发货时，哪里同步减少冰粒库存，并在库存操作失败时撤销刚建的记录？")
```

**搜索语义**：司机登记一笔发货时，哪里同步减少冰粒库存，并在库存操作失败时撤销刚建的记录？

**预期意图**：定位发货创建与库存流水联动及失败补偿

**期待来源**：保存出库记录后创建扣库流水，失败删除刚插入记录，成功回写流水 ID 并记录操作日志。

- 可替代目标：[backend/src/services/delivery.service.ts:818](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>)；create；关键实现行 [837, 853, 865, 871, 873]。保存出库记录后创建扣库流水，失败删除刚插入记录，成功回写流水 ID 并记录操作日志。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 2 条结果；语义首次可回答名次 [第 2 名](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>)，对应 [backend/src/services/delivery.service.ts:818-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>)。
- 判断依据：Top1 是出库表结构，尚无库存联动行为；Top2 返回完整创建方法，可见扣库流水创建、失败删除新记录、成功绑定库存流水及日志。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 2 条；[backend/src/services/delivery.service.ts:818-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>)；核验行 837、853、865、871、873。保存出库记录后创建扣库流水，失败删除刚插入记录，成功回写流水 ID 并记录操作日志。

**执行状态**：有效；真实工具 4.024 秒，批次含索引 135.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：2 / 2 / 2；单片段严格诊断名次：2。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | stockOutRecords | [backend/src/db/schema/delivery.ts:21-58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:21>) | 未覆盖预定目标 |
| 2 | StockOutService.create | [backend/src/services/delivery.service.ts:818-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) | 单条覆盖声明及关键行 |
| 3 | returnRecords | [backend/src/db/schema/delivery.ts:63-100](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:63>) | 未覆盖预定目标 |
| 4 | iceInventoryRoutes | [backend/src/routes/iceInventory.ts:45-120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:45>) | 未覆盖预定目标 |
| 5 | IceInventorySummary | [frontend/src/types/iceInventory.ts:87-96](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/iceInventory.ts:87>) | 未覆盖预定目标 |
| 6 | IceInventoryService.create | [backend/src/services/iceInventory.service.ts:775-828](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:775>) | 未覆盖预定目标 |
| 7 | useIceInventory | [frontend/src/composables/useIceInventory.ts:34-141](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useIceInventory.ts:34>) | 未覆盖预定目标 |
| 8 | 文本块 | [frontend/src/views/IceInventory/components/IceInventoryForm.vue:161-216](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/components/IceInventoryForm.vue:161>) | 未覆盖预定目标 |
| 9 | StockOutService.delete | [backend/src/services/delivery.service.ts:987-1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:987>) | 仅文件命中 |
| 10 | 文本块 | [backend/drizzle/0007_delivery.sql:1-96](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0007_delivery.sql:1>) | 未覆盖预定目标 |




## 38. 1604-apps — 定位收款创建前的单据状态与累计待收余额验证

```text
search_code("给客户登记收款时，哪里检查送货单已经批准，而且累计金额不能超过订单金额？")
```

**搜索语义**：给客户登记收款时，哪里检查送货单已经批准，而且累计金额不能超过订单金额？

**预期意图**：定位收款创建前的单据状态与累计待收余额验证

**期待来源**：检查对应送货单批准状态，合并已收和待审金额限制超额收款，再保存待审核收款记录。

- 可替代目标：[backend/src/services/finance.service.ts:1931](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>)；createPayment；关键实现行 [1952, 1968, 1973, 1980]。检查对应送货单批准状态，合并已收和待审金额限制超额收款，再保存待审核收款记录。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>)，对应 [backend/src/services/finance.service.ts:1931-1993](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>)。
- 判断依据：Top1 完整包含送货单批准校验、已收与待审金额合并上限，以及待审核收款记录写入。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[backend/src/services/finance.service.ts:1931-1993](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>)；核验行 1952、1968、1973、1980。检查对应送货单批准状态，合并已收和待审金额限制超额收款，再保存待审核收款记录。

**执行状态**：有效；真实工具 4.193 秒，批次含索引 135.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | FinanceService.createPayment | [backend/src/services/finance.service.ts:1931-1993](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [backend/drizzle/0009_finance.sql:100-189](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0009_finance.sql:100>) | 未覆盖预定目标 |
| 3 | FinanceService.auditPayment | [backend/src/services/finance.service.ts:2119-2178](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:2119>) | 仅文件命中 |
| 4 | deliveryBills | [backend/src/db/schema/finance.ts:20-53](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:20>) | 未覆盖预定目标 |
| 5 | customerOrderPayments | [backend/src/db/schema/finance.ts:92-124](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:92>) | 未覆盖预定目标 |
| 6 | DeliveryBillReviewInput | [backend/src/services/finance.service.ts:281-294](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:281>) | 仅文件命中 |
| 7 | reviewBill | [frontend/src/views/DeliveryBill/index.vue:230-249](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:230>) | 未覆盖预定目标 |
| 8 | useDeliveryBill.review | [frontend/src/composables/useFinance.ts:141-145](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useFinance.ts:141>) | 未覆盖预定目标 |
| 9 | CustomerDebtRow.receivedAmount | [backend/src/services/report.service.ts:142-142](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:142>) | 未覆盖预定目标 |
| 10 | CustomerOrderRecord | [frontend/src/types/finance.ts:269-354](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/finance.ts:269>) | 未覆盖预定目标 |




## 39. 1604-apps — 定位送货单驳回必填原因以及审核记录持久化

```text
search_code("把送货单退回重新处理时，哪里要求填写理由并记下审核人和时间？")
```

**搜索语义**：把送货单退回重新处理时，哪里要求填写理由并记下审核人和时间？

**预期意图**：定位送货单驳回必填原因以及审核记录持久化

**期待来源**：校验单据归属和审核人有效性，驳回时必须有原因，更新审核状态、人、时间并记录日志。

- 可替代目标：[backend/src/services/finance.service.ts:1579](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>)；reviewDeliveryBill；关键实现行 [1605, 1612, 1613, 1614, 1615, 1619]。校验单据归属和审核人有效性，驳回时必须有原因，更新审核状态、人、时间并记录日志。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>)，对应 [backend/src/services/finance.service.ts:1579-1630](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>)。
- 判断依据：Top1 包含驳回原因必填校验和审核人、时间、原因的持久化，另有操作日志；所需行为全部在返回正文。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[backend/src/services/finance.service.ts:1579-1630](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>)；核验行 1605、1612、1613、1614、1615、1619。校验单据归属和审核人有效性，驳回时必须有原因，更新审核状态、人、时间并记录日志。

**执行状态**：有效；真实工具 3.906 秒，批次含索引 135.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | FinanceService.reviewDeliveryBill | [backend/src/services/finance.service.ts:1579-1630](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>) | 单条覆盖声明及关键行 |
| 2 | FinanceService.auditPayment | [backend/src/services/finance.service.ts:2119-2178](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:2119>) | 仅文件命中 |
| 3 | reviewBill | [frontend/src/views/DeliveryBill/index.vue:230-249](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:230>) | 未覆盖预定目标 |
| 4 | DeliveryBillRecord | [frontend/src/types/finance.ts:43-144](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/finance.ts:43>) | 未覆盖预定目标 |
| 5 | DeliveryBillReviewInput | [backend/src/services/finance.service.ts:281-294](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:281>) | 仅文件命中 |
| 6 | useDeliveryBill.review | [frontend/src/composables/useFinance.ts:141-145](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useFinance.ts:141>) | 未覆盖预定目标 |
| 7 | ReimbursementService.review | [backend/src/services/reimbursement.service.ts:532-581](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:532>) | 未覆盖预定目标 |
| 8 | 文本块 | [backend/drizzle/0009_finance.sql:100-189](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0009_finance.sql:100>) | 未覆盖预定目标 |
| 9 | customerOrderPayments | [backend/src/db/schema/finance.ts:92-124](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:92>) | 未覆盖预定目标 |
| 10 | DeliveryBillFormProps | [frontend/src/views/DeliveryBill/components/DeliveryBillForm.vue:48-77](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/components/DeliveryBillForm.vue:48>) | 未覆盖预定目标 |




## 40. 1604-apps — 定位报销确认付款的状态限制、完成状态及日志

```text
search_code("员工报销的钱付完后，哪里把单据标成已付款或已办结，并保留操作记录？")
```

**搜索语义**：员工报销的钱付完后，哪里把单据标成已付款或已办结，并保留操作记录？

**预期意图**：定位报销确认付款的状态限制、完成状态及日志

**期待来源**：只允许批准或已支付单确认，校验操作者后按 completed 标记付款/完成并写入时间和日志。

- 可替代目标：[backend/src/services/reimbursement.service.ts:591](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>)；pay；关键实现行 [604, 620, 621, 625, 627]。只允许批准或已支付单确认，校验操作者后按 completed 标记付款/完成并写入时间和日志。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>)，对应 [backend/src/services/reimbursement.service.ts:591-635](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>)。
- 判断依据：Top1 返回报销付款完整方法，显示可操作状态限制、付款/办结分支、时间写入与日志记录。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[backend/src/services/reimbursement.service.ts:591-635](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>)；核验行 604、620、621、625、627。只允许批准或已支付单确认，校验操作者后按 completed 标记付款/完成并写入时间和日志。

**执行状态**：有效；真实工具 3.429 秒，批次含索引 135.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | ReimbursementService.pay | [backend/src/services/reimbursement.service.ts:591-635](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>) | 单条覆盖声明及关键行 |
| 2 | payRecord | [frontend/src/views/Reimbursement/index.vue:213-284](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Reimbursement/index.vue:213>) | 未覆盖预定目标 |
| 3 | reimbursementOperationLogs | [backend/src/db/schema/reimbursement.ts:50-67](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/reimbursement.ts:50>) | 未覆盖预定目标 |
| 4 | ReimbursementPayInput | [backend/src/services/reimbursement.service.ts:105-114](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:105>) | 仅文件命中 |
| 5 | reimbursements | [backend/src/db/schema/reimbursement.ts:18-45](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/reimbursement.ts:18>) | 未覆盖预定目标 |
| 6 | ReimbursementService.review | [backend/src/services/reimbursement.service.ts:532-581](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:532>) | 仅文件命中 |
| 7 | 文本块 | [backend/drizzle/0010_reimbursement.sql:1-92](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0010_reimbursement.sql:1>) | 未覆盖预定目标 |
| 8 | ReimbursementRecord | [frontend/src/types/reimbursement.ts:16-77](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/reimbursement.ts:16>) | 未覆盖预定目标 |
| 9 | 文本块 | [frontend/src/views/Reimbursement/components/ReimbursementDeleteDialog.vue:53-86](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Reimbursement/components/ReimbursementDeleteDialog.vue:53>) | 未覆盖预定目标 |
| 10 | ReimbursementStatus | [frontend/src/types/reimbursement.ts:6-6](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/reimbursement.ts:6>) | 未覆盖预定目标 |




## 41. 1604-apps — 定位本人修改密码的旧密码验证与安全写入

```text
search_code("用户自己换登录密码时，哪里验证旧密码并把新密码加密保存？")
```

**搜索语义**：用户自己换登录密码时，哪里验证旧密码并把新密码加密保存？

**预期意图**：定位本人修改密码的旧密码验证与安全写入

**期待来源**：检查账号存在和启用，验证旧密码，拒绝新旧相同，再保存新密码哈希。

- 可替代目标：[backend/src/services/user.service.ts:750](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>)；changePassword；关键实现行 [761, 767, 774, 777]。检查账号存在和启用，验证旧密码，拒绝新旧相同，再保存新密码哈希。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>)，对应 [backend/src/services/user.service.ts:750-780](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>)。
- 判断依据：Top1 返回本人修改密码完整方法，实际执行旧密码验证、新旧相同校验和新密码哈希写入，区别于管理员重置入口。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[backend/src/services/user.service.ts:750-780](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>)；核验行 761、767、774、777。检查账号存在和启用，验证旧密码，拒绝新旧相同，再保存新密码哈希。

**执行状态**：有效；真实工具 3.126 秒，批次含索引 135.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | UserService.changePassword | [backend/src/services/user.service.ts:750-780](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>) | 单条覆盖声明及关键行 |
| 2 | ChangePasswordPayload | [frontend/src/types/user.ts:178-187](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/user.ts:178>) | 未覆盖预定目标 |
| 3 | UserService.resetPassword | [backend/src/services/user.service.ts:726-741](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:726>) | 仅文件命中 |
| 4 | ChangePasswordRequest | [backend/src/routes/auth.ts:39-48](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/auth.ts:39>) | 未覆盖预定目标 |
| 5 | useAuthStore.changePassword | [frontend/src/stores/auth.ts:204-207](</Users/teriri/IdeaProjects/1604-apps/frontend/src/stores/auth.ts:204>) | 未覆盖预定目标 |
| 6 | UserService.verifyLogin | [backend/src/services/user.service.ts:619-640](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:619>) | 仅文件命中 |
| 7 | useUser.resetPassword | [frontend/src/composables/useUser.ts:91-95](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useUser.ts:91>) | 未覆盖预定目标 |
| 8 | 文本块 | [docs/tasks/admin-account-entry.md:1-26](</Users/teriri/IdeaProjects/1604-apps/docs/tasks/admin-account-entry.md:1>) | 未覆盖预定目标 |
| 9 | PasswordFormValues | [frontend/src/views/Account/index.vue:22-27](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Account/index.vue:22>) | 未覆盖预定目标 |
| 10 | submitPassword | [frontend/src/views/Account/index.vue:90-97](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Account/index.vue:90>) | 未覆盖预定目标 |




## 42. 1604-apps — 定位客户基础资料、区域重名验证以及负责人关系事务替换

```text
search_code("编辑客户资料时，哪里会一起替换负责人的分配关系，并防止同一区域出现重名客户？")
```

**搜索语义**：编辑客户资料时，哪里会一起替换负责人的分配关系，并防止同一区域出现重名客户？

**预期意图**：定位客户基础资料、区域重名验证以及负责人关系事务替换

**期待来源**：验证负责人存在及同区域重名，将资料更新与旧关联删除、新关联创建放在同一事务中。

- 可替代目标：[backend/src/services/customer.service.ts:466](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>)；update；关键实现行 [481, 487, 508, 511, 519]。验证负责人存在及同区域重名，将资料更新与旧关联删除、新关联创建放在同一事务中。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>)，对应 [backend/src/services/customer.service.ts:466-533](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>)。
- 判断依据：Top1 返回客户修改完整事务，先检查区域重名，再更新资料、删除旧负责人关联并创建新关联；不是新增客户或只有类型说明。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[backend/src/services/customer.service.ts:466-533](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>)；核验行 481、487、508、511、519。验证负责人存在及同区域重名，将资料更新与旧关联删除、新关联创建放在同一事务中。

**执行状态**：有效；真实工具 3.509 秒，批次含索引 135.0 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662393863840000-fc70e38adab0-idea-1604-apps-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | CustomerService.update | [backend/src/services/customer.service.ts:466-533](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>) | 单条覆盖声明及关键行 |
| 2 | CustomerService.create | [backend/src/services/customer.service.ts:402-457](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:402>) | 仅文件命中 |
| 3 | CustomerRecord | [backend/src/services/customer.service.ts:86-95](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:86>) | 仅文件命中 |
| 4 | 文本块 | [frontend/src/views/Customer/components/CustomerForm.vue:316-366](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/components/CustomerForm.vue:316>) | 未覆盖预定目标 |
| 5 | 文本块 | [frontend/src/views/Customer/components/CustomerForm.vue:33-143](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/components/CustomerForm.vue:33>) | 未覆盖预定目标 |
| 6 | customers | [backend/src/db/schema/customers.ts:22-55](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/customers.ts:22>) | 未覆盖预定目标 |
| 7 | 文本块 | [frontend/src/views/CashOrder/components/CustomerOrderForm.vue:113-168](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/CashOrder/components/CustomerOrderForm.vue:113>) | 未覆盖预定目标 |
| 8 | 文本块 | [frontend/src/views/Customer/index.vue:327-386](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/index.vue:327>) | 未覆盖预定目标 |
| 9 | Customer.responsibles | [frontend/src/types/customer.ts:60-60](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/customer.ts:60>) | 未覆盖预定目标 |
| 10 | CustomerPayload.responsibleUserIds | [frontend/src/types/customer.ts:115-115](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/customer.ts:115>) | 未覆盖预定目标 |




## 43. 1086-backend — 定位新建贷款工单的角色校验、多表关联保存及事务回滚

```text
search_code("申请一笔汽车贷款时，哪里把借款资料、身份证正反面和附件作为同一笔工单保存？")
```

**搜索语义**：申请一笔汽车贷款时，哪里把借款资料、身份证正反面和附件作为同一笔工单保存？

**预期意图**：定位新建贷款工单的角色校验、多表关联保存及事务回滚

**期待来源**：限制业务员和审查角色，组装贷款资料并在事务中保存主工单、身份证图片与材料文件，异常回滚。

- 可替代目标：[application/api/controller/Index.php:1074](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1074>)；postOrder；关键实现行 [1082, 1160, 1162, 1168, 1174, 1176, 1178]。限制业务员和审查角色，组装贷款资料并在事务中保存主工单、身份证图片与材料文件，异常回滚。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 5 条结果；语义首次可回答名次 [第 5 名](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>)，对应 [application/api/controller/Index.php:1134-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>)。
- 判断依据：Top5 返回身份证双面和文件数组的构造，并在同一事务内保存主工单、绑定子表工单ID及批量保存图片/附件，足以回答原问题的同单保存位置。缺失函数入口和前半段借款字段组装，严格冻结声明/正文指标仍为 null。Top2 相似片段的身份证与附件保存全部被注释，不能当作实现。
- 冻结目标说明：只召回 postOrder 后半段1134-1188；缺声明1074、角色检查1082及借款资料组装1087-1121；自动decl/body为null
- 实际返回结果：第 5 条；[application/api/controller/Index.php:1134-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>)；核验行 1143、1144、1155、1160、1162、1166、1168、1172、1174、1176、1178。主表保存后将同一个工单ID写入身份证与文件子表，三个保存步骤处于同一事务；包含提交和回滚。

**执行状态**：有效；真实工具 4.963 秒，批次含索引 273.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：2 / None / None；单片段严格诊断名次：None。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [update.sql:55-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) | 未覆盖预定目标 |
| 2 | 文本块 | [application/api/controller/Index.php:647-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅文件命中 |
| 3 | 文本块 | [application/api/controller/Index.php:388-439](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) | 仅文件命中 |
| 4 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [application/api/controller/Index.php:1134-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) | 仅文件命中 |
| 6 | 文本块 | [application/admin/controller/Workorder.php:1-156](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [application/admin/controller/Workorderimage.php:1-103](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [application/admin/view/workorder/upload.html:1-22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [application/common/model/Workorder.php:1-43](</Users/teriri/IdeaProjects/1086-backend/application/common/model/Workorder.php:1>) | 未覆盖预定目标 |





## 44. 1086-backend — 定位待审工单认领及负责人绑定

```text
search_code("审核人员领取尚未分派的申请后，哪里把处理人改成自己并推进办理状态？")
```

**搜索语义**：审核人员领取尚未分派的申请后，哪里把处理人改成自己并推进办理状态？

**预期意图**：定位待审工单认领及负责人绑定

**期待来源**：查找待审且尚无审核人的工单，更新为处理中并绑定当前认证用户后保存。

- 可替代目标：[application/api/controller/Index.php:744](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:744>)；takeOrder；关键实现行 [751, 755, 756, 757]。查找待审且尚无审核人的工单，更新为处理中并绑定当前认证用户后保存。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>)，对应 [application/api/controller/Index.php:704-820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>)。
- 判断依据：Top1 完整包含认领方法，查询要求尚未分派的待审记录，再把状态推进并把审核人绑定到当前用户。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[application/api/controller/Index.php:704-820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>)；核验行 751、755、756、757。查找待审且尚无审核人的工单，更新为处理中并绑定当前认证用户后保存。

**执行状态**：有效；真实工具 4.871 秒，批次含索引 273.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:704-820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [application/admin/controller/Workorder.php:247-308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) | 未覆盖预定目标 |
| 3 | 文本块 | [application/api/controller/Index.php:587-646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:587>) | 仅文件命中 |
| 4 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [update.sql:115-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) | 未覆盖预定目标 |
| 6 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [application/admin/view/workorderfile/index.html:1-46](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/index.html:1>) | 未覆盖预定目标 |
| 9 | 文本块 | [application/admin/controller/Workorder.php:59-115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:59>) | 未覆盖预定目标 |
| 10 | 文本块 | [application/admin/view/workorderimage/add.html:1-58](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/add.html:1>) | 未覆盖预定目标 |



## 45. 1086-backend — 定位工单分页查询中的公司和角色数据范围过滤

```text
search_code("不同公司、业务员和抵押经办人查看申请列表时，哪里决定各自能看到哪些工单？")
```

**搜索语义**：不同公司、业务员和抵押经办人查看申请列表时，哪里决定各自能看到哪些工单？

**预期意图**：定位工单分页查询中的公司和角色数据范围过滤

**期待来源**：按公司限制查询，对业务员和抵押员绑定本人 ID，对审核员加入待审核条件，并分页查询。

- 可替代目标：[application/api/controller/Index.php:774](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:774>)；workorder；关键实现行 [784, 791, 792, 801, 802, 812]。按公司限制查询，对业务员和抵押员绑定本人 ID，对审核员加入待审核条件，并分页查询。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>)，对应 [application/api/controller/Index.php:764-820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>)。
- 判断依据：Top1 已包含公司、业务员和抵押员对应的过滤条件以及分页执行，足以回答访问范围。正文结尾停在返回前，但所问查询限制没有丢失。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[application/api/controller/Index.php:764-820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>)；核验行 784、791、792、801、802、812。按公司限制查询，对业务员和抵押员绑定本人 ID，对审核员加入待审核条件，并分页查询。

**执行状态**：有效；真实工具 5.546 秒，批次含索引 273.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:764-820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [update.sql:1-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:1>) | 未覆盖预定目标 |
| 3 | 文本块 | [application/admin/controller/Workorder.php:1-115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:1>) | 未覆盖预定目标 |
| 4 | 文本块 | [application/api/controller/Index.php:440-481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) | 仅文件命中 |
| 5 | 文本块 | [application/admin/controller/Workorder.php:247-306](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) | 未覆盖预定目标 |
| 6 | 文本块 | [application/api/controller/Index.php:529-586](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:529>) | 仅文件命中 |
| 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [application/admin/view/workorder/edit.html:211-260](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/edit.html:211>) | 未覆盖预定目标 |
| 9 | 文本块 | [application/admin/controller/Workorderimage.php:1-103](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [application/common/model/Workorder.php:1-43](</Users/teriri/IdeaProjects/1086-backend/application/common/model/Workorder.php:1>) | 未覆盖预定目标 |




## 46. 1086-backend — 定位工单详情中材料项完成标记的来源

```text
search_code("查看一份贷款申请时，哪里判断每项要求的资料是否已经上传过文件或图片？")
```

**搜索语义**：查看一份贷款申请时，哪里判断每项要求的资料是否已经上传过文件或图片？

**预期意图**：定位工单详情中材料项完成标记的来源

**期待来源**：读取贷款模式的材料项，逐项检查文件和图片记录，设置 is_check 后随详情返回。

- 可替代目标：[application/api/controller/Index.php:833](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:833>)；workorderDetail；关键实现行 [851, 859, 865, 872, 877]。读取贷款模式的材料项，逐项检查文件和图片记录，设置 is_check 后随详情返回。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:821>)，对应 [application/api/controller/Index.php:821-880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:821>)。
- 判断依据：Top1 包含贷款材料项循环，分别检查文件与图片记录并设置上传完成标记；所需判断完整，缺末尾通用响应不影响回答。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[application/api/controller/Index.php:821-880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:821>)；核验行 851、859、865、872、877。读取贷款模式的材料项，逐项检查文件和图片记录，设置 is_check 后随详情返回。

**执行状态**：有效；真实工具 5.388 秒，批次含索引 273.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:821-880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:821>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [application/admin/view/workorder/upload.html:1-22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) | 未覆盖预定目标 |
| 3 | 文本块 | [application/admin/controller/Workorder.php:59-115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:59>) | 未覆盖预定目标 |
| 4 | 文本块 | [application/admin/view/loantype/index.html:1-29](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/loantype/index.html:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [application/api/validate/IndexValidate.php:1-63](</Users/teriri/IdeaProjects/1086-backend/application/api/validate/IndexValidate.php:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [application/api/controller/Index.php:647-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅文件命中 |
| 7 | 文本块 | [application/api/controller/Index.php:482-528](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:482>) | 仅文件命中 |
| 8 | 文本块 | [application/admin/view/workorderfile/edit.html:1-53](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/edit.html:1>) | 未覆盖预定目标 |
| 9 | Upload.events.onUploadComplete | [public/assets/js/require-upload.js:94-117](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/require-upload.js:94>) | 未覆盖预定目标 |
| 10 | 文本块 | [application/admin/view/workorder/add.html:205-251](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/add.html:205>) | 未覆盖预定目标 |




## 47. 1086-backend — 定位工单审核提交的状态及归属限制

```text
search_code("审核人员提交处理结论时，哪里确认这张单正在由本人办理，再保存新的审核状态？")
```

**搜索语义**：审核人员提交处理结论时，哪里确认这张单正在由本人办理，再保存新的审核状态？

**预期意图**：定位工单审核提交的状态及归属限制

**期待来源**：仅选择处于办理状态且分派给当前审核员的指定工单，将提交状态和审核人保存。

- 可替代目标：[application/api/controller/Index.php:712](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:712>)；checkOrder；关键实现行 [719, 723, 724, 725]。仅选择处于办理状态且分派给当前审核员的指定工单，将提交状态和审核人保存。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 2 条结果；语义首次可回答名次 [第 2 名](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>)，对应 [application/api/controller/Index.php:704-763](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>)。
- 判断依据：Top1 只有审核员保存入口的参数和资料组装开头，未展示最终保存；Top2 完整包含本人办理中工单的筛选及审核状态保存。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 2 条；[application/api/controller/Index.php:704-763](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>)；核验行 719、723、724、725。仅选择处于办理状态且分派给当前审核员的指定工单，将提交状态和审核人保存。

**执行状态**：有效；真实工具 5.724 秒，批次含索引 273.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 2 / 2；单片段严格诊断名次：2。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:587-646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:587>) | 仅文件命中 |
| 2 | 文本块 | [application/api/controller/Index.php:704-763](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) | 单条覆盖声明及关键行 |
| 3 | 文本块 | [update.sql:115-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) | 未覆盖预定目标 |
| 4 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) | 未覆盖预定目标 |
| 5 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [application/api/controller/Index.php:440-481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) | 仅文件命中 |
| 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [application/admin/controller/Workorder.php:247-308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) | 未覆盖预定目标 |
| 9 | 文本块 | [application/admin/model/Workorderfile.php:1-57](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorderfile.php:1>) | 未覆盖预定目标 |
| 10 | 文本块 | [application/admin/model/Workorder.php:1-57](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorder.php:1>) | 未覆盖预定目标 |




## 48. 1086-backend — 定位工单附件打包、HTTP 下载及临时资源清理流程

```text
search_code("后台怎样把一张申请的全部附件和身份证照片打成压缩包，下载后清理临时文件？")
```

**搜索语义**：后台怎样把一张申请的全部附件和身份证照片打成压缩包，下载后清理临时文件？

**预期意图**：定位工单附件打包、HTTP 下载及临时资源清理流程

**期待来源**：收集文件和证件两面图片，下载到临时目录、加入 ZIP、输出下载头和文件，再清理压缩包及临时目录。

- 可替代目标：[application/admin/controller/Workorder.php:116](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>)；download；关键实现行 [127, 133, 155, 176, 188, 190, 194]。收集文件和证件两面图片，下载到临时目录、加入 ZIP、输出下载头和文件，再清理压缩包及临时目录。

**人工语义核验**：可由返回结果回答

- 可回答范围：前 1 条结果；语义首次可回答名次 [第 1 名](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>)，对应 [application/admin/controller/Workorder.php:116-197](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>)。
- 判断依据：Top1 完整包含文件与身份证双面照片收集、下载临时文件、ZIP加入、输出下载和临时清理，满足全部步骤。
- 冻结目标说明：完整覆盖冻结目标声明和关键正文
- 实际返回结果：第 1 条；[application/admin/controller/Workorder.php:116-197](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>)；核验行 127、133、155、176、188、190、194。收集文件和证件两面图片，下载到临时目录、加入 ZIP、输出下载头和文件，再清理压缩包及临时目录。

**执行状态**：有效；真实工具 5.465 秒，批次含索引 273.2 秒。冻结目标的文件/声明/关键行首次覆盖名次：1 / 1 / 1；单片段严格诊断名次：1。

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/real-search-2026-09-06/logs/1788662529014692000-ac3bde1fdbe6-idea-1086-backend-01-attempt1.stderr>)

| 名次 | 返回符号 | 路径与行范围 | 判定 |
|---:|---|---|---|
| 1 | 文本块 | [application/admin/controller/Workorder.php:116-197](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>) | 单条覆盖声明及关键行 |
| 2 | 文本块 | [application/admin/controller/general/Attachment.php:1-160](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/general/Attachment.php:1>) | 未覆盖预定目标 |
| 3 | 文本块 | [application/api/controller/Index.php:1134-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) | 未覆盖预定目标 |
| 4 | 文本块 | [application/api/controller/Index.php:647-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 未覆盖预定目标 |
| 5 | 文本块 | [application/admin/controller/Workorderfile.php:1-54](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderfile.php:1>) | 未覆盖预定目标 |
| 6 | 文本块 | [application/admin/controller/Workorderimage.php:1-43](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖预定目标 |
| 7 | 文本块 | [application/admin/controller/Frontpackage.php:1-124](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Frontpackage.php:1>) | 未覆盖预定目标 |
| 8 | 文本块 | [application/api/controller/Index.php:1469-1528](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1469>) | 未覆盖预定目标 |
| 9 | 文本块 | [application/common/library/Upload.php:213-313](</Users/teriri/IdeaProjects/1086-backend/application/common/library/Upload.php:213>) | 未覆盖预定目标 |
| 10 | 文本块 | [application/admin/lang/zh-cn/general/attachment.php:1-41](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/general/attachment.php:1>) | 未覆盖预定目标 |
