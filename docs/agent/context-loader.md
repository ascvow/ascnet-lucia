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
- 达到当前模型最大上下文窗口的 80% 时，先静默清理较旧的成功工具结果，同时保留失败结果和合法调用配对；仍然保留的结果按长度封顶。
- 工具结果收缩后仍达到同一水位时，禁用工具与推理，通过当前模型路由生成结构化摘要，并保留近期完整 API 轮次。

摘要请求使用英文 XML 指令，正文仍按用户语言生成。只请求最终 `<summary>`，不再要求输出
随后会丢弃的分析草稿；旧响应的前导 `<analysis>` 仍兼容清理，摘要正文中的同名字面标签不删除。
历史 system/developer 消息降为转义后的来源记录，不带入摘要请求的活动指令层级。回注时将正文转义为
`<conversation_summary>` 内的历史记录，明确它可能不完整或过时，不是新指令或执行成功的证明；
当前指令、近期消息与核验后的状态优先。近期轮次中的超长工具结果可能已封顶，因此不承诺全部原文保留。

### 水位推导

水位全部由**真实上下文窗口**推导，调用方通过 `with_context_window` 传入配置或
models.dev 元数据中的窗口；未声明时按 200k 假定值兜底。TUI 切换模型时同步更新加载器的
模型路由与窗口，并清除旧压缩缓存。

| 水位 | 取值 |
| --- | --- |
| 自动压缩 | 当前模型最大上下文窗口的 80%，先微压缩，必要时再生成完整摘要 |
| 保留的近期上下文 | 窗口的 30%（同时为摘要输出预留最多一半的压缩预算） |

低于 80% 时不提前执行微压缩或完整摘要。例如，200k 窗口在 160k 时开始自动压缩，
1m 窗口在 800k 时开始自动压缩，32k 窗口对应 25.6k。摘要输出上限默认仍为 20k，
不参与触发水位计算；手动 `/compact` 不受自动水位限制。

### token 估算

按字节估算，ASCII 与非 ASCII 分开折算：ASCII 约四字节一个 token，非 ASCII 约三字节
（一个汉字）一个 token。统一按三字节折算会把英文高估约三成，直接导致英文为主的会话
提前触发压缩、白白损失可用上下文。

### 保留区的结果封顶

微压缩清空的是**较旧**结果的正文，完整压缩替换的是**较旧**的历史；两者都不处理保留
下来的近期结果，而单条动辄上百 KB 的工具输出常常就出现在最近几条里。因此两条路径都会
对保留区内超过上限的工具结果做首尾折叠：保留开头（这是什么）与结尾（最终怎么了），
中段替换为省略标记。

原生 `/compact` 将当前 Session 以 `user_initiated` 请求交给同一加载器，无条件尝试完整压缩；内容变化时以 revision 比较并交换方式持久化并立即替换当前 Session。默认插件版与 `--no-default-features` 纯 Core 版使用相同行为。

压缩事实直接写入 Agent EventSink，供 TUI 展示使用。

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
