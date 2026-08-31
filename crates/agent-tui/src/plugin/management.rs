//! TUI 人工插件管理的应用服务。

use agent_plugin_host::audit::{snapshot_plugin_authorization_profile, PluginAuthorizationProfile};
use agent_plugin_manager::{
    GithubInstallOptions, GithubPluginSource, InstallOptions, InstalledPlugin,
    PluginBundleIdentity, PluginManager, RegistryRequest,
};
use anyhow::Result;
use std::path::PathBuf;

/// 人工插件管理允许执行的本地写操作。
///
/// Registry 和 GitHub 获取仍由 Plugin Manager 负责，但 TUI 的基础事务只接受用户明确选择的
/// 本地 Bundle，不下载、不构建、不签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManualPluginAction {
    /// 从官方 Registry 安装用户明确选择的兼容版本。
    InstallRegistry {
        /// Registry `name[@semver]` 请求。
        request: String,
        /// 是否请求安装后直接启用；人工 Registry 安装只接受 `false`。
        enabled: bool,
    },
    /// 从用户明确指定的 GitHub Release 安装插件。
    InstallGithub {
        /// GitHub owner/repository 或受支持 URL。
        source: String,
        /// 是否请求安装后直接启用；人工 GitHub 安装只接受 `false`。
        enabled: bool,
    },
    /// 安装一个尚未受管理的本地 Bundle。
    InstallLocal {
        /// Bundle 根目录。
        bundle: PathBuf,
        /// 是否请求安装后直接启用；人工本地安装只接受 `false`。
        enabled: bool,
    },
    /// 用本地 Bundle 替换同 ID 的已安装版本。
    ReplaceLocal {
        /// 新 Bundle 根目录。
        bundle: PathBuf,
        /// 是否请求继承启用状态；人工替换始终保持禁用并要求重新审查 Bundle 身份。
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
        /// 第一次确认时用户看到的不可变 Bundle 身份。
        expected_identity: PluginBundleIdentity,
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
    /// 在用户完成启用确认后，原子启用插件并选择独占 Capability Owner。
    EnableAndSelectCapabilityOwner {
        /// Capability 稳定 ID。
        capability_id: String,
        /// 已完成来源与能力确认的插件稳定 ID。
        plugin_id: String,
        /// Owner 启用确认绑定的不可变 Bundle 身份。
        expected_identity: PluginBundleIdentity,
    },
    /// 清除独占 Capability Owner 选择。
    ClearCapabilityOwner {
        /// Capability 稳定 ID。
        capability_id: String,
    },
}

/// 人工插件管理操作的结果。
#[derive(Debug, Clone)]
pub(crate) struct PluginManagementOutcome {
    /// Plugin Manager 返回的用户可读操作结果。
    pub(crate) message: String,
    /// 本次操作是否真正改变了插件状态。
    pub(crate) changed: bool,
}

/// 仅供 TUI 人工控制面持有的可写 Plugin Manager 应用服务。
///
/// Manager 在自身事务内保证写入原子性；应用层不使用旧快照补偿并发变更。活动 Session 与
/// 已经实例化的 Host 始终保持不变。
#[derive(Debug, Clone)]
pub(crate) struct PluginManagementService {
    manager: PluginManager,
}

impl PluginManagementService {
    /// 创建 TUI 人工插件管理服务，不访问文件系统。
    pub(crate) fn new(lucia_home: PathBuf) -> Self {
        Self {
            manager: PluginManager::new(&lucia_home),
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

    /// 返回与列表中精确 Bundle 身份绑定的 Host 校验授权 profile，供用户在启用前审查。
    ///
    /// # Errors
    ///
    /// 列表读取后 Bundle 身份变化，或插件 Bundle、manifest、能力声明无效时返回错误，
    /// 不展示可能属于另一 Bundle 的权限信息。
    pub(crate) fn authorization_profile(
        &self,
        plugin: &InstalledPlugin,
    ) -> Result<PluginAuthorizationProfile> {
        let manifest = self
            .manager
            .inspect_if_matches(&plugin.id, &plugin.bundle_identity())?;
        snapshot_plugin_authorization_profile(&manifest)
    }

    /// 执行一次人工插件写操作。
    ///
    /// 当前 Session 和已实例化 Host 不会被重载；成功后只有新 Session 使用新的插件环境。
    ///
    /// # Errors
    ///
    /// Manager 原子写操作或后续状态读取失败时返回错误。应用层不用旧快照补偿，
    /// 避免失败处理覆盖其他进程已提交的禁用或撤权。
    pub(crate) async fn apply(
        &self,
        action: ManualPluginAction,
    ) -> Result<PluginManagementOutcome> {
        let before_plugins = self.manager.list()?;
        let before_runtime = self.manager.runtime_config()?;
        let message = self.perform_action(&action).await?;
        let after_plugins = self.manager.list()?;
        let after_runtime = self.manager.runtime_config()?;
        let changed = before_plugins != after_plugins || before_runtime != after_runtime;
        Ok(PluginManagementOutcome { message, changed })
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
                Ok(format_plugin_update(&plugin))
            }
            ManualPluginAction::UpdateRegistry { plugin_id } => {
                let result = self.manager.update_registry(Some(plugin_id)).await?;
                let updated = result
                    .updated
                    .iter()
                    .map(format_plugin_update)
                    .collect::<Vec<_>>()
                    .join("、");
                if updated.is_empty() {
                    Ok(format!("插件 {plugin_id} 已是最新兼容版本"))
                } else {
                    Ok(format!("已从 Registry 更新 {updated}"))
                }
            }
            ManualPluginAction::Enable {
                plugin_id,
                expected_identity,
            } => {
                let plugin = self
                    .manager
                    .enable_if_matches(plugin_id, expected_identity)?;
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
                if plugin.enabled {
                    Ok(format!(
                        "已人工回滚 {} 到固定版本 {}",
                        plugin.id, plugin.version
                    ))
                } else {
                    Ok(format!(
                        "已人工回滚 {} 到固定版本 {}；插件保持禁用，请查看来源和能力后连续按两次 e 确认启用",
                        plugin.id, plugin.version
                    ))
                }
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
            ManualPluginAction::EnableAndSelectCapabilityOwner {
                capability_id,
                plugin_id,
                expected_identity,
            } => {
                let plugin =
                    self.manager
                        .enable_and_select(capability_id, plugin_id, expected_identity)?;
                Ok(format!(
                    "已启用 {} {}，并选择为能力 {capability_id} 的 Owner",
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
}

/// 格式化更新结果；能力变化导致禁用时必须明确提示需要重新确认。
fn format_plugin_update(plugin: &InstalledPlugin) -> String {
    if plugin.enabled {
        format!("{} {}", plugin.id, plugin.version)
    } else {
        format!(
            "{} {}（已禁用，需确认当前能力后重新启用）",
            plugin.id, plugin.version
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 本地安装必须写入 Plugin Manager 并报告状态已改变。
    #[tokio::test]
    async fn local_install_updates_plugin_manager() {
        let root = temp_root("install");
        let fixture_root = root.with_extension("fixtures");
        let bundle = create_bundle(&fixture_root, "echo", "1.0.0");
        let service = PluginManagementService::new(root.clone());

        let outcome = service
            .apply(ManualPluginAction::InstallLocal {
                bundle,
                enabled: false,
            })
            .await
            .expect("人工安装应成功");

        assert!(outcome.changed);
        let installed = service.list().expect("应读取已安装插件");
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].id, "echo");
        assert!(!installed[0].enabled);
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// 人工回滚必须恢复固定版本，并保留被替换版本供再次回滚。
    #[tokio::test]
    async fn manual_rollback_restores_selected_version() {
        let root = temp_root("rollback");
        let fixture_root = root.with_extension("fixtures");
        let first = create_bundle(&fixture_root, "echo", "1.0.0");
        let second = create_bundle(&fixture_root, "echo", "2.0.0");
        let service = PluginManagementService::new(root.clone());
        service
            .apply(ManualPluginAction::InstallLocal {
                bundle: first,
                enabled: false,
            })
            .await
            .expect("初始人工安装应成功");
        service
            .apply(ManualPluginAction::ReplaceLocal {
                bundle: second,
                enabled: false,
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

        assert!(outcome.changed);
        assert_eq!(service.list().expect("应读取插件")[0].version, "1.0.0");
        assert!(!service.list().expect("应读取插件")[0].enabled);
        assert!(outcome.message.contains("连续按两次 e"));
        assert_eq!(
            service.list_version_history().expect("应读取人工版本历史")[0].version,
            "2.0.0"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// Capability Owner 选择只负责路由，不能把尚未审查的插件隐式启用。
    #[tokio::test]
    async fn capability_owner_selection_requires_enabled_plugin() {
        let root = temp_root("owner-requires-enable");
        let fixture_root = root.with_extension("fixtures");
        let bundle = create_bundle(&fixture_root, "provider", "1.0.0");
        let mut manifest =
            std::fs::read_to_string(bundle.join("plugin.toml")).expect("应读取测试 Manifest");
        manifest.push_str(
            "\n[[provides]]\nid = \"agent.context-loader\"\nversion = \"1.0.0\"\nmode = \"exclusive\"\n",
        );
        std::fs::write(bundle.join("plugin.toml"), manifest).expect("应写入独占能力");
        let service = PluginManagementService::new(root.clone());
        service
            .apply(ManualPluginAction::InstallLocal {
                bundle,
                enabled: false,
            })
            .await
            .expect("禁用插件应安装成功");

        let error = service
            .apply(ManualPluginAction::SelectCapabilityOwner {
                capability_id: "agent.context-loader".into(),
                plugin_id: "provider".into(),
            })
            .await
            .expect_err("owner 选择不得绕过双重启用确认");

        assert!(error.to_string().contains("尚未启用"));
        assert!(!service.list().expect("应读取插件")[0].enabled);
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// 完成来源与能力确认后，应用服务应以单一事务启用插件并选择 Capability Owner。
    #[tokio::test]
    async fn confirmed_owner_selection_enables_and_selects_atomically() {
        let root = temp_root("owner-confirmed");
        let fixture_root = root.with_extension("fixtures");
        let bundle = create_bundle(&fixture_root, "provider", "1.0.0");
        let mut manifest =
            std::fs::read_to_string(bundle.join("plugin.toml")).expect("应读取测试 Manifest");
        manifest.push_str(
            "\n[[provides]]\nid = \"agent.context-loader\"\nversion = \"1.0.0\"\nmode = \"exclusive\"\n",
        );
        std::fs::write(bundle.join("plugin.toml"), manifest).expect("应写入独占能力");
        let service = PluginManagementService::new(root.clone());
        service
            .apply(ManualPluginAction::InstallLocal {
                bundle,
                enabled: false,
            })
            .await
            .expect("禁用插件应安装成功");
        let confirmed_identity = service.list().expect("应读取待确认插件")[0].bundle_identity();

        let outcome = service
            .apply(ManualPluginAction::EnableAndSelectCapabilityOwner {
                capability_id: "agent.context-loader".into(),
                plugin_id: "provider".into(),
                expected_identity: confirmed_identity,
            })
            .await
            .expect("确认后应原子启用并选择 Owner");

        assert!(outcome.changed);
        assert!(service.list().expect("应读取插件")[0].enabled);
        assert_eq!(
            service
                .manager
                .runtime_config()
                .expect("运行时配置应有效")
                .capability_selection
                .get("agent.context-loader")
                .map(String::as_str),
            Some("provider")
        );
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// 两次启用确认之间 Bundle 被替换时，应用服务不得把旧确认应用到新身份。
    #[tokio::test]
    async fn enable_rejects_bundle_identity_changed_after_confirmation() {
        let root = temp_root("enable-identity-changed");
        let fixture_root = root.with_extension("fixtures");
        let first = create_bundle(&fixture_root, "echo", "1.0.0");
        let second = create_bundle(&fixture_root, "echo", "2.0.0");
        let service = PluginManagementService::new(root.clone());
        service
            .apply(ManualPluginAction::InstallLocal {
                bundle: first,
                enabled: false,
            })
            .await
            .expect("旧 Bundle 应以禁用状态安装");
        let confirmed_identity = service.list().expect("应读取待确认插件")[0].bundle_identity();
        service
            .manager
            .replace_with_options(&second, InstallOptions { enabled: false })
            .expect("并行管理操作应能替换 Bundle");

        let error = service
            .apply(ManualPluginAction::Enable {
                plugin_id: "echo".into(),
                expected_identity: confirmed_identity,
            })
            .await
            .expect_err("旧确认不得启用替换后的 Bundle");

        assert!(error.to_string().contains("Bundle 身份已在确认后变化"));
        let current = service.list().expect("拒绝后应读取当前插件");
        assert_eq!(current[0].version, "2.0.0");
        assert!(!current[0].enabled);
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }
}
