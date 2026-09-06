# 全语言 AST 分块技术验收记录

日期：2026-09-06。**两轮完整 A/B 均未通过业务检索验收，实验代码与依赖已回退，只保留报告及完整补丁。** 以下语言能力与实现细节均指实验快照，不代表当前运行代码已启用这些能力。

48 条新业务的首位命中为 **28 → 23 → 26**，MRR 为 **0.6974 → 0.6273 → 0.6655**。
第二轮相对基线双侧 p=0.5811、两次尝试校正 p=1；完整实现覆盖没有显著改善。
[72 条搜索语义与实际 Top-10](./all-language-ast-search-results-2026-09-06.md) 可直接逐条复核。

## 1. 目标与边界

本次把已有索引白名单中的源码、配置和文档接入各自的成熟语法解析路径，范围包括 23 个语言类别、51 个扩展名与 11 个特殊文件名。纯文本仍明确按文本处理。语言和方言名称用于语法分派；没有加入任何仓库业务词、项目路径或符号白名单。

统一的是声明、作用域、原文行号与分块输出契约，不要求所有语言换成同一个底层解析器。保留 syn、Oxc、rustpython-parser、gosyn 与 html5gum；缺失语法采用单个 Tree-sitter 运行时及成熟 grammar。Tree-sitter 提供具体语法树，适配器从其命名节点和字段提取声明及无名结构，不能把“加载 grammar 成功”当作“完整代码检索通过”。

思路参考仅来自 [OCE 固定提交 a359272560bbbdb321055aaed6c16ba1f4e06887 的 AST 分块实现](https://github.com/oce-ai/oce/blob/a359272560bbbdb321055aaed6c16ba1f4e06887/src/oce/infrastructure/astchunk/astchunk_builder.py#L327-L378)：沿语法边界递归拆大节点、保留可容纳的声明、合并过小片段。本次没有照搬 OCE 的经验阈值、Vue 正则方案、嵌入模型或重排实现，也没有运行同真值的 OCE 对照；**不声称 OCE 已证明更准**。完整前期核验位于 [OCE 源码研究记录](../target/oce-research-2026-09-06/report.md)。

## 2. 已验证的语言与方言矩阵

[语言矩阵测试](../target/all-language-ast-implementation-2026-09-06/implementation-v2/src/parser/language_matrix_tests.rs) 从真实路径分类入口调用完整 parse_file 链，冻结声明名、声明首末行与正文锚点，并检查所有返回区间不越界。51 个扩展名和 11 个特殊文件名分别计数，不使用单一 JavaScript 示例代表 JSX/TSX 的特有语法。

| 语言／文件 | 路由与核验内容 |
| --- | --- |
| Rust：rs | syn；声明、trait/impl、嵌套语句、导入与文档 |
| TypeScript／JavaScript：ts、tsx、mts、cts、js、jsx、mjs、cjs | Oxc 按实际扩展名确定语法；另测 JSX/TSX 标签、TS 断言及 module/script 边界 |
| Python：py、pyi | rustpython-parser；函数、类、复合语句和模块文档 |
| Go：go | gosyn；函数、方法接收者、复合语句与 Unicode 字符偏移 |
| Vue：vue、nvue | html5gum 识别真实区域；脚本交给 Oxc，模板交给 HTML，style 按 CSS 方言处理 |
| Java：java | Tree-sitter；类、接口、枚举、构造函数、方法及错误恢复后的健康兄弟 |
| PHP：php | PHP grammar；命名空间、类与函数；仅真实模板文本节点再次进入 HTML，字符串里的标签不制造函数 |
| C／C++：c、h、cc、cpp、cxx、hpp、hh | 各自 grammar；h 先按 C，只有 C 有错且 C++ 全树有效时切换 |
| C#：cs | C# grammar；类型、方法、属性及命名空间 |
| Ruby：rb、Rakefile、Gemfile | Ruby grammar；声明与无名调用块分别保留 |
| Swift：swift | Swift grammar；类型、函数与扩展 |
| Kotlin：kt、kts | Kotlin grammar；类型、函数与脚本结构 |
| Shell：sh、bash、zsh、fish | Bash／Zsh／Fish 分别使用实际 grammar；含解释器判定用例 |
| Makefile | Make grammar，目标与正文区间 |
| Dockerfile、Containerfile | Dockerfile grammar，无名指令区域，不假装成 Bash |
| Justfile | 固定 Just 官方 parser/scanner，recipe 声明与正文 |
| SQL：sql | tree-sitter-sequel；按该 grammar 已验证的声明和语句提取；不宣称覆盖所有厂商存储过程扩展 |
| Markdown：md、README、CHANGELOG | mdast 段落／代码围栏；已知围栏交给对应代码解析器，未知围栏仍是文档内容 |
| MDX：mdx | mdast 加 Oxc 校验嵌入 JavaScript；保留 ESM、表达式与属性回调原行号 |
| JSON／JSONC：json、jsonc | jsonc-parser 显式区分严格 JSON 和仅放宽注释／尾逗号的 JSONC |
| TOML：toml；YAML：yaml、yml | 各自 grammar，保留配置区域，不给配置键制造函数 |
| HTML：html、htm | HTML grammar；真实 script/style 再按其语言解析，JSON script MIME 使用严格 JSON |
| 样式：css、scss、sass、less | Raffia 四种真实方言，含缩进 Sass；先做资源保护，再生成语法区域 |
| WIT：wit；Proto：proto；GraphQL：graphql、gql | 各自 grammar，WIT 接口函数、Proto 消息与 GraphQL 类型保留真实范围 |
| txt、LICENSE、NOTICE、CODEOWNERS | 明确为纯文本，不计作 AST 成功 |

以上证明白名单路由和固定样本正确，不等于所有方言扩展及错误组合都已穷举。Java 的中文 CRLF 错误恢复用例保留两个健康方法的精确声明范围。Kotlin-ng 1.1.0 对一种合法单行类方法写法仍会报告 MISSING _class_member_semi；规范换行样例通过，因此必须保留 Partial 状态，不能把任何恢复树当作完整成功。

## 3. 解析器版本与来源

以下版本来自本轮实际依赖解析，而非“最新版本”的推测。

| 组件 | 固定或实际解析版本 |
| --- | --- |
| 统一 Tree-sitter 运行时／LanguageFn | tree-sitter 0.26.13；tree-sitter-language 0.1.8 |
| Java／PHP／Ruby | 0.23.5／0.24.2／0.23.1 |
| C／C++／C# | 0.24.2／0.23.4／0.23.5 |
| Swift／Kotlin | tree-sitter-swift 0.7.3；tree-sitter-kotlin-ng 1.1.0 |
| Bash／Zsh／Fish | 0.25.1／0.63.4／3.6.0 |
| SQL | tree-sitter-sequel 0.3.11 |
| TOML／YAML／HTML | tree-sitter-toml-ng 0.7.0；yaml 0.7.2；html 0.23.2 |
| Make／Dockerfile | tree-sitter-make 1.1.1；tree-sitter-dockerfile-updated 0.2.0 |
| WIT／Proto／GraphQL | 0.2.0／0.6.0／0.2.1 |
| 样式 AST／前置词法保护 | raffia 0.13.0；cssparser 0.37.0，后者禁用默认加速 feature |
| Markdown／JSON | markdown 1.0.0；jsonc-parser 0.33.1 |
| 保留的既有解析器 | syn 3.0.4；Oxc 0.148.0；rustpython-parser 0.4.0；gosyn 0.2.15；html5gum 0.8.4 |
| C 编译胶水 | cc 1.2.66，为本轮锁定版本 |

最终插件的 Tree-sitter 适配枚举共 21 种语法，包含 vendor Just；它不是前期 32 入口探针的计数。Dockerfile-updated 和 WIT 的版本约束能够共享 0.26.13，无需第二份 native runtime。没有引入 arborium 主运行时或整套 371 语言包。tree-sitter-css、tree-sitter-md 和 tree-sitter-json 仅参与过前期探针，最终插件由专用解析器负责这些语法，不保留冗余 grammar 依赖。

Just 0.2.0 原始 Rust 绑定要求 tree-sitter ~0.25.5，与选定运行时冲突，因此仅原样保留其 parser.c、scanner.c、头文件及 node-types.json，插件 build.rs 静态编译并用 LanguageFn 连接。固定提交为 5685543a6e64f66335e25518c9ae8ffa1dae3d01，发布包 SHA-256 为 bd46794f2c270f43d73b2f2c8c256992471f3493c903604da0886df984771920。发布包未带 LICENSE，已从同一固定提交补齐 MIT 许可，详见 [Just 来源记录](../target/all-language-ast-implementation-2026-09-06/implementation-v2/vendor/tree-sitter-just/PROVENANCE.md)。

## 4. 分块、嵌入与缓存语义

每个解析器输出 Declaration、StructuralRegion 与 has_errors。声明继续提供完整符号导航；无名语句、配置和文档仅提供区域。健康 AST、错误区域与文本回退并存时标记 Partial，不将局部失败计作完整结构化成功。当前索引统计保持既有字段，Partial 保守计入 text_files。

[公共区域装配](../target/all-language-ast-implementation-2026-09-06/implementation-v2/src/parser/regions.rs) 以 AST 边界和原文件行范围补齐长声明尾部，按既有 1200 字符／60 行预算生成有界语义片段。小声明原有表示保留；属于完整声明的子区间只补语义召回，不重复加入词法正文。单个语法叶节点过长时局部行切并标记降级，不丢整文件。声明起止行是硬边界，防止将上一函数尾部与下一函数文档拼成桥接窗口，影响最终输出的符号元数据。子区间继承父声明文档同样限制为 600 字符。

正式 Host 暴露的符号回归也已修复：单个标识符优先匹配语料中的完整名字，不把 camelCase 拆出的父类型词元提升为精确限定名。含连字符的 WIT 名称、Ruby 问号方法由实际完整符号裁决，不增加语言或业务白名单。

这是索引侧覆盖改进，查询时没有新增生成调用、HyDE 或查询改写。每块嵌入正文仍受原 1200 字符上限约束，本次通过结构化子区间让长函数后半段获得独立表示；没有宣称把所有长函数全文塞入单条向量。

结构索引版本从 4 升为 5，旧结构进入重建。manifest schema 仍为 1，插件版本仍为 0.1.0，两者不跟随结构版本联动。向量新鲜度由正文摘要加实际注入的父符号、签名、文档和导入共同确定：只有结构语境变化也重新嵌入，输入完全一致可复用；旧版纯正文摘要自然失配。不需要手工删除 SQLite 数据库或其 -wal／-shm。跨实例重开、仅元数据改变和旧摘要一次性迁移均有回归测试。

## 5. 原生、WASI 与资源探针

全部证据已选取复制到 [持久化运行时证据目录](../target/all-language-ast-implementation-2026-09-06/runtime-evidence/manifest.json)，归档文件清单及每个文件的 SHA-256 均可核对。没有复制模型密钥、完整临时工作区或构建缓存。探针与完整插件必须分开解读。

### 5.1 静态运行时 32 项解析

独立 crate 使用官方 Tree-sitter 0.26.13，在 aarch64-apple-darwin 与 wasm32-wasip2 编译，实际解析 26 个 Tree-sitter 入口（含 PHP-only、Markdown inline 等方案比较入口）、Raffia 四方言、MDX、JSONC，共 32 项。每项检查真实语法树与错误标记，不止 set_language 成功。

WASI 由独立 Wasmtime 46.0.3 宿主执行，不继承目录、环境或网络，线性内存限制 64 MiB。全部成功，最大线性内存申请 30,146,560 字节；调试组件大小 55,738,148 字节。该探针多带了最终删除的比较 grammar，也不包含 Lucia 原有全部解析器、SQLite、索引数据和完整 ABI，**不能把这两个数字当成正式插件体积或内存峰值**。

选定 grammar 的全部 scanner 均为 C。最终完整探针只需 WASI SDK 34 的 CC、AR 与 sysroot，无需 CXX，也没有为了构建 grammar 修改 crates 或宿主接口。[WASI 实际输出](../target/all-language-ast-implementation-2026-09-06/runtime-evidence/wasi-run.log)。

### 5.2 Raffia 失败与保护

最初在构建 AST 后限制遍历深度，无法阻止第三方解析器先耗尽栈；小文件的合法深层样式因此也会中止解析。进一步实测发现：原生允许 64 层并不代表 WASI 安全，WASI 调试组件在 48 层出现 out-of-bounds memory access。

最终在 Raffia 前使用成熟 cssparser 词法的括号／函数嵌套入口，并对缩进 Sass 加保守缩进层数预算，合并上限为 32。它只负责资源拒绝，不据此识别声明、生成分块或宣称语法合法；字符串与块注释中的括号不会增加预算。没有维护 Raffia fork。

[最终 WASI 保护日志](../target/all-language-ast-implementation-2026-09-06/runtime-evidence/style-budget-wasi.log) 证明：Sass 与括号样式 16／32 层真实 AST 成功；Sass 33／256 层和括号 33／48／64／256／2048 层在 AST 前安全拒绝，宿主正常退出。输入超过 4 MiB 时在第三方解析前降级。代价是极深但合法的样式会使用文本回退，这是明确的资源限制。

### 5.3 JSON／Markdown 的 48 个边界实例

另以每例独立 WASI 实例、64 MiB 和 5000 万 fuel 运行有限矩阵：[完整结果](../target/all-language-ast-implementation-2026-09-06/runtime-evidence/document-stack/README.md)。

- JSON／JSONC 的数组、对象分别测试 128／256／511／512／1000 层，debug 和与仓库一致的 release 各跑一轮。数组至 512 层成功并释放 AST，1000 层安全返回深度错误；对象因属性也计入官方 range_stack，511 层起安全返回错误。没有栈 trap。
- Markdown 引用和 MDX 静态标签分别测试 128／512 层。release 均成功构建并释放 AST，实际树深度为 130／514，最高线性内存申请 3,342,336 字节。
- debug 的两项 512 层文档被 5000 万 fuel 安全中止；release 同样例在该限制内完成。它不是解析前栈故障。
- MDX 边界样本不含 JavaScript 表达式，不能据此证明任意 Oxc 嵌入输入；也不把有限深度试验扩大成任意输入的安全证明。

## 6. 当前完整插件验证状态

| 验证项 | 已有证据与当前状态 |
| --- | --- |
| 最终原生测试 r2 | 490 单元、9 benchmark、6 report、1 本地 e2e、1 doc，共 507 项通过；11 项按原定义 ignored |
| 最终 Clippy r2 | 用户要求的 all-targets、test-support 检查通过，无警告 |
| 父文档 600 字符及符号回归 | regions 3 项定向检查与完整 r2 已通过；r2 同时覆盖 Vue 文件级脚本预算、完整符号匹配修复 |
| 正式 WASM 构建 | 标准分发入口通过，无额外 CXX 环境变量；最终组件 36,126,365 字节，SHA-256 为 81c305d131f8ce1af99ccd19f8bee52388533f7d44f8b090820ff689afe73fec |
| 正式 Host 全链验收 | 13/13 测试、44 条真实 code_search 调用通过：28 个精确声明首位定位、11 种无名结构、PHP 假脚本排除、CSS/Sass 32/33 层资源边界；默认 64 MiB / 5000 万 fuel，初始线性内存 28.625 MiB，未测峰值 |
| 正式原生实验二进制 | SHA-256 为 555842638eb4184681738c0d4c01b7e562788a5d305bb605ca1e133e9864153a，37,408,400 字节；包含全部行为修复，仅有随后两处文档注释更正 |
| 第二轮结构验证 | 509 项原生测试通过、11 原有 ignored；Clippy 无警告；真实 WASM Host 13/13、44 条调用再次通过 |
| 真实仓库准确率与耗时 | 两轮完整 A/B 已结束，均未通过业务检索验收，详见第 8 节 |

正式 Host 的每条请求与真实返回源码见 [逐条报告](../target/all-language-ast-2026-09-06/host-smoke/full-plugin-host-report.md)、
[原始结果](../target/all-language-ast-2026-09-06/host-smoke/queries.jsonl)；
组件摘要和完整日志保存在同一目录。这组夹具验证协议、坐标及结构导航，业务质量由十二仓 A/B 判断。

### 6.1 真实大仓默认 Host 的已证限制

完整 fixture 通过后，另以真实 `/Users/teriri/IdeaProjects/1086-backend` 全根目录进行冷启动对照。
不注入 HTTP／模型服务，不上传源码；只限制检索结果路径，不缩小索引根目录。基线与实验使用
同一真实 Host 测试二进制、独立状态目录和默认 64 MiB／每次 5000 万 fuel，未放宽额度。

基线从明确 HEAD `94027dac61609ea9c60b14f5aacb1f6ffd7c29b7` 的全部插件文件在独立临时工作区
重新构建，未使用无法确定源码版本的旧 WASM。两轮均在连续 20 次 `indexing` 后，第 21 次调用
返回 `fuel_exhausted`；最后已返回的 files／chunks／symbols 都为 0，没有获得成功检索结果。
这一统计不能单独确定失败发生在目录扫描还是首文件处理内部，也不能将共同失败归因于新 AST。

| 指标 | HEAD 基线 | 最终实验 |
| --- | --- | --- |
| component 大小 | 5,510,230 字节 | 36,126,365 字节 |
| 声明初始线性内存 | 2.5 MiB | 28.625 MiB |
| 全根结构索引结果 | 第 21 次调用 `fuel_exhausted` | 第 21 次调用 `fuel_exhausted` |
| 全仓动态峰值内存 | 未验证 | 未验证 |

实际搜索语义为定位 `Index.php:744` 的 `takeOrder`，核验接受工单时写入状态与当前接单用户。
请求与失败响应、哈希和全部调用统计见 [大仓对照报告](../target/all-language-ast-2026-09-06/host-smoke/real-1086-default-host-report.md)、
[实验原始日志](../target/all-language-ast-2026-09-06/host-smoke/real-1086-default-host.log)、
[HEAD 基线原始日志](../target/all-language-ast-2026-09-06/host-smoke/real-1086-baseline-default-host.log)
及 [基线构建来源与哈希](../target/all-language-ast-2026-09-06/host-smoke/real-1086-baseline-component.json)。
因此只能确认小型多语言 fixture 的完整 Host 验收通过，不能宣称真实大仓已在默认 Host 下可用，
也不能用原生 API 评测结果替代这一运行限制；本次没有越过范围修改 Host 或放宽内存继续跑。

### 6.2 两轮原生 API 索引与查询成本

基线、第一轮正式臂 experiment-final、第二轮 restored-vue-context 均完整保存 72 条；初始诊断 experiment 保留 65／72 条及失败日志。索引阶段从 stderr 的“索引完成”计时提取，批次墙钟还包括进程启动、源码快照和最多 6 条查询，不能相互替代。[两轮逐仓成本表](../target/all-language-ast-implementation-2026-09-06/index-cost-v2/report.md)、[逐批汇总](../target/all-language-ast-implementation-2026-09-06/index-cost-v2/summary.json) 和 [最终查询／缓存规模](../target/all-language-ast-implementation-2026-09-06/index-cost-v2/final-resource-summary.json) 均独立归档，第一轮成本文件及所有原始数据未改。

| 运行记录 | HEAD 基线 | 初始诊断 AST | 正式第一轮 AST | 第二轮恢复 Vue 上下文 |
| --- | --- | --- | --- | --- |
| 完成查询 | 72／72 | 65／72 | 72／72 | 72／72 |
| 全部批次墙钟合计 | 490.0 秒 | 2204.8 秒 | 824.5 秒 | 462.9 秒 |
| 其中失败批次墙钟 | 17.7 秒 | 1260.1 秒 | 0 秒 | 0 秒 |
| 有完成标记的索引阶段合计 | 18.2 秒 | 635.1 秒 | 342.7 秒 | 78.6 秒 |
| 最终可用结构规模 | 4876 文件／37530 块／20609 符号 | 1086 未完成，不合并比较 | 4876 文件／83237 块／49155 符号 | 4876 文件／85204 块／49155 符号 |

该表不是严格冷启动速度对照：基线使用既有向量快照，第一轮正式 AST 复用初始诊断已持久化的向量，第二轮又复用第一轮缓存。重复进程启动仍重建原生内存里的结构索引。失败批次即使已打印索引完成，后续查询失败也计入失败墙钟，不能只挑成功批次。

1086-backend 的 2682 文件在两轮 AST 下均产生 50972 块、30314 符号。初始诊断三次约 420 秒超时，共 **1260.068 秒**，每次都没有索引完成标记，无法给出精确索引时长。第一轮正式首批续用同一向量库，墙钟 **353.781 秒，其中索引 312.2 秒**；此前失败加首次成功共观察到 1613.848 秒批次支出，含查询和进程开销，不能简写为“冷索引 353 秒”。随后单条查询进程索引 14.9 秒、批次 26.758 秒。第二轮该仓两个已热缓存批次的索引分别为 14.9／15.0 秒，批次合计 78.787 秒；这不代表重新向量化大仓只需 30 秒。

| 三个完整 72 条运行 | 查询中位数 | P90 | P95 |
| --- | --- | --- | --- |
| HEAD 基线 | 6.002 秒 | 8.376 秒 | 10.469 秒 |
| 第一轮 AST | 5.058 秒 | 10.394 秒 | 16.604 秒 |
| 第二轮恢复 Vue 上下文 | 4.345 秒 | 7.040 秒 | 9.526 秒 |

查询时间只使用逐条 query_ms，不分摊索引或批次墙钟，也不据不同时间段的 API 波动单独推断算法提速。索引成本、检索质量与远程 API 延迟分别报告。

第二轮结束后，只读检查 12 个共享实验缓存，实际持久化 **69779 条向量**，其中 1086-backend 为 42496 条。所有数据库当时均无 -wal，使用 immutable 只读模式查询普通元数据表，读取前后核对大小／修改时间和 WAL 状态；没有创建、删除或截断缓存及其日志。该数字是最终缓存快照，不能倒填第一轮向量数。第二轮结构块比第一轮增加 1967，符号数不变；增量属于恢复的 Vue 语义窗口，首轮冷构建成本和失败证据仍单独保留。

## 7. 构建与交付限制

正式目标仍为 wasm32-wasip2，沿用 WASI SDK 静态编译路径。标准分发入口已在未设置 CXX
及 CXXFLAGS 的条件下构建成功，不需改动插件目录外的构建脚本。运行时不下载语法库，
不引入本地模型或外部解析服务。

插件属于根 workspace，增加依赖需要根 Cargo.lock。最小提案新增 31 包，没有删除、
升级或降级已有包，仅修改 code-context-plugin 的依赖清单，详见
[锁变更统计](../target/all-language-ast-implementation-2026-09-06/runtime-evidence/lock-change-summary.json)
及 [可审查补丁](../target/all-language-ast-implementation-2026-09-06/runtime-evidence/Cargo.lock.proposed.patch)。

主 Cargo.lock 曾出现 +321 行且内容与提案一致，恢复后也曾被运行环境重写，具体写入来源
未确认。最终实验代码回退后，再次逐字核对并恢复根锁，git diff 已确认无差异；本轮不需
应用范围例外，也不提交新依赖。其余已有 crates、workflow-plugin 及 artifacts 改动均未触碰。

## 8. 两轮 A/B 结论：不合入运行代码

两轮均执行同一冻结 72 查询：48 条新业务按查询配对，24 条同义表达按十二家族汇总。
基线、第一轮、恢复 Vue 窗口后的第二轮均完整保存。前期诊断臂不替代第一轮，
第二轮原始 p 和两次尝试的 Bonferroni 校正同时保留。

| 48 条新业务 | 基线 | 第一轮 AST | 第二轮恢复 Vue 窗口 |
| --- | ---: | ---: | ---: |
| Recall@1 | 28/48 | 23/48 | 26/48 |
| Recall@3 | 39/48 | 36/48 | 36/48 |
| Recall@10 | 43/48 | 43/48 | 43/48 |
| MRR | 0.697421 | 0.627257 | 0.665451 |

第二轮相对基线 MRR 改善 5、退步 8、持平 35，双侧 p=0.581055，校正 p=1。
相对第一轮 MRR 回升 0.038194，双侧 p=0.375，校正 p=0.750。
恢复窗口修复了一部分共同语境缺失，未证明整体准确率高于原基线。前十总数持平也不等于
所有查询均无损伤：真实样本有改善也有退化，必要保存或调用代码仍可能缺失。

十二个同义家族的平均 MRR 为 0.506944 → 0.404167 → 0.442361；第二轮相对基线 p=1。
第二轮新业务中位/P95 为 4.305/6.714 秒，同义为 5.188/11.505 秒；远端负载及时序未作
随机交替控制，不能把这个观测当作算法提速证明。

三份比较分别有 44、37、17 个变化样本，均完成实际源码与返回片段审计，零待审。
冻结标签漏列等价实现或缺关键被调函数的情况单独解释，没有回填标签以提高分数。
详见 [逐查询搜索语义与 Top-10 结果](./all-language-ast-search-results-2026-09-06.md)；
包含所有原始返回正文的完整报告分别为
[第一轮](../target/all-language-ast-implementation-2026-09-06/analysis-v1-final/report.md)、
[第二轮](../target/all-language-ast-implementation-2026-09-06/analysis-v2-final/report.md)、
[两轮之间](../target/all-language-ast-implementation-2026-09-06/analysis-v1-v2/report.md)。

**两轮均不作为准确率优化合入。** 实验代码与新依赖已回退，运行代码恢复到任务开始时版本；
完整实现保存在 [代码补丁](./experiments/all-language-ast-2026-09-06.patch)，所需根锁改动保存在
[依赖补丁](./experiments/all-language-ast-dependencies-2026-09-06.patch)。补丁是已验证但未通过
业务质量门槛的实验档案，不代表已启用全语言 AST。

回退后的主工作区再次运行用户指定 Cargo 测试与 Clippy（附加 locked/offline 防止改写依赖）：
443 项通过、11 项原有 ignored，Clippy 无警告。代码、README 和根 Cargo.lock 已确认与任务
开始时一致；此次交付只有报告和完整实验补丁，未触碰 crates 等已有改动。

后续应分开验证精确符号导航和自然语言语义表示：先保证共同实现所需的上下文不因声明拆分
丢失，再测试通用的相邻声明或调用关联表示。不能仅因 AST 更精确、或个别查询恢复就认定检索更准。
