# 自然语言函数定位验收

日期：2026-09-05。语料为当前 Lucia 工作区，包括原有未提交改动；评测脚本、本进度文件和
本报告不进入索引。模型为真实 `BAAI/bge-m3`（1024 维）与 `Qwen/Qwen3-Reranker-4B`，
密钥仅通过测试进程环境传入。复用按请求内容寻址的模型响应缓存，不使用伪向量或伪重排。

## 方案与边界

- 不改嵌入模板、存储版本、插件权限或 Agent 工具协议。
- 自然语言扩大词法与语义召回深度，输出一条结果也保留足够候选；远端重排候选通常为 40–48 条。
- 重排携带受限真实正文，不仅比较签名。自然语言采用模型相关度并保留测试降权；精确与混合
  查询保留既有符号保护。模型失败仍降级到本地排序，不把相关度当成命中概率。
- 曾试验定义字段与正文字段独立归一化，但离线整体召回没有改善且精确定义回归，因此未保留。

## 八条业务查询

目标判据为文件与具体符号同时命中，不把同文件的调用点或其他方法算作成功。

| 指标 | 改动前内部检索 | 改动后内部检索 | 改动后真实工具入口 |
| --- | --- | --- | --- |
| 目标定义 Top-1 | 4/8 | 6/8 | 6/8 |
| 目标定义 Top-3 | 6/8 | 7/8 | 7/8 |
| 目标定义 Top-10 | 6/8 | 8/8 | 8/8 |

| 自然语言查询 | 返回的目标符号 | 工具名次 | 验收时行号 |
| --- | --- | --- | --- |
| 代码搜索请求参数实体在哪里定义 | `CodeSearchRequest` | 1 | `src/tool.rs:35–46` |
| 插件管理页面怎么绘制 | `render_native_plugin_dialog` | 1 | `crates/agent-tui/src/command/render.rs:197–414` |
| 工具调用在什么情况下需要用户审批 | `requires_approval` | 1 | `examples/plugins/permission-plugin/src/lib.rs:893–896` |
| 代码索引进度怎么发布到终端底栏 | `CodeContextPlugin::emit_index_progress` | 1 | `src/lib.rs:731–808` |
| 思考等级选择面板怎么渲染 | `render_native_reasoning_panel` | 1 | `crates/agent-tui/src/command/render.rs:159–195` |
| 驼峰命名的标识符如何拆成可检索的词 | `split_camel_case` | 1 | `src/retrieval/tokenize.rs:51–77` |
| 仓库代码搜索请求如何执行并打包代码片段 | `CodeContextPlugin::search` | 3 | `src/lib.rs:930–1029` |
| 文件修改后如何增量刷新代码索引 | `CodeContextPlugin::advance_index` | 7 | `src/lib.rs:615–729` |

表中 `src/` 相对于 `examples/plugins/code-context-plugin/`。行号为验收时快照，后续编辑可能移动。
真实入口为 `call_tool_with_host` 调用 `code_search`，经过生产检索、父子抑制、正文回填及 token
预算。每条返回的源码都与 `lines` 对应；仅对显式 `truncated` 结果剥离协议规定的截断提示。
未使用真实 App 仓库，不能宣称已验收下单接口定位；八条查询也不能代表跨仓库普遍准确率。

## 另外十六条 Agent 场景

复用原有 `agent_scenario_report` 的查询与答案，不改评测目标。这组只考察文件命中，不能用
其 100% Top-10 代替函数定位准确率。

| 指标 | 改动前 | 改动后 |
| --- | --- | --- |
| 文件 Top-1 | 8/16（50.0%） | 11/16（68.8%） |
| 文件 Top-3 | 10/16（62.5%） | 14/16（87.5%） |
| 文件 Top-10 | 14/16（87.5%） | 16/16（100%） |
| MRR | 0.616 | 0.786 |

最终一轮平均检索延迟约 1745 ms，16 条自然语言查询均触发重排；这是本机与当次端点的结果，
不作为跨环境 SLA。基线部分请求已缓存，不能直接计算前后延迟倍率。审批查询的目标文件
名次从第五退到第八，流式增量从第二退到第三，工具注册从第一退到第二；总体提升不代表
所有查询都改善，仍需扩充独立查询集。

## 检查

- 插件离线测试：397 项单元、9 项检索基准、6 项离线报告、1 项持久化及 1 项文档测试通过。
- `cargo clippy --offline -p code-context-plugin --all-targets` 通过。
- `bun run build:plugin:code-context` 与 9 项真实 WASM Host smoke test 通过。
- 真实模型的八条函数定位和十六条 Agent 场景分别运行，不以离线伪向量测试替代模型验收。

## 复现

在宿主环境配置 `SILICONFLOW_API_KEY`，然后执行：

```sh
RETRIEVAL_CACHE=warm RETRIEVAL_LOCATION_REPORT=/tmp/lucia-location.json \
  cargo test --offline -p code-context-plugin --test retrieval_report \
  implementation_location_report -- --ignored --nocapture
```

`warm` 复用真实响应缓存并跳过缓存场景的嵌入请求间隔；`RETRIEVAL_CACHE=off` 强制重新请求。
报告包含分路、union、融合、最终函数名次，以及实际工具位置。索引未就绪、重排失败、目标
定义未进入 Top-10 或正文与行号不一致均使测试失败。缓存冷热与远端波动不同，不能把不同
轮次的耗时直接当严格性能 A/B；自然语言现在会增加重排请求，精确符号查询不受影响。
