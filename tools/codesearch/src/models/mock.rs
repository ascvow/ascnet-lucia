//! 确定性 Mock Provider，仅在 `test-support` feature 下编译。
//!
//! 这**不是**语义搜索：向量由词袋哈希投影得出，只保证"相同输入得到相同向量"
//! 与"共享词条越多越接近"，用于验证索引流水线、缓存、向量日志与恢复逻辑。
//! 生产配置无法选择本 Provider（[`crate::config::ProviderKind`] 中没有对应取值），
//! 因此不存在用伪向量冒充语义检索的可能。
//!
//! 用 Mock 得到的检索效果不能作为真实语义质量结论。

use crate::config::EmbeddingConfig;
use crate::error::Result;
use crate::models::{normalize_l2, EmbeddingSpace};
use crate::retrieval::tokenize::{extract_terms, space_cjk};

/// Provider 类型标识。与 HTTP Provider 不同，因此两者的 Embedding 空间天然隔离。
pub const PROVIDER_KIND: &str = "mock_deterministic";

/// 确定性 Mock Provider。
#[derive(Debug, Clone)]
pub struct MockProvider {
    space: EmbeddingSpace,
    /// 记录调用次数，供测试断言缓存是否生效。
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl MockProvider {
    /// 按给定维度构造 Mock Provider。
    pub fn new(config: &EmbeddingConfig, dimensions: usize) -> Self {
        Self {
            space: EmbeddingSpace::new(PROVIDER_KIND, config, dimensions),
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// 便捷构造：使用默认配置与指定模型名。
    pub fn with_model(model: &str, dimensions: usize) -> Self {
        let config = EmbeddingConfig {
            model: model.to_string(),
            ..EmbeddingConfig::default()
        };
        Self::new(&config, dimensions)
    }

    /// 对应的 Embedding 空间。
    pub fn space(&self) -> &EmbeddingSpace {
        &self.space
    }

    /// 累计的模型调用次数（按输入条数统计）。
    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 生成确定性向量。
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls
            .fetch_add(inputs.len(), std::sync::atomic::Ordering::SeqCst);
        inputs.iter().map(|input| self.embed_one(input)).collect()
    }

    /// 词袋哈希投影：每个词条散列到若干维度并累加，再做 L2 归一化。
    fn embed_one(&self, input: &str) -> Result<Vec<f32>> {
        let dimensions = self.space.dimensions;
        let mut vector = vec![0.0f32; dimensions];
        // 与索引侧一致地处理中文，保证中文查询与中文文档能产生重叠词条。
        let terms = extract_terms(&space_cjk(input));
        let mut any = false;
        for term in terms.split_whitespace() {
            any = true;
            let hash = blake3::hash(term.as_bytes());
            let bytes = hash.as_bytes();
            // 每个词条影响 4 个维度，符号由哈希决定，降低维度冲突的影响。
            for slot in 0..4 {
                let offset = slot * 5;
                let index = u32::from_le_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ]) as usize
                    % dimensions;
                let sign = if bytes[offset + 4].is_multiple_of(2) { 1.0 } else { -1.0 };
                vector[index] += sign;
            }
        }
        if !any {
            // 空输入也必须返回合法向量，否则无法与真实 Provider 的校验保持一致。
            vector[0] = 1.0;
        }
        normalize_l2(&mut vector)?;
        Ok(vector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::cosine_similarity;

    #[tokio::test]
    async fn same_input_always_yields_same_vector() {
        let provider = MockProvider::with_model("mock", 64);
        let first = provider.embed(&["fn retry() {}".to_string()]).await.expect("embed");
        let second = provider.embed(&["fn retry() {}".to_string()]).await.expect("embed");
        assert_eq!(first, second);
        assert_eq!(provider.call_count(), 2);
    }

    #[tokio::test]
    async fn shared_terms_increase_similarity() {
        let provider = MockProvider::with_model("mock", 256);
        let vectors = provider
            .embed(&[
                "retry policy execute backoff".to_string(),
                "retry policy execute delay".to_string(),
                "unrelated json parser tokens".to_string(),
            ])
            .await
            .expect("embed");
        let close = cosine_similarity(&vectors[0], &vectors[1]);
        let far = cosine_similarity(&vectors[0], &vectors[2]);
        assert!(close > far, "共享词条应更接近: {close} vs {far}");
    }

    #[tokio::test]
    async fn vectors_are_normalized_and_finite() {
        let provider = MockProvider::with_model("mock", 32);
        let vectors = provider
            .embed(&["中文注释与代码".to_string(), String::new()])
            .await
            .expect("embed");
        for vector in vectors {
            assert_eq!(vector.len(), 32);
            assert!(vector.iter().all(|v| v.is_finite()));
            let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "必须归一化: {norm}");
        }
    }

    #[test]
    fn mock_space_differs_from_http_space() {
        let config = EmbeddingConfig {
            model: "same-model".to_string(),
            ..EmbeddingConfig::default()
        };
        let mock = MockProvider::new(&config, 768);
        let http_space = EmbeddingSpace::new(crate::models::http::PROVIDER_KIND, &config, 768);
        // 同模型同维度，但 provider 不同，空间身份必须不同。
        assert_ne!(mock.space().id, http_space.id);
    }
}
