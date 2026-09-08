//! 重排的真实 HTTP 与 MCP 协议回归，不用模拟模型的分数证明真实语义质量。
mod support;

use codesearch::config::{Config, RerankConfig};
use codesearch::models::rerank::HttpReranker;
use rmcp::{model::CallToolRequestParams, ServiceExt};
use serde_json::json;
use support::{sample_repo, write_file, EmbeddingServer, Reply};

/// 本地免鉴权重排服务；默认单次调用上限足够覆盖测试候选。
fn config(server: &EmbeddingServer) -> RerankConfig {
    RerankConfig {
        base_url: server.base_url().into(),
        model: "fixture-reranker".into(),
        ..Default::default()
    }
}

/// 请求语义与按 index 对齐必须一致；空文档不得发出模型调用。
#[tokio::test]
async fn http_contract_and_empty_input() {
    let server = EmbeddingServer::start().await;
    server.script(vec![Reply::Rerank]);
    let provider = HttpReranker::new(&config(&server)).unwrap();
    assert!(provider.score("查询", &[]).await.unwrap().is_empty());
    assert_eq!(server.hits(), 0);
    assert_eq!(
        provider
            .score("查询", &["甲".into(), "乙".into()])
            .await
            .unwrap(),
        vec![0.0, 1.0]
    );
    let request = server.requests().remove(0);
    assert_eq!(request.path, "/v1/rerank");
    assert_eq!(
        request.body,
        json!({"model":"fixture-reranker","query":"查询","documents":["甲","乙"],"top_n":2,"return_documents":false})
    );
}

/// 超时、HTTP 错误、非法 JSON 及重复索引都必须明确失败，不泄漏响应体。
#[tokio::test]
async fn errors_are_explicit_and_bounded() {
    let server = EmbeddingServer::start().await;
    let mut cfg = config(&server);
    cfg.timeout_secs = 1;
    let provider = HttpReranker::new(&cfg).unwrap();
    for reply in [
        Reply::Status(500),
        Reply::Malformed,
        Reply::Raw("secret-response-body".into()),
        Reply::Raw(r#"{"results":[{"index":1,"relevance_score":0.5}]}"#.into()),
        Reply::Slow,
    ] {
        server.script(vec![reply]);
        let error = provider.score("查询", &["文档".into()]).await.unwrap_err();
        assert!(!error.to_string().contains("secret-response-body"));
    }
    assert_eq!(server.hits(), 5, "不隐式重试慢请求");
}

/// 重排发生在最终候选截断前，失败不会返回伪成功；完整符号与离线模式不会额外发送源码。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_reranks_before_limit_and_preserves_exact_lookup() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    let mut cfg = Config {
        rerank: Some(config(&server)),
        ..Default::default()
    };
    cfg.retrieval.candidate_limit = 1;
    write_file(
        &repo.path().join("codesearch.toml"),
        &toml::to_string(&cfg).unwrap(),
    );
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codesearch"));
    command
        .arg("serve")
        .arg("--root")
        .arg(repo.path())
        .arg("--watch")
        .arg("false")
        .env_remove("CODESEARCH_TRACE_JSONL");
    let service =
        ().serve(rmcp::transport::TokioChildProcess::new(command).unwrap())
            .await
            .unwrap();
    server.script(vec![Reply::Rerank]);
    let response = service
        .call_tool(
            CallToolRequestParams::new("search_context").with_arguments(
                json!({"query":"请求失败后的重试逻辑"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    let response = response.structured_content.unwrap();
    let request = server.requests().remove(0);
    assert!(request.inputs.len() > 1, "不得先裁为 1 个候选再重排");
    assert_eq!(
        response["results"][0]["path"],
        request.inputs.last().unwrap().lines().next().unwrap()
    );
    let before = server.hits();
    service
        .call_tool(
            CallToolRequestParams::new("search_context")
                .with_arguments(json!({"query":"RetryPolicy"}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(server.hits(), before, "完整符号保持原有精确通路");
    server.script(vec![Reply::Status(500)]);
    let error = service
        .call_tool(
            CallToolRequestParams::new("search_context").with_arguments(
                json!({"query":"请求失败后的重试逻辑"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("500"));
    service.cancel().await.unwrap();
    let mut options = codesearch::engine::EngineOptions::new(repo.path());
    options.offline = true;
    let engine = codesearch::engine::Engine::open(options).await.unwrap();
    let before = server.hits();
    let request = codesearch::types::SearchRequest {
        query: "请求失败后的重试逻辑".into(),
        scope: Default::default(),
        max_results: None,
        token_budget: None,
        mode: Default::default(),
    };
    engine.search(&request).await.unwrap();
    assert_eq!(server.hits(), before);
}

/// 慢模型不能持有索引锁阻塞状态查询；该测试使用有界等待并等待搜索任务正常结束。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_reranker_does_not_hold_index_locks() {
    let server = EmbeddingServer::start().await;
    let repo = sample_repo();
    let mut rerank = config(&server);
    rerank.timeout_secs = 1;
    let cfg = Config {
        rerank: Some(rerank),
        ..Default::default()
    };
    write_file(
        &repo.path().join("codesearch.toml"),
        &toml::to_string(&cfg).unwrap(),
    );
    let engine = std::sync::Arc::new(
        codesearch::engine::Engine::open(codesearch::engine::EngineOptions::new(repo.path()))
            .await
            .unwrap(),
    );
    engine.index(false).await.unwrap();
    server.script(vec![Reply::Slow]);
    let search_engine = engine.clone();
    let search = tokio::spawn(async move {
        search_engine
            .search(&codesearch::types::SearchRequest {
                query: "请求失败后的重试逻辑".into(),
                scope: Default::default(),
                max_results: None,
                token_budget: None,
                mode: Default::default(),
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while server.hits() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let status = tokio::task::spawn_blocking(move || engine.status());
    let checked = tokio::time::timeout(std::time::Duration::from_millis(500), status).await;
    assert!(search.await.unwrap().is_err());
    assert!(checked.is_ok(), "等待模型时不得阻塞索引读取");
    checked.unwrap().unwrap().unwrap();
}
