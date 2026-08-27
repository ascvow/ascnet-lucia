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

应用负责如何生成摘要；Core 只保证加载结果真正成为模型上下文。

## 默认原生实现

TUI 默认装配 `agent-context` 的 `NativeContextLoader`，不经过 Plugin Host，也不需要安装 bundle。默认实现按模型上下文水位执行三种行为：

- 低于水位时透传完整上下文。
- 达到微压缩水位时静默清理较旧的成功工具结果，同时保留失败结果和合法调用配对。
- 达到完整压缩水位时禁用工具与推理，通过应用固定的模型路由生成结构化摘要，并保留近期完整 API 轮次。

原生 `/compact` 将当前 Session 以 `user_initiated` 请求交给同一加载器，无条件尝试完整压缩；内容变化时以 revision 比较并交换方式持久化并立即替换当前 Session。默认插件版与 `--no-default-features` 纯 Core 版使用相同行为。

Evidence 运行从 Artifact CAS 装配 Genome 固定的 Context Policy，`PolicyRef.id` 必须为原生稳定 owner `native-context`。压缩事实直接写入 Agent EventSink，供 TUI 展示与 Evidence 记录使用。

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
