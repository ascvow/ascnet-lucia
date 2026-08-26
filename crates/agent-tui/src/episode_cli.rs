//! Episode 证据的只读查询与脱敏导出命令。

use crate::app_config::lucia_home_dir;
use agent_evolution::{
    load_episode_evidence, EpisodeQuery, EpisodeStore, FileArtifactStore, FileEpisodeStore,
};
use agent_evolution_protocol::{EpisodeId, Outcome};
use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use serde::Serialize;
use std::path::PathBuf;

/// `lucia episode` 的公共参数。
#[derive(Debug, ClapArgs)]
pub(crate) struct EpisodeArgs {
    /// Evolution 数据根目录；默认使用 `$LUCIA_HOME/evolution`。
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// 要执行的只读 Episode 操作。
    #[command(subcommand)]
    command: EpisodeCommand,
}

/// Episode 的只读运维命令。
#[derive(Debug, Subcommand)]
enum EpisodeCommand {
    /// 列出 Episode Header，可按会话或 Outcome 过滤。
    List(EpisodeListArgs),
    /// 校验并显示一条 Episode 的监督摘要。
    Inspect(EpisodeTargetArgs),
    /// 导出已经持久化的脱敏事件和监督证据。
    Export(EpisodeExportArgs),
}

/// Episode 列表参数。
#[derive(Debug, ClapArgs)]
struct EpisodeListArgs {
    /// 只显示指定会话。
    #[arg(long)]
    session_id: Option<String>,
    /// 只显示指定终态。
    #[arg(long, value_enum)]
    outcome: Option<OutcomeArg>,
    /// 输出稳定 JSON，而不是紧凑文本。
    #[arg(long)]
    json: bool,
}

/// 单个 Episode 目标参数。
#[derive(Debug, ClapArgs)]
struct EpisodeTargetArgs {
    /// 强类型 Episode ID。
    episode_id: String,
    /// 输出稳定 JSON，而不是紧凑文本。
    #[arg(long)]
    json: bool,
}

/// 脱敏导出参数。
#[derive(Debug, ClapArgs)]
struct EpisodeExportArgs {
    /// 强类型 Episode ID。
    episode_id: String,
    /// 明确确认只导出 Recorder 已脱敏内容；缺少该标志时拒绝执行。
    #[arg(long, required = true)]
    redacted: bool,
}

/// CLI 可选的 Episode Outcome。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutcomeArg {
    /// 可信成功。
    Success,
    /// 恢复后成功。
    SuccessWithRecovery,
    /// 任务失败。
    TaskFailure,
    /// 安全失败。
    SafetyFailure,
    /// 预算失败。
    BudgetFailure,
    /// 已取消。
    Cancelled,
    /// 基础设施失败。
    InfrastructureFailure,
    /// 不可验证。
    Unverifiable,
}

impl From<OutcomeArg> for Outcome {
    fn from(value: OutcomeArg) -> Self {
        match value {
            OutcomeArg::Success => Self::Success,
            OutcomeArg::SuccessWithRecovery => Self::SuccessWithRecovery,
            OutcomeArg::TaskFailure => Self::TaskFailure,
            OutcomeArg::SafetyFailure => Self::SafetyFailure,
            OutcomeArg::BudgetFailure => Self::BudgetFailure,
            OutcomeArg::Cancelled => Self::Cancelled,
            OutcomeArg::InfrastructureFailure => Self::InfrastructureFailure,
            OutcomeArg::Unverifiable => Self::Unverifiable,
        }
    }
}

/// 一次脱敏导出的稳定 JSON 结构。
#[derive(Serialize)]
struct RedactedEpisodeExport {
    /// 只追加 Episode Header。
    episode: agent_evolution_protocol::Episode,
    /// Recorder 已脱敏事件。
    events: Vec<agent_evolution_protocol::EpisodeEvent>,
    /// Supervisor Incident。
    incidents: Vec<agent_evolution_protocol::Incident>,
    /// 初始 Outcome 修订。
    initial_outcome_revision: Option<agent_evolution_protocol::OutcomeRevision>,
}

/// 执行 Episode CLI；该路径不启动 TUI、模型或插件。
pub(crate) async fn run(args: EpisodeArgs) -> Result<()> {
    let root = args.root.unwrap_or(lucia_home_dir()?.join("evolution"));
    let episodes = FileEpisodeStore::new(root.join("episodes"));
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    match args.command {
        EpisodeCommand::List(options) => {
            let query = EpisodeQuery {
                outcome: options.outcome.map(Outcome::from),
                session_id: options.session_id,
            };
            let records = episodes.query(&query).await.context("查询 Episode 失败")?;
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&records).context("序列化 Episode 列表失败")?
                );
            } else {
                for episode in records {
                    println!(
                        "{}\t{}\t{}\t{:?}",
                        episode.episode_id,
                        episode.run_id,
                        episode.session_id,
                        episode.outcome.unwrap_or(Outcome::Unverifiable)
                    );
                }
            }
        }
        EpisodeCommand::Inspect(options) => {
            let episode_id =
                EpisodeId::new(options.episode_id).context("参数不是合法的 EpisodeId")?;
            let evidence = load_episode_evidence(&episodes, &artifacts, &episode_id)
                .await
                .context("读取 Episode 证据失败")?;
            if options.json {
                let export = RedactedEpisodeExport {
                    episode: evidence.episode,
                    events: evidence.events,
                    incidents: evidence.incidents,
                    initial_outcome_revision: evidence.initial_outcome_revision,
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&export).context("序列化 Episode 证据失败")?
                );
            } else {
                println!(
                    "Episode: {}\nRun: {}\nSession: {}\nOutcome: {:?}\nEvents: {}\nIncidents: {}\nInitial revision: {}",
                    evidence.episode.episode_id,
                    evidence.episode.run_id,
                    evidence.episode.session_id,
                    evidence.episode.outcome,
                    evidence.events.len(),
                    evidence.incidents.len(),
                    evidence.initial_outcome_revision.is_some(),
                );
            }
        }
        EpisodeCommand::Export(options) => {
            debug_assert!(options.redacted, "Clap 已强制 --redacted");
            let episode_id =
                EpisodeId::new(options.episode_id).context("参数不是合法的 EpisodeId")?;
            let evidence = load_episode_evidence(&episodes, &artifacts, &episode_id)
                .await
                .context("读取 Episode 证据失败")?;
            anyhow::ensure!(
                evidence.episode.data_policy.permits_mutation_input(),
                "Episode 数据策略不允许导出为 Evolution 输入"
            );
            let export = RedactedEpisodeExport {
                episode: evidence.episode,
                events: evidence.events,
                incidents: evidence.incidents,
                initial_outcome_revision: evidence.initial_outcome_revision,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&export).context("序列化脱敏 Episode 导出失败")?
            );
        }
    }
    Ok(())
}
