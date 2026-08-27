//! M6 Context Policy 离线观察 Fixture 的受信加载边界。

use agent_evolution_protocol::{
    ArtifactDigest, ContextEvaluationObservationV1, DatasetVersionId, GenomeRevisionId,
    InvalidContextEvaluation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::fs;

/// 当前 Context Observation Fixture schema 版本。
pub const CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION: u32 = 1;
/// 单个 Context Observation Fixture 的最大字节数。
pub const MAX_CONTEXT_OBSERVATION_FIXTURE_BYTES: u64 = 2 * 1024 * 1024;

/// 受信 Evaluator 使用的版本化 Context 原始观察集合。
///
/// 文件只保存脱敏聚合计数和资源度量，不包含用户正文、模型输出、ToolResult 或 Hidden 答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextObservationFixtureV1 {
    /// Fixture schema 版本。
    pub schema_version: u32,
    /// Fixture 内容版本，供 Evolver 声明预期前置条件。
    pub fixture_version: DatasetVersionId,
    /// 按真实 Genome Revision 绑定的原始观察。
    pub observations: BTreeMap<GenomeRevisionId, ContextEvaluationObservationV1>,
}

impl ContextObservationFixtureV1 {
    /// 校验版本、观察数量和每项原始计数边界。
    ///
    /// # Errors
    ///
    /// Schema 未知、观察少于两项或任一观察无效时返回 [`ContextFixtureError`]。
    pub fn validate(&self) -> Result<(), ContextFixtureError> {
        if self.schema_version != CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION {
            return Err(ContextFixtureError::UnsupportedSchema {
                found: self.schema_version,
                supported: CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION,
            });
        }
        if self.observations.len() < 2 {
            return Err(ContextFixtureError::InsufficientObservations);
        }
        for observation in self.observations.values() {
            observation.validate()?;
        }
        Ok(())
    }
}

/// 已按固定摘要加载并校验的 Context Fixture。
#[derive(Debug, Clone)]
pub struct TrustedContextObservationFixture {
    fixture: ContextObservationFixtureV1,
    digest: ArtifactDigest,
}

impl TrustedContextObservationFixture {
    /// 从受信绝对目录中的 `fixture.json` 加载固定摘要 Fixture。
    ///
    /// 根目录与文件都拒绝符号链接；摘要覆盖文件原始字节，避免调用方替换格式等价但未审核的
    /// 制品。
    ///
    /// # Errors
    ///
    /// 路径、大小、摘要、JSON 或协议不合法时返回 [`ContextFixtureError`]。
    pub async fn open_pinned(
        root: impl Into<PathBuf>,
        expected_digest: ArtifactDigest,
    ) -> Result<Self, ContextFixtureError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(ContextFixtureError::RelativeRoot(root));
        }
        let root_metadata = fs::symlink_metadata(&root)
            .await
            .map_err(|source| io_error("检查 Context Fixture 根目录", &root, source))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(ContextFixtureError::UnsafePath(root));
        }
        let path = root.join("fixture.json");
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(|source| io_error("检查 Context Fixture", &path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ContextFixtureError::UnsafePath(path));
        }
        enforce_size(metadata.len())?;
        let bytes = fs::read(&path)
            .await
            .map_err(|source| io_error("读取 Context Fixture", &path, source))?;
        enforce_size(bytes.len() as u64)?;
        let actual_digest = digest_bytes(&bytes)?;
        if actual_digest != expected_digest {
            return Err(ContextFixtureError::DigestMismatch {
                expected: expected_digest,
                actual: actual_digest,
            });
        }
        let fixture: ContextObservationFixtureV1 = serde_json::from_slice(&bytes)
            .map_err(|source| ContextFixtureError::InvalidJson { path, source })?;
        fixture.validate()?;
        Ok(Self {
            fixture,
            digest: actual_digest,
        })
    }

    /// 返回 Fixture 内容版本。
    pub fn version(&self) -> &DatasetVersionId {
        &self.fixture.fixture_version
    }

    /// 返回固定 Fixture 原始字节摘要。
    pub fn digest(&self) -> &ArtifactDigest {
        &self.digest
    }

    /// 按 Revision 读取一份已校验观察。
    ///
    /// # Errors
    ///
    /// Fixture 未包含指定 Revision 时返回 [`ContextFixtureError::ObservationNotFound`]。
    pub fn observation(
        &self,
        revision_id: &GenomeRevisionId,
    ) -> Result<&ContextEvaluationObservationV1, ContextFixtureError> {
        self.fixture
            .observations
            .get(revision_id)
            .ok_or_else(|| ContextFixtureError::ObservationNotFound(revision_id.clone()))
    }
}

/// Context Observation Fixture 加载或校验错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextFixtureError {
    /// Fixture 根目录必须来自绝对受信配置。
    #[error("Context Fixture 根目录必须是绝对路径：{0}")]
    RelativeRoot(PathBuf),
    /// 根目录或文件不是预期的非符号链接类型。
    #[error("Context Fixture 路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// Fixture schema 不受支持。
    #[error("不支持的 Context Fixture schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchema {
        /// 实际版本。
        found: u32,
        /// 当前版本。
        supported: u32,
    },
    /// 对照评测至少需要 Parent 和一个 Candidate 观察。
    #[error("Context Fixture 至少需要两项 Revision 观察")]
    InsufficientObservations,
    /// 原始观察违反 Context Evaluation 协议。
    #[error(transparent)]
    InvalidObservation(#[from] InvalidContextEvaluation),
    /// Fixture 原始文件摘要与受信配置不一致。
    #[error("Context Fixture 摘要不匹配：期望 {expected}，实际 {actual}")]
    DigestMismatch {
        /// 受信配置固定的摘要。
        expected: ArtifactDigest,
        /// 实际文件摘要。
        actual: ArtifactDigest,
    },
    /// Fixture 中缺少请求 Revision 的观察。
    #[error("Context Fixture 缺少 Revision 观察：{0}")]
    ObservationNotFound(GenomeRevisionId),
    /// Fixture 超过固定读取上限。
    #[error("Context Fixture 过大：{actual} 字节，上限 {maximum} 字节")]
    TooLarge {
        /// 实际字节数。
        actual: u64,
        /// 固定上限。
        maximum: u64,
    },
    /// Fixture JSON 无法解析。
    #[error("Context Fixture JSON 损坏 `{path}`：{source}")]
    InvalidJson {
        /// Fixture 文件路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        source: serde_json::Error,
    },
    /// SHA-256 文本无法构造成协议摘要。
    #[error("构造 Context Fixture 摘要失败：{0}")]
    InvalidDigest(String),
    /// 文件系统操作失败。
    #[error("{operation}失败 `{path}`：{source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: std::io::Error,
    },
}

/// 校验 Fixture 字节数不超过固定上限。
fn enforce_size(actual: u64) -> Result<(), ContextFixtureError> {
    if actual > MAX_CONTEXT_OBSERVATION_FIXTURE_BYTES {
        return Err(ContextFixtureError::TooLarge {
            actual,
            maximum: MAX_CONTEXT_OBSERVATION_FIXTURE_BYTES,
        });
    }
    Ok(())
}

/// 计算原始 Fixture 文件的协议摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, ContextFixtureError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| ContextFixtureError::InvalidDigest(error.to_string()))
}

/// 构造保留路径上下文的 Fixture I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> ContextFixtureError {
    ContextFixtureError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        RecallObservationV1, CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
    };
    use tempfile::TempDir;

    /// 构造全部召回且资源计数合法的固定观察。
    fn observation(tokens_after: u64) -> ContextEvaluationObservationV1 {
        let recall = RecallObservationV1 {
            expected: 10,
            recalled: 10,
        };
        ContextEvaluationObservationV1 {
            schema_version: CONTEXT_EVALUATION_OBSERVATION_SCHEMA_VERSION,
            facts: recall,
            constraints: recall,
            tool_states: recall,
            plan_states: recall,
            downstream_tasks: recall,
            tokens_before: 10_000,
            tokens_after,
            cost_microunits: 100,
            latency_ms: 800,
        }
    }

    /// 生成包含 Parent 与 Candidate 的合法 Fixture 原始字节。
    fn fixture_bytes() -> (Vec<u8>, ContextObservationFixtureV1) {
        let fixture = ContextObservationFixtureV1 {
            schema_version: CONTEXT_OBSERVATION_FIXTURE_SCHEMA_VERSION,
            fixture_version: DatasetVersionId::generate(),
            observations: BTreeMap::from([
                (GenomeRevisionId::generate(), observation(7_000)),
                (GenomeRevisionId::generate(), observation(6_000)),
            ]),
        };
        let bytes = serde_json::to_vec(&fixture).expect("Fixture 应可序列化");
        (bytes, fixture)
    }

    /// 固定摘要加载必须接受原始制品，并在文件替换后失败关闭。
    #[tokio::test]
    async fn pinned_fixture_rejects_replaced_bytes() {
        let temp = TempDir::new().expect("应创建 Fixture 临时目录");
        let (bytes, fixture) = fixture_bytes();
        let expected_digest = digest_bytes(&bytes).expect("Fixture 摘要应合法");
        fs::write(temp.path().join("fixture.json"), &bytes)
            .await
            .expect("应写入 Fixture");

        let trusted =
            TrustedContextObservationFixture::open_pinned(temp.path(), expected_digest.clone())
                .await
                .expect("固定摘要匹配时应加载成功");
        assert_eq!(trusted.version(), &fixture.fixture_version);
        assert_eq!(trusted.digest(), &expected_digest);

        fs::write(temp.path().join("fixture.json"), b"{}")
            .await
            .expect("应替换 Fixture 测试制品");
        let error = TrustedContextObservationFixture::open_pinned(temp.path(), expected_digest)
            .await
            .expect_err("固定摘要不匹配时必须拒绝");
        assert!(matches!(error, ContextFixtureError::DigestMismatch { .. }));
    }

    /// Fixture 根必须是绝对目录，并拒绝符号链接替代受信文件。
    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_fixture_rejects_relative_root_and_symlink() {
        use std::os::unix::fs::symlink;

        let (_, fixture) = fixture_bytes();
        let relative_error = TrustedContextObservationFixture::open_pinned(
            PathBuf::from("relative-fixture"),
            digest('0'),
        )
        .await
        .expect_err("相对 Fixture 根必须拒绝");
        assert!(matches!(
            relative_error,
            ContextFixtureError::RelativeRoot(_)
        ));

        let temp = TempDir::new().expect("应创建 Fixture 临时目录");
        let outside = TempDir::new().expect("应创建外部临时目录");
        let bytes = serde_json::to_vec(&fixture).expect("Fixture 应可序列化");
        fs::write(outside.path().join("fixture.json"), &bytes)
            .await
            .expect("应写入外部 Fixture");
        symlink(
            outside.path().join("fixture.json"),
            temp.path().join("fixture.json"),
        )
        .expect("应创建 Fixture 符号链接");

        let error = TrustedContextObservationFixture::open_pinned(
            temp.path(),
            digest_bytes(&bytes).expect("Fixture 摘要应合法"),
        )
        .await
        .expect_err("Fixture 符号链接必须拒绝");
        assert!(matches!(error, ContextFixtureError::UnsafePath(_)));
    }

    /// 从十六进制字符构造固定测试摘要。
    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
    }
}
