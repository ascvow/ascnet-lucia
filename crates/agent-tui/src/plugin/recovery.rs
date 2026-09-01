//! 无需启动 TUI 的受管理插件恢复命令。

use crate::cli::{PluginsArgs, PluginsCommand};
use crate::config::lucia_home_dir;
use agent_plugin_manager::PluginManager;
use anyhow::Result;

/// 执行显式插件恢复命令，不加载、实例化或执行任何 WASM。
///
/// `list` 只查询 Manager 的有效状态；`disable` 通过 Plugin Manager 原子收缩启用状态、授权摘要和
/// Capability Owner。锁文件读取、依赖校验或原子写入失败时返回错误。
pub(crate) fn run(args: PluginsArgs) -> Result<()> {
    let manager = PluginManager::new(lucia_home_dir()?);
    match args.command {
        PluginsCommand::List => {
            let plugins = manager.list()?;
            if plugins.is_empty() {
                println!("未安装受管理插件");
            } else {
                for plugin in plugins {
                    let status = if plugin.enabled {
                        "已启用"
                    } else {
                        "已禁用"
                    };
                    println!(
                        "{}\t{}\t{}\tapi_version={}",
                        plugin.id, plugin.version, status, plugin.api_version
                    );
                }
            }
        }
        PluginsCommand::Disable { plugin_id } => {
            let plugin = manager.disable(&plugin_id)?;
            println!("已禁用 {} {}", plugin.id, plugin.version);
        }
    }
    Ok(())
}
