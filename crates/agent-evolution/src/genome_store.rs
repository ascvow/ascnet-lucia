//! Genome 修订的不可变文件存储。

use agent_evolution_protocol::{GenomeDigest, GenomeRevision, GenomeRevisionId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// 当前稳定 Genome 引用 JSON 的结构版本。
pub const STABLE_GENOME_REF_SCHEMA_VERSION: u32 = 1;

/// Stable 名称或精确修订构成的 Genome 解析选择器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenomeSelector {
    /// 解析不可变的精确修订。
    Revision(GenomeRevisionId),
    /// 解析可信控制面发布的 Stable lineage。
    Stable(String),
}

/// 一个 lineage 当前指向的不可变 Genome 修订。
///
/// Stable 引用不包含行为正文；读取时 Resolver 会重新校验目标 Revision 及其摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableGenomeRef {
    /// JSON 结构版本。
    pub schema_version: u32,
    /// 稳定 lineage 名称，例如 `stable/general`。
    pub lineage: String,
    /// 当前发布的 Genome 修订。
    pub revision_id: GenomeRevisionId,
    /// 发布时绑定的行为摘要。
    pub digest: GenomeDigest,
    /// lineage 内单调递增的代数。
    pub generation: u64,
}

impl StableGenomeRef {
    /// 从已验证修订创建 Stable 引用数据；本函数不写文件，也不执行 Promotion。
    ///
    /// # Errors
    ///
    /// lineage 不安全或 Revision 摘要无效时返回错误。
    pub fn new(
        lineage: impl Into<String>,
        revision: &GenomeRevision,
        generation: u64,
    ) -> Result<Self, GenomeResolverError> {
        let lineage = lineage.into();
        validate_lineage(&lineage)?;
        revision
            .validate()
            .map_err(|error| GenomeResolverError::InvalidStableRef(error.to_string()))?;
        Ok(Self {
            schema_version: STABLE_GENOME_REF_SCHEMA_VERSION,
            lineage,
            revision_id: revision.revision_id.clone(),
            digest: revision.digest.clone(),
            generation,
        })
    }

    /// 校验 Stable 引用自身的版本、lineage 与字段边界。
    ///
    /// # Errors
    ///
    /// 结构版本未知、lineage 不安全时返回错误。
    pub fn validate(&self) -> Result<(), GenomeResolverError> {
        if self.schema_version != STABLE_GENOME_REF_SCHEMA_VERSION {
            return Err(GenomeResolverError::InvalidStableRef(format!(
                "不支持的 schema_version {}",
                self.schema_version
            )));
        }
        validate_lineage(&self.lineage)
    }
}

/// Genome Resolver 的只读契约。
#[async_trait]
pub trait GenomeResolver: Send + Sync {
    /// 解析并验证选择器指向的不可变 Revision。
    ///
    /// # Errors
    ///
    /// 目标不存在、路径不安全、记录损坏或摘要不一致时返回错误。
    async fn resolve(
        &self,
        selector: &GenomeSelector,
    ) -> Result<GenomeRevision, GenomeResolverError>;
}

/// 文件系统上的只读 Genome Resolver。
///
/// 数据根下的 `genomes/` 保存不可变 Revision，`stable/` 保存由可信发布控制面写入的
/// Stable 引用。Resolver 自身不公开写 Stable 的接口，避免普通运行路径冒充 Promotion。
#[derive(Debug, Clone)]
pub struct FileGenomeResolver {
    store: FileGenomeStore,
    stable_root: PathBuf,
}

impl FileGenomeResolver {
    /// 按 Evolution 数据根创建 Resolver，且不触碰文件系统。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            store: FileGenomeStore::new(root.join("genomes")),
            stable_root: root.join("stable"),
        }
    }

    /// 返回 Resolver 使用的不可变 Revision Store。
    pub fn store(&self) -> &FileGenomeStore {
        &self.store
    }

    /// 返回 lineage 对应的固定引用路径；文件名只来自 lineage 摘要，不包含原始路径片段。
    fn stable_ref_path(&self, lineage: &str) -> PathBuf {
        let name = format!("{:x}.json", Sha256::digest(lineage.as_bytes()));
        self.stable_root.join(name)
    }
}

#[async_trait]
impl GenomeResolver for FileGenomeResolver {
    async fn resolve(
        &self,
        selector: &GenomeSelector,
    ) -> Result<GenomeRevision, GenomeResolverError> {
        let (revision_id, stable) = match selector {
            GenomeSelector::Revision(revision_id) => (revision_id.clone(), None),
            GenomeSelector::Stable(lineage) => {
                validate_lineage(lineage)?;
                let reference =
                    read_stable_ref(&self.stable_root, &self.stable_ref_path(lineage), lineage)
                        .await?;
                (reference.revision_id.clone(), Some(reference))
            }
        };
        let revision = self
            .store
            .get(&revision_id)
            .await?
            .ok_or_else(|| GenomeResolverError::RevisionNotFound(revision_id.clone()))?;
        if let Some(reference) = stable {
            if revision.digest != reference.digest {
                return Err(GenomeResolverError::StableDigestMismatch {
                    lineage: reference.lineage,
                    declared: reference.digest,
                    actual: revision.digest,
                });
            }
        }
        Ok(revision)
    }
}

/// Genome 解析错误。
#[derive(Debug, thiserror::Error)]
pub enum GenomeResolverError {
    /// 不可变 Revision Store 失败。
    #[error(transparent)]
    Store(#[from] GenomeStoreError),
    /// 精确修订或 Stable 目标不存在。
    #[error("Genome 修订不存在：{0}")]
    RevisionNotFound(GenomeRevisionId),
    /// Stable 引用不存在。
    #[error("Stable Genome 引用不存在：{0}")]
    StableNotFound(String),
    /// Stable 引用结构或 lineage 无效。
    #[error("Stable Genome 引用无效：{0}")]
    InvalidStableRef(String),
    /// Stable 存储路径不安全。
    #[error("Stable Genome 路径不安全：{path}: {reason}")]
    UnsafeStablePath {
        /// 不安全路径。
        path: PathBuf,
        /// 稳定错误原因。
        reason: &'static str,
    },
    /// Stable 引用 JSON 损坏。
    #[error("Stable Genome 引用损坏：{path}: {source}")]
    InvalidStableRecord {
        /// 损坏记录路径。
        path: PathBuf,
        /// JSON 解析错误。
        #[source]
        source: serde_json::Error,
    },
    /// Stable 引用的 lineage 与请求不一致。
    #[error("Stable Genome lineage 不匹配：期望 {expected}，实际 {actual}")]
    StableLineageMismatch {
        /// 请求 lineage。
        expected: String,
        /// 记录 lineage。
        actual: String,
    },
    /// Stable 引用摘要与不可变 Revision 不一致。
    #[error("Stable Genome 摘要不匹配：lineage {lineage}，声明 {declared}，实际 {actual}")]
    StableDigestMismatch {
        /// 引用 lineage。
        lineage: String,
        /// Stable 引用声明摘要。
        declared: GenomeDigest,
        /// Revision 实际摘要。
        actual: GenomeDigest,
    },
    /// Stable 引用文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    StableIo {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        #[source]
        source: std::io::Error,
    },
}

/// Genome 修订存储契约。
///
/// 实现必须保持修订不可变，并在读取时重新校验行为摘要，防止 Episode 绑定到被篡改的
/// Genome 内容。
#[async_trait]
pub trait GenomeStore: Send + Sync {
    /// 只追加一条 Genome 修订。
    ///
    /// # Errors
    ///
    /// 修订无效、同一 ID 已存在或持久化失败时返回错误。
    async fn append(&self, revision: &GenomeRevision) -> Result<(), GenomeStoreError>;

    /// 按修订 ID 读取并验证 Genome。
    ///
    /// 返回 `None` 表示目标不存在。
    ///
    /// # Errors
    ///
    /// 路径不安全、记录损坏、ID 不匹配、摘要不一致或读取失败时返回错误。
    async fn get(&self, id: &GenomeRevisionId) -> Result<Option<GenomeRevision>, GenomeStoreError>;
}

/// 基于本地文件的不可变 Genome Store。
#[derive(Debug, Clone)]
pub struct FileGenomeStore {
    root: PathBuf,
}

impl FileGenomeStore {
    /// 创建尚未触碰文件系统的 Store。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回 Genome 修订根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回指定修订的固定文件路径。
    fn revision_path(&self, id: &GenomeRevisionId) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
}

#[async_trait]
impl GenomeStore for FileGenomeStore {
    async fn append(&self, revision: &GenomeRevision) -> Result<(), GenomeStoreError> {
        revision
            .validate()
            .map_err(|error| GenomeStoreError::InvalidRevision(error.to_string()))?;
        ensure_safe_root(&self.root).await?;

        let path = self.revision_path(&revision.revision_id);
        reject_existing_target(&path, &revision.revision_id).await?;
        let bytes = serde_json::to_vec_pretty(revision)
            .map_err(|source| GenomeStoreError::Serialization { source })?;
        let temporary = self.root.join(format!(".{}.tmp", Uuid::new_v4().simple()));
        let result = async {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|source| io_error("创建 Genome 临时文件", &temporary, source))?;
            file.write_all(&bytes)
                .await
                .map_err(|source| io_error("写入 Genome 临时文件", &temporary, source))?;
            file.sync_all()
                .await
                .map_err(|source| io_error("同步 Genome 临时文件", &temporary, source))?;
            drop(file);
            fs::hard_link(&temporary, &path).await.map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    GenomeStoreError::AlreadyExists(revision.revision_id.clone())
                } else {
                    io_error("提交不可变 Genome 修订", &path, source)
                }
            })
        }
        .await;
        let _ = fs::remove_file(&temporary).await;
        result?;

        // 提交后重新读取，确保最终可见内容通过 ID 与摘要校验。
        read_revision(&path, Some(&revision.revision_id))
            .await?
            .ok_or(GenomeStoreError::UnsafePath {
                path,
                reason: "提交后 Genome 修订不可见",
            })?;
        Ok(())
    }

    async fn get(&self, id: &GenomeRevisionId) -> Result<Option<GenomeRevision>, GenomeStoreError> {
        if !validate_existing_root(&self.root).await? {
            return Ok(None);
        }
        read_revision(&self.revision_path(id), Some(id)).await
    }
}

/// Genome Store 错误。
#[derive(Debug, thiserror::Error)]
pub enum GenomeStoreError {
    /// Genome 修订的结构或行为摘要无效。
    #[error("Genome 修订无效：{0}")]
    InvalidRevision(String),
    /// 同一修订 ID 已存在，禁止覆盖。
    #[error("Genome 修订已存在，禁止覆盖：{0}")]
    AlreadyExists(GenomeRevisionId),
    /// 路径包含符号链接或不是预期类型。
    #[error("Genome 存储路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 稳定错误原因。
        reason: &'static str,
    },
    /// Genome JSON 编码失败。
    #[error("序列化 Genome 修订失败：{source}")]
    Serialization {
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// Genome JSON 记录损坏。
    #[error("Genome 修订记录损坏：{path}: {source}")]
    InvalidRecord {
        /// 损坏文件路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 文件名中的修订 ID 与记录内容不一致。
    #[error("Genome 文件名与修订 ID 不一致：{path}")]
    IdMismatch {
        /// 不匹配的记录路径。
        path: PathBuf,
    },
    /// 文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        #[source]
        source: std::io::Error,
    },
}

/// 校验 Stable lineage 是有限、可展示且不含路径逃逸语义的 ASCII 名称。
fn validate_lineage(lineage: &str) -> Result<(), GenomeResolverError> {
    if lineage.is_empty()
        || lineage.len() > 128
        || lineage.starts_with('/')
        || lineage.ends_with('/')
        || lineage
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || !lineage
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(GenomeResolverError::InvalidStableRef(format!(
            "lineage `{lineage}` 不符合安全标识规则"
        )));
    }
    Ok(())
}

/// 读取并校验 Stable 引用，拒绝根目录或记录文件的符号链接替换。
async fn read_stable_ref(
    root: &Path,
    path: &Path,
    expected_lineage: &str,
) -> Result<StableGenomeRef, GenomeResolverError> {
    let root_metadata = match fs::symlink_metadata(root).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(GenomeResolverError::StableNotFound(
                expected_lineage.to_string(),
            ));
        }
        Err(source) => {
            return Err(stable_io_error("检查 Stable Genome 目录", root, source));
        }
        Ok(metadata) => metadata,
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(GenomeResolverError::UnsafeStablePath {
            path: root.to_path_buf(),
            reason: "Stable Genome 根路径必须是非符号链接目录",
        });
    }
    let metadata = match fs::symlink_metadata(path).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(GenomeResolverError::StableNotFound(
                expected_lineage.to_string(),
            ));
        }
        Err(source) => return Err(stable_io_error("检查 Stable Genome 引用", path, source)),
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(GenomeResolverError::UnsafeStablePath {
            path: path.to_path_buf(),
            reason: "Stable Genome 引用必须是非符号链接普通文件",
        });
    }
    let bytes = fs::read(path)
        .await
        .map_err(|source| stable_io_error("读取 Stable Genome 引用", path, source))?;
    let reference: StableGenomeRef = serde_json::from_slice(&bytes).map_err(|source| {
        GenomeResolverError::InvalidStableRecord {
            path: path.to_path_buf(),
            source,
        }
    })?;
    reference.validate()?;
    if reference.lineage != expected_lineage {
        return Err(GenomeResolverError::StableLineageMismatch {
            expected: expected_lineage.to_string(),
            actual: reference.lineage,
        });
    }
    Ok(reference)
}

/// 构造带路径上下文的 Stable 引用 I/O 错误。
fn stable_io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> GenomeResolverError {
    GenomeResolverError::StableIo {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

/// 创建并验证 Genome 根目录自身，拒绝符号链接替换。
async fn ensure_safe_root(root: &Path) -> Result<(), GenomeStoreError> {
    fs::create_dir_all(root)
        .await
        .map_err(|source| io_error("创建 Genome 目录", root, source))?;
    let metadata = fs::symlink_metadata(root)
        .await
        .map_err(|source| io_error("检查 Genome 目录", root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(GenomeStoreError::UnsafePath {
            path: root.to_path_buf(),
            reason: "Genome 根路径必须是非符号链接目录",
        });
    }
    Ok(())
}

/// 验证只读路径的根目录；不存在时不创建并返回 `false`。
async fn validate_existing_root(root: &Path) -> Result<bool, GenomeStoreError> {
    match fs::symlink_metadata(root).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("检查 Genome 目录", root, source)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(GenomeStoreError::UnsafePath {
                path: root.to_path_buf(),
                reason: "Genome 根路径必须是非符号链接目录",
            })
        }
        Ok(_) => Ok(true),
    }
}

/// 在提交前拒绝任何已存在的目标，包括符号链接和目录。
async fn reject_existing_target(
    path: &Path,
    id: &GenomeRevisionId,
) -> Result<(), GenomeStoreError> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Err(GenomeStoreError::AlreadyExists(id.clone())),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("检查 Genome 修订文件", path, source)),
    }
}

/// 读取并校验单条 Genome 修订。
async fn read_revision(
    path: &Path,
    expected_id: Option<&GenomeRevisionId>,
) -> Result<Option<GenomeRevision>, GenomeStoreError> {
    match fs::symlink_metadata(path).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("检查 Genome 修订文件", path, source)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(GenomeStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "Genome 修订必须是非符号链接普通文件",
            });
        }
        Ok(_) => {}
    }
    let bytes = fs::read(path)
        .await
        .map_err(|source| io_error("读取 Genome 修订", path, source))?;
    let revision: GenomeRevision =
        serde_json::from_slice(&bytes).map_err(|source| GenomeStoreError::InvalidRecord {
            path: path.to_path_buf(),
            source,
        })?;
    if expected_id.is_some_and(|id| id != &revision.revision_id) {
        return Err(GenomeStoreError::IdMismatch {
            path: path.to_path_buf(),
        });
    }
    revision
        .validate()
        .map_err(|error| GenomeStoreError::InvalidRevision(error.to_string()))?;
    Ok(Some(revision))
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> GenomeStoreError {
    GenomeStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        AgentGenome, GenomeMetadata, ModelGenome, PromptGenome, RuntimeIdentity, ToolProfileGenome,
        GENOME_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::collections::{BTreeMap, BTreeSet};

    /// 构造不会与并发测试冲突的临时目录。
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-genomes-{}", Uuid::new_v4().simple()))
    }

    /// 构造最小合法修订，聚焦 Store 的不可变与完整性行为。
    fn revision() -> GenomeRevision {
        GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".into(),
                    git_commit: "test".into(),
                    git_dirty: true,
                    target_triple: "test-target".into(),
                    features: BTreeSet::new(),
                },
                model: ModelGenome {
                    provider: "test".into(),
                    provider_kind: "test".into(),
                    model: "fixture".into(),
                    base_url: None,
                    protocol: None,
                    max_tokens: Some(64),
                    temperature: None,
                    stream: false,
                    provider_options_digest: None,
                },
                prompt: PromptGenome::default(),
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("测试 Genome 应合法")
    }

    /// Store 必须只追加修订，并在读取时保持完整内容。
    #[tokio::test]
    async fn appends_and_reads_immutable_revision() {
        let root = temp_root();
        let store = FileGenomeStore::new(&root);
        let revision = revision();

        store.append(&revision).await.expect("首次追加应成功");
        assert_eq!(
            store.get(&revision.revision_id).await.expect("读取应成功"),
            Some(revision.clone())
        );
        assert!(matches!(
            store.append(&revision).await,
            Err(GenomeStoreError::AlreadyExists(_))
        ));

        let _ = fs::remove_dir_all(root).await;
    }

    /// Resolver 应同时支持精确 Revision 与经过摘要复核的 Stable lineage。
    #[tokio::test]
    async fn resolves_exact_and_stable_revision() {
        let root = temp_root();
        let resolver = FileGenomeResolver::new(&root);
        let revision = revision();
        resolver
            .store()
            .append(&revision)
            .await
            .expect("应登记 Revision");
        let reference =
            StableGenomeRef::new("stable/general", &revision, 3).expect("应构造 Stable 引用");
        fs::create_dir_all(&resolver.stable_root)
            .await
            .expect("应创建 Stable 目录");
        fs::write(
            resolver.stable_ref_path("stable/general"),
            serde_json::to_vec_pretty(&reference).expect("应序列化 Stable 引用"),
        )
        .await
        .expect("应写入 Stable fixture");

        assert_eq!(
            resolver
                .resolve(&GenomeSelector::Revision(revision.revision_id.clone()))
                .await
                .expect("应解析精确 Revision"),
            revision
        );
        assert_eq!(
            resolver
                .resolve(&GenomeSelector::Stable("stable/general".into()))
                .await
                .expect("应解析 Stable lineage"),
            revision
        );
        let _ = fs::remove_dir_all(root).await;
    }

    /// Stable 引用不能用旧摘要指向另一份行为内容。
    #[tokio::test]
    async fn stable_ref_rejects_digest_mismatch() {
        let root = temp_root();
        let resolver = FileGenomeResolver::new(&root);
        let revision = revision();
        resolver
            .store()
            .append(&revision)
            .await
            .expect("应登记 Revision");
        let mut reference =
            StableGenomeRef::new("stable/general", &revision, 1).expect("应构造引用");
        reference.digest = GenomeDigest::from_sha256_hex("0".repeat(64)).expect("摘要应合法");
        fs::create_dir_all(&resolver.stable_root)
            .await
            .expect("应创建 Stable 目录");
        fs::write(
            resolver.stable_ref_path("stable/general"),
            serde_json::to_vec_pretty(&reference).expect("应序列化引用"),
        )
        .await
        .expect("应写入篡改引用");

        assert!(matches!(
            resolver
                .resolve(&GenomeSelector::Stable("stable/general".into()))
                .await,
            Err(GenomeResolverError::StableDigestMismatch { .. })
        ));
        let _ = fs::remove_dir_all(root).await;
    }

    /// lineage 不能携带父目录、绝对路径或空分段。
    #[test]
    fn rejects_unsafe_stable_lineage() {
        let revision = revision();
        for lineage in ["../stable", "/stable/general", "stable//general"] {
            assert!(StableGenomeRef::new(lineage, &revision, 1).is_err());
        }
    }

    /// 篡改行为内容但保留旧摘要时，后续读取必须失败。
    #[tokio::test]
    async fn rejects_tampered_revision_on_read() {
        let root = temp_root();
        let store = FileGenomeStore::new(&root);
        let revision = revision();
        store.append(&revision).await.expect("首次追加应成功");

        let path = store.revision_path(&revision.revision_id);
        let mut tampered = revision.clone();
        tampered.genome.model.model = "tampered".into();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&tampered).expect("应序列化"),
        )
        .await
        .expect("应写入篡改 fixture");

        assert!(matches!(
            store.get(&revision.revision_id).await,
            Err(GenomeStoreError::InvalidRevision(_))
        ));
        let _ = fs::remove_dir_all(root).await;
    }

    /// 读取路径是符号链接时必须拒绝，不能逃逸到 Store 外部。
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_revision() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        fs::create_dir_all(&root).await.expect("应创建目录");
        let revision = revision();
        let outside = root.with_extension("outside.json");
        fs::write(&outside, serde_json::to_vec(&revision).expect("应序列化"))
            .await
            .expect("应写入外部记录");
        symlink(
            &outside,
            root.join(format!("{}.json", revision.revision_id)),
        )
        .expect("应创建符号链接");
        let store = FileGenomeStore::new(&root);

        assert!(matches!(
            store.get(&revision.revision_id).await,
            Err(GenomeStoreError::UnsafePath { .. })
        ));
        let _ = fs::remove_dir_all(root).await;
        let _ = fs::remove_file(outside).await;
    }

    /// Store 根目录本身是符号链接时也必须拒绝，不能只检查最终记录文件。
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_root_on_read() {
        use std::os::unix::fs::symlink;

        let link = temp_root();
        let outside = link.with_extension("outside");
        fs::create_dir_all(&outside).await.expect("应创建外部目录");
        let revision = revision();
        fs::write(
            outside.join(format!("{}.json", revision.revision_id)),
            serde_json::to_vec(&revision).expect("应序列化"),
        )
        .await
        .expect("应写入外部记录");
        symlink(&outside, &link).expect("应创建根目录符号链接");
        let store = FileGenomeStore::new(&link);

        assert!(matches!(
            store.get(&revision.revision_id).await,
            Err(GenomeStoreError::UnsafePath { .. })
        ));
        let _ = fs::remove_file(link).await;
        let _ = fs::remove_dir_all(outside).await;
    }
}
