//! Genome 修订的不可变文件存储。

use agent_evolution_protocol::{GenomeRevision, GenomeRevisionId};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

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
}
