//! TaskCase、Dataset Manifest 与受信制品加载。
//!
//! Dataset 路径只在受信评测进程内解析。对 Mutator 暴露的视图由
//! [`TrustedDataset::mutator_view`] 单向派生，不包含 EvaluatorOnly Case、文件路径、
//! Hidden 输入、Fixture、模型脚本或 Verifier 引用。

use agent_evolution_protocol::{
    ArtifactDigest, DataClass, DatasetKind, DatasetVersionId, TaskCaseId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

/// 当前支持的 Dataset Manifest schema 版本。
pub const DATASET_MANIFEST_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 TaskCase schema 版本。
pub const TASK_CASE_SCHEMA_VERSION: u32 = 1;
/// Dataset 根目录中的固定 Manifest 文件名。
const MANIFEST_FILE_NAME: &str = "manifest.json";
/// 单个 Dataset 允许的最大 Case 数，防止意外载入无界清单。
const MAX_CASES: usize = 10_000;
/// 单个 TaskCase 允许的最大 Repeat 数。
const MAX_REPEATS: u32 = 32;

/// Dataset 对 Mutator 和 Candidate 的可见级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetVisibility {
    /// 任务正文可以公开展示。
    Public,
    /// 任务正文只允许 Mutator 读取，用于修复已知问题。
    MutatorVisible,
    /// 只允许受信 Evaluator 读取，禁止进入 Mutator 输入和评测报告。
    EvaluatorOnly,
}

/// Dataset 内一个受摘要保护的文件引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedArtifactRef {
    /// 相对于 Dataset 根目录的 UTF-8 路径；只允许普通相对路径组件。
    pub path: String,
    /// 文件原始字节的 SHA-256 摘要。
    pub digest: ArtifactDigest,
}

/// Dataset Manifest 中一个 TaskCase 的非正文索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetCaseRef {
    /// TaskCase 稳定标识。
    pub id: TaskCaseId,
    /// TaskCase 语义版本；相同 ID 的行为变化必须递增。
    pub version: u32,
    /// TaskCase 所属任务族。
    pub family: String,
    /// Repair、Regression、Hidden 或 Safety 用途。
    pub kind: DatasetKind,
    /// Mutator 可见边界。
    pub visibility: DatasetVisibility,
    /// 是否属于不可丢失的关键回归或安全 Case。
    #[serde(default)]
    pub critical: bool,
    /// 是否由确定性的模型、Fixture 与 Verifier 组成。
    #[serde(default)]
    pub deterministic: bool,
    /// TaskCase 正文文件引用。
    pub artifact: TrustedArtifactRef,
}

/// 一个版本化 Dataset 的可信清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetManifest {
    /// Manifest schema 版本；未知版本必须拒绝加载。
    pub schema_version: u32,
    /// Dataset 内容版本标识。
    pub dataset_version: DatasetVersionId,
    /// TaskCase 索引，加载时按该顺序执行。
    pub cases: Vec<DatasetCaseRef>,
}

/// TaskCase 输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskInput {
    /// 单条文本用户指令。
    Text {
        /// 传给 Agent 的完整用户输入。
        text: String,
    },
}

impl TaskInput {
    /// 返回供 Agent 执行的文本，不暴露 Dataset 路径或 Verifier 内容。
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text { text } => text,
        }
    }
}

/// 一个 TaskCase 的可信资源上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudgets {
    /// 最大 ReAct 步数，必须大于零。
    pub max_steps: usize,
    /// 单次模型响应最大 Token 数，必须大于零。
    pub max_tokens: u32,
    /// 单次 Repeat 的墙钟超时，单位毫秒，必须大于零。
    pub wall_clock_ms: u64,
    /// 最大工具调用次数；纯文本任务可以为零。
    pub max_tool_calls: u64,
}

/// 受信 Evaluator 执行的完整 TaskCase。
///
/// 该类型可以在 Dataset 文件中序列化，但不得写入普通 Evidence、EvaluationReport 或
/// Mutator 输入。外部制品均使用摘要引用，加载时会再次校验根目录、符号链接和内容摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCase {
    /// TaskCase schema 版本。
    pub schema_version: u32,
    /// TaskCase 稳定标识。
    pub id: TaskCaseId,
    /// TaskCase 语义版本。
    pub version: u32,
    /// Task Family 稳定名称。
    pub family: String,
    /// Repair、Regression、Hidden 或 Safety 用途。
    pub kind: DatasetKind,
    /// 真实任务输入。
    pub input: TaskInput,
    /// 可选初始文件环境 Fixture。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_environment: Option<TrustedArtifactRef>,
    /// 可选工具 Fixture。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_fixture: Option<TrustedArtifactRef>,
    /// 离线 Model Mock 脚本；标准评测不得调用真实模型。
    pub model_mock: TrustedArtifactRef,
    /// 最终 Verifier 规则；只由受信 Evaluator 读取。
    pub verifier: TrustedArtifactRef,
    /// 单次 Repeat 的资源上限。
    pub budgets: TaskBudgets,
    /// 独立 Repeat 次数。
    pub repeats: u32,
    /// Mutator 可见边界。
    pub visibility: DatasetVisibility,
    /// 输入与 Fixture 的最高数据等级；Secret 不允许直接存入 Dataset。
    pub data_class: DataClass,
    /// 用于筛选的稳定标签；不得包含正文、答案或路径。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 是否属于不可丢失的关键回归或安全 Case。
    #[serde(default)]
    pub critical: bool,
    /// 是否由确定性的模型、Fixture 与 Verifier 组成。
    #[serde(default)]
    pub deterministic: bool,
    /// 单次 Repeat 通过阈值；`None` 表示由可信评测策略决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_threshold: Option<f64>,
}

/// 可以安全提供给 Mutator 的 TaskCase 摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleTaskCase {
    /// TaskCase 稳定标识。
    pub id: TaskCaseId,
    /// TaskCase 语义版本。
    pub version: u32,
    /// Task Family 稳定名称。
    pub family: String,
    /// Dataset 用途。
    pub kind: DatasetKind,
    /// 可见级别；这里永远不是 EvaluatorOnly。
    pub visibility: DatasetVisibility,
    /// Public 或 MutatorVisible 的任务输入；不包含环境、答案或 Verifier。
    pub input: TaskInput,
    /// 稳定标签，不含正文或答案。
    pub tags: Vec<String>,
}

/// Mutator 可读取的 Dataset 视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutatorDatasetView {
    /// Dataset 内容版本标识。
    pub dataset_version: DatasetVersionId,
    /// Public 和 MutatorVisible Case；不含任何受信文件引用。
    pub cases: Vec<VisibleTaskCase>,
}

/// 已完成路径、摘要和结构校验的受信 Dataset。
#[derive(Debug, Clone)]
pub struct TrustedDataset {
    manifest: DatasetManifest,
    cases: Vec<TaskCase>,
    root: PathBuf,
}

impl TrustedDataset {
    /// 返回可信 Manifest；普通 Mutator 调用方不应持有本类型。
    pub fn manifest(&self) -> &DatasetManifest {
        &self.manifest
    }

    /// 返回全部可信 TaskCase，包括 EvaluatorOnly 内容。
    pub fn cases(&self) -> &[TaskCase] {
        &self.cases
    }

    /// 返回 Dataset 的规范化根目录，仅供受信 Fixture 和 Verifier 加载器使用。
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// 派生不含 EvaluatorOnly 内容和任何文件引用的 Mutator 视图。
    pub fn mutator_view(&self) -> MutatorDatasetView {
        let cases = self
            .cases
            .iter()
            .filter(|case| case.visibility != DatasetVisibility::EvaluatorOnly)
            .map(|case| VisibleTaskCase {
                id: case.id.clone(),
                version: case.version,
                family: case.family.clone(),
                kind: case.kind,
                visibility: case.visibility,
                input: case.input.clone(),
                tags: case.tags.clone(),
            })
            .collect();
        MutatorDatasetView {
            dataset_version: self.manifest.dataset_version.clone(),
            cases,
        }
    }

    /// 从已校验 Dataset 中读取并反序列化一个受信引用。
    ///
    /// # Errors
    ///
    /// 路径越界、符号链接、文件读取、摘要或 JSON 解析失败时返回 [`DatasetError`]。
    pub(crate) fn load_artifact<T: DeserializeOwned>(
        &self,
        artifact: &TrustedArtifactRef,
    ) -> Result<T, DatasetError> {
        load_artifact_from_root(&self.root, artifact)
    }
}

/// 只在受信 Evaluator 中构造的 Dataset Store。
#[derive(Debug, Clone)]
pub struct TrustedDatasetStore {
    root: PathBuf,
    expected_manifest_digest: Option<ArtifactDigest>,
}

impl TrustedDatasetStore {
    /// 打开并规范化一个 Dataset 根目录。
    ///
    /// # Errors
    ///
    /// 根目录不存在、不是目录或自身为符号链接时返回 [`DatasetError`]。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DatasetError> {
        let root = root.as_ref();
        let metadata = fs::symlink_metadata(root).map_err(|source| DatasetError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(DatasetError::Symlink(root.to_path_buf()));
        }
        if !metadata.is_dir() {
            return Err(DatasetError::InvalidManifest(
                "Dataset 根路径不是目录".to_string(),
            ));
        }
        let root = fs::canonicalize(root).map_err(|source| DatasetError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Self {
            root,
            expected_manifest_digest: None,
        })
    }

    /// 打开并绑定受信配置声明的 Manifest SHA-256。
    ///
    /// 与 [`Self::open`] 相同地规范化根路径，同时要求后续加载的 `manifest.json` 原始字节
    /// 必须匹配 `expected_manifest_digest`。生产 `lucia-eval` 必须使用该入口，防止整体替换
    /// Dataset 并重算内部引用。
    ///
    /// # Errors
    ///
    /// 根目录不安全时返回 [`DatasetError`]。
    pub fn open_pinned(
        root: impl AsRef<Path>,
        expected_manifest_digest: ArtifactDigest,
    ) -> Result<Self, DatasetError> {
        let mut store = Self::open(root)?;
        store.expected_manifest_digest = Some(expected_manifest_digest);
        Ok(store)
    }

    /// 加载并交叉校验 Manifest 与全部 TaskCase。
    ///
    /// # Errors
    ///
    /// Manifest/TaskCase schema、ID、元数据、路径、摘要或 JSON 任一不一致时返回错误，
    /// 不会返回部分 Dataset。
    pub fn load(&self) -> Result<TrustedDataset, DatasetError> {
        let manifest_path = resolve_trusted_path(&self.root, MANIFEST_FILE_NAME)?;
        if let Some(expected) = &self.expected_manifest_digest {
            let bytes = fs::read(&manifest_path).map_err(|source| DatasetError::Io {
                path: manifest_path.clone(),
                source,
            })?;
            let actual = digest_bytes(&bytes);
            if &actual != expected {
                return Err(DatasetError::DigestMismatch {
                    path: manifest_path,
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        let manifest_path = resolve_trusted_path(&self.root, MANIFEST_FILE_NAME)?;
        let manifest = parse_json::<DatasetManifest>(&manifest_path)?;
        validate_manifest(&manifest)?;

        let mut cases = Vec::with_capacity(manifest.cases.len());
        for indexed in &manifest.cases {
            let task_case = load_artifact_from_root::<TaskCase>(&self.root, &indexed.artifact)?;
            validate_task_case(&task_case)?;
            validate_index_binding(indexed, &task_case)?;
            validate_case_artifact_refs(&self.root, &task_case)?;
            cases.push(task_case);
        }
        Ok(TrustedDataset {
            manifest,
            cases,
            root: self.root.clone(),
        })
    }
}

/// Dataset 加载与结构校验错误。
#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    /// 文件系统操作失败。
    #[error("Dataset 文件操作失败 `{path}`：{source}")]
    Io {
        /// 失败路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 引用包含绝对路径、上级目录或非普通组件。
    #[error("Dataset 引用不是安全相对路径：{0}")]
    UnsafePath(String),
    /// Dataset 路径链中出现符号链接。
    #[error("Dataset 禁止符号链接：{0}")]
    Symlink(PathBuf),
    /// Dataset 文件内容摘要与引用不一致。
    #[error("Dataset 制品摘要不匹配 `{path}`：期望 {expected}，实际 {actual}")]
    DigestMismatch {
        /// 摘要不匹配的文件。
        path: PathBuf,
        /// Manifest 或 TaskCase 声明的摘要。
        expected: ArtifactDigest,
        /// 实际文件摘要。
        actual: ArtifactDigest,
    },
    /// JSON 不是当前 schema 的合法结构。
    #[error("Dataset JSON 无法解析 `{path}`：{source}")]
    Json {
        /// 解析失败的文件。
        path: PathBuf,
        /// 底层 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// Manifest 结构不变量被破坏。
    #[error("Dataset Manifest 不合法：{0}")]
    InvalidManifest(String),
    /// TaskCase 结构或索引绑定不合法。
    #[error("TaskCase 不合法：{0}")]
    InvalidTaskCase(String),
}

/// 校验 Manifest schema、数量、ID 唯一性和 Hidden 可见性。
fn validate_manifest(manifest: &DatasetManifest) -> Result<(), DatasetError> {
    if manifest.schema_version != DATASET_MANIFEST_SCHEMA_VERSION {
        return Err(DatasetError::InvalidManifest(format!(
            "不支持 schema 版本 {}，当前支持 {}",
            manifest.schema_version, DATASET_MANIFEST_SCHEMA_VERSION
        )));
    }
    if manifest.cases.is_empty() || manifest.cases.len() > MAX_CASES {
        return Err(DatasetError::InvalidManifest(format!(
            "Case 数量必须在 1 到 {MAX_CASES} 之间"
        )));
    }
    let mut ids = BTreeSet::new();
    for case in &manifest.cases {
        if !ids.insert(case.id.clone()) {
            return Err(DatasetError::InvalidManifest(format!(
                "TaskCase ID 重复：{}",
                case.id
            )));
        }
        validate_stable_label(&case.family, "Task Family")?;
        validate_reference(&case.artifact)?;
        if case.version == 0 {
            return Err(DatasetError::InvalidManifest(format!(
                "TaskCase {} 的版本必须大于零",
                case.id
            )));
        }
        if case.kind == DatasetKind::Hidden && case.visibility != DatasetVisibility::EvaluatorOnly {
            return Err(DatasetError::InvalidManifest(format!(
                "Hidden TaskCase {} 必须为 evaluator_only",
                case.id
            )));
        }
    }
    Ok(())
}

/// 校验一个完整 TaskCase 的本地结构约束。
fn validate_task_case(task_case: &TaskCase) -> Result<(), DatasetError> {
    if task_case.schema_version != TASK_CASE_SCHEMA_VERSION {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{} 使用不支持的 schema 版本 {}",
            task_case.id, task_case.schema_version
        )));
    }
    if task_case.version == 0 {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{} 的版本必须大于零",
            task_case.id
        )));
    }
    validate_stable_label(&task_case.family, "Task Family")?;
    if task_case.input.as_text().trim().is_empty() {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{} 的输入不能为空",
            task_case.id
        )));
    }
    if task_case.repeats == 0 || task_case.repeats > MAX_REPEATS {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{} 的 repeats 必须在 1 到 {MAX_REPEATS} 之间",
            task_case.id
        )));
    }
    if task_case.budgets.max_steps == 0
        || task_case.budgets.max_tokens == 0
        || task_case.budgets.wall_clock_ms == 0
    {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{} 的步骤、Token 和墙钟预算必须大于零",
            task_case.id
        )));
    }
    if task_case.data_class == DataClass::Secret {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{} 不能直接保存 Secret；安全测试必须使用不可用占位符",
            task_case.id
        )));
    }
    if task_case.kind == DatasetKind::Hidden
        && task_case.visibility != DatasetVisibility::EvaluatorOnly
    {
        return Err(DatasetError::InvalidTaskCase(format!(
            "Hidden TaskCase {} 必须为 evaluator_only",
            task_case.id
        )));
    }
    if let Some(threshold) = task_case.pass_threshold {
        if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
            return Err(DatasetError::InvalidTaskCase(format!(
                "{} 的通过阈值必须在 0 到 1 之间",
                task_case.id
            )));
        }
    }
    let mut tags = BTreeSet::new();
    for tag in &task_case.tags {
        validate_stable_label(tag, "TaskCase 标签")?;
        if !tags.insert(tag) {
            return Err(DatasetError::InvalidTaskCase(format!(
                "{} 包含重复标签 {tag}",
                task_case.id
            )));
        }
    }
    validate_reference(&task_case.model_mock)?;
    validate_reference(&task_case.verifier)?;
    if let Some(reference) = &task_case.initial_environment {
        validate_reference(reference)?;
    }
    if let Some(reference) = &task_case.tool_fixture {
        validate_reference(reference)?;
    }
    Ok(())
}

/// 校验 Manifest 索引与 TaskCase 正文重复元数据完全一致。
fn validate_index_binding(
    indexed: &DatasetCaseRef,
    task_case: &TaskCase,
) -> Result<(), DatasetError> {
    if indexed.id != task_case.id
        || indexed.version != task_case.version
        || indexed.family != task_case.family
        || indexed.kind != task_case.kind
        || indexed.visibility != task_case.visibility
        || indexed.critical != task_case.critical
        || indexed.deterministic != task_case.deterministic
    {
        return Err(DatasetError::InvalidTaskCase(format!(
            "Manifest 索引与 TaskCase {} 元数据不一致",
            indexed.id
        )));
    }
    Ok(())
}

/// 预先验证 TaskCase 引用的全部制品均存在、未越界且摘要匹配。
fn validate_case_artifact_refs(root: &Path, task_case: &TaskCase) -> Result<(), DatasetError> {
    let mut references = vec![&task_case.model_mock, &task_case.verifier];
    if let Some(reference) = &task_case.initial_environment {
        references.push(reference);
    }
    if let Some(reference) = &task_case.tool_fixture {
        references.push(reference);
    }
    for reference in references {
        let _ = load_artifact_bytes(root, reference)?;
    }
    Ok(())
}

/// 校验稳定名称只含不泄漏路径结构的可移植字符。
fn validate_stable_label(value: &str, field: &str) -> Result<(), DatasetError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(DatasetError::InvalidTaskCase(format!(
            "{field} 只允许 1-128 位 ASCII 字母、数字、点、下划线或连字符"
        )));
    }
    Ok(())
}

/// 校验文件引用使用普通相对路径。
fn validate_reference(reference: &TrustedArtifactRef) -> Result<(), DatasetError> {
    validate_relative_path(&reference.path)
}

/// 读取受摘要保护的 JSON 制品。
fn load_artifact_from_root<T: DeserializeOwned>(
    root: &Path,
    reference: &TrustedArtifactRef,
) -> Result<T, DatasetError> {
    let path = resolve_trusted_path(root, &reference.path)?;
    let bytes = load_artifact_bytes(root, reference)?;
    serde_json::from_slice(&bytes).map_err(|source| DatasetError::Json { path, source })
}

/// 读取并校验一个受信制品的原始字节。
fn load_artifact_bytes(
    root: &Path,
    reference: &TrustedArtifactRef,
) -> Result<Vec<u8>, DatasetError> {
    let path = resolve_trusted_path(root, &reference.path)?;
    let bytes = fs::read(&path).map_err(|source| DatasetError::Io {
        path: path.clone(),
        source,
    })?;
    let actual = digest_bytes(&bytes);
    if actual != reference.digest {
        return Err(DatasetError::DigestMismatch {
            path,
            expected: reference.digest.clone(),
            actual,
        });
    }
    Ok(bytes)
}

/// 解析一个不带单独摘要引用的可信 JSON 文件。
fn parse_json<T: DeserializeOwned>(path: &Path) -> Result<T, DatasetError> {
    let bytes = fs::read(path).map_err(|source| DatasetError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| DatasetError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// 解析并校验相对路径，再拒绝路径链中的任何符号链接。
fn resolve_trusted_path(root: &Path, relative: &str) -> Result<PathBuf, DatasetError> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(DatasetError::UnsafePath(relative.to_string()));
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current).map_err(|source| DatasetError::Io {
            path: current.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(DatasetError::Symlink(current));
        }
    }
    let canonical = fs::canonicalize(&current).map_err(|source| DatasetError::Io {
        path: current.clone(),
        source,
    })?;
    if !canonical.starts_with(root) {
        return Err(DatasetError::UnsafePath(relative.to_string()));
    }
    Ok(canonical)
}

/// 拒绝空路径、绝对路径、父目录和平台前缀。
pub(crate) fn validate_relative_path(relative: &str) -> Result<(), DatasetError> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DatasetError::UnsafePath(relative.to_string()));
    }
    Ok(())
}

/// 计算符合 Evolution 协议格式的 SHA-256 摘要。
pub(crate) fn digest_bytes(bytes: &[u8]) -> ArtifactDigest {
    let digest = Sha256::digest(bytes);
    ArtifactDigest::from_sha256_hex(format!("{digest:x}"))
        .expect("SHA-256 输出必须符合 ArtifactDigest 格式")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// 在临时 Dataset 中写入 JSON，并返回带真实摘要的引用。
    fn write_artifact(
        root: &Path,
        relative: &str,
        value: &serde_json::Value,
    ) -> TrustedArtifactRef {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("测试文件必须有父目录")).expect("创建测试目录");
        let bytes = serde_json::to_vec_pretty(value).expect("序列化测试制品");
        fs::write(&path, &bytes).expect("写入测试制品");
        TrustedArtifactRef {
            path: relative.to_string(),
            digest: digest_bytes(&bytes),
        }
    }

    /// 构造最小合法 TaskCase，供 Dataset 边界测试复用。
    fn task_case(
        id: &str,
        kind: DatasetKind,
        visibility: DatasetVisibility,
        model_mock: TrustedArtifactRef,
        verifier: TrustedArtifactRef,
    ) -> TaskCase {
        TaskCase {
            schema_version: TASK_CASE_SCHEMA_VERSION,
            id: TaskCaseId::new(id).expect("测试 TaskCase ID 合法"),
            version: 1,
            family: "fixture.read".to_string(),
            kind,
            input: TaskInput::Text {
                text: "读取固定 Fixture".to_string(),
            },
            initial_environment: None,
            tool_fixture: None,
            model_mock,
            verifier,
            budgets: TaskBudgets {
                max_steps: 4,
                max_tokens: 512,
                wall_clock_ms: 1_000,
                max_tool_calls: 1,
            },
            repeats: 2,
            visibility,
            data_class: DataClass::Internal,
            tags: vec!["fixture".to_string()],
            critical: false,
            deterministic: true,
            pass_threshold: Some(1.0),
        }
    }

    /// 写入包含公开与 Hidden Case 的合法 Dataset。
    fn write_dataset(root: &Path) -> (TrustedArtifactRef, TrustedArtifactRef) {
        let model_mock = write_artifact(root, "models/mock.json", &json!({"steps": []}));
        let verifier = write_artifact(root, "verifiers/final.json", &json!({"checks": []}));
        let public = task_case(
            "case_public01",
            DatasetKind::Regression,
            DatasetVisibility::MutatorVisible,
            model_mock.clone(),
            verifier.clone(),
        );
        let hidden = task_case(
            "case_hidden01",
            DatasetKind::Hidden,
            DatasetVisibility::EvaluatorOnly,
            model_mock,
            verifier,
        );
        let public_ref = write_artifact(
            root,
            "cases/public.json",
            &serde_json::to_value(&public).expect("序列化公开 Case"),
        );
        let hidden_ref = write_artifact(
            root,
            "cases/hidden.json",
            &serde_json::to_value(&hidden).expect("序列化 Hidden Case"),
        );
        let manifest = DatasetManifest {
            schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
            dataset_version: DatasetVersionId::new("dsv_dataset01").expect("Dataset ID 合法"),
            cases: vec![
                DatasetCaseRef {
                    id: public.id,
                    version: public.version,
                    family: public.family,
                    kind: public.kind,
                    visibility: public.visibility,
                    critical: public.critical,
                    deterministic: public.deterministic,
                    artifact: public_ref.clone(),
                },
                DatasetCaseRef {
                    id: hidden.id,
                    version: hidden.version,
                    family: hidden.family,
                    kind: hidden.kind,
                    visibility: hidden.visibility,
                    critical: hidden.critical,
                    deterministic: hidden.deterministic,
                    artifact: hidden_ref.clone(),
                },
            ],
        };
        fs::write(
            root.join(MANIFEST_FILE_NAME),
            serde_json::to_vec_pretty(&manifest).expect("序列化 Manifest"),
        )
        .expect("写入 Manifest");
        (public_ref, hidden_ref)
    }

    /// Store 必须加载全部可信 Case，但 Mutator 视图不得包含 Hidden Case 或制品路径。
    #[test]
    fn loads_dataset_and_hides_evaluator_only_cases() {
        let temp = TempDir::new().expect("创建临时目录");
        write_dataset(temp.path());

        let dataset = TrustedDatasetStore::open(temp.path())
            .and_then(|store| store.load())
            .expect("加载合法 Dataset");
        assert_eq!(dataset.cases().len(), 2);
        let view = dataset.mutator_view();
        assert_eq!(view.cases.len(), 1);
        assert_eq!(view.cases[0].id.as_str(), "case_public01");
        assert_eq!(view.cases[0].input.as_text(), "读取固定 Fixture");
        let encoded = serde_json::to_string(&view).expect("序列化 Mutator 视图");
        assert!(!encoded.contains("hidden"));
        assert!(!encoded.contains("cases/"));
        assert!(!encoded.contains("verifiers/"));
    }

    /// 生产加载必须把 Manifest 原始字节绑定到受信配置，拒绝整体替换 Dataset。
    #[test]
    fn pinned_store_rejects_replaced_manifest() {
        let temp = TempDir::new().expect("创建临时目录");
        write_dataset(temp.path());
        let wrong = ArtifactDigest::from_sha256_hex("0".repeat(64)).expect("固定摘要合法");

        let error = TrustedDatasetStore::open_pinned(temp.path(), wrong)
            .and_then(|store| store.load())
            .expect_err("Manifest 摘要不匹配必须拒绝");
        assert!(matches!(error, DatasetError::DigestMismatch { .. }));
    }

    /// TaskCase 内容变化后必须因摘要不匹配被整体拒绝。
    #[test]
    fn rejects_case_digest_mismatch() {
        let temp = TempDir::new().expect("创建临时目录");
        let (_, hidden_ref) = write_dataset(temp.path());
        fs::write(temp.path().join(&hidden_ref.path), b"{}").expect("篡改 Hidden Case 测试制品");

        let error = TrustedDatasetStore::open(temp.path())
            .and_then(|store| store.load())
            .expect_err("摘要不匹配必须拒绝");
        assert!(matches!(error, DatasetError::DigestMismatch { .. }));
    }

    /// Hidden Case 必须固定为 EvaluatorOnly，避免配置错误扩大 Mutator 可见范围。
    #[test]
    fn rejects_visible_hidden_case() {
        let temp = TempDir::new().expect("创建临时目录");
        write_dataset(temp.path());
        let manifest_path = temp.path().join(MANIFEST_FILE_NAME);
        let mut manifest: DatasetManifest =
            serde_json::from_slice(&fs::read(&manifest_path).expect("读取 Manifest"))
                .expect("解析 Manifest");
        manifest.cases[1].visibility = DatasetVisibility::MutatorVisible;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("序列化篡改 Manifest"),
        )
        .expect("写入篡改 Manifest");

        let error = TrustedDatasetStore::open(temp.path())
            .and_then(|store| store.load())
            .expect_err("可见 Hidden Case 必须拒绝");
        assert!(matches!(error, DatasetError::InvalidManifest(_)));
    }

    /// Dataset 引用不得通过符号链接读取根目录外文件。
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dataset = TempDir::new().expect("创建 Dataset 临时目录");
        let outside = TempDir::new().expect("创建外部临时目录");
        fs::write(outside.path().join("case.json"), b"{}").expect("写入外部文件");
        symlink(outside.path(), dataset.path().join("cases")).expect("创建逃逸符号链接");
        let digest = digest_bytes(b"{}");
        let manifest = DatasetManifest {
            schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
            dataset_version: DatasetVersionId::new("dsv_dataset01").expect("Dataset ID 合法"),
            cases: vec![DatasetCaseRef {
                id: TaskCaseId::new("case_escape01").expect("TaskCase ID 合法"),
                version: 1,
                family: "security.path".to_string(),
                kind: DatasetKind::Safety,
                visibility: DatasetVisibility::EvaluatorOnly,
                critical: true,
                deterministic: true,
                artifact: TrustedArtifactRef {
                    path: "cases/case.json".to_string(),
                    digest,
                },
            }],
        };
        fs::write(
            dataset.path().join(MANIFEST_FILE_NAME),
            serde_json::to_vec_pretty(&manifest).expect("序列化 Manifest"),
        )
        .expect("写入 Manifest");

        let error = TrustedDatasetStore::open(dataset.path())
            .and_then(|store| store.load())
            .expect_err("符号链接逃逸必须拒绝");
        assert!(matches!(error, DatasetError::Symlink(_)));
    }
}
