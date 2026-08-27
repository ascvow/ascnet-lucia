//! 派生 Scorecard 与 EvolutionCertificate 的不可变归档。

use crate::{EvolutionCertificate, EvolutionScorecard};
use agent_evolution_protocol::{EvaluationReportId, ReleaseId};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
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

    /// 只追加一个 Promotion Certificate 状态修订。
    ///
    /// Certificate 会先校验自身结构、摘要和前序状态；同一 Release 的旧修订不会被覆盖。
    ///
    /// # Errors
    ///
    /// Certificate 无效、文件已存在、路径不安全或 I/O 失败时返回错误。
    pub async fn append_certificate(
        &self,
        certificate: &EvolutionCertificate,
    ) -> Result<PathBuf, ArchiveError> {
        certificate.verify_digest()?;
        let revisions = self
            .certificate_revisions(&certificate.release_record)
            .await?;
        validate_appended_revision(&revisions, certificate)?;
        let directory = self.root.join("certificates");
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&directory).await?;
        let path = directory.join(format!(
            "{}--r{}.json",
            certificate.release_record, certificate.revision
        ));
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
        let revisions = self.certificate_history(release).await?;
        Ok(latest_certificate_revision(&revisions)?.cloned())
    }

    /// 返回同一 Release 的完整不可变 Certificate 状态修订链。
    ///
    /// 结果按修订号排序，并校验摘要、连续序号和前序摘要；不存在时返回空列表。
    ///
    /// # Errors
    ///
    /// 任一修订损坏、Schema 未知、链断裂、分叉或路径不安全时返回错误。
    pub async fn certificate_history(
        &self,
        release: &ReleaseId,
    ) -> Result<Vec<EvolutionCertificate>, ArchiveError> {
        let revisions = self.certificate_revisions(release).await?;
        latest_certificate_revision(&revisions)?;
        Ok(revisions)
    }

    /// 读取全部 Certificate，供历史分析使用。
    ///
    /// # Errors
    ///
    /// 任一文件损坏、摘要无效或路径不安全时返回错误。
    pub async fn list_certificates(&self) -> Result<Vec<EvolutionCertificate>, ArchiveError> {
        let values =
            read_json_directory::<EvolutionCertificate>(&self.root.join("certificates")).await?;
        for certificate in &values {
            certificate.verify_digest()?;
        }
        let mut grouped: BTreeMap<ReleaseId, Vec<EvolutionCertificate>> = BTreeMap::new();
        for certificate in values {
            grouped
                .entry(certificate.release_record.clone())
                .or_default()
                .push(certificate);
        }
        let mut latest = Vec::with_capacity(grouped.len());
        for revisions in grouped.values() {
            if let Some(certificate) = latest_certificate_revision(revisions)? {
                latest.push(certificate.clone());
            }
        }
        Ok(latest)
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

    /// 读取同一 Release 的全部不可变 Certificate 状态修订。
    async fn certificate_revisions(
        &self,
        release: &ReleaseId,
    ) -> Result<Vec<EvolutionCertificate>, ArchiveError> {
        let values =
            read_json_directory::<EvolutionCertificate>(&self.root.join("certificates")).await?;
        let mut revisions = Vec::new();
        for certificate in values {
            if &certificate.release_record == release {
                certificate.verify_digest()?;
                revisions.push(certificate);
            }
        }
        revisions.sort_by_key(|certificate| certificate.revision);
        Ok(revisions)
    }
}

/// 校验新修订严格延续当前唯一链尾。
fn validate_appended_revision(
    revisions: &[EvolutionCertificate],
    appended: &EvolutionCertificate,
) -> Result<(), ArchiveError> {
    let latest = latest_certificate_revision(revisions)?;
    match latest {
        None if appended.revision == 0 && appended.previous_certificate_digest.is_none() => Ok(()),
        Some(previous)
            if appended.revision == previous.revision.saturating_add(1)
                && appended.previous_certificate_digest.as_ref()
                    == Some(&previous.certificate_digest)
                && same_promotion_identity(previous, appended)
                && valid_lifecycle_transition(previous, appended) =>
        {
            Ok(())
        }
        _ => Err(ArchiveError::InvalidCertificateChain(format!(
            "Release {} 的修订 r{} 没有延续当前链尾",
            appended.release_record, appended.revision
        ))),
    }
}

/// 返回通过 `previous_certificate_digest` 串联的唯一链尾，并拒绝断链或分叉。
fn latest_certificate_revision(
    revisions: &[EvolutionCertificate],
) -> Result<Option<&EvolutionCertificate>, ArchiveError> {
    if revisions.is_empty() {
        return Ok(None);
    }
    let digests: BTreeSet<_> = revisions
        .iter()
        .map(|certificate| certificate.certificate_digest.clone())
        .collect();
    if digests.len() != revisions.len() {
        return Err(ArchiveError::InvalidCertificateChain(
            "存在重复 Certificate 摘要".into(),
        ));
    }
    let mut by_revision: BTreeMap<u32, &EvolutionCertificate> = BTreeMap::new();
    for certificate in revisions {
        if by_revision
            .insert(certificate.revision, certificate)
            .is_some()
        {
            return Err(ArchiveError::InvalidCertificateChain(format!(
                "Release {} 的修订 r{} 发生分叉",
                certificate.release_record, certificate.revision
            )));
        }
    }
    let root = by_revision
        .get(&0)
        .copied()
        .ok_or_else(|| ArchiveError::InvalidCertificateChain("缺少 r0 初始修订".into()))?;
    if root.previous_certificate_digest.is_some() {
        return Err(ArchiveError::InvalidCertificateChain(
            "r0 不能声明前序 Certificate".into(),
        ));
    }
    let mut previous = root;
    for revision in 1..by_revision.len() as u32 {
        let current = by_revision.get(&revision).copied().ok_or_else(|| {
            ArchiveError::InvalidCertificateChain(format!("缺少连续修订 r{revision}"))
        })?;
        if current.previous_certificate_digest.as_ref() != Some(&previous.certificate_digest) {
            return Err(ArchiveError::InvalidCertificateChain(format!(
                "修订 r{revision} 的前序摘要不匹配"
            )));
        }
        if !same_promotion_identity(previous, current) {
            return Err(ArchiveError::InvalidCertificateChain(format!(
                "修订 r{revision} 修改了不可变 Promotion 字段"
            )));
        }
        if !valid_lifecycle_transition(previous, current) {
            return Err(ArchiveError::InvalidCertificateChain(format!(
                "修订 r{revision} 的生命周期不能从 {:?} 转换为 {:?}",
                previous.lifecycle, current.lifecycle
            )));
        }
        previous = current;
    }
    Ok(Some(previous))
}

/// 判断两个状态修订绑定同一份不可变 Promotion 事实。
fn same_promotion_identity(
    previous: &EvolutionCertificate,
    current: &EvolutionCertificate,
) -> bool {
    previous.schema_version == current.schema_version
        && previous.parent_revision == current.parent_revision
        && previous.child_revision == current.child_revision
        && previous.source_episode_ids == current.source_episode_ids
        && previous.evolution_issue_id == current.evolution_issue_id
        && previous.mutation_id == current.mutation_id
        && previous.allowed_diff == current.allowed_diff
        && previous.candidate_artifacts == current.candidate_artifacts
        && previous.repair_dataset == current.repair_dataset
        && previous.regression_dataset == current.regression_dataset
        && previous.hidden_dataset == current.hidden_dataset
        && previous.safety_dataset == current.safety_dataset
        && previous.repaired_task_case_ids == current.repaired_task_case_ids
        && previous.evaluation_report == current.evaluation_report
        && previous.scorecard == current.scorecard
        && previous.gate_decision == current.gate_decision
        && previous.release_record == current.release_record
}

/// 限制 Certificate 状态只能沿 Promotion → InheritanceVerified → RolledBack 前进。
fn valid_lifecycle_transition(
    previous: &EvolutionCertificate,
    current: &EvolutionCertificate,
) -> bool {
    matches!(
        (previous.lifecycle, current.lifecycle),
        (
            agent_evolution_protocol::EvolutionLifecycle::Promoted,
            agent_evolution_protocol::EvolutionLifecycle::InheritanceVerified
                | agent_evolution_protocol::EvolutionLifecycle::RolledBack
        ) | (
            agent_evolution_protocol::EvolutionLifecycle::InheritanceVerified,
            agent_evolution_protocol::EvolutionLifecycle::RolledBack
        )
    )
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
    /// 同一 Release 的 Certificate 状态修订链断裂、分叉或倒退。
    #[error("EvolutionCertificate 状态修订链无效：{0}")]
    InvalidCertificateChain(String),
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
