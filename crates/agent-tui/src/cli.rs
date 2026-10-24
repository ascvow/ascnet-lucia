//! Lucia 命令行参数与非交互式子命令定义。

use clap::{Args as ClapArgs, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(author, version, about = "Lucia 交互式 ReAct Agent")]
pub(crate) struct Args {
    /// 不启动 TUI，执行指定的 Lucia 管理命令。
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// 初始化配置文件后退出；默认写入 `$LUCIA_HOME/config.json`。
    #[arg(long = "init", alias = "init-config")]
    pub(crate) init: bool,

    /// 使用内置脚本模型。
    #[arg(long)]
    pub(crate) demo: bool,

    /// JSON 配置文件路径；默认读取 `LUCIA_CONFIG` 或 `$LUCIA_HOME/config.json`。
    #[arg(long, global = true)]
    pub(crate) config: Option<PathBuf>,

    /// 可选的 agent 事件 JSONL 输出文件，用于排查模型请求与工具调用。
    #[arg(long = "events-jsonl")]
    pub(crate) events_jsonl: Option<PathBuf>,

    /// 项目会话根目录；覆盖配置文件和 `$LUCIA_HOME/projects` 默认值。
    #[arg(long = "sessions-dir")]
    pub(crate) sessions_dir: Option<PathBuf>,

    /// 要恢复和持续更新的稳定会话标识；覆盖配置中的默认值。
    #[arg(long = "session-id")]
    pub(crate) session_id: Option<String>,

    /// 恢复最近更新的持久化会话；显式 `--session-id` 优先。
    #[arg(long = "resume-latest")]
    pub(crate) resume_latest: bool,

    /// 列出持久化会话后退出，不连接模型服务。
    #[arg(long = "list-sessions")]
    pub(crate) list_sessions: bool,

    /// 将脱敏 Host 能力审计写入专用私有目录；有界队列与轮转，不包含正文或密钥。
    #[cfg(feature = "plugins")]
    #[arg(long = "capability-audit-dir")]
    pub(crate) capability_audit_dir: Option<PathBuf>,

    /// 插件 manifest 路径；可以重复传入并按参数顺序占用 UI 插槽。
    #[cfg(feature = "plugins")]
    #[arg(long = "plugin-manifest")]
    pub(crate) plugin_manifests: Vec<PathBuf>,
}

/// Lucia 的非交互式管理命令。
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// 对整个 Lucia 安装执行无侵入只读诊断。
    Doctor(DoctorArgs),
    /// 在 TUI 无法启动时执行最小受管理插件恢复操作。
    #[cfg(feature = "plugins")]
    Plugins(PluginsArgs),
}

/// 全局诊断参数。
#[derive(Debug, ClapArgs)]
pub(crate) struct DoctorArgs {
    /// 使用稳定 JSON 结构输出诊断报告。
    #[arg(long)]
    pub(crate) json: bool,
    /// 额外检查 GitHub API 连通性；默认诊断不联网。
    #[arg(long)]
    pub(crate) network: bool,
}

/// 无需启动 TUI 的插件恢复参数。
#[cfg(feature = "plugins")]
#[derive(Debug, ClapArgs)]
pub(crate) struct PluginsArgs {
    /// 要执行的只读查询或安全收缩操作。
    #[command(subcommand)]
    pub(crate) command: PluginsCommand,
}

/// TUI 启动失败时仍可使用的最小插件管理命令。
#[cfg(feature = "plugins")]
#[derive(Debug, Subcommand)]
pub(crate) enum PluginsCommand {
    /// 列出锁文件中的当前受管理插件，不加载或执行 WASM。
    List,
    /// 禁用指定插件并撤销其授权与 Capability Owner。
    Disable {
        /// 要禁用的稳定插件 ID。
        plugin_id: String,
    },
}

#[cfg(all(test, feature = "plugins"))]
mod tests {
    use super::*;

    /// 插件恢复命令必须按稳定层级解析，避免退化为 TUI 启动参数。
    #[test]
    fn parses_plugin_disable_recovery_command() {
        let args = Args::try_parse_from(["lucia", "plugins", "disable", "teammate"])
            .expect("插件恢复命令应可解析");

        assert!(matches!(
            args.command,
            Some(Command::Plugins(PluginsArgs {
                command: PluginsCommand::Disable { plugin_id }
            })) if plugin_id == "teammate"
        ));
    }
}
