# 上下文压缩插件

这是 Lucia 官方上下文管理插件。它声明独占能力 `agent.context-loader`，参考 Claude Code 的上下文管理方式，在每次模型请求前执行分层压缩。实现不依赖网络或额外模型调用。

## 压缩策略

- 默认按 200k 上下文计算：在约 120k token 时静默清理旧的成功工具结果正文，在约 167k token 时执行完整压缩。
- `[1m]` 模型使用约 900k 和 967k 两级水位。
- 完整压缩按 assistant 响应划分 API 轮次，避免拆散工具调用和结果。
- 较旧轮次会按用户意图、技术上下文、文件与工具状态、错误、处理过程、用户消息、待办、当前工作和下一步依据重建为结构化摘要，约 40k token 的近期轮次保持原文。
- system 提示、工具调用名称与参数会进入重建上下文；微压缩会原样保留最近三条成功工具结果和全部失败工具结果，完整压缩会单独提取近期错误状态。

## 主动压缩

官方 Command 插件内置：

```text
/compact
```

Command 插件生成受控的 `CompactSession` 动作，原生 TUI 随即把当前完整 Session 交给 `context.compact`。Context 插件忽略自动水位并同步返回压缩后的替换上下文；TUI 成功持久化后立即刷新当前 Session，不需要再发送一条消息。当前会话没有可安全切分的历史轮次时，主事件列表会提示“没有可压缩的历史上下文”。

Lucia 当前的插件 ABI 不提供独立模型完成接口，因此这里采用确定性结构化提取，不会把本地裁剪伪装成模型摘要。与 Claude Code 相比，它实现了预算预留、微压缩、轮次安全切分、摘要重建和近期状态保留，但不包含额外摘要模型调用、文件内容重新注入或持久化 compact boundary。

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
