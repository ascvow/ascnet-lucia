//! 持久化需要人工介入的失败处置请求。
//!
//! 请求记录一经写入永不修改；处理完成通过独立 `.resolved` 标记表达，确保审计、恢复和
//! 迁移都不依赖原地覆盖。

use agent_evolution_protocol::{
    DiagnosticStatus, EpisodeId, EvolutionIssueId, FailureDisposition, FailureKind, Outcome,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs, io::AsyncWriteExt};

/// 人工干预请求结构版本。
pub const INTERVENTION_QUEUE_SCHEMA_VERSION: u32 = 1;
/// 单份人工干预请求允许的最大字节数。
pub const MAX_INTERVENTION_QUEUE_ITEM_BYTES: u64 = 64 * 1024;
/// 人工干预请求文件名前缀。
const INTERVENTION_PREFIX: &str = "intervention";

/// 一条不可变的人工干预请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterventionQueueItemV1 {
    /// 请求结构版本。
    pub schema_version: u32,
    /// 从来源身份确定性派生的请求 ID。
    pub intervention_id: String,
    /// 来源 Episode。
    pub episode_id: EpisodeId,
    /// Episode 的可信终态。
    pub outcome: Outcome,
    /// 必须由人工或外部控制面处理的处置。
    pub disposition: FailureDisposition,
    /// 聚合后的 Issue；旧 Outbox 记录可能没有该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<EvolutionIssueId>,
    /// Issue 当前诊断状态。
    pub issue_status: DiagnosticStatus,
    /// 可信 Pipeline 已知的失败类别；旧 Outbox 迁移时可能未知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<FailureKind>,
    /// 迁移来源的旧 Evolution Outbox ID；新请求为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_outbox_id: Option<String>,
    /// Unix 毫秒时间戳。
    pub created_at_ms: u64,
}

impl InterventionQueueItemV1 {
    /// 从可信路由字段构造确定性人工干预请求。
    ///
    /// 相同来源字段始终得到相同 ID，允许崩溃恢复重试安全地复用已提交记录。
    ///
    /// # Errors
    ///
    /// 处置不属于人工队列、旧 Outbox ID 不安全或生成的请求不满足结构约束时返回错误。
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        episode_id: EpisodeId,
        outcome: Outcome,
        disposition: FailureDisposition,
        issue_id: Option<EvolutionIssueId>,
        issue_status: DiagnosticStatus,
        failure_kind: Option<FailureKind>,
        legacy_outbox_id: Option<String>,
        created_at_ms: u64,
    ) -> Result<Self, InterventionQueueError> {
        validate_disposition(disposition)?;
        if let Some(value) = legacy_outbox_id.as_deref() {
            validate_control_id(value)?;
        }
        let intervention_id = deterministic_intervention_id(
            &episode_id,
            disposition,
            issue_id.as_ref(),
            failure_kind,
            legacy_outbox_id.as_deref(),
        );
        let item = Self {
            schema_version: INTERVENTION_QUEUE_SCHEMA_VERSION,
            intervention_id,
            episode_id,
            outcome,
            disposition,
            issue_id,
            issue_status,
            failure_kind,
            legacy_outbox_id,
            created_at_ms,
        };
        item.validate()?;
        Ok(item)
    }

    /// 校验版本、确定性身份和人工处置白名单。
    ///
    /// # Errors
    ///
    /// Schema、ID、处置或旧 Outbox 绑定不合法时返回错误。
    pub fn validate(&self) -> Result<(), InterventionQueueError> {
        if self.schema_version != INTERVENTION_QUEUE_SCHEMA_VERSION {
            return Err(InterventionQueueError::UnsupportedSchema {
                found: self.schema_version,
                supported: INTERVENTION_QUEUE_SCHEMA_VERSION,
            });
        }
        validate_control_id(&self.intervention_id)?;
        validate_disposition(self.disposition)?;
        if let Some(value) = self.legacy_outbox_id.as_deref() {
            validate_control_id(value)?;
        }
        let expected = deterministic_intervention_id(
            &self.episode_id,
            self.disposition,
            self.issue_id.as_ref(),
            self.failure_kind,
            self.legacy_outbox_id.as_deref(),
        );
        if self.intervention_id != expected {
            return Err(InterventionQueueError::IdentityMismatch);
        }
        Ok(())
    }
}

/// 只追加人工干预队列接口。
#[async_trait]
pub trait InterventionQueue: Send + Sync {
    /// 幂等追加一条不可变请求；同 ID 不同内容必须拒绝。
    ///
    /// # Errors
    ///
    /// 请求无效、同 ID 冲突或文件系统失败时返回错误。
    async fn append(&self, item: &InterventionQueueItemV1) -> Result<(), InterventionQueueError>;

    /// 返回全部未解决请求，按创建时间和 ID 稳定排序。
    ///
    /// # Errors
    ///
    /// 路径、记录、大小或解决标记无效时返回错误。
    async fn pending(&self) -> Result<Vec<InterventionQueueItemV1>, InterventionQueueError>;

    /// 通过独立标记把已存在请求记为解决；重复调用保持幂等。
    ///
    /// 不存在的请求返回 `Ok(false)`，原始请求文件永不修改。
    ///
    /// # Errors
    ///
    /// ID、路径或文件系统操作无效时返回错误。
    async fn mark_resolved(&self, intervention_id: &str) -> Result<bool, InterventionQueueError>;
}

/// 文件系统上的只追加人工干预队列。
#[derive(Debug, Clone)]
pub struct FileInterventionQueue {
    root: PathBuf,
}

impl FileInterventionQueue {
    /// 创建延迟初始化的人工干预队列。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回队列根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn item_path(&self, intervention_id: &str) -> PathBuf {
        self.root
            .join(format!("{INTERVENTION_PREFIX}-{intervention_id}.json"))
    }

    fn resolved_path(&self, intervention_id: &str) -> PathBuf {
        self.root
            .join(format!("{INTERVENTION_PREFIX}-{intervention_id}.resolved"))
    }
}

#[async_trait]
impl InterventionQueue for FileInterventionQueue {
    async fn append(&self, item: &InterventionQueueItemV1) -> Result<(), InterventionQueueError> {
        item.validate()?;
        ensure_safe_root(&self.root).await?;
        let bytes = serde_json::to_vec_pretty(item)
            .map_err(|source| InterventionQueueError::Serialization { source })?;
        enforce_size(bytes.len() as u64)?;
        let path = self.item_path(&item.intervention_id);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                file.write_all(&bytes)
                    .await
                    .map_err(|source| io_error("写入人工干预请求", &path, source))?;
                file.sync_all()
                    .await
                    .map_err(|source| io_error("同步人工干预请求", &path, source))?;
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let observed = read_item(&path).await?;
                if observed != *item {
                    return Err(InterventionQueueError::AlreadyExistsConflict(path));
                }
            }
            Err(source) => return Err(io_error("创建人工干预请求", &path, source)),
        }
        let observed = read_item(&path).await?;
        if observed != *item {
            return Err(InterventionQueueError::CommitVerificationFailed(path));
        }
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<InterventionQueueItemV1>, InterventionQueueError> {
        let metadata = match fs::symlink_metadata(&self.root).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("检查人工干预目录", &self.root, source)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(InterventionQueueError::UnsafePath(self.root.clone()));
        }
        let mut directory = fs::read_dir(&self.root)
            .await
            .map_err(|source| io_error("遍历人工干预目录", &self.root, source))?;
        let mut items = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|source| io_error("读取人工干预目录项", &self.root, source))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let item = read_item(&path).await?;
            if self.item_path(&item.intervention_id) != path {
                return Err(InterventionQueueError::UnsafePath(path));
            }
            let marker = self.resolved_path(&item.intervention_id);
            match fs::symlink_metadata(&marker).await {
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => items.push(item),
                Err(source) => return Err(io_error("检查人工干预解决标记", &marker, source)),
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(InterventionQueueError::UnsafePath(marker));
                }
                Ok(_) => {}
            }
        }
        items.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.intervention_id.cmp(&right.intervention_id))
        });
        Ok(items)
    }

    async fn mark_resolved(&self, intervention_id: &str) -> Result<bool, InterventionQueueError> {
        validate_control_id(intervention_id)?;
        ensure_safe_root(&self.root).await?;
        let item_path = self.item_path(intervention_id);
        match fs::symlink_metadata(&item_path).await {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(io_error("检查人工干预请求", &item_path, source)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(InterventionQueueError::UnsafePath(item_path));
            }
            Ok(_) => {}
        }
        let marker = self.resolved_path(intervention_id);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .await
        {
            Ok(file) => file
                .sync_all()
                .await
                .map_err(|source| io_error("同步人工干预解决标记", &marker, source))?,
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&marker)
                    .await
                    .map_err(|source| io_error("检查人工干预解决标记", &marker, source))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(InterventionQueueError::UnsafePath(marker));
                }
            }
            Err(source) => return Err(io_error("创建人工干预解决标记", &marker, source)),
        }
        Ok(true)
    }
}

/// 人工干预队列错误。
#[derive(Debug, thiserror::Error)]
pub enum InterventionQueueError {
    /// 请求 schema 不受支持。
    #[error("人工干预请求 schema 不受支持：实际 {found}，支持 {supported}")]
    UnsupportedSchema {
        /// 实际版本。
        found: u32,
        /// 支持版本。
        supported: u32,
    },
    /// 处置不属于人工干预队列。
    #[error("处置不能进入人工干预队列：{0:?}")]
    InvalidDisposition(FailureDisposition),
    /// 控制面 ID 不能安全用作单段文件名。
    #[error("人工干预控制 ID 无效：{0}")]
    InvalidId(String),
    /// 请求 ID 与来源字段的确定性摘要不一致。
    #[error("人工干预请求确定性 ID 不匹配")]
    IdentityMismatch,
    /// 请求 JSON 编码失败。
    #[error("序列化人工干预请求失败：{source}")]
    Serialization {
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 请求 JSON 损坏。
    #[error("人工干预请求损坏：{path}: {source}")]
    InvalidRecord {
        /// 损坏文件路径。
        path: PathBuf,
        /// 原始 JSON 错误。
        #[source]
        source: serde_json::Error,
    },
    /// 请求超过固定字节上限。
    #[error("人工干预请求过大：{actual} 字节，上限 {maximum} 字节")]
    TooLarge {
        /// 实际字节数。
        actual: u64,
        /// 固定上限。
        maximum: u64,
    },
    /// 路径包含符号链接或意外文件类型。
    #[error("人工干预队列路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// 同一确定性 ID 已绑定其他内容。
    #[error("人工干预请求 ID 已绑定其他内容：{0}")]
    AlreadyExistsConflict(PathBuf),
    /// 写入后复读内容不一致。
    #[error("人工干预请求提交后验证失败：{0}")]
    CommitVerificationFailed(PathBuf),
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

/// 判断处置是否必须进入人工干预队列。
pub(crate) fn is_intervention_disposition(disposition: FailureDisposition) -> bool {
    matches!(
        disposition,
        FailureDisposition::ManualReview
            | FailureDisposition::PlatformEngineering
            | FailureDisposition::PluginMaintenance
            | FailureDisposition::SecurityIncident
            | FailureDisposition::InfrastructureOperations
    )
}

fn validate_disposition(disposition: FailureDisposition) -> Result<(), InterventionQueueError> {
    if !is_intervention_disposition(disposition) {
        return Err(InterventionQueueError::InvalidDisposition(disposition));
    }
    Ok(())
}

fn deterministic_intervention_id(
    episode_id: &EpisodeId,
    disposition: FailureDisposition,
    issue_id: Option<&EvolutionIssueId>,
    failure_kind: Option<FailureKind>,
    legacy_outbox_id: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    for value in [
        "lucia-intervention-v1".to_string(),
        episode_id.to_string(),
        format!("{disposition:?}"),
        issue_id.map(ToString::to_string).unwrap_or_default(),
        failure_kind
            .map(|value| format!("{value:?}"))
            .unwrap_or_default(),
        legacy_outbox_id.unwrap_or_default().to_string(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("int_{:x}", hasher.finalize())
}

fn validate_control_id(value: &str) -> Result<(), InterventionQueueError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(InterventionQueueError::InvalidId(
            value.chars().take(64).collect(),
        ));
    }
    Ok(())
}

async fn ensure_safe_root(root: &Path) -> Result<(), InterventionQueueError> {
    fs::create_dir_all(root)
        .await
        .map_err(|source| io_error("创建人工干预目录", root, source))?;
    let metadata = fs::symlink_metadata(root)
        .await
        .map_err(|source| io_error("检查人工干预目录", root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InterventionQueueError::UnsafePath(root.to_path_buf()));
    }
    Ok(())
}

async fn read_item(path: &Path) -> Result<InterventionQueueItemV1, InterventionQueueError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error("检查人工干预请求", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InterventionQueueError::UnsafePath(path.to_path_buf()));
    }
    enforce_size(metadata.len())?;
    let bytes = fs::read(path)
        .await
        .map_err(|source| io_error("读取人工干预请求", path, source))?;
    let item: InterventionQueueItemV1 =
        serde_json::from_slice(&bytes).map_err(|source| InterventionQueueError::InvalidRecord {
            path: path.to_path_buf(),
            source,
        })?;
    item.validate()?;
    Ok(item)
}

fn enforce_size(actual: u64) -> Result<(), InterventionQueueError> {
    if actual > MAX_INTERVENTION_QUEUE_ITEM_BYTES {
        return Err(InterventionQueueError::TooLarge {
            actual,
            maximum: MAX_INTERVENTION_QUEUE_ITEM_BYTES,
        });
    }
    Ok(())
}

fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> InterventionQueueError {
    InterventionQueueError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-intervention-{}", Uuid::new_v4().simple()))
    }

    fn item(disposition: FailureDisposition) -> InterventionQueueItemV1 {
        InterventionQueueItemV1::create(
            EpisodeId::generate(),
            Outcome::TaskFailure,
            disposition,
            Some(EvolutionIssueId::generate()),
            DiagnosticStatus::Confirmed,
            Some(FailureKind::PluginFailure),
            None,
            10,
        )
        .expect("人工处置应合法")
    }

    /// 五类人工处置必须进入队列，自动处置必须全部拒绝。
    #[test]
    fn accepts_only_intervention_dispositions() {
        for disposition in [
            FailureDisposition::ManualReview,
            FailureDisposition::PlatformEngineering,
            FailureDisposition::PluginMaintenance,
            FailureDisposition::SecurityIncident,
            FailureDisposition::InfrastructureOperations,
        ] {
            item(disposition).validate().expect("人工处置应通过");
        }
        for disposition in [
            FailureDisposition::EvolutionCandidate,
            FailureDisposition::Observe,
            FailureDisposition::Ignore,
            FailureDisposition::RetryInTurn,
        ] {
            assert!(matches!(
                InterventionQueueItemV1::create(
                    EpisodeId::generate(),
                    Outcome::TaskFailure,
                    disposition,
                    None,
                    DiagnosticStatus::Observed,
                    None,
                    None,
                    1,
                ),
                Err(InterventionQueueError::InvalidDisposition(actual)) if actual == disposition
            ));
        }
    }

    /// 相同来源必须幂等复用请求，解决标记不能改写原始记录。
    #[tokio::test]
    async fn append_is_idempotent_and_resolution_is_additive() {
        let root = temp_root();
        let queue = FileInterventionQueue::new(root.join("interventions"));
        let request = item(FailureDisposition::PluginMaintenance);
        queue.append(&request).await.expect("首次追加应成功");
        queue.append(&request).await.expect("相同请求应幂等成功");
        let path = queue.item_path(&request.intervention_id);
        let before = fs::read(&path).await.expect("应读取原始请求");
        assert_eq!(
            queue.pending().await.expect("应读取队列"),
            vec![request.clone()]
        );
        assert!(queue
            .mark_resolved(&request.intervention_id)
            .await
            .expect("应标记解决"));
        assert!(queue.pending().await.expect("应重新读取").is_empty());
        assert_eq!(fs::read(path).await.expect("应复读原始请求"), before);
        let _ = fs::remove_dir_all(root).await;
    }

    /// 确定性 ID 冲突、符号链接和超大记录必须失败关闭。
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_conflict_symlink_and_oversized_record() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let queue = FileInterventionQueue::new(root.join("interventions"));
        let request = item(FailureDisposition::SecurityIncident);
        queue.append(&request).await.expect("首次追加应成功");
        let mut conflict = request.clone();
        conflict.created_at_ms += 1;
        assert!(matches!(
            queue.append(&conflict).await,
            Err(InterventionQueueError::AlreadyExistsConflict(_))
        ));

        let outside = root.join("outside.json");
        fs::write(&outside, b"{}").await.expect("应写入外部文件");
        let link = queue.root().join("intervention-link.json");
        symlink(&outside, &link).expect("应创建符号链接");
        assert!(matches!(
            queue.pending().await,
            Err(InterventionQueueError::UnsafePath(path)) if path == link
        ));

        let oversized = root.join("oversized.json");
        fs::write(
            &oversized,
            vec![b'x'; MAX_INTERVENTION_QUEUE_ITEM_BYTES as usize + 1],
        )
        .await
        .expect("应写入超大记录");
        assert!(matches!(
            read_item(&oversized).await,
            Err(InterventionQueueError::TooLarge { .. })
        ));
        let _ = fs::remove_dir_all(root).await;
    }
}
