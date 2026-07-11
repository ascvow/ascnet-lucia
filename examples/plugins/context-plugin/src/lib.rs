//! 基于 Claude Code 分层策略的 Lucia 上下文压缩插件。

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, ContextLoadRequest, EventPresentation,
    EventPresentationTone, ExtensionEvent, LoadedContext, PluginHostApi, Result, ServiceCall,
    ServiceSpec,
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
/// 结构化摘要的字符上限，避免摘要本身再次挤占上下文。
const SUMMARY_CHARACTER_LIMIT: usize = 24_000;
/// 被微压缩的工具结果占位文本。
const CLEARED_TOOL_RESULT: &str = "[旧工具结果内容已清理]";
/// 原生 TUI 调用主动压缩服务时使用的稳定身份。
const TUI_PLUGIN_ID: &str = "lucia-tui";
/// Context 插件接收主动压缩请求的服务名。
const CONTEXT_COMPACT_SERVICE: &str = "context.compact";
/// 主动压缩服务当前版本。
const CONTEXT_SERVICE_VERSION: &str = "1.0.0";

/// 提供分层上下文压缩能力的插件。
#[derive(Default)]
struct ContextPlugin {
    /// 最近一次自动压缩结果，用于增量追加后续消息，避免重复处理同一历史前缀。
    cache: Option<CompressionCache>,
}

/// 最近一次自动压缩的原始前缀与有效模型上下文。
struct CompressionCache {
    provider: String,
    model: String,
    system: Option<String>,
    source_messages: Vec<Value>,
    loaded_messages: Vec<Value>,
}

impl ContextPlugin {
    /// 复用已压缩历史，只把当前请求新增的消息追加到有效上下文后再判断水位。
    fn compress_incrementally(&mut self, request: ContextLoadRequest) -> CompressionOutcome {
        let cache_hit = self.cache.as_ref().is_some_and(|cache| {
            cache.provider == request.provider
                && cache.model == request.model
                && cache.system == request.system
                && request.messages.starts_with(&cache.source_messages)
        });
        let source_messages = request.messages.clone();
        let provider = request.provider.clone();
        let model = request.model.clone();
        let system = request.system.clone();
        let effective_request = if cache_hit {
            let cache = self.cache.as_ref().expect("命中缓存时必须存在缓存内容");
            let mut messages = cache.loaded_messages.clone();
            messages.extend_from_slice(&request.messages[cache.source_messages.len()..]);
            ContextLoadRequest {
                messages,
                ..request
            }
        } else {
            request
        };
        let outcome = compress_context(effective_request, false);

        if cache_hit || outcome.event.is_some() {
            self.cache = Some(CompressionCache {
                provider,
                model,
                system,
                source_messages,
                loaded_messages: outcome.context.messages.clone(),
            });
        } else {
            self.cache = None;
        }
        outcome
    }
}

impl AgentPlugin for ContextPlugin {
    /// 注册主动压缩服务；命令定义由官方 Command 插件维护。
    fn activate(&mut self, host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        host.upsert_service(&ServiceSpec {
            name: CONTEXT_COMPACT_SERVICE.into(),
            version: CONTEXT_SERVICE_VERSION.into(),
            description: Some("立即压缩原生 TUI 提供的完整 Session 上下文".into()),
        })?;
        Ok(())
    }

    /// 注销主动压缩服务，并释放仅服务于运行期请求的压缩缓存。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_service(CONTEXT_COMPACT_SERVICE)?;
        self.cache = None;
        Ok(())
    }

    /// 接收原生 TUI 的完整 Session，并同步返回压缩后的替换上下文。
    fn handle_service(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        if call.name != CONTEXT_COMPACT_SERVICE {
            return Err(anyhow!("Context 插件未实现服务 `{}`", call.name));
        }
        if call.caller_id != TUI_PLUGIN_ID {
            return Err(anyhow!("调用方 `{}` 无权请求上下文压缩", call.caller_id));
        }
        self.cache = None;
        let outcome = compact_service_request(call.payload)?;
        if let Some(event) = outcome.event {
            host.emit_event(&event)?;
        }
        serde_json::to_value(outcome.context).context("序列化主动压缩结果失败")
    }

    /// 根据估算 token 水位透传、微压缩或完整压缩上下文，并发布对应事件。
    fn load_context(
        &mut self,
        host: &dyn PluginHostApi,
        request: ContextLoadRequest,
    ) -> Result<Option<LoadedContext>> {
        let outcome = self.compress_incrementally(request);
        if let Some(event) = outcome.event {
            host.emit_event(&event)?;
        }
        Ok(Some(outcome.context))
    }
}

/// 一次压缩计算的上下文和可选展示事件。
struct CompressionOutcome {
    context: LoadedContext,
    event: Option<ExtensionEvent>,
}

/// 按模型窗口、当前输入规模和手动请求选择分层压缩策略。
fn compress_context(request: ContextLoadRequest, manual: bool) -> CompressionOutcome {
    let before_messages = request.messages.len();
    let before_tokens = estimate_context_tokens(request.system.as_deref(), &request.messages);
    let (micro_threshold, compact_threshold) = thresholds_for_model(&request.model);

    if manual || before_tokens >= compact_threshold {
        if let Some((messages, summarized_messages)) = compact_messages(&request.messages, manual) {
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            return CompressionOutcome {
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
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "structured_summary_with_recent_tail"
                    }),
                    presentation: Some(EventPresentation::divider(
                        "上下文压缩",
                        EventPresentationTone::Info,
                    )),
                }),
            };
        }
    }

    if manual || before_tokens >= micro_threshold {
        let (messages, cleared_results) = micro_compact_tool_results(&request.messages);
        if cleared_results > 0 {
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            return CompressionOutcome {
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
                        "strategy": "clear_old_tool_results"
                    }),
                    presentation: Some(EventPresentation::divider(
                        "上下文微压缩",
                        EventPresentationTone::Muted,
                    )),
                }),
            };
        }
    }

    if manual {
        return CompressionOutcome {
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
        };
    }

    CompressionOutcome {
        context: LoadedContext {
            system: request.system,
            messages: request.messages,
        },
        event: None,
    }
}

/// 解析原生 TUI 提供的完整请求，并立即执行不受自动水位限制的压缩。
fn compact_service_request(payload: Value) -> Result<CompressionOutcome> {
    let request = serde_json::from_value(payload).context("解析主动压缩请求失败")?;
    Ok(compress_context(request, true))
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

/// 清理较旧工具结果正文，同时保留调用关联、工具名、错误状态和最近结果。
fn micro_compact_tool_results(messages: &[Value]) -> (Vec<Value>, usize) {
    let total_results = messages.iter().map(tool_result_count).sum::<usize>();
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

/// 统计一条消息中的工具结果块数量。
fn tool_result_count(message: &Value) -> usize {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
                .count()
        })
        .unwrap_or(0)
}

/// 把较旧 API 轮次替换为结构化摘要；手动模式只保留最新完整轮次。
fn compact_messages(messages: &[Value], manual: bool) -> Option<(Vec<Value>, usize)> {
    let group_starts = api_round_group_starts(messages);
    if group_starts.len() < 2 {
        return None;
    }

    let split_index = recent_tail_start(messages, &group_starts, manual);
    if split_index == 0 || split_index >= messages.len() {
        return None;
    }

    let summary = build_structured_summary(&messages[..split_index]);
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
    Some((compacted, split_index))
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

/// 从被替换的历史中提取用户意图、技术进展、工具状态和继续工作线索。
fn build_structured_summary(messages: &[Value]) -> String {
    let mut user_requests = Vec::new();
    let mut assistant_progress = Vec::new();
    let mut developer_context = Vec::new();
    let mut tool_activity = Vec::new();

    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => collect_text_blocks(message, &mut user_requests, 2_000, 20),
            Some("assistant") => {
                collect_text_blocks(message, &mut assistant_progress, 1_500, 20);
                collect_tool_calls(message, &mut tool_activity, 40);
            }
            Some("developer") | Some("system") => {
                collect_text_blocks(message, &mut developer_context, 1_500, 12);
            }
            Some("tool") => collect_tool_results(message, &mut tool_activity, 40),
            _ => {}
        }
    }

    let mut summary = format!(
        "压缩范围：{} 条较旧消息。\n\n## 用户请求与意图\n{}\n\n## 开发者约束与已有上下文\n{}\n\n## 已完成工作与技术进展\n{}\n\n## 工具调用、文件和错误状态\n{}\n\n## 继续工作上下文\n{}",
        messages.len(),
        render_items(&user_requests),
        render_items(&developer_context),
        render_items(&assistant_progress),
        render_items(&tool_activity),
        render_recent_context(messages),
    );
    truncate_chars(&mut summary, SUMMARY_CHARACTER_LIMIT);
    summary
}

/// 收集消息文本块，限制单项和总项数以稳定摘要规模。
fn collect_text_blocks(
    message: &Value,
    output: &mut Vec<String>,
    item_limit: usize,
    max_items: usize,
) {
    if output.len() >= max_items {
        return;
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if output.len() >= max_items {
            break;
        }
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                output.push(truncated(text, item_limit));
            }
        }
    }
}

/// 收集工具名和参数摘要，使文件路径及关键操作在压缩后仍可恢复。
fn collect_tool_calls(message: &Value, output: &mut Vec<String>, max_items: usize) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if output.len() >= max_items
            || block.get("type").and_then(Value::as_str) != Some("tool_call")
        {
            continue;
        }
        let Some(call) = block.get("call") else {
            continue;
        };
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let args = call.get("args").cloned().unwrap_or(Value::Null).to_string();
        output.push(format!("调用 `{name}`，参数：{}", truncated(&args, 600)));
    }
}

/// 收集工具结果状态；成功结果只保留短摘录，错误结果保留更长诊断信息。
fn collect_tool_results(message: &Value, output: &mut Vec<String>, max_items: usize) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if output.len() >= max_items
            || block.get("type").and_then(Value::as_str) != Some("tool_result")
        {
            continue;
        }
        let Some(result) = block.get("result") else {
            continue;
        };
        let name = result
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let is_error = result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = result
            .get("content")
            .cloned()
            .unwrap_or(Value::Null)
            .to_string();
        let status = if is_error { "失败" } else { "成功" };
        let limit = if is_error { 1_000 } else { 400 };
        output.push(format!(
            "`{name}` {status}，结果：{}",
            truncated(&content, limit)
        ));
    }
}

/// 渲染摘要列表；没有内容时给出明确占位，避免模型自行补全事实。
fn render_items(items: &[String]) -> String {
    if items.is_empty() {
        return "- 无可提取内容".into();
    }
    items
        .iter()
        .map(|item| format!("- {}", item.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 保留被压缩范围末尾的原文线索，降低任务交接时的意图漂移。
fn render_recent_context(messages: &[Value]) -> String {
    let mut recent = Vec::new();
    for message in messages.iter().rev().take(4).rev() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let text = message_text(message);
        if !text.is_empty() {
            recent.push(format!("- {role}: {}", truncated(&text, 1_200)));
        }
    }
    render_items_without_prefix_duplication(&recent)
}

/// 渲染已经带列表前缀的近期上下文。
fn render_items_without_prefix_duplication(items: &[String]) -> String {
    if items.is_empty() {
        "- 无可提取内容".into()
    } else {
        items.join("\n")
    }
}

/// 拼接一条消息中的全部文本内容。
fn message_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 返回按字符边界截断的文本副本。
fn truncated(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut result = text.chars().take(limit).collect::<String>();
    result.push_str("...[已截断]");
    result
}

/// 原地限制摘要字符数。
fn truncate_chars(text: &mut String, limit: usize) {
    if text.chars().count() > limit {
        *text = truncated(text, limit);
    }
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

    /// 低于水位时必须完整透传，避免短会话发生无意义改写。
    #[test]
    fn keeps_small_context_unchanged() {
        let messages = vec![text_message("user", "检查当前实现")];
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "run-small".into(),
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
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "run-micro".into(),
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

    /// 完整压缩应生成结构化摘要，并逐字保留最近 API 轮次。
    #[test]
    fn full_compaction_summarizes_prefix_and_keeps_recent_rounds() {
        let recent_request = "继续修复最新失败用例";
        let messages = vec![
            text_message("user", "分析上下文压缩"),
            text_message("assistant", "已定位压缩入口"),
            tool_result_message(0, 520_000),
            text_message("assistant", "已完成旧历史分析"),
            text_message("user", recent_request),
            text_message("assistant", "正在处理最新用例"),
        ];
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "run-full".into(),
                step: 2,
                provider: "test".into(),
                model: "test-model".into(),
                system: None,
                messages,
            },
            false,
        );

        assert_eq!(
            outcome.event.as_ref().map(|event| event.name.as_str()),
            Some("context.compaction.completed")
        );
        assert_eq!(outcome.context.messages[0]["role"], "developer");
        assert!(outcome.context.messages[0]["content"][0]["text"]
            .as_str()
            .expect("摘要应为文本")
            .contains("用户请求与意图"));
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
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "run-two-rounds".into(),
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
        let outcome = compress_context(
            ContextLoadRequest {
                run_id: "run-manual".into(),
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
        let first = plugin.compress_incrementally(ContextLoadRequest {
            run_id: "incremental-run".into(),
            step: 0,
            provider: "test".into(),
            model: "test-model".into(),
            system: None,
            messages: original_messages.clone(),
        });
        assert!(first.event.is_some(), "首轮超过水位时应执行自动压缩");

        let mut extended_messages = original_messages;
        extended_messages.push(text_message("user", "检查刚才的修改"));
        let second = plugin.compress_incrementally(ContextLoadRequest {
            run_id: "incremental-run".into(),
            step: 1,
            provider: "test".into(),
            model: "test-model".into(),
            system: None,
            messages: extended_messages,
        });

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

    /// 主动压缩服务立即返回替换上下文，不保留下一轮待处理状态。
    #[test]
    fn compact_service_returns_replacement_immediately() {
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
        };
        let outcome =
            compact_service_request(serde_json::to_value(request).expect("主动压缩请求应能序列化"))
                .expect("主动压缩服务应立即返回结果");

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
