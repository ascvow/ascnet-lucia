//! 真实 Embedding 服务 smoke test，默认**不执行**。
//!
//! 需要显式提供凭证才会真正跑：
//!
//! ```bash
//! export SILICONFLOW_API_KEY=...
//! cargo test --test real_model_smoke -- --nocapture
//! ```
//!
//! 缺少凭证时每个用例都会打印"已跳过"并直接通过，因此默认测试套件不依赖网络。
//! 也可以用 `CODESEARCH_SMOKE_BASE_URL` / `CODESEARCH_SMOKE_MODEL` /
//! `CODESEARCH_SMOKE_API_KEY_ENV` / `CODESEARCH_SMOKE_DIMENSIONS` 指向其他
//! OpenAI-compatible 服务（包括本地推理服务）。
//!
//! 这些用例验证的是"能否与真实服务正确协作"：协议、维度、缓存与检索闭环。
//! 通过与否不构成对检索质量的量化结论。

mod support;

use codesearch::config::EmbeddingConfig;
use codesearch::models::http::HttpProvider;
use codesearch::types::{ScopeFilter, SearchMode, SearchRequest};
use codesearch::{AccessMode, Engine, EngineOptions};

use support::{sample_repo, write_file};

/// 默认指向硅基流动的免费向量模型。
const DEFAULT_BASE_URL: &str = "https://api.siliconflow.cn/v1";
const DEFAULT_MODEL: &str = "BAAI/bge-m3";
const DEFAULT_API_KEY_ENV: &str = "SILICONFLOW_API_KEY";
const DEFAULT_DIMENSIONS: usize = 1024;

/// 真实服务参数。
struct Target {
    base_url: String,
    model: String,
    api_key_env: String,
    dimensions: usize,
}

/// 读取 smoke test 目标；未设置凭证时返回 `None`。
fn target() -> Option<Target> {
    let api_key_env = std::env::var("CODESEARCH_SMOKE_API_KEY_ENV")
        .unwrap_or_else(|_| DEFAULT_API_KEY_ENV.to_string());
    // 凭证必须真实存在，否则视为未启用。
    let has_key = std::env::var(&api_key_env)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !has_key {
        return None;
    }
    Some(Target {
        base_url: std::env::var("CODESEARCH_SMOKE_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
        model: std::env::var("CODESEARCH_SMOKE_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        api_key_env,
        dimensions: std::env::var("CODESEARCH_SMOKE_DIMENSIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_DIMENSIONS),
    })
}

/// 未启用时打印明确说明，避免把"没跑"误读成"通过"。
fn skip_notice(test: &str) {
    eprintln!(
        "[skipped] {test}: 未设置 {DEFAULT_API_KEY_ENV}（或 CODESEARCH_SMOKE_API_KEY_ENV 指定的变量），\
真实模型 smoke test 未执行。"
    );
}

fn embedding_config(target: &Target) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: codesearch::config::ProviderKind::OpenaiCompatible,
        base_url: target.base_url.clone(),
        model: target.model.clone(),
        api_key_env: target.api_key_env.clone(),
        dimensions: Some(target.dimensions),
        // 多数服务（含硅基流动）会拒绝请求体中的 dimensions。
        send_dimensions: false,
        batch_size: 16,
        max_concurrency: 2,
        max_retries: 5,
        timeout_secs: 60,
        ..EmbeddingConfig::default()
    }
}

/// 生成指向真实服务的配置文件。只写环境变量名，不写凭证。
fn config_toml(target: &Target) -> String {
    format!(
        r#"[embedding]
provider = "openai_compatible"
base_url = "{}"
model = "{}"
api_key_env = "{}"
dimensions = {}
send_dimensions = false
batch_size = 16
max_concurrency = 2
max_retries = 5
timeout_secs = 60

[index]
refresh_on_search = false
"#,
        target.base_url, target.model, target.api_key_env, target.dimensions
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn real_provider_returns_usable_vectors() {
    let Some(target) = target() else {
        return skip_notice("real_provider_returns_usable_vectors");
    };
    let provider = HttpProvider::connect(&embedding_config(&target), None)
        .await
        .expect("连接真实服务");
    assert_eq!(
        provider.space().dimensions,
        target.dimensions,
        "服务返回维度与配置不一致"
    );

    let inputs = vec![
        "fn execute(&self) -> Result<(), RetryError> { /* 指数退避重试 */ }".to_string(),
        "请求失败后的退避与重试策略".to_string(),
        "JSON 词法分析器读取下一个 token".to_string(),
    ];
    let vectors = provider.embed(&inputs).await.expect("真实服务生成向量");
    assert_eq!(vectors.len(), inputs.len());
    for vector in &vectors {
        assert_eq!(vector.len(), target.dimensions);
        assert!(vector.iter().all(|value| value.is_finite()));
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "向量应已归一化: {norm}");
    }

    // 真实模型下，语义相近的两条应比无关的一条更接近。
    // 这只是一个方向性检查，不是质量指标。
    let close = codesearch::models::cosine_similarity(&vectors[0], &vectors[1]);
    let far = codesearch::models::cosine_similarity(&vectors[0], &vectors[2]);
    eprintln!("[smoke] 相近={close:.4} 无关={far:.4}（{}）", target.model);
    assert!(
        close > far,
        "重试相关的两条应比 JSON 词法更接近: {close:.4} vs {far:.4}"
    );

    // 相同输入必须得到相同向量，否则缓存键失去意义。
    let repeat = provider
        .embed(std::slice::from_ref(&inputs[0]))
        .await
        .expect("重复请求");
    let drift = codesearch::models::cosine_similarity(&vectors[0], &repeat[0]);
    assert!(drift > 0.999, "同一输入的向量应稳定: 相似度 {drift:.6}");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_model_end_to_end_index_and_search() {
    let Some(target) = target() else {
        return skip_notice("real_model_end_to_end_index_and_search");
    };
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), &config_toml(&target));

    let engine = Engine::open(EngineOptions {
        root: repo.path().to_path_buf(),
        config_path: None,
        data_dir: None,
        offline: false,
        mode: AccessMode::ReadWrite,
    })
    .await
    .expect("打开引擎");

    let report = engine.index(false).await.expect("真实模型索引");
    eprintln!(
        "[smoke] 模型={} 文件={} 代码块={} 新增向量={} 失败={}",
        target.model,
        report.scanned_files,
        report.current_chunks,
        report.embeddings_created,
        report.embeddings_failed
    );
    assert!(report.embeddings_created > 0, "应真实生成向量");
    assert_eq!(report.embeddings_failed, 0, "不应有失败的 Embedding");
    assert_eq!(report.embeddings_pending, 0, "索引结束后不应有待处理项");
    assert!(!report.degraded, "全部就绪时不应报告降级");

    // 纯自然语言查询：不含目标符号名，只能靠语义命中。
    let response = engine
        .search(&SearchRequest {
            query: "请求失败之后怎么等待再重试".to_string(),
            scope: ScopeFilter::default(),
            mode: SearchMode::Fast,
            max_results: Some(8),
            token_budget: None,
        })
        .await
        .expect("真实模型搜索");
    assert!(response.status.vector_search_used, "应真实走向量召回");
    assert!(!response.status.degraded);

    let paths: Vec<&str> = response
        .results
        .iter()
        .map(|result| result.path.as_str())
        .collect();
    eprintln!("[smoke] 命中路径: {paths:?}");
    assert!(
        paths.iter().any(|path| path.contains("retry")),
        "自然语言查询应召回重试相关文件: {paths:?}"
    );

    // 缓存复用：内容未变时重新索引不应再请求模型。
    let again = engine.index(true).await.expect("强制重扫");
    assert_eq!(again.embeddings_created, 0, "内容未变应全部命中缓存");
}
