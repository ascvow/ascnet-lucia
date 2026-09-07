//! 端到端流程：index → search → read → 修改 → 增量 → 再搜 → 重启 → 再搜。
//!
//! 全程使用真实组件：真实解析、真实 SQLite/FTS5、真实 USearch、真实 HTTP Provider
//! （指向本地测试服务）。向量数值由确定性投影生成，因此这里验证的是流程与
//! 一致性语义，不是真实模型的语义检索质量。

mod support;

use std::path::Path;

use codesearch::types::{ReadExpand, ReadRequest, ScopeFilter, SearchMode, SearchRequest};
use codesearch::{AccessMode, Engine, EngineOptions};

use support::{config_toml, sample_repo, write_file, EmbeddingServer, Reply};

/// 打开一个指向本地 Embedding 服务的引擎。
async fn open_engine(root: &Path, offline: bool) -> Engine {
    Engine::open(EngineOptions {
        root: root.to_path_buf(),
        config_path: None,
        data_dir: None,
        offline,
        mode: AccessMode::ReadWrite,
    })
    .await
    .expect("打开引擎")
}

fn search_request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        scope: ScopeFilter::default(),
        mode: SearchMode::Fast,
        max_results: Some(10),
        token_budget: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn full_lifecycle_index_search_read_update_restart() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml(server.base_url(), ""),
    );

    // ---- 1. 首次索引 ----
    let engine = open_engine(repo.path(), false).await;
    let report = engine.index(false).await.expect("首次索引");
    assert!(report.scanned_files > 0, "应扫描到文件");
    assert!(report.current_chunks > 0, "应产生代码块");
    assert_eq!(report.embeddings_pending, 0, "首次索引后不应有待嵌入项");
    assert_eq!(report.embeddings_failed, 0);
    assert!(!report.semantic_skipped, "配置了 Provider 时语义阶段应执行");

    // 默认排除、敏感规则与 .gitignore 都必须生效。
    let skipped: Vec<&str> = report
        .skipped
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    assert!(
        skipped.iter().any(|path| path.contains("logo.png")),
        "二进制文件应给出可观察的跳过原因: {skipped:?}"
    );

    let status = engine.status().expect("状态");
    assert!(status.active_files > 0);
    assert!(status.embeddings_ready > 0, "应有已就绪的向量");
    assert_eq!(status.embeddings_pending, 0);
    assert!(status.vector_count > 0, "USearch 中应有向量");
    assert_eq!(
        status.vector_applied_seq, status.vector_durable_seq,
        "索引结束后检查点应已追平内存"
    );
    let space = status.embedding_space.expect("应有 Embedding 空间");
    assert_eq!(space.dimensions, support::DIMENSIONS);

    // ---- 2. 搜索 ----
    let response = engine
        .search(&search_request("请求失败后的重试逻辑在哪里"))
        .await
        .expect("搜索");
    assert!(!response.results.is_empty(), "自然语言查询应有结果");
    assert!(response.status.vector_search_used, "向量通路应被真实使用");
    assert!(!response.status.degraded, "全部就绪时不应报告降级");
    assert_eq!(response.status.freshness, "best_effort");
    assert!(response.estimated, "token 数是估算值，必须如实标记");
    assert!(
        response
            .results
            .iter()
            .any(|result| result.path.contains("retry.rs")),
        "应召回 retry.rs: {:?}",
        response.results.iter().map(|r| &r.path).collect::<Vec<_>>()
    );

    // 每条结果都必须带完整出处。
    for result in &response.results {
        assert!(!result.code_ref.is_empty());
        assert!(!result.file_hash.is_empty());
        assert!(result.start_line >= 1, "行号从 1 开始");
        assert!(result.end_line >= result.start_line);
        assert!(!result.content.is_empty());
        assert!(!result.matched_by.is_empty());
    }

    // ---- 3. 按引用读取 ----
    let target = response
        .results
        .iter()
        .find(|result| result.path.contains("retry.rs"))
        .expect("retry.rs 结果")
        .clone();
    let read = engine
        .read(&ReadRequest {
            code_ref: target.code_ref.clone(),
            expand: ReadExpand::None,
            token_budget: None,
        })
        .expect("读取");
    assert_eq!(read.path, target.path);
    assert_eq!(read.start_line, target.start_line);
    assert_eq!(read.file_hash, target.file_hash);
    assert!(read.is_current, "刚索引的引用应是当前版本");
    assert_eq!(read.content.trim(), target.content.trim());

    // 扩展到包含它的符号后，范围只会变大不会变小。
    let expanded = engine
        .read(&ReadRequest {
            code_ref: target.code_ref.clone(),
            expand: ReadExpand::ContainingSymbol,
            token_budget: None,
        })
        .expect("扩展读取");
    assert!(expanded.start_line <= read.start_line);
    assert!(expanded.end_line >= read.end_line);

    // ---- 4. 修改文件并增量索引 ----
    let retry_path = repo.path().join("src").join("network").join("retry.rs");
    let original = support::read_file(&retry_path);
    let updated = original.replace("pub fn execute<F, T>", "pub fn run_with_backoff<F, T>");
    assert_ne!(original, updated, "替换必须真实发生");
    std::fs::write(&retry_path, &updated).expect("写入修改");

    let incremental = engine.index(false).await.expect("增量索引");
    assert_eq!(incremental.changed_files, 1, "只应有一个文件发生变更");
    assert!(
        incremental.unchanged_files > 0,
        "其余文件应命中未变更快速路径"
    );

    // 新符号立即可搜；旧符号不再作为当前代码返回。
    let fresh = engine
        .search(&search_request("run_with_backoff"))
        .await
        .expect("搜索新符号");
    assert!(
        fresh
            .results
            .iter()
            .any(|result| result.content.contains("run_with_backoff")),
        "新符号应可被搜到"
    );
    let stale = engine
        .search(&search_request("RetryPolicy::execute"))
        .await
        .expect("搜索旧符号");
    assert!(
        stale
            .results
            .iter()
            .all(|result| !result.content.contains("pub fn execute<F, T>")),
        "旧版本代码不应再作为当前代码返回"
    );

    // 旧引用仍指向旧版本，并如实标记它不是当前版本。
    let old_read = engine.read(&ReadRequest {
        code_ref: target.code_ref.clone(),
        expand: ReadExpand::None,
        token_budget: None,
    });
    match old_read {
        Ok(previous) => {
            assert!(!previous.is_current, "旧引用不能冒充当前版本");
            assert_eq!(previous.file_hash, target.file_hash, "必须仍是旧版本内容");
        }
        Err(error) => {
            // 历史被清理时必须是明确的失效错误，而不是返回最新文件。
            assert_eq!(error.code(), "reference_expired", "非预期错误: {error}");
        }
    }

    // ---- 5. 新增与删除文件 ----
    write_file(
        &repo.path().join("src").join("network").join("circuit.rs"),
        "/// 熔断器：连续失败达到阈值后直接拒绝请求。\npub struct CircuitBreaker {\n    pub threshold: u32,\n}\n\nimpl CircuitBreaker {\n    /// 判断当前是否应拒绝请求。\n    pub fn should_reject(&self, failures: u32) -> bool {\n        failures >= self.threshold\n    }\n}\n",
    );
    std::fs::remove_file(repo.path().join("src").join("json").join("lexer.rs")).expect("删除文件");
    engine.index(false).await.expect("再次增量索引");

    let added = engine
        .search(&search_request("CircuitBreaker::should_reject"))
        .await
        .expect("搜索新文件");
    assert!(
        added
            .results
            .iter()
            .any(|result| result.path.contains("circuit.rs")),
        "新增文件应可被搜到"
    );
    let removed = engine
        .search(&search_request("JsonLexer next_token"))
        .await
        .expect("搜索已删除文件");
    assert!(
        removed
            .results
            .iter()
            .all(|result| !result.path.contains("lexer.rs")),
        "已删除文件不应出现在结果中: {:?}",
        removed.results.iter().map(|r| &r.path).collect::<Vec<_>>()
    );

    // ---- 6. 重启后仍然可用，且不重新请求模型 ----
    let before_restart = engine
        .search(&search_request("退避与重试策略"))
        .await
        .expect("重启前搜索");
    let baseline: Vec<String> = before_restart
        .results
        .iter()
        .map(|result| result.code_ref.clone())
        .collect();
    let epoch_before = before_restart.catalog_epoch;
    drop(engine); // 释放 workspace 写锁

    server.reset();
    let restarted = open_engine(repo.path(), false).await;
    let status = restarted.status().expect("重启后状态");
    assert!(status.vector_count > 0, "重启后应从检查点恢复向量");
    assert_eq!(status.embeddings_pending, 0, "重启不应产生新的待嵌入项");
    assert_eq!(status.catalog_epoch, epoch_before, "未变更时目录版本应保持");

    let after_restart = restarted
        .search(&search_request("退避与重试策略"))
        .await
        .expect("重启后搜索");
    let refreshed: Vec<String> = after_restart
        .results
        .iter()
        .map(|result| result.code_ref.clone())
        .collect();
    assert_eq!(baseline, refreshed, "重启后相同查询应得到相同结果");
    assert!(after_restart.status.vector_search_used);
    // 只有查询向量需要请求模型，文档向量全部来自已保存的数据。
    let document_requests = server
        .requests()
        .iter()
        .filter(|request| request.inputs.iter().any(|input| input.contains("Path:")))
        .count();
    assert_eq!(document_requests, 0, "重启不应重新生成文档向量");
}

#[tokio::test(flavor = "multi_thread")]
async fn embedding_cache_is_reused_across_reindex() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml(server.base_url(), ""),
    );

    let engine = open_engine(repo.path(), false).await;
    let first = engine.index(false).await.expect("首次索引");
    assert!(first.embeddings_created > 0, "首次应真实生成向量");
    let created = first.embeddings_created;

    // 强制重新计算全部 Hash，但内容未变，向量应全部命中缓存。
    server.reset();
    let forced = engine.index(true).await.expect("强制重扫");
    assert_eq!(forced.embeddings_created, 0, "内容未变不应重新生成向量");
    assert_eq!(server.hits(), 0, "不应再请求模型");

    // 触碰一个文件但内容不变：仍然不应重新嵌入。
    let notes = repo.path().join("notes.txt");
    let contents = support::read_file(&notes);
    std::fs::write(&notes, contents).expect("重写同样内容");
    let touched = engine.index(false).await.expect("再次索引");
    assert_eq!(touched.embeddings_created, 0);

    // 真正修改内容时才会请求模型，且只针对受影响的代码块。
    server.reset();
    write_file(
        &repo.path().join("src").join("network").join("timeout.rs"),
        "/// 超时控制。\npub fn deadline_for(attempt: u32) -> u64 {\n    100 * attempt as u64\n}\n",
    );
    let changed = engine.index(false).await.expect("变更后索引");
    assert!(changed.embeddings_created > 0, "新增内容应生成向量");
    assert!(
        changed.embeddings_created < created,
        "只应为新增代码块生成向量: {} vs {created}",
        changed.embeddings_created
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_mode_never_calls_the_model_but_still_searches() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml(server.base_url(), ""),
    );

    let engine = open_engine(repo.path(), true).await;
    let report = engine.index(false).await.expect("离线索引");
    assert!(report.current_chunks > 0, "离线模式仍应建立符号与全文索引");
    assert!(report.semantic_skipped, "离线模式必须跳过语义阶段");
    assert!(report.semantic_note.is_some(), "必须说明为何跳过");

    // 符号与全文检索照常工作。
    let response = engine
        .search(&search_request("RetryPolicy"))
        .await
        .expect("离线搜索");
    assert!(!response.results.is_empty(), "离线模式仍应返回结果");
    assert!(!response.status.vector_search_used);
    assert!(response.status.degraded, "缺少语义通路必须报告降级");
    assert!(
        response
            .status
            .reasons
            .iter()
            .any(|reason| reason.contains("offline")),
        "降级原因必须出现在结构化状态中: {:?}",
        response.status.reasons
    );

    // 全程零模型请求。
    assert_eq!(server.hits(), 0, "offline 模式不允许任何模型请求");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_degrades_when_provider_is_unreachable() {
    let repo = sample_repo();
    // 指向一个不监听的端口：Provider 连接失败不应让检索整体失败。
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml("http://127.0.0.1:1/v1", ""),
    );

    let engine = open_engine(repo.path(), false).await;
    let report = engine
        .index(false)
        .await
        .expect("Provider 不可用时仍应完成目录索引");
    assert!(report.current_chunks > 0, "目录索引必须完整完成");
    // 配置了维度时 connect 不发探测请求，因此 Provider 构造成功、语义阶段确实执行过。
    // 此时应如实报告"尝试过但失败"，而不是"跳过"。
    assert!(
        !report.semantic_skipped,
        "语义阶段确实执行了，不应报告为跳过"
    );
    assert!(
        report.embeddings_pending > 0 || report.embeddings_failed > 0,
        "连接失败必须体现在计数中"
    );
    assert!(report.degraded);

    let response = engine
        .search(&search_request("重试"))
        .await
        .expect("搜索不应失败");
    assert!(!response.results.is_empty(), "向量通路不可用仍应有结果");
    assert!(response.status.degraded);
    assert!(!response.status.vector_search_used);
    assert!(
        !response.status.reasons.is_empty(),
        "降级原因必须出现在结构化状态中，而不是只写日志"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn second_writer_process_gets_a_clear_busy_error() {
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml("http://127.0.0.1:1/v1", ""),
    );
    let first = open_engine(repo.path(), false).await;
    first.index(false).await.expect("首次索引");

    // 同一 workspace 的第二个写者必须得到明确的 busy 错误。
    let busy = Engine::open(EngineOptions {
        root: repo.path().to_path_buf(),
        config_path: None,
        data_dir: None,
        offline: true,
        mode: AccessMode::ReadWrite,
    })
    .await;
    let error = busy.expect_err("第二个写者应失败");
    assert_eq!(error.code(), "workspace_busy", "错误码不符: {error}");

    // Auto 模式降级为只读并给出说明，查询仍可用。
    let reader = Engine::open(EngineOptions {
        root: repo.path().to_path_buf(),
        config_path: None,
        data_dir: None,
        offline: true,
        mode: AccessMode::Auto,
    })
    .await
    .expect("Auto 模式应降级为只读");
    assert!(!reader.is_writable(), "写锁被占用时不应声称可写");
    let response = reader
        .search(&search_request("RetryPolicy"))
        .await
        .expect("只读搜索");
    assert!(!response.results.is_empty(), "只读会话仍应能查询");
    assert!(
        response
            .status
            .reasons
            .iter()
            .any(|reason| reason.contains("只读")),
        "降级原因必须可观察: {:?}",
        response.status.reasons
    );

    // 只读会话不允许写入。
    let error = reader.index(false).await.expect_err("只读会话不应能索引");
    assert_eq!(error.code(), "read_only");
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_filters_and_invalid_input_are_handled() {
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml("http://127.0.0.1:1/v1", ""),
    );
    let engine = open_engine(repo.path(), false).await;
    engine.index(false).await.expect("索引");

    // 语言过滤。
    let response = engine
        .search(&SearchRequest {
            query: "retry".to_string(),
            scope: ScopeFilter {
                languages: vec![codesearch::types::Language::TypeScript],
                ..ScopeFilter::default()
            },
            mode: SearchMode::Fast,
            max_results: Some(10),
            token_budget: None,
        })
        .await
        .expect("语言过滤搜索");
    assert!(!response.results.is_empty());
    assert!(
        response
            .results
            .iter()
            .all(|result| result.path.ends_with(".ts")),
        "语言过滤未生效: {:?}",
        response.results.iter().map(|r| &r.path).collect::<Vec<_>>()
    );

    // include 过滤。
    let response = engine
        .search(&SearchRequest {
            query: "retry".to_string(),
            scope: ScopeFilter {
                include: vec!["src/network/**".to_string()],
                ..ScopeFilter::default()
            },
            mode: SearchMode::Fast,
            max_results: Some(10),
            token_budget: None,
        })
        .await
        .expect("include 过滤搜索");
    assert!(
        response
            .results
            .iter()
            .all(|result| result.path.starts_with("src/network/")),
        "include 未生效"
    );

    // 空查询与非法 glob 都必须报明确错误。
    let error = engine
        .search(&search_request("   "))
        .await
        .expect_err("空查询应报错");
    assert_eq!(error.code(), "invalid_query");
    let error = engine
        .search(&SearchRequest {
            query: "retry".to_string(),
            scope: ScopeFilter {
                include: vec!["src/[".to_string()],
                ..ScopeFilter::default()
            },
            mode: SearchMode::Fast,
            max_results: None,
            token_budget: None,
        })
        .await
        .expect_err("非法 glob 应报错");
    assert_eq!(error.code(), "invalid_query");
}

#[tokio::test(flavor = "multi_thread")]
async fn token_budget_truncates_and_reports_estimates() {
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml("http://127.0.0.1:1/v1", ""),
    );
    let engine = open_engine(repo.path(), false).await;
    engine.index(false).await.expect("索引");

    let generous = engine
        .search(&SearchRequest {
            query: "retry".to_string(),
            scope: ScopeFilter::default(),
            mode: SearchMode::Fast,
            max_results: Some(10),
            token_budget: Some(4000),
        })
        .await
        .expect("宽预算搜索");

    let tight = engine
        .search(&SearchRequest {
            query: "retry".to_string(),
            scope: ScopeFilter::default(),
            mode: SearchMode::Fast,
            max_results: Some(10),
            token_budget: Some(80),
        })
        .await
        .expect("紧预算搜索");

    assert!(tight.estimated, "无精确 tokenizer 时必须标记为估算");
    assert!(
        tight.total_estimated_tokens <= generous.total_estimated_tokens,
        "紧预算的总量不应更大"
    );
    assert!(
        tight.results.len() <= generous.results.len(),
        "紧预算应返回更少或同样多的结果"
    );
    assert!(
        tight.truncated || tight.results.len() < generous.results.len(),
        "紧预算应体现为截断或结果减少"
    );
    // 即使预算很紧也不能返回空内容的结果。
    for result in &tight.results {
        assert!(!result.content.trim().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_scope_cannot_be_escaped() {
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml("http://127.0.0.1:1/v1", ""),
    );
    let engine = open_engine(repo.path(), false).await;
    engine.index(false).await.expect("索引");

    // 构造越界引用：路径穿越与绝对路径都必须被拒绝，而不是读到 workspace 之外。
    for bad in [
        "cs1:../../etc/passwd:0000:1-2",
        "cs1:/etc/passwd:0000:1-2",
        "not-a-ref",
        "cs1:999999999:deadbeef:1-2",
    ] {
        let error = engine
            .read(&ReadRequest {
                code_ref: bad.to_string(),
                expand: ReadExpand::None,
                token_budget: None,
            })
            .expect_err("越界或非法引用必须报错");
        assert!(
            matches!(
                error.code(),
                "invalid_reference" | "reference_expired" | "path_out_of_scope"
            ),
            "引用 `{bad}` 的错误码不符: {} ({error})",
            error.code()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_indexed_refreshes_only_the_requested_paths() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml(server.base_url(), ""),
    );
    let engine = open_engine(repo.path(), false).await;
    engine.index(false).await.expect("首次索引");

    // 直接写入一个新文件，然后只针对它调用 ensure_indexed。
    write_file(
        &repo.path().join("src").join("network").join("hedge.rs"),
        "/// 对冲请求：同时发出备份请求以降低尾延迟。\npub fn hedge_after_ms() -> u64 {\n    50\n}\n",
    );
    engine
        .ensure_indexed(&["src/network/hedge.rs".to_string()])
        .await
        .expect("ensure_indexed");

    let response = engine
        .search(&search_request("hedge_after_ms"))
        .await
        .expect("搜索");
    assert!(
        response
            .results
            .iter()
            .any(|result| result.path.contains("hedge.rs")),
        "ensure_indexed 后该路径必须可搜到"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_failures_are_recorded_without_losing_the_catalog() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml(server.base_url(), ""),
    );

    // 让服务持续返回 500：目录索引必须完成，语义部分如实报告失败或待处理。
    server.script(vec![Reply::Status(500); 64]);
    let engine = open_engine(repo.path(), false).await;
    let report = engine
        .index(false)
        .await
        .expect("目录索引不应因模型失败而中断");
    assert!(report.current_chunks > 0, "目录状态必须完整提交");
    assert!(
        report.embeddings_pending > 0 || report.embeddings_failed > 0,
        "模型失败必须体现在计数中"
    );
    assert!(report.degraded, "必须报告降级");

    // 符号检索仍然可用。
    let response = engine
        .search(&search_request("RetryPolicy"))
        .await
        .expect("搜索");
    assert!(!response.results.is_empty());
    assert!(response.status.degraded);

    // 服务恢复后重新索引可以补齐语义索引。
    server.reset();
    let recovered = engine.index(false).await.expect("恢复后索引");
    assert!(
        recovered.embeddings_created > 0 || recovered.embeddings_pending == 0,
        "服务恢复后应补齐向量"
    );
}

/// 一个超大代码块不能吃光 token 预算，把其余结果全部挤掉。
///
/// 回归：容器概览块的字节范围覆盖整个容器，命中时会占满整个预算，
/// 使多路召回退化成"只返回一条结果"。预算现在按剩余名额分摊。
#[tokio::test(flavor = "multi_thread")]
async fn one_huge_chunk_does_not_starve_the_other_results() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    write_file(
        &repo.path().join("codesearch.toml"),
        &config_toml(server.base_url(), ""),
    );

    // 一个远超预算的巨大容器，且带有会被查询命中的符号。
    let mut huge = String::from(
        "//! 超大容器，用于验证预算分摊。\n\npub struct BudgetHog;\n\nimpl BudgetHog {\n",
    );
    for index in 0..400 {
        huge.push_str(&format!(
            "    /// 第 {index} 个方法，正文足够长以撑满 token 预算。\n    pub fn budget_hog_{index}(&self, value: u64) -> u64 {{\n        let intermediate = value.wrapping_mul({index} + 1);\n        intermediate.wrapping_add(0x5DEECE66D)\n    }}\n\n"
        ));
    }
    huge.push_str("}\n");
    write_file(&repo.path().join("src").join("budget_hog.rs"), &huge);

    let engine = open_engine(repo.path(), false).await;
    engine.index(false).await.expect("索引");

    let response = engine
        .search(&SearchRequest {
            query: "budget_hog".to_string(),
            scope: ScopeFilter::default(),
            mode: SearchMode::Fast,
            max_results: Some(5),
            token_budget: Some(4_000),
        })
        .await
        .expect("搜索");

    assert!(
        response.results.len() > 1,
        "超大代码块吃光了预算，只返回了 {} 条结果",
        response.results.len()
    );
    let budget = 4_000usize;
    for result in &response.results {
        assert!(
            result.estimated_tokens < budget,
            "单条结果占用 {} tokens，几乎等于全部预算 {budget}",
            result.estimated_tokens
        );
    }
    let total: usize = response.results.iter().map(|r| r.estimated_tokens).sum();
    assert!(total <= budget, "结果合计 {total} 超出预算 {budget}");
}
