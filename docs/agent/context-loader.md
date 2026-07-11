# 上下文加载

`ContextLoader` 是每次模型请求的唯一上下文入口。它适用于裁剪、摘要、检索增强、外部会话存储以及按模型切换上下文策略。

## 完整替换语义

加载器收到 `ContextLoadRequest`：

- `run_id` 与当前 `step`
- provider 与 model
- 顶层 system 提示
- 扩展提示和完整 Session 消息

它返回 `LoadedContext`。返回的 system 与 messages 会完整替换原始值。加载失败会终止 run，不会回退到完整历史。

<div class="arch-flow">Session 完整历史 + Extension prompts
  -> ContextLoader::load
  -> LoadedContext
  -> provider-neutral 清洗
  -> ModelRequest</div>

## 摘要加载器示例

```rust
use anyhow::Result;
use agent_core::{
    ContextLoadRequest, ContextLoader, LoadedContext, MessageRole, ModelMessage,
};
use async_trait::async_trait;

struct SummaryLoader;

#[async_trait]
impl ContextLoader for SummaryLoader {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        let summary = load_summary_for_run(&request.run_id).await?;
        let latest_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .cloned();

        let mut messages = vec![ModelMessage::text(
            MessageRole::Developer,
            format!("历史摘要：{summary}"),
        )];
        messages.extend(latest_user);
        Ok(LoadedContext::new(request.system, messages))
    }
}
```

插件或应用负责如何生成摘要；Core 只保证加载结果真正成为模型上下文。

## WASM 插件桥接

插件在 manifest 中声明独占能力，并实现 SDK 的 `load_context`：

```toml
[[provides]]
id = "agent.context-loader"
version = "1.0.0"
mode = "exclusive"
```

Plugin Host 会把选中的 owner 适配成同一个 `ContextLoader` 接口。插件收到 provider-neutral 消息 JSON，返回的 `LoadedContext` 完整替换模型输入；返回错误会终止当前 run，不会回退到完整历史。多个独占 provider 的选择规则见 [Manifest 与权限](/host/manifest-capabilities)。

官方实现位于 `examples/plugins/context-plugin`。它按 token 水位执行工具结果微压缩或结构化历史压缩，并保留近期完整 API 轮次。

## 挂载与恢复

```rust
agent.set_context_loader(Arc::new(SummaryLoader));
agent.reset_context_loader();
```

旧的同步闭包仍可通过 `with_context_transform` 或 `set_context_transform` 使用。它会被 `TransformContextLoader` 适配，但不能执行异步存储或模型调用。

## 不要做的事

- 加载失败时自行返回未压缩的完整历史，除非这是产品明确策略。
- 修改 Session 来模拟压缩；Session 应继续保留完整事实历史。
- 在 provider adapter 内裁剪消息；那会让不同 provider 的行为不一致。
