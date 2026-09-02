//! Lucia 默认原生上下文压缩、模型摘要和 Context Policy 执行能力。

#![deny(missing_docs)]

use agent_core::{
    event::{AgentEvent, AgentEventKind, EventSink},
    model::{ModelGateway, ModelRequest},
    ContextLoadRequest as CoreContextLoadRequest, ContextLoader,
    LoadedContext as CoreLoadedContext, ReasoningLevel, ToolChoice,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// 未知上下文窗口时假定的窗口大小。
///
/// 只在既没有配置也没有 models.dev 元数据时兜底；正常路径应由调用方传入真实窗口。
const ASSUMED_CONTEXT_WINDOW_TOKENS: usize = 200_000;
/// 触发完整压缩前为下一轮请求预留的缓冲。
///
/// 压缩本身要发一次模型请求，压缩后还要立刻发起真正的那一轮；不留缓冲会在压缩
/// 完成的同一步就再次越界。
const NEXT_TURN_BUFFER_TOKENS: usize = 13_000;
/// 完整压缩水位相对窗口的下限比例。
///
/// 小窗口模型减去固定预留后可能归零，此时改用窗口比例，避免每一轮都触发压缩。
const MIN_COMPACT_THRESHOLD_RATIO: usize = 55;
/// 微压缩水位相对完整压缩水位的比例。
///
/// 先用无损代价更低的手段清理旧工具结果，尽量把完整摘要推迟到真正需要时。
const MICRO_COMPACT_THRESHOLD_RATIO: usize = 72;
/// 完整压缩后保留的近期上下文占窗口的比例。
///
/// 固定 40k 在小窗口上会吃掉整个预算、在百万窗口上又过于吝啬，因此按窗口比例伸缩。
const RECENT_CONTEXT_TARGET_RATIO: usize = 20;
/// 保留区内单个工具结果允许的最大字符数。
///
/// 超过该长度的结果保留首尾片段：巨型日志和堆栈的中段几乎不携带增量信息，
/// 但首尾同时决定了"这是什么"和"最终怎么了"。
const MAX_RETAINED_TOOL_RESULT_CHARS: usize = 2_000;
/// 微压缩始终原样保留的最近工具结果数量。
const RECENT_TOOL_RESULTS_TO_KEEP: usize = 3;
/// Claude Code 为完整摘要预留的最大输出 token 数。
/// 原生上下文摘要请求允许的默认最大输出 token 数。
pub const DEFAULT_MAX_SUMMARY_TOKENS: u32 = 20_000;
/// 被微压缩的工具结果占位文本。
const CLEARED_TOOL_RESULT: &str = "[旧工具结果内容已清理]";
/// 独立摘要调用使用的固定 system 提示，不携带主 Agent 的工具和行为人格。
const SUMMARY_SYSTEM_PROMPT: &str =
    "你是一名负责压缩长对话的助手。请准确、完整地总结已有上下文，禁止调用工具。";
/// 参考 Claude Code 的完整压缩提示，要求模型保留继续开发所需的全部关键状态。
const SUMMARY_REQUEST_PROMPT: &str = r#"请为此前对话生成一份详细的继续工作摘要，重点保留用户明确要求、已经执行的操作、技术决策和当前状态。

摘要必须包含以下部分：
1. 用户主要请求与意图
2. 关键技术概念与约束
3. 涉及的文件、代码位置和重要代码变化
4. 遇到的错误、原因与修复
5. 已完成的问题处理过程
6. 所有非工具结果的用户消息
7. 尚未完成的任务
8. 压缩前正在进行的工作
9. 与最近工作直接相关的下一步

不要调用工具，不要虚构信息，不要把摘要写成泛泛建议。直接输出 <summary>...</summary>，确保仅凭摘要和后续保留的近期消息即可继续工作。"#;

/// 压缩算法内部使用的 JSON 消息请求；只在 Core 类型边界做一次双向转换。
#[derive(Clone)]
struct ContextLoadRequest {
    run_id: String,
    step: usize,
    provider: String,
    model: String,
    system: Option<String>,
    messages: Vec<Value>,
    user_initiated: bool,
}

/// 压缩算法内部使用的 JSON 消息结果。
struct LoadedContext {
    system: Option<String>,
    messages: Vec<Value>,
}

fn encode_core_request(request: CoreContextLoadRequest) -> Result<ContextLoadRequest> {
    Ok(ContextLoadRequest {
        run_id: request.run_id,
        step: request.step,
        provider: request.provider,
        model: request.model,
        system: request.system,
        messages: request
            .messages
            .into_iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<Vec<_>>>()
            .context("编码 Core 上下文消息失败")?,
        user_initiated: request.user_initiated,
    })
}

fn decode_core_context(context: LoadedContext) -> Result<CoreLoadedContext> {
    Ok(CoreLoadedContext {
        system: context.system,
        messages: context
            .messages
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<Vec<_>>>()
            .context("解码原生压缩上下文消息失败")?,
    })
}

/// Lucia 默认原生上下文加载器。
///
/// 加载器在每次模型请求前按水位执行微压缩或模型摘要，并维护同一 Run 内的追加缓存。
/// 它只使用应用固定的模型路由，摘要请求禁用工具与推理。
pub struct NativeContextLoader {
    gateway: ModelGateway,
    provider: String,
    model: String,
    stream: bool,
    max_summary_tokens: u32,
    /// 目标模型的真实上下文窗口；`None` 时按假定窗口推导水位。
    context_window: Option<usize>,
    events: Arc<dyn EventSink>,
    state: Mutex<CompressionState>,
}

#[derive(Default)]
struct CompressionState {
    /// 最近一次自动压缩结果，用于增量追加后续消息，避免重复处理同一历史前缀。
    cache: Option<CompressionCache>,
}

/// 最近一次自动压缩的追加边界与有效模型上下文。
#[derive(Clone)]
struct CompressionCache {
    /// 同一 Agent 运行内的会话消息只会追加，跨运行必须使缓存失效。
    run_id: String,
    provider: String,
    model: String,
    system: Option<String>,
    /// 生成压缩结果时原始上下文的消息数，用于识别后续追加内容。
    source_message_count: usize,
    loaded_messages: Vec<Value>,
}

impl NativeContextLoader {
    /// 创建默认原生上下文加载器。
    ///
    /// `max_summary_tokens` 是运行策略允许的摘要输出上限。
    /// `events` 接收微压缩、完整压缩和手动跳过事件，供 TUI 使用同一事实源。
    ///
    /// # Errors
    ///
    /// 模型路由为空或摘要输出上限为零时返回错误。
    pub fn new(
        gateway: ModelGateway,
        provider: impl Into<String>,
        model: impl Into<String>,
        stream: bool,
        max_summary_tokens: u32,
        events: Arc<dyn EventSink>,
    ) -> Result<Self> {
        let provider = provider.into();
        let model = model.into();
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(anyhow!("原生上下文压缩的模型路由不能为空"));
        }
        if max_summary_tokens == 0 {
            return Err(anyhow!("原生上下文压缩的摘要 token 上限必须大于零"));
        }
        Ok(Self {
            gateway,
            provider,
            model,
            stream,
            max_summary_tokens,
            context_window: None,
            events,
            state: Mutex::new(CompressionState::default()),
        })
    }

    /// 声明目标模型的真实上下文窗口。
    ///
    /// 未声明时按 200k 假定窗口推导水位；真实窗口更大时会过早压缩，更小时会越界。
    /// 调用方应把配置或 models.dev 元数据里的窗口传进来。
    pub fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        self.context_window = context_window
            .filter(|window| *window > 0)
            .map(|window| usize::try_from(window).unwrap_or(usize::MAX));
        self
    }

    /// 在不持有锁的情况下构造本轮有效请求及缓存更新元数据。
    fn prepare_request(
        &self,
        request: ContextLoadRequest,
    ) -> Result<(ContextLoadRequest, CachePlan)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("原生上下文压缩缓存锁已损坏"))?;
        if request.user_initiated {
            state.cache = None;
            return Ok((request, CachePlan::Manual));
        }
        let cache = state.cache.clone();
        let cache_hit = cache.as_ref().is_some_and(|cache| {
            cache.run_id == request.run_id
                && cache.provider == request.provider
                && cache.model == request.model
                && cache.system == request.system
                && request.messages.len() >= cache.source_message_count
        });
        let plan = CachePlan::Automatic {
            run_id: request.run_id.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
            system: request.system.clone(),
            source_message_count: request.messages.len(),
            cache_hit,
        };
        if let Some(cache) = cache.filter(|_| cache_hit) {
            let mut messages = cache.loaded_messages;
            messages.extend_from_slice(&request.messages[cache.source_message_count..]);
            Ok((
                ContextLoadRequest {
                    messages,
                    ..request
                },
                plan,
            ))
        } else {
            Ok((request, plan))
        }
    }

    /// 提交本轮压缩结果；模型调用期间不持有缓存锁。
    fn commit_cache(&self, plan: CachePlan, outcome: &CompressionOutcome) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("原生上下文压缩缓存锁已损坏"))?;
        match plan {
            CachePlan::Manual => state.cache = None,
            CachePlan::Automatic {
                run_id,
                provider,
                model,
                system,
                source_message_count,
                cache_hit,
            } if cache_hit || outcome.event.is_some() => {
                state.cache = Some(CompressionCache {
                    run_id,
                    provider,
                    model,
                    system,
                    source_message_count,
                    loaded_messages: outcome.context.messages.clone(),
                });
            }
            CachePlan::Automatic { .. } => state.cache = None,
        }
        Ok(())
    }

    /// 把压缩事实写入 Agent 事件流；无事件的透传请求不产生记录。
    async fn emit_event(&self, request: &ContextLoadRequest, event: Option<&Value>) -> Result<()> {
        let Some(event) = event else {
            return Ok(());
        };
        self.events
            .record(&AgentEvent::new(
                request.run_id.clone(),
                AgentEventKind::Extension,
                request.step,
                event.clone(),
            ))
            .await
            .context("记录原生上下文压缩事件失败")
    }
}

enum CachePlan {
    Manual,
    Automatic {
        run_id: String,
        provider: String,
        model: String,
        system: Option<String>,
        source_message_count: usize,
        cache_hit: bool,
    },
}

#[async_trait]
impl ContextLoader for NativeContextLoader {
    /// 根据估算 token 水位透传、微压缩或完整压缩上下文。
    ///
    /// 手动请求跳过自动水位并清空增量缓存；摘要失败或事件持久化失败时关闭本轮模型请求。
    async fn load(&self, request: CoreContextLoadRequest) -> Result<CoreLoadedContext> {
        let (effective, plan) = self.prepare_request(encode_core_request(request)?)?;
        let budget = compaction_budget(self.context_window, self.max_summary_tokens as usize);
        let outcome =
            compress_context(effective.clone(), effective.user_initiated, budget, self).await?;
        self.commit_cache(plan, &outcome)?;
        self.emit_event(&effective, outcome.event.as_ref()).await?;
        decode_core_context(outcome.context)
    }
}

/// 一次压缩计算的上下文和可选展示事件。
struct CompressionOutcome {
    context: LoadedContext,
    event: Option<Value>,
}

/// 完整压缩生成的替换消息、摘要范围和模型用量。
struct CompactedMessages {
    messages: Vec<Value>,
    summarized_messages: usize,
    /// 保留区内被折叠为首尾片段的超长工具结果数量。
    capped_results: usize,
    summary_usage: Option<Value>,
}

/// 摘要模型调用的最小内部结果。
struct SummaryResponse {
    text: String,
    usage: Option<Value>,
}

/// 上下文压缩只依赖该受限摘要接口，测试可注入不联网的确定性实现。
#[async_trait]
trait ContextSummarizer: Send + Sync {
    /// 摘要指定旧历史，并遵守输出预算。
    async fn summarize(&self, messages: &[Value], max_tokens: u32) -> Result<SummaryResponse>;
}

/// 按模型窗口、当前输入规模和手动请求选择分层压缩策略。
async fn compress_context(
    request: ContextLoadRequest,
    manual: bool,
    budget: CompactionBudget,
    summarizer: &dyn ContextSummarizer,
) -> Result<CompressionOutcome> {
    let before_messages = request.messages.len();
    let before_tokens = estimate_context_tokens(request.system.as_deref(), &request.messages);
    let CompactionBudget {
        compact: compact_threshold,
        micro: micro_threshold,
        recent_target,
    } = budget;

    if manual || before_tokens >= compact_threshold {
        if let Some(compacted) =
            compact_messages(&request.messages, manual, recent_target, summarizer).await?
        {
            let CompactedMessages {
                messages,
                summarized_messages,
                capped_results,
                summary_usage,
            } = compacted;
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            let after_messages = messages.len();
            return Ok(CompressionOutcome {
                context: LoadedContext {
                    system: request.system,
                    messages,
                },
                event: Some(json!({
                    "name": "context.compaction.completed",
                    "data": {
                        "run_id": request.run_id,
                        "step": request.step,
                        "before_messages": before_messages,
                        "after_messages": after_messages,
                        "summarized_messages": summarized_messages,
                        "capped_tool_results": capped_results,
                        "estimated_tokens_before": before_tokens,
                        "estimated_tokens_after": after_tokens,
                        "summary_usage": summary_usage,
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "model_summary_with_recent_tail",
                        "summary_max_output_tokens": DEFAULT_MAX_SUMMARY_TOKENS
                    },
                    "presentation": {
                        "target": "main_event_list",
                        "variant": "divider",
                        "tone": "info",
                        "text": "上下文压缩"
                    }
                })),
            });
        }
    }

    if manual || before_tokens >= micro_threshold {
        let (messages, cleared_results) = micro_compact_tool_results(&request.messages);
        // 最近若干条结果不清空，但同样需要封顶：巨型输出常常就出现在最新几条里。
        let (messages, capped_results) =
            cap_retained_tool_results(&messages, MAX_RETAINED_TOOL_RESULT_CHARS);
        if cleared_results > 0 || capped_results > 0 {
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            return Ok(CompressionOutcome {
                context: LoadedContext {
                    system: request.system,
                    messages,
                },
                event: Some(json!({
                    "name": "context.micro_compaction.completed",
                    "data": {
                        "run_id": request.run_id,
                        "step": request.step,
                        "before_messages": before_messages,
                        "after_messages": before_messages,
                        "cleared_tool_results": cleared_results,
                        "capped_tool_results": capped_results,
                        "estimated_tokens_before": before_tokens,
                        "estimated_tokens_after": after_tokens,
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "clear_old_successful_tool_results_and_cap_retained"
                    },
                    "presentation": { "target": "none" }
                })),
            });
        }
    }

    if manual {
        return Ok(CompressionOutcome {
            context: LoadedContext {
                system: request.system,
                messages: request.messages,
            },
            event: Some(json!({
                "name": "context.compaction.skipped",
                "data": {
                    "run_id": request.run_id,
                    "step": request.step,
                    "estimated_tokens": before_tokens,
                    "reason": "no_compressible_history",
                    "trigger": "manual"
                },
                "presentation": {
                    "target": "main_event_list",
                    "variant": "divider",
                    "tone": "muted",
                    "text": "没有可压缩的历史上下文"
                }
            })),
        });
    }

    Ok(CompressionOutcome {
        context: LoadedContext {
            system: request.system,
            messages: request.messages,
        },
        event: None,
    })
}

/// 一次压缩决策使用的水位与保留预算，全部由真实上下文窗口推导。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionBudget {
    /// 触发完整模型摘要的水位。
    compact: usize,
    /// 触发工具结果微压缩的水位。
    micro: usize,
    /// 完整压缩后期望保留的近期上下文规模。
    recent_target: usize,
}

/// 按上下文窗口与摘要输出预算推导各级水位。
///
/// 此前按模型名里是否含 `[1m]` 在两组硬编码常量之间二选一：模型名是展示字符串，
/// 换个服务商或别名就失配，而 128k、256k 这类窗口根本无法表达。这里改为从调用方
/// 传入的真实窗口推导，未知窗口才回落到假定值。
fn compaction_budget(context_window: Option<usize>, summary_tokens: usize) -> CompactionBudget {
    let window = context_window
        .filter(|window| *window > 0)
        .unwrap_or(ASSUMED_CONTEXT_WINDOW_TOKENS);
    // 摘要输出与下一轮请求都要占用同一个窗口，先扣掉再决定何时压缩。
    let reserved = summary_tokens.saturating_add(NEXT_TURN_BUFFER_TOKENS);
    // 小窗口扣完预留可能归零，此时用窗口比例兜底，避免每轮都压缩。
    let compact = window
        .saturating_sub(reserved)
        .max(window * MIN_COMPACT_THRESHOLD_RATIO / 100)
        .max(1);
    CompactionBudget {
        compact,
        micro: (compact * MICRO_COMPACT_THRESHOLD_RATIO / 100).max(1),
        recent_target: (window * RECENT_CONTEXT_TARGET_RATIO / 100).max(1),
    }
}

/// 用序列化字节数进行估算，ASCII 与非 ASCII 分别计费。
fn estimate_context_tokens(system: Option<&str>, messages: &[Value]) -> usize {
    let system_tokens = system.map_or(0, estimate_text_tokens);
    system_tokens
        + messages
            .iter()
            .map(|message| estimate_text_tokens(&message.to_string()))
            .sum::<usize>()
}

/// 将文本字节数换算为粗略 token 数。
///
/// 统一按三字节一个 token 会把英文高估约三成——英文约四字节一个 token，中文才是
/// 三字节（一个汉字）一个 token。高估直接导致英文为主的会话提前触发压缩，白白
/// 损失可用上下文，因此两类字节分开折算。
fn estimate_text_tokens(text: &str) -> usize {
    let non_ascii_bytes = text.bytes().filter(|byte| !byte.is_ascii()).count();
    let ascii_bytes = text.len() - non_ascii_bytes;
    ascii_bytes.div_ceil(4) + non_ascii_bytes.div_ceil(3)
}

/// 清理较旧的成功工具结果正文，同时保留调用关联、错误结果和最近成功结果。
fn micro_compact_tool_results(messages: &[Value]) -> (Vec<Value>, usize) {
    let keep = RECENT_TOOL_RESULTS_TO_KEEP;
    let total_results = messages
        .iter()
        .map(compactable_tool_result_count)
        .sum::<usize>();
    let mut remaining_to_clear = total_results.saturating_sub(keep);
    if remaining_to_clear == 0 {
        return (messages.to_vec(), 0);
    }

    let mut cleared = 0;
    let compacted = messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
                return message;
            };
            for block in blocks {
                if remaining_to_clear == 0
                    || block.get("type").and_then(Value::as_str) != Some("tool_result")
                {
                    continue;
                }
                let Some(result) = block.get_mut("result") else {
                    continue;
                };
                if result
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }
                let Some(content) = result.get_mut("content") else {
                    continue;
                };
                if content.as_str() == Some(CLEARED_TOOL_RESULT) {
                    remaining_to_clear -= 1;
                    continue;
                }
                *content = Value::String(CLEARED_TOOL_RESULT.into());
                remaining_to_clear -= 1;
                cleared += 1;
            }
            message
        })
        .collect();
    (compacted, cleared)
}

/// 把超长文本折叠为首尾片段，中段替换为省略标记。
///
/// 保留首尾而不是只留开头：巨型日志和堆栈的中段几乎不携带增量信息，但开头决定
/// "这是什么"，结尾决定"最终怎么了"，只留其一都会让结果失去判断价值。
fn truncate_middle(text: &str, max_chars: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return None;
    }
    // 首段多留一些：工具结果的开头通常是路径、命令或错误类型。
    let head = max_chars * 2 / 3;
    let tail = max_chars - head;
    let omitted = chars.len() - head - tail;
    Some(format!(
        "{}\n… [已省略 {omitted} 字符] …\n{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>(),
    ))
}

/// 折叠保留区内超长的工具结果正文，返回新消息与被折叠的结果数。
///
/// 与微压缩互补：微压缩整体清空**较旧**结果的正文，这里只对**保留下来**的结果做
/// 长度封顶。近期结果仍需可读，因此不能清空，但单条动辄上百 KB 的输出同样会挤占
/// 窗口——完整压缩此前完全没有处理保留区，尾部经常仍是几万 token。
fn cap_retained_tool_results(messages: &[Value], max_chars: usize) -> (Vec<Value>, usize) {
    let mut capped = 0;
    let compacted = messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
                return message;
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(content) = block.pointer_mut("/result/content") else {
                    continue;
                };
                let Some(text) = content.as_str() else {
                    continue;
                };
                if text == CLEARED_TOOL_RESULT {
                    continue;
                }
                if let Some(truncated) = truncate_middle(text, max_chars) {
                    *content = Value::String(truncated);
                    capped += 1;
                }
            }
            message
        })
        .collect();
    (compacted, capped)
}

/// 统计一条消息中允许微压缩的成功工具结果块数量。
fn compactable_tool_result_count(message: &Value) -> usize {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && !block
                            .pointer("/result/is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// 用独立模型摘要替换较旧 API 轮次；手动模式只保留最新完整轮次。
async fn compact_messages(
    messages: &[Value],
    manual: bool,
    recent_target: usize,
    summarizer: &dyn ContextSummarizer,
) -> Result<Option<CompactedMessages>> {
    let group_starts = api_round_group_starts(messages);
    if group_starts.len() < 2 {
        return Ok(None);
    }

    let split_index = recent_tail_start(messages, &group_starts, manual, recent_target);
    if split_index == 0 || split_index >= messages.len() {
        return Ok(None);
    }

    let response = summarizer
        .summarize(&messages[..split_index], DEFAULT_MAX_SUMMARY_TOKENS)
        .await?;
    let summary = normalize_model_summary(&response.text)?;
    // 保留区仍是原文，单条超长结果会让尾部继续占据数万 token；这里只封顶不清空。
    let (retained, capped_results) =
        cap_retained_tool_results(&messages[split_index..], MAX_RETAINED_TOOL_RESULT_CHARS);
    let mut compacted = Vec::with_capacity(retained.len() + 1);
    compacted.push(json!({
        "role": "developer",
        "content": [{
            "type": "text",
            "text": format!(
                "本会话早期上下文已压缩为以下结构化摘要。近期消息仍按原文附在摘要之后。\n\n{summary}"
            )
        }]
    }));
    compacted.extend(retained);
    Ok(Some(CompactedMessages {
        messages: compacted,
        summarized_messages: split_index,
        capped_results,
        summary_usage: response.usage,
    }))
}

/// 以 assistant 响应开头划分 API 轮次，避免拆散工具调用与对应结果。
fn api_round_group_starts(messages: &[Value]) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, message) in messages.iter().enumerate().skip(1) {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            starts.push(index);
        }
    }
    starts
}

/// 自动模式保留约 40k 尾部；手动模式只保留最新完整轮次以确保实际发生压缩。
fn recent_tail_start(
    messages: &[Value],
    group_starts: &[usize],
    manual: bool,
    recent_target: usize,
) -> usize {
    if manual {
        return *group_starts.last().unwrap_or(&0);
    }
    let mut preserved_tokens: usize = 0;
    let mut start = *group_starts.last().unwrap_or(&0);

    for (preserved_groups, group_index) in (0..group_starts.len()).rev().enumerate() {
        let group_start = group_starts[group_index];
        let group_end = group_starts
            .get(group_index + 1)
            .copied()
            .unwrap_or(messages.len());
        let group_tokens = estimate_context_tokens(None, &messages[group_start..group_end]);
        if preserved_groups >= 1 && preserved_tokens.saturating_add(group_tokens) > recent_target {
            break;
        }
        preserved_tokens += group_tokens;
        start = group_start;
    }
    start
}

/// 通过 Host 固定路由调用模型生成摘要；Guest 无法指定 provider、model 或工具。
#[async_trait]
impl ContextSummarizer for NativeContextLoader {
    /// 使用应用固定的 provider/model 生成摘要；调用强制禁用工具、推理和 provider 逃生参数。
    async fn summarize(&self, messages: &[Value], max_tokens: u32) -> Result<SummaryResponse> {
        let mut summary_messages = messages.to_vec();
        summary_messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": SUMMARY_REQUEST_PROMPT}]
        }));
        let messages = summary_messages
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<Vec<_>>>()
            .context("解码上下文摘要模型消息失败")?;
        let mut request = ModelRequest::new(self.model.clone(), messages);
        request.system = Some(SUMMARY_SYSTEM_PROMPT.into());
        request.tool_choice = ToolChoice::None;
        request.reasoning = ReasoningLevel::Off;
        request.max_tokens = Some(max_tokens.min(self.max_summary_tokens).max(1));
        let response = if self.stream {
            self.gateway
                .stream(&self.provider, request)
                .await
                .context("启动原生上下文摘要流失败")?
                .result()
                .await
                .context("原生上下文摘要流调用失败")?
        } else {
            self.gateway
                .complete(&self.provider, request)
                .await
                .context("原生上下文摘要模型调用失败")?
        };
        if !response.tool_calls.is_empty() {
            return Err(anyhow!("原生上下文摘要返回了未授权工具调用"));
        }
        let text = response.text_content();
        if text.trim().is_empty() {
            return Err(anyhow!("原生上下文摘要返回了空文本"));
        }
        Ok(SummaryResponse {
            text,
            usage: response
                .usage
                .map(serde_json::to_value)
                .transpose()
                .context("编码上下文摘要用量失败")?,
        })
    }
}

/// 清理模型可能返回的分析草稿和 summary 标签，并拒绝空摘要。
fn normalize_model_summary(text: &str) -> Result<String> {
    let trimmed = remove_tagged_section(text, "<analysis>", "</analysis>");
    let trimmed = trimmed.trim();
    let summary = match (trimmed.find("<summary>"), trimmed.rfind("</summary>")) {
        (Some(start), Some(end)) if start + "<summary>".len() <= end => {
            &trimmed[start + "<summary>".len()..end]
        }
        _ => trimmed,
    }
    .trim();
    if summary.is_empty() {
        return Err(anyhow!("模型返回了空上下文摘要"));
    }
    Ok(summary.to_string())
}

/// 删除第一段完整的 XML 风格区块；标签缺失时保持原文。
fn remove_tagged_section(text: &str, open: &str, close: &str) -> String {
    let Some(start) = text.find(open) else {
        return text.to_string();
    };
    let Some(relative_end) = text[start + open.len()..].find(close) else {
        return text.to_string();
    };
    let end = start + open.len() + relative_end + close.len();
    format!("{}{}", &text[..start], &text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试使用的普通文本消息。
    fn text_message(role: &str, text: impl Into<String>) -> Value {
        json!({
            "role": role,
            "content": [{"type": "text", "text": text.into()}]
        })
    }

    /// 构造测试使用的工具结果消息。
    fn tool_result_message(index: usize, size: usize) -> Value {
        json!({
            "role": "tool",
            "content": [{
                "type": "tool_result",
                "result": {
                    "call_id": format!("call-{index}"),
                    "name": "Read",
                    "content": "x".repeat(size),
                    "is_error": false
                }
            }]
        })
    }

    /// 构造必须跨微压缩保留的失败工具结果。
    fn failed_tool_result_message(index: usize, content: impl Into<String>) -> Value {
        json!({
            "role": "tool",
            "content": [{
                "type": "tool_result",
                "result": {
                    "call_id": format!("failed-call-{index}"),
                    "name": "Read",
                    "content": content.into(),
                    "is_error": true
                }
            }]
        })
    }

    /// 返回测试使用的确定性模型摘要，并模拟 Provider 用量。
    struct TestSummarizer;

    #[async_trait]
    impl ContextSummarizer for TestSummarizer {
        async fn summarize(
            &self,
            _messages: &[Value],
            _max_tokens: u32,
        ) -> Result<SummaryResponse> {
            Ok(SummaryResponse {
                text: "<analysis>测试草稿</analysis><summary>模型生成的上下文摘要：保留用户请求、错误状态与下一步。</summary>".into(),
                usage: Some(json!({
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "total_tokens": 120
                })),
            })
        }
    }

    /// 测试统一使用 200k 窗口推导出的水位，与未声明窗口时的默认行为一致。
    fn test_budget() -> CompactionBudget {
        compaction_budget(None, DEFAULT_MAX_SUMMARY_TOKENS as usize)
    }

    /// 返回单条 ASCII 工具结果跨过指定 token 水位所需的字符数。
    ///
    /// 夹具按当前估算口径反推，而不是写死字节数：估算口径调整时测试意图仍然成立，
    /// 不会因为常量过时而误判为行为回归。
    fn chars_to_exceed(tokens: usize, messages: usize) -> usize {
        // ASCII 按四字节一个 token 折算，留一条消息的余量覆盖 JSON 包装开销。
        tokens * 4 / messages + 1_024
    }

    /// 使用确定性摘要器执行一次测试压缩。
    async fn compress_for_test(request: ContextLoadRequest, manual: bool) -> CompressionOutcome {
        compress_context(request, manual, test_budget(), &TestSummarizer)
            .await
            .expect("测试压缩应成功")
    }

    /// 低于水位时必须完整透传，避免短会话发生无意义改写。
    #[tokio::test]
    async fn keeps_small_context_unchanged() {
        let messages = vec![text_message("user", "检查当前实现")];
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "run-small".into(),
                user_initiated: false,
                step: 0,
                provider: "test".into(),
                model: "test-model".into(),
                system: Some("保持准确".into()),
                messages: messages.clone(),
            },
            false,
        )
        .await;

        assert_eq!(outcome.context.messages, messages);
        assert!(outcome.event.is_none());
    }

    /// 微压缩只清理旧结果，最近三条工具结果必须保持原文。
    #[tokio::test]
    async fn micro_compaction_preserves_recent_tool_results() {
        let size = chars_to_exceed(test_budget().micro, 6);
        let messages = (0..6)
            .map(|index| tool_result_message(index, size))
            .collect::<Vec<_>>();
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "run-micro".into(),
                user_initiated: false,
                step: 1,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages,
            },
            false,
        )
        .await;

        assert_eq!(
            outcome
                .event
                .as_ref()
                .and_then(|event| event["name"].as_str()),
            Some("context.micro_compaction.completed")
        );
        assert!(
            outcome.event.as_ref().is_some_and(|event| event
                .pointer("/presentation/target")
                .and_then(Value::as_str)
                == Some("none")),
            "微压缩事件不应请求 UI 展示"
        );
        for message in &outcome.context.messages[..3] {
            assert_eq!(
                message["content"][0]["result"]["content"],
                CLEARED_TOOL_RESULT
            );
        }
        for message in &outcome.context.messages[3..] {
            assert_ne!(
                message["content"][0]["result"]["content"],
                CLEARED_TOOL_RESULT
            );
        }
    }

    /// 微压缩不能清理失败结果，否则后续模型会失去仍待处理的错误状态。
    #[tokio::test]
    async fn micro_compaction_preserves_failed_tool_results() {
        let failure = failed_tool_result_message(0, "文件不存在：src/missing.rs");
        let size = chars_to_exceed(test_budget().micro, 4);
        let mut messages = vec![failure.clone()];
        messages.extend((0..4).map(|index| tool_result_message(index, size)));
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "run-micro-error".into(),
                user_initiated: false,
                step: 1,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages,
            },
            false,
        )
        .await;

        assert_eq!(outcome.context.messages[0], failure);
        assert_eq!(
            outcome.context.messages[1]["content"][0]["result"]["content"],
            CLEARED_TOOL_RESULT
        );
    }

    /// 完整压缩应生成结构化摘要，并逐字保留最近 API 轮次。
    #[tokio::test]
    async fn full_compaction_summarizes_prefix_and_keeps_recent_rounds() {
        let recent_request = "继续修复最新失败用例";
        let messages = vec![
            text_message("user", "分析上下文压缩"),
            text_message("assistant", "已定位压缩入口"),
            failed_tool_result_message(0, "读取配置失败：权限不足"),
            tool_result_message(0, chars_to_exceed(test_budget().compact, 1)),
            text_message("assistant", "已完成旧历史分析"),
            text_message("user", recent_request),
            text_message("assistant", "正在处理最新用例"),
        ];
        struct RecordingSummarizer(Mutex<Vec<Value>>);

        #[async_trait]
        impl ContextSummarizer for RecordingSummarizer {
            async fn summarize(
                &self,
                messages: &[Value],
                max_tokens: u32,
            ) -> Result<SummaryResponse> {
                *self.0.lock().expect("记录锁不应损坏") = messages.to_vec();
                TestSummarizer.summarize(messages, max_tokens).await
            }
        }

        let summarizer = RecordingSummarizer(Mutex::new(Vec::new()));
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "run-full".into(),
                user_initiated: false,
                step: 2,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages,
            },
            false,
            test_budget(),
            &summarizer,
        )
        .await
        .expect("完整压缩应调用模型摘要");

        assert_eq!(
            outcome
                .event
                .as_ref()
                .and_then(|event| event["name"].as_str()),
            Some("context.compaction.completed")
        );
        assert_eq!(outcome.context.messages[0]["role"], "developer");
        let summary = outcome.context.messages[0]["content"][0]["text"]
            .as_str()
            .expect("摘要应为文本");
        assert!(summary.contains("模型生成的上下文摘要"));
        assert!(!summary.contains("测试草稿"));
        assert!(summarizer
            .0
            .lock()
            .expect("记录锁不应损坏")
            .iter()
            .any(|message| message.to_string().contains("读取配置失败：权限不足")));
        assert_eq!(
            outcome.event.as_ref().expect("应发布压缩事件")["data"]["summary_usage"]
                ["output_tokens"],
            20
        );
        assert!(outcome
            .context
            .messages
            .iter()
            .any(|message| { message["content"][0]["text"].as_str() == Some(recent_request) }));
    }

    /// 水位必须随真实上下文窗口伸缩，而不是按模型名猜测。
    #[test]
    fn budget_scales_with_real_context_window() {
        let summary = DEFAULT_MAX_SUMMARY_TOKENS as usize;
        let small = compaction_budget(Some(200_000), summary);
        let large = compaction_budget(Some(1_000_000), summary);

        // 200k 扣掉 20k 摘要输出与 13k 下一轮缓冲。
        assert_eq!(small.compact, 167_000);
        assert_eq!(large.compact, 967_000);
        assert!(small.micro < small.compact);
        assert!(large.recent_target > small.recent_target);

        // 未声明窗口时按 200k 假定值兜底，与显式传入 200k 等价。
        assert_eq!(compaction_budget(None, summary), small);
        assert_eq!(compaction_budget(Some(0), summary), small);
    }

    /// 小窗口扣除固定预留后不得归零，否则每一轮都会触发压缩。
    #[test]
    fn small_window_budget_falls_back_to_window_ratio() {
        let budget = compaction_budget(Some(32_000), DEFAULT_MAX_SUMMARY_TOKENS as usize);

        assert_eq!(budget.compact, 32_000 * MIN_COMPACT_THRESHOLD_RATIO / 100);
        assert!(budget.compact > 0);
        assert!(budget.micro > 0);
        assert!(budget.recent_target > 0);
    }

    /// 英文与中文按不同字节率折算，避免英文会话被高估而提前压缩。
    #[test]
    fn token_estimate_separates_ascii_from_wide_characters() {
        // 400 字节 ASCII 约 100 token；统一按三字节折算会高估到 134。
        assert_eq!(estimate_text_tokens(&"x".repeat(400)), 100);
        // 100 个汉字共 300 字节，约 100 token。
        assert_eq!(estimate_text_tokens(&"中".repeat(100)), 100);
    }

    /// 超长文本折叠为首尾片段，短文本保持原样。
    #[test]
    fn oversized_text_keeps_head_and_tail() {
        assert_eq!(truncate_middle("短文本", 100), None);

        let text = format!("开头{}结尾", "x".repeat(500));
        let truncated = truncate_middle(&text, 100).expect("超长文本应被折叠");

        assert!(truncated.starts_with("开头"), "{truncated}");
        assert!(truncated.ends_with("结尾"), "{truncated}");
        assert!(truncated.contains("已省略"), "{truncated}");
        assert!(truncated.chars().count() < text.chars().count());
    }

    /// 保留区内的超长工具结果必须封顶，否则完整压缩后尾部仍占据大量窗口。
    #[tokio::test]
    async fn full_compaction_caps_oversized_retained_results() {
        let messages = vec![
            text_message("user", "分析压缩"),
            text_message("assistant", "已定位入口"),
            text_message(
                "user",
                "x".repeat(chars_to_exceed(test_budget().compact, 1)),
            ),
            text_message("assistant", "最新一轮"),
            tool_result_message(9, MAX_RETAINED_TOOL_RESULT_CHARS * 4),
        ];
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "run-cap".into(),
                user_initiated: false,
                step: 1,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages,
            },
            false,
        )
        .await;

        let event = outcome.event.as_ref().expect("应发生完整压缩");
        assert_eq!(event["name"], "context.compaction.completed");
        assert_eq!(event["data"]["capped_tool_results"], 1);

        let retained = outcome
            .context
            .messages
            .iter()
            .filter_map(|message| message.pointer("/content/0/result/content"))
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(retained.len(), 1);
        // 折叠后仍远小于原文，同时保留了首尾片段。
        assert!(retained[0].chars().count() < MAX_RETAINED_TOOL_RESULT_CHARS * 2);
        assert!(retained[0].contains("已省略"));
    }

    /// 只有两个 API 轮次时也必须能压缩超大前缀，不能因保留轮次数下限失效。
    #[tokio::test]
    async fn full_compaction_handles_two_api_rounds() {
        let recent_request = "保留当前请求";
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "run-two-rounds".into(),
                user_initiated: false,
                step: 1,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: vec![
                    text_message(
                        "user",
                        "x".repeat(chars_to_exceed(test_budget().compact, 1)),
                    ),
                    text_message("assistant", "已接收旧请求"),
                    text_message("user", recent_request),
                ],
            },
            false,
        )
        .await;

        assert_eq!(
            outcome
                .event
                .as_ref()
                .and_then(|event| event["name"].as_str()),
            Some("context.compaction.completed")
        );
        assert!(outcome
            .context
            .messages
            .iter()
            .any(|message| { message["content"][0]["text"].as_str() == Some(recent_request) }));
    }

    /// 手动压缩不受自动水位限制，并在存在历史轮次时生成完整摘要。
    #[tokio::test]
    async fn manual_compaction_bypasses_automatic_threshold() {
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "run-manual".into(),
                user_initiated: true,
                step: 0,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: vec![
                    text_message("user", "较早请求"),
                    text_message("assistant", "较早回复"),
                    text_message("user", "当前请求"),
                ],
            },
            true,
        )
        .await;

        assert_eq!(
            outcome
                .event
                .as_ref()
                .and_then(|event| event["name"].as_str()),
            Some("context.compaction.completed")
        );
        assert_eq!(
            outcome.event.expect("应发布压缩事件")["data"]["trigger"],
            "manual"
        );
    }

    /// 自动压缩后新增消息应复用已压缩前缀，且不重复发布压缩事件。
    #[tokio::test]
    async fn incremental_compaction_reuses_compacted_prefix() {
        let large_history = "历史工具输出".repeat(90_000);
        let original_messages = vec![
            text_message("user", &large_history),
            text_message("assistant", "已分析历史"),
            text_message("user", "继续处理"),
            text_message("assistant", "正在处理"),
        ];
        let loader = NativeContextLoader::new(
            ModelGateway::new(),
            "test",
            "test-model",
            false,
            DEFAULT_MAX_SUMMARY_TOKENS,
            Arc::new(agent_core::InMemoryEventSink::new()),
        )
        .expect("应创建原生上下文加载器");
        let (request, plan) = loader
            .prepare_request(ContextLoadRequest {
                run_id: "incremental-run".into(),
                step: 0,
                user_initiated: false,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: original_messages.clone(),
            })
            .expect("应准备首次压缩请求");
        let first = compress_context(request, false, test_budget(), &TestSummarizer)
            .await
            .expect("首次增量压缩应成功");
        loader
            .commit_cache(plan, &first)
            .expect("应提交首次压缩缓存");
        assert!(first.event.is_some(), "首轮超过水位时应执行自动压缩");

        let mut extended_messages = original_messages;
        extended_messages.push(text_message("user", "检查刚才的修改"));
        let (request, plan) = loader
            .prepare_request(ContextLoadRequest {
                run_id: "incremental-run".into(),
                step: 1,
                user_initiated: false,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: extended_messages,
            })
            .expect("应准备追加压缩请求");
        let second = compress_context(request, false, test_budget(), &TestSummarizer)
            .await
            .expect("后续增量压缩应成功");
        loader
            .commit_cache(plan, &second)
            .expect("应提交追加压缩缓存");

        assert!(second.event.is_none(), "复用压缩前缀时不应重复发布压缩事件");
        assert_eq!(
            second.context.messages.len(),
            first.context.messages.len() + 1
        );
        assert_eq!(
            second.context.messages.last(),
            Some(&text_message("user", "检查刚才的修改"))
        );
    }

    /// 用户显式发起的压缩不受水位限制，立即返回替换上下文。
    #[tokio::test]
    async fn user_initiated_compaction_returns_replacement_immediately() {
        let request = ContextLoadRequest {
            run_id: "compact-now".into(),
            step: 0,
            provider: "manual".into(),
            model: "test-model".into(),
            system: None,
            messages: vec![
                text_message("user", "较早请求"),
                text_message("assistant", "较早回复"),
                text_message("user", "中间请求"),
                text_message("assistant", "中间回复"),
                text_message("user", "当前请求"),
            ],
            user_initiated: true,
        };
        let outcome = compress_context(request, true, test_budget(), &TestSummarizer)
            .await
            .expect("主动压缩应立即返回结果");

        assert_eq!(outcome.context.messages.len(), 3);
        assert!(outcome.context.messages[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("本会话早期上下文已压缩")));
        assert_eq!(
            outcome.event.expect("应发布压缩事件")["data"]["trigger"],
            "manual"
        );
    }
}
