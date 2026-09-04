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
use agent_plugin::{HttpRequest, PluginHostApi};
use std::collections::HashMap;

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
        let outcome = embed_batched(self, EmbeddingInputKind::Document, inputs, usize::MAX);
        match outcome.stopped_by {
            Some(error) => Err(error),
            None => Ok(outcome.vectors),
        }
    }

    /// 分批嵌入文档，允许中途失败并保留已完成的部分。
    ///
    /// 与 [`EmbeddingProvider::embed_documents`] 的区别是**失败语义**：那个入口是全有或
    /// 全无，一个批次被限流就丢弃整轮结果；这个入口保留成功前缀，把中断原因一并交回。
    ///
    /// 这条区分不是可有可无的。嵌入一个中等仓库需要几百次请求，429 限流几乎必然发生；
    /// 全有或全无意味着**永远建不完索引**，而且失败是静默的——检索照常返回结果，只是
    /// 少了语义那一路。`max_batches` 限制单轮工作量，让调用方可以跨多次调用续建。
    fn embed_documents_resumable(&self, inputs: &[String], max_batches: usize) -> BatchOutcome {
        embed_batched(self, EmbeddingInputKind::Document, inputs, max_batches)
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
        let outcome = embed_batched(
            self,
            EmbeddingInputKind::Query,
            std::slice::from_ref(&query.to_string()),
            usize::MAX,
        );
        if let Some(error) = outcome.stopped_by {
            return Err(error);
        }
        let mut vectors = outcome.vectors;
        if vectors.len() != 1 {
            return Err(CodeContextError::EmbeddingUnavailable(format!(
                "查询嵌入应返回 1 条向量，实际 {}",
                vectors.len()
            )));
        }
        Ok(vectors.remove(0))
    }
}

/// 一次分批嵌入的产物。
///
/// `vectors` 始终对应 `inputs` 的**前缀**：第 n 条向量属于第 n 条输入。中断时后续输入
/// 没有向量，调用方据此决定哪些还需要重试。
pub struct BatchOutcome {
    /// 已成功嵌入的向量，与输入前缀一一对应。
    pub vectors: Vec<Vec<f32>>,
    /// 中断原因；全部完成时为 `None`。
    pub stopped_by: Option<CodeContextError>,
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
    max_batches: usize,
) -> BatchOutcome {
    if inputs.is_empty() {
        return BatchOutcome {
            vectors: Vec::new(),
            stopped_by: None,
        };
    }
    let batch_size = provider.max_batch_size().max(1);
    let expected_dimension = provider.dimension();
    let mut vectors = Vec::with_capacity(inputs.len());

    for (index, batch) in inputs.chunks(batch_size).enumerate() {
        if index >= max_batches {
            return BatchOutcome {
                vectors,
                stopped_by: Some(CodeContextError::EmbeddingTransient(format!(
                    "本轮已提交 {max_batches} 批，剩余部分留待下一轮继续"
                ))),
            };
        }
        let produced = match provider.embed(kind, batch) {
            Ok(produced) => produced,
            Err(error) => {
                return BatchOutcome {
                    vectors,
                    stopped_by: Some(error),
                }
            }
        };
        // 条数或维度不符属于协议级错误，不是瞬时故障：保留已完成的前缀，但中断本轮。
        if produced.len() != batch.len() {
            return BatchOutcome {
                vectors,
                stopped_by: Some(CodeContextError::EmbeddingUnavailable(format!(
                    "{} 嵌入返回 {} 条向量，与输入的 {} 条不符",
                    kind.as_str(),
                    produced.len(),
                    batch.len()
                ))),
            };
        }
        if let Some(dimension) = expected_dimension {
            if let Some(actual) = produced
                .iter()
                .map(Vec::len)
                .find(|length| *length != dimension as usize)
            {
                return BatchOutcome {
                    vectors,
                    stopped_by: Some(CodeContextError::EmbeddingUnavailable(format!(
                        "嵌入维度为 {actual}，与嵌入空间声明的 {dimension} 不符"
                    ))),
                };
            }
        }
        vectors.extend(produced);
    }
    BatchOutcome {
        vectors,
        stopped_by: None,
    }
}

/// 按配置选择嵌入 provider。
///
/// Api 分支在宿主注入了 HTTP 端点时返回 [`ApiEmbeddingProvider`]；没有端点说明用户
/// 没配 `http_base_url`，此时返回未配置 provider 并说明缺什么。Local 分支仍等待宿主
/// 受控推理能力，理由见 `adr-local-embedding.md` 与 `PROGRESS.md` 的 Blocked Phases。
///
/// 刻意不提供"选不到就用 Mock"的回退：伪造向量会让语义链路的错误推迟到检索结果里才暴露。
pub fn select_provider<'host>(
    config: &CodeContextConfig,
    host: &'host dyn PluginHostApi,
) -> Box<dyn EmbeddingProvider + 'host> {
    match config.embedding_mode {
        EmbeddingMode::Api => match ApiEmbeddingProvider::from_config(config, host) {
            Some(provider) => Box::new(provider),
            None => Box::new(UnconfiguredEmbeddingProvider::new(config.embedding_mode)),
        },
        EmbeddingMode::Local => Box::new(UnconfiguredEmbeddingProvider::new(config.embedding_mode)),
    }
}

/// 通过宿主 HTTP 能力访问 OpenAI 兼容 `/v1/embeddings` 端点的 provider。
///
/// # 边界
///
/// provider **不知道端点地址，也拿不到密钥**：宿主按 manifest allowlist 与应用配置
/// 注入 base URL 和 `Authorization`，这里只提交相对路径 `embeddings` 与请求体。
/// 这是 `adr-api-embedding.md` 定下的分工，也是 `host-model-complete` 的同一条范式。
pub struct ApiEmbeddingProvider<'host> {
    /// 宿主能力入口。
    host: &'host dyn PluginHostApi,
    /// 该 provider 声明的嵌入空间。
    space: EmbeddingSpace,
    /// 单批最大条数。
    batch_size: usize,
    /// 是否在请求中发送 `dimensions` 参数。
    send_dimensions: bool,
}

impl std::fmt::Debug for ApiEmbeddingProvider<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiEmbeddingProvider")
            .field("space", &self.space)
            .field("batch_size", &self.batch_size)
            .finish_non_exhaustive()
    }
}

impl<'host> ApiEmbeddingProvider<'host> {
    /// 按配置构造 provider；宿主未注入 HTTP 端点时返回 `None`。
    ///
    /// 端点摘要必须存在：没有它就无法判定嵌入空间是否变化，而把不同端点的向量混在
    /// 同一个索引里，检索结果会长期缓慢地不正确。
    pub fn from_config(config: &CodeContextConfig, host: &'host dyn PluginHostApi) -> Option<Self> {
        let endpoint_id = config.http_endpoint_id.as_ref()?;
        Some(Self {
            host,
            space: EmbeddingSpace {
                provider_type: "api".to_string(),
                provider_id: endpoint_id.clone(),
                model: config.embedding_model.clone(),
                // 端点不提供模型修订信息；留空而不是编造一个值。
                model_revision: String::new(),
                dimension: config.embedding_dimension,
                // OpenAI 兼容端点返回已归一化向量；余弦相似度因此可退化为点积。
                normalization: true,
                query_template_version: 1,
                document_template_version: 2,
            },
            batch_size: config.embedding_batch_size.max(1),
            send_dimensions: config.embedding_send_dimensions,
        })
    }

    /// 向端点提交一次嵌入请求并解析返回的向量。
    fn request_embeddings(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, CodeContextError> {
        let mut body = serde_json::json!({
            "model": self.space.model,
            "input": inputs,
        });
        if self.send_dimensions {
            body["dimensions"] = serde_json::json!(self.space.dimension);
        }
        let request = HttpRequest {
            method: "POST".to_string(),
            path: "embeddings".to_string(),
            headers: HashMap::from([("content-type".to_string(), "application/json".to_string())]),
            body: Some(body.to_string()),
        };
        let response = self.host.http_request(&request).map_err(|error| {
            // 宿主 HTTP 失败多为网络抖动或超时，按瞬时处理并在后续轮次重试。
            CodeContextError::EmbeddingTransient(format!("嵌入端点调用失败：{error}"))
        })?;
        if !(200..300).contains(&response.status) {
            // 回传状态码与正文摘要：4xx 通常是配置错误（模型名、维度、密钥），
            // 用户需要看到端点自己的说明才能改对。
            let detail = format!(
                "嵌入端点返回 HTTP {}：{}",
                response.status,
                truncate_for_message(&response.body)
            );
            // 429 是限流、5xx 是服务端故障，两者都会自行恢复；其余状态码是配置问题，
            // 重试只会把同一个错误再犯一遍。
            return Err(if response.status == 429 || response.status >= 500 {
                CodeContextError::EmbeddingTransient(detail)
            } else {
                CodeContextError::EmbeddingUnavailable(detail)
            });
        }
        parse_embedding_response(&response.body)
    }
}

impl EmbeddingProvider for ApiEmbeddingProvider<'_> {
    fn space(&self) -> Option<EmbeddingSpace> {
        Some(self.space.clone())
    }

    fn max_batch_size(&self) -> usize {
        self.batch_size
    }

    /// 提交一批文本并返回向量。
    ///
    /// 查询与文档走同一个端点：OpenAI 兼容的嵌入接口没有 query/document 之分，模板
    /// 版本差异体现在 [`EmbeddingSpace`] 的标识里，供将来换用非对称模型时区分。
    fn embed(
        &self,
        _kind: EmbeddingInputKind,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, CodeContextError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.request_embeddings(inputs)
    }

    /// 用一次最小请求探测端点可用性。
    ///
    /// 覆盖默认实现是必要的：默认实现只检查是否声明了嵌入空间，而 API provider 的
    /// 声明总是成立，真正会失败的是密钥、模型名与网络。把失败提前到这里，降级判断
    /// 才不必等到第一次真正嵌入。
    fn health_check(&self) -> Result<(), CodeContextError> {
        self.request_embeddings(std::slice::from_ref(&"ping".to_string()))
            .map(|_| ())
    }
}

/// 解析 OpenAI 兼容嵌入响应。
///
/// 按 `index` 字段重排：接口不保证返回顺序与输入一致，而向量与 chunk 是按下标对齐的，
/// 顺序错了不会报错，只会让检索结果长期缓慢地不正确。
fn parse_embedding_response(body: &str) -> Result<Vec<Vec<f32>>, CodeContextError> {
    /// 响应中的单条嵌入。
    #[derive(serde::Deserialize)]
    struct EmbeddingItem {
        /// 对应输入的下标；部分端点省略该字段。
        #[serde(default)]
        index: Option<usize>,
        /// 向量本体。
        embedding: Vec<f32>,
    }

    /// 嵌入响应信封。
    #[derive(serde::Deserialize)]
    struct EmbeddingEnvelope {
        /// 与输入等长的嵌入列表。
        data: Vec<EmbeddingItem>,
    }

    let envelope: EmbeddingEnvelope = serde_json::from_str(body).map_err(|error| {
        CodeContextError::EmbeddingUnavailable(format!(
            "解析嵌入响应失败：{error}；响应片段：{}",
            truncate_for_message(body)
        ))
    })?;
    let count = envelope.data.len();
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; count];
    for (position, item) in envelope.data.into_iter().enumerate() {
        let index = item.index.unwrap_or(position);
        let slot = slots.get_mut(index).ok_or_else(|| {
            CodeContextError::EmbeddingUnavailable(format!(
                "嵌入响应的 index {index} 超出返回条数 {count}"
            ))
        })?;
        if slot.is_some() {
            return Err(CodeContextError::EmbeddingUnavailable(format!(
                "嵌入响应的 index {index} 重复"
            )));
        }
        *slot = Some(item.embedding);
    }
    slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.ok_or_else(|| {
                CodeContextError::EmbeddingUnavailable(format!("嵌入响应缺少 index {index}"))
            })
        })
        .collect()
}

/// 截断用于错误信息的文本，避免把整个响应体塞进日志。
fn truncate_for_message(text: &str) -> String {
    const LIMIT: usize = 256;
    let trimmed = text.trim();
    match trimmed.char_indices().nth(LIMIT) {
        Some((offset, _)) => format!("{}…", &trimmed[..offset]),
        None => trimmed.to_string(),
    }
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
                document_template_version: 2,
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
    use crate::testing::{http_json, TestHost};

    /// 构造已注入宿主端点的 API 配置。
    fn api_config() -> CodeContextConfig {
        CodeContextConfig {
            embedding_mode: EmbeddingMode::Api,
            embedding_dimension: 3,
            http_endpoint_id: Some("endpoint-abc".to_string()),
            ..CodeContextConfig::default()
        }
    }

    /// 构造一条 OpenAI 兼容的嵌入响应正文。
    fn embedding_body(vectors: &[(usize, [f32; 3])]) -> String {
        let items: Vec<String> = vectors
            .iter()
            .map(|(index, vector)| {
                format!(
                    r#"{{"index":{index},"embedding":[{},{},{}]}}"#,
                    vector[0], vector[1], vector[2]
                )
            })
            .collect();
        format!(r#"{{"data":[{}]}}"#, items.join(","))
    }

    // -----------------------------------------------------------------------
    // API provider
    // -----------------------------------------------------------------------

    /// provider 必须只提交相对路径，绝不能构造绝对地址或携带凭证。
    #[test]
    fn api_provider_submits_relative_path_without_credentials() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            &embedding_body(&[(0, [1.0, 0.0, 0.0])]),
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        provider
            .embed(EmbeddingInputKind::Document, &["fn check()".to_string()])
            .expect("嵌入应成功");

        let requests = host.http_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "embeddings", "只能提交相对路径");
        // 凭证由宿主注入，插件不得自行设置任何认证 header。
        assert!(!requests[0].headers.contains_key("authorization"));
        let body: serde_json::Value =
            serde_json::from_str(requests[0].body.as_deref().expect("应有正文"))
                .expect("正文应是 JSON");
        assert_eq!(body["model"], "text-embedding-3-small");
        // 默认不发 `dimensions`：固定维度模型（bge-m3 等）会用 HTTP 400 拒绝它，
        // 而那次失败会让整条语义链路静默失效。
        assert!(
            body.get("dimensions").is_none(),
            "默认不得发送 dimensions：{body}"
        );
    }

    /// 显式开启后才发送 `dimensions`，供支持维度截断的模型使用。
    #[test]
    fn dimensions_are_sent_only_when_explicitly_enabled() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            &embedding_body(&[(0, [1.0, 0.0, 0.0])]),
        )]);
        let mut config = api_config();
        config.embedding_send_dimensions = true;
        let provider = ApiEmbeddingProvider::from_config(&config, &host).expect("应构造成功");

        provider
            .embed(EmbeddingInputKind::Document, &["fn check()".to_string()])
            .expect("嵌入应成功");

        let requests = host.http_requests();
        let body: serde_json::Value =
            serde_json::from_str(requests[0].body.as_deref().expect("应有正文"))
                .expect("正文应是 JSON");
        assert_eq!(body["dimensions"], 3);
    }

    /// 端点乱序返回时必须按 index 重排，否则向量会错挂到别的 chunk 上。
    #[test]
    fn out_of_order_response_is_reordered_by_index() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            // 刻意倒序返回。
            &embedding_body(&[(1, [0.0, 1.0, 0.0]), (0, [1.0, 0.0, 0.0])]),
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        let vectors = provider
            .embed(
                EmbeddingInputKind::Document,
                &["first".to_string(), "second".to_string()],
            )
            .expect("嵌入应成功");

        assert_eq!(vectors[0], vec![1.0, 0.0, 0.0], "index 0 必须排在前面");
        assert_eq!(vectors[1], vec![0.0, 1.0, 0.0]);
    }

    /// 缺少某个 index 的响应必须失败，而不是返回条数不足的结果。
    #[test]
    fn response_with_missing_index_is_rejected() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            r#"{"data":[{"index":1,"embedding":[1,0,0]},{"index":1,"embedding":[0,1,0]}]}"#,
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        let error = provider
            .embed(
                EmbeddingInputKind::Document,
                &["a".to_string(), "b".to_string()],
            )
            .expect_err("index 重复应被拒绝");
        // 协议级错误（条数、维度、下标）是永久性的：同一个端点重试只会再犯一次。
        assert!(matches!(error, CodeContextError::EmbeddingUnavailable(_)));
        assert!(!error.is_transient(), "协议错误不应被重试");
    }

    /// 条数与输入不符必须在抽象层被拒绝。
    #[test]
    fn response_with_wrong_count_is_rejected() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            &embedding_body(&[(0, [1.0, 0.0, 0.0])]),
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        let error = provider
            .embed_documents(&["a".to_string(), "b".to_string()])
            .expect_err("条数不符应被拒绝");
        // 协议级错误（条数、维度、下标）是永久性的：同一个端点重试只会再犯一次。
        assert!(matches!(error, CodeContextError::EmbeddingUnavailable(_)));
        assert!(!error.is_transient(), "协议错误不应被重试");
    }

    /// 维度与声明不符必须被拒绝——向量不可比时静默接受最难发现。
    #[test]
    fn response_with_wrong_dimension_is_rejected() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            r#"{"data":[{"index":0,"embedding":[1,0]}]}"#,
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        let error = provider
            .embed_documents(&["a".to_string()])
            .expect_err("维度不符应被拒绝");
        // 协议级错误（条数、维度、下标）是永久性的：同一个端点重试只会再犯一次。
        assert!(matches!(error, CodeContextError::EmbeddingUnavailable(_)));
        assert!(!error.is_transient(), "协议错误不应被重试");
    }

    /// 非 2xx 必须携带状态码与端点说明，便于用户改对配置。
    #[test]
    fn non_success_status_reports_endpoint_message() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            401,
            r#"{"error":{"message":"Incorrect API key provided"}}"#,
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        let error = provider
            .embed_documents(&["a".to_string()])
            .expect_err("401 应失败");
        let message = error.to_string();
        assert!(message.contains("401"), "应带状态码：{message}");
        assert!(
            message.contains("Incorrect API key"),
            "应带端点说明：{message}"
        );
    }

    /// 宿主 HTTP 失败必须降级为**瞬时**不可用，而不是让整次检索崩溃。
    ///
    /// 归为瞬时而不是永久：网络抖动与超时会自行恢复，判成永久会让语义索引就此停摆。
    #[test]
    fn host_failure_degrades_to_unavailable() {
        let host = TestHost::new().with_http_responses(vec![Err(anyhow::anyhow!("网络不可达"))]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        let error = provider
            .embed_documents(&["a".to_string()])
            .expect_err("宿主失败应传播");
        assert!(matches!(error, CodeContextError::EmbeddingTransient(_)));
        assert!(error.is_transient(), "网络失败必须可重试");
    }

    /// 健康检查必须真的打一次请求，而不是只看配置是否齐全。
    #[test]
    fn health_check_probes_the_endpoint() {
        let host = TestHost::new().with_http_responses(vec![http_json(
            200,
            &embedding_body(&[(0, [1.0, 0.0, 0.0])]),
        )]);
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        provider.health_check().expect("端点可用时应通过");
        assert_eq!(host.http_requests().len(), 1, "必须真的发起探测请求");
    }

    /// 端点摘要变化必须让嵌入空间失效，触发语义索引重建。
    #[test]
    fn changing_endpoint_invalidates_the_space() {
        let host = TestHost::new();
        let first = ApiEmbeddingProvider::from_config(&api_config(), &host)
            .expect("应构造成功")
            .space()
            .expect("应有空间");
        let second = ApiEmbeddingProvider::from_config(
            &CodeContextConfig {
                http_endpoint_id: Some("endpoint-xyz".to_string()),
                ..api_config()
            },
            &host,
        )
        .expect("应构造成功")
        .space()
        .expect("应有空间");

        assert_ne!(first.id(), second.id(), "换端点必须改变嵌入空间标识");
    }

    /// 空输入不得发起请求。
    #[test]
    fn empty_input_makes_no_request() {
        let host = TestHost::new();
        let provider = ApiEmbeddingProvider::from_config(&api_config(), &host).expect("应构造成功");

        assert!(provider
            .embed_documents(&[])
            .expect("空输入应成功")
            .is_empty());
        assert!(host.http_requests().is_empty(), "空输入不应发起请求");
    }

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

    /// 没有宿主 HTTP 端点时，两种模式都必须是"不可用"而不是伪造实现。
    #[test]
    fn provider_selection_without_endpoint_is_unavailable() {
        let host = crate::testing::TestHost::new();
        for mode in [EmbeddingMode::Api, EmbeddingMode::Local] {
            let config = CodeContextConfig {
                embedding_mode: mode,
                http_endpoint_id: None,
                ..CodeContextConfig::default()
            };
            let provider = select_provider(&config, &host);
            assert!(provider.space().is_none(), "{mode:?} 不应声明嵌入空间");
            assert!(provider.health_check().is_err());
            assert!(provider
                .embed_query("permission check")
                .is_err_and(|error| matches!(error, CodeContextError::EmbeddingUnavailable(_))));
        }
    }

    /// 宿主注入端点后，Api 模式必须选中真实 provider；Local 模式仍不可用。
    #[test]
    fn api_mode_selects_real_provider_when_endpoint_is_injected() {
        let host = crate::testing::TestHost::new();
        let config = api_config();

        let provider = select_provider(&config, &host);
        let space = provider.space().expect("Api 模式应声明嵌入空间");
        assert_eq!(space.provider_type, "api");
        assert_eq!(space.model, "text-embedding-3-small");

        let local = select_provider(
            &CodeContextConfig {
                embedding_mode: EmbeddingMode::Local,
                ..config
            },
            &host,
        );
        assert!(local.space().is_none(), "Local 分支仍被宿主能力阻塞");
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
