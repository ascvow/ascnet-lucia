//! Lucia 默认原生上下文压缩、模型摘要和 Context Policy 执行能力。

#![deny(missing_docs)]

use agent_core::{
    event::{AgentEvent, AgentEventKind, EventSink},
    model::{ModelGateway, ModelRequest},
    ContextLoadRequest as CoreContextLoadRequest, ContextLoader,
    LoadedContext as CoreLoadedContext, PromptCacheRetention, ReasoningLevel, ToolChoice,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// 未知上下文窗口时假定的窗口大小。
///
/// 只在既没有配置也没有 models.dev 元数据时兜底；正常路径应由调用方传入真实窗口。
const ASSUMED_CONTEXT_WINDOW_TOKENS: usize = 200_000;
/// 自动压缩相对当前模型最大上下文窗口的触发比例。
///
/// 微压缩与完整摘要共用该水位，避免未达到 80% 时提前改写上下文。
const COMPACT_THRESHOLD_RATIO: usize = 80;
/// 完整压缩后保留的近期上下文占窗口的比例。
///
/// 提高到 30% 以保留更多最近 API 轮次；摘要输出仍会预留最多一半的压缩预算。
const RECENT_CONTEXT_TARGET_RATIO: usize = 30;
/// 保留区内单个工具结果允许的最大字符数。
///
/// 超过该长度的结果保留首尾片段：巨型日志和堆栈的中段几乎不携带增量信息，
/// 但首尾同时决定了"这是什么"和"最终怎么了"。
const MAX_RETAINED_TOOL_RESULT_CHARS: usize = 2_000;
/// 微压缩始终原样保留的最近工具结果数量。
const RECENT_TOOL_RESULTS_TO_KEEP: usize = 3;
/// 微压缩推进清理边界所需的最小积压条数。
///
/// 清理边界一旦前移就改写了历史中间的消息，服务端提示缓存会从改写点断开。按"保留最近
/// 三条"逐轮推进意味着每一轮都断一次；这里改为积压够一批才推进一次，让绝大多数轮次的
/// 上下文前缀保持逐字节稳定，代价只是旧结果多留一小段时间。
const MICRO_COMPACT_CLEAR_BATCH: usize = 8;
/// Claude Code 为完整摘要预留的最大输出 token 数。
/// 原生上下文摘要请求允许的默认最大输出 token 数。
pub const DEFAULT_MAX_SUMMARY_TOKENS: u32 = 20_000;
/// 被微压缩的工具结果占位文本。
const CLEARED_TOOL_RESULT: &str = "[older tool result elided]";
/// 独立摘要调用使用的固定 system 提示，不携带主 Agent 的工具和行为人格。
const SUMMARY_SYSTEM_PROMPT: &str =
    "You compress long agent conversations into faithful, self-contained handoff summaries. Never call tools.";
/// 完整压缩指令，作为尾部 user 消息追加在待摘要历史之后。
///
/// 使用英文以贴合模型的指令跟随分布；摘要正文的语言由对话本身决定。要求先写
/// `<analysis>` 再写 `<summary>`，前者会被 [`normalize_model_summary`] 剥离，只把
/// 结构化摘要写回上下文。
const SUMMARY_REQUEST_PROMPT: &str = r#"Write a handoff summary of the entire conversation above. It will REPLACE that conversation: the only things carried forward are your summary plus the few most recent messages, so anything you leave out is permanently lost.

First, inside <analysis> tags, walk through the conversation chronologically and note what must survive: each user request, the technical decisions taken, the files touched, and the work still open. Keep it brief; this section is discarded.

Then, inside <summary> tags, write the summary using exactly these sections:

1. User requests and intent — every explicit request, verbatim where the exact wording matters, in the order they were made.
2. Technical context — the stack, architecture, conventions and constraints that govern the work.
3. Files and code — for each file: its path, why it was touched, and the exact content of important changes. Quote code and signatures verbatim rather than describing them.
4. Errors and fixes — each failure encountered, its root cause, the fix applied, and any user correction of your approach.
5. Completed work — what is done and verified, including how it was verified (commands run, tests passed).
6. Pending work — what remains, with enough detail to resume without re-reading the conversation.
7. Current state — the task in progress at the moment of compaction, the exact file and function being edited, and the immediate next step. Only list a next step if it continues work already underway; do not invent new directions.

Rules: preserve identifiers, paths, commands, versions, and error strings exactly — never paraphrase them. Record decisions with the reasoning that produced them. State facts, not advice. Do not speculate about anything the conversation does not establish. Write the summary in the language the user has been using."#;

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

fn encode_core_request(
    request: CoreContextLoadRequest,
) -> Result<(CoreContextLoadRequest, ContextLoadRequest)> {
    let CoreContextLoadRequest {
        run_id,
        step,
        provider,
        model,
        system,
        messages,
        user_initiated,
    } = request;
    let encoded = messages
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()
        .context("编码 Core 上下文消息失败")?;
    Ok((
        CoreContextLoadRequest {
            run_id: run_id.clone(),
            step,
            provider: provider.clone(),
            model: model.clone(),
            system: system.clone(),
            messages,
            user_initiated,
        },
        ContextLoadRequest {
            run_id,
            step,
            provider,
            model,
            system,
            messages: encoded,
            user_initiated,
        },
    ))
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
/// 它使用应用当前配置的模型路由，摘要请求禁用工具与推理。
pub struct NativeContextLoader {
    events: Arc<dyn EventSink>,
    state: Mutex<CompressionState>,
}

/// 一次上下文加载固定使用的模型路由、窗口与摘要调用参数。
#[derive(Clone)]
struct NativeSummaryModel {
    gateway: ModelGateway,
    provider: String,
    model: String,
    stream: bool,
    max_summary_tokens: u32,
    /// 目标模型的真实上下文窗口；`None` 时按假定窗口推导水位。
    context_window: Option<usize>,
}

struct CompressionState {
    /// 路由与窗口由同一快照承载，防止热切换时读取到不同模型的参数。
    summary_model: Arc<NativeSummaryModel>,
    /// 最近一次自动压缩结果，用于增量追加后续消息，避免重复处理同一历史前缀。
    cache: Option<CompressionCache>,
}

/// 最近一次自动压缩的追加边界与有效模型上下文。
#[derive(Clone)]
struct CompressionCache {
    /// 跨运行必须使缓存失效；同一运行内仍需校验消息前缀，防止动态提示或历史被修订。
    run_id: String,
    provider: String,
    model: String,
    system: Option<String>,
    /// 生成压缩结果时原始上下文的消息数，用于识别后续追加内容。
    source_message_count: usize,
    /// 原始消息前缀的稳定摘要；内容被修订时不得复用旧压缩结果。
    source_messages_digest: [u8; 32],
    /// 上一轮生效的微压缩清理边界，即已被清空的成功工具结果条数。
    micro_cleared: usize,
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
            events,
            state: Mutex::new(CompressionState {
                summary_model: Arc::new(NativeSummaryModel {
                    gateway,
                    provider,
                    model,
                    stream,
                    max_summary_tokens,
                    context_window: None,
                }),
                cache: None,
            }),
        })
    }

    /// 声明目标模型的真实上下文窗口。
    ///
    /// 未声明时按 200k 假定窗口推导水位；真实窗口更大时会过早压缩，更小时会越界。
    /// 调用方应把配置或 models.dev 元数据里的窗口传进来。
    pub fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        Arc::make_mut(&mut state.summary_model).context_window =
            normalize_context_window(context_window);
        state.cache = None;
        self
    }

    /// 原子更新当前摘要模型路由和上下文窗口，并使已有压缩缓存失效。
    ///
    /// `context_window` 为零或未提供时按 200k 假定窗口推导水位。已经开始的加载继续使用
    /// 原路由与窗口快照，其结果不会回填到新路由的缓存。
    ///
    /// # 错误
    ///
    /// 模型路由为空或内部状态锁已损坏时返回错误，保留原配置。
    pub fn update_model_route(
        &self,
        provider: String,
        model: String,
        context_window: Option<u64>,
    ) -> Result<()> {
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(anyhow!("原生上下文压缩的模型路由不能为空"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("原生上下文压缩缓存锁已损坏"))?;
        state.summary_model = Arc::new(NativeSummaryModel {
            provider,
            model,
            context_window: normalize_context_window(context_window),
            ..state.summary_model.as_ref().clone()
        });
        state.cache = None;
        Ok(())
    }

    /// 在不持有锁的情况下构造本轮有效请求及缓存更新元数据。
    fn prepare_request(
        &self,
        request: ContextLoadRequest,
    ) -> Result<(ContextLoadRequest, CachePlan)> {
        let (summary_model, cache) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("原生上下文压缩缓存锁已损坏"))?;
            let summary_model = Arc::clone(&state.summary_model);
            if request.user_initiated {
                state.cache = None;
                return Ok((request, CachePlan::Manual { summary_model }));
            }
            (summary_model, state.cache.clone())
        };
        // 大历史的摘要计算不应占用缓存锁，否则并行请求会被纯 CPU 工作串行阻塞。
        let cache_candidate = cache.as_ref().filter(|cache| {
            cache.run_id == request.run_id
                && cache.provider == request.provider
                && cache.model == request.model
                && cache.system == request.system
                && request.messages.len() >= cache.source_message_count
        });
        let micro_budget = compaction_budget(
            summary_model.context_window,
            summary_model.max_summary_tokens as usize,
        )
        .micro;
        // 缓存命中判定只覆盖已压缩前缀。需要为新缓存计算全量摘要时从同一哈希状态
        // 继续追加剩余消息，避免把整段历史重复扫描两遍。
        let mut prefix_digest = Sha256::new();
        let prefix_len = cache_candidate.map(|cache| cache.source_message_count);
        let cache_hit = match cache_candidate {
            Some(cache) => {
                hash_messages_into(
                    &mut prefix_digest,
                    &request.messages[..cache.source_message_count],
                )?;
                <[u8; 32]>::from(prefix_digest.clone().finalize()) == cache.source_messages_digest
            }
            None => false,
        };
        let mut estimated_tokens = None;
        let source_messages_digest = if let Some(cache) = cache_candidate
            .filter(|cache| cache_hit && request.messages.len() == cache.source_message_count)
        {
            Some(cache.source_messages_digest)
        } else if cache_hit {
            match prefix_len {
                Some(prefix_len) => {
                    hash_messages_into(&mut prefix_digest, &request.messages[prefix_len..])?;
                    Some(<[u8; 32]>::from(prefix_digest.finalize()))
                }
                None => unreachable!("缓存命中时必须存在有效的缓存前缀"),
            }
        } else {
            let (before_tokens, full_digest) = if cache_candidate.is_none() {
                let metrics = message_metrics(&request.messages)?;
                (
                    request
                        .system
                        .as_deref()
                        .map_or(0, estimate_text_tokens)
                        .saturating_add(metrics.tokens),
                    Some(metrics.digest),
                )
            } else {
                (
                    estimate_context_tokens(request.system.as_deref(), &request.messages),
                    None,
                )
            };
            estimated_tokens = Some(before_tokens);
            if before_tokens < micro_budget {
                None
            } else if let Some(full_digest) = full_digest {
                Some(full_digest)
            } else {
                match prefix_len {
                    Some(prefix_len) => {
                        hash_messages_into(&mut prefix_digest, &request.messages[prefix_len..])?;
                        Some(<[u8; 32]>::from(prefix_digest.finalize()))
                    }
                    None => unreachable!("未命中缓存时必须使用完整消息摘要"),
                }
            }
        };
        // 缓存未命中说明前缀不是上一轮的延续，清理边界必须从零重新推导。
        let micro_floor = cache
            .as_ref()
            .filter(|_| cache_hit)
            .map_or(0, |cache| cache.micro_cleared);
        let plan = CachePlan::Automatic {
            summary_model,
            run_id: request.run_id.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
            system: request.system.clone(),
            source_message_count: request.messages.len(),
            source_messages_digest,
            estimated_tokens,
            micro_floor,
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
        // 切换会替换配置快照；旧加载即使稍后完成，也不能恢复已失效的缓存。
        if !Arc::ptr_eq(&state.summary_model, plan.summary_model()) {
            return Ok(());
        }
        match plan {
            CachePlan::Manual { .. } => state.cache = None,
            CachePlan::Automatic {
                run_id,
                provider,
                model,
                system,
                source_message_count,
                source_messages_digest,
                cache_hit,
                ..
            } if cache_hit || outcome.event.is_some() => {
                let source_messages_digest = source_messages_digest
                    .ok_or_else(|| anyhow!("原生上下文压缩缓存缺少原始消息摘要"))?;
                state.cache = Some(CompressionCache {
                    run_id,
                    provider,
                    model,
                    system,
                    source_message_count,
                    source_messages_digest,
                    micro_cleared: outcome.micro_cleared,
                    loaded_messages: outcome.context.messages.clone(),
                });
            }
            CachePlan::Automatic { .. } => state.cache = None,
        }
        Ok(())
    }

    /// 把压缩事实写入 Agent 事件流；无事件的透传请求不产生记录。
    async fn emit_event(&self, run_id: &str, step: usize, event: Option<&Value>) -> Result<()> {
        let Some(event) = event else {
            return Ok(());
        };
        self.events
            .record(&AgentEvent::new(
                run_id.to_string(),
                AgentEventKind::Extension,
                step,
                event.clone(),
            ))
            .await
            .context("记录原生上下文压缩事件失败")
    }
}

enum CachePlan {
    Manual {
        summary_model: Arc<NativeSummaryModel>,
    },
    Automatic {
        summary_model: Arc<NativeSummaryModel>,
        run_id: String,
        provider: String,
        model: String,
        system: Option<String>,
        source_message_count: usize,
        /// 本轮原始消息的稳定摘要；未达到压缩水位且没有旧缓存时延迟计算。
        source_messages_digest: Option<[u8; 32]>,
        /// 无缓存透传路径已经计算出的原始 token 估算，避免压缩阶段再次序列化。
        estimated_tokens: Option<usize>,
        /// 本轮允许沿用的微压缩清理边界下限。
        micro_floor: usize,
        cache_hit: bool,
    },
}

impl CachePlan {
    /// 返回本轮固定使用的路由与窗口快照。
    fn summary_model(&self) -> &Arc<NativeSummaryModel> {
        match self {
            Self::Manual { summary_model } | Self::Automatic { summary_model, .. } => summary_model,
        }
    }

    /// 返回本轮微压缩可以沿用的清理边界；手动压缩不继承任何边界。
    fn micro_floor(&self) -> usize {
        match self {
            Self::Manual { .. } => 0,
            Self::Automatic { micro_floor, .. } => *micro_floor,
        }
    }

    /// 返回可直接传给压缩决策的原始 token 估算；缓存命中时必须重新估算有效上下文。
    fn estimated_tokens(&self) -> Option<usize> {
        match self {
            Self::Manual { .. } => None,
            Self::Automatic {
                estimated_tokens, ..
            } => *estimated_tokens,
        }
    }

    /// 判断本轮是否仍然使用原始请求消息；缓存命中后的上下文不能直接复用原始值。
    fn can_reuse_original_messages(&self) -> bool {
        matches!(
            self,
            Self::Automatic {
                cache_hit: false,
                ..
            }
        )
    }
}

#[async_trait]
impl ContextLoader for NativeContextLoader {
    /// 根据估算 token 水位透传、微压缩或完整压缩上下文。
    ///
    /// 手动请求跳过自动水位并清空增量缓存；摘要失败或事件持久化失败时关闭本轮模型请求。
    async fn load(&self, request: CoreContextLoadRequest) -> Result<CoreLoadedContext> {
        let (original, encoded) = encode_core_request(request)?;
        let (effective, plan) = self.prepare_request(encoded)?;
        let summary_model = plan.summary_model();
        let budget = compaction_budget(
            summary_model.context_window,
            summary_model.max_summary_tokens as usize,
        );
        // 事件只需要 run_id 与 step。先取出这两个字段，就不必为了 `emit_event`
        // 而深度克隆整份上下文——压缩结果为压缩后形态，与该请求已无引用关系。
        let run_id = effective.run_id.clone();
        let step = effective.step;
        let manual = effective.user_initiated;
        let reuse_original_messages = plan.can_reuse_original_messages();
        let outcome = compress_context_with_tokens(
            effective,
            manual,
            budget,
            summary_model.as_ref(),
            plan.micro_floor(),
            plan.estimated_tokens(),
        )
        .await?;
        self.commit_cache(plan, &outcome)?;
        self.emit_event(&run_id, step, outcome.event.as_ref())
            .await?;
        if reuse_original_messages && outcome.event.is_none() {
            return Ok(CoreLoadedContext {
                system: original.system,
                messages: original.messages,
            });
        }
        decode_core_context(outcome.context)
    }
}

/// 一次压缩计算的上下文和可选展示事件。
struct CompressionOutcome {
    context: LoadedContext,
    event: Option<Value>,
    /// 本轮生效的微压缩清理边界，作为下一轮的下限延续。
    ///
    /// 完整压缩会用摘要替换历史，工具结果计数随之重置，因此该值归零。
    micro_cleared: usize,
}

/// 完整压缩生成的替换消息、摘要范围和模型用量。
struct CompactedMessages {
    messages: Vec<Value>,
    summarized_messages: usize,
    /// 保留区内被折叠为首尾片段的超长工具结果数量。
    capped_results: usize,
    summary_usage: Option<Value>,
}

/// 完整摘要前可直接应用的确定性工具结果收缩候选。
struct MicroCompaction {
    /// 没有任何可改写内容时保持 `None`，避免为原始消息深拷贝一份无变化的 JSON 树。
    messages: Option<Vec<Value>>,
    cleared_results: usize,
    capped_results: usize,
    clear_target: usize,
    estimated_tokens: usize,
}

impl MicroCompaction {
    /// 是否实际改写了至少一条工具结果。
    fn changed(&self) -> bool {
        self.cleared_results > 0 || self.capped_results > 0
    }
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
    ///
    /// `route` 是本次调用的会话路由标识，实现据此保持服务商侧的会话亲和。
    async fn summarize(
        &self,
        route: &str,
        messages: &[Value],
        max_tokens: u32,
    ) -> Result<SummaryResponse>;
}

/// 按模型窗口、当前输入规模和手动请求选择分层压缩策略。
///
/// `micro_floor` 是上一轮已生效的微压缩清理边界；本轮只在积压足够时才向后推进，
/// 使多数轮次的上下文前缀保持稳定。
#[cfg(test)]
async fn compress_context(
    request: ContextLoadRequest,
    manual: bool,
    budget: CompactionBudget,
    summarizer: &dyn ContextSummarizer,
    micro_floor: usize,
) -> Result<CompressionOutcome> {
    compress_context_with_tokens(request, manual, budget, summarizer, micro_floor, None).await
}

/// 使用调用方已经完成的 token 估算执行压缩，避免透传路径重复序列化消息。
async fn compress_context_with_tokens(
    request: ContextLoadRequest,
    manual: bool,
    budget: CompactionBudget,
    summarizer: &dyn ContextSummarizer,
    micro_floor: usize,
    estimated_tokens: Option<usize>,
) -> Result<CompressionOutcome> {
    let before_messages = request.messages.len();
    let before_tokens = estimated_tokens
        .unwrap_or_else(|| estimate_context_tokens(request.system.as_deref(), &request.messages));
    let CompactionBudget {
        compact: compact_threshold,
        micro: micro_threshold,
        recent_target,
        summary_max_tokens,
    } = budget;

    // 先执行无模型成本的工具结果收缩。若它已经把上下文拉回完整压缩水位以下，
    // 本轮无需让摘要模型再次读取和概括大量工具正文。
    let micro = if manual || before_tokens >= micro_threshold {
        let clear_target = micro_clear_target(&request.messages, micro_floor, manual);
        let (messages, cleared_results) =
            micro_compact_tool_results(&request.messages, clear_target);
        let (messages, capped_results) = if let Some(messages) = messages {
            let (messages, capped_results) =
                cap_tool_results(&messages, MAX_RETAINED_TOOL_RESULT_CHARS);
            (Some(messages), capped_results)
        } else if has_oversized_tool_result(&request.messages, MAX_RETAINED_TOOL_RESULT_CHARS) {
            let (messages, capped_results) =
                cap_tool_results(&request.messages, MAX_RETAINED_TOOL_RESULT_CHARS);
            (Some(messages), capped_results)
        } else {
            (None, 0)
        };
        let estimated_tokens = messages.as_deref().map_or(before_tokens, |messages| {
            estimate_context_tokens(request.system.as_deref(), messages)
        });
        Some(MicroCompaction {
            messages,
            cleared_results,
            capped_results,
            clear_target,
            estimated_tokens,
        })
    } else {
        None
    };

    let needs_full_compaction = manual
        || (before_tokens >= compact_threshold
            && micro
                .as_ref()
                .is_none_or(|micro| micro.estimated_tokens >= compact_threshold));

    if needs_full_compaction {
        if let Some(compacted) = compact_messages(
            &request.messages,
            &request.run_id,
            manual,
            recent_target,
            summary_max_tokens,
            summarizer,
        )
        .await?
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
                        "summary_max_output_tokens": summary_max_tokens
                    },
                    "presentation": {
                        "target": "main_event_list",
                        "variant": "divider",
                        "tone": "info",
                        "text": "上下文压缩"
                    }
                })),
                // 历史已被摘要替换，工具结果重新计数。
                micro_cleared: 0,
            });
        }
    }

    if let Some(micro) = micro {
        if micro.changed() {
            return Ok(CompressionOutcome {
                context: LoadedContext {
                    system: request.system,
                    messages: micro
                        .messages
                        .expect("微压缩发生变化时必须保留改写后的消息"),
                },
                event: Some(json!({
                    "name": "context.micro_compaction.completed",
                    "data": {
                        "run_id": request.run_id,
                        "step": request.step,
                        "before_messages": before_messages,
                        "after_messages": before_messages,
                        "cleared_tool_results": micro.cleared_results,
                        "capped_tool_results": micro.capped_results,
                        "estimated_tokens_before": before_tokens,
                        "estimated_tokens_after": micro.estimated_tokens,
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "reduce_tool_results_before_model_summary"
                    },
                    "presentation": { "target": "none" }
                })),
                micro_cleared: micro.clear_target,
            });
        }
        // 本轮没有推进边界：保持上下文原样，让前缀继续命中缓存。
        // 手动压缩仍要落到下面的"无可压缩历史"说明，不在这里收尾。
        if !manual {
            return Ok(CompressionOutcome {
                context: LoadedContext {
                    system: request.system,
                    messages: micro.messages.unwrap_or(request.messages),
                },
                event: None,
                micro_cleared: micro.clear_target,
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
            micro_cleared: 0,
        });
    }

    Ok(CompressionOutcome {
        context: LoadedContext {
            system: request.system,
            messages: request.messages,
        },
        event: None,
        micro_cleared: micro_floor,
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
    /// 本轮摘要模型实际允许的最大输出 token 数。
    summary_max_tokens: u32,
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
    let compact = percentage_of(window, COMPACT_THRESHOLD_RATIO).max(1);
    let summary_reserve = summary_tokens.min(compact / 2);
    let recent_target = percentage_of(window, RECENT_CONTEXT_TARGET_RATIO)
        .min(compact.saturating_sub(summary_reserve))
        .max(1);
    CompactionBudget {
        compact,
        micro: compact,
        recent_target,
        summary_max_tokens: u32::try_from(summary_tokens).unwrap_or(u32::MAX).max(1),
    }
}

/// 将配置与元数据中的窗口转换为当前平台可表达的正数 token 数。
fn normalize_context_window(context_window: Option<u64>) -> Option<usize> {
    context_window
        .filter(|window| *window > 0)
        .map(|window| usize::try_from(window).unwrap_or(usize::MAX))
}

/// 计算不超过百分之百的整数比例，并在极大上下文窗口下避免乘法溢出。
fn percentage_of(value: usize, percentage: usize) -> usize {
    value / 100 * percentage + value % 100 * percentage / 100
}

/// 把序列化消息直接写入 SHA-256，避免为大历史额外分配完整 JSON 字符串。
struct DigestWriter<'a>(&'a mut Sha256);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 把一段消息追加进进行中的 SHA-256 状态；零字节分隔符用于保留消息边界。
fn hash_messages_into(digest: &mut Sha256, messages: &[Value]) -> Result<()> {
    for message in messages {
        serde_json::to_writer(DigestWriter(digest), message)
            .context("计算原生上下文消息摘要失败")?;
        digest.update([0]);
    }
    Ok(())
}

/// 计算消息序列的稳定摘要；零字节分隔符用于保留消息边界。
#[cfg(test)]
fn message_digest(messages: &[Value]) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    hash_messages_into(&mut digest, messages)?;
    Ok(digest.finalize().into())
}

/// 直接统计序列化字节的 ASCII / 非 ASCII 分布，避免为每条消息额外分配完整 JSON 字符串。
///
/// 折算比例与 [`estimate_text_tokens`] 一致，因此结果与先 `to_string` 再折算完全相同。
#[derive(Default)]
struct TokenCountingWriter {
    /// ASCII 字节数，按四字节一个 token 折算。
    ascii_bytes: usize,
    /// 非 ASCII 字节数，按三字节一个 token 折算。
    non_ascii_bytes: usize,
}

impl TokenCountingWriter {
    /// 按 ASCII 四字节、非 ASCII 三字节的比例折算 token 数。
    fn tokens(&self) -> usize {
        self.ascii_bytes.div_ceil(4) + self.non_ascii_bytes.div_ceil(3)
    }
}

/// 单次消息序列化同时产出的摘要和 token 估算。
#[derive(Debug, Clone, Copy)]
struct MessageMetrics {
    digest: [u8; 32],
    tokens: usize,
}

/// 把序列化字节同时写入摘要器并统计 ASCII / 非 ASCII 字节数。
struct TokenDigestWriter<'a> {
    digest: &'a mut Sha256,
    ascii_bytes: usize,
    non_ascii_bytes: usize,
}

impl TokenDigestWriter<'_> {
    /// 返回当前消息的粗略 token 数。
    fn tokens(&self) -> usize {
        self.ascii_bytes.div_ceil(4) + self.non_ascii_bytes.div_ceil(3)
    }
}

impl Write for TokenDigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.digest.update(buffer);
        let non_ascii_bytes = buffer.iter().filter(|byte| !byte.is_ascii()).count();
        self.non_ascii_bytes += non_ascii_bytes;
        self.ascii_bytes += buffer.len() - non_ascii_bytes;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 单次遍历消息，取得完整摘要和 token 估算，避免两次序列化同一批 JSON。
fn message_metrics(messages: &[Value]) -> Result<MessageMetrics> {
    let mut digest = Sha256::new();
    let mut tokens = 0usize;
    for message in messages {
        let mut writer = TokenDigestWriter {
            digest: &mut digest,
            ascii_bytes: 0,
            non_ascii_bytes: 0,
        };
        serde_json::to_writer(&mut writer, message).context("计算上下文消息指标失败")?;
        tokens = tokens.saturating_add(writer.tokens());
        digest.update([0]);
    }
    Ok(MessageMetrics {
        digest: digest.finalize().into(),
        tokens,
    })
}

impl Write for TokenCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let non_ascii_bytes = buffer.iter().filter(|byte| !byte.is_ascii()).count();
        self.non_ascii_bytes += non_ascii_bytes;
        self.ascii_bytes += buffer.len() - non_ascii_bytes;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 估算单条 JSON 消息的 token 数，等价于序列化后按字节折算，但不分配字符串。
fn estimate_value_tokens(value: &Value) -> usize {
    let mut writer = TokenCountingWriter::default();
    // `Value` 不包含无法表达的数（如 NaN），序列化实际不会失败；回退分支只为保持
    // 与旧实现一致的"总能给出估算"语义，不改变返回值。
    if serde_json::to_writer(&mut writer, value).is_err() {
        return estimate_text_tokens(&value.to_string());
    }
    writer.tokens()
}

/// 用序列化字节数进行估算，ASCII 与非 ASCII 分别计费。
fn estimate_context_tokens(system: Option<&str>, messages: &[Value]) -> usize {
    let system_tokens = system.map_or(0, estimate_text_tokens);
    system_tokens + messages.iter().map(estimate_value_tokens).sum::<usize>()
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

/// 推导本轮生效的微压缩清理边界，即从头数应处于清空态的成功工具结果条数。
///
/// 首次清理（`floor` 为零）直接清到位：此时不清理就省不下 token，而前缀本来就要因这次
/// 改写重建一次。之后边界只在积压满一批时才前移——每次前移都会改写历史中间的消息，让
/// 服务端提示缓存从改写点断开，因此稳定不动的轮次越多越省。`manual` 请求要求立刻见效，
/// 同样直接取满。`floor` 超过当前可清理量时说明历史已被完整压缩替换，按当前量收敛。
fn micro_clear_target(messages: &[Value], floor: usize, manual: bool) -> usize {
    let clearable = messages
        .iter()
        .map(compactable_tool_result_count)
        .sum::<usize>()
        .saturating_sub(RECENT_TOOL_RESULTS_TO_KEEP);
    let floor = floor.min(clearable);
    if manual || floor == 0 || clearable >= floor.saturating_add(MICRO_COMPACT_CLEAR_BATCH) {
        clearable
    } else {
        floor
    }
}

/// 清理较旧的成功工具结果正文，同时保留调用关联、错误结果和最近成功结果。
///
/// `clear_target` 是从头计数、应处于清空态的成功工具结果条数，由
/// [`micro_clear_target`] 推导；返回值中的计数只统计本轮新清空的条数，已是占位符的
/// 结果不重复计入。
fn micro_compact_tool_results(
    messages: &[Value],
    clear_target: usize,
) -> (Option<Vec<Value>>, usize) {
    let mut remaining_to_clear = clear_target;
    if remaining_to_clear == 0 {
        return (None, 0);
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
    (Some(compacted), cleared)
}

/// 判断是否存在需要截断的工具结果；没有候选时避免复制整段消息树。
fn has_oversized_tool_result(messages: &[Value], max_chars: usize) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        return false;
                    }
                    let Some(text) = block.pointer("/result/content").and_then(Value::as_str)
                    else {
                        return false;
                    };
                    text != CLEARED_TOOL_RESULT
                        && text.len() > max_chars
                        && text.chars().count() > max_chars
                })
            })
    })
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
        "{}\n… [{omitted} characters elided] …\n{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>(),
    ))
}

/// 折叠指定消息内超长的工具结果正文，返回新消息与被折叠的结果数。
///
/// 该处理同时用于完整摘要的旧前缀和最终保留的近期尾部：旧前缀需要给摘要模型留下
/// 错误、路径和最终状态证据，近期尾部则要保持可继续执行，但两者都不应携带巨型原文。
fn cap_tool_results(messages: &[Value], max_chars: usize) -> (Vec<Value>, usize) {
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
    route: &str,
    manual: bool,
    recent_target: usize,
    summary_max_tokens: u32,
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

    // 摘要模型只读取折叠后的工具证据，避免冷启动或单次巨型输出直接撑爆摘要请求。
    let (summary_input, summarized_capped_results) =
        cap_tool_results(&messages[..split_index], MAX_RETAINED_TOOL_RESULT_CHARS);
    let response = summarizer
        .summarize(route, &summary_input, summary_max_tokens)
        .await?;
    let summary = normalize_model_summary(&response.text)?;
    // 保留区仍是原文，单条超长结果会让尾部继续占据数万 token；这里只封顶不清空。
    let (retained, retained_capped_results) =
        cap_tool_results(&messages[split_index..], MAX_RETAINED_TOOL_RESULT_CHARS);
    let mut compacted = Vec::with_capacity(retained.len() + 1);
    compacted.push(json!({
        "role": "developer",
        "content": [{
            "type": "text",
            "text": format!(
                "The earlier part of this session was compacted into the structured summary below. \
                 Treat it as the record of everything that happened before; the most recent messages \
                 follow verbatim after it. Continue the work from this state.\n\n{summary}"
            )
        }]
    }));
    compacted.extend(retained);
    Ok(Some(CompactedMessages {
        messages: compacted,
        summarized_messages: split_index,
        capped_results: summarized_capped_results.saturating_add(retained_capped_results),
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

/// 自动模式保留窗口约 30% 的近期尾部；手动模式只保留最新完整轮次以确保实际发生压缩。
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
impl ContextSummarizer for NativeSummaryModel {
    /// 使用应用固定的 provider/model 生成摘要；调用强制禁用工具、推理和 provider 逃生参数。
    ///
    /// 摘要是一次性请求：它的前缀在压缩完成后就不复存在，写进去的缓存条目永远读不到，
    /// 因此显式关闭缓存保留，只保留会话路由标识让请求落在同一台机器上。
    async fn summarize(
        &self,
        route: &str,
        messages: &[Value],
        max_tokens: u32,
    ) -> Result<SummaryResponse> {
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
        request.cache_retention = PromptCacheRetention::None;
        request.session_id = Some(route.to_string());
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
    use agent_core::model::{ChatModel, ModelEventStream, ModelResponse, ProviderAdapter};

    /// 记录收到的模型请求并返回固定摘要的测试 provider。
    struct CapturingSummaryModel(Arc<Mutex<Vec<ModelRequest>>>);

    #[async_trait]
    impl ChatModel for CapturingSummaryModel {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            self.0.lock().expect("请求记录锁不应损坏").push(request);
            Ok(ModelResponse::text("<summary>受控摘要</summary>"))
        }

        async fn stream(&self, request: ModelRequest) -> ModelEventStream {
            self.0.lock().expect("请求记录锁不应损坏").push(request);
            let (sender, stream) = ModelEventStream::channel();
            sender.done(ModelResponse::text("<summary>受控摘要</summary>"));
            stream
        }
    }

    impl ProviderAdapter for CapturingSummaryModel {
        fn name(&self) -> &'static str {
            "capturing-summary"
        }
    }

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
            _route: &str,
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

    /// 使用确定性摘要器执行一次测试压缩；清理边界从零开始。
    async fn compress_for_test(request: ContextLoadRequest, manual: bool) -> CompressionOutcome {
        compress_context(request, manual, test_budget(), &TestSummarizer, 0)
            .await
            .expect("测试压缩应成功")
    }

    /// 分段续算的摘要必须与一次性计算整段历史完全一致。
    ///
    /// 缓存命中判定先对前缀取摘要，再决定是否续算全量摘要；两段拼接的结果必须与
    /// `message_digest` 逐位相同，否则缓存会在命中时被误判为失效。
    #[test]
    fn incremental_digest_matches_single_pass() {
        let messages = vec![
            text_message("user", "第一段"),
            text_message("assistant", "mixed 中文 ascii"),
            json!({ "role": "tool", "content": "x".repeat(1024) }),
            text_message("user", "queued"),
        ];
        let expected = message_digest(&messages).expect("整段摘要应成功");
        for split in 0..=messages.len() {
            let mut hasher = Sha256::new();
            hash_messages_into(&mut hasher, &messages[..split]).expect("前缀摘要应成功");
            hash_messages_into(&mut hasher, &messages[split..]).expect("后缀摘要应成功");
            assert_eq!(
                <[u8; 32]>::from(hasher.finalize()),
                expected,
                "切分位置 {split} 的续算摘要与整段摘要不一致"
            );
        }
    }

    /// 不分配字符串的字节统计必须与先 `to_string` 再折算的结果逐位一致。
    ///
    /// 估算口径直接决定何时触发压缩，任何偏差都会改变会话行为，因此用混合 ASCII、
    /// 中日韩文字、转义字符和数字的消息覆盖，防止只对纯 ASCII 成立。
    #[test]
    fn token_estimation_matches_string_round_trip() {
        let messages = vec![
            text_message("user", "plain ascii only"),
            text_message("user", "中文与日本語の混在文本"),
            text_message("assistant", "quote \" backslash \\ newline \n tab \t"),
            json!({ "role": "tool", "content": json!({ "value": 12345, "flag": true, "nil": null }) }),
            json!({ "role": "tool", "content": "x".repeat(4096) }),
        ];
        for message in &messages {
            assert_eq!(
                estimate_value_tokens(message),
                estimate_text_tokens(&message.to_string()),
                "单条消息估算必须与字符串折算一致: {message}"
            );
        }
        for split in 0..messages.len() {
            let expected =
                estimate_text_tokens(&Value::Array(messages[..split].to_vec()).to_string());
            let mut writer = TokenCountingWriter::default();
            serde_json::to_writer(&mut writer, &Value::Array(messages[..split].to_vec()))
                .expect("序列化测试消息应成功");
            assert_eq!(writer.tokens(), expected, "数组前缀长度 {split} 估算不一致");
        }
        let expected_total = messages
            .iter()
            .map(|message| estimate_text_tokens(&message.to_string()))
            .sum::<usize>();
        assert_eq!(
            estimate_context_tokens(None, &messages),
            expected_total,
            "整体估算必须等于逐条字符串折算之和"
        );
        let metrics = message_metrics(&messages).expect("消息指标应可一次计算");
        assert_eq!(metrics.tokens, expected_total);
        assert_eq!(
            metrics.digest,
            message_digest(&messages).expect("摘要应可重复计算")
        );
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

    /// 首次清理后，边界必须停在原处直到积压满一批，否则每轮都会改写历史、打断提示缓存。
    #[tokio::test]
    async fn micro_compaction_holds_boundary_until_batch_is_reached() {
        let size = chars_to_exceed(test_budget().micro, 6);
        let messages = (0..6)
            .map(|index| tool_result_message(index, size))
            .collect::<Vec<_>>();
        let loader = NativeContextLoader::new(
            ModelGateway::new(),
            "test",
            "test-model",
            false,
            DEFAULT_MAX_SUMMARY_TOKENS,
            Arc::new(agent_core::InMemoryEventSink::new()),
        )
        .expect("应创建原生上下文加载器");
        let request = |messages: Vec<Value>| ContextLoadRequest {
            run_id: "micro-hold".into(),
            step: 0,
            user_initiated: false,
            provider: "test".into(),
            model: "test-model".into(),
            system: None,
            messages,
        };

        let (prepared, plan) = loader
            .prepare_request(request(messages.clone()))
            .expect("应准备首轮微压缩请求");
        let first = compress_context(
            prepared,
            false,
            test_budget(),
            &TestSummarizer,
            plan.micro_floor(),
        )
        .await
        .expect("首轮微压缩应成功");
        loader.commit_cache(plan, &first).expect("应提交首轮边界");
        // 六条结果保留最近三条，首次直接清到位。
        assert_eq!(first.micro_cleared, 3);

        // 追加一条新结果：积压只有一条，远不到一批，边界必须原地不动。
        let mut extended = messages;
        extended.push(tool_result_message(6, size));
        let (prepared, plan) = loader
            .prepare_request(request(extended.clone()))
            .expect("应准备次轮微压缩请求");
        let second = compress_context(
            prepared,
            false,
            test_budget(),
            &TestSummarizer,
            plan.micro_floor(),
        )
        .await
        .expect("次轮微压缩应成功");

        assert_eq!(second.micro_cleared, 3);
        assert!(second.event.is_none(), "边界未推进时不应发布微压缩事件");
        // 前缀逐字节不变，新消息只在末尾追加，服务端缓存才能继续命中。
        let mut expected = first.context.messages;
        expected.push(tool_result_message(6, size));
        assert_eq!(second.context.messages, expected);
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
            tool_result_message(0, MAX_RETAINED_TOOL_RESULT_CHARS * 4),
            text_message(
                "user",
                "x".repeat(chars_to_exceed(test_budget().compact, 1)),
            ),
            text_message("assistant", "已完成旧历史分析"),
            text_message("user", recent_request),
            text_message("assistant", "正在处理最新用例"),
        ];
        struct RecordingSummarizer(Mutex<Vec<Value>>);

        #[async_trait]
        impl ContextSummarizer for RecordingSummarizer {
            async fn summarize(
                &self,
                route: &str,
                messages: &[Value],
                max_tokens: u32,
            ) -> Result<SummaryResponse> {
                *self.0.lock().expect("记录锁不应损坏") = messages.to_vec();
                TestSummarizer.summarize(route, messages, max_tokens).await
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
            0,
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
        let summarized = summarizer.0.lock().expect("记录锁不应损坏");
        assert!(summarized
            .iter()
            .any(|message| message.to_string().contains("读取配置失败：权限不足")));
        assert!(summarized
            .iter()
            .filter_map(|message| message.pointer("/content/0/result/content"))
            .filter_map(Value::as_str)
            .any(|content| content.contains("characters elided")));
        drop(summarized);
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

    /// 工具结果收缩后低于完整水位时，不应再调用昂贵的模型摘要。
    #[tokio::test]
    async fn tool_result_reduction_can_avoid_full_compaction() {
        struct RejectingSummarizer;

        #[async_trait]
        impl ContextSummarizer for RejectingSummarizer {
            async fn summarize(
                &self,
                _route: &str,
                _messages: &[Value],
                _max_tokens: u32,
            ) -> Result<SummaryResponse> {
                Err(anyhow!("工具结果收缩后不应调用摘要模型"))
            }
        }

        let size = chars_to_exceed(test_budget().compact, 6);
        let mut messages = vec![
            text_message("user", "执行一组工具调用"),
            text_message("assistant", "开始执行"),
        ];
        messages.extend((0..6).map(|index| tool_result_message(index, size)));
        messages.push(text_message("assistant", "工具调用已完成"));
        messages.push(text_message("user", "继续处理结果"));
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "reduce-before-summary".into(),
                user_initiated: false,
                step: 1,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages,
            },
            false,
            test_budget(),
            &RejectingSummarizer,
            0,
        )
        .await
        .expect("工具结果收缩应避免完整摘要");

        let event = outcome.event.expect("应发布工具结果收缩事件");
        assert_eq!(event["name"], "context.micro_compaction.completed");
        assert!(event["data"]["estimated_tokens_after"]
            .as_u64()
            .is_some_and(|tokens| tokens < test_budget().compact as u64));
        assert_eq!(event["data"]["cleared_tool_results"], 3);
        assert_eq!(event["data"]["capped_tool_results"], 3);
    }

    /// 没有可清理的旧结果时，单条超长结果仍应走截断路径而不是复制原始消息。
    #[tokio::test]
    async fn oversized_tool_result_is_capped_without_clearable_results() {
        let outcome = compress_for_test(
            ContextLoadRequest {
                run_id: "cap-only".into(),
                user_initiated: false,
                step: 1,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: vec![tool_result_message(
                    0,
                    chars_to_exceed(test_budget().micro, 1),
                )],
            },
            false,
        )
        .await;

        let event = outcome.event.expect("超长工具结果应发布微压缩事件");
        assert_eq!(event["name"], "context.micro_compaction.completed");
        assert_eq!(event["data"]["cleared_tool_results"], 0);
        assert_eq!(event["data"]["capped_tool_results"], 1);
        let content = outcome.context.messages[0]["content"][0]["result"]["content"]
            .as_str()
            .expect("截断后的工具结果应为文本");
        assert!(content.chars().count() <= MAX_RETAINED_TOOL_RESULT_CHARS * 2);
    }

    /// 水位必须随真实上下文窗口伸缩，而不是按模型名猜测。
    #[test]
    fn budget_scales_with_real_context_window() {
        let summary = DEFAULT_MAX_SUMMARY_TOKENS as usize;
        let small = compaction_budget(Some(200_000), summary);
        let large = compaction_budget(Some(1_000_000), summary);

        assert_eq!(small.compact, 160_000);
        assert_eq!(large.compact, 800_000);
        assert_eq!(small.micro, small.compact);
        assert_eq!(large.micro, large.compact);
        assert!(large.recent_target > small.recent_target);

        // 未声明窗口时按 200k 假定值兜底，与显式传入 200k 等价。
        assert_eq!(compaction_budget(None, summary), small);
        assert_eq!(compaction_budget(Some(0), summary), small);
    }

    /// 小窗口也按 80% 触发压缩，不受固定摘要输出上限影响。
    #[test]
    fn small_window_budget_uses_same_window_ratio() {
        let budget = compaction_budget(Some(32_000), DEFAULT_MAX_SUMMARY_TOKENS as usize);

        assert_eq!(budget.compact, 25_600);
        assert!(budget.compact > 0);
        assert!(budget.micro > 0);
        assert!(budget.recent_target > 0);
    }

    /// 极大窗口的比例预算不得因中间乘法溢出而回绕或 panic。
    #[test]
    fn maximum_window_budget_avoids_percentage_overflow() {
        let budget = compaction_budget(Some(usize::MAX), DEFAULT_MAX_SUMMARY_TOKENS as usize);

        assert_eq!(
            budget.recent_target,
            percentage_of(usize::MAX, RECENT_CONTEXT_TARGET_RATIO)
        );
        assert_eq!(
            budget.compact,
            percentage_of(usize::MAX, COMPACT_THRESHOLD_RATIO)
        );
        assert_eq!(budget.micro, budget.compact);
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
        assert!(truncated.contains("characters elided"), "{truncated}");
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
        assert!(retained[0].contains("characters elided"));
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

    /// 热切换必须同时更新摘要路由和窗口，并清除旧路由的压缩缓存。
    #[tokio::test]
    async fn model_route_update_changes_budget_and_invalidates_cache() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gateway = ModelGateway::new();
        gateway
            .register(
                "new-provider",
                Arc::new(CapturingSummaryModel(Arc::clone(&requests))),
            )
            .expect("应注册切换后的摘要 provider");
        let loader = NativeContextLoader::new(
            gateway,
            "old-provider",
            "old-model",
            false,
            DEFAULT_MAX_SUMMARY_TOKENS,
            Arc::new(agent_core::InMemoryEventSink::new()),
        )
        .expect("应创建原生上下文加载器")
        .with_context_window(Some(200_000));
        let messages = vec![
            agent_core::model::ModelMessage::text(
                agent_core::MessageRole::User,
                "x".repeat(110_000),
            ),
            agent_core::model::ModelMessage::text(agent_core::MessageRole::Assistant, "已分析历史"),
            agent_core::model::ModelMessage::text(agent_core::MessageRole::User, "继续处理"),
            agent_core::model::ModelMessage::text(agent_core::MessageRole::Assistant, "正在处理"),
        ];
        let request = |provider: &str, model: &str| CoreContextLoadRequest {
            run_id: "route-update".into(),
            step: 0,
            provider: provider.into(),
            model: model.into(),
            system: None,
            messages: messages.clone(),
            user_initiated: false,
        };

        let original = loader
            .load(request("old-provider", "old-model"))
            .await
            .expect("低于原窗口水位时应保持原文");
        assert_eq!(original.messages, messages);
        assert!(requests.lock().expect("请求记录锁不应损坏").is_empty());

        loader
            .update_model_route("new-provider".into(), "new-model".into(), Some(32_000))
            .expect("应更新模型路由与窗口");
        let compacted = loader
            .load(request("new-provider", "new-model"))
            .await
            .expect("切换到小窗口后应使用新路由压缩");
        assert_ne!(compacted.messages, messages);
        {
            let captured = requests.lock().expect("请求记录锁不应损坏");
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].model, "new-model");
        }
        assert!(loader.state.lock().expect("状态锁不应损坏").cache.is_some());

        loader
            .update_model_route("new-provider".into(), "new-model".into(), Some(200_000))
            .expect("同一路由的窗口更新也应清除缓存");
        let restored = loader
            .load(request("new-provider", "new-model"))
            .await
            .expect("恢复大窗口后不应复用旧压缩结果");
        assert_eq!(restored.messages, messages);
        assert_eq!(requests.lock().expect("请求记录锁不应损坏").len(), 1);
        assert!(loader
            .update_model_route(" ".into(), "invalid".into(), Some(1))
            .is_err());
        assert_eq!(
            loader
                .state
                .lock()
                .expect("状态锁不应损坏")
                .summary_model
                .context_window,
            Some(200_000)
        );
    }

    /// 切换前已经准备的加载沿用同一配置快照，且不能在完成后回填旧缓存。
    #[tokio::test]
    async fn prepared_request_keeps_route_snapshot_and_rejects_stale_cache() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gateway = ModelGateway::new();
        gateway
            .register(
                "old-provider",
                Arc::new(CapturingSummaryModel(Arc::clone(&requests))),
            )
            .expect("应注册原摘要 provider");
        let loader = NativeContextLoader::new(
            gateway,
            "old-provider",
            "old-model",
            false,
            DEFAULT_MAX_SUMMARY_TOKENS,
            Arc::new(agent_core::InMemoryEventSink::new()),
        )
        .expect("应创建原生上下文加载器")
        .with_context_window(Some(32_000));
        let (prepared, plan) = loader
            .prepare_request(ContextLoadRequest {
                run_id: "route-snapshot".into(),
                step: 0,
                provider: "old-provider".into(),
                model: "old-model".into(),
                system: None,
                messages: vec![
                    text_message("user", "x".repeat(110_000)),
                    text_message("assistant", "已分析历史"),
                    text_message("user", "继续处理"),
                    text_message("assistant", "正在处理"),
                ],
                user_initiated: false,
            })
            .expect("应准备原路由加载");
        loader
            .update_model_route("new-provider".into(), "new-model".into(), Some(200_000))
            .expect("应切换到新路由与大窗口");

        let summary_model = plan.summary_model();
        let budget = compaction_budget(
            summary_model.context_window,
            summary_model.max_summary_tokens as usize,
        );
        assert_eq!(budget.compact, 25_600);
        let outcome = compress_context(
            prepared,
            false,
            budget,
            summary_model.as_ref(),
            plan.micro_floor(),
        )
        .await
        .expect("原加载应沿用旧路由和小窗口完成摘要");
        assert!(outcome.event.is_some());
        assert_eq!(
            requests.lock().expect("请求记录锁不应损坏")[0].model,
            "old-model"
        );
        loader
            .commit_cache(plan, &outcome)
            .expect("过期加载应跳过缓存回填");
        let state = loader.state.lock().expect("状态锁不应损坏");
        assert!(state.cache.is_none());
        assert_eq!(state.summary_model.provider, "new-provider");
        assert_eq!(state.summary_model.context_window, Some(200_000));
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
        let first = compress_context(request, false, test_budget(), &TestSummarizer, 0)
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
        let second = compress_context(
            request,
            false,
            test_budget(),
            &TestSummarizer,
            plan.micro_floor(),
        )
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

    /// 同一运行内的历史被修订时必须使缓存失效，不能只凭消息数量沿用旧摘要。
    #[tokio::test]
    async fn incremental_compaction_rejects_revised_source_prefix() {
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
                run_id: "revised-run".into(),
                step: 0,
                user_initiated: false,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: original_messages.clone(),
            })
            .expect("应准备首次压缩请求");
        let first = compress_context(request, false, test_budget(), &TestSummarizer, 0)
            .await
            .expect("首次压缩应成功");
        loader
            .commit_cache(plan, &first)
            .expect("应提交首次压缩缓存");

        let mut revised_messages = original_messages;
        revised_messages[2] = text_message("user", "修正：改为检查缓存一致性");
        revised_messages.push(text_message("assistant", "已收到修正"));
        let (prepared, plan) = loader
            .prepare_request(ContextLoadRequest {
                run_id: "revised-run".into(),
                step: 1,
                user_initiated: false,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: revised_messages.clone(),
            })
            .expect("修订历史后仍应准备请求");

        assert_eq!(prepared.messages, revised_messages);
        assert!(matches!(
            plan,
            CachePlan::Automatic {
                cache_hit: false,
                micro_floor: 0,
                ..
            }
        ));
    }

    /// 自定义摘要上限必须同时约束模型调用并写入压缩事件。
    #[tokio::test]
    async fn compaction_uses_configured_summary_token_limit() {
        struct TokenRecordingSummarizer(Mutex<Vec<u32>>);

        #[async_trait]
        impl ContextSummarizer for TokenRecordingSummarizer {
            async fn summarize(
                &self,
                route: &str,
                messages: &[Value],
                max_tokens: u32,
            ) -> Result<SummaryResponse> {
                self.0
                    .lock()
                    .expect("摘要上限记录锁不应损坏")
                    .push(max_tokens);
                TestSummarizer.summarize(route, messages, max_tokens).await
            }
        }

        let max_summary_tokens = 1_234;
        let budget = compaction_budget(None, max_summary_tokens as usize);
        let summarizer = TokenRecordingSummarizer(Mutex::new(Vec::new()));
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "custom-summary-limit".into(),
                step: 0,
                user_initiated: false,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages: vec![
                    text_message("user", "x".repeat(chars_to_exceed(budget.compact, 1))),
                    text_message("assistant", "已处理旧历史"),
                    text_message("user", "继续处理"),
                ],
            },
            false,
            budget,
            &summarizer,
            0,
        )
        .await
        .expect("自定义摘要上限的压缩应成功");

        assert_eq!(
            summarizer
                .0
                .lock()
                .expect("摘要上限记录锁不应损坏")
                .as_slice(),
            &[max_summary_tokens]
        );
        assert_eq!(
            outcome.event.expect("应发布压缩事件")["data"]["summary_max_output_tokens"],
            max_summary_tokens
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
        let outcome = compress_context(request, true, test_budget(), &TestSummarizer, 0)
            .await
            .expect("主动压缩应立即返回结果");

        assert_eq!(outcome.context.messages.len(), 3);
        assert!(outcome.context.messages[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("compacted into the structured summary")));
        assert_eq!(
            outcome.event.expect("应发布压缩事件")["data"]["trigger"],
            "manual"
        );
    }

    /// 摘要是一次性请求：必须关闭缓存保留，同时保留会话路由标识。
    ///
    /// 摘要的前缀在压缩完成后就不存在了，写进去的缓存条目永远读不到；继续按主会话策略
    /// 写缓存只会占用并驱逐主会话自己的缓存分区。路由标识仍要发送，使同一次运行里的
    /// 摘要请求落到同一台机器上。
    #[tokio::test]
    async fn summary_request_disables_cache_and_keeps_session_route() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gateway = ModelGateway::new();
        gateway
            .register(
                "test",
                Arc::new(CapturingSummaryModel(Arc::clone(&requests))),
            )
            .expect("应注册测试 provider");
        let loader = NativeContextLoader::new(
            gateway,
            "test",
            "test-model",
            false,
            DEFAULT_MAX_SUMMARY_TOKENS,
            Arc::new(agent_core::InMemoryEventSink::new()),
        )
        .expect("应创建原生上下文加载器");
        let history = "历史工具输出".repeat(90_000);
        let messages = vec![
            agent_core::model::ModelMessage::text(agent_core::MessageRole::User, history),
            agent_core::model::ModelMessage::text(agent_core::MessageRole::Assistant, "已分析历史"),
            agent_core::model::ModelMessage::text(agent_core::MessageRole::User, "继续处理"),
            agent_core::model::ModelMessage::text(agent_core::MessageRole::Assistant, "正在处理"),
        ];

        loader
            .load(CoreContextLoadRequest {
                run_id: "prefix-run".into(),
                step: 0,
                provider: "test".into(),
                model: "test-model".into(),
                system: Some("主 Agent 系统提示".into()),
                messages,
                user_initiated: false,
            })
            .await
            .expect("超过水位时应执行完整压缩");

        let captured = requests.lock().expect("请求记录锁不应损坏");
        let request = captured.first().expect("应发出一次摘要请求");
        assert_eq!(request.cache_retention, PromptCacheRetention::None);
        assert_eq!(request.session_id.as_deref(), Some("prefix-run"));
        // 摘要不携带主 Agent 的人格与工具，避免模型把压缩当成继续执行任务。
        assert_eq!(request.system.as_deref(), Some(SUMMARY_SYSTEM_PROMPT));
        assert!(request.tools.is_empty());
        assert_eq!(request.tool_choice, ToolChoice::None);
    }
}
