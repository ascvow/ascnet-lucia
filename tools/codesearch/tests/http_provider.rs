//! HTTP Embedding Provider 契约测试。
//!
//! 全部针对一个真实的本地 HTTP 服务，验证请求体形状、响应解析、乱序重排、
//! 有界重试、错误分类与凭证处理。这些是与外部服务之间的协议边界，
//! 只测内部函数不足以保证兼容性。

mod support;

use codesearch::config::EmbeddingConfig;
use codesearch::models::http::HttpProvider;

use support::{EmbeddingServer, Reply, DIMENSIONS};

/// 指向本地服务的基础配置。
fn config(server: &EmbeddingServer) -> EmbeddingConfig {
    EmbeddingConfig {
        base_url: server.base_url().to_string(),
        model: "fixture-embed".to_string(),
        api_key_env: String::new(),
        dimensions: Some(DIMENSIONS),
        batch_size: 4,
        max_concurrency: 2,
        max_retries: 2,
        timeout_secs: 5,
        ..EmbeddingConfig::default()
    }
}

fn inputs(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("document number {index} about retry policy"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn request_body_matches_the_openai_embeddings_protocol() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    let batch = inputs(2);
    let vectors = provider.embed(&batch).await.expect("生成向量");
    assert_eq!(vectors.len(), 2);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "两条输入应合并为一次请求");
    let request = &requests[0];
    assert_eq!(request.model, "fixture-embed");
    assert_eq!(request.inputs, batch, "输入必须原样发送且顺序一致");
    assert_eq!(
        request.encoding_format.as_deref(),
        Some("float"),
        "必须显式要求浮点数组，避免服务默认返回 base64"
    );
    assert_eq!(
        request.dimensions, None,
        "默认不写入 dimensions：多数服务会因未知参数返回 400"
    );
    assert!(
        request.authorization.is_none(),
        "未配置 api_key_env 时不应发送凭证"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dimensions_are_sent_only_when_explicitly_enabled() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    settings.send_dimensions = true;
    let provider = HttpProvider::connect(&settings, None).await.expect("连接");
    provider.embed(&inputs(1)).await.expect("生成向量");
    assert_eq!(
        server.requests()[0].dimensions,
        Some(DIMENSIONS),
        "显式开启后才写入 dimensions"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn vectors_are_normalized_and_aligned_with_inputs() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    let batch = inputs(3);
    let vectors = provider.embed(&batch).await.expect("生成向量");
    for vector in &vectors {
        assert_eq!(vector.len(), DIMENSIONS);
        let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "向量必须 L2 归一化: {norm}");
    }

    // 同一输入两次调用得到相同向量，说明没有把批次内顺序搞乱。
    let repeat = provider.embed(&batch).await.expect("再次生成");
    assert_eq!(vectors, repeat);

    // 单条输入单独请求，结果必须与批量中的对应项一致。
    let single = provider
        .embed(std::slice::from_ref(&batch[1]))
        .await
        .expect("单条");
    assert_eq!(single[0], vectors[1], "批量与单条结果必须一致");
}

#[tokio::test(flavor = "multi_thread")]
async fn out_of_order_response_is_realigned_by_index() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");
    let batch = inputs(3);

    let ordered = provider.embed(&batch).await.expect("正常顺序");
    // 服务倒序返回，但 index 字段正确：Provider 必须按 index 重排。
    server.script(vec![Reply::Shuffled]);
    let shuffled = provider.embed(&batch).await.expect("乱序响应");
    assert_eq!(ordered, shuffled, "乱序响应必须按 index 重排后才能使用");
}

#[tokio::test(flavor = "multi_thread")]
async fn response_without_index_falls_back_to_array_order() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");
    let batch = inputs(3);

    let ordered = provider.embed(&batch).await.expect("正常顺序");
    server.script(vec![Reply::WithoutIndex]);
    let without = provider.embed(&batch).await.expect("缺少 index 字段");
    assert_eq!(ordered, without, "省略 index 时按数组顺序处理");
}

#[tokio::test(flavor = "multi_thread")]
async fn batching_splits_inputs_and_preserves_overall_order() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    settings.batch_size = 2;
    settings.max_concurrency = 3;
    let provider = HttpProvider::connect(&settings, None).await.expect("连接");

    let batch = inputs(7);
    let vectors = provider.embed(&batch).await.expect("生成向量");
    assert_eq!(vectors.len(), 7);

    let requests = server.requests();
    assert_eq!(requests.len(), 4, "7 条输入按 batch_size=2 应切成 4 批");
    assert!(
        requests.iter().all(|request| request.inputs.len() <= 2),
        "每批不得超过 batch_size"
    );

    // 并发返回后仍要保持整体顺序：逐条单独请求做交叉验证。
    for (index, input) in batch.iter().enumerate() {
        let single = provider
            .embed(std::slice::from_ref(input))
            .await
            .expect("单条");
        assert_eq!(single[0], vectors[index], "第 {index} 条顺序错位");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn transient_failures_are_retried_within_the_bound() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    // 429 与 5xx 值得重试；max_retries=2 意味着最多 3 次请求。
    server.script(vec![Reply::Status(429), Reply::Status(503), Reply::Ok]);
    let vectors = provider.embed(&inputs(1)).await.expect("重试后应成功");
    assert_eq!(vectors.len(), 1);
    assert_eq!(server.hits(), 3, "应为 1 次首发 + 2 次重试");
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_are_bounded_and_eventually_fail() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    server.script(vec![Reply::Status(500); 10]);
    let error = provider
        .embed(&inputs(1))
        .await
        .expect_err("持续失败应报错");
    assert_eq!(error.code(), "provider_error");
    assert!(
        error.to_string().contains("500"),
        "错误应包含状态码: {error}"
    );
    assert_eq!(server.hits(), 3, "重试次数必须有界");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_errors_are_not_retried() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    // 4xx 表示请求本身有问题，重试没有意义。
    server.script(vec![Reply::Status(400), Reply::Ok, Reply::Ok]);
    let error = provider
        .embed(&inputs(1))
        .await
        .expect_err("4xx 应直接失败");
    assert!(error.to_string().contains("400"));
    assert_eq!(server.hits(), 1, "4xx 不应重试");
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_response_fails_without_retrying() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    server.script(vec![Reply::Malformed, Reply::Ok]);
    let error = provider
        .embed(&inputs(1))
        .await
        .expect_err("无法解析应报错");
    assert!(
        error.to_string().contains("解析"),
        "错误应说明是解析失败: {error}"
    );
    assert_eq!(server.hits(), 1, "响应无法解析通常是配置问题，不应重试");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_vectors_are_rejected() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");

    // 数量不符。
    server.script(vec![Reply::MissingVector]);
    let error = provider
        .embed(&inputs(3))
        .await
        .expect_err("数量不符应报错");
    assert!(error.to_string().contains("不一致"), "{error}");

    // 维度不符。
    server.script(vec![Reply::WrongDimensions(DIMENSIONS + 1)]);
    let error = provider
        .embed(&inputs(1))
        .await
        .expect_err("维度不符应报错");
    assert!(error.to_string().contains("维度"), "{error}");

    // NaN。
    server.script(vec![Reply::NotFinite]);
    let error = provider.embed(&inputs(1)).await.expect_err("NaN 应报错");
    assert!(error.to_string().contains("有限数"), "{error}");

    // 空向量。
    server.script(vec![Reply::EmptyVector]);
    let error = provider.embed(&inputs(1)).await.expect_err("空向量应报错");
    assert!(
        error.to_string().contains("为空") || error.to_string().contains("维度"),
        "{error}"
    );

    // 重复 index。
    server.script(vec![Reply::DuplicateIndex]);
    let error = provider
        .embed(&inputs(2))
        .await
        .expect_err("重复 index 应报错");
    assert!(error.to_string().contains("重复"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_input_short_circuits_without_a_request() {
    let server = EmbeddingServer::start().await;
    let provider = HttpProvider::connect(&config(&server), None)
        .await
        .expect("连接");
    let vectors = provider.embed(&[]).await.expect("空输入");
    assert!(vectors.is_empty());
    assert_eq!(server.hits(), 0, "空输入不应发出请求");
}

#[tokio::test(flavor = "multi_thread")]
async fn dimensions_are_probed_when_not_configured() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    settings.dimensions = None;
    let provider = HttpProvider::connect(&settings, None).await.expect("连接");

    assert_eq!(provider.space().dimensions, DIMENSIONS, "应探测出真实维度");
    assert_eq!(server.hits(), 1, "未配置维度时应发一次探测请求");
    let probe = &server.requests()[0];
    assert_eq!(probe.inputs.len(), 1, "探测请求应尽量小");
    assert!(probe.dimensions.is_none(), "探测时不应写入 dimensions");

    // 复用上次探测结果时不再发送探测请求。
    server.reset();
    let reused = HttpProvider::connect(&settings, Some(DIMENSIONS))
        .await
        .expect("复用维度");
    assert_eq!(reused.space().dimensions, DIMENSIONS);
    assert_eq!(server.hits(), 0, "已知维度时不应重复探测");
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_dimensions_must_match_the_service() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    // 配置一个错误维度：连接阶段不探测，但首次请求就必须报错。
    settings.dimensions = Some(DIMENSIONS * 2);
    let provider = HttpProvider::connect(&settings, None).await.expect("连接");
    let error = provider
        .embed(&inputs(1))
        .await
        .expect_err("维度不符必须报错而不是静默接受");
    assert!(error.to_string().contains("维度"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_is_sent_from_the_environment_and_never_logged() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    // 变量名进入配置，取值只存在于环境变量中。
    let variable = "CODESEARCH_TEST_API_KEY";
    settings.api_key_env = variable.to_string();
    let secret = "sk-fixture-do-not-log-1234567890";
    // SAFETY: 测试单线程设置自己的环境变量，随后立即使用。
    unsafe { std::env::set_var(variable, secret) };

    let provider = HttpProvider::connect(&settings, None).await.expect("连接");
    provider.embed(&inputs(1)).await.expect("生成向量");

    let request = &server.requests()[0];
    assert_eq!(
        request.authorization.as_deref(),
        Some(format!("Bearer {secret}").as_str()),
        "凭证应通过 Authorization 头发送"
    );

    // 错误信息不得包含凭证。
    server.script(vec![Reply::Status(401); 4]);
    let error = provider.embed(&inputs(1)).await.expect_err("401 应报错");
    let text = error.to_string();
    assert!(!text.contains(secret), "错误信息泄漏了凭证: {text}");
    assert!(
        !format!("{provider:?}").contains(secret),
        "Debug 输出泄漏了凭证"
    );

    unsafe { std::env::remove_var(variable) };
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_api_key_variable_is_a_config_error() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    settings.api_key_env = "CODESEARCH_TEST_DEFINITELY_UNSET_KEY".to_string();
    let error = HttpProvider::connect(&settings, None)
        .await
        .expect_err("未设置的环境变量必须报错");
    assert_eq!(error.code(), "config_error");
    // 只提变量名，不含任何取值。
    assert!(error
        .to_string()
        .contains("CODESEARCH_TEST_DEFINITELY_UNSET_KEY"));
    assert_eq!(server.hits(), 0, "凭证缺失时不应发出请求");
}

#[tokio::test(flavor = "multi_thread")]
async fn query_and_document_inputs_use_their_configured_prefixes() {
    let server = EmbeddingServer::start().await;
    let mut settings = config(&server);
    settings.query_prefix = "query: ".to_string();
    settings.document_prefix = "passage: ".to_string();
    let provider = HttpProvider::connect(&settings, None).await.expect("连接");

    // embed_query 必须带查询前缀；前缀参与 Embedding 空间身份。
    let space = provider.space().clone();
    assert_eq!(space.query_prefix, "query: ");
    assert_eq!(space.document_prefix, "passage: ");
    assert_eq!(space.query_input("retry"), "query: retry");

    provider
        .embed(&[space.query_input("retry")])
        .await
        .expect("查询向量");
    assert_eq!(server.requests()[0].inputs[0], "query: retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_is_reported_as_a_provider_error() {
    // 指向一个不监听的地址：连接失败必须是明确的 provider 错误。
    let settings = EmbeddingConfig {
        base_url: "http://127.0.0.1:1/v1".to_string(),
        model: "m".to_string(),
        api_key_env: String::new(),
        dimensions: Some(DIMENSIONS),
        max_retries: 0,
        timeout_secs: 2,
        ..EmbeddingConfig::default()
    };
    let provider = HttpProvider::connect(&settings, None)
        .await
        .expect("连接对象可建立");
    let error = provider
        .embed(&inputs(1))
        .await
        .expect_err("无法连接必须报错");
    assert_eq!(error.code(), "provider_error");
}
