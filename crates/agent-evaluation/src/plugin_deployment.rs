//! M8 插件 Canary、Stable Promotion 与生产回滚的部署执行器。
//!
//! 发布签名、Gate、Canary 状态机与只追加归档仍由 `PluginReleaseController` 负责；本模块只
//! 消费它已经返回的归档结果，把确定性 bundle、Plugin Manager 与 Genome Stable 指针绑定。

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

        let installed = self
            .manager
            .replace(prepared.root())
            .map_err(|error| PluginDeploymentError::Install(error.to_string()))?;
        if let Err(error) = verify_installed(&installed, candidate_plugin)
            .and_then(|_| self.verify_current_install(candidate_plugin))
        {
            return Err(self.compensated_failure(
                "Canary 安装后验证",
                error,
                &previous_bundle,
                parent_plugin,
            ));
        }
        let observed_parent = self
            .publisher
            .resolver()
            .stable_reference(lineage)
            .await
            .map_err(|error| {
                PluginDeploymentError::Genome(format!("重新读取 Parent Stable 失败：{error}"))
            });
        match observed_parent {
            Ok(observed) if observed == parent_stable => {}
            Ok(_) => {
                return Err(self.compensated_failure(
                    "Canary 安装并发前置条件",
                    PluginDeploymentError::Binding(
                        "Canary 安装期间 Stable Genome 已变化".to_string(),
                    ),
                    &previous_bundle,
                    parent_plugin,
                ));
            }
            Err(error) => {
                return Err(self.compensated_failure(
                    "Canary 安装后 Stable 复核",
                    error,
                    &previous_bundle,
                    parent_plugin,
                ));
            }
        }

        Ok(PluginCanaryDeployment {
            admission,
            candidate_revision: candidate_revision.clone(),
            parent_stable,
            parent_revision,
            previous_bundle,
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
        validate_rollback_authorization(
            &deployment,
            evaluation,
            rollback_release,
            trusted_stable_release,
        )?;
        self.require_parent_stable(&deployment).await?;
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
        self.require_parent_stable(&deployment).await?;
        Ok(PluginRollbackReceipt {
            installed,
            stable: deployment.parent_stable,
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
