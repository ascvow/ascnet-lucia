//! 离线 Model Mock 与请求/响应 Record-Replay Adapter。

use agent_core::{ChatModel, ModelRequest, ModelResponse, ProviderAdapter};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

/// 当前支持的 Model Fixture schema 版本。
pub const MODEL_FIXTURE_SCHEMA_VERSION: u32 = 1;
/// 单个 Model Fixture 允许的最大模型调用数。
const MAX_MODEL_CALLS: u32 = 1_000;

/// 对一次模型请求的确定性匹配条件。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestMatcher {
    /// system prompt 必须包含的全部文本片段。
    #[serde(default)]
    pub system_contains_all: Vec<String>,
    /// 完整消息视图必须包含的全部文本片段。
    #[serde(default)]
    pub messages_contain_all: Vec<String>,
    /// 若存在，模型可见工具名必须与该列表按稳定顺序完全相等。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_tool_names: Option<Vec<String>>,
}

impl ModelRequestMatcher {
    /// 判断请求是否满足全部稳定条件。
    fn matches(&self, request: &ModelRequest) -> bool {
        let system = request.system.as_deref().unwrap_or_default();
        if !self
            .system_contains_all
            .iter()
            .all(|needle| system.contains(needle))
        {
            return false;
        }
        // 匹配视图必须包含 ToolResult JSON；只拼接文本块会让第二轮 Fixture 无法根据
        // 工具观察选择响应。该序列化只在受信内存中比较，不进入错误或评测报告。
        let message_text = serde_json::to_string(&request.messages).unwrap_or_default();
        if !self
            .messages_contain_all
            .iter()
            .all(|needle| message_text.contains(needle))
        {
            return false;
        }
        if let Some(expected) = &self.exact_tool_names {
            let actual = request
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>();
            if &actual != expected {
                return false;
            }
        }
        true
    }
}

/// Model Fixture 在指定调用序号上的一个条件响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFixtureInteraction {
    /// 从零开始的模型调用序号。
    pub call_index: u32,
    /// 对该调用的匹配条件；同一序号可有多个互斥分支。
    #[serde(default)]
    pub request: ModelRequestMatcher,
    /// 条件命中后返回的完整 provider-neutral 响应。
    pub response: ModelResponse,
}

/// 同一 TaskCase 内 Parent 与 Candidate 共享的离线 Model Mock 脚本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFixture {
    /// Fixture schema 版本。
    pub schema_version: u32,
    /// 一次完整任务预期发起的模型调用数。
    pub expected_calls: u32,
    /// 按 `call_index` 和请求条件选择的响应。
    pub interactions: Vec<ModelFixtureInteraction>,
}

impl ModelFixture {
    /// 校验 schema、调用上限和每个调用序号的覆盖。
    ///
    /// # Errors
    ///
    /// schema 未知、调用数为零或过大、交互越界或某个序号没有响应时返回错误。
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != MODEL_FIXTURE_SCHEMA_VERSION {
            return Err(anyhow!(
                "Model Fixture schema 版本 {} 不受支持，当前支持 {}",
                self.schema_version,
                MODEL_FIXTURE_SCHEMA_VERSION
            ));
        }
        if self.expected_calls == 0 || self.expected_calls > MAX_MODEL_CALLS {
            return Err(anyhow!(
                "Model Fixture expected_calls 必须在 1 到 {MAX_MODEL_CALLS} 之间"
            ));
        }
        for interaction in &self.interactions {
            if interaction.call_index >= self.expected_calls {
                return Err(anyhow!(
                    "Model Fixture 交互序号 {} 超出 expected_calls {}",
                    interaction.call_index,
                    self.expected_calls
                ));
            }
        }
        for index in 0..self.expected_calls {
            if !self
                .interactions
                .iter()
                .any(|interaction| interaction.call_index == index)
            {
                return Err(anyhow!("Model Fixture 缺少调用序号 {index} 的响应"));
            }
        }
        Ok(())
    }
}

/// 一次完整、可序列化的模型请求与响应交换。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelExchange {
    /// Core 实际发送的 provider-neutral 请求。
    pub request: ModelRequest,
    /// Adapter 实际返回的 provider-neutral 响应。
    pub response: ModelResponse,
}

/// 按共享 Fixture 条件返回响应的离线模型。
pub struct ModelMock {
    fixture: ModelFixture,
    calls: AtomicUsize,
    exchanges: Mutex<Vec<ModelExchange>>,
}

impl ModelMock {
    /// 校验 Fixture 并创建一个未消费的离线模型。
    ///
    /// # Errors
    ///
    /// Fixture 结构不合法时返回错误。
    pub fn new(fixture: ModelFixture) -> Result<Self> {
        fixture.validate()?;
        Ok(Self {
            fixture,
            calls: AtomicUsize::new(0),
            exchanges: Mutex::new(Vec::new()),
        })
    }

    /// 返回已完成模型交换的快照。
    ///
    /// # Errors
    ///
    /// 内部状态锁中毒时返回错误，调用方不得继续生成可信评测结果。
    pub fn transcript(&self) -> Result<Vec<ModelExchange>> {
        self.exchanges
            .lock()
            .map(|exchanges| exchanges.clone())
            .map_err(|_| anyhow!("Model Mock 交换记录锁中毒"))
    }

    /// 返回已经尝试的模型调用次数。
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 确认实际模型调用数与 Fixture 完全一致。
    ///
    /// # Errors
    ///
    /// 调用过少或过多时返回错误。
    pub fn assert_exhausted(&self) -> Result<()> {
        let actual = self.call_count();
        if actual != self.fixture.expected_calls as usize {
            return Err(anyhow!(
                "Model Fixture 未完整消费：期望 {} 次，实际 {} 次",
                self.fixture.expected_calls,
                actual
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ChatModel for ModelMock {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst) as u32;
        let interaction = self
            .fixture
            .interactions
            .iter()
            .find(|interaction| {
                interaction.call_index == call_index && interaction.request.matches(&request)
            })
            .ok_or_else(|| {
                anyhow!("Model Fixture 在调用序号 {call_index} 没有匹配当前请求的响应")
            })?;
        let response = interaction.response.clone();
        self.exchanges
            .lock()
            .map_err(|_| anyhow!("Model Mock 交换记录锁中毒"))?
            .push(ModelExchange {
                request,
                response: response.clone(),
            });
        Ok(response)
    }
}

#[async_trait]
impl ProviderAdapter for ModelMock {
    fn name(&self) -> &'static str {
        "fixture-model"
    }
}

/// 包装任意模型并记录完整交换的 Adapter。
///
/// Record 模式可能调用真实模型，只能由显式的受信录制流程使用；标准离线 CI 应使用
/// [`ModelMock`] 或 [`ReplayModel`]。
pub struct RecordingModel {
    inner: Arc<dyn ChatModel>,
    exchanges: Mutex<Vec<ModelExchange>>,
}

impl RecordingModel {
    /// 使用指定模型创建空记录器。
    pub fn new(inner: Arc<dyn ChatModel>) -> Self {
        Self {
            inner,
            exchanges: Mutex::new(Vec::new()),
        }
    }

    /// 返回已经成功完成的模型交换快照。
    ///
    /// # Errors
    ///
    /// 内部状态锁中毒时返回错误。
    pub fn transcript(&self) -> Result<Vec<ModelExchange>> {
        self.exchanges
            .lock()
            .map(|exchanges| exchanges.clone())
            .map_err(|_| anyhow!("模型录制记录锁中毒"))
    }
}

#[async_trait]
impl ChatModel for RecordingModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let response = self.inner.complete(request.clone()).await?;
        self.exchanges
            .lock()
            .map_err(|_| anyhow!("模型录制记录锁中毒"))?
            .push(ModelExchange {
                request,
                response: response.clone(),
            });
        Ok(response)
    }
}

#[async_trait]
impl ProviderAdapter for RecordingModel {
    fn name(&self) -> &'static str {
        "recording-model"
    }
}

/// 只读取已录制交换、并对实际请求执行完全相等校验的离线 Adapter。
pub struct ReplayModel {
    exchanges: Vec<ModelExchange>,
    calls: AtomicUsize,
}

impl ReplayModel {
    /// 使用不可变交换记录创建离线回放模型。
    pub fn new(exchanges: Vec<ModelExchange>) -> Self {
        Self {
            exchanges,
            calls: AtomicUsize::new(0),
        }
    }

    /// 返回录制交换的不可变快照。
    pub fn transcript(&self) -> Vec<ModelExchange> {
        self.exchanges.clone()
    }

    /// 返回已经尝试的模型调用次数。
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 确认全部交换恰好被消费一次。
    ///
    /// # Errors
    ///
    /// 实际调用数与录制交换数不一致时返回错误。
    pub fn assert_exhausted(&self) -> Result<()> {
        let actual = self.call_count();
        if actual != self.exchanges.len() {
            return Err(anyhow!(
                "Model Replay 未完整消费：期望 {} 次，实际 {} 次",
                self.exchanges.len(),
                actual
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ChatModel for ReplayModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let exchange = self
            .exchanges
            .get(index)
            .ok_or_else(|| anyhow!("Model Replay 在调用序号 {index} 已耗尽"))?;
        let expected = serde_json::to_value(&exchange.request)?;
        let actual = serde_json::to_value(&request)?;
        if expected != actual {
            return Err(anyhow!("Model Replay 在调用序号 {index} 检测到请求差异"));
        }
        Ok(exchange.response.clone())
    }
}

#[async_trait]
impl ProviderAdapter for ReplayModel {
    fn name(&self) -> &'static str {
        "replay-model"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{MessageRole, ModelMessage};

    /// 构造包含指定 system 和用户文本的最小请求。
    fn request(system: &str, user: &str) -> ModelRequest {
        let mut request = ModelRequest::new(
            "fixture-model",
            vec![ModelMessage::text(MessageRole::User, user)],
        );
        request.system = Some(system.to_string());
        request
    }

    /// 同一个 Mock Fixture 必须能按 Parent/Candidate system prompt 选择确定性分支。
    #[tokio::test]
    async fn model_mock_branches_on_system_prompt() {
        let fixture = ModelFixture {
            schema_version: MODEL_FIXTURE_SCHEMA_VERSION,
            expected_calls: 1,
            interactions: vec![
                ModelFixtureInteraction {
                    call_index: 0,
                    request: ModelRequestMatcher {
                        system_contains_all: vec!["parent".to_string()],
                        ..ModelRequestMatcher::default()
                    },
                    response: ModelResponse::text("父版本"),
                },
                ModelFixtureInteraction {
                    call_index: 0,
                    request: ModelRequestMatcher {
                        system_contains_all: vec!["candidate".to_string()],
                        ..ModelRequestMatcher::default()
                    },
                    response: ModelResponse::text("候选版本"),
                },
            ],
        };
        let model = ModelMock::new(fixture).expect("创建 Model Mock");
        let response = model
            .complete(request("candidate", "执行任务"))
            .await
            .expect("命中 Candidate 分支");

        assert_eq!(response.text_content(), "候选版本");
        model.assert_exhausted().expect("Model Fixture 完整消费");
        assert_eq!(model.transcript().expect("读取交换记录").len(), 1);
    }

    /// Record/Replay 必须返回相同响应，并拒绝任何请求差异。
    #[tokio::test]
    async fn record_replay_detects_request_differences() {
        let fixture = ModelFixture {
            schema_version: MODEL_FIXTURE_SCHEMA_VERSION,
            expected_calls: 1,
            interactions: vec![ModelFixtureInteraction {
                call_index: 0,
                request: ModelRequestMatcher::default(),
                response: ModelResponse::text("固定响应"),
            }],
        };
        let recording =
            RecordingModel::new(Arc::new(ModelMock::new(fixture).expect("创建被录制模型")));
        let original = request("stable", "执行任务");
        recording
            .complete(original.clone())
            .await
            .expect("录制模型交换");
        let transcript = recording.transcript().expect("读取录制交换");

        let replay = ReplayModel::new(transcript.clone());
        let response = replay.complete(original).await.expect("精确请求应可回放");
        assert_eq!(response.text_content(), "固定响应");
        replay.assert_exhausted().expect("回放完整消费");

        let divergent = ReplayModel::new(transcript);
        let error = divergent
            .complete(request("changed", "执行任务"))
            .await
            .expect_err("system prompt 差异必须被检测");
        assert!(error.to_string().contains("请求差异"));
    }
}
