# 真实模型分层测试

`lucia-live-tests` 使用实际模型端点验证 Lucia 的完整 Agent 路径。它从单轮文本响应逐步增加确定性工具链和真实 WASM 插件，便于先区分模型接入问题，再定位 ReAct、工具事件或插件路由问题。

## 测试层级

| 场景 | 验证内容 | 通过条件 |
| --- | --- | --- |
| `minimal` | 最小 Agent 请求和最终文本 | 模型精确返回固定标记 |
| `react` | 单次原生工具调用 | 工具事件、确定性 nonce 和最终标记同时匹配 |
| `complex` | 有顺序依赖的多工具需求 | 三个工具按顺序成功，后一步使用前一步结果 |
| `plugin` | Plugin Host、WIT 和真实 WASM 工具 | `echo` 插件工具事件与最终标记同时匹配 |

文本标记不能代替工具证据。`react`、`complex` 和 `plugin` 都会检查 Agent 事件中的成功工具调用及确定性结果，模型直接猜出最终文本仍会失败。

## 模型配置

仓库提供不含凭据的 `examples/config/live-tests.example.toml` 模板。修改后的真实配置建议放在仓库外，例如 `/private/tmp/lucia-live.toml`：

```toml
[model]
name = "live"
provider = "open-ai-compatible"
model = "replace-with-model-id"
base_url = "https://replace-with-endpoint/v1"
api_key_env = "LUCIA_LIVE_API_KEY"
openai_protocol = "chat-completions"

[agent]
max_steps = 12
max_tokens = 4096
stream = true
```

也可以使用 `open-ai` 的 Responses 协议或 `anthropic` provider；字段与普通 Lucia 配置一致。真实测试需要模型支持结构化工具调用，`minimal` 通过不代表后续场景一定兼容。

通过环境变量或 CI secret 注入凭据：

```bash
export LUCIA_LIVE_API_KEY="replace-with-local-secret"
```

不要把实际 API key、带 token 的 URL、私有 header 或真实测试配置提交到仓库。报告不会保存模型原文、工具参数、工具结果、API key 或服务商原始响应，但报告文件仍应按内部测试产物管理。

## 先运行离线测试

```bash
bun run test:live:unit
```

该命令不访问网络，验证场景判定、工具顺序和报告脱敏。修改测试运行器后应先通过这一层，再消耗真实模型额度。

## 逐层运行

从最小场景开始：

```bash
bun run test:live -- \
  --config /private/tmp/lucia-live.toml \
  --scenario minimal
```

随后验证一次 ReAct 工具调用和复杂工具链：

```bash
bun run test:live -- \
  --config /private/tmp/lucia-live.toml \
  --scenario react

bun run test:live -- \
  --config /private/tmp/lucia-live.toml \
  --scenario complex
```

插件场景需要先构建示例 component：

```bash
cargo build --offline \
  --manifest-path examples/plugins/echo-plugin/Cargo.toml \
  --release \
  --target wasm32-wasip2

bun run test:live -- \
  --config /private/tmp/lucia-live.toml \
  --scenario plugin \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml
```

省略 `--scenario` 或传入 `--scenario all` 会按 `minimal`、`react`、`complex`、`plugin` 顺序运行。插件 manifest 也可以通过模型配置中的第一个 `[[plugins]]` 条目提供，显式参数优先。

## JSON 报告

报告始终写入标准输出，也可以同时保存到文件：

```bash
bun run test:live -- \
  --config /private/tmp/lucia-live.toml \
  --scenario all \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml \
  --report /private/tmp/lucia-live-report.json
```

总报告包含 schema 版本、整体通过状态和总耗时；每个场景包含耗时、ReAct 步数、工具名称与成功状态、工具耗时、token 用量和脱敏错误。任一场景失败都会使进程返回非零退出码，但不会阻止后续场景继续执行。

真实测试用于验证端点兼容性和完整功能路径，不替代可重复的 Core、Host 与插件单元测试。CI 中应把真实测试放在受控凭据和费用预算下运行。
