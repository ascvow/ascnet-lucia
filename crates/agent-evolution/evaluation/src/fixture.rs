//! 确定性的初始环境与工具 Fixture Runtime。

use crate::dataset::{validate_relative_path, DatasetError};
use agent_tool::{
    validate_tool_name, Tool, ToolCall, ToolErrorKind, ToolRegistry, ToolResult, ToolSpec,
};
use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// 当前支持的初始环境 Fixture schema 版本。
pub const ENVIRONMENT_FIXTURE_SCHEMA_VERSION: u32 = 1;
/// 当前支持的工具 Fixture schema 版本。
pub const TOOL_FIXTURE_SCHEMA_VERSION: u32 = 1;
/// 单个环境 Fixture 允许的最大文件数。
const MAX_ENVIRONMENT_FILES: usize = 1_000;
/// 单个工具 Fixture 允许的最大交互数。
const MAX_TOOL_INTERACTIONS: usize = 10_000;

/// 初始环境中的一个 UTF-8 文本文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentFile {
    /// 相对于本次 Repeat Workspace 的安全路径。
    pub path: String,
    /// 文件完整文本内容；Secret 不得进入 Fixture。
    pub content: String,
}

/// 每次 Repeat 前重新物化的确定性文件环境。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentFixture {
    /// Fixture schema 版本。
    pub schema_version: u32,
    /// 需要创建的文件，路径必须唯一。
    #[serde(default)]
    pub files: Vec<EnvironmentFile>,
}

impl EnvironmentFixture {
    /// 校验并把环境物化到一个新建的空目录中。
    ///
    /// # Errors
    ///
    /// schema 未知、路径不安全、路径重复、目标不为空或文件系统操作失败时返回错误。
    /// 已创建的前序文件不会自动回滚；调用方应使用一次性临时目录。
    pub fn materialize(&self, root: impl AsRef<Path>) -> Result<(), FixtureError> {
        self.validate()?;
        let root = root.as_ref();
        let metadata = fs::symlink_metadata(root).map_err(|source| FixtureError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(FixtureError::InvalidEnvironment(
                "Fixture Workspace 必须是非符号链接目录".to_string(),
            ));
        }
        if fs::read_dir(root)
            .map_err(|source| FixtureError::Io {
                path: root.to_path_buf(),
                source,
            })?
            .next()
            .is_some()
        {
            return Err(FixtureError::InvalidEnvironment(
                "Fixture Workspace 必须为空".to_string(),
            ));
        }

        for file in &self.files {
            let path = root.join(&file.path);
            let parent = path.parent().ok_or_else(|| {
                FixtureError::InvalidEnvironment("Fixture 文件缺少父目录".to_string())
            })?;
            fs::create_dir_all(parent).map_err(|source| FixtureError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            fs::write(&path, file.content.as_bytes())
                .map_err(|source| FixtureError::Io { path, source })?;
        }
        Ok(())
    }

    /// 校验 schema、文件数量和安全路径。
    fn validate(&self) -> Result<(), FixtureError> {
        if self.schema_version != ENVIRONMENT_FIXTURE_SCHEMA_VERSION {
            return Err(FixtureError::InvalidEnvironment(format!(
                "不支持 schema 版本 {}，当前支持 {}",
                self.schema_version, ENVIRONMENT_FIXTURE_SCHEMA_VERSION
            )));
        }
        if self.files.len() > MAX_ENVIRONMENT_FILES {
            return Err(FixtureError::InvalidEnvironment(format!(
                "文件数量不能超过 {MAX_ENVIRONMENT_FILES}"
            )));
        }
        let mut paths = BTreeSet::new();
        for file in &self.files {
            validate_relative_path(&file.path).map_err(FixtureError::Dataset)?;
            if !paths.insert(file.path.as_str()) {
                return Err(FixtureError::InvalidEnvironment(format!(
                    "文件路径重复：{}",
                    file.path
                )));
            }
        }
        Ok(())
    }
}

/// 工具结果中不由模型控制的模板字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultTemplate {
    /// 返回给模型的 JSON 内容。
    pub content: Value,
    /// 是否为工具级错误。
    #[serde(default)]
    pub is_error: bool,
    /// 由受信 Fixture Runtime 注入的稳定错误类别。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    /// 只供可信评测和 UI 使用的结构化细节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolResultTemplate {
    /// 构造一个成功结果模板。
    pub fn success(content: Value) -> Self {
        Self {
            content,
            is_error: false,
            error_kind: None,
            details: None,
        }
    }

    /// 绑定实际 ToolCall ID 和工具名，生成 Core 可消费的结果。
    fn bind(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: self.content.clone(),
            is_error: self.is_error,
            error_kind: self.error_kind,
            details: self.details.clone(),
        }
    }
}

/// 工具 Fixture 中一次有序的期望调用与固定响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolFixtureInteraction {
    /// 必须调用的工具名。
    pub tool: String,
    /// 必须完全相等的 JSON 参数。
    pub arguments: Value,
    /// 固定返回模板。
    pub result: ToolResultTemplate,
}

/// 一个离线工具集合及其全局有序交互脚本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolFixture {
    /// Fixture schema 版本。
    pub schema_version: u32,
    /// 暴露给模型的工具定义。
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// 所有工具共享的调用顺序。
    #[serde(default)]
    pub interactions: Vec<ToolFixtureInteraction>,
}

impl ToolFixture {
    /// 校验工具名、定义唯一性、交互上限和引用完整性。
    ///
    /// # Errors
    ///
    /// schema 未知、工具定义重复、出现进程工具、交互引用未知工具或数量超限时返回错误。
    pub fn validate(&self) -> Result<(), FixtureError> {
        if self.schema_version != TOOL_FIXTURE_SCHEMA_VERSION {
            return Err(FixtureError::InvalidToolFixture(format!(
                "不支持 schema 版本 {}，当前支持 {}",
                self.schema_version, TOOL_FIXTURE_SCHEMA_VERSION
            )));
        }
        if self.interactions.len() > MAX_TOOL_INTERACTIONS {
            return Err(FixtureError::InvalidToolFixture(format!(
                "工具交互数不能超过 {MAX_TOOL_INTERACTIONS}"
            )));
        }
        let mut names = BTreeSet::new();
        for spec in &self.tools {
            validate_tool_name(&spec.name)
                .map_err(|error| FixtureError::InvalidToolFixture(error.to_string()))?;
            if matches!(spec.name.as_str(), "shell" | "process_exec") {
                return Err(FixtureError::InvalidToolFixture(format!(
                    "Evaluation Fixture 禁止进程工具 {}",
                    spec.name
                )));
            }
            if !names.insert(spec.name.as_str()) {
                return Err(FixtureError::InvalidToolFixture(format!(
                    "工具定义重复：{}",
                    spec.name
                )));
            }
        }
        for interaction in &self.interactions {
            if !names.contains(interaction.tool.as_str()) {
                return Err(FixtureError::InvalidToolFixture(format!(
                    "交互引用未知工具：{}",
                    interaction.tool
                )));
            }
            if !interaction.result.is_error && interaction.result.error_kind.is_some() {
                return Err(FixtureError::InvalidToolFixture(format!(
                    "成功工具结果不能携带错误类别：{}",
                    interaction.tool
                )));
            }
        }
        Ok(())
    }
}

/// 一次实际 Fixture 工具调用记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureCallRecord {
    /// 模型发出的原始调用。
    pub call: ToolCall,
    /// Runtime 返回的绑定后结果。
    pub result: ToolResult,
    /// 工具名与参数是否匹配当前期望交互。
    pub matched: bool,
}

/// 可为每次 Repeat 构造独立注册表的工具 Fixture Runtime。
#[derive(Clone)]
pub struct ToolFixtureRuntime {
    fixture: Arc<ToolFixture>,
    state: Arc<Mutex<ToolFixtureState>>,
}

#[derive(Debug, Default)]
struct ToolFixtureState {
    next_interaction: usize,
    records: Vec<FixtureCallRecord>,
}

impl ToolFixtureRuntime {
    /// 校验脚本并创建一个未消费的 Runtime。
    ///
    /// # Errors
    ///
    /// Fixture 结构不合法时返回 [`FixtureError`]。
    pub fn new(fixture: ToolFixture) -> Result<Self, FixtureError> {
        fixture.validate()?;
        Ok(Self {
            fixture: Arc::new(fixture),
            state: Arc::new(Mutex::new(ToolFixtureState::default())),
        })
    }

    /// 构造只包含 Fixture 工具的原生注册表。
    ///
    /// 注册表不继承生产工具、网络、Shell 或插件贡献，所有工具共享同一有序脚本状态。
    ///
    /// # Errors
    ///
    /// 工具名校验或注册发生异常时返回错误。
    pub fn registry(&self) -> Result<ToolRegistry, FixtureError> {
        let mut registry = ToolRegistry::new();
        for spec in &self.fixture.tools {
            registry
                .register(FixtureTool {
                    spec: spec.clone(),
                    fixture: self.fixture.clone(),
                    state: self.state.clone(),
                })
                .map_err(|error| FixtureError::InvalidToolFixture(error.to_string()))?;
        }
        Ok(registry)
    }

    /// 返回按实际调用顺序排列的不可变记录快照。
    pub fn records(&self) -> Result<Vec<FixtureCallRecord>, FixtureError> {
        self.state
            .lock()
            .map(|state| state.records.clone())
            .map_err(|_| FixtureError::Poisoned)
    }

    /// 确认脚本被完整、逐项匹配地消费。
    ///
    /// # Errors
    ///
    /// 存在漏调、多调、错序、错名或参数差异时返回 [`FixtureError::ReplayDiverged`]。
    pub fn assert_exhausted(&self) -> Result<(), FixtureError> {
        let state = self.state.lock().map_err(|_| FixtureError::Poisoned)?;
        if state.next_interaction != self.fixture.interactions.len()
            || state.records.iter().any(|record| !record.matched)
        {
            return Err(FixtureError::ReplayDiverged {
                expected: self.fixture.interactions.len(),
                consumed: state.next_interaction,
                reason: "工具调用序列与 Fixture 不一致".to_string(),
            });
        }
        Ok(())
    }

    /// 返回按工具名分组的实际调用次数，供预算和 Verifier 使用。
    pub fn call_counts(&self) -> Result<BTreeMap<String, u64>, FixtureError> {
        let mut counts = BTreeMap::new();
        for record in self.records()? {
            *counts.entry(record.call.name).or_insert(0) += 1;
        }
        Ok(counts)
    }
}

struct FixtureTool {
    spec: ToolSpec,
    fixture: Arc<ToolFixture>,
    state: Arc<Mutex<ToolFixtureState>>,
}

#[async_trait]
impl Tool for FixtureTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, call: ToolCall) -> AnyResult<ToolResult> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("工具 Fixture 状态锁中毒"))?;
        let expected = self.fixture.interactions.get(state.next_interaction);
        let matched = expected
            .is_some_and(|expected| expected.tool == call.name && expected.arguments == call.args);
        let result = if matched {
            let expected = expected.expect("matched 已证明存在期望交互");
            state.next_interaction += 1;
            expected.result.bind(&call)
        } else {
            ToolResult::error_with_kind(
                call.id.clone(),
                call.name.clone(),
                ToolErrorKind::Execution,
                "工具调用与离线 Fixture 不一致",
            )
        };
        state.records.push(FixtureCallRecord {
            call,
            result: result.clone(),
            matched,
        });
        Ok(result)
    }
}

/// Fixture schema、I/O 与回放错误。
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// 初始环境结构不合法。
    #[error("初始环境 Fixture 不合法：{0}")]
    InvalidEnvironment(String),
    /// 工具 Fixture 结构不合法。
    #[error("工具 Fixture 不合法：{0}")]
    InvalidToolFixture(String),
    /// 文件系统操作失败。
    #[error("Fixture 文件操作失败 `{path}`：{source}")]
    Io {
        /// 失败路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// Dataset 路径约束失败。
    #[error(transparent)]
    Dataset(#[from] DatasetError),
    /// 实际调用序列与脚本不一致。
    #[error("Fixture Replay 分歧：期望 {expected} 次，匹配 {consumed} 次，{reason}")]
    ReplayDiverged {
        /// 期望交互数。
        expected: usize,
        /// 成功匹配并消费的交互数。
        consumed: usize,
        /// 稳定分歧原因。
        reason: String,
    },
    /// 共享状态锁中毒，无法可信继续。
    #[error("Fixture Runtime 状态锁中毒")]
    Poisoned,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// 构造只有一个查询调用的工具 Fixture。
    fn lookup_fixture() -> ToolFixture {
        ToolFixture {
            schema_version: TOOL_FIXTURE_SCHEMA_VERSION,
            tools: vec![ToolSpec::new(
                "lookup",
                "查询固定数据",
                ToolSpec::empty_object_schema(),
            )],
            interactions: vec![ToolFixtureInteraction {
                tool: "lookup".to_string(),
                arguments: json!({"key": "city"}),
                result: ToolResultTemplate::success(json!({"value": "杭州"})),
            }],
        }
    }

    /// Runtime 必须按全局顺序匹配调用，并把实际 call_id 绑定到固定结果。
    #[tokio::test]
    async fn tool_fixture_binds_call_identity_and_exhausts() {
        let runtime = ToolFixtureRuntime::new(lookup_fixture()).expect("创建工具 Fixture");
        let registry = runtime.registry().expect("构造工具注册表");
        let result = registry
            .call(ToolCall::new("call-1", "lookup", json!({"key": "city"})))
            .await
            .expect("执行 Fixture 工具");

        assert_eq!(result.call_id, "call-1");
        assert_eq!(result.name, "lookup");
        assert_eq!(result.content, json!({"value": "杭州"}));
        runtime.assert_exhausted().expect("脚本完整消费");
    }

    /// 参数差异必须返回工具错误，并保留未消费状态供 Replay 判定失败。
    #[tokio::test]
    async fn tool_fixture_reports_argument_divergence() {
        let runtime = ToolFixtureRuntime::new(lookup_fixture()).expect("创建工具 Fixture");
        let registry = runtime.registry().expect("构造工具注册表");
        let result = registry
            .call(ToolCall::new("call-1", "lookup", json!({"key": "country"})))
            .await
            .expect("Fixture 差异应返回结构化工具错误");

        assert!(result.is_error);
        assert!(matches!(
            runtime.assert_exhausted(),
            Err(FixtureError::ReplayDiverged { .. })
        ));
    }

    /// 初始环境必须拒绝父目录路径，并能在空 Workspace 中完整物化。
    #[test]
    fn environment_materialization_enforces_relative_paths() {
        let temp = TempDir::new().expect("创建临时 Workspace");
        let valid = EnvironmentFixture {
            schema_version: ENVIRONMENT_FIXTURE_SCHEMA_VERSION,
            files: vec![EnvironmentFile {
                path: "config/input.txt".to_string(),
                content: "固定输入".to_string(),
            }],
        };
        valid.materialize(temp.path()).expect("物化合法环境");
        assert_eq!(
            fs::read_to_string(temp.path().join("config/input.txt")).expect("读取物化文件"),
            "固定输入"
        );

        let invalid = EnvironmentFixture {
            schema_version: ENVIRONMENT_FIXTURE_SCHEMA_VERSION,
            files: vec![EnvironmentFile {
                path: "../escape.txt".to_string(),
                content: "越界".to_string(),
            }],
        };
        let other = TempDir::new().expect("创建第二个临时 Workspace");
        assert!(invalid.materialize(other.path()).is_err());
    }
}
