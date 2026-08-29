//! Lucia 命令行参数与非交互式子命令定义。

use clap::{Args as ClapArgs, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(author, version, about = "Lucia 交互式 ReAct Agent")]
pub(crate) struct Args {
    /// 不启动 TUI，执行指定的 Lucia 管理命令。
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// 初始化配置文件后退出；默认写入 `$LUCIA_HOME/config.toml`。
    #[arg(long = "init", alias = "init-config")]
    pub(crate) init: bool,

    /// 使用内置脚本模型。
    #[arg(long)]
    pub(crate) demo: bool,

    /// TOML 配置文件路径；默认读取 `LUCIA_CONFIG` 或 `$LUCIA_HOME/config.toml`。
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
