# 上下文压缩插件

这是 Lucia 官方上下文管理插件。它声明独占能力 `agent.context-loader`，参考 Claude Code 的上下文管理方式，在每次模型请求前执行分层压缩。微压缩只做本地清理；完整压缩会通过 Host 受控能力额外调用一次模型生成摘要。

## 压缩策略

- 未注入 Genome Context Policy 时完整保持历史默认行为：按 200k 上下文计算，在约 120k token 时静默清理旧的成功工具结果正文，在约 167k token 时执行完整压缩。
- `[1m]` 模型使用约 900k 和 967k 两级水位。
- 完整压缩按 assistant 响应划分 API 轮次，避免拆散工具调用和结果。
- 较旧轮次会交给模型，按用户意图、技术上下文、文件与工具状态、错误、处理过程、用户消息、待办、当前工作和下一步依据生成结构化摘要，约 40k token 的近期轮次保持原文。
- system 提示会继续原样保留；工具调用名称、参数和结果会进入摘要输入。微压缩会原样保留最近三条成功工具结果和全部失败工具结果，完整压缩提示会明确要求模型保留错误状态。

Evidence Genome 声明 `context_policy` 时，TUI 会从 Artifact CAS 重新校验规范 JSON，并只向 `agent.context-loader` 的真实 owner 注入 `context_policy_json` 和 `context_policy_digest`。Guest 会重新计算正文 SHA-256，随后真实执行以下版本化参数：

```json
{
  "schema_version": 1,
  "micro_compact_threshold_tokens": 120000,
  "full_compact_threshold_tokens": 167000,
  "recent_message_count": 8,
  "tool_result_retention": {
    "preserve_errors_and_recent": {
      "recent_successful_results": 3
    }
  },
  "user_constraints": {
    "pinned_structured_and_summary": {
      "max_items": 64
    }
  },
  "plan_snapshot": {
    "latest_snapshot_and_summary": {
      "max_items": 100
    }
  },
  "summary_token_budget": 20000,
  "post_summary_validation": {
    "structured_marker_coverage_v1": {
      "min_coverage_bps": 10000
    }
  }
}
```

用户约束和事实只通过完整文本块中的显式 JSON 契约识别，普通自然语言不会被猜测为结构化状态：

```json
{"schema":"lucia.context/user-constraint/v1","id":"constraint-read-only","value":{"mode":"read_only"}}
{"schema":"lucia.context/fact/v1","id":"fact-config-path","value":{"path":"src/config.rs"}}
```

约束消息会逐值进入摘要外固定区；事实 ID 必须由摘要标记信封确认。Plan 只识别 plan-plugin 成功的 `update_plan`/`get_plan` ToolResult，逐值保留最新 `schema_version`、`revision` 和完整 `plan`。被保留的 ToolResult 会同时保留对应 ToolCall。摘要缺少必需 constraint、fact、ToolResult 调用 ID 或 Plan 修订标记，或最终固定区逐值校验失败时，本轮上下文加载失败关闭。

## 主动压缩

官方 Command 插件内置：

```text
/compact
```

Command 插件生成受控的 `ReloadSessionContext` 动作，原生 TUI 随即在后台把当前完整 Session 以 `user_initiated` 标记交给已注册的上下文加载器。Context 插件对用户显式发起的加载忽略自动水位，无条件执行完整压缩并返回替换上下文；TUI 成功持久化后立即刷新当前 Session，等待期间界面保持可交互并显示命令执行指示。压缩结果的用户可见说明完全由本插件发布的展示事件提供：完成时为“上下文压缩”分隔线，没有可安全切分的历史轮次时提示“没有可压缩的历史上下文”。

完整压缩通过 `PluginHostApi::complete_model` 发送旧历史，摘要模型使用与当前 Agent 相同的 Host 可信 provider/model 和流式开关（Host 侧聚合流式增量后整体返回）。未配置策略时输出上限为 20k token；配置策略时使用 `summary_token_budget`，并继续受 Host 固定上限收紧。Guest 不能指定路由、流式模式、工具、推理级别或 provider options；Host 强制禁用工具与推理。TUI 会自动注入该服务，其他应用加载此插件时也必须通过 `PluginHostServices::with_model_completion` 注入模型网关。

与 Claude Code 相比，当前实现包含预算预留、微压缩、轮次安全切分、模型摘要、结构化固定区和确定性摘要验证，但尚未实现文件内容重新注入或持久化 compact boundary。

## 构建

```bash
bun run build:plugin:context
```

## 测试

```bash
bun run test:plugin:context
```

## 加载

```bash
bun run build:tui:plugins
target/plugin-tui/release/lucia \
  --demo \
  --plugin-manifest examples/plugins/context-plugin/plugin.toml
```

微压缩只发布无展示字段的结构化事件，不在主事件列表显示文本；完整压缩会显示“上下文压缩”分隔信息。
