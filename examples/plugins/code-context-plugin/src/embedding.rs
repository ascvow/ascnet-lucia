//! 嵌入 provider 边界。
//!
//! 这一层的存在理由是**隔离**，不是复用：检索核心只依赖 [`EmbeddingProvider`]，因此
//! Local 与 API 两条分支的可用性变化不会渗进索引与检索代码。
//!
//! 当前两个真实实现都不存在，原因写在各自的 ADR 中：
//!
//! - API：宿主在加载期拒绝 `http` 能力，见 `adr-api-embedding.md`。
//! - Local：`host-fs-read` 只返回 UTF-8，权重文件在 ABI 层面读不出来，见
//!   `adr-local-embedding.md`。
//!
//! 因此本阶段只提供 [`UnconfiguredEmbeddingProvider`]，它对每次调用都明确报告不可用。
//! **它不返回任何伪造向量**——一份看似成功的随机向量会让整条语义链路的错误推迟到
//! 检索结果里才暴露。

use crate::config::{CodeContextConfig, EmbeddingMode};
use crate::error::CodeContextError;
use crate::model::EmbeddingSpace;

/// 单批默认最大输入条数。
///
/// 远端嵌入端点普遍对单次请求的条数与总字节数设限；本地推理则受批大小与显存约束。
/// 这个默认值只在 provider 未声明自己的上限时生效。
pub const DEFAULT_MAX_BATCH_SIZE: usize = 64;

/// 嵌入输入的用途。
///
/// 查询与文档通常使用不同的提示模板，因此两者必须在接口上区分，而不是靠调用方
/// 自行拼接——模板版本是 [`EmbeddingSpace`] 的一部分，混用会让向量不可比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingInputKind {
    /// 检索查询。
    Query,
    /// 待索引的代码块。
    Document,
}

impl EmbeddingInputKind {
    /// 返回稳定的机器可读名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Document => "document",
        }
    }
}

/// 嵌入 provider。
///
/// [`EmbeddingProvider::space`] 与 [`EmbeddingProvider::embed`] 是 Phase 1 冻结的最小契约，
/// 实现者只需提供这两项；其余方法都有默认实现，从嵌入空间派生或建立在 `embed` 之上。
/// 这样新增 provider 的成本是两个方法，而分批、维度校验与降级判断对所有 provider 一致。
pub trait EmbeddingProvider {
    /// 返回该 provider 当前的嵌入空间；未配置时返回 `None`。
    ///
    /// 语义索引的兼容性判断依赖它：返回 `None` 表示当前没有可用的语义能力，
    /// 已有向量应当保留而不是重建。
    fn space(&self) -> Option<EmbeddingSpace>;

    /// 批量计算嵌入向量。
    ///
    /// 实现者可以假定 `inputs` 长度不超过 [`EmbeddingProvider::max_batch_size`]：
    /// 分批由 [`EmbeddingProvider::embed_documents`] 负责。
    ///
    /// # Errors
    ///
    /// provider 不可用或远端失败时返回 [`CodeContextError::EmbeddingUnavailable`]。
    /// 调用方必须把该错误处理为「进入降级检索」，而不是让整次检索失败。
    fn embed(
        &self,
        kind: EmbeddingInputKind,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, CodeContextError>;

    /// 单次 [`EmbeddingProvider::embed`] 允许的最大输入条数。
    fn max_batch_size(&self) -> usize {
        DEFAULT_MAX_BATCH_SIZE
    }

    /// provider 实例标识；未配置时为 `None`。
    fn provider_id(&self) -> Option<String> {
        self.space().map(|space| space.provider_id)
    }

    /// 模型标识；未配置时为 `None`。
    fn model_id(&self) -> Option<String> {
        self.space().map(|space| space.model)
    }

    /// 向量维度；未配置时为 `None`。
    fn dimension(&self) -> Option<u32> {
        self.space().map(|space| space.dimension)
    }

    /// 探测 provider 是否可用。
    ///
    /// 默认实现只检查是否声明了嵌入空间。真实 provider 应当覆盖它并做一次轻量探测
    /// （API 打一次最小请求、Local 确认模型已加载），使**降级判断不必等到第一次真正
    /// 嵌入失败**才发生。
    ///
    /// # Errors
    ///
    /// provider 不可用时返回 [`CodeContextError::EmbeddingUnavailable`]。
    fn health_check(&self) -> Result<(), CodeContextError> {
        match self.space() {
            Some(_) => Ok(()),
            None => Err(CodeContextError::EmbeddingUnavailable(
                "当前没有可用的嵌入 provider".into(),
            )),
        }
    }

    /// 嵌入一批待索引文档，自动按 [`EmbeddingProvider::max_batch_size`] 分批。
    ///
    /// 返回的向量与 `inputs` 一一对应且顺序一致。
    ///
    /// # Errors
    ///
    /// provider 失败，或返回的向量条数、维度与预期不符时返回错误。
    fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, CodeContextError> {
        embed_batched(self, EmbeddingInputKind::Document, inputs)
    }

    /// 嵌入一条检索查询。
    ///
    /// 与 [`EmbeddingProvider::embed_documents`] 分开，是因为两者使用不同的提示模板；
    /// 模板版本参与 [`EmbeddingSpace`] 的标识，混用会让查询向量与文档向量不可比较。
    ///
    /// # Errors
    ///
    /// provider 失败或未返回恰好一条向量时返回错误。
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CodeContextError> {
        let mut vectors = embed_batched(
            self,
            EmbeddingInputKind::Query,
            std::slice::from_ref(&query.to_string()),
        )?;
        if vectors.len() != 1 {
            return Err(CodeContextError::EmbeddingUnavailable(format!(
                "查询嵌入应返回 1 条向量，实际 {}",
                vectors.len()
            )));
        }
        Ok(vectors.remove(0))
    }
}

/// 按 provider 声明的批大小分批调用，并校验返回结果。
///
/// # 为什么校验条数与维度
///
/// 向量与 chunk 是**按下标对齐**的。provider 少返回一条，后面全部向量就会错位挂到别的
/// chunk 上——这种错误不会让检索失败，只会让结果长期缓慢地不正确，是最难发现的一类 bug。
/// 在抽象层一次性挡住，比让每个 provider 各自记得检查可靠。
fn embed_batched<P: EmbeddingProvider + ?Sized>(
    provider: &P,
    kind: EmbeddingInputKind,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, CodeContextError> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let batch_size = provider.max_batch_size().max(1);
    let expected_dimension = provider.dimension();
    let mut vectors = Vec::with_capacity(inputs.len());

    for batch in inputs.chunks(batch_size) {
        let produced = provider.embed(kind, batch)?;
        if produced.len() != batch.len() {
            return Err(CodeContextError::EmbeddingUnavailable(format!(
                "{} 嵌入返回 {} 条向量，与输入的 {} 条不符",
                kind.as_str(),
                produced.len(),
                batch.len()
            )));
        }
        if let Some(dimension) = expected_dimension {
            if let Some(actual) = produced
                .iter()
                .map(Vec::len)
                .find(|length| *length != dimension as usize)
            {
                return Err(CodeContextError::EmbeddingUnavailable(format!(
                    "嵌入维度为 {actual}，与嵌入空间声明的 {dimension} 不符"
                )));
            }
        }
        vectors.extend(produced);
    }
    Ok(vectors)
}

/// 按配置选择嵌入 provider。
///
/// 两种模式当前都返回 [`UnconfiguredEmbeddingProvider`]：Api 分支等待宿主 HTTP 能力，
/// Local 分支等待宿主受控推理能力，理由见各自 ADR 与 `PROGRESS.md` 的 Blocked Phases。
/// **真实实现在 Phase 7 与 Phase 12 接入时替换本函数的分支，调用方不需要改动。**
///
/// 刻意不提供"选不到就用 Mock"的回退：伪造向量会让语义链路的错误推迟到检索结果里才暴露。
pub fn select_provider(config: &CodeContextConfig) -> Box<dyn EmbeddingProvider> {
    Box::new(UnconfiguredEmbeddingProvider::new(config.embedding_mode))
}

/// 尚未配置的嵌入 provider。
///
/// 命名刻意说明状态而不是伪装成实现：调用它永远失败，且失败原因指出缺的是哪一项
/// 宿主能力，使用户看到的是「还没有这个能力」而不是「检索坏了」。
#[derive(Debug, Clone, Copy)]
pub struct UnconfiguredEmbeddingProvider {
    /// 用户配置的目标模式，用于给出对应的不可用原因。
    mode: EmbeddingMode,
}

impl UnconfiguredEmbeddingProvider {
    /// 按目标模式创建未配置 provider。
    pub fn new(mode: EmbeddingMode) -> Self {
        Self { mode }
    }

    /// 返回面向用户的不可用原因。
    pub fn reason(self) -> String {
        match self.mode {
            EmbeddingMode::Api => {
                "API 嵌入需要宿主提供 HTTP 能力，当前宿主在加载期拒绝该声明".to_string()
            }
            EmbeddingMode::Local => {
                "本地嵌入需要宿主提供受控推理能力，当前插件运行时无法加载模型权重".to_string()
            }
        }
    }
}

impl EmbeddingProvider for UnconfiguredEmbeddingProvider {
    fn space(&self) -> Option<EmbeddingSpace> {
        None
    }

    fn embed(
        &self,
        _kind: EmbeddingInputKind,
        _inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, CodeContextError> {
        Err(CodeContextError::EmbeddingUnavailable(self.reason()))
    }
}

/// 产出确定性伪向量的测试 provider。
///
/// # 只能出现在测试中
///
/// 它带 `#[cfg(test)]`，因此**不会被编译进插件产物**，也无法被 [`select_provider`] 选中。
/// 这条约束不是形式主义：一份看起来成功的随机向量会让整条语义链路的错误推迟到检索结果
/// 里才暴露，比明确的"能力不可用"有害得多。骨架阶段确立的"绝不返回伪造向量"在生产路径上
/// 依然成立。
///
/// 它的用途是在真实 provider 缺席时验证**接口契约本身**：分批、维度校验、
/// 查询与文档模板分离、失败降级。
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct MockEmbeddingProvider {
    /// 声明的嵌入空间。
    space: EmbeddingSpace,
    /// 单批最大条数。
    batch_size: usize,
    /// 非空时每次调用都以该原因失败，用于测试降级路径。
    failure: Option<String>,
    /// 强制返回的向量维度，用于测试维度校验。
    forced_dimension: Option<usize>,
    /// 强制少返回若干条向量，用于测试条数校验。
    drop_outputs: usize,
}

#[cfg(test)]
impl MockEmbeddingProvider {
    /// 用给定维度创建可用的 mock。
    pub fn new(dimension: u32) -> Self {
        Self {
            space: EmbeddingSpace {
                provider_type: "mock".into(),
                provider_id: "mock-provider".into(),
                model: "mock-model".into(),
                model_revision: "v1".into(),
                dimension,
                normalization: true,
                query_template_version: 1,
                document_template_version: 1,
            },
            batch_size: DEFAULT_MAX_BATCH_SIZE,
            failure: None,
            forced_dimension: None,
            drop_outputs: 0,
        }
    }

    /// 覆盖嵌入空间。
    pub fn with_space(mut self, space: EmbeddingSpace) -> Self {
        self.space = space;
        self
    }

    /// 设置单批最大条数。
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    /// 让每次调用都失败。
    pub fn failing(mut self, reason: &str) -> Self {
        self.failure = Some(reason.to_string());
        self
    }

    /// 让返回的向量使用错误维度。
    pub fn with_wrong_dimension(mut self, dimension: usize) -> Self {
        self.forced_dimension = Some(dimension);
        self
    }

    /// 让返回的向量少若干条。
    pub fn dropping(mut self, count: usize) -> Self {
        self.drop_outputs = count;
        self
    }

    /// 由输入确定性地生成一条单位向量。
    ///
    /// `kind` 参与摘要，因此同一段文本作为查询与作为文档会得到不同向量——这正是查询模板
    /// 与文档模板分离在向量上的体现。
    fn vector_of(&self, kind: EmbeddingInputKind, input: &str) -> Vec<f32> {
        use sha2::{Digest, Sha256};

        let dimension = self
            .forced_dimension
            .unwrap_or(self.space.dimension as usize);
        let mut digest = Sha256::new();
        digest.update(b"code-context-mock-embedding-v1\0");
        digest.update(kind.as_str().as_bytes());
        digest.update([0]);
        digest.update(input.as_bytes());
        let seed = digest.finalize();

        // 用摘要做种子的线性同余序列：确定、无外部依赖、分布足以区分不同输入。
        let mut state = u64::from_be_bytes(seed[..8].try_into().unwrap_or([0; 8])) | 1;
        let mut values: Vec<f32> = Vec::with_capacity(dimension);
        for _ in 0..dimension {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            values.push(((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0);
        }
        if self.space.normalization {
            let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
            if norm > f32::EPSILON {
                for value in &mut values {
                    *value /= norm;
                }
            }
        }
        values
    }
}

#[cfg(test)]
impl EmbeddingProvider for MockEmbeddingProvider {
    fn space(&self) -> Option<EmbeddingSpace> {
        Some(self.space.clone())
    }

    fn embed(
        &self,
        kind: EmbeddingInputKind,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, CodeContextError> {
        if let Some(reason) = self.failure.as_deref() {
            return Err(CodeContextError::EmbeddingUnavailable(reason.to_string()));
        }
        let mut vectors: Vec<Vec<f32>> = inputs
            .iter()
            .map(|input| self.vector_of(kind, input))
            .collect();
        for _ in 0..self.drop_outputs.min(vectors.len()) {
            vectors.pop();
        }
        Ok(vectors)
    }

    fn max_batch_size(&self) -> usize {
        self.batch_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未配置 provider 不得声明任何嵌入空间，否则会污染语义索引兼容判断。
    #[test]
    fn unconfigured_provider_has_no_space() {
        let provider = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Api);
        assert!(provider.space().is_none());
    }

    /// 未配置 provider 必须失败，绝不能返回伪造向量。
    #[test]
    fn unconfigured_provider_never_returns_vectors() {
        for mode in [EmbeddingMode::Api, EmbeddingMode::Local] {
            let provider = UnconfiguredEmbeddingProvider::new(mode);
            let result = provider.embed(
                EmbeddingInputKind::Query,
                &["where are permissions checked".to_string()],
            );
            assert!(matches!(
                result,
                Err(CodeContextError::EmbeddingUnavailable(_))
            ));
        }
    }

    /// 两种模式必须给出各自缺失的宿主能力，而不是同一句泛化文案。
    #[test]
    fn unavailable_reason_names_the_missing_capability() {
        let api = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Api).reason();
        let local = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Local).reason();
        assert!(api.contains("HTTP"), "{api}");
        assert!(local.contains("推理"), "{local}");
        assert_ne!(api, local);
    }

    /// 未配置 provider 的健康检查必须失败，使降级判断不必等到第一次嵌入。
    #[test]
    fn unconfigured_provider_fails_health_check() {
        let provider = UnconfiguredEmbeddingProvider::new(EmbeddingMode::Api);
        assert!(matches!(
            provider.health_check(),
            Err(CodeContextError::EmbeddingUnavailable(_))
        ));
        assert!(provider.provider_id().is_none());
        assert!(provider.model_id().is_none());
        assert!(provider.dimension().is_none());
    }

    /// provider 选择必须遵循配置模式，且**永远不会选中 mock**。
    #[test]
    fn provider_selection_follows_configuration() {
        for mode in [EmbeddingMode::Api, EmbeddingMode::Local] {
            let config = CodeContextConfig {
                enabled: true,
                embedding_mode: mode,
            };
            let provider = select_provider(&config);
            // 两条分支目前都被宿主能力阻塞，因此都必须是"不可用"而不是伪造实现。
            assert!(provider.space().is_none(), "{mode:?} 不应声明嵌入空间");
            assert!(provider.health_check().is_err());
            assert!(provider
                .embed_query("permission check")
                .is_err_and(|error| matches!(error, CodeContextError::EmbeddingUnavailable(_))));
        }
    }

    // -----------------------------------------------------------------------
    // 通过 MockEmbeddingProvider 验证接口契约
    // -----------------------------------------------------------------------

    /// 派生信息必须全部来自嵌入空间，不重复保存状态。
    #[test]
    fn derived_metadata_comes_from_space() {
        let provider = MockEmbeddingProvider::new(8);
        assert_eq!(provider.provider_id().as_deref(), Some("mock-provider"));
        assert_eq!(provider.model_id().as_deref(), Some("mock-model"));
        assert_eq!(provider.dimension(), Some(8));
        assert!(provider.health_check().is_ok());
    }

    /// 文档嵌入必须与输入一一对应且顺序一致。
    #[test]
    fn document_embedding_preserves_order_and_count() {
        let provider = MockEmbeddingProvider::new(8);
        let inputs: Vec<String> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|item| (*item).to_string())
            .collect();

        let vectors = provider.embed_documents(&inputs).expect("嵌入应成功");

        assert_eq!(vectors.len(), 3);
        assert!(vectors.iter().all(|vector| vector.len() == 8));
        // 顺序一致：逐条单独嵌入的结果必须与批量结果相同。
        for (index, input) in inputs.iter().enumerate() {
            let single = provider
                .embed_documents(std::slice::from_ref(input))
                .expect("单条嵌入应成功");
            assert_eq!(single[0], vectors[index], "第 {index} 条向量与输入不对应");
        }
    }

    /// 相同输入必须得到相同向量，不同输入必须不同。
    #[test]
    fn embedding_is_deterministic() {
        let provider = MockEmbeddingProvider::new(16);
        let first = provider.embed_query("permission").expect("嵌入应成功");
        let second = provider.embed_query("permission").expect("嵌入应成功");
        let other = provider.embed_query("session").expect("嵌入应成功");

        assert_eq!(first, second);
        assert_ne!(first, other);
    }

    /// 查询与文档必须走不同模板，因此同一段文本得到不同向量。
    #[test]
    fn query_and_document_templates_differ() {
        let provider = MockEmbeddingProvider::new(16);
        let text = "permission manager".to_string();

        let query = provider.embed_query(&text).expect("查询嵌入应成功");
        let document = provider
            .embed_documents(std::slice::from_ref(&text))
            .expect("文档嵌入应成功");

        assert_ne!(
            query, document[0],
            "查询与文档使用不同模板，向量不应相同——相同意味着模板未真正分离"
        );
    }

    /// 超过批大小的输入必须被自动分批，且结果完整。
    #[test]
    fn oversized_input_is_split_into_batches() {
        let provider = MockEmbeddingProvider::new(4).with_batch_size(3);
        let inputs: Vec<String> = (0..10).map(|index| format!("chunk-{index}")).collect();

        let vectors = provider.embed_documents(&inputs).expect("分批嵌入应成功");

        assert_eq!(vectors.len(), 10, "分批不得丢失或重复任何一条");
        for (index, input) in inputs.iter().enumerate() {
            let single = provider
                .embed_documents(std::slice::from_ref(input))
                .expect("单条嵌入应成功");
            assert_eq!(single[0], vectors[index]);
        }
    }

    /// 批大小为 0 时必须退化为 1 而不是死循环。
    #[test]
    fn zero_batch_size_is_clamped() {
        let provider = MockEmbeddingProvider::new(4).with_batch_size(0);
        let inputs: Vec<String> = (0..3).map(|index| format!("chunk-{index}")).collect();
        assert_eq!(provider.embed_documents(&inputs).expect("应成功").len(), 3);
    }

    /// 空输入必须直接返回空，不发起任何调用。
    #[test]
    fn empty_input_returns_empty() {
        let provider = MockEmbeddingProvider::new(4).failing("不应被调用");
        assert!(provider
            .embed_documents(&[])
            .expect("空输入不应失败")
            .is_empty());
    }

    /// provider 少返回向量必须被抽象层挡住。
    ///
    /// 这类错误不会让检索失败，只会让向量与 chunk 静默错位，是最难发现的一类 bug。
    #[test]
    fn missing_vectors_are_rejected() {
        let provider = MockEmbeddingProvider::new(4).dropping(1);
        let inputs: Vec<String> = (0..3).map(|index| format!("chunk-{index}")).collect();

        let error = provider
            .embed_documents(&inputs)
            .expect_err("条数不符必须报错");
        assert!(
            matches!(error, CodeContextError::EmbeddingUnavailable(ref reason) if reason.contains("条向量")),
            "{error}"
        );
    }

    /// provider 返回错误维度必须被抽象层挡住。
    #[test]
    fn wrong_dimension_is_rejected() {
        let provider = MockEmbeddingProvider::new(8).with_wrong_dimension(4);
        let inputs = vec!["chunk".to_string()];

        let error = provider
            .embed_documents(&inputs)
            .expect_err("维度不符必须报错");
        assert!(
            matches!(error, CodeContextError::EmbeddingUnavailable(ref reason) if reason.contains("维度")),
            "{error}"
        );
    }

    /// provider 失败必须以可降级的错误类型返回，而不是 panic 或空结果。
    #[test]
    fn provider_failure_degrades_cleanly() {
        let provider = MockEmbeddingProvider::new(8).failing("远端不可达");
        let inputs = vec!["chunk".to_string()];

        assert!(matches!(
            provider.embed_documents(&inputs),
            Err(CodeContextError::EmbeddingUnavailable(_))
        ));
        assert!(matches!(
            provider.embed_query("query"),
            Err(CodeContextError::EmbeddingUnavailable(_))
        ));
    }

    /// 声明归一化的空间必须产出单位向量。
    #[test]
    fn normalized_space_produces_unit_vectors() {
        let provider = MockEmbeddingProvider::new(32);
        let vector = provider.embed_query("permission").expect("嵌入应成功");
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "模长应为 1，实际 {norm}");
    }

    /// provider 可以通过 trait 对象使用，保证检索核心不依赖具体实现。
    #[test]
    fn provider_is_usable_as_trait_object() {
        let provider: Box<dyn EmbeddingProvider> = Box::new(MockEmbeddingProvider::new(8));
        assert_eq!(provider.dimension(), Some(8));
        assert_eq!(provider.embed_query("x").expect("嵌入应成功").len(), 8);
    }

    /// 换模型必须让嵌入空间不兼容，即使维度相同。
    ///
    /// 这条把"空间兼容性"与"provider 切换"连起来：用户换模型后，`select_provider` 给出的
    /// 新空间与旧空间不兼容，manifest 因此判定为需要重建语义索引。维度相同不能让这条失效。
    #[test]
    fn switching_model_invalidates_the_space_even_at_same_dimension() {
        let base = MockEmbeddingProvider::new(1024);
        let switched = MockEmbeddingProvider::new(1024).with_space(EmbeddingSpace {
            model: "another-model".into(),
            ..base.space().expect("mock 必然声明空间")
        });

        let before = base.space().expect("空间");
        let after = switched.space().expect("空间");

        assert_eq!(before.dimension, after.dimension, "前提：维度相同");
        assert!(
            !before.is_compatible_with(&after),
            "换模型后向量不可比较，维度相同不改变这一点"
        );
    }

    /// 向量的每一维都必须落在归一化后的合理范围内。
    ///
    /// 防御 mock 自身出错：如果伪向量全是 0 或全是同一个值，上面那些"不同输入得到不同
    /// 向量"的断言会变得毫无意义。
    #[test]
    fn mock_vectors_are_well_formed() {
        let provider = MockEmbeddingProvider::new(64);
        let vector = provider
            .embed_query("permission manager")
            .expect("嵌入应成功");

        assert_eq!(vector.len(), 64);
        assert!(
            vector.iter().all(|value| value.is_finite()),
            "向量不得含 NaN 或无穷"
        );
        let distinct = vector
            .iter()
            .map(|value| (value * 1e6) as i64)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            distinct.len() > 32,
            "伪向量必须有足够的分布，实际只有 {} 个不同值",
            distinct.len()
        );
    }
}
