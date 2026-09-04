//! 基于远端模型的候选重排。
//!
//! 混合检索负责高召回，rerank 模型负责理解自然语言问题与候选源码之间的整体相关性。
//! 远端不可用时由调用方保留原有融合顺序，因此模型重排始终是可降级的质量增强项。

use crate::retrieval::{
    AnalyzedQuery, CandidateSource, ChunkRole, ConfidenceLevel, QueryGoal, QueryIntent,
    RetrievalConfidence, SearchResult, SymbolMatch,
};
use agent_plugin::{HttpRequest, PluginHostApi};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// 单次模型重排最多提交的候选数量。
const MAX_RERANK_CANDIDATES: usize = 48;

/// 本地名次与模型名次做 RRF 时的平滑常量。
///
/// 与 [`crate::retrieval::FusionConfig`] 取同一个值，使两处"排名融合"的量级一致。
const RRF_K: f32 = 60.0;

/// 模型重排允许施加的最大分数增量。
///
/// 取 0.75 个 rank-1 当量：足以区分本地分数接近的困难候选，但不足以覆盖精确符号、角色与
/// 多路一致性已经建立的强本地证据。
const MAX_RERANK_DELTA: f32 = 0.75 / (RRF_K + 1.0);

/// 每条候选最多携带的源码字符数。
///
/// 限制单条长度可以把 40 条左右的候选稳定控制在常见 rerank 上下文窗口内，同时保留
/// 足够的函数实现供模型判断调用流程，而不是只看符号名。
const MAX_SOURCE_CHARS: usize = 1_800;

/// 计算本地融合结果的可解释置信度。
///
/// 精确符号、多路一致、目标角色匹配和 Top-1 分差共同形成高置信度；文档压过实现、候选
/// 分差过小或通道分歧会保持低置信度。该判断不调用模型，也不读取源码。
pub(crate) fn assess_confidence(
    query: &AnalyzedQuery,
    candidates: &[SearchResult],
) -> RetrievalConfidence {
    let Some(top) = candidates.first() else {
        return RetrievalConfidence {
            level: ConfidenceLevel::Low,
            top_margin: 0.0,
            symbol_lexical_agreement: false,
            multi_channel_top: false,
            exact_symbol_hit: false,
            role_conflict: false,
            has_implementation: false,
            reasons: vec!["没有候选".to_string()],
        };
    };
    let top_margin = candidates.get(1).map_or(1.0, |second| {
        (top.score - second.score).max(0.0) / top.score.abs().max(f32::MIN_POSITIVE)
    });
    let symbol_lexical_agreement = [CandidateSource::Symbol, CandidateSource::Lexical]
        .into_iter()
        .all(|source| {
            top.debug
                .ranks
                .iter()
                .any(|(candidate, _)| *candidate == source)
        });
    // 同一通道可能以多种方式命中同一 chunk，按通道去重后再计数。
    let mut top_channels: Vec<CandidateSource> =
        top.debug.ranks.iter().map(|(source, _)| *source).collect();
    top_channels.sort_by_key(|source| source.as_str());
    top_channels.dedup();
    let multi_channel_top = top_channels.len() >= 2;
    let protection = protection_of(query, top);
    let exact_symbol_hit = matches!(
        protection,
        CandidateProtection::ExactQualified | CandidateProtection::ExactSymbol
    );
    let has_implementation = candidates
        .iter()
        .any(|candidate| candidate.role == ChunkRole::Implementation);
    let top_role_matches = role_matches_goal(query.goal, top.role);
    let role_conflict = !top_role_matches
        && candidates
            .iter()
            .skip(1)
            .any(|candidate| role_matches_goal(query.goal, candidate.role));
    let high = exact_symbol_hit
        || (symbol_lexical_agreement && top_role_matches)
        || (multi_channel_top && top_role_matches && !role_conflict)
        || (top_margin >= 0.20 && top_role_matches && !role_conflict);
    let mut reasons = Vec::new();
    if exact_symbol_hit {
        reasons.push("Top-1 为精确符号命中".to_string());
    }
    if multi_channel_top {
        reasons.push(format!("{} 路召回共同支持 Top-1", top_channels.len()));
    }
    if symbol_lexical_agreement {
        reasons.push("符号与词法通道一致支持 Top-1".to_string());
    }
    if top_role_matches {
        reasons.push(format!("Top-1 角色 {} 匹配查询目标", top.role.as_str()));
    }
    if role_conflict {
        reasons.push("Top-1 角色与后续目标角色候选冲突".to_string());
    }
    if top_margin < 0.20 {
        reasons.push(format!("Top-1 相对分差仅 {:.3}", top_margin));
    }
    RetrievalConfidence {
        level: if high {
            ConfidenceLevel::High
        } else {
            ConfidenceLevel::Low
        },
        top_margin,
        symbol_lexical_agreement,
        multi_channel_top,
        exact_symbol_hit,
        role_conflict,
        has_implementation,
        reasons,
    }
}

/// 判断本次查询是否需要调用远端模型重排。
///
/// 只有自然语言或混合查询、候选多于一条、本地置信度低，**且 Top-1 缺少多路佐证**时才
/// 返回 `true`。
///
/// 最后一条是实测加上的：低置信度只说明"本地没把握"，不等于"模型能做得更好"。当 Top-1
/// 已被两路及以上召回器共同选中时，本地排序已有相互独立的证据支撑，此时再花一次远端
/// 往返，模型往往只是把正确结果往下压——实测中一条中文查询的三个候选拿到的增量全是负值，
/// 其中语义第一名被压得最狠，净效果是多付一秒并让排序变差。
pub(crate) fn should_rerank(
    query: &AnalyzedQuery,
    candidates: &[SearchResult],
    confidence: &RetrievalConfidence,
) -> bool {
    candidates.len() > 1
        && confidence.level == ConfidenceLevel::Low
        && !confidence.multi_channel_top
        && matches!(
            query.intent,
            QueryIntent::NaturalLanguage | QueryIntent::Mixed
        )
}

/// 判断代码角色是否直接满足查询目标。
fn role_matches_goal(goal: QueryGoal, role: ChunkRole) -> bool {
    match goal {
        QueryGoal::Definition => matches!(role, ChunkRole::Definition | ChunkRole::TypeDefinition),
        QueryGoal::Implementation | QueryGoal::Behavior => role == ChunkRole::Implementation,
        QueryGoal::CallSite => role == ChunkRole::CallSite,
        QueryGoal::TypeLookup => role == ChunkRole::TypeDefinition,
        QueryGoal::Concept => matches!(
            role,
            ChunkRole::Documentation
                | ChunkRole::Definition
                | ChunkRole::TypeDefinition
                | ChunkRole::Config
        ),
    }
}

/// 计算模型重排前的召回数量。
///
/// 默认扩大四倍，为语义相近但初始排名偏低的调用链节点保留进入模型重排的机会。请求数量
/// 本身超过上限时仍以请求数量为准，保证调用方不会得到少于期望数量的结果。
pub(crate) fn candidate_limit(limit: usize) -> usize {
    limit
        .saturating_mul(4)
        .min(MAX_RERANK_CANDIDATES.max(limit))
}

/// 使用宿主 HTTP 能力调用兼容 `/v1/rerank` 的模型端点。
///
/// `results` 必须保持初始融合顺序；该顺序用于模型分数相同时稳定决胜。请求会按文件缓存
/// 源码读取，每个文件最多读取一次，并把路径、符号、签名、文档注释和对应源码一并交给
/// rerank 模型。
///
/// # Errors
///
/// 宿主 HTTP 调用失败、端点返回非 2xx、响应结构非法、下标重复或缺失时返回可读错误。
/// 调用方应保留输入候选作为降级结果。
pub(crate) fn rerank_results(
    host: &dyn PluginHostApi,
    model: &str,
    query: &str,
    results: &[SearchResult],
) -> Result<Vec<SearchResult>, String> {
    if results.len() <= 1 {
        return Ok(results.to_vec());
    }

    let mut sources: HashMap<String, Option<String>> = HashMap::new();
    let documents: Vec<String> = results
        .iter()
        .map(|result| candidate_document(host, result, &mut sources))
        .collect();
    let body = serde_json::json!({
        "model": model,
        "query": query,
        "documents": documents,
        "top_n": results.len(),
        "return_documents": false,
    });
    let response = host
        .http_request(&HttpRequest {
            method: "POST".to_string(),
            path: "rerank".to_string(),
            headers: HashMap::from([("content-type".to_string(), "application/json".to_string())]),
            body: Some(body.to_string()),
        })
        .map_err(|error| format!("rerank 端点调用失败：{error}"))?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "rerank 端点返回 HTTP {}：{}",
            response.status,
            truncate_for_message(&response.body)
        ));
    }
    apply_response(&response.body, query, results.to_vec())
}

/// 构造一条供模型判断的候选文档。
fn candidate_document(
    host: &dyn PluginHostApi,
    result: &SearchResult,
    sources: &mut HashMap<String, Option<String>>,
) -> String {
    let source = sources
        .entry(result.file_path.clone())
        .or_insert_with(|| host.read_file(&result.file_path).ok())
        .as_deref();
    let snippet = source
        .and_then(|content| slice_lines(content, result.start_line, result.end_line))
        .map(|content| truncate_chars(&content, MAX_SOURCE_CHARS))
        .unwrap_or_default();

    let mut document = format!(
        "文件：{}\n语言：{}\n行号：{}-{}",
        result.file_path, result.language, result.start_line, result.end_line
    );
    if let Some(symbol) = result
        .qualified_symbol
        .as_deref()
        .or(result.symbol_name.as_deref())
    {
        document.push_str("\n符号：");
        document.push_str(symbol);
    }
    if let Some(kind) = result.symbol_kind {
        document.push_str(&format!("\n类型：{kind:?}"));
    }
    if let Some(signature) = result.signature.as_deref() {
        document.push_str("\n签名：");
        document.push_str(signature.trim());
    }
    if let Some(comment) = result.doc_comment.as_deref() {
        document.push_str("\n说明：");
        document.push_str(comment.trim());
    }
    if !snippet.is_empty() {
        document.push_str("\n源码：\n");
        document.push_str(&snippet);
    }
    document
}

/// 候选需要保留的最低排名保护级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateProtection {
    /// 精确限定名命中，最强保护。
    ExactQualified,
    /// 精确短名或高置信度定义命中。
    ExactSymbol,
    /// 强词法与实现角色共同命中。
    StrongImplementation,
    /// 被两路及以上召回器共同召回。
    ///
    /// 多路一致是相互独立的佐证，强度弱于精确符号命中，但足以要求模型重排不得把它压出
    /// 前列——模型只看候选文本，看不到"这条同时被词法和语义选中"这个事实。
    MultiChannel,
    /// 不需要排名保护。
    None,
}

impl CandidateProtection {
    /// 返回该保护级别允许的最差零基排名。
    fn rank_floor(self) -> Option<usize> {
        match self {
            Self::ExactQualified => Some(0),
            Self::ExactSymbol => Some(2),
            Self::StrongImplementation => Some(4),
            Self::MultiChannel => Some(5),
            Self::None => None,
        }
    }
}

/// 模型调整后的一条候选及其稳定决胜信息。
struct RerankedResult {
    /// 模型原始相关度。
    relevance: f32,
    /// 模型重排前的本地名次。
    local_rank: usize,
    /// 已施加有界增量的候选。
    result: SearchResult,
}

/// 把 rerank 响应映射回候选，并在本地融合分上施加有界调整。
fn apply_response(
    body: &str,
    query: &str,
    results: Vec<SearchResult>,
) -> Result<Vec<SearchResult>, String> {
    /// 端点返回的一条重排结果。
    #[derive(Deserialize)]
    struct RerankItem {
        /// 对应请求 `documents` 的下标。
        index: usize,
        /// 模型给出的相关度分数。
        relevance_score: f32,
    }

    /// rerank 响应信封。
    #[derive(Deserialize)]
    struct RerankEnvelope {
        /// 已按相关性计算的候选结果。
        results: Vec<RerankItem>,
    }

    let expected = results.len();
    let envelope: RerankEnvelope = serde_json::from_str(body).map_err(|error| {
        format!(
            "解析 rerank 响应失败：{error}；响应片段：{}",
            truncate_for_message(body)
        )
    })?;
    if envelope.results.len() != expected {
        return Err(format!(
            "rerank 响应返回 {} 条结果，与提交的 {expected} 条候选不符",
            envelope.results.len()
        ));
    }

    let protections = strongest_protections(query, &results);
    let mut seen = HashSet::with_capacity(expected);
    let mut slots: Vec<Option<SearchResult>> = results.into_iter().map(Some).collect();
    // 先按模型分数排出模型自己的名次，再与本地融合名次做一次 RRF。
    let mut model_order: Vec<(f32, usize)> = Vec::with_capacity(expected);
    for item in &envelope.results {
        if !item.relevance_score.is_finite() {
            return Err(format!("rerank 响应的 index {} 分数不是有限数", item.index));
        }
        if !seen.insert(item.index) {
            return Err(format!("rerank 响应的 index {} 重复", item.index));
        }
        if item.index >= expected {
            return Err(format!(
                "rerank 响应的 index {} 超出候选数量 {expected}",
                item.index
            ));
        }
        model_order.push((item.relevance_score, item.index));
    }
    model_order.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)));

    let denominator = expected.saturating_sub(1).max(1) as f32;
    let mut scored = Vec::with_capacity(expected);
    for (model_rank, (relevance, index)) in model_order.into_iter().enumerate() {
        let mut result = slots
            .get_mut(index)
            .and_then(Option::take)
            .ok_or_else(|| format!("rerank 响应的 index {index} 重复"))?;
        // `results` 传入时已按本地融合分降序，因此下标即本地名次。模型只能按名次差施加
        // 有界增量，不能把原始相关度直接覆盖本地结构分。
        let rank_shift = (index as f32 - model_rank as f32) / denominator;
        let delta = rank_shift.clamp(-1.0, 1.0) * MAX_RERANK_DELTA;
        result.score += delta;
        result.debug.model_rerank_score = Some(relevance);
        result.debug.model_rerank_delta = delta;
        scored.push(RerankedResult {
            relevance,
            local_rank: index,
            result,
        });
    }

    // 分数之后使用模型分、原本本地名次、路径、符号与 chunk ID 稳定决胜。
    scored.sort_by(|left, right| {
        right
            .result
            .score
            .total_cmp(&left.result.score)
            .then_with(|| right.relevance.total_cmp(&left.relevance))
            .then_with(|| left.local_rank.cmp(&right.local_rank))
            .then_with(|| left.result.file_path.cmp(&right.result.file_path))
            .then_with(|| {
                left.result
                    .qualified_symbol
                    .as_deref()
                    .or(left.result.symbol_name.as_deref())
                    .cmp(
                        &right
                            .result
                            .qualified_symbol
                            .as_deref()
                            .or(right.result.symbol_name.as_deref()),
                    )
            })
            .then_with(|| left.result.chunk_id.cmp(&right.result.chunk_id))
    });
    apply_rank_floors(&mut scored, &protections);
    Ok(scored.into_iter().map(|item| item.result).collect())
}

/// 计算候选的强命中保护级别。
fn protection_of(query: &AnalyzedQuery, result: &SearchResult) -> CandidateProtection {
    let identifier_matches = query.identifiers.iter().any(|identifier| {
        result
            .symbol_name
            .as_deref()
            .is_some_and(|symbol| symbol.eq_ignore_ascii_case(identifier))
            || result
                .qualified_symbol
                .as_deref()
                .is_some_and(|symbol| symbol.eq_ignore_ascii_case(identifier))
    });
    if query.intent == QueryIntent::QualifiedSymbol
        && result.debug.symbol_match == Some(SymbolMatch::ExactQualified)
        && identifier_matches
    {
        return CandidateProtection::ExactQualified;
    }
    if matches!(
        query.intent,
        QueryIntent::Identifier | QueryIntent::QualifiedSymbol | QueryIntent::Mixed
    ) && matches!(
        result.debug.symbol_match,
        Some(SymbolMatch::ExactQualified | SymbolMatch::ExactName)
    ) && identifier_matches
    {
        return CandidateProtection::ExactSymbol;
    }
    let lexical_rank = result
        .debug
        .ranks
        .iter()
        .find_map(|(source, rank)| (*source == CandidateSource::Lexical).then_some(*rank));
    let symbol_rank = result
        .debug
        .ranks
        .iter()
        .find_map(|(source, rank)| (*source == CandidateSource::Symbol).then_some(*rank));
    let metadata_coverage = if result.debug.rerank_query_tokens == 0 {
        0.0
    } else {
        result.debug.rerank_matched_tokens as f32 / result.debug.rerank_query_tokens as f32
    };
    if matches!(query.goal, QueryGoal::Implementation | QueryGoal::Behavior)
        && result.role == ChunkRole::Implementation
        && lexical_rank.is_some_and(|rank| rank <= 3)
        && (symbol_rank.is_some_and(|rank| rank <= 5) || metadata_coverage >= 0.5)
    {
        return CandidateProtection::StrongImplementation;
    }
    // 多路一致：两个相互独立的召回器同时选中它。模型只看候选文本，看不到这个事实，
    // 因此不能让它把这类候选压出前列。
    let mut channels: Vec<CandidateSource> = result
        .debug
        .ranks
        .iter()
        .map(|(source, _)| *source)
        .collect();
    channels.sort_by_key(|source| source.as_str());
    channels.dedup();
    if channels.len() >= 2 {
        return CandidateProtection::MultiChannel;
    }
    CandidateProtection::None
}

/// 从本地顺序选择每类最强候选，避免多个同名重载争抢同一个保护位置。
fn strongest_protections(query: &str, results: &[SearchResult]) -> Vec<(String, usize)> {
    let analyzed = AnalyzedQuery::analyze(query);
    let mut selected = Vec::new();
    for level in [
        CandidateProtection::ExactQualified,
        CandidateProtection::ExactSymbol,
        CandidateProtection::StrongImplementation,
        CandidateProtection::MultiChannel,
    ] {
        let Some(result) = results
            .iter()
            .find(|result| protection_of(&analyzed, result) == level)
        else {
            continue;
        };
        if selected
            .iter()
            .any(|(chunk_id, _)| chunk_id == &result.chunk_id)
        {
            continue;
        }
        if let Some(floor) = level.rank_floor() {
            selected.push((result.chunk_id.clone(), floor));
        }
    }
    selected
}

/// 把受保护候选移回允许的最差排名，其他候选保持当前相对顺序。
fn apply_rank_floors(results: &mut Vec<RerankedResult>, protections: &[(String, usize)]) {
    for (chunk_id, floor) in protections {
        let Some(position) = results
            .iter()
            .position(|item| item.result.chunk_id == *chunk_id)
        else {
            continue;
        };
        if position <= *floor {
            continue;
        }
        let item = results.remove(position);
        results.insert((*floor).min(results.len()), item);
    }
}

/// 读取闭区间行范围；范围失效时返回 `None`。
fn slice_lines(content: &str, start_line: u32, end_line: u32) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start = start_line.saturating_sub(1) as usize;
    let end = (end_line as usize).min(lines.len());
    (start < end).then(|| lines[start..end].join("\n"))
}

/// 按字符边界截断候选正文。
fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((offset, _)) => format!("{}…", &text[..offset]),
        None => text.to_string(),
    }
}

/// 截断用于错误信息的远端响应，避免日志被大段 HTML 或 JSON 占满。
fn truncate_for_message(text: &str) -> String {
    truncate_chars(text.trim(), 256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SymbolKind;
    use crate::retrieval::{ChunkRole, FusionDebug};
    use crate::testing::{http_json, RepoHost};

    /// 构造最小候选，便于聚焦 rerank 协议本身。
    fn result(id: &str, symbol: &str, start_line: u32, score: f32) -> SearchResult {
        SearchResult {
            chunk_id: id.to_string(),
            file_path: "src/agent.rs".to_string(),
            start_line,
            end_line: start_line + 2,
            language: "rust".to_string(),
            symbol_name: Some(symbol.to_string()),
            qualified_symbol: Some(format!("Agent::{symbol}")),
            symbol_kind: Some(SymbolKind::Function),
            parent_symbol: Some("Agent".to_string()),
            signature: Some(format!("pub fn {symbol}(&self)")),
            doc_comment: Some(format!("执行 {symbol}。")),
            role: ChunkRole::Implementation,
            score,
            debug: FusionDebug {
                ranks: Vec::new(),
                rrf_score: score,
                symbol_boost: 0.0,
                path_boost: 0.0,
                rerank_boost: 0.0,
                role_boost: 0.0,
                model_rerank_score: None,
                model_rerank_delta: 0.0,
                penalty: 0.0,
                rerank_matched_tokens: 0,
                rerank_query_tokens: 0,
                symbol_match: None,
                role: ChunkRole::Implementation,
            },
        }
    }

    /// 模型分数应能重排本地分接近的候选，并且请求必须包含真实源码。
    #[test]
    fn model_score_reorders_candidates_with_source_documents() {
        let host = RepoHost::new(&[(
            "src/agent.rs",
            "fn unrelated() {}\n\n/// 准备会话。\npub fn prepare_session() {}\n\n/// 运行会话。\npub fn run_session() {}\n",
        )])
        .with_http_responses(vec![http_json(
            200,
            r#"{"results":[{"index":1,"relevance_score":0.93},{"index":0,"relevance_score":0.12}]}"#,
        )]);

        let reranked = rerank_results(
            &host,
            "Qwen/Qwen3-Reranker-4B",
            "Agent 怎么进行对话",
            &[
                result("first", "unrelated", 1, 0.020),
                result("second", "prepare_session", 3, 0.019),
            ],
        )
        .expect("模型重排应成功");

        assert_eq!(reranked[0].chunk_id, "second");
        assert_eq!(reranked[0].debug.model_rerank_score, Some(0.93));
        assert!(reranked[0].debug.model_rerank_delta > 0.0);
        let requests = host.http_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "rerank");
        let body: serde_json::Value =
            serde_json::from_str(requests[0].body.as_deref().expect("rerank 请求应有正文"))
                .expect("请求正文应为 JSON");
        assert_eq!(body["top_n"], 2);
        assert_eq!(body["return_documents"], false);
        assert!(body["documents"][1]
            .as_str()
            .is_some_and(|document| document.contains("pub fn prepare_session")));
    }

    /// 响应少返回候选时必须失败，调用方才能完整保留本地排序。
    #[test]
    fn incomplete_response_is_rejected() {
        let error = apply_response(
            r#"{"results":[{"index":0,"relevance_score":0.9}]}"#,
            "候选排序",
            vec![
                result("first", "first", 1, 0.2),
                result("second", "second", 2, 0.1),
            ],
        )
        .expect_err("缺少候选的响应必须失败");

        assert!(error.contains("与提交的 2 条候选不符"), "{error}");
    }

    /// 模型把本地第一名压到末位时，排名融合必须保留它的竞争力。
    ///
    /// 纯模型分数排序下，一次误判就能把正确结果打到最后；与本地名次做 RRF 后，
    /// 只有两边都认为不相关的候选才会真的沉下去。
    #[test]
    fn hostile_model_cannot_bury_the_local_top_result() {
        let mut candidates: Vec<SearchResult> = (0..8)
            .map(|index| result(&format!("chunk-{index}"), &format!("symbol{index}"), 1, 1.0))
            .collect();
        candidates[0].debug.symbol_match = Some(SymbolMatch::ExactName);
        // 模型给出与本地完全相反的顺序。
        let items: Vec<String> = (0..8)
            .map(|index| {
                format!(
                    r#"{{"index":{index},"relevance_score":{:.2}}}"#,
                    index as f32 / 10.0
                )
            })
            .collect();

        let reranked = apply_response(
            &format!(r#"{{"results":[{}]}}"#, items.join(",")),
            "symbol0",
            candidates,
        )
        .expect("重排应成功");

        let position = reranked
            .iter()
            .position(|item| item.chunk_id == "chunk-0")
            .expect("本地第一名必须仍在结果中");
        assert!(
            position <= 2,
            "精确符号被模型判为最差时仍应留在 Top-3，实际排在第 {}",
            position + 1
        );
    }

    /// 精确限定名即使被模型判为最差，也必须保持第一名。
    #[test]
    fn exact_qualified_symbol_keeps_top_one_rank_floor() {
        let mut candidates = vec![
            result("qualified", "remove_chunks", 1, 0.02),
            result("other", "remove_file", 4, 0.019),
            result("third", "search", 7, 0.018),
        ];
        candidates[0].debug.symbol_match = Some(SymbolMatch::ExactQualified);
        let reranked = apply_response(
            r#"{"results":[
                {"index":2,"relevance_score":0.9},
                {"index":1,"relevance_score":0.8},
                {"index":0,"relevance_score":0.0}
            ]}"#,
            "Agent::remove_chunks",
            candidates,
        )
        .expect("重排应成功");

        assert_eq!(reranked[0].chunk_id, "qualified");
    }

    /// 强词法与实现角色共同命中时，模型不得把候选移出 Top-5 保护窗口。
    #[test]
    fn strong_implementation_hit_keeps_protected_window() {
        let mut candidates: Vec<SearchResult> = (0..8)
            .map(|index| result(&format!("chunk-{index}"), &format!("symbol{index}"), 1, 1.0))
            .collect();
        candidates[0].debug.ranks =
            vec![(CandidateSource::Lexical, 1), (CandidateSource::Symbol, 3)];
        candidates[0].debug.rerank_matched_tokens = 2;
        candidates[0].debug.rerank_query_tokens = 3;
        let items: Vec<String> = (0..8)
            .map(|index| {
                format!(
                    r#"{{"index":{index},"relevance_score":{:.2}}}"#,
                    index as f32 / 10.0
                )
            })
            .collect();

        let reranked = apply_response(
            &format!(r#"{{"results":[{}]}}"#, items.join(",")),
            "插件管理页面怎么绘制",
            candidates,
        )
        .expect("重排应成功");

        let position = reranked
            .iter()
            .position(|item| item.chunk_id == "chunk-0")
            .expect("强实现候选必须仍在结果中");
        assert!(
            position <= 4,
            "强实现候选应保持 Top-5，实际第 {}",
            position + 1
        );
    }

    /// 两边一致认为不相关的候选仍应沉到末位，否则融合就成了无视模型。
    #[test]
    fn agreed_irrelevant_candidate_still_sinks() {
        let candidates: Vec<SearchResult> = (0..4)
            .map(|index| result(&format!("chunk-{index}"), &format!("symbol{index}"), 1, 1.0))
            .collect();
        // 本地末位的 chunk-3 同时被模型判为最差。
        let reranked = apply_response(
            r#"{"results":[
                {"index":0,"relevance_score":0.9},
                {"index":1,"relevance_score":0.8},
                {"index":2,"relevance_score":0.7},
                {"index":3,"relevance_score":0.0}
            ]}"#,
            "候选排序",
            candidates,
        )
        .expect("重排应成功");

        assert_eq!(
            reranked.last().expect("结果非空").chunk_id,
            "chunk-3",
            "两边一致判为不相关的候选必须排在末位"
        );
    }

    /// Top-1 已被多路召回共同支持时不得再触发模型重排。
    ///
    /// 低置信度只说明"本地没把握"，不等于"模型能做得更好"。实测中一条中文查询的三个
    /// 候选拿到的重排增量全为负，其中语义第一名被压得最狠——多付一秒还让排序变差。
    #[test]
    fn multi_channel_top_skips_rerank() {
        let query = AnalyzedQuery::analyze("权限检查在哪里实现");
        let mut agreed = result("agreed", "check_permission", 1, 0.9);
        agreed.debug.ranks = vec![
            (CandidateSource::Lexical, 3),
            (CandidateSource::Semantic, 1),
        ];
        agreed.role = ChunkRole::Documentation;
        let mut single = result("single", "other", 2, 0.89);
        single.debug.ranks = vec![(CandidateSource::Semantic, 2)];
        single.role = ChunkRole::Documentation;

        let candidates = vec![agreed.clone(), single.clone()];
        let confidence = assess_confidence(&query, &candidates);
        assert!(confidence.multi_channel_top, "Top-1 应被判为多路一致");
        assert!(
            !should_rerank(&query, &candidates, &confidence),
            "多路一致的 Top-1 不应触发重排"
        );

        // 只有单路支持时仍然允许重排，避免把这条规则变成"永不重排"。
        let single_only = vec![single, agreed];
        let confidence = assess_confidence(&query, &single_only);
        assert!(!confidence.multi_channel_top);
        assert!(should_rerank(&query, &single_only, &confidence));
    }

    /// 多路一致的候选不得被模型重排压出前列。
    #[test]
    fn multi_channel_candidate_keeps_rank_floor() {
        let mut agreed = result("agreed", "check_permission", 1, 0.9);
        agreed.debug.ranks = vec![
            (CandidateSource::Lexical, 2),
            (CandidateSource::Semantic, 1),
        ];
        let mut others: Vec<SearchResult> = (0..9)
            .map(|index| {
                let mut item = result(&format!("other-{index}"), &format!("other{index}"), 2, 0.5);
                item.debug.ranks = vec![(CandidateSource::Semantic, index as u32 + 2)];
                item
            })
            .collect();
        let mut candidates = vec![agreed];
        candidates.append(&mut others);

        // 模型把多路一致的候选判为最差。
        let items: Vec<String> = (0..candidates.len())
            .map(|index| {
                let score = if index == 0 {
                    0.0
                } else {
                    1.0 - index as f32 / 100.0
                };
                format!(r#"{{"index":{index},"relevance_score":{score}}}"#)
            })
            .collect();
        let reranked = apply_response(
            &format!(r#"{{"results":[{}]}}"#, items.join(",")),
            "权限检查在哪里实现",
            candidates,
        )
        .expect("重排应成功");

        let position = reranked
            .iter()
            .position(|item| item.chunk_id == "agreed")
            .expect("多路一致候选应仍在结果中");
        assert!(
            position <= 5,
            "多路一致候选不得被压到第 {} 位",
            position + 1
        );
    }

    /// 默认召回深度必须扩大，但不能无限增加远端输入。
    #[test]
    fn candidate_pool_is_expanded_and_bounded() {
        assert_eq!(candidate_limit(10), 40);
        assert_eq!(candidate_limit(20), 48);
        assert_eq!(candidate_limit(50), 50);
    }

    /// 精确符号命中必须形成高置信度，并跳过模型重排。
    #[test]
    fn exact_symbol_is_high_confidence_without_rerank() {
        let mut candidates = vec![result("exact", "split_camel_case", 1, 0.05)];
        candidates[0].debug.symbol_match = Some(SymbolMatch::ExactName);
        candidates[0].debug.ranks =
            vec![(CandidateSource::Symbol, 1), (CandidateSource::Lexical, 1)];
        let query = AnalyzedQuery::analyze("split_camel_case");

        let confidence = assess_confidence(&query, &candidates);

        assert_eq!(confidence.level, ConfidenceLevel::High);
        assert!(confidence.exact_symbol_hit);
        assert!(!should_rerank(&query, &candidates, &confidence));
    }

    /// 实现查询中文档压过实现且分差接近时必须进入低置信度模型重排。
    #[test]
    fn documentation_implementation_conflict_triggers_rerank() {
        let mut candidates = vec![
            result("docs", "plugin_management", 1, 0.020),
            result("impl", "render_plugin_dialog", 4, 0.019),
        ];
        candidates[0].role = ChunkRole::Documentation;
        candidates[0].debug.role = ChunkRole::Documentation;
        let query = AnalyzedQuery::analyze("插件管理页面怎么绘制");

        let confidence = assess_confidence(&query, &candidates);

        assert_eq!(confidence.level, ConfidenceLevel::Low);
        assert!(confidence.role_conflict);
        assert!(should_rerank(&query, &candidates, &confidence));
    }
}
