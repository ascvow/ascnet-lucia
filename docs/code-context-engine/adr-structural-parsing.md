# ADR：结构化解析

## 状态

已接受，2026-09-03。**本 ADR 修订了此前"只能手写 Tier 2 解析器"的结论。**

## 背景

Phase 3 需要把源文件转换为 `CodeChunk` 与 `Symbol`：识别函数、类型、方法、接口等声明，
给出准确的行区间、限定名、签名与文档注释。

立项期的[插件系统审计](./plugin-system-audit.md)与[架构](./architecture.md)第 12 问认定：

```
Tier 1  完整 AST            —— V1 不提供（工具链缺失）
Tier 2  纯 Rust 结构化解析   —— 按 LanguageAdapter 提取符号、签名、doc、边界
Tier 3  文本分块            —— 兜底
```

其中 Tier 2 被理解为**由本项目自行实现**的按语言启发式解析器（括号深度、缩进、关键字表）。

## 触发修订的原因

按该结论动手实现后，问题立刻暴露：

1. **不可扩展。** 每新增一种语言就要新写一套词法屏蔽 + 声明识别 + 边界推导。项目技术栈涉及
   JS/TS、Vue、PHP、Java、Rust，逐语言手写是无底洞。
2. **准确度天花板低。** 手写解析必须处理字符串里的括号、注释里的关键字、Rust 生命周期与字符
   字面量的歧义、模板字面量、原始字符串、JSX……每一条都是已知的错误来源，而这些问题在成熟
   解析器里早已解决。
3. **维护成本转嫁给未来。** 语言语法持续演进（TS 装饰器、Rust 新语法），手写实现会持续腐化。

关键在于，原结论的论证只对 **Tree-sitter** 成立，而它被无声地推广到了"所有第三方解析器"。

## 重新验证

原论证的核心事实（`plugin-system-audit.md` 的构建流水线一节）：

> 本机无 wasi-sdk，`clang -print-targets` 的注册目标中没有 wasm 后端，
> 构建流水线 `scripts/build-plugin.ts` 使用 `--offline`。

这条对 Tree-sitter 成立——Tree-sitter 的运行时与全部语法都是 C，必须有能产出 wasm 的 C 工具链。

**但它对纯 Rust 解析器完全不成立。** 纯 Rust crate 走的是 `rustc` 的 `wasm32-wasip2` 后端，
与 C 工具链无关。这一点在原审计中没有被单独区分，是本次修订的起因。

实测验证（2026-09-03）：

| 验证项 | 结果 |
| --- | --- |
| 依赖可获取（镜像源 `rsproxy-sparse`） | 通过 |
| `cargo build --target wasm32-wasip2` | 通过 |
| 四种语言的实际解析与位置信息 | 通过 |
| 插件 wasm 体积 | 592 KB → 3.4 MB |

## 决策

**结构化解析一律使用现成的纯 Rust 解析器，不自建语法分析。**

| 语言 | 解析器 | 说明 |
| --- | --- | --- |
| Rust | `syn` 3 | 需要 `proc-macro2` 的 `span-locations` feature 才有行号 |
| TypeScript / JavaScript（含 TSX / JSX） | `oxc_parser` 0.148 | 容错解析器，语法错误时仍返回可用 AST |
| Python | `rustpython-parser` 0.4 | |
| Go | `gosyn` 0.2 | |
| 其余语言 | 无 | Tier 3 文本分块 |

层级模型相应修订为：

```
Tier 1  现成解析器的完整 AST   —— Rust / TS / JS / Python / Go
Tier 2  （取消）              —— 不再自建启发式解析
Tier 3  文本分块              —— 未覆盖语言、解析失败、无声明的文件
```

插件只保留一层薄适配：把各家 AST 归一化成统一的 `Declaration`，之后的限定名推导、
chunk 装配与符号表构建对所有语言共用同一套代码。**新增语言 = 新增一个适配器**，
不再是新增一套解析器。

## 后果

正面：

- 解析准确度直接达到工业级：字符串、注释、生命周期、模板字面量、JSX 全部由解析器正确处理。
- 新增语言成本从"一套解析器"降到"一个 AST 适配器"。
- `oxc` 的容错特性让**编辑中的半成品文件依然可被检索**，这正是 Coding Agent 场景所需。
- 拿到的是真实 AST，未来接入 LSP 富化时的定位信息已经准确。

负面，且必须如实承认：

- **插件 wasm 从 592 KB 增长到 3.4 MB。** 模块体积不占用 64 MiB 线性内存预算（那是运行时堆），
  但会增加分发体积与模块编译时间。可接受。
- 引入 6 个直接依赖及其依赖树，`--offline` 构建要求它们已在本地 registry 缓存中。
- 各解析器的 API 会随大版本变化（本次实现已遇到 `oxc` 0.148 把 `TSModuleDeclaration` 拆成
  `TSNamespaceDeclaration` / `TSExternalModuleDeclaration` / `TSGlobalDeclaration`）。
  适配层集中在四个文件里，升级影响面可控。

## 未覆盖语言

PHP、Java、C/C++、Ruby 等目前走 Tier 3 文本分块：仍然进入索引，仍然可被词法与
标识符检索命中，只是没有符号级边界。后续按需增加适配器，**不因为缺适配器就把文件排除在索引外**。

2026-09-05 补充：Vue/nvue 的顶层脚本已由 `html5gum` 按 HTML 语法识别边界，再交给
`oxc` 解析。该依赖只属于插件，禁用可选特性且不引入本地模型或宿主能力；模板、样式、
外链与未知脚本区域继续文本兜底，保留源码位置和同文件检索覆盖。

## 与既有 ADR 的关系

- [ADR：索引存储后端](./adr-storage.md) 中"SQLite 无法编译"的论证**依然成立**——那确实是 C 库。
  本 ADR 只修正了把该论证外推到解析器选型的部分。
- [ADR：本地嵌入](./adr-local-embedding.md) 与 [ADR：API 嵌入](./adr-api-embedding.md) 不受影响：
  它们的阻断点是 Host 能力缺失（`host-fs-read` 只返回 UTF-8、无 HTTP 能力），
  与工具链无关。

## 关于 Tree-sitter

仍然不使用，理由不变（C 工具链缺失）。若将来 Host 侧提供 wasi-sdk 或改用预编译的
Tree-sitter wasm 语法，可重新评估——它的优势是语法覆盖面广，一次接入即可支持数十种语言。
在此之前，"每种语言一个纯 Rust 解析器"是可行且准确度更高的方案。
