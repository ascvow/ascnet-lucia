//! 不可变 EvaluationReport 文件存储与 Parent/Candidate 索引。

use agent_evolution_protocol::{
    EvaluationReport, EvaluationReportId, GenomeRevisionId, InvalidEvaluationReport,
};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

/// 本地 EvaluationReport Store。
///
/// 报告正文按 ID 只追加；Parent/Candidate 索引是可替换的小文件，只指向最近生成的
/// 不可变报告，因此 compare 查询不需要扫描 Episode 或反序列化全部历史。
#[derive(Debug, Clone)]
pub struct FileEvaluationReportStore {
    root: PathBuf,
}

impl FileEvaluationReportStore {
    /// 创建尚未触碰文件系统的 Store。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回 Store 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 只追加一份报告并原子更新 Parent/Candidate 最近报告索引。
    ///
    /// # Errors
    ///
    /// 报告无效、ID 已存在、路径不安全、序列化或文件系统操作失败时返回错误。
    pub async fn append(&self, report: &EvaluationReport) -> Result<(), EvaluationStoreError> {
        report.validate()?;
        let reports = self.root.join("reports");
        let comparisons = self.root.join("comparisons");
        ensure_safe_directory(&self.root).await?;
        ensure_safe_directory(&reports).await?;
        ensure_safe_directory(&comparisons).await?;
        let path = reports.join(format!("{}.json", report.report_id));
        let bytes = serde_json::to_vec_pretty(report).map_err(EvaluationStoreError::Serialize)?;
        write_create_new(&path, &bytes, &report.report_id).await?;
        let index = comparisons.join(comparison_index_name(
            &report.parent.genome_revision,
            &report.candidate.genome_revision,
        ));
        atomic_replace(&index, format!("{}\n", report.report_id).as_bytes()).await
    }

    /// 按报告 ID 读取并验证不可变报告。
    ///
    /// 不存在时返回 `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 路径不安全、JSON 损坏、报告 ID 不匹配、报告结构无效或读取失败时返回错误。
    pub async fn get(
        &self,
        id: &EvaluationReportId,
    ) -> Result<Option<EvaluationReport>, EvaluationStoreError> {
        read_report(
            &self.root.join("reports").join(format!("{id}.json")),
            Some(id),
        )
        .await
    }

    /// 使用小索引读取指定 Parent/Candidate 的最近报告。
    ///
    /// 索引或报告不存在时返回 `Ok(None)`，不会回退扫描全部历史。
    ///
    /// # Errors
    ///
    /// 索引路径不安全、索引 ID 非法、报告损坏或读取失败时返回错误。
    pub async fn find_comparison(
        &self,
        parent: &GenomeRevisionId,
        candidate: &GenomeRevisionId,
    ) -> Result<Option<EvaluationReport>, EvaluationStoreError> {
        let index = self
            .root
            .join("comparisons")
            .join(comparison_index_name(parent, candidate));
        let Some(bytes) = read_safe_file(&index).await? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| EvaluationStoreError::InvalidIndex(index.clone()))?;
        let id = EvaluationReportId::new(text.trim())
            .map_err(|_| EvaluationStoreError::InvalidIndex(index))?;
        self.get(&id).await
    }

    /// 读取全部小型 EvaluationReport，供显式历史命令使用。
    ///
    /// 该方法不会读取 Episode 或 CAS 大制品；结果按生成时间和报告 ID 稳定排序。
    ///
    /// # Errors
    ///
    /// 目录不安全、任一报告损坏或目录遍历失败时返回错误，不会静默跳过坏数据。
    pub async fn list(&self) -> Result<Vec<EvaluationReport>, EvaluationStoreError> {
        let reports = self.root.join("reports");
        let Some(metadata) = safe_metadata(&reports).await? else {
            return Ok(Vec::new());
        };
        if !metadata.is_dir() {
            return Err(EvaluationStoreError::UnsafePath {
                path: reports,
                reason: "EvaluationReport 目录必须是普通目录",
            });
        }
        let mut entries = fs::read_dir(&reports)
            .await
            .map_err(|source| io_error("遍历 EvaluationReport 目录", &reports, source))?;
        let mut values = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| io_error("读取 EvaluationReport 目录项", &reports, source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let report = read_report(&path, None).await?.ok_or_else(|| {
                EvaluationStoreError::UnsafePath {
                    path: path.clone(),
                    reason: "遍历期间 EvaluationReport 被移除",
                }
            })?;
            values.push(report);
        }
        values.sort_by(|left, right| {
            left.generated_at_ms
                .cmp(&right.generated_at_ms)
                .then_with(|| left.report_id.cmp(&right.report_id))
        });
        Ok(values)
    }
}

/// 读取用户显式指定的单份 EvaluationReport 文件。
///
/// # Errors
///
/// 文件不存在、为符号链接、JSON 损坏、Schema 未知或结构无效时返回错误。
pub async fn load_evaluation_report(
    path: impl AsRef<Path>,
) -> Result<EvaluationReport, EvaluationStoreError> {
    let path = path.as_ref();
    read_report(path, None)
        .await?
        .ok_or_else(|| EvaluationStoreError::NotFound(path.to_path_buf()))
}

/// EvaluationReport Store 错误。
#[derive(Debug, thiserror::Error)]
pub enum EvaluationStoreError {
    /// 报告结构或 Schema 无效。
    #[error("EvaluationReport 无效：{0}")]
    InvalidReport(#[from] InvalidEvaluationReport),
    /// 同一报告 ID 已存在，禁止覆盖。
    #[error("EvaluationReport 已存在，禁止覆盖：{0}")]
    AlreadyExists(EvaluationReportId),
    /// 显式指定的报告不存在。
    #[error("EvaluationReport 不存在：{0}")]
    NotFound(PathBuf),
    /// 比较索引内容不是合法报告 ID。
    #[error("EvaluationReport 比较索引损坏：{0}")]
    InvalidIndex(PathBuf),
    /// 报告文件名与正文 ID 不一致。
    #[error("EvaluationReport 文件名与正文 ID 不一致：{0}")]
    IdMismatch(PathBuf),
    /// 路径为符号链接或类型不符合预期。
    #[error("EvaluationReport 路径不安全：{path}: {reason}")]
    UnsafePath {
        /// 不安全路径。
        path: PathBuf,
        /// 稳定原因。
        reason: &'static str,
    },
    /// JSON 序列化失败。
    #[error("序列化 EvaluationReport 失败：{0}")]
    Serialize(serde_json::Error),
    /// JSON 反序列化失败。
    #[error("EvaluationReport JSON 损坏：{path}: {source}")]
    InvalidJson {
        /// 损坏路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
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

/// 返回不会发生路径分隔符注入的比较索引文件名。
fn comparison_index_name(parent: &GenomeRevisionId, candidate: &GenomeRevisionId) -> String {
    format!("{}--{}.ref", parent.as_str(), candidate.as_str())
}

/// 创建并验证普通目录，拒绝符号链接根。
async fn ensure_safe_directory(path: &Path) -> Result<(), EvaluationStoreError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| io_error("创建 EvaluationReport 目录", path, source))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查 EvaluationReport 目录", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EvaluationStoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "目录必须是非符号链接普通目录",
        });
    }
    Ok(())
}

/// 使用 create-new 语义写入不可变报告。
async fn write_create_new(
    path: &Path,
    bytes: &[u8],
    id: &EvaluationReportId,
) -> Result<(), EvaluationStoreError> {
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(EvaluationStoreError::AlreadyExists(id.clone()));
        }
        Err(source) => return Err(io_error("创建 EvaluationReport", path, source)),
    };
    file.write_all(bytes)
        .await
        .map_err(|source| io_error("写入 EvaluationReport", path, source))?;
    file.sync_all()
        .await
        .map_err(|source| io_error("同步 EvaluationReport", path, source))
}

/// 以同目录临时文件原子替换可重建索引。
async fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), EvaluationStoreError> {
    let parent = path.parent().expect("索引路径必须有父目录");
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|source| io_error("创建比较索引临时文件", &temporary, source))?;
        file.write_all(bytes)
            .await
            .map_err(|source| io_error("写入比较索引临时文件", &temporary, source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("同步比较索引临时文件", &temporary, source))?;
        drop(file);
        fs::rename(&temporary, path)
            .await
            .map_err(|source| io_error("提交比较索引", path, source))
    }
    .await;
    let _ = fs::remove_file(&temporary).await;
    result
}

/// 读取并验证单份报告。
async fn read_report(
    path: &Path,
    expected_id: Option<&EvaluationReportId>,
) -> Result<Option<EvaluationReport>, EvaluationStoreError> {
    let Some(bytes) = read_safe_file(path).await? else {
        return Ok(None);
    };
    let report: EvaluationReport =
        serde_json::from_slice(&bytes).map_err(|source| EvaluationStoreError::InvalidJson {
            path: path.to_path_buf(),
            source,
        })?;
    if expected_id.is_some_and(|id| id != &report.report_id) {
        return Err(EvaluationStoreError::IdMismatch(path.to_path_buf()));
    }
    report.validate()?;
    Ok(Some(report))
}

/// 返回路径 metadata；不存在时返回 `None`，符号链接直接拒绝。
async fn safe_metadata(path: &Path) -> Result<Option<std::fs::Metadata>, EvaluationStoreError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(EvaluationStoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "拒绝符号链接",
            })
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("检查 EvaluationReport 路径", path, source)),
    }
}

/// 只读取非符号链接普通文件。
async fn read_safe_file(path: &Path) -> Result<Option<Vec<u8>>, EvaluationStoreError> {
    let Some(metadata) = safe_metadata(path).await? else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(EvaluationStoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "目标必须是普通文件",
        });
    }
    fs::read(path)
        .await
        .map(Some)
        .map_err(|source| io_error("读取 EvaluationReport", path, source))
}

/// 构造带路径上下文的 I/O 错误。
fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> EvaluationStoreError {
    EvaluationStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        EvaluationEnvironment, EvaluationRun, EvaluationRunId, EvolutionLifecycle, GateDecision,
        GenomeDiff, EVALUATION_REPORT_SCHEMA_VERSION,
    };
    use std::collections::{BTreeMap, BTreeSet};

    /// 构造不含 Case 的结构合法报告。
    fn report() -> EvaluationReport {
        let environment = EvaluationEnvironment {
            kernel_ref: "kernel".into(),
            model_provider: "fixture".into(),
            model: "model".into(),
            model_parameters_digest: "params".into(),
            tool_profile_digest: "tools".into(),
            execution_profile_digest: "execution".into(),
            plugin_set_digest: "plugins".into(),
            capability_owner_digest: "owners".into(),
            resource_budget_digest: "budget".into(),
            verifier_version: "verifier".into(),
            evaluation_policy_version: "policy".into(),
            environment_fixture_digest: "fixture".into(),
            repeat_count: 1,
        };
        EvaluationReport {
            schema_version: EVALUATION_REPORT_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            parent: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: GenomeRevisionId::generate(),
                environment: environment.clone(),
                datasets: BTreeMap::new(),
                task_cases: Vec::new(),
            },
            candidate: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: GenomeRevisionId::generate(),
                environment,
                datasets: BTreeMap::new(),
                task_cases: Vec::new(),
            },
            genome_diff: GenomeDiff::default(),
            allowed_mutation_surfaces: BTreeSet::new(),
            gate_decision: GateDecision::Unknown,
            lifecycle: EvolutionLifecycle::Evaluated,
            release_record: None,
            inheritance: None,
            artifact_integrity_verified: None,
            audit_integrity_verified: None,
            hidden_dataset_isolated: None,
            generated_at_ms: 1,
        }
    }

    #[tokio::test]
    async fn appends_and_resolves_comparison_without_history_scan() {
        let root = std::env::temp_dir().join(format!(
            "lucia-evaluation-reports-{}",
            Uuid::new_v4().simple()
        ));
        let store = FileEvaluationReportStore::new(&root);
        let value = report();
        store.append(&value).await.expect("报告应写入");
        let loaded = store
            .find_comparison(
                &value.parent.genome_revision,
                &value.candidate.genome_revision,
            )
            .await
            .expect("索引应可读取")
            .expect("报告应存在");
        assert_eq!(loaded.report_id, value.report_id);
        assert!(matches!(
            store.append(&value).await,
            Err(EvaluationStoreError::AlreadyExists(_))
        ));
        let _ = fs::remove_dir_all(root).await;
    }
}
