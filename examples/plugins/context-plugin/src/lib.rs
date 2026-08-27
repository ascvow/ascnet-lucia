//! 基于 Claude Code 分层策略的 Lucia 上下文压缩插件。

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, ContextLoadRequest, EventPresentation,
    EventPresentationTone, ExtensionEvent, LoadedContext, ModelCompletionRequest,
    ModelCompletionResponse, PluginHostApi, Result,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

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
/// Genome Context Policy 激活元数据中的规范 JSON 键。
const CONTEXT_POLICY_JSON_METADATA_KEY: &str = "context_policy_json";
/// Genome Context Policy 激活元数据中的 CAS 摘要键。
const CONTEXT_POLICY_DIGEST_METADATA_KEY: &str = "context_policy_digest";
/// 当前插件消费的 Context Policy 结构版本。
const CONTEXT_POLICY_SCHEMA_VERSION: u32 = 1;
/// Context Policy 自动压缩水位允许的固定范围。
const MIN_CONTEXT_THRESHOLD_TOKENS: u32 = 4_096;
const MAX_CONTEXT_THRESHOLD_TOKENS: u32 = 2_000_000;
/// Context Policy 各有界计数和摘要预算的固定上限。
const MAX_RECENT_MESSAGE_COUNT: u16 = 256;
const MAX_PINNED_ITEM_COUNT: u16 = 256;
const MAX_RECENT_TOOL_RESULT_COUNT: u16 = 64;
const MIN_SUMMARY_TOKEN_BUDGET: u32 = 256;
const MAX_SUMMARY_TOKEN_BUDGET: u32 = 32_768;
const MIN_SUMMARY_VALIDATION_COVERAGE_BPS: u16 = 9_500;
/// 上游显式结构化用户约束消息的稳定 JSON schema。
const USER_CONSTRAINT_MARKER_SCHEMA: &str = "lucia.context/user-constraint/v1";
/// 上游显式结构化事实消息的稳定 JSON schema；事实正文只进入摘要，不进入固定区。
const FACT_MARKER_SCHEMA: &str = "lucia.context/fact/v1";
/// 摘要模型返回确定性标记信封时使用的固定标签。
const SUMMARY_MARKERS_OPEN: &str = "<context-policy-markers>";
const SUMMARY_MARKERS_CLOSE: &str = "</context-policy-markers>";
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
    /// `None` 表示未注入 Genome 策略，完整保持历史默认行为。
    policy: Option<ContextPolicyV1>,
    /// 已由插件按激活 JSON 重新计算并核对的 CAS 摘要。
    policy_digest: Option<String>,
}

/// 旧 ToolResult 的版本化保留策略；JSON 形态与 M6 控制面协议一致。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolResultRetentionPolicyV1 {
    /// 原样保留全部 ToolResult。
    PreserveAll,
    /// 原样保留错误和指定数量的近期成功结果。
    PreserveErrorsAndRecent {
        /// 从新到旧保留的成功结果数量。
        recent_successful_results: u16,
    },
}

impl Default for ToolResultRetentionPolicyV1 {
    fn default() -> Self {
        Self::PreserveErrorsAndRecent {
            recent_successful_results: 3,
        }
    }
}

/// 显式结构化用户约束的固定区策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UserConstraintRetentionPolicyV1 {
    /// 固定区逐值保留，不要求摘要信封重复 ID。
    PinnedStructured { max_items: u16 },
    /// 固定区逐值保留，并要求摘要信封确认全部 ID。
    PinnedStructuredAndSummary { max_items: u16 },
}

impl Default for UserConstraintRetentionPolicyV1 {
    fn default() -> Self {
        Self::PinnedStructuredAndSummary { max_items: 64 }
    }
}

/// plan-plugin 结构化快照的固定区策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlanSnapshotRetentionPolicyV1 {
    /// 保留最新完整快照，不要求摘要信封重复修订号。
    LatestSnapshot { max_items: u16 },
    /// 保留最新完整快照，并要求摘要信封确认修订号。
    LatestSnapshotAndSummary { max_items: u16 },
}

impl Default for PlanSnapshotRetentionPolicyV1 {
    fn default() -> Self {
        Self::LatestSnapshotAndSummary { max_items: 100 }
    }
}

/// 摘要后允许的确定性验证算法。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PostSummaryValidationAlgorithmV1 {
    /// 对稳定标记做逐字节集合覆盖验证。
    StructuredMarkerCoverageV1 { min_coverage_bps: u16 },
}

impl Default for PostSummaryValidationAlgorithmV1 {
    fn default() -> Self {
        Self::StructuredMarkerCoverageV1 {
            min_coverage_bps: 10_000,
        }
    }
}

/// 插件侧兼容读取的 M6 Context Policy V1。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ContextPolicyV1 {
    schema_version: u32,
    micro_compact_threshold_tokens: u32,
    full_compact_threshold_tokens: u32,
    recent_message_count: u16,
    tool_result_retention: ToolResultRetentionPolicyV1,
    user_constraints: UserConstraintRetentionPolicyV1,
    plan_snapshot: PlanSnapshotRetentionPolicyV1,
    summary_token_budget: u32,
    post_summary_validation: PostSummaryValidationAlgorithmV1,
}

impl Default for ContextPolicyV1 {
    fn default() -> Self {
        Self {
            schema_version: CONTEXT_POLICY_SCHEMA_VERSION,
            micro_compact_threshold_tokens: DEFAULT_MICRO_COMPACT_THRESHOLD_TOKENS as u32,
            full_compact_threshold_tokens: DEFAULT_COMPACT_THRESHOLD_TOKENS as u32,
            recent_message_count: 8,
            tool_result_retention: ToolResultRetentionPolicyV1::default(),
            user_constraints: UserConstraintRetentionPolicyV1::default(),
            plan_snapshot: PlanSnapshotRetentionPolicyV1::default(),
            summary_token_budget: SUMMARY_MAX_OUTPUT_TOKENS,
            post_summary_validation: PostSummaryValidationAlgorithmV1::default(),
        }
    }
}

impl ContextPolicyV1 {
    /// 复核 Guest 实际执行的版本、范围关系与不可关闭安全下限。
    fn validate(&self) -> Result<()> {
        if self.schema_version != CONTEXT_POLICY_SCHEMA_VERSION {
            return Err(anyhow!(
                "不支持的 Context Policy schema_version：{}",
                self.schema_version
            ));
        }
        for (name, value) in [
            (
                "micro_compact_threshold_tokens",
                self.micro_compact_threshold_tokens,
            ),
            (
                "full_compact_threshold_tokens",
                self.full_compact_threshold_tokens,
            ),
        ] {
            if !(MIN_CONTEXT_THRESHOLD_TOKENS..=MAX_CONTEXT_THRESHOLD_TOKENS).contains(&value) {
                return Err(anyhow!("Context Policy `{name}` 超出允许范围"));
            }
        }
        if self.micro_compact_threshold_tokens >= self.full_compact_threshold_tokens {
            return Err(anyhow!("Context Policy 微压缩水位必须小于完整压缩水位"));
        }
        if self.recent_message_count == 0 || self.recent_message_count > MAX_RECENT_MESSAGE_COUNT {
            return Err(anyhow!(
                "Context Policy `recent_message_count` 超出允许范围"
            ));
        }
        if let ToolResultRetentionPolicyV1::PreserveErrorsAndRecent {
            recent_successful_results,
        } = &self.tool_result_retention
        {
            if *recent_successful_results == 0
                || *recent_successful_results > MAX_RECENT_TOOL_RESULT_COUNT
            {
                return Err(anyhow!("Context Policy 近期 ToolResult 数量超出允许范围"));
            }
        }
        for (name, value) in [
            (
                "user_constraints.max_items",
                constraint_limit(&self.user_constraints),
            ),
            ("plan_snapshot.max_items", plan_limit(&self.plan_snapshot)),
        ] {
            if value == 0 || value > MAX_PINNED_ITEM_COUNT {
                return Err(anyhow!("Context Policy `{name}` 超出允许范围"));
            }
        }
        if !((MIN_SUMMARY_TOKEN_BUDGET..=MAX_SUMMARY_TOKEN_BUDGET)
            .contains(&self.summary_token_budget)
            && self.summary_token_budget < self.full_compact_threshold_tokens)
        {
            return Err(anyhow!("Context Policy 摘要 token 预算无效"));
        }
        let PostSummaryValidationAlgorithmV1::StructuredMarkerCoverageV1 { min_coverage_bps } =
            self.post_summary_validation;
        if !(MIN_SUMMARY_VALIDATION_COVERAGE_BPS..=10_000).contains(&min_coverage_bps) {
            return Err(anyhow!("Context Policy 摘要验证覆盖率无效"));
        }
        Ok(())
    }
}

/// 返回结构化约束固定区上限。
fn constraint_limit(policy: &UserConstraintRetentionPolicyV1) -> u16 {
    match policy {
        UserConstraintRetentionPolicyV1::PinnedStructured { max_items }
        | UserConstraintRetentionPolicyV1::PinnedStructuredAndSummary { max_items } => *max_items,
    }
}

/// 返回 Plan 快照步骤上限。
fn plan_limit(policy: &PlanSnapshotRetentionPolicyV1) -> u16 {
    match policy {
        PlanSnapshotRetentionPolicyV1::LatestSnapshot { max_items }
        | PlanSnapshotRetentionPolicyV1::LatestSnapshotAndSummary { max_items } => *max_items,
    }
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
        summarize: &SummaryFunction<'_>,
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
        let outcome = compress_context(
            effective_request,
            false,
            self.policy.as_ref(),
            self.policy_digest.as_deref(),
            summarize,
        )?;

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
    /// 激活时消费 Host 按插件 ID 隔离注入的 Context Policy，并复核原始 JSON 摘要。
    ///
    /// 两个元数据键都不存在时保持历史默认算法；只出现一个键、JSON 无效或摘要不匹配时
    /// 失败关闭，避免运行未被 Genome 固定的策略。
    fn activate(&mut self, _host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        let policy_json = context.metadata.get(CONTEXT_POLICY_JSON_METADATA_KEY);
        let policy_digest = context.metadata.get(CONTEXT_POLICY_DIGEST_METADATA_KEY);
        match (policy_json, policy_digest) {
            (None, None) => {
                self.policy = None;
                self.policy_digest = None;
            }
            (Some(policy_json), Some(policy_digest)) => {
                let actual_digest = format!("sha256:{:x}", Sha256::digest(policy_json.as_bytes()));
                if actual_digest != *policy_digest {
                    return Err(anyhow!(
                        "Context Policy 摘要不匹配：声明 {policy_digest}，实际 {actual_digest}"
                    ));
                }
                let policy: ContextPolicyV1 = serde_json::from_str(policy_json)
                    .context("解析 Host 注入的 Context Policy JSON 失败")?;
                policy.validate()?;
                self.policy = Some(policy);
                self.policy_digest = Some(policy_digest.clone());
            }
            _ => {
                return Err(anyhow!(
                    "Context Policy 激活元数据不完整，正文和摘要必须同时存在"
                ));
            }
        }
        self.cache = None;
        Ok(())
    }

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
            let summarize = |messages: &[Value], requirements: &SummaryRequirements, max_tokens| {
                summarize_with_model(host, messages, requirements, max_tokens)
            };
            let outcome = compress_context(
                request,
                true,
                self.policy.as_ref(),
                self.policy_digest.as_deref(),
                &summarize,
            )?;
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
        let summarize = |messages: &[Value], requirements: &SummaryRequirements, max_tokens| {
            summarize_with_model(host, messages, requirements, max_tokens)
        };
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
    preserved_user_constraints: usize,
    preserved_plan_snapshots: usize,
    preservation_verified: bool,
}

/// 摘要模型必须回传的确定性稳定标记集合。
#[derive(Debug, Default, Serialize)]
struct SummaryRequirements {
    schema_version: u32,
    constraint_ids: BTreeSet<String>,
    tool_result_call_ids: BTreeSet<String>,
    fact_ids: BTreeSet<String>,
    plan_revision: Option<u64>,
}

/// 摘要模型返回的结构化标记信封。
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SummaryMarkers {
    schema_version: u32,
    #[serde(default)]
    constraint_ids: BTreeSet<String>,
    #[serde(default)]
    tool_result_call_ids: BTreeSet<String>,
    #[serde(default)]
    fact_ids: BTreeSet<String>,
    #[serde(default)]
    plan_revision: Option<u64>,
}

/// 需要在摘要外固定区逐值保留的结构化消息。
#[derive(Debug, Default)]
struct PinnedMessages {
    indices: BTreeSet<usize>,
    user_constraint_ids: BTreeSet<String>,
    plan_revision: Option<u64>,
    plan_message_index: Option<usize>,
}

/// 摘要调用的内部函数签名，统一约束模型输入、标记要求和输出预算。
type SummaryFunction<'a> =
    dyn Fn(&[Value], &SummaryRequirements, u32) -> Result<ModelCompletionResponse> + 'a;

/// 按模型窗口、当前输入规模和手动请求选择分层压缩策略。
fn compress_context(
    request: ContextLoadRequest,
    manual: bool,
    policy: Option<&ContextPolicyV1>,
    policy_digest: Option<&str>,
    summarize: &SummaryFunction<'_>,
) -> Result<CompressionOutcome> {
    let before_messages = request.messages.len();
    let before_tokens = estimate_context_tokens(request.system.as_deref(), &request.messages);
    let (micro_threshold, compact_threshold) = match policy {
        Some(policy) => (
            policy.micro_compact_threshold_tokens as usize,
            policy.full_compact_threshold_tokens as usize,
        ),
        None => thresholds_for_model(&request.model),
    };
    let policy_summary = policy.map(policy_event_data);

    if manual || before_tokens >= compact_threshold {
        if let Some(compacted) = compact_messages(&request.messages, manual, policy, summarize)? {
            let CompactedMessages {
                messages,
                summarized_messages,
                summary_usage,
                preserved_user_constraints,
                preserved_plan_snapshots,
                preservation_verified,
            } = compacted;
            let after_tokens = estimate_context_tokens(request.system.as_deref(), &messages);
            let after_messages = messages.len();
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
                        "after_messages": after_messages,
                        "summarized_messages": summarized_messages,
                        "estimated_tokens_before": before_tokens,
                        "estimated_tokens_after": after_tokens,
                        "summary_usage": summary_usage,
                        "trigger": if manual { "manual" } else { "auto" },
                        "strategy": "model_summary_with_recent_tail",
                        "policy_digest": policy_digest,
                        "policy": policy_summary,
                        "policy_verified": policy.is_some(),
                        "preservation_verified": preservation_verified,
                        "preserved_user_constraints": preserved_user_constraints,
                        "preserved_plan_snapshots": preserved_plan_snapshots,
                        "summary_max_output_tokens": policy
                            .map_or(SUMMARY_MAX_OUTPUT_TOKENS, |policy| policy.summary_token_budget)
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
        let (messages, cleared_results) = micro_compact_tool_results(
            &request.messages,
            policy.map(|policy| &policy.tool_result_retention),
        );
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
                        "strategy": "clear_old_successful_tool_results",
                        "policy_digest": policy_digest,
                        "policy": policy_summary,
                        "policy_verified": policy.is_some()
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

/// 把当前实际执行的策略参数写入结构化事件，便于 Evidence 对照 Genome 摘要。
fn policy_event_data(policy: &ContextPolicyV1) -> Value {
    json!({
        "schema_version": policy.schema_version,
        "micro_compact_threshold_tokens": policy.micro_compact_threshold_tokens,
        "full_compact_threshold_tokens": policy.full_compact_threshold_tokens,
        "recent_message_count": policy.recent_message_count,
        "tool_result_retention": policy.tool_result_retention,
        "user_constraints": policy.user_constraints,
        "plan_snapshot": policy.plan_snapshot,
        "summary_token_budget": policy.summary_token_budget,
        "post_summary_validation": policy.post_summary_validation,
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
fn micro_compact_tool_results(
    messages: &[Value],
    retention: Option<&ToolResultRetentionPolicyV1>,
) -> (Vec<Value>, usize) {
    let keep = match retention {
        Some(ToolResultRetentionPolicyV1::PreserveAll) => return (messages.to_vec(), 0),
        Some(ToolResultRetentionPolicyV1::PreserveErrorsAndRecent {
            recent_successful_results,
        }) => *recent_successful_results as usize,
        None => RECENT_TOOL_RESULTS_TO_KEEP,
    };
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
    policy: Option<&ContextPolicyV1>,
    summarize: &SummaryFunction<'_>,
) -> Result<Option<CompactedMessages>> {
    let group_starts = api_round_group_starts(messages);
    if group_starts.len() < 2 {
        return Ok(None);
    }

    let split_index = recent_tail_start(messages, &group_starts, manual, policy);
    if split_index == 0 || split_index >= messages.len() {
        return Ok(None);
    }

    let pinned = match policy {
        Some(policy) => collect_pinned_messages(messages, split_index, policy)?,
        None => PinnedMessages::default(),
    };
    let requirements = summary_requirements(messages, split_index, policy, &pinned)?;
    let max_tokens = policy.map_or(SUMMARY_MAX_OUTPUT_TOKENS, |policy| {
        policy.summary_token_budget
    });
    let response = summarize(&messages[..split_index], &requirements, max_tokens)?;
    let (summary, markers) = normalize_model_summary(&response.text, policy.is_some())?;
    if let Some(policy) = policy {
        verify_summary_markers(
            markers
                .as_ref()
                .ok_or_else(|| anyhow!("模型摘要缺少结构化标记信封"))?,
            &requirements,
            policy,
        )?;
    }
    let mut compacted = Vec::with_capacity(messages.len() - split_index + pinned.indices.len() + 1);
    compacted.push(json!({
        "role": "developer",
        "content": [{
            "type": "text",
            "text": format!(
                "本会话早期上下文已压缩为以下结构化摘要。近期消息仍按原文附在摘要之后。\n\n{summary}"
            )
        }]
    }));
    for index in &pinned.indices {
        if *index < split_index {
            compacted.push(messages[*index].clone());
        }
    }
    compacted.extend_from_slice(&messages[split_index..]);
    let preservation_verified = verify_pinned_messages(&compacted, messages, &pinned)?;
    Ok(Some(CompactedMessages {
        messages: compacted,
        summarized_messages: split_index,
        summary_usage: response.usage,
        preserved_user_constraints: pinned.user_constraint_ids.len(),
        preserved_plan_snapshots: usize::from(pinned.plan_message_index.is_some()),
        preservation_verified,
    }))
}

/// 收集上游显式标记的用户约束和最新成功 Plan 快照；不解析普通自然语言。
fn collect_pinned_messages(
    messages: &[Value],
    split_index: usize,
    policy: &ContextPolicyV1,
) -> Result<PinnedMessages> {
    let mut pinned = PinnedMessages::default();
    for (index, message) in messages.iter().take(split_index).enumerate() {
        if message.get("role").and_then(Value::as_str) == Some("user") {
            for id in explicit_user_constraint_ids(message)? {
                if !pinned.user_constraint_ids.insert(id.clone()) {
                    return Err(anyhow!("结构化用户约束 ID 重复：{id}"));
                }
                pinned.indices.insert(index);
            }
        }
        if let Some(revision) = plan_snapshot_revision(message, plan_limit(&policy.plan_snapshot))?
        {
            pinned.plan_revision = Some(revision);
            pinned.plan_message_index = Some(index);
        }
    }
    if pinned.user_constraint_ids.len() > constraint_limit(&policy.user_constraints) as usize {
        return Err(anyhow!("结构化用户约束数量超过 Context Policy 固定区上限"));
    }
    if let Some(index) = pinned.plan_message_index {
        pinned.indices.insert(index);
    }
    pin_tool_result_messages(
        messages,
        split_index,
        &policy.tool_result_retention,
        &mut pinned.indices,
    );
    Ok(pinned)
}

/// 按策略把摘要前缀中的错误与近期成功 ToolResult 固定为原始消息。
fn pin_tool_result_messages(
    messages: &[Value],
    split_index: usize,
    policy: &ToolResultRetentionPolicyV1,
    indices: &mut BTreeSet<usize>,
) {
    let mut successful = Vec::new();
    for (index, message) in messages.iter().take(split_index).enumerate() {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        let mut has_result = false;
        let mut has_error = false;
        let mut has_success = false;
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            has_result = true;
            if block
                .pointer("/result/is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                has_error = true;
            } else {
                has_success = true;
            }
        }
        match policy {
            ToolResultRetentionPolicyV1::PreserveAll if has_result => {
                indices.insert(index);
            }
            ToolResultRetentionPolicyV1::PreserveErrorsAndRecent { .. } => {
                if has_error {
                    indices.insert(index);
                }
                if has_success {
                    successful.push(index);
                }
            }
            _ => {}
        }
    }
    if let ToolResultRetentionPolicyV1::PreserveErrorsAndRecent {
        recent_successful_results,
    } = policy
    {
        for index in successful
            .into_iter()
            .rev()
            .take(*recent_successful_results as usize)
        {
            indices.insert(index);
        }
    }
    let retained_call_ids = indices
        .iter()
        .flat_map(|index| tool_result_call_ids(&messages[*index]))
        .collect::<BTreeSet<_>>();
    for (index, message) in messages.iter().take(split_index).enumerate() {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        if blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_call")
                && block
                    .pointer("/call/id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| retained_call_ids.contains(id))
        }) {
            indices.insert(index);
        }
    }
}

/// 返回一条消息内全部非空 ToolResult 调用 ID。
fn tool_result_call_ids(message: &Value) -> Vec<String> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.pointer("/result/call_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// 从完整 user 文本块读取专用 JSON 契约；非 JSON 或其他 schema 视为普通文本。
fn explicit_user_constraint_ids(message: &Value) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Ok(ids);
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if value.get("schema").and_then(Value::as_str) != Some(USER_CONSTRAINT_MARKER_SCHEMA) {
            continue;
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("结构化用户约束缺少非空 `id`"))?;
        if value.get("value").is_none_or(Value::is_null) {
            return Err(anyhow!("结构化用户约束 `{id}` 缺少 `value`"));
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

/// 识别 plan-plugin 成功 ToolResult 中的最新完整只读快照。
fn plan_snapshot_revision(message: &Value, max_items: u16) -> Result<Option<u64>> {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut revision = None;
    for block in blocks {
        let Some(result) = block.get("result") else {
            continue;
        };
        if result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || !matches!(
                result.get("name").and_then(Value::as_str),
                Some("update_plan" | "get_plan")
            )
        {
            continue;
        }
        let Some(content) = result.get("content") else {
            continue;
        };
        if content.get("schema_version").and_then(Value::as_u64) != Some(1) {
            return Err(anyhow!("Plan 快照 schema_version 不受支持"));
        }
        let current_revision = content
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Plan 快照缺少 revision"))?;
        let plan = content
            .get("plan")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Plan 快照缺少 plan 数组"))?;
        if plan.len() > max_items as usize {
            return Err(anyhow!("Plan 快照步骤数超过 Context Policy 固定区上限"));
        }
        revision = Some(current_revision);
    }
    Ok(revision)
}

/// 构造摘要模型必须逐字返回的稳定标记要求。
fn summary_requirements(
    messages: &[Value],
    split_index: usize,
    policy: Option<&ContextPolicyV1>,
    pinned: &PinnedMessages,
) -> Result<SummaryRequirements> {
    let Some(policy) = policy else {
        return Ok(SummaryRequirements::default());
    };
    let constraint_ids = match &policy.user_constraints {
        UserConstraintRetentionPolicyV1::PinnedStructured { .. } => BTreeSet::new(),
        UserConstraintRetentionPolicyV1::PinnedStructuredAndSummary { .. } => {
            pinned.user_constraint_ids.clone()
        }
    };
    let plan_revision = match &policy.plan_snapshot {
        PlanSnapshotRetentionPolicyV1::LatestSnapshot { .. } => None,
        PlanSnapshotRetentionPolicyV1::LatestSnapshotAndSummary { .. } => pinned.plan_revision,
    };
    let mut tool_result_call_ids = BTreeSet::new();
    let mut fact_ids = BTreeSet::new();
    for message in messages.iter().take(split_index) {
        for id in explicit_fact_ids(message)? {
            fact_ids.insert(id);
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let call_id = block
                .pointer("/result/call_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow!("摘要范围内的 ToolResult 缺少 call_id"))?;
            tool_result_call_ids.insert(call_id.to_string());
        }
    }
    Ok(SummaryRequirements {
        schema_version: 1,
        constraint_ids,
        tool_result_call_ids,
        fact_ids,
        plan_revision,
    })
}

/// 从任意角色的完整文本块读取专用事实 JSON 契约；普通文本永远不参与事实识别。
fn explicit_fact_ids(message: &Value) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Ok(ids);
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if value.get("schema").and_then(Value::as_str) != Some(FACT_MARKER_SCHEMA) {
            continue;
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("结构化事实缺少非空 `id`"))?;
        if value.get("value").is_none_or(Value::is_null) {
            return Err(anyhow!("结构化事实 `{id}` 缺少 `value`"));
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

/// 按协议覆盖率逐字节验证模型回传标记，Plan 修订必须完全相等。
fn verify_summary_markers(
    markers: &SummaryMarkers,
    required: &SummaryRequirements,
    policy: &ContextPolicyV1,
) -> Result<()> {
    if markers.schema_version != 1 {
        return Err(anyhow!("摘要标记信封 schema_version 不受支持"));
    }
    if markers.plan_revision != required.plan_revision {
        return Err(anyhow!("摘要标记信封未确认最新 Plan 修订"));
    }
    let expected = required.constraint_ids.len()
        + required.tool_result_call_ids.len()
        + required.fact_ids.len();
    let recalled = required
        .constraint_ids
        .intersection(&markers.constraint_ids)
        .count()
        + required
            .tool_result_call_ids
            .intersection(&markers.tool_result_call_ids)
            .count()
        + required.fact_ids.intersection(&markers.fact_ids).count();
    let coverage_bps = (recalled * 10_000).checked_div(expected).unwrap_or(10_000) as u16;
    let PostSummaryValidationAlgorithmV1::StructuredMarkerCoverageV1 { min_coverage_bps } =
        policy.post_summary_validation;
    if coverage_bps < min_coverage_bps {
        return Err(anyhow!(
            "摘要结构化标记覆盖率不足：{coverage_bps} bps，要求 {min_coverage_bps} bps"
        ));
    }
    Ok(())
}

/// 对最终替换上下文逐值确认固定区消息未被摘要模型改写或遗漏。
fn verify_pinned_messages(
    compacted: &[Value],
    source: &[Value],
    pinned: &PinnedMessages,
) -> Result<bool> {
    for index in &pinned.indices {
        let expected = &source[*index];
        if !compacted.iter().any(|message| message == expected) {
            return Err(anyhow!("摘要后固定区消息未逐值保留"));
        }
    }
    Ok(true)
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
    policy: Option<&ContextPolicyV1>,
) -> usize {
    if let Some(policy) = policy {
        let target = messages
            .len()
            .saturating_sub(policy.recent_message_count as usize);
        if target == 0 {
            return 0;
        }
        return group_starts
            .iter()
            .rev()
            .copied()
            .find(|start| *start <= target)
            .unwrap_or(target);
    }
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
    requirements: &SummaryRequirements,
    max_tokens: u32,
) -> Result<ModelCompletionResponse> {
    let mut summary_messages = messages.to_vec();
    let request_prompt = if requirements.schema_version == 0 {
        SUMMARY_REQUEST_PROMPT.to_string()
    } else {
        let marker_contract =
            serde_json::to_string(requirements).context("编码 Context Policy 摘要标记要求失败")?;
        format!(
            "{SUMMARY_REQUEST_PROMPT}\n\n在 summary 标签之后逐字输出以下标记信封；不得增删 ID 或修改 Plan 修订：\n{SUMMARY_MARKERS_OPEN}{marker_contract}{SUMMARY_MARKERS_CLOSE}"
        )
    };
    summary_messages.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": request_prompt}]
    }));
    host.complete_model(&ModelCompletionRequest {
        system: Some(SUMMARY_SYSTEM_PROMPT.into()),
        messages: summary_messages,
        max_tokens: Some(max_tokens),
    })
    .context("调用模型生成上下文摘要失败")
}

/// 清理模型可能返回的分析草稿和 summary 标签，并拒绝空摘要。
fn normalize_model_summary(
    text: &str,
    require_markers: bool,
) -> Result<(String, Option<SummaryMarkers>)> {
    let without_analysis = remove_tagged_section(text, "<analysis>", "</analysis>");
    let (without_markers, markers) = extract_summary_markers(&without_analysis)?;
    if require_markers && markers.is_none() {
        return Err(anyhow!("模型摘要缺少结构化标记信封"));
    }
    let trimmed = without_markers.trim();
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
    let summary = match markers.as_ref() {
        Some(markers) => format!(
            "{summary}\n\n{SUMMARY_MARKERS_OPEN}{}{SUMMARY_MARKERS_CLOSE}",
            serde_json::to_string(markers).context("重新编码摘要结构化标记信封失败")?
        ),
        None => summary.to_string(),
    };
    Ok((summary, markers))
}

/// 从模型文本中提取唯一标记信封，并从摘要正文中移除该区块。
fn extract_summary_markers(text: &str) -> Result<(String, Option<SummaryMarkers>)> {
    let Some(start) = text.find(SUMMARY_MARKERS_OPEN) else {
        return Ok((text.to_string(), None));
    };
    if text[start + SUMMARY_MARKERS_OPEN.len()..].contains(SUMMARY_MARKERS_OPEN) {
        return Err(anyhow!("模型摘要包含重复的结构化标记信封"));
    }
    let content_start = start + SUMMARY_MARKERS_OPEN.len();
    let relative_end = text[content_start..]
        .find(SUMMARY_MARKERS_CLOSE)
        .ok_or_else(|| anyhow!("模型摘要结构化标记信封未闭合"))?;
    let end = content_start + relative_end;
    let markers: SummaryMarkers = serde_json::from_str(&text[content_start..end])
        .context("解析模型摘要结构化标记信封失败")?;
    let block_end = end + SUMMARY_MARKERS_CLOSE.len();
    Ok((
        format!("{}{}", &text[..start], &text[block_end..]),
        Some(markers),
    ))
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
    fn test_summary(
        _messages: &[Value],
        _requirements: &SummaryRequirements,
        _max_tokens: u32,
    ) -> Result<ModelCompletionResponse> {
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
        compress_context(request, manual, None, None, &test_summary).expect("测试压缩应成功")
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
        let summarize = |messages: &[Value], requirements: &SummaryRequirements, max_tokens| {
            summarized_messages.replace(messages.to_vec());
            test_summary(messages, requirements, max_tokens)
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
            None,
            None,
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
        let outcome = compress_context(request, true, None, None, &test_summary)
            .expect("主动压缩应立即返回结果");

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
