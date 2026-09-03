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
| scope floor | `Network`（`manifest.rs:1109` 已定义） | 比 `Shell` 低一档，且语义准确 |

注意 `manifest.rs:1109` 已经为 `http` 预留了 `ToolPermissionScope::Network` 的映射——能力被拒绝，但 scope 语义已经设计好了。这说明 HTTP 能力在本仓库是"已规划未实现"，不是"被否决"。

`Network` scope 仍然高于 `Read`。因此 API Mode 启用后 `code_search` 的审批要求会上升。缓解方式：把 embedding 调用与 `code_search` 工具解耦——索引构建时批量嵌入（后台任务，不经过工具调用路径），查询时的单条查询嵌入才是 `code_search` 内的网络访问。若这一点在实践中仍造成摩擦，可考虑把 embedding 拆成独立的 provider 插件，通过 `host-service-call` 供 Code Context Engine 调用，让网络权限留在 provider 插件的 manifest 上——这是本仓库已有的插件依赖与服务机制（`docs/plugin/dependencies-services.md`），不需要新能力。

## 隐私

API Mode 会把**源码片段**（chunk 正文）发送到用户配置的 provider。

插件配置界面与文档必须原样写出这句话。**禁止使用"只有查询会离开你的机器"这类表述**——那是错的：文档嵌入阶段发送的正是源码本身。

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

API 不可达、鉴权失败、配额耗尽时，`code_search` 进入 `Degraded`（符号 + 词法），标注模式并继续返回结果。失败不重试到超时——单次请求失败即本轮降级，避免让一次检索卡在网络等待上。

## 后果

- API Mode 推迟到 V2，前置一项 Host 能力。
- V1 交付的纯词法 + 符号检索不依赖任何网络，是最低硬件与最低权限路径。
- 一旦 HTTP 能力落地，接入成本是 `EmbeddingProvider` 的一个实现。
