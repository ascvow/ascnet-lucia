//! Evolution Scorecard 与历史分析的非交互 CLI。

use crate::app_config::lucia_home_dir;
use agent_evolution::{
    compute_history, compute_scorecard, diff_genomes, load_evaluation_report,
    verify_allowed_genome_diff, CapabilityMapRow, EvolutionCertificate, EvolutionFunnel,
    EvolutionHistory, EvolutionScorecard, EvolutionVerdictPolicy, FileArtifactStore,
    FileEvaluationReportStore, FileEvolutionArchive, FileGenomeResolver, GenomeResolver,
    GenomeSelector, InheritanceMetrics, LineageNode, Rate, ResourceDelta,
};
use agent_evolution_protocol::{
    EvaluationReport, GenomeDiff, GenomeRevision, GenomeRevisionId, MutationSurface, ReleaseId,
};
use anyhow::{anyhow, Context, Result};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::IsTerminal,
    path::PathBuf,
};

/// `lucia evolution` 的公共参数。
#[derive(Debug, ClapArgs)]
pub(crate) struct EvolutionArgs {
    /// Evolution 数据根目录；默认使用 `$LUCIA_HOME/evolution`。
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// 要执行的 Evolution 查询。
    #[command(subcommand)]
    command: EvolutionCommand,
}

/// 当前支持的 Evolution 子命令。
#[derive(Debug, Subcommand)]
enum EvolutionCommand {
    /// 检查不可变 Genome、Stable 引用和 Parent/Candidate 差异。
    Genome(GenomeArgs),
    /// 比较 Parent 与 Candidate 的真实 EvaluationReport。
    Compare(CompareArgs),
    /// 显示最近一次已发布 Candidate 的 Scorecard。
    Dashboard(DashboardArgs),
    /// 查看或验证一次 Promotion 的不可变证明包。
    Certificate(CertificateArgs),
    /// 显示多代进化指标与趋势。
    History(HistoryArgs),
    /// 显示指定 Lineage 的 Candidate、发布、拒绝与回滚节点。
    Lineage(LineageArgs),
    /// 显示 Task Family × Generation 能力图。
    CapabilityMap(HistoryArgs),
    /// 显示 Evolution Engine 漏斗与 Candidate Yield。
    Funnel(HistoryArgs),
}

/// `lucia evolution genome` 参数。
#[derive(Debug, ClapArgs)]
struct GenomeArgs {
    /// Genome 运维查询。
    #[command(subcommand)]
    command: GenomeCommand,
}

/// Genome 的只读运维命令。
#[derive(Debug, Subcommand)]
enum GenomeCommand {
    /// 显示已验证 Revision 的行为配置与摘要。
    Inspect(GenomeTargetArgs),
    /// 重新计算摘要并验证 Revision 或 Stable 引用。
    Verify(GenomeTargetArgs),
    /// 生成 Parent/Candidate 的可信差异，并可校验允许表面。
    Diff(GenomeDiffArgs),
}

/// 选择精确 Revision 或 Stable lineage。
#[derive(Debug, ClapArgs)]
struct GenomeTargetArgs {
    /// 精确 Genome Revision ID。
    #[arg(long, conflicts_with = "stable", required_unless_present = "stable")]
    revision: Option<String>,
    /// Stable lineage，例如 `stable/general`。
    #[arg(
        long,
        conflicts_with = "revision",
        required_unless_present = "revision"
    )]
    stable: Option<String>,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
}

/// Parent/Candidate Genome 差异参数。
#[derive(Debug, ClapArgs)]
struct GenomeDiffArgs {
    /// Parent Genome Revision ID。
    #[arg(long)]
    parent: String,
    /// Candidate Genome Revision ID。
    #[arg(long)]
    candidate: String,
    /// 可信 Evolution Policy 允许的表面；指定任一项后同时执行越界校验。
    #[arg(long = "allow", value_enum, value_delimiter = ',')]
    allowed: Vec<GenomeSurfaceArg>,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
}

/// CLI 可选择的已知 Genome 变异表面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum GenomeSurfaceArg {
    /// Task Strategy Prompt。
    TaskStrategyPrompt,
    /// Context Policy。
    ContextPolicy,
    /// Planning Policy。
    PlanningPolicy,
    /// Skill。
    Skill,
    /// Plugin 或 Capability owner。
    Plugin,
    /// Model。
    Model,
    /// Tool Profile。
    ToolProfile,
    /// Execution Profile。
    ExecutionProfile,
    /// Runtime 或 Kernel。
    Runtime,
    /// 受保护 Prompt。
    ProtectedPrompt,
}

impl From<GenomeSurfaceArg> for MutationSurface {
    fn from(value: GenomeSurfaceArg) -> Self {
        match value {
            GenomeSurfaceArg::TaskStrategyPrompt => Self::TaskStrategyPrompt,
            GenomeSurfaceArg::ContextPolicy => Self::ContextPolicy,
            GenomeSurfaceArg::PlanningPolicy => Self::PlanningPolicy,
            GenomeSurfaceArg::Skill => Self::Skill,
            GenomeSurfaceArg::Plugin => Self::Plugin,
            GenomeSurfaceArg::Model => Self::Model,
            GenomeSurfaceArg::ToolProfile => Self::ToolProfile,
            GenomeSurfaceArg::ExecutionProfile => Self::ExecutionProfile,
            GenomeSurfaceArg::Runtime => Self::Runtime,
            GenomeSurfaceArg::ProtectedPrompt => Self::ProtectedPrompt,
        }
    }
}

/// `lucia evolution compare` 参数。
#[derive(Debug, ClapArgs)]
struct CompareArgs {
    /// Parent Genome 修订；与报告内容不一致时拒绝输出。
    #[arg(long)]
    parent: Option<String>,
    /// Candidate Genome 修订；与报告内容不一致时拒绝输出。
    #[arg(long)]
    candidate: Option<String>,
    /// 显式 EvaluationReport JSON；省略时按 Parent/Candidate 查询 Store 索引。
    #[arg(long)]
    report: Option<PathBuf>,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
    /// 版本化 EvolutionVerdictPolicy JSON；省略时使用内置 v1 策略。
    #[arg(long)]
    policy: Option<PathBuf>,
    /// 覆盖表格显示宽度，主要用于日志与 Golden Test。
    #[arg(long, hide = true)]
    width: Option<u16>,
}

/// `lucia evolution dashboard` 参数。
#[derive(Debug, ClapArgs)]
struct DashboardArgs {
    /// 只选择显式绑定该 Lineage 的最近发布。
    #[arg(long)]
    lineage: Option<String>,
    /// 在当前 `lucia` 二进制内启动四页 Ratatui Dashboard。
    #[arg(long)]
    tui: bool,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
    /// 版本化 EvolutionVerdictPolicy JSON。
    #[arg(long)]
    policy: Option<PathBuf>,
    /// 覆盖表格显示宽度。
    #[arg(long, hide = true)]
    width: Option<u16>,
}

/// 历史、能力图与漏斗的公共参数。
#[derive(Debug, ClapArgs)]
struct HistoryArgs {
    /// 只分析显式绑定该 Lineage 的报告。
    #[arg(long)]
    lineage: Option<String>,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
    /// 版本化 EvolutionVerdictPolicy JSON。
    #[arg(long)]
    policy: Option<PathBuf>,
}

/// `lucia evolution lineage [lineage]` 参数。
#[derive(Debug, ClapArgs)]
struct LineageArgs {
    /// Lineage 稳定名称；省略时显示全部显式 Lineage。
    lineage: Option<String>,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
    /// 版本化 EvolutionVerdictPolicy JSON。
    #[arg(long)]
    policy: Option<PathBuf>,
}

/// `lucia evolution certificate` 参数。
#[derive(Debug, ClapArgs)]
struct CertificateArgs {
    /// Promotion 的 Release ID。
    release: String,
    /// 读取 CAS 并验证 Certificate 自身与全部引用制品。
    #[arg(long)]
    verify: bool,
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = ScorecardFormat::Table)]
    format: ScorecardFormat,
}

/// Scorecard 输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScorecardFormat {
    /// 面向终端的无 ANSI 文本表格。
    Table,
    /// 带 `schema_version` 的稳定 JSON。
    Json,
    /// 面向审计报告的 Markdown。
    Markdown,
}

/// 执行 Evolution CLI；该路径不启动 TUI 或模型服务。
pub(crate) async fn run(args: EvolutionArgs) -> Result<()> {
    let root = args.root.unwrap_or(lucia_home_dir()?.join("evolution"));
    match args.command {
        EvolutionCommand::Genome(options) => run_genome(&root, options).await,
        EvolutionCommand::Compare(options) => run_compare(&root, options).await,
        EvolutionCommand::Dashboard(options) => run_dashboard(&root, options).await,
        EvolutionCommand::Certificate(options) => run_certificate(&root, options).await,
        EvolutionCommand::History(options) => {
            run_history(&root, options, HistoryView::Summary).await
        }
        EvolutionCommand::Lineage(options) => run_lineage(&root, options).await,
        EvolutionCommand::CapabilityMap(options) => {
            run_history(&root, options, HistoryView::CapabilityMap).await
        }
        EvolutionCommand::Funnel(options) => run_history(&root, options, HistoryView::Funnel).await,
    }
}

/// 执行只读 Genome 运维命令；该路径不会写 Stable 引用或修改 Revision。
async fn run_genome(root: &std::path::Path, options: GenomeArgs) -> Result<()> {
    let resolver = FileGenomeResolver::new(root);
    match options.command {
        GenomeCommand::Inspect(target) => {
            let revision = resolve_genome_target(&resolver, &target).await?;
            println!("{}", render_genome(&revision, target.format, false)?);
        }
        GenomeCommand::Verify(target) => {
            let revision = resolve_genome_target(&resolver, &target).await?;
            revision.validate().context("Genome Revision 验证失败")?;
            println!("{}", render_genome(&revision, target.format, true)?);
        }
        GenomeCommand::Diff(options) => {
            let parent_id = GenomeRevisionId::new(options.parent)
                .context("--parent 不是合法 GenomeRevisionId")?;
            let candidate_id = GenomeRevisionId::new(options.candidate)
                .context("--candidate 不是合法 GenomeRevisionId")?;
            let parent = resolver
                .resolve(&GenomeSelector::Revision(parent_id))
                .await
                .context("解析 Parent Genome 失败")?;
            let candidate = resolver
                .resolve(&GenomeSelector::Revision(candidate_id))
                .await
                .context("解析 Candidate Genome 失败")?;
            let diff = if options.allowed.is_empty() {
                diff_genomes(&parent, &candidate).context("生成可信 Genome Diff 失败")?
            } else {
                let allowed = options
                    .allowed
                    .into_iter()
                    .map(MutationSurface::from)
                    .collect::<BTreeSet<_>>();
                verify_allowed_genome_diff(&parent, &candidate, &allowed)
                    .context("Candidate Genome 超出允许变异表面")?
            };
            println!(
                "{}",
                render_genome_diff(&parent, &candidate, &diff, options.format)?
            );
        }
    }
    Ok(())
}

/// 解析 CLI 指定的精确 Revision 或 Stable lineage。
async fn resolve_genome_target(
    resolver: &FileGenomeResolver,
    target: &GenomeTargetArgs,
) -> Result<GenomeRevision> {
    let selector = match (target.revision.as_deref(), target.stable.as_deref()) {
        (Some(revision), None) => GenomeSelector::Revision(
            GenomeRevisionId::new(revision).context("--revision 不是合法 GenomeRevisionId")?,
        ),
        (None, Some(lineage)) => GenomeSelector::Stable(lineage.to_string()),
        _ => return Err(anyhow!("必须且只能指定 --revision 或 --stable")),
    };
    resolver
        .resolve(&selector)
        .await
        .with_context(|| format!("解析 Genome 失败：{selector:?}"))
}

/// 生成稳定的 Genome inspect/verify 文本或 JSON。
fn render_genome(
    revision: &GenomeRevision,
    format: ScorecardFormat,
    verified: bool,
) -> Result<String> {
    match format {
        ScorecardFormat::Json => serde_json::to_string_pretty(revision)
            .context("序列化 Genome Revision JSON 失败"),
        ScorecardFormat::Table => Ok(format!(
            "Lucia Agent Genome\nRevision: {}\nDigest: {}\nProfile: {:?}\nModel: {}/{}\nPlugins: {}\nNative tools: {}\nVerification: {}",
            revision.revision_id,
            revision.digest,
            revision.genome.execution.profile(),
            revision.genome.model.provider,
            revision.genome.model.model,
            revision.genome.plugins.len(),
            revision.genome.tools.native_tools.len(),
            if verified { "PASS" } else { "VALIDATED_ON_READ" },
        )),
        ScorecardFormat::Markdown => Ok(format!(
            "# Lucia Agent Genome\n\n- Revision: `{}`\n- Digest: `{}`\n- Profile: `{:?}`\n- Model: `{}/{}`\n- Plugins: `{}`\n- Native tools: `{}`\n- Verification: **{}**\n",
            revision.revision_id,
            revision.digest,
            revision.genome.execution.profile(),
            revision.genome.model.provider,
            revision.genome.model.model,
            revision.genome.plugins.len(),
            revision.genome.tools.native_tools.len(),
            if verified { "PASS" } else { "VALIDATED_ON_READ" },
        )),
    }
}

/// 生成不包含 Prompt、Skill 或插件配置正文的可信 Genome Diff。
fn render_genome_diff(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
    diff: &GenomeDiff,
    format: ScorecardFormat,
) -> Result<String> {
    match format {
        ScorecardFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "parent_revision": parent.revision_id,
            "candidate_revision": candidate.revision_id,
            "diff": diff,
        }))
        .context("序列化 Genome Diff JSON 失败"),
        ScorecardFormat::Table => {
            let changes = if diff.summary.is_empty() {
                "无行为变化".to_string()
            } else {
                diff.summary.join("\n- ")
            };
            Ok(format!(
                "Lucia Genome Diff\nParent: {}\nCandidate: {}\nChanged surfaces: {}\n- {}",
                parent.revision_id,
                candidate.revision_id,
                diff.changed_surfaces.len(),
                changes,
            ))
        }
        ScorecardFormat::Markdown => {
            let changes = if diff.summary.is_empty() {
                "- 无行为变化".to_string()
            } else {
                diff.summary
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(format!(
                "# Lucia Genome Diff\n\n- Parent: `{}`\n- Candidate: `{}`\n- Changed surfaces: `{}`\n\n{}\n",
                parent.revision_id,
                candidate.revision_id,
                diff.changed_surfaces.len(),
                changes,
            ))
        }
    }
}

/// 读取 Certificate，并按需验证所有 CAS 引用。
async fn run_certificate(root: &std::path::Path, options: CertificateArgs) -> Result<()> {
    let release = ReleaseId::new(options.release).context("release 不是合法 ReleaseId")?;
    let certificate = FileEvolutionArchive::new(root)
        .certificate(&release)
        .await
        .context("读取 EvolutionCertificate 失败")?
        .ok_or_else(|| anyhow!("没有找到 Release 对应的 EvolutionCertificate"))?;
    if options.verify {
        certificate
            .verify(&FileArtifactStore::new(root.join("artifacts")))
            .await
            .context("EvolutionCertificate 验证失败")?;
    }
    println!(
        "{}",
        render_certificate(&certificate, options.format, options.verify)?
    );
    Ok(())
}

/// 加载指定报告或 Store 索引并输出 Scorecard。
async fn run_compare(root: &std::path::Path, options: CompareArgs) -> Result<()> {
    let report = if let Some(path) = &options.report {
        load_evaluation_report(path)
            .await
            .with_context(|| format!("读取 EvaluationReport 失败：{}", path.display()))?
    } else {
        let parent = parse_revision(options.parent.as_deref(), "--parent")?;
        let candidate = parse_revision(options.candidate.as_deref(), "--candidate")?;
        FileEvaluationReportStore::new(root)
            .find_comparison(&parent, &candidate)
            .await
            .context("查询 Parent/Candidate EvaluationReport 失败")?
            .ok_or_else(|| anyhow!("没有找到指定 Parent/Candidate 的 EvaluationReport"))?
    };
    validate_requested_revisions(
        &report,
        options.parent.as_deref(),
        options.candidate.as_deref(),
    )?;
    let policy = load_policy(options.policy.as_ref()).await?;
    let scorecard = compute_scorecard(&report, &policy).context("计算 Evolution Scorecard 失败")?;
    println!(
        "{}",
        render_scorecard(&scorecard, options.format, resolve_width(options.width))?
    );
    Ok(())
}

/// 从 Store 中选择最近一次有 Release 的正式报告。
async fn run_dashboard(root: &std::path::Path, options: DashboardArgs) -> Result<()> {
    let reports = match FileEvaluationReportStore::new(root).list().await {
        Ok(reports) => reports,
        Err(error) if options.tui => {
            return crate::evolution_dashboard::run(
                crate::evolution_dashboard::EvolutionDashboardState::failed(format!(
                    "读取 Evolution 历史失败：{error}"
                )),
            );
        }
        Err(error) => return Err(error).context("读取 Evolution 历史失败"),
    };
    let policy = load_policy(options.policy.as_ref()).await?;
    let report = reports.iter().rev().find(|report| {
        report.release_record.is_some()
            && options
                .lineage
                .as_deref()
                .is_none_or(|lineage| report.lineage.as_deref() == Some(lineage))
    });
    let scorecard = report
        .map(|report| compute_scorecard(report, &policy))
        .transpose()
        .context("计算 Evolution Scorecard 失败")?;
    if options.tui {
        let certificates = match FileEvolutionArchive::new(root).list_certificates().await {
            Ok(certificates) => certificates,
            Err(error) => {
                return crate::evolution_dashboard::run(
                    crate::evolution_dashboard::EvolutionDashboardState::failed(format!(
                        "读取 EvolutionCertificate 历史失败：{error}"
                    )),
                );
            }
        };
        let history =
            match compute_history(&reports, &certificates, &policy, options.lineage.as_deref()) {
                Ok(history) => history,
                Err(error) => {
                    return crate::evolution_dashboard::run(
                        crate::evolution_dashboard::EvolutionDashboardState::failed(format!(
                            "计算 Evolution 历史失败：{error}"
                        )),
                    );
                }
            };
        let certificate = scorecard.as_ref().and_then(|scorecard| {
            let release = scorecard.release_record.as_ref()?;
            certificates
                .iter()
                .find(|certificate| {
                    &certificate.release_record == release
                        && certificate.evaluation_report == scorecard.evaluation_report
                })
                .cloned()
        });
        return crate::evolution_dashboard::run(
            crate::evolution_dashboard::EvolutionDashboardState::loaded(
                scorecard,
                certificate,
                history,
            ),
        );
    }
    let scorecard = scorecard
        .ok_or_else(|| anyhow!("暂无已发布 Evolution 数据；请先生成可信 EvaluationReport"))?;
    println!(
        "{}",
        render_scorecard(&scorecard, options.format, resolve_width(options.width))?
    );
    Ok(())
}

/// 历史命令选择的表格视图。
#[derive(Debug, Clone, Copy)]
enum HistoryView {
    /// 综合历史摘要。
    Summary,
    /// Task Family × Generation 能力图。
    CapabilityMap,
    /// Evolution Engine 漏斗。
    Funnel,
}

/// 加载真实报告与 Certificate 并输出历史视图。
async fn run_history(
    root: &std::path::Path,
    options: HistoryArgs,
    view: HistoryView,
) -> Result<()> {
    let policy = load_policy(options.policy.as_ref()).await?;
    let history = load_history(root, &policy, options.lineage.as_deref()).await?;
    println!(
        "{}",
        render_history(&history, options.format, view).context("渲染 Evolution 历史失败")?
    );
    Ok(())
}

/// 适配 positional Lineage 参数到公共历史加载路径。
async fn run_lineage(root: &std::path::Path, options: LineageArgs) -> Result<()> {
    let policy = load_policy(options.policy.as_ref()).await?;
    let history = load_history(root, &policy, options.lineage.as_deref()).await?;
    println!(
        "{}",
        render_lineage(&history, options.format).context("渲染 Evolution Lineage 失败")?
    );
    Ok(())
}

/// 从不可变报告与 Certificate 归档计算当前 Policy 下的历史结果。
async fn load_history(
    root: &std::path::Path,
    policy: &EvolutionVerdictPolicy,
    lineage: Option<&str>,
) -> Result<EvolutionHistory> {
    let reports = FileEvaluationReportStore::new(root)
        .list()
        .await
        .context("读取 EvaluationReport 历史失败")?;
    let certificates = FileEvolutionArchive::new(root)
        .list_certificates()
        .await
        .context("读取 EvolutionCertificate 历史失败")?;
    compute_history(&reports, &certificates, policy, lineage).context("计算 Evolution 历史指标失败")
}

/// 解析必需的 Genome 修订参数。
fn parse_revision(value: Option<&str>, flag: &'static str) -> Result<GenomeRevisionId> {
    let value = value.ok_or_else(|| anyhow!("省略 --report 时必须提供 {flag}"))?;
    GenomeRevisionId::new(value).with_context(|| format!("{flag} 不是合法 GenomeRevisionId"))
}

/// 拒绝命令行修订与报告绑定不一致，避免查错报告后仍展示数字。
fn validate_requested_revisions(
    report: &EvaluationReport,
    parent: Option<&str>,
    candidate: Option<&str>,
) -> Result<()> {
    if let Some(parent) = parent {
        let parent = GenomeRevisionId::new(parent).context("--parent 不是合法 GenomeRevisionId")?;
        if parent != report.parent.genome_revision {
            return Err(anyhow!("--parent 与 EvaluationReport 绑定的 Parent 不一致"));
        }
    }
    if let Some(candidate) = candidate {
        let candidate =
            GenomeRevisionId::new(candidate).context("--candidate 不是合法 GenomeRevisionId")?;
        if candidate != report.candidate.genome_revision {
            return Err(anyhow!(
                "--candidate 与 EvaluationReport 绑定的 Candidate 不一致"
            ));
        }
    }
    Ok(())
}

/// 加载显式 JSON 策略；没有文件时使用内置版本化默认值。
async fn load_policy(path: Option<&PathBuf>) -> Result<EvolutionVerdictPolicy> {
    let Some(path) = path else {
        return Ok(EvolutionVerdictPolicy::default());
    };
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("读取 Verdict Policy 失败：{}", path.display()))?;
    let policy: EvolutionVerdictPolicy = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析 Verdict Policy 失败：{}", path.display()))?;
    policy.validate().context("Verdict Policy 未通过校验")?;
    Ok(policy)
}

/// 非交互终端使用保守宽度，交互终端读取当前列数；任何路径均不输出 ANSI。
fn resolve_width(explicit: Option<u16>) -> u16 {
    explicit.unwrap_or_else(|| {
        if std::io::stdout().is_terminal() {
            crossterm::terminal::size()
                .map(|(width, _)| width)
                .unwrap_or(100)
        } else {
            100
        }
    })
}

/// 生成指定格式的稳定 Scorecard 文本。
fn render_scorecard(
    scorecard: &EvolutionScorecard,
    format: ScorecardFormat,
    width: u16,
) -> Result<String> {
    match format {
        ScorecardFormat::Table => Ok(render_table(scorecard, width)),
        ScorecardFormat::Json => {
            serde_json::to_string_pretty(scorecard).context("序列化 Scorecard JSON 失败")
        }
        ScorecardFormat::Markdown => Ok(render_markdown(scorecard)),
    }
}

/// 生成 Certificate 的稳定文本或 JSON。
fn render_certificate(
    certificate: &EvolutionCertificate,
    format: ScorecardFormat,
    verified: bool,
) -> Result<String> {
    match format {
        ScorecardFormat::Json => {
            serde_json::to_string_pretty(certificate).context("序列化 Certificate JSON 失败")
        }
        ScorecardFormat::Table => Ok(format!(
            "Lucia Evolution Certificate\nRelease: {}\nParent: {}\nChild: {}\nGate: {:?}\nLifecycle: {:?}\nEvaluationReport: {}\nSource Episodes: {}\nRepaired Cases: {}\nPost-promotion Runs: {}\nVerification: {}\nDigest: {}",
            certificate.release_record,
            certificate.parent_revision,
            certificate.child_revision,
            certificate.gate_decision,
            certificate.lifecycle,
            certificate.evaluation_report,
            certificate.source_episode_ids.len(),
            certificate.repaired_task_case_ids.len(),
            certificate.post_promotion_run_ids.len(),
            if verified { "PASS" } else { "NOT_REQUESTED" },
            certificate.certificate_digest,
        )),
        ScorecardFormat::Markdown => Ok(format!(
            "# Lucia Evolution Certificate\n\n- Release: `{}`\n- Parent: `{}`\n- Child: `{}`\n- Gate: `{:?}`\n- Lifecycle: `{:?}`\n- EvaluationReport: `{}`\n- Verification: **{}**\n- Digest: `{}`\n",
            certificate.release_record,
            certificate.parent_revision,
            certificate.child_revision,
            certificate.gate_decision,
            certificate.lifecycle,
            certificate.evaluation_report,
            if verified { "PASS" } else { "NOT_REQUESTED" },
            certificate.certificate_digest,
        )),
    }
}

/// 渲染综合历史、能力图或漏斗；JSON 始终携带 Schema 版本。
fn render_history(
    history: &EvolutionHistory,
    format: ScorecardFormat,
    view: HistoryView,
) -> Result<String> {
    match format {
        ScorecardFormat::Json => match view {
            HistoryView::Summary => {
                serde_json::to_string_pretty(history).context("序列化 EvolutionHistory 失败")
            }
            HistoryView::CapabilityMap => serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": history.schema_version,
                "lineage": history.lineage,
                "capability_map": history.capability_map,
            }))
            .context("序列化 CapabilityMap 失败"),
            HistoryView::Funnel => serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": history.schema_version,
                "lineage": history.lineage,
                "funnel": history.funnel,
                "candidate_yield": history.candidate_yield,
                "rollback_rate": history.rollback_rate,
            }))
            .context("序列化 EvolutionFunnel 失败"),
        },
        ScorecardFormat::Table => Ok(match view {
            HistoryView::Summary => render_history_summary(history),
            HistoryView::CapabilityMap => render_capability_map(&history.capability_map),
            HistoryView::Funnel => render_funnel(&history.funnel, history),
        }),
        ScorecardFormat::Markdown => Ok(match view {
            HistoryView::Summary => render_history_markdown(history),
            HistoryView::CapabilityMap => {
                format!(
                    "# Lucia Capability Map\n\n```text\n{}\n```\n",
                    render_capability_map(&history.capability_map)
                )
            }
            HistoryView::Funnel => {
                format!(
                    "# Lucia Evolution Funnel\n\n```text\n{}\n```\n",
                    render_funnel(&history.funnel, history)
                )
            }
        }),
    }
}

/// 渲染 Lineage 节点；JSON 与 Markdown 复用稳定节点结构。
fn render_lineage(history: &EvolutionHistory, format: ScorecardFormat) -> Result<String> {
    match format {
        ScorecardFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": history.schema_version,
            "lineage": history.lineage,
            "nodes": history.lineage_nodes,
        }))
        .context("序列化 Lineage 失败"),
        ScorecardFormat::Table => Ok(render_lineage_nodes(&history.lineage_nodes)),
        ScorecardFormat::Markdown => Ok(format!(
            "# Lucia Evolution Lineage\n\n```text\n{}\n```\n",
            render_lineage_nodes(&history.lineage_nodes)
        )),
    }
}

/// 渲染历史摘要，保留 Dataset 版本分段与缺失值。
fn render_history_summary(history: &EvolutionHistory) -> String {
    let mut lines = vec![
        "Lucia Evolution History".into(),
        format!(
            "Lineage: {}  Evaluated: {}  Promotions: {}  Rollbacks: {}",
            history.lineage.as_deref().unwrap_or("ALL"),
            history.funnel.evaluated_candidates,
            history.funnel.promotions,
            history.funnel.rollbacks
        ),
        format!(
            "Candidate Yield: {}  Rollback Rate: {}",
            rate(history.candidate_yield),
            rate(history.rollback_rate)
        ),
    ];
    for survival in &history.fix_survival {
        lines.push(format!(
            "Fix Survival @{}: {}",
            survival.generations,
            rate(survival.rate)
        ));
    }
    lines.push("Hidden Dataset segments:".into());
    if history.hidden_trends.is_empty() {
        lines.push("  N/A".into());
    } else {
        for segment in &history.hidden_trends {
            lines.push(format!(
                "  {}  generations {}  cumulative {}",
                segment.dataset_version,
                segment.points.len(),
                pp(segment.cumulative_gain_pp)
            ));
        }
    }
    lines.push(String::new());
    lines.push(render_lineage_nodes(&history.lineage_nodes));
    lines.join("\n")
}

/// 渲染 Engine 漏斗；上游没有结构化记录的阶段显示 N/A。
fn render_funnel(funnel: &EvolutionFunnel, history: &EvolutionHistory) -> String {
    [
        "Lucia Evolution Funnel".into(),
        format!("Episodes              {}", optional_count(funnel.episodes)),
        format!("Incidents             {}", optional_count(funnel.incidents)),
        format!(
            "Confirmed Failures    {}",
            optional_count(funnel.confirmed_failures)
        ),
        format!(
            "Clustered Issues      {}",
            optional_count(funnel.clustered_issues)
        ),
        format!(
            "Eligible Issues       {}",
            optional_count(funnel.eligible_issues)
        ),
        format!(
            "Generated Candidates  {}",
            optional_count(funnel.generated_candidates)
        ),
        format!("Valid Candidates      {}", funnel.valid_candidates),
        format!("Evaluated Candidates  {}", funnel.evaluated_candidates),
        format!("Gate Passed           {}", funnel.gate_passed_candidates),
        format!("Promotions            {}", funnel.promotions),
        format!("Rollbacks             {}", funnel.rollbacks),
        format!("Candidate Yield       {}", rate(history.candidate_yield)),
        format!("Rollback Rate         {}", rate(history.rollback_rate)),
    ]
    .join("\n")
}

/// 渲染 Task Family × Generation 数值图，不以颜色作为唯一信息。
fn render_capability_map(rows: &[CapabilityMapRow]) -> String {
    if rows.is_empty() {
        return "Lucia Capability Map\n暂无可验证的 Task Family 数据".into();
    }
    let generations: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row.cells.iter().map(|cell| cell.generation))
        .collect();
    let mut lines = vec![format!(
        "Task Family                   {}",
        generations
            .iter()
            .map(|generation| format!("G{generation:>4}"))
            .collect::<Vec<_>>()
            .join(" ")
    )];
    for row in rows {
        let cells: BTreeMap<_, _> = row
            .cells
            .iter()
            .map(|cell| (cell.generation, cell))
            .collect();
        lines.push(format!(
            "{:<28} {}",
            truncate_chars(&row.task_family, 28),
            generations
                .iter()
                .map(|generation| {
                    cells
                        .get(generation)
                        .and_then(|cell| cell.score)
                        .map(|score| format!("{:>5.1}%", score * 100.0))
                        .unwrap_or_else(|| "  N/A".into())
                })
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    lines.join("\n")
}

/// 渲染包含拒绝、隔离、发布与回滚节点的 Lineage。
fn render_lineage_nodes(nodes: &[LineageNode]) -> String {
    if nodes.is_empty() {
        return "Lineage: 暂无可验证节点".into();
    }
    let mut lines = vec![
        "Generation  Revision            Parent              Behavior                 Gate       Lifecycle              Capability  Hidden".into(),
    ];
    for node in nodes {
        lines.push(format!(
            "{:>10}  {:<18}  {:<18}  {:<23}  {:<9}  {:<21}  {:>10}  {:>7}",
            node.generation
                .map(|generation| format!("G{generation}"))
                .unwrap_or_else(|| "N/A".into()),
            truncate_chars(node.revision.as_str(), 18),
            truncate_chars(node.parent.as_str(), 18),
            format!("{:?}", node.behavior_assessment),
            format!("{:?}", node.gate_decision),
            format!("{:?}", node.lifecycle),
            number(node.capability_score, ""),
            percent(node.hidden_score),
        ));
    }
    lines.join("\n")
}

/// 渲染简洁 Markdown 历史摘要。
fn render_history_markdown(history: &EvolutionHistory) -> String {
    format!(
        "# Lucia Evolution History\n\n- Lineage: `{}`\n- Evaluated Candidates: {}\n- Promotions: {}\n- Rollbacks: {}\n- Candidate Yield: {}\n- Rollback Rate: {}\n\n```text\n{}\n```\n",
        history.lineage.as_deref().unwrap_or("ALL"),
        history.funnel.evaluated_candidates,
        history.funnel.promotions,
        history.funnel.rollbacks,
        rate(history.candidate_yield),
        rate(history.rollback_rate),
        render_lineage_nodes(&history.lineage_nodes),
    )
}

/// 区分未知计数与真实零。
fn optional_count(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "N/A".into())
}

/// 按 Unicode 字符数截断表格文本。
fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.into()
    } else {
        value
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

/// 输出无 ANSI 的 Scorecard；窄终端降级为逐行键值形式。
fn render_table(scorecard: &EvolutionScorecard, width: u16) -> String {
    if width < 72 {
        return render_narrow_table(scorecard);
    }
    let mut lines = Vec::new();
    lines.push("Lucia Evolution Scorecard".into());
    lines.push(format!(
        "Verdict: {:<20} Comparable: {:<3} Gate: {:?}",
        scorecard.headline_verdict.label(),
        yes_no(scorecard.comparison_validity.valid),
        scorecard.gate.decision
    ));
    lines.push(format!(
        "Parent: {}  Candidate: {}  Lifecycle: {:?}",
        scorecard.parent_revision, scorecard.candidate_revision, scorecard.lifecycle
    ));
    lines.push(format!(
        "SAFETY  Critical: {}  Permission expansion: {}  Hidden access: {}  Integrity: {}",
        scorecard.safety.candidate.critical_failures,
        scorecard.safety.candidate.permission_expansions,
        scorecard.safety.candidate.hidden_dataset_access_attempts,
        safety_integrity_label(scorecard)
    ));
    if !scorecard.gate.hard_failures.is_empty() {
        lines.push(format!(
            "HARD FAILURES: {}",
            scorecard.gate.hard_failures.join(", ")
        ));
    }
    lines.push("-----------------------------------------------------------------------".into());
    lines.push(format!(
        "Capability  Parent {:>6}  Candidate {:>6}  Net {:>7}",
        number(scorecard.capability.parent_score, ""),
        number(scorecard.capability.candidate_score, ""),
        signed(scorecard.capability.net_gain, "")
    ));
    lines.push(format!(
        "Repair      {:>6} -> {:>6}  {:>8}",
        percent(scorecard.datasets.repair.parent_score),
        percent(scorecard.datasets.repair.candidate_score),
        pp(scorecard.datasets.repair.delta_pp.map(|delta| delta.0))
    ));
    lines.push(format!(
        "Hidden      {:>6} -> {:>6}  {:>8}",
        percent(scorecard.datasets.hidden.parent_score),
        percent(scorecard.datasets.hidden.candidate_score),
        pp(scorecard.datasets.hidden.delta_pp.map(|delta| delta.0))
    ));
    lines.push(format!(
        "Retention   {} / {}  {:>6}  Critical lost: {}",
        scorecard.datasets.regression.retention.retained_cases,
        scorecard.datasets.regression.retention.parent_passed_cases,
        rate(scorecard.datasets.regression.retention.retention),
        scorecard
            .datasets
            .regression
            .retention
            .critical_lost_cases
            .len()
    ));
    lines.push(format!(
        "Stability   {:>6} -> {:>6}  Flaky: {} -> {}",
        percent(scorecard.datasets.parent_stability.stability),
        percent(scorecard.datasets.candidate_stability.stability),
        scorecard.datasets.parent_stability.flaky_cases,
        scorecard.datasets.candidate_stability.flaky_cases
    ));
    lines.push("-----------------------------------------------------------------------".into());
    lines.push(resource_line("Token", &scorecard.resources.tokens));
    lines.push(resource_line("Cost", &scorecard.resources.cost));
    lines.push(resource_line("Latency ms", &scorecard.resources.latency_ms));
    lines.push(resource_line("Tool Calls", &scorecard.resources.tool_calls));
    lines.push(resource_line(
        "Model Calls",
        &scorecard.resources.model_calls,
    ));
    lines.push(resource_line(
        "ReAct Steps",
        &scorecard.resources.react_steps,
    ));
    lines.push(resource_line(
        "Child Agents",
        &scorecard.resources.child_agents,
    ));
    lines.push(format!(
        "Timeout     {} -> {}  Budget failure {} -> {}",
        rate(scorecard.resources.parent_timeout_rate),
        rate(scorecard.resources.candidate_timeout_rate),
        rate(scorecard.resources.parent_budget_failure_rate),
        rate(scorecard.resources.candidate_budget_failure_rate)
    ));
    lines.push("-----------------------------------------------------------------------".into());
    lines.extend(inheritance_lines(scorecard.inheritance.as_ref()));
    lines.push(format!(
        "Confidence: {}  Report: {}  Schema: {}",
        confidence_label(scorecard),
        scorecard.evaluation_report,
        scorecard.schema_version
    ));
    lines.join("\n")
}

/// 窄终端只保留首屏硬门槛与核心数字，不截断标识或依赖颜色。
fn render_narrow_table(scorecard: &EvolutionScorecard) -> String {
    let mut lines = vec![
        "Lucia Evolution Scorecard".into(),
        format!("Verdict: {}", scorecard.headline_verdict.label()),
        format!(
            "Comparable: {}",
            yes_no(scorecard.comparison_validity.valid)
        ),
        format!("Gate: {:?}", scorecard.gate.decision),
        format!("Lifecycle: {:?}", scorecard.lifecycle),
        format!(
            "Safety critical: {}",
            scorecard.safety.candidate.critical_failures
        ),
        format!(
            "Permission expansion: {}",
            scorecard.safety.candidate.permission_expansions
        ),
        format!(
            "Capability: {} -> {} ({})",
            number(scorecard.capability.parent_score, ""),
            number(scorecard.capability.candidate_score, ""),
            signed(scorecard.capability.net_gain, "")
        ),
        format!(
            "Hidden: {} -> {} ({})",
            percent(scorecard.datasets.hidden.parent_score),
            percent(scorecard.datasets.hidden.candidate_score),
            pp(scorecard.datasets.hidden.delta_pp.map(|delta| delta.0))
        ),
        format!(
            "Retention: {}",
            rate(scorecard.datasets.regression.retention.retention)
        ),
        format!(
            "Inheritance: {}",
            scorecard
                .inheritance
                .as_ref()
                .map(|metrics| rate(metrics.rate()))
                .unwrap_or_else(|| "N/A".into())
        ),
    ];
    if !scorecard.gate.hard_failures.is_empty() {
        lines.push(format!(
            "HARD FAILURES: {}",
            scorecard.gate.hard_failures.join(", ")
        ));
    }
    lines.join("\n")
}

/// 输出审计友好的 Markdown，不包含 Hidden TaskCase 内容。
fn render_markdown(scorecard: &EvolutionScorecard) -> String {
    let rows = [
        (
            "Capability",
            number(scorecard.capability.parent_score, ""),
            number(scorecard.capability.candidate_score, ""),
            signed(scorecard.capability.net_gain, ""),
        ),
        (
            "Repair",
            percent(scorecard.datasets.repair.parent_score),
            percent(scorecard.datasets.repair.candidate_score),
            pp(scorecard.datasets.repair.delta_pp.map(|delta| delta.0)),
        ),
        (
            "Hidden",
            percent(scorecard.datasets.hidden.parent_score),
            percent(scorecard.datasets.hidden.candidate_score),
            pp(scorecard.datasets.hidden.delta_pp.map(|delta| delta.0)),
        ),
        (
            "Retention",
            "100%".into(),
            rate(scorecard.datasets.regression.retention.retention),
            format!(
                "lost {}",
                scorecard.datasets.regression.retention.lost_cases.len()
            ),
        ),
    ];
    let table = rows
        .into_iter()
        .map(|(metric, parent, candidate, delta)| {
            format!("| {metric} | {parent} | {candidate} | {delta} |")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Lucia Evolution Scorecard\n\n- Verdict: **{}**\n- Comparable: {}\n- Gate: `{:?}`\n- Lifecycle: `{:?}`\n- Critical safety failures: {}\n- Permission expansions: {}\n\n| Metric | Parent | Candidate | Delta |\n|---|---:|---:|---:|\n{}\n",
        scorecard.headline_verdict.label(),
        yes_no(scorecard.comparison_validity.valid),
        scorecard.gate.decision,
        scorecard.lifecycle,
        scorecard.safety.candidate.critical_failures,
        scorecard.safety.candidate.permission_expansions,
        table
    )
}

/// 输出一个资源行；缺失值和零值保持可区分。
fn resource_line(label: &str, value: &ResourceDelta) -> String {
    let relative = value
        .relative
        .map(|delta| format!("{:+.1}%", delta.0))
        .unwrap_or_else(|| "N/A".into());
    format!(
        "{label:<12} {:>8} -> {:>8}  {relative:>8}",
        number(value.parent, ""),
        number(value.candidate, "")
    )
}

/// 输出继承的重启、新 Session、旧 Session、Stable Ref 与摘要结果。
fn inheritance_lines(inheritance: Option<&InheritanceMetrics>) -> Vec<String> {
    let Some(inheritance) = inheritance else {
        return vec!["Inheritance: N/A".into()];
    };
    vec![
        format!(
            "Inheritance  Restart {}  New session {}  Combined {}",
            rate(inheritance.restart),
            rate(inheritance.new_session),
            rate(inheritance.rate())
        ),
        format!(
            "             Old session parent: {}  Stable ref: {}  Digest: {}",
            optional_bool(inheritance.old_session_parent_preserved),
            yes_no(inheritance.stable_reference_verified),
            yes_no(inheritance.genome_digest_verified)
        ),
    ]
}

/// 返回置信度类型与关键区间摘要。
fn confidence_label(scorecard: &EvolutionScorecard) -> String {
    match &scorecard.confidence {
        agent_evolution::EvaluationConfidence::Deterministic => "Deterministic".into(),
        agent_evolution::EvaluationConfidence::PairedBootstrap { hidden_gain, .. } => format!(
            "Paired Bootstrap {:.0}% [hidden {:+.1}pp, {:+.1}pp]",
            hidden_gain.confidence_level * 100.0,
            hidden_gain.lower,
            hidden_gain.upper
        ),
        agent_evolution::EvaluationConfidence::Insufficient { reason, .. } => {
            format!("Insufficient ({reason})")
        }
    }
}

/// 汇总两类完整性硬门槛；未知不会显示为 PASS。
fn safety_integrity_label(scorecard: &EvolutionScorecard) -> &'static str {
    if scorecard.safety.candidate.artifact_integrity_failures != 0
        || scorecard.safety.candidate.audit_integrity_failures != 0
    {
        "FAIL"
    } else if scorecard.safety.candidate.missing_attempts != 0 {
        "UNKNOWN"
    } else {
        "PASS"
    }
}

/// 格式化可选 `[0,1]` 比率为百分比。
fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.1}%", value * 100.0))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化保留原始计数的 Rate。
fn rate(value: Rate) -> String {
    value
        .percent()
        .map(|percent| format!("{percent:.1}% ({}/{})", value.numerator, value.denominator))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化百分点变化。
fn pp(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:+.1}pp"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化可选普通数值。
fn number(value: Option<f64>, suffix: &str) -> String {
    value
        .map(|value| format!("{value:.1}{suffix}"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化可选有符号普通数值。
fn signed(value: Option<f64>, suffix: &str) -> String {
    value
        .map(|value| format!("{value:+.1}{suffix}"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化布尔值且不依赖颜色。
const fn yes_no(value: bool) -> &'static str {
    if value {
        "YES"
    } else {
        "NO"
    }
}

/// 区分未知布尔值与 false。
const fn optional_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "YES",
        Some(false) => "NO",
        None => "N/A",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution::{
        BehaviorAssessment, CapabilityScoreSummary, ComparisonValidity, DatasetComparison,
        DatasetMetricSummary, EvaluationConfidence, GateSummary, HeadlineVerdict,
        RegressionComparison, RegressionRetention, ResourceComparison, ResourceDelta,
        SafetyComparison, SafetyMetrics, StabilityMetrics,
    };
    use agent_evolution_protocol::{
        EvaluationReportId, EvolutionLifecycle, GateDecision, GenomeRevisionId,
    };

    /// 构造缺失指标的固定 Scorecard，验证展示不会伪造零。
    fn missing_scorecard(verdict: HeadlineVerdict) -> EvolutionScorecard {
        let empty_dataset = DatasetComparison {
            parent_score: None,
            candidate_score: None,
            delta_pp: None,
            parent_cases: 0,
            candidate_cases: 0,
            paired_cases: 0,
            parent_infrastructure_failures: 0,
            candidate_infrastructure_failures: 0,
        };
        let empty_resource = || ResourceDelta {
            parent: None,
            candidate: None,
            absolute: None,
            relative: None,
        };
        EvolutionScorecard {
            schema_version: 1,
            parent_revision: GenomeRevisionId::generate(),
            candidate_revision: GenomeRevisionId::generate(),
            lineage: Some("stable/general".into()),
            parent_generation: Some(1),
            candidate_generation: Some(2),
            comparison_validity: ComparisonValidity {
                valid: true,
                violations: Vec::new(),
            },
            behavior_assessment: BehaviorAssessment::Inconclusive,
            lifecycle: EvolutionLifecycle::Evaluated,
            headline_verdict: verdict,
            gate: GateSummary {
                decision: GateDecision::Unknown,
                hard_failures: Vec::new(),
                resource_gate_passed: None,
            },
            capability: CapabilityScoreSummary {
                parent_score: None,
                candidate_score: None,
                net_gain: None,
                policy_version: "capability-v1".into(),
            },
            datasets: DatasetMetricSummary {
                repair: empty_dataset.clone(),
                hidden: empty_dataset.clone(),
                regression: RegressionComparison {
                    dataset: empty_dataset,
                    retention: RegressionRetention {
                        parent_passed_cases: 0,
                        retained_cases: 0,
                        lost_cases: Vec::new(),
                        retention: Rate::new(0, 0),
                        critical_parent_passed_cases: 0,
                        critical_retained_cases: 0,
                        critical_lost_cases: Vec::new(),
                        critical_retention: Rate::new(0, 0),
                    },
                },
                parent_stability: StabilityMetrics {
                    stability: None,
                    repeated_cases: 0,
                    flaky_cases: 0,
                    success_rate_stddev: None,
                    repeat_count: 0,
                    deterministic: false,
                },
                candidate_stability: StabilityMetrics {
                    stability: None,
                    repeated_cases: 0,
                    flaky_cases: 0,
                    success_rate_stddev: None,
                    repeat_count: 0,
                    deterministic: false,
                },
            },
            resources: ResourceComparison {
                tokens: empty_resource(),
                cost: empty_resource(),
                latency_ms: empty_resource(),
                tool_calls: empty_resource(),
                model_calls: empty_resource(),
                react_steps: empty_resource(),
                child_agents: empty_resource(),
                parent_timeout_rate: Rate::new(0, 0),
                candidate_timeout_rate: Rate::new(0, 0),
                parent_budget_failure_rate: Rate::new(0, 0),
                candidate_budget_failure_rate: Rate::new(0, 0),
            },
            safety: SafetyComparison {
                parent: SafetyMetrics::default(),
                candidate: SafetyMetrics {
                    missing_attempts: 1,
                    ..SafetyMetrics::default()
                },
            },
            confidence: EvaluationConfidence::Insufficient {
                reason: "缺少数据".into(),
                effective_cases: 0,
                unpaired_cases: 0,
            },
            inheritance: None,
            evaluation_report: EvaluationReportId::generate(),
            release_record: None,
            metrics_policy_version: "capability-v1".into(),
            verdict_policy_version: "verdict-v1".into(),
            source_report_digest: format!("sha256:{}", "0".repeat(64)),
            generated_at_ms: 0,
        }
    }

    #[test]
    fn table_uses_na_and_contains_no_ansi() {
        let rendered = render_table(&missing_scorecard(HeadlineVerdict::Inconclusive), 100);
        assert!(rendered.contains("N/A"));
        assert!(rendered.contains("Integrity: UNKNOWN"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn narrow_table_keeps_verdict_and_safety_on_first_screen() {
        let rendered = render_table(&missing_scorecard(HeadlineVerdict::Unsafe), 50);
        let lines: Vec<_> = rendered.lines().collect();
        assert!(lines[1].contains("UNSAFE"));
        assert!(lines
            .iter()
            .take(8)
            .any(|line| line.contains("Safety critical")));
    }

    #[test]
    fn json_output_preserves_schema_version() {
        let rendered = render_scorecard(
            &missing_scorecard(HeadlineVerdict::Inconclusive),
            ScorecardFormat::Json,
            100,
        )
        .expect("JSON 应可渲染");
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("应是 JSON");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["headline_verdict"], "INCONCLUSIVE");
    }
}
