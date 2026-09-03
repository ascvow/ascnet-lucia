# ADR：本地嵌入

## 状态

已接受，2026-09-03。

## 背景

产品希望提供 Local 语义模式，初始目标模型 Qwen3-Embedding-0.6B，承诺源码不离开本机。立项前不假定该模型能直接运行在 WASM 插件内，需要先做能力审计。

## 分类结论

```
Local Embedding Architecture: C. WASM + proposed generic Host Capability
```

```
Local Mode: NOT CURRENTLY SUPPORTED
```

V1 与 V2 都不交付 Local Mode。产品界面必须明确显示该模式不可用，而不是提供一个"能跑但破坏架构"的实现。

## 为什么不是 A（Pure WASM）

五条独立阻断，任一条都足以否决。这里不做 PoC 基准测试——PoC 的目的是在可行性存疑时给出数据，而下面每一条都是 ABI 与配置层面的确定性事实，不存在需要测量的不确定性。

### 1. 权重文件在 ABI 层面读不出来

`crates/agent-plugins/host/src/capability/mod.rs:613`：

```rust
let content = std::fs::read_to_string(&path)?;
```

`host-fs-read` 只返回 UTF-8 字符串。GGUF、safetensors、ONNX 全是二进制，`read_to_string` 会在 UTF-8 校验上直接失败。**插件没有任何途径把模型权重读进线性内存。**

这一条单独就终结了讨论：连权重都读不到，后面的内存、线程、fuel 都无从谈起。

### 2. 内存差两个数量级

`wasm/engine.rs:18`：

```rust
const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
```

且 `StoreLimitsBuilder::trap_on_grow_failure(true)`（`wasm/mod.rs:510`），超限直接 trap。0.6B 参数模型即使 int8 量化也在 600 MB 量级，fp16 更高。全部 loader 使用 `WasmPluginLimits::default()`（`wasm/loader.rs:95`、`:580`、`:655`），应用层没有提升路径。

即便把上限提到 1 GB，`engine.rs:52` 未开启 memory64，wasm32 线性内存硬上限 4 GiB，且单块分配还要受宿主实际内存约束——把一个推理运行时塞进插件沙箱本身就是错误的资源模型。

### 3. 无线程

`engine.rs:52` 未调用 `wasm_threads`（wasmtime 46 默认关闭，见 `wasmtime-46.0.1/src/config.rs:935`），linker 只注入 `wasmtime_wasi::p2`，无 wasi-threads（`wasm/mod.rs:496`）。此外同插件的全部导出调用被 `Arc<Mutex<WasmPluginState>>` 串行化（`wasm/mod.rs:277`）。

单线程 CPU 推理。

### 4. fuel 预算不匹配

`engine.rs:15` 每插件 5000 万 fuel，且在**每次导出调用前重置**（`fuel.rs:12`）。一次 transformer 前向的指令数远超此量级，fuel 耗尽即 trap（`engine.rs:159` 的测试已证实这条路径）。

分片规避不成立：推理无法在导出调用之间保存中间状态并恢复，因为线性内存中的 KV 与激活值会被下一次调用的其他逻辑扰动，而且插件无法自己决定何时被再次调用。

### 5. 无 GPU

WASM 沙箱没有 Metal、CUDA、DirectML 或 WebGPU 的任何接口。

## 为什么不是 B（现有 Host 能力）

`host-model-complete`（`wit/plugin.wit:87`）是**文本补全**，不是 embedding。它的 Host 实现（`services.rs:269`）绑定 `ModelGateway` + `AgentModelConfig`，强制工具列表为空、关闭推理、限制输出 token，返回的是文本。没有任何返回向量的路径。

`process_exec` 在技术上可行——启动 llama.cpp / Python 服务并通过 stdio 通信，mcp-plugin 就是这个模式。但被明确否决，理由与 API Mode 相同且更强：

- **权限代价。** `manifest.rs:1100` 把 `process_exec` 映射为 `Shell` scope floor，`code_search` 从此每次调用都需要用户审批。产品目标是"减少 Agent 的交互与 token 成本"，一个每次都要审批的检索工具与目标直接冲突。
- **信任代价。** `docs/host/manifest-capabilities.md:197` 明确 `process_exec` 等价于授予插件当前操作系统用户的完整原生进程权限，"上述结构限制不能替代插件来源校验和用户信任决策"。
- **协议代价。** stdio 只支持逐行文本，单行 4 MiB（`capability/mod.rs:53`）。向量要编成文本传输，批量嵌入的吞吐会被这条管道压死。
- **架构代价。** 需求文档 §7 明确禁止"直接 spawn python / 启动 Node server / 偷偷增加 native daemon"。用 `process_exec` 跑一个嵌入服务，无论它在 manifest 里多合规，形态上就是 native daemon。
- **承诺代价。** Local Mode 的产品承诺是"源码留在本机"。用一个插件自己拉起的、生命周期不受 Host 管理的子进程去兑现这个承诺，比不提供该模式更糟。

## 为什么不是 D

D（当前不可行，无明确路径）不成立。正确的目标形态清晰、通用、且与本仓库既有范式同构：

```wit
/// 使用宿主固定的嵌入路由把文本批次转换为向量。
import host-embedding-embed: func(request-json: string) -> string;
```

Guest 提交：文本批次、可选的 `input_type`（query/document）。
Host 固定：provider、model、后端（Metal/CUDA/CPU）、权重路径、批大小、维度、归一化策略、并发与配额。

这与 `host-model-complete` 完全同构（`services.rs:269` 的注入模式：应用注册网关 → manifest 声明 `embedding = true` → Host 执行策略预算）。

**Host 不知道这些向量用于代码检索。** 记忆插件、知识插件、任何需要语义相似度的插件共用同一能力。这满足 `api-capability-map.md:67` 的 API 准入检查第 1 条（至少两类互不相关的插件需要同一机制）。

## 决策

1. **V1、V2 不交付 Local Mode。** 产品显示 `Local Mode: NOT SUPPORTED`，附一句说明"当前插件运行时不提供本地推理能力"。不提供任何变通实现。
2. **`EmbeddingProvider` trait 从第一版就按 Local/Api 双分支设计**，Local 分支返回"能力不可用"错误。这样 Host 能力落地后接入是加一个实现，不是重构。
3. **Local Mode 的前置条件是 Host 新增通用 `embedding` 能力**，形状如上。该提案不属于本插件的交付范围，应作为独立的 Host 能力提案走 `api-capability-map.md:67` 的准入检查。
4. 语义能力缺席时，检索进入 `Degraded`（符号 + 词法），而不是不可用。

## 后果

- 用户在 V1 只能用词法 + 符号检索，或（V2 起）配置 API provider。
- 承诺不打折：不会出现"名义上本地、实际上拉起一个不受管的子进程"的形态。
- 一旦 Host 具备通用嵌入能力，本插件的接入成本是一个 trait 实现。

## 关于 PoC

需求文档 §34 要求在决定支持 Local Mode 前做独立 PoC，测量加载时间、峰值内存、延迟等。

**本 ADR 判定该 PoC 当前不必要，理由是它无法执行。** PoC 的第一步是把权重读进插件，而 `host-fs-read` 只返回 UTF-8——这一步就走不通。测量一个在 ABI 层面无法开始的流程没有意义。

PoC 应当在 `embedding` Host 能力提案阶段进行，且测量对象是 **Host 侧的原生推理运行时**（加载时间、峰值内存、延迟、批吞吐、后端选择、取消响应），而不是 WASM 内的推理。那时的问题是"宿主该怎么跑这个模型"，与插件沙箱无关。
