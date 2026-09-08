//! 交叉编码器 `/rerank` 协议；仅显式配置后发送候选正文，失败不冒充成功重排。

use crate::{
    config::RerankConfig,
    error::{Error, Result},
};
use serde::Deserialize;
use std::time::Duration;

/// 可复用 HTTP 客户端；不实现含凭证的 Debug 输出。
pub struct HttpReranker {
    client: reqwest::Client,
    endpoint: String,
    key: Option<String>,
    config: RerankConfig,
    permits: tokio::sync::Semaphore,
}

/// 服务可任意排列结果，必须用 index 对齐原始候选。
#[derive(Deserialize)]
struct Response {
    results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    index: usize,
    relevance_score: f64,
}

impl HttpReranker {
    /// 构造客户端但不发送请求；配置或凭证无效时返回错误。
    pub fn new(config: &RerankConfig) -> Result<Self> {
        let mut validation = crate::config::Config {
            rerank: Some(config.clone()),
            ..Default::default()
        };
        validation.retrieval.candidate_limit = config.max_candidates;
        validation.validate()?;
        let base = config.base_url.trim_end_matches('/');
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| Error::Provider("重排客户端构造失败".into()))?,
            endpoint: if base.ends_with("/rerank") {
                base.into()
            } else {
                format!("{base}/rerank")
            },
            key: super::http::read_api_key(&config.api_key_env)?,
            config: config.clone(),
            permits: tokio::sync::Semaphore::new(2),
        })
    }

    /// 返回与文档一一对应的有限分数；空输入不发网络请求。超时包含并发排队，不自动重试。
    pub async fn score(&self, query: &str, documents: &[String]) -> Result<Vec<f64>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        if documents.len() > self.config.max_candidates
            || documents
                .iter()
                .any(|d| d.chars().count() > self.config.max_input_chars)
        {
            return Err(Error::Provider("重排文档数量或长度超出配置上限".into()));
        }
        tokio::time::timeout(Duration::from_secs(self.config.timeout_secs), async {
            let _permit = self.permits.acquire().await.map_err(|_| Error::Provider("重排并发队列已关闭".into()))?;
            let body = serde_json::json!({"model":self.config.model,"query":query,"documents":documents,
                "top_n":documents.len(),"return_documents":false});
            let mut request = self.client.post(&self.endpoint).json(&body);
            if let Some(key) = &self.key { request = request.bearer_auth(key); }
            let mut response = request.send().await.map_err(|_| Error::Provider("重排网络请求失败".into()))?;
            if !response.status().is_success() {
                return Err(Error::Provider(format!("重排服务返回 HTTP {}", response.status().as_u16())));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| Error::Provider("重排响应读取失败".into()))? {
                if bytes.len() + chunk.len() > 1_048_576 { return Err(Error::Provider("重排响应超出 1 MiB 上限".into())); }
                bytes.extend_from_slice(&chunk);
            }
            let response: Response = serde_json::from_slice(&bytes).map_err(|_| Error::Provider("重排响应不是有效的结果 JSON".into()))?;
            align(response, documents.len())
        }).await.map_err(|_| Error::Provider("重排服务超时，未返回未经重排的替代结果".into()))?
    }
}

/// 严格校验完整的一一映射，拒绝重复、缺项、越界和非有限分数。
fn align(response: Response, count: usize) -> Result<Vec<f64>> {
    if response.results.len() != count {
        return Err(Error::Provider("重排结果数量不匹配".into()));
    }
    let mut scores = vec![None; count];
    for item in response.results {
        if item.index >= count || scores[item.index].is_some() || !item.relevance_score.is_finite()
        {
            return Err(Error::Provider("重排结果索引或分数无效".into()));
        }
        scores[item.index] = Some(item.relevance_score);
    }
    scores
        .into_iter()
        .map(|value| value.ok_or_else(|| Error::Provider("重排结果缺项".into())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 服务端的数组顺序不可信，非法响应不得绑定到错误代码块。
    #[test]
    fn validates_and_aligns_scores() {
        let good: Response = serde_json::from_str(
            r#"{"results":[{"index":1,"relevance_score":0.9},{"index":0,"relevance_score":0.1}]}"#,
        )
        .unwrap();
        assert_eq!(align(good, 2).unwrap(), vec![0.1, 0.9]);
        for indices in [[0, 0], [0, 2]] {
            assert!(align(
                Response {
                    results: indices
                        .into_iter()
                        .map(|index| Item {
                            index,
                            relevance_score: 0.5
                        })
                        .collect()
                },
                2
            )
            .is_err());
        }
        assert!(align(Response { results: vec![] }, 1).is_err());
        assert!(align(
            Response {
                results: vec![Item {
                    index: 0,
                    relevance_score: f64::NAN
                }]
            },
            1
        )
        .is_err());
    }
}
