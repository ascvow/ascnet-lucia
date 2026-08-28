//! TUI 人工插件管理与 Genome 基线提交的应用服务。

use crate::genome_binding::snapshot_plugin_environment;
use crate::plugin_startup::{
    discover_plugin_manifests, merge_plugin_manifests, remove_disabled_plugin_manifests,
};
use agent_evolution::{
    FileStableGenomePublisher, GenomeResolver, GenomeSelector, GenomeStore, StableGenomeRef,
};
use agent_evolution_protocol::{
    GenomeDigest, GenomeMetadata, GenomeRevision, GenomeRevisionId, PluginEnvironmentDigest,
    RevisionOrigin,
};
use agent_plugin_manager::{
    GithubInstallOptions, GithubPluginSource, InstallOptions, InstalledPlugin, PluginManager,
    PluginManagerStateSnapshot, RegistryRequest,
};
use anyhow::{anyhow, Context, Result};
use std::{collections::HashMap, path::PathBuf};

/// 人工插件管理允许执行的本地写操作。
///
/// Registry 和 GitHub 获取仍由 Plugin Manager 负责，但 TUI 的基础事务只接受用户明确选择的
/// 本地 Bundle，不下载、不构建、不签名，也不允许 Evolution 调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManualPluginAction {
    /// 从官方 Registry 安装用户明确选择的兼容版本。
    InstallRegistry {
        /// Registry `name[@semver]` 请求。
        request: String,
        /// 安装后是否启用根插件。
        enabled: bool,
    },
    /// 从用户明确指定的 GitHub Release 安装插件。
    InstallGithub {
        /// GitHub owner/repository 或受支持 URL。
        source: String,
        /// 安装后是否启用。
        enabled: bool,
    },
    /// 安装一个尚未受管理的本地 Bundle。
    InstallLocal {
        /// Bundle 根目录。
        bundle: PathBuf,
        /// 安装后是否启用。
        enabled: bool,
    },
    /// 用本地 Bundle 替换同 ID 的已安装版本。
    ReplaceLocal {
        /// 新 Bundle 根目录。
        bundle: PathBuf,
        /// 替换后是否启用。
        enabled: bool,
    },
    /// 将一个 Registry 插件更新到当前 ABI 的最新兼容版本。
    UpdateRegistry {
        /// 插件稳定 ID。
        plugin_id: String,
    },
    /// 启用已安装插件。
    Enable {
        /// 插件稳定 ID。
        plugin_id: String,
    },
    /// 禁用已安装插件。
    Disable {
        /// 插件稳定 ID。
        plugin_id: String,
    },
    /// 移除已安装插件。
    Remove {
        /// 插件稳定 ID。
        plugin_id: String,
    },
    /// 从人工安装版本历史回滚固定 Bundle。
    Rollback {
        /// 插件稳定 ID。
        plugin_id: String,
        /// 用户明确选择的历史版本。
        version: String,
    },
    /// 选择独占 Capability Owner。
    SelectCapabilityOwner {
        /// Capability 稳定 ID。
        capability_id: String,
        /// Owner 插件稳定 ID。
        plugin_id: String,
    },
    /// 清除独占 Capability Owner 选择。
    ClearCapabilityOwner {
        /// Capability 稳定 ID。
        capability_id: String,
    },
}

impl ManualPluginAction {
    /// 返回写入人工 Genome 修订的稳定中文审计描述。
    fn description(&self) -> String {
        match self {
            Self::InstallRegistry { request, .. } => {
                format!("人工从 Registry 安装插件：{request}")
            }
            Self::InstallGithub { source, .. } => {
                format!("人工从 GitHub Release 安装插件：{source}")
            }
            Self::InstallLocal { bundle, .. } => {
                format!("人工安装本地插件 Bundle：{}", bundle.display())
            }
            Self::ReplaceLocal { bundle, .. } => {
                format!("人工替换本地插件 Bundle：{}", bundle.display())
            }
            Self::UpdateRegistry { plugin_id } => {
                format!("人工更新 Registry 插件：{plugin_id}")
            }
            Self::Enable { plugin_id } => format!("人工启用插件：{plugin_id}"),
            Self::Disable { plugin_id } => format!("人工禁用插件：{plugin_id}"),
            Self::Remove { plugin_id } => format!("人工移除插件：{plugin_id}"),
            Self::Rollback { plugin_id, version } => {
                format!("人工回滚插件：{plugin_id} -> {version}")
            }
            Self::SelectCapabilityOwner {
                capability_id,
                plugin_id,
            } => format!("人工选择 Capability Owner：{capability_id} -> {plugin_id}"),
            Self::ClearCapabilityOwner { capability_id } => {
                format!("人工清除 Capability Owner：{capability_id}")
            }
        }
    }
}

/// 一次已提交人工插件管理基线的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginManagementBaseline {
    /// 新 Stable 指向的不可变修订。
    pub(crate) revision_id: GenomeRevisionId,
    /// 新 Genome 行为摘要。
    pub(crate) genome_digest: GenomeDigest,
    /// 新冻结插件环境摘要。
    pub(crate) plugin_environment_digest: PluginEnvironmentDigest,
    /// 新 Stable 代数。
    pub(crate) generation: u64,
}

/// 人工插件管理操作及其基线提交结果。
#[derive(Debug, Clone)]
pub(crate) struct PluginManagementOutcome {
    /// Plugin Manager 返回的用户可读操作结果。
    pub(crate) message: String,
    /// 状态没有变化时为 `None`，否则必须包含已提交的新人工基线。
    pub(crate) baseline: Option<PluginManagementBaseline>,
}

/// 仅供 TUI 人工控制面持有的可写 Plugin Manager 应用服务。
///
/// Evolution 代码只读取 PluginEnvironmentSnapshot，不依赖也不持有本类型。服务在一个事务中
/// 编排 Manager 写入、完整插件快照、不可变 Genome Revision 和 Stable CAS；任一后续步骤失败
/// 都恢复操作前的 Manager 状态，活动 Session 与已经实例化的 Host 始终保持不变。
#[derive(Debug, Clone)]
pub(crate) struct PluginManagementService {
    manager: PluginManager,
    publisher: FileStableGenomePublisher,
    lineage: String,
    lucia_home: PathBuf,
    base_manifest_paths: Vec<PathBuf>,
    base_capability_selection: HashMap<String, String>,
    disabled_plugins: Vec<String>,
}

impl PluginManagementService {
    /// 创建 TUI 人工插件管理服务，不访问文件系统。
    ///
    /// `base_manifest_paths` 与 `base_capability_selection` 是启动参数和普通配置中的固定来源；
    /// 用户目录、受管理目录与官方目录会在每次事务中重新扫描。
    pub(crate) fn new(
        lucia_home: PathBuf,
        registry_root: PathBuf,
        lineage: String,
        base_manifest_paths: Vec<PathBuf>,
        base_capability_selection: HashMap<String, String>,
        disabled_plugins: Vec<String>,
    ) -> Self {
        Self {
            manager: PluginManager::new(&lucia_home),
            publisher: FileStableGenomePublisher::new(registry_root),
            lineage,
            lucia_home,
            base_manifest_paths,
            base_capability_selection,
            disabled_plugins,
        }
    }

    /// 返回全部受管理插件，供 TUI 列表只读展示。
    ///
    /// # Errors
    ///
    /// 插件锁文件不存在以外的读取或 Schema 错误会返回。
    pub(crate) fn list(&self) -> Result<Vec<InstalledPlugin>> {
        self.manager.list()
    }

    /// 返回只读的 Installed Version History，供 TUI 展示和人工选择回滚目标。
    ///
    /// # Errors
    ///
    /// 插件锁文件格式无效或读取失败时返回错误。
    pub(crate) fn list_version_history(&self) -> Result<Vec<InstalledPlugin>> {
        self.manager.list_version_history()
    }

    /// 返回当前 Stable 插件环境及摘要，供 Evolution 页面或插件页只读展示。
    ///
    /// # Errors
    ///
    /// Stable、Revision 或插件环境摘要损坏时返回错误。
    pub(crate) async fn frozen_environment(
        &self,
    ) -> Result<(StableGenomeRef, PluginEnvironmentDigest)> {
        let stable = self
            .publisher
            .resolver()
            .stable_reference(&self.lineage)
            .await
            .context("读取插件管理 Stable 基线失败")?;
        let revision = self
            .publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(stable.revision_id.clone()))
            .await
            .context("读取插件管理 Stable Genome 失败")?;
        let digest = revision
            .genome
            .plugin_environment_snapshot()
            .digest()
            .context("计算冻结插件环境摘要失败")?;
        Ok((stable, digest))
    }

    /// 执行一次人工插件写操作并提交新的 PluginManagement Stable 基线。
    ///
    /// 当前 Session 和已实例化 Host 不会被重载。成功后只有新 Session 会从更新后的 Stable
    /// 解析新插件环境；旧 Candidate 因 expected-current 已变化而不能自动 Promotion。
    ///
    /// # Errors
    ///
    /// Manager 写操作、完整快照、Genome 登记或 Stable CAS 任一步失败时返回错误，并先尝试
    /// 恢复操作前 Manager 状态；补偿恢复也失败时错误文本会同时保留两个原因。
    pub(crate) async fn apply(
        &self,
        action: ManualPluginAction,
    ) -> Result<PluginManagementOutcome> {
        let expected = self
            .publisher
            .resolver()
            .stable_reference(&self.lineage)
            .await
            .context("读取人工插件变更前 Stable 基线失败")?;
        let parent = self
            .publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(expected.revision_id.clone()))
            .await
            .context("读取人工插件变更前 Genome 失败")?;
        let before_plugins = self.manager.list()?;
        let before_runtime = self.manager.runtime_config()?;
        let snapshot = self.manager.capture_state()?;
        let message = match self.perform_action(&action).await {
            Ok(message) => message,
            Err(error) => return self.fail_with_restore(&snapshot, error),
        };
        let after_plugins = match self.manager.list() {
            Ok(plugins) => plugins,
            Err(error) => return self.fail_with_restore(&snapshot, error),
        };
        let after_runtime = match self.manager.runtime_config() {
            Ok(runtime) => runtime,
            Err(error) => return self.fail_with_restore(&snapshot, error),
        };
        if before_plugins == after_plugins && before_runtime == after_runtime {
            return Ok(PluginManagementOutcome {
                message,
                baseline: None,
            });
        }
        let environment = match self.current_plugin_environment() {
            Ok(environment) => environment,
            Err(error) => return self.fail_with_restore(&snapshot, error),
        };
        let environment_digest = match environment.digest() {
            Ok(digest) => digest,
            Err(error) => return self.fail_with_restore(&snapshot, error.into()),
        };
        let mut genome = parent.genome.clone();
        genome.plugins = environment.plugins;
        genome.capability_owners = environment.capability_owners;
        let revision = match GenomeRevision::create(
            genome,
            GenomeMetadata {
                origin: RevisionOrigin::PluginManagement,
                description: Some(action.description()),
                parent: Some(parent.revision_id.clone()),
                mutation: None,
                ..GenomeMetadata::default()
            },
        ) {
            Ok(revision) => revision,
            Err(error) => return self.fail_with_restore(&snapshot, error.into()),
        };
        if let Err(error) = self.publisher.resolver().store().append(&revision).await {
            return self.fail_with_restore(&snapshot, error.into());
        }
        let published = match self
            .publisher
            .publish_manual_bound(&expected, &revision)
            .await
        {
            Ok(reference) => reference,
            Err(error) => return self.fail_with_restore(&snapshot, error.into()),
        };
        Ok(PluginManagementOutcome {
            message,
            baseline: Some(PluginManagementBaseline {
                revision_id: revision.revision_id,
                genome_digest: revision.digest,
                plugin_environment_digest: environment_digest,
                generation: published.generation,
            }),
        })
    }

    /// 执行不涉及网络或构建的单个 Manager 写操作。
    async fn perform_action(&self, action: &ManualPluginAction) -> Result<String> {
        match action {
            ManualPluginAction::InstallRegistry { request, enabled } => {
                let request = RegistryRequest::parse(request)?;
                let result = self.manager.install_registry(&request, *enabled).await?;
                if result.already_satisfied {
                    Ok(format!(
                        "{} 已安装且满足 {}",
                        result.requested, request.requirement
                    ))
                } else {
                    let installed = result
                        .installed
                        .iter()
                        .map(|plugin| format!("{} {}", plugin.id, plugin.version))
                        .collect::<Vec<_>>()
                        .join("、");
                    Ok(format!("已从 Registry 安装 {installed}"))
                }
            }
            ManualPluginAction::InstallGithub { source, enabled } => {
                let source = GithubPluginSource::parse(source)?;
                let result = self
                    .manager
                    .install_github(
                        &source,
                        GithubInstallOptions {
                            enabled: *enabled,
                            ..GithubInstallOptions::default()
                        },
                    )
                    .await?;
                Ok(format!(
                    "已从 GitHub {} 安装 {} {}",
                    result.release_tag, result.plugin.id, result.plugin.version
                ))
            }
            ManualPluginAction::InstallLocal { bundle, enabled } => {
                let plugin = self
                    .manager
                    .install_with_options(bundle, InstallOptions { enabled: *enabled })?;
                Ok(format!("已安装 {} {}", plugin.id, plugin.version))
            }
            ManualPluginAction::ReplaceLocal { bundle, enabled } => {
                let plugin = self
                    .manager
                    .replace_with_options(bundle, InstallOptions { enabled: *enabled })?;
                Ok(format!("已更新 {} 到 {}", plugin.id, plugin.version))
            }
            ManualPluginAction::UpdateRegistry { plugin_id } => {
                let result = self.manager.update_registry(Some(plugin_id)).await?;
                let updated = result
                    .updated
                    .iter()
                    .map(|plugin| format!("{} {}", plugin.id, plugin.version))
                    .collect::<Vec<_>>()
                    .join("、");
                if updated.is_empty() {
                    Ok(format!("插件 {plugin_id} 已是最新兼容版本"))
                } else {
                    Ok(format!("已从 Registry 更新 {updated}"))
                }
            }
            ManualPluginAction::Enable { plugin_id } => {
                let plugin = self.manager.enable(plugin_id)?;
                Ok(format!("已启用 {} {}", plugin.id, plugin.version))
            }
            ManualPluginAction::Disable { plugin_id } => {
                let plugin = self.manager.disable(plugin_id)?;
                Ok(format!("已禁用 {} {}", plugin.id, plugin.version))
            }
            ManualPluginAction::Remove { plugin_id } => {
                let plugin = self.manager.remove(plugin_id)?;
                Ok(format!(
                    "已移除 {} {}；Bundle 已保留到人工安装版本历史",
                    plugin.id, plugin.version
                ))
            }
            ManualPluginAction::Rollback { plugin_id, version } => {
                let plugin = self.manager.rollback(plugin_id, version)?;
                Ok(format!(
                    "已人工回滚 {} 到固定版本 {}",
                    plugin.id, plugin.version
                ))
            }
            ManualPluginAction::SelectCapabilityOwner {
                capability_id,
                plugin_id,
            } => {
                let plugin = self.manager.select(capability_id, plugin_id)?;
                Ok(format!(
                    "能力 {capability_id} 已选择插件 {} {}",
                    plugin.id, plugin.version
                ))
            }
            ManualPluginAction::ClearCapabilityOwner { capability_id } => {
                match self.manager.clear_selection(capability_id)? {
                    Some(plugin_id) => {
                        Ok(format!("已清除能力 {capability_id} 的插件选择 {plugin_id}"))
                    }
                    None => Ok(format!("能力 {capability_id} 没有显式插件选择")),
                }
            }
        }
    }

    /// 按与启动装配一致的优先级重建完整运行插件环境。
    fn current_plugin_environment(
        &self,
    ) -> Result<agent_evolution_protocol::PluginEnvironmentSnapshot> {
        let mut manifests = self.base_manifest_paths.clone();
        let user_manifests = discover_plugin_manifests(&self.lucia_home.join("plugins"))
            .context("扫描用户插件目录失败")?;
        merge_plugin_manifests(&mut manifests, user_manifests);
        let managed = self.manager.runtime_config()?;
        merge_plugin_manifests(&mut manifests, managed.manifest_paths);
        let official = discover_plugin_manifests(&self.lucia_home.join("official-plugins"))
            .context("扫描官方插件目录失败")?;
        merge_plugin_manifests(&mut manifests, official);
        remove_disabled_plugin_manifests(&mut manifests, &self.disabled_plugins);
        let mut selections = self.base_capability_selection.clone();
        for (capability, owner) in managed.capability_selection {
            selections.entry(capability).or_insert(owner);
        }
        snapshot_plugin_environment(&manifests, &selections)
    }

    /// 把主错误与补偿恢复结果合并为单一失败返回。
    fn fail_with_restore<T>(
        &self,
        snapshot: &PluginManagerStateSnapshot,
        error: anyhow::Error,
    ) -> Result<T> {
        match self.manager.restore_state(snapshot) {
            Ok(()) => Err(error),
            Err(rollback) => Err(anyhow!(
                "人工插件管理失败：{error:#}；恢复操作前插件状态也失败：{rollback:#}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution::{FileGenomeStore, GenomeResolver, GenomeStore};
    use agent_evolution_protocol::{
        AgentGenome, ModelGenome, PromptGenome, RuntimeIdentity, ToolProfileGenome,
        GENOME_SCHEMA_VERSION,
    };
    use agent_tool::ExecutionPolicy;
    use std::collections::{BTreeMap, BTreeSet};

    /// 构造插件管理事务测试使用的独立目录。
    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lucia-plugin-management-{label}-{}",
            agent_session::SessionId::generate()
        ))
    }

    /// 创建不依赖真实 Component 编译的最小本地 Bundle。
    fn create_bundle(root: &std::path::Path, id: &str, version: &str) -> PathBuf {
        let bundle = root.join(format!("fixture-{id}-{version}"));
        std::fs::create_dir_all(&bundle).expect("应创建测试 Bundle");
        std::fs::write(bundle.join("plugin.wasm"), b"fixture").expect("应写入测试 WASM");
        std::fs::write(
            bundle.join("plugin.toml"),
            format!(
                r#"
[plugin]
id = "{id}"
name = "测试插件"
version = "{version}"
api_version = "0.7.0"
wasm = "plugin.wasm"
"#
            ),
        )
        .expect("应写入测试 Manifest");
        bundle
    }

    /// 创建只含固定 Agent 行为、尚未装载插件的 Parent Genome。
    fn parent_revision() -> GenomeRevision {
        GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".into(),
                    git_commit: "fixture".into(),
                    git_dirty: true,
                    target_triple: "fixture".into(),
                    features: BTreeSet::from(["plugins".into()]),
                },
                model: ModelGenome {
                    provider: "fixture".into(),
                    provider_kind: "fixture".into(),
                    model: "fixture".into(),
                    base_url: None,
                    protocol: None,
                    max_tokens: None,
                    temperature: None,
                    stream: false,
                    provider_options_digest: None,
                },
                prompt: PromptGenome::default(),
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("Parent Genome 应合法")
    }

    /// 初始化插件管理服务所需的不可变 Registry 与 Stable Parent。
    async fn service_fixture(
        root: &std::path::Path,
        base_manifests: Vec<PathBuf>,
    ) -> (PluginManagementService, GenomeRevision) {
        let registry = root.join("evolution");
        let parent = parent_revision();
        FileGenomeStore::new(registry.join("genomes"))
            .append(&parent)
            .await
            .expect("应登记 Parent Genome");
        FileStableGenomePublisher::new(&registry)
            .publish("stable/general", &parent, 1)
            .await
            .expect("应发布 Parent Stable");
        (
            PluginManagementService::new(
                root.to_path_buf(),
                registry,
                "stable/general".into(),
                base_manifests,
                HashMap::new(),
                Vec::new(),
            ),
            parent,
        )
    }

    /// TUI 本地安装必须创建 PluginManagement 修订，且不能绑定 Evolution Release。
    #[tokio::test]
    async fn local_install_creates_plugin_management_baseline() {
        let root = temp_root("install-baseline");
        let fixture_root = root.with_extension("fixtures");
        let bundle = create_bundle(&fixture_root, "echo", "1.0.0");
        let (service, parent) = service_fixture(&root, Vec::new()).await;

        let outcome = service
            .apply(ManualPluginAction::InstallLocal {
                bundle,
                enabled: true,
            })
            .await
            .expect("人工安装应提交新基线");
        let baseline = outcome.baseline.expect("插件变化应产生基线");
        let stable = service
            .publisher
            .resolver()
            .stable_reference("stable/general")
            .await
            .expect("应读取新 Stable");
        let revision = service
            .publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(stable.revision_id.clone()))
            .await
            .expect("应读取人工基线修订");

        assert_eq!(stable.generation, 2);
        assert_eq!(stable.revision_id, baseline.revision_id);
        assert!(stable.release_id.is_none());
        assert!(stable.evaluation_report_id.is_none());
        assert_eq!(revision.metadata.origin, RevisionOrigin::PluginManagement);
        assert_eq!(revision.metadata.parent, Some(parent.revision_id));
        assert!(revision.metadata.mutation.is_none());
        assert_eq!(revision.genome.plugins.len(), 1);
        assert_eq!(revision.genome.plugins[0].id, "echo");
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// TUI 人工回滚必须创建新的 PluginManagement 基线，并保留被替换版本供再次回滚。
    #[tokio::test]
    async fn manual_rollback_creates_plugin_management_baseline() {
        let root = temp_root("rollback-baseline");
        let fixture_root = root.with_extension("fixtures");
        let first = create_bundle(&fixture_root, "echo", "1.0.0");
        let second = create_bundle(&fixture_root, "echo", "2.0.0");
        let (service, _) = service_fixture(&root, Vec::new()).await;
        service
            .apply(ManualPluginAction::InstallLocal {
                bundle: first,
                enabled: true,
            })
            .await
            .expect("初始人工安装应成功");
        service
            .apply(ManualPluginAction::ReplaceLocal {
                bundle: second,
                enabled: true,
            })
            .await
            .expect("人工替换应成功");

        let outcome = service
            .apply(ManualPluginAction::Rollback {
                plugin_id: "echo".into(),
                version: "1.0.0".into(),
            })
            .await
            .expect("人工回滚应成功");
        let baseline = outcome.baseline.expect("人工回滚必须产生新基线");
        let stable = service
            .publisher
            .resolver()
            .stable_reference("stable/general")
            .await
            .expect("应读取回滚后的 Stable");
        let revision = service
            .publisher
            .resolver()
            .resolve(&GenomeSelector::Revision(stable.revision_id.clone()))
            .await
            .expect("应读取人工回滚修订");

        assert_eq!(stable.generation, 4);
        assert_eq!(stable.revision_id, baseline.revision_id);
        assert!(stable.release_id.is_none());
        assert!(stable.evaluation_report_id.is_none());
        assert_eq!(revision.metadata.origin, RevisionOrigin::PluginManagement);
        assert!(revision.metadata.mutation.is_none());
        assert_eq!(revision.genome.plugins[0].version, "1.0.0");
        assert_eq!(
            service.list_version_history().expect("应读取人工版本历史")[0].version,
            "2.0.0"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// Genome 快照提交失败时必须补偿恢复 Plugin Manager，不能留下半完成安装。
    #[tokio::test]
    async fn failed_baseline_restores_plugin_manager_state() {
        let root = temp_root("restore-on-failure");
        let fixture_root = root.with_extension("fixtures");
        let bundle = create_bundle(&fixture_root, "echo", "1.0.0");
        let missing = root.join("missing/plugin.toml");
        let (service, _) = service_fixture(&root, vec![missing]).await;

        let error = service
            .apply(ManualPluginAction::InstallLocal {
                bundle,
                enabled: true,
            })
            .await
            .expect_err("无效固定 Manifest 应阻止基线提交");

        assert!(error.to_string().contains("读取插件 manifest"));
        assert!(service
            .manager
            .list()
            .expect("恢复后锁文件应可读")
            .is_empty());
        let stable = service
            .publisher
            .resolver()
            .stable_reference("stable/general")
            .await
            .expect("Stable 应保持 Parent");
        assert_eq!(stable.generation, 1);
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }
}
