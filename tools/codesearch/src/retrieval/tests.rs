//! 检索通路集成测试。
//!
//! 使用真实解析、真实 SQLite/FTS5 与真实 USearch 索引，只有 Embedding 由确定性
//! Mock 提供。因此这些断言验证的是**通路与排序机制**是否正确，
//! 不能用来证明真实语义检索质量。

use super::*;
use crate::config::Config;
use crate::indexer::prepare_file;
use crate::models::mock::MockProvider;
use crate::store::EmbeddingSpaceRow;
use crate::types::{Language, MatchSource, ScopeFilter};

const DIM: usize = 256;

/// 测试用 fixture：一小份 Rust + TypeScript 代码。
const RETRY_RS: &str = r#"use std::time::Duration;

/// 请求失败后的重试策略。
pub struct RetryPolicy {
    pub max_attempts: u32,
}

impl RetryPolicy {
    /// 执行带退避的重试，直到成功或耗尽尝试次数。
    pub fn execute(&self, request: &str) -> Result<String, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            if attempt >= self.max_attempts {
                return Err("connection reset by peer".to_string());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
"#;

const PARSER_RS: &str = r#"/// JSON 词法分析器。
pub struct JsonLexer {
    input: String,
}

impl JsonLexer {
    /// 读取下一个 token。
    pub fn next_token(&mut self) -> Option<String> {
        self.input.pop().map(|c| c.to_string())
    }
}
"#;

const RETRY_TEST_RS: &str = r#"#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_gives_up_after_max_attempts() {
        let policy = RetryPolicy { max_attempts: 3 };
        assert!(policy.execute("ping").is_err());
    }
}
"#;

const CLIENT_TS: &str = r#"export class HttpClient {
  async fetchWithRetry(url: string): Promise<Response> {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      try {
        return await fetch(url);
      } catch (error) {
        continue;
      }
    }
    throw new Error("request failed after retries");
  }
}
"#;

struct Fixture {
    store: Store,
    config: Config,
    space: EmbeddingSpaceRow,
    provider: MockProvider,
    index: VectorIndex,
}

impl Fixture {
    /// 建立索引：解析 → 分块 → 提交 → 生成 Mock 向量 → 重放到向量索引。
    fn build(vectors: bool) -> Self {
        let config = Config::default();
        let mut store = Store::open_in_memory().expect("store");
        let provider = MockProvider::with_model("mock-retrieval", DIM);
        let space_id = provider.space().id.clone();
        let space = EmbeddingSpaceRow {
            id: space_id.clone(),
            provider: crate::models::mock::PROVIDER_KIND.into(),
            model: "mock-retrieval".into(),
            cache_namespace: config.embedding.cache_namespace.clone(),
            dimensions: DIM,
            metric: "cosine".into(),
            normalize: "l2".into(),
            query_prefix: config.embedding.query_prefix.clone(),
            document_prefix: config.embedding.document_prefix.clone(),
            template_version: crate::chunker::EMBED_TEMPLATE_VERSION,
        };
        store.ensure_embedding_space(&space).expect("space");

        let files = [
            ("src/network/retry.rs", Language::Rust, RETRY_RS),
            ("src/json/parser.rs", Language::Rust, PARSER_RS),
            ("tests/retry_test.rs", Language::Rust, RETRY_TEST_RS),
            ("web/src/client.ts", Language::TypeScript, CLIENT_TS),
        ];
        for (path, language, text) in files {
            commit(&mut store, &config, path, language, text);
        }

        let mut index = VectorIndex::in_memory(DIM).expect("index");
        if vectors {
            embed_all(&mut store, &space, &provider, &mut index);
        }
        Self {
            store,
            config,
            space,
            provider,
            index,
        }
    }

    /// 按原始查询串执行检索，返回候选与降级信息。
    fn search(&self, query: &str, use_vectors: bool) -> RetrievalOutcome {
        let plan = analyze(query, &ScopeFilter::default()).expect("analyze");
        self.search_plan(&plan, use_vectors)
    }

    fn search_scoped(
        &self,
        query: &str,
        scope: ScopeFilter,
        use_vectors: bool,
    ) -> RetrievalOutcome {
        let plan = analyze(query, &scope).expect("analyze");
        self.search_plan(&plan, use_vectors)
    }

    fn search_plan(&self, plan: &QueryPlan, use_vectors: bool) -> RetrievalOutcome {
        let vector = if use_vectors {
            Some(query_vector(&self.provider, plan))
        } else {
            None
        };
        let input = RetrievalInput {
            plan,
            query_vector: vector.as_deref(),
            space_id: if use_vectors {
                Some(&self.space.id)
            } else {
                None
            },
        };
        let index = if use_vectors { Some(&self.index) } else { None };
        retrieve(&self.store, index, &self.config.retrieval, &input).expect("retrieve")
    }

    /// 候选对应的路径列表，按候选顺序。
    fn paths(&self, outcome: &RetrievalOutcome) -> Vec<String> {
        outcome
            .candidates
            .iter()
            .map(|candidate| {
                self.store
                    .chunk_by_id(candidate.chunk_id)
                    .expect("row")
                    .expect("exists")
                    .path
            })
            .collect()
    }

    /// 候选对应的符号列表，按候选顺序。
    ///
    /// 注意这是代码块的展示符号：体量不大的 `impl` 会整体保留为一块，
    /// 此时块的符号是容器名，方法名只登记在符号表里。
    fn symbols(&self, outcome: &RetrievalOutcome) -> Vec<String> {
        outcome
            .candidates
            .iter()
            .filter_map(|candidate| {
                self.store
                    .chunk_by_id(candidate.chunk_id)
                    .expect("row")
                    .expect("exists")
                    .symbol
            })
            .collect()
    }

    /// 候选对应的原始代码，按候选顺序。
    fn contents(&self, outcome: &RetrievalOutcome) -> Vec<String> {
        outcome
            .candidates
            .iter()
            .map(|candidate| self.content_of(candidate.chunk_id))
            .collect()
    }

    fn content_of(&self, chunk_id: i64) -> String {
        let row = self
            .store
            .chunk_by_id(chunk_id)
            .expect("row")
            .expect("exists");
        self.store
            .chunk_content(row.revision_id, row.start_byte, row.end_byte)
            .expect("content")
    }
}

fn commit(store: &mut Store, config: &Config, path: &str, language: Language, text: &str) {
    let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
    let prepared = prepare_file(
        config,
        path,
        language,
        text.to_string(),
        hash,
        text.len() as u64,
        1,
    )
    .expect("prepare");
    store.commit_file(&prepared).expect("commit");
}

/// 为所有待处理代码块生成 Mock 向量并重放到索引。
fn embed_all(
    store: &mut Store,
    space: &EmbeddingSpaceRow,
    provider: &MockProvider,
    index: &mut VectorIndex,
) {
    let pending = store.pending_embeddings(4096, 3).expect("pending");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    for item in pending {
        let cache_key = provider.space().cache_key(&item.input);
        if store
            .cached_vector(&space.id, &cache_key)
            .expect("cache")
            .is_none()
        {
            let vectors = runtime
                .block_on(provider.embed(std::slice::from_ref(&item.input)))
                .expect("embed");
            store
                .put_cached_vector(&space.id, &cache_key, &vectors[0])
                .expect("put");
        }
        store
            .attach_embedding(item.chunk_id, &space.id, &cache_key)
            .expect("attach");
    }
    index.rebuild_from_store(store).expect("rebuild");
}

fn query_vector(provider: &MockProvider, plan: &QueryPlan) -> Vec<f32> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let text = if plan.text.is_empty() {
        plan.raw.clone()
    } else {
        plan.text.clone()
    };
    runtime
        .block_on(provider.embed(&[text]))
        .expect("embed")
        .remove(0)
}

#[test]
fn full_symbol_query_hits_the_exact_definition_first() {
    let fixture = Fixture::build(true);
    let outcome = fixture.search("RetryPolicy::execute", true);
    assert!(!outcome.candidates.is_empty());
    assert!(outcome.candidates[0].exact, "首位应是精确符号命中");
    assert!(outcome.candidates[0]
        .matched_by
        .contains(&MatchSource::Symbol));
    // 方法定义在小型 impl 块内，因此首位代码块应包含该方法的完整实现。
    let top = fixture.content_of(outcome.candidates[0].chunk_id);
    assert!(
        top.contains("fn execute"),
        "首位结果应包含 execute 的实现:\n{top}"
    );
    let paths = fixture.paths(&outcome);
    assert_eq!(paths[0], "src/network/retry.rs");
}

#[test]
fn bare_method_name_falls_back_to_suffix_match() {
    let fixture = Fixture::build(false);
    // 查询只给出方法名，符号表里存的是限定名 JsonLexer::next_token。
    let outcome = fixture.search("next_token", false);
    assert!(!outcome.candidates.is_empty(), "后缀匹配不应为空");
    let top = fixture.content_of(outcome.candidates[0].chunk_id);
    assert!(
        top.contains("fn next_token"),
        "后缀匹配应命中 JsonLexer::next_token:\n{top}"
    );
}

#[test]
fn path_query_returns_chunks_from_that_file() {
    let fixture = Fixture::build(false);
    let outcome = fixture.search("src/network/retry.rs", false);
    let paths = fixture.paths(&outcome);
    assert!(!paths.is_empty(), "路径查询不应为空");
    // 路径精确命中权重最高，因此该文件的代码块必须占据前列。
    // FTS 仍会按共享词条（src、retry）召回其他文件，只是排名靠后，这是预期行为。
    let file_chunks = fixture
        .store
        .current_chunk_scopes()
        .expect("scopes")
        .iter()
        .filter(|row| row.path == "src/network/retry.rs")
        .count();
    let head = paths.iter().take(file_chunks).collect::<Vec<_>>();
    assert!(
        head.iter().all(|p| *p == "src/network/retry.rs"),
        "路径查询应把该文件排在最前: {paths:?}"
    );
}

#[test]
fn error_message_literal_is_found_verbatim() {
    let fixture = Fixture::build(false);
    // 带空格的错误消息用引号包裹，作为字面量精确检索。
    let outcome = fixture.search("\"connection reset by peer\"", false);
    let paths = fixture.paths(&outcome);
    assert!(
        paths.iter().any(|p| p == "src/network/retry.rs"),
        "应在 retry.rs 中找到该错误消息: {paths:?}"
    );
    assert!(
        outcome.candidates.iter().any(|c| c.exact),
        "字面量原样命中应标记为精确"
    );
}

#[test]
fn special_characters_in_queries_do_not_break_fts_syntax() {
    let fixture = Fixture::build(false);
    // 这些查询若被直接拼进 FTS 表达式会造成语法错误。
    for query in [
        "Result<String, String>",
        "fn execute(&self) -> Result",
        "\"unterminated",
        "a OR b AND NOT c",
        "col:umn NEAR/2 x",
        "((()))",
        "*",
        "^start",
        "-minus",
    ] {
        let outcome = fixture.search(query, false);
        // 关键断言是"不 panic 也不返回错误"；FTS 通路不应报告失败。
        assert!(
            !outcome
                .notes
                .iter()
                .any(|note| note.contains("FTS 召回不可用")),
            "查询 `{query}` 触发了 FTS 语法错误: {:?}",
            outcome.notes
        );
    }
}

#[test]
fn identifier_splitting_matches_camel_and_snake_case() {
    let fixture = Fixture::build(false);
    // 查询用空格分词，索引里是 camelCase。
    let outcome = fixture.search("fetch with retry", false);
    let paths = fixture.paths(&outcome);
    assert!(
        paths.iter().any(|p| p == "web/src/client.ts"),
        "拆词应命中 fetchWithRetry: {paths:?}"
    );
}

#[test]
fn chinese_query_recalls_chinese_documentation() {
    let fixture = Fixture::build(false);
    // 中文按 bigram 召回，doc 注释里有"重试策略"。
    let outcome = fixture.search("重试策略在哪里实现", false);
    let paths = fixture.paths(&outcome);
    assert!(
        paths.iter().any(|p| p == "src/network/retry.rs"),
        "中文查询应命中含中文文档注释的文件: {paths:?}"
    );
}

#[test]
fn language_scope_excludes_other_languages() {
    let fixture = Fixture::build(true);
    let scope = ScopeFilter {
        languages: vec![Language::TypeScript],
        ..ScopeFilter::default()
    };
    let outcome = fixture.search_scoped("retry", scope, true);
    let paths = fixture.paths(&outcome);
    assert!(!paths.is_empty(), "TypeScript 中确实有 retry 相关代码");
    assert!(
        paths.iter().all(|p| p.ends_with(".ts")),
        "语言过滤后不应出现其他语言: {paths:?}"
    );
}

#[test]
fn include_and_exclude_globs_are_applied() {
    let fixture = Fixture::build(true);

    let include_only = ScopeFilter {
        include: vec!["src/network/**".into()],
        ..ScopeFilter::default()
    };
    let paths = fixture.paths(&fixture.search_scoped("retry", include_only, true));
    assert!(!paths.is_empty());
    assert!(
        paths.iter().all(|p| p.starts_with("src/network/")),
        "include 未生效: {paths:?}"
    );

    let excluded = ScopeFilter {
        exclude: vec!["tests/**".into(), "web/**".into()],
        ..ScopeFilter::default()
    };
    let paths = fixture.paths(&fixture.search_scoped("retry", excluded, true));
    assert!(
        paths
            .iter()
            .all(|p| !p.starts_with("tests/") && !p.starts_with("web/")),
        "exclude 未生效: {paths:?}"
    );
}

#[test]
fn exclude_wins_over_include_for_the_same_path() {
    let fixture = Fixture::build(false);
    let scope = ScopeFilter {
        include: vec!["src/**".into()],
        exclude: vec!["src/network/**".into()],
        ..ScopeFilter::default()
    };
    let paths = fixture.paths(&fixture.search_scoped("retry", scope, false));
    assert!(
        paths.iter().all(|p| !p.starts_with("src/network/")),
        "exclude 优先级必须高于 include: {paths:?}"
    );
}

#[test]
fn invalid_glob_is_reported_as_an_invalid_query() {
    let scope = ScopeFilter {
        include: vec!["src/[".into()],
        ..ScopeFilter::default()
    };
    let error = ScopeMatcher::new(&scope).expect_err("非法 glob 必须报错");
    assert_eq!(error.code(), "invalid_query");
}

#[test]
fn candidates_are_deduplicated_across_recall_paths() {
    let fixture = Fixture::build(true);
    let outcome = fixture.search("RetryPolicy execute 重试", true);
    let ids: Vec<i64> = outcome.candidates.iter().map(|c| c.chunk_id).collect();
    let unique: std::collections::HashSet<i64> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "融合后不应出现重复候选: {ids:?}");
    // 多路命中的候选会记录多个来源。
    assert!(
        outcome.candidates.iter().any(|c| c.matched_by.len() > 1),
        "至少应有候选被多路召回"
    );
}

#[test]
fn results_are_ordered_by_descending_score() {
    let fixture = Fixture::build(true);
    let outcome = fixture.search("重试逻辑", true);
    let scores: Vec<f64> = outcome.candidates.iter().map(|c| c.score).collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "候选必须按分数降序: {scores:?}"
    );
}

#[test]
fn retrieval_is_deterministic_for_the_same_query() {
    let fixture = Fixture::build(true);
    let first = fixture.search("retry backoff", true);
    let second = fixture.search("retry backoff", true);
    let left: Vec<i64> = first.candidates.iter().map(|c| c.chunk_id).collect();
    let right: Vec<i64> = second.candidates.iter().map(|c| c.chunk_id).collect();
    assert_eq!(left, right, "相同查询必须得到相同顺序");
}

/// 固定查询向量与索引，验证旁路捕获不改变任何候选分数、来源或组装正文。
#[test]
fn diagnostics_do_not_change_hybrid_retrieval_or_context() {
    let fixture = Fixture::build(true);
    let plan = analyze("请求失败后的重试逻辑", &ScopeFilter::default()).unwrap();
    let vector = query_vector(&fixture.provider, &plan);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let run = || {
        let outcome = retrieve(
            &fixture.store,
            Some(&fixture.index),
            &fixture.config.retrieval,
            &RetrievalInput {
                plan: &plan,
                query_vector: Some(&vector),
                space_id: Some(&fixture.space.id),
            },
        )
        .unwrap();
        assert!(outcome.vector_used);
        let assembled = crate::context::assemble(
            &fixture.store,
            &crate::context::AssembleInput {
                candidates: &outcome.candidates,
                plan: &plan,
                max_results: 10,
                token_budget: 6000,
                config: &fixture.config.retrieval,
            },
        )
        .unwrap();
        serde_json::json!({"candidates":outcome.candidates,"notes":outcome.notes,"results":assembled.results,
            "tokens":assembled.total_estimated_tokens,"truncated":assembled.truncated})
    };
    let before = run();
    let capture = crate::diagnostics::Capture::at(Some(path.clone()));
    let after = run();
    capture.finish(&plan.raw, fixture.store.catalog_epoch().unwrap());
    assert_eq!(before, after);
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let recall = record["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["stage"] == "recall")
        .unwrap();
    assert!(recall["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|list| list["source"] == "vector"));
}

#[test]
fn vector_path_reports_degradation_when_unavailable() {
    let fixture = Fixture::build(false);
    let outcome = fixture.search("请求失败后如何退避", false);
    assert!(!outcome.vector_used, "未配置向量时不能声称使用了语义召回");
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("未启用向量召回")),
        "降级原因必须可观察: {:?}",
        outcome.notes
    );
    // 降级后仍应通过 FTS 返回结果。
    assert!(!outcome.candidates.is_empty(), "降级不等于没有结果");
}

#[test]
fn empty_vector_index_is_reported_instead_of_claiming_success() {
    let fixture = Fixture::build(false); // 已提交代码但没有生成向量
    let plan = analyze("重试", &ScopeFilter::default()).expect("analyze");
    let vector = query_vector(&fixture.provider, &plan);
    let input = RetrievalInput {
        plan: &plan,
        query_vector: Some(&vector),
        space_id: Some(&fixture.space.id),
    };
    let outcome = retrieve(
        &fixture.store,
        Some(&fixture.index),
        &fixture.config.retrieval,
        &input,
    )
    .expect("retrieve");
    assert!(!outcome.vector_used);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("向量索引为空")),
        "空索引必须明确说明: {:?}",
        outcome.notes
    );
}

#[test]
fn vector_path_is_actually_used_when_available() {
    let fixture = Fixture::build(true);
    let outcome = fixture.search("请求失败后的退避与重试", true);
    assert!(outcome.vector_used, "配置可用时应真实使用向量召回");
    assert!(outcome
        .candidates
        .iter()
        .any(|c| c.matched_by.contains(&MatchSource::Vector)));
}

#[test]
fn stale_chunks_never_appear_in_results() {
    let mut fixture = Fixture::build(false);
    // 改写文件：旧符号 execute 消失，新符号 attempt_once 出现。
    let updated = RETRY_RS.replace("pub fn execute", "pub fn attempt_once");
    commit(
        &mut fixture.store,
        &fixture.config,
        "src/network/retry.rs",
        Language::Rust,
        &updated,
    );

    let stale = fixture.search("RetryPolicy::execute", false);
    assert!(
        fixture
            .contents(&stale)
            .iter()
            .all(|code| !code.contains("fn execute")),
        "旧版本代码不应再作为当前代码返回"
    );

    // 新符号无需等待语义索引，符号与全文通路立即可见。
    let fresh = fixture.search("attempt_once", false);
    assert!(
        fixture
            .contents(&fresh)
            .iter()
            .any(|code| code.contains("fn attempt_once")),
        "新符号应立即可通过符号检索命中"
    );
}

#[test]
fn deleted_files_disappear_from_all_recall_paths() {
    let mut fixture = Fixture::build(true);
    assert!(fixture
        .store
        .mark_deleted("src/json/parser.rs")
        .expect("delete"));
    // 向量索引同步重放删除操作。
    fixture
        .index
        .rebuild_from_store(&fixture.store)
        .expect("rebuild");

    for (query, use_vectors) in [
        ("JsonLexer", false),
        ("next_token", false),
        ("JSON 词法", true),
    ] {
        let outcome = fixture.search(query, use_vectors);
        let paths = fixture.paths(&outcome);
        assert!(
            paths.iter().all(|p| p != "src/json/parser.rs"),
            "查询 `{query}` 仍返回已删除文件: {paths:?}"
        );
    }
}

#[test]
fn test_intent_is_detected_and_test_files_are_reachable() {
    let fixture = Fixture::build(false);
    let plan = analyze("retry 的测试在哪里", &ScopeFilter::default()).expect("analyze");
    assert!(
        plan.intents.contains(&Intent::Test),
        "应识别出测试意图: {:?}",
        plan.intents
    );
    let outcome = fixture.search_plan(&plan, false);
    let paths = fixture.paths(&outcome);
    assert!(
        paths.iter().any(|p| p.starts_with("tests/")),
        "测试意图查询应能召回测试文件: {paths:?}"
    );
}

#[test]
fn candidate_limit_is_respected() {
    let mut fixture = Fixture::build(true);
    fixture.config.retrieval.candidate_limit = 3;
    let outcome = fixture.search("retry", true);
    assert!(
        outcome.candidates.len() <= 3,
        "候选数量必须受上限约束: {}",
        outcome.candidates.len()
    );
}

#[test]
fn scope_that_matches_nothing_returns_no_candidates() {
    let fixture = Fixture::build(true);
    let scope = ScopeFilter {
        include: vec!["does/not/exist/**".into()],
        ..ScopeFilter::default()
    };
    let outcome = fixture.search_scoped("retry", scope, true);
    assert!(
        outcome.candidates.is_empty(),
        "作用域为空时不能返回越界结果"
    );
}

/// 记录一个有意的设计选择：体量不大的容器整体保留为一块。
///
/// 因此 `RetryPolicy::execute` 命中的代码块展示符号是容器名 `RetryPolicy`，
/// 方法名只登记在符号表中。这样避免父节点与每个子节点重复索引同一段代码。
#[test]
fn small_containers_stay_in_one_chunk_labelled_by_the_container() {
    let fixture = Fixture::build(false);
    let outcome = fixture.search("RetryPolicy::execute", false);
    let symbols = fixture.symbols(&outcome);
    assert_eq!(symbols[0], "RetryPolicy");
    let top = fixture.content_of(outcome.candidates[0].chunk_id);
    // 同一个块里既有容器也有方法实现，不存在重复索引。
    assert!(top.contains("impl RetryPolicy") && top.contains("fn execute"));
}

#[test]
fn scope_matcher_treats_empty_filter_as_unrestricted() {
    let matcher = ScopeMatcher::new(&ScopeFilter::default()).expect("matcher");
    assert!(matcher.is_unrestricted());
    assert!(matcher.matches("any/path.rs", Language::Rust));
}
