//! TUI 人工插件管理的应用服务。

use agent_plugin_manager::{
    GithubInstallOptions, GithubPluginSource, InstallOptions, InstalledPlugin, PluginManager,
    PluginManagerStateSnapshot, RegistryRequest,
};
use anyhow::{anyhow, Result};
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
/// 写操作失败时恢复操作前的 Manager 状态，活动 Session 与已经实例化的 Host 始终保持不变。
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

    /// 执行一次人工插件写操作。
    ///
    /// 当前 Session 和已实例化 Host 不会被重载；成功后只有新 Session 使用新的插件环境。
    ///
    /// # Errors
    ///
    /// Manager 写操作或状态读取失败时返回错误，并先尝试恢复操作前 Manager 状态；补偿恢复
    /// 也失败时错误文本会同时保留两个原因。
    pub(crate) async fn apply(
        &self,
        action: ManualPluginAction,
    ) -> Result<PluginManagementOutcome> {
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
                enabled: true,
            })
            .await
            .expect("人工安装应成功");

        assert!(outcome.changed);
        let installed = service.list().expect("应读取已安装插件");
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].id, "echo");
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

        assert!(outcome.changed);
        assert_eq!(service.list().expect("应读取插件")[0].version, "1.0.0");
        assert_eq!(
            service.list_version_history().expect("应读取人工版本历史")[0].version,
            "2.0.0"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
        let _ = tokio::fs::remove_dir_all(fixture_root).await;
    }

    /// 写操作失败时必须补偿恢复 Plugin Manager，不能留下半完成安装。
    #[tokio::test]
    async fn failed_action_restores_plugin_manager_state() {
        let root = temp_root("restore-on-failure");
        let service = PluginManagementService::new(root.clone());

        let error = service
            .apply(ManualPluginAction::InstallLocal {
                bundle: root.join("missing-bundle"),
                enabled: true,
            })
            .await
            .expect_err("无效 Bundle 应失败");

        assert!(!error.to_string().is_empty());
        assert!(service
            .manager
            .list()
            .expect("恢复后锁文件应可读")
            .is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
