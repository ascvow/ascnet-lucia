//! OpenAI-compatible `/embeddings` HTTP Provider。
//!
//! 请求体：`{"model": ..., "input": [...], "encoding_format": "float"}`；
//! 响应体：`{"data": [{"index": 0, "embedding": [...]}, ...]}`。
//!
//! 关键约定：
//!
//! - 响应的 `data` 顺序**不保证**与输入一致，因此一律按 `index` 字段重排后再校验，
//!   避免向量与代码块错位关联。
//! - API Key 只从环境变量读取，不写入源码、数据库和日志；错误信息也不包含它。
//! - 只对 429、5xx 与网络错误做有界重试；4xx 直接失败，避免无意义重试。
//! - 本地服务与远程服务共用同一套适配层，不在进程内加载模型。

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::EmbeddingConfig;
use crate::error::{Error, Result};
use crate::models::{normalize_l2, EmbeddingSpace};

/// Provider 类型标识，参与 Embedding 空间身份。
pub const PROVIDER_KIND: &str = "openai_compatible";

/// 探测维度时使用的输入。内容无意义，只为读取响应维度。
const PROBE_INPUT: &str = "codesearch dimension probe";
/// 重试基础退避时长。
const BASE_BACKOFF_MS: u64 = 250;
/// 单次退避上限。
const MAX_BACKOFF_MS: u64 = 8_000;
/// 错误信息中保留的响应体最大字符数。
const MAX_ERROR_BODY_CHARS: usize = 400;

#[derive(Debug, Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    /// 显式要求浮点数组，避免部分服务默认返回 base64。
    encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    #[serde(default)]
    data: Vec<EmbeddingItem>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingItem {
    /// 部分服务省略该字段，此时按数组顺序处理。
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    embedding: Vec<f32>,
}

/// Provider 内部共享状态，便于并发任务克隆。
///
/// 手写 [`std::fmt::Debug`]：派生实现会把 API Key 原样打印出来，
/// 任何一次 `tracing::debug!(?provider)` 就会把凭证写进日志。
struct Inner {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    /// 是否把 `dimensions` 写进请求体。
    request_dimensions: Option<usize>,
    max_retries: usize,
    semaphore: Semaphore,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            // 只暴露"是否配置了凭证"，不暴露取值。
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("request_dimensions", &self.request_dimensions)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

/// 真实 HTTP Embedding Provider。
#[derive(Debug)]
pub struct HttpProvider {
    inner: Arc<Inner>,
    batch_size: usize,
    space: EmbeddingSpace,
}

impl HttpProvider {
    /// 建立 Provider。
    ///
    /// `probed_dimensions` 用于复用上次探测结果；未配置 `dimensions` 且没有缓存时，
    /// 会发送一次极小的探测请求来确定维度，之后维度固定进入 Embedding 空间身份。
    pub async fn connect(
        config: &EmbeddingConfig,
        probed_dimensions: Option<usize>,
    ) -> Result<Self> {
        let api_key = read_api_key(&config.api_key_env)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| Error::Provider(format!("构建 HTTP 客户端失败: {e}")))?;
        let inner = Arc::new(Inner {
            client,
            endpoint: build_endpoint(&config.base_url),
            api_key,
            model: config.model.clone(),
            // 只有显式开启时才把 dimensions 写进请求体：多数服务会拒绝未知参数。
            request_dimensions: config
                .send_dimensions
                .then_some(config.dimensions)
                .flatten(),
            max_retries: config.max_retries,
            semaphore: Semaphore::new(config.max_concurrency.max(1)),
        });

        let dimensions = match config.dimensions.or(probed_dimensions) {
            Some(dimensions) => dimensions,
            None => probe_dimensions(&inner).await?,
        };
        if let Some(configured) = config.dimensions {
            if configured != dimensions {
                return Err(Error::Provider(format!(
                    "配置维度 {configured} 与服务返回维度 {dimensions} 不一致"
                )));
            }
        }

        Ok(Self {
            inner,
            batch_size: config.batch_size.max(1),
            space: EmbeddingSpace::new(PROVIDER_KIND, config, dimensions),
        })
    }

    /// 该 Provider 对应的 Embedding 空间。
    pub fn space(&self) -> &EmbeddingSpace {
        &self.space
    }

    /// 批量生成向量，返回顺序与输入严格一致。
    ///
    /// 输入按 `batch_size` 切批，批之间以 `max_concurrency` 为上限并发；
    /// 每个向量都会做 L2 归一化，与 [`EmbeddingSpace`] 声明的归一化规则一致。
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let batches: Vec<Vec<String>> = inputs
            .chunks(self.batch_size)
            .map(|chunk| chunk.to_vec())
            .collect();

        let mut tasks = tokio::task::JoinSet::new();
        for (index, batch) in batches.into_iter().enumerate() {
            let inner = Arc::clone(&self.inner);
            let dimensions = self.space.dimensions;
            tasks.spawn(async move {
                let result = embed_batch(&inner, &batch, dimensions).await;
                (index, result)
            });
        }

        let mut collected: Vec<Option<Vec<Vec<f32>>>> = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let (index, result) =
                joined.map_err(|e| Error::Provider(format!("Embedding 任务失败: {e}")))?;
            let vectors = result?;
            if collected.len() <= index {
                collected.resize(index + 1, None);
            }
            collected[index] = Some(vectors);
        }

        // 按批次序号拼接，保证整体顺序与输入一致。
        let mut out = Vec::with_capacity(inputs.len());
        for (index, batch) in collected.into_iter().enumerate() {
            let batch = batch
                .ok_or_else(|| Error::Provider(format!("第 {index} 批 Embedding 结果缺失")))?;
            out.extend(batch);
        }
        if out.len() != inputs.len() {
            return Err(Error::Provider(format!(
                "返回向量数量 {} 与输入数量 {} 不一致",
                out.len(),
                inputs.len()
            )));
        }
        Ok(out)
    }
}

/// 请求一批向量并按 `index` 重排。
async fn embed_batch(inner: &Inner, batch: &[String], dimensions: usize) -> Result<Vec<Vec<f32>>> {
    let permit = inner
        .semaphore
        .acquire()
        .await
        .map_err(|_| Error::Provider("Embedding 并发信号量已关闭".to_string()))?;
    let response = request_with_retry(inner, batch).await;
    drop(permit);
    let response = response?;

    if response.data.len() != batch.len() {
        return Err(Error::Provider(format!(
            "服务返回 {} 个向量，与请求的 {} 条输入不一致",
            response.data.len(),
            batch.len()
        )));
    }

    // data 顺序不保证，按 index 重排；index 缺失时退回数组顺序。
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; batch.len()];
    for (position, item) in response.data.into_iter().enumerate() {
        let target = item.index.unwrap_or(position);
        if target >= slots.len() {
            return Err(Error::Provider(format!(
                "响应中的 index {target} 超出请求范围 0..{}",
                slots.len()
            )));
        }
        if slots[target].is_some() {
            return Err(Error::Provider(format!("响应中的 index {target} 重复")));
        }
        slots[target] = Some(item.embedding);
    }

    let mut out = Vec::with_capacity(batch.len());
    for (position, slot) in slots.into_iter().enumerate() {
        let mut vector =
            slot.ok_or_else(|| Error::Provider(format!("响应缺少 index {position} 的向量")))?;
        if vector.is_empty() {
            return Err(Error::Provider(format!("index {position} 的向量为空")));
        }
        if vector.len() != dimensions {
            return Err(Error::Provider(format!(
                "index {position} 的向量维度 {} 与期望 {dimensions} 不一致",
                vector.len()
            )));
        }
        if let Some(bad) = vector.iter().position(|value| !value.is_finite()) {
            return Err(Error::Provider(format!(
                "index {position} 的第 {bad} 维不是有限数（NaN 或 Infinity）"
            )));
        }
        normalize_l2(&mut vector)?;
        out.push(vector);
    }
    Ok(out)
}

/// 带有界重试与退避的单次请求。
async fn request_with_retry(inner: &Inner, batch: &[String]) -> Result<EmbeddingResponse> {
    let mut attempt = 0usize;
    loop {
        match send_once(inner, batch).await {
            Ok(response) => return Ok(response),
            Err(failure) => {
                if !failure.retryable || attempt >= inner.max_retries {
                    return Err(failure.error);
                }
                // 指数退避并叠加确定性抖动，避免多批同时重试。
                let backoff = (BASE_BACKOFF_MS << attempt.min(5)).min(MAX_BACKOFF_MS);
                let jitter = (batch.len() as u64 * 17) % 100;
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
                attempt += 1;
            }
        }
    }
}

/// 请求失败信息，标记是否值得重试。
struct RequestFailure {
    error: Error,
    retryable: bool,
}

async fn send_once(
    inner: &Inner,
    batch: &[String],
) -> std::result::Result<EmbeddingResponse, RequestFailure> {
    let body = EmbeddingRequest {
        model: &inner.model,
        input: batch,
        encoding_format: "float",
        dimensions: inner.request_dimensions,
    };
    let mut request = inner.client.post(&inner.endpoint).json(&body);
    if let Some(key) = &inner.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|error| RequestFailure {
        // 网络错误信息不含请求头，因此不会泄漏凭证。
        retryable: error.is_timeout() || error.is_connect() || error.is_request(),
        error: Error::Provider(format!("请求 Embedding 服务失败: {error}")),
    })?;

    let status = response.status();
    if !status.is_success() {
        let retryable = status.as_u16() == 429 || status.is_server_error();
        let body = response.text().await.unwrap_or_default();
        return Err(RequestFailure {
            retryable,
            error: Error::Provider(format!(
                "Embedding 服务返回 {}: {}",
                status,
                truncate(&body, MAX_ERROR_BODY_CHARS)
            )),
        });
    }

    let text = response.text().await.map_err(|error| RequestFailure {
        retryable: true,
        error: Error::Provider(format!("读取 Embedding 响应失败: {error}")),
    })?;
    serde_json::from_str::<EmbeddingResponse>(&text).map_err(|error| RequestFailure {
        // 响应无法解析通常是服务端配置问题，重试没有意义。
        retryable: false,
        error: Error::Provider(format!(
            "解析 Embedding 响应失败: {error}；响应片段: {}",
            truncate(&text, MAX_ERROR_BODY_CHARS)
        )),
    })
}

/// 发送一次探测请求以确定向量维度。
async fn probe_dimensions(inner: &Inner) -> Result<usize> {
    let batch = vec![PROBE_INPUT.to_string()];
    let response = request_with_retry(inner, &batch).await?;
    let dimensions = response
        .data
        .first()
        .map(|item| item.embedding.len())
        .unwrap_or(0);
    if dimensions == 0 {
        return Err(Error::Provider(
            "探测维度失败：服务未返回有效向量".to_string(),
        ));
    }
    Ok(dimensions)
}

/// 从环境变量读取 API Key。
///
/// 变量名为空表示该服务不需要鉴权（常见于本地推理服务）。
/// 变量名已配置但未设置值时报错，避免静默发出未鉴权请求。
fn read_api_key(env_name: &str) -> Result<Option<String>> {
    let name = env_name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Err(Error::Config(format!("环境变量 {name} 为空"))),
        Err(_) => Err(Error::Config(format!(
            "环境变量 {name} 未设置；请设置后重试，或把 embedding.api_key_env 留空以免鉴权"
        ))),
    }
}

/// 由 `base_url` 推导 `/embeddings` 端点。
///
/// 已经指向 `/embeddings` 的地址原样使用，便于对接非标准路径的本地服务。
pub fn build_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/embeddings") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/embeddings")
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let cleaned = text.trim();
    if cleaned.chars().count() <= max_chars {
        return cleaned.to_string();
    }
    cleaned.chars().take(max_chars).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_derivation_handles_common_shapes() {
        assert_eq!(
            build_endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            build_endpoint("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/embeddings"
        );
        // 已经是完整端点时不重复追加。
        assert_eq!(
            build_endpoint("http://127.0.0.1:8080/v1/embeddings"),
            "http://127.0.0.1:8080/v1/embeddings"
        );
    }

    #[test]
    fn api_key_env_is_required_only_when_configured() {
        assert!(read_api_key("").expect("空变量名允许").is_none());
        let missing = read_api_key("CODESEARCH_TEST_MISSING_KEY_VAR").expect_err("应报错");
        assert!(matches!(missing, Error::Config(_)));
        // 错误信息只提到变量名，不含任何取值。
        assert!(missing
            .to_string()
            .contains("CODESEARCH_TEST_MISSING_KEY_VAR"));
    }

    #[test]
    fn error_body_is_truncated() {
        let long = "x".repeat(1000);
        let truncated = truncate(&long, 10);
        assert_eq!(truncated.chars().count(), 11); // 10 个字符加省略号
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn request_body_shape_matches_openai_protocol() {
        let inputs = vec!["a".to_string(), "b".to_string()];
        let body = EmbeddingRequest {
            model: "text-embedding-3-small",
            input: &inputs,
            encoding_format: "float",
            dimensions: Some(768),
        };
        let json = serde_json::to_value(&body).expect("序列化");
        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["encoding_format"], "float");
        assert_eq!(json["dimensions"], 768);
        assert_eq!(json["input"][1], "b");

        // 未配置维度时不写入该字段，兼容不支持 dimensions 的服务。
        let without = EmbeddingRequest {
            model: "m",
            input: &inputs,
            encoding_format: "float",
            dimensions: None,
        };
        let json = serde_json::to_value(&without).expect("序列化");
        assert!(json.get("dimensions").is_none());
    }

    #[test]
    fn response_parsing_accepts_out_of_order_and_missing_index() {
        let text =
            r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#;
        let parsed: EmbeddingResponse = serde_json::from_str(text).expect("解析");
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].index, Some(1));

        let without_index = r#"{"data":[{"embedding":[1.0]}]}"#;
        let parsed: EmbeddingResponse = serde_json::from_str(without_index).expect("解析");
        assert_eq!(parsed.data[0].index, None);
    }
}
