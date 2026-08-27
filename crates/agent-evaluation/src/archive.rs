//! 正式 EvaluationReport、Gate、Audit 与 Seal 的可信提交闭包。
//!
//! Report 正文和 Audit 无法跨文件形成单一文件系统事务，因此 Seal 是唯一提交点。中途崩溃
//! 产生的孤立 Report 或 Audit 记录不会被 [`TrustedEvaluationArchive::get_verified`] 返回，
//! Release Controller 也不得绕过本模块直接消费普通 Report Store。

use crate::{
    evaluation_report_digest, AuditEvent, AuditStoreError, CommitGateOutcome, FileAuditLog,
    ReportBuildError, TrustedEvaluationReport,
};
use agent_evolution::{
    ArtifactStore, ArtifactStoreError, EvaluationStoreError, FileArtifactStore,
    FileEvaluationReportStore,
};
use agent_evolution_protocol::{
    ArtifactDigest, AuditRecordId, EvaluationReport, EvaluationReportId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};

/// 当前可信 Evaluation Seal schema 版本。
pub const EVALUATION_SEAL_SCHEMA_VERSION: u32 = 1;

/// 报告提交完成后生成的不可变 Seal。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationSeal {
    /// Seal schema 版本。
    pub schema_version: u32,
    /// 正式报告标识。
    pub report_id: EvaluationReportId,
    /// 正式报告 pretty JSON 的 SHA-256。
    pub report_digest: ArtifactDigest,
    /// 可信 Commit Gate 的完整聚合输出，不含 Hidden 逐 Case内容。
    pub gate: CommitGateOutcome,
    /// Gate 使用的固定 Commit Policy 版本。
    pub commit_policy_version: String,
    /// Runner 使用的 Evaluation Policy 版本。
    pub evaluation_policy_version: String,
    /// Runner 实际加载的 Verifier Set 摘要。
    pub verifier_set_digest: String,
    /// Evaluator 私有完整录制 CAS 制品摘要。
    pub private_artifact_digest: ArtifactDigest,
    /// 绑定正式报告提交事件的 Audit 记录标识。
    pub audit_record_id: AuditRecordId,
    /// 绑定正式报告提交事件的 Audit 记录摘要。
    pub audit_record_digest: ArtifactDigest,
    /// Seal 除本字段外全部内容的 SHA-256。
    pub seal_digest: ArtifactDigest,
}

impl EvaluationSeal {
    /// 校验 Seal schema、自身摘要以及 Gate 与报告的一致性。
    ///
    /// # Errors
    ///
    /// Schema、摘要、决策或生命周期不一致时返回错误。
    fn validate(&self, report: &EvaluationReport) -> Result<(), EvaluationArchiveError> {
        if self.schema_version != EVALUATION_SEAL_SCHEMA_VERSION {
            return Err(EvaluationArchiveError::UnsupportedSealSchema(
                self.schema_version,
            ));
        }
        let actual = self.compute_digest()?;
        if actual != self.seal_digest {
            return Err(EvaluationArchiveError::SealDigestMismatch {
                declared: self.seal_digest.clone(),
                actual,
            });
        }
        if self.report_id != report.report_id
            || self.gate.decision != report.gate_decision
            || self.gate.lifecycle != report.lifecycle
        {
            return Err(EvaluationArchiveError::SealReportMismatch);
        }
        Ok(())
    }

    /// 计算不包含 `seal_digest` 字段的稳定摘要。
    fn compute_digest(&self) -> Result<ArtifactDigest, EvaluationArchiveError> {
        #[derive(Serialize)]
        struct DigestInput<'a> {
            schema_version: u32,
            report_id: &'a EvaluationReportId,
            report_digest: &'a ArtifactDigest,
            gate: &'a CommitGateOutcome,
            commit_policy_version: &'a str,
            evaluation_policy_version: &'a str,
            verifier_set_digest: &'a str,
            private_artifact_digest: &'a ArtifactDigest,
            audit_record_id: &'a AuditRecordId,
            audit_record_digest: &'a ArtifactDigest,
        }

        digest_bytes(
            &serde_json::to_vec(&DigestInput {
                schema_version: self.schema_version,
                report_id: &self.report_id,
                report_digest: &self.report_digest,
                gate: &self.gate,
                commit_policy_version: &self.commit_policy_version,
                evaluation_policy_version: &self.evaluation_policy_version,
                verifier_set_digest: &self.verifier_set_digest,
                private_artifact_digest: &self.private_artifact_digest,
                audit_record_id: &self.audit_record_id,
                audit_record_digest: &self.audit_record_digest,
            })
            .map_err(EvaluationArchiveError::Serialize)?,
        )
    }
}

/// 已通过 Report 摘要、Seal 与完整 Audit 链三重验证的正式评测。
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedEvaluation {
    report: EvaluationReport,
    seal: EvaluationSeal,
}

impl VerifiedEvaluation {
    /// 返回正式脱敏 EvaluationReport。
    pub fn report(&self) -> &EvaluationReport {
        &self.report
    }

    /// 返回绑定 Gate、Policy、Verifier 与 Audit 的可信 Seal。
    pub fn seal(&self) -> &EvaluationSeal {
        &self.seal
    }
}

/// 只返回完成 Seal 提交的正式评测归档。
#[derive(Debug, Clone)]
pub struct TrustedEvaluationArchive {
    root: PathBuf,
    reports: FileEvaluationReportStore,
    private_artifacts: FileArtifactStore,
    audit: FileAuditLog,
}

impl TrustedEvaluationArchive {
    /// 按受信 Evaluation 数据根创建归档，不触碰文件系统。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            reports: FileEvaluationReportStore::new(root.join("reports")),
            private_artifacts: FileArtifactStore::new(root.join("private-artifacts")),
            audit: FileAuditLog::new(root.join("audit")),
            root,
        }
    }

    /// 返回归档根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回共用的只追加 Audit Log，供同一受信控制面的 Release 事件使用。
    pub fn audit_log(&self) -> &FileAuditLog {
        &self.audit
    }

    /// 提交正式 Report、Audit 记录与最终 Seal。
    ///
    /// Report 与 Audit 允许幂等恢复；只有 Seal 使用 create-new 作为提交点。已存在但内容不同的
    /// Report 或 Seal 会失败关闭，不能用重试覆盖历史。
    ///
    /// # Errors
    ///
    /// 报告、Audit、Seal、路径或 I/O 任一校验失败时返回错误。失败后可能留下不可消费的
    /// 孤立制品，重新提交同一 `TrustedEvaluationReport` 可继续完成。
    pub async fn commit(
        &self,
        trusted: &TrustedEvaluationReport,
        occurred_at_ms: u64,
    ) -> Result<VerifiedEvaluation, EvaluationArchiveError> {
        self.audit.verify().await?;
        let report = trusted.report();
        let report_digest = evaluation_report_digest(report)?;
        let private_artifact = self
            .private_artifacts
            .put(
                "application/vnd.lucia.evaluation-recordings+json",
                trusted.private_artifact_bytes(),
            )
            .await?;
        if private_artifact.digest != *trusted.private_artifact_digest() {
            return Err(EvaluationArchiveError::PrivateArtifactDigestMismatch);
        }

        match self.reports.get(&report.report_id).await? {
            Some(existing) if existing != *report => {
                return Err(EvaluationArchiveError::ReportConflict(
                    report.report_id.clone(),
                ));
            }
            Some(_) => {}
            None => self.reports.append(report).await?,
        }

        let audit_record = match self
            .audit
            .verify_evaluation_report_commit(
                &report.report_id,
                &report.parent.genome_revision,
                &report.candidate.genome_revision,
                report.gate_decision,
                &report_digest,
            )
            .await
        {
            Ok(record) => record,
            Err(AuditStoreError::EvaluationReportCommitNotFound(_)) => {
                self.audit
                    .append(
                        occurred_at_ms,
                        AuditEvent::EvaluationReportCommitted {
                            report_id: report.report_id.clone(),
                            parent: report.parent.genome_revision.clone(),
                            candidate: report.candidate.genome_revision.clone(),
                            decision: report.gate_decision,
                            report_digest: report_digest.clone(),
                        },
                    )
                    .await?
            }
            Err(error) => return Err(error.into()),
        };

        let mut seal = EvaluationSeal {
            schema_version: EVALUATION_SEAL_SCHEMA_VERSION,
            report_id: report.report_id.clone(),
            report_digest,
            gate: trusted.gate().clone(),
            commit_policy_version: trusted.commit_policy_version().to_string(),
            evaluation_policy_version: report.parent.environment.evaluation_policy_version.clone(),
            verifier_set_digest: report.parent.environment.verifier_version.clone(),
            private_artifact_digest: trusted.private_artifact_digest().clone(),
            audit_record_id: audit_record.record_id,
            audit_record_digest: audit_record.digest,
            seal_digest: digest_bytes(&[])?,
        };
        seal.seal_digest = seal.compute_digest()?;
        self.write_seal_create_new_or_same(&seal).await?;
        self.get_verified(&seal.report_id).await
    }

    /// 按报告 ID 读取并验证已完成提交的评测。
    ///
    /// # Errors
    ///
    /// Seal 不存在、Report/Audit/Seal 任一损坏或三者绑定不一致时返回错误。
    pub async fn get_verified(
        &self,
        report_id: &EvaluationReportId,
    ) -> Result<VerifiedEvaluation, EvaluationArchiveError> {
        let seal = self.read_seal(report_id).await?;
        let report = self
            .reports
            .get(report_id)
            .await?
            .ok_or_else(|| EvaluationArchiveError::ReportNotFound(report_id.clone()))?;
        seal.validate(&report)?;
        let actual_digest = evaluation_report_digest(&report)?;
        if actual_digest != seal.report_digest {
            return Err(EvaluationArchiveError::ReportDigestMismatch {
                declared: seal.report_digest,
                actual: actual_digest,
            });
        }
        let audit_record = self
            .audit
            .verify_evaluation_report_commit(
                report_id,
                &report.parent.genome_revision,
                &report.candidate.genome_revision,
                report.gate_decision,
                &actual_digest,
            )
            .await?;
        if audit_record.record_id != seal.audit_record_id
            || audit_record.digest != seal.audit_record_digest
        {
            return Err(EvaluationArchiveError::SealAuditMismatch);
        }
        if self
            .private_artifacts
            .get(&seal.private_artifact_digest)
            .await?
            .is_none()
        {
            return Err(EvaluationArchiveError::PrivateArtifactNotFound(
                seal.private_artifact_digest,
            ));
        }
        Ok(VerifiedEvaluation { report, seal })
    }

    /// 使用 create-new 写 Seal；相同内容视为幂等成功。
    async fn write_seal_create_new_or_same(
        &self,
        seal: &EvaluationSeal,
    ) -> Result<(), EvaluationArchiveError> {
        let seals = self.root.join("seals");
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&seals).await?;
        let path = seals.join(format!("{}.json", seal.report_id));
        let bytes = serde_json::to_vec_pretty(seal).map_err(EvaluationArchiveError::Serialize)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                file.write_all(&bytes)
                    .await
                    .map_err(|source| io_error("写入 Evaluation Seal", &path, source))?;
                file.sync_all()
                    .await
                    .map_err(|source| io_error("同步 Evaluation Seal", &path, source))
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = self.read_seal(&seal.report_id).await?;
                if existing == *seal {
                    Ok(())
                } else {
                    Err(EvaluationArchiveError::SealConflict(seal.report_id.clone()))
                }
            }
            Err(source) => Err(io_error("创建 Evaluation Seal", &path, source)),
        }
    }

    /// 读取并解析单个非符号链接 Seal 文件。
    async fn read_seal(
        &self,
        report_id: &EvaluationReportId,
    ) -> Result<EvaluationSeal, EvaluationArchiveError> {
        let path = self.root.join("seals").join(format!("{report_id}.json"));
        let metadata = fs::symlink_metadata(&path).await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                EvaluationArchiveError::SealNotFound(report_id.clone())
            } else {
                io_error("检查 Evaluation Seal", &path, source)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(EvaluationArchiveError::UnsafePath(path));
        }
        let bytes = fs::read(&path)
            .await
            .map_err(|source| io_error("读取 Evaluation Seal", &path, source))?;
        serde_json::from_slice(&bytes)
            .map_err(|source| EvaluationArchiveError::InvalidJson { path, source })
    }
}

/// Evaluation Archive 提交、Seal 与验证错误。
#[derive(Debug, thiserror::Error)]
pub enum EvaluationArchiveError {
    /// Seal schema 超出当前实现。
    #[error("不支持的 Evaluation Seal schema：{0}")]
    UnsupportedSealSchema(u32),
    /// Seal 自身摘要不匹配。
    #[error("Evaluation Seal 摘要不匹配：声明 {declared}，实际 {actual}")]
    SealDigestMismatch {
        /// Seal 声明摘要。
        declared: ArtifactDigest,
        /// 重新计算摘要。
        actual: ArtifactDigest,
    },
    /// Seal 与报告的身份、Gate 或生命周期不一致。
    #[error("Evaluation Seal 与正式报告不一致")]
    SealReportMismatch,
    /// Seal 与 Audit 提交记录不一致。
    #[error("Evaluation Seal 与 Audit 提交记录不一致")]
    SealAuditMismatch,
    /// 同 ID 报告已存在但内容不同。
    #[error("EvaluationReport ID 已被不同内容占用：{0}")]
    ReportConflict(EvaluationReportId),
    /// 同 ID Seal 已存在但内容不同。
    #[error("Evaluation Seal ID 已被不同内容占用：{0}")]
    SealConflict(EvaluationReportId),
    /// Seal 已提交但正式报告不存在。
    #[error("EvaluationReport 不存在：{0}")]
    ReportNotFound(EvaluationReportId),
    /// 目标 Seal 尚未提交。
    #[error("Evaluation Seal 不存在：{0}")]
    SealNotFound(EvaluationReportId),
    /// 正式报告摘要与 Seal 不一致。
    #[error("EvaluationReport 摘要不匹配：声明 {declared}，实际 {actual}")]
    ReportDigestMismatch {
        /// Seal 声明摘要。
        declared: ArtifactDigest,
        /// 重新计算摘要。
        actual: ArtifactDigest,
    },
    /// 私有录制 CAS 返回的摘要与 Builder 绑定不一致。
    #[error("Evaluator 私有录制制品摘要不一致")]
    PrivateArtifactDigestMismatch,
    /// Seal 绑定的私有录制制品不存在。
    #[error("Evaluator 私有录制制品不存在：{0}")]
    PrivateArtifactNotFound(ArtifactDigest),
    /// 路径是符号链接或不是预期类型。
    #[error("Evaluation Archive 路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// Seal JSON 损坏。
    #[error("Evaluation Seal JSON 损坏 `{path}`：{source}")]
    InvalidJson {
        /// 损坏文件路径。
        path: PathBuf,
        /// JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// JSON 序列化失败。
    #[error("序列化 Evaluation Archive 制品失败：{0}")]
    Serialize(serde_json::Error),
    /// SHA-256 强类型摘要构造失败。
    #[error("构造 Evaluation Archive 摘要失败：{0}")]
    InvalidDigest(String),
    /// 文件系统操作失败。
    #[error("{operation}失败 `{path}`：{source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 普通不可变 Report Store 错误。
    #[error(transparent)]
    ReportStore(#[from] EvaluationStoreError),
    /// Evaluator 私有 CAS 错误。
    #[error(transparent)]
    PrivateArtifact(#[from] ArtifactStoreError),
    /// Audit 链错误。
    #[error(transparent)]
    Audit(#[from] AuditStoreError),
    /// 报告构建或摘要错误。
    #[error(transparent)]
    ReportBuild(#[from] ReportBuildError),
}

/// 创建并验证非符号链接目录。
async fn ensure_safe_directory(path: &Path) -> Result<(), EvaluationArchiveError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 Evaluation Archive 目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 Evaluation Archive 目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EvaluationArchiveError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

/// 计算协议格式的 SHA-256 摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, EvaluationArchiveError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| EvaluationArchiveError::InvalidDigest(error.to_string()))
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> EvaluationArchiveError {
    EvaluationArchiveError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}
