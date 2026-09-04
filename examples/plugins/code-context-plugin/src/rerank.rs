//! 基于远端模型的候选重排。
//!
//! 混合检索负责高召回，rerank 模型负责理解自然语言问题与候选源码之间的整体相关性。
//! 远端不可用时由调用方保留原有融合顺序，因此模型重排始终是可降级的质量增强项。

use crate::retrieval::SearchResult;
use agent_plugin::{HttpRequest, PluginHostApi};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// 单次模型重排最多提交的候选数量。
const MAX_RERANK_CANDIDATES: usize = 48;

/// 每条候选最多携带的源码字符数。
///
/// 限制单条长度可以把 40 条左右的候选稳定控制在常见 rerank 上下文窗口内，同时保留
/// 足够的函数实现供模型判断调用流程，而不是只看符号名。
const MAX_SOURCE_CHARS: usize = 1_800;

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
    apply_response(&response.body, results.to_vec())
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

/// 把 rerank 响应映射回候选并按模型分数排序。
fn apply_response(body: &str, results: Vec<SearchResult>) -> Result<Vec<SearchResult>, String> {
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

    let mut seen = HashSet::with_capacity(expected);
    let mut slots: Vec<Option<SearchResult>> = results.into_iter().map(Some).collect();
    let mut scored = Vec::with_capacity(expected);
    for item in envelope.results {
        if !item.relevance_score.is_finite() {
            return Err(format!("rerank 响应的 index {} 分数不是有限数", item.index));
        }
        if !seen.insert(item.index) {
            return Err(format!("rerank 响应的 index {} 重复", item.index));
        }
        let slot = slots
            .get_mut(item.index)
            .ok_or_else(|| format!("rerank 响应的 index {} 超出候选数量 {expected}", item.index))?;
        let mut result = slot
            .take()
            .ok_or_else(|| format!("rerank 响应的 index {} 重复", item.index))?;
        let fusion_score = result.score;
        result.score = item.relevance_score;
        scored.push((item.relevance_score, fusion_score, result));
    }

    scored.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| right.1.total_cmp(&left.1))
            .then_with(|| left.2.chunk_id.cmp(&right.2.chunk_id))
    });
    Ok(scored.into_iter().map(|(_, _, result)| result).collect())
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
    use crate::retrieval::FusionDebug;
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
            score,
            debug: FusionDebug {
                ranks: Vec::new(),
                rrf_score: score,
                symbol_boost: 0.0,
                path_boost: 0.0,
                rerank_boost: 0.0,
                rerank_matched_tokens: 0,
                rerank_query_tokens: 0,
                symbol_match: None,
            },
        }
    }

    /// 模型分数必须成为最终排序依据，并且请求必须包含真实源码。
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
                result("first", "unrelated", 1, 0.9),
                result("second", "prepare_session", 3, 0.2),
            ],
        )
        .expect("模型重排应成功");

        assert_eq!(reranked[0].chunk_id, "second");
        assert_eq!(reranked[0].score, 0.93);
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
            vec![
                result("first", "first", 1, 0.2),
                result("second", "second", 2, 0.1),
            ],
        )
        .expect_err("缺少候选的响应必须失败");

        assert!(error.contains("与提交的 2 条候选不符"), "{error}");
    }

    /// 默认召回深度必须扩大，但不能无限增加远端输入。
    #[test]
    fn candidate_pool_is_expanded_and_bounded() {
        assert_eq!(candidate_limit(10), 40);
        assert_eq!(candidate_limit(20), 48);
        assert_eq!(candidate_limit(50), 50);
    }
}
