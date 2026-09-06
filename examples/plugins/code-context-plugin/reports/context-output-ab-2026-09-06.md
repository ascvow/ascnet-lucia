# Lucia 上下文输出修复 A/B 验收

首位准确性仍未改善：审计 Top-1 保持 36/48。此次保留的是确定的输出缺陷修复，让已召回的正文通过去重、合并和预算筛选后仍能返回。原冻结关键行为 Top-10 为 43→47/48；审计关键行为 Top-10 为 46→48/48。原冻结 MRR 单侧 p=0.03125、双侧 p=0.0625；审计 MRR 单侧 p=0.0625，不能宣称整体排序已显著提高。

实现先保留已有有界重排池，再按实际可见行合并和去重；不增加模型候选数或 API 调用，不换模型、不重嵌向量。两轮中间失败、截断注记解析恢复及源码核验均已保存。

[完整比较 JSON（含源码片段）](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/quality-fix-2026-09-06/final-comparison.json>) · [逐条源码核验](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/quality-fix-2026-09-06/final-quality-review.md>) · [验收与失败记录](</Users/teriri/teririProjects/ascnet-lucia/docs/code-context-engine/retrieval-acceptance-2026-09-05-directive-sweep.md:491>)



本报告列出 8 个真实项目的 48 条独立查询，逐条展示搜索语义、基线与最终版本的实际返回位置和缺行。界面写法为 `search_code(...)`，实际注册和调用工具为 `code_search`。完整源码片段保留在原始比较 JSON，本报告不复制其他仓库源码。

声明锚点覆盖要求目标声明或模板行为锚点出现；关键行为联合覆盖还要求全部冻结关键行在累计 Top-K 实际片段中按文件、行号和原文齐全。审计口径允许经源码核验的完整替代实现，原冻结口径单独保留。重复运行同一组 48 条用于稳定性检验，不能当作 96 个独立样本。

比较证据：[/Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/quality-fix-2026-09-06/final-comparison.json](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/quality-fix-2026-09-06/final-comparison.json>)；SHA-256：`c27a76d3fd2d24d9348da4bd2e9c30483e941306c8a2b0343ac9f9babedee27c`。

交替复测继续保留同48个样本，Top-10与A/B净改善复现；两侧文档提取查询都从第3变第4，不能宣称全部结果稳定。基线/最终中位5.1130/5.2086秒，P95为10.1314/9.0482秒；24条更快、24条更慢（双侧p=1），未检出速度差异，但两侧均未达到4.7秒参考值。首次耗时仍保留在下表后的说明。

[交替复测比较](</Users/teriri/teririProjects/ascnet-lucia/examples/plugins/code-context-plugin/target/quality-fix-2026-09-06/paired-comparison.json>)。首位审计关键行为仍为36/48，本次只证明已召回代码在输出阶段更完整。

## 汇总

| 真值口径 | 指标 | 基线 | 最终 | 赢 / 输 / 平 | 单侧 p | 双侧 p |
| --- | --- | --- | --- | --- | --- | --- |
| 原冻结 | 声明锚点覆盖@1 | 34/48 | 35/48 | 1 / 0 / 47 | 0.5 | 1 |
| 原冻结 | 声明锚点覆盖@3 | 44/48 | 45/48 | 1 / 0 / 47 | 0.5 | 1 |
| 原冻结 | 声明锚点覆盖@10 | 44/48 | 47/48 | 3 / 0 / 45 | 0.125 | 0.25 |
| 原冻结 | 关键行为联合覆盖@1 | 34/48 | 35/48 | 1 / 0 / 47 | 0.5 | 1 |
| 原冻结 | 关键行为联合覆盖@3 | 43/48 | 45/48 | 2 / 0 / 46 | 0.25 | 0.5 |
| 原冻结 | 关键行为联合覆盖@10 | 43/48 | 47/48 | 4 / 0 / 44 | 0.0625 | 0.125 |
| 原冻结 | 关键行为 MRR | 0.7917 | 0.8297 | 5 / 0 / 43 | 0.03125 | 0.0625 |
| 审计校准 | 声明锚点覆盖@1 | 36/48 | 36/48 | 0 / 0 / 48 | 1 | 1 |
| 审计校准 | 声明锚点覆盖@3 | 46/48 | 47/48 | 1 / 0 / 47 | 0.5 | 1 |
| 审计校准 | 声明锚点覆盖@10 | 47/48 | 48/48 | 1 / 0 / 47 | 0.5 | 1 |
| 审计校准 | 关键行为联合覆盖@1 | 36/48 | 36/48 | 0 / 0 / 48 | 1 | 1 |
| 审计校准 | 关键行为联合覆盖@3 | 45/48 | 47/48 | 2 / 0 / 46 | 0.25 | 0.5 |
| 审计校准 | 关键行为联合覆盖@10 | 46/48 | 48/48 | 2 / 0 / 46 | 0.25 | 0.5 |
| 审计校准 | 冻结范围全部非空行覆盖@1 | 34/48 | 36/48 | 2 / 0 / 46 | 0.25 | 0.5 |
| 审计校准 | 冻结范围全部非空行覆盖@3 | 43/48 | 47/48 | 4 / 0 / 44 | 0.0625 | 0.125 |
| 审计校准 | 冻结范围全部非空行覆盖@10 | 44/48 | 48/48 | 4 / 0 / 44 | 0.0625 | 0.125 |
| 审计校准 | 关键行为 MRR | 0.8375 | 0.8583 | 4 / 0 / 44 | 0.0625 | 0.125 |

查询耗时中位数：4344.7 → 4853.5 毫秒；P95：7499.3 → 11051.7 毫秒。

历史基线与实验不同时运行，API服务负载和网络有时段差异；只把时延视为观察结果。

所有指标均报告逐查询符号检验；主口径是原冻结关键行为，审计口径并列。多指标未校正，不能挑选单个显著指标宣称整体成功。

## web-1623-01 · 1623-mobile

```text
search_code("实习照片怎样从手机拍照或相册选取后上传并填回记录？")
```

搜索意图：图片选取、异步上传和表单回填

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/pages/internship/form.vue:114–172](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:114>) |
| 基线 | 2 | 文本块 | [src/pages/internship/form.vue:1–54](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) |
| 基线 | 3 | handleUpload | [src/pages/basic/index.vue:77–101](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:77>) |
| 基线 | 4 | 文本块 | [src/pages/internship/detail.vue:1–115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) |
| 基线 | 5 | onChooseAvatar | [src/components/LoginWrapper/index.vue:204–212](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:204>) |
| 基线 | 6 | 文本块 | [src/pages/internship/index.vue:26–34](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:26>) |
| 基线 | 7 | handleSubmit | [src/pages/basic/internship.vue:97–107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) |
| 基线 | 8 | 文本块 | [src/pages/internship/index.vue:1–21](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) |
| 基线 | 9 | 文本块 | [src/pages/basic/internship.vue:1–60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 基线 | 10 | savePracticeRecord | [src/api/practice.js:80–86](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:80>) |
| 最终 | 1 | 文本块 | [src/pages/internship/form.vue:1–179](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) |
| 最终 | 2 | handleUpload | [src/pages/basic/index.vue:77–101](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:77>) |
| 最终 | 3 | 文本块 | [src/pages/internship/detail.vue:1–167](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) |
| 最终 | 4 | onChooseAvatar | [src/components/LoginWrapper/index.vue:204–212](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:204>) |
| 最终 | 5 | 文本块 | [src/pages/internship/index.vue:1–55](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) |
| 最终 | 6 | handleSubmit | [src/pages/basic/internship.vue:97–130](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) |
| 最终 | 7 | ensureRecordPermission | [src/pages/internship/form.vue:201–235](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:201>) |
| 最终 | 8 | 文本块 | [src/pages/basic/internship.vue:1–73](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 最终 | 9 | savePracticeRecord | [src/api/practice.js:80–86](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:80>) |
| 最终 | 10 | 文本块 | [src/pages/internship/students.vue:1–48](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/internship/form.vue · handleUpload](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:130>) | 无 | 无 |

查询耗时：3506.6 → 4160.5 毫秒。

## web-1623-02 · 1623-mobile

```text
search_code("学生填写某月的实习内容、心得和照片后，在哪里保存并返回列表？")
```

搜索意图：按月份和学生保存完整实习报告

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | handleSubmit | [src/pages/internship/form.vue:156–171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) |
| 基线 | 2 | loadDetail | [src/pages/internship/form.vue:118–128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:118>) |
| 基线 | 3 | 文本块 | [src/pages/internship/detail.vue:1–115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) |
| 基线 | 4 | 文本块 | [src/pages/internship/index.vue:1–55](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) |
| 基线 | 5 | 文本块 | [src/pages/internship/students.vue:1–56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) |
| 基线 | 6 | handleSubmit | [src/pages/basic/internship.vue:97–107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) |
| 基线 | 7 | 文本块 | [src/pages/basic/internship.vue:1–60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 基线 | 8 | 文本块 | [src/pages/rating/index.vue:1–57](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:1>) |
| 基线 | 9 | 文本块 | [src/pages/rating/form.vue:1–51](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:1>) |
| 基线 | 10 | savePracticeInfo | [src/api/practice.js:49–55](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:49>) |
| 最终 | 1 | handleSubmit | [src/pages/internship/form.vue:156–171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) |
| 最终 | 2 | loadDetail | [src/pages/internship/form.vue:1–128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) |
| 最终 | 3 | 文本块 | [src/pages/internship/detail.vue:1–173](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) |
| 最终 | 4 | 文本块 | [src/pages/internship/index.vue:1–55](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) |
| 最终 | 5 | 文本块 | [src/pages/internship/students.vue:1–56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) |
| 最终 | 6 | handleSubmit | [src/pages/basic/internship.vue:97–107](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:97>) |
| 最终 | 7 | 文本块 | [src/pages/basic/internship.vue:1–73](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 最终 | 8 | 文本块 | [src/pages/rating/index.vue:1–57](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:1>) |
| 最终 | 9 | 文本块 | [src/pages/rating/form.vue:1–60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:1>) |
| 最终 | 10 | savePracticeInfo | [src/api/practice.js:49–55](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:49>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/internship/form.vue · handleSubmit](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) | 无 | 无 |

查询耗时：3681.2 → 3520.2 毫秒。

## web-1623-03 · 1623-mobile

```text
search_code("语音识别服务修正前面识别错的片段时，输入框里的文字怎样重新合并？")
```

搜索意图：流式转写替换旧片段并有序回填表单

原冻结声明锚点覆盖名次：**2 → 2**；原冻结关键行为联合覆盖：**2 → 2**。

审计关键行为联合覆盖：**2 → 2**；冻结范围全部非空行覆盖：2 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | updateVoiceField | [src/pages/internship/form.vue:470–477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) |
| 基线 | 2 | handleIatMessage | [src/pages/internship/form.vue:422–458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) |
| 基线 | 3 | default.methods.trimSpliter | [src/components/SkuSelector/index.vue:291–300](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:291>) |
| 基线 | 4 | 文本块 | [src/components/PracticeDesign/FormField.vue:48–100](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/FormField.vue:48>) |
| 基线 | 5 | 文本块 | [src/manifest.json:61–91](</Users/teriri/WebstormProjects/1623-mobile/src/manifest.json:61>) |
| 基线 | 6 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17–24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) |
| 基线 | 7 | 文本块 | [src/pages/index/index.vue:75–80](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:75>) |
| 基线 | 8 | processVueFiles.textMap | [scripts/generate-language.js:326–326](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:326>) |
| 基线 | 9 | handleAgree | [src/pages/launch/index.vue:21–24](</Users/teriri/WebstormProjects/1623-mobile/src/pages/launch/index.vue:21>) |
| 基线 | 10 | default.methods.combineAttr.result | [src/components/SkuSelector/index.vue:164–164](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:164>) |
| 最终 | 1 | updateVoiceField | [src/pages/internship/form.vue:470–477](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) |
| 最终 | 2 | handleIatMessage | [src/pages/internship/form.vue:422–458](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) |
| 最终 | 3 | parseIatResult | [src/pages/internship/form.vue:460–468](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:460>) |
| 最终 | 4 | 文本块 | [src/pages/internship/form.vue:201–345](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:201>) |
| 最终 | 5 | default.methods.trimSpliter | [src/components/SkuSelector/index.vue:291–300](</Users/teriri/WebstormProjects/1623-mobile/src/components/SkuSelector/index.vue:291>) |
| 最终 | 6 | 文本块 | [src/components/PracticeDesign/FormField.vue:48–100](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/FormField.vue:48>) |
| 最终 | 7 | 文本块 | [src/manifest.json:61–91](</Users/teriri/WebstormProjects/1623-mobile/src/manifest.json:61>) |
| 最终 | 8 | 文本块 | [src/components/PracticeDesign/SubmitBar.vue:17–24](</Users/teriri/WebstormProjects/1623-mobile/src/components/PracticeDesign/SubmitBar.vue:17>) |
| 最终 | 9 | 文本块 | [src/pages/index/index.vue:75–88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:75>) |
| 最终 | 10 | processVueFiles.textMap | [scripts/generate-language.js:326–326](</Users/teriri/WebstormProjects/1623-mobile/scripts/generate-language.js:326>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/internship/form.vue · handleIatMessage](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:422>) | 无 | 无 |
| 原冻结 | [src/pages/internship/form.vue · updateVoiceField](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:470>) | 无 | 无 |

查询耗时：6206 → 4211.4 毫秒。

## web-1623-04 · 1623-mobile

```text
search_code("老师怎样保存某个学生当月的迟到请假次数、工作时长和实习评级？")
```

搜索意图：按学生和月份提交考勤及等级

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/pages/rating/form.vue:52–110](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:52>) |
| 基线 | 2 | 文本块 | [src/pages/internship/detail.vue:54–115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:54>) |
| 基线 | 3 | 文本块 | [src/pages/index/index.vue:1–49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) |
| 基线 | 4 | handleSubmit | [src/pages/internship/form.vue:156–171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) |
| 基线 | 5 | loadDetail | [src/pages/internship/form.vue:118–128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:118>) |
| 基线 | 6 | 文本块 | [src/pages/index/index.vue:110–169](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:110>) |
| 基线 | 7 | 文本块 | [src/pages/rating/index.vue:1–57](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:1>) |
| 基线 | 8 | 文本块 | [src/pages/internship/students.vue:1–56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) |
| 基线 | 9 | saveAttendance | [src/api/practice.js:119–125](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:119>) |
| 基线 | 10 | 文本块 | [src/pages/basic/internship.vue:1–115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 最终 | 1 | 文本块 | [src/pages/rating/form.vue:1–110](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:1>) |
| 最终 | 2 | 文本块 | [src/pages/internship/detail.vue:1–115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/detail.vue:1>) |
| 最终 | 3 | 文本块 | [src/pages/index/index.vue:1–169](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) |
| 最终 | 4 | handleSubmit | [src/pages/internship/form.vue:156–171](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:156>) |
| 最终 | 5 | loadDetail | [src/pages/internship/form.vue:1–128](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/form.vue:1>) |
| 最终 | 6 | 文本块 | [src/pages/rating/index.vue:1–57](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:1>) |
| 最终 | 7 | 文本块 | [src/pages/internship/students.vue:1–56](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/students.vue:1>) |
| 最终 | 8 | saveAttendance | [src/api/practice.js:119–125](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:119>) |
| 最终 | 9 | 文本块 | [src/pages/basic/internship.vue:1–115](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 最终 | 10 | 文本块 | [src/pages/internship/index.vue:1–34](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/rating/form.vue · handleSubmit](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/form.vue:91>) | 无 | 无 |

查询耗时：4034.8 → 3585 毫秒。

## web-1623-05 · 1623-mobile

```text
search_code("选中指导老师后，怎样自动带出老师的姓名和联系电话？")
```

搜索意图：指导教师选择后回填关联资料

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | handleTeacherSelect | [src/pages/basic/internship.vue:80–95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) |
| 基线 | 2 | 文本块 | [src/pages/basic/internship.vue:1–60](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 基线 | 3 | getTeacherList | [src/api/practice.js:57–62](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:57>) |
| 基线 | 4 | 文本块 | [src/pages/internship/index.vue:26–34](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:26>) |
| 基线 | 5 | 文本块 | [src/pages/basic/index.vue:1–49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:1>) |
| 基线 | 6 | 文本块 | [src/pages/index/index.vue:1–49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) |
| 基线 | 7 | 文本块 | [src/pages/index/index.vue:68–74](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) |
| 基线 | 8 | handleSelect | [src/pages/rating/index.vue:36–42](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:36>) |
| 基线 | 9 | 文本块 | [src/pages/login/role.vue:53–58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:53>) |
| 基线 | 10 | loadHomeData | [src/pages/index/index.vue:81–88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:81>) |
| 最终 | 1 | handleTeacherSelect | [src/pages/basic/internship.vue:80–95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) |
| 最终 | 2 | 文本块 | [src/pages/basic/internship.vue:1–73](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:1>) |
| 最终 | 3 | loadTeacherList | [src/pages/basic/internship.vue:75–78](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:75>) |
| 最终 | 4 | getTeacherList | [src/api/practice.js:57–62](</Users/teriri/WebstormProjects/1623-mobile/src/api/practice.js:57>) |
| 最终 | 5 | 文本块 | [src/pages/internship/index.vue:26–34](</Users/teriri/WebstormProjects/1623-mobile/src/pages/internship/index.vue:26>) |
| 最终 | 6 | 文本块 | [src/pages/index/index.vue:1–88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) |
| 最终 | 7 | 文本块 | [src/pages/basic/index.vue:1–49](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/index.vue:1>) |
| 最终 | 8 | handleSelect | [src/pages/rating/index.vue:36–42](</Users/teriri/WebstormProjects/1623-mobile/src/pages/rating/index.vue:36>) |
| 最终 | 9 | 文本块 | [src/pages/login/role.vue:53–58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:53>) |
| 最终 | 10 | 文本块 | [src/pages.json:1–120](</Users/teriri/WebstormProjects/1623-mobile/src/pages.json:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/basic/internship.vue · handleTeacherSelect](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) | 无 | 无 |

查询耗时：3441.2 → 4832.6 毫秒。

## web-1623-06 · 1623-mobile

```text
search_code("用手机号密码登录时，怎样携带学生或老师身份并在成功后记住账号？")
```

搜索意图：带角色账号登录和会话保存

原冻结声明锚点覆盖名次：**2 → 2**；原冻结关键行为联合覆盖：**2 → 2**。

审计关键行为联合覆盖：**2 → 2**；冻结范围全部非空行覆盖：2 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/pages/login/account.vue:68–76](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:68>) |
| 基线 | 2 | handleLogin | [src/pages/login/account.vue:104–132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) |
| 基线 | 3 | loginByPhone | [src/components/LoginWrapper/index.vue:242–262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) |
| 基线 | 4 | 文本块 | [src/pages/login/role.vue:53–58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:53>) |
| 基线 | 5 | 文本块 | [src/pages/index/index.vue:68–80](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:68>) |
| 基线 | 6 | 文本块 | [src/components/LoginWrapper/index.vue:58–115](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:58>) |
| 基线 | 7 | getWechatMobile | [src/api/common.js:6–12](</Users/teriri/WebstormProjects/1623-mobile/src/api/common.js:6>) |
| 基线 | 8 | handleTeacherSelect | [src/pages/basic/internship.vue:80–95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) |
| 基线 | 9 | handleEnter | [src/pages/login/role.vue:60–62](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:60>) |
| 基线 | 10 | useWechatMobileLogin | [src/hooks/useWechatLogin.js:53–67](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatLogin.js:53>) |
| 最终 | 1 | 文本块 | [src/pages/login/account.vue:1–94](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:1>) |
| 最终 | 2 | handleLogin | [src/pages/login/account.vue:104–132](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) |
| 最终 | 3 | loginByPhone | [src/components/LoginWrapper/index.vue:242–262](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:242>) |
| 最终 | 4 | 文本块 | [src/pages/login/role.vue:53–58](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:53>) |
| 最终 | 5 | 文本块 | [src/pages/index/index.vue:1–88](</Users/teriri/WebstormProjects/1623-mobile/src/pages/index/index.vue:1>) |
| 最终 | 6 | 文本块 | [src/components/LoginWrapper/index.vue:1–201](</Users/teriri/WebstormProjects/1623-mobile/src/components/LoginWrapper/index.vue:1>) |
| 最终 | 7 | getWechatMobile | [src/api/common.js:6–12](</Users/teriri/WebstormProjects/1623-mobile/src/api/common.js:6>) |
| 最终 | 8 | handleTeacherSelect | [src/pages/basic/internship.vue:80–95](</Users/teriri/WebstormProjects/1623-mobile/src/pages/basic/internship.vue:80>) |
| 最终 | 9 | handleEnter | [src/pages/login/role.vue:60–62](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/role.vue:60>) |
| 最终 | 10 | useWechatMobileLogin | [src/hooks/useWechatLogin.js:53–67](</Users/teriri/WebstormProjects/1623-mobile/src/hooks/useWechatLogin.js:53>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/login/account.vue · handleLogin](</Users/teriri/WebstormProjects/1623-mobile/src/pages/login/account.vue:104>) | 无 | 无 |

查询耗时：6043.8 → 4840.9 毫秒。

## web-1321-01 · 1321_frontend

```text
search_code("预订蛋糕时怎样只提交数量大于零的品种，并带上选定的取货日期？")
```

搜索意图：按日期汇总非零数量提交蛋糕预订

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | handleUpload | [src/pages/book/index.vue:280–363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) |
| 基线 | 2 | handleUpload | [src/pages/item/index.vue:336–450](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:336>) |
| 基线 | 3 | apiIndexBookingCake | [src/api/index.js:168–174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) |
| 基线 | 4 | 文本块 | [src/pages/book/index.vue:45–96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) |
| 基线 | 5 | 文本块 | [src/pages/stock/index.vue:533–647](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:533>) |
| 基线 | 6 | 文本块 | [src/pages/stock/index.vue:416–473](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:416>) |
| 基线 | 7 | apiIndexBookingArticles | [src/api/index.js:249–255](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:249>) |
| 基线 | 8 | 文本块 | [src/pages/delivery/index.vue:48–99](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:48>) |
| 基线 | 9 | apiIndexCakeList | [src/api/index.js:138–144](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:138>) |
| 基线 | 10 | 文本块 | [src/language/zh-CN.js:1–116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:1>) |
| 最终 | 1 | handleUpload | [src/pages/book/index.vue:280–432](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) |
| 最终 | 2 | handleUpload | [src/pages/item/index.vue:309–512](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:309>) |
| 最终 | 3 | apiIndexBookingCake | [src/api/index.js:168–174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) |
| 最终 | 4 | 文本块 | [src/pages/book/index.vue:1–103](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:1>) |
| 最终 | 5 | 文本块 | [src/pages/book/index.vue:105–221](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:105>) |
| 最终 | 6 | 文本块 | [src/pages/stock/index.vue:523–665](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:523>) |
| 最终 | 7 | 文本块 | [src/pages/stock/index.vue:416–473](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:416>) |
| 最终 | 8 | 文本块 | [src/pages/delivery/index.vue:48–99](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:48>) |
| 最终 | 9 | apiIndexCakeList | [src/api/index.js:138–144](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:138>) |
| 最终 | 10 | apiIndexBookingArticles | [src/api/index.js:249–255](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:249>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/book/index.vue · handleUpload](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:286>) | 无 | 无 |

查询耗时：5823.5 → 5313 毫秒。

## web-1321-02 · 1321_frontend

```text
search_code("盘点时新增一个损坏蛋糕，怎样扣减可用数量，并防止剩余库存变成负数？")
```

搜索意图：损坏数量调整时限制可用库存

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | handleDamageIncrease | [src/pages/stock/index.vue:459–472](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>) |
| 基线 | 2 | 文本块 | [src/pages/stock/index.vue:474–532](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) |
| 基线 | 3 | default.methods.handleChangeNumber | [src/components/SkuSelector/index.vue:707–716](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:707>) |
| 基线 | 4 | apiIndexEditStock | [src/api/index.js:413–419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) |
| 基线 | 5 | decrease | [src/components/QuantityControl/index.vue:55–61](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:55>) |
| 基线 | 6 | 文本块 | [src/pages/book/index.vue:45–104](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) |
| 基线 | 7 | totalQuantity | [src/pages/book/index.vue:198–208](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:198>) |
| 基线 | 8 | 文本块 | [src/pages/statistics/index.vue:54–225](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) |
| 基线 | 9 | increase | [src/components/QuantityControl/index.vue:63–69](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:63>) |
| 基线 | 10 | apiIndexBookingCake | [src/api/index.js:168–174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) |
| 最终 | 1 | handleDamageIncrease | [src/pages/stock/index.vue:459–472](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>) |
| 最终 | 2 | 文本块 | [src/pages/stock/index.vue:474–703](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) |
| 最终 | 3 | 文本块 | [src/pages/stock/index.vue:275–429](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:275>) |
| 最终 | 4 | updateStockAfterQuantityChange | [src/pages/stock/index.vue:451–456](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:451>) |
| 最终 | 5 | default.methods.handleChangeNumber | [src/components/SkuSelector/index.vue:707–716](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:707>) |
| 最终 | 6 | apiIndexEditStock | [src/api/index.js:413–419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) |
| 最终 | 7 | decrease | [src/components/QuantityControl/index.vue:50–61](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:50>) |
| 最终 | 8 | 文本块 | [src/pages/book/index.vue:45–96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) |
| 最终 | 9 | totalQuantity | [src/pages/book/index.vue:198–208](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:198>) |
| 最终 | 10 | 文本块 | [src/pages/statistics/index.vue:54–225](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/stock/index.vue · handleDamageIncrease](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:459>) | 无 | 无 |

查询耗时：4454.6 → 8864.8 毫秒。

## web-1321-03 · 1321_frontend

```text
search_code("库存盘点保存时怎样找出真正改动的蛋糕，并一并提交剩余、损坏和过期数量？")
```

搜索意图：检测库存字段变化并批量保存

原冻结声明锚点覆盖名次：**3 → 3**；原冻结关键行为联合覆盖：**未覆盖 → 3**。

审计关键行为联合覆盖：**未覆盖 → 3**；冻结范围全部非空行覆盖：未覆盖 → 3。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/pages/stock/index.vue:308–360](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:308>) |
| 基线 | 2 | isValueModified | [src/pages/stock/index.vue:512–520](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:512>) |
| 基线 | 3 | 文本块 | [src/pages/stock/index.vue:533–587](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:533>) |
| 基线 | 4 | apiIndexEditStock | [src/api/index.js:413–419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) |
| 基线 | 5 | 文本块 | [src/pages/delivery/index.vue:270–320](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:270>) |
| 基线 | 6 | 文本块 | [src/pages/statistics/index.vue:54–167](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) |
| 基线 | 7 | 文本块 | [src/pages/book/index.vue:45–96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) |
| 基线 | 8 | 文本块 | [src/pages/delivery/index.vue:100–129](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:100>) |
| 基线 | 9 | 文本块 | [src/language/zh-CN.js:61–116](</Users/teriri/WebstormProjects/1321_frontend/src/language/zh-CN.js:61>) |
| 基线 | 10 | apiIndexBookingCake | [src/api/index.js:168–174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) |
| 最终 | 1 | 文本块 | [src/pages/stock/index.vue:275–360](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:275>) |
| 最终 | 2 | isValueModified | [src/pages/stock/index.vue:512–520](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:512>) |
| 最终 | 3 | 文本块 | [src/pages/stock/index.vue:523–733](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:523>) |
| 最终 | 4 | toggleItemDetails | [src/pages/stock/index.vue:437–448](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:437>) |
| 最终 | 5 | apiIndexEditStock | [src/api/index.js:413–419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) |
| 最终 | 6 | 文本块 | [src/pages/delivery/index.vue:270–377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:270>) |
| 最终 | 7 | 文本块 | [src/pages/statistics/index.vue:54–167](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:54>) |
| 最终 | 8 | 文本块 | [src/pages/book/index.vue:45–96](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:45>) |
| 最终 | 9 | 文本块 | [src/pages/delivery/index.vue:48–136](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:48>) |
| 最终 | 10 | apiIndexBookingCake | [src/api/index.js:168–174](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:168>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/stock/index.vue · handleConfirmChanges](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:575>) | 589、591–593、624–626、649 | 无 |

查询耗时：4295.9 → 4677.4 毫秒。

## web-1321-04 · 1321_frontend

```text
search_code("核对送货数量后，怎样只保存修改过的商品并刷新当日送货清单？")
```

搜索意图：配送核对保存差异数量

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | handleConfirmChanges | [src/pages/delivery/index.vue:306–377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>) |
| 基线 | 2 | handleVerify | [src/pages/delivery/index.vue:271–286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:271>) |
| 基线 | 3 | loadDeliveryData | [src/pages/index/index.vue:59–133](</Users/teriri/WebstormProjects/1321_frontend/src/pages/index/index.vue:59>) |
| 基线 | 4 | handleConfirmChanges | [src/pages/stock/index.vue:575–665](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:575>) |
| 基线 | 5 | toggleItemDetails | [src/pages/stock/index.vue:437–448](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:437>) |
| 基线 | 6 | apiIndexEditSupplyOrder | [src/api/index.js:357–363](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:357>) |
| 基线 | 7 | apiIndexEditStock | [src/api/index.js:413–419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) |
| 基线 | 8 | apiIndexIndex | [src/api/index.js:112–118](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:112>) |
| 基线 | 9 | 文本块 | [src/language/en-US.js:61–116](</Users/teriri/WebstormProjects/1321_frontend/src/language/en-US.js:61>) |
| 基线 | 10 | handleUpload | [src/pages/item/index.vue:367–450](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) |
| 最终 | 1 | handleConfirmChanges | [src/pages/delivery/index.vue:306–377](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>) |
| 最终 | 2 | handleVerify | [src/pages/delivery/index.vue:271–286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:271>) |
| 最终 | 3 | 文本块 | [src/pages/delivery/index.vue:1–229](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:1>) |
| 最终 | 4 | 文本块 | [src/pages/delivery/index.vue:289–303](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:289>) |
| 最终 | 5 | loadDeliveryData | [src/pages/index/index.vue:1–224](</Users/teriri/WebstormProjects/1321_frontend/src/pages/index/index.vue:1>) |
| 最终 | 6 | handleConfirmChanges | [src/pages/stock/index.vue:474–665](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) |
| 最终 | 7 | toggleItemDetails | [src/pages/stock/index.vue:437–448](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:437>) |
| 最终 | 8 | apiIndexEditSupplyOrder | [src/api/index.js:357–363](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:357>) |
| 最终 | 9 | apiIndexEditStock | [src/api/index.js:413–419](</Users/teriri/WebstormProjects/1321_frontend/src/api/index.js:413>) |
| 最终 | 10 | modifiedCount | [src/pages/stock/index.vue:291–304](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:291>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/delivery/index.vue · handleConfirmChanges](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:306>) | 无 | 无 |

查询耗时：4263.1 → 6286 毫秒。

## web-1321-05 · 1321_frontend

```text
search_code("统计页点本周后，如何算出周一到周日的范围并自动重新查询？")
```

搜索意图：本周快捷时间范围及刷新

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | selectQuickDate | [src/pages/statistics/index.vue:180–203](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>) |
| 基线 | 2 | 文本块 | [src/pages/statistics/index.vue:284–341](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:284>) |
| 基线 | 3 | generateWeekDates | [src/pages/stock/index.vue:174–198](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:174>) |
| 基线 | 4 | generateWeekDates | [src/pages/item/index.vue:164–188](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:164>) |
| 基线 | 5 | generateWeekDates | [src/pages/book/index.vue:106–131](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:106>) |
| 基线 | 6 | 文本块 | [src/pages/stock/index.vue:201–207](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:201>) |
| 基线 | 7 | 文本块 | [src/pages/item/index.vue:191–197](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:191>) |
| 基线 | 8 | 文本块 | [src/pages/book/index.vue:133–142](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:133>) |
| 基线 | 9 | 文本块 | [src/pages/index/index.vue:1–58](</Users/teriri/WebstormProjects/1321_frontend/src/pages/index/index.vue:1>) |
| 基线 | 10 | 文本块 | [src/pages/delivery/index.vue:130–136](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:130>) |
| 最终 | 1 | selectQuickDate | [src/pages/statistics/index.vue:180–203](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>) |
| 最终 | 2 | 文本块 | [src/pages/statistics/index.vue:272–438](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:272>) |
| 最终 | 3 | 文本块 | [src/pages/statistics/index.vue:1–133](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:1>) |
| 最终 | 4 | generateDateRange | [src/pages/statistics/index.vue:206–216](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:206>) |
| 最终 | 5 | generateWeekDates | [src/pages/stock/index.vue:174–198](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:174>) |
| 最终 | 6 | generateWeekDates | [src/pages/item/index.vue:1–188](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:1>) |
| 最终 | 7 | generateWeekDates | [src/pages/book/index.vue:106–131](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:106>) |
| 最终 | 8 | 文本块 | [src/pages/stock/index.vue:201–207](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:201>) |
| 最终 | 9 | 文本块 | [src/pages/item/index.vue:191–197](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:191>) |
| 最终 | 10 | 文本块 | [src/pages/book/index.vue:133–142](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:133>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/pages/statistics/index.vue · selectQuickDate](</Users/teriri/WebstormProjects/1321_frontend/src/pages/statistics/index.vue:180>) | 无 | 无 |

查询耗时：3917.6 → 5809.2 毫秒。

## web-1321-06 · 1321_frontend

```text
search_code("手动输入商品数量后，怎样处理空值和非法数字，并把超出范围的值限制到上下限？")
```

搜索意图：数量输入失焦校验与边界约束

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | handleInputBlur | [src/components/QuantityControl/index.vue:81–97](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>) |
| 基线 | 2 | 文本块 | [src/components/QuantityControl/index.vue:1–54](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:1>) |
| 基线 | 3 | default.methods.handleChangeNumber | [src/components/SkuSelector/index.vue:707–716](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:707>) |
| 基线 | 4 | 文本块 | [src/components/SkuSelector/index.vue:1–120](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:1>) |
| 基线 | 5 | handleVerify | [src/pages/delivery/index.vue:271–286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:271>) |
| 基线 | 6 | handleUpload | [src/pages/item/index.vue:367–444](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) |
| 基线 | 7 | updateStockAfterQuantityChange | [src/pages/stock/index.vue:451–456](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:451>) |
| 基线 | 8 | 文本块 | [src/pages/book/index.vue:280–363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) |
| 基线 | 9 | 文本块 | [src/pages/stock/index.vue:474–539](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:474>) |
| 基线 | 10 | 文本块 | [src/pages/item/index.vue:49–133](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:49>) |
| 最终 | 1 | handleInputBlur | [src/components/QuantityControl/index.vue:81–158](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>) |
| 最终 | 2 | 文本块 | [src/components/QuantityControl/index.vue:1–61](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:1>) |
| 最终 | 3 | increase | [src/components/QuantityControl/index.vue:63–69](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:63>) |
| 最终 | 4 | default.methods.handleChangeNumber | [src/components/SkuSelector/index.vue:707–716](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:707>) |
| 最终 | 5 | 文本块 | [src/components/SkuSelector/index.vue:1–120](</Users/teriri/WebstormProjects/1321_frontend/src/components/SkuSelector/index.vue:1>) |
| 最终 | 6 | handleVerify | [src/pages/delivery/index.vue:271–286](</Users/teriri/WebstormProjects/1321_frontend/src/pages/delivery/index.vue:271>) |
| 最终 | 7 | startEdit | [src/components/QuantityControl/index.vue:71–79](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:71>) |
| 最终 | 8 | handleUpload | [src/pages/item/index.vue:367–512](</Users/teriri/WebstormProjects/1321_frontend/src/pages/item/index.vue:367>) |
| 最终 | 9 | updateStockAfterQuantityChange | [src/pages/stock/index.vue:451–456](</Users/teriri/WebstormProjects/1321_frontend/src/pages/stock/index.vue:451>) |
| 最终 | 10 | 文本块 | [src/pages/book/index.vue:280–363](</Users/teriri/WebstormProjects/1321_frontend/src/pages/book/index.vue:280>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/components/QuantityControl/index.vue · handleInputBlur](</Users/teriri/WebstormProjects/1321_frontend/src/components/QuantityControl/index.vue:81>) | 无 | 无 |

查询耗时：4558.6 → 4944.9 毫秒。

## web-1305-01 · 1305_pc

```text
search_code("新增预约时，怎样先核验会员、服务和员工，再把预约时间提交给服务端？")
```

搜索意图：预约创建的实体核验和服务端保存链路

原冻结声明锚点覆盖名次：**未覆盖 → 8**；原冻结关键行为联合覆盖：**未覆盖 → 8**。

审计关键行为联合覆盖：**5 → 2**；冻结范围全部非空行覆盖：5 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/views/appointment-list/index.vue:302–412](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:302>) |
| 基线 | 2 | resolveAppointmentCreateContext | [src/views/appointment-board/index.vue:882–955](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:882>) |
| 基线 | 3 | useBusinessStore.createAppointment | [src/store/useBusinessStore.js:6071–6111](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6071>) |
| 基线 | 4 | 文本块 | [src/views/appointment-board/index.vue:184–243](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:184>) |
| 基线 | 5 | submitAppointmentCreateApi | [src/views/appointment-board/index.vue:963–986](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>) |
| 基线 | 6 | 文本块 | [src/views/appointment-board/index.vue:302–361](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) |
| 基线 | 7 | useBusinessStore.resolveAppointmentMemberContext | [src/store/useBusinessStore.js:6032–6041](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6032>) |
| 基线 | 8 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121–6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) |
| 基线 | 9 | 文本块 | [src/views/member-list/index.vue:853–907](</Users/teriri/WebstormProjects/1305_pc/src/views/member-list/index.vue:853>) |
| 基线 | 10 | submitForm | [src/views/member-list/index.vue:924–945](</Users/teriri/WebstormProjects/1305_pc/src/views/member-list/index.vue:924>) |
| 最终 | 1 | 文本块 | [src/views/appointment-list/index.vue:302–512](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:302>) |
| 最终 | 2 | resolveAppointmentCreateContext | [src/views/appointment-board/index.vue:825–986](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:825>) |
| 最终 | 3 | useBusinessStore.createAppointment | [src/store/useBusinessStore.js:6071–6111](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6071>) |
| 最终 | 4 | 文本块 | [src/views/appointment-board/index.vue:184–243](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:184>) |
| 最终 | 5 | 文本块 | [src/views/appointment-board/index.vue:302–361](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) |
| 最终 | 6 | saveAppointment | [src/views/appointment-board/index.vue:1135–1251](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1135>) |
| 最终 | 7 | rules | [src/views/appointment-list/index.vue:214–222](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:214>) |
| 最终 | 8 | submitForm | [src/views/appointment-list/index.vue:537–596](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:537>) |
| 最终 | 9 | useBusinessStore.resolveAppointmentMemberContext | [src/store/useBusinessStore.js:6032–6041](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6032>) |
| 最终 | 10 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121–6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/views/appointment-list/index.vue · submitForm](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:543>) | 543–544、551–552、554、559 | 无 |
| 原冻结 | [src/views/appointment-list/index.vue · buildAppointmentCreateParams](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:366>) | 无 | 无 |
| 审计替代组 appointment-board | [src/views/appointment-board/index.vue · resolveAppointmentCreateContext](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:918>) | 无 | 无 |
| 审计替代组 appointment-board | [src/views/appointment-board/index.vue · submitAppointmentCreateApi](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:963>) | 无 | 无 |

审计说明：增加已审计的看板双方法替代组；不得仅返回校验方法就算完成创建提交。

查询耗时：4109.1 → 4769.1 毫秒。

## web-1305-02 · 1305_pc

```text
search_code("用户取消预约并确认后，怎样更新服务器状态并刷新预约列表？")
```

搜索意图：确认取消预约、后端状态写入和刷新

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | cancelRow | [src/views/appointment-list/index.vue:604–620](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>) |
| 基线 | 2 | submitAppointmentCancelApi | [src/views/appointment-board/index.vue:994–1003](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:994>) |
| 基线 | 3 | 文本块 | [src/views/appointment-list/index.vue:622–677](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:622>) |
| 基线 | 4 | 文本块 | [src/views/appointment-board/index.vue:1195–1201](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1195>) |
| 基线 | 5 | toggleStatus | [src/views/cashier-advance/index.vue:490–509](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-advance/index.vue:490>) |
| 基线 | 6 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121–6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) |
| 基线 | 7 | toggleStatus | [src/views/document-consume/index.vue:636–648](</Users/teriri/WebstormProjects/1305_pc/src/views/document-consume/index.vue:636>) |
| 基线 | 8 | toggleStatus | [src/views/document-package-sale/index.vue:635–647](</Users/teriri/WebstormProjects/1305_pc/src/views/document-package-sale/index.vue:635>) |
| 基线 | 9 | toggleStatus | [src/views/document-card-refund/index.vue:514–526](</Users/teriri/WebstormProjects/1305_pc/src/views/document-card-refund/index.vue:514>) |
| 基线 | 10 | toggleStatus | [src/views/document-return/index.vue:525–537](</Users/teriri/WebstormProjects/1305_pc/src/views/document-return/index.vue:525>) |
| 最终 | 1 | cancelRow | [src/views/appointment-list/index.vue:537–677](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:537>) |
| 最终 | 2 | submitAppointmentCancelApi | [src/views/appointment-board/index.vue:918–1003](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:918>) |
| 最终 | 3 | 文本块 | [src/views/appointment-board/index.vue:1042–1251](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1042>) |
| 最终 | 4 | toggleStatus | [src/views/cashier-advance/index.vue:490–509](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-advance/index.vue:490>) |
| 最终 | 5 | useBusinessStore.updateAppointment | [src/store/useBusinessStore.js:6121–6151](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:6121>) |
| 最终 | 6 | 文本块 | [src/views/appointment-board/index.vue:302–361](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:302>) |
| 最终 | 7 | 文本块 | [src/views/appointment-board/index.vue:144–183](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:144>) |
| 最终 | 8 | toggleStatus | [src/views/document-consume/index.vue:636–648](</Users/teriri/WebstormProjects/1305_pc/src/views/document-consume/index.vue:636>) |
| 最终 | 9 | toggleStatus | [src/views/document-package-sale/index.vue:635–647](</Users/teriri/WebstormProjects/1305_pc/src/views/document-package-sale/index.vue:635>) |
| 最终 | 10 | 文本块 | [src/views/appointment-list/index.vue:1–43](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/views/appointment-list/index.vue · cancelRow](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-list/index.vue:604>) | 无 | 无 |

查询耗时：3512.2 → 3928.3 毫秒。

## web-1305-03 · 1305_pc

```text
search_code("组合收款时，怎样把会员卡、次卡和普通支付统一转换成接口需要的支付配置和金额？")
```

搜索意图：不同支付类型合并为服务端支付明细

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | buildOrderSettlementApiPayArr | [src/views/order-settlement/index.vue:864–878](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>) |
| 基线 | 2 | 文本块 | [src/views/order-settlement/index.vue:791–850](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:791>) |
| 基线 | 3 | buildCardOpenApiPayArr | [src/views/business-flow/index.vue:1586–1633](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:1586>) |
| 基线 | 4 | 文本块 | [src/views/business-flow/index.vue:2490–2496](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2490>) |
| 基线 | 5 | 文本块 | [src/views/business-flow/index.vue:2726–2788](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2726>) |
| 基线 | 6 | 文本块 | [src/views/cashier-counter/index.vue:587–635](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-counter/index.vue:587>) |
| 基线 | 7 | useBusinessStore.normalizePackageBuyPaymentLines | [src/store/useBusinessStore.js:5288–5315](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:5288>) |
| 基线 | 8 | 文本块 | [src/views/cashier-counter/index.vue:513–532](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-counter/index.vue:513>) |
| 基线 | 9 | useBusinessStore.settleOrder | [src/store/useBusinessStore.js:4550–4699](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4550>) |
| 基线 | 10 | 文本块 | [src/views/home-action/index.vue:2076–2132](</Users/teriri/WebstormProjects/1305_pc/src/views/home-action/index.vue:2076>) |
| 最终 | 1 | buildOrderSettlementApiPayArr | [src/views/order-settlement/index.vue:858–878](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:858>) |
| 最终 | 2 | 文本块 | [src/views/order-settlement/index.vue:791–850](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:791>) |
| 最终 | 3 | buildCardOpenApiPayArr | [src/views/business-flow/index.vue:1586–1633](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:1586>) |
| 最终 | 4 | 文本块 | [src/views/business-flow/index.vue:2490–2496](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2490>) |
| 最终 | 5 | 文本块 | [src/views/business-flow/index.vue:2725–2796](</Users/teriri/WebstormProjects/1305_pc/src/views/business-flow/index.vue:2725>) |
| 最终 | 6 | 文本块 | [src/views/cashier-counter/index.vue:587–635](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-counter/index.vue:587>) |
| 最终 | 7 | useBusinessStore.normalizePackageBuyPaymentLines | [src/store/useBusinessStore.js:5288–5315](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:5288>) |
| 最终 | 8 | 文本块 | [src/views/cashier-counter/index.vue:513–532](</Users/teriri/WebstormProjects/1305_pc/src/views/cashier-counter/index.vue:513>) |
| 最终 | 9 | useBusinessStore.settleOrder | [src/store/useBusinessStore.js:4550–4699](</Users/teriri/WebstormProjects/1305_pc/src/store/useBusinessStore.js:4550>) |
| 最终 | 10 | 文本块 | [src/views/home-action/index.vue:2076–2180](</Users/teriri/WebstormProjects/1305_pc/src/views/home-action/index.vue:2076>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/views/order-settlement/index.vue · buildOrderSettlementApiPayArr](</Users/teriri/WebstormProjects/1305_pc/src/views/order-settlement/index.vue:864>) | 无 | 无 |

查询耗时：4380.7 → 6858.9 毫秒。

## web-1305-04 · 1305_pc

```text
search_code("员工接口返回的权限规则，怎样只保留已启用项并转换成桌面菜单能识别的权限？")
```

搜索意图：启用权限筛选及菜单映射

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | resolvePcPermissionKeys | [src/utils/pcAdminSession.js:308–317](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>) |
| 基线 | 2 | 文本块 | [src/views/employee-permission/index.vue:117–219](</Users/teriri/WebstormProjects/1305_pc/src/views/employee-permission/index.vue:117>) |
| 基线 | 3 | syncStaffPermissions | [src/utils/pcAdminSession.js:448–455](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:448>) |
| 基线 | 4 | employeeDefaultPermissions | [src/config/pcAdmin.js:41–179](</Users/teriri/WebstormProjects/1305_pc/src/config/pcAdmin.js:41>) |
| 基线 | 5 | backendAuthorizedMenuKeys | [src/config/pcAdmin.js:17–25](</Users/teriri/WebstormProjects/1305_pc/src/config/pcAdmin.js:17>) |
| 基线 | 6 | flattenMenuItems | [src/router/system.js:637–655](</Users/teriri/WebstormProjects/1305_pc/src/router/system.js:637>) |
| 基线 | 7 | normalizeEmployeePermissions | [src/store/useAdminStore.js:74–76](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:74>) |
| 基线 | 8 | useAdminStore.syncEmployeePermissionsFromApi | [src/store/useAdminStore.js:242–244](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:242>) |
| 基线 | 9 | useAdminStore.setEmployeePermissions | [src/store/useAdminStore.js:233–235](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:233>) |
| 基线 | 10 | resolvePcPermissionKeysByRuleName | [src/utils/pcAdminSession.js:297–300](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:297>) |
| 最终 | 1 | resolvePcPermissionKeys | [src/utils/pcAdminSession.js:308–317](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>) |
| 最终 | 2 | 文本块 | [src/views/employee-permission/index.vue:1–219](</Users/teriri/WebstormProjects/1305_pc/src/views/employee-permission/index.vue:1>) |
| 最终 | 3 | syncStaffPermissions | [src/utils/pcAdminSession.js:448–455](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:448>) |
| 最终 | 4 | employeeDefaultPermissions | [src/config/pcAdmin.js:41–179](</Users/teriri/WebstormProjects/1305_pc/src/config/pcAdmin.js:41>) |
| 最终 | 5 | backendAuthorizedMenuKeys | [src/config/pcAdmin.js:17–25](</Users/teriri/WebstormProjects/1305_pc/src/config/pcAdmin.js:17>) |
| 最终 | 6 | flattenMenuItems | [src/router/system.js:637–655](</Users/teriri/WebstormProjects/1305_pc/src/router/system.js:637>) |
| 最终 | 7 | normalizeEmployeePermissions | [src/store/useAdminStore.js:74–76](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:74>) |
| 最终 | 8 | useAdminStore.syncEmployeePermissionsFromApi | [src/store/useAdminStore.js:242–244](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:242>) |
| 最终 | 9 | useAdminStore.setEmployeePermissions | [src/store/useAdminStore.js:233–235](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:233>) |
| 最终 | 10 | resolvePcPermissionKeysByRuleName | [src/utils/pcAdminSession.js:297–300](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:297>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/utils/pcAdminSession.js · resolvePcPermissionKeys](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:308>) | 无 | 无 |

查询耗时：4181.6 → 5093.7 毫秒。

## web-1305-05 · 1305_pc

```text
search_code("多个页面同时读取登录后的门店资料时，怎样避免重复请求并复用短时间缓存？")
```

搜索意图：会话资料读取中的缓存与并发请求合并

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | ensurePcAdminSession | [src/utils/pcAdminSession.js:520–543](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>) |
| 基线 | 2 | syncPcBusinessResources | [src/utils/pcBusinessResources.js:2703–2728](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:2703>) |
| 基线 | 3 | syncPcAdminSession | [src/utils/pcAdminSession.js:462–511](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:462>) |
| 基线 | 4 | pageDescription | [src/views/auth/AuthPage.vue:155–163](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:155>) |
| 基线 | 5 | resetPcBusinessResourcesCache | [src/utils/pcBusinessResources.js:2755–2758](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:2755>) |
| 基线 | 6 | 文本块 | [src/views/auth/AuthPage.vue:292–299](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:292>) |
| 基线 | 7 | resetPcAdminSessionCache | [src/utils/pcAdminSession.js:548–550](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:548>) |
| 基线 | 8 | 文本块 | [src/views/appointment-board/index.vue:1080–1117](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1080>) |
| 基线 | 9 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:44–68](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:44>) |
| 基线 | 10 | 文本块 | [src/views/auth/AuthPage.vue:355–397](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:355>) |
| 最终 | 1 | ensurePcAdminSession | [src/utils/pcAdminSession.js:520–543](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>) |
| 最终 | 2 | syncPcBusinessResources | [src/utils/pcBusinessResources.js:2703–2728](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:2703>) |
| 最终 | 3 | syncPcAdminSession | [src/utils/pcAdminSession.js:462–511](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:462>) |
| 最终 | 4 | pageDescription | [src/views/auth/AuthPage.vue:155–163](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:155>) |
| 最终 | 5 | resetPcBusinessResourcesCache | [src/utils/pcBusinessResources.js:2755–2758](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:2755>) |
| 最终 | 6 | 文本块 | [src/views/auth/AuthPage.vue:292–303](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:292>) |
| 最终 | 7 | resetPcAdminSessionCache | [src/utils/pcAdminSession.js:548–550](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:548>) |
| 最终 | 8 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:30–98](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:30>) |
| 最终 | 9 | 文本块 | [src/views/appointment-board/index.vue:1080–1117](</Users/teriri/WebstormProjects/1305_pc/src/views/appointment-board/index.vue:1080>) |
| 最终 | 10 | 文本块 | [src/views/auth/AuthPage.vue:355–446](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:355>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/utils/pcAdminSession.js · ensurePcAdminSession](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:520>) | 无 | 无 |

查询耗时：4175.1 → 5497.4 毫秒。

## web-1305-06 · 1305_pc

```text
search_code("门店资料返回后，怎样区分未绑定、停用、已过期和可正常使用？")
```

搜索意图：从门店资料推导可用状态

原冻结声明锚点覆盖名次：**2 → 2**；原冻结关键行为联合覆盖：**2 → 2**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/views/auth/AuthPage.vue:305–354](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:305>) |
| 基线 | 2 | resolveStoreStatusFromPayload | [src/utils/pcAdminSession.js:159–174](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>) |
| 基线 | 3 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:50–68](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:50>) |
| 基线 | 4 | 文本块 | [src/views/auth/StoreStatusPage.vue:30–43](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:30>) |
| 基线 | 5 | resolveStoreStatusFromError | [src/utils/pcAdminSession.js:199–205](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:199>) |
| 基线 | 6 | useAdminStore.syncCurrentStoreFromApi | [src/store/useAdminStore.js:198–212](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:198>) |
| 基线 | 7 | resolveStoreCardStatus.statusMap | [src/utils/pcBusinessResources.js:1980–1986](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1980>) |
| 基线 | 8 | resolveTimeCardSaleStatus.statusMap | [src/utils/pcBusinessResources.js:1947–1953](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1947>) |
| 基线 | 9 | resolveUserPackageCardStatus.statusMap | [src/utils/pcBusinessResources.js:732–739](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:732>) |
| 基线 | 10 | isUsableMemberCard | [src/views/home-action/index.vue:1381–1391](</Users/teriri/WebstormProjects/1305_pc/src/views/home-action/index.vue:1381>) |
| 最终 | 1 | 文本块 | [src/views/auth/AuthPage.vue:292–447](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:292>) |
| 最终 | 2 | resolveStoreStatusFromPayload | [src/utils/pcAdminSession.js:159–174](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>) |
| 最终 | 3 | refreshStoreStatus | [src/views/auth/StoreStatusPage.vue:30–68](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/StoreStatusPage.vue:30>) |
| 最终 | 4 | resolveStoreStatusFromError | [src/utils/pcAdminSession.js:199–205](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:199>) |
| 最终 | 5 | useAdminStore.syncCurrentStoreFromApi | [src/store/useAdminStore.js:198–212](</Users/teriri/WebstormProjects/1305_pc/src/store/useAdminStore.js:198>) |
| 最终 | 6 | resolveStoreCardStatus.statusMap | [src/utils/pcBusinessResources.js:1980–1986](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1980>) |
| 最终 | 7 | resolveTimeCardSaleStatus.statusMap | [src/utils/pcBusinessResources.js:1947–1953](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:1947>) |
| 最终 | 8 | resolveUserPackageCardStatus.statusMap | [src/utils/pcBusinessResources.js:732–739](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcBusinessResources.js:732>) |
| 最终 | 9 | 文本块 | [src/views/auth/AuthPage.vue:228–234](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:228>) |
| 最终 | 10 | isUsableMemberCard | [src/views/home-action/index.vue:1381–1391](</Users/teriri/WebstormProjects/1305_pc/src/views/home-action/index.vue:1381>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/utils/pcAdminSession.js · resolveStoreStatusFromPayload](</Users/teriri/WebstormProjects/1305_pc/src/utils/pcAdminSession.js:159>) | 无 | 无 |
| 审计替代组 auth-store-status | [src/views/auth/AuthPage.vue · resolveStoreStatus](</Users/teriri/WebstormProjects/1305_pc/src/views/auth/AuthPage.vue:330>) | 无 | 无 |

审计说明：增加已审计的四态替代实现；仅适用于本条原始查询语义。

查询耗时：4113.5 → 4154.7 毫秒。

## web-1573-01 · 1573-frontend

```text
search_code("课程视频播放到互动节点时，哪里判断是否该暂停视频并弹出互动内容？")
```

搜索意图：视频播放阈值触发一次互动暂停

原冻结声明锚点覆盖名次：**3 → 3**；原冻结关键行为联合覆盖：**3 → 3**。

审计关键行为联合覆盖：**3 → 3**；冻结范围全部非空行覆盖：3 → 3。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75–89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) |
| 基线 | 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:111–226](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:111>) |
| 基线 | 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1–73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) |
| 基线 | 4 | 文本块 | [apps/web/metadata.json:1–6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) |
| 基线 | 5 | segments | [apps/web/src/pages/PackageDetail.vue:1–80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) |
| 基线 | 6 | 文本块 | [apps/web/src/pages/PackageList.vue:50–82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:50>) |
| 基线 | 7 | 文本块 | [apps/web/src/pages/ModelViewer.vue:22–28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:22>) |
| 基线 | 8 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97–98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) |
| 基线 | 9 | createJointControls | [apps/web/src/pages/ModelViewer.vue:103–110](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:103>) |
| 基线 | 10 | appendLog | [apps/web/src/pages/ModelViewer.vue:126–128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:126>) |
| 最终 | 1 | maybeOpenInteraction | [apps/web/src/pages/VideoSegment.vue:75–89](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) |
| 最终 | 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:111–343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:111>) |
| 最终 | 3 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1–73](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) |
| 最终 | 4 | 文本块 | [apps/web/metadata.json:1–6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/metadata.json:1>) |
| 最终 | 5 | segments | [apps/web/src/pages/PackageDetail.vue:1–80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) |
| 最终 | 6 | updateFullscreenState | [apps/web/src/pages/VideoSegment.vue:91–95](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:91>) |
| 最终 | 7 | 文本块 | [apps/web/src/pages/PackageList.vue:50–82](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:50>) |
| 最终 | 8 | 文本块 | [apps/web/src/pages/ModelViewer.vue:22–28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:22>) |
| 最终 | 9 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97–98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) |
| 最终 | 10 | appendLog | [apps/web/src/pages/ModelViewer.vue:126–128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:126>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [apps/web/src/pages/VideoSegment.vue · maybeOpenInteraction](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:75>) | 无 | 无 |
| 原冻结 | [apps/web/src/pages/VideoSegment.vue · openInteraction](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:69>) | 无 | 无 |

查询耗时：6472.5 → 4769 毫秒。

## web-1573-02 · 1573-frontend

```text
search_code("点击视频进度条时，如何把鼠标位置换算成播放时间并检查是否进入互动？")
```

搜索意图：视频进度条跳转和互动触发

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | seekVideo | [apps/web/src/pages/VideoSegment.vue:185–203](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>) |
| 基线 | 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1–110](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) |
| 基线 | 3 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132–140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) |
| 基线 | 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:280–334](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:280>) |
| 基线 | 5 | segments | [apps/web/src/pages/PackageDetail.vue:1–80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) |
| 基线 | 6 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97–98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) |
| 基线 | 7 | initializeScene.handleCanvasClick | [apps/web/src/pages/ModelViewer.vue:592–612](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:592>) |
| 基线 | 8 | handleMove | [apps/web/src/components/neural/SVGDraggableNumber.vue:37–47](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:37>) |
| 基线 | 9 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:21–28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:21>) |
| 基线 | 10 | handleMove | [apps/web/src/components/neural/DraggableNumber.vue:32–42](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>) |
| 最终 | 1 | seekVideo | [apps/web/src/pages/VideoSegment.vue:185–203](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>) |
| 最终 | 2 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1–121](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) |
| 最终 | 3 | handleVideoTimeUpdate | [apps/web/src/pages/VideoSegment.vue:132–140](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:132>) |
| 最终 | 4 | 文本块 | [apps/web/src/pages/VideoSegment.vue:205–343](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:205>) |
| 最终 | 5 | segments | [apps/web/src/pages/PackageDetail.vue:1–80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) |
| 最终 | 6 | initializeScene.handleCanvasClick | [apps/web/src/pages/ModelViewer.vue:592–612](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:592>) |
| 最终 | 7 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:97–98](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:97>) |
| 最终 | 8 | handleMove | [apps/web/src/components/neural/SVGDraggableNumber.vue:37–47](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:37>) |
| 最终 | 9 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:21–28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:21>) |
| 最终 | 10 | handleMove | [apps/web/src/components/neural/DraggableNumber.vue:32–42](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [apps/web/src/pages/VideoSegment.vue · seekVideo](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:185>) | 无 | 无 |

查询耗时：4355.6 → 6227.4 毫秒。

## web-1573-03 · 1573-frontend

```text
search_code("拖动样本或调整权重以后，神经网络演示怎样重新计算分类结果和正确率？")
```

搜索意图：响应式神经网络前向计算及分类准确率

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | processed | [apps/web/src/components/NeuralGridDemo.vue:1–155](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) |
| 基线 | 2 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:215–275](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:215>) |
| 基线 | 3 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:277–452](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:277>) |
| 基线 | 4 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:661–718](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:661>) |
| 基线 | 5 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:21–28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:21>) |
| 基线 | 6 | 文本块 | [apps/web/src/components/neural/DraggableNumber.vue:17–23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:17>) |
| 基线 | 7 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1–6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) |
| 基线 | 8 | handleUp | [apps/web/src/components/neural/DraggableNumber.vue:44–71](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:44>) |
| 基线 | 9 | handleUp | [apps/web/src/components/neural/SVGDraggableNumber.vue:49–59](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:49>) |
| 基线 | 10 | handleDown | [apps/web/src/components/neural/SVGDraggableNumber.vue:30–35](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:30>) |
| 最终 | 1 | processed | [apps/web/src/components/NeuralGridDemo.vue:1–155](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) |
| 最终 | 2 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:215–275](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:215>) |
| 最终 | 3 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:661–718](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:661>) |
| 最终 | 4 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:277–490](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:277>) |
| 最终 | 5 | 文本块 | [apps/web/src/components/neural/SVGDraggableNumber.vue:21–28](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:21>) |
| 最终 | 6 | 文本块 | [apps/web/src/components/neural/DraggableNumber.vue:17–23](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:17>) |
| 最终 | 7 | 文本块 | [apps/web/src/pages/VideoSegment.vue:1–44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:1>) |
| 最终 | 8 | handleUp | [apps/web/src/components/neural/DraggableNumber.vue:44–71](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:44>) |
| 最终 | 9 | handleUp | [apps/web/src/components/neural/SVGDraggableNumber.vue:49–81](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/SVGDraggableNumber.vue:49>) |
| 最终 | 10 | handleMove | [apps/web/src/components/neural/DraggableNumber.vue:32–42](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/neural/DraggableNumber.vue:32>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [apps/web/src/components/NeuralGridDemo.vue · processed](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:93>) | 无 | 无 |

查询耗时：3541.6 → 4249.4 毫秒。

## web-1573-04 · 1573-frontend

```text
search_code("浏览器刷新后，怎样从本地恢复上次登录的账号和凭证，数据损坏时又怎么处理？")
```

搜索意图：浏览器认证状态恢复及损坏数据清理

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | initializeAuth | [apps/web/src/context/auth.ts:28–41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) |
| 基线 | 2 | saveSession | [apps/web/src/context/auth.ts:43–49](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:43>) |
| 基线 | 3 | logout | [apps/web/src/context/auth.ts:76–83](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:76>) |
| 基线 | 4 | 文本块 | [apps/web/src/pages/Login.vue:1–6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:1>) |
| 基线 | 5 | 文本块 | [apps/web/src/pages/Login.vue:45–101](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:45>) |
| 基线 | 6 | 文本块 | [apps/web/src/pages/Login.vue:13–14](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:13>) |
| 基线 | 7 | 文本块 | [apps/api/src/app.ts:16–25](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:16>) |
| 基线 | 8 | 文本块 | [apps/api/src/auth.ts:15–16](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:15>) |
| 基线 | 9 | 文本块 | [apps/web/src/main.ts:1–9](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/main.ts:1>) |
| 基线 | 10 | 文本块 | [apps/api/src/auth.ts:60–66](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:60>) |
| 最终 | 1 | initializeAuth | [apps/web/src/context/auth.ts:28–41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) |
| 最终 | 2 | saveSession | [apps/web/src/context/auth.ts:43–49](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:43>) |
| 最终 | 3 | logout | [apps/web/src/context/auth.ts:76–83](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:76>) |
| 最终 | 4 | requestSession | [apps/web/src/context/auth.ts:51–66](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:51>) |
| 最终 | 5 | 文本块 | [apps/web/src/pages/Login.vue:1–6](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:1>) |
| 最终 | 6 | 文本块 | [apps/web/src/pages/Login.vue:41–101](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:41>) |
| 最终 | 7 | 文本块 | [apps/web/src/pages/Login.vue:13–14](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:13>) |
| 最终 | 8 | 文本块 | [apps/api/src/app.ts:16–25](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:16>) |
| 最终 | 9 | 文本块 | [apps/api/src/auth.ts:15–16](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:15>) |
| 最终 | 10 | 文本块 | [apps/web/src/main.ts:1–9](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/main.ts:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [apps/web/src/context/auth.ts · initializeAuth](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) | 无 | 无 |

查询耗时：3153.2 → 3117.5 毫秒。

## web-1573-05 · 1573-frontend

```text
search_code("服务端怎样检查登录令牌有没有被篡改或已经过期？")
```

搜索意图：令牌签名校验和过期拒绝

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | verifyJwt | [apps/api/src/auth.ts:83–106](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>) |
| 基线 | 2 | getUserByToken | [apps/api/src/auth.ts:117–138](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:117>) |
| 基线 | 3 | 文本块 | [apps/api/src/auth.ts:8–11](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:8>) |
| 基线 | 4 | initializeAuth | [apps/web/src/context/auth.ts:28–41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) |
| 基线 | 5 | AuthSession | [apps/web/src/context/auth.ts:8–11](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:8>) |
| 基线 | 6 | saveSession | [apps/web/src/context/auth.ts:43–49](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:43>) |
| 基线 | 7 | getWebSocketToken | [apps/api/src/app.ts:128–136](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:128>) |
| 基线 | 8 | app | [apps/api/src/app.ts:138–306](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:138>) |
| 基线 | 9 | env | [apps/api/src/env.ts:8–14](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/env.ts:8>) |
| 基线 | 10 | 文本块 | [apps/api/src/app.ts:16–25](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:16>) |
| 最终 | 1 | verifyJwt | [apps/api/src/auth.ts:83–106](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>) |
| 最终 | 2 | getUserByToken | [apps/api/src/auth.ts:117–138](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:117>) |
| 最终 | 3 | 文本块 | [apps/api/src/auth.ts:8–11](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:8>) |
| 最终 | 4 | isJwtPayload | [apps/api/src/auth.ts:46–58](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:46>) |
| 最终 | 5 | initializeAuth | [apps/web/src/context/auth.ts:28–41](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:28>) |
| 最终 | 6 | AuthSession | [apps/web/src/context/auth.ts:8–11](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:8>) |
| 最终 | 7 | saveSession | [apps/web/src/context/auth.ts:43–49](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:43>) |
| 最终 | 8 | getWebSocketToken | [apps/api/src/app.ts:128–136](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:128>) |
| 最终 | 9 | 文本块 | [apps/web/src/context/auth.ts:85–91](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/context/auth.ts:85>) |
| 最终 | 10 | app | [apps/api/src/app.ts:138–306](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/app.ts:138>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [apps/api/src/auth.ts · verifyJwt](</Users/teriri/WebstormProjects/1573-frontend/apps/api/src/auth.ts:83>) | 无 | 无 |

查询耗时：3627.2 → 6174.1 毫秒。

## web-1573-06 · 1573-frontend

```text
search_code("课程包尚未解锁时，界面在哪里把卡片变灰并显示锁定标志？")
```

搜索意图：模板按课程锁定状态切换视觉与标识

原冻结声明锚点覆盖名次：**未覆盖 → 1**；原冻结关键行为联合覆盖：**未覆盖 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [apps/web/src/pages/PackageList.vue:83–128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:83>) |
| 基线 | 2 | packages | [apps/web/src/pages/PackageList.vue:19–32](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:19>) |
| 基线 | 3 | 文本块 | [apps/web/src/pages/PackageDetail.vue:1–80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) |
| 基线 | 4 | 文本块 | [apps/web/src/pages/Login.vue:1–101](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:1>) |
| 基线 | 5 | 文本块 | [apps/web/src/pages/VideoSegment.vue:221–279](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:221>) |
| 基线 | 6 | hoverOverlay.isDraggingThis | [apps/web/src/components/NeuralGridDemo.vue:156–156](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) |
| 基线 | 7 | 文本块 | [apps/web/src/pages/VideoSegment.vue:7–34](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:7>) |
| 基线 | 8 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:1–44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) |
| 基线 | 9 | 文本块 | [apps/web/src/pages/VideoSegment.vue:42–44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:42>) |
| 基线 | 10 | formatServerMessage | [apps/web/src/pages/ModelViewer.vue:644–654](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/ModelViewer.vue:644>) |
| 最终 | 1 | 文本块 | [apps/web/src/pages/PackageList.vue:46–128](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:46>) |
| 最终 | 2 | packages | [apps/web/src/pages/PackageList.vue:19–32](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:19>) |
| 最终 | 3 | handlePackageClick | [apps/web/src/pages/PackageList.vue:34–44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:34>) |
| 最终 | 4 | 文本块 | [apps/web/src/pages/PackageList.vue:8–11](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:8>) |
| 最终 | 5 | 文本块 | [apps/web/src/pages/PackageDetail.vue:1–80](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageDetail.vue:1>) |
| 最终 | 6 | 文本块 | [apps/web/src/pages/Login.vue:1–123](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/Login.vue:1>) |
| 最终 | 7 | 文本块 | [apps/web/src/pages/VideoSegment.vue:205–334](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:205>) |
| 最终 | 8 | hoverOverlay.isDraggingThis | [apps/web/src/components/NeuralGridDemo.vue:156–156](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:156>) |
| 最终 | 9 | 文本块 | [apps/web/src/pages/VideoSegment.vue:7–34](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/VideoSegment.vue:7>) |
| 最终 | 10 | 文本块 | [apps/web/src/components/NeuralGridDemo.vue:1–44](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/components/NeuralGridDemo.vue:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [apps/web/src/pages/PackageList.vue · 行为锚点](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:56>) | 56 | 无 |
| 审计替代组 template-lock-behavior | [apps/web/src/pages/PackageList.vue · 行为锚点](</Users/teriri/WebstormProjects/1573-frontend/apps/web/src/pages/PackageList.vue:56>) | 无 | 无 |

审计说明：仅取消与问题无关的外层 template 起点要求；七条行为行保持原样，锚点明确标为模板行为而非函数声明。

查询耗时：3353.8 → 5746.8 毫秒。

## idea-bid-review-app-01 · bid-review-app

```text
search_code("文档太长时怎样切成多段，并让相邻片段保留一部分相同内容？")
```

搜索意图：定位长文档切分时长度、边界及重叠区间的具体计算

原冻结声明锚点覆盖名次：**2 → 2**；原冻结关键行为联合覆盖：**2 → 2**。

审计关键行为联合覆盖：**2 → 2**；冻结范围全部非空行覆盖：2 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1–55](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) |
| 基线 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1–167](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>) |
| 基线 | 3 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:112–228](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:112>) |
| 基线 | 4 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:172–225](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:172>) |
| 基线 | 5 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1–57](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1>) |
| 基线 | 6 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455–512](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455>) |
| 基线 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1–60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:1>) |
| 基线 | 8 | 文本块 | [src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164–206](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164>) |
| 基线 | 9 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:120–174](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/LongTextLlmService.java:120>) |
| 基线 | 10 | 文本块 | [docs/bid-review-architecture.md:210–262](</Users/teriri/IdeaProjects/bid-review-app/docs/bid-review-architecture.md:210>) |
| 最终 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1–228](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/llm/impl/SmartTextChunker.java:1>) |
| 最终 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1–206](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:1>) |
| 最终 | 3 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1–225](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:1>) |
| 最终 | 4 | 文本块 | [src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455–528](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/OverlapChunkingService.java:455>) |
| 最终 | 5 | 文本块 | [src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164–206](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/testextract/TestExtractFlowService.java:164>) |
| 最终 | 6 | 文本块 | [docs/bid-review-architecture.md:210–262](</Users/teriri/IdeaProjects/bid-review-app/docs/bid-review-architecture.md:210>) |
| 最终 | 7 | truncateText | [bidreview-app-ui/src/utils/helpers.ts:90–93](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/utils/helpers.ts:90>) |
| 最终 | 8 | 文本块 | [src/main/java/com/kita/bidreview/model/document/DocumentElementType.java:1–25](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/DocumentElementType.java:1>) |
| 最终 | 9 | getFileExtension | [bidreview-app-ui/src/utils/helpers.ts:247–249](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/utils/helpers.ts:247>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java · createChunks](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/TextChunkingService.java:98>) | 无 | 无 |

查询耗时：7499.3 → 12597.4 毫秒。

## idea-bid-review-app-02 · bid-review-app

```text
search_code("模型抽取结果里重复的条目和空白内容在哪里被清理，同时不能把零和假值丢掉？")
```

搜索意图：定位结构化模型结果的递归清洗及去重实现

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289–348](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289>) |
| 基线 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:175–231](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:175>) |
| 基线 | 3 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:423–523](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:423>) |
| 基线 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:401–458](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:401>) |
| 基线 | 5 | 文本块 | [src/main/java/com/kita/bidreview/model/document/MarkdownExtractionResult.java:1–56](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/MarkdownExtractionResult.java:1>) |
| 基线 | 6 | 文本块 | [src/main/java/com/kita/bidreview/model/document/TextExtractionResult.java:1–44](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/TextExtractionResult.java:1>) |
| 基线 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/VllmOcrStrategy.java:232–304](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/VllmOcrStrategy.java:232>) |
| 基线 | 8 | 文本块 | [src/main/java/com/kita/bidreview/model/document/BatchTextExtractionResult.java:1–49](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/BatchTextExtractionResult.java:1>) |
| 基线 | 9 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextOptimizationService.java:118–290](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextOptimizationService.java:118>) |
| 基线 | 10 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:253–309](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:253>) |
| 最终 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289–508](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:289>) |
| 最终 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:60–231](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:60>) |
| 最终 | 3 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:363–523](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:363>) |
| 最终 | 4 | 文本块 | [src/main/java/com/kita/bidreview/model/document/MarkdownExtractionResult.java:1–56](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/MarkdownExtractionResult.java:1>) |
| 最终 | 5 | 文本块 | [src/main/java/com/kita/bidreview/model/document/TextExtractionResult.java:1–44](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/TextExtractionResult.java:1>) |
| 最终 | 6 | 文本块 | [src/main/java/com/kita/bidreview/model/document/BatchTextExtractionResult.java:1–49](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/document/BatchTextExtractionResult.java:1>) |
| 最终 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/VllmOcrStrategy.java:232–304](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/VllmOcrStrategy.java:232>) |
| 最终 | 8 | 文本块 | [src/main/java/com/kita/bidreview/util/LocalFileStorage.java:253–309](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/util/LocalFileStorage.java:253>) |
| 最终 | 9 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextOptimizationService.java:347–351](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextOptimizationService.java:347>) |
| 最终 | 10 | 文本块 | [bidreview-app-ui/src/stores/test.ts:150–159](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/stores/test.ts:150>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java · cleanAndDedupe](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/ResultMergingService.java:310>) | 无 | 无 |

查询耗时：5019.8 → 6008.3 毫秒。

## idea-bid-review-app-03 · bid-review-app

```text
search_code("再次检查投标材料时，哪里把招标要求、第一次判断和原始标书一起交给模型？")
```

搜索意图：定位复审请求内容的组装，包含已有结论、原文件及可选指示

原冻结声明锚点覆盖名次：**未覆盖 → 未覆盖**；原冻结关键行为联合覆盖：**未覆盖 → 未覆盖**。

审计关键行为联合覆盖：**2 → 2**；冻结范围全部非空行覆盖：2 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530–649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) |
| 基线 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288–314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>) |
| 基线 | 3 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:692–751](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:692>) |
| 基线 | 4 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-full.txt:181–240](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/tender-full.txt:181>) |
| 基线 | 5 | 文本块 | [uploads/bid-review/a3a83331-cebb-4d67-a86d-05b8dc0dee9a/requirement-original.txt:361–420](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/a3a83331-cebb-4d67-a86d-05b8dc0dee9a/requirement-original.txt:361>) |
| 基线 | 6 | 文本块 | [src/main/resources/template-config.yaml:1–24](</Users/teriri/IdeaProjects/bid-review-app/src/main/resources/template-config.yaml:1>) |
| 最终 | 1 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530–649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) |
| 最终 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288–314](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:288>) |
| 最终 | 3 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:632–751](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:632>) |
| 最终 | 4 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/final-review-result.txt:1–46](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/final-review-result.txt:1>) |
| 最终 | 5 | 文本块 | [src/main/resources/template-config.yaml:1–24](</Users/teriri/IdeaProjects/bid-review-app/src/main/resources/template-config.yaml:1>) |
| 最终 | 6 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/chunks/tender-requirements/chunk_003_162708.json:1–60](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/chunks/tender-requirements/chunk_003_162708.json:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java · buildReReviewUserContent](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/PromptTemplateService.java:90>) | 90、94–96、100、105 | 90、94–96、100、105 |
| 审计替代组 secondary-review-content | [src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java · buildSecondaryReviewUserContent](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/LlmReviewServiceImpl.java:296>) | 无 | 无 |

审计说明：增加已审计的完整复审内容组装方法；仅命中上传文本或模板说明不能成立。

查询耗时：12818.3 → 12965.8 毫秒。

## idea-bid-review-app-04 · bid-review-app

```text
search_code("给一个本地文档路径或远程链接，哪里会读取内容并提取可供审查的文字？")
```

搜索意图：定位文档来源识别、提取分支、失败校验和文字合并

原冻结声明锚点覆盖名次：**3 → 3**；原冻结关键行为联合覆盖：**3 → 3**。

审计关键行为联合覆盖：**3 → 3**；冻结范围全部非空行覆盖：3 → 3。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1–120](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1>) |
| 基线 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181–234](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181>) |
| 基线 | 3 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1–60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>) |
| 基线 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1–99](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1>) |
| 基线 | 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1–54](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1>) |
| 基线 | 6 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:583–616](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:583>) |
| 基线 | 7 | 文本块 | [src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1–56](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1>) |
| 基线 | 8 | 文本块 | [src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1–30](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1>) |
| 基线 | 9 | DocumentInfo | [bidreview-app-ui/src/types/index.ts:17–23](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/types/index.ts:17>) |
| 基线 | 10 | 文本块 | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:546–656](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:546>) |
| 最终 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181–287](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:181>) |
| 最终 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1–120](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/TextExtractionService.java:1>) |
| 最终 | 3 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1–60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:1>) |
| 最终 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1–99](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/infra/DocumentExtractionTaskService.java:1>) |
| 最终 | 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1–72](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/DocumentExtractionController.java:1>) |
| 最终 | 6 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:583–616](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/MarkdownExtractionService.java:583>) |
| 最终 | 7 | 文本块 | [src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1–56](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/model/bidreview/DocumentInfo.java:1>) |
| 最终 | 8 | DocumentInfo | [bidreview-app-ui/src/types/index.ts:17–23](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/types/index.ts:17>) |
| 最终 | 9 | 文本块 | [src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1–30](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/DocumentProcessingService.java:1>) |
| 最终 | 10 | 文本块 | [src/main/java/com/kita/bidreview/controller/BidReviewController.java:491–656](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/BidReviewController.java:491>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java · extractTextContent](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/DocumentProcessingServiceImpl.java:27>) | 无 | 无 |

查询耗时：5007.2 → 5296.9 毫秒。

## idea-bid-review-app-05 · bid-review-app

```text
search_code("图片识字服务失败以后，系统在哪按顺序换其他可用厂商继续尝试？")
```

搜索意图：定位 OCR 提供商失败后的筛选、优先级切换和全部失败处理

原冻结声明锚点覆盖名次：**3 → 2**；原冻结关键行为联合覆盖：**3 → 2**。

审计关键行为联合覆盖：**3 → 2**；冻结范围全部非空行覆盖：3 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:175–290](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:175>) |
| 基线 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61–114](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61>) |
| 基线 | 3 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228–399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>) |
| 基线 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1–60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1>) |
| 基线 | 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/SystemController.java:228–284](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/SystemController.java:228>) |
| 基线 | 6 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrConfig.java:1–58](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrConfig.java:1>) |
| 基线 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1–84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1>) |
| 基线 | 8 | 文本块 | [src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:106–216](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/document/impl/ConcurrentOcrProcessor.java:106>) |
| 基线 | 9 | 文本块 | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:342–390](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:342>) |
| 基线 | 10 | 文本块 | [bidreview-app-ui/src/components/SystemTest.vue:156–207](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/SystemTest.vue:156>) |
| 最终 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61–290](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategyManager.java:61>) |
| 最终 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228–399](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:228>) |
| 最终 | 3 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1–60](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrService.java:1>) |
| 最终 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:113–168](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:113>) |
| 最终 | 5 | 文本块 | [src/main/java/com/kita/bidreview/controller/SystemController.java:228–284](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/controller/SystemController.java:228>) |
| 最终 | 6 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:169–227](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:169>) |
| 最终 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrConfig.java:1–58](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrConfig.java:1>) |
| 最终 | 8 | 文本块 | [src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1–84](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/OcrStrategy.java:1>) |
| 最终 | 9 | 文本块 | [src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:342–390](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/executor/BidReviewTaskExecutor.java:342>) |
| 最终 | 10 | testLlm | [bidreview-app-ui/src/components/SystemTest.vue:392–398](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/SystemTest.vue:392>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java · tryFallbackStrategy](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/ocr/impl/OcrServiceImpl.java:330>) | 无 | 无 |

查询耗时：4497.6 → 7633.6 毫秒。

## idea-bid-review-app-06 · bid-review-app

```text
search_code("删除一次标书审查记录时，上传材料和缓存记录是在哪里一起清掉的？")
```

搜索意图：定位删除审查任务时的文件与存储清理流程

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340–395](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>) |
| 基线 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:59–116](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:59>) |
| 基线 | 3 | bidReviewApi | [bidreview-app-ui/src/api/bid-review.ts:13–112](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:13>) |
| 基线 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868–925](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868>) |
| 基线 | 5 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:212–271](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:212>) |
| 基线 | 6 | 文本块 | [uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530–649](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/52f71abb-0916-41fa-bc96-0b7c5174275a/requirement-original.txt:530>) |
| 基线 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:55–111](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:55>) |
| 基线 | 8 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:452–511](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:452>) |
| 基线 | 9 | clearHistory | [bidreview-app-ui/src/components/TaskList.vue:490–539](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/TaskList.vue:490>) |
| 基线 | 10 | 文本块 | [bidreview-app-ui/src/views/Dashboard.vue:1–56](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/views/Dashboard.vue:1>) |
| 最终 | 1 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340–433](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:340>) |
| 最终 | 2 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:1–176](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:1>) |
| 最终 | 3 | bidReviewApi | [bidreview-app-ui/src/api/bid-review.ts:13–112](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/api/bid-review.ts:13>) |
| 最终 | 4 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868–925](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:868>) |
| 最终 | 5 | 文本块 | [uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:181–271](</Users/teriri/IdeaProjects/bid-review-app/uploads/bid-review/973f0dad-5558-4760-b1c5-479126609897/requirement-original.txt:181>) |
| 最终 | 6 | 文本块 | [src/main/java/com/kita/bidreview/service/review/BidReviewService.java:1–192](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/BidReviewService.java:1>) |
| 最终 | 7 | 文本块 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:1038–1170](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:1038>) |
| 最终 | 8 | getDocumentName.nameMap | [bidreview-app-ui/src/components/ExtractedContentModal.vue:206–211](</Users/teriri/IdeaProjects/bid-review-app/bidreview-app-ui/src/components/ExtractedContentModal.vue:206>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java · deleteBidReviewTask](</Users/teriri/IdeaProjects/bid-review-app/src/main/java/com/kita/bidreview/service/review/impl/BidReviewServiceImpl.java:363>) | 无 | 无 |

查询耗时：7593.5 → 11051.7 毫秒。

## idea-teriri-api-hub-01 · teriri-api-hub

```text
search_code("新的聊天请求如何复用此前上下文，并立即向网页建立持续接收回答的连接？")
```

搜索意图：定位聊天上下文复用、SSE 握手及后台发起推理的多步骤流程

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:1–178](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>) |
| 基线 | 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:60–228](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:60>) |
| 基线 | 3 | sendSSE | [llm-tester/src/api/index.js:3–26](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/api/index.js:3>) |
| 基线 | 4 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1–16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) |
| 基线 | 5 | send | [llm-tester/src/App.vue:55–153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) |
| 基线 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1–157](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1>) |
| 基线 | 7 | connectAsrWebSocket | [llm-tester/src/App.vue:412–490](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:412>) |
| 基线 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/ChatController.java:1–32](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/ChatController.java:1>) |
| 基线 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/event/llm/LLMCompletionEvent.java:1–22](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/event/llm/LLMCompletionEvent.java:1>) |
| 基线 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55–102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) |
| 最终 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:1–199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>) |
| 最终 | 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/LLMClient.java:1–228](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/LLMClient.java:1>) |
| 最终 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1–16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) |
| 最终 | 4 | sendSSE | [llm-tester/src/api/index.js:3–26](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/api/index.js:3>) |
| 最终 | 5 | send | [llm-tester/src/App.vue:55–178](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) |
| 最终 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1–235](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:1>) |
| 最终 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/ChatController.java:1–32](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/ChatController.java:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/teriri/teririapihub/service/ChatService.java · chatWithLLMStream](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:72>) | 无 | 无 |

查询耗时：4392.8 → 4714.2 毫秒。

## idea-teriri-api-hub-02 · teriri-api-hub

```text
search_code("模型每生成一段回答，哪里把文字推送给前端并按开关送去转成声音？")
```

搜索意图：定位流式文本事件到 SSE 和语音合成两条输出的连接

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:61–199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>) |
| 基线 | 2 | send | [llm-tester/src/App.vue:55–153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) |
| 基线 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1–7](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1>) |
| 基线 | 4 | sendSSE | [llm-tester/src/api/index.js:3–26](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/api/index.js:3>) |
| 基线 | 5 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55–102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) |
| 基线 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151–203](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) |
| 基线 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1–17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) |
| 基线 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1–108](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) |
| 基线 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1–16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) |
| 基线 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1–70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) |
| 最终 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:1–199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>) |
| 最终 | 2 | send | [llm-tester/src/App.vue:1–168](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:1>) |
| 最终 | 3 | sendSSE | [llm-tester/src/api/index.js:3–26](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/api/index.js:3>) |
| 最终 | 4 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55–102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) |
| 最终 | 5 | sendAsrAudioChunkViaHttp | [llm-tester/src/App.vue:314–343](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:314>) |
| 最终 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1–7](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/LLMCallback.java:1>) |
| 最终 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151–295](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) |
| 最终 | 8 | 文本块 | [llm-tester/src/App.vue:545–657](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:545>) |
| 最终 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1–17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) |
| 最终 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1–16](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/ChatRequest.java:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/teriri/teririapihub/service/ChatService.java · onLLMTextChunk](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:156>) | 无 | 无 |

查询耗时：5319.1 → 4820.3 毫秒。

## idea-teriri-api-hub-03 · teriri-api-hub

```text
search_code("很多人同时请求语音合成时，哪里限制连接总数并防止重复建立同一个会话？")
```

搜索意图：定位语音会话复用、容量上限和并发创建保护

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1–119](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1>) |
| 基线 | 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:92–201](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:92>) |
| 基线 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:1–102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:1>) |
| 基线 | 4 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1–70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) |
| 基线 | 5 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:511–569](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:511>) |
| 基线 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1–159](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1>) |
| 基线 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:204–247](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:204>) |
| 基线 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1–57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) |
| 基线 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/dto/tts/TTSOptional.java:1–60](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/dto/tts/TTSOptional.java:1>) |
| 基线 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:219–275](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:219>) |
| 最终 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1–119](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:1>) |
| 最终 | 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:32–247](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:32>) |
| 最终 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:1–102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:1>) |
| 最终 | 4 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1–70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) |
| 最终 | 5 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:461–616](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:461>) |
| 最终 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:304–400](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:304>) |
| 最终 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151–295](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) |
| 最终 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/exception/TTSSessionException.java:1–11](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/exception/TTSSessionException.java:1>) |
| 最终 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/dto/tts/TTSResponse.java:1–11](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/dto/tts/TTSResponse.java:1>) |
| 最终 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1–17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java · acquireSession](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/pool/TTSSessionPool.java:50>) | 无 | 无 |

查询耗时：4420.7 → 4519.5 毫秒。

## idea-teriri-api-hub-04 · teriri-api-hub

```text
search_code("前端分批传来的编码音频在哪里还原成字节并接到实时识别的数据流？")
```

搜索意图：定位分块编码音频解码后写入语音识别输入管道

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1–169](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) |
| 基线 | 2 | send | [llm-tester/src/App.vue:83–153](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:83>) |
| 基线 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1–57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1>) |
| 基线 | 4 | setupMicrophone | [llm-tester/src/App.vue:263–312](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:263>) |
| 基线 | 5 | base64ToUint8Array | [llm-tester/src/App.vue:44–53](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:44>) |
| 基线 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:158–214](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:158>) |
| 基线 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:164–219](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:164>) |
| 基线 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1–125](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) |
| 基线 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:384–425](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:384>) |
| 基线 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1–21](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1>) |
| 最终 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1–169](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) |
| 最终 | 2 | send | [llm-tester/src/App.vue:83–261](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:83>) |
| 最终 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1–57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1>) |
| 最终 | 4 | setupMicrophone | [llm-tester/src/App.vue:263–344](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:263>) |
| 最终 | 5 | base64ToUint8Array | [llm-tester/src/App.vue:44–53](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:44>) |
| 最终 | 6 | setupMediaSource | [llm-tester/src/App.vue:66–81](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:66>) |
| 最终 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:164–325](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:164>) |
| 最终 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:384–425](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:384>) |
| 最终 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/AsrRequestBo.java:1–10](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/AsrRequestBo.java:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java · appendAudioBase64Chunk](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:142>) | 无 | 无 |

查询耗时：4191.4 → 4317.7 毫秒。

## idea-teriri-api-hub-05 · teriri-api-hub

```text
search_code("语音识别连接退出时，哪里关闭网络和音频资源并通知调用方？")
```

搜索意图：定位识别客户端关闭时的资源释放、回调和状态清理

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:492–538](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:492>) |
| 基线 | 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1–111](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) |
| 基线 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1–125](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) |
| 基线 | 4 | 文本块 | [llm-tester/src/App.vue:492–544](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:492>) |
| 基线 | 5 | stopRecordingAndCleanUp | [llm-tester/src/App.vue:345–390](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:345>) |
| 基线 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1–21](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1>) |
| 基线 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:109–235](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/ASRWebSocketHandler.java:109>) |
| 基线 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1–17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) |
| 基线 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:570–616](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:570>) |
| 基线 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1–57](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/ASRProtocolParser.java:1>) |
| 最终 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:436–538](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:436>) |
| 最终 | 2 | 文本块 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java:1–111](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:1>) |
| 最终 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1–125](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/handlers/WebSocketASRCallback.java:1>) |
| 最终 | 4 | 文本块 | [llm-tester/src/App.vue:392–615](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:392>) |
| 最终 | 5 | stopRecordingAndCleanUp | [llm-tester/src/App.vue:196–390](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:196>) |
| 最终 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1–21](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/ASRCallback.java:1>) |
| 最终 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1–17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/callback/TTSCallback.java:1>) |
| 最终 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/model/bo/AsrRequestBo.java:1–10](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/bo/AsrRequestBo.java:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/teriri/teririapihub/client/ASRClient.java · close](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/ASRClient.java:498>) | 无 | 无 |

查询耗时：4333.8 → 4563.1 毫秒。

## idea-teriri-api-hub-06 · teriri-api-hub

```text
search_code("朗读内容发送时抢不到锁，哪里会把这段文字放回队列等待下次处理？")
```

搜索意图：定位语音合成发送锁超时后的队列回退

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:248–353](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:248>) |
| 基线 | 2 | 文本块 | [llm-tester/src/App.vue:55–114](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) |
| 基线 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:32–91](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:32>) |
| 基线 | 4 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:61–199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:61>) |
| 基线 | 5 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:248–295](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:248>) |
| 基线 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1–159](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1>) |
| 基线 | 7 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:151–203](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:151>) |
| 基线 | 8 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:55–102](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:55>) |
| 基线 | 9 | 文本块 | [src/main/java/com/teriri/teririapihub/model/dto/tts/TextRequest.java:1–17](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/model/dto/tts/TextRequest.java:1>) |
| 基线 | 10 | 文本块 | [src/main/java/com/teriri/teririapihub/controller/TTSController.java:1–70](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/controller/TTSController.java:1>) |
| 最终 | 1 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:248–400](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:248>) |
| 最终 | 2 | 文本块 | [llm-tester/src/App.vue:55–178](</Users/teriri/IdeaProjects/teriri-api-hub/llm-tester/src/App.vue:55>) |
| 最终 | 3 | 文本块 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java:1–201](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:1>) |
| 最终 | 4 | 文本块 | [src/main/java/com/teriri/teririapihub/service/ChatService.java:1–199](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/ChatService.java:1>) |
| 最终 | 5 | 文本块 | [src/main/java/com/teriri/teririapihub/service/TTSService.java:248–295](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/service/TTSService.java:248>) |
| 最终 | 6 | 文本块 | [src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1–192](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/utils/TTSMessageBuilder.java:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [src/main/java/com/teriri/teririapihub/client/TTSClient.java · processTextFromQueue](</Users/teriri/IdeaProjects/teriri-api-hub/src/main/java/com/teriri/teririapihub/client/TTSClient.java:315>) | 无 | 无 |

查询耗时：6008.3 → 4751.7 毫秒。

## idea-1604-apps-01 · 1604-apps

```text
search_code("司机登记一笔发货时，哪里同步减少冰粒库存，并在库存操作失败时撤销刚建的记录？")
```

搜索意图：定位发货创建与库存流水联动及失败补偿

原冻结声明锚点覆盖名次：**2 → 2**；原冻结关键行为联合覆盖：**2 → 2**。

审计关键行为联合覆盖：**2 → 2**；冻结范围全部非空行覆盖：2 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | stockOutRecords | [backend/src/db/schema/delivery.ts:21–58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:21>) |
| 基线 | 2 | StockOutService.create | [backend/src/services/delivery.service.ts:818–884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) |
| 基线 | 3 | returnRecords | [backend/src/db/schema/delivery.ts:63–100](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:63>) |
| 基线 | 4 | iceInventoryRoutes | [backend/src/routes/iceInventory.ts:45–120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:45>) |
| 基线 | 5 | IceInventorySummary | [frontend/src/types/iceInventory.ts:87–96](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/iceInventory.ts:87>) |
| 基线 | 6 | IceInventoryService.create | [backend/src/services/iceInventory.service.ts:775–828](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:775>) |
| 基线 | 7 | useIceInventory | [frontend/src/composables/useIceInventory.ts:34–141](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useIceInventory.ts:34>) |
| 基线 | 8 | 文本块 | [frontend/src/views/IceInventory/components/IceInventoryForm.vue:161–216](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/components/IceInventoryForm.vue:161>) |
| 基线 | 9 | StockOutService.delete | [backend/src/services/delivery.service.ts:987–1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:987>) |
| 基线 | 10 | 文本块 | [backend/drizzle/0007_delivery.sql:1–96](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0007_delivery.sql:1>) |
| 最终 | 1 | stockOutRecords | [backend/src/db/schema/delivery.ts:21–58](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:21>) |
| 最终 | 2 | StockOutService.create | [backend/src/services/delivery.service.ts:818–884](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) |
| 最终 | 3 | returnRecords | [backend/src/db/schema/delivery.ts:63–100](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/delivery.ts:63>) |
| 最终 | 4 | iceInventoryRoutes | [backend/src/routes/iceInventory.ts:45–120](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/iceInventory.ts:45>) |
| 最终 | 5 | IceInventorySummary | [frontend/src/types/iceInventory.ts:87–96](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/iceInventory.ts:87>) |
| 最终 | 6 | IceInventoryService.create | [backend/src/services/iceInventory.service.ts:775–828](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/iceInventory.service.ts:775>) |
| 最终 | 7 | useIceInventory | [frontend/src/composables/useIceInventory.ts:34–141](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useIceInventory.ts:34>) |
| 最终 | 8 | 文本块 | [frontend/src/views/IceInventory/components/IceInventoryForm.vue:105–216](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/IceInventory/components/IceInventoryForm.vue:105>) |
| 最终 | 9 | StockOutService.delete | [backend/src/services/delivery.service.ts:987–1018](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:987>) |
| 最终 | 10 | 文本块 | [backend/drizzle/0007_delivery.sql:1–96](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0007_delivery.sql:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [backend/src/services/delivery.service.ts · create](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/delivery.service.ts:818>) | 无 | 无 |

查询耗时：4024.1 → 5930.9 毫秒。

## idea-1604-apps-02 · 1604-apps

```text
search_code("给客户登记收款时，哪里检查送货单已经批准，而且累计金额不能超过订单金额？")
```

搜索意图：定位收款创建前的单据状态与累计待收余额验证

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | FinanceService.createPayment | [backend/src/services/finance.service.ts:1931–1993](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>) |
| 基线 | 2 | 文本块 | [backend/drizzle/0009_finance.sql:100–189](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0009_finance.sql:100>) |
| 基线 | 3 | FinanceService.auditPayment | [backend/src/services/finance.service.ts:2119–2178](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:2119>) |
| 基线 | 4 | deliveryBills | [backend/src/db/schema/finance.ts:20–53](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:20>) |
| 基线 | 5 | customerOrderPayments | [backend/src/db/schema/finance.ts:92–124](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:92>) |
| 基线 | 6 | DeliveryBillReviewInput | [backend/src/services/finance.service.ts:281–294](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:281>) |
| 基线 | 7 | reviewBill | [frontend/src/views/DeliveryBill/index.vue:230–249](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:230>) |
| 基线 | 8 | useDeliveryBill.review | [frontend/src/composables/useFinance.ts:141–145](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useFinance.ts:141>) |
| 基线 | 9 | CustomerDebtRow.receivedAmount | [backend/src/services/report.service.ts:142–142](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/report.service.ts:142>) |
| 基线 | 10 | CustomerOrderRecord | [frontend/src/types/finance.ts:269–354](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/finance.ts:269>) |
| 最终 | 1 | FinanceService.createPayment | [backend/src/services/finance.service.ts:1931–1993](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>) |
| 最终 | 2 | 文本块 | [backend/drizzle/0009_finance.sql:100–189](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0009_finance.sql:100>) |
| 最终 | 3 | FinanceService.auditPayment | [backend/src/services/finance.service.ts:2119–2178](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:2119>) |
| 最终 | 4 | deliveryBills | [backend/src/db/schema/finance.ts:20–53](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:20>) |
| 最终 | 5 | customerOrderPayments | [backend/src/db/schema/finance.ts:92–124](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:92>) |
| 最终 | 6 | reviewBill | [frontend/src/views/DeliveryBill/index.vue:230–249](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:230>) |
| 最终 | 7 | DeliveryBillReviewInput | [backend/src/services/finance.service.ts:281–294](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:281>) |
| 最终 | 8 | CustomerOrderRecord.receivedAmount | [backend/src/services/finance.service.ts:495–495](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:495>) |
| 最终 | 9 | useDeliveryBill.review | [frontend/src/composables/useFinance.ts:141–145](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useFinance.ts:141>) |
| 最终 | 10 | 文本块 | [frontend/src/views/DebtAudit/index.vue:184–273](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DebtAudit/index.vue:184>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [backend/src/services/finance.service.ts · createPayment](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1931>) | 无 | 无 |

查询耗时：4193.4 → 3549.8 毫秒。

## idea-1604-apps-03 · 1604-apps

```text
search_code("把送货单退回重新处理时，哪里要求填写理由并记下审核人和时间？")
```

搜索意图：定位送货单驳回必填原因以及审核记录持久化

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | FinanceService.reviewDeliveryBill | [backend/src/services/finance.service.ts:1579–1630](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>) |
| 基线 | 2 | FinanceService.auditPayment | [backend/src/services/finance.service.ts:2119–2178](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:2119>) |
| 基线 | 3 | reviewBill | [frontend/src/views/DeliveryBill/index.vue:230–249](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:230>) |
| 基线 | 4 | DeliveryBillRecord | [frontend/src/types/finance.ts:43–144](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/finance.ts:43>) |
| 基线 | 5 | DeliveryBillReviewInput | [backend/src/services/finance.service.ts:281–294](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:281>) |
| 基线 | 6 | useDeliveryBill.review | [frontend/src/composables/useFinance.ts:141–145](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useFinance.ts:141>) |
| 基线 | 7 | ReimbursementService.review | [backend/src/services/reimbursement.service.ts:532–581](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:532>) |
| 基线 | 8 | 文本块 | [backend/drizzle/0009_finance.sql:100–189](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0009_finance.sql:100>) |
| 基线 | 9 | customerOrderPayments | [backend/src/db/schema/finance.ts:92–124](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/finance.ts:92>) |
| 基线 | 10 | DeliveryBillFormProps | [frontend/src/views/DeliveryBill/components/DeliveryBillForm.vue:48–77](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/components/DeliveryBillForm.vue:48>) |
| 最终 | 1 | FinanceService.reviewDeliveryBill | [backend/src/services/finance.service.ts:1579–1630](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>) |
| 最终 | 2 | FinanceService.auditPayment | [backend/src/services/finance.service.ts:2119–2178](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:2119>) |
| 最终 | 3 | reviewBill | [frontend/src/views/DeliveryBill/index.vue:217–256](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:217>) |
| 最终 | 4 | DeliveryBillRecord | [frontend/src/types/finance.ts:43–144](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/finance.ts:43>) |
| 最终 | 5 | DeliveryBillReviewInput | [backend/src/services/finance.service.ts:281–294](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:281>) |
| 最终 | 6 | useDeliveryBill.review | [frontend/src/composables/useFinance.ts:141–145](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useFinance.ts:141>) |
| 最终 | 7 | ReimbursementService.review | [backend/src/services/reimbursement.service.ts:532–581](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:532>) |
| 最终 | 8 | FinanceService.updateDeliveryBill | [backend/src/services/finance.service.ts:1427–1522](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1427>) |
| 最终 | 9 | 文本块 | [backend/drizzle/0009_finance.sql:100–189](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0009_finance.sql:100>) |
| 最终 | 10 | 文本块 | [frontend/src/views/DeliveryBill/index.vue:703–752](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/DeliveryBill/index.vue:703>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [backend/src/services/finance.service.ts · reviewDeliveryBill](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/finance.service.ts:1579>) | 无 | 无 |

查询耗时：3906.4 → 3962.3 毫秒。

## idea-1604-apps-04 · 1604-apps

```text
search_code("员工报销的钱付完后，哪里把单据标成已付款或已办结，并保留操作记录？")
```

搜索意图：定位报销确认付款的状态限制、完成状态及日志

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | ReimbursementService.pay | [backend/src/services/reimbursement.service.ts:591–635](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>) |
| 基线 | 2 | payRecord | [frontend/src/views/Reimbursement/index.vue:213–284](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Reimbursement/index.vue:213>) |
| 基线 | 3 | reimbursementOperationLogs | [backend/src/db/schema/reimbursement.ts:50–67](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/reimbursement.ts:50>) |
| 基线 | 4 | ReimbursementPayInput | [backend/src/services/reimbursement.service.ts:105–114](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:105>) |
| 基线 | 5 | reimbursements | [backend/src/db/schema/reimbursement.ts:18–45](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/reimbursement.ts:18>) |
| 基线 | 6 | ReimbursementService.review | [backend/src/services/reimbursement.service.ts:532–581](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:532>) |
| 基线 | 7 | 文本块 | [backend/drizzle/0010_reimbursement.sql:1–92](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0010_reimbursement.sql:1>) |
| 基线 | 8 | ReimbursementRecord | [frontend/src/types/reimbursement.ts:16–77](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/reimbursement.ts:16>) |
| 基线 | 9 | 文本块 | [frontend/src/views/Reimbursement/components/ReimbursementDeleteDialog.vue:53–86](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Reimbursement/components/ReimbursementDeleteDialog.vue:53>) |
| 基线 | 10 | ReimbursementStatus | [frontend/src/types/reimbursement.ts:6–6](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/reimbursement.ts:6>) |
| 最终 | 1 | ReimbursementService.pay | [backend/src/services/reimbursement.service.ts:591–635](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>) |
| 最终 | 2 | payRecord | [frontend/src/views/Reimbursement/index.vue:176–365](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Reimbursement/index.vue:176>) |
| 最终 | 3 | reimbursementOperationLogs | [backend/src/db/schema/reimbursement.ts:50–67](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/reimbursement.ts:50>) |
| 最终 | 4 | ReimbursementPayInput | [backend/src/services/reimbursement.service.ts:105–114](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:105>) |
| 最终 | 5 | reimbursements | [backend/src/db/schema/reimbursement.ts:18–45](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/reimbursement.ts:18>) |
| 最终 | 6 | ReimbursementService.review | [backend/src/services/reimbursement.service.ts:532–581](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:532>) |
| 最终 | 7 | 文本块 | [backend/drizzle/0010_reimbursement.sql:1–92](</Users/teriri/IdeaProjects/1604-apps/backend/drizzle/0010_reimbursement.sql:1>) |
| 最终 | 8 | ReimbursementRecord | [frontend/src/types/reimbursement.ts:16–77](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/reimbursement.ts:16>) |
| 最终 | 9 | ReimbursementStatus | [backend/src/services/reimbursement.service.ts:18–18](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:18>) |
| 最终 | 10 | 文本块 | [frontend/src/views/Reimbursement/components/ReimbursementDeleteDialog.vue:53–86](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Reimbursement/components/ReimbursementDeleteDialog.vue:53>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [backend/src/services/reimbursement.service.ts · pay](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/reimbursement.service.ts:591>) | 无 | 无 |

查询耗时：3429.3 → 3562.2 毫秒。

## idea-1604-apps-05 · 1604-apps

```text
search_code("用户自己换登录密码时，哪里验证旧密码并把新密码加密保存？")
```

搜索意图：定位本人修改密码的旧密码验证与安全写入

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | UserService.changePassword | [backend/src/services/user.service.ts:750–780](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>) |
| 基线 | 2 | ChangePasswordPayload | [frontend/src/types/user.ts:178–187](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/user.ts:178>) |
| 基线 | 3 | UserService.resetPassword | [backend/src/services/user.service.ts:726–741](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:726>) |
| 基线 | 4 | ChangePasswordRequest | [backend/src/routes/auth.ts:39–48](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/auth.ts:39>) |
| 基线 | 5 | useAuthStore.changePassword | [frontend/src/stores/auth.ts:204–207](</Users/teriri/IdeaProjects/1604-apps/frontend/src/stores/auth.ts:204>) |
| 基线 | 6 | UserService.verifyLogin | [backend/src/services/user.service.ts:619–640](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:619>) |
| 基线 | 7 | useUser.resetPassword | [frontend/src/composables/useUser.ts:91–95](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useUser.ts:91>) |
| 基线 | 8 | 文本块 | [docs/tasks/admin-account-entry.md:1–26](</Users/teriri/IdeaProjects/1604-apps/docs/tasks/admin-account-entry.md:1>) |
| 基线 | 9 | PasswordFormValues | [frontend/src/views/Account/index.vue:22–27](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Account/index.vue:22>) |
| 基线 | 10 | submitPassword | [frontend/src/views/Account/index.vue:90–97](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Account/index.vue:90>) |
| 最终 | 1 | UserService.changePassword | [backend/src/services/user.service.ts:750–780](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>) |
| 最终 | 2 | ChangePasswordPayload | [frontend/src/types/user.ts:178–187](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/user.ts:178>) |
| 最终 | 3 | UserService.resetPassword | [backend/src/services/user.service.ts:726–741](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:726>) |
| 最终 | 4 | ChangePasswordRequest | [backend/src/routes/auth.ts:39–48](</Users/teriri/IdeaProjects/1604-apps/backend/src/routes/auth.ts:39>) |
| 最终 | 5 | useAuthStore.changePassword | [frontend/src/stores/auth.ts:204–207](</Users/teriri/IdeaProjects/1604-apps/frontend/src/stores/auth.ts:204>) |
| 最终 | 6 | UserService.verifyLogin | [backend/src/services/user.service.ts:619–640](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:619>) |
| 最终 | 7 | useUser.resetPassword | [frontend/src/composables/useUser.ts:91–95](</Users/teriri/IdeaProjects/1604-apps/frontend/src/composables/useUser.ts:91>) |
| 最终 | 8 | 文本块 | [docs/tasks/admin-account-entry.md:1–26](</Users/teriri/IdeaProjects/1604-apps/docs/tasks/admin-account-entry.md:1>) |
| 最终 | 9 | submitPassword | [frontend/src/views/Account/index.vue:90–97](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Account/index.vue:90>) |
| 最终 | 10 | PasswordFormValues | [frontend/src/views/Account/index.vue:1–27](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Account/index.vue:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [backend/src/services/user.service.ts · changePassword](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/user.service.ts:750>) | 无 | 无 |

查询耗时：3126.2 → 3247.6 毫秒。

## idea-1604-apps-06 · 1604-apps

```text
search_code("编辑客户资料时，哪里会一起替换负责人的分配关系，并防止同一区域出现重名客户？")
```

搜索意图：定位客户基础资料、区域重名验证以及负责人关系事务替换

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | CustomerService.update | [backend/src/services/customer.service.ts:466–533](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>) |
| 基线 | 2 | CustomerService.create | [backend/src/services/customer.service.ts:402–457](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:402>) |
| 基线 | 3 | CustomerRecord | [backend/src/services/customer.service.ts:86–95](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:86>) |
| 基线 | 4 | 文本块 | [frontend/src/views/Customer/components/CustomerForm.vue:316–366](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/components/CustomerForm.vue:316>) |
| 基线 | 5 | 文本块 | [frontend/src/views/Customer/components/CustomerForm.vue:33–143](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/components/CustomerForm.vue:33>) |
| 基线 | 6 | customers | [backend/src/db/schema/customers.ts:22–55](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/customers.ts:22>) |
| 基线 | 7 | 文本块 | [frontend/src/views/CashOrder/components/CustomerOrderForm.vue:113–168](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/CashOrder/components/CustomerOrderForm.vue:113>) |
| 基线 | 8 | 文本块 | [frontend/src/views/Customer/index.vue:327–386](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/index.vue:327>) |
| 基线 | 9 | Customer.responsibles | [frontend/src/types/customer.ts:60–60](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/customer.ts:60>) |
| 基线 | 10 | CustomerPayload.responsibleUserIds | [frontend/src/types/customer.ts:115–115](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/customer.ts:115>) |
| 最终 | 1 | CustomerService.update | [backend/src/services/customer.service.ts:466–533](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>) |
| 最终 | 2 | CustomerService.create | [backend/src/services/customer.service.ts:402–457](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:402>) |
| 最终 | 3 | CustomerRecord | [backend/src/services/customer.service.ts:86–95](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:86>) |
| 最终 | 4 | 文本块 | [frontend/src/views/Customer/components/CustomerForm.vue:316–366](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/components/CustomerForm.vue:316>) |
| 最终 | 5 | 文本块 | [frontend/src/views/Customer/components/CustomerForm.vue:33–143](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/components/CustomerForm.vue:33>) |
| 最终 | 6 | 文本块 | [frontend/src/views/CashOrder/components/CustomerOrderForm.vue:56–206](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/CashOrder/components/CustomerOrderForm.vue:56>) |
| 最终 | 7 | customers | [backend/src/db/schema/customers.ts:22–55](</Users/teriri/IdeaProjects/1604-apps/backend/src/db/schema/customers.ts:22>) |
| 最终 | 8 | Customer.responsibles | [frontend/src/types/customer.ts:60–60](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/customer.ts:60>) |
| 最终 | 9 | 文本块 | [frontend/src/views/Customer/index.vue:214–386](</Users/teriri/IdeaProjects/1604-apps/frontend/src/views/Customer/index.vue:214>) |
| 最终 | 10 | CustomerPayload.responsibleUserIds | [frontend/src/types/customer.ts:115–115](</Users/teriri/IdeaProjects/1604-apps/frontend/src/types/customer.ts:115>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [backend/src/services/customer.service.ts · update](</Users/teriri/IdeaProjects/1604-apps/backend/src/services/customer.service.ts:466>) | 无 | 无 |

查询耗时：3509.3 → 3690.8 毫秒。

## idea-1086-backend-01 · 1086-backend

```text
search_code("申请一笔汽车贷款时，哪里把借款资料、身份证正反面和附件作为同一笔工单保存？")
```

搜索意图：定位新建贷款工单的角色校验、多表关联保存及事务回滚

原冻结声明锚点覆盖名次：**未覆盖 → 5**；原冻结关键行为联合覆盖：**未覆盖 → 5**。

审计关键行为联合覆盖：**未覆盖 → 5**；冻结范围全部非空行覆盖：未覆盖 → 5。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [update.sql:55–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) |
| 基线 | 2 | 文本块 | [application/api/controller/Index.php:647–703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) |
| 基线 | 3 | 文本块 | [application/api/controller/Index.php:388–439](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) |
| 基线 | 4 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) |
| 基线 | 5 | 文本块 | [application/api/controller/Index.php:1134–1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) |
| 基线 | 6 | 文本块 | [application/admin/controller/Workorder.php:1–156](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:1>) |
| 基线 | 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1–65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) |
| 基线 | 8 | 文本块 | [application/admin/controller/Workorderimage.php:1–103](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) |
| 基线 | 9 | 文本块 | [application/admin/view/workorder/upload.html:1–22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) |
| 基线 | 10 | 文本块 | [application/common/model/Workorder.php:1–43](</Users/teriri/IdeaProjects/1086-backend/application/common/model/Workorder.php:1>) |
| 最终 | 1 | 文本块 | [update.sql:55–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:55>) |
| 最终 | 2 | 文本块 | [application/api/controller/Index.php:529–703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:529>) |
| 最终 | 3 | 文本块 | [application/api/controller/Index.php:388–481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) |
| 最终 | 4 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) |
| 最终 | 5 | 文本块 | [application/api/controller/Index.php:1027–1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1027>) |
| 最终 | 6 | 文本块 | [application/api/controller/Index.php:764–993](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>) |
| 最终 | 7 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) |
| 最终 | 8 | 文本块 | [application/admin/validate/Workorderfile.php:1–27](</Users/teriri/IdeaProjects/1086-backend/application/admin/validate/Workorderfile.php:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [application/api/controller/Index.php · postOrder](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1074>) | 1074、1082 | 无 |
| 审计替代组 complete-loan-create | [application/api/controller/Index.php · postOrder](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1074>) | 1074、1082、1087–1091、1097–1098、1103、1107、1120、1130–1131 | 无 |

审计说明：加强原问题的借款字段和同单绑定覆盖；保留后半段局部命中说明，禁止算为完整实现。

查询耗时：4963.1 → 7374.8 毫秒。

## idea-1086-backend-02 · 1086-backend

```text
search_code("审核人员领取尚未分派的申请后，哪里把处理人改成自己并推进办理状态？")
```

搜索意图：定位待审工单认领及负责人绑定

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [application/api/controller/Index.php:704–820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) |
| 基线 | 2 | 文本块 | [application/admin/controller/Workorder.php:247–308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) |
| 基线 | 3 | 文本块 | [application/api/controller/Index.php:587–646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:587>) |
| 基线 | 4 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) |
| 基线 | 5 | 文本块 | [update.sql:115–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) |
| 基线 | 6 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) |
| 基线 | 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1–65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) |
| 基线 | 8 | 文本块 | [application/admin/view/workorderfile/index.html:1–46](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/index.html:1>) |
| 基线 | 9 | 文本块 | [application/admin/controller/Workorder.php:59–115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:59>) |
| 基线 | 10 | 文本块 | [application/admin/view/workorderimage/add.html:1–58](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderimage/add.html:1>) |
| 最终 | 1 | 文本块 | [application/api/controller/Index.php:704–820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) |
| 最终 | 2 | 文本块 | [application/admin/controller/Workorder.php:157–308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:157>) |
| 最终 | 3 | 文本块 | [application/api/controller/Index.php:440–646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) |
| 最终 | 4 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) |
| 最终 | 5 | 文本块 | [update.sql:115–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) |
| 最终 | 6 | 文本块 | [application/api/controller/Index.php:268–327](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:268>) |
| 最终 | 7 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) |
| 最终 | 8 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1–65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) |
| 最终 | 9 | 文本块 | [application/api/controller/Index.php:1027–1085](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1027>) |
| 最终 | 10 | state | [public/assets/libs/jquery/src/deferred.js:64–64](</Users/teriri/IdeaProjects/1086-backend/public/assets/libs/jquery/src/deferred.js:64>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [application/api/controller/Index.php · takeOrder](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:744>) | 无 | 无 |

查询耗时：4870.9 → 5680.1 毫秒。

## idea-1086-backend-03 · 1086-backend

```text
search_code("不同公司、业务员和抵押经办人查看申请列表时，哪里决定各自能看到哪些工单？")
```

搜索意图：定位工单分页查询中的公司和角色数据范围过滤

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：未覆盖 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [application/api/controller/Index.php:764–820](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>) |
| 基线 | 2 | 文本块 | [update.sql:1–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:1>) |
| 基线 | 3 | 文本块 | [application/admin/controller/Workorder.php:1–115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:1>) |
| 基线 | 4 | 文本块 | [application/api/controller/Index.php:440–481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) |
| 基线 | 5 | 文本块 | [application/admin/controller/Workorder.php:247–306](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) |
| 基线 | 6 | 文本块 | [application/api/controller/Index.php:529–586](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:529>) |
| 基线 | 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1–65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) |
| 基线 | 8 | 文本块 | [application/admin/view/workorder/edit.html:211–260](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/edit.html:211>) |
| 基线 | 9 | 文本块 | [application/admin/controller/Workorderimage.php:1–103](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) |
| 基线 | 10 | 文本块 | [application/common/model/Workorder.php:1–43](</Users/teriri/IdeaProjects/1086-backend/application/common/model/Workorder.php:1>) |
| 最终 | 1 | 文本块 | [application/api/controller/Index.php:704–880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) |
| 最终 | 2 | 文本块 | [update.sql:1–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:1>) |
| 最终 | 3 | 文本块 | [application/admin/controller/Workorder.php:1–115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:1>) |
| 最终 | 4 | 文本块 | [application/api/controller/Index.php:388–481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) |
| 最终 | 5 | 文本块 | [application/admin/controller/Workorder.php:247–308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) |
| 最终 | 6 | 文本块 | [application/api/controller/Index.php:529–646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:529>) |
| 最终 | 7 | 文本块 | [application/api/controller/Index.php:1027–1085](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1027>) |
| 最终 | 8 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) |
| 最终 | 9 | 文本块 | [application/admin/validate/Workorderfile.php:1–27](</Users/teriri/IdeaProjects/1086-backend/application/admin/validate/Workorderfile.php:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [application/api/controller/Index.php · workorder](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:774>) | 无 | 无 |

查询耗时：5545.6 → 5796.2 毫秒。

## idea-1086-backend-04 · 1086-backend

```text
search_code("查看一份贷款申请时，哪里判断每项要求的资料是否已经上传过文件或图片？")
```

搜索意图：定位工单详情中材料项完成标记的来源

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：未覆盖 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [application/api/controller/Index.php:821–880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:821>) |
| 基线 | 2 | 文本块 | [application/admin/view/workorder/upload.html:1–22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) |
| 基线 | 3 | 文本块 | [application/admin/controller/Workorder.php:59–115](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:59>) |
| 基线 | 4 | 文本块 | [application/admin/view/loantype/index.html:1–29](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/loantype/index.html:1>) |
| 基线 | 5 | 文本块 | [application/api/validate/IndexValidate.php:1–63](</Users/teriri/IdeaProjects/1086-backend/application/api/validate/IndexValidate.php:1>) |
| 基线 | 6 | 文本块 | [application/api/controller/Index.php:647–703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) |
| 基线 | 7 | 文本块 | [application/api/controller/Index.php:482–528](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:482>) |
| 基线 | 8 | 文本块 | [application/admin/view/workorderfile/edit.html:1–53](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/edit.html:1>) |
| 基线 | 9 | Upload.events.onUploadComplete | [public/assets/js/require-upload.js:94–117](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/require-upload.js:94>) |
| 基线 | 10 | 文本块 | [application/admin/view/workorder/add.html:205–251](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/add.html:205>) |
| 最终 | 1 | 文本块 | [application/api/controller/Index.php:764–993](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:764>) |
| 最终 | 2 | 文本块 | [application/admin/view/workorder/upload.html:1–22](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/upload.html:1>) |
| 最终 | 3 | 文本块 | [application/admin/controller/Workorder.php:59–156](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:59>) |
| 最终 | 4 | 文本块 | [application/admin/view/loantype/index.html:1–29](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/loantype/index.html:1>) |
| 最终 | 5 | 文本块 | [application/api/validate/IndexValidate.php:1–63](</Users/teriri/IdeaProjects/1086-backend/application/api/validate/IndexValidate.php:1>) |
| 最终 | 6 | 文本块 | [application/api/controller/Index.php:647–703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) |
| 最终 | 7 | 文本块 | [application/api/controller/Index.php:440–586](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) |
| 最终 | 8 | 文本块 | [application/admin/view/workorderfile/edit.html:1–53](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorderfile/edit.html:1>) |
| 最终 | 9 | Upload.events.onUploadComplete | [public/assets/js/require-upload.js:94–117](</Users/teriri/IdeaProjects/1086-backend/public/assets/js/require-upload.js:94>) |
| 最终 | 10 | 文本块 | [application/admin/view/workorder/add.html:205–251](</Users/teriri/IdeaProjects/1086-backend/application/admin/view/workorder/add.html:205>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [application/api/controller/Index.php · workorderDetail](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:833>) | 无 | 无 |

查询耗时：5388.5 → 5828.7 毫秒。

## idea-1086-backend-05 · 1086-backend

```text
search_code("审核人员提交处理结论时，哪里确认这张单正在由本人办理，再保存新的审核状态？")
```

搜索意图：定位工单审核提交的状态及归属限制

原冻结声明锚点覆盖名次：**2 → 2**；原冻结关键行为联合覆盖：**2 → 2**。

审计关键行为联合覆盖：**2 → 2**；冻结范围全部非空行覆盖：2 → 2。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [application/api/controller/Index.php:587–646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:587>) |
| 基线 | 2 | 文本块 | [application/api/controller/Index.php:704–763](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) |
| 基线 | 3 | 文本块 | [update.sql:115–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) |
| 基线 | 4 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) |
| 基线 | 5 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) |
| 基线 | 6 | 文本块 | [application/api/controller/Index.php:440–481](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:440>) |
| 基线 | 7 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1–65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) |
| 基线 | 8 | 文本块 | [application/admin/controller/Workorder.php:247–308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:247>) |
| 基线 | 9 | 文本块 | [application/admin/model/Workorderfile.php:1–57](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorderfile.php:1>) |
| 基线 | 10 | 文本块 | [application/admin/model/Workorder.php:1–57](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorder.php:1>) |
| 最终 | 1 | 文本块 | [application/api/controller/Index.php:587–646](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:587>) |
| 最终 | 2 | 文本块 | [application/api/controller/Index.php:704–880](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:704>) |
| 最终 | 3 | 文本块 | [update.sql:115–154](</Users/teriri/IdeaProjects/1086-backend/update.sql:115>) |
| 最终 | 4 | 文本块 | [application/admin/lang/zh-cn/workorderfile.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderfile.php:1>) |
| 最终 | 5 | 文本块 | [application/admin/lang/zh-cn/workorderimage.php:1–18](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorderimage.php:1>) |
| 最终 | 6 | 文本块 | [application/api/controller/Index.php:388–528](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:388>) |
| 最终 | 7 | 文本块 | [application/api/controller/Index.php:268–327](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:268>) |
| 最终 | 8 | 文本块 | [application/admin/lang/zh-cn/workorder.php:1–65](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/workorder.php:1>) |
| 最终 | 9 | 文本块 | [application/admin/controller/Workorder.php:198–308](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:198>) |
| 最终 | 10 | 文本块 | [application/admin/model/Workorderfile.php:1–57](</Users/teriri/IdeaProjects/1086-backend/application/admin/model/Workorderfile.php:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [application/api/controller/Index.php · checkOrder](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:712>) | 无 | 无 |

查询耗时：5724.1 → 4866.1 毫秒。

## idea-1086-backend-06 · 1086-backend

```text
search_code("后台怎样把一张申请的全部附件和身份证照片打成压缩包，下载后清理临时文件？")
```

搜索意图：定位工单附件打包、HTTP 下载及临时资源清理流程

原冻结声明锚点覆盖名次：**1 → 1**；原冻结关键行为联合覆盖：**1 → 1**。

审计关键行为联合覆盖：**1 → 1**；冻结范围全部非空行覆盖：1 → 1。

| 侧别 | 名次 | 返回符号 | 实际返回路径与行号 |
| --- | --- | --- | --- |
| 基线 | 1 | 文本块 | [application/admin/controller/Workorder.php:116–197](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>) |
| 基线 | 2 | 文本块 | [application/admin/controller/general/Attachment.php:1–160](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/general/Attachment.php:1>) |
| 基线 | 3 | 文本块 | [application/api/controller/Index.php:1134–1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) |
| 基线 | 4 | 文本块 | [application/api/controller/Index.php:647–703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) |
| 基线 | 5 | 文本块 | [application/admin/controller/Workorderfile.php:1–54](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderfile.php:1>) |
| 基线 | 6 | 文本块 | [application/admin/controller/Workorderimage.php:1–43](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) |
| 基线 | 7 | 文本块 | [application/admin/controller/Frontpackage.php:1–124](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Frontpackage.php:1>) |
| 基线 | 8 | 文本块 | [application/api/controller/Index.php:1469–1528](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1469>) |
| 基线 | 9 | 文本块 | [application/common/library/Upload.php:213–313](</Users/teriri/IdeaProjects/1086-backend/application/common/library/Upload.php:213>) |
| 基线 | 10 | 文本块 | [application/admin/lang/zh-cn/general/attachment.php:1–41](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/general/attachment.php:1>) |
| 最终 | 1 | 文本块 | [application/admin/controller/Workorder.php:59–197](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:59>) |
| 最终 | 2 | 文本块 | [application/admin/controller/general/Attachment.php:1–160](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/general/Attachment.php:1>) |
| 最终 | 3 | 文本块 | [application/api/controller/Index.php:1134–1188](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:1134>) |
| 最终 | 4 | 文本块 | [application/api/controller/Index.php:647–703](</Users/teriri/IdeaProjects/1086-backend/application/api/controller/Index.php:647>) |
| 最终 | 5 | 文本块 | [application/admin/controller/Workorderfile.php:1–151](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderfile.php:1>) |
| 最终 | 6 | 文本块 | [application/admin/controller/Workorderimage.php:1–210](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorderimage.php:1>) |
| 最终 | 7 | 文本块 | [application/admin/controller/Frontpackage.php:1–124](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Frontpackage.php:1>) |
| 最终 | 8 | 文本块 | [application/admin/lang/zh-cn/frontpackage.php:1–9](</Users/teriri/IdeaProjects/1086-backend/application/admin/lang/zh-cn/frontpackage.php:1>) |

Top-10 缺行按各目标的声明及冻结关键行为逐行核对，缺行列表不会因同文件的其他相同语句而算作覆盖。

| 目标口径 | 预定目标 | 基线缺行 | 最终缺行 |
| --- | --- | --- | --- |
| 原冻结 | [application/admin/controller/Workorder.php · download](</Users/teriri/IdeaProjects/1086-backend/application/admin/controller/Workorder.php:116>) | 无 | 无 |

查询耗时：5465.5 → 5641.8 毫秒。
