//! 基于 Claude Code 分层策略的 Lucia 上下文压缩插件。

use agent_plugin::{
    export_plugin, AgentPlugin, ContextLoadRequest, EventPresentation, EventPresentationTone,
    ExtensionEvent, LoadedContext, ModelCompletionRequest, ModelCompletionResponse, PluginHostApi,
    Result,
};
use anyhow::{anyhow, Context};
use serde_json::{json, Value};

/// 200k 上下文扣除 20k 摘要输出预算和 13k 下一轮缓冲后的自动压缩阈值。
const DEFAULT_COMPACT_THRESHOLD_TOKENS: usize = 167_000;
/// 百万上下文使用与 Claude Code 相同的摘要输出和下一轮缓冲预留。
const LARGE_COMPACT_THRESHOLD_TOKENS: usize = 967_000;
/// 在完整压缩前先清理旧工具结果，降低无效历史占用。
const DEFAULT_MICRO_COMPACT_THRESHOLD_TOKENS: usize = 120_000;
/// 百万上下文的微压缩水位。
const LARGE_MICRO_COMPACT_THRESHOLD_TOKENS: usize = 900_000;
/// 完整压缩后至少保留的近期上下文目标。
const RECENT_CONTEXT_TARGET_TOKENS: usize = 40_000;
/// 微压缩始终原样保留的最近工具结果数量。
const RECENT_TOOL_RESULTS_TO_KEEP: usize = 3;
/// Claude Code 为完整摘要预留的最大输出 token 数。
const SUMMARY_MAX_OUTPUT_TOKENS: u32 = 20_000;
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

/// 提供分层上下文压缩能力的插件。
#[derive(Default)]
struct ContextPlugin {
    /// 最近一次自动压缩结果，用于增量追加后续消息，避免重复处理同一历史前缀。
    cache: Option<CompressionCache>,
}

/// 最近一次自动压缩的追加边界与有效模型上下文。
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

impl ContextPlugin {
    /// 复用已压缩历史，只把当前请求新增的消息追加到有效上下文后再判断水位。
    fn compress_incrementally(
        &mut self,
        request: ContextLoadRequest,
        summarize: &dyn Fn(&[Value]) -> Result<ModelCompletionResponse>,
    ) -> Result<CompressionOutcome> {
        let cache_hit = self.cache.as_ref().is_some_and(|cache| {
            // Agent 在单次 run 中只会向 Session 追加消息；避免比较超大 JSON 前缀，
            // 否则微压缩后的下一轮会在 Guest 内耗尽 fuel。
            cache.run_id == request.run_id
                && cache.provider == request.provider
                && cache.model == request.model
                && cache.system == request.system
                && request.messages.len() >= cache.source_message_count
        });
        let source_message_count = request.messages.len();
        let run_id = request.run_id.clone();
        let provider = request.provider.clone();
        let model = request.model.clone();
        let system = request.system.clone();
        let effective_request = if cache_hit {
            let cache = self.cache.as_ref().expect("命中缓存时必须存在缓存内容");
            let mut messages = cache.loaded_messages.clone();
            messages.extend_from_slice(&request.messages[cache.source_message_count..]);
            ContextLoadRequest {
                messages,
                ..request
            }
        } else {
            request
        };
        let outcome = compress_context(effective_request, false, summarize)?;

        if cache_hit || outcome.event.is_some() {
            self.cache = Some(CompressionCache {
                run_id,
                provider,
                model,
                system,
                source_message_count,
                loaded_messages: outcome.context.messages.clone(),
            });
        } else {
            self.cache = None;
        }
        Ok(outcome)
    }
}

impl AgentPlugin for ContextPlugin {
    /// 释放仅服务于运行期请求的压缩缓存。
    fn deactivate(&mut self, _host: &dyn PluginHostApi) -> Result<()> {
        self.cache = None;
        Ok(())
    }

    /// 根据估算 token 水位透传、微压缩或完整压缩上下文，并发布对应事件。
    ///
    /// 用户显式发起的加载（`user_initiated`）跳过增量缓存与水位判断，
    /// 无条件执行完整压缩策略并始终返回替换上下文。
    fn load_context(
        &mut self,
        host: &dyn PluginHostApi,
        request: ContextLoadRequest,
    ) -> Result<Option<LoadedContext>> {
        if request.user_initiated {
            self.cache = None;
            let summarize = |messages: &[Value]| summarize_with_model(host, messages);
            let outcome = compress_context(request, true, &summarize)?;
            if let Some(event) = outcome.event {
                host.emit_event(&event)?;
            }
            return Ok(Some(outcome.context));
        }
        let cache_hit = self.cache.as_ref().is_some_and(|cache| {
            cache.run_id == request.run_id
                && cache.provider == request.provider
                && cache.model == request.model
                && cache.system == request.system
                && request.messages.len() >= cache.source_message_count
        });
        let summarize = |messages: &[Value]| summarize_with_model(host, messages);
        let outcome = self.compress_incrementally(request, &summarize)?;
        if let Some(event) = outcome.event {
            host.emit_event(&event)?;
            return Ok(Some(outcome.context));
        }
        // 未压缩的完整历史已由 Host 持有，无需跨 WASM 边界往返序列化。
        // 命中缓存时仍须返回插件维护的已压缩上下文及本轮追加消息。
        Ok(cache_hit.then_some(outcome.context))
    }
}

/// 一次压缩计算的上下文和可选展示事件。
struct CompressionOutcome {
    context: LoadedContext,
    event: Option<ExtensionEvent>,
}

/// 完整压缩生成的替换消息、摘要范围和模型用量。
struct CompactedMessages {
    messages: Vec<Value>,
    summarized_messages: usize,
    summary_usage: Option<Value>,
}

/// 按模型窗口、当前输入规模和手动请求选择分层压缩策略。
fn compress_context(
    request: ContextLoadRequest,
    manual: bool,
    summarize: &dyn Fn(&[Value]) -> Result<ModelCompletionResponse>,
) -> Result<CompressionOutcome> {
    let before_messages = request.messages.len();
    let before_tokens = estimate_context_tokens(request.system.as_deref(), &request.messages);
    let (micro_threshold, compact_threshold) = thresholds_for_model(&request.model);

    if manual || before_tokens >= compact_threshold {
        if let Some(compacted) = compact_messages(&request.messages, manual, summarize)? {
            let CompactedMessages {
                messages,
                summarized_messages,
                summary_usage,
            } = compacted;
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            return Ok(CompressionOutcome {
                context: LoadedContext {
                    system: request.system,
                    messages,
                },
                event: Some(ExtensionEvent {
                    name: "context.compaction.completed".into(),
                    data: json!({
                        "run_id": request.run_id,
                        "step": request.step,
                        "before_messages": before_messages,
                        "after_messages": before_messages - summarized_messages + 1,
                        "summarized_messages": summarized_messages,
                        "estimated_tokens_before": before_tokens,
                        "estimated_tokens_after": after_tokens,
                        "summary_usage": summary_usage,
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "model_summary_with_recent_tail"
                    }),
                    presentation: Some(EventPresentation::divider(
                        "上下文压缩",
                        EventPresentationTone::Info,
                    )),
                }),
            });
        }
    }

    if manual || before_tokens >= micro_threshold {
        let (messages, cleared_results) = micro_compact_tool_results(&request.messages);
        if cleared_results > 0 {
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            return Ok(CompressionOutcome {
                context: LoadedContext {
                    system: request.system,
                    messages,
                },
                event: Some(ExtensionEvent {
                    name: "context.micro_compaction.completed".into(),
                    data: json!({
                        "run_id": request.run_id,
                        "step": request.step,
                        "before_messages": before_messages,
                        "after_messages": before_messages,
                        "cleared_tool_results": cleared_results,
                        "estimated_tokens_before": before_tokens,
                        "estimated_tokens_after": after_tokens,
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "clear_old_successful_tool_results"
                    }),
                    presentation: None,
                }),
            });
        }
    }

    if manual {
        return Ok(CompressionOutcome {
            context: LoadedContext {
                system: request.system,
                messages: request.messages,
            },
            event: Some(ExtensionEvent {
                name: "context.compaction.skipped".into(),
                data: json!({
                    "run_id": request.run_id,
                    "step": request.step,
                    "estimated_tokens": before_tokens,
                    "reason": "no_compressible_history",
                    "trigger": "manual"
                }),
                presentation: Some(EventPresentation::divider(
                    "没有可压缩的历史上下文",
                    EventPresentationTone::Muted,
                )),
            }),
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

/// 根据 Claude Code 的 `[1m]` 模型标记返回微压缩和完整压缩水位。
fn thresholds_for_model(model: &str) -> (usize, usize) {
    if model.to_ascii_lowercase().contains("[1m]") {
        (
            LARGE_MICRO_COMPACT_THRESHOLD_TOKENS,
            LARGE_COMPACT_THRESHOLD_TOKENS,
        )
    } else {
        (
            DEFAULT_MICRO_COMPACT_THRESHOLD_TOKENS,
            DEFAULT_COMPACT_THRESHOLD_TOKENS,
        )
    }
}

/// 用序列化字节数进行保守估算；三字节约一个 token，兼顾英文和中文内容。
fn estimate_context_tokens(system: Option<&str>, messages: &[Value]) -> usize {
    let system_tokens = system.map_or(0, estimate_text_tokens);
    system_tokens
        + messages
            .iter()
            .map(|message| estimate_text_tokens(&message.to_string()))
            .sum::<usize>()
}

/// 将文本字节数换算为粗略 token 数。
fn estimate_text_tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// 清理较旧的成功工具结果正文，同时保留调用关联、错误结果和最近成功结果。
fn micro_compact_tool_results(messages: &[Value]) -> (Vec<Value>, usize) {
    let total_results = messages
        .iter()
        .map(compactable_tool_result_count)
        .sum::<usize>();
    let mut remaining_to_clear = total_results.saturating_sub(RECENT_TOOL_RESULTS_TO_KEEP);
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
fn compact_messages(
    messages: &[Value],
    manual: bool,
    summarize: &dyn Fn(&[Value]) -> Result<ModelCompletionResponse>,
) -> Result<Option<CompactedMessages>> {
    let group_starts = api_round_group_starts(messages);
    if group_starts.len() < 2 {
        return Ok(None);
    }

    let split_index = recent_tail_start(messages, &group_starts, manual);
    if split_index == 0 || split_index >= messages.len() {
        return Ok(None);
    }

    let response = summarize(&messages[..split_index])?;
    let summary = normalize_model_summary(&response.text)?;
    let mut compacted = Vec::with_capacity(messages.len() - split_index + 1);
    compacted.push(json!({
        "role": "developer",
        "content": [{
            "type": "text",
            "text": format!(
                "本会话早期上下文已压缩为以下结构化摘要。近期消息仍按原文附在摘要之后。\n\n{summary}"
            )
        }]
    }));
    compacted.extend_from_slice(&messages[split_index..]);
    Ok(Some(CompactedMessages {
        messages: compacted,
        summarized_messages: split_index,
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
fn recent_tail_start(messages: &[Value], group_starts: &[usize], manual: bool) -> usize {
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
        if preserved_groups >= 1
            && preserved_tokens.saturating_add(group_tokens) > RECENT_CONTEXT_TARGET_TOKENS
        {
            break;
        }
        preserved_tokens += group_tokens;
        start = group_start;
    }
    start
}

/// 通过 Host 固定路由调用模型生成摘要；Guest 无法指定 provider、model 或工具。
fn summarize_with_model(
    host: &dyn PluginHostApi,
    messages: &[Value],
) -> Result<ModelCompletionResponse> {
    let mut summary_messages = messages.to_vec();
    summary_messages.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": SUMMARY_REQUEST_PROMPT}]
    }));
    host.complete_model(&ModelCompletionRequest {
        system: Some(SUMMARY_SYSTEM_PROMPT.into()),
        messages: summary_messages,
        max_tokens: Some(SUMMARY_MAX_OUTPUT_TOKENS),
    })
    .context("调用模型生成上下文摘要失败")
}

/// 清理模型可能返回的分析草稿和 summary 标签，并拒绝空摘要。
fn normalize_model_summary(text: &str) -> Result<String> {
    let without_analysis = remove_tagged_section(text, "<analysis>", "</analysis>");
    let trimmed = without_analysis.trim();
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

export_plugin!(ContextPlugin);

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
    fn test_summary(_messages: &[Value]) -> Result<ModelCompletionResponse> {
        Ok(ModelCompletionResponse {
            text: "<analysis>测试草稿</analysis><summary>模型生成的上下文摘要：保留用户请求、错误状态与下一步。</summary>".into(),
            usage: Some(json!({
                "input_tokens": 100,
                "output_tokens": 20,
                "total_tokens": 120
            })),
        })
    }

    /// 使用确定性摘要器执行一次测试压缩。
    fn compress_for_test(request: ContextLoadRequest, manual: bool) -> CompressionOutcome {
        compress_context(request, manual, &test_summary).expect("测试压缩应成功")
    }

    /// 低于水位时必须完整透传，避免短会话发生无意义改写。
    #[test]
    fn keeps_small_context_unchanged() {
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
        );

        assert_eq!(outcome.context.messages, messages);
        assert!(outcome.event.is_none());
    }

    /// 微压缩只清理旧结果，最近三条工具结果必须保持原文。
    #[test]
    fn micro_compaction_preserves_recent_tool_results() {
        let messages = (0..6)
            .map(|index| tool_result_message(index, 70_000))
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
        );

        assert_eq!(
            outcome.event.as_ref().map(|event| event.name.as_str()),
            Some("context.micro_compaction.completed")
        );
        assert!(
            outcome
                .event
                .as_ref()
                .is_some_and(|event| event.presentation.is_none()),
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
    #[test]
    fn micro_compaction_preserves_failed_tool_results() {
        let failure = failed_tool_result_message(0, "文件不存在：src/missing.rs");
        let mut messages = vec![failure.clone()];
        messages.extend((0..4).map(|index| tool_result_message(index, 100_000)));
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
        );

        assert_eq!(outcome.context.messages[0], failure);
        assert_eq!(
            outcome.context.messages[1]["content"][0]["result"]["content"],
            CLEARED_TOOL_RESULT
        );
    }

    /// 完整压缩应生成结构化摘要，并逐字保留最近 API 轮次。
    #[test]
    fn full_compaction_summarizes_prefix_and_keeps_recent_rounds() {
        use std::cell::RefCell;

        let recent_request = "继续修复最新失败用例";
        let messages = vec![
            text_message("user", "分析上下文压缩"),
            text_message("assistant", "已定位压缩入口"),
            failed_tool_result_message(0, "读取配置失败：权限不足"),
            tool_result_message(0, 520_000),
            text_message("assistant", "已完成旧历史分析"),
            text_message("user", recent_request),
            text_message("assistant", "正在处理最新用例"),
        ];
        let summarized_messages = RefCell::new(Vec::new());
        let summarize = |messages: &[Value]| {
            summarized_messages.replace(messages.to_vec());
            test_summary(messages)
        };
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
            &summarize,
        )
        .expect("完整压缩应调用模型摘要");

        assert_eq!(
            outcome.event.as_ref().map(|event| event.name.as_str()),
            Some("context.compaction.completed")
        );
        assert_eq!(outcome.context.messages[0]["role"], "developer");
        let summary = outcome.context.messages[0]["content"][0]["text"]
            .as_str()
            .expect("摘要应为文本");
        assert!(summary.contains("模型生成的上下文摘要"));
        assert!(!summary.contains("测试草稿"));
        assert!(summarized_messages
            .borrow()
            .iter()
            .any(|message| message.to_string().contains("读取配置失败：权限不足")));
        assert_eq!(
            outcome.event.as_ref().expect("应发布压缩事件").data["summary_usage"]["output_tokens"],
            20
        );
        assert!(outcome
            .context
            .messages
            .iter()
            .any(|message| { message["content"][0]["text"].as_str() == Some(recent_request) }));
    }

    /// 只有两个 API 轮次时也必须能压缩超大前缀，不能因保留轮次数下限失效。
    #[test]
    fn full_compaction_handles_two_api_rounds() {
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
                    text_message("user", "x".repeat(520_000)),
                    text_message("assistant", "已接收旧请求"),
                    text_message("user", recent_request),
                ],
            },
            false,
        );

        assert_eq!(
            outcome.event.as_ref().map(|event| event.name.as_str()),
            Some("context.compaction.completed")
        );
        assert!(outcome
            .context
            .messages
            .iter()
            .any(|message| { message["content"][0]["text"].as_str() == Some(recent_request) }));
    }

    /// 手动压缩不受自动水位限制，并在存在历史轮次时生成完整摘要。
    #[test]
    fn manual_compaction_bypasses_automatic_threshold() {
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
        );

        assert_eq!(
            outcome.event.as_ref().map(|event| event.name.as_str()),
            Some("context.compaction.completed")
        );
        assert_eq!(
            outcome.event.expect("应发布压缩事件").data["trigger"],
            "manual"
        );
    }

    /// 自动压缩后新增消息应复用已压缩前缀，且不重复发布压缩事件。
    #[test]
    fn incremental_compaction_reuses_compacted_prefix() {
        let large_history = "历史工具输出".repeat(90_000);
        let original_messages = vec![
            text_message("user", &large_history),
            text_message("assistant", "已分析历史"),
            text_message("user", "继续处理"),
            text_message("assistant", "正在处理"),
        ];
        let mut plugin = ContextPlugin::default();
        let first = plugin
            .compress_incrementally(
                ContextLoadRequest {
                    run_id: "incremental-run".into(),
                    step: 0,
                    user_initiated: false,
                    provider: "test".into(),
                    model: "test-model".into(),
                    system: None,
                    messages: original_messages.clone(),
                },
                &test_summary,
            )
            .expect("首次增量压缩应成功");
        assert!(first.event.is_some(), "首轮超过水位时应执行自动压缩");

        let mut extended_messages = original_messages;
        extended_messages.push(text_message("user", "检查刚才的修改"));
        let second = plugin
            .compress_incrementally(
                ContextLoadRequest {
                    run_id: "incremental-run".into(),
                    step: 1,
                    user_initiated: false,
                    provider: "test".into(),
                    model: "test-model".into(),
                    system: None,
                    messages: extended_messages,
                },
                &test_summary,
            )
            .expect("后续增量压缩应成功");

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
    #[test]
    fn user_initiated_compaction_returns_replacement_immediately() {
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
        let outcome =
            compress_context(request, true, &test_summary).expect("主动压缩应立即返回结果");

        assert_eq!(outcome.context.messages.len(), 3);
        assert!(outcome.context.messages[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("本会话早期上下文已压缩")));
        assert_eq!(
            outcome.event.expect("应发布压缩事件").data["trigger"],
            "manual"
        );
    }
}
