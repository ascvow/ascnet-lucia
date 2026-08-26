//! Evolution Scorecard 与历史分析的非交互 CLI。

use crate::app_config::lucia_home_dir;
use agent_evolution::{
    compute_scorecard, load_evaluation_report, EvolutionScorecard, EvolutionVerdictPolicy,
    FileEvaluationReportStore, HeadlineVerdict, InheritanceMetrics, Rate, ResourceDelta,
};
use agent_evolution_protocol::{EvaluationReport, GenomeRevisionId};
use anyhow::{anyhow, Context, Result};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use std::{io::IsTerminal, path::PathBuf};

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
    /// 比较 Parent 与 Candidate 的真实 EvaluationReport。
    Compare(CompareArgs),
    /// 显示最近一次已发布 Candidate 的 Scorecard。
    Dashboard(DashboardArgs),
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
        EvolutionCommand::Compare(options) => run_compare(&root, options).await,
        EvolutionCommand::Dashboard(options) => run_dashboard(&root, options).await,
    }
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
    let reports = FileEvaluationReportStore::new(root)
        .list()
        .await
        .context("读取 Evolution 历史失败")?;
    let report = reports
        .into_iter()
        .rev()
        .find(|report| report.release_record.is_some())
        .ok_or_else(|| anyhow!("暂无已发布 Evolution 数据；请先生成可信 EvaluationReport"))?;
    let policy = load_policy(options.policy.as_ref()).await?;
    let scorecard = compute_scorecard(&report, &policy).context("计算 Evolution Scorecard 失败")?;
    println!(
        "{}",
        render_scorecard(&scorecard, options.format, resolve_width(options.width))?
    );
    Ok(())
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
        DatasetMetricSummary, EvaluationConfidence, GateSummary, RegressionComparison,
        RegressionRetention, ResourceComparison, ResourceDelta, SafetyComparison, SafetyMetrics,
        StabilityMetrics,
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
