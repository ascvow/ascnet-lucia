//! M8 插件跨进程部署状态与旧 Stable bundle 的安全持久化。

use crate::{PluginCanaryAdmissionV1, PluginCanaryDeployment};
use agent_evolution::{ArtifactStore, ArtifactStoreError, FileArtifactStore, StableGenomeRef};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, CandidateId, GenomeDigest, GenomeRevision, GenomeRevisionId,
    MutationId, PluginCanaryState, ReleaseId,
};
use agent_plugin_manager::InstalledPlugin;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self as std_fs, OpenOptions as StdOpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{Mutex, MutexGuard};

/// 第一版插件部署状态记录 schema。
pub const PLUGIN_CANARY_DEPLOYMENT_RECORD_SCHEMA_VERSION: u32 = 1;
/// 旧 Stable bundle 在 Artifact CAS 中使用的媒体类型。
pub const PREVIOUS_PLUGIN_BUNDLE_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.previous-plugin-bundle.v1";

/// 跨 Store 句柄和进程文件锁之前的进程内事务门禁。
static DEPLOYMENT_TRANSACTION_GUARD: Mutex<()> = Mutex::const_new(());
/// 进程内只追加临时文件序号，避免依赖额外随机数 crate。
static APPEND_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 一次 Canary 生产部署的强类型 ID。
///
/// 每个 Canary Release 最多对应一次生产部署，因此 ID 透明包装 Canary `ReleaseId`，不会
/// 引入可与 Release 错绑的第二个随机身份。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginDeploymentId(ReleaseId);

impl PluginDeploymentId {
    /// 从已验证 Canary Release ID 构造确定性部署 ID。
    pub fn for_canary_release(release_id: ReleaseId) -> Self {
        Self(release_id)
    }

    /// 返回部署绑定的 Canary Release ID。
    pub fn canary_release_id(&self) -> &ReleaseId {
        &self.0
    }

    /// 返回用于日志和稳定寻址的强类型 ID 文本。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for PluginDeploymentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 持久化部署状态机的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginDeploymentStateV1 {
    /// 部署身份与旧 bundle 已在真实安装副作用前落盘。
    Planned,
    /// Candidate 已由 Plugin Manager 安装并通过安装后复核。
    CanaryInstalled,
    /// Candidate 已完成 Stable Promotion。
    Promoted,
    /// Canary 已恢复部署前的旧 Stable bundle。
    RolledBack,
}

impl PluginDeploymentStateV1 {
    /// 返回状态文件的固定只追加文件名。
    fn file_name(self) -> &'static str {
        match self {
            Self::Planned => "00-planned.json",
            Self::CanaryInstalled => "01-canary-installed.json",
            Self::Promoted => "02-promoted.json",
            Self::RolledBack => "02-rolled-back.json",
        }
    }

    /// 返回状态历史的稳定排序等级。
    fn rank(self) -> u8 {
        match self {
            Self::Planned => 0,
            Self::CanaryInstalled => 1,
            Self::Promoted | Self::RolledBack => 2,
        }
    }

    /// 判断当前状态是否为不可重复追加的部署终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Promoted | Self::RolledBack)
    }
}

/// 不含原始 bundle 的部署身份与摘要绑定。
///
/// 控制器可在真实 `replace` 之前从已验证 Admission 与 Parent/Candidate Revision 构造该值，
/// 再由 Store 把旧 bundle 字节写入 CAS。当前安装摘要属于状态快照，不进入稳定身份绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCanaryDeploymentBindingV1 {
    /// 确定性部署 ID。
    pub deployment_id: PluginDeploymentId,
    /// 当前 Canary Release ID。
    pub canary_release_id: ReleaseId,
    /// 当前 Mutation ID。
    pub mutation_id: MutationId,
    /// 当前 Candidate ID。
    pub candidate_id: CandidateId,
    /// 部署开始时完整、可跨进程复核的 Parent Stable 引用。
    pub parent_stable: StableGenomeRef,
    /// 部署前 Parent Genome Revision ID。
    pub parent_revision_id: GenomeRevisionId,
    /// 部署前 Parent Genome 行为摘要。
    pub parent_revision_digest: GenomeDigest,
    /// Candidate Genome Revision ID。
    pub candidate_revision_id: GenomeRevisionId,
    /// Candidate Genome 行为摘要。
    pub candidate_revision_digest: GenomeDigest,
    /// 完整 Canary Admission 的规范摘要。
    pub admission_digest: ArtifactDigest,
}

impl PluginCanaryDeploymentBindingV1 {
    /// 在真实 Candidate 安装前从发布计划构造稳定部署绑定。
    ///
    /// # Errors
    ///
    /// Admission、Revision 或 Parent/Candidate lineage 发生错绑时返回错误。
    pub fn from_plan(
        admission: &PluginCanaryAdmissionV1,
        parent: &StableGenomeRef,
        candidate: &GenomeRevision,
    ) -> Result<Self, PluginDeploymentStoreError> {
        binding_from_parts(admission, parent, candidate)
    }

    /// 从已完成真实安装的内存部署对象提取稳定持久化绑定。
    ///
    /// # Errors
    ///
    /// Admission、Revision、安装记录或 Parent/Candidate lineage 发生错绑时返回错误。
    pub fn from_deployment(
        deployment: &PluginCanaryDeployment,
    ) -> Result<Self, PluginDeploymentStoreError> {
        let binding = Self::from_plan(
            deployment.admission(),
            deployment.parent_stable(),
            deployment.candidate_revision(),
        )?;
        validate_installed(
            deployment.admission(),
            deployment.candidate_revision(),
            deployment.installed(),
        )?;
        Ok(binding)
    }

    /// 校验强类型部署身份和 Parent/Candidate 修订边界。
    ///
    /// # Errors
    ///
    /// Deployment ID 未绑定 Canary Release，或 Parent/Candidate 使用同一 Revision 时返回错误。
    pub fn validate(&self) -> Result<(), PluginDeploymentStoreError> {
        if self.deployment_id.canary_release_id() != &self.canary_release_id {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "deployment_id 与 canary_release_id 不一致",
            ));
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "Parent 与 Candidate Revision ID 不得相同",
            ));
        }
        self.parent_stable
            .validate()
            .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))?;
        if self.parent_stable.revision_id != self.parent_revision_id
            || self.parent_stable.digest != self.parent_revision_digest
        {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "Parent Stable 与 Parent Revision 摘要不一致",
            ));
        }
        Ok(())
    }
}

/// Store 可持久化的内存 Canary 部署只读视图。
///
/// 生产实现由 [`PluginCanaryDeployment`] 提供；测试或其他可信控制器可实现同一契约，但
/// Store 仍会把返回值与已有 Planned 记录及 Artifact CAS 重新绑定。
pub trait PluginCanaryDeploymentPersistenceView {
    /// 返回不随部署阶段改变的完整身份与摘要绑定。
    ///
    /// # Errors
    ///
    /// 内存对象内部身份不一致时返回错误。
    fn persistence_binding(
        &self,
    ) -> Result<PluginCanaryDeploymentBindingV1, PluginDeploymentStoreError>;

    /// 返回 Plugin Manager 在 Candidate 安装后生成的安装记录。
    fn installed(&self) -> &InstalledPlugin;

    /// 返回安装 Candidate 前归档的旧 Stable bundle 原始字节。
    fn previous_bundle_bytes(&self) -> &[u8];
}

impl PluginCanaryDeploymentPersistenceView for PluginCanaryDeployment {
    fn persistence_binding(
        &self,
    ) -> Result<PluginCanaryDeploymentBindingV1, PluginDeploymentStoreError> {
        PluginCanaryDeploymentBindingV1::from_deployment(self)
    }

    fn installed(&self) -> &InstalledPlugin {
        self.installed()
    }

    fn previous_bundle_bytes(&self) -> &[u8] {
        self.previous_bundle()
    }
}

/// 第一版只追加 Canary 部署状态快照。
///
/// 记录只包含强类型身份、Parent/Candidate Revision 绑定、Admission/安装摘要与旧 bundle
/// `ArtifactRef`；原始 bundle 始终只保存在 `FileArtifactStore`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCanaryDeploymentRecordV1 {
    /// 部署记录 schema 版本。
    pub schema_version: u32,
    /// 确定性部署 ID。
    pub deployment_id: PluginDeploymentId,
    /// 当前 Canary Release ID。
    pub canary_release_id: ReleaseId,
    /// 当前 Mutation ID。
    pub mutation_id: MutationId,
    /// 当前 Candidate ID。
    pub candidate_id: CandidateId,
    /// 部署开始时完整固定的 Parent Stable 引用。
    ///
    /// 该字段是 V1 的加法扩展；旧记录反序列化为空并在生产恢复校验时失败关闭。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_stable: Option<StableGenomeRef>,
    /// 部署前 Parent Genome Revision ID。
    pub parent_revision_id: GenomeRevisionId,
    /// 部署前 Parent Genome 行为摘要。
    pub parent_revision_digest: GenomeDigest,
    /// Candidate Genome Revision ID。
    pub candidate_revision_id: GenomeRevisionId,
    /// Candidate Genome 行为摘要。
    pub candidate_revision_digest: GenomeDigest,
    /// 完整 Canary Admission 的规范摘要。
    pub admission_digest: ArtifactDigest,
    /// 当前 Plugin Manager 可见 bundle 的内容摘要。
    ///
    /// Planned/RolledBack 指向旧 Stable bundle，CanaryInstalled/Promoted 指向 Candidate。
    pub installed_digest: ArtifactDigest,
    /// 部署前旧 Stable bundle 的不可变 CAS 引用。
    pub previous_bundle: ArtifactRef,
    /// 当前只追加部署阶段。
    pub state: PluginDeploymentStateV1,
}

impl PluginCanaryDeploymentRecordV1 {
    /// 返回不含状态和 CAS 物理位置的部署身份绑定。
    ///
    /// # Errors
    ///
    /// 旧 V1 记录缺少恢复所需的 Parent Stable 加法字段时失败关闭。
    pub fn binding(&self) -> Result<PluginCanaryDeploymentBindingV1, PluginDeploymentStoreError> {
        Ok(PluginCanaryDeploymentBindingV1 {
            deployment_id: self.deployment_id.clone(),
            canary_release_id: self.canary_release_id.clone(),
            mutation_id: self.mutation_id.clone(),
            candidate_id: self.candidate_id.clone(),
            parent_stable: self.parent_stable.clone().ok_or(
                PluginDeploymentStoreError::BindingMismatch("部署记录缺少 Parent Stable 加法字段"),
            )?,
            parent_revision_id: self.parent_revision_id.clone(),
            parent_revision_digest: self.parent_revision_digest.clone(),
            candidate_revision_id: self.candidate_revision_id.clone(),
            candidate_revision_digest: self.candidate_revision_digest.clone(),
            admission_digest: self.admission_digest.clone(),
        })
    }

    /// 校验 schema、强类型身份、Revision 绑定与旧 bundle CAS 元数据。
    ///
    /// # Errors
    ///
    /// schema、部署身份、媒体类型、bundle 长度或 Revision lineage 无效时返回错误。
    pub fn validate(&self) -> Result<(), PluginDeploymentStoreError> {
        if self.schema_version != PLUGIN_CANARY_DEPLOYMENT_RECORD_SCHEMA_VERSION {
            return Err(PluginDeploymentStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        self.binding()?.validate()?;
        if self.previous_bundle.media_type != PREVIOUS_PLUGIN_BUNDLE_MEDIA_TYPE
            || self.previous_bundle.size_bytes == 0
        {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "旧 bundle ArtifactRef 的媒体类型或长度无效",
            ));
        }
        let digest_matches_state = match self.state {
            PluginDeploymentStateV1::Planned | PluginDeploymentStateV1::RolledBack => {
                self.installed_digest == self.previous_bundle.digest
            }
            PluginDeploymentStateV1::CanaryInstalled | PluginDeploymentStateV1::Promoted => true,
        };
        if !digest_matches_state {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "当前安装摘要与部署阶段不一致",
            ));
        }
        Ok(())
    }
}

/// 基于不可变 Artifact CAS 和安全只追加状态文件的插件部署 Store。
#[derive(Debug, Clone)]
pub struct FilePluginDeploymentStore<'a> {
    root: PathBuf,
    artifacts: &'a FileArtifactStore,
    write_guard: Arc<Mutex<()>>,
}

/// 跨越一次真实部署副作用窗口的 Store 事务租约。
///
/// 租约同时持有进程内门禁、Store 全局文件锁和单 Deployment 文件锁。只有生产部署控制器
/// 可以取得该租约，避免 Manager replace、Stable publish 与状态追加被其他进程交叉执行。
pub(crate) struct PluginDeploymentTransaction<'store, 'artifacts> {
    store: &'store FilePluginDeploymentStore<'artifacts>,
    deployment_id: PluginDeploymentId,
    _store_guard: MutexGuard<'store, ()>,
    _transaction_guard: MutexGuard<'static, ()>,
    _global_file_lock: DeploymentFileLock,
    _deployment_file_lock: DeploymentFileLock,
}

impl<'a> FilePluginDeploymentStore<'a> {
    /// 创建部署 Store，并把根目录固定到已规范化的真实目录。
    ///
    /// # Errors
    ///
    /// 根路径不是绝对路径、包含点号跳转、目标是符号链接/非目录或无法安全创建时返回错误。
    pub fn new(
        root: impl Into<PathBuf>,
        artifacts: &'a FileArtifactStore,
    ) -> Result<Self, PluginDeploymentStoreError> {
        Ok(Self {
            root: prepare_store_root(root.into())?,
            artifacts,
            write_guard: Arc::new(Mutex::new(())),
        })
    }

    /// 返回已规范化的部署状态根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 在真实 Candidate 安装前写入 Planned 状态和旧 bundle CAS 引用。
    ///
    /// 相同内容重试保持幂等；同一 Deployment ID 已绑定其他内容或已有后续状态时拒绝。
    ///
    /// # Errors
    ///
    /// 绑定无效、旧 bundle 为空、CAS 写入失败、历史冲突或文件路径不安全时返回错误。
    pub async fn append_planned(
        &self,
        binding: &PluginCanaryDeploymentBindingV1,
        previous_bundle: &[u8],
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        let transaction = self.transaction(&binding.deployment_id).await?;
        transaction.append_planned(binding, previous_bundle).await
    }

    /// 从真实安装完成的内存 Canary 对象安全追加 CanaryInstalled 状态。
    ///
    /// Store 会重新写入并复核旧 bundle CAS，且要求磁盘上已经存在内容完全一致的 Planned
    /// 记录，防止安装后对象被错绑到其他跨进程部署。
    ///
    /// # Errors
    ///
    /// 内存对象无效、缺少 Planned、CAS 旧 bundle 或任一摘要错绑、状态非法时返回错误。
    pub async fn append_canary_installed<V: PluginCanaryDeploymentPersistenceView + ?Sized>(
        &self,
        deployment: &V,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        let binding = deployment.persistence_binding()?;
        let transaction = self.transaction(&binding.deployment_id).await?;
        transaction.append_canary_installed(deployment).await
    }

    /// 把已安装 Canary 追加为 Promoted 终态。
    ///
    /// # Errors
    ///
    /// Deployment 不存在、尚未安装、已进入任一终态或磁盘历史无效时返回错误。
    pub async fn mark_promoted(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        let transaction = self.transaction(deployment_id).await?;
        transaction.mark_promoted().await
    }

    /// 把已安装 Canary 追加为 RolledBack 终态。
    ///
    /// # Errors
    ///
    /// Deployment 不存在、尚未安装、已进入任一终态或磁盘历史无效时返回错误。
    pub async fn mark_rolled_back(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        let transaction = self.transaction(deployment_id).await?;
        transaction.mark_rolled_back().await
    }

    /// 按 Deployment ID 重新加载、验证并返回最新状态。
    ///
    /// # Errors
    ///
    /// 状态目录、历史迁移、身份摘要或旧 bundle CAS 引用无效时返回错误。
    pub async fn load(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<Option<PluginCanaryDeploymentRecordV1>, PluginDeploymentStoreError> {
        let history = self.history(deployment_id).await?;
        Ok(history.last().cloned())
    }

    /// 读取并复核指定 Deployment 的完整只追加历史。
    ///
    /// # Errors
    ///
    /// 文件名、JSON、CAS、身份或状态迁移不合法时返回错误。
    pub async fn history(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<Vec<PluginCanaryDeploymentRecordV1>, PluginDeploymentStoreError> {
        let transaction = self.transaction(deployment_id).await?;
        transaction.history().await
    }

    /// 从 CAS 读取并重新验证记录引用的旧 Stable bundle。
    ///
    /// # Errors
    ///
    /// 记录无效、CAS 制品缺失、媒体类型或长度不匹配时返回错误。
    pub async fn previous_bundle(
        &self,
        record: &PluginCanaryDeploymentRecordV1,
    ) -> Result<Vec<u8>, PluginDeploymentStoreError> {
        record.validate()?;
        self.verify_previous_bundle(&record.previous_bundle).await
    }

    /// 获取跨越真实部署副作用窗口的全局排他事务租约。
    pub(crate) async fn transaction(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<PluginDeploymentTransaction<'_, 'a>, PluginDeploymentStoreError> {
        let store_guard = self.write_guard.lock().await;
        let transaction_guard = DEPLOYMENT_TRANSACTION_GUARD.lock().await;
        let global_file_lock = self.acquire_global_lock().await?;
        let deployment_file_lock = self.acquire_deployment_lock(deployment_id).await?;
        Ok(PluginDeploymentTransaction {
            store: self,
            deployment_id: deployment_id.clone(),
            _store_guard: store_guard,
            _transaction_guard: transaction_guard,
            _global_file_lock: global_file_lock,
            _deployment_file_lock: deployment_file_lock,
        })
    }

    /// 在持有进程锁和 Deployment 文件锁时读取历史。
    async fn history_unlocked(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<Vec<PluginCanaryDeploymentRecordV1>, PluginDeploymentStoreError> {
        let directory = self.deployment_directory(deployment_id);
        let mut records = read_state_directory(&self.root, &directory).await?;
        records.sort_by_key(|record| record.state.rank());
        validate_history(&records, deployment_id)?;
        for record in &records {
            self.verify_previous_bundle(&record.previous_bundle).await?;
        }
        Ok(records)
    }

    /// 读取并复核旧 bundle CAS 引用。
    async fn verify_previous_bundle(
        &self,
        reference: &ArtifactRef,
    ) -> Result<Vec<u8>, PluginDeploymentStoreError> {
        if reference.media_type != PREVIOUS_PLUGIN_BUNDLE_MEDIA_TYPE || reference.size_bytes == 0 {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "旧 bundle ArtifactRef 无效",
            ));
        }
        let bytes = self
            .artifacts
            .get(&reference.digest)
            .await?
            .ok_or_else(|| PluginDeploymentStoreError::MissingArtifact(reference.digest.clone()))?;
        if bytes.len() as u64 != reference.size_bytes {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "旧 bundle CAS 长度与 ArtifactRef 不一致",
            ));
        }
        Ok(bytes)
    }

    /// 返回 Deployment 状态目录的摘要寻址路径。
    fn deployment_directory(&self, deployment_id: &PluginDeploymentId) -> PathBuf {
        self.root.join("deployments").join(format!(
            "{:x}",
            Sha256::digest(deployment_id.as_str().as_bytes())
        ))
    }

    /// 获取单个 Deployment 的跨进程排他锁。
    async fn acquire_deployment_lock(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<DeploymentFileLock, PluginDeploymentStoreError> {
        let path = self.root.join("locks").join(format!(
            "{:x}.lock",
            Sha256::digest(deployment_id.as_str().as_bytes())
        ));
        let parent = path
            .parent()
            .ok_or_else(|| PluginDeploymentStoreError::UnsafePath(path.clone()))?;
        ensure_store_directory(&self.root, parent).await?;
        acquire_file_lock(path).await
    }

    /// 获取覆盖全部生产部署副作用的跨进程全局排他锁。
    async fn acquire_global_lock(&self) -> Result<DeploymentFileLock, PluginDeploymentStoreError> {
        let path = self.root.join("locks").join("deployment-global.lock");
        let parent = path
            .parent()
            .ok_or_else(|| PluginDeploymentStoreError::UnsafePath(path.clone()))?;
        ensure_store_directory(&self.root, parent).await?;
        acquire_file_lock(path).await
    }
}

impl PluginDeploymentTransaction<'_, '_> {
    /// 返回当前事务锁定的 Deployment ID。
    pub(crate) fn deployment_id(&self) -> &PluginDeploymentId {
        &self.deployment_id
    }

    /// 在当前排他事务内读取并复核完整部署历史。
    pub(crate) async fn history(
        &self,
    ) -> Result<Vec<PluginCanaryDeploymentRecordV1>, PluginDeploymentStoreError> {
        self.store.history_unlocked(self.deployment_id()).await
    }

    /// 在真实安装前原子追加 Planned 与旧 bundle CAS 引用。
    pub(crate) async fn append_planned(
        &self,
        binding: &PluginCanaryDeploymentBindingV1,
        previous_bundle: &[u8],
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        self.ensure_deployment_id(&binding.deployment_id)?;
        binding.validate()?;
        if previous_bundle.is_empty() {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "旧 Stable bundle 不能为空",
            ));
        }
        let artifact = self
            .store
            .artifacts
            .put(PREVIOUS_PLUGIN_BUNDLE_MEDIA_TYPE, previous_bundle)
            .await?;
        let installed_digest = artifact.digest.clone();
        let record = record(
            binding,
            artifact,
            installed_digest,
            PluginDeploymentStateV1::Planned,
        );
        self.append_record(&record, true).await?;
        Ok(record)
    }

    /// 从内存部署对象追加不可重复的 CanaryInstalled 快照。
    async fn append_canary_installed<V: PluginCanaryDeploymentPersistenceView + ?Sized>(
        &self,
        deployment: &V,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        let binding = deployment.persistence_binding()?;
        self.ensure_deployment_id(&binding.deployment_id)?;
        binding.validate()?;
        let installed_digest = installed_bundle_digest(deployment.installed())?;
        let previous_bundle = deployment.previous_bundle_bytes();
        if previous_bundle.is_empty() {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "内存部署的旧 Stable bundle 不能为空",
            ));
        }
        let artifact = self
            .store
            .artifacts
            .put(PREVIOUS_PLUGIN_BUNDLE_MEDIA_TYPE, previous_bundle)
            .await?;
        let record = record(
            &binding,
            artifact,
            installed_digest,
            PluginDeploymentStateV1::CanaryInstalled,
        );
        self.append_record(&record, false).await?;
        Ok(record)
    }

    /// 根据磁盘 Planned 与 Plugin Manager 当前安装幂等补记 CanaryInstalled。
    ///
    /// # Errors
    ///
    /// Planned 缺失、稳定身份/CAS 错绑、安装摘要无效或已进入终态时返回错误。
    pub(crate) async fn reconcile_canary_installed(
        &self,
        binding: &PluginCanaryDeploymentBindingV1,
        installed: &InstalledPlugin,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        self.ensure_deployment_id(&binding.deployment_id)?;
        binding.validate()?;
        let installed_digest = installed_bundle_digest(installed)?;
        let history = self.history().await?;
        let planned = history
            .first()
            .ok_or_else(|| PluginDeploymentStoreError::NotFound(binding.deployment_id.clone()))?;
        if planned.state != PluginDeploymentStateV1::Planned || planned.binding()? != *binding {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "Manager 安装结果未绑定磁盘 Planned",
            ));
        }
        let record = record(
            binding,
            planned.previous_bundle.clone(),
            installed_digest,
            PluginDeploymentStateV1::CanaryInstalled,
        );
        match history.last().map(|entry| entry.state) {
            Some(PluginDeploymentStateV1::Planned) => {
                self.append_record(&record, false).await?;
                Ok(record)
            }
            Some(PluginDeploymentStateV1::CanaryInstalled) if history.last() == Some(&record) => {
                Ok(record)
            }
            Some(PluginDeploymentStateV1::Promoted | PluginDeploymentStateV1::RolledBack) => {
                Err(PluginDeploymentStoreError::DuplicateTerminal {
                    current: history.last().expect("历史非空").state,
                    requested: PluginDeploymentStateV1::CanaryInstalled,
                })
            }
            _ => Err(PluginDeploymentStoreError::BindingMismatch(
                "CanaryInstalled 与既有安装摘要不一致",
            )),
        }
    }

    /// 在当前事务内读取并重新验证旧 Stable bundle。
    pub(crate) async fn previous_bundle(
        &self,
        record: &PluginCanaryDeploymentRecordV1,
    ) -> Result<Vec<u8>, PluginDeploymentStoreError> {
        self.ensure_deployment_id(&record.deployment_id)?;
        record.validate()?;
        self.store
            .verify_previous_bundle(&record.previous_bundle)
            .await
    }

    /// 在当前事务内追加 Promoted 终态。
    pub(crate) async fn mark_promoted(
        &self,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        self.append_terminal(PluginDeploymentStateV1::Promoted)
            .await
    }

    /// 在当前事务内追加 RolledBack 终态。
    pub(crate) async fn mark_rolled_back(
        &self,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        self.append_terminal(PluginDeploymentStateV1::RolledBack)
            .await
    }

    /// 返回事务绑定的 Deployment ID，并拒绝锁文件名异常。
    /// 确认调用目标与事务锁定的 Deployment 完全一致。
    fn ensure_deployment_id(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<(), PluginDeploymentStoreError> {
        if self.deployment_id().as_str() != deployment_id.as_str() {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "事务 Deployment ID 与请求不一致",
            ));
        }
        Ok(())
    }

    /// 追加一个已完成结构校验的状态快照。
    async fn append_record(
        &self,
        record: &PluginCanaryDeploymentRecordV1,
        allow_same_planned: bool,
    ) -> Result<(), PluginDeploymentStoreError> {
        self.ensure_deployment_id(&record.deployment_id)?;
        record.validate()?;
        self.store
            .verify_previous_bundle(&record.previous_bundle)
            .await?;
        let mut history = self.store.history_unlocked(&record.deployment_id).await?;
        if allow_same_planned
            && history.len() == 1
            && history[0] == *record
            && record.state == PluginDeploymentStateV1::Planned
        {
            return Ok(());
        }
        if history.iter().any(|existing| existing == record) {
            return Err(PluginDeploymentStoreError::DuplicateState(record.state));
        }
        validate_next_state(history.last(), record)?;
        if let Some(first) = history.first() {
            ensure_same_binding(first, record)?;
        }
        history.push(record.clone());
        validate_history(&history, &record.deployment_id)?;
        let directory = self.store.deployment_directory(&record.deployment_id);
        let path = directory.join(record.state.file_name());
        let bytes = serde_json::to_vec(record)?;
        write_create_new_or_same(&self.store.root, &path, &bytes).await
    }

    /// 从 CanaryInstalled 追加一个不可重复的部署终态。
    async fn append_terminal(
        &self,
        state: PluginDeploymentStateV1,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentStoreError> {
        debug_assert!(state.is_terminal());
        let deployment_id = self.deployment_id().clone();
        let mut history = self.store.history_unlocked(&deployment_id).await?;
        let previous = history
            .last()
            .ok_or_else(|| PluginDeploymentStoreError::NotFound(deployment_id.clone()))?;
        if previous.state.is_terminal() {
            return Err(PluginDeploymentStoreError::DuplicateTerminal {
                current: previous.state,
                requested: state,
            });
        }
        let mut next = previous.clone();
        next.state = state;
        if state == PluginDeploymentStateV1::RolledBack {
            next.installed_digest = next.previous_bundle.digest.clone();
        }
        validate_next_state(Some(previous), &next)?;
        history.push(next.clone());
        validate_history(&history, &deployment_id)?;
        let path = self
            .store
            .deployment_directory(&deployment_id)
            .join(state.file_name());
        let bytes = serde_json::to_vec(&next)?;
        write_create_new_or_same(&self.store.root, &path, &bytes).await?;
        Ok(next)
    }
}

/// 从已验证 Admission 与 Parent/Candidate Revision 重建稳定持久化绑定。
fn binding_from_parts(
    admission: &PluginCanaryAdmissionV1,
    parent: &StableGenomeRef,
    candidate: &GenomeRevision,
) -> Result<PluginCanaryDeploymentBindingV1, PluginDeploymentStoreError> {
    admission
        .release
        .release
        .validate()
        .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))?;
    admission
        .canary
        .validate()
        .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))?;
    parent
        .validate()
        .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))?;
    candidate
        .validate()
        .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))?;
    let release = &admission.release.release;
    if release.release_id != admission.canary.release_id
        || admission.canary.state != PluginCanaryState::Planned
        || release.mutation_id != admission.canary.mutation_id
        || release.candidate_id != admission.canary.candidate_id
        || candidate.metadata.parent.as_ref() != Some(&parent.revision_id)
        || candidate.metadata.mutation.as_ref() != Some(&release.mutation_id)
    {
        return Err(PluginDeploymentStoreError::BindingMismatch(
            "Canary 部署计划的 Admission 或 Revision 不一致",
        ));
    }
    let candidate_plugin = candidate
        .genome
        .plugins
        .iter()
        .find(|plugin| plugin.id == release.plugin_id)
        .ok_or(PluginDeploymentStoreError::BindingMismatch(
            "Candidate Revision 缺少目标插件",
        ))?;
    if candidate_plugin.bundle != release.bundle_digest {
        return Err(PluginDeploymentStoreError::BindingMismatch(
            "Candidate PluginGenome 与 Release bundle 不一致",
        ));
    }
    let canary_release_id = release.release_id.clone();
    let binding = PluginCanaryDeploymentBindingV1 {
        deployment_id: PluginDeploymentId::for_canary_release(canary_release_id.clone()),
        canary_release_id,
        mutation_id: release.mutation_id.clone(),
        candidate_id: release.candidate_id.clone(),
        parent_stable: parent.clone(),
        parent_revision_id: parent.revision_id.clone(),
        parent_revision_digest: parent.digest.clone(),
        candidate_revision_id: candidate.revision_id.clone(),
        candidate_revision_digest: candidate.digest.clone(),
        admission_digest: canonical_digest(
            "ascnet.lucia.plugin-canary-admission.v1",
            &(&admission.release, &admission.canary),
        )?,
    };
    binding.validate()?;
    Ok(binding)
}

/// 校验 Candidate 安装记录并返回其 bundle 内容摘要。
fn validate_installed(
    admission: &PluginCanaryAdmissionV1,
    candidate: &GenomeRevision,
    installed: &InstalledPlugin,
) -> Result<ArtifactDigest, PluginDeploymentStoreError> {
    let release = &admission.release.release;
    let installed_digest = installed_bundle_digest(installed)?;
    let candidate_plugin = candidate
        .genome
        .plugins
        .iter()
        .find(|plugin| plugin.id == release.plugin_id)
        .ok_or(PluginDeploymentStoreError::BindingMismatch(
            "Candidate Revision 缺少目标插件",
        ))?;
    if installed.id != release.plugin_id
        || installed_digest != release.bundle_digest
        || candidate_plugin.bundle != installed_digest
        || candidate_plugin.version != installed.version
        || candidate_plugin.api_version != installed.api_version
        || !installed.enabled
    {
        return Err(PluginDeploymentStoreError::BindingMismatch(
            "Candidate 安装记录与 Admission 或 Revision 不一致",
        ));
    }
    Ok(installed_digest)
}

/// 解析并验证 Plugin Manager 使用的不带算法前缀 SHA-256 摘要。
fn installed_bundle_digest(
    installed: &InstalledPlugin,
) -> Result<ArtifactDigest, PluginDeploymentStoreError> {
    if !installed.enabled {
        return Err(PluginDeploymentStoreError::BindingMismatch(
            "Candidate 安装后必须处于启用状态",
        ));
    }
    ArtifactDigest::from_sha256_hex(&installed.sha256)
        .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))
}

/// 从身份绑定、CAS 引用和阶段构造记录。
fn record(
    binding: &PluginCanaryDeploymentBindingV1,
    previous_bundle: ArtifactRef,
    installed_digest: ArtifactDigest,
    state: PluginDeploymentStateV1,
) -> PluginCanaryDeploymentRecordV1 {
    PluginCanaryDeploymentRecordV1 {
        schema_version: PLUGIN_CANARY_DEPLOYMENT_RECORD_SCHEMA_VERSION,
        deployment_id: binding.deployment_id.clone(),
        canary_release_id: binding.canary_release_id.clone(),
        mutation_id: binding.mutation_id.clone(),
        candidate_id: binding.candidate_id.clone(),
        parent_stable: Some(binding.parent_stable.clone()),
        parent_revision_id: binding.parent_revision_id.clone(),
        parent_revision_digest: binding.parent_revision_digest.clone(),
        candidate_revision_id: binding.candidate_revision_id.clone(),
        candidate_revision_digest: binding.candidate_revision_digest.clone(),
        admission_digest: binding.admission_digest.clone(),
        installed_digest,
        previous_bundle,
        state,
    }
}

/// 校验相邻状态仅允许 Planned→CanaryInstalled→Promoted/RolledBack。
fn validate_next_state(
    previous: Option<&PluginCanaryDeploymentRecordV1>,
    next: &PluginCanaryDeploymentRecordV1,
) -> Result<(), PluginDeploymentStoreError> {
    let valid = matches!(
        (previous.map(|record| record.state), next.state),
        (None, PluginDeploymentStateV1::Planned)
            | (
                Some(PluginDeploymentStateV1::Planned),
                PluginDeploymentStateV1::CanaryInstalled
            )
            | (
                Some(PluginDeploymentStateV1::CanaryInstalled),
                PluginDeploymentStateV1::Promoted | PluginDeploymentStateV1::RolledBack
            )
    );
    if !valid {
        return Err(PluginDeploymentStoreError::InvalidTransition {
            from: previous.map(|record| record.state),
            to: next.state,
        });
    }
    if let Some(previous) = previous {
        let digest_transition_valid = match next.state {
            PluginDeploymentStateV1::CanaryInstalled => true,
            PluginDeploymentStateV1::Promoted => next.installed_digest == previous.installed_digest,
            PluginDeploymentStateV1::RolledBack => {
                next.installed_digest == next.previous_bundle.digest
            }
            PluginDeploymentStateV1::Planned => false,
        };
        if !digest_transition_valid {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "当前安装摘要未按部署阶段单调迁移",
            ));
        }
    }
    Ok(())
}

/// 校验一条 Deployment 历史的身份、状态和终态唯一性。
fn validate_history(
    records: &[PluginCanaryDeploymentRecordV1],
    deployment_id: &PluginDeploymentId,
) -> Result<(), PluginDeploymentStoreError> {
    for record in records {
        record.validate()?;
        if &record.deployment_id != deployment_id {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "状态目录包含其他 Deployment ID",
            ));
        }
    }
    if let Some(first) = records.first() {
        if first.state != PluginDeploymentStateV1::Planned {
            return Err(PluginDeploymentStoreError::InvalidTransition {
                from: None,
                to: first.state,
            });
        }
        for record in &records[1..] {
            ensure_same_binding(first, record)?;
        }
    }
    for pair in records.windows(2) {
        validate_next_state(Some(&pair[0]), &pair[1])?;
    }
    if records
        .iter()
        .filter(|record| record.state.is_terminal())
        .count()
        > 1
    {
        return Err(PluginDeploymentStoreError::InvalidRecord(
            "同一 Deployment 存在多个终态".to_string(),
        ));
    }
    Ok(())
}

/// 校验状态快照的稳定身份和旧 bundle 引用始终绑定同一 Deployment。
fn ensure_same_binding(
    expected: &PluginCanaryDeploymentRecordV1,
    actual: &PluginCanaryDeploymentRecordV1,
) -> Result<(), PluginDeploymentStoreError> {
    if expected.binding()? != actual.binding()?
        || expected.previous_bundle != actual.previous_bundle
    {
        return Err(PluginDeploymentStoreError::BindingMismatch(
            "部署状态快照改写了稳定身份或旧 bundle 引用",
        ));
    }
    Ok(())
}

/// 为受信对象附加稳定域并计算规范 SHA-256 摘要。
fn canonical_digest<T: Serialize>(
    domain: &'static str,
    value: &T,
) -> Result<ArtifactDigest, PluginDeploymentStoreError> {
    let bytes = serde_json::to_vec(&(domain, value))?;
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| PluginDeploymentStoreError::InvalidRecord(error.to_string()))
}

/// 读取一个 Deployment 目录内的固定阶段文件并拒绝未知条目。
async fn read_state_directory(
    root: &Path,
    directory: &Path,
) -> Result<Vec<PluginCanaryDeploymentRecordV1>, PluginDeploymentStoreError> {
    if !validate_existing_store_directory(root, directory).await? {
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
            .map_err(|_| PluginDeploymentStoreError::UnsafePath(path.clone()))?;
        if file_name.starts_with(".append-") && file_name.ends_with(".tmp") {
            continue;
        }
        if !matches!(
            file_name.as_str(),
            "00-planned.json"
                | "01-canary-installed.json"
                | "02-promoted.json"
                | "02-rolled-back.json"
        ) {
            return Err(PluginDeploymentStoreError::UnsafePath(path));
        }
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(|source| io_error(&path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(PluginDeploymentStoreError::UnsafePath(path));
        }
        let bytes = read_safe_file(root, &path)
            .await?
            .ok_or_else(|| PluginDeploymentStoreError::UnsafePath(path.clone()))?;
        let record: PluginCanaryDeploymentRecordV1 = serde_json::from_slice(&bytes)?;
        if file_name != record.state.file_name() {
            return Err(PluginDeploymentStoreError::BindingMismatch(
                "部署状态文件名与记录阶段不一致",
            ));
        }
        records.push(record);
    }
    Ok(records)
}

/// 使用 create-new-or-same 语义安全提交不可变状态文件。
async fn write_create_new_or_same(
    root: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), PluginDeploymentStoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| PluginDeploymentStoreError::UnsafePath(path.to_path_buf()))?;
    ensure_store_directory(root, parent).await?;
    if let Some(existing) = read_safe_file(root, path).await? {
        return if existing == bytes {
            Ok(())
        } else {
            Err(PluginDeploymentStoreError::AppendConflict(
                path.to_path_buf(),
            ))
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
    .map_err(|source| PluginDeploymentStoreError::BlockingTask(source.to_string()))?;
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
                .ok_or_else(|| PluginDeploymentStoreError::AppendConflict(path.to_path_buf()))?;
            if existing == bytes {
                Ok(())
            } else {
                Err(PluginDeploymentStoreError::AppendConflict(
                    path.to_path_buf(),
                ))
            }
        }
        Err(source) => Err(io_error(path, source)),
    }
}

/// 创建并逐段验证 Store 内部目录，拒绝符号链接和路径逃逸。
async fn ensure_store_directory(
    root: &Path,
    path: &Path,
) -> Result<(), PluginDeploymentStoreError> {
    let relative = store_relative(root, path)?;
    validate_directory(root).await?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(PluginDeploymentStoreError::UnsafePath(path.to_path_buf()));
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

/// 验证已存在 Store 目录的每一段都是真实目录。
async fn validate_existing_store_directory(
    root: &Path,
    path: &Path,
) -> Result<bool, PluginDeploymentStoreError> {
    let relative = store_relative(root, path)?;
    validate_directory(root).await?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(PluginDeploymentStoreError::UnsafePath(path.to_path_buf()));
        };
        current.push(name);
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            Ok(_) => return Err(PluginDeploymentStoreError::UnsafePath(current)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(io_error(&current, source)),
        }
    }
    verify_canonical_directory(path).await?;
    Ok(true)
}

/// 使用 O_NOFOLLOW 读取非符号链接普通文件。
async fn read_safe_file(
    root: &Path,
    path: &Path,
) -> Result<Option<Vec<u8>>, PluginDeploymentStoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| PluginDeploymentStoreError::UnsafePath(path.to_path_buf()))?;
    if !validate_existing_store_directory(root, parent).await? {
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
            return Err(PluginDeploymentStoreError::UnsafePath(path));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| io_error(&path, source))?;
        Ok(Some(bytes))
    })
    .await
    .map_err(|source| PluginDeploymentStoreError::BlockingTask(source.to_string()))?
}

/// 创建并规范化绝对 Store 根，拒绝符号链接和点号跳转。
fn prepare_store_root(root: PathBuf) -> Result<PathBuf, PluginDeploymentStoreError> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(PluginDeploymentStoreError::UnsafeRoot(root));
    }
    match std_fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(PluginDeploymentStoreError::UnsafeRoot(root));
        }
        Ok(_) => return std_fs::canonicalize(&root).map_err(|source| io_error(&root, source)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&root, source)),
    }
    let mut existing = root.as_path();
    let mut missing = Vec::new();
    loop {
        match std_fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| PluginDeploymentStoreError::UnsafeRoot(root.clone()))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| PluginDeploymentStoreError::UnsafeRoot(root.clone()))?;
            }
            Err(source) => return Err(io_error(existing, source)),
        }
    }
    let mut canonical =
        std_fs::canonicalize(existing).map_err(|source| io_error(existing, source))?;
    for name in missing.into_iter().rev() {
        canonical.push(name);
        match std_fs::create_dir(&canonical) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std_fs::symlink_metadata(&canonical)
                    .map_err(|source| io_error(&canonical, source))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(PluginDeploymentStoreError::UnsafeRoot(canonical));
                }
            }
            Err(source) => return Err(io_error(&canonical, source)),
        }
    }
    Ok(canonical)
}

/// 确认目标路径位于 Store 根内。
fn store_relative<'a>(
    root: &'a Path,
    path: &'a Path,
) -> Result<&'a Path, PluginDeploymentStoreError> {
    path.strip_prefix(root)
        .map_err(|_| PluginDeploymentStoreError::UnsafePath(path.to_path_buf()))
}

/// 验证路径是非符号链接目录。
async fn validate_directory(path: &Path) -> Result<(), PluginDeploymentStoreError> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginDeploymentStoreError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

/// 确认目录的规范路径未因符号链接发生变化。
async fn verify_canonical_directory(path: &Path) -> Result<(), PluginDeploymentStoreError> {
    let canonical = fs::canonicalize(path)
        .await
        .map_err(|source| io_error(path, source))?;
    if canonical != path {
        return Err(PluginDeploymentStoreError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

/// 跨进程部署文件锁；析构时释放，不删除锁文件。
struct DeploymentFileLock {
    file: std_fs::File,
}

impl Drop for DeploymentFileLock {
    fn drop(&mut self) {
        unlock_file(&self.file);
    }
}

/// 获取拒绝符号链接的跨进程排他锁。
async fn acquire_file_lock(
    path: PathBuf,
) -> Result<DeploymentFileLock, PluginDeploymentStoreError> {
    let task_path = path.clone();
    let file = tokio::task::spawn_blocking(move || {
        let mut options = StdOpenOptions::new();
        options.read(true).write(true).create(true);
        set_no_follow(&mut options);
        let file = options
            .open(&task_path)
            .map_err(|source| io_error(&task_path, source))?;
        lock_file(&file).map_err(|source| io_error(&task_path, source))?;
        Ok::<_, PluginDeploymentStoreError>(file)
    })
    .await
    .map_err(|source| PluginDeploymentStoreError::BlockingTask(source.to_string()))??;
    Ok(DeploymentFileLock { file })
}

/// 为文件打开选项启用拒绝符号链接和关闭时释放描述符。
#[cfg(unix)]
fn set_no_follow(options: &mut StdOpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
}

/// 非 Unix 平台依赖 create-new 和打开后类型复核。
#[cfg(not(unix))]
fn set_no_follow(_options: &mut StdOpenOptions) {}

/// 使用 flock 获取排他锁。
#[cfg(unix)]
fn lock_file(file: &std_fs::File) -> Result<(), std::io::Error> {
    // SAFETY：`file` 在调用期间保持有效，`flock` 只使用其原生文件描述符。
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 非 Unix 平台当前退化为进程内事务门禁。
#[cfg(not(unix))]
fn lock_file(_file: &std_fs::File) -> Result<(), std::io::Error> {
    Ok(())
}

/// 释放 flock 排他锁。
#[cfg(unix)]
fn unlock_file(file: &std_fs::File) {
    // SAFETY：`file` 在析构结束前保持有效；解锁失败不会改变只追加状态文件。
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

/// 非 Unix 平台没有额外文件锁需要释放。
#[cfg(not(unix))]
fn unlock_file(_file: &std_fs::File) {}

/// 为文件系统错误附加稳定路径上下文。
fn io_error(path: impl AsRef<Path>, source: std::io::Error) -> PluginDeploymentStoreError {
    PluginDeploymentStoreError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

/// 插件跨进程部署状态持久化错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginDeploymentStoreError {
    /// 记录 schema 版本不受支持。
    #[error("不支持的插件部署状态 schema 版本 {found}")]
    UnsupportedSchema {
        /// 实际 schema 版本。
        found: u32,
    },
    /// Admission、Revision、安装摘要或旧 bundle 引用错绑。
    #[error("插件部署状态绑定不一致：{0}")]
    BindingMismatch(&'static str),
    /// 单条记录或完整历史损坏。
    #[error("插件部署状态记录无效：{0}")]
    InvalidRecord(String),
    /// Store 根路径不是安全绝对目录。
    #[error("插件部署 Store 根路径不安全：{0}")]
    UnsafeRoot(PathBuf),
    /// Store 内部路径逃逸、包含符号链接或不是预期文件类型。
    #[error("插件部署 Store 路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// create-new-or-same 目标已经绑定其他内容。
    #[error("插件部署只追加状态冲突：{0}")]
    AppendConflict(PathBuf),
    /// 指定 Deployment 尚未登记。
    #[error("插件部署不存在：{0}")]
    NotFound(PluginDeploymentId),
    /// 状态迁移不是允许的单调边。
    #[error("插件部署状态迁移无效：{from:?} -> {to:?}")]
    InvalidTransition {
        /// 当前状态；首次追加时为 `None`。
        from: Option<PluginDeploymentStateV1>,
        /// 请求追加的状态。
        to: PluginDeploymentStateV1,
    },
    /// 相同非终态快照被重复追加。
    #[error("插件部署状态重复追加：{0:?}")]
    DuplicateState(PluginDeploymentStateV1),
    /// 已进入终态后再次请求终态迁移。
    #[error("插件部署终态不可重复或改写：{current:?} -> {requested:?}")]
    DuplicateTerminal {
        /// 已持久化终态。
        current: PluginDeploymentStateV1,
        /// 再次请求的终态。
        requested: PluginDeploymentStateV1,
    },
    /// 旧 bundle CAS 制品不存在。
    #[error("插件部署旧 bundle CAS 制品不存在：{0}")]
    MissingArtifact(ArtifactDigest),
    /// Artifact CAS 操作失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// JSON 编解码失败，包括截断状态文件。
    #[error("插件部署状态 JSON 无效：{0}")]
    Json(#[from] serde_json::Error),
    /// 文件系统操作失败。
    #[error("插件部署 Store I/O 失败：{path}: {source}")]
    Io {
        /// 失败路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 阻塞文件任务异常终止。
    #[error("插件部署 Store 阻塞任务异常终止：{0}")]
    BlockingTask(String),
}
