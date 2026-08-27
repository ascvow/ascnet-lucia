//! 绑定正式 EvaluationReport 的 Promotion 与原子 Rollback 控制器。
//!
//! 控制器持有独立文件锁，重载并验证 Report/Seal/Audit/Genome/Stable expected-current 后才
//! 原子替换 Stable 引用。Stable 指针本身保存 Report 与 Release 绑定，因此提交后不会退化为
//! “只看 Gate 布尔值”的裸 Revision 指针。

use crate::{
    AuditEvent, AuditRecord, AuditStoreError, EvaluationArchiveError, TrustedEvaluationArchive,
};
use agent_evolution::{
    diff_genomes, FileGenomeResolver, FileStableGenomePublisher, GenomeDiffError,
    GenomePromotionError, GenomeResolver, GenomeResolverError, GenomeSelector, StableGenomeRef,
};
use agent_evolution_protocol::{
    AuditRecordId, EvaluationReportId, EvolutionLifecycle, GateDecision, GenomeRevisionId,
    ReleaseId,
};
use serde::Serialize;
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

/// Promotion 或 Rollback 成功后的脱敏回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseReceipt {
    /// 本次 Stable 切换的 Release 标识。
    pub release_id: ReleaseId,
    /// Promotion 报告；Rollback 使用被撤销发布原先的报告。
    pub report_id: EvaluationReportId,
    /// Stable lineage。
    pub lineage: String,
    /// 切换前 Revision。
    pub from: GenomeRevisionId,
    /// 切换后 Revision。
    pub to: GenomeRevisionId,
    /// 切换后的单调代数。
    pub generation: u64,
    /// 记录最终控制面事件的 Audit ID。
    pub audit_record_id: AuditRecordId,
    /// Rollback 时为被撤销的 Promotion Release。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<ReleaseId>,
}

/// 只能由受信 `lucia-eval` 进程装配的 Release Controller。
#[derive(Debug, Clone)]
pub struct ReleaseController {
    publisher: FileStableGenomePublisher,
    archive: TrustedEvaluationArchive,
    lock_root: PathBuf,
}

impl ReleaseController {
    /// 使用共享 Evolution Registry 与可信 Evaluation Archive 创建控制器。
    pub fn new(evolution_root: impl Into<PathBuf>, archive_root: impl Into<PathBuf>) -> Self {
        let evolution_root = evolution_root.into();
        Self {
            publisher: FileStableGenomePublisher::new(&evolution_root),
            archive: TrustedEvaluationArchive::new(archive_root),
            lock_root: evolution_root.join("release-control"),
        }
    }

    /// 把 Gate Pass 的 Candidate 原子晋升为 Stable。
    ///
    /// `release_id` 由受信编排器生成并用于幂等恢复。同一 Release 已切换 Stable 但 Audit
    /// 尚未追加时，重试会只补齐 Audit，不重复递增代数。
    ///
    /// # Errors
    ///
    /// Report/Seal/Audit 不完整、Gate 未通过、Genome Diff 不一致、Candidate 构建不可发布、
    /// Stable expected-current 不匹配或原子写入失败时返回错误。
    pub async fn promote(
        &self,
        report_id: &EvaluationReportId,
        release_id: ReleaseId,
        occurred_at_ms: u64,
    ) -> Result<ReleaseReceipt, ReleaseError> {
        let _lock = ReleaseFileLock::acquire(&self.lock_root).await?;
        let verified = self.archive.get_verified(report_id).await?;
        validate_eligible_evaluation(&verified)?;
        let report = verified.report();
        let lineage = report
            .lineage
            .as_deref()
            .ok_or(ReleaseError::MissingLineage)?;
        let parent_generation = report
            .parent_generation
            .ok_or(ReleaseError::MissingLineage)?;
        let candidate_generation = report
            .candidate_generation
            .ok_or(ReleaseError::MissingLineage)?;
        let resolver = self.publisher.resolver();
        let parent = resolve_revision(resolver, &report.parent.genome_revision).await?;
        let candidate = resolve_revision(resolver, &report.candidate.genome_revision).await?;
        if !candidate.genome.runtime.is_promotable() {
            return Err(ReleaseError::DirtyRuntime);
        }
        let actual_diff = diff_genomes(&parent, &candidate)?;
        if actual_diff != report.genome_diff {
            return Err(ReleaseError::GenomeDiffMismatch);
        }

        let current = resolver.stable_reference(lineage).await?;
        if current.revision_id == candidate.revision_id
            && current.generation == candidate_generation
            && current.release_id.as_ref() == Some(&release_id)
            && current.evaluation_report_id.as_ref() == Some(report_id)
        {
            let audit = self
                .ensure_promotion_audit(
                    release_id.clone(),
                    report_id.clone(),
                    lineage,
                    &parent.revision_id,
                    &candidate.revision_id,
                    candidate_generation,
                    occurred_at_ms,
                )
                .await?;
            return Ok(release_receipt(&current, audit, None));
        }
        if current.revision_id != parent.revision_id
            || current.digest != parent.digest
            || current.generation != parent_generation
        {
            return Err(ReleaseError::ExpectedCurrentMismatch);
        }

        let stable = self
            .publisher
            .publish_bound(
                &current,
                &candidate,
                candidate_generation,
                release_id.clone(),
                report_id.clone(),
                None,
            )
            .await?;
        let audit = self
            .ensure_promotion_audit(
                release_id,
                report_id.clone(),
                lineage,
                &parent.revision_id,
                &candidate.revision_id,
                candidate_generation,
                occurred_at_ms,
            )
            .await?;
        Ok(release_receipt(&stable, audit, None))
    }

    /// 把指定已发布 Candidate 原子回滚到其报告中的 Parent Revision。
    ///
    /// 回滚代数继续单调递增；Revision 回到 Parent。`rollback_release_id` 支持 Stable 已切换、
    /// Audit 尚未落盘时的幂等恢复。
    ///
    /// # Errors
    ///
    /// 当前 Stable 不是目标发布、原报告不可信、Parent 不存在、代数溢出或原子替换失败时
    /// 返回错误。
    pub async fn rollback(
        &self,
        release_id: &ReleaseId,
        rollback_release_id: ReleaseId,
        occurred_at_ms: u64,
    ) -> Result<ReleaseReceipt, ReleaseError> {
        let _lock = ReleaseFileLock::acquire(&self.lock_root).await?;
        let promotion = self.find_promotion(release_id).await?;
        let report_id = promotion.report_id.clone();
        let verified = self.archive.get_verified(&report_id).await?;
        validate_eligible_evaluation(&verified)?;
        let report = verified.report();
        let lineage = report
            .lineage
            .as_deref()
            .ok_or(ReleaseError::MissingLineage)?;
        let resolver = self.publisher.resolver();
        let observed = resolver.stable_reference(lineage).await?;
        if lineage != promotion.lineage {
            return Err(ReleaseError::ExpectedCurrentMismatch);
        }
        let target_generation = promotion
            .generation
            .checked_add(1)
            .ok_or(ReleaseError::GenerationOverflow)?;
        let parent = resolve_revision(resolver, &report.parent.genome_revision).await?;

        if observed.revision_id == parent.revision_id
            && observed.generation == target_generation
            && observed.release_id.as_ref() == Some(&rollback_release_id)
            && observed.rollback_of.as_ref() == Some(release_id)
        {
            let audit = self
                .ensure_rollback_audit(
                    rollback_release_id.clone(),
                    release_id.clone(),
                    report_id.clone(),
                    lineage,
                    &promotion.candidate,
                    &parent.revision_id,
                    target_generation,
                    occurred_at_ms,
                )
                .await?;
            return Ok(release_receipt(&observed, audit, Some(release_id.clone())));
        }
        if observed.release_id.as_ref() != Some(release_id)
            || observed.revision_id != promotion.candidate
            || observed.revision_id != report.candidate.genome_revision
            || observed.generation != promotion.generation
        {
            return Err(ReleaseError::ExpectedCurrentMismatch);
        }

        let stable = self
            .publisher
            .publish_bound(
                &observed,
                &parent,
                target_generation,
                rollback_release_id.clone(),
                report_id.clone(),
                Some(release_id.clone()),
            )
            .await?;
        let audit = self
            .ensure_rollback_audit(
                rollback_release_id,
                release_id.clone(),
                report_id,
                lineage,
                &promotion.candidate,
                &parent.revision_id,
                target_generation,
                occurred_at_ms,
            )
            .await?;
        Ok(release_receipt(&stable, audit, Some(release_id.clone())))
    }

    /// 在 Audit 中查找指定 Promotion，缺失时追加一次。
    #[allow(clippy::too_many_arguments)]
    async fn ensure_promotion_audit(
        &self,
        release_id: ReleaseId,
        report_id: EvaluationReportId,
        lineage: &str,
        parent: &GenomeRevisionId,
        candidate: &GenomeRevisionId,
        generation: u64,
        occurred_at_ms: u64,
    ) -> Result<AuditRecord, ReleaseError> {
        if let Some(record) = self
            .archive
            .audit_log()
            .records()
            .await?
            .into_iter()
            .find(|record| {
                matches!(
                    &record.event,
                    AuditEvent::PromotionCommitted {
                        release_id: actual_release,
                        report_id: actual_report,
                        lineage: actual_lineage,
                        parent: actual_parent,
                        candidate: actual_candidate,
                        generation: actual_generation,
                    } if actual_release == &release_id
                        && actual_report == &report_id
                        && actual_lineage == lineage
                        && actual_parent == parent
                        && actual_candidate == candidate
                        && *actual_generation == generation
                )
            })
        {
            return Ok(record);
        }
        self.archive
            .audit_log()
            .append(
                occurred_at_ms,
                AuditEvent::PromotionCommitted {
                    release_id,
                    report_id,
                    lineage: lineage.to_string(),
                    parent: parent.clone(),
                    candidate: candidate.clone(),
                    generation,
                },
            )
            .await
            .map_err(ReleaseError::from)
    }

    /// 在 Audit 中查找指定 Rollback，缺失时追加一次。
    #[allow(clippy::too_many_arguments)]
    async fn ensure_rollback_audit(
        &self,
        rollback_release_id: ReleaseId,
        release_id: ReleaseId,
        report_id: EvaluationReportId,
        lineage: &str,
        from: &GenomeRevisionId,
        to: &GenomeRevisionId,
        generation: u64,
        occurred_at_ms: u64,
    ) -> Result<AuditRecord, ReleaseError> {
        if let Some(record) = self
            .archive
            .audit_log()
            .records()
            .await?
            .into_iter()
            .find(|record| {
                matches!(
                    &record.event,
                    AuditEvent::RollbackCommitted {
                        rollback_release_id: actual_rollback,
                        release_id: actual_release,
                        report_id: actual_report,
                        lineage: actual_lineage,
                        from: actual_from,
                        to: actual_to,
                        generation: actual_generation,
                    } if actual_rollback == &rollback_release_id
                        && actual_release == &release_id
                        && actual_report == &report_id
                        && actual_lineage == lineage
                        && actual_from == from
                        && actual_to == to
                        && *actual_generation == generation
                )
            })
        {
            return Ok(record);
        }
        self.archive
            .audit_log()
            .append(
                occurred_at_ms,
                AuditEvent::RollbackCommitted {
                    rollback_release_id,
                    release_id,
                    report_id,
                    lineage: lineage.to_string(),
                    from: from.clone(),
                    to: to.clone(),
                    generation,
                },
            )
            .await
            .map_err(ReleaseError::from)
    }

    /// 从 Promotion Audit 解析指定 Release 的可信绑定。
    async fn find_promotion(
        &self,
        release_id: &ReleaseId,
    ) -> Result<PromotionBinding, ReleaseError> {
        let record = self
            .archive
            .audit_log()
            .records()
            .await?
            .into_iter()
            .rev()
            .find(|record| {
                matches!(
                    &record.event,
                    AuditEvent::PromotionCommitted { release_id: actual, .. }
                        if actual == release_id
                )
            })
            .ok_or_else(|| ReleaseError::ReleaseNotFound(release_id.clone()))?;
        let AuditEvent::PromotionCommitted {
            report_id,
            lineage,
            candidate,
            generation,
            ..
        } = record.event
        else {
            unreachable!("匹配分支保证事件类型")
        };
        Ok(PromotionBinding {
            report_id,
            lineage,
            candidate,
            generation,
        })
    }
}

/// Promotion Audit 中供 Rollback expected-current 使用的最小绑定。
struct PromotionBinding {
    report_id: EvaluationReportId,
    lineage: String,
    candidate: GenomeRevisionId,
    generation: u64,
}

/// 校验 Release 需要的全部可信 Gate 与报告保证。
fn validate_eligible_evaluation(verified: &crate::VerifiedEvaluation) -> Result<(), ReleaseError> {
    let report = verified.report();
    let gate = &verified.seal().gate;
    if report.gate_decision != GateDecision::Pass
        || report.lifecycle != EvolutionLifecycle::Eligible
        || gate.decision != GateDecision::Pass
        || gate.lifecycle != EvolutionLifecycle::Eligible
        || !gate.hard_failures.is_empty()
        || !gate.inconclusive_reasons.is_empty()
        || !gate.behavior_failures.is_empty()
    {
        return Err(ReleaseError::GateNotEligible);
    }
    if report.artifact_integrity_verified != Some(true)
        || report.hidden_dataset_isolated != Some(true)
    {
        return Err(ReleaseError::IntegrityNotVerified);
    }
    Ok(())
}

/// 解析并校验精确 Genome Revision。
async fn resolve_revision(
    resolver: &FileGenomeResolver,
    revision_id: &GenomeRevisionId,
) -> Result<agent_evolution_protocol::GenomeRevision, ReleaseError> {
    resolver
        .resolve(&GenomeSelector::Revision(revision_id.clone()))
        .await
        .map_err(ReleaseError::from)
}

/// 从最终 Stable 引用与 Audit 记录构造回执。
fn release_receipt(
    stable: &StableGenomeRef,
    audit: AuditRecord,
    rollback_of: Option<ReleaseId>,
) -> ReleaseReceipt {
    ReleaseReceipt {
        release_id: stable.release_id.clone().expect("受信发布必须绑定 Release"),
        report_id: stable
            .evaluation_report_id
            .clone()
            .expect("受信发布必须绑定 Report"),
        lineage: stable.lineage.clone(),
        from: stable
            .previous_revision_id
            .clone()
            .expect("受信发布必须绑定前序 Revision"),
        to: stable.revision_id.clone(),
        generation: stable.generation,
        audit_record_id: audit.record_id,
        rollback_of,
    }
}

/// 跨进程串行化同一 Evolution 根下的 Release 操作。
struct ReleaseFileLock {
    file: std::fs::File,
}

impl ReleaseFileLock {
    /// 使用 `flock` 获取排他锁，拒绝符号链接锁文件。
    async fn acquire(root: &Path) -> Result<Self, ReleaseError> {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || acquire_blocking_lock(&root))
            .await
            .map_err(|_| ReleaseError::LockTaskFailed)?
    }
}

#[cfg(unix)]
impl Drop for ReleaseFileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Drop 路径不能传播错误；进程关闭文件描述符时也会释放锁。
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// 同步创建安全锁文件并阻塞等待排他锁。
#[cfg(unix)]
fn acquire_blocking_lock(root: &Path) -> Result<ReleaseFileLock, ReleaseError> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

    std::fs::create_dir_all(root).map_err(|source| lock_io_error(root, source))?;
    let metadata = std::fs::symlink_metadata(root).map_err(|source| lock_io_error(root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ReleaseError::UnsafeLockPath(root.to_path_buf()));
    }
    let path = root.join("release.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|source| lock_io_error(&path, source))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(lock_io_error(&path, std::io::Error::last_os_error()));
    }
    Ok(ReleaseFileLock { file })
}

/// 非 Unix 平台当前不提供与本地文件发布语义等价的进程锁。
#[cfg(not(unix))]
fn acquire_blocking_lock(_root: &Path) -> Result<ReleaseFileLock, ReleaseError> {
    Err(ReleaseError::UnsupportedPlatform)
}

/// Release Controller 错误。
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// 报告缺少完整 lineage 与代数。
    #[error("EvaluationReport 缺少完整 lineage 与代数")]
    MissingLineage,
    /// Gate 或生命周期不允许 Promotion。
    #[error("EvaluationReport 未通过可信 Commit Gate")]
    GateNotEligible,
    /// Artifact 或 Hidden 隔离保证缺失。
    #[error("EvaluationReport 完整性保证缺失")]
    IntegrityNotVerified,
    /// Candidate 使用 dirty Runtime 构建。
    #[error("dirty Runtime Genome 禁止自动 Promotion")]
    DirtyRuntime,
    /// 重算的 Genome Diff 与正式报告不一致。
    #[error("正式报告的 Genome Diff 与 Registry 不一致")]
    GenomeDiffMismatch,
    /// 当前 Stable 不满足报告或调用方的 expected-current。
    #[error("Stable Genome 不满足 expected-current 前置条件")]
    ExpectedCurrentMismatch,
    /// Stable 指针缺少 Release/Report 绑定。
    #[error("Stable Genome 缺少可信 Release 绑定")]
    StableBindingMissing,
    /// 指定 Release 没有 Promotion Audit。
    #[error("Release 不存在：{0}")]
    ReleaseNotFound(ReleaseId),
    /// lineage 代数已溢出。
    #[error("Stable Genome 代数溢出")]
    GenerationOverflow,
    /// Release 锁根不是安全普通目录。
    #[error("Release 锁路径不安全：{0}")]
    UnsafeLockPath(PathBuf),
    /// Release 锁任务异常终止。
    #[error("Release 锁任务异常终止")]
    LockTaskFailed,
    /// 当前平台不支持本地排他锁。
    #[cfg(not(unix))]
    #[error("当前平台不支持本地 Release 排他锁")]
    UnsupportedPlatform,
    /// Release 锁 I/O 错误。
    #[error("Release 锁文件操作失败 `{path}`：{source}")]
    LockIo {
        /// 锁路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// Evaluation Archive 失败。
    #[error(transparent)]
    Archive(#[from] EvaluationArchiveError),
    /// Audit 失败。
    #[error(transparent)]
    Audit(#[from] AuditStoreError),
    /// Genome Resolver 失败。
    #[error(transparent)]
    Resolver(#[from] GenomeResolverError),
    /// Stable 原子更新失败。
    #[error(transparent)]
    Promotion(#[from] GenomePromotionError),
    /// 可信 Genome Diff 失败。
    #[error(transparent)]
    Diff(#[from] GenomeDiffError),
}

/// 构造 Release 锁 I/O 错误。
fn lock_io_error(path: impl AsRef<Path>, source: std::io::Error) -> ReleaseError {
    ReleaseError::LockIo {
        path: path.as_ref().to_path_buf(),
        source,
    }
}
