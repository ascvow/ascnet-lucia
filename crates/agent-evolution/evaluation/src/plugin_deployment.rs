//! M8 插件 Canary、Stable Promotion 与生产回滚的部署执行器。
//!
//! 发布签名、Gate、Canary 状态机与只追加归档仍由 `PluginReleaseController` 负责；本模块只
//! 消费它已经返回的归档结果，把确定性 bundle、Plugin Manager 与 Genome Stable 指针绑定。

use crate::plugin_deployment_store::{
    FilePluginDeploymentStore, PluginCanaryDeploymentBindingV1, PluginCanaryDeploymentRecordV1,
    PluginDeploymentId, PluginDeploymentStateV1, PluginDeploymentStoreError,
    PluginDeploymentTransaction,
};
use crate::plugin_release::{FilePluginReleaseArchive, PluginReleaseError};
use crate::{
    PluginCanaryAdmissionV1, PluginEvaluationArchiveRecordV1, PluginReleaseArchiveRecordV1,
};
use agent_evolution::{
    FileStableGenomePublisher, GenomeResolver, GenomeSelector, GenomeStore, StableGenomeRef,
};
use agent_evolution_protocol::{
    ArtifactDigest, GenomeRevision, PluginCanaryState, PluginGenome, PluginReleaseStage, ReleaseId,
};
use agent_plugin_manager::{
    pack_plugin_bundle, unpack_plugin_bundle, InstalledPlugin, PluginManager,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::{Builder as TempDirectoryBuilder, TempDir};
use tokio::sync::Mutex;

/// 一次已安装、尚未切换 Stable Genome 的 Canary 部署。
///
/// 该对象保存部署前的 Parent Genome 和确定性 bundle 快照。它只能由
/// [`PluginDeploymentController::deploy_canary`] 创建，供后续 Promotion 或健康失败回滚消费。
#[derive(Debug)]
pub struct PluginCanaryDeployment {
    admission: PluginCanaryAdmissionV1,
    candidate_revision: GenomeRevision,
    parent_stable: StableGenomeRef,
    parent_revision: GenomeRevision,
    previous_bundle: Vec<u8>,
    installed: InstalledPlugin,
}

impl PluginCanaryDeployment {
    /// 返回已经安装到 Plugin Manager 的 Candidate。
    pub fn installed(&self) -> &InstalledPlugin {
        &self.installed
    }

    /// 返回 Canary 发布 ID。
    pub fn canary_release_id(&self) -> &ReleaseId {
        &self.admission.release.release.release_id
    }

    /// 返回部署前仍保持可见的 Parent Stable 引用。
    pub fn parent_stable(&self) -> &StableGenomeRef {
        &self.parent_stable
    }

    /// 返回 Candidate Genome 修订。
    pub fn candidate_revision(&self) -> &GenomeRevision {
        &self.candidate_revision
    }

    /// 返回通过发布控制面验证的 Canary Admission。
    pub fn admission(&self) -> &PluginCanaryAdmissionV1 {
        &self.admission
    }

    /// 返回部署前确定性归档的旧 Stable bundle 字节。
    ///
    /// 调用方只能将该快照写入不可变 Artifact CAS；不得把原始字节内嵌到部署状态记录。
    pub fn previous_bundle(&self) -> &[u8] {
        &self.previous_bundle
    }
}

/// 一次成功 Stable Promotion 的生产部署回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPromotionReceipt {
    /// Plugin Manager 最终可见的 Candidate 安装记录。
    pub installed: InstalledPlugin,
    /// 原子发布后最终可见的 Stable Genome 引用。
    pub stable: StableGenomeRef,
}

/// 一次健康失败回滚的生产部署回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRollbackReceipt {
    /// Plugin Manager 最终恢复的旧 Stable 安装记录。
    pub installed: InstalledPlugin,
    /// 回滚后仍指向 Parent Genome 的 Stable 引用。
    pub stable: StableGenomeRef,
}

/// 把已验证插件发布结果落到 Plugin Manager 和 Genome Registry 的生产控制器。
///
/// 控制器在单个实例内串行化部署，但不会宣称跨 Plugin Manager、Genome Store 和 Stable 指针
/// 的强事务。Candidate 安装后的步骤失败时会尽力恢复部署前 bundle，并在错误中保留补偿结果。
pub struct PluginDeploymentController<'a> {
    manager: &'a PluginManager,
    publisher: &'a FileStableGenomePublisher,
    staging_root: PathBuf,
    deployment_guard: Mutex<()>,
}

/// 强制通过受信 Release Archive 与部署 Store 执行生产副作用的可恢复控制器。
///
/// 该控制器不接受调用方提供的 bundle、Admission 或 Evaluation 副本；所有恢复输入均从
/// Release Archive、Genome Store、Plugin Manager 和部署 Store/CAS 重新读取并复核。
pub struct PersistentPluginDeploymentController<'a> {
    controller: PluginDeploymentController<'a>,
    release_archive: &'a FilePluginReleaseArchive<'a>,
    deployment_store: &'a FilePluginDeploymentStore<'a>,
}

/// 已完成 Manager 副作用前全部只读校验的 Canary 部署准备结果。
struct PreparedCanaryDeployment {
    admission: PluginCanaryAdmissionV1,
    candidate_revision: GenomeRevision,
    parent_stable: StableGenomeRef,
    parent_revision: GenomeRevision,
    previous_bundle: Vec<u8>,
    prepared_bundle: PreparedBundle,
}

/// 从四个受信存储面重建、尚未执行终态动作的部署输入。
struct RecoveredCanaryDeployment {
    deployment: PluginCanaryDeployment,
    record: PluginCanaryDeploymentRecordV1,
}

impl<'a> PluginDeploymentController<'a> {
    /// 使用真实 Plugin Manager、Stable 发布器和受信暂存根创建控制器。
    ///
    /// Candidate Revision 始终写入 `publisher.resolver().store()`，因此不存在可配置的第二个
    /// Genome 根。
    ///
    /// `staging_root` 必须是绝对路径；实际访问发生在部署方法中，目录不能位于 Plugin Manager
    /// 管理根内，否则 Manager 的来源隔离检查会拒绝安装。
    pub fn new(
        manager: &'a PluginManager,
        publisher: &'a FileStableGenomePublisher,
        staging_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            manager,
            publisher,
            staging_root: staging_root.into(),
            deployment_guard: Mutex::new(()),
        }
    }

    /// 安装已通过 `PluginReleaseController::admit_canary` 的 Candidate bundle。
    ///
    /// 本步骤不会追加 Candidate Genome，也不会切换 Stable 指针。安装前会保存当前插件的
    /// 确定性 bundle，并验证它与 Parent Genome 一致；Candidate 归档摘要、解包后的树摘要、
    /// manifest 身份和 Candidate `PluginGenome` 也必须完全一致。
    ///
    /// # Errors
    ///
    /// Canary 归档错绑、Parent/插件安装状态不可信、bundle 解包或 `replace` 失败，或安装期间
    /// Stable 被并发改写时返回错误。`replace` 自身失败时依赖 Plugin Manager 保留旧安装；安装
    /// 后检查失败时会尽力恢复旧 bundle。
    pub async fn deploy_canary(
        &self,
        lineage: &str,
        admission: PluginCanaryAdmissionV1,
        candidate_revision: &GenomeRevision,
        bundle_bytes: &[u8],
    ) -> Result<PluginCanaryDeployment, PluginDeploymentError> {
        let _guard = self.deployment_guard.lock().await;
        let prepared = self
            .prepare_canary_deployment(lineage, admission, candidate_revision, bundle_bytes)
            .await?;
        self.install_prepared_canary(prepared).await
    }

    /// 完成 Candidate 安装前的受信输入、Parent 和 bundle 校验。
    async fn prepare_canary_deployment(
        &self,
        lineage: &str,
        admission: PluginCanaryAdmissionV1,
        candidate_revision: &GenomeRevision,
        bundle_bytes: &[u8],
    ) -> Result<PreparedCanaryDeployment, PluginDeploymentError> {
        let parent_stable = self
            .publisher
            .resolver()
            .stable_reference(lineage)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("读取 Parent Stable 失败：{error}"))
            })?;
        let parent_revision = self
            .publisher
            .resolver()
            .resolve(&GenomeSelector::Stable(lineage.to_string()))
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("解析 Parent Genome 失败：{error}"))
            })?;
        validate_parent_binding(candidate_revision, &parent_stable, &parent_revision)?;
        let candidate_plugin =
            validate_canary_authorization(&admission, candidate_revision, bundle_bytes)?;
        let previous_installed = self.current_installed(&candidate_plugin.id)?;
        let parent_plugin = plugin_genome(&parent_revision, &candidate_plugin.id)?;
        verify_installed(&previous_installed, parent_plugin)?;
        let previous_bundle = self.snapshot_installed_bundle(&previous_installed, parent_plugin)?;
        let prepared = self.prepare_bundle("candidate", bundle_bytes)?;
        verify_prepared_bundle(&prepared, candidate_plugin)?;

        Ok(PreparedCanaryDeployment {
            admission,
            candidate_revision: candidate_revision.clone(),
            parent_stable,
            parent_revision,
            previous_bundle,
            prepared_bundle: prepared,
        })
    }

    /// 执行真实 Candidate replace，并复核 Manager 与 Parent Stable 未并发变化。
    async fn install_prepared_canary(
        &self,
        prepared: PreparedCanaryDeployment,
    ) -> Result<PluginCanaryDeployment, PluginDeploymentError> {
        let candidate_plugin = plugin_genome(
            &prepared.candidate_revision,
            &prepared.admission.release.release.plugin_id,
        )?;
        let parent_plugin = plugin_genome(&prepared.parent_revision, &candidate_plugin.id)?;
        let installed = self
            .manager
            .replace(prepared.prepared_bundle.root())
            .map_err(|error| PluginDeploymentError::Install(error.to_string()))?;
        if let Err(error) = verify_installed(&installed, candidate_plugin)
            .and_then(|_| self.verify_current_install(candidate_plugin))
        {
            return Err(self.compensated_failure(
                "Canary 安装后验证",
                error,
                &prepared.previous_bundle,
                parent_plugin,
            ));
        }
        let observed_parent = self
            .publisher
            .resolver()
            .stable_reference(&prepared.parent_stable.lineage)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("重新读取 Parent Stable 失败：{error}"))
            });
        match observed_parent {
            Ok(observed) if observed == prepared.parent_stable => {}
            Ok(_) => {
                return Err(self.compensated_failure(
                    "Canary 安装并发前置条件",
                    PluginDeploymentError::Binding(
                        "Canary 安装期间 Stable Genome 已变化".to_string(),
                    ),
                    &prepared.previous_bundle,
                    parent_plugin,
                ));
            }
            Err(error) => {
                return Err(self.compensated_failure(
                    "Canary 安装后 Stable 复核",
                    error,
                    &prepared.previous_bundle,
                    parent_plugin,
                ));
            }
        }

        Ok(PluginCanaryDeployment {
            admission: prepared.admission,
            candidate_revision: prepared.candidate_revision,
            parent_stable: prepared.parent_stable,
            parent_revision: prepared.parent_revision,
            previous_bundle: prepared.previous_bundle,
            installed,
        })
    }

    /// 把成功 Canary 对应的 Stable Release 发布为正式 Genome。
    ///
    /// `stable_release` 和 `evaluation` 必须是 `PluginReleaseController::promote_stable` 已返回的
    /// 精确归档结果。方法先复核当前安装和 Parent 前置条件，再只追加 Candidate Revision，最后
    /// 原子发布 Stable 引用。任一后续步骤失败都会尽力恢复 Canary 前的旧 bundle；已成功追加的
    /// 不可变 Candidate Revision 不会删除，因此该补偿不是跨存储强事务。
    ///
    /// # Errors
    ///
    /// Stable 归档与 Canary 错绑、当前状态并发变化、Genome 追加/发布失败时返回带补偿信息的
    /// [`PluginDeploymentError::PostCanaryFailure`]。
    pub async fn promote_stable(
        &self,
        deployment: PluginCanaryDeployment,
        evaluation: &PluginEvaluationArchiveRecordV1,
        stable_release: &PluginReleaseArchiveRecordV1,
        generation: u64,
    ) -> Result<PluginPromotionReceipt, PluginDeploymentError> {
        let _guard = self.deployment_guard.lock().await;
        let result = self
            .promote_stable_inner(&deployment, evaluation, stable_release, generation)
            .await;
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                let parent_plugin = plugin_genome(
                    &deployment.parent_revision,
                    &deployment.admission.release.release.plugin_id,
                )?;
                Err(self.compensated_failure(
                    "Stable Promotion",
                    error,
                    &deployment.previous_bundle,
                    parent_plugin,
                ))
            }
        }
    }

    /// 在 Canary 健康失败后恢复先前受信 Stable bundle，并确认 Parent Genome 未被改写。
    ///
    /// `rollback_release` 必须来自 `PluginReleaseController::rollback_failed_canary`；
    /// `trusted_stable_release` 是该控制器重新验签过的先前 Stable 记录。只有旧 bundle 同时匹配
    /// 该 Stable Release 和 Parent `PluginGenome`，且 Rollback Component 指向同一先前 Stable
    /// 时才执行真实 `replace`。
    ///
    /// # Errors
    ///
    /// Rollback/Stable 归档错绑、Parent Stable 已变化、旧 bundle 不可信或恢复安装失败时返回
    /// 错误。恢复安装失败时 Plugin Manager 的原子替换语义会保留当前 Candidate。
    pub async fn rollback_failed_canary(
        &self,
        deployment: PluginCanaryDeployment,
        evaluation: &PluginEvaluationArchiveRecordV1,
        rollback_release: &PluginReleaseArchiveRecordV1,
        trusted_stable_release: &PluginReleaseArchiveRecordV1,
    ) -> Result<PluginRollbackReceipt, PluginDeploymentError> {
        let _guard = self.deployment_guard.lock().await;
        self.rollback_failed_canary_inner(
            &deployment,
            evaluation,
            rollback_release,
            trusted_stable_release,
        )
        .await
    }

    /// 执行健康失败回滚主路径，调用方负责持有进程内或跨进程部署锁。
    async fn rollback_failed_canary_inner(
        &self,
        deployment: &PluginCanaryDeployment,
        evaluation: &PluginEvaluationArchiveRecordV1,
        rollback_release: &PluginReleaseArchiveRecordV1,
        trusted_stable_release: &PluginReleaseArchiveRecordV1,
    ) -> Result<PluginRollbackReceipt, PluginDeploymentError> {
        validate_rollback_authorization(
            deployment,
            evaluation,
            rollback_release,
            trusted_stable_release,
        )?;
        self.require_parent_stable(deployment).await?;
        let parent_plugin = plugin_genome(
            &deployment.parent_revision,
            &deployment.admission.release.release.plugin_id,
        )?;
        validate_bundle_artifact(trusted_stable_release, &deployment.previous_bundle)?;
        let prepared = self.prepare_bundle("rollback", &deployment.previous_bundle)?;
        verify_prepared_bundle(&prepared, parent_plugin)?;
        let installed = self
            .manager
            .replace(prepared.root())
            .map_err(|error| PluginDeploymentError::RollbackInstall(error.to_string()))?;
        verify_installed(&installed, parent_plugin)?;
        self.verify_current_install(parent_plugin)?;
        self.require_parent_stable(deployment).await?;
        Ok(PluginRollbackReceipt {
            installed,
            stable: deployment.parent_stable.clone(),
        })
    }

    /// 执行 Promotion 主路径，调用方负责在失败时补偿 bundle。
    async fn promote_stable_inner(
        &self,
        deployment: &PluginCanaryDeployment,
        evaluation: &PluginEvaluationArchiveRecordV1,
        stable_release: &PluginReleaseArchiveRecordV1,
        generation: u64,
    ) -> Result<PluginPromotionReceipt, PluginDeploymentError> {
        validate_stable_authorization(deployment, evaluation, stable_release)?;
        let candidate_plugin = plugin_genome(
            &deployment.candidate_revision,
            &deployment.admission.release.release.plugin_id,
        )?;
        self.verify_current_install(candidate_plugin)?;
        self.require_parent_stable(deployment).await?;
        self.append_revision_idempotently(&deployment.candidate_revision)
            .await?;
        let stable = self
            .publisher
            .publish_bound(
                &deployment.parent_stable,
                &deployment.candidate_revision,
                generation,
                stable_release.release.release_id.clone(),
                evaluation.report_id.clone(),
                None,
            )
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("发布 Stable Genome 失败：{error}"))
            })?;
        let installed = self.current_installed(&candidate_plugin.id)?;
        verify_installed(&installed, candidate_plugin)?;
        Ok(PluginPromotionReceipt { installed, stable })
    }

    /// 以 create-new-or-same 语义登记 Candidate Genome，支持发布进程安全重试。
    async fn append_revision_idempotently(
        &self,
        revision: &GenomeRevision,
    ) -> Result<(), PluginDeploymentError> {
        match self
            .publisher
            .resolver()
            .store()
            .get(&revision.revision_id)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("预检 Candidate Genome 失败：{error}"))
            })? {
            Some(existing) if existing == *revision => return Ok(()),
            Some(_) => {
                return Err(PluginDeploymentError::Binding(
                    "Candidate Revision ID 已绑定其他 Genome".to_string(),
                ));
            }
            None => {}
        }
        self.publisher
            .resolver()
            .store()
            .append(revision)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("追加 Candidate Genome 失败：{error}"))
            })
    }

    /// 读取并诊断当前插件安装记录。
    fn current_installed(&self, plugin_id: &str) -> Result<InstalledPlugin, PluginDeploymentError> {
        let doctor = self.manager.doctor().map_err(|error| {
            PluginDeploymentError::Manager(format!("诊断 Plugin Manager 失败：{error}"))
        })?;
        if !doctor.is_healthy() {
            return Err(PluginDeploymentError::Manager(format!(
                "Plugin Manager 完整性诊断失败：{}",
                doctor
                    .issues
                    .iter()
                    .map(|issue| issue.message.as_str())
                    .collect::<Vec<_>>()
                    .join("；")
            )));
        }
        self.manager
            .list()
            .map_err(|error| PluginDeploymentError::Manager(format!("读取插件锁失败：{error}")))?
            .into_iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| PluginDeploymentError::Binding(format!("插件 `{plugin_id}` 尚未安装")))
    }

    /// 验证 Plugin Manager 当前记录与目标 Genome 一致。
    fn verify_current_install(&self, expected: &PluginGenome) -> Result<(), PluginDeploymentError> {
        let installed = self.current_installed(&expected.id)?;
        verify_installed(&installed, expected)
    }

    /// 把当前受管理 bundle 编码为确定性快照，并复核 Parent Genome 摘要。
    fn snapshot_installed_bundle(
        &self,
        installed: &InstalledPlugin,
        expected: &PluginGenome,
    ) -> Result<Vec<u8>, PluginDeploymentError> {
        let manifest = self.manager.root().join(&installed.manifest);
        let root = manifest.parent().ok_or_else(|| {
            PluginDeploymentError::Binding("锁定 manifest 缺少 bundle 根目录".to_string())
        })?;
        let archive = pack_plugin_bundle(root).map_err(|error| {
            PluginDeploymentError::Manager(format!("归档旧 Stable bundle 失败：{error}"))
        })?;
        let digest = digest_bytes(&archive)?;
        if digest != expected.bundle {
            return Err(PluginDeploymentError::Binding(
                "旧 Stable bundle 快照与 Parent PluginGenome 不一致".to_string(),
            ));
        }
        Ok(archive)
    }

    /// 在隔离临时目录中还原并验证一个确定性 bundle。
    fn prepare_bundle(
        &self,
        label: &'static str,
        archive: &[u8],
    ) -> Result<PreparedBundle, PluginDeploymentError> {
        if !self.staging_root.is_absolute() {
            return Err(PluginDeploymentError::Staging(format!(
                "部署暂存根必须是绝对路径：{}",
                self.staging_root.display()
            )));
        }
        fs::create_dir_all(&self.staging_root).map_err(|error| {
            PluginDeploymentError::Staging(format!(
                "创建部署暂存根失败 `{}`：{error}",
                self.staging_root.display()
            ))
        })?;
        let temporary = TempDirectoryBuilder::new()
            .prefix("lucia-plugin-deployment-")
            .tempdir_in(&self.staging_root)
            .map_err(|error| {
                PluginDeploymentError::Staging(format!("创建部署临时目录失败：{error}"))
            })?;
        let root = temporary.path().join(label);
        let tree_hex = unpack_plugin_bundle(archive, &root).map_err(|error| {
            PluginDeploymentError::Bundle(format!("解包 {label} bundle 失败：{error}"))
        })?;
        let tree_digest = ArtifactDigest::from_sha256_hex(tree_hex).map_err(|error| {
            PluginDeploymentError::Bundle(format!("bundle 树摘要无效：{error}"))
        })?;
        Ok(PreparedBundle {
            _temporary: temporary,
            root,
            tree_digest,
        })
    }

    /// 确认 Canary 期间 Stable 指针仍是部署前 Parent。
    async fn require_parent_stable(
        &self,
        deployment: &PluginCanaryDeployment,
    ) -> Result<(), PluginDeploymentError> {
        let observed = self
            .publisher
            .resolver()
            .stable_reference(&deployment.parent_stable.lineage)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("读取当前 Stable 失败：{error}"))
            })?;
        if observed != deployment.parent_stable {
            return Err(PluginDeploymentError::Binding(
                "Canary 期间 Stable Genome 已变化，拒绝覆盖并发发布".to_string(),
            ));
        }
        let resolved = self
            .publisher
            .resolver()
            .resolve(&GenomeSelector::Stable(
                deployment.parent_stable.lineage.clone(),
            ))
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("解析当前 Stable 失败：{error}"))
            })?;
        if resolved != deployment.parent_revision {
            return Err(PluginDeploymentError::Binding(
                "Stable 指针未解析到部署时固定的 Parent Genome".to_string(),
            ));
        }
        Ok(())
    }

    /// 在 Candidate 已安装后尽力恢复旧 bundle，并保留主失败与补偿失败。
    fn compensated_failure(
        &self,
        stage: &'static str,
        primary: PluginDeploymentError,
        previous_bundle: &[u8],
        parent_plugin: &PluginGenome,
    ) -> PluginDeploymentError {
        let restoration_error = self
            .restore_previous_bundle(previous_bundle, parent_plugin)
            .err()
            .map(|error| error.to_string());
        PluginDeploymentError::PostCanaryFailure {
            stage,
            primary: primary.to_string(),
            restoration_error,
        }
    }

    /// 还原部署前 bundle，并对最终锁记录执行身份与摘要复核。
    fn restore_previous_bundle(
        &self,
        archive: &[u8],
        expected: &PluginGenome,
    ) -> Result<(), PluginDeploymentError> {
        let prepared = self.prepare_bundle("compensation", archive)?;
        verify_prepared_bundle(&prepared, expected)?;
        let installed = self
            .manager
            .replace(prepared.root())
            .map_err(|error| PluginDeploymentError::RollbackInstall(error.to_string()))?;
        verify_installed(&installed, expected)?;
        self.verify_current_install(expected)
    }
}

impl<'a> PersistentPluginDeploymentController<'a> {
    /// 使用真实 Manager、Genome Publisher、Release Archive、Deployment Store 和暂存根创建
    /// 可跨进程恢复的生产部署控制器。
    pub fn new(
        manager: &'a PluginManager,
        publisher: &'a FileStableGenomePublisher,
        release_archive: &'a FilePluginReleaseArchive<'a>,
        deployment_store: &'a FilePluginDeploymentStore<'a>,
        staging_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            controller: PluginDeploymentController::new(manager, publisher, staging_root),
            release_archive,
            deployment_store,
        }
    }

    /// 从受信 Release Archive 安装 Canary，并在 replace 前后分别持久化 Planned 与
    /// CanaryInstalled。
    ///
    /// Candidate Revision 会先以 create-new-or-same 语义进入不可变 Genome Store。部署已存在
    /// 时调用会按 Manager 当前摘要恢复崩溃窗口，不信任调用方提供的 bundle 或 Admission。
    ///
    /// # Errors
    ///
    /// Release Archive、Parent/Candidate Genome、Manager、CAS 或状态历史错绑，或任一真实安装
    /// 副作用失败时返回错误。
    pub async fn deploy_canary(
        &self,
        lineage: &str,
        canary_release_id: &ReleaseId,
        candidate_revision: &GenomeRevision,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentError> {
        let _guard = self.controller.deployment_guard.lock().await;
        let deployment_id = PluginDeploymentId::for_canary_release(canary_release_id.clone());
        let transaction = self.deployment_store.transaction(&deployment_id).await?;
        if !transaction.history().await?.is_empty() {
            let recovered = self.recover_canary_locked(&transaction, true).await?;
            if recovered.deployment.candidate_revision != *candidate_revision {
                return Err(PluginDeploymentError::Binding(
                    "重试传入的 Candidate Revision 与持久化部署不一致".to_string(),
                ));
            }
            return Ok(recovered.record);
        }
        let admission = self
            .release_archive
            .canary_admission(canary_release_id)
            .await?;
        let bundle = self
            .release_archive
            .release_bundle(&admission.release)
            .await?;
        let prepared = self
            .controller
            .prepare_canary_deployment(lineage, admission, candidate_revision, &bundle)
            .await?;
        self.controller
            .append_revision_idempotently(candidate_revision)
            .await?;
        let binding = PluginCanaryDeploymentBindingV1::from_plan(
            &prepared.admission,
            &prepared.parent_stable,
            &prepared.candidate_revision,
        )?;
        transaction
            .append_planned(&binding, &prepared.previous_bundle)
            .await?;
        let deployment = self.controller.install_prepared_canary(prepared).await?;
        transaction
            .reconcile_canary_installed(&binding, deployment.installed())
            .await
            .map_err(Into::into)
    }

    /// 重启后从受信存储面恢复 Planned 或 replace 后未落盘的 Canary 安装。
    ///
    /// # Errors
    ///
    /// Deployment 不存在、Manager 当前安装既非 Parent 也非 Candidate，或任何受信输入错绑时
    /// 返回错误。
    pub async fn recover_canary_install(
        &self,
        deployment_id: &PluginDeploymentId,
    ) -> Result<PluginCanaryDeploymentRecordV1, PluginDeploymentError> {
        let _guard = self.controller.deployment_guard.lock().await;
        let transaction = self.deployment_store.transaction(deployment_id).await?;
        Ok(self.recover_canary_locked(&transaction, true).await?.record)
    }

    /// 从受信 Stable Release 恢复并完成 Promotion；Stable 已发布但终态未落盘时只补记终态。
    ///
    /// # Errors
    ///
    /// Stable Release 未承接当前 Canary、Manager/Stable 已被其他部署改写或持久化终态冲突时
    /// 返回错误。
    pub async fn promote_stable(
        &self,
        deployment_id: &PluginDeploymentId,
        stable_release_id: &ReleaseId,
    ) -> Result<PluginPromotionReceipt, PluginDeploymentError> {
        let _guard = self.controller.deployment_guard.lock().await;
        let transaction = self.deployment_store.transaction(deployment_id).await?;
        let recovered = self.recover_canary_locked(&transaction, true).await?;
        if recovered.record.state == PluginDeploymentStateV1::RolledBack {
            return Err(PluginDeploymentError::Binding(
                "已回滚 Deployment 不得改写为 Promoted".to_string(),
            ));
        }
        let stable_release = self
            .release_archive
            .release(stable_release_id)
            .await?
            .ok_or_else(|| PluginReleaseError::ReleaseNotFound(stable_release_id.clone()))?;
        let evaluation = self
            .release_archive
            .evaluation_for_release(&stable_release)
            .await?;
        validate_stable_authorization(&recovered.deployment, &evaluation, &stable_release)?;
        let candidate_plugin = plugin_genome(
            &recovered.deployment.candidate_revision,
            &recovered.deployment.admission.release.release.plugin_id,
        )?;
        self.controller.verify_current_install(candidate_plugin)?;
        let observed = self
            .controller
            .publisher
            .resolver()
            .stable_reference(&recovered.deployment.parent_stable.lineage)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("读取 Promotion Stable 失败：{error}"))
            })?;
        let receipt = if observed == recovered.deployment.parent_stable {
            let generation = recovered
                .deployment
                .parent_stable
                .generation
                .checked_add(1)
                .ok_or_else(|| {
                    PluginDeploymentError::Binding("Stable generation 已溢出".to_string())
                })?;
            self.controller
                .promote_stable_inner(
                    &recovered.deployment,
                    &evaluation,
                    &stable_release,
                    generation,
                )
                .await?
        } else if stable_matches_promotion(
            &observed,
            &recovered.deployment,
            &evaluation,
            &stable_release,
        ) {
            PluginPromotionReceipt {
                installed: recovered.deployment.installed.clone(),
                stable: observed,
            }
        } else {
            return Err(PluginDeploymentError::Binding(
                "Stable 已被其他生产部署改写".to_string(),
            ));
        };
        if recovered.record.state != PluginDeploymentStateV1::Promoted {
            transaction.mark_promoted().await?;
        }
        Ok(receipt)
    }

    /// 从受信 Rollback 与先前 Stable Release 恢复健康失败回滚；Manager 已恢复但终态未落盘
    /// 时只补记 RolledBack。
    ///
    /// # Errors
    ///
    /// Rollback 授权、旧 bundle、Parent Stable 或 Manager 当前安装错绑，或终态冲突时返回错误。
    pub async fn rollback_failed_canary(
        &self,
        deployment_id: &PluginDeploymentId,
        rollback_release_id: &ReleaseId,
        trusted_stable_release_id: &ReleaseId,
    ) -> Result<PluginRollbackReceipt, PluginDeploymentError> {
        let _guard = self.controller.deployment_guard.lock().await;
        let transaction = self.deployment_store.transaction(deployment_id).await?;
        let recovered = self.recover_canary_locked(&transaction, false).await?;
        if recovered.record.state == PluginDeploymentStateV1::Promoted {
            return Err(PluginDeploymentError::Binding(
                "已 Promotion 的 Deployment 不得改写为 RolledBack".to_string(),
            ));
        }
        let rollback_release = self
            .release_archive
            .release(rollback_release_id)
            .await?
            .ok_or_else(|| PluginReleaseError::ReleaseNotFound(rollback_release_id.clone()))?;
        let trusted_stable_release = self
            .release_archive
            .release(trusted_stable_release_id)
            .await?
            .ok_or_else(|| {
                PluginReleaseError::ReleaseNotFound(trusted_stable_release_id.clone())
            })?;
        let evaluation = self
            .release_archive
            .evaluation_for_release(&rollback_release)
            .await?;
        validate_rollback_authorization(
            &recovered.deployment,
            &evaluation,
            &rollback_release,
            &trusted_stable_release,
        )?;
        let parent_plugin = plugin_genome(
            &recovered.deployment.parent_revision,
            &recovered.deployment.admission.release.release.plugin_id,
        )?;
        let current = self.controller.current_installed(&parent_plugin.id)?;
        let receipt = if verify_installed(&current, parent_plugin).is_ok() {
            self.controller
                .require_parent_stable(&recovered.deployment)
                .await?;
            PluginRollbackReceipt {
                installed: current,
                stable: recovered.deployment.parent_stable.clone(),
            }
        } else {
            self.controller
                .rollback_failed_canary_inner(
                    &recovered.deployment,
                    &evaluation,
                    &rollback_release,
                    &trusted_stable_release,
                )
                .await?
        };
        if recovered.record.state != PluginDeploymentStateV1::RolledBack {
            transaction.mark_rolled_back().await?;
        }
        Ok(receipt)
    }

    /// 在持有跨进程事务锁时从四个受信存储面重建部署，并按 Manager 摘要恢复安装窗口。
    async fn recover_canary_locked(
        &self,
        transaction: &PluginDeploymentTransaction<'_, '_>,
        install_if_planned: bool,
    ) -> Result<RecoveredCanaryDeployment, PluginDeploymentError> {
        let history = transaction.history().await?;
        let planned = history
            .first()
            .filter(|record| record.state == PluginDeploymentStateV1::Planned)
            .cloned()
            .ok_or_else(|| {
                PluginDeploymentError::Persistence(PluginDeploymentStoreError::NotFound(
                    transaction.deployment_id().clone(),
                ))
            })?;
        let admission = self
            .release_archive
            .canary_admission(&planned.canary_release_id)
            .await?;
        let parent_stable = planned.parent_stable.clone().ok_or_else(|| {
            PluginDeploymentError::Binding("持久化 Planned 缺少 Parent Stable 加法字段".to_string())
        })?;
        let candidate_revision = self
            .controller
            .publisher
            .resolver()
            .store()
            .get(&planned.candidate_revision_id)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("读取 Candidate Revision 失败：{error}"))
            })?
            .ok_or_else(|| {
                PluginDeploymentError::Binding("Candidate Revision 尚未登记".to_string())
            })?;
        let parent_revision = self
            .controller
            .publisher
            .resolver()
            .store()
            .get(&planned.parent_revision_id)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("读取 Parent Revision 失败：{error}"))
            })?
            .ok_or_else(|| PluginDeploymentError::Binding("Parent Revision 不存在".to_string()))?;
        validate_parent_binding(&candidate_revision, &parent_stable, &parent_revision)?;
        let binding = PluginCanaryDeploymentBindingV1::from_plan(
            &admission,
            &parent_stable,
            &candidate_revision,
        )?;
        if binding != planned.binding()? {
            return Err(PluginDeploymentError::Binding(
                "持久化 Planned 与 Release/Genome 重建结果不一致".to_string(),
            ));
        }
        let previous_bundle = transaction.previous_bundle(&planned).await?;
        let candidate_bundle = self
            .release_archive
            .release_bundle(&admission.release)
            .await?;
        let candidate_plugin =
            validate_canary_authorization(&admission, &candidate_revision, &candidate_bundle)?;
        let parent_plugin = plugin_genome(&parent_revision, &candidate_plugin.id)?;
        let prepared_parent = self
            .controller
            .prepare_bundle("persisted-parent", &previous_bundle)?;
        verify_prepared_bundle(&prepared_parent, parent_plugin)?;
        let current = self.controller.current_installed(&candidate_plugin.id)?;
        let mut record = history.last().cloned().expect("Planned 历史非空");
        let installed = if verify_installed(&current, candidate_plugin).is_ok() {
            if record.state == PluginDeploymentStateV1::Planned {
                record = transaction
                    .reconcile_canary_installed(&binding, &current)
                    .await?;
            }
            current
        } else if verify_installed(&current, parent_plugin).is_ok()
            && record.state == PluginDeploymentStateV1::Planned
            && install_if_planned
        {
            let prepared_bundle = self
                .controller
                .prepare_bundle("candidate", &candidate_bundle)?;
            verify_prepared_bundle(&prepared_bundle, candidate_plugin)?;
            let prepared = PreparedCanaryDeployment {
                admission: admission.clone(),
                candidate_revision: candidate_revision.clone(),
                parent_stable: parent_stable.clone(),
                parent_revision: parent_revision.clone(),
                previous_bundle: previous_bundle.clone(),
                prepared_bundle,
            };
            let deployment = self.controller.install_prepared_canary(prepared).await?;
            record = transaction
                .reconcile_canary_installed(&binding, deployment.installed())
                .await?;
            deployment.installed
        } else if verify_installed(&current, parent_plugin).is_ok()
            && matches!(
                record.state,
                PluginDeploymentStateV1::CanaryInstalled | PluginDeploymentStateV1::RolledBack
            )
        {
            current
        } else {
            return Err(PluginDeploymentError::Binding(
                "Plugin Manager 当前安装既非持久化 Parent 也非 Candidate".to_string(),
            ));
        };
        Ok(RecoveredCanaryDeployment {
            deployment: PluginCanaryDeployment {
                admission,
                candidate_revision,
                parent_stable,
                parent_revision,
                previous_bundle,
                installed,
            },
            record,
        })
    }
}

/// 验证当前 Stable 是同一部署已提交但尚未补记终态的精确 Promotion 结果。
fn stable_matches_promotion(
    stable: &StableGenomeRef,
    deployment: &PluginCanaryDeployment,
    evaluation: &PluginEvaluationArchiveRecordV1,
    release: &PluginReleaseArchiveRecordV1,
) -> bool {
    stable.lineage == deployment.parent_stable.lineage
        && stable.revision_id == deployment.candidate_revision.revision_id
        && stable.digest == deployment.candidate_revision.digest
        && stable.generation == deployment.parent_stable.generation.saturating_add(1)
        && stable.release_id.as_ref() == Some(&release.release.release_id)
        && stable.evaluation_report_id.as_ref() == Some(&evaluation.report_id)
        && stable.previous_revision_id.as_ref() == Some(&deployment.parent_stable.revision_id)
        && stable.rollback_of.is_none()
}

/// 一个仍由临时目录拥有的已解包 bundle。
struct PreparedBundle {
    _temporary: TempDir,
    root: PathBuf,
    tree_digest: ArtifactDigest,
}

impl PreparedBundle {
    /// 返回供 Plugin Manager 读取的 bundle 根目录。
    fn root(&self) -> &Path {
        &self.root
    }
}

/// 验证 Candidate 修订确实以当前 Stable 为 Parent。
fn validate_parent_binding(
    candidate: &GenomeRevision,
    parent_stable: &StableGenomeRef,
    parent: &GenomeRevision,
) -> Result<(), PluginDeploymentError> {
    candidate.validate().map_err(|error| {
        PluginDeploymentError::Binding(format!("Candidate Genome 无效：{error}"))
    })?;
    if parent.revision_id != parent_stable.revision_id
        || parent.digest != parent_stable.digest
        || candidate.metadata.parent.as_ref() != Some(&parent.revision_id)
    {
        return Err(PluginDeploymentError::Binding(
            "Candidate Genome 未精确继承当前 Parent Stable".to_string(),
        ));
    }
    Ok(())
}

/// 验证 Canary 归档、bundle 归档和 Candidate PluginGenome 的完整绑定。
fn validate_canary_authorization<'a>(
    admission: &PluginCanaryAdmissionV1,
    candidate: &'a GenomeRevision,
    bundle_bytes: &[u8],
) -> Result<&'a PluginGenome, PluginDeploymentError> {
    let release = &admission.release.release;
    release
        .validate()
        .map_err(|error| PluginDeploymentError::Binding(format!("Canary Release 无效：{error}")))?;
    admission
        .canary
        .validate()
        .map_err(|error| PluginDeploymentError::Binding(format!("Canary 记录无效：{error}")))?;
    if release.stage != PluginReleaseStage::Canary
        || admission.canary.state != PluginCanaryState::Planned
        || admission.canary.release_id != release.release_id
        || admission.canary.plugin_id != release.plugin_id
        || admission.canary.mutation_id != release.mutation_id
        || admission.canary.candidate_id != release.candidate_id
        || admission.canary.component_digest != release.attestation.component_digest
        || admission.canary.release_digest
            != release.signing_digest().map_err(|error| {
                PluginDeploymentError::Binding(format!("Canary Release 无效：{error}"))
            })?
    {
        return Err(PluginDeploymentError::Binding(
            "Canary Admission 与归档 Release 不一致".to_string(),
        ));
    }
    validate_bundle_artifact(&admission.release, bundle_bytes)?;
    let plugin = plugin_genome(candidate, &release.plugin_id)?;
    if plugin.bundle != release.bundle_digest {
        return Err(PluginDeploymentError::Binding(
            "Candidate PluginGenome 与 Canary bundle 摘要不一致".to_string(),
        ));
    }
    Ok(plugin)
}

/// 验证 Stable 记录承接当前部署的精确成功 Canary。
fn validate_stable_authorization(
    deployment: &PluginCanaryDeployment,
    evaluation: &PluginEvaluationArchiveRecordV1,
    stable: &PluginReleaseArchiveRecordV1,
) -> Result<(), PluginDeploymentError> {
    let canary = &deployment.admission.release;
    stable
        .release
        .validate()
        .map_err(|error| PluginDeploymentError::Binding(format!("Stable Release 无效：{error}")))?;
    if stable.release.stage != PluginReleaseStage::Stable
        || stable.release.canary_of.as_ref() != Some(&canary.release.release_id)
        || stable.release.plugin_id != canary.release.plugin_id
        || stable.release.mutation_id != canary.release.mutation_id
        || stable.release.candidate_id != canary.release.candidate_id
        || stable.release.bundle_digest != canary.release.bundle_digest
        || stable.release.attestation.component_digest
            != canary.release.attestation.component_digest
        || stable.evaluation_report_artifact != evaluation.report_artifact
        || stable.release.evaluation_report_digest != evaluation.report_artifact.digest
        || evaluation.plugin_id != stable.release.plugin_id
    {
        return Err(PluginDeploymentError::Binding(
            "Stable Release 未承接当前 Canary 或 Evaluation 归档".to_string(),
        ));
    }
    Ok(())
}

/// 验证 Rollback 指向当前失败 Canary 和先前受信 Stable Component。
fn validate_rollback_authorization(
    deployment: &PluginCanaryDeployment,
    evaluation: &PluginEvaluationArchiveRecordV1,
    rollback: &PluginReleaseArchiveRecordV1,
    trusted_stable: &PluginReleaseArchiveRecordV1,
) -> Result<(), PluginDeploymentError> {
    let canary = &deployment.admission.release.release;
    rollback.release.validate().map_err(|error| {
        PluginDeploymentError::Binding(format!("Rollback Release 无效：{error}"))
    })?;
    trusted_stable.release.validate().map_err(|error| {
        PluginDeploymentError::Binding(format!("先前 Stable Release 无效：{error}"))
    })?;
    if rollback.release.stage != PluginReleaseStage::Rollback
        || rollback.release.rollback_of.as_ref() != Some(&canary.release_id)
        || rollback.release.plugin_id != canary.plugin_id
        || rollback.release.mutation_id != canary.mutation_id
        || rollback.release.candidate_id != canary.candidate_id
        || rollback.evaluation_report_artifact != evaluation.report_artifact
        || rollback.release.evaluation_report_digest != evaluation.report_artifact.digest
        || evaluation.plugin_id != rollback.release.plugin_id
    {
        return Err(PluginDeploymentError::Binding(
            "Rollback Release 未承接当前失败 Canary".to_string(),
        ));
    }
    if trusted_stable.release.stage != PluginReleaseStage::Stable
        || trusted_stable.release.plugin_id != canary.plugin_id
        || rollback.release.rollback_target_component_digest.as_ref()
            != Some(&trusted_stable.release.attestation.component_digest)
        || rollback
            .rollback_target_artifact
            .as_ref()
            .map(|artifact| &artifact.digest)
            != Some(&trusted_stable.release.attestation.component_digest)
    {
        return Err(PluginDeploymentError::Binding(
            "Rollback 目标不是先前受信 Stable Component".to_string(),
        ));
    }
    let parent_plugin = plugin_genome(&deployment.parent_revision, &canary.plugin_id)?;
    if parent_plugin.bundle != trusted_stable.release.bundle_digest
        || trusted_stable.bundle_artifact.digest != parent_plugin.bundle
    {
        return Err(PluginDeploymentError::Binding(
            "先前 Stable Release 的 bundle 与 Parent PluginGenome 不一致".to_string(),
        ));
    }
    Ok(())
}

/// 验证归档 bundle 字节与 Release 信封和 CAS 引用一致。
fn validate_bundle_artifact(
    release: &PluginReleaseArchiveRecordV1,
    bundle_bytes: &[u8],
) -> Result<(), PluginDeploymentError> {
    let digest = digest_bytes(bundle_bytes)?;
    if digest != release.release.bundle_digest
        || digest != release.bundle_artifact.digest
        || release.bundle_artifact.size_bytes != bundle_bytes.len() as u64
    {
        return Err(PluginDeploymentError::Binding(
            "bundle 字节与 Release/CAS 归档不一致".to_string(),
        ));
    }
    Ok(())
}

/// 在 AgentGenome 中查找唯一目标插件，并先验证完整 Revision。
///
/// `GenomeRevision::validate` 会验证插件列表按 ID 严格排序，因此同 ID 重复项会在查找前被拒绝。
fn plugin_genome<'a>(
    revision: &'a GenomeRevision,
    plugin_id: &str,
) -> Result<&'a PluginGenome, PluginDeploymentError> {
    revision.validate().map_err(|error| {
        PluginDeploymentError::Binding(format!("Genome Revision 无效：{error}"))
    })?;
    revision
        .genome
        .plugins
        .iter()
        .find(|plugin| plugin.id == plugin_id)
        .ok_or_else(|| {
            PluginDeploymentError::Binding(format!("Genome 未固定发布插件 `{plugin_id}`"))
        })
}

/// 验证解包后的树摘要与目标 PluginGenome 一致。
fn verify_prepared_bundle(
    prepared: &PreparedBundle,
    expected: &PluginGenome,
) -> Result<(), PluginDeploymentError> {
    if prepared.tree_digest != expected.bundle {
        return Err(PluginDeploymentError::Binding(format!(
            "插件 `{}` 的 bundle 树摘要与 PluginGenome 不一致",
            expected.id
        )));
    }
    Ok(())
}

/// 验证 Plugin Manager 锁记录的身份、ABI、启用状态和树摘要。
fn verify_installed(
    installed: &InstalledPlugin,
    expected: &PluginGenome,
) -> Result<(), PluginDeploymentError> {
    let installed_digest =
        ArtifactDigest::from_sha256_hex(installed.sha256.clone()).map_err(|error| {
            PluginDeploymentError::Manager(format!("Plugin Manager 摘要无效：{error}"))
        })?;
    if installed.id != expected.id
        || installed.version != expected.version
        || installed.api_version != expected.api_version
        || installed_digest != expected.bundle
        || !installed.enabled
    {
        return Err(PluginDeploymentError::Binding(format!(
            "插件 `{}` 的安装记录与 PluginGenome 不一致",
            expected.id
        )));
    }
    Ok(())
}

/// 计算协议使用的 SHA-256 Artifact 摘要。
fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, PluginDeploymentError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| PluginDeploymentError::Bundle(format!("构造 bundle 摘要失败：{error}")))
}

/// M8 生产插件部署错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginDeploymentError {
    /// 跨进程部署状态、CAS 或事务锁失败。
    #[error("插件部署状态持久化失败：{0}")]
    Persistence(#[from] PluginDeploymentStoreError),
    /// 受信 Release Archive 无法重建部署授权输入。
    #[error("插件部署 Release Archive 读取失败：{0}")]
    ReleaseArchive(#[from] PluginReleaseError),
    /// Release、Genome、安装记录或并发前置条件错绑。
    #[error("插件部署绑定无效：{0}")]
    Binding(String),
    /// Plugin Manager 状态无法可信读取或归档。
    #[error("Plugin Manager 状态无效：{0}")]
    Manager(String),
    /// Candidate 原子替换失败；Plugin Manager 保留旧安装。
    #[error("Canary 插件安装失败：{0}")]
    Install(String),
    /// Rollback 或补偿的原子替换失败。
    #[error("旧 Stable 插件恢复失败：{0}")]
    RollbackInstall(String),
    /// 确定性 bundle 编解码或摘要构造失败。
    #[error("插件 bundle 无效：{0}")]
    Bundle(String),
    /// Genome Store、Resolver 或 Stable Publisher 失败。
    #[error("插件 Genome 部署失败：{0}")]
    Genome(String),
    /// 受信暂存目录无法安全创建或使用。
    #[error("插件部署暂存失败：{0}")]
    Staging(String),
    /// Canary 已安装后的步骤失败，并已尽力恢复旧 bundle。
    #[error(
        "{stage}失败，已尽力恢复旧 bundle；原始错误：{primary}；补偿错误：{restoration_error:?}"
    )]
    PostCanaryFailure {
        /// 失败部署阶段。
        stage: &'static str,
        /// 原始失败，不会被补偿结果覆盖。
        primary: String,
        /// 恢复成功时为空；恢复失败时保存完整错误文本。
        restoration_error: Option<String>,
    },
}
