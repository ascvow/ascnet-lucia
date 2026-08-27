//! M8 插件评测归档、Canary、Stable 与回滚控制面。
//!
//! 本模块只接受完整 Gate 输入和报告，执行真实签名验证，并先把输入、报告、Component、
//! Bundle 和 Release 信封写入不可变 CAS/只追加索引，再返回可执行发布结果。它不加载
//! Candidate 代码，也不允许跳过 Canary 直接进入 Stable。

use crate::{PluginSignatureError, TrustedPluginKeyring};
use agent_evolution::{ArtifactStore, ArtifactStoreError, FileArtifactStore};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, InvalidPluginEvolution, PluginCanaryRecord, PluginCanaryState,
    PluginEvaluationGateInput, PluginEvaluationReport, PluginReleaseEnvelope, PluginReleaseStage,
    PluginSourceGateDecision, ReleaseId, SignaturePurpose, PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions as StdOpenOptions,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{fs, sync::Mutex};

#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

/// 只追加临时文件名的进程内单调序号。
static APPEND_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// 同一进程内跨归档句柄串行化 Canary 读改写事务。
static CANARY_TRANSACTION_GUARD: Mutex<()> = Mutex::const_new(());

/// 插件 Gate 输入在 CAS 中的媒体类型。
pub const PLUGIN_GATE_INPUT_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.plugin-gate-input.v1+json";
/// 插件 Gate 报告在 CAS 中的媒体类型。
pub const PLUGIN_EVALUATION_REPORT_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.plugin-evaluation-report.v1+json";
/// 插件 WASM Component 在 CAS 中的媒体类型。
pub const PLUGIN_COMPONENT_MEDIA_TYPE: &str = "application/wasm";
/// 完整插件 Bundle 在 CAS 中的媒体类型。
pub const PLUGIN_BUNDLE_MEDIA_TYPE: &str = "application/vnd.ascnet.lucia.plugin-bundle.v1";
/// 插件 Release 信封在 CAS 中的媒体类型。
pub const PLUGIN_RELEASE_ENVELOPE_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.plugin-release-envelope.v1+json";
/// 插件评测归档索引 schema 版本。
pub const PLUGIN_EVALUATION_ARCHIVE_SCHEMA_VERSION: u32 = 1;
/// 插件 Release 归档索引 schema 版本。
pub const PLUGIN_RELEASE_ARCHIVE_SCHEMA_VERSION: u32 = 1;

/// 一项只追加插件评测归档索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEvaluationArchiveRecordV1 {
    /// 索引 schema 版本。
    pub schema_version: u32,
    /// 报告 ID。
    pub report_id: agent_evolution_protocol::EvaluationReportId,
    /// 插件 ID。
    pub plugin_id: String,
    /// 完整 Gate 输入的不可变 CAS 引用。
    pub gate_input_artifact: ArtifactRef,
    /// 受信 Gate 报告的不可变 CAS 引用。
    pub report_artifact: ArtifactRef,
    /// Gate 报告的决策；只可能 RequireApproval 或 Canary。
    pub decision: PluginSourceGateDecision,
}

/// 一项只追加插件 Release 归档索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginReleaseArchiveRecordV1 {
    /// 索引 schema 版本。
    pub schema_version: u32,
    /// 完整 Release 信封。
    pub release: PluginReleaseEnvelope,
    /// Release 信封的不可变 CAS 引用。
    pub release_artifact: ArtifactRef,
    /// 对应 Gate 报告的不可变 CAS 引用。
    pub evaluation_report_artifact: ArtifactRef,
    /// Candidate Component 的不可变 CAS 引用。
    pub component_artifact: ArtifactRef,
    /// 完整 Bundle 的不可变 CAS 引用。
    pub bundle_artifact: ArtifactRef,
    /// Rollback 恢复目标 Component；非 Rollback 阶段为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_target_artifact: Option<ArtifactRef>,
}

/// 基于不可变 CAS 和只追加 JSON 索引的插件发布归档。
///
/// 同一进程内追加通过 Mutex 串行化；磁盘文件使用 `create_new`，不同内容不能覆盖已有 ID。
#[derive(Debug, Clone)]
pub struct FilePluginReleaseArchive<'a> {
    root: PathBuf,
    artifacts: &'a FileArtifactStore,
    write_guard: Arc<Mutex<()>>,
}

impl<'a> FilePluginReleaseArchive<'a> {
    /// 创建归档句柄，并把根目录固定到已规范化的真实路径。
    ///
    /// # Errors
    ///
    /// 根路径不是绝对路径、包含 `.`/`..`、目标是符号链接/非目录，或无法安全创建时返回错误。
    pub fn new(
        root: impl Into<PathBuf>,
        artifacts: &'a FileArtifactStore,
    ) -> Result<Self, PluginReleaseError> {
        Ok(Self {
            root: prepare_archive_root(root.into())?,
            artifacts,
            write_guard: Arc::new(Mutex::new(())),
        })
    }

    /// 返回归档根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把完整 Gate 输入与报告先写入 CAS，再只追加报告索引。
    ///
    /// # Errors
    ///
    /// 报告与输入错绑、CAS 或文件系统失败、同一报告 ID 内容冲突时返回错误。
    pub async fn append_evaluation(
        &self,
        input: &PluginEvaluationGateInput,
        report: &PluginEvaluationReport,
    ) -> Result<PluginEvaluationArchiveRecordV1, PluginReleaseError> {
        report.validate_for_input(input)?;
        let input_bytes = serde_json::to_vec(input)?;
        let report_bytes = serde_json::to_vec(report)?;
        let gate_input_artifact = self
            .artifacts
            .put(PLUGIN_GATE_INPUT_MEDIA_TYPE, &input_bytes)
            .await?;
        if gate_input_artifact.digest != input.digest()? {
            return Err(PluginReleaseError::ArtifactBindingMismatch("gate_input"));
        }
        let report_artifact = self
            .artifacts
            .put(PLUGIN_EVALUATION_REPORT_MEDIA_TYPE, &report_bytes)
            .await?;
        if report_artifact.digest != report.digest_for_input(input)? {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "evaluation_report",
            ));
        }
        let record = PluginEvaluationArchiveRecordV1 {
            schema_version: PLUGIN_EVALUATION_ARCHIVE_SCHEMA_VERSION,
            report_id: report.report_id.clone(),
            plugin_id: report.plugin_id.clone(),
            gate_input_artifact,
            report_artifact,
            decision: report.decision,
        };
        let path = self
            .root
            .join("evaluations")
            .join(format!("{}.json", report.report_id));
        self.append_json(&path, &record).await?;
        Ok(record)
    }

    /// 读取并复核指定报告的归档索引。
    ///
    /// # Errors
    ///
    /// 文件不安全、JSON 无效或 CAS 引用缺失/篡改时返回错误。
    pub async fn evaluation(
        &self,
        report_id: &agent_evolution_protocol::EvaluationReportId,
    ) -> Result<Option<PluginEvaluationArchiveRecordV1>, PluginReleaseError> {
        let path = self
            .root
            .join("evaluations")
            .join(format!("{report_id}.json"));
        let Some(bytes) = read_safe_file(&self.root, &path).await? else {
            return Ok(None);
        };
        let record: PluginEvaluationArchiveRecordV1 = serde_json::from_slice(&bytes)?;
        self.verify_evaluation_record(&record, report_id).await?;
        Ok(Some(record))
    }

    /// 只追加一项已完成真实验签和制品绑定的 Release 记录。
    ///
    /// # Errors
    ///
    /// Release ID 已被不同内容占用、文件系统失败或记录引用的 CAS 制品缺失时返回错误。
    pub async fn append_release(
        &self,
        record: &PluginReleaseArchiveRecordV1,
    ) -> Result<(), PluginReleaseError> {
        validate_release_record(record)?;
        self.verify_release_record_artifacts(record).await?;
        let path = self
            .root
            .join("releases")
            .join(format!("{}.json", record.release.release_id));
        self.append_json(&path, record).await
    }

    /// 读取指定 Release 并复核全部 CAS 引用。
    ///
    /// # Errors
    ///
    /// 文件、JSON、协议或任一 CAS 引用无效时返回错误。
    pub async fn release(
        &self,
        release_id: &ReleaseId,
    ) -> Result<Option<PluginReleaseArchiveRecordV1>, PluginReleaseError> {
        let path = self
            .root
            .join("releases")
            .join(format!("{release_id}.json"));
        let Some(bytes) = read_safe_file(&self.root, &path).await? else {
            return Ok(None);
        };
        let record: PluginReleaseArchiveRecordV1 = serde_json::from_slice(&bytes)?;
        validate_release_record(&record)?;
        if record.release.release_id != *release_id {
            return Err(PluginReleaseError::ArtifactBindingMismatch("release_id"));
        }
        self.verify_release_record_artifacts(&record).await?;
        Ok(Some(record))
    }

    /// 从受信归档重建指定 Canary Release 的初始 Admission。
    ///
    /// 该入口重新读取 Release、Evaluation 和完整 Canary 历史，不接受调用方提供的 Admission
    /// 副本，供部署进程重启后恢复授权边界。
    ///
    /// # Errors
    ///
    /// Release 不存在、初始 Planned Canary 缺失，或任一归档/CAS/身份绑定无效时返回错误。
    pub async fn canary_admission(
        &self,
        release_id: &ReleaseId,
    ) -> Result<PluginCanaryAdmissionV1, PluginReleaseError> {
        let release = self
            .release(release_id)
            .await?
            .ok_or_else(|| PluginReleaseError::ReleaseNotFound(release_id.clone()))?;
        let report_bytes = self
            .read_artifact(
                &release.evaluation_report_artifact,
                PLUGIN_EVALUATION_REPORT_MEDIA_TYPE,
            )
            .await?;
        let report: PluginEvaluationReport = serde_json::from_slice(&report_bytes)?;
        let evaluation = self.evaluation_for_release(&release).await?;
        let input_bytes = self
            .read_artifact(
                &evaluation.gate_input_artifact,
                PLUGIN_GATE_INPUT_MEDIA_TYPE,
            )
            .await?;
        let input: PluginEvaluationGateInput = serde_json::from_slice(&input_bytes)?;
        let history = self
            .canary_history(&canary_id_for_release(release_id))
            .await?;
        let canary = history
            .first()
            .filter(|record| record.state == PluginCanaryState::Planned)
            .cloned()
            .ok_or(PluginReleaseError::CanaryAdmissionNotFound(
                release_id.clone(),
            ))?;
        canary.validate_against_release(&release.release, &report, &input)?;
        Ok(PluginCanaryAdmissionV1 { release, canary })
    }

    /// 从受信 Release 归档读取并复核完整 bundle 字节。
    ///
    /// # Errors
    ///
    /// 传入记录不是归档中的精确记录，或 bundle CAS 缺失、篡改、长度不符时返回错误。
    pub async fn release_bundle(
        &self,
        record: &PluginReleaseArchiveRecordV1,
    ) -> Result<Vec<u8>, PluginReleaseError> {
        let archived = self
            .release(&record.release.release_id)
            .await?
            .ok_or_else(|| {
                PluginReleaseError::ReleaseNotFound(record.release.release_id.clone())
            })?;
        if archived != *record {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "release_archive_record",
            ));
        }
        self.read_artifact(&record.bundle_artifact, PLUGIN_BUNDLE_MEDIA_TYPE)
            .await
    }

    /// 从受信 Release 记录定位并复核对应 Evaluation 归档。
    ///
    /// # Errors
    ///
    /// Release 未归档、报告 CAS 无效或 Evaluation 索引缺失/错绑时返回错误。
    pub async fn evaluation_for_release(
        &self,
        record: &PluginReleaseArchiveRecordV1,
    ) -> Result<PluginEvaluationArchiveRecordV1, PluginReleaseError> {
        let archived = self
            .release(&record.release.release_id)
            .await?
            .ok_or_else(|| {
                PluginReleaseError::ReleaseNotFound(record.release.release_id.clone())
            })?;
        if archived != *record {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "release_archive_record",
            ));
        }
        let report_bytes = self
            .read_artifact(
                &record.evaluation_report_artifact,
                PLUGIN_EVALUATION_REPORT_MEDIA_TYPE,
            )
            .await?;
        let report: PluginEvaluationReport = serde_json::from_slice(&report_bytes)?;
        let evaluation = self
            .evaluation(&report.report_id)
            .await?
            .ok_or_else(|| PluginReleaseError::EvaluationNotFound(report.report_id.clone()))?;
        if evaluation.report_artifact != record.evaluation_report_artifact {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "evaluation_report",
            ));
        }
        Ok(evaluation)
    }

    /// 追加一项 Canary 状态快照，并校验完整历史为单调状态链。
    ///
    /// 相同快照重试幂等；同一进度键出现不同内容会被视为分叉。
    ///
    /// # Errors
    ///
    /// 快照无效、lineage 改写、状态跳跃/回退、历史分叉或文件系统失败时返回错误。
    pub async fn append_canary(
        &self,
        record: &PluginCanaryRecord,
    ) -> Result<(), PluginReleaseError> {
        record.validate()?;
        let _guard = self.write_guard.lock().await;
        let _transaction_guard = CANARY_TRANSACTION_GUARD.lock().await;
        let _file_lock = self.acquire_canary_lock(&record.canary_id).await?;
        let mut history = self.canary_history_unlocked(&record.canary_id).await?;
        if history.iter().any(|existing| existing == record) {
            return Ok(());
        }
        if let Some(previous) = history.last() {
            validate_canary_transition(previous, record)?;
        } else if record.state != PluginCanaryState::Planned {
            return Err(PluginReleaseError::InvalidCanaryTransition {
                from: None,
                to: record.state,
            });
        }
        history.push(record.clone());
        validate_canary_history(&history)?;
        let bytes = serde_json::to_vec(record)?;
        let digest = Sha256::digest(&bytes);
        let directory = self.canary_directory(&record.canary_id);
        let path = directory.join(format!("{:x}.json", digest));
        write_create_new_or_same(&self.root, &path, &bytes).await
    }

    /// 读取一条 Canary 的完整只追加历史。
    ///
    /// # Errors
    ///
    /// 目录含符号链接、记录无效、lineage 分叉或状态链不单调时返回错误。
    pub async fn canary_history(
        &self,
        canary_id: &str,
    ) -> Result<Vec<PluginCanaryRecord>, PluginReleaseError> {
        let _guard = self.write_guard.lock().await;
        let _transaction_guard = CANARY_TRANSACTION_GUARD.lock().await;
        let _file_lock = self.acquire_canary_lock(canary_id).await?;
        self.canary_history_unlocked(canary_id).await
    }

    async fn canary_history_unlocked(
        &self,
        canary_id: &str,
    ) -> Result<Vec<PluginCanaryRecord>, PluginReleaseError> {
        let directory = self.canary_directory(canary_id);
        let mut records = read_json_directory::<PluginCanaryRecord>(&self.root, &directory).await?;
        for record in &records {
            record.validate()?;
            if record.canary_id != canary_id {
                return Err(PluginReleaseError::CanaryLineageMismatch);
            }
        }
        records.sort_by_key(canary_sort_key);
        validate_canary_history(&records)?;
        Ok(records)
    }

    fn canary_directory(&self, canary_id: &str) -> PathBuf {
        let digest = Sha256::digest(canary_id.as_bytes());
        self.root.join("canaries").join(format!("{:x}", digest))
    }

    async fn acquire_canary_lock(
        &self,
        canary_id: &str,
    ) -> Result<ArchiveFileLock, PluginReleaseError> {
        let digest = Sha256::digest(canary_id.as_bytes());
        let path = self
            .root
            .join("locks")
            .join("canaries")
            .join(format!("{digest:x}.lock"));
        let parent = path
            .parent()
            .ok_or_else(|| PluginReleaseError::AppendConflict(path.clone()))?;
        ensure_archive_directory(&self.root, parent).await?;
        acquire_file_lock(path).await
    }

    async fn append_json<T: Serialize + PartialEq + for<'de> Deserialize<'de>>(
        &self,
        path: &Path,
        value: &T,
    ) -> Result<(), PluginReleaseError> {
        let _guard = self.write_guard.lock().await;
        let bytes = serde_json::to_vec(value)?;
        if let Some(existing) = read_safe_file(&self.root, path).await? {
            let existing_value: T = serde_json::from_slice(&existing)?;
            if existing_value == *value {
                return Ok(());
            }
            return Err(PluginReleaseError::AppendConflict(path.to_path_buf()));
        }
        write_create_new_or_same(&self.root, path, &bytes).await
    }

    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        expected_media_type: &'static str,
    ) -> Result<Vec<u8>, PluginReleaseError> {
        if reference.media_type != expected_media_type {
            return Err(PluginReleaseError::ArtifactBindingMismatch("media_type"));
        }
        let bytes = self
            .artifacts
            .get(&reference.digest)
            .await?
            .ok_or_else(|| PluginReleaseError::MissingArtifact(reference.digest.clone()))?;
        if bytes.len() as u64 != reference.size_bytes {
            return Err(PluginReleaseError::ArtifactBindingMismatch("size"));
        }
        Ok(bytes)
    }

    async fn verify_evaluation_record(
        &self,
        record: &PluginEvaluationArchiveRecordV1,
        expected_report_id: &agent_evolution_protocol::EvaluationReportId,
    ) -> Result<(), PluginReleaseError> {
        if record.schema_version != PLUGIN_EVALUATION_ARCHIVE_SCHEMA_VERSION
            || record.report_id != *expected_report_id
        {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "evaluation_archive_record",
            ));
        }
        let input_bytes = self
            .read_artifact(&record.gate_input_artifact, PLUGIN_GATE_INPUT_MEDIA_TYPE)
            .await?;
        let report_bytes = self
            .read_artifact(&record.report_artifact, PLUGIN_EVALUATION_REPORT_MEDIA_TYPE)
            .await?;
        let input: PluginEvaluationGateInput = serde_json::from_slice(&input_bytes)?;
        let report: PluginEvaluationReport = serde_json::from_slice(&report_bytes)?;
        report.validate_for_input(&input)?;
        if report.report_id != record.report_id
            || report.plugin_id != record.plugin_id
            || report.decision != record.decision
            || input.report_id != record.report_id
            || input.digest()? != record.gate_input_artifact.digest
            || report.digest_for_input(&input)? != record.report_artifact.digest
        {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "evaluation_archive_record",
            ));
        }
        Ok(())
    }

    async fn verify_release_record_artifacts(
        &self,
        record: &PluginReleaseArchiveRecordV1,
    ) -> Result<(), PluginReleaseError> {
        let release_bytes = self
            .read_artifact(&record.release_artifact, PLUGIN_RELEASE_ENVELOPE_MEDIA_TYPE)
            .await?;
        let archived_release: PluginReleaseEnvelope = serde_json::from_slice(&release_bytes)?;
        let report_bytes = self
            .read_artifact(
                &record.evaluation_report_artifact,
                PLUGIN_EVALUATION_REPORT_MEDIA_TYPE,
            )
            .await?;
        let report: PluginEvaluationReport = serde_json::from_slice(&report_bytes)?;
        self.read_artifact(&record.component_artifact, PLUGIN_COMPONENT_MEDIA_TYPE)
            .await?;
        self.read_artifact(&record.bundle_artifact, PLUGIN_BUNDLE_MEDIA_TYPE)
            .await?;
        if let Some(target) = &record.rollback_target_artifact {
            self.read_artifact(target, PLUGIN_COMPONENT_MEDIA_TYPE)
                .await?;
        }
        if archived_release != record.release
            || serde_json::to_vec(&archived_release)? != release_bytes
            || report.plugin_id != record.release.plugin_id
            || report.mutation_id != record.release.mutation_id
            || report.candidate_id != record.release.candidate_id
            || report.component_digest != record.release.attestation.component_digest
            || report.bundle_digest != record.release.bundle_digest
        {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "release_archive_record",
            ));
        }
        let evaluation = self
            .evaluation(&report.report_id)
            .await?
            .ok_or_else(|| PluginReleaseError::EvaluationNotFound(report.report_id.clone()))?;
        if evaluation.report_artifact != record.evaluation_report_artifact {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "evaluation_report",
            ));
        }
        Ok(())
    }
}

/// 已完成归档、可进入受控 Canary 的发布结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCanaryAdmissionV1 {
    /// 只追加 Release 归档记录。
    pub release: PluginReleaseArchiveRecordV1,
    /// 初始 Planned Canary 状态。
    pub canary: PluginCanaryRecord,
}

/// 一次失败 Canary 回滚所需的完整受信输入与制品字节。
pub struct PluginRollbackRequestV1<'a> {
    /// 失败 Candidate 的完整 Gate 输入。
    pub input: &'a PluginEvaluationGateInput,
    /// 与 Gate 输入精确绑定的 Canary 报告。
    pub report: &'a PluginEvaluationReport,
    /// 已进入只追加历史的 Failed Canary 终态。
    pub failed: &'a PluginCanaryRecord,
    /// 指向旧 Stable Component 的已签名 Rollback 信封。
    pub rollback: &'a PluginReleaseEnvelope,
    /// 旧 Stable Release ID；控制器会重新读取并验签。
    pub rollback_target_release_id: &'a ReleaseId,
    /// 失败 Candidate 的真实 Component 字节。
    pub candidate_component_bytes: &'a [u8],
    /// 失败 Candidate 的完整 Bundle 字节。
    pub bundle_bytes: &'a [u8],
    /// 要恢复的旧 Stable Component 字节。
    pub rollback_target_bytes: &'a [u8],
}

/// 执行真实验签、CAS 绑定和发布阶段门禁的受信控制器。
pub struct PluginReleaseController<'a> {
    archive: &'a FilePluginReleaseArchive<'a>,
    build_keys: &'a TrustedPluginKeyring,
    approval_keys: &'a TrustedPluginKeyring,
    release_keys: &'a TrustedPluginKeyring,
}

impl<'a> PluginReleaseController<'a> {
    /// 使用三类用途隔离的 Keyring 创建发布控制器。
    pub fn new(
        archive: &'a FilePluginReleaseArchive<'a>,
        build_keys: &'a TrustedPluginKeyring,
        approval_keys: &'a TrustedPluginKeyring,
        release_keys: &'a TrustedPluginKeyring,
    ) -> Self {
        Self {
            archive,
            build_keys,
            approval_keys,
            release_keys,
        }
    }

    /// 归档任意完整 Gate 结果；RequireApproval 只归档，不产生发布副作用。
    ///
    /// # Errors
    ///
    /// Gate 输入或报告无效、CAS/归档失败时返回错误。
    pub async fn archive_evaluation(
        &self,
        input: &PluginEvaluationGateInput,
        report: &PluginEvaluationReport,
    ) -> Result<PluginEvaluationArchiveRecordV1, PluginReleaseError> {
        self.archive.append_evaluation(input, report).await
    }

    /// 验证完整 Gate、真实构建/发布/审批签名与制品字节，归档后进入 Planned Canary。
    ///
    /// RequireApproval 报告、Stable/Rollback 信封、能力扩张缺少真实审批签名、摘要错绑或
    /// 任何制品篡改都会失败关闭。
    ///
    /// # Errors
    ///
    /// 协议、签名、CAS、归档或发布阶段不满足要求时返回错误。
    pub async fn admit_canary(
        &self,
        input: &PluginEvaluationGateInput,
        report: &PluginEvaluationReport,
        release: &PluginReleaseEnvelope,
        component_bytes: &[u8],
        bundle_bytes: &[u8],
    ) -> Result<PluginCanaryAdmissionV1, PluginReleaseError> {
        if release.stage != PluginReleaseStage::Canary
            || report.decision != PluginSourceGateDecision::Canary
        {
            return Err(PluginReleaseError::CanaryRequired);
        }
        release.validate_for_evaluation(report, input)?;
        input
            .build_attestation
            .validate_for_proposal(&input.proposal)?;
        self.verify_release_signatures(release)?;
        let evaluation = self.archive.append_evaluation(input, report).await?;
        let record = self
            .archive_release_artifacts(
                release,
                evaluation.report_artifact,
                component_bytes,
                bundle_bytes,
                None,
            )
            .await?;
        self.archive.append_release(&record).await?;
        let canary = planned_canary(release)?;
        self.archive.append_canary(&canary).await?;
        Ok(PluginCanaryAdmissionV1 {
            release: record,
            canary,
        })
    }

    /// 在归档中只追加一项 Running/Succeeded/Failed Canary 观察。
    ///
    /// # Errors
    ///
    /// 原 Release 不存在、记录错绑或状态迁移不单调时返回错误。
    pub async fn record_canary_observation(
        &self,
        input: &PluginEvaluationGateInput,
        report: &PluginEvaluationReport,
        observation: &PluginCanaryRecord,
    ) -> Result<(), PluginReleaseError> {
        let release = self
            .archive
            .release(&observation.release_id)
            .await?
            .ok_or_else(|| PluginReleaseError::ReleaseNotFound(observation.release_id.clone()))?;
        observation.validate_against_release(&release.release, report, input)?;
        self.archive.append_canary(observation).await
    }

    /// 只有成功 Canary 才能归档 Stable 信封；Stable 必须承接精确 Canary lineage。
    ///
    /// # Errors
    ///
    /// Canary 未成功、Stable 绕过/错绑 Canary、签名或制品无效时返回错误。
    pub async fn promote_stable(
        &self,
        input: &PluginEvaluationGateInput,
        report: &PluginEvaluationReport,
        succeeded: &PluginCanaryRecord,
        stable: &PluginReleaseEnvelope,
        component_bytes: &[u8],
        bundle_bytes: &[u8],
    ) -> Result<PluginReleaseArchiveRecordV1, PluginReleaseError> {
        if succeeded.state != PluginCanaryState::Succeeded
            || stable.stage != PluginReleaseStage::Stable
            || stable.canary_of.as_ref() != Some(&succeeded.release_id)
        {
            return Err(PluginReleaseError::SuccessfulCanaryRequired);
        }
        let canary = self
            .archive
            .release(&succeeded.release_id)
            .await?
            .ok_or_else(|| PluginReleaseError::ReleaseNotFound(succeeded.release_id.clone()))?;
        succeeded.validate_against_release(&canary.release, report, input)?;
        self.require_archived_canary_terminal(succeeded).await?;
        ensure_same_candidate_lineage(&canary.release, stable)?;
        stable.validate_for_evaluation(report, input)?;
        self.verify_release_signatures(stable)?;
        let evaluation = self.archive.append_evaluation(input, report).await?;
        let record = self
            .archive_release_artifacts(
                stable,
                evaluation.report_artifact,
                component_bytes,
                bundle_bytes,
                None,
            )
            .await?;
        self.archive.append_release(&record).await?;
        Ok(record)
    }

    /// 健康失败后归档指向先前受信 Component 的 Rollback，并追加 RolledBack Canary 终态。
    ///
    /// # Errors
    ///
    /// Canary 未失败、Rollback lineage/目标/签名无效或归档失败时返回错误。
    pub async fn rollback_failed_canary(
        &self,
        request: PluginRollbackRequestV1<'_>,
    ) -> Result<PluginReleaseArchiveRecordV1, PluginReleaseError> {
        let PluginRollbackRequestV1 {
            input,
            report,
            failed,
            rollback,
            rollback_target_release_id,
            candidate_component_bytes,
            bundle_bytes,
            rollback_target_bytes,
        } = request;
        if failed.state != PluginCanaryState::Failed
            || rollback.stage != PluginReleaseStage::Rollback
            || rollback.rollback_of.as_ref() != Some(&failed.release_id)
        {
            return Err(PluginReleaseError::FailedCanaryRequired);
        }
        let canary = self
            .archive
            .release(&failed.release_id)
            .await?
            .ok_or_else(|| PluginReleaseError::ReleaseNotFound(failed.release_id.clone()))?;
        failed.validate_against_release(&canary.release, report, input)?;
        self.require_archived_canary_terminal(failed).await?;
        ensure_same_candidate_lineage(&canary.release, rollback)?;
        rollback.validate_for_evaluation(report, input)?;
        self.verify_release_signatures(rollback)?;
        let rollback_target = self
            .archive
            .release(rollback_target_release_id)
            .await?
            .ok_or_else(|| {
                PluginReleaseError::ReleaseNotFound(rollback_target_release_id.clone())
            })?;
        if rollback_target.release.stage != PluginReleaseStage::Stable
            || rollback_target.release.plugin_id != rollback.plugin_id
            || rollback_target.release.issued_at_ms >= canary.release.issued_at_ms
            || Some(&rollback_target.release.attestation.component_digest)
                != rollback.rollback_target_component_digest.as_ref()
        {
            return Err(PluginReleaseError::InvalidRollbackTarget);
        }
        self.verify_release_signatures(&rollback_target.release)?;
        let target_artifact = self
            .archive
            .artifacts
            .put(PLUGIN_COMPONENT_MEDIA_TYPE, rollback_target_bytes)
            .await?;
        if rollback.rollback_target_component_digest.as_ref() != Some(&target_artifact.digest) {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "rollback_target_component",
            ));
        }
        let evaluation = self.archive.append_evaluation(input, report).await?;
        let record = self
            .archive_release_artifacts(
                rollback,
                evaluation.report_artifact,
                candidate_component_bytes,
                bundle_bytes,
                Some(target_artifact),
            )
            .await?;
        self.archive.append_release(&record).await?;
        let mut rolled_back = failed.clone();
        rolled_back.state = PluginCanaryState::RolledBack;
        rolled_back.rollback_release_id = Some(rollback.release_id.clone());
        rolled_back.validate()?;
        self.archive.append_canary(&rolled_back).await?;
        Ok(record)
    }

    async fn require_archived_canary_terminal(
        &self,
        expected: &PluginCanaryRecord,
    ) -> Result<(), PluginReleaseError> {
        let history = self.archive.canary_history(&expected.canary_id).await?;
        if history.last() != Some(expected) {
            return Err(PluginReleaseError::CanaryTerminalNotArchived);
        }
        Ok(())
    }

    fn verify_release_signatures(
        &self,
        release: &PluginReleaseEnvelope,
    ) -> Result<(), PluginReleaseError> {
        self.build_keys.verify(
            &release.attestation_signature,
            SignaturePurpose::BuildAttestation,
            release.issued_at_ms,
        )?;
        if let Some(approval) = &release.approval {
            self.approval_keys.verify(
                &approval.signature,
                SignaturePurpose::CapabilityApproval,
                release.issued_at_ms,
            )?;
        }
        self.release_keys.verify(
            &release.signature,
            SignaturePurpose::PluginRelease,
            release.issued_at_ms,
        )?;
        Ok(())
    }

    async fn archive_release_artifacts(
        &self,
        release: &PluginReleaseEnvelope,
        evaluation_report_artifact: ArtifactRef,
        component_bytes: &[u8],
        bundle_bytes: &[u8],
        rollback_target_artifact: Option<ArtifactRef>,
    ) -> Result<PluginReleaseArchiveRecordV1, PluginReleaseError> {
        let component_artifact = self
            .archive
            .artifacts
            .put(PLUGIN_COMPONENT_MEDIA_TYPE, component_bytes)
            .await?;
        if component_artifact.digest != release.attestation.component_digest
            || component_artifact.size_bytes != release.attestation.component_size_bytes
        {
            return Err(PluginReleaseError::ArtifactBindingMismatch("component"));
        }
        let bundle_artifact = self
            .archive
            .artifacts
            .put(PLUGIN_BUNDLE_MEDIA_TYPE, bundle_bytes)
            .await?;
        if bundle_artifact.digest != release.bundle_digest {
            return Err(PluginReleaseError::ArtifactBindingMismatch("bundle"));
        }
        if evaluation_report_artifact.digest != release.evaluation_report_digest {
            return Err(PluginReleaseError::ArtifactBindingMismatch(
                "evaluation_report",
            ));
        }
        let release_bytes = serde_json::to_vec(release)?;
        let release_artifact = self
            .archive
            .artifacts
            .put(PLUGIN_RELEASE_ENVELOPE_MEDIA_TYPE, &release_bytes)
            .await?;
        Ok(PluginReleaseArchiveRecordV1 {
            schema_version: PLUGIN_RELEASE_ARCHIVE_SCHEMA_VERSION,
            release: release.clone(),
            release_artifact,
            evaluation_report_artifact,
            component_artifact,
            bundle_artifact,
            rollback_target_artifact,
        })
    }
}

/// M8 发布归档和控制面错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginReleaseError {
    /// 插件协议对象无效或错绑。
    #[error("插件发布协议无效：{0}")]
    Protocol(#[from] InvalidPluginEvolution),
    /// 真实签名或 Keyring 验证失败。
    #[error("插件发布签名验证失败：{0}")]
    Signature(#[from] PluginSignatureError),
    /// Artifact CAS 操作失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// JSON 编解码失败。
    #[error("插件发布归档 JSON 无效：{0}")]
    Json(#[from] serde_json::Error),
    /// 归档文件系统操作失败。
    #[error("插件发布归档 I/O 失败：{path}: {source}")]
    Io {
        /// 失败路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 归档根路径没有形成可安全寻址的绝对目录。
    #[error("插件发布归档根路径不安全：{0}")]
    UnsafeArchiveRoot(PathBuf),
    /// 阻塞文件系统任务异常终止。
    #[error("插件发布归档阻塞任务异常终止：{0}")]
    BlockingTask(String),
    /// 同一只追加索引 ID 已绑定其他内容。
    #[error("插件发布只追加索引冲突：{0}")]
    AppendConflict(PathBuf),
    /// CAS 引用缺失。
    #[error("插件发布归档引用的 CAS 制品不存在：{0}")]
    MissingArtifact(ArtifactDigest),
    /// 真实制品摘要或长度与受信协议不一致。
    #[error("插件发布制品绑定不一致：{0}")]
    ArtifactBindingMismatch(&'static str),
    /// 只有无失败 Canary 决策可自动进入 Canary。
    #[error("插件源码 Gate 必须为 Canary，且发布阶段必须是 Canary")]
    CanaryRequired,
    /// Stable 必须承接成功 Canary。
    #[error("插件 Stable 发布必须承接精确成功 Canary")]
    SuccessfulCanaryRequired,
    /// Rollback 必须承接失败 Canary。
    #[error("插件 Rollback 必须承接精确失败 Canary")]
    FailedCanaryRequired,
    /// Stable 或 Rollback 所引用的 Canary 终态尚未进入只追加历史。
    #[error("插件 Canary 终态尚未归档")]
    CanaryTerminalNotArchived,
    /// Rollback 目标不是同插件在 Canary 之前已签名归档的 Stable Component。
    #[error("插件 Rollback 目标不是先前受信 Stable Component")]
    InvalidRollbackTarget,
    /// 指定 Release 不存在。
    #[error("插件 Release 不存在：{0}")]
    ReleaseNotFound(ReleaseId),
    /// 指定 Canary Release 缺少可信 Planned Admission。
    #[error("插件 Canary Admission 不存在：{0}")]
    CanaryAdmissionNotFound(ReleaseId),
    /// 指定评测报告尚未归档。
    #[error("插件评测报告不存在：{0}")]
    EvaluationNotFound(agent_evolution_protocol::EvaluationReportId),
    /// Stable/Rollback 改写了原 Canary Candidate lineage。
    #[error("插件发布 Candidate lineage 不一致")]
    ReleaseLineageMismatch,
    /// Canary 记录改写了初始 lineage。
    #[error("插件 Canary lineage 不一致")]
    CanaryLineageMismatch,
    /// Canary 状态迁移不是允许的单调边。
    #[error("插件 Canary 状态迁移无效：{from:?} -> {to:?}")]
    InvalidCanaryTransition {
        /// 前一状态；首次追加为 None。
        from: Option<PluginCanaryState>,
        /// 目标状态。
        to: PluginCanaryState,
    },
    /// 相同 Canary 进度出现多个不同快照。
    #[error("插件 Canary 只追加历史发生分叉")]
    CanaryHistoryFork,
}

fn planned_canary(
    release: &PluginReleaseEnvelope,
) -> Result<PluginCanaryRecord, PluginReleaseError> {
    let release_digest = release.signing_digest()?;
    let record = PluginCanaryRecord {
        schema_version: PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
        canary_id: canary_id_for_release(&release.release_id),
        release_id: release.release_id.clone(),
        release_digest,
        plugin_id: release.plugin_id.clone(),
        mutation_id: release.mutation_id.clone(),
        candidate_id: release.candidate_id.clone(),
        component_digest: release.attestation.component_digest.clone(),
        state: PluginCanaryState::Planned,
        started_at_ms: None,
        finished_at_ms: None,
        observed_runs: 0,
        passed_runs: 0,
        failed_runs: 0,
        health_report_digest: None,
        rollback_release_id: None,
    };
    record.validate()?;
    Ok(record)
}

/// 从 Release ID 构造 Canary 历史使用的稳定摘要 ID。
fn canary_id_for_release(release_id: &ReleaseId) -> String {
    let digest = Sha256::digest(release_id.as_str().as_bytes());
    format!("canary-{digest:x}")
}

fn validate_release_record(
    record: &PluginReleaseArchiveRecordV1,
) -> Result<(), PluginReleaseError> {
    if record.schema_version != PLUGIN_RELEASE_ARCHIVE_SCHEMA_VERSION {
        return Err(PluginReleaseError::ArtifactBindingMismatch(
            "release_archive_schema",
        ));
    }
    record.release.validate()?;
    if record.evaluation_report_artifact.digest != record.release.evaluation_report_digest
        || record.component_artifact.digest != record.release.attestation.component_digest
        || record.component_artifact.size_bytes != record.release.attestation.component_size_bytes
        || record.bundle_artifact.digest != record.release.bundle_digest
        || record
            .rollback_target_artifact
            .as_ref()
            .map(|artifact| &artifact.digest)
            != record.release.rollback_target_component_digest.as_ref()
    {
        return Err(PluginReleaseError::ArtifactBindingMismatch(
            "release_archive_record",
        ));
    }
    Ok(())
}

fn ensure_same_candidate_lineage(
    canary: &PluginReleaseEnvelope,
    next: &PluginReleaseEnvelope,
) -> Result<(), PluginReleaseError> {
    if canary.plugin_id != next.plugin_id
        || canary.mutation_id != next.mutation_id
        || canary.candidate_id != next.candidate_id
        || canary.proposal_digest != next.proposal_digest
        || canary.source_digest != next.source_digest
        || canary.bundle_digest != next.bundle_digest
        || canary.evaluation_report_digest != next.evaluation_report_digest
        || canary.attestation != next.attestation
        || canary.attestation_signature != next.attestation_signature
    {
        return Err(PluginReleaseError::ReleaseLineageMismatch);
    }
    Ok(())
}

fn validate_canary_history(records: &[PluginCanaryRecord]) -> Result<(), PluginReleaseError> {
    for pair in records.windows(2) {
        validate_canary_transition(&pair[0], &pair[1])?;
    }
    for pair in records.windows(2) {
        if canary_sort_key(&pair[0]) == canary_sort_key(&pair[1]) && pair[0] != pair[1] {
            return Err(PluginReleaseError::CanaryHistoryFork);
        }
    }
    Ok(())
}

fn validate_canary_transition(
    previous: &PluginCanaryRecord,
    next: &PluginCanaryRecord,
) -> Result<(), PluginReleaseError> {
    if previous.canary_id != next.canary_id
        || previous.release_id != next.release_id
        || previous.release_digest != next.release_digest
        || previous.plugin_id != next.plugin_id
        || previous.mutation_id != next.mutation_id
        || previous.candidate_id != next.candidate_id
        || previous.component_digest != next.component_digest
        || next.observed_runs < previous.observed_runs
        || next.passed_runs < previous.passed_runs
        || next.failed_runs < previous.failed_runs
    {
        return Err(PluginReleaseError::CanaryLineageMismatch);
    }
    let valid_edge = matches!(
        (previous.state, next.state),
        (PluginCanaryState::Planned, PluginCanaryState::Running)
            | (PluginCanaryState::Running, PluginCanaryState::Running)
            | (PluginCanaryState::Running, PluginCanaryState::Succeeded)
            | (PluginCanaryState::Running, PluginCanaryState::Failed)
            | (PluginCanaryState::Failed, PluginCanaryState::RolledBack)
    );
    if !valid_edge {
        return Err(PluginReleaseError::InvalidCanaryTransition {
            from: Some(previous.state),
            to: next.state,
        });
    }
    if previous.started_at_ms.is_some() && next.started_at_ms != previous.started_at_ms
        || previous.finished_at_ms.is_some() && next.finished_at_ms != previous.finished_at_ms
        || previous.health_report_digest.is_some()
            && next.health_report_digest != previous.health_report_digest
    {
        return Err(PluginReleaseError::CanaryLineageMismatch);
    }
    Ok(())
}

fn canary_sort_key(record: &PluginCanaryRecord) -> (u8, u64, u64) {
    let rank = match record.state {
        PluginCanaryState::Planned => 0,
        PluginCanaryState::Running => 1,
        PluginCanaryState::Succeeded | PluginCanaryState::Failed => 2,
        PluginCanaryState::RolledBack => 3,
    };
    (
        rank,
        record.observed_runs,
        record.finished_at_ms.unwrap_or(0),
    )
}

async fn read_json_directory<T: for<'de> Deserialize<'de>>(
    root: &Path,
    directory: &Path,
) -> Result<Vec<T>, PluginReleaseError> {
    if !validate_existing_archive_directory(root, directory).await? {
        return Ok(Vec::new());
    }
    let mut reader = fs::read_dir(directory)
        .await
        .map_err(|source| io_error(directory, source))?;
    let mut records = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|source| io_error(directory, source))?
    {
        let path = entry.path();
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| PluginReleaseError::AppendConflict(path.clone()))?;
        // 崩溃可能遗留尚未硬链接提交的临时文件；它们不是 Canary 历史的一部分。
        if is_append_temporary_name(&file_name) {
            continue;
        }
        if !is_sha256_json_name(&file_name) {
            return Err(PluginReleaseError::AppendConflict(path));
        }
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(|source| io_error(&path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(PluginReleaseError::AppendConflict(path));
        }
        let bytes = read_safe_file(root, &path)
            .await?
            .ok_or_else(|| PluginReleaseError::AppendConflict(path.clone()))?;
        let expected_name = format!("{:x}.json", Sha256::digest(&bytes));
        if file_name != expected_name {
            return Err(PluginReleaseError::AppendConflict(path));
        }
        records.push(serde_json::from_slice(&bytes)?);
    }
    Ok(records)
}

/// 判断目录项是否是一次只追加写入在崩溃后遗留的未提交临时文件。
fn is_append_temporary_name(file_name: &str) -> bool {
    file_name.starts_with(".append-") && file_name.ends_with(".tmp")
}

/// 判断 Canary 正式记录名是否为小写 SHA-256 加 `.json` 的规范形式。
fn is_sha256_json_name(file_name: &str) -> bool {
    file_name.strip_suffix(".json").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

async fn write_create_new_or_same(
    root: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), PluginReleaseError> {
    let parent = path
        .parent()
        .ok_or_else(|| PluginReleaseError::AppendConflict(path.to_path_buf()))?;
    ensure_archive_directory(root, parent).await?;
    if let Some(existing) = read_safe_file(root, path).await? {
        return if existing == bytes {
            Ok(())
        } else {
            Err(PluginReleaseError::AppendConflict(path.to_path_buf()))
        };
    }
    let sequence = APPEND_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".append-{}-{sequence}.tmp", std::process::id()));
    let temporary_for_write = temporary.clone();
    let owned_bytes = bytes.to_vec();
    let write_result = tokio::task::spawn_blocking(move || {
        let mut options = StdOpenOptions::new();
        options.write(true).create_new(true);
        set_no_follow(&mut options);
        let mut file = options
            .open(&temporary_for_write)
            .map_err(|source| io_error(&temporary_for_write, source))?;
        file.write_all(&owned_bytes)
            .map_err(|source| io_error(&temporary_for_write, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary_for_write, source))
    })
    .await
    .map_err(|source| PluginReleaseError::BlockingTask(source.to_string()))?;
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary).await;
        return Err(error);
    }

    let commit = fs::hard_link(&temporary, path).await;
    let cleanup = fs::remove_file(&temporary).await;
    if let Err(source) = cleanup {
        if source.kind() != std::io::ErrorKind::NotFound {
            return Err(io_error(&temporary, source));
        }
    }
    match commit {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_safe_file(root, path)
                .await?
                .ok_or_else(|| PluginReleaseError::AppendConflict(path.to_path_buf()))?;
            if existing == bytes {
                Ok(())
            } else {
                Err(PluginReleaseError::AppendConflict(path.to_path_buf()))
            }
        }
        Err(source) => Err(io_error(path, source)),
    }
}

async fn ensure_archive_directory(root: &Path, path: &Path) -> Result<(), PluginReleaseError> {
    let relative = archive_relative(root, path)?;
    validate_directory(root).await?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(PluginReleaseError::UnsafeArchiveRoot(path.to_path_buf()));
        };
        current.push(name);
        match fs::create_dir(&current).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                validate_directory(&current).await?;
            }
            Err(source) => return Err(io_error(&current, source)),
        }
    }
    verify_canonical_directory(path).await
}

async fn validate_existing_archive_directory(
    root: &Path,
    path: &Path,
) -> Result<bool, PluginReleaseError> {
    let relative = archive_relative(root, path)?;
    validate_directory(root).await?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(PluginReleaseError::UnsafeArchiveRoot(path.to_path_buf()));
        };
        current.push(name);
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            Ok(_) => return Err(PluginReleaseError::AppendConflict(current)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(io_error(&current, source)),
        }
    }
    verify_canonical_directory(path).await?;
    Ok(true)
}

async fn read_safe_file(root: &Path, path: &Path) -> Result<Option<Vec<u8>>, PluginReleaseError> {
    let parent = path
        .parent()
        .ok_or_else(|| PluginReleaseError::AppendConflict(path.to_path_buf()))?;
    if !validate_existing_archive_directory(root, parent).await? {
        return Ok(None);
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut options = StdOpenOptions::new();
        options.read(true);
        set_no_follow(&mut options);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(&path, source)),
        };
        let metadata = file.metadata().map_err(|source| io_error(&path, source))?;
        if !metadata.is_file() {
            return Err(PluginReleaseError::AppendConflict(path));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| io_error(&path, source))?;
        Ok(Some(bytes))
    })
    .await
    .map_err(|source| PluginReleaseError::BlockingTask(source.to_string()))?
}

fn prepare_archive_root(root: PathBuf) -> Result<PathBuf, PluginReleaseError> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(PluginReleaseError::UnsafeArchiveRoot(root));
    }
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(PluginReleaseError::UnsafeArchiveRoot(root));
        }
        Ok(_) => {
            return std::fs::canonicalize(&root).map_err(|source| io_error(&root, source));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&root, source)),
    }

    let mut existing = root.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| PluginReleaseError::UnsafeArchiveRoot(root.clone()))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| PluginReleaseError::UnsafeArchiveRoot(root.clone()))?;
            }
            Err(source) => return Err(io_error(existing, source)),
        }
    }
    let mut canonical =
        std::fs::canonicalize(existing).map_err(|source| io_error(existing, source))?;
    for name in missing.into_iter().rev() {
        canonical.push(name);
        match std::fs::create_dir(&canonical) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&canonical)
                    .map_err(|source| io_error(&canonical, source))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(PluginReleaseError::UnsafeArchiveRoot(canonical));
                }
            }
            Err(source) => return Err(io_error(&canonical, source)),
        }
    }
    Ok(canonical)
}

fn archive_relative<'a>(root: &'a Path, path: &'a Path) -> Result<&'a Path, PluginReleaseError> {
    path.strip_prefix(root)
        .map_err(|_| PluginReleaseError::UnsafeArchiveRoot(path.to_path_buf()))
}

async fn validate_directory(path: &Path) -> Result<(), PluginReleaseError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginReleaseError::AppendConflict(path.to_path_buf()));
    }
    Ok(())
}

async fn verify_canonical_directory(path: &Path) -> Result<(), PluginReleaseError> {
    let canonical = fs::canonicalize(path)
        .await
        .map_err(|source| io_error(path, source))?;
    if canonical != path {
        return Err(PluginReleaseError::AppendConflict(path.to_path_buf()));
    }
    Ok(())
}

/// 跨归档句柄持有的进程文件锁；析构时释放，不删除锁文件。
struct ArchiveFileLock {
    file: std::fs::File,
}

impl Drop for ArchiveFileLock {
    fn drop(&mut self) {
        unlock_file(&self.file);
    }
}

async fn acquire_file_lock(path: PathBuf) -> Result<ArchiveFileLock, PluginReleaseError> {
    let task_path = path.clone();
    let file = tokio::task::spawn_blocking(move || {
        let mut options = StdOpenOptions::new();
        options.read(true).write(true).create(true);
        set_no_follow(&mut options);
        let file = options
            .open(&task_path)
            .map_err(|source| io_error(&task_path, source))?;
        lock_file(&file).map_err(|source| io_error(&task_path, source))?;
        Ok::<_, PluginReleaseError>(file)
    })
    .await
    .map_err(|source| PluginReleaseError::BlockingTask(source.to_string()))??;
    Ok(ArchiveFileLock { file })
}

#[cfg(unix)]
fn set_no_follow(options: &mut StdOpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
}

#[cfg(not(unix))]
fn set_no_follow(_options: &mut StdOpenOptions) {}

#[cfg(unix)]
fn lock_file(file: &std::fs::File) -> Result<(), std::io::Error> {
    // SAFETY：`file` 在调用期间保持有效，`flock` 只使用其原生文件描述符。
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_file(_file: &std::fs::File) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn unlock_file(file: &std::fs::File) {
    // SAFETY：`file` 在析构完成前仍保持有效；解锁失败不会破坏只追加内容。
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(not(unix))]
fn unlock_file(_file: &std::fs::File) {}

fn io_error(path: &Path, source: std::io::Error) -> PluginReleaseError {
    PluginReleaseError::Io {
        path: path.to_path_buf(),
        source,
    }
}
