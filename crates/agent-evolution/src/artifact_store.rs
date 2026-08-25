//! SHA-256 内容寻址的不可变制品存储。

use agent_evolution_protocol::{ArtifactDigest, ArtifactRef};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// 不可变制品存储接口。
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// 写入制品并返回内容引用；相同内容必须得到相同引用。
    ///
    /// # Errors
    ///
    /// 存储目录不安全、摘要冲突或 I/O 失败时返回错误。
    async fn put(&self, media_type: &str, bytes: &[u8]) -> Result<ArtifactRef, ArtifactStoreError>;

    /// 按摘要读取完整制品，不存在时返回 `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 文件不安全、内容摘要不匹配或 I/O 失败时返回错误。
    async fn get(&self, digest: &ArtifactDigest) -> Result<Option<Vec<u8>>, ArtifactStoreError>;
}

/// 本地文件 Artifact CAS。
#[derive(Debug, Clone)]
pub struct FileArtifactStore {
    root: PathBuf,
}

impl FileArtifactStore {
    /// 打开指定根目录；目录在首次写入时创建。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 将摘要解析为两级固定路径，避免单目录堆积大量文件。
    fn artifact_path(&self, digest: &ArtifactDigest) -> PathBuf {
        let hex = digest.as_str().trim_start_matches("sha256:");
        self.root.join("sha256").join(&hex[..2]).join(hex)
    }
}

#[async_trait]
impl ArtifactStore for FileArtifactStore {
    async fn put(&self, media_type: &str, bytes: &[u8]) -> Result<ArtifactRef, ArtifactStoreError> {
        let hex = format!("{:x}", Sha256::digest(bytes));
        let digest = ArtifactDigest::from_sha256_hex(hex)
            .map_err(|error| ArtifactStoreError::InvalidDigest(error.to_string()))?;
        let path = self.artifact_path(&digest);
        let parent = path.parent().expect("制品路径必有父目录");
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&self.root.join("sha256")).await?;
        ensure_safe_directory(parent).await?;

        if let Some(existing) = read_safe_file(&path).await? {
            verify_digest(&digest, &existing)?;
        } else {
            let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4().simple()));
            let result = async {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .await
                    .map_err(|source| io_error("创建制品临时文件", &temporary, source))?;
                file.write_all(bytes)
                    .await
                    .map_err(|source| io_error("写入制品临时文件", &temporary, source))?;
                file.sync_all()
                    .await
                    .map_err(|source| io_error("同步制品临时文件", &temporary, source))?;
                drop(file);
                // 硬链接的创建具有 create-new 语义，不会像 Unix rename 那样覆盖竞态中
                // 已由另一进程提交的同摘要制品。
                match fs::hard_link(&temporary, &path).await {
                    Ok(()) => Ok(()),
                    Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(source) => Err(io_error("提交不可变制品", &path, source)),
                }
            }
            .await;
            let _ = fs::remove_file(&temporary).await;
            result?;
            let committed =
                read_safe_file(&path)
                    .await?
                    .ok_or_else(|| ArtifactStoreError::UnsafePath {
                        path: path.clone(),
                        reason: "提交后制品不可见",
                    })?;
            verify_digest(&digest, &committed)?;
        }

        Ok(ArtifactRef {
            digest,
            media_type: media_type.to_string(),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, digest: &ArtifactDigest) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        let Some(bytes) = read_safe_file(&self.artifact_path(digest)).await? else {
            return Ok(None);
        };
        verify_digest(digest, &bytes)?;
        Ok(Some(bytes))
    }
}

/// Artifact CAS 错误。
#[derive(Debug, thiserror::Error)]
pub enum ArtifactStoreError {
    /// 摘要构造失败。
    #[error("制品摘要无效：{0}")]
    InvalidDigest(String),
    /// 路径包含符号链接或不是预期类型。
    #[error("制品存储路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 稳定原因。
        reason: &'static str,
    },
    /// CAS 内容不匹配其文件名摘要。
    #[error("制品内容摘要不匹配：期望 {expected}，实际 {actual}")]
    DigestMismatch {
        /// 文件名声明的摘要。
        expected: ArtifactDigest,
        /// 实际内容摘要文本。
        actual: String,
    },
    /// 文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始错误。
        #[source]
        source: std::io::Error,
    },
}

/// 创建并验证目标目录自身，拒绝目标被符号链接替换。
///
/// macOS 的 `/var` 本身是系统符号链接，因此不能沿绝对路径向根目录逐段拒绝；调用方
/// 指定的 CAS 目录及其内部目录仍必须都是实际目录。
async fn ensure_safe_directory(path: &Path) -> Result<(), ArtifactStoreError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建制品目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查制品目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArtifactStoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "制品目录必须是非符号链接目录",
        });
    }
    Ok(())
}

/// 只读取非符号链接普通文件。
async fn read_safe_file(path: &Path) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ArtifactStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "制品必须是非符号链接普通文件",
            })
        }
        Ok(_) => fs::read(path)
            .await
            .map(Some)
            .map_err(|source| io_error("读取制品", path, source)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("检查制品", path, source)),
    }
}

/// 验证读取内容仍与 CAS 摘要一致。
fn verify_digest(digest: &ArtifactDigest, bytes: &[u8]) -> Result<(), ArtifactStoreError> {
    let actual = format!("sha256:{:x}", Sha256::digest(bytes));
    if actual != digest.as_str() {
        return Err(ArtifactStoreError::DigestMismatch {
            expected: digest.clone(),
            actual,
        });
    }
    Ok(())
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> ArtifactStoreError {
    ArtifactStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-artifacts-{}", Uuid::new_v4().simple()))
    }

    #[tokio::test]
    async fn stores_identical_content_once_and_verifies_reads() {
        let root = temp_root();
        let store = FileArtifactStore::new(&root);
        let first = store.put("text/plain", b"evidence").await.expect("应写入");
        let second = store.put("text/plain", b"evidence").await.expect("应复用");
        assert_eq!(first, second);
        assert_eq!(
            store.get(&first.digest).await.expect("应读取"),
            Some(b"evidence".to_vec())
        );
        let _ = fs::remove_dir_all(root).await;
    }
}
