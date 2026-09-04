# ADR：API 嵌入

## 状态

已接受，2026-09-03。

## 背景

产品需要支持用户自配的 OpenAI 兼容 `/v1/embeddings` 端点，配置项为 `base_url`、`api_key`、`model`、可选 `dimensions`。插件不绑定具体厂商。问题是：WASM 插件今天能否发出 HTTP 请求。

## 分类结论

```
API Architecture: C. proposed Host HTTP capability
```

```
API Mode: REQUIRES EXTENSION（V2 交付，前置条件是通用 http 能力）
```

## 为什么不是 A（WASI 网络）

`crates/agent-plugins/host/src/wasm/mod.rs:505`：

```rust
let mut wasi = WasiCtxBuilder::new();
// 刻意保持最小 WASI：不继承环境变量、不预打开目录、不继承 stdio。
wasi.arg(&manifest.plugin.id);
```

没有 `inherit_network()`、没有 `socket_addr_check()`、没有 `allow_ip_name_lookup()`。WASI p2 的 `wasi:sockets` 在这份 ctx 下不可用。

`docs/host/manifest-capabilities.md:99` 把这条写成了明确规则："Host 不继承 WASI 环境变量、预打开目录或 stdio。需要 I/O 的插件必须使用 `PluginHostApi`。"

这是刻意的信任边界设计，不是遗漏。

## 为什么不是 B（现有 Host 能力）

`manifest.rs:1153`：

```rust
if self.http || self.secrets || !self.fs_write.is_empty() {
    return Err(anyhow!("当前 Lucia 版本尚未实现插件 HTTP、secret 或文件写入能力"));
}
```

`http = true` 在加载期直接拒绝，插件根本无法启动。WIT world（`wit/plugin.wit`）中也没有任何 HTTP import。

### 关于 process_exec + curl

技术上今天就能跑通：`process_exec = true`，spawn `curl`，逐行读 JSON 响应。mcp-plugin 就是用这条路径与 MCP 服务端通信的。

**否决。** 四条理由，第一条是决定性的：

**1. 权限 scope 崩塌。** `manifest.rs:1100`：

```rust
if self.process_exec {
    include(ToolPermissionScope::Shell);
}
```

`bind_tool_scope`（`manifest.rs:1072`）把这个 floor 施加到插件的**全部公开工具**上。`code_search` 会从 `read` 变成 `shell`，意味着每次调用都需要用户审批。

产品目标是把 Agent 的 `rg → read → rg → read` 循环压缩成一次调用，以减少工具调用数与 token 消耗。一个每次都要弹审批的检索工具在这个目标下是负收益。**这不是安全洁癖，这是产品可用性。**

**2. 密钥必须进入 Guest。** 子进程的 `command`、`args`、`env` 都由 Guest 构造（`capability/requests.rs:90`）。要让 curl 带上 `Authorization`，API key 必须先出现在插件的线性内存里。

这违反本仓库已确立的范式。`crates/agent-tui/src/plugin/startup.rs:346` 的注释写得很清楚："插件配置在 Host 侧解析：模型路由由宿主消费并注册成专属 provider，其余键值作为激活元数据下发，**密钥不进入 Guest**。" `host-model-complete` 就是这个范式的成品实现。

**3. 原生进程权限。** `docs/host/manifest-capabilities.md:197`："`process_exec` 仍等价于授予插件当前操作系统用户的原生进程权限；上述结构限制不能替代插件来源校验和用户信任决策。"

一个代码检索插件要求操作系统级进程权限，是明显的权限过度。

**4. 传输形态错配。** stdio 逐行文本，单行 4 MiB（`capability/mod.rs:53`），读超时上限 120 秒。批量嵌入的请求与响应都是大 JSON，要在这条管道上做分片与重组，而且 curl 的错误处理、重试、超时全部要靠解析 stderr 与退出码，插件拿不到结构化的 HTTP 状态码。

## 决策

**API Mode 的实现前置条件是 Host 新增通用 HTTP 能力。** 在此之前，产品显示 `API Mode: 需要宿主 HTTP 能力`，不提供子进程变通实现。

> **实施状态（2026-09-03 更新）**：该前置条件已满足。通用 `host-http-request` 能力已实现
> （`crates/agent-plugins/host/src/capability/http.rs`、`services.rs` 的 `HttpHostServices`、
> `wasm/linker.rs` 注册），API Mode 可用。本节以下的契约设计**全部按原样落地**，
> 仅有一处判断被实测推翻，见下方「scope floor 的实际结果」。

契约沿用 `docs/plugin/api-capability-map.md:39` 已列为 P0 的推荐形状（Host 执行的结构化请求；域名 allowlist、响应上限、超时与重定向策略），并补充凭证处理：

```toml
[capabilities.http]
allow_hosts = ["api.openai.com", "api.siliconflow.cn"]
```

```wit
/// 执行一次 manifest 允许的结构化 HTTP 请求。
import host-http-request: func(request-json: string) -> string;
```

关键设计点：

| 项 | 归属 | 理由 |
| --- | --- | --- |
| method、path、body、非敏感 header | Guest | 业务协议属于插件 |
| host allowlist | manifest + Host 校验 | 用户可审计的授权边界 |
| `base_url` 实际值 | 应用配置 | 与 `[plugins.<id>]` 模型路由同一处 |
| `Authorization` 等凭证 header | **Host 注入** | 复制 `host-model-complete` 范式，密钥不进 Guest |
| 超时、重定向、响应体上限、并发 | Host | 与 `ExecutionPolicy` 的既有收紧语义一致 |
| scope floor | 见下方「scope floor 的实际结果」 | 原判断为 `Network`，实测为 `Unknown` |

注意 `manifest.rs` 已经为 `http` 预留了 `ToolPermissionScope::Network` 的映射——能力被拒绝，但 scope 语义已经设计好了。这说明 HTTP 能力在本仓库是"已规划未实现"，不是"被否决"。

### scope floor 的实际结果

**本节原先的判断「scope floor 为 `Network`，比 `Shell` 低一档」在本插件上不成立，实测结果是 `Unknown`。**

原因是 `component_capability_scope_floor()` 会把插件持有的**全部**能力折算成一个下限，再用 `merge_tool_permission_scopes()` 合并：本插件同时持有 `workspace_read`（贡献 `Read`）与 `http`（贡献 `Network`），而该函数对两个不相等且都不是 `Control` 的 scope 返回 `Unknown`。这不是缺陷——它准确表达了「Host 无法刻画这个工具的副作用范围」，但代价比原先预计的更大：

- `code_search` 进入最保守的审批档位，而不是 `Network` 档；
- `code_search` 不再满足 `agent/run.rs` 中 `read_only_batch_end()` 的 `Read` 判定，因此**失去与其他只读工具并行批处理的资格**。

原文提出的缓解方式（把批量嵌入放在索引构建期、不经过工具调用路径）**已经实现**，但它缓解的是调用频次，不改变 scope 折算结果——scope 是按 component 持有的能力静态计算的，与实际调用时机无关。

因此真正能消除该代价的只有原文末尾提到的方案：**把 embedding 拆成独立 provider 插件**，通过 `host-service-call` 供 Code Context Engine 调用，让网络权限留在 provider 插件的 manifest 上，使本插件保持 `Read`。这是本仓库已有的插件依赖与服务机制（`docs/plugin/dependencies-services.md`），不需要新能力。当前实现选择了承担代价而不拆分，该方案保留待需要时实施。

## 隐私

API Mode 会把**源码信息**发送到用户配置的 provider。

插件配置界面与文档必须原样写出这句话。**禁止使用"只有查询会离开你的机器"这类表述**——那是错的：文档嵌入阶段发送的正是仓库内容。

**实际发送范围（2026-09-04 更新）**：当前实现会按文件回读源码，为每个语义文档发送
**文件路径 + 文件级文档 + 父符号 + 限定符号名 + 声明签名 + 块级文档注释 + import 上下文
+ 代码正文**（`semantic_document_text()`）。正文最多取 1200 个字符；import 最多取 16 条、
总计 600 个字符；文件级文档最多取 300 个字符。

结构化解析还会把同一父节点下相邻的过短兄弟声明合成语义专用复合块。原子块继续服务符号与
BM25，复合块只参与向量检索和模型重排；被覆盖的原子块不再重复生成向量。这个策略减少请求数，
但也意味着一次语义文档可能包含多个相邻声明的完整正文。

因此发送范围足以还原代码实现与设计意图，不能表述为“只发送签名、注释”或“只有查询离开本机”。

用户首次启用 API 语义索引时应有一次明确确认，说明将要发送的内容范围（受 `path_scope` 与 ignore 规则约束）与目标地址。

## EmbeddingSpace 兼容性

```
EmbeddingSpace {
    provider_type, provider_id, model, model_revision,
    dimension, normalization,
    query_template_version, document_template_version
}
→ embedding_space_id
```

用户切换 provider、model 或 `dimensions` 时，`embedding_space_id` 变化，**只重建语义索引，结构索引原样保留**。这是把 embedding 与结构化 chunk 分离存储的直接收益。

## 降级

鉴权失败、模型或参数错误等永久错误会让 `code_search` 进入 `Degraded`（符号 + 词法），标注原因并继续返回结果。

API 嵌入请求统一应用最小请求间隔；单批遇到 429 或 5xx 时按 `Retry-After` 或本地指数退避
自动重试，单次等待最多 30 秒。每批成功向量立即写入当前语义存储，后续失败不会回滚成功前缀。

5xx 与网络抖动仍按普通瞬时故障处理：连续多轮完全没有进展后落到终态降级，避免界面永久
停在建立中。429 使用独立错误类别，重试耗尽后保持 `Indexing`，不累计普通停滞轮次；剩余语义
文档由后续 `code_search` 调用继续建立。原生 sqlite-vec 入口还会跨进程复用已完成向量，因此
5760/6600 这类部分索引不会因一次 TPM 限流永久丢失。

## 后果

- API Mode 推迟到 V2，前置一项 Host 能力。
- V1 交付的纯词法 + 符号检索不依赖任何网络，是最低硬件与最低权限路径。
- 一旦 HTTP 能力落地，接入成本是 `EmbeddingProvider` 的一个实现。
