//! `lucia plugin` 的应用层命令编排。

use super::{lucia_home_dir, PluginArgs, PluginCommand};
use agent_plugin_manager::{
    GithubInstallOptions, GithubPluginSource, InstallOptions, PluginManager, RegistryRequest,
};
use anyhow::{bail, Result};
use std::path::Path;

/// 执行插件管理命令；安装和状态规则全部委托给 `agent-plugin-manager`。
pub(crate) async fn run(args: PluginArgs) -> Result<()> {
    let manager = PluginManager::new(lucia_home_dir()?);
    let mut plugin_environment_changed = false;
    match args.command {
        PluginCommand::Install {
            source,
            local,
            github,
            tag,
            asset,
            disabled,
        } => {
            if local && github {
                bail!("--local 与 --github 不能同时使用");
            }
            if local {
                if tag.is_some() || asset.is_some() {
                    bail!("本地安装不能使用 --tag 或 --asset");
                }
                let plugin = manager.install_with_options(
                    Path::new(&source),
                    InstallOptions { enabled: !disabled },
                )?;
                println!(
                    "已安装 {} {}（{}）",
                    plugin.id,
                    plugin.version,
                    state_label(plugin.enabled)
                );
                plugin_environment_changed = true;
            } else if github {
                let github = GithubPluginSource::parse(&source)?;
                let result = manager
                    .install_github(
                        &github,
                        GithubInstallOptions {
                            enabled: !disabled,
                            tag,
                            asset,
                            ..GithubInstallOptions::default()
                        },
                    )
                    .await?;
                println!(
                    "已从 {} 的 {} 安装 {} {}（{}）",
                    github.repository_url(),
                    result.release_tag,
                    result.plugin.id,
                    result.plugin.version,
                    state_label(result.plugin.enabled)
                );
                if result.checksum_verified {
                    println!("Release SHA-256 校验通过：{}", result.asset_name);
                } else {
                    println!(
                        "警告：Release 未提供 {}.sha256，已记录安装后 bundle 摘要",
                        result.asset_name
                    );
                }
                plugin_environment_changed = true;
            } else {
                if tag.is_some() || asset.is_some() {
                    bail!("Registry 安装不能使用 --tag 或 --asset；任意 Release 请增加 --github");
                }
                let request = RegistryRequest::parse(&source)?;
                let result = manager.install_registry(&request, !disabled).await?;
                if result.already_satisfied {
                    println!("{} 已安装且满足 {}", result.requested, request.requirement);
                } else {
                    plugin_environment_changed = true;
                    for plugin in result.installed {
                        println!(
                            "已安装 {} {}（{}）",
                            plugin.id,
                            plugin.version,
                            state_label(plugin.enabled)
                        );
                    }
                }
            }
        }
        PluginCommand::Search { query } => {
            let results = manager.search_registry(&query).await?;
            if results.is_empty() {
                println!("Registry 中没有匹配插件");
            } else {
                for plugin in results {
                    let ownership = if plugin.official {
                        "官方"
                    } else {
                        "第三方"
                    };
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        plugin.name,
                        plugin.latest_version,
                        ownership,
                        plugin.publisher,
                        plugin.description
                    );
                }
            }
        }
        PluginCommand::Outdated => {
            let plugins = manager.outdated_registry().await?;
            if plugins.is_empty() {
                println!("已安装插件均为 Registry 中的最新兼容版本");
            } else {
                for plugin in plugins {
                    println!(
                        "{}\t{} -> {}",
                        plugin.name, plugin.current_version, plugin.latest_version
                    );
                }
            }
        }
        PluginCommand::Update { id } => {
            let result = manager.update_registry(id.as_deref()).await?;
            if result.updated.is_empty() {
                println!("没有可更新的 Registry 插件");
            } else {
                plugin_environment_changed = true;
                for plugin in result.updated {
                    println!("已更新 {} 到 {}", plugin.id, plugin.version);
                }
            }
        }
        PluginCommand::List => {
            let plugins = manager.list()?;
            if plugins.is_empty() {
                println!("未安装受管理插件");
            } else {
                for plugin in plugins {
                    println!(
                        "{}\t{}\t{}\t{}",
                        plugin.id,
                        plugin.version,
                        state_label(plugin.enabled),
                        plugin.source
                    );
                }
            }
        }
        PluginCommand::Enable { id } => {
            let plugin = manager.enable(&id)?;
            plugin_environment_changed = true;
            println!("已启用 {} {}", plugin.id, plugin.version);
        }
        PluginCommand::Disable { id } => {
            let plugin = manager.disable(&id)?;
            plugin_environment_changed = true;
            println!("已禁用 {} {}", plugin.id, plugin.version);
        }
        PluginCommand::Select { capability, plugin } => {
            let selected = manager.select(&capability, &plugin)?;
            plugin_environment_changed = true;
            println!(
                "能力 {} 已选择插件 {} {}",
                capability, selected.id, selected.version
            );
        }
        PluginCommand::Unselect { capability } => match manager.clear_selection(&capability)? {
            Some(plugin) => {
                plugin_environment_changed = true;
                println!("已清除能力 {} 的插件选择 {}", capability, plugin);
            }
            None => println!("能力 {} 没有显式插件选择", capability),
        },
        PluginCommand::Remove { id } => {
            let plugin = manager.remove(&id)?;
            plugin_environment_changed = true;
            println!("已移除 {} {}", plugin.id, plugin.version);
        }
    }
    if plugin_environment_changed {
        println!(
            "插件环境已改变。新的 Agent Run 将使用新的人工插件管理基线；现有 Session 继续使用启动时快照，旧 Evolution Candidate 不得自动应用到新基线。"
        );
    }
    Ok(())
}

/// 返回插件启用状态的中文标签。
fn state_label(enabled: bool) -> &'static str {
    if enabled {
        "已启用"
    } else {
        "已禁用"
    }
}
