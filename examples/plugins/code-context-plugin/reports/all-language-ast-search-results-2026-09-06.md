# 全语言 AST 第二轮：搜索语义与实际结果

两轮均未通过准确率门槛，实验代码已回退。本表保留72条搜索语义和基线/第二轮全部Top-10路径、符号与原文行号；完整返回源码及逐段审计见 [原始正文报告](../target/all-language-ast-implementation-2026-09-06/analysis-v2-final/report.md)。

# 多语言 AST 检索 A/B 统计

这是同一冻结语义查询集的真实 API 比较；未混合新业务与同义变体计算整体准确率。

比较用途：事前登记第二轮探索性修复；参照臂 baseline，不替换第一轮 primary；正式实验臂依据事前 evaluation-plan.json，不按结果择优。

有效配对：72/72；冻结 SHA：`768079d1be2e43d9c50a8f51dc4bc72407d729b8b57e431e0b73d64af4a72b36`。

主分数是同路径、源码锚点与关键正文逐行相等后的答案组联合覆盖。范围覆盖是额外诊断，不能替代人工判断完整业务逻辑。

主显著性采用精确双侧配对符号检验，去掉平局。48 条新业务按查询配对；同义问法主检验按 12 个业务族，Recall 要求同族两种问法都成功，MRR 先族内平均。

同时报告原始 p 与两次尝试的 Bonferroni 校正 min(1, 2p)。第二轮是在查看第一轮失败后进行的探索性修复；校正不能把同一数据集上的迭代变成独立泛化证明，多个次要指标之间也未再作额外校正。

## 48 条新业务

| 指标 | 基线 | 实验 | 变化 | 改善/变差/平局 | 双侧 p | 两次校正 p |
|---|---:|---:|---:|---:|---:|---:|
| Recall@1 | 58.33% | 54.17% | -4.17 个百分点 | 1/3/44 | 0.625000 | 1.000000 |
| Recall@3 | 81.25% | 75.00% | -6.25 个百分点 | 1/4/43 | 0.375000 | 0.750000 |
| Recall@10 | 89.58% | 89.58% | +0.00 个百分点 | 2/2/44 | 1.000000 | 1.000000 |
| mrr | 0.6974 | 0.6655 | -0.0320 | 5/8/35 | 0.581055 | 1.000000 |

查询阶段中位/P95：基线 5.931 秒/10.469 秒；实验 4.305 秒/6.714 秒。索引与进程初始化不计入 query_ms。

## 24 条同义问法（描述统计）

| 指标 | 基线 | 实验 | 变化 | 改善/变差/平局 | 双侧 p | 两次校正 p |
|---|---:|---:|---:|---:|---:|---:|
| Recall@1 | 25.00% | 20.83% | -4.17 个百分点 | 2/3/19 | 不作 24 独立样本检验 | — |
| Recall@3 | 79.17% | 70.83% | -8.33 个百分点 | 2/4/18 | 不作 24 独立样本检验 | — |
| Recall@10 | 87.50% | 83.33% | -4.17 个百分点 | 1/2/21 | 不作 24 独立样本检验 | — |
| mrr | 0.5069 | 0.4424 | -0.0646 | 4/7/13 | 不作 24 独立样本检验 | — |

查询阶段中位/P95：基线 6.093 秒/10.320 秒；实验 5.188 秒/11.505 秒。索引与进程初始化不计入 query_ms。

## 12 个同义业务族（主检验）

| 指标 | 基线 | 实验 | 变化 | 改善/变差/平局 | 双侧 p | 两次校正 p |
|---|---:|---:|---:|---:|---:|---:|
| Recall@1 | 16.67% | 8.33% | -8.33 个百分点 | 0/1/11 | 1.000000 | 1.000000 |
| Recall@3 | 58.33% | 58.33% | +0.00 个百分点 | 2/2/8 | 1.000000 | 1.000000 |
| Recall@10 | 75.00% | 75.00% | +0.00 个百分点 | 1/1/10 | 1.000000 | 1.000000 |
| mrr | 0.5069 | 0.4424 | -0.0646 | 3/4/5 | 1.000000 | 1.000000 |

## 人工审计状态

覆盖或 Top-1 来源变化 37 条；已填写审计 50 条；待审计 0 条。

自动字段变化不能直接证明语义改善；人工审计需核对所问动作、冻结答案合理性、真实返回正文及替代实现。未完成审计前不宣称准确率已得到证明。

未成对查询：无。

模式：冻结语义集真实 A/B。完整正文见 [第二轮完整报告](../target/all-language-ast-implementation-2026-09-06/analysis-v2-final/report.md)，原始证据仍保留在两侧 results.jsonl 和 logs。

## 执行与环境记录

基线：成功查询 72，成功批次 15，成功查询中发生重试 0；首批有效执行前失败批次 36，其中日志确认 DNS 不可用 36；其他失败批次 0。失败未计入命中率分母。

实验：成功查询 72，成功批次 15，成功查询中发生重试 0；首批有效执行前失败批次 0，其中日志确认 DNS 不可用 0；其他失败批次 0。失败未计入命中率分母。

## 逐查询搜索语义与真实结果

以下 `search_code(查询)` 是用户指定的报告记法；实际插件工具名保持 `code_search`，原始日志可核对。

## holdout-web-1623-01

`search_code("修改学生和监护人的联系方式后，在哪里保存资料并进入实习信息填写？")`

搜索语义：基本资料保存与下一步导航

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/basic/index.vue:103](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:103>)；符号 handleNext；锚点 [103]；关键行 [104, 106, 108, 109, 112, 116]。

人工审计：保存资料的handleNext103–117仍包含本人/监护人联系方式、失败返回码分支及实习资料跳转。第二轮恢复103–143整段文本，核心实现未变，邻接样式重新进入片段，只改变边界与符号标签。

### 基线

查询耗时 3.778 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/basic/index.vue:103-143](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:103>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/internship/students.vue:1-108](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/pages/basic/internship.vue:55-115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:55>) | 未覆盖冻结目标 |
| 4 | savePracticeInfo | [src/api/practice.js:49-55](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:49>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/basic/internship.vue:1-53](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/internship/index.vue:1-55](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/internship/form.vue:1-128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) | 未覆盖冻结目标 |
| 8 | handleSubmit | [src/pages/internship/form.vue:156-171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/internship/detail.vue:1-173](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) | 未覆盖冻结目标 |
| 10 | savePracticeRecord | [src/api/practice.js:80-86](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:80>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

### 实验

查询耗时 4.617 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/basic/index.vue:103-143](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:103>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/internship/students.vue:1-56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/pages/basic/internship.vue:1-130](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) | 未覆盖冻结目标 |
| 4 | savePracticeInfo | [src/api/practice.js:49-55](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:49>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/internship/index.vue:1-55](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/internship/form.vue:1-128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) | 未覆盖冻结目标 |
| 7 | handleSubmit | [src/pages/internship/form.vue:156-171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/internship/detail.vue:1-173](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) | 未覆盖冻结目标 |
| 9 | savePracticeRecord | [src/api/practice.js:80-86](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:80>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/index/index.vue:90-108](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:90>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

## holdout-web-1623-02

`search_code("停止浏览器录音后怎样释放麦克风，并通知识别服务已经结束？")`

搜索语义：录音资源释放与结束帧发送

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/internship/form.vue:564](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:564>)；符号 stopH5Recorder；锚点 [564]；关键行 [566, 570, 579, 583, 587, 590, 591, 592]。

### 基线

查询耗时 4.122 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | cleanupVoiceInput | [src/pages/internship/form.vue:485-493](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:485>) | 仅目标文件 |
| 2 | stopVoiceInput | [src/pages/internship/form.vue:386-396](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:386>) | 仅目标文件 |
| 3 | stopH5Recorder | [src/pages/internship/form.vue:520-594](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:520>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [src/pages/internship/form.vue:173-345](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:173>) | 仅目标文件 |
| 5 | usePermissions.judgeIosPermissionRecord | [src/hooks/useAppPermission.js:60-70](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useAppPermission.js:60>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/App.vue:9-21](</Users/teriri/WebstormProjects/1623-mobile/src/App.vue:9>) | 未覆盖冻结目标 |
| 7 | useWechatPermissions | [src/hooks/useWechatPermissions.js:22-131](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatPermissions.js:22>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/hooks/useAppPermission.js:21-59](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useAppPermission.js:21>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/hooks/useAppPermission.js:71-279](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useAppPermission.js:71>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/manifest.json:1-91](</Users/teriri/WebstormProjects/1623-mobile/src/manifest.json:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

### 实验

查询耗时 4.145 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | cleanupVoiceInput | [src/pages/internship/form.vue:485-493](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:485>) | 仅目标文件 |
| 2 | stopVoiceInput | [src/pages/internship/form.vue:386-396](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:386>) | 仅目标文件 |
| 3 | stopH5Recorder | [src/pages/internship/form.vue:520-594](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:520>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [src/pages/internship/form.vue:173-345](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:173>) | 仅目标文件 |
| 5 | 文本块 | [src/manifest.json:1-91](</Users/teriri/WebstormProjects/1623-mobile/src/manifest.json:1>) | 未覆盖冻结目标 |
| 6 | usePermissions.judgeIosPermissionRecord | [src/hooks/useAppPermission.js:60-70](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useAppPermission.js:60>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/App.vue:9-21](</Users/teriri/WebstormProjects/1623-mobile/src/App.vue:9>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/hooks/useAppPermission.js:228-233](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useAppPermission.js:228>) | 未覆盖冻结目标 |
| 9 | useWechatPermissions | [src/hooks/useWechatPermissions.js:1-131](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatPermissions.js:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/hooks/useAppPermission.js:21-59](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useAppPermission.js:21>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

## holdout-web-1623-03

`search_code("提交意见反馈")`

搜索语义：意见反馈提交入口与存储动作

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/store/useFeedbackStore.js:26](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:26>)；符号 submit；锚点 [26]；关键行 [27, 28, 34, 36, 44, 45, 49]。

冻结答案组 original:1：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/feedback/index.vue:117](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:117>)；符号 submit；锚点 [117]；关键行 [120, 121, 122, 126, 129, 131]。

### 基线

查询耗时 7.530 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | submitFeedback | [src/api/feedback.js:12-18](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:12>) | 未覆盖冻结目标 |
| 2 | useFeedbackStore.submit | [src/store/useFeedbackStore.js:26-53](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:26>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/pages/feedback/index.vue:1-174](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:1>) | 单片段覆盖某目标关键正文 |
| 4 | deleteFeedback | [src/api/feedback.js:35-40](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:35>) | 未覆盖冻结目标 |
| 5 | getFeedbackList | [src/api/feedback.js:24-29](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:24>) | 未覆盖冻结目标 |
| 6 | feedbackUrl | [src/api/feedback.js:5-6](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:5>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/feedback/index.vue:231-290](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:231>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/feedback/index.vue:175-230](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:175>) | 仅目标文件 |
| 9 | useFeedbackStore.remove | [src/store/useFeedbackStore.js:69-81](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:69>) | 仅目标文件 |
| 10 | useFeedbackStore.fetchList | [src/store/useFeedbackStore.js:55-67](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:55>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

### 实验

查询耗时 3.575 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | submitFeedback | [src/api/feedback.js:12-18](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:12>) | 未覆盖冻结目标 |
| 2 | useFeedbackStore.submit | [src/store/useFeedbackStore.js:26-53](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:26>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/pages/feedback/index.vue:1-69](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:1>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/feedback/index.vue:83-270](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:83>) | 单片段覆盖某目标关键正文 |
| 5 | deleteFeedback | [src/api/feedback.js:35-40](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:35>) | 未覆盖冻结目标 |
| 6 | getFeedbackList | [src/api/feedback.js:24-29](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:24>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/store/useFeedbackStore.js:1-12](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:1>) | 仅目标文件 |
| 8 | feedbackUrl | [src/api/feedback.js:5-6](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:5>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/store/useFeedbackStore.js:14-24](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:14>) | 仅目标文件 |
| 10 | 文本块 | [src/store/useFeedbackStore.js:83-100](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:83>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

## web-1623-03-para-1

`search_code("语音转文字改正旧片段后，输入框怎么合成新文字？")`

搜索语义：流式转写替换旧片段并有序回填表单

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：paraphrase。

业务族：web-1623-03；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/pages/internship/form.vue:422](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>)；符号 handleIatMessage；锚点 [422]；关键行 [446, 447, 448, 451, 452]。
- [src/pages/internship/form.vue:470](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>)；符号 updateVoiceField；锚点 [470]；关键行 [472, 473, 474, 476]。

### 基线

查询耗时 5.565 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | updateVoiceField | [src/pages/internship/form.vue:470-477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) | 单片段覆盖某目标关键正文 |
| 2 | parseIatResult | [src/pages/internship/form.vue:460-468](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:460>) | 仅目标文件 |
| 3 | handleIatMessage | [src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) | 单片段覆盖某目标关键正文 |
| 4 | cleanupVoiceInput | [src/pages/internship/form.vue:485-493](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:485>) | 仅目标文件 |
| 5 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17-24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) | 未覆盖冻结目标 |
| 6 | processVueFiles.textMap | [scripts/generate-language.js:326-326](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:326>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/components/PracticeDesign/FormField.vue:48-100](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/FormField.vue:48>) | 未覆盖冻结目标 |
| 8 | handleSubmit | [src/pages/basic/internship.vue:97-107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/index/index.vue:68-74](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/feedback/index.vue:231-290](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:231>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

### 实验

查询耗时 4.101 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | updateVoiceField | [src/pages/internship/form.vue:470-477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) | 单片段覆盖某目标关键正文 |
| 2 | parseIatResult | [src/pages/internship/form.vue:460-468](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:460>) | 仅目标文件 |
| 3 | handleIatMessage | [src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) | 单片段覆盖某目标关键正文 |
| 4 | cleanupVoiceInput | [src/pages/internship/form.vue:485-493](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:485>) | 仅目标文件 |
| 5 | 文本块 | [scripts/generate-language.js:294-378](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:294>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17-24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) | 未覆盖冻结目标 |
| 7 | 文本块 | [scripts/generate-language.js:412-502](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:412>) | 未覆盖冻结目标 |
| 8 | handleSubmit | [src/pages/basic/internship.vue:97-107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/index/index.vue:68-74](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/components/PracticeDesign/FormField.vue:45-100](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/FormField.vue:45>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

## web-1623-03-para-2

`search_code("语音转写过程中收到对前文的更正，哪里用修订后的片段重新拼出并显示输入框文字？")`

搜索语义：流式转写替换旧片段并有序回填表单

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：paraphrase。

业务族：web-1623-03；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/pages/internship/form.vue:422](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>)；符号 handleIatMessage；锚点 [422]；关键行 [446, 447, 448, 451, 452]。
- [src/pages/internship/form.vue:470](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>)；符号 updateVoiceField；锚点 [470]；关键行 [472, 473, 474, 476]。

### 基线

查询耗时 4.588 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | updateVoiceField | [src/pages/internship/form.vue:470-477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) | 单片段覆盖某目标关键正文 |
| 2 | handleIatMessage | [src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) | 单片段覆盖某目标关键正文 |
| 3 | cleanupVoiceInput | [src/pages/internship/form.vue:485-493](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:485>) | 仅目标文件 |
| 4 | finishVoiceInput | [src/pages/internship/form.vue:479-483](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:479>) | 仅目标文件 |
| 5 | 文本块 | [src/pages/feedback/index.vue:117-290](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:117>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17-24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) | 未覆盖冻结目标 |
| 7 | default.computed.btnConfirmShowText | [src/components/SkuSelector/index.vue:737-739](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:737>) | 未覆盖冻结目标 |
| 8 | processVueFiles.textMap | [scripts/generate-language.js:326-326](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:326>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/store/useFeedbackStore.js:17-24](</Users/teriri/WebstormProjects/1623-mobile/src/store/useFeedbackStore.js:17>) | 未覆盖冻结目标 |
| 10 | handleClick | [src/components/PracticeDesign/SubmitBar.vue:26-28](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:26>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

### 实验

查询耗时 4.003 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | updateVoiceField | [src/pages/internship/form.vue:470-477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) | 单片段覆盖某目标关键正文 |
| 2 | handleIatMessage | [src/pages/internship/form.vue:422-458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) | 单片段覆盖某目标关键正文 |
| 3 | cleanupVoiceInput | [src/pages/internship/form.vue:485-493](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:485>) | 仅目标文件 |
| 4 | finishVoiceInput | [src/pages/internship/form.vue:479-483](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:479>) | 仅目标文件 |
| 5 | 文本块 | [src/pages/feedback/index.vue:1-150](</Users/teriri/WebstormProjects/1623-mobile/src/pages/feedback/index.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17-24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) | 未覆盖冻结目标 |
| 7 | default.computed.btnConfirmShowText | [src/components/SkuSelector/index.vue:737-739](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:737>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/api/feedback.js:1-2](</Users/teriri/WebstormProjects/1623-mobile/src/api/feedback.js:1>) | 未覆盖冻结目标 |
| 9 | processVueFiles.textMap | [scripts/generate-language.js:294-378](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:294>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/internship/detail.vue:173-173](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:173>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

## web-1623-06-para-1

`search_code("学生和老师用手机号码、密码登录后，身份和账号怎么记住？")`

搜索语义：带角色账号登录和会话保存

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：paraphrase。

业务族：web-1623-06；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/pages/login/account.vue:104](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>)；符号 handleLogin；锚点 [104]；关键行 [113, 114, 115, 116, 123, 124, 125, 126]。

### 基线

查询耗时 6.638 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/login/account.vue:1-94](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:1>) | 仅目标文件 |
| 2 | handleLogin | [src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) | 单片段覆盖某目标关键正文 |
| 3 | loginByPhone | [src/components/LoginWrapper/index.vue:242-262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/login/role.vue:1-58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/components/LoginWrapper/index.vue:1-86](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:1>) | 未覆盖冻结目标 |
| 6 | useWechatMobileLogin | [src/hooks/useWechatLogin.js:53-67](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatLogin.js:53>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/components/LoginWrapper/index.vue:114-201](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:114>) | 未覆盖冻结目标 |
| 8 | handleTeacherSelect | [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/index/index.vue:1-116](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/login/index.vue:38-65](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/index.vue:38>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675040833263000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

### 实验

查询耗时 4.093 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/login/account.vue:1-94](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:1>) | 仅目标文件 |
| 2 | handleLogin | [src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) | 单片段覆盖某目标关键正文 |
| 3 | loginByPhone | [src/components/LoginWrapper/index.vue:242-262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/login/role.vue:1-58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/components/LoginWrapper/index.vue:1-201](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:1>) | 未覆盖冻结目标 |
| 6 | useWechatMobileLogin | [src/hooks/useWechatLogin.js:53-67](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatLogin.js:53>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/login/index.vue:38-65](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/index.vue:38>) | 未覆盖冻结目标 |
| 8 | handleTeacherSelect | [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/index/index.vue:1-116](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) | 未覆盖冻结目标 |
| 10 | loadInfo | [src/pages/basic/internship.vue:61-73](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:61>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678945782020000-40e118dc62f6-holdout-web-1623-01-attempt1.stderr>)

## web-1623-06-para-2

`search_code("通过手机号与口令进入系统时，哪里提交所选的学生或教师角色，并在认证成功后保留登录信息？")`

搜索语义：带角色账号登录和会话保存

项目：/Users/teriri/WebstormProjects/1623-mobile；样本组：paraphrase。

业务族：web-1623-06；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/pages/login/account.vue:104](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>)；符号 handleLogin；锚点 [104]；关键行 [113, 114, 115, 116, 123, 124, 125, 126]。

### 基线

查询耗时 4.472 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/login/account.vue:1-94](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:1>) | 仅目标文件 |
| 2 | handleLogin | [src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/pages/login/role.vue:1-58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:1>) | 未覆盖冻结目标 |
| 4 | handleEnter | [src/pages/login/role.vue:60-62](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:60>) | 未覆盖冻结目标 |
| 5 | handleTeacherSelect | [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 未覆盖冻结目标 |
| 6 | loginByPhone | [src/components/LoginWrapper/index.vue:242-262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/index/index.vue:1-88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/login/index.vue:41-65](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/index.vue:41>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/components/LoginWrapper/index.vue:114-124](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:114>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/components/LoginWrapper/index.vue:1-86](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675073172803000-40e118dc62f6-web-1623-06-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675073172803000-40e118dc62f6-web-1623-06-para-2-attempt1.stderr>)

### 实验

查询耗时 3.933 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/login/account.vue:1-94](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:1>) | 仅目标文件 |
| 2 | handleLogin | [src/pages/login/account.vue:104-132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/pages/login/role.vue:1-58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:1>) | 未覆盖冻结目标 |
| 4 | handleEnter | [src/pages/login/role.vue:60-62](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:60>) | 未覆盖冻结目标 |
| 5 | loginByPhone | [src/components/LoginWrapper/index.vue:242-262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) | 未覆盖冻结目标 |
| 6 | handleTeacherSelect | [src/pages/basic/internship.vue:80-95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/index/index.vue:68-88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/login/index.vue:41-65](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/index.vue:41>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/components/LoginWrapper/index.vue:1-86](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/components/LoginWrapper/index.vue:114-124](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:114>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678972916258000-40e118dc62f6-web-1623-06-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678972916258000-40e118dc62f6-web-1623-06-para-2-attempt1.stderr>)

## holdout-web-1321-01

`search_code("账号密码登录")`

搜索语义：账号密码校验与登录后导航

项目：/Users/teriri/WebstormProjects/1321_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/login/index.vue:110](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:110>)；符号 handleLogin；锚点 [110]；关键行 [111, 120, 132, 134, 140, 141, 151]。

人工审计：真实账号密码检查和userStore.onLogin、成功重进首页仍由handleLogin110–153提供。首条apiIndexLogin是请求封装，不能替代页面登录动作；三侧完整答案均为第2。

### 基线

查询耗时 6.037 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | apiIndexLogin | [src/api/index.js:20-26](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:20>) | 未覆盖冻结目标 |
| 2 | handleLogin | [src/pages/login/index.vue:110-153](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:110>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/language/zh-CN.js:1-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/login/index.vue:1-104](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:1>) | 仅目标文件 |
| 5 | onGetPhoneNumber | [src/components/LoginWrapper/index.vue:166-191](</Users/teriri/WebstormProjects/1321_frontend/src/components/LoginWrapper/index.vue:166>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/components/LoginWrapper/index.vue:1-108](</Users/teriri/WebstormProjects/1321_frontend/src/components/LoginWrapper/index.vue:1>) | 未覆盖冻结目标 |
| 7 | getWechatMobile | [src/api/common.js:6-12](</Users/teriri/WebstormProjects/1321_frontend/src/api/common.js:6>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/login/index.vue:272-362](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:272>) | 仅目标文件 |
| 9 | wechatLogin | [src/api/common.js:20-26](</Users/teriri/WebstormProjects/1321_frontend/src/api/common.js:20>) | 未覆盖冻结目标 |
| 10 | useUserStore.res | [src/store/useUserStore.js:22-25](</Users/teriri/WebstormProjects/1321_frontend/src/store/useUserStore.js:22>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

### 实验

查询耗时 4.022 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | apiIndexLogin | [src/api/index.js:1-48](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:1>) | 未覆盖冻结目标 |
| 2 | handleLogin | [src/pages/login/index.vue:110-153](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:110>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/language/zh-CN.js:68-100](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:68>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/login/index.vue:1-104](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:1>) | 仅目标文件 |
| 5 | 文本块 | [src/language/en-US.js:75-98](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:75>) | 未覆盖冻结目标 |
| 6 | onGetPhoneNumber | [src/components/LoginWrapper/index.vue:166-191](</Users/teriri/WebstormProjects/1321_frontend/src/components/LoginWrapper/index.vue:166>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/login/index.vue:284-362](</Users/teriri/WebstormProjects/1321_frontend/src/pages/login/index.vue:284>) | 仅目标文件 |
| 8 | 文本块 | [src/components/LoginWrapper/index.vue:1-108](</Users/teriri/WebstormProjects/1321_frontend/src/components/LoginWrapper/index.vue:1>) | 未覆盖冻结目标 |
| 9 | getWechatMobile | [src/api/common.js:1-31](</Users/teriri/WebstormProjects/1321_frontend/src/api/common.js:1>) | 未覆盖冻结目标 |
| 10 | useUserStore.res | [src/store/useUserStore.js:1-70](</Users/teriri/WebstormProjects/1321_frontend/src/store/useUserStore.js:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

## holdout-web-1321-02

`search_code("查看某天的蛋糕订单明细")`

搜索语义：按日期加载订单分类数据

项目：/Users/teriri/WebstormProjects/1321_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/order-detail/index.vue:105](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:105>)；符号 loadOrderDetails；锚点 [105]；关键行 [106, 112, 114, 115, 131]。

人工审计：loadOrderDetails105–133携日期调用订单明细接口并写入分类，第二轮与第一轮均第4；原基线第6。首条仍为预订页面的蛋糕目录加载，不是订单明细。

### 基线

查询耗时 7.135 秒；声明/关键正文/范围首次覆盖：6 / 6 / 6。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | loadCakeData | [src/pages/book/index.vue:105-221](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:105>) | 未覆盖冻结目标 |
| 2 | apiIndexBookingCake | [src/api/index.js:168-174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) | 未覆盖冻结目标 |
| 3 | apiIndexCakeList | [src/api/index.js:138-144](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:138>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/statistics/index.vue:1-172](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:1>) | 未覆盖冻结目标 |
| 5 | handleViewAllDetails | [src/pages/statistics/index.vue:297-380](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:297>) | 未覆盖冻结目标 |
| 6 | loadOrderDetails | [src/pages/order-detail/index.vue:1-201](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:1>) | 单片段覆盖某目标关键正文 |
| 7 | loadStockData | [src/pages/stock/index.vue:156-259](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:156>) | 未覆盖冻结目标 |
| 8 | handleUpload | [src/pages/book/index.vue:280-432](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) | 未覆盖冻结目标 |
| 9 | apiIndexOrderInfo | [src/api/index.js:301-307](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:301>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/language/en-US.js:61-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:61>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

### 实验

查询耗时 4.156 秒；声明/关键正文/范围首次覆盖：4 / 4 / 4。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | loadCakeData | [src/pages/book/index.vue:105-221](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:105>) | 未覆盖冻结目标 |
| 2 | apiIndexBookingCake | [src/api/index.js:93-192](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:93>) | 未覆盖冻结目标 |
| 3 | handleViewAllDetails | [src/pages/statistics/index.vue:297-381](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:297>) | 未覆盖冻结目标 |
| 4 | loadOrderDetails | [src/pages/order-detail/index.vue:105-266](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:105>) | 单片段覆盖某目标关键正文 |
| 5 | 文本块 | [src/language/zh-CN.js:36-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:36>) | 未覆盖冻结目标 |
| 6 | handleUpload | [src/pages/book/index.vue:280-433](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) | 未覆盖冻结目标 |
| 7 | apiIndexOrderInfo | [src/api/index.js:200-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:200>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/language/en-US.js:27-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:27>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/order-detail/index.vue:1-58](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:1>) | 仅目标文件 |
| 10 | 文本块 | [src/pages/book/index.vue:261-278](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:261>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

## holdout-web-1321-03

`search_code("点击用品分类后，页面怎样滚到对应的商品区域？")`

搜索语义：用品分类选中与滚动锚点

项目：/Users/teriri/WebstormProjects/1321_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/item/index.vue:291](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:291>)；符号 selectCategory；锚点 [291]；关键行 [292, 294, 296, 299]。

### 基线

查询耗时 4.225 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | selectCategory | [src/pages/item/index.vue:291-301](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:291>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/item/index.vue:1-108](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:1>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/item/index.vue:367-512](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/item/index.vue:134-149](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:134>) | 仅目标文件 |
| 5 | apiIndexArticlesClassify | [src/api/index.js:193-199](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:193>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/stock/index.vue:1-171](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/order-detail/index.vue:1-58](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:1>) | 未覆盖冻结目标 |
| 8 | handleScroll | [src/pages/order-detail/index.vue:60-102](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:60>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/order-detail/index.vue:105-258](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:105>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/book/index.vue:488-544](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:488>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

### 实验

查询耗时 3.808 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | selectCategory | [src/pages/item/index.vue:291-301](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:291>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/item/index.vue:1-126](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:1>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/item/index.vue:367-514](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/item/index.vue:134-149](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:134>) | 仅目标文件 |
| 5 | apiIndexArticlesClassify | [src/api/index.js:175-255](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:175>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/book/index.vue:1-104](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/stock/index.vue:57-80](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:57>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/order-detail/index.vue:1-58](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:1>) | 未覆盖冻结目标 |
| 9 | handleScroll | [src/pages/order-detail/index.vue:60-102](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:60>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/order-detail/index.vue:148-266](</Users/teriri/WebstormProjects/1321_frontend/src/pages/order-detail/index.vue:148>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

## web-1321-03-para-1

`search_code("盘点蛋糕后，怎么只保存改过的剩余、损坏和过期数？")`

搜索语义：检测库存字段变化并批量保存

项目：/Users/teriri/WebstormProjects/1321_frontend；样本组：paraphrase。

业务族：web-1321-03；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/pages/stock/index.vue:575](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:575>)；符号 handleConfirmChanges；锚点 [575]；关键行 [583, 584, 585, 586, 589, 591, 592, 593, 624, 625, 626, 649]。

人工审计：handleConfirmChanges575–665筛选剩余、损坏、过期变化并携日期批量提交。第二轮第2的474–665完整返回，与原基线一致；第一轮只有第3提供。首条changedItems只是展示变化列表，不能替代提交。

### 基线

查询耗时 6.505 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/stock/index.vue:308-360](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:308>) | 仅目标文件 |
| 2 | 文本块 | [src/pages/stock/index.vue:474-665](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) | 单片段覆盖某目标关键正文 |
| 3 | modifiedCount | [src/pages/stock/index.vue:291-304](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:291>) | 仅目标文件 |
| 4 | apiIndexEditStock | [src/api/index.js:413-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/stock/index.vue:416-473](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:416>) | 仅目标文件 |
| 6 | statisticsData | [src/pages/statistics/index.vue:127-133](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:127>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/statistics/index.vue:1-112](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/language/zh-CN.js:1-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/delivery/index.vue:270-377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:270>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/delivery/index.vue:1-229](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

### 实验

查询耗时 4.092 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/stock/index.vue:308-360](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:308>) | 仅目标文件 |
| 2 | 文本块 | [src/pages/stock/index.vue:474-665](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) | 单片段覆盖某目标关键正文 |
| 3 | modifiedCount | [src/pages/stock/index.vue:291-304](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:291>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/stock/index.vue:1-171](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:1>) | 仅目标文件 |
| 5 | apiIndexEditStock | [src/api/index.js:390-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:390>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/language/en-US.js:75-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:75>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/language/zh-CN.js:36-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:36>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/statistics/index.vue:113-167](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:113>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/statistics/index.vue:34-98](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:34>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/delivery/index.vue:270-377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:270>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

## web-1321-03-para-2

`search_code("确认库存清点时，哪里筛出数量有变化的蛋糕，把余量、破损量和过期量一起提交？")`

搜索语义：检测库存字段变化并批量保存

项目：/Users/teriri/WebstormProjects/1321_frontend；样本组：paraphrase。

业务族：web-1321-03；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/pages/stock/index.vue:575](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:575>)；符号 handleConfirmChanges；锚点 [575]；关键行 [583, 584, 585, 586, 589, 591, 592, 593, 624, 625, 626, 649]。

人工审计：同一库存提交实现575–665在第二轮第3的523–735完整出现，三侧完整名次均为3。恢复前后邻接函数使片段加宽，首条仍只有变更展示，未改善完整答案位置。

### 基线

查询耗时 6.158 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/stock/index.vue:308-429](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:308>) | 仅目标文件 |
| 2 | modifiedCount | [src/pages/stock/index.vue:291-304](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:291>) | 仅目标文件 |
| 3 | handleConfirmChanges | [src/pages/stock/index.vue:523-733](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:523>) | 单片段覆盖某目标关键正文 |
| 4 | apiIndexEditStock | [src/api/index.js:413-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) | 未覆盖冻结目标 |
| 5 | isValueModified.fieldMap | [src/pages/stock/index.vue:513-517](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:513>) | 仅目标文件 |
| 6 | 文本块 | [src/pages/statistics/index.vue:54-172](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) | 未覆盖冻结目标 |
| 7 | apiIndexBookingCake | [src/api/index.js:168-174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/book/index.vue:45-96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/language/zh-CN.js:1-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:1>) | 未覆盖冻结目标 |
| 10 | totalQuantity | [src/pages/book/index.vue:198-208](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:198>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675077779958000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

### 实验

查询耗时 5.403 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/stock/index.vue:308-429](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:308>) | 仅目标文件 |
| 2 | modifiedCount | [src/pages/stock/index.vue:291-304](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:291>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/stock/index.vue:523-735](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:523>) | 单片段覆盖某目标关键正文 |
| 4 | apiIndexEditStock | [src/api/index.js:334-419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:334>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/stock/index.vue:1-160](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:1>) | 仅目标文件 |
| 6 | 文本块 | [src/language/zh-CN.js:1-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/language/en-US.js:75-116](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:75>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/statistics/index.vue:34-98](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:34>) | 未覆盖冻结目标 |
| 9 | apiIndexBookingCake | [src/api/index.js:119-174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:119>) | 未覆盖冻结目标 |
| 10 | totalQuantity | [src/pages/book/index.vue:198-208](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:198>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788678977091830000-ea9b8b3f99ad-holdout-web-1321-01-attempt1.stderr>)

## holdout-web-1305-01

`search_code("删除供应商")`

搜索语义：供应商确认删除与列表刷新

项目：/Users/teriri/WebstormProjects/1305_pc；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/views/supplier/index.vue:371](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:371>)；符号 deleteRow；锚点 [371]；关键行 [373, 374, 377, 379, 381]。

### 基线

查询耗时 4.733 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | deleteRow | [src/views/supplier/index.vue:343-387](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:343>) | 单片段覆盖某目标关键正文 |
| 2 | useBusinessStore.assertSupplierDeletable | [src/store/useBusinessStore.js:4076-4082](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4076>) | 未覆盖冻结目标 |
| 3 | apiIndexSupplierDelete | [src/api/index.js:8103-8109](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:8103>) | 未覆盖冻结目标 |
| 4 | useBusinessStore.deleteSupplier | [src/store/useBusinessStore.js:4089-4093](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4089>) | 未覆盖冻结目标 |
| 5 | useBusinessStore.saveSupplier | [src/store/useBusinessStore.js:4011-4067](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4011>) | 未覆盖冻结目标 |
| 6 | resetForm | [src/views/supplier/index.vue:181-184](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:181>) | 仅目标文件 |
| 7 | 文本块 | [src/views/supplier/index.vue:1-54](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:1>) | 仅目标文件 |
| 8 | openEditDialog | [src/views/supplier/index.vue:279-334](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:279>) | 仅目标文件 |
| 9 | useBusinessStore.suppliers | [src/store/useBusinessStore.js:206-210](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:206>) | 未覆盖冻结目标 |
| 10 | normalizeSupplier | [src/utils/pcBusinessResources.js:991-1005](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:991>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

### 实验

查询耗时 3.302 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | deleteRow | [src/views/supplier/index.vue:343-387](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:343>) | 单片段覆盖某目标关键正文 |
| 2 | useBusinessStore.assertSupplierDeletable | [src/store/useBusinessStore.js:4011-4093](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4011>) | 未覆盖冻结目标 |
| 3 | apiIndexSupplierDelete | [src/api/index.js:8103-8109](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:8103>) | 未覆盖冻结目标 |
| 4 | resetForm | [src/views/supplier/index.vue:181-184](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:181>) | 仅目标文件 |
| 5 | 文本块 | [src/views/supplier/index.vue:1-33](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:1>) | 仅目标文件 |
| 6 | openEditDialog | [src/views/supplier/index.vue:279-302](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:279>) | 仅目标文件 |
| 7 | useBusinessStore.suppliers | [src/store/useBusinessStore.js:206-210](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:206>) | 未覆盖冻结目标 |
| 8 | normalizeSupplier | [src/utils/pcBusinessResources.js:983-1005](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:983>) | 未覆盖冻结目标 |
| 9 | useBusinessStore.assertEnabledSupplier | [src/store/useBusinessStore.js:561-565](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:561>) | 未覆盖冻结目标 |
| 10 | supplierOptions | [src/views/stock-record/index.vue:194-199](</Users/teriri/WebstormProjects/1305_pc/src/views/stock-record/index.vue:194>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

## holdout-web-1305-02

`search_code("禁用仓库失败时，界面状态如何恢复？")`

搜索语义：仓库状态提交失败回滚

项目：/Users/teriri/WebstormProjects/1305_pc；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/views/warehouse/index.vue:391](</Users/teriri/WebstormProjects/1305_pc/src/views/warehouse/index.vue:391>)；符号 changeStatus；锚点 [391]；关键行 [392, 396, 398, 401, 402, 408, 409]。

### 基线

查询耗时 3.855 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | changeStatus | [src/views/warehouse/index.vue:265-471](</Users/teriri/WebstormProjects/1305_pc/src/views/warehouse/index.vue:265>) | 单片段覆盖某目标关键正文 |
| 2 | syncSelectedWarehouseStock | [src/views/stock-flow/index.vue:516-568](</Users/teriri/WebstormProjects/1305_pc/src/views/stock-flow/index.vue:516>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/views/auth/StoreStatusPage.vue:30-98](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:30>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/views/department/index.vue:309-361](</Users/teriri/WebstormProjects/1305_pc/src/views/department/index.vue:309>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/views/supplier/index.vue:336-388](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:336>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/views/warehouse/index.vue:1-57](</Users/teriri/WebstormProjects/1305_pc/src/views/warehouse/index.vue:1>) | 仅目标文件 |
| 7 | 文本块 | [src/views/settings-store-management/index.vue:294-353](</Users/teriri/WebstormProjects/1305_pc/src/views/settings-store-management/index.vue:294>) | 未覆盖冻结目标 |
| 8 | persistTimeCardTemplate | [src/views/package-template/index.vue:678-706](</Users/teriri/WebstormProjects/1305_pc/src/views/package-template/index.vue:678>) | 未覆盖冻结目标 |
| 9 | resolveStoreStatusFromError | [src/utils/pcAdminSession.js:199-205](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:199>) | 未覆盖冻结目标 |
| 10 | loadPermission | [src/views/employee-permission/index.vue:185-203](</Users/teriri/WebstormProjects/1305_pc/src/views/employee-permission/index.vue:185>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

### 实验

查询耗时 4.801 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | changeStatus | [src/views/warehouse/index.vue:266-471](</Users/teriri/WebstormProjects/1305_pc/src/views/warehouse/index.vue:266>) | 单片段覆盖某目标关键正文 |
| 2 | syncSelectedWarehouseStock | [src/views/stock-flow/index.vue:522-568](</Users/teriri/WebstormProjects/1305_pc/src/views/stock-flow/index.vue:522>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/views/auth/AuthPage.vue:305-446](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:305>) | 未覆盖冻结目标 |
| 4 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:50-68](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:50>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/views/department/index.vue:309-361](</Users/teriri/WebstormProjects/1305_pc/src/views/department/index.vue:309>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/views/supplier/index.vue:336-388](</Users/teriri/WebstormProjects/1305_pc/src/views/supplier/index.vue:336>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/views/settings-store-management/index.vue:294-353](</Users/teriri/WebstormProjects/1305_pc/src/views/settings-store-management/index.vue:294>) | 未覆盖冻结目标 |
| 8 | persistTimeCardTemplate | [src/views/package-template/index.vue:678-706](</Users/teriri/WebstormProjects/1305_pc/src/views/package-template/index.vue:678>) | 未覆盖冻结目标 |
| 9 | resolveStoreStatusFromError | [src/utils/pcAdminSession.js:159-216](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>) | 未覆盖冻结目标 |
| 10 | loadPermission | [src/views/employee-permission/index.vue:191-203](</Users/teriri/WebstormProjects/1305_pc/src/views/employee-permission/index.vue:191>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

## holdout-web-1305-03

`search_code("新增或修改优惠券后，哪里提交服务器并让列表显示最新配置？")`

搜索语义：优惠券新增修改与刷新

项目：/Users/teriri/WebstormProjects/1305_pc；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/views/marketing-coupon/index.vue:293](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:293>)；符号 submitForm；锚点 [293]；关键行 [294, 301, 307, 311, 313, 314, 316]。

### 基线

查询耗时 5.056 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | submitForm | [src/views/marketing-coupon/index.vue:269-344](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:269>) | 单片段覆盖某目标关键正文 |
| 2 | refreshRows | [src/views/marketing-coupon/index.vue:246-259](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:246>) | 仅目标文件 |
| 3 | 文本块 | [src/views/marketing-gift/index.vue:226-309](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-gift/index.vue:226>) | 未覆盖冻结目标 |
| 4 | refreshRows | [src/views/marketing-coupon-record/index.vue:202-215](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon-record/index.vue:202>) | 未覆盖冻结目标 |
| 5 | useBusinessStore.saveCoupon | [src/store/useBusinessStore.js:4152-4182](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4152>) | 未覆盖冻结目标 |
| 6 | apiIndexCouponUpdate | [src/api/index.js:3514-3520](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:3514>) | 未覆盖冻结目标 |
| 7 | submitForm | [src/views/card-template/index.vue:407-433](</Users/teriri/WebstormProjects/1305_pc/src/views/card-template/index.vue:407>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/views/marketing-coupon/index.vue:217-236](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:217>) | 仅目标文件 |
| 9 | 文本块 | [src/views/marketing-coupon/index.vue:127-199](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:127>) | 仅目标文件 |
| 10 | apiIndexCouponCreate | [src/api/index.js:3120-3126](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:3120>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

### 实验

查询耗时 3.899 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | submitForm | [src/views/marketing-coupon/index.vue:269-351](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:269>) | 单片段覆盖某目标关键正文 |
| 2 | refreshRows | [src/views/marketing-coupon/index.vue:246-259](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:246>) | 仅目标文件 |
| 3 | 文本块 | [src/views/marketing-gift/index.vue:226-309](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-gift/index.vue:226>) | 未覆盖冻结目标 |
| 4 | refreshRows | [src/views/marketing-coupon-record/index.vue:202-221](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon-record/index.vue:202>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/store/useBusinessStore.js:4175-4182](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4175>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/views/marketing-coupon/index.vue:217-223](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:217>) | 仅目标文件 |
| 7 | 文本块 | [src/store/useBusinessStore.js:4144-4163](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4144>) | 未覆盖冻结目标 |
| 8 | submitCouponGrantApi | [src/views/business-flow/index.vue:2645-2690](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2645>) | 未覆盖冻结目标 |
| 9 | rules | [src/views/marketing-coupon/index.vue:145-151](</Users/teriri/WebstormProjects/1305_pc/src/views/marketing-coupon/index.vue:145>) | 仅目标文件 |
| 10 | loadSettlementUserCoupons | [src/views/order-settlement/index.vue:654-690](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:654>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

## web-1305-01-para-1

`search_code("创建预约，怎么检查会员、服务和员工，再提交时间？")`

搜索语义：预约创建的实体核验和服务端保存链路

项目：/Users/teriri/WebstormProjects/1305_pc；样本组：paraphrase。

业务族：web-1305-01；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/views/appointment-list/index.vue:543](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:543>)；符号 submitForm；锚点 [543]；关键行 [544, 551, 552, 554, 559]。
- [src/views/appointment-list/index.vue:366](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:366>)；符号 buildAppointmentCreateParams；锚点 [366]；关键行 [367, 368, 369, 373, 374, 375, 377, 378, 379, 382, 383]。

冻结答案组 appointment-board：查询未限定列表页；看板的有效会员、启用服务、正常员工核验和时间提交构成同一完整替代链路，两个方法必须同时覆盖
- [src/views/appointment-board/index.vue:918](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:918>)；符号 resolveAppointmentCreateContext；锚点 [918]；关键行 [919, 920, 923, 925, 926, 927, 929, 932, 934, 947, 948, 951, 954]。
- [src/views/appointment-board/index.vue:963](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>)；符号 submitAppointmentCreateApi；锚点 [963]；关键行 [964, 968, 969, 970, 971, 973, 974, 975, 981]。

### 基线

查询耗时 7.020 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | resolveAppointmentCreateContext | [src/views/appointment-board/index.vue:882-1041](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:882>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/views/appointment-list/index.vue:302-412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:302>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/views/appointment-board/index.vue:1047-1251](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1047>) | 仅目标文件 |
| 4 | useBusinessStore.createAppointment | [src/store/useBusinessStore.js:6071-6111](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6071>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/views/appointment-board/index.vue:302-395](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) | 仅目标文件 |
| 6 | 文本块 | [src/views/appointment-list/index.vue:136-243](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:136>) | 仅目标文件 |
| 7 | 文本块 | [src/views/appointment-board/index.vue:99-243](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:99>) | 仅目标文件 |
| 8 | pendingAppointment | [scripts/verify-pc-admin.js:843-854](</Users/teriri/WebstormProjects/1305_pc/scripts/verify-pc-admin.js:843>) | 未覆盖冻结目标 |
| 9 | apiIndexBookOrderCreate | [src/api/index.js:4904-4910](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:4904>) | 未覆盖冻结目标 |
| 10 | apiIndexBookCreate | [src/api/index.js:4870-4876](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:4870>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

### 实验

查询耗时 4.628 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | resolveAppointmentCreateContext | [src/views/appointment-board/index.vue:882-1041](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:882>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/views/appointment-list/index.vue:186-412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:186>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/store/useBusinessStore.js:6059-6091](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6059>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/views/appointment-board/index.vue:1047-1140](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1047>) | 仅目标文件 |
| 5 | 文本块 | [src/views/appointment-board/index.vue:1146-1176](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1146>) | 仅目标文件 |
| 6 | 文本块 | [src/views/appointment-board/index.vue:302-395](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) | 仅目标文件 |
| 7 | pendingAppointment | [scripts/verify-pc-admin.js:843-879](</Users/teriri/WebstormProjects/1305_pc/scripts/verify-pc-admin.js:843>) | 未覆盖冻结目标 |
| 8 | useBusinessStore.assertAppointmentCapacity | [src/store/useBusinessStore.js:5969-6041](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:5969>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/views/appointment-list/index.vue:460-512](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:460>) | 仅目标文件 |
| 10 | apiIndexBookOrderCreate | [src/api/index.js:4877-4910](</Users/teriri/WebstormProjects/1305_pc/src/api/index.js:4877>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

## web-1305-01-para-2

`search_code("安排一条新预约时，哪里确认顾客会员资料、所选服务及工作人员有效，并把预约的起止时刻送到后台？")`

搜索语义：预约创建的实体核验和服务端保存链路

项目：/Users/teriri/WebstormProjects/1305_pc；样本组：paraphrase。

业务族：web-1305-01；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/views/appointment-list/index.vue:543](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:543>)；符号 submitForm；锚点 [543]；关键行 [544, 551, 552, 554, 559]。
- [src/views/appointment-list/index.vue:366](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:366>)；符号 buildAppointmentCreateParams；锚点 [366]；关键行 [367, 368, 369, 373, 374, 375, 377, 378, 379, 382, 383]。

冻结答案组 appointment-board：查询未限定列表页；看板的有效会员、启用服务、正常员工核验和时间提交构成同一完整替代链路，两个方法必须同时覆盖
- [src/views/appointment-board/index.vue:918](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:918>)；符号 resolveAppointmentCreateContext；锚点 [918]；关键行 [919, 920, 923, 925, 926, 927, 929, 932, 934, 947, 948, 951, 954]。
- [src/views/appointment-board/index.vue:963](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>)；符号 submitAppointmentCreateApi；锚点 [963]；关键行 [964, 968, 969, 970, 971, 973, 974, 975, 981]。

人工审计：第二轮第4的预约看板882–986完整包含会员/服务/员工校验与apiIndexBookOrderCreate日期起止时间提交。原基线第1、第一轮第3。第二轮首条988–1041是后续取消/重置逻辑，不含新建提交，所以恢复窗口没有修好此查询，反而继续退后一位。

### 基线

查询耗时 4.537 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/views/appointment-board/index.vue:882-1041](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:882>) | 单片段覆盖某目标关键正文 |
| 2 | buildAppointmentCreateParams | [src/views/appointment-list/index.vue:244-412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:244>) | 单片段覆盖某目标关键正文 |
| 3 | useBusinessStore.createAppointment | [src/store/useBusinessStore.js:6071-6111](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6071>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/views/appointment-board/index.vue:1047-1235](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1047>) | 仅目标文件 |
| 5 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121-6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) | 未覆盖冻结目标 |
| 6 | transferAppointmentFromBoard | [src/views/appointment-board/index.vue:1240-1250](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1240>) | 仅目标文件 |
| 7 | pendingAppointment | [scripts/verify-pc-admin.js:843-854](</Users/teriri/WebstormProjects/1305_pc/scripts/verify-pc-admin.js:843>) | 未覆盖冻结目标 |
| 8 | rules | [src/views/appointment-list/index.vue:214-222](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:214>) | 仅目标文件 |
| 9 | 文本块 | [src/views/appointment-board/index.vue:302-395](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) | 仅目标文件 |
| 10 | 文本块 | [src/views/appointment-list/index.vue:460-512](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:460>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675108000148000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

### 实验

查询耗时 7.040 秒；声明/关键正文/范围首次覆盖：4 / 4 / 4。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/views/appointment-board/index.vue:988-1041](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:988>) | 仅目标文件 |
| 2 | 文本块 | [src/store/useBusinessStore.js:6050-6091](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6050>) | 未覆盖冻结目标 |
| 3 | buildAppointmentCreateParams | [src/views/appointment-list/index.vue:186-412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:186>) | 单片段覆盖某目标关键正文 |
| 4 | submitAppointmentCreateApi | [src/views/appointment-board/index.vue:882-986](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:882>) | 单片段覆盖某目标关键正文 |
| 5 | 文本块 | [src/views/appointment-board/index.vue:1060-1138](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1060>) | 仅目标文件 |
| 6 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6109-6165](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6109>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/views/appointment-board/index.vue:1146-1176](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1146>) | 仅目标文件 |
| 8 | 文本块 | [src/utils/pcBusinessResources.js:1142-1231](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1142>) | 未覆盖冻结目标 |
| 9 | pendingAppointment | [scripts/verify-pc-admin.js:843-879](</Users/teriri/WebstormProjects/1305_pc/scripts/verify-pc-admin.js:843>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/views/appointment-list/index.vue:460-512](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:460>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679000746884000-426e74ea9ee4-holdout-web-1305-01-attempt1.stderr>)

## holdout-web-1573-01

`search_code("调整机器人关节角度时，输入值怎样限制范围并真正转动三维模型？")`

搜索语义：关节角度输入及三维节点旋转

项目：/Users/teriri/WebstormProjects/1573-frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [apps/web/src/pages/ModelViewer.vue:411](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:411>)；符号 handleJointInput；锚点 [411]；关键行 [413, 418, 419, 420]。
- [apps/web/src/pages/ModelViewer.vue:400](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:400>)；符号 applyJointControl；锚点 [400]；关键行 [401, 402, 403, 407, 408]。

人工审计：第二轮首条400–625重新联合显示handleJointInput411–421和applyJointControl400–409，恢复到原基线首条；第一轮拆成第1/第4。冻结目标仍遗漏clampDegree149–151实际限幅公式，三侧均不得称完整数值限幅实现已经验证。

### 基线

查询耗时 5.372 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [apps/web/src/pages/ModelViewer.vue:400-625](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:400>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [apps/web/src/pages/ModelViewer.vue:695-848](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:695>) | 仅目标文件 |
| 3 | handleMaxRotationInput | [apps/web/src/pages/ModelViewer.vue:384-398](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:384>) | 仅目标文件 |
| 4 | 文本块 | [apps/web/src/pages/ModelViewer.vue:165-294](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:165>) | 仅目标文件 |
| 5 | segments | [apps/web/src/pages/PackageDetail.vue:10-23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:10>) | 未覆盖冻结目标 |
| 6 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:16-23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:16>) | 未覆盖冻结目标 |
| 8 | handleMove | [apps/web/src/components/neural/DraggableNumber.vue:32-42](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>) | 未覆盖冻结目标 |
| 9 | handleMove | [apps/web/src/components/neural/SVGDraggableNumber.vue:37-47](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:37>) | 未覆盖冻结目标 |
| 10 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:453-600](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:453>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

### 实验

查询耗时 5.433 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [apps/web/src/pages/ModelViewer.vue:400-625](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:400>) | 单片段覆盖某目标关键正文 |
| 2 | handleMaxRotationInput | [apps/web/src/pages/ModelViewer.vue:384-398](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:384>) | 仅目标文件 |
| 3 | 文本块 | [apps/web/src/pages/ModelViewer.vue:165-294](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:165>) | 仅目标文件 |
| 4 | 文本块 | [apps/web/src/pages/ModelViewer.vue:1-111](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:1>) | 仅目标文件 |
| 5 | segments | [apps/web/src/pages/PackageDetail.vue:10-23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:10>) | 未覆盖冻结目标 |
| 6 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:576-707](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:576>) | 未覆盖冻结目标 |
| 8 | handleMove | [apps/web/src/components/neural/SVGDraggableNumber.vue:37-47](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:37>) | 未覆盖冻结目标 |
| 9 | handleMove | [apps/web/src/components/neural/DraggableNumber.vue:32-42](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>) | 未覆盖冻结目标 |
| 10 | startServer | [apps/web/server.ts:7-61](</Users/teriri/WebstormProjects/1573-frontend/apps/web/server.ts:7>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

## holdout-web-1573-02

`search_code("左右拖动神经网络参数数字")`

搜索语义：拖动数值的步长计算与范围限制

项目：/Users/teriri/WebstormProjects/1573-frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [apps/web/src/components/neural/SVGDraggableNumber.vue:37](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:37>)；符号 handleMove；锚点 [37]；关键行 [38, 42, 43, 44, 45, 46]。

冻结答案组 original:1：从原平面真值等价转换，组内全部目标必须覆盖
- [apps/web/src/components/neural/DraggableNumber.vue:32](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>)；符号 handleMove；锚点 [32]；关键行 [33, 37, 38, 39, 40, 41]。

人工审计：handleMove32–42按横向位移计算步数、限制min/max并emit新值。第二轮第2完整控件1–71恢复该实现，优于第一轮第3但仍晚于原基线第1；首条NeuralGridDemo页面不是数值拖动算法。

### 基线

查询耗时 4.357 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [apps/web/src/components/neural/DraggableNumber.vue:1-71](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:491-718](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:491>) | 未覆盖冻结目标 |
| 3 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:1-81](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:1>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:1-134](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:156-330](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) | 未覆盖冻结目标 |
| 6 | getTransformGridLine | [apps/web/src/components/NeuralGridDemo.vue:367-378](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:367>) | 未覆盖冻结目标 |
| 7 | PanelDragState | [apps/web/src/pages/ModelViewer.vue:46-52](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:46>) | 未覆盖冻结目标 |
| 8 | handlePanelDragMove | [apps/web/src/pages/ModelViewer.vue:345-354](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:345>) | 未覆盖冻结目标 |
| 9 | 文本块 | [apps/web/src/pages/VideoSegment.vue:280-334](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:280>) | 未覆盖冻结目标 |
| 10 | handlePanelDragStart | [apps/web/src/pages/ModelViewer.vue:333-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:333>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

### 实验

查询耗时 3.828 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:479-707](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:479>) | 未覆盖冻结目标 |
| 2 | 文本块 | [apps/web/src/components/neural/DraggableNumber.vue:1-71](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:1>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:1-81](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:1>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:1-134](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:156-340](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) | 未覆盖冻结目标 |
| 6 | getTransformGridLine | [apps/web/src/components/NeuralGridDemo.vue:367-378](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:367>) | 未覆盖冻结目标 |
| 7 | PanelDragState | [apps/web/src/pages/ModelViewer.vue:46-52](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:46>) | 未覆盖冻结目标 |
| 8 | handlePanelDragMove | [apps/web/src/pages/ModelViewer.vue:345-354](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:345>) | 未覆盖冻结目标 |
| 9 | handlePanelDragStart | [apps/web/src/pages/ModelViewer.vue:333-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:333>) | 未覆盖冻结目标 |
| 10 | handlePanelDragEnd | [apps/web/src/pages/ModelViewer.vue:356-367](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:356>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

## holdout-web-1573-03

`search_code("课程互动完成以后，哪里关闭互动界面并继续播放视频？")`

搜索语义：完成课程互动与恢复播放

项目：/Users/teriri/WebstormProjects/1573-frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [apps/web/src/pages/VideoSegment.vue:205](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:205>)；符号 completeInteraction；锚点 [205]；关键行 [207, 208, 209, 211, 216, 218]。

### 基线

查询耗时 6.933 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | completeInteraction | [apps/web/src/pages/VideoSegment.vue:205-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:205>) | 单片段覆盖某目标关键正文 |
| 2 | handleVideoEnded | [apps/web/src/pages/VideoSegment.vue:151-160](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:151>) | 仅目标文件 |
| 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-121](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 仅目标文件 |
| 4 | toggleVideoPlayback | [apps/web/src/pages/VideoSegment.vue:162-183](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:162>) | 仅目标文件 |
| 5 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 6 | segments | [apps/web/src/pages/PackageDetail.vue:1-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/pages/PackageList.vue:1-82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:1>) | 未覆盖冻结目标 |
| 8 | clearJointRuntime | [apps/web/src/pages/ModelViewer.vue:210-214](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:210>) | 未覆盖冻结目标 |
| 9 | clearSelectedJoint | [apps/web/src/pages/ModelViewer.vue:369-373](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:369>) | 未覆盖冻结目标 |
| 10 | 文本块 | [apps/web/src/pages/ModelViewer.vue:849-857](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:849>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

### 实验

查询耗时 3.709 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | completeInteraction | [apps/web/src/pages/VideoSegment.vue:205-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:205>) | 单片段覆盖某目标关键正文 |
| 2 | handleVideoEnded | [apps/web/src/pages/VideoSegment.vue:151-160](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:151>) | 仅目标文件 |
| 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-121](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 仅目标文件 |
| 4 | toggleVideoPlayback | [apps/web/src/pages/VideoSegment.vue:162-183](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:162>) | 仅目标文件 |
| 5 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 6 | segments | [apps/web/src/pages/PackageDetail.vue:10-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:10>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/pages/PackageList.vue:1-82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [apps/web/src/pages/ModelViewer.vue:537-759](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:537>) | 未覆盖冻结目标 |
| 9 | clearJointRuntime | [apps/web/src/pages/ModelViewer.vue:210-214](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:210>) | 未覆盖冻结目标 |
| 10 | clearSelectedJoint | [apps/web/src/pages/ModelViewer.vue:369-373](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:369>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

## web-1573-01-para-1

`search_code("课程视频到了互动点，哪里决定暂停并显示互动？")`

搜索语义：视频播放阈值触发一次互动暂停

项目：/Users/teriri/WebstormProjects/1573-frontend；样本组：paraphrase。

业务族：web-1573-01；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [apps/web/src/pages/VideoSegment.vue:75](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>)；符号 maybeOpenInteraction；锚点 [75]；关键行 [79, 80, 81, 82, 83, 88]。
- [apps/web/src/pages/VideoSegment.vue:69](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:69>)；符号 openInteraction；锚点 [69]；关键行 [70, 71, 72]。

人工审计：第二轮第1有maybeOpenInteraction75–89条件判断，第3的1–73补回openInteraction69–73的互动状态设置与video.pause。第一轮Top10缺这个被调函数；原基线需第4，因此恢复窗口确实修复共同实现的遗漏。

### 基线

查询耗时 4.727 秒；声明/关键正文/范围首次覆盖：4 / 4 / 4。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75-89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:162-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:162>) | 仅目标文件 |
| 3 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132-140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) | 仅目标文件 |
| 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 单片段覆盖某目标关键正文 |
| 5 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 6 | segments | [apps/web/src/pages/PackageDetail.vue:1-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/pages/Login.vue:7-17](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:7>) | 未覆盖冻结目标 |
| 8 | 文本块 | [apps/web/src/pages/PackageList.vue:46-82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:46>) | 未覆盖冻结目标 |
| 9 | hoverOverlay.point | [apps/web/src/components/NeuralGridDemo.vue:151-151](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:151>) | 未覆盖冻结目标 |
| 10 | makeJointClickable | [apps/web/src/pages/ModelViewer.vue:242-250](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:242>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

### 实验

查询耗时 3.628 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75-89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) | 单片段覆盖某目标关键正文 |
| 2 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132-140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) | 仅目标文件 |
| 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:162-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:162>) | 仅目标文件 |
| 5 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 6 | segments | [apps/web/src/pages/PackageDetail.vue:10-23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:10>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/pages/PackageList.vue:46-82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:46>) | 未覆盖冻结目标 |
| 8 | 文本块 | [apps/web/src/pages/PackageDetail.vue:48-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:48>) | 未覆盖冻结目标 |
| 9 | hoverOverlay.point | [apps/web/src/components/NeuralGridDemo.vue:151-151](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:151>) | 未覆盖冻结目标 |
| 10 | makeJointClickable | [apps/web/src/pages/ModelViewer.vue:242-250](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:242>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

## web-1573-01-para-2

`search_code("教学视频播到预设交互时刻后，哪段代码检查触发条件、停下播放并打开对应的互动内容？")`

搜索语义：视频播放阈值触发一次互动暂停

项目：/Users/teriri/WebstormProjects/1573-frontend；样本组：paraphrase。

业务族：web-1573-01；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [apps/web/src/pages/VideoSegment.vue:75](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>)；符号 maybeOpenInteraction；锚点 [75]；关键行 [79, 80, 81, 82, 83, 88]。
- [apps/web/src/pages/VideoSegment.vue:69](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:69>)；符号 openInteraction；锚点 [69]；关键行 [70, 71, 72]。

### 基线

查询耗时 4.939 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132-140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) | 仅目标文件 |
| 2 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75-89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:162-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:162>) | 仅目标文件 |
| 5 | segments | [apps/web/src/pages/PackageDetail.vue:1-80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 7 | hoveredPoint | [apps/web/src/components/NeuralGridDemo.vue:138-144](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:138>) | 未覆盖冻结目标 |
| 8 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97-98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) | 未覆盖冻结目标 |
| 9 | handleJointInput | [apps/web/src/pages/ModelViewer.vue:411-421](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:411>) | 未覆盖冻结目标 |
| 10 | hoverOverlay.isDraggingThis | [apps/web/src/components/NeuralGridDemo.vue:156-156](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675134560749000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

### 实验

查询耗时 3.546 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132-140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) | 仅目标文件 |
| 2 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75-89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1-73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:162-343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:162>) | 仅目标文件 |
| 5 | segments | [apps/web/src/pages/PackageDetail.vue:10-23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:10>) | 未覆盖冻结目标 |
| 6 | 文本块 | [apps/web/metadata.json:1-6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97-98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) | 未覆盖冻结目标 |
| 8 | handleJointInput | [apps/web/src/pages/ModelViewer.vue:411-421](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:411>) | 未覆盖冻结目标 |
| 9 | hoverOverlay.isDraggingThis | [apps/web/src/components/NeuralGridDemo.vue:156-156](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) | 未覆盖冻结目标 |
| 10 | 文本块 | [apps/web/src/pages/ModelViewer.vue:614-642](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:614>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679044851757000-9654beeb071b-holdout-web-1573-01-attempt1.stderr>)

## holdout-web-1198-01

`search_code("生成一周食谱")`

搜索语义：请求生成食谱并保存周计划

项目：/Users/teriri/WebstormProjects/1198_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/createCookBookFill/createCookBookFill.vue:340](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBookFill/createCookBookFill.vue:340>)；符号 handleCreateRecipe；锚点 [340]；关键行 [351, 352, 360, 368, 371, 372, 380, 381, 385, 386]。

人工审计：生成食谱的接口调用、整周数据和日期保存、旧状态清理与跳转都由handleCreateRecipe340–401实现。第二轮第3的309–420完整返回，与原基线和第一轮同为第3；前两条是编辑食谱页面片段，不能算生成调用已在首条。

### 基线

查询耗时 6.980 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:734-793](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:734>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:170-389](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:170>) | 未覆盖冻结目标 |
| 3 | handleGenerate | [src/pages/createCookBookFill/createCookBookFill.vue:277-420](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBookFill/createCookBookFill.vue:277>) | 单片段覆盖某目标关键正文 |
| 4 | handleWeekDish | [src/pages/cookbook/cookbook.vue:558-623](</Users/teriri/WebstormProjects/1198_frontend/src/pages/cookbook/cookbook.vue:558>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/confirmOrder/confirmOrder.vue:292-299](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:292>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:312-319](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:312>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/createCookBook/createCookBook.vue:1-87](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBook/createCookBook.vue:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/createCookBook/createCookBook.vue:547-656](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBook/createCookBook.vue:547>) | 未覆盖冻结目标 |
| 9 | useJumpStore | [src/store/useJumpStore.js:5-244](</Users/teriri/WebstormProjects/1198_frontend/src/store/useJumpStore.js:5>) | 未覆盖冻结目标 |
| 10 | weekDays | [src/pages/cookbook/cookbook.vue:236-252](</Users/teriri/WebstormProjects/1198_frontend/src/pages/cookbook/cookbook.vue:236>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

### 实验

查询耗时 7.059 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:734-793](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:734>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:133-261](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:133>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/pages/createCookBookFill/createCookBookFill.vue:309-420](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBookFill/createCookBookFill.vue:309>) | 单片段覆盖某目标关键正文 |
| 4 | handleWeekDish | [src/pages/cookbook/cookbook.vue:558-625](</Users/teriri/WebstormProjects/1198_frontend/src/pages/cookbook/cookbook.vue:558>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:312-319](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:312>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/confirmOrder/confirmOrder.vue:292-299](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:292>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/createCookBook/createCookBook.vue:547-656](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBook/createCookBook.vue:547>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/createCookBook/createCookBook.vue:118-262](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBook/createCookBook.vue:118>) | 未覆盖冻结目标 |
| 9 | weekDays | [src/pages/cookbook/cookbook.vue:236-252](</Users/teriri/WebstormProjects/1198_frontend/src/pages/cookbook/cookbook.vue:236>) | 未覆盖冻结目标 |
| 10 | handlePopupRightBtn | [src/pages/createCookBook/createCookBook.vue:684-771](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBook/createCookBook.vue:684>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

## holdout-web-1198-02

`search_code("给采购订单补传一张票据照片，怎样保存到订单并更新缩略图？")`

搜索语义：票据照片上传与订单关联

项目：/Users/teriri/WebstormProjects/1198_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/orderForm/orderForm.vue:336](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:336>)；符号 handleUploadReceipt；锚点 [336]；关键行 [337, 338, 341, 343, 348, 351, 357, 358, 359, 362, 368, 369]。

### 基线

查询耗时 6.014 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleUploadReceipt | [src/pages/orderForm/orderForm.vue:328-390](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:328>) | 单片段覆盖某目标关键正文 |
| 2 | indexTicketImageUpdate | [src/api/index.js:156-162](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:156>) | 未覆盖冻结目标 |
| 3 | popupConfirm | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:383-431](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:383>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/orderForm/orderForm.vue:1-108](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:1>) | 仅目标文件 |
| 5 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1-226](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1>) | 未覆盖冻结目标 |
| 6 | indexOrderImageUpdate | [src/api/index.js:187-193](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:187>) | 未覆盖冻结目标 |
| 7 | handleConfirm.submitData | [src/pages/purchaseFood/purchaseFood.vue:709-715](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:709>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/orderForm/orderForm.vue:215-271](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:215>) | 仅目标文件 |
| 9 | saveToAlbum | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1016-1134](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1016>) | 未覆盖冻结目标 |
| 10 | handleBatchInFoodUpload | [src/pages/orderFormDetail/orderFormDetail.vue:1321-1334](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:1321>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

### 实验

查询耗时 4.361 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleUploadReceipt | [src/pages/orderForm/orderForm.vue:328-390](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:328>) | 单片段覆盖某目标关键正文 |
| 2 | indexTicketImageUpdate | [src/api/index.js:156-162](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:156>) | 未覆盖冻结目标 |
| 3 | popupConfirm | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:383-431](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:383>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:53-226](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:53>) | 未覆盖冻结目标 |
| 5 | indexOrderImageUpdate | [src/api/index.js:187-193](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:187>) | 未覆盖冻结目标 |
| 6 | handleConfirm.submitData | [src/pages/purchaseFood/purchaseFood.vue:709-715](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:709>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/orderForm/orderForm.vue:226-267](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:226>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/orderForm/orderForm.vue:33-100](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:33>) | 仅目标文件 |
| 9 | handleBatchInFoodUpload | [src/pages/orderFormDetail/orderFormDetail.vue:1321-1334](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:1321>) | 未覆盖冻结目标 |
| 10 | saveToAlbum | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1016-1134](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1016>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

## holdout-web-1198-03

`search_code("保存修改过的食材采购价格")`

搜索语义：采购改价批量保存与列表刷新

项目：/Users/teriri/WebstormProjects/1198_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/purchaseFood/purchaseFood.vue:556](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:556>)；符号 handleSaveInfo；锚点 [556]；关键行 [559, 560, 562, 566, 572, 573]。

人工审计：采购修改保存handleSaveInfo556–578仍在第二轮第1的522–579，包含editedArr构造、成功后清空改动和重载。三侧核心动作均首条，恢复窗口只重新带入价格输入逻辑。

### 基线

查询耗时 5.499 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:522-579](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:522>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:1-117](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:1>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:178-288](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:178>) | 仅目标文件 |
| 4 | updateGoodsBuy | [src/pages/orderFormDetail/orderFormDetail.vue:483-508](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:483>) | 未覆盖冻结目标 |
| 5 | indexGoodsBuyUpdate | [src/api/index.js:358-364](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:358>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/components/FoodRate.vue:1-121](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/components/FoodRate.vue:124-132](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:124>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/adjustRate/adjustRate.vue:242-380](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:242>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/adjustRate/adjustRate.vue:439-498](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:439>) | 未覆盖冻结目标 |
| 10 | indexGoodsBuy | [src/api/index.js:228-234](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:228>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

### 实验

查询耗时 5.049 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:522-579](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:522>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:47-72](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:47>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/adjustRate/adjustRate.vue:247-414](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:247>) | 未覆盖冻结目标 |
| 4 | updateGoodsBuy | [src/pages/orderFormDetail/orderFormDetail.vue:483-508](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:483>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:118-288](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:118>) | 仅目标文件 |
| 6 | indexGoodsBuyUpdate | [src/api/index.js:358-364](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:358>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/components/FoodRate.vue:1-121](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/purchaseFood/purchaseFood.vue:334-416](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:334>) | 仅目标文件 |
| 9 | 文本块 | [src/pages/adjustRate/adjustRate.vue:427-536](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:427>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/components/FoodRate.vue:124-132](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:124>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

## holdout-web-1198-04

`search_code("调整某道菜的用量时，怎样兼顾固定菜品、总餐费和其他菜的分摊金额？")`

搜索语义：菜品比例调整与预算分配

项目：/Users/teriri/WebstormProjects/1198_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/adjustRate/adjustRate.vue:427](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:427>)；符号 handleRateChange；锚点 [427]；关键行 [429, 432, 436, 437, 444, 446, 452, 455, 463, 466, 484, 491, 493, 495, 507, 530, 533, 534]。

人工审计：第二轮首条381–592完整覆盖handleRateChange427–541的固定菜品、预算、分摊、负数处理，与两参照臂首条覆盖一致。标签为文本块且片段加宽，不构成准确率变化。

### 基线

查询耗时 4.655 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleRateChange | [src/pages/adjustRate/adjustRate.vue:381-592](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:381>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/components/FoodRate.vue:1-176](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:1>) | 未覆盖冻结目标 |
| 3 | initFoodData | [src/pages/adjustRate/adjustRate.vue:49-236](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:49>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:531-733](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:531>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/adjustRate/adjustRate.vue:289-328](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:289>) | 仅目标文件 |
| 6 | 文本块 | [src/pages/adjustRate/adjustRate.vue:329-380](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:329>) | 仅目标文件 |
| 7 | totalPrice | [src/pages/cookbook/cookbook.vue:360-386](</Users/teriri/WebstormProjects/1198_frontend/src/pages/cookbook/cookbook.vue:360>) | 未覆盖冻结目标 |
| 8 | handleAmountInput | [src/pages/purchaseFood/purchaseFood.vue:538-550](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:538>) | 未覆盖冻结目标 |
| 9 | handleEditDish | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:434-437](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:434>) | 未覆盖冻结目标 |
| 10 | handleEditDish | [src/pages/confirmOrder/confirmOrder.vue:392-395](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:392>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

### 实验

查询耗时 4.579 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/adjustRate/adjustRate.vue:381-592](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:381>) | 单片段覆盖某目标关键正文 |
| 2 | initFoodData | [src/pages/adjustRate/adjustRate.vue:49-236](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:49>) | 仅目标文件 |
| 3 | 文本块 | [src/components/FoodRate.vue:61-176](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:61>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/components/FoodRate.vue:1-25](</Users/teriri/WebstormProjects/1198_frontend/src/components/FoodRate.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:531-733](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:531>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/createCookBookFill/createCookBookFill.vue:193-369](</Users/teriri/WebstormProjects/1198_frontend/src/pages/createCookBookFill/createCookBookFill.vue:193>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/adjustRate/adjustRate.vue:345-380](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:345>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/adjustRate/adjustRate.vue:648-649](</Users/teriri/WebstormProjects/1198_frontend/src/pages/adjustRate/adjustRate.vue:648>) | 仅目标文件 |
| 9 | isShowPopup | [src/pages/editOrder/cookbook/cookbook.vue:422-422](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:422>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

## holdout-web-1198-05

`search_code("订单列表分页加载")`

搜索语义：日期筛选下的采购订单分页

项目：/Users/teriri/WebstormProjects/1198_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/orderForm/orderForm.vue:184](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:184>)；符号 queryList；锚点 [184]；关键行 [187, 190, 194, 195, 196, 197, 198, 203, 205, 212, 284, 288]。

人工审计：指定orderForm/queryList184–290第二轮第5完整出现，第一轮也是5、原基线6。但更宽泛的实际问题另有order/order.vue66–80有效分页实现，三侧均在第2；冻结漏掉这一替代，不应把指定目标6→5当用户可回答性提升。

### 基线

查询耗时 5.920 秒；声明/关键正文/范围首次覆盖：6 / 6 / 6。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | loadInventoryRecordList | [src/pages/materialStore/materialStore.vue:573-602](</Users/teriri/WebstormProjects/1198_frontend/src/pages/materialStore/materialStore.vue:573>) | 未覆盖冻结目标 |
| 2 | queryList | [src/pages/order/order.vue:66-107](</Users/teriri/WebstormProjects/1198_frontend/src/pages/order/order.vue:66>) | 未覆盖冻结目标 |
| 3 | loadInventoryGoodsList | [src/pages/materialStore/materialStore.vue:608-634](</Users/teriri/WebstormProjects/1198_frontend/src/pages/materialStore/materialStore.vue:608>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/order/order.vue:1-64](</Users/teriri/WebstormProjects/1198_frontend/src/pages/order/order.vue:1>) | 未覆盖冻结目标 |
| 5 | indexOrderDateList | [src/api/index.js:146-152](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:146>) | 未覆盖冻结目标 |
| 6 | queryList | [src/pages/orderForm/orderForm.vue:184-327](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:184>) | 单片段覆盖某目标关键正文 |
| 7 | 文本块 | [src/pages/dishLibrary/dishLibrary.vue:56-222](</Users/teriri/WebstormProjects/1198_frontend/src/pages/dishLibrary/dishLibrary.vue:56>) | 未覆盖冻结目标 |
| 8 | indexOrderList | [src/api/index.js:197-203](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:197>) | 未覆盖冻结目标 |
| 9 | handleRecordPageChange | [src/pages/materialStore/materialStore.vue:704-725](</Users/teriri/WebstormProjects/1198_frontend/src/pages/materialStore/materialStore.vue:704>) | 未覆盖冻结目标 |
| 10 | queryList | [src/pages/purchaseFood/purchaseFood.vue:230-269](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:230>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

### 实验

查询耗时 3.901 秒；声明/关键正文/范围首次覆盖：5 / 5 / 5。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | loadInventoryRecordList | [src/pages/materialStore/materialStore.vue:573-602](</Users/teriri/WebstormProjects/1198_frontend/src/pages/materialStore/materialStore.vue:573>) | 未覆盖冻结目标 |
| 2 | queryList | [src/pages/order/order.vue:66-80](</Users/teriri/WebstormProjects/1198_frontend/src/pages/order/order.vue:66>) | 未覆盖冻结目标 |
| 3 | loadInventoryGoodsList | [src/pages/materialStore/materialStore.vue:608-634](</Users/teriri/WebstormProjects/1198_frontend/src/pages/materialStore/materialStore.vue:608>) | 未覆盖冻结目标 |
| 4 | indexOrderDateList | [src/api/index.js:146-152](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:146>) | 未覆盖冻结目标 |
| 5 | queryList | [src/pages/orderForm/orderForm.vue:184-327](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderForm/orderForm.vue:184>) | 单片段覆盖某目标关键正文 |
| 6 | 文本块 | [src/pages/dishLibrary/dishLibrary.vue:56-138](</Users/teriri/WebstormProjects/1198_frontend/src/pages/dishLibrary/dishLibrary.vue:56>) | 未覆盖冻结目标 |
| 7 | indexOrderList | [src/api/index.js:197-203](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:197>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/order/order.vue:1-26](</Users/teriri/WebstormProjects/1198_frontend/src/pages/order/order.vue:1>) | 未覆盖冻结目标 |
| 9 | handleRecordPageChange | [src/pages/materialStore/materialStore.vue:704-725](</Users/teriri/WebstormProjects/1198_frontend/src/pages/materialStore/materialStore.vue:704>) | 未覆盖冻结目标 |
| 10 | queryList | [src/pages/purchaseFood/purchaseFood.vue:230-269](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:230>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

## holdout-web-1198-06

`search_code("确认食谱订单时，怎样避免重复提交并把日期和食谱图片一起保存？")`

搜索语义：防止重复提交的食谱订单确认

项目：/Users/teriri/WebstormProjects/1198_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/confirmOrder/confirmOrder.vue:365](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:365>)；符号 popupConfirm；锚点 [365]；关键行 [366, 367, 374, 375, 376, 377, 380, 382, 387]。

人工审计：第二轮Top10依旧没有创建食谱订单popupConfirm365–389的防重复、start/end/image提交及finally释放状态。第2是编辑订单popupConfirm，图片分支没有实现，也不提交起止日期。原基线第3完整命中，第一轮未命中，恢复窗口仍未修复此真实遗漏。

### 基线

查询耗时 6.158 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleConfirmBatchAction | [src/pages/orderFormDetail/orderFormDetail.vue:813-986](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:813>) | 未覆盖冻结目标 |
| 2 | popupConfirm | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:325-431](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:325>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/pages/confirmOrder/confirmOrder.vue:164-396](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:164>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:162-265](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:162>) | 未覆盖冻结目标 |
| 5 | handleConfirm | [src/pages/purchaseFood/purchaseFood.vue:640-756](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:640>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1-110](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/confirmOrder/confirmOrder.vue:1-112](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:1>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/order/order.vue:1-56](</Users/teriri/WebstormProjects/1198_frontend/src/pages/order/order.vue:1>) | 未覆盖冻结目标 |
| 9 | subrecipeorder | [src/api/index.js:278-284](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:278>) | 未覆盖冻结目标 |
| 10 | handleConfirmStockOut | [src/pages/orderFormDetail/orderFormDetail.vue:1373-1489](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:1373>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675160965002000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

### 实验

查询耗时 4.632 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleConfirmBatchAction | [src/pages/orderFormDetail/orderFormDetail.vue:871-986](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:871>) | 未覆盖冻结目标 |
| 2 | popupConfirm | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:325-431](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:325>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/pages/confirmOrder/confirmOrder.vue:164-339](</Users/teriri/WebstormProjects/1198_frontend/src/pages/confirmOrder/confirmOrder.vue:164>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:162-265](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/confirmEditOrder/confirmEditOrder.vue:162>) | 未覆盖冻结目标 |
| 5 | handleConfirm | [src/pages/purchaseFood/purchaseFood.vue:640-756](</Users/teriri/WebstormProjects/1198_frontend/src/pages/purchaseFood/purchaseFood.vue:640>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/store/useJumpStore.js:5-244](</Users/teriri/WebstormProjects/1198_frontend/src/store/useJumpStore.js:5>) | 未覆盖冻结目标 |
| 7 | subrecipeorder | [src/api/index.js:278-284](</Users/teriri/WebstormProjects/1198_frontend/src/api/index.js:278>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/order/order.vue:1-26](</Users/teriri/WebstormProjects/1198_frontend/src/pages/order/order.vue:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/editOrder/cookbook/cookbook.vue:1-38](</Users/teriri/WebstormProjects/1198_frontend/src/pages/editOrder/cookbook/cookbook.vue:1>) | 未覆盖冻结目标 |
| 10 | handleConfirmStockOut | [src/pages/orderFormDetail/orderFormDetail.vue:1373-1489](</Users/teriri/WebstormProjects/1198_frontend/src/pages/orderFormDetail/orderFormDetail.vue:1373>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679065842726000-4116fda61c62-holdout-web-1198-01-attempt1.stderr>)

## holdout-web-1341-01

`search_code("提交员工请假")`

搜索语义：员工请假日期转换与申请创建

项目：/Users/teriri/WebstormProjects/1341_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/employee-leave/index.vue:105](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:105>)；符号 handleSubmit；锚点 [105]；关键行 [106, 115, 119, 122, 123, 126, 129, 137]。

人工审计：第二轮首条1–205重新覆盖handleSubmit105–142的日期拆分、bizLeaveRecordCreate和返回上一页；原基线也首条，第一轮第2才显示。说明语境窗口恢复了提交实现，而不是仅表单命中。

### 基线

查询耗时 6.817 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleSubmit | [src/pages/employee-leave/index.vue:1-204](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:1>) | 单片段覆盖某目标关键正文 |
| 2 | bizLeaveRecordCreate | [src/api/index.ts:72-78](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:72>) | 未覆盖冻结目标 |
| 3 | bizLeaveRecordUpdate | [src/api/index.ts:243-249](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:243>) | 未覆盖冻结目标 |
| 4 | bizLeaveRecordDelete | [src/api/index.ts:381-387](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:381>) | 未覆盖冻结目标 |
| 5 | bizLeaveRecordGet | [src/api/index.ts:543-549](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:543>) | 未覆盖冻结目标 |
| 6 | handleEmployeeLeave | [src/pages/my/index.vue:166-168](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my/index.vue:166>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/employee-leave/index.vue:205-258](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:205>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/employee-leave/index.vue:259-279](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:259>) | 仅目标文件 |
| 9 | 文本块 | [src/pages.json:61-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:61>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/my/index.vue:60-107](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my/index.vue:60>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

### 实验

查询耗时 3.588 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/employee-leave/index.vue:1-205](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:1>) | 单片段覆盖某目标关键正文 |
| 2 | bizLeaveRecordCreate | [src/api/index.ts:49-101](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:49>) | 未覆盖冻结目标 |
| 3 | bizLeaveRecordUpdate | [src/api/index.ts:227-272](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:227>) | 未覆盖冻结目标 |
| 4 | bizLeaveRecordDelete | [src/api/index.ts:358-410](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:358>) | 未覆盖冻结目标 |
| 5 | bizLeaveRecordGet | [src/api/index.ts:520-595](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:520>) | 未覆盖冻结目标 |
| 6 | handleEmployeeLeave | [src/pages/my/index.vue:166-168](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my/index.vue:166>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/employee-leave/index.vue:206-265](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:206>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/employee-leave/index.vue:266-279](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-leave/index.vue:266>) | 仅目标文件 |
| 9 | 文本块 | [src/pages/my/index.vue:77-110](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my/index.vue:77>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages.json:58-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:58>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

## holdout-web-1341-02

`search_code("发布用工任务时，哪里检查结束时间晚于开始时间，并区分新建和编辑？")`

搜索语义：用工任务时间校验与保存分支

项目：/Users/teriri/WebstormProjects/1341_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/publish/index.vue:238](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:238>)；符号 handleSubmit；锚点 [238]；关键行 [265, 266, 267, 284, 285, 291, 297, 306]。

### 基线

查询耗时 5.782 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleSubmit | [src/pages/publish/index.vue:228-341](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:228>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/publish/index.vue:1-169](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:1>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/schedule/index.vue:362-601](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:362>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/components/ScheduleGantt/index.vue:83-98](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:83>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/audit-detail/index.vue:137-151](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:137>) | 未覆盖冻结目标 |
| 6 | handleDateTimeConfirm | [src/pages/publish/index.vue:202-209](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:202>) | 仅目标文件 |
| 7 | 文本块 | [src/pages/schedule/index.vue:119-135](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:119>) | 未覆盖冻结目标 |
| 8 | getTaskStyle | [src/components/ScheduleGantt/index.vue:236-280](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:236>) | 未覆盖冻结目标 |
| 9 | AuditTask | [src/pages/audit/index.vue:88-99](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit/index.vue:88>) | 未覆盖冻结目标 |
| 10 | TaskDetail | [src/pages/task-detail/index.vue:113-131](</Users/teriri/WebstormProjects/1341_frontend/src/pages/task-detail/index.vue:113>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

### 实验

查询耗时 4.720 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleSubmit | [src/pages/publish/index.vue:228-308](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:228>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/publish/index.vue:1-169](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:1>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/schedule/index.vue:362-601](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:362>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/components/ScheduleGantt/index.vue:83-98](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:83>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/audit-detail/index.vue:137-151](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:137>) | 未覆盖冻结目标 |
| 6 | handleDateTimeConfirm | [src/pages/publish/index.vue:202-209](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:202>) | 仅目标文件 |
| 7 | 文本块 | [src/pages/schedule/index.vue:119-135](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:119>) | 未覆盖冻结目标 |
| 8 | getTaskStyle | [src/components/ScheduleGantt/index.vue:236-280](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:236>) | 未覆盖冻结目标 |
| 9 | AuditTask | [src/pages/audit/index.vue:88-99](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit/index.vue:88>) | 未覆盖冻结目标 |
| 10 | TaskDetail | [src/pages/task-detail/index.vue:113-131](</Users/teriri/WebstormProjects/1341_frontend/src/pages/task-detail/index.vue:113>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

## holdout-web-1341-03

`search_code("驳回用工申请")`

搜索语义：任务审核驳回与意见提交

项目：/Users/teriri/WebstormProjects/1341_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/audit-detail/index.vue:226](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:226>)；符号 handleReject；锚点 [226]；关键行 [227, 231, 233, 234, 235, 238, 244]。

### 基线

查询耗时 5.020 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | bizApplicationReview | [src/api/index.ts:986-992](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:986>) | 未覆盖冻结目标 |
| 2 | handleReject | [src/pages/audit-detail/index.vue:226-249](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:226>) | 单片段覆盖某目标关键正文 |
| 3 | ApplicationStatus | [src/pages/my-applications/index.vue:37-87](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my-applications/index.vue:37>) | 未覆盖冻结目标 |
| 4 | bizApplicationDelete | [src/api/index.ts:473-479](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:473>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/audit-detail/index.vue:1-173](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:1>) | 仅目标文件 |
| 6 | handleSubmitApplication | [src/pages/apply/index.vue:433-589](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:433>) | 未覆盖冻结目标 |
| 7 | handleApprove | [src/pages/audit-detail/index.vue:200-223](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:200>) | 仅目标文件 |
| 8 | 文本块 | [src/pages/my-applications/index.vue:101-111](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my-applications/index.vue:101>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/application-detail/index.vue:108-220](</Users/teriri/WebstormProjects/1341_frontend/src/pages/application-detail/index.vue:108>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/my-publish/index.vue:56-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my-publish/index.vue:56>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

### 实验

查询耗时 3.772 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | bizApplicationReview | [src/api/index.ts:969-1040](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:969>) | 未覆盖冻结目标 |
| 2 | handleReject | [src/pages/audit-detail/index.vue:226-249](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:226>) | 单片段覆盖某目标关键正文 |
| 3 | ApplicationStatus | [src/pages/my-applications/index.vue:81-87](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my-applications/index.vue:81>) | 未覆盖冻结目标 |
| 4 | bizApplicationDelete | [src/api/index.ts:434-479](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:434>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/audit-detail/index.vue:82-173](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:82>) | 仅目标文件 |
| 6 | handleApprove | [src/pages/audit-detail/index.vue:200-223](</Users/teriri/WebstormProjects/1341_frontend/src/pages/audit-detail/index.vue:200>) | 仅目标文件 |
| 7 | handleSubmitApplication | [src/pages/apply/index.vue:433-589](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:433>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages.json:58-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:58>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/application-detail/index.vue:108-220](</Users/teriri/WebstormProjects/1341_frontend/src/pages/application-detail/index.vue:108>) | 未覆盖冻结目标 |
| 10 | handleSubmit | [src/pages/publish/index.vue:238-308](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:238>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

## holdout-web-1341-04

`search_code("报名兼职任务时，个人资料不全的话在哪里先补齐，再提交报名？")`

搜索语义：补全个人资料后报名任务

项目：/Users/teriri/WebstormProjects/1341_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/apply/index.vue:504](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:504>)；符号 handleSubmitApplication；锚点 [504]；关键行 [507, 515, 516, 526, 529, 534, 535, 537, 548, 550, 556, 561]。

### 基线

查询耗时 5.942 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleSubmitApplication | [src/pages/apply/index.vue:373-589](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:373>) | 单片段覆盖某目标关键正文 |
| 2 | needToFillProfile | [src/pages/apply/index.vue:280-280](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:280>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/apply/index.vue:1-102](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:1>) | 仅目标文件 |
| 4 | isProfileComplete | [src/pages/apply/index.vue:283-293](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:283>) | 仅目标文件 |
| 5 | 文本块 | [src/pages.json:1-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/publish/index.vue:162-308](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:162>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/application-detail/index.vue:1-220](</Users/teriri/WebstormProjects/1341_frontend/src/pages/application-detail/index.vue:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [docs/backend-requirements.md:1-115](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [docs/backend-requirements.md:175-412](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:175>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/pages/my/index.vue:144-168](</Users/teriri/WebstormProjects/1341_frontend/src/pages/my/index.vue:144>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

### 实验

查询耗时 3.580 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | handleSubmitApplication | [src/pages/apply/index.vue:457-589](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:457>) | 单片段覆盖某目标关键正文 |
| 2 | needToFillProfile | [src/pages/apply/index.vue:280-280](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:280>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/apply/index.vue:1-38](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:1>) | 仅目标文件 |
| 4 | 文本块 | [src/pages/apply/index.vue:373-432](</Users/teriri/WebstormProjects/1341_frontend/src/pages/apply/index.vue:373>) | 仅目标文件 |
| 5 | 文本块 | [src/pages.json:1-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/publish/index.vue:228-308](</Users/teriri/WebstormProjects/1341_frontend/src/pages/publish/index.vue:228>) | 未覆盖冻结目标 |
| 7 | 文本块 | [docs/backend-requirements.md:359-418](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:359>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/application-detail/index.vue:79-220](</Users/teriri/WebstormProjects/1341_frontend/src/pages/application-detail/index.vue:79>) | 未覆盖冻结目标 |
| 9 | 文本块 | [docs/backend-requirements.md:1-45](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [docs/backend-requirements.md:134-313](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:134>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

## holdout-web-1341-05

`search_code("按员工姓名和日期查询工时")`

搜索语义：工时列表条件组装与分页

项目：/Users/teriri/WebstormProjects/1341_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/employee-work-hours/index.vue:153](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:153>)；符号 onLoadList；锚点 [153]；关键行 [156, 157, 158, 159, 162, 163, 166, 171, 172]。

人工审计：第二轮首条1–240重新包含onLoadList153–177的姓名/站点/日期参数和工时查询，恢复原基线首条，第一轮第2才包含实现。

### 基线

查询耗时 4.721 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/employee-work-hours/index.vue:1-235](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/employee-work-hours/index.vue:292-348](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:292>) | 仅目标文件 |
| 3 | bizLeaveRecordPage | [src/api/index.ts:778-784](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:778>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/pages/employee-work-hours/index.vue:236-291](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:236>) | 仅目标文件 |
| 5 | bizStatisticsWorkHours | [src/api/index.ts:1119-1125](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:1119>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/employee-work-hours/index.vue:349-377](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:349>) | 仅目标文件 |
| 7 | 文本块 | [src/pages.json:61-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:61>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/pages/order/index.vue:121-235](</Users/teriri/WebstormProjects/1341_frontend/src/pages/order/index.vue:121>) | 未覆盖冻结目标 |
| 9 | bizSchedulePage | [src/api/index.ts:838-844](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:838>) | 未覆盖冻结目标 |
| 10 | 文本块 | [docs/backend-requirements.md:413-465](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:413>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

### 实验

查询耗时 3.699 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/employee-work-hours/index.vue:1-240](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/pages/employee-work-hours/index.vue:301-359](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:301>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/employee-work-hours/index.vue:241-300](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:241>) | 仅目标文件 |
| 4 | bizLeaveRecordPage | [src/api/index.ts:718-784](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:718>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/api/index.ts:808-887](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:808>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages.json:58-113](</Users/teriri/WebstormProjects/1341_frontend/src/pages.json:58>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/pages/employee-work-hours/index.vue:360-377](</Users/teriri/WebstormProjects/1341_frontend/src/pages/employee-work-hours/index.vue:360>) | 仅目标文件 |
| 8 | bizStatisticsWorkHours | [src/api/index.ts:1119-1125](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:1119>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/pages/order/index.vue:121-176](</Users/teriri/WebstormProjects/1341_frontend/src/pages/order/index.vue:121>) | 未覆盖冻结目标 |
| 10 | 文本块 | [docs/backend-requirements.md:314-358](</Users/teriri/WebstormProjects/1341_frontend/docs/backend-requirements.md:314>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

## holdout-web-1341-06

`search_code("排班跨过午夜时，怎样拆到每天显示，并只保留当前查看范围内的部分？")`

搜索语义：跨天排班分片与可见日期裁剪

项目：/Users/teriri/WebstormProjects/1341_frontend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/pages/schedule/index.vue:362](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:362>)；符号 transformScheduleData；锚点 [362]；关键行 [396, 397, 398, 409, 421, 422, 425, 438, 439, 442, 443, 468, 486, 487, 498, 551, 552, 621, 622, 623, 628]。

### 基线

查询耗时 9.969 秒；声明/关键正文/范围首次覆盖：1 / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/schedule/index.vue:362-601](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:362>) | 覆盖声明锚点 |
| 2 | 文本块 | [src/pages/schedule/index.vue:1-116](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:1>) | 仅目标文件 |
| 3 | 文本块 | [src/pages/schedule/index.vue:119-135](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:119>) | 仅目标文件 |
| 4 | getMultiDayTimeDisplay | [src/components/ScheduleGantt/index.vue:213-222](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:213>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/components/ScheduleGantt/index.vue:1-169](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/pages/index/index.vue:180-255](</Users/teriri/WebstormProjects/1341_frontend/src/pages/index/index.vue:180>) | 未覆盖冻结目标 |
| 7 | headerLabels | [src/pages/schedule/index.vue:174-215](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:174>) | 仅目标文件 |
| 8 | 文本块 | [src/components/ScheduleGantt/index.vue:341-506](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:341>) | 未覆盖冻结目标 |
| 9 | getTaskStyle | [src/components/ScheduleGantt/index.vue:236-280](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:236>) | 未覆盖冻结目标 |
| 10 | bizScheduleDelete | [src/api/index.ts:404-410](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:404>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675196505449000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

### 实验

查询耗时 4.304 秒；声明/关键正文/范围首次覆盖：1 / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/pages/schedule/index.vue:362-601](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:362>) | 覆盖声明锚点 |
| 2 | getMultiDayTimeDisplay | [src/components/ScheduleGantt/index.vue:213-222](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:213>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/pages/schedule/index.vue:119-135](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:119>) | 仅目标文件 |
| 4 | 文本块 | [src/components/ScheduleGantt/index.vue:1-169](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/pages/index/index.vue:180-255](</Users/teriri/WebstormProjects/1341_frontend/src/pages/index/index.vue:180>) | 未覆盖冻结目标 |
| 6 | headerLabels | [src/pages/schedule/index.vue:174-215](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:174>) | 仅目标文件 |
| 7 | 文本块 | [src/pages/schedule/index.vue:602-624](</Users/teriri/WebstormProjects/1341_frontend/src/pages/schedule/index.vue:602>) | 仅目标文件 |
| 8 | 文本块 | [src/components/ScheduleGantt/index.vue:341-520](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:341>) | 未覆盖冻结目标 |
| 9 | getTaskStyle | [src/components/ScheduleGantt/index.vue:236-280](</Users/teriri/WebstormProjects/1341_frontend/src/components/ScheduleGantt/index.vue:236>) | 未覆盖冻结目标 |
| 10 | bizScheduleDelete | [src/api/index.ts:388-410](</Users/teriri/WebstormProjects/1341_frontend/src/api/index.ts:388>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679102156793000-879610de3c06-holdout-web-1341-01-attempt1.stderr>)

## holdout-idea-bid-review-app-01

`search_code("取消标书审查")`

搜索语义：定位取消尚未终止的审查任务、记录结束时间并保存状态的实现。

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:341](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:341>)；符号 cancelBidReviewTask；锚点 [341]；关键行 [342, 348, 352, 353, 356]。

人工审计：两侧第1都包含cancelBidReviewTask341–360，查询任务、拒绝终止态、写CANCELLED和结束时间并保存。正式AST恢复限定方法名并将340–433收窄至340–360，完整取消动作名次未变。

### 基线

查询耗时 10.469 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340-433](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>) | 单片段覆盖某目标关键正文 |
| 2 | bidReviewApi.cancelTask | [bidreview-app-ui/src/api/bid-review.ts:78-80](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:78>) | 未覆盖冻结目标 |
| 3 | cancelTask | [bidreview-app-ui/src/components/TaskList.vue:572-583](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskList.vue:572>) | 未覆盖冻结目标 |
| 4 | cancelTask | [bidreview-app-ui/src/components/TaskProgress.vue:370-386](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskProgress.vue:370>) | 未覆盖冻结目标 |
| 5 | cancelTask | [bidreview-app-ui/src/components/TaskDetailModal.vue:388-399](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskDetailModal.vue:388>) | 未覆盖冻结目标 |
| 6 | useBidReviewStore.cancelTask | [bidreview-app-ui/src/stores/bidReview.ts:135-149](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/stores/bidReview.ts:135>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:1-111](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/service/review/ReviewTaskManager.java:98-212](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/ReviewTaskManager.java:98>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868-925](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868>) | 仅目标文件 |
| 10 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/final-review-result.txt:1-47](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/final-review-result.txt:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

### 实验

查询耗时 5.054 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.review.impl.BidReviewServiceImpl.cancelBidReviewTask | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340-360](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>) | 单片段覆盖某目标关键正文 |
| 2 | com.kita.bidreview.controller.BidReviewController.cancelBidReviewTask | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:188-201](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:188>) | 未覆盖冻结目标 |
| 3 | com.kita.bidreview.service.review.ReviewTaskManager.cancelReview | [src/main/java/com/kita/bidreview/service/review/ReviewTaskManager.java:136-163](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/ReviewTaskManager.java:136>) | 未覆盖冻结目标 |
| 4 | bidReviewApi.cancelTask | [bidreview-app-ui/src/api/bid-review.ts:78-80](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:78>) | 未覆盖冻结目标 |
| 5 | cancelTask | [bidreview-app-ui/src/components/TaskList.vue:572-583](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskList.vue:572>) | 未覆盖冻结目标 |
| 6 | com.kita.bidreview.service.review.impl.BidReviewServiceImpl.deleteBidReviewTaskFromStorage | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:887-909](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:887>) | 仅目标文件 |
| 7 | com.kita.bidreview.service.review.impl.BidReviewServiceImpl.deleteBidReviewTask | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:362-394](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:362>) | 仅目标文件 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:69-98](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:69>) | 未覆盖冻结目标 |
| 9 | cancelTask | [bidreview-app-ui/src/components/TaskProgress.vue:370-386](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskProgress.vue:370>) | 未覆盖冻结目标 |
| 10 | cancelTask | [bidreview-app-ui/src/components/TaskDetailModal.vue:388-399](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskDetailModal.vue:388>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

## holdout-idea-bid-review-app-02

`search_code("创建审查任务前，怎样拒绝缺少招标文件或某份空投标文件，并限制上传格式？")`

搜索语义：查找新建审查的必填材料与扩展名校验完整流程。

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:136](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:136>)；符号 validateInputParameters；锚点 [136]；关键行 [146, 151, 156, 161, 164]。
- [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:171](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:171>)；符号 validateFileType；锚点 [171]；关键行 [172, 173, 177, 178, 179]。

人工审计：基线首条完整含validateInputParameters136–166和validateFileType171–183：逐份非空检查及扩展名白名单。正式Top10没有这两个服务方法。首条TestExtractController只检查单招标/投标文件再调用未返回的格式helper；第3控制器只委派创建，不展示逐份检查，不能完整回答所问的创建前校验。

### 基线

查询耗时 13.681 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:117-282](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:117>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:1-111](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [bidreview-app-ui/src/components/CreateTaskModal.vue:49-148](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/CreateTaskModal.vue:49>) | 未覆盖冻结目标 |
| 4 | 文本块 | [bidreview-app-ui/src/components/CreateTaskModal.vue:207-391](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/CreateTaskModal.vue:207>) | 未覆盖冻结目标 |
| 5 | bidReviewApi.createTask | [bidreview-app-ui/src/api/bid-review.ts:17-23](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:17>) | 未覆盖冻结目标 |
| 6 | 文本块 | [bidreview-app-ui/src/views/Dashboard.vue:57-108](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/views/Dashboard.vue:57>) | 未覆盖冻结目标 |
| 7 | bidReviewApi.createTask | [bidreview-app-ui/src/services/api.ts:82-97](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/services/api.ts:82>) | 未覆盖冻结目标 |
| 8 | 文本块 | [bidreview-app-ui/src/components/FileUpload.vue:416-530](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/FileUpload.vue:416>) | 未覆盖冻结目标 |
| 9 | 文本块 | [bidreview-app-ui/src/components/FileUpload.vue:251-305](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/FileUpload.vue:251>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:446-491](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:446>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

### 实验

查询耗时 9.107 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.controller.TestExtractController.extract | [src/main/java/com/kita/bidreview/controller/TestExtractController.java:28-62](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/TestExtractController.java:28>) | 未覆盖冻结目标 |
| 2 | 文本块 | [bidreview-app-ui/src/components/CreateTaskModal.vue:230-391](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/CreateTaskModal.vue:230>) | 未覆盖冻结目标 |
| 3 | com.kita.bidreview.controller.BidReviewController.createBidReviewTask | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:65-129](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:65>) | 未覆盖冻结目标 |
| 4 | bidReviewApi.createTask | [bidreview-app-ui/src/api/bid-review.ts:17-23](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:17>) | 未覆盖冻结目标 |
| 5 | 文本块 | [bidreview-app-ui/src/components/CreateTaskModal.vue:36-116](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/CreateTaskModal.vue:36>) | 未覆盖冻结目标 |
| 6 | com.kita.bidreview.service.review.BidReviewService | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:29-192](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:29>) | 未覆盖冻结目标 |
| 7 | bidReviewApi.createTask | [bidreview-app-ui/src/services/api.ts:82-97](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/services/api.ts:82>) | 未覆盖冻结目标 |
| 8 | 文本块 | [bidreview-app-ui/src/components/FileUpload.vue:410-530](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/FileUpload.vue:410>) | 未覆盖冻结目标 |
| 9 | 文本块 | [bidreview-app-ui/src/components/FileUpload.vue:251-309](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/FileUpload.vue:251>) | 未覆盖冻结目标 |
| 10 | 文本块 | [bidreview-app-ui/src/views/Dashboard.vue:61-92](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/views/Dashboard.vue:61>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

## holdout-idea-bid-review-app-03

`search_code("生成临时下载链接")`

搜索语义：查找对象存储按有效分钟数签发下载 URL 的实现。

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/bidreview/service/infra/impl/OssServiceImpl.java:213](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/impl/OssServiceImpl.java:213>)；符号 generatePresignedUrl；锚点 [213]；关键行 [215, 218, 219, 223, 228, 229]。

人工审计：两侧均未完整给出OssServiceImpl.generatePresignedUrl213–235。基线第2仅从227开始，有签名返回但无对象请求和有效期构造；正式首条只是接口声明，第二条控制器仍调用服务，不能当作完整实现。边界和符号更清晰不等于找到了签名生成正文。

### 基线

查询耗时 5.418 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/controller/OssController.java:181-262](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/OssController.java:181>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/impl/OssServiceImpl.java:227-282](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/impl/OssServiceImpl.java:227>) | 仅目标文件 |
| 3 | downloadContent | [bidreview-app-ui/src/components/ExtractedContentModal.vue:235-296](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ExtractedContentModal.vue:235>) | 未覆盖冻结目标 |
| 4 | downloadResponse | [bidreview-app-ui/src/components/LlmRawResponseModal.vue:235-247](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/LlmRawResponseModal.vue:235>) | 未覆盖冻结目标 |
| 5 | downloadLlmResponse | [bidreview-app-ui/src/components/ResultModal.vue:181-193](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ResultModal.vue:181>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/OssService.java:1-170](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/OssService.java:1>) | 未覆盖冻结目标 |
| 7 | downloadResult | [bidreview-app-ui/src/components/ResultModal.vue:195-223](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ResultModal.vue:195>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:549-608](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:549>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/controller/OssController.java:61-120](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/OssController.java:61>) | 未覆盖冻结目标 |
| 10 | 文本块 | [docs/OSS客户端使用说明.md:59-118](</Users/teriri/IdeaProjects/bid-review-app/docs/OSS客户端使用说明.md:59>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

### 实验

查询耗时 4.258 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.infra.OssService.generatePresignedUrl | [src/main/java/com/kita/bidreview/service/infra/OssService.java:95-103](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/OssService.java:95>) | 未覆盖冻结目标 |
| 2 | com.kita.bidreview.controller.OssController.generatePresignedUrl | [src/main/java/com/kita/bidreview/controller/OssController.java:188-207](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/OssController.java:188>) | 未覆盖冻结目标 |
| 3 | downloadContent | [bidreview-app-ui/src/components/ExtractedContentModal.vue:179-296](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ExtractedContentModal.vue:179>) | 未覆盖冻结目标 |
| 4 | downloadResponse | [bidreview-app-ui/src/components/LlmRawResponseModal.vue:235-247](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/LlmRawResponseModal.vue:235>) | 未覆盖冻结目标 |
| 5 | downloadLlmResponse | [bidreview-app-ui/src/components/ResultModal.vue:181-193](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ResultModal.vue:181>) | 未覆盖冻结目标 |
| 6 | downloadResult | [bidreview-app-ui/src/components/ResultModal.vue:195-223](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ResultModal.vue:195>) | 未覆盖冻结目标 |
| 7 | com.kita.bidreview.controller.OssController.downloadFile | [src/main/java/com/kita/bidreview/controller/OssController.java:98-119](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/OssController.java:98>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/controller/OssController.java:44-69](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/OssController.java:44>) | 未覆盖冻结目标 |
| 9 | 文本块 | [docs/OSS对象存储服务使用指南.md:171-175](</Users/teriri/IdeaProjects/bid-review-app/docs/OSS对象存储服务使用指南.md:171>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:188-197](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:188>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

## idea-bid-review-app-01-para-1

`search_code("长文档怎么分段，还让前后两段有些重叠？")`

搜索语义：定位长文档切分时长度、边界及重叠区间的具体计算

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-01；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98>)；符号 createChunks；锚点 [98]；关键行 [104, 108, 121, 126, 131]。

人工审计：冻结的TextChunkingService.createChunks从第1退至第3，但正式首条OverlapChunkingService.createOverlapChunks140–216同样按最大块长迭代，后续块从start-overlap开始并记录重叠长度，确实回答通用长文档重叠分段查询。基线首条标准实现与正式首条替代实现都可回答，不能把冻结1→3直接说成语义首位退化。

### 基线

查询耗时 5.989 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1-206](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1-225](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1-228](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455-528](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164-206](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:344-399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:344>) | 未覆盖冻结目标 |
| 7 | 文本块 | [uploads/bid-review/9701536e-ec65-4c23-830e-12463aea0d82/bid-full_01.txt:661-780](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/9701536e-ec65-4c23-830e-12463aea0d82/bid-full_01.txt:661>) | 未覆盖冻结目标 |
| 8 | getFileExtension | [bidreview-app-ui/src/utils/helpers.ts:247-249](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/utils/helpers.ts:247>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

### 实验

查询耗时 4.346 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.testextract.OverlapChunkingService.createOverlapChunks | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:140-216](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:140>) | 未覆盖冻结目标 |
| 2 | com.kita.bidreview.service.llm.impl.SmartTextChunker.performSmartChunking | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:173-214](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:173>) | 未覆盖冻结目标 |
| 3 | com.kita.bidreview.service.infra.TextChunkingService.createChunks | [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98-144](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98>) | 单片段覆盖某目标关键正文 |
| 4 | com.kita.bidreview.testextract.OverlapChunkingService.findOptimalEndPosition | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:458-486](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:458>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:28-172](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:28>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:215-291](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:215>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1-27](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) | 未覆盖冻结目标 |
| 8 | com.kita.bidreview.testextract.OverlapChunkingService.findOptimalStartPosition | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:491-514](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:491>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:31-139](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:31>) | 未覆盖冻结目标 |
| 10 | com.kita.bidreview.orchestrator.KeyInfoExtractionOrchestrator.buildChunkSystemPrompt | [src/main/java/com/kita/bidreview/orchestrator/KeyInfoExtractionOrchestrator.java:283-311](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/orchestrator/KeyInfoExtractionOrchestrator.java:283>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

## idea-bid-review-app-01-para-2

`search_code("处理篇幅过长的材料时，哪里将正文拆成若干块，并在相接的两块中重复保留一段文字？")`

搜索语义：定位长文档切分时长度、边界及重叠区间的具体计算

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-01；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98>)；符号 createChunks；锚点 [98]；关键行 [104, 108, 121, 126, 131]。

### 基线

查询耗时 5.937 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1-228](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1-226](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1-225](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [uploads/bid-review/a3a83331-cebb-4d67-a86d-05b8dc0dee9a/bid-contents_merged_20250918_162939.json:1-60](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/a3a83331-cebb-4d67-a86d-05b8dc0dee9a/bid-contents_merged_20250918_162939.json:1>) | 未覆盖冻结目标 |
| 5 | truncateText | [bidreview-app-ui/src/utils/helpers.ts:90-93](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/utils/helpers.ts:90>) | 未覆盖冻结目标 |
| 6 | 文本块 | [bidreview-app-ui/src/components/LlmRawResponseModal.vue:152-158](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/LlmRawResponseModal.vue:152>) | 未覆盖冻结目标 |
| 7 | 文本块 | [bidreview-app-ui/src/components/ExtractedContentModal.vue:356-370](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ExtractedContentModal.vue:356>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

### 实验

查询耗时 8.117 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1-32](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:93-100](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:93>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:82-89](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:82>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:33-81](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:33>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:85-93](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:85>) | 未覆盖冻结目标 |
| 6 | com.kita.bidreview.testextract.OverlapChunkingService.createOverlapChunks | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:140-216](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:140>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1-48](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1-35](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:49-84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:49>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:94-333](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:94>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

## idea-bid-review-app-03-para-1

`search_code("复查标书时，招标条件、初审结论和原文在哪一起给模型？")`

搜索语义：定位复审请求内容的组装，包含已有结论、原文件及可选指示

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-03；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90>)；符号 buildReReviewUserContent；锚点 [90]；关键行 [94, 95, 96, 100, 105]。

冻结答案组 secondary-review-content：活动调用链中的实际组装方法包含招标要求、首次判断和标书三项输入，以及模板渲染或委托冻结方法两个分支
- [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296>)；符号 buildSecondaryReviewUserContent；锚点 [296]；关键行 [296, 301, 302, 303, 306, 312]。

人工审计：buildReReviewUserContent把招标条件、初审结论和投标原文写入参数后渲染复审模板，基线第2、正式第1均完整显示90–106，真实模板组装实现前移。正式不再让招标材料正文先于代码。

### 基线

查询耗时 18.724 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:48-107](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:48>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:173-228](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:173>) | 仅目标文件 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288-314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>) | 单片段覆盖某目标关键正文 |
| 5 | 文本块 | [uploads/bid-review/9701536e-ec65-4c23-830e-12463aea0d82/tender-full.txt:241-300](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/9701536e-ec65-4c23-830e-12463aea0d82/tender-full.txt:241>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/model/bidreview/BidReviewConfig.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/BidReviewConfig.java:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/review/TemplateLoaderService.java:218-268](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/TemplateLoaderService.java:218>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/resources/template-config.yaml:1-24](</Users/teriri/IdeaProjects/bid-review-app/src/main/resources/template-config.yaml:1>) | 未覆盖冻结目标 |
| 9 | downloadResult | [bidreview-app-ui/src/components/ResultModal.vue:195-223](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ResultModal.vue:195>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/model/bidreview/DocumentType.java:1-34](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/DocumentType.java:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675235000386000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

### 实验

查询耗时 14.596 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.review.PromptTemplateService.buildReReviewUserContent | [src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:80-106](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:80>) | 单片段覆盖某目标关键正文 |
| 2 | com.kita.bidreview.service.review.impl.LlmReviewServiceImpl.buildLlmRequest | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:92-149](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:92>) | 仅目标文件 |
| 3 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) | 未覆盖冻结目标 |
| 4 | com.kita.bidreview.model.bidreview.BidReviewConfig | [src/main/java/com/kita/bidreview/model/bidreview/BidReviewConfig.java:1-80](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/BidReviewConfig.java:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [uploads/bid-review/9701536e-ec65-4c23-830e-12463aea0d82/tender-full.txt:241-300](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/9701536e-ec65-4c23-830e-12463aea0d82/tender-full.txt:241>) | 未覆盖冻结目标 |
| 6 | com.kita.bidreview.model.bidreview.BidReviewTask | [src/main/java/com/kita/bidreview/model/bidreview/BidReviewTask.java:1-104](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/BidReviewTask.java:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:212-271](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:212>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:591-606](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:591>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679128043693000-64e5e537c7b8-holdout-idea-bid-review-app-01-attempt1.stderr>)

## idea-bid-review-app-03-para-2

`search_code("对投标文件进行第二次核验时，哪里把采购要求、首轮评审意见以及原始投标文本合在一起提交给模型？")`

搜索语义：定位复审请求内容的组装，包含已有结论、原文件及可选指示

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-03；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90>)；符号 buildReReviewUserContent；锚点 [90]；关键行 [94, 95, 96, 100, 105]。

冻结答案组 secondary-review-content：活动调用链中的实际组装方法包含招标要求、首次判断和标书三项输入，以及模板渲染或委托冻结方法两个分支
- [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296>)；符号 buildSecondaryReviewUserContent；锚点 [296]；关键行 [296, 301, 302, 303, 306, 312]。

人工审计：完整复审内容组装buildSecondaryReviewUserContent296–313从第4退至第5，正式第6另有同样有效的PromptTemplateService实现。前几条为招标材料和buildLlmRequest片段92–110，后者未包含实际三项参数组装，所以此次完整答案名次退后一位。

### 基线

查询耗时 10.320 秒；声明/关键正文/范围首次覆盖：4 / 4 / 4。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) | 未覆盖冻结目标 |
| 2 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:632-751](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:632>) | 未覆盖冻结目标 |
| 3 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-requirements_merged_20250918_162708.json:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-requirements_merged_20250918_162708.json:530>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288-314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>) | 单片段覆盖某目标关键正文 |
| 5 | 文本块 | [src/main/resources/template-config.yaml:1-24](</Users/teriri/IdeaProjects/bid-review-app/src/main/resources/template-config.yaml:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/tender-requirements_merged_20250918_162640.json:212-271](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/tender-requirements_merged_20250918_162640.json:212>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

### 实验

查询耗时 9.526 秒；声明/关键正文/范围首次覆盖：5 / 5 / 5。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) | 未覆盖冻结目标 |
| 2 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:632-751](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:632>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:92-110](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:92>) | 仅目标文件 |
| 4 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-requirements_merged_20250918_162708.json:530-649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-requirements_merged_20250918_162708.json:530>) | 未覆盖冻结目标 |
| 5 | com.kita.bidreview.service.review.impl.LlmReviewServiceImpl.buildSecondaryReviewUserContent | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296-313](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296>) | 单片段覆盖某目标关键正文 |
| 6 | com.kita.bidreview.service.review.PromptTemplateService.buildReReviewUserContent | [src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90-106](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90>) | 单片段覆盖某目标关键正文 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:287-311](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:287>) | 未覆盖冻结目标 |
| 8 | com.kita.bidreview.executor.BidReviewTaskExecutor.AnalysisBundle | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:97-97](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:97>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

## idea-bid-review-app-04-para-1

`search_code("本地或网上的文档，在哪读出来转成审查文字？")`

搜索语义：定位文档来源识别、提取分支、失败校验和文字合并

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-04；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27>)；符号 extractTextContent；锚点 [27]；关键行 [31, 37, 40, 44, 48]。

人工审计：DocumentProcessingServiceImpl27–49判断URL/本地路径、本地打开FileInputStream、网络委托提取、检查状态并合并评审文字，基线第2到正式第6。正式首条TextExtractionService51–84只有调用getInputStreamFromSource；后者对FILE明确抛出不支持，第二条doExtract也强制URL，不是支持本地和网络两路的完整替代。

### 基线

查询耗时 6.028 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1-234](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:491-656](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:491>) | 未覆盖冻结目标 |
| 4 | 文本块 | [docs/text-extraction-guide.md:1-170](</Users/teriri/IdeaProjects/bid-review-app/docs/text-extraction-guide.md:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:1-176](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/model/document/DocumentInputType.java:1-16](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/DocumentInputType.java:1>) | 未覆盖冻结目标 |
| 7 | viewDocumentContent | [bidreview-app-ui/src/components/ExtractedContentModal.vue:245-247](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ExtractedContentModal.vue:245>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

### 实验

查询耗时 5.106 秒；声明/关键正文/范围首次覆盖：6 / 6 / 6。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.document.impl.TextExtractionService.extractTextFromDocument | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:51-84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:51>) | 未覆盖冻结目标 |
| 2 | com.kita.bidreview.service.infra.DocumentExtractionTaskService.doExtract | [src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:78-97](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:78>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:30-50](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:30>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:85-324](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:85>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:325-334](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:325>) | 未覆盖冻结目标 |
| 6 | com.kita.bidreview.service.document.impl.DocumentProcessingServiceImpl | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>) | 单片段覆盖某目标关键正文 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:205-218](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:205>) | 未覆盖冻结目标 |
| 8 | com.kita.bidreview.service.document.impl.MarkdownExtractionService.convertElementToMarkdown | [src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:309-355](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:309>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:357-381](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:357>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:128-172](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:128>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

## idea-bid-review-app-04-para-2

`search_code("输入磁盘上的文件位置或者网络文档地址后，哪里载入对应材料并抽出用于评审的文本内容？")`

搜索语义：定位文档来源识别、提取分支、失败校验和文字合并

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-04；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27>)；符号 extractTextContent；锚点 [27]；关键行 [31, 37, 40, 44, 48]。

人工审计：第二轮仍未返回DocumentProcessingServiceImpl27–49的本地路径/URL分流实现。首条doExtract写死URL，第7只给extractTextFromDocument；源码getInputStreamFromSource对FILE直接报错，而它在第二轮也未返回。第4loadDocumentContentFromFile是直接读取文本，不是根据两类输入解析文档；新第9是使用指南，不能代替实现。原基线第3完整命中，第二轮与第一轮均未完整覆盖。

### 基线

查询耗时 6.911 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/model/document/DocumentInput.java:1-38](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/DocumentInput.java:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181-234](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1-99](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1-54](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1-30](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/model/document/DocumentInputType.java:1-16](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/DocumentInputType.java:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [docs/text-extraction-guide.md:1-113](</Users/teriri/IdeaProjects/bid-review-app/docs/text-extraction-guide.md:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/chunks/tender-requirements/chunk_001_162708.json:1-60](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/chunks/tender-requirements/chunk_001_162708.json:1>) | 未覆盖冻结目标 |
| 10 | viewDocumentContent | [bidreview-app-ui/src/components/ExtractedContentModal.vue:245-247](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ExtractedContentModal.vue:245>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

### 实验

查询耗时 5.502 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.infra.DocumentExtractionTaskService.doExtract | [src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:78-97](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:78>) | 未覆盖冻结目标 |
| 2 | com.kita.bidreview.model.bidreview.DocumentInfo | [src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1-61](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1>) | 未覆盖冻结目标 |
| 3 | com.kita.bidreview.controller.DocumentExtractionController.extractByUrl | [src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:33-53](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:33>) | 未覆盖冻结目标 |
| 4 | com.kita.bidreview.controller.BidReviewController.loadDocumentContentFromFile | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:395-403](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:395>) | 未覆盖冻结目标 |
| 5 | com.kita.bidreview.service.document.DocumentProcessingService.extractTextContent | [src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:21-21](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:21>) | 未覆盖冻结目标 |
| 6 | com.kita.bidreview.model.document.DocumentInput | [src/main/java/com/kita/bidreview/model/document/DocumentInput.java:10-38](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/DocumentInput.java:10>) | 未覆盖冻结目标 |
| 7 | com.kita.bidreview.service.document.impl.TextExtractionService.extractTextFromDocument | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:51-84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:51>) | 未覆盖冻结目标 |
| 8 | com.kita.bidreview.executor.BidReviewTaskExecutor.extractDocuments | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:405-454](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:405>) | 未覆盖冻结目标 |
| 9 | 文本块 | [docs/text-extraction-guide.md:1-115](</Users/teriri/IdeaProjects/bid-review-app/docs/text-extraction-guide.md:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:30-50](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:30>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

## idea-bid-review-app-05-para-1

`search_code("图片识字失败后，在哪依次换别家可用服务再试？")`

搜索语义：定位 OCR 提供商失败后的筛选、优先级切换和全部失败处理

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-05；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330>)；符号 tryFallbackStrategy；锚点 [330]；关键行 [336, 337, 338, 355, 371]。

人工审计：冻结的带启用过滤、优先级排序和字节流重建的OcrServiceImpl330–373由未完整覆盖变为第2，边界补齐是真实收获。但短查询只要求依次换别家可用服务，基线第1的OcrStrategyManager234–250已经有排除失败策略、遍历其他策略并捕获失败继续尝试的真实算法，正式第1也返回它。因此不宜把冻结未命中→第2直接当成从无法回答变为可回答。

### 基线

查询耗时 10.193 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61-290](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:343-442](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:343>) | 仅目标文件 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1-60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:113-286](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:113>) | 仅目标文件 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:111-216](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:111>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1-84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:1-54](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

### 实验

查询耗时 11.505 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.ocr.OcrStrategyManager.tryFallbackStrategy | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:234-250](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:234>) | 未覆盖冻结目标 |
| 2 | com.kita.bidreview.service.ocr.impl.OcrServiceImpl.tryFallbackStrategy | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330-373](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330>) | 单片段覆盖某目标关键正文 |
| 3 | com.kita.bidreview.service.ocr.OcrStrategyManager.recognize | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:89-178](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:89>) | 未覆盖冻结目标 |
| 4 | com.kita.bidreview.service.ocr.OcrService | [src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1-145](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:126-159](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:126>) | 仅目标文件 |
| 6 | com.kita.bidreview.service.document.impl.DefaultImageConverterService.trySaveFailedInputForDebug | [src/main/java/com/kita/bidreview/service/document/impl/DefaultImageConverterService.java:113-139](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DefaultImageConverterService.java:113>) | 未覆盖冻结目标 |
| 7 | com.kita.bidreview.controller.SystemController.getOcrFailuresByProvider | [src/main/java/com/kita/bidreview/controller/SystemController.java:101-126](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/SystemController.java:101>) | 未覆盖冻结目标 |
| 8 | com.kita.bidreview.service.ocr.OcrStrategy | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:25-84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:25>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/DefaultImageConverterService.java:37-86](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DefaultImageConverterService.java:37>) | 未覆盖冻结目标 |
| 10 | com.kita.bidreview.controller.SystemController.getOcrFailuresByTask | [src/main/java/com/kita/bidreview/controller/SystemController.java:131-154](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/SystemController.java:131>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

## idea-bid-review-app-05-para-2

`search_code("从图片提取文字的服务出错时，哪段逻辑会按既定次序切换到其他可用供应商继续识别？")`

搜索语义：定位 OCR 提供商失败后的筛选、优先级切换和全部失败处理

项目：/Users/teriri/IdeaProjects/bid-review-app；样本组：paraphrase。

业务族：idea-bid-review-app-05；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330>)；符号 tryFallbackStrategy；锚点 [330]；关键行 [336, 337, 338, 355, 371]。

人工审计：问法明确要求按既定次序切换可用供应商，OcrServiceImpl330–373的启用筛选与getProviderPriority排序、逐项识别、捕获后继续循环是完整实现。基线第1包含，正式第2才有。正式第1recognize只选择当前策略并调用fallback，未展示所问优先顺序算法。

### 基线

查询耗时 7.434 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228-399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:175-316](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:175>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61-114](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:113-168](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:113>) | 仅目标文件 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:1-216](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1-120](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/bidreview/service/document/ImageConverterService.java:1-37](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/ImageConverterService.java:1>) | 未覆盖冻结目标 |
| 8 | getStatusText.statusMap | [bidreview-app-ui/src/utils/helpers.ts:12-23](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/utils/helpers.ts:12>) | 未覆盖冻结目标 |
| 9 | 文本块 | [bidreview-app-ui/src/types/index.ts:18-22](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/types/index.ts:18>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675295712026000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

### 实验

查询耗时 6.470 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.bidreview.service.ocr.impl.OcrServiceImpl.recognize | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:126-159](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:126>) | 仅目标文件 |
| 2 | com.kita.bidreview.service.ocr.impl.OcrServiceImpl.tryFallbackStrategy | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330-373](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330>) | 单片段覆盖某目标关键正文 |
| 3 | com.kita.bidreview.service.ocr.OcrStrategyManager.recognize | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:89-178](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:89>) | 未覆盖冻结目标 |
| 4 | com.kita.bidreview.service.ocr.impl.OcrServiceImpl.selectStrategy | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:244-277](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:244>) | 仅目标文件 |
| 5 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:35-88](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:35>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:179-316](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:179>) | 未覆盖冻结目标 |
| 7 | com.kita.bidreview.service.ocr.OcrService | [src/main/java/com/kita/bidreview/service/ocr/OcrService.java:29-145](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrService.java:29>) | 未覆盖冻结目标 |
| 8 | com.kita.bidreview.service.document.impl.TextExtractionService.extractTextContentsWithOrder | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:281-316](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:281>) | 未覆盖冻结目标 |
| 9 | com.kita.bidreview.service.ocr.impl.ZhipuAiOcrStrategy.executeOcrWithKey | [src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:135-208](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:135>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:37-134](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/ZhipuAiOcrStrategy.java:37>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679176747832000-64e5e537c7b8-idea-bid-review-app-03-para-2-attempt1.stderr>)

## holdout-idea-teriri-api-hub-01

`search_code("长时间没用的朗读连接怎么回收？")`

搜索语义：定位语音合成闲置超时判断及关闭连接、移除记录的完整链路。

项目：/Users/teriri/IdeaProjects/teriri-api-hub；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:109](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:109>)；符号 cleanupExpiredSessions；锚点 [109]；关键行 [110, 112, 113, 115]。
- [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:97](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:97>)；符号 releaseSession；锚点 [97]；关键行 [98, 99, 101]。

人工审计：第二轮第10首次返回TTSSessionPool.cleanupExpiredSessions109–118，已可见最后活动时间超时判定，比第一轮只有手动close/SSE超时更接近问题。但被调releaseSession97–104的移除活动集合与关闭逻辑仍不在Top10，冻结完整链路两侧仍未覆盖；必须区分新增部分正确结果和完整答案命中。

### 基线

查询耗时 6.779 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:461-616](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:461>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:1-102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:32-247](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:32>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151-295](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) | 未覆盖冻结目标 |
| 5 | send | [llm-tester/src/App.vue:1-168](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:304-400](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:304>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1-70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [package.json:1-5](</Users/teriri/IdeaProjects/teriri-api-hub/package.json:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [llm-tester/src/main.js:1-5](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/main.js:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675337111916000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675337111916000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stderr>)

### 实验

查询耗时 4.513 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.teriri.teririapihub.service.TTSService.createEmitter | [src/main/java/com/teriri/teririapihub/service/TTSService.java:210-246](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:210>) | 未覆盖冻结目标 |
| 2 | com.teriri.teririapihub.client.TTSClient.close | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:559-593](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:559>) | 未覆盖冻结目标 |
| 3 | 文本块 | [llm-tester/src/App.vue:55-123](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:446-465](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:446>) | 未覆盖冻结目标 |
| 5 | com.teriri.teririapihub.client.TTSClient.connectToTTS | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:173-234](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:173>) | 未覆盖冻结目标 |
| 6 | com.teriri.teririapihub.client.TTSClient.connectLock | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:106-106](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:106>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:1-25](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [llm-tester/src/App.vue:545-592](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:545>) | 未覆盖冻结目标 |
| 9 | cleanUpAsrWebSocket | [llm-tester/src/App.vue:492-507](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:492>) | 未覆盖冻结目标 |
| 10 | com.teriri.teririapihub.pool.TTSSessionPool.cleanupExpiredSessions | [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:109-118](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:109>) | 单片段覆盖某目标关键正文 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679215968123000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679215968123000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stderr>)

## holdout-idea-teriri-api-hub-02

`search_code("浏览器开始识音前，怎样按传入的采样率和声道建立会话并通知它已经就绪？")`

搜索语义：查找实时语音识别的准备过程，包括音频参数、客户端登记及就绪响应。

项目：/Users/teriri/IdeaProjects/teriri-api-hub；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:97](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:97>)；符号 handlePrepareAsr；锚点 [97]；关键行 [103, 105, 109, 114, 116, 120, 125]。

人工审计：两侧首条都完整含handlePrepareAsr97–132：读取sampleRate/channels等音频参数，创建客户端、配置流、记录会话并发asr_prepared消息。正式从整文件1–235收窄为具名方法97–132，准确率名次不变。

### 基线

查询耗时 8.335 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1-235](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1>) | 单片段覆盖某目标关键正文 |
| 2 | startRecordingAndAsr | [llm-tester/src/App.vue:169-344](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:169>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1-111](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) | 未覆盖冻结目标 |
| 4 | connectAsrWebSocket | [llm-tester/src/App.vue:392-491](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:392>) | 未覆盖冻结目标 |
| 5 | 文本块 | [llm-tester/src/App.vue:616-712](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:616>) | 未覆盖冻结目标 |
| 6 | 文本块 | [llm-tester/src/App.vue:154-155](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:154>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675337111916000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675337111916000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stderr>)

### 实验

查询耗时 4.149 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.teriri.teririapihub.handlers.ASRWebSocketHandler.handlePrepareAsr | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:97-132](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:97>) | 单片段覆盖某目标关键正文 |
| 2 | startRecordingAndAsr | [llm-tester/src/App.vue:196-390](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:196>) | 未覆盖冻结目标 |
| 3 | com.teriri.teririapihub.client.ASRClient.prepareStreamForAppend | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:81-131](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:81>) | 未覆盖冻结目标 |
| 4 | connectAsrWebSocket | [llm-tester/src/App.vue:412-491](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:412>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:77-158](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:77>) | 未覆盖冻结目标 |
| 6 | com.teriri.teririapihub.client.ASRClient.sendFullClientRequest | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:358-375](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:358>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:236-260](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:236>) | 未覆盖冻结目标 |
| 8 | com.teriri.teririapihub.handlers.ASRWebSocketHandler.handleStartAsr | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:134-156](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:134>) | 仅目标文件 |
| 9 | 文本块 | [llm-tester/src/App.vue:593-697](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:593>) | 未覆盖冻结目标 |
| 10 | com.teriri.teririapihub.utils.ASRProtocolParser.buildFullClientRequest | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:160-205](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:160>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679215968123000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679215968123000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stderr>)

## holdout-idea-teriri-api-hub-03

`search_code("系统提示词在哪里写入模型的新会话？")`

搜索语义：定位模型上下文创建请求内的系统角色消息及默认提示选择。

项目：/Users/teriri/IdeaProjects/teriri-api-hub；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/teriri/teririapihub/client/LLMClient.java:112](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:112>)；符号 initContext；锚点 [112]；关键行 [117, 121, 124, 125, 131, 132]。

人工审计：两侧第1都完整显示initContext112–134：选择传入或默认prompt、构造SYSTEM消息并调用createContext返回会话ID。正式恢复限定名和较小范围，业务实现覆盖仍是第1。

### 基线

查询耗时 6.198 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:1-228](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1-16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:1-199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>) | 未覆盖冻结目标 |
| 4 | startRecordingAndAsr | [llm-tester/src/App.vue:196-285](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:196>) | 未覆盖冻结目标 |
| 5 | 文本块 | [llm-tester/src/App.vue:55-153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) | 未覆盖冻结目标 |
| 6 | sendAsrWebSocketMessage.messagePayload | [llm-tester/src/App.vue:183-187](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:183>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/ChatController.java:1-32](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/ChatController.java:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1-7](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [llm-tester/src/App.vue:1-41](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/teriri/teririapihub/event/llm/LLMCompletionEvent.java:1-22](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/event/llm/LLMCompletionEvent.java:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675337111916000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675337111916000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stderr>)

### 实验

查询耗时 4.622 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.teriri.teririapihub.client.LLMClient.initContext | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:91-134](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:91>) | 单片段覆盖某目标关键正文 |
| 2 | com.teriri.teririapihub.model.bo.ChatRequest | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:7-16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:7>) | 未覆盖冻结目标 |
| 3 | com.teriri.teririapihub.client.LLMClient.defaultPrompt | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:51-52](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:51>) | 仅目标文件 |
| 4 | 文本块 | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:140-216](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:140>) | 仅目标文件 |
| 5 | com.teriri.teririapihub.service.ChatService.chatWithLLM | [src/main/java/com/teriri/teririapihub/service/ChatService.java:49-70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:49>) | 未覆盖冻结目标 |
| 6 | com.teriri.teririapihub.client.LLMClient.contextId | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:78-78](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:78>) | 仅目标文件 |
| 7 | startRecordingAndAsr | [llm-tester/src/App.vue:196-261](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:196>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/ChatController.java:1-32](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/ChatController.java:1>) | 未覆盖冻结目标 |
| 9 | sendAsrWebSocketMessage.messagePayload | [llm-tester/src/App.vue:183-187](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:183>) | 未覆盖冻结目标 |
| 10 | com.teriri.teririapihub.model.vo.LLMTextVo.chatId | [src/main/java/com/teriri/teririapihub/model/vo/LLMTextVo.java:16-16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/vo/LLMTextVo.java:16>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679215968123000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679215968123000-7779a25c5907-holdout-idea-teriri-api-hub-01-attempt1.stderr>)

## holdout-idea-1604-apps-01

`search_code("新增车辆时，重复的车牌和已经停用的司机在哪里被拦住？")`

搜索语义：查找服务端新增车辆校验并落库的实现。

项目：/Users/teriri/IdeaProjects/1604-apps；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [backend/src/services/vehicle.service.ts:312](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/vehicle.service.ts:312>)；符号 create；锚点 [312]；关键行 [315, 322, 324, 329, 331, 332]。

### 基线

查询耗时 6.115 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | VehicleService.create | [backend/src/services/vehicle.service.ts:312-345](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/vehicle.service.ts:312>) | 单片段覆盖某目标关键正文 |
| 2 | VehicleService.update | [backend/src/services/vehicle.service.ts:354-395](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/vehicle.service.ts:354>) | 仅目标文件 |
| 3 | vehicleRoutes | [backend/src/routes/vehicle.ts:39-104](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/vehicle.ts:39>) | 未覆盖冻结目标 |
| 4 | VehiclePayload | [frontend/src/types/vehicle.ts:75-88](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/vehicle.ts:75>) | 未覆盖冻结目标 |
| 5 | 文本块 | [backend/drizzle/0004_vehicles.sql:1-54](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0004_vehicles.sql:1>) | 未覆盖冻结目标 |
| 6 | VehicleInput | [backend/src/services/vehicle.service.ts:36-53](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/vehicle.service.ts:36>) | 仅目标文件 |
| 7 | vehicles | [backend/src/db/schema/vehicles.ts:17-36](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/vehicles.ts:17>) | 未覆盖冻结目标 |
| 8 | VehicleFormProps.vehicle | [frontend/src/views/Vehicle/components/VehicleForm.vue:40-40](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/components/VehicleForm.vue:40>) | 未覆盖冻结目标 |
| 9 | 文本块 | [frontend/src/views/Vehicle/components/VehicleForm.vue:51-168](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/components/VehicleForm.vue:51>) | 未覆盖冻结目标 |
| 10 | 文本块 | [frontend/src/views/Vehicle/index.vue:170-334](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/index.vue:170>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

### 实验

查询耗时 3.524 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | VehicleService.create | [backend/src/services/vehicle.service.ts:205-415](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/vehicle.service.ts:205>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [backend/src/routes/vehicle.ts:39-104](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/vehicle.ts:39>) | 未覆盖冻结目标 |
| 3 | VehiclePayload | [frontend/src/types/vehicle.ts:75-88](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/vehicle.ts:75>) | 未覆盖冻结目标 |
| 4 | 文本块 | [backend/drizzle/0004_vehicles.sql:1-19](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0004_vehicles.sql:1>) | 未覆盖冻结目标 |
| 5 | VehicleInput | [backend/src/services/vehicle.service.ts:36-53](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/vehicle.service.ts:36>) | 仅目标文件 |
| 6 | vehicles | [backend/src/db/schema/vehicles.ts:17-36](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/vehicles.ts:17>) | 未覆盖冻结目标 |
| 7 | 文本块 | [frontend/src/views/Vehicle/index.vue:170-217](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/index.vue:170>) | 未覆盖冻结目标 |
| 8 | VehicleFormProps.vehicle | [frontend/src/views/Vehicle/components/VehicleForm.vue:40-40](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/components/VehicleForm.vue:40>) | 未覆盖冻结目标 |
| 9 | 文本块 | [frontend/src/views/Vehicle/components/VehicleForm.vue:51-168](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/components/VehicleForm.vue:51>) | 未覆盖冻结目标 |
| 10 | 文本块 | [frontend/src/views/Vehicle/components/VehicleDeleteDialog.vue:60-87](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Vehicle/components/VehicleDeleteDialog.vue:60>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

## holdout-idea-1604-apps-02

`search_code("库存页面删除一条手工登记记录时，怎么防止库存算成负数并留下操作日志？")`

搜索语义：定位人工库存流水删除边界、负库存校验及日志保存。

项目：/Users/teriri/IdeaProjects/1604-apps；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [backend/src/services/iceInventory.service.ts:918](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:918>)；符号 delete；锚点 [918]；关键行 [931, 935, 936, 938, 942, 951]。

### 基线

查询耗时 7.598 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | IceInventoryService.delete | [backend/src/services/iceInventory.service.ts:918-954](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:918>) | 单片段覆盖某目标关键正文 |
| 2 | deleteIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:552-583](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:552>) | 仅目标文件 |
| 3 | IceBagService.delete | [backend/src/services/iceBag.service.ts:854-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceBag.service.ts:854>) | 未覆盖冻结目标 |
| 4 | handleDelete | [frontend/src/views/IceInventory/index.vue:195-207](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/index.vue:195>) | 未覆盖冻结目标 |
| 5 | StockOutService.delete | [backend/src/services/delivery.service.ts:987-1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:987>) | 未覆盖冻结目标 |
| 6 | createIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:442-482](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:442>) | 仅目标文件 |
| 7 | handleDelete | [frontend/src/views/StockOut/index.vue:267-279](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/StockOut/index.vue:267>) | 未覆盖冻结目标 |
| 8 | updateIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:491-544](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:491>) | 仅目标文件 |
| 9 | 文本块 | [frontend/src/views/IceInventory/components/IceInventoryDeleteDialog.vue:60-87](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/components/IceInventoryDeleteDialog.vue:60>) | 未覆盖冻结目标 |
| 10 | IceInventoryRecord | [frontend/src/types/iceInventory.ts:21-82](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/iceInventory.ts:21>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

### 实验

查询耗时 5.191 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | IceInventoryService.delete | [backend/src/services/iceInventory.service.ts:838-954](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:838>) | 单片段覆盖某目标关键正文 |
| 2 | deleteIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:442-583](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:442>) | 仅目标文件 |
| 3 | IceBagService.delete | [backend/src/services/iceBag.service.ts:846-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceBag.service.ts:846>) | 未覆盖冻结目标 |
| 4 | handleDelete | [frontend/src/views/IceInventory/index.vue:195-207](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/index.vue:195>) | 未覆盖冻结目标 |
| 5 | StockOutService.delete | [backend/src/services/delivery.service.ts:987-1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:987>) | 未覆盖冻结目标 |
| 6 | handleDelete | [frontend/src/views/StockOut/index.vue:267-279](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/StockOut/index.vue:267>) | 未覆盖冻结目标 |
| 7 | 文本块 | [frontend/src/views/IceInventory/components/IceInventoryDeleteDialog.vue:60-87](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/components/IceInventoryDeleteDialog.vue:60>) | 未覆盖冻结目标 |
| 8 | IceInventoryRecord | [frontend/src/types/iceInventory.ts:21-82](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/iceInventory.ts:21>) | 未覆盖冻结目标 |
| 9 | IceInventoryService.create | [backend/src/services/iceInventory.service.ts:775-828](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:775>) | 仅目标文件 |
| 10 | 文本块 | [backend/drizzle/0006_ice_inventory.sql:45-55](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0006_ice_inventory.sql:45>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

## holdout-idea-1604-apps-03

`search_code("月报怎么判断二月该查到哪天？")`

搜索语义：定位月份格式验证和月报起止日期计算，包含不同年份月末处理。

项目：/Users/teriri/IdeaProjects/1604-apps；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [backend/src/services/report.service.ts:166](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:166>)；符号 resolveMonthlyReportDateRange；锚点 [166]；关键行 [167, 169, 175, 178, 179]。

### 基线

查询耗时 3.328 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | resolveMonthlyReportDateRange | [backend/src/services/report.service.ts:166-181](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:166>) | 单片段覆盖某目标关键正文 |
| 2 | MonthlyReportDateRange | [backend/src/services/report.service.ts:80-85](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:80>) | 仅目标文件 |
| 3 | MonthlySalesReportOptions | [backend/src/services/report.service.ts:58-75](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:58>) | 仅目标文件 |
| 4 | MonthlySalesReportFilters | [frontend/src/types/report.ts:221-228](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/report.ts:221>) | 未覆盖冻结目标 |
| 5 | 文本块 | [frontend/src/views/MonthlyReport/index.vue:69-86](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/MonthlyReport/index.vue:69>) | 未覆盖冻结目标 |
| 6 | MonthlySalesReport | [frontend/src/types/report.ts:281-290](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/report.ts:281>) | 未覆盖冻结目标 |
| 7 | 文本块 | [frontend/src/views/MonthlyReport/index.vue:99-328](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/MonthlyReport/index.vue:99>) | 未覆盖冻结目标 |
| 8 | resolveCurrentMonth | [frontend/src/views/MonthlyReport/index.vue:29-44](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/MonthlyReport/index.vue:29>) | 未覆盖冻结目标 |
| 9 | MonthlyReportCustomerType | [frontend/src/types/report.ts:216-216](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/report.ts:216>) | 未覆盖冻结目标 |
| 10 | monthlySalesReportQuerySchema | [backend/src/routes/report.ts:25-29](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/report.ts:25>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

### 实验

查询耗时 6.714 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | resolveMonthlyReportDateRange | [backend/src/services/report.service.ts:158-181](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:158>) | 单片段覆盖某目标关键正文 |
| 2 | MonthlyReportDateRange | [backend/src/services/report.service.ts:80-85](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:80>) | 仅目标文件 |
| 3 | MonthlySalesReportOptions | [backend/src/services/report.service.ts:58-75](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:58>) | 仅目标文件 |
| 4 | MonthlySalesReportFilters | [frontend/src/types/report.ts:221-228](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/report.ts:221>) | 未覆盖冻结目标 |
| 5 | loadReport | [frontend/src/views/MonthlyReport/index.vue:69-86](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/MonthlyReport/index.vue:69>) | 未覆盖冻结目标 |
| 6 | 文本块 | [backend/src/services/report.service.ts:488-585](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:488>) | 仅目标文件 |
| 7 | MonthlySalesReport | [frontend/src/types/report.ts:281-290](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/report.ts:281>) | 未覆盖冻结目标 |
| 8 | MonthlyReportCustomerType | [frontend/src/types/report.ts:216-216](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/report.ts:216>) | 未覆盖冻结目标 |
| 9 | resolveCurrentMonth | [frontend/src/views/MonthlyReport/index.vue:29-44](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/MonthlyReport/index.vue:29>) | 未覆盖冻结目标 |
| 10 | monthlySalesReportQuerySchema | [backend/src/routes/report.ts:25-29](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/report.ts:25>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

## idea-1604-apps-01-para-1

`search_code("司机登记发货怎么扣冰粒库存，扣失败又怎么撤回记录？")`

搜索语义：定位发货创建与库存流水联动及失败补偿

项目：/Users/teriri/IdeaProjects/1604-apps；样本组：paraphrase。

业务族：idea-1604-apps-01；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [backend/src/services/delivery.service.ts:818](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>)；符号 create；锚点 [818]；关键行 [837, 853, 865, 871, 873]。

人工审计：第二轮首条818–884完整含新建出库、outbound库存流水、失败删除记录及成功回写日志，恢复到原基线同范围。第一轮也是首位完整，但返回到1018；本轮只是减少相邻内容，完整答案排名不变。

### 基线

查询耗时 8.350 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | StockOutService.create | [backend/src/services/delivery.service.ts:818-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) | 单片段覆盖某目标关键正文 |
| 2 | createIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:442-482](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:442>) | 未覆盖冻结目标 |
| 3 | stockOutRecords | [backend/src/db/schema/delivery.ts:21-58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:21>) | 未覆盖冻结目标 |
| 4 | iceInventoryRoutes | [backend/src/routes/iceInventory.ts:45-120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:45>) | 未覆盖冻结目标 |
| 5 | deleteIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:552-583](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:552>) | 未覆盖冻结目标 |
| 6 | updateIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:491-544](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:491>) | 未覆盖冻结目标 |
| 7 | StockOutService.delete | [backend/src/services/delivery.service.ts:987-1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:987>) | 仅目标文件 |
| 8 | ReturnRecordService.create | [backend/src/services/delivery.service.ts:1126-1192](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:1126>) | 仅目标文件 |
| 9 | returnRecords | [backend/src/db/schema/delivery.ts:63-100](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:63>) | 未覆盖冻结目标 |
| 10 | createIceInventorySourceTransaction | [backend/src/services/iceInventory.service.ts:426-434](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:426>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

### 实验

查询耗时 11.188 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [backend/src/services/delivery.service.ts:818-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [backend/src/db/schema/delivery.ts:1-58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:1>) | 未覆盖冻结目标 |
| 3 | createIceInventorySourceTransactionRecord | [backend/src/services/iceInventory.service.ts:408-583](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:408>) | 未覆盖冻结目标 |
| 4 | 文本块 | [backend/drizzle/0011_rbac_permission_catalog.sql:20-85](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0011_rbac_permission_catalog.sql:20>) | 未覆盖冻结目标 |
| 5 | 文本块 | [backend/src/services/delivery.service.ts:894-1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:894>) | 仅目标文件 |
| 6 | 文本块 | [backend/src/routes/iceInventory.ts:72-120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:72>) | 未覆盖冻结目标 |
| 7 | 文本块 | [backend/src/services/delivery.service.ts:1118-1326](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:1118>) | 仅目标文件 |
| 8 | IceInventoryRecord | [frontend/src/types/iceInventory.ts:21-82](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/iceInventory.ts:21>) | 未覆盖冻结目标 |
| 9 | 文本块 | [backend/src/services/iceInventory.service.ts:745-751](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:745>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

## idea-1604-apps-01-para-2

`search_code("驾驶员录入一单冰粒出货后，哪里联动扣除库存，并在扣库失败时移除这次新建的发货记录？")`

搜索语义：定位发货创建与库存流水联动及失败补偿

项目：/Users/teriri/IdeaProjects/1604-apps；样本组：paraphrase。

业务族：idea-1604-apps-01；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [backend/src/services/delivery.service.ts:818](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>)；符号 create；锚点 [818]；关键行 [837, 853, 865, 871, 873]。

人工审计：完整新建出库、扣库存及失败撤销都在818–884，两侧第1逐行相同。正式显示810–1018并混入相邻动作，符号标签退化为文本块，但业务覆盖排名不变。

### 基线

查询耗时 6.887 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | StockOutService.create | [backend/src/services/delivery.service.ts:818-884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) | 单片段覆盖某目标关键正文 |
| 2 | stockOutRecords | [backend/src/db/schema/delivery.ts:21-58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:21>) | 未覆盖冻结目标 |
| 3 | 文本块 | [frontend/src/views/StockOut/index.vue:290-329](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/StockOut/index.vue:290>) | 未覆盖冻结目标 |
| 4 | 文本块 | [backend/drizzle/0007_delivery.sql:1-141](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0007_delivery.sql:1>) | 未覆盖冻结目标 |
| 5 | returnRecords | [backend/src/db/schema/delivery.ts:63-100](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:63>) | 未覆盖冻结目标 |
| 6 | 文本块 | [frontend/src/views/StockOut/components/StockOutForm.vue:224-268](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/StockOut/components/StockOutForm.vue:224>) | 未覆盖冻结目标 |
| 7 | DeliveryRecord | [backend/src/services/delivery.service.ts:130-211](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:130>) | 仅目标文件 |
| 8 | iceInventoryRoutes | [backend/src/routes/iceInventory.ts:45-120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:45>) | 未覆盖冻结目标 |
| 9 | ReturnRecordService.create | [backend/src/services/delivery.service.ts:1126-1192](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:1126>) | 仅目标文件 |
| 10 | IceInventoryService.list | [backend/src/services/iceInventory.service.ts:595-651](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:595>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675358550725000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

### 实验

查询耗时 5.270 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [backend/src/services/delivery.service.ts:810-1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:810>) | 单片段覆盖某目标关键正文 |
| 2 | stockOutRecords | [backend/src/db/schema/delivery.ts:1-58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [frontend/src/views/StockOut/index.vue:290-331](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/StockOut/index.vue:290>) | 未覆盖冻结目标 |
| 4 | returnRecords | [backend/src/db/schema/delivery.ts:63-100](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:63>) | 未覆盖冻结目标 |
| 5 | 文本块 | [backend/drizzle/0007_delivery.sql:16-30](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0007_delivery.sql:16>) | 未覆盖冻结目标 |
| 6 | 文本块 | [backend/drizzle/0011_rbac_permission_catalog.sql:20-85](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0011_rbac_permission_catalog.sql:20>) | 未覆盖冻结目标 |
| 7 | DeliveryRecord | [backend/src/services/delivery.service.ts:130-211](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:130>) | 仅目标文件 |
| 8 | ReturnRecordService.create | [backend/src/services/delivery.service.ts:1126-1326](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:1126>) | 仅目标文件 |
| 9 | iceInventoryRoutes | [backend/src/routes/iceInventory.ts:45-120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:45>) | 未覆盖冻结目标 |
| 10 | DeliveryRecord.inventoryTransactionId | [frontend/src/types/delivery.ts:87-87](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/delivery.ts:87>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679229852892000-fc70e38adab0-holdout-idea-1604-apps-01-attempt1.stderr>)

## holdout-idea-1086-backend-01

`search_code("放款办理完成后，哪里保存放款材料、备注和完成时间？")`

搜索语义：定位汽车贷款办理结果提交，包括附件字段与完成时间写入。

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [application/api/controller/Index.php:394](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:394>)；符号 lanpostOrder；锚点 [394]；关键行 [416, 417, 419, 421, 422, 426, 427]。

人工审计：两臂首位都完整返回 lanpostOrder：416–419 行保存放款日期、附件及备注，421–422 行在完成状态写 loan_finish_time，426–427 行保存并提交事务。实验将基线 268–439 的大文本块收窄为 394–438 的方法并补符号信息，属于边界与可读性改善，所问业务在首位的命中没有变化。

### 基线

查询耗时 5.550 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:268-439](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:268>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [update.sql:55-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/admin/view/workorder/upload.html:1-22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [application/admin/lang/zh-cn/loantype.php:1-8](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/loantype.php:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [application/admin/lang/zh-cn/loantypeitem.php:1-9](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/loantypeitem.php:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [application/common/lang/zh-cn/addon.php:61-97](</Users/teriri/IdeaProjects/1086-backend/application/common/lang/zh-cn/addon.php:61>) | 未覆盖冻结目标 |
| 8 | 文本块 | [application/admin/model/Workorderfile.php:1-57](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorderfile.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/api/controller/Index.php:647-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅目标文件 |
| 10 | 文本块 | [application/admin/model/Loantype.php:1-40](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Loantype.php:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

### 实验

查询耗时 4.987 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | app\api\controller.Index.lanpostOrder | [application/api/controller/Index.php:394-438](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:394>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [public/assets/js/backend/workorder.js:95-129](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/backend/workorder.js:95>) | 未覆盖冻结目标 |
| 3 | 文本块 | [application/admin/lang/zh-cn/workorder.php:3-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:3>) | 未覆盖冻结目标 |
| 4 | 文本块 | [update.sql:100-127](</Users/teriri/IdeaProjects/1086-backend/update.sql:100>) | 未覆盖冻结目标 |
| 5 | 文本块 | [application/admin/view/workorder/add.html:231-247](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/add.html:231>) | 未覆盖冻结目标 |
| 6 | 文本块 | [runtime/temp/fa0f57e1b90fb8864efbdae078df2649.php:292-308](</Users/teriri/IdeaProjects/1086-backend/runtime/temp/fa0f57e1b90fb8864efbdae078df2649.php:292>) | 未覆盖冻结目标 |
| 7 | 文本块 | [runtime/temp/444f82246918337da38a29ed127cc5c3.php:61-93](</Users/teriri/IdeaProjects/1086-backend/runtime/temp/444f82246918337da38a29ed127cc5c3.php:61>) | 未覆盖冻结目标 |
| 8 | 文本块 | [application/admin/view/workorder/edit.html:238-260](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/edit.html:238>) | 未覆盖冻结目标 |
| 9 | 文本块 | [runtime/temp/d3a3dd69776dbeb53859d9d8cd21c13a.php:293-306](</Users/teriri/IdeaProjects/1086-backend/runtime/temp/d3a3dd69776dbeb53859d9d8cd21c13a.php:293>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/admin/view/workorder/upload.html:1-15](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

## holdout-idea-1086-backend-02

`search_code("身份证照片审核")`

搜索语义：定位工单证件照片的审核状态、说明和材料类型更新。

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [application/api/controller/Index.php:448](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:448>)；符号 checkorderidimage；锚点 [448]；关键行 [455, 459, 466, 467, 468, 470]。

人工审计：实际照片审核方法 checkorderidimage 完整返回由第 7 名到第 6 名：455–464 行查询关联工单与审核人条件，466–470 行写照片审核状态、说明及材料类型。实验首位 647–702 来自 postCheckOrder，其身份证与附件处理 650–689 行均为注释，不能当作更靠前的审核实现；idCard 验证函数也只是证件号码校验。因此只确认目标实现提前一位，不宣称首位或显著性提升。

### 基线

查询耗时 5.233 秒；声明/关键正文/范围首次覆盖：7 / 7 / 7。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [application/admin/controller/Workorderimage.php:1-43](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [update.sql:115-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/api/controller/Index.php:1134-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) | 仅目标文件 |
| 5 | 文本块 | [application/api/controller/Index.php:587-763](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:587>) | 仅目标文件 |
| 6 | 文本块 | [application/api/controller/Index.php:881-1026](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:881>) | 仅目标文件 |
| 7 | 文本块 | [application/api/controller/Index.php:440-528](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) | 单片段覆盖某目标关键正文 |
| 8 | 文本块 | [application/api/validate/BaseValidate.php:1-98](</Users/teriri/IdeaProjects/1086-backend/application/api/validate/BaseValidate.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/api/controller/Demo.php:106-157](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Demo.php:106>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/admin/controller/Workorderimage.php:104-244](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:104>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

### 实验

查询耗时 4.302 秒；声明/关键正文/范围首次覆盖：6 / 6 / 6。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:647-702](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅目标文件 |
| 2 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [public/assets/js/backend/workorder.js:50-70](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/backend/workorder.js:50>) | 未覆盖冻结目标 |
| 4 | app\api\validate.BaseValidate.idCard | [application/api/validate/BaseValidate.php:23-73](</Users/teriri/IdeaProjects/1086-backend/application/api/validate/BaseValidate.php:23>) | 未覆盖冻结目标 |
| 5 | app\api\controller.Index.postIdimage | [application/api/controller/Index.php:918-1001](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:918>) | 仅目标文件 |
| 6 | app\api\controller.Index.checkorderidimage | [application/api/controller/Index.php:448-480](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:448>) | 单片段覆盖某目标关键正文 |
| 7 | app\admin\controller.Workorderimage | [application/admin/controller/Workorderimage.php:14-244](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:14>) | 未覆盖冻结目标 |
| 8 | 文本块 | [application/admin/controller/Workorderimage.php:1-13](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖冻结目标 |
| 9 | getIDCardInfo | [application/api/common.php:29-81](</Users/teriri/IdeaProjects/1086-backend/application/api/common.php:29>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/api/controller/Index.php:1120-1153](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1120>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

## holdout-idea-1086-backend-03

`search_code("修改已有贷款申请的证件照片时，如何删除选中的旧照片并保存新的正反面？")`

搜索语义：定位已存在工单的证件图片增改删事务，不把新建工单算成答案。

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [application/api/controller/Index.php:927](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:927>)；符号 postIdimage；锚点 [927]；关键行 [943, 966, 967, 975, 978, 982, 984, 986]。

人工审计：实验首位完整返回 postIdimage：934–946 行定位现有且按角色收窄的工单，947 行取 del_ids，965–972 行组装正反面和已有图片 id，975–986 行在同一事务删除选中旧图、写工单关联并批量保存。基线首位仅为照片编辑表单，第四条只到方法声明和参数检查，缺 943–986 的实现；基线第六条属于新建工单末段，不执行选中旧图删除。后台图片 add 也只是单独保存图片。确认从前十缺完整修改链到首位完整命中。

### 基线

查询耗时 8.376 秒；声明/关键正文/范围首次覆盖：4 / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/admin/view/workorderimage/edit.html:1-67](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/edit.html:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [application/api/controller/Index.php:647-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅目标文件 |
| 3 | 文本块 | [application/admin/view/workorderimage/add.html:1-67](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/add.html:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/api/controller/Index.php:881-933](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:881>) | 覆盖声明锚点 |
| 5 | 文本块 | [update.sql:55-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) | 未覆盖冻结目标 |
| 6 | 文本块 | [application/api/controller/Index.php:1134-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) | 仅目标文件 |
| 7 | 文本块 | [application/admin/view/workorder/upload.html:1-22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/controller/Workorderimage.php:1-150](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/admin/view/workorderfile/edit.html:1-53](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/edit.html:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

### 实验

查询耗时 4.960 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | app\api\controller.Index.postIdimage | [application/api/controller/Index.php:927-1001](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:927>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [application/admin/view/workorderimage/edit.html:1-44](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/edit.html:1>) | 未覆盖冻结目标 |
| 3 | `mk_workorderimage` | [update.sql:129-140](</Users/teriri/IdeaProjects/1086-backend/update.sql:129>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/api/controller/Index.php:647-678](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅目标文件 |
| 5 | 文本块 | [application/admin/view/workorder/edit.html:238-260](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/edit.html:238>) | 未覆盖冻结目标 |
| 6 | app\admin\controller.Workorderimage | [application/admin/controller/Workorderimage.php:14-244](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:14>) | 未覆盖冻结目标 |
| 7 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [application/admin/view/workorderimage/add.html:1-19](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/add.html:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/view/workorderimage/add.html:34-49](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/add.html:34>) | 未覆盖冻结目标 |
| 10 | app\admin\controller.Workorder.edit | [application/admin/controller/Workorder.php:49-114](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:49>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

## idea-1086-backend-01-para-1

`search_code("车贷申请怎么把借款资料、身份证两面和附件存到同一张单？")`

搜索语义：定位新建贷款工单的角色校验、多表关联保存及事务回滚

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：paraphrase。

业务族：idea-1086-backend-01；问法：short。

冻结答案组 complete-loan-create：申请保存需要入口、借款字段、身份证双面、附件、同单ID和事务；仅后半段能定位保存位置，不能代表完整实现
- [application/api/controller/Index.php:1074](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1074>)；符号 postOrder；锚点 [1074]；关键行 [1082, 1087, 1088, 1089, 1090, 1091, 1097, 1098, 1103, 1107, 1120, 1130, 1131, 1143, 1144, 1155, 1160, 1162, 1166, 1168, 1172, 1174, 1176, 1178]。

人工审计：冻结完整实现判据两臂均失败，但不能解释为语义等价：基线第 4 条虽缺 postOrder 的声明和 1082 行角色检查，仍完整返回 1087–1187 的借款信息、创建工单、正反面数组、附件数组及 1162–1176 的关联保存事务，足以观察题目所问的关联机制。实验前十没有该保存事务：首位 postCheckOrder 的照片处理是注释，第三条 postIdimage 只修改已有工单，不能创建并关联新申请。保持原真值与分数不变，记录严格声明/权限锚点使同分掩盖了可见业务正文减少。

### 基线

查询耗时 4.544 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:647-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) | 仅目标文件 |
| 2 | 文本块 | [update.sql:55-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) | 未覆盖冻结目标 |
| 3 | 文本块 | [application/admin/view/workorder/upload.html:1-22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/api/controller/Index.php:1086-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1086>) | 仅目标文件 |
| 5 | 文本块 | [application/admin/view/workorderimage/add.html:1-58](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/add.html:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [application/api/controller/Index.php:881-933](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:881>) | 仅目标文件 |
| 7 | 文本块 | [application/api/controller/Index.php:388-439](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) | 仅目标文件 |
| 8 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/view/workorderfile/edit.html:1-53](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/edit.html:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

### 实验

查询耗时 3.856 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:631-678](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:631>) | 仅目标文件 |
| 2 | 文本块 | [runtime/temp/444f82246918337da38a29ed127cc5c3.php:61-71](</Users/teriri/IdeaProjects/1086-backend/runtime/temp/444f82246918337da38a29ed127cc5c3.php:61>) | 未覆盖冻结目标 |
| 3 | app\api\controller.Index.postIdimage | [application/api/controller/Index.php:927-1001](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:927>) | 仅目标文件 |
| 4 | 文本块 | [application/admin/view/workorder/add.html:231-247](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/add.html:231>) | 未覆盖冻结目标 |
| 5 | 文本块 | [application/admin/view/workorder/edit.html:226-260](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/edit.html:226>) | 未覆盖冻结目标 |
| 6 | 文本块 | [runtime/temp/d3a3dd69776dbeb53859d9d8cd21c13a.php:293-306](</Users/teriri/IdeaProjects/1086-backend/runtime/temp/d3a3dd69776dbeb53859d9d8cd21c13a.php:293>) | 未覆盖冻结目标 |
| 7 | 文本块 | [application/api/controller/Index.php:394-438](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:394>) | 仅目标文件 |
| 8 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [runtime/temp/fa0f57e1b90fb8864efbdae078df2649.php:292-308](</Users/teriri/IdeaProjects/1086-backend/runtime/temp/fa0f57e1b90fb8864efbdae078df2649.php:292>) | 未覆盖冻结目标 |
| 10 | `mk_workorderimage` | [update.sql:129-140](</Users/teriri/IdeaProjects/1086-backend/update.sql:129>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

## idea-1086-backend-01-para-2

`search_code("新建汽车借贷申请时，哪里将借款信息、身份证的正反面照片及相关材料关联到同一个工单后保存？")`

搜索语义：定位新建贷款工单的角色校验、多表关联保存及事务回滚

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：paraphrase。

业务族：idea-1086-backend-01；问法：behavior。

冻结答案组 complete-loan-create：申请保存需要入口、借款字段、身份证双面、附件、同单ID和事务；仅后半段能定位保存位置，不能代表完整实现
- [application/api/controller/Index.php:1074](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1074>)；符号 postOrder；锚点 [1074]；关键行 [1082, 1087, 1088, 1089, 1090, 1091, 1097, 1098, 1103, 1107, 1120, 1130, 1131, 1143, 1144, 1155, 1160, 1162, 1166, 1168, 1172, 1174, 1176, 1178]。

人工审计：基线第 2 条完整返回 postOrder 1074–1187，包含新建 Workorder、正反面/附件集合及事务中为两类资料写相同 workorder_id 后保存。实验只在第 5 条保留 1087–1153 的前半段，缺声明、权限检查及全部 1160–1178 保存/提交/回滚；其余命中 postIdimage 是既有工单图片更新，Workorderimage.add 和 Workorderfile.add 分别向已存在工单添加单种资料，无法替代创建申请并关联两类资料的完整链。确认原第 2 名完整命中退化为前十未完整命中。

### 基线

查询耗时 5.250 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:482-703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:482>) | 仅目标文件 |
| 2 | 文本块 | [application/api/controller/Index.php:1027-1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1027>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [update.sql:1-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/api/controller/Index.php:821-993](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:821>) | 仅目标文件 |
| 5 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [application/common/model/Workorder.php:1-43](</Users/teriri/IdeaProjects/1086-backend/application/common/model/Workorder.php:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [application/admin/model/Workorderimage.php:1-49](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorderimage.php:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

### 实验

查询耗时 4.298 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | app\admin\controller.Workorderimage | [application/admin/controller/Workorderimage.php:14-244](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:14>) | 未覆盖冻结目标 |
| 2 | 文本块 | [application/api/controller/Index.php:614-702](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:614>) | 仅目标文件 |
| 3 | app\api\controller.Index.postIdimage | [application/api/controller/Index.php:927-1001](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:927>) | 仅目标文件 |
| 4 | `mk_workorderimage` | [update.sql:129-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:129>) | 未覆盖冻结目标 |
| 5 | 文本块 | [application/api/controller/Index.php:1087-1153](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1087>) | 仅目标文件 |
| 6 | `mk_workorder` | [update.sql:84-127](</Users/teriri/IdeaProjects/1086-backend/update.sql:84>) | 未覆盖冻结目标 |
| 7 | 文本块 | [application/api/controller/Index.php:833-883](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:833>) | 仅目标文件 |
| 8 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/controller/Workorderimage.php:1-13](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) | 未覆盖冻结目标 |
| 10 | app\admin\controller.Workorderfile.add | [application/admin/controller/Workorderfile.php:136-185](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderfile.php:136>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

## idea-1086-backend-05-para-1

`search_code("提交审核结果前，在哪确认工单归自己办理，再更新状态？")`

搜索语义：定位工单审核提交的状态及归属限制

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：paraphrase。

业务族：idea-1086-backend-05；问法：short。

冻结答案组 original:0：原冻结实现链路完整保留
- [application/api/controller/Index.php:712](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:712>)；符号 checkOrder；锚点 [712]；关键行 [719, 723, 724, 725]。

人工审计：第二轮实际首位完整返回 checkOrder712–734，基线同一实现位于第2名。719行先限制status=30、check_user_id=当前用户和目标id，723–725行写新审核状态、当前审核人并保存；第二条takeOrder是未分配工单接单，不能替代。确认本次实际返回的完整目标提前一位。该PHP查询的目标正文与第一轮相同，而第二轮代码只恢复Vue语义窗口，因此这一名次变化不能单独归因于恢复补丁，可能包含远程重排重复调用波动。

### 基线

查询耗时 5.522 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/admin/controller/Workorder.php:157-308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:157>) | 未覆盖冻结目标 |
| 2 | 文本块 | [application/api/controller/Index.php:704-880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [update.sql:55-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) | 未覆盖冻结目标 |
| 4 | 文本块 | [application/api/controller/Index.php:440-646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) | 仅目标文件 |
| 5 | 文本块 | [application/admin/view/workorder/index.html:1-55](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/index.html:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [application/common/model/Workorder.php:1-43](</Users/teriri/IdeaProjects/1086-backend/application/common/model/Workorder.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/validate/Workorder.php:1-27](</Users/teriri/IdeaProjects/1086-backend/application/admin/validate/Workorder.php:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675392168848000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

### 实验

查询耗时 5.492 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | app\api\controller.Index.checkOrder | [application/api/controller/Index.php:712-734](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:712>) | 单片段覆盖某目标关键正文 |
| 2 | app\api\controller.Index.takeOrder | [application/api/controller/Index.php:744-766](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:744>) | 仅目标文件 |
| 3 | 文本块 | [application/admin/controller/Workorder.php:268-303](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:268>) | 未覆盖冻结目标 |
| 4 | app\admin\controller.Workorderimage | [application/admin/controller/Workorderimage.php:14-244](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:14>) | 未覆盖冻结目标 |
| 5 | app\api\controller.Index.postCheckOrder | [application/api/controller/Index.php:597-702](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:597>) | 仅目标文件 |
| 6 | 文本块 | [application/admin/lang/zh-cn/workorder.php:3-61](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:3>) | 未覆盖冻结目标 |
| 7 | app\api\controller.Index.workorder | [application/api/controller/Index.php:774-823](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:774>) | 仅目标文件 |
| 8 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [public/assets/js/backend/workorder.js:114-129](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/backend/workorder.js:114>) | 未覆盖冻结目标 |
| 10 | 文本块 | [update.sql:76-127](</Users/teriri/IdeaProjects/1086-backend/update.sql:76>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679275596758000-ac3bde1fdbe6-holdout-idea-1086-backend-01-attempt1.stderr>)

## idea-1086-backend-05-para-2

`search_code("审核员送出处理意见时，哪段代码先核对该申请仍由当前本人经办，随后写入新的审核状态？")`

搜索语义：定位工单审核提交的状态及归属限制

项目：/Users/teriri/IdeaProjects/1086-backend；样本组：paraphrase。

业务族：idea-1086-backend-05；问法：behavior。

冻结答案组 original:0：原冻结实现链路完整保留
- [application/api/controller/Index.php:712](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:712>)；符号 checkOrder；锚点 [712]；关键行 [719, 723, 724, 725]。

人工审计：两臂完整 checkOrder 都在第 3 名，当前经办人/状态约束与新状态保存均完整。实验首位 postCheckOrder 虽在 604–607 行核验当前审核人和办理状态，却只保存表单信息；614–648 的 info 没有新审核状态，678 行保存不能替代 723–725 的状态更新。因此首位完整命中没有改善，符号边界变清楚不能算业务排序提升。

### 基线

查询耗时 6.184 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [application/api/controller/Index.php:529-646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:529>) | 仅目标文件 |
| 2 | 文本块 | [update.sql:115-154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) | 未覆盖冻结目标 |
| 3 | 文本块 | [application/api/controller/Index.php:704-880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [application/admin/controller/Workorder.php:247-306](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) | 未覆盖冻结目标 |
| 5 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [application/api/controller/Index.php:268-327](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:268>) | 仅目标文件 |
| 7 | 文本块 | [application/api/controller/Index.php:388-481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) | 仅目标文件 |
| 8 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/index/view/third/prepare.html:60-76](</Users/teriri/IdeaProjects/1086-backend/application/index/view/third/prepare.html:60>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675429479320000-ac3bde1fdbe6-idea-1086-backend-05-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675429479320000-ac3bde1fdbe6-idea-1086-backend-05-para-2-attempt1.stderr>)

### 实验

查询耗时 5.952 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | app\api\controller.Index.postCheckOrder | [application/api/controller/Index.php:597-702](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:597>) | 仅目标文件 |
| 2 | 文本块 | [application/admin/lang/zh-cn/workorder.php:3-65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:3>) | 未覆盖冻结目标 |
| 3 | app\api\controller.Index.checkOrder | [application/api/controller/Index.php:712-734](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:712>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [application/api/controller/Index.php:774-808](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:774>) | 仅目标文件 |
| 5 | 文本块 | [application/api/controller/Index.php:522-550](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:522>) | 仅目标文件 |
| 6 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [update.sql:114-127](</Users/teriri/IdeaProjects/1086-backend/update.sql:114>) | 未覆盖冻结目标 |
| 8 | 文本块 | [public/assets/js/backend/workorder.js:50-70](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/backend/workorder.js:50>) | 未覆盖冻结目标 |
| 9 | 文本块 | [application/admin/controller/Workorder.php:268-303](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:268>) | 未覆盖冻结目标 |
| 10 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1-18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679326306042000-ac3bde1fdbe6-idea-1086-backend-05-para-2-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679326306042000-ac3bde1fdbe6-idea-1086-backend-05-para-2-attempt1.stderr>)

## holdout-idea-KitaTool-01

`search_code("识别出来只有页码或杂乱符号的内容要在哪里丢掉？")`

搜索语义：定位 OCR 输出的有效内容判定，覆盖空白、无意义符号、低有效字符比例和短数字。

项目：/Users/teriri/IdeaProjects/KitaTool；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/kitatool/util/OcrResultValidator.java:49](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/util/OcrResultValidator.java:49>)；符号 isValidOcrResult；锚点 [49]；关键行 [50, 58, 73, 80, 81, 88, 91]。

人工审计：isValidOcrResult49–98按空白、无意义字符、有效字符数量/比例与短数字判定拒绝OCR结果，两侧均第1完整返回。实验从整文件1–112缩到49–98，正文更集中，但完整判断名次没有改善，不能把边界变化算成准确率提升。

### 基线

查询耗时 7.795 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/util/OcrResultValidator.java:1-112](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/util/OcrResultValidator.java:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [ocr-debug/2025-08-31/success/12-38-58-996_dashscope_image35_result.txt:1-25](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/12-38-58-996_dashscope_image35_result.txt:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [ocr-debug/2025-08-31/success/12-37-01-884_dashscope_image35_result.txt:1-25](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/12-37-01-884_dashscope_image35_result.txt:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [ocr-debug/2025-08-31/success/11-25-49-510_dashscope_image35_result.txt:1-25](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-25-49-510_dashscope_image35_result.txt:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [ocr-debug/2025-08-31/success/11-20-53-333_dashscope_image35_result.txt:1-25](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-20-53-333_dashscope_image35_result.txt:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [ocr-debug/2025-08-31/success/12-36-51-662_dashscope_image27_result.txt:1-16](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/12-36-51-662_dashscope_image27_result.txt:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [ocr-debug/2025-08-31/success/11-19-30-464_dashscope_image35_result.txt:1-25](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-19-30-464_dashscope_image35_result.txt:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [ocr-debug/2025-08-31/success/12-38-55-321_dashscope_image27_result.txt:1-16](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/12-38-55-321_dashscope_image27_result.txt:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [ocr-debug/2025-08-31/success/11-19-29-088_dashscope_image27_result.txt:1-16](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-19-29-088_dashscope_image27_result.txt:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [ocr-debug/2025-08-31/success/11-19-19-992_dashscope_image27_result.txt:1-16](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-19-19-992_dashscope_image27_result.txt:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

### 实验

查询耗时 6.331 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/util/OcrResultValidator.java:49-98](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/util/OcrResultValidator.java:49>) | 单片段覆盖某目标关键正文 |
| 2 | com.kita.kitatool.service.PdfService.analyzePageTextForTables | [src/main/java/com/kita/kitatool/service/PdfService.java:404-460](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/PdfService.java:404>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/kitatool/service/FailedImageCollector.java:44-53](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/FailedImageCollector.java:44>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/service/FailedImageCollector.java:28-43](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/FailedImageCollector.java:28>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/service/FailedImageCollector.java:54-268](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/FailedImageCollector.java:54>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:212-256](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:212>) | 未覆盖冻结目标 |
| 7 | 文本块 | [ocr-debug/2025-08-31/success/11-25-27-524_dashscope_image11_result.txt:1-38](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-25-27-524_dashscope_image11_result.txt:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [ocr-debug/2025-08-31/success/12-36-41-085_dashscope_image11_result.txt:1-38](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/12-36-41-085_dashscope_image11_result.txt:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [ocr-debug/2025-08-31/success/11-20-32-351_dashscope_image11_result.txt:1-38](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-20-32-351_dashscope_image11_result.txt:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [ocr-debug/2025-08-31/success/11-19-06-511_dashscope_image11_result.txt:1-38](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-19-06-511_dashscope_image11_result.txt:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

## holdout-idea-KitaTool-02

`search_code("图片太大时怎么压缩再给文字识别用？")`

搜索语义：定位 OCR 前处理入口的大小判断、缩放、JPEG 编码和进一步压缩。

项目：/Users/teriri/IdeaProjects/KitaTool；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:70](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:70>)；符号 preprocessImage；锚点 [70]；关键行 [80, 86, 116, 120, 123, 132, 134, 142]。

人工审计：基线第1从119行开始，只含入口末段与压缩辅助方法，Top10缺少70–118的开关、图片大小判定、解码失败与压缩比例选择。实验第4完整返回preprocessImage70–143，实际入口召回改善；第1仍只是furtherCompress298–316，不能代表完整前处理。要看真实尺寸缩放，实验第5补齐compressImage252–293，因此完整入口加缩放计算需要读到第5。底层JPEG编码imageToBase64359行以后两侧Top10都未提供，冻结入口评分的未命中→4不能扩大成全部图像编码实现均已覆盖。

### 基线

查询耗时 7.433 秒；声明/关键正文/范围首次覆盖：None / None / None。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:119-349](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:119>) | 仅目标文件 |
| 2 | 文本块 | [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:1-58](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:1>) | 仅目标文件 |
| 3 | 文本块 | [src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:1-59](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:106-163](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:106>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/resources/application.yml:120-209](</Users/teriri/IdeaProjects/KitaTool/src/main/resources/application.yml:120>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/WordService.java:571-684](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/WordService.java:571>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/service/N8nOcrService.java:56-143](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/N8nOcrService.java:56>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/kitatool/service/ocr/OcrStrategy.java:1-33](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ocr/OcrStrategy.java:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [ocr-debug/2025-08-31/success/11-21-00-779_dashscope_image49_result.txt:1-9](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/11-21-00-779_dashscope_image49_result.txt:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [ocr-debug/2025-08-31/success/12-37-07-941_dashscope_image49_result.txt:1-9](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-31/success/12-37-07-941_dashscope_image49_result.txt:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

### 实验

查询耗时 4.345 秒；声明/关键正文/范围首次覆盖：4 / 4 / 4。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.kitatool.service.ImagePreprocessingService.furtherCompress | [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:298-316](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:298>) | 仅目标文件 |
| 2 | 文本块 | [src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:115-139](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:115>) | 未覆盖冻结目标 |
| 3 | com.kita.kitatool.service.ImagePreprocessingService.optimizeImageQuality | [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:207-231](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:207>) | 仅目标文件 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:28-206](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:28>) | 单片段覆盖某目标关键正文 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:232-297](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ImagePreprocessingService.java:232>) | 仅目标文件 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/BigModelOcrService.java:109-153](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/BigModelOcrService.java:109>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:140-372](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:140>) | 未覆盖冻结目标 |
| 8 | com.kita.kitatool.service.WordService.processPicture | [src/main/java/com/kita/kitatool/service/WordService.java:562-588](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/WordService.java:562>) | 未覆盖冻结目标 |
| 9 | com.kita.kitatool.service.ocr.OcrStrategy.extractTextFromBase64 | [src/main/java/com/kita/kitatool/service/ocr/OcrStrategy.java:25-25](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ocr/OcrStrategy.java:25>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:105-114](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DashScopeOcrService.java:105>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

## holdout-idea-KitaTool-03

`search_code("一个凭证连续报错以后怎么停止在当前文档任务里使用？")`

搜索语义：定位每任务失败计数到阈值停用凭证的流程，区分任务级与全局配置。

项目：/Users/teriri/IdeaProjects/KitaTool；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/kitatool/service/TaskKeyManager.java:138](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/TaskKeyManager.java:138>)；符号 recordKeyFailureForTask；锚点 [138]；关键行 [139, 142, 143, 149, 150]。
- [src/main/java/com/kita/kitatool/service/TaskKeyManager.java:108](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/TaskKeyManager.java:108>)；符号 disableApiKeyForTask；锚点 [108]；关键行 [114, 116, 120, 123]。

人工审计：recordKeyFailureForTask138–155计数并在阈值调用停用函数；disableApiKeyForTask108–128才根据任务级开关决定全局停用或写入taskDisabledKeys。基线第2同时覆盖两段；实验计数入口虽到第1，任务级真正停用写入退到第5，完整必要链2→5。实验首位不能独立证明凭证已被加入当前任务停用集合。

### 基线

查询耗时 7.216 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1-225](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/kitatool/service/TaskKeyManager.java:49-263](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/TaskKeyManager.java:49>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:118-172](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:118>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:273-488](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:273>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:1-59](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:574-594](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:574>) | 未覆盖冻结目标 |
| 7 | 文本块 | [ocr-debug/2025-08-29/failure/14-48-02-283_dashscope_image1_result.txt:1-9](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-29/failure/14-48-02-283_dashscope_image1_result.txt:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [ocr-debug/2025-08-29/failure/14-48-02-457_dashscope_image2_result.txt:1-9](</Users/teriri/IdeaProjects/KitaTool/ocr-debug/2025-08-29/failure/14-48-02-457_dashscope_image2_result.txt:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

### 实验

查询耗时 5.384 秒；声明/关键正文/范围首次覆盖：5 / 5 / 5。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.kitatool.service.TaskKeyManager.recordKeyFailureForTask | [src/main/java/com/kita/kitatool/service/TaskKeyManager.java:138-155](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/TaskKeyManager.java:138>) | 单片段覆盖某目标关键正文 |
| 2 | com.kita.kitatool.service.DocumentTaskService.cancelTask | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:153-204](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:153>) | 未覆盖冻结目标 |
| 3 | com.kita.kitatool.controller.DocumentController.cancelTask | [src/main/java/com/kita/kitatool/controller/DocumentController.java:412-465](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:412>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/service/BigModelOcrService.java:245-287](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/BigModelOcrService.java:245>) | 未覆盖冻结目标 |
| 5 | com.kita.kitatool.service.TaskKeyManager.disableApiKeyForTask | [src/main/java/com/kita/kitatool/service/TaskKeyManager.java:108-128](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/TaskKeyManager.java:108>) | 单片段覆盖某目标关键正文 |
| 6 | com.kita.kitatool.entity.DocumentTask.markCancelled | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:128-150](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:128>) | 未覆盖冻结目标 |
| 7 | com.kita.kitatool.service.TaskKeyManager.resetKeyFailureForTask | [src/main/java/com/kita/kitatool/service/TaskKeyManager.java:163-175](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/TaskKeyManager.java:163>) | 仅目标文件 |
| 8 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:253-290](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:253>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:140-151](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:140>) | 未覆盖冻结目标 |
| 10 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:585-593](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:585>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

## holdout-idea-KitaTool-04

`search_code("清掉过期的文档处理记录")`

搜索语义：查找根据创建时间清理已完成文档任务的实现，保留正在处理的任务。

项目：/Users/teriri/IdeaProjects/KitaTool；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:285](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:285>)；符号 cleanupOldTasks；锚点 [285]；关键行 [286, 289, 291, 292, 301]。

人工审计：两侧第1均是cleanupCompletedTasks32–48定时入口，只调用cleanupOldTasks(4)。真正按截止创建时间遍历并移除终态任务的cleanupOldTasks285–302均第2完整返回，所以完整清理动作名次未变。isCompleted实际上包含成功、失败和取消，实验第8实体源码可确认；不能把第1调度器的名称或边界改善说成首位实现命中。

### 基线

查询耗时 6.129 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:1-70](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:1>) | 未覆盖冻结目标 |
| 2 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:279-334](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:279>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [src/main/java/com/kita/kitatool/service/OcrResultSaveService.java:236-286](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/OcrResultSaveService.java:236>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:1-59](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentService.java:347-394](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentService.java:347>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/WordService.java:745-938](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/WordService.java:745>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:698-808](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:698>) | 未覆盖冻结目标 |
| 8 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1-60](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1>) | 仅目标文件 |
| 9 | 文本块 | [src/main/java/com/kita/kitatool/service/ThreadPoolCleanupScheduler.java:1-51](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/ThreadPoolCleanupScheduler.java:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [docs/ocr-debug-save.md:56-85](</Users/teriri/IdeaProjects/KitaTool/docs/ocr-debug-save.md:56>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

### 实验

查询耗时 4.306 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.kitatool.service.DocumentTaskCleanupScheduler.cleanupCompletedTasks | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:32-48](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:32>) | 未覆盖冻结目标 |
| 2 | com.kita.kitatool.service.DocumentTaskService.cleanupOldTasks | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:278-302](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:278>) | 单片段覆盖某目标关键正文 |
| 3 | com.kita.kitatool.service.DocumentTaskCleanupScheduler.logTaskStatistics | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:53-69](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:53>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentService.java:359-437](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentService.java:359>) | 未覆盖冻结目标 |
| 5 | com.kita.kitatool.service.WordService.processDocumentWithTimeout | [src/main/java/com/kita/kitatool/service/WordService.java:825-881](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/WordService.java:825>) | 未覆盖冻结目标 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:18-20](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:18>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/service/OcrResultSaveService.java:239-285](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/OcrResultSaveService.java:239>) | 未覆盖冻结目标 |
| 8 | com.kita.kitatool.entity.DocumentTask | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:1-172](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:1>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:24-263](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:24>) | 仅目标文件 |
| 10 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:264-277](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:264>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

## holdout-idea-KitaTool-05

`search_code("取消文档处理任务")`

搜索语义：查找检查任务能否取消并保存取消原因的实际实现。

项目：/Users/teriri/IdeaProjects/KitaTool；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:177](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:177>)；符号 cancelTask；锚点 [177]；关键行 [178, 179, 184, 190, 192]。

人工审计：取消服务cancelTask177–193检查任务存在和isCancellable后调用markCancelled(reason)，由基线第1退到实验第2；实验首位DocumentController448–465只转发并封装响应。更强的完整写入链还需要实体markCancelled145–150及isCancellable169–171：基线第4实体全段均有，实验Top10仅第8有isCancellable，真正写cancelled/CANCELLED/errorMessage的145–150缺失。不能只报冻结1→2而宣称最终Top10完整包含取消状态与原因写入。

### 基线

查询耗时 8.114 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:117-334](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:117>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:1-70](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:432-594](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:432>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:1-172](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:61-116](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:61>) | 仅目标文件 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1-60](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1>) | 仅目标文件 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:335-393](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:335>) | 仅目标文件 |
| 8 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:377-431](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:377>) | 未覆盖冻结目标 |
| 9 | 文本块 | [src/main/java/com/kita/kitatool/service/DynamicThreadPoolManager.java:274-279](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DynamicThreadPoolManager.java:274>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

### 实验

查询耗时 3.835 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.kitatool.controller.DocumentController.cancelTask | [src/main/java/com/kita/kitatool/controller/DocumentController.java:439-465](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:439>) | 未覆盖冻结目标 |
| 2 | com.kita.kitatool.service.DocumentTaskService.cancelTask | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:169-193](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:169>) | 单片段覆盖某目标关键正文 |
| 3 | com.kita.kitatool.service.DocumentTaskCleanupScheduler.cleanupCompletedTasks | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:32-48](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:32>) | 未覆盖冻结目标 |
| 4 | com.kita.kitatool.service.DocumentTaskService.removeTask | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:270-277](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:270>) | 仅目标文件 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:1-20](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskCleanupScheduler.java:1>) | 未覆盖冻结目标 |
| 6 | com.kita.kitatool.service.DocumentTaskService.cleanupOldTasks | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:285-302](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:285>) | 仅目标文件 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:24-168](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:24>) | 仅目标文件 |
| 8 | com.kita.kitatool.entity.DocumentTask.isCancellable | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:169-171](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:169>) | 未覆盖冻结目标 |
| 9 | com.kita.kitatool.controller.DocumentController.createAndProcessTaskSync | [src/main/java/com/kita/kitatool/controller/DocumentController.java:253-290](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:253>) | 未覆盖冻结目标 |
| 10 | com.kita.kitatool.controller.DocumentController.createAndProcessBatchTaskSync | [src/main/java/com/kita/kitatool/controller/DocumentController.java:557-593](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:557>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

## holdout-idea-KitaTool-06

`search_code("文档处理完成后，结果和页数、图片数在哪里保存？")`

搜索语义：定位完成结果序列化、统计字段写入，以及序列化异常转失败的实现。

项目：/Users/teriri/IdeaProjects/KitaTool；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:123](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:123>)；符号 markTaskCompleted；锚点 [123]；关键行 [128, 129, 133, 135, 145, 147]。

人工审计：markTaskCompleted123–152从taskStore取任务，将结果序列化JSON，写页数、图片数等统计，序列化异常调用markFailed，两侧均第1完整提供。实验增加第8实体markCompleted123–127可看见状态和result具体setter；这是补充链路证据，冻结入口名次仍1→1。这里保存的是taskStore中的任务对象，不能从这段代码推断已持久化到数据库。

### 基线

查询耗时 12.497 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1-169](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:1>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [src/main/java/com/kita/kitatool/service/PdfService.java:321-435](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/PdfService.java:321>) | 未覆盖冻结目标 |
| 3 | 文本块 | [src/main/java/com/kita/kitatool/dto/DocumentCompleteResult.java:1-55](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/dto/DocumentCompleteResult.java:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentService.java:347-564](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentService.java:347>) | 未覆盖冻结目标 |
| 5 | 文本块 | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:454-513](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:454>) | 仅目标文件 |
| 6 | 文本块 | [src/main/java/com/kita/kitatool/dto/PdfCompleteResult.java:1-99](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/dto/PdfCompleteResult.java:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/controller/DocumentController.java:752-808](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:752>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675438552455000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

### 实验

查询耗时 4.374 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | com.kita.kitatool.service.DocumentTaskService.markTaskCompleted | [src/main/java/com/kita/kitatool/service/DocumentTaskService.java:116-152](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentTaskService.java:116>) | 单片段覆盖某目标关键正文 |
| 2 | com.kita.kitatool.controller.DocumentController.logCompleteResults | [src/main/java/com/kita/kitatool/controller/DocumentController.java:786-830](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:786>) | 未覆盖冻结目标 |
| 3 | com.kita.kitatool.service.DocumentService.convertToDocumentResult | [src/main/java/com/kita/kitatool/service/DocumentService.java:546-563](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentService.java:546>) | 未覆盖冻结目标 |
| 4 | com.kita.kitatool.dto.DocumentCompleteResult | [src/main/java/com/kita/kitatool/dto/DocumentCompleteResult.java:16-55](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/dto/DocumentCompleteResult.java:16>) | 未覆盖冻结目标 |
| 5 | com.kita.kitatool.service.DocumentService.convertToDocumentResult | [src/main/java/com/kita/kitatool/service/DocumentService.java:521-537](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/DocumentService.java:521>) | 未覆盖冻结目标 |
| 6 | com.kita.kitatool.controller.WordController.logCompleteResults | [src/main/java/com/kita/kitatool/controller/WordController.java:96-134](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/WordController.java:96>) | 未覆盖冻结目标 |
| 7 | 文本块 | [src/main/java/com/kita/kitatool/controller/WordController.java:163-179](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/WordController.java:163>) | 未覆盖冻结目标 |
| 8 | com.kita.kitatool.entity.DocumentTask | [src/main/java/com/kita/kitatool/entity/DocumentTask.java:1-172](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/entity/DocumentTask.java:1>) | 未覆盖冻结目标 |
| 9 | com.kita.kitatool.service.WordService.calculateBatchStats | [src/main/java/com/kita/kitatool/service/WordService.java:889-924](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/service/WordService.java:889>) | 未覆盖冻结目标 |
| 10 | com.kita.kitatool.controller.DocumentController.logBatchResult | [src/main/java/com/kita/kitatool/controller/DocumentController.java:643-661](</Users/teriri/IdeaProjects/KitaTool/src/main/java/com/kita/kitatool/controller/DocumentController.java:643>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679354547352000-0fb363ead817-holdout-idea-KitaTool-01-attempt1.stderr>)

## holdout-idea-zid-332-01

`search_code("筛选待办任务")`

搜索语义：定位根据标题备注关键词、状态和优先级筛选任务列表的核心实现。

项目：/Users/teriri/IdeaProjects/zid-332；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [mobile/src/utils/focusCore.js:28](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:28>)；符号 filterTaskList；锚点 [28]；关键行 [29, 30, 31, 32, 33, 34]。

人工审计：filterTaskList28–36先规范关键词，再对任务标题/备注、状态和优先级取交集，两侧均第1以相同完整函数返回。其后页面模板或历史记录过滤并不能替代该任务筛选逻辑，首位业务质量没有变化。

### 基线

查询耗时 4.059 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | filterTaskList | [mobile/src/utils/focusCore.js:28-36](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:28>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [mobile/src/pages/task/index.vue:1-35](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/task/index.vue:1>) | 未覆盖冻结目标 |
| 3 | filterRecordList | [mobile/src/utils/focusCore.js:38-57](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:38>) | 仅目标文件 |
| 4 | 文本块 | [mobile/src/pages/task/index.vue:76-262](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/task/index.vue:76>) | 未覆盖冻结目标 |
| 5 | INITIAL_TASKS | [mobile/src/constants/focusData.js:171-205](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:171>) | 未覆盖冻结目标 |
| 6 | useTaskStore | [mobile/src/store/useTaskStore.js:11-104](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:11>) | 未覆盖冻结目标 |
| 7 | 文本块 | [docs/frontend/modules/task.md:1-20](</Users/teriri/IdeaProjects/zid-332/docs/frontend/modules/task.md:1>) | 未覆盖冻结目标 |
| 8 | 文本块 | [docs/biz-explorer/pages.md:1-132](</Users/teriri/IdeaProjects/zid-332/docs/biz-explorer/pages.md:1>) | 未覆盖冻结目标 |
| 9 | INITIAL_RECORDS | [mobile/src/constants/focusData.js:207-253](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:207>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/pages/task/index.vue:36-75](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/task/index.vue:36>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

### 实验

查询耗时 4.191 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | filterTaskList | [mobile/src/utils/focusCore.js:28-36](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:28>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [mobile/src/pages/task/index.vue:1-46](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/task/index.vue:1>) | 未覆盖冻结目标 |
| 3 | filterRecordList | [mobile/src/utils/focusCore.js:38-57](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:38>) | 仅目标文件 |
| 4 | 文本块 | [mobile/src/pages/task/index.vue:76-266](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/task/index.vue:76>) | 未覆盖冻结目标 |
| 5 | INITIAL_TASKS | [mobile/src/constants/focusData.js:171-205](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:171>) | 未覆盖冻结目标 |
| 6 | useTaskStore | [mobile/src/store/useTaskStore.js:11-104](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:11>) | 未覆盖冻结目标 |
| 7 | 文本块 | [docs/frontend/modules/task.md:1-20](</Users/teriri/IdeaProjects/zid-332/docs/frontend/modules/task.md:1>) | 未覆盖冻结目标 |
| 8 | INITIAL_RECORDS | [mobile/src/constants/focusData.js:207-253](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:207>) | 未覆盖冻结目标 |
| 9 | 文本块 | [docs/biz-explorer/pages.md:1-132](</Users/teriri/IdeaProjects/zid-332/docs/biz-explorer/pages.md:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/pages/task/edit/index.vue:1-95](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/task/edit/index.vue:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

## holdout-idea-zid-332-02

`search_code("专注倒计时归零以后，哪里停表并自动保存本次记录？")`

搜索语义：定位计时循环归零分支、停止计时、提交完成请求及通知回调。

项目：/Users/teriri/IdeaProjects/zid-332；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [mobile/src/hooks/useFocusTimer.js:24](</Users/teriri/IdeaProjects/zid-332/mobile/src/hooks/useFocusTimer.js:24>)；符号 start；锚点 [24]；关键行 [29, 35, 36, 38, 40, 43, 47]。

人工审计：真正执行每秒递减、归零、stop、提交finishFocusSessionApi并调用onFinish的useFocusTimer.start24–56，基线第3、V1第3，V2退到第4。V2首位虽恢复focus页面186–351大段上下文，只有计时显示、完成弹窗、开始/暂停/重置入口，没有归零循环；第2全屏页也只是显示和转发。实际记录保存finishSession92–125在基线与V2均第5，V1为第4。故恢复Vue窗口没有改善这条停止并自动保存的实现定位，反而把必要核心后移；两侧Top10仍缺clearActiveTimer8–14真实clearInterval代码，冻结标签不扩充。

### 基线

查询耗时 3.927 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [mobile/src/pages/focus/index.vue:130-351](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:130>) | 未覆盖冻结目标 |
| 2 | 文本块 | [mobile/src/pages/focus/lock/index.vue:1-161](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/lock/index.vue:1>) | 未覆盖冻结目标 |
| 3 | useFocusTimer | [mobile/src/hooks/useFocusTimer.js:16-66](</Users/teriri/IdeaProjects/zid-332/mobile/src/hooks/useFocusTimer.js:16>) | 单片段覆盖某目标关键正文 |
| 4 | useRecordStore | [mobile/src/store/useRecordStore.js:19-93](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useRecordStore.js:19>) | 未覆盖冻结目标 |
| 5 | handlers.POST:/api/focus/finishSession | [mobile/src/api/local/index.js:92-125](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:92>) | 未覆盖冻结目标 |
| 6 | 文本块 | [mobile/src/api/local/index.js:51-91](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:51>) | 未覆盖冻结目标 |
| 7 | 文本块 | [mobile/src/api/local/index.js:126-323](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:126>) | 未覆盖冻结目标 |
| 8 | 文本块 | [mobile/src/pages/focus/duration/index.vue:1-54](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/duration/index.vue:1>) | 未覆盖冻结目标 |
| 9 | useFocusStore.updateRemainingSeconds | [mobile/src/store/useFocusStore.js:69-76](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useFocusStore.js:69>) | 未覆盖冻结目标 |
| 10 | handleSave | [mobile/src/pages/focus/duration/index.vue:68-79](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/duration/index.vue:68>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

### 实验

查询耗时 3.705 秒；声明/关键正文/范围首次覆盖：4 / 4 / 4。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [mobile/src/pages/focus/index.vue:186-351](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:186>) | 未覆盖冻结目标 |
| 2 | 文本块 | [mobile/src/pages/focus/lock/index.vue:1-161](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/lock/index.vue:1>) | 未覆盖冻结目标 |
| 3 | useRecordStore | [mobile/src/store/useRecordStore.js:19-93](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useRecordStore.js:19>) | 未覆盖冻结目标 |
| 4 | useFocusTimer | [mobile/src/hooks/useFocusTimer.js:16-66](</Users/teriri/IdeaProjects/zid-332/mobile/src/hooks/useFocusTimer.js:16>) | 单片段覆盖某目标关键正文 |
| 5 | handlers.POST:/api/focus/finishSession | [mobile/src/api/local/index.js:92-125](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:92>) | 未覆盖冻结目标 |
| 6 | 文本块 | [mobile/src/api/local/index.js:51-91](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:51>) | 未覆盖冻结目标 |
| 7 | 文本块 | [mobile/src/api/local/index.js:126-323](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:126>) | 未覆盖冻结目标 |
| 8 | useFocusStore.updateRemainingSeconds | [mobile/src/store/useFocusStore.js:69-76](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useFocusStore.js:69>) | 未覆盖冻结目标 |
| 9 | handleSave | [mobile/src/pages/focus/duration/index.vue:68-79](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/duration/index.vue:68>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/pages/focus/duration/index.vue:1-57](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/duration/index.vue:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

## holdout-idea-zid-332-03

`search_code("连续打卡天数怎么算？")`

搜索语义：定位按有专注记录的日期去重后计算截至今天的连续天数实现。

项目：/Users/teriri/IdeaProjects/zid-332；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [mobile/src/utils/focusCore.js:235](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:235>)；符号 buildCheckinSummary；锚点 [235]；关键行 [236, 239, 244, 245, 246, 248, 255]。

人工审计：buildCheckinSummary235–259按startedAt日期去重倒序，从今天开始逐日比较并在断档停止，完整连续天数计算由第2退第3。两侧首位BADGE_PRESETS255–304只有徽章描述和默认阈值，没有日期比较循环；实验第2签到模板也仅展示配置，不能用文件或页面相关性掩盖计算名次退后。

### 基线

查询耗时 4.118 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | BADGE_PRESETS | [mobile/src/constants/focusData.js:255-304](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:255>) | 未覆盖冻结目标 |
| 2 | buildCheckinSummary | [mobile/src/utils/focusCore.js:235-259](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:235>) | 单片段覆盖某目标关键正文 |
| 3 | 文本块 | [mobile/src/pages/checkin/index.vue:1-103](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/checkin/index.vue:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [mobile/src/pages/checkin/index.vue:114-126](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/checkin/index.vue:114>) | 未覆盖冻结目标 |
| 5 | 文本块 | [mobile/src/pages/stats/today/index.vue:1-137](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/today/index.vue:1>) | 未覆盖冻结目标 |
| 6 | 文本块 | [mobile/src/store/useGrowthStore.js:14-22](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useGrowthStore.js:14>) | 未覆盖冻结目标 |
| 7 | calendarDays | [mobile/src/pages/checkin/index.vue:128-142](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/checkin/index.vue:128>) | 未覆盖冻结目标 |
| 8 | handlers.GET:/api/stats/getCheckinSummary | [mobile/src/api/local/index.js:197-203](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:197>) | 未覆盖冻结目标 |
| 9 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 仅目标文件 |
| 10 | loadData | [mobile/src/pages/stats/today/index.vue:139-146](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/today/index.vue:139>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

### 实验

查询耗时 4.344 秒；声明/关键正文/范围首次覆盖：3 / 3 / 3。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | BADGE_PRESETS | [mobile/src/constants/focusData.js:255-304](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:255>) | 未覆盖冻结目标 |
| 2 | 文本块 | [mobile/src/pages/checkin/index.vue:1-107](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/checkin/index.vue:1>) | 未覆盖冻结目标 |
| 3 | buildCheckinSummary | [mobile/src/utils/focusCore.js:235-259](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:235>) | 单片段覆盖某目标关键正文 |
| 4 | 文本块 | [mobile/src/pages/stats/today/index.vue:1-111](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/today/index.vue:1>) | 未覆盖冻结目标 |
| 5 | 文本块 | [mobile/src/pages/checkin/index.vue:114-126](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/checkin/index.vue:114>) | 未覆盖冻结目标 |
| 6 | 文本块 | [mobile/src/pages/stats/today/index.vue:119-137](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/today/index.vue:119>) | 未覆盖冻结目标 |
| 7 | 文本块 | [mobile/src/store/useGrowthStore.js:14-22](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useGrowthStore.js:14>) | 未覆盖冻结目标 |
| 8 | calendarDays | [mobile/src/pages/checkin/index.vue:128-142](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/checkin/index.vue:128>) | 未覆盖冻结目标 |
| 9 | handlers.GET:/api/stats/getCheckinSummary | [mobile/src/api/local/index.js:197-203](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:197>) | 未覆盖冻结目标 |
| 10 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 仅目标文件 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

## holdout-idea-zid-332-04

`search_code("成就徽章什么时候解锁？")`

搜索语义：定位按总次数、累计时长或连续天数计算徽章进度及首次解锁时间。

项目：/Users/teriri/IdeaProjects/zid-332；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [mobile/src/utils/focusCore.js:274](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>)；符号 rebuildBadges；锚点 [274]；关键行 [275, 276, 277, 280, 281, 285, 286]。

人工审计：实际动态解锁判定rebuildBadges274–289按次数、累计分钟或streakCount选择progressValue，比较targetValue并保留首次unlockedAt，由基线第6退到V2第8（V1也第8）。V2首位恢复成就页1–122，既有模板只显示isUnlocked，loadData60–63只是getAchievementList后把list赋给badges，没有判定或解锁时间计算；第2BADGE_PRESETS仅给预设阈值。因此不能因首位从配置换成页面就宣称完整实现改善。

### 基线

查询耗时 5.034 秒；声明/关键正文/范围首次覆盖：6 / 6 / 6。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [mobile/src/pages/achievement/index.vue:1-58](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/achievement/index.vue:1>) | 未覆盖冻结目标 |
| 2 | BADGE_PRESETS | [mobile/src/constants/focusData.js:255-304](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:255>) | 未覆盖冻结目标 |
| 3 | 文本块 | [docs/biz-explorer/pages.md:118-132](</Users/teriri/IdeaProjects/zid-332/docs/biz-explorer/pages.md:118>) | 未覆盖冻结目标 |
| 4 | 文本块 | [progress.json:121-180](</Users/teriri/IdeaProjects/zid-332/progress.json:121>) | 未覆盖冻结目标 |
| 5 | 文本块 | [docs/frontend/modules/profile-growth.md:1-24](</Users/teriri/IdeaProjects/zid-332/docs/frontend/modules/profile-growth.md:1>) | 未覆盖冻结目标 |
| 6 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 单片段覆盖某目标关键正文 |
| 7 | 文本块 | [mobile/src/store/useGrowthStore.js:14-22](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useGrowthStore.js:14>) | 未覆盖冻结目标 |
| 8 | 文本块 | [mobile/src/pages/achievement/index.vue:60-129](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/achievement/index.vue:60>) | 未覆盖冻结目标 |
| 9 | 文本块 | [mobile/src/pages/profile/index.vue:1-58](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/profile/index.vue:1>) | 未覆盖冻结目标 |
| 10 | 文本块 | [docs/biz-explorer/flows.md:1-62](</Users/teriri/IdeaProjects/zid-332/docs/biz-explorer/flows.md:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

### 实验

查询耗时 4.684 秒；声明/关键正文/范围首次覆盖：8 / 8 / 8。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | 文本块 | [mobile/src/pages/achievement/index.vue:1-122](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/achievement/index.vue:1>) | 未覆盖冻结目标 |
| 2 | BADGE_PRESETS | [mobile/src/constants/focusData.js:255-304](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:255>) | 未覆盖冻结目标 |
| 3 | 文本块 | [progress.json:1-194](</Users/teriri/IdeaProjects/zid-332/progress.json:1>) | 未覆盖冻结目标 |
| 4 | 文本块 | [docs/biz-explorer/pages.md:113-132](</Users/teriri/IdeaProjects/zid-332/docs/biz-explorer/pages.md:113>) | 未覆盖冻结目标 |
| 5 | 文本块 | [docs/contracts/api-contracts.md:414-460](</Users/teriri/IdeaProjects/zid-332/docs/contracts/api-contracts.md:414>) | 未覆盖冻结目标 |
| 6 | 文本块 | [docs/frontend/modules/profile-growth.md:1-24](</Users/teriri/IdeaProjects/zid-332/docs/frontend/modules/profile-growth.md:1>) | 未覆盖冻结目标 |
| 7 | 文本块 | [docs/biz-explorer/data-models.md:61-120](</Users/teriri/IdeaProjects/zid-332/docs/biz-explorer/data-models.md:61>) | 未覆盖冻结目标 |
| 8 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 单片段覆盖某目标关键正文 |
| 9 | 文本块 | [mobile/src/store/useGrowthStore.js:14-22](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useGrowthStore.js:14>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/pages/profile/index.vue:34-58](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/profile/index.vue:34>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

## holdout-idea-zid-332-05

`search_code("统计里的总专注时长、平均每次时长和目标完成百分比怎么算？")`

搜索语义：定位按时间范围汇总专注记录并与对应周期目标比较的计算。

项目：/Users/teriri/IdeaProjects/zid-332；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [mobile/src/utils/focusCore.js:165](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:165>)；符号 buildStatsSummary；锚点 [165]；关键行 [166, 167, 169, 171, 172, 176]。

人工审计：buildStatsSummary165–183两侧均第1完整返回：先筛时间范围，累加actualMinutes、计算平均值，按日/周/月目标选择基数，并把完成百分比封顶100。源码和名次一致；该结果直接解释题目列出的三个指标，后续展示模板变化不改变首位质量。

### 基线

查询耗时 4.891 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | buildStatsSummary | [mobile/src/utils/focusCore.js:165-183](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:165>) | 单片段覆盖某目标关键正文 |
| 2 | useRecordStore | [mobile/src/store/useRecordStore.js:19-93](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useRecordStore.js:19>) | 未覆盖冻结目标 |
| 3 | 文本块 | [mobile/src/pages/focus/index.vue:130-302](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:130>) | 未覆盖冻结目标 |
| 4 | 文本块 | [mobile/src/pages/stats/today/index.vue:1-147](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/today/index.vue:1>) | 未覆盖冻结目标 |
| 5 | buildModeRatios | [mobile/src/utils/focusCore.js:209-227](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:209>) | 仅目标文件 |
| 6 | 文本块 | [mobile/src/store/useFocusStore.js:27-28](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useFocusStore.js:27>) | 未覆盖冻结目标 |
| 7 | rebuildGoals | [mobile/src/utils/focusCore.js:261-272](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:261>) | 仅目标文件 |
| 8 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 仅目标文件 |
| 9 | barHeight | [mobile/src/pages/stats/analysis/index.vue:118-124](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/analysis/index.vue:118>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/pages/focus/duration/index.vue:1-112](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/duration/index.vue:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

### 实验

查询耗时 3.935 秒；声明/关键正文/范围首次覆盖：1 / 1 / 1。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | buildStatsSummary | [mobile/src/utils/focusCore.js:165-183](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:165>) | 单片段覆盖某目标关键正文 |
| 2 | 文本块 | [mobile/src/pages/stats/today/index.vue:1-147](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/stats/today/index.vue:1>) | 未覆盖冻结目标 |
| 3 | 文本块 | [mobile/src/pages/focus/index.vue:186-302](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:186>) | 未覆盖冻结目标 |
| 4 | buildModeRatios | [mobile/src/utils/focusCore.js:209-227](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:209>) | 仅目标文件 |
| 5 | 文本块 | [mobile/src/store/useFocusStore.js:27-28](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useFocusStore.js:27>) | 未覆盖冻结目标 |
| 6 | rebuildGoals | [mobile/src/utils/focusCore.js:261-272](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:261>) | 仅目标文件 |
| 7 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 仅目标文件 |
| 8 | 文本块 | [mobile/src/pages/focus/index.vue:1-179](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:1>) | 未覆盖冻结目标 |
| 9 | useRecordStore.exportText | [mobile/src/store/useRecordStore.js:50-59](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useRecordStore.js:50>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/pages/focus/duration/index.vue:1-48](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/duration/index.vue:1>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

## holdout-idea-zid-332-06

`search_code("结束一次专注后，怎么更新关联任务的完成次数，达到计划次数就标为完成？")`

搜索语义：定位结束专注写入记录及更新关联任务次数、完成态的完整流程。

项目：/Users/teriri/IdeaProjects/zid-332；样本组：holdout。

冻结答案组 original:0：从原平面真值等价转换，组内全部目标必须覆盖
- [mobile/src/api/local/index.js:92](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:92>)；符号 POST:/api/focus/finishSession；锚点 [92]；关键行 [99, 105, 114, 115, 116, 117]。
- [mobile/src/store/useTaskStore.js:72](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:72>)；符号 applyFocusRecord；锚点 [72]；关键行 [73, 74, 76, 77, 78, 80, 81]。

人工审计：两侧第1均完整包含applyFocusRecord72–83：找到关联任务，次数加一，达到estimatedSessions改done后saveItem。第2均完整返回finishSession92–125，负责从当前会话建立记录、保存记录、调用applyFocusRecord、重建成长数据并清会话。完整入口加任务更新链均需前2条，实验第1扩大到104行没有提前完整答案。

### 基线

查询耗时 4.170 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | useTaskStore.applyFocusRecord | [mobile/src/store/useTaskStore.js:72-83](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:72>) | 单片段覆盖某目标关键正文 |
| 2 | handlers.POST:/api/focus/finishSession | [mobile/src/api/local/index.js:92-125](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:92>) | 单片段覆盖某目标关键正文 |
| 3 | BADGE_PRESETS | [mobile/src/constants/focusData.js:255-304](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:255>) | 未覆盖冻结目标 |
| 4 | useRecordStore | [mobile/src/store/useRecordStore.js:19-93](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useRecordStore.js:19>) | 未覆盖冻结目标 |
| 5 | 文本块 | [mobile/src/pages/focus/index.vue:237-351](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:237>) | 未覆盖冻结目标 |
| 6 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 未覆盖冻结目标 |
| 7 | 文本块 | [mobile/src/pages/focus/index.vue:96-185](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:96>) | 未覆盖冻结目标 |
| 8 | normalizeTaskPayload | [mobile/src/utils/focusCore.js:113-126](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:113>) | 未覆盖冻结目标 |
| 9 | useTaskStore.toggleComplete | [mobile/src/store/useTaskStore.js:48-60](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:48>) | 仅目标文件 |
| 10 | 文本块 | [mobile/src/store/useFocusStore.js:32-67](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useFocusStore.js:32>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/baseline/logs/1788675487948472000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)

### 实验

查询耗时 3.817 秒；声明/关键正文/范围首次覆盖：2 / 2 / 2。

| 名次 | 实际符号 | 路径与行号 | 冻结目标诊断 |
|---:|---|---|---|
| 1 | useTaskStore.applyFocusRecord | [mobile/src/store/useTaskStore.js:72-104](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:72>) | 单片段覆盖某目标关键正文 |
| 2 | handlers.POST:/api/focus/finishSession | [mobile/src/api/local/index.js:92-125](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/local/index.js:92>) | 单片段覆盖某目标关键正文 |
| 3 | BADGE_PRESETS | [mobile/src/constants/focusData.js:255-304](</Users/teriri/IdeaProjects/zid-332/mobile/src/constants/focusData.js:255>) | 未覆盖冻结目标 |
| 4 | 文本块 | [mobile/src/pages/focus/index.vue:224-351](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:224>) | 未覆盖冻结目标 |
| 5 | 文本块 | [mobile/src/pages/focus/index.vue:1-179](</Users/teriri/IdeaProjects/zid-332/mobile/src/pages/focus/index.vue:1>) | 未覆盖冻结目标 |
| 6 | rebuildBadges | [mobile/src/utils/focusCore.js:274-289](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:274>) | 未覆盖冻结目标 |
| 7 | normalizeTaskPayload | [mobile/src/utils/focusCore.js:113-126](</Users/teriri/IdeaProjects/zid-332/mobile/src/utils/focusCore.js:113>) | 未覆盖冻结目标 |
| 8 | useTaskStore.toggleComplete | [mobile/src/store/useTaskStore.js:48-60](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useTaskStore.js:48>) | 仅目标文件 |
| 9 | 文本块 | [mobile/src/api/focusApi.js:3-11](</Users/teriri/IdeaProjects/zid-332/mobile/src/api/focusApi.js:3>) | 未覆盖冻结目标 |
| 10 | 文本块 | [mobile/src/store/useFocusStore.js:32-67](</Users/teriri/IdeaProjects/zid-332/mobile/src/store/useFocusStore.js:32>) | 未覆盖冻结目标 |

原始 [stdout](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stdout>) · [stderr](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/all-language-ast-implementation-2026-09-06/restored-vue-context/logs/1788679383530164000-02deb2f35af1-holdout-idea-zid-332-01-attempt1.stderr>)
