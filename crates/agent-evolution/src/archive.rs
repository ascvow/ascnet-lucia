//! 派生 Scorecard 与 EvolutionCertificate 的不可变归档。

use crate::{EvolutionCertificate, EvolutionScorecard};
use agent_evolution_protocol::{EvaluationReportId, ReleaseId};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};

/// 本地 Evolution Analytics 归档。
///
/// EvaluationReport 由 [`crate::FileEvaluationReportStore`] 管理；本 Store 只追加可按 Policy
/// 版本重新计算的派生 Scorecard 和 Promotion Certificate，不覆盖源报告。
#[derive(Debug, Clone)]
pub struct FileEvolutionArchive {
    root: PathBuf,
}

impl FileEvolutionArchive {
    /// 创建尚未访问文件系统的归档。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回归档根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 只追加一个 Scorecard；同一报告和 Verdict Policy 可保存多个 Metrics Policy 版本。
    ///
    /// # Errors
    ///
    /// 文件已存在、路径不安全、序列化或文件系统操作失败时返回错误。
    pub async fn append_scorecard(
        &self,
        scorecard: &EvolutionScorecard,
    ) -> Result<PathBuf, ArchiveError> {
        let directory = self.root.join("scorecards");
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&directory).await?;
        let path = directory.join(scorecard_filename(scorecard));
        let bytes = serde_json::to_vec_pretty(scorecard).map_err(ArchiveError::Serialize)?;
        write_create_new(&path, &bytes).await?;
        Ok(path)
    }

    /// 读取全部 Scorecard，按生成时间与报告 ID 稳定排序。
    ///
    /// # Errors
    ///
    /// 任一文件损坏、Schema 未知或路径不安全时返回错误，不会静默跳过坏数据。
    pub async fn list_scorecards(&self) -> Result<Vec<EvolutionScorecard>, ArchiveError> {
        let mut values =
            read_json_directory::<EvolutionScorecard>(&self.root.join("scorecards")).await?;
        for scorecard in &values {
            if scorecard.schema_version != crate::EVOLUTION_SCORECARD_SCHEMA_VERSION {
                return Err(ArchiveError::UnsupportedScorecardSchema(
                    scorecard.schema_version,
                ));
            }
        }
        values.sort_by(|left, right| {
            left.generated_at_ms
                .cmp(&right.generated_at_ms)
                .then_with(|| left.evaluation_report.cmp(&right.evaluation_report))
        });
        Ok(values)
    }

    /// 只追加一个 Promotion Certificate。
    ///
    /// Certificate 会先校验自身结构与摘要；同一 Release ID 已存在时拒绝覆盖。
    ///
    /// # Errors
    ///
    /// Certificate 无效、文件已存在、路径不安全或 I/O 失败时返回错误。
    pub async fn append_certificate(
        &self,
        certificate: &EvolutionCertificate,
    ) -> Result<PathBuf, ArchiveError> {
        certificate.verify_digest()?;
        let directory = self.root.join("certificates");
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&directory).await?;
        let path = directory.join(format!("{}.json", certificate.release_record));
        let bytes = serde_json::to_vec_pretty(certificate).map_err(ArchiveError::Serialize)?;
        write_create_new(&path, &bytes).await?;
        Ok(path)
    }

    /// 按 Release ID 读取并验证 Certificate 自身摘要。
    ///
    /// 不存在时返回 `Ok(None)`；CAS 引用由显式 `--verify` 或 Promotion Gate 校验。
    ///
    /// # Errors
    ///
    /// 文件损坏、Release ID 不匹配、Certificate 摘要无效或路径不安全时返回错误。
    pub async fn certificate(
        &self,
        release: &ReleaseId,
    ) -> Result<Option<EvolutionCertificate>, ArchiveError> {
        let path = self
            .root
            .join("certificates")
            .join(format!("{release}.json"));
        let Some(bytes) = read_safe_file(&path).await? else {
            return Ok(None);
        };
        let certificate: EvolutionCertificate =
            serde_json::from_slice(&bytes).map_err(|source| ArchiveError::InvalidJson {
                path: path.clone(),
                source,
            })?;
        if &certificate.release_record != release {
            return Err(ArchiveError::ReleaseMismatch(path));
        }
        certificate.verify_digest()?;
        Ok(Some(certificate))
    }

    /// 读取全部 Certificate，供历史分析使用。
    ///
    /// # Errors
    ///
    /// 任一文件损坏、摘要无效或路径不安全时返回错误。
    pub async fn list_certificates(&self) -> Result<Vec<EvolutionCertificate>, ArchiveError> {
        let mut values =
            read_json_directory::<EvolutionCertificate>(&self.root.join("certificates")).await?;
        for certificate in &values {
            certificate.verify_digest()?;
        }
        values.sort_by(|left, right| left.release_record.cmp(&right.release_record));
        Ok(values)
    }

    /// 返回同一报告、Metrics Policy 与 Verdict Policy 的 Scorecard 文件名。
    pub fn scorecard_path(
        &self,
        report: &EvaluationReportId,
        metrics_policy: &str,
        verdict_policy: &str,
    ) -> PathBuf {
        self.root.join("scorecards").join(format!(
            "{}--{}--{}.json",
            report,
            safe_segment(metrics_policy),
            safe_segment(verdict_policy)
        ))
    }
}

/// Evolution Analytics 归档错误。
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// 目标文件已经存在，禁止覆盖。
    #[error("Evolution 归档文件已存在，禁止覆盖：{0}")]
    AlreadyExists(PathBuf),
    /// 路径是符号链接或类型不符合预期。
    #[error("Evolution 归档路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 稳定原因。
        reason: &'static str,
    },
    /// JSON 序列化失败。
    #[error("序列化 Evolution 归档失败：{0}")]
    Serialize(serde_json::Error),
    /// JSON 文件损坏。
    #[error("Evolution 归档 JSON 损坏：{path}: {source}")]
    InvalidJson {
        /// 损坏路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// Scorecard Schema 未知。
    #[error("不支持的 EvolutionScorecard schema 版本 {0}")]
    UnsupportedScorecardSchema(u32),
    /// Certificate 文件名与正文 Release ID 不一致。
    #[error("EvolutionCertificate 文件名与 Release ID 不一致：{0}")]
    ReleaseMismatch(PathBuf),
    /// Certificate 自身或其摘要无效。
    #[error("EvolutionCertificate 无效：{0}")]
    Certificate(#[from] crate::CertificateError),
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

/// 创建 Scorecard 的稳定文件名，策略字符串只作为显示友好的索引段。
fn scorecard_filename(scorecard: &EvolutionScorecard) -> String {
    format!(
        "{}--{}--{}.json",
        scorecard.evaluation_report,
        safe_segment(&scorecard.metrics_policy_version),
        safe_segment(&scorecard.verdict_policy_version)
    )
}

/// 把策略版本收窄为文件名安全字符，空值使用固定占位。
fn safe_segment(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(64)
        .collect();
    if value.is_empty() {
        "unknown".into()
    } else {
        value
    }
}

/// 创建并验证普通目录。
async fn ensure_safe_directory(path: &Path) -> Result<(), ArchiveError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 Evolution 归档目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Evolution 归档目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArchiveError::UnsafePath {
            path: path.to_path_buf(),
            reason: "目录必须是非符号链接普通目录",
        });
    }
    Ok(())
}

/// 以 create-new 语义写入归档文件。
async fn write_create_new(path: &Path, bytes: &[u8]) -> Result<(), ArchiveError> {
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ArchiveError::AlreadyExists(path.to_path_buf()));
        }
        Err(source) => return Err(io_error("创建 Evolution 归档", path, source)),
    };
    file.write_all(bytes)
        .await
        .map_err(|source| io_error("写入 Evolution 归档", path, source))?;
    file.sync_all()
        .await
        .map_err(|source| io_error("同步 Evolution 归档", path, source))
}

/// 读取一个目录内全部 JSON 文件；不存在时返回空列表。
async fn read_json_directory<T: serde::de::DeserializeOwned>(
    directory: &Path,
) -> Result<Vec<T>, ArchiveError> {
    match fs::symlink_metadata(directory).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error("检查 Evolution 归档目录", directory, source)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ArchiveError::UnsafePath {
                path: directory.to_path_buf(),
                reason: "归档目录必须是非符号链接普通目录",
            });
        }
        Ok(_) => {}
    }
    let mut entries = fs::read_dir(directory)
        .await
        .map_err(|source| io_error("遍历 Evolution 归档目录", directory, source))?;
    let mut values = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| io_error("读取 Evolution 归档目录项", directory, source))?
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes = read_safe_file(&path)
            .await?
            .ok_or_else(|| ArchiveError::UnsafePath {
                path: path.clone(),
                reason: "遍历期间归档文件被移除",
            })?;
        values.push(
            serde_json::from_slice(&bytes)
                .map_err(|source| ArchiveError::InvalidJson { path, source })?,
        );
    }
    Ok(values)
}

/// 只读取非符号链接普通文件。
async fn read_safe_file(path: &Path) -> Result<Option<Vec<u8>>, ArchiveError> {
    match fs::symlink_metadata(path).await {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("检查 Evolution 归档文件", path, source)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ArchiveError::UnsafePath {
                path: path.to_path_buf(),
                reason: "归档目标必须是非符号链接普通文件",
            })
        }
        Ok(_) => fs::read(path)
            .await
            .map(Some)
            .map_err(|source| io_error("读取 Evolution 归档文件", path, source)),
    }
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> ArchiveError {
    ArchiveError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_version_is_reduced_to_safe_filename_segment() {
        assert_eq!(safe_segment("v1/../../secret"), "v1secret");
        assert_eq!(safe_segment(""), "unknown");
    }
}
