//! Embedding 空间身份、缓存键与 Provider。
//!
//! 本模块只负责模型调用与向量校验，不涉及检索排序或存储事务。
//!
//! 安全约束：API Key 只从环境变量读取，不写入源码、数据库和日志；
//! 未经明确配置和启用时不会向任何远程服务发送源码。

pub mod http;
#[cfg(feature = "test-support")]
pub mod mock;
pub mod rerank;

use crate::chunker::EMBED_TEMPLATE_VERSION;
use crate::config::{Config, EmbeddingConfig, ProviderKind};
use crate::error::{Error, Result};
use crate::store::EmbeddingSpaceRow;

/// 向量距离度量。第一版固定使用余弦距离，并对向量做 L2 归一化。
pub const METRIC_COSINE: &str = "cosine";
/// 归一化规则标识。
pub const NORMALIZE_L2: &str = "l2";

/// Embedding 空间身份。
///
/// 身份包含模型标识、模型修订（`cache_namespace`）、向量维度、距离度量、归一化规则、
/// query/document 输入配置与输入模板版本。任何一项变化都会产生新的空间，
/// 因此同维度但不同模型的向量不会被混用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpace {
    /// 由全部身份字段派生的哈希。
    pub id: String,
    /// Provider 类型标识。
    pub provider: String,
    /// 模型名。
    pub model: String,
    /// 模型修订或显式缓存命名空间。
    pub cache_namespace: String,
    /// 向量维度。
    pub dimensions: usize,
    /// 距离度量。
    pub metric: String,
    /// 归一化规则。
    pub normalize: String,
    /// 查询输入前缀。
    pub query_prefix: String,
    /// 文档输入前缀。
    pub document_prefix: String,
    /// 输入模板版本。
    pub template_version: u32,
}

impl EmbeddingSpace {
    /// 由配置与已确定的维度构造空间身份。
    pub fn new(provider: &str, config: &EmbeddingConfig, dimensions: usize) -> Self {
        let mut space = Self {
            id: String::new(),
            provider: provider.to_string(),
            model: config.model.clone(),
            cache_namespace: config.cache_namespace.clone(),
            dimensions,
            metric: METRIC_COSINE.to_string(),
            normalize: NORMALIZE_L2.to_string(),
            query_prefix: config.query_prefix.clone(),
            document_prefix: config.document_prefix.clone(),
            template_version: EMBED_TEMPLATE_VERSION,
        };
        space.id = space.compute_id();
        space
    }

    /// 计算身份哈希。字段之间用 `\n` 分隔，字段名写入摘要，避免不同字段拼接歧义。
    fn compute_id(&self) -> String {
        let canonical = format!(
            "provider={}\nmodel={}\nnamespace={}\ndimensions={}\nmetric={}\nnormalize={}\nquery_prefix={}\ndocument_prefix={}\ntemplate={}\n",
            self.provider,
            self.model,
            self.cache_namespace,
            self.dimensions,
            self.metric,
            self.normalize,
            self.query_prefix,
            self.document_prefix,
            self.template_version
        );
        blake3::hash(canonical.as_bytes()).to_hex()[..24].to_string()
    }

    /// 计算某份文档输入对应的缓存键。
    ///
    /// 键由 `hash(embedding_space_id + 实际发送给模型的完整文档输入)` 得出，
    /// 因此空间变化或输入变化都不会命中旧缓存。
    pub fn cache_key(&self, document_input: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.id.as_bytes());
        hasher.update(b"\n");
        hasher.update(document_input.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// 转换为可写入数据库的行。
    pub fn to_row(&self) -> EmbeddingSpaceRow {
        EmbeddingSpaceRow {
            id: self.id.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            cache_namespace: self.cache_namespace.clone(),
            dimensions: self.dimensions,
            metric: self.metric.clone(),
            normalize: self.normalize.clone(),
            query_prefix: self.query_prefix.clone(),
            document_prefix: self.document_prefix.clone(),
            template_version: self.template_version,
        }
    }

    /// 给查询文本加上查询前缀（部分模型要求区分 query 与 document）。
    pub fn query_input(&self, query: &str) -> String {
        if self.query_prefix.is_empty() {
            query.to_string()
        } else {
            format!("{}{}", self.query_prefix, query)
        }
    }
}

/// Embedding Provider。
///
/// 使用枚举而不是 `dyn trait`：只有"真实 HTTP"与"测试用确定性 Mock"两种实现，
/// 且 Mock 只在 `test-support` feature 下存在，生产配置无法选择它。
#[derive(Debug)]
pub enum Provider {
    /// OpenAI-compatible `/embeddings` HTTP 协议（本地或远程服务均可）。
    Http(http::HttpProvider),
    /// 仅测试可用的确定性 Mock。
    #[cfg(feature = "test-support")]
    Mock(mock::MockProvider),
}

impl Provider {
    /// 该 Provider 对应的 Embedding 空间。
    pub fn space(&self) -> &EmbeddingSpace {
        match self {
            Provider::Http(provider) => provider.space(),
            #[cfg(feature = "test-support")]
            Provider::Mock(provider) => provider.space(),
        }
    }

    /// 批量生成向量。
    ///
    /// 返回向量与输入一一对应且顺序一致；数量、维度、NaN/Infinity 与空向量都会被校验。
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = match self {
            Provider::Http(provider) => provider.embed(inputs).await?,
            #[cfg(feature = "test-support")]
            Provider::Mock(provider) => provider.embed(inputs).await?,
        };
        validate_batch(inputs.len(), self.space().dimensions, &vectors)?;
        Ok(vectors)
    }

    /// 生成单个查询向量（自动加上查询前缀）。
    pub async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let input = self.space().query_input(query);
        let mut vectors = self.embed(&[input]).await?;
        vectors
            .pop()
            .ok_or_else(|| Error::Provider("查询向量为空".to_string()))
    }
}

/// 根据配置构建 Provider。
///
/// - `provider = "none"` 或 `offline = true` 时返回 `None`，检索退化为符号与全文搜索。
/// - 维度未配置时，通过一次探测请求确定维度；`probed_dimensions` 可复用上次结果，避免重复探测。
pub async fn build_provider(
    config: &Config,
    offline: bool,
    probed_dimensions: Option<usize>,
) -> Result<Option<Provider>> {
    if offline || config.embedding.provider == ProviderKind::None {
        return Ok(None);
    }
    let provider = http::HttpProvider::connect(&config.embedding, probed_dimensions).await?;
    Ok(Some(Provider::Http(provider)))
}

/// 校验一批返回向量：数量、维度、有限性与非空。
pub fn validate_batch(expected: usize, dimensions: usize, vectors: &[Vec<f32>]) -> Result<()> {
    if vectors.len() != expected {
        return Err(Error::Provider(format!(
            "返回向量数量 {} 与输入数量 {expected} 不一致",
            vectors.len()
        )));
    }
    for (index, vector) in vectors.iter().enumerate() {
        if vector.is_empty() {
            return Err(Error::Provider(format!("第 {index} 个向量为空")));
        }
        if vector.len() != dimensions {
            return Err(Error::Provider(format!(
                "第 {index} 个向量维度 {} 与期望 {dimensions} 不一致",
                vector.len()
            )));
        }
        if let Some(bad) = vector.iter().position(|value| !value.is_finite()) {
            return Err(Error::Provider(format!(
                "第 {index} 个向量的第 {bad} 维不是有限数（NaN 或 Infinity）"
            )));
        }
    }
    Ok(())
}

/// 对向量做 L2 归一化。
///
/// 归一化后余弦距离与内积等价，也让 USearch 的距离更稳定。
/// 零向量无法归一化，视为 Provider 返回了无意义结果。
pub fn normalize_l2(vector: &mut [f32]) -> Result<()> {
    let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(Error::Provider("向量模长为 0，无法归一化".to_string()));
    }
    for value in vector.iter_mut() {
        *value /= norm;
    }
    Ok(())
}

/// 余弦相似度（要求两个向量已归一化，长度相同）。
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return -1.0;
    }
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

/// 重排接口预留。
///
/// 第一版不依赖重排即可完成检索：默认实现保持候选顺序不变。
/// 这里刻意只留一个最小 trait，不建立插件系统。
pub trait Reranker {
    /// 对候选重新排序，返回与输入等长的分数。分数越大越相关。
    fn rerank(&self, query: &str, documents: &[String]) -> Vec<f32>;
}

/// 恒等重排：保持原有顺序，用于表明检索链路不依赖重排。
#[derive(Debug, Default, Clone, Copy)]
pub struct IdentityReranker;

impl Reranker for IdentityReranker {
    fn rerank(&self, _query: &str, documents: &[String]) -> Vec<f32> {
        (0..documents.len()).map(|index| -(index as f32)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmbeddingConfig;

    fn config(model: &str, namespace: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            model: model.to_string(),
            cache_namespace: namespace.to_string(),
            ..EmbeddingConfig::default()
        }
    }

    #[test]
    fn space_id_changes_with_every_identity_field() {
        let base = EmbeddingSpace::new("openai_compatible", &config("m1", "r1"), 768);
        // 模型不同。
        assert_ne!(
            base.id,
            EmbeddingSpace::new("openai_compatible", &config("m2", "r1"), 768).id
        );
        // 模型修订不同。
        assert_ne!(
            base.id,
            EmbeddingSpace::new("openai_compatible", &config("m1", "r2"), 768).id
        );
        // 维度不同。
        assert_ne!(
            base.id,
            EmbeddingSpace::new("openai_compatible", &config("m1", "r1"), 1024).id
        );
        // Provider 不同。
        assert_ne!(
            base.id,
            EmbeddingSpace::new("other", &config("m1", "r1"), 768).id
        );
        // 前缀不同。
        let mut prefixed = config("m1", "r1");
        prefixed.query_prefix = "query: ".into();
        assert_ne!(
            base.id,
            EmbeddingSpace::new("openai_compatible", &prefixed, 768).id
        );
        // 相同配置必须得到相同身份。
        assert_eq!(
            base.id,
            EmbeddingSpace::new("openai_compatible", &config("m1", "r1"), 768).id
        );
    }

    #[test]
    fn same_dimensions_different_model_do_not_share_cache_keys() {
        let left = EmbeddingSpace::new("openai_compatible", &config("m1", "r1"), 768);
        let right = EmbeddingSpace::new("openai_compatible", &config("m2", "r1"), 768);
        let input = "Language: rust\nPath: a.rs\nCode:\nfn a() {}";
        assert_ne!(left.cache_key(input), right.cache_key(input));
        // 同空间同输入必须稳定复用。
        assert_eq!(left.cache_key(input), left.cache_key(input));
        // 输入变化即换键。
        assert_ne!(left.cache_key(input), left.cache_key("other"));
    }

    #[test]
    fn query_prefix_is_applied_only_when_configured() {
        let mut cfg = config("m1", "r1");
        let plain = EmbeddingSpace::new("p", &cfg, 4);
        assert_eq!(plain.query_input("重试"), "重试");
        cfg.query_prefix = "query: ".into();
        let prefixed = EmbeddingSpace::new("p", &cfg, 4);
        assert_eq!(prefixed.query_input("重试"), "query: 重试");
    }

    #[test]
    fn batch_validation_rejects_bad_provider_responses() {
        // 数量不符。
        assert!(validate_batch(2, 3, &[vec![1.0, 0.0, 0.0]]).is_err());
        // 维度不符。
        assert!(validate_batch(1, 3, &[vec![1.0, 0.0]]).is_err());
        // 空向量。
        assert!(validate_batch(1, 3, &[vec![]]).is_err());
        // NaN 与 Infinity。
        assert!(validate_batch(1, 3, &[vec![1.0, f32::NAN, 0.0]]).is_err());
        assert!(validate_batch(1, 3, &[vec![1.0, f32::INFINITY, 0.0]]).is_err());
        // 合法响应。
        validate_batch(2, 3, &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]).expect("合法");
    }

    #[test]
    fn normalize_and_cosine_behave_as_expected() {
        let mut vector = vec![3.0f32, 4.0];
        normalize_l2(&mut vector).expect("normalize");
        assert!((vector[0] - 0.6).abs() < 1e-6);
        assert!((vector[1] - 0.8).abs() < 1e-6);
        assert!((cosine_similarity(&vector, &vector) - 1.0).abs() < 1e-6);
        // 零向量必须报错而不是产生 NaN。
        assert!(normalize_l2(&mut [0.0, 0.0]).is_err());
    }

    #[tokio::test]
    async fn offline_and_none_provider_yield_no_provider() {
        let mut config = Config::default();
        // 未配置模型。
        assert!(build_provider(&config, false, None)
            .await
            .expect("none")
            .is_none());
        // 即使配置了模型，offline 也不允许建立连接。
        config.embedding.provider = ProviderKind::OpenaiCompatible;
        config.embedding.base_url = "http://127.0.0.1:1/v1".into();
        config.embedding.model = "test".into();
        assert!(build_provider(&config, true, None)
            .await
            .expect("offline")
            .is_none());
    }

    #[test]
    fn identity_reranker_preserves_order() {
        let scores = IdentityReranker.rerank("q", &["a".into(), "b".into(), "c".into()]);
        assert_eq!(scores.len(), 3);
        assert!(scores[0] > scores[1] && scores[1] > scores[2]);
    }
}
