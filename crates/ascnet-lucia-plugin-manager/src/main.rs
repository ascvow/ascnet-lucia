//! Lucia 本地插件管理命令行。

use agent_plugin_manager::{DoctorSeverity, InstallOptions, PluginManager};
use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Lucia 插件管理命令。
#[derive(Debug, Parser)]
#[command(name = "agent-plugin", version, about = "管理本地 Agent 插件")]
struct Cli {
    /// 插件管理根目录，也可通过 `LUCIA_PLUGIN_ROOT` 指定。
    #[arg(long, env = "LUCIA_PLUGIN_ROOT")]
    root: PathBuf,

    /// 要执行的插件管理操作。
    #[command(subcommand)]
    command: Command,
}

/// 支持的插件管理子命令。
#[derive(Debug, Subcommand)]
enum Command {
    /// 从本地 bundle 目录安装插件。
    Install {
        /// 包含 `plugin.toml` 的 bundle 目录。
        bundle: PathBuf,
        /// 安装后保持禁用状态。
        #[arg(long)]
        disabled: bool,
    },
    /// 列出所有受管理插件。
    List,
    /// 启用指定插件。
    Enable {
        /// 插件稳定 ID。
        id: String,
    },
    /// 禁用指定插件。
    Disable {
        /// 插件稳定 ID。
        id: String,
    },
    /// 为独占能力选择 owner，并启用目标插件。
    Select {
        /// 稳定能力 ID。
        capability: String,
        /// 提供该能力的插件 ID。
        plugin: String,
    },
    /// 清除指定独占能力的 owner 选择。
    Unselect {
        /// 稳定能力 ID。
        capability: String,
    },
    /// 移除指定插件。
    Remove {
        /// 插件稳定 ID。
        id: String,
    },
    /// 检查插件完整性、依赖和能力冲突。
    Doctor,
}

/// 执行插件管理命令并以中文输出结果。
fn main() -> Result<()> {
    let cli = Cli::parse();
    let manager = PluginManager::new(cli.root);
    match cli.command {
        Command::Install { bundle, disabled } => {
            let plugin =
                manager.install_with_options(bundle, InstallOptions { enabled: !disabled })?;
            println!(
                "已安装 {} {}（{}）",
                plugin.id,
                plugin.version,
                state_label(plugin.enabled)
            );
        }
        Command::List => {
            let plugins = manager.list()?;
            if plugins.is_empty() {
                println!("未安装插件");
            } else {
                for plugin in plugins {
                    println!(
                        "{}\t{}\t{}\t{}",
                        plugin.id,
                        plugin.version,
                        state_label(plugin.enabled),
                        plugin.sha256
                    );
                }
            }
        }
        Command::Enable { id } => {
            let plugin = manager.enable(&id)?;
            println!("已启用 {} {}", plugin.id, plugin.version);
        }
        Command::Disable { id } => {
            let plugin = manager.disable(&id)?;
            println!("已禁用 {} {}", plugin.id, plugin.version);
        }
        Command::Select { capability, plugin } => {
            let selected = manager.select(&capability, &plugin)?;
            println!(
                "能力 {} 已选择插件 {} {}",
                capability, selected.id, selected.version
            );
        }
        Command::Unselect { capability } => match manager.clear_selection(&capability)? {
            Some(plugin) => println!("已清除能力 {} 的插件选择 {}", capability, plugin),
            None => println!("能力 {} 没有显式插件选择", capability),
        },
        Command::Remove { id } => {
            let plugin = manager.remove(&id)?;
            println!("已移除 {} {}", plugin.id, plugin.version);
        }
        Command::Doctor => {
            let report = manager.doctor()?;
            if report.is_healthy() {
                println!("插件诊断通过，共检查 {} 个插件", report.checked_plugins);
            } else {
                for issue in report.issues {
                    let severity = match issue.severity {
                        DoctorSeverity::Error => "错误",
                        DoctorSeverity::Warning => "警告",
                    };
                    let owner = issue.plugin_id.as_deref().unwrap_or("全局");
                    eprintln!("[{severity}] {owner}: {}", issue.message);
                }
                bail!("插件诊断未通过");
            }
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
