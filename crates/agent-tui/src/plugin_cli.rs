//! `lucia plugin` 的应用层命令编排。

use super::{lucia_home_dir, PluginArgs, PluginCommand};
use agent_plugin_manager::{
    GithubInstallOptions, GithubPluginSource, InstallOptions, PluginManager,
};
use anyhow::{bail, Result};
use std::path::Path;

/// 执行插件管理命令；安装和状态规则全部委托给 `agent-plugin-manager`。
pub(crate) async fn run(args: PluginArgs) -> Result<()> {
    let manager = PluginManager::new(lucia_home_dir()?);
    match args.command {
        PluginCommand::Install {
            source,
            local,
            tag,
            asset,
            disabled,
        } => {
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
            } else {
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
            println!("已启用 {} {}", plugin.id, plugin.version);
        }
        PluginCommand::Disable { id } => {
            let plugin = manager.disable(&id)?;
            println!("已禁用 {} {}", plugin.id, plugin.version);
        }
        PluginCommand::Select { capability, plugin } => {
            let selected = manager.select(&capability, &plugin)?;
            println!(
                "能力 {} 已选择插件 {} {}",
                capability, selected.id, selected.version
            );
        }
        PluginCommand::Unselect { capability } => match manager.clear_selection(&capability)? {
            Some(plugin) => println!("已清除能力 {} 的插件选择 {}", capability, plugin),
            None => println!("能力 {} 没有显式插件选择", capability),
        },
        PluginCommand::Remove { id } => {
            let plugin = manager.remove(&id)?;
            println!("已移除 {} {}", plugin.id, plugin.version);
        }
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
