//! Trusted Evaluation 与 Release Controller 共用的只追加审计哈希链。
//!
//! 每个序号只能通过 `create_new` 提交一次，记录摘要覆盖前序摘要、事件和时间。读取或追加前
//! 都会重放完整链；损坏、缺口、分叉或符号链接会失败关闭。

use agent_evolution_protocol::{
    ArtifactDigest, AuditRecordId, EvaluationReportId, GateDecision, GenomeRevisionId, ReleaseId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{
    fs,
    io::{AsyncWriteExt, BufWriter},
};

/// 当前审计记录 schema 版本。
pub const AUDIT_RECORD_SCHEMA_VERSION: u32 = 1;

/// 只追加审计链支持的可信控制面事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    /// 正式 EvaluationReport 已写入不可变 Store。
    EvaluationReportCommitted {
        /// 报告标识。
        report_id: EvaluationReportId,
        /// Parent Genome 修订。
        parent: GenomeRevisionId,
        /// Candidate Genome 修订。
        candidate: GenomeRevisionId,
        /// 受信 Commit Gate 决策。
        decision: GateDecision,
        /// 不可变报告 JSON 的 SHA-256。
        report_digest: ArtifactDigest,
    },
    /// Release Controller 已原子切换 Stable 引用。
    PromotionCommitted {
        /// 发布标识。
        release_id: ReleaseId,
        /// 绑定的 EvaluationReport。
        report_id: EvaluationReportId,
        /// Stable lineage。
        lineage: String,
        /// 切换前 Genome 修订。
        parent: GenomeRevisionId,
        /// 切换后 Genome 修订。
        candidate: GenomeRevisionId,
        /// 切换后的 lineage 代数。
        generation: u64,
    },
    /// Rollback Controller 已把 Stable 引用恢复到先前修订。
    RollbackCommitted {
        /// 本次 Rollback 自身的发布标识。
        rollback_release_id: ReleaseId,
        /// 被回滚的发布标识。
        release_id: ReleaseId,
        /// 回滚所依据的正式 EvaluationReport。
        report_id: EvaluationReportId,
        /// Stable lineage。
        lineage: String,
        /// 回滚前的 Genome 修订。
        from: GenomeRevisionId,
        /// 回滚后的 Genome 修订。
        to: GenomeRevisionId,
        /// 回滚提交后的 lineage 代数。
        generation: u64,
    },
}

/// 一条带前序摘要的不可变审计记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// 记录 schema 版本。
    pub schema_version: u32,
    /// 审计记录强类型标识。
    pub record_id: AuditRecordId,
    /// 从 0 开始连续递增的全局序号。
    pub sequence: u64,
    /// 前一条记录摘要；首条记录为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_digest: Option<ArtifactDigest>,
    /// 由可信控制面提供的 Unix 毫秒时间。
    pub occurred_at_ms: u64,
    /// 受信控制面事件。
    pub event: AuditEvent,
    /// 当前记录除本字段外全部内容的 SHA-256。
    pub digest: ArtifactDigest,
}

impl AuditRecord {
    /// 构造并摘要绑定一条新记录。
    fn new(
        sequence: u64,
        previous_digest: Option<ArtifactDigest>,
        occurred_at_ms: u64,
        event: AuditEvent,
    ) -> Result<Self, AuditStoreError> {
        let mut record = Self {
            schema_version: AUDIT_RECORD_SCHEMA_VERSION,
            record_id: AuditRecordId::generate(),
            sequence,
            previous_digest,
            occurred_at_ms,
            event,
            digest: empty_digest()?,
        };
        record.digest = record.compute_digest()?;
        Ok(record)
    }

    /// 重新计算记录摘要并校验 schema。
    fn validate(&self) -> Result<(), AuditStoreError> {
        if self.schema_version != AUDIT_RECORD_SCHEMA_VERSION {
            return Err(AuditStoreError::UnsupportedSchema(self.schema_version));
        }
        let actual = self.compute_digest()?;
        if actual != self.digest {
            return Err(AuditStoreError::DigestMismatch {
                sequence: self.sequence,
                declared: self.digest.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// 计算不包含 `digest` 字段的规范结构摘要。
    fn compute_digest(&self) -> Result<ArtifactDigest, AuditStoreError> {
        #[derive(Serialize)]
        struct DigestInput<'a> {
            schema_version: u32,
            record_id: &'a AuditRecordId,
            sequence: u64,
            previous_digest: &'a Option<ArtifactDigest>,
            occurred_at_ms: u64,
            event: &'a AuditEvent,
        }

        digest_bytes(
            &serde_json::to_vec(&DigestInput {
                schema_version: self.schema_version,
                record_id: &self.record_id,
                sequence: self.sequence,
                previous_digest: &self.previous_digest,
                occurred_at_ms: self.occurred_at_ms,
                event: &self.event,
            })
            .map_err(AuditStoreError::Serialize)?,
        )
    }
}

/// 完整重放审计链后得到的不可伪造验证凭据。
///
/// 字段保持私有，只有 [`FileAuditLog::verify`] 能构造。EvaluationReport Builder 接收该类型
/// 而不是调用方自报布尔值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditVerification {
    next_sequence: u64,
    previous_digest: Option<ArtifactDigest>,
}

impl AuditVerification {
    /// 返回当前链包含的记录数，也是下一条记录的序号。
    pub fn record_count(&self) -> u64 {
        self.next_sequence
    }

    /// 返回当前链尾摘要；空链为 `None`。
    pub fn head_digest(&self) -> Option<&ArtifactDigest> {
        self.previous_digest.as_ref()
    }
}

/// 本地只追加审计哈希链。
#[derive(Debug, Clone)]
pub struct FileAuditLog {
    root: PathBuf,
}

impl FileAuditLog {
    /// 创建尚未触碰文件系统的审计 Store。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回审计 Store 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 重放并验证完整哈希链。
    ///
    /// # Errors
    ///
    /// 根目录或记录是符号链接、文件名/JSON/schema/序号/前序摘要/自身摘要不合法，或发生
    /// I/O 错误时返回错误。空目录是合法的已验证链。
    pub async fn verify(&self) -> Result<AuditVerification, AuditStoreError> {
        ensure_safe_directory(&self.root).await?;
        let records_root = self.root.join("records");
        ensure_safe_directory(&records_root).await?;
        let mut entries = fs::read_dir(&records_root)
            .await
            .map_err(|source| io_error("遍历审计目录", &records_root, source))?;
        let mut paths = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| io_error("读取审计目录项", &records_root, source))?
        {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(|source| io_error("检查审计记录", &path, source))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(AuditStoreError::UnsafePath {
                    path,
                    reason: "审计目录只允许非符号链接普通文件",
                });
            }
            paths.push(entry.path());
        }
        paths.sort();

        let mut expected_sequence = 0_u64;
        let mut previous_digest = None;
        for path in paths {
            let file_sequence = parse_record_name(&path)?;
            if file_sequence != expected_sequence {
                return Err(AuditStoreError::SequenceGap {
                    expected: expected_sequence,
                    actual: file_sequence,
                });
            }
            let bytes = fs::read(&path)
                .await
                .map_err(|source| io_error("读取审计记录", &path, source))?;
            let record: AuditRecord = serde_json::from_slice(&bytes)
                .map_err(|source| AuditStoreError::InvalidJson { path, source })?;
            if record.sequence != file_sequence {
                return Err(AuditStoreError::FileSequenceMismatch(file_sequence));
            }
            if record.previous_digest != previous_digest {
                return Err(AuditStoreError::PreviousDigestMismatch(record.sequence));
            }
            record.validate()?;
            previous_digest = Some(record.digest);
            expected_sequence += 1;
        }
        Ok(AuditVerification {
            next_sequence: expected_sequence,
            previous_digest,
        })
    }

    /// 在验证后的链尾追加一条记录。
    ///
    /// 每个序号使用固定文件名和 `create_new` 语义。并发写者只有一个能提交，失败者必须
    /// 重新验证链后重试，不能覆盖或形成分叉。
    ///
    /// # Errors
    ///
    /// 现有链损坏、序号被并发占用、序列化或文件系统操作失败时返回错误。
    pub async fn append(
        &self,
        occurred_at_ms: u64,
        event: AuditEvent,
    ) -> Result<AuditRecord, AuditStoreError> {
        let verification = self.verify().await?;
        let record = AuditRecord::new(
            verification.next_sequence,
            verification.previous_digest,
            occurred_at_ms,
            event,
        )?;
        let path = self.root.join("records").join(record_name(record.sequence));
        let bytes = serde_json::to_vec_pretty(&record).map_err(AuditStoreError::Serialize)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    AuditStoreError::ConcurrentAppend(record.sequence)
                } else {
                    io_error("创建审计记录", &path, source)
                }
            })?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(&bytes)
            .await
            .map_err(|source| io_error("写入审计记录", &path, source))?;
        writer
            .flush()
            .await
            .map_err(|source| io_error("刷新审计记录", &path, source))?;
        writer
            .into_inner()
            .sync_all()
            .await
            .map_err(|source| io_error("同步审计记录", &path, source))?;
        Ok(record)
    }

    /// 读取并验证全部审计记录。
    ///
    /// # Errors
    ///
    /// 哈希链或任一记录损坏、路径不安全或读取失败时返回错误。
    pub async fn records(&self) -> Result<Vec<AuditRecord>, AuditStoreError> {
        let verification = self.verify().await?;
        let mut records = Vec::with_capacity(verification.record_count() as usize);
        for sequence in 0..verification.record_count() {
            let path = self.root.join("records").join(record_name(sequence));
            let bytes = fs::read(&path)
                .await
                .map_err(|source| io_error("读取审计记录", &path, source))?;
            let record = serde_json::from_slice(&bytes)
                .map_err(|source| AuditStoreError::InvalidJson { path, source })?;
            records.push(record);
        }
        Ok(records)
    }

    /// 验证指定 EvaluationReport 提交事件存在于完整哈希链中。
    ///
    /// # Errors
    ///
    /// 链损坏，或没有与报告身份、摘要、Parent/Candidate 和 Gate 决策完全一致的事件时
    /// 返回错误。该验证是 Release Controller 消费报告前的强制前置条件。
    pub async fn verify_evaluation_report_commit(
        &self,
        report_id: &EvaluationReportId,
        parent: &GenomeRevisionId,
        candidate: &GenomeRevisionId,
        decision: GateDecision,
        report_digest: &ArtifactDigest,
    ) -> Result<AuditRecord, AuditStoreError> {
        self.records()
            .await?
            .into_iter()
            .find(|record| {
                matches!(
                    &record.event,
                    AuditEvent::EvaluationReportCommitted {
                        report_id: actual_report,
                        parent: actual_parent,
                        candidate: actual_candidate,
                        decision: actual_decision,
                        report_digest: actual_digest,
                    } if actual_report == report_id
                        && actual_parent == parent
                        && actual_candidate == candidate
                        && *actual_decision == decision
                        && actual_digest == report_digest
                )
            })
            .ok_or_else(|| AuditStoreError::EvaluationReportCommitNotFound(report_id.clone()))
    }
}

/// 审计 Store 结构、并发和 I/O 错误。
#[derive(Debug, thiserror::Error)]
pub enum AuditStoreError {
    /// 记录 schema 超出当前实现。
    #[error("不支持的审计记录 schema 版本：{0}")]
    UnsupportedSchema(u32),
    /// 记录自身摘要不匹配。
    #[error("审计记录 {sequence} 摘要不匹配：声明 {declared}，实际 {actual}")]
    DigestMismatch {
        /// 损坏记录序号。
        sequence: u64,
        /// 记录声明的摘要。
        declared: ArtifactDigest,
        /// 重新计算的摘要。
        actual: ArtifactDigest,
    },
    /// 固定序号文件出现缺口或额外记录。
    #[error("审计序号不连续：期望 {expected}，实际 {actual}")]
    SequenceGap {
        /// 期望序号。
        expected: u64,
        /// 文件名中的实际序号。
        actual: u64,
    },
    /// 文件名序号与记录正文不一致。
    #[error("审计文件名与正文序号不一致：{0}")]
    FileSequenceMismatch(u64),
    /// 记录未指向前一条真实摘要。
    #[error("审计记录 {0} 的前序摘要不匹配")]
    PreviousDigestMismatch(u64),
    /// 同一序号已被并发写者占用。
    #[error("审计序号 {0} 已被并发写者提交，请重新验证后重试")]
    ConcurrentAppend(u64),
    /// 完整链中不存在与目标正式报告完全绑定的提交事件。
    #[error("审计链中不存在 EvaluationReport 提交记录：{0}")]
    EvaluationReportCommitNotFound(EvaluationReportId),
    /// 文件名不是固定二十位序号。
    #[error("审计记录文件名不合法：{0}")]
    InvalidFileName(PathBuf),
    /// 审计路径包含符号链接或非预期文件类型。
    #[error("审计路径不安全 `{path}`：{reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 固定错误原因。
        reason: &'static str,
    },
    /// JSON 无法解析。
    #[error("审计记录 JSON 损坏 `{path}`：{source}")]
    InvalidJson {
        /// 损坏文件。
        path: PathBuf,
        /// JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// JSON 无法序列化。
    #[error("序列化审计记录失败：{0}")]
    Serialize(serde_json::Error),
    /// 文件系统操作失败。
    #[error("{operation}失败 `{path}`：{source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 失败路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// SHA-256 强类型摘要构造失败。
    #[error("构造审计摘要失败：{0}")]
    InvalidDigest(String),
}

/// 创建并验证非符号链接普通目录。
async fn ensure_safe_directory(path: &Path) -> Result<(), AuditStoreError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建审计目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查审计目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AuditStoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "审计根必须是非符号链接普通目录",
        });
    }
    Ok(())
}

/// 返回按字典序与数值序一致的固定记录文件名。
fn record_name(sequence: u64) -> String {
    format!("{sequence:020}.json")
}

/// 从固定记录文件名解析序号。
fn parse_record_name(path: &Path) -> Result<u64, AuditStoreError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(AuditStoreError::InvalidFileName(path.to_path_buf()));
    };
    let Some(number) = name.strip_suffix(".json") else {
        return Err(AuditStoreError::InvalidFileName(path.to_path_buf()));
    };
    if number.len() != 20 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AuditStoreError::InvalidFileName(path.to_path_buf()));
    }
    number
        .parse()
        .map_err(|_| AuditStoreError::InvalidFileName(path.to_path_buf()))
}

/// 计算协议格式的 SHA-256 摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, AuditStoreError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| AuditStoreError::InvalidDigest(error.to_string()))
}

/// 构造仅在初始化结构时使用的确定性空摘要。
fn empty_digest() -> Result<ArtifactDigest, AuditStoreError> {
    digest_bytes(&[])
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> AuditStoreError {
    AuditStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::EvaluationReportId;
    use tempfile::TempDir;

    /// 构造一条测试评测提交事件。
    fn event(seed: char) -> AuditEvent {
        AuditEvent::EvaluationReportCommitted {
            report_id: EvaluationReportId::generate(),
            parent: GenomeRevisionId::generate(),
            candidate: GenomeRevisionId::generate(),
            decision: GateDecision::Pass,
            report_digest: ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64))
                .expect("测试摘要合法"),
        }
    }

    /// 多条记录必须形成连续、可完整重放的摘要链。
    #[tokio::test]
    async fn appends_and_verifies_hash_chain() {
        let root = TempDir::new().expect("创建审计根");
        let log = FileAuditLog::new(root.path());
        let first = log.append(1, event('1')).await.expect("追加首条记录");
        let second = log.append(2, event('2')).await.expect("追加第二条记录");

        assert_eq!(first.sequence, 0);
        assert_eq!(second.previous_digest, Some(first.digest));
        let verified = log.verify().await.expect("验证完整审计链");
        assert_eq!(verified.record_count(), 2);
        assert_eq!(verified.head_digest(), Some(&second.digest));
    }

    /// 历史记录正文被改写后必须在摘要校验处失败关闭。
    #[tokio::test]
    async fn rejects_tampered_history() {
        let root = TempDir::new().expect("创建审计根");
        let log = FileAuditLog::new(root.path());
        log.append(1, event('1')).await.expect("追加首条记录");
        let path = root.path().join("records").join(record_name(0));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).await.expect("读取测试记录"))
                .expect("解析测试记录");
        value["occurred_at_ms"] = serde_json::json!(999);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("序列化篡改记录"),
        )
        .await
        .expect("写入篡改记录");

        assert!(matches!(
            log.verify().await,
            Err(AuditStoreError::DigestMismatch { .. })
        ));
    }
}
