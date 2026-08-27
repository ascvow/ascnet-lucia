//! 多代 Evolution 历史指标、漏斗、能力图与 Lineage 视图。

use crate::{
    aggregate_case, compute_scorecard, EvolutionCertificate, EvolutionScorecard,
    EvolutionVerdictPolicy, Rate, RollbackCategory, RollbackRecord, ScorecardError,
};
use agent_evolution_protocol::{
    DatasetKind, DatasetVersionId, EvaluationReport, EvolutionLifecycle, GateDecision,
    GenomeRevisionId, MutationSurface, ReleaseId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 当前历史分析 JSON 结构版本。
pub const EVOLUTION_HISTORY_SCHEMA_VERSION: u32 = 1;

/// Evolution 漏斗；无法从 Evaluation 归档证明的早期阶段使用 `None`，不会填充零。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionFunnel {
    /// 进入可信证据平面的 Episode 数。
    pub episodes: Option<u64>,
    /// Supervisor 产生的 Incident 数。
    pub incidents: Option<u64>,
    /// 确认失败数。
    pub confirmed_failures: Option<u64>,
    /// 聚类 Issue 数。
    pub clustered_issues: Option<u64>,
    /// 满足进化资格的 Issue 数。
    pub eligible_issues: Option<u64>,
    /// 已生成 Candidate 数；EvaluationReport 无法证明未完成评测的 Candidate。
    pub generated_candidates: Option<u64>,
    /// Comparison Validity 通过的 Candidate 数。
    pub valid_candidates: u64,
    /// 有正式 EvaluationReport 的 Candidate 数。
    pub evaluated_candidates: u64,
    /// Commit Gate PASS 的 Candidate 数。
    pub gate_passed_candidates: u64,
    /// 有 ReleaseRecord 的 Candidate 数。
    pub promotions: u64,
    /// 生命周期为 RolledBack 的发布数。
    pub rollbacks: u64,
}

/// 一代之后历史修复的保持情况。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixSurvivalPoint {
    /// 相对 Promotion 的代数偏移。
    pub generations: u64,
    /// 只以在目标代可验证的 Repair Case 为分母。
    pub rate: Rate,
    /// 目标代仍达到原通过门槛的 `<release>:<task_case>` 键。
    pub retained_repairs: Vec<String>,
    /// 目标代已回归的 `<release>:<task_case>` 键。
    pub lost_repairs: Vec<String>,
}

/// 同一 Hidden Dataset 版本内的趋势点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HiddenTrendPoint {
    /// Candidate 代数。
    pub generation: u64,
    /// Candidate Hidden Score。
    pub score: f64,
    /// Candidate Genome 修订。
    pub revision: GenomeRevisionId,
}

/// Dataset 版本变化时分段的 Hidden 趋势。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HiddenTrendSegment {
    /// 该段绑定的 Hidden Dataset 版本。
    pub dataset_version: DatasetVersionId,
    /// 同一版本的代际趋势点。
    pub points: Vec<HiddenTrendPoint>,
    /// 当前 Stable 相对本段最早 Parent 基线的累计百分点增益。
    pub cumulative_gain_pp: Option<f64>,
}

/// 一次有效 Candidate 的 Evolution Velocity。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionVelocityPoint {
    /// Candidate Genome 修订。
    pub revision: GenomeRevisionId,
    /// Candidate 代数。
    pub generation: Option<u64>,
    /// Capability 绝对分数增益。
    pub net_capability_gain: Option<f64>,
    /// 每百万 Evaluation Token 的 Capability points。
    pub points_per_million_tokens: Option<f64>,
    /// 每 100 个有效 Candidate run 的 Capability points。
    pub points_per_hundred_candidates: Option<f64>,
    /// 每货币单位的 Capability points；成本缺失或为零时为 `None`。
    pub points_per_monetary_unit: Option<f64>,
}

/// 按正式 RollbackRecord 分类的回滚数量。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RollbackBreakdown {
    /// 安全、权限、泄漏或完整性回滚。
    pub safety: u64,
    /// 能力、稳定性或资源性能回滚。
    pub performance: u64,
    /// Registry、依赖、评测或运行基础设施回滚。
    pub infrastructure: u64,
    /// 授权人员基于外部条件执行的人工回滚。
    pub manual: u64,
}

/// Task Family × Generation 的单个能力格。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityMapCell {
    /// Candidate 代数。
    pub generation: u64,
    /// 该 Task Family 下等权 TaskCase 平均分。
    pub score: Option<f64>,
    /// 有分数的 Case 数。
    pub scored_cases: u64,
    /// 全部 Case 数。
    pub total_cases: u64,
    /// 本代涉及的数据集版本，用于显示版本边界。
    pub dataset_versions: BTreeSet<DatasetVersionId>,
}

/// 一个 Task Family 的代际能力行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityMapRow {
    /// 只来自 TaskCase metadata 的任务族名称。
    pub task_family: String,
    /// 按代数排列的能力格。
    pub cells: Vec<CapabilityMapCell>,
}

/// 一个 Lineage 节点的可审计摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageNode {
    /// Candidate 修订。
    pub revision: GenomeRevisionId,
    /// Parent 修订。
    pub parent: GenomeRevisionId,
    /// Candidate 代数；旧报告未知时为 `None`。
    pub generation: Option<u64>,
    /// 实际 Genome Diff 表面。
    pub mutation_surfaces: BTreeSet<MutationSurface>,
    /// 行为判定。
    pub behavior_assessment: crate::BehaviorAssessment,
    /// Gate 决策。
    pub gate_decision: GateDecision,
    /// 生命周期。
    pub lifecycle: EvolutionLifecycle,
    /// Candidate Capability Score。
    pub capability_score: Option<f64>,
    /// Candidate Hidden Score。
    pub hidden_score: Option<f64>,
    /// Candidate Repair Score。
    pub repair_score: Option<f64>,
    /// Candidate 对 Parent 已通过 Regression Case 的保持率。
    pub regression_retention: Rate,
    /// Candidate Repeat 稳定性。
    pub stability: Option<f64>,
    /// Candidate 每个有效 Attempt 的平均 Token。
    pub average_tokens: Option<f64>,
    /// Candidate 每个有效 Attempt 的平均延迟毫秒。
    pub average_latency_ms: Option<f64>,
    /// Candidate 按严重级别汇总的安全失败数。
    pub safety_failures: u64,
    /// Release ID。
    pub release: Option<ReleaseId>,
    /// 是否已回滚。
    pub rolled_back: bool,
    /// 回滚时绑定的正式记录；旧归档缺失时为 `None`。
    pub rollback_record: Option<RollbackRecord>,
}

/// 历史分析的完整稳定 JSON 输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionHistory {
    /// JSON 结构版本。
    pub schema_version: u32,
    /// 本次查询的 Lineage；未过滤时为 `None`。
    pub lineage: Option<String>,
    /// 漏斗统计。
    pub funnel: EvolutionFunnel,
    /// Gate PASS / Comparison Valid Evaluation 的 Candidate Yield。
    pub candidate_yield: Rate,
    /// Rollback / Promotion 的回滚率。
    pub rollback_rate: Rate,
    /// 正式 RollbackRecord 的原因分类。
    pub rollback_breakdown: RollbackBreakdown,
    /// 一、三、五代后的 Fix Survival。
    pub fix_survival: Vec<FixSurvivalPoint>,
    /// 按 Hidden Dataset 版本分段的累计趋势。
    pub hidden_trends: Vec<HiddenTrendSegment>,
    /// 各 Candidate 的 Evolution Velocity。
    pub velocity: Vec<EvolutionVelocityPoint>,
    /// Task Family × Generation 能力图。
    pub capability_map: Vec<CapabilityMapRow>,
    /// Lineage 节点。
    pub lineage_nodes: Vec<LineageNode>,
}

/// 历史分析错误。
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// 某份报告无法生成一致 Policy 下的 Scorecard。
    #[error("历史 Scorecard 计算失败：{0}")]
    Scorecard(#[from] ScorecardError),
}

/// 从真实 EvaluationReport 与 Certificate 计算完整历史分析。
///
/// `lineage` 只匹配报告的显式 Lineage 字段；旧报告的未知 Lineage 不会被猜入结果。
///
/// # Errors
///
/// 任一报告无效或无法按指定 Policy 生成 Scorecard 时返回错误，不会跳过坏记录。
pub fn compute_history(
    reports: &[EvaluationReport],
    certificates: &[EvolutionCertificate],
    policy: &EvolutionVerdictPolicy,
    lineage: Option<&str>,
) -> Result<EvolutionHistory, HistoryError> {
    let mut records = Vec::new();
    for report in reports
        .iter()
        .filter(|report| lineage.is_none_or(|expected| report.lineage.as_deref() == Some(expected)))
    {
        records.push(HistoryRecord {
            report,
            scorecard: compute_scorecard(report, policy)?,
        });
    }
    records.sort_by(|left, right| {
        left.report
            .candidate_generation
            .cmp(&right.report.candidate_generation)
            .then_with(|| {
                left.report
                    .generated_at_ms
                    .cmp(&right.report.generated_at_ms)
            })
            .then_with(|| left.report.report_id.cmp(&right.report.report_id))
    });
    let relevant_certificates: Vec<_> = certificates
        .iter()
        .filter(|certificate| {
            records
                .iter()
                .any(|record| record.report.report_id == certificate.evaluation_report)
        })
        .collect();
    let funnel = evolution_funnel(&records, &relevant_certificates);
    let candidate_yield = Rate::new(funnel.gate_passed_candidates, funnel.valid_candidates);
    let rollback_rate = Rate::new(funnel.rollbacks, funnel.promotions);
    Ok(EvolutionHistory {
        schema_version: EVOLUTION_HISTORY_SCHEMA_VERSION,
        lineage: lineage.map(str::to_owned),
        funnel,
        candidate_yield,
        rollback_rate,
        rollback_breakdown: rollback_breakdown(&relevant_certificates),
        fix_survival: fix_survival(&records, &relevant_certificates, policy),
        hidden_trends: hidden_trends(&records),
        velocity: evolution_velocity(&records),
        capability_map: capability_map(&records, policy),
        lineage_nodes: lineage_nodes(&records, &relevant_certificates),
    })
}

/// 绑定同一报告与派生 Scorecard的内部视图。
struct HistoryRecord<'a> {
    /// 源报告。
    report: &'a EvaluationReport,
    /// 当前查询 Policy 下的评分卡。
    scorecard: EvolutionScorecard,
}

/// 只从当前归档能证明的阶段计算漏斗。
fn evolution_funnel(
    records: &[HistoryRecord<'_>],
    certificates: &[&EvolutionCertificate],
) -> EvolutionFunnel {
    EvolutionFunnel {
        episodes: None,
        incidents: None,
        confirmed_failures: None,
        clustered_issues: None,
        eligible_issues: None,
        generated_candidates: None,
        valid_candidates: records
            .iter()
            .filter(|record| record.scorecard.comparison_validity.valid)
            .count() as u64,
        evaluated_candidates: records.len() as u64,
        gate_passed_candidates: records
            .iter()
            .filter(|record| {
                record.scorecard.comparison_validity.valid
                    && record.report.gate_decision == GateDecision::Pass
            })
            .count() as u64,
        promotions: records
            .iter()
            .filter(|record| record.report.release_record.is_some())
            .count() as u64,
        rollbacks: certificates
            .iter()
            .filter(|certificate| certificate.lifecycle == EvolutionLifecycle::RolledBack)
            .count() as u64,
    }
}

/// 按正式 RollbackRecord 分类；旧归档只有生命周期时不猜测原因。
fn rollback_breakdown(certificates: &[&EvolutionCertificate]) -> RollbackBreakdown {
    let mut breakdown = RollbackBreakdown::default();
    for category in certificates.iter().filter_map(|certificate| {
        certificate
            .rollback_record
            .as_ref()
            .map(|record| record.category)
    }) {
        match category {
            RollbackCategory::Safety => breakdown.safety += 1,
            RollbackCategory::Performance => breakdown.performance += 1,
            RollbackCategory::Infrastructure => breakdown.infrastructure += 1,
            RollbackCategory::Manual => breakdown.manual += 1,
        }
    }
    breakdown
}

/// 计算 Promotion 后 1、3、5 代的历史修复保持率。
fn fix_survival(
    records: &[HistoryRecord<'_>],
    certificates: &[&EvolutionCertificate],
    policy: &EvolutionVerdictPolicy,
) -> Vec<FixSurvivalPoint> {
    [1_u64, 3, 5]
        .into_iter()
        .map(|generations| {
            let mut retained = Vec::new();
            let mut lost = Vec::new();
            for certificate in certificates {
                let Some(origin) = records
                    .iter()
                    .find(|record| record.report.report_id == certificate.evaluation_report)
                else {
                    continue;
                };
                let Some(origin_generation) = origin.report.candidate_generation else {
                    continue;
                };
                let target_generation = origin_generation + generations;
                let Some(target) = records
                    .iter()
                    .filter(|record| {
                        record.report.lineage == origin.report.lineage
                            && record.report.candidate_generation == Some(target_generation)
                            && record.report.release_record.is_some()
                            && matches!(
                                record.report.lifecycle,
                                EvolutionLifecycle::Promoted
                                    | EvolutionLifecycle::InheritanceVerified
                            )
                            && is_descendant_of(
                                record,
                                &origin.report.candidate.genome_revision,
                                records,
                            )
                    })
                    .max_by_key(|record| record.report.generated_at_ms)
                else {
                    continue;
                };
                for task_case_id in &certificate.repaired_task_case_ids {
                    let Some(case) = target.report.candidate.task_cases.iter().find(|case| {
                        case.metadata.dataset_kind == DatasetKind::Regression
                            && &case.metadata.task_case_id == task_case_id
                    }) else {
                        continue;
                    };
                    let metric = aggregate_case(case, policy.min_valid_repeats_per_case);
                    let Some(score) = metric.score else {
                        continue;
                    };
                    let key = format!("{}:{task_case_id}", certificate.release_record);
                    if score >= metric.pass_threshold {
                        retained.push(key);
                    } else {
                        lost.push(key);
                    }
                }
            }
            retained.sort();
            lost.sort();
            FixSurvivalPoint {
                generations,
                rate: Rate::new(retained.len() as u64, (retained.len() + lost.len()) as u64),
                retained_repairs: retained,
                lost_repairs: lost,
            }
        })
        .collect()
}

/// 验证目标记录能沿已归档 Parent 链回溯到指定祖先修订。
fn is_descendant_of(
    target: &HistoryRecord<'_>,
    ancestor: &GenomeRevisionId,
    records: &[HistoryRecord<'_>],
) -> bool {
    let mut cursor = target.report.parent.genome_revision.clone();
    let mut visited = BTreeSet::new();
    loop {
        if &cursor == ancestor {
            return true;
        }
        if !visited.insert(cursor.clone()) {
            return false;
        }
        let Some(parent) = records.iter().find(|record| {
            record.report.lineage == target.report.lineage
                && record.report.candidate.genome_revision == cursor
        }) else {
            return false;
        };
        cursor = parent.report.parent.genome_revision.clone();
    }
}

/// 一个 Hidden Dataset 版本的内部累计状态。
#[derive(Default)]
struct HiddenSegmentAccumulator {
    /// 可比较 Candidate 的趋势点，包括未晋升节点。
    points: Vec<HiddenTrendPoint>,
    /// 最早 Parent 的 `(generation, generated_at_ms, score)`。
    baseline: Option<(u64, u64, f64)>,
    /// 最新未回滚 Stable 的 `(generation, generated_at_ms, score)`。
    current_stable: Option<(u64, u64, f64)>,
}

/// 按 Hidden Dataset 版本分段，并以最早 Parent 到当前 Stable 计算累计增益。
fn hidden_trends(records: &[HistoryRecord<'_>]) -> Vec<HiddenTrendSegment> {
    let mut segments: BTreeMap<DatasetVersionId, HiddenSegmentAccumulator> = BTreeMap::new();
    for record in records
        .iter()
        .filter(|record| record.scorecard.comparison_validity.valid)
    {
        let (
            Some(generation),
            Some(parent_generation),
            Some(score),
            Some(parent_score),
            Some(version),
        ) = (
            record.report.candidate_generation,
            record.report.parent_generation,
            record.scorecard.datasets.hidden.candidate_score,
            record.scorecard.datasets.hidden.parent_score,
            record.report.candidate.datasets.get(&DatasetKind::Hidden),
        )
        else {
            continue;
        };
        let segment = segments.entry(version.clone()).or_default();
        segment.points.push(HiddenTrendPoint {
            generation,
            score,
            revision: record.report.candidate.genome_revision.clone(),
        });
        let baseline = (
            parent_generation,
            record.report.generated_at_ms,
            parent_score,
        );
        if segment
            .baseline
            .is_none_or(|current| (baseline.0, baseline.1) < (current.0, current.1))
        {
            segment.baseline = Some(baseline);
        }
        if record.report.release_record.is_some()
            && record.report.gate_decision == GateDecision::Pass
            && matches!(
                record.report.lifecycle,
                EvolutionLifecycle::Promoted | EvolutionLifecycle::InheritanceVerified
            )
        {
            let stable = (generation, record.report.generated_at_ms, score);
            if segment
                .current_stable
                .is_none_or(|current| (stable.0, stable.1) > (current.0, current.1))
            {
                segment.current_stable = Some(stable);
            }
        }
    }
    segments
        .into_iter()
        .map(|(dataset_version, mut segment)| {
            segment.points.sort_by(|left, right| {
                left.generation
                    .cmp(&right.generation)
                    .then_with(|| left.revision.cmp(&right.revision))
            });
            let cumulative_gain_pp = segment
                .baseline
                .zip(segment.current_stable)
                .map(|(baseline, current)| (current.2 - baseline.2) * 100.0);
            HiddenTrendSegment {
                dataset_version,
                points: segment.points,
                cumulative_gain_pp,
            }
        })
        .collect()
}

/// 计算每次 Candidate 的 Token、数量与货币三种 Evolution Velocity。
fn evolution_velocity(records: &[HistoryRecord<'_>]) -> Vec<EvolutionVelocityPoint> {
    records
        .iter()
        .filter(|record| record.scorecard.comparison_validity.valid)
        .map(|record| {
            let gain = record.scorecard.capability.net_gain;
            let attempts: Vec<_> = record
                .report
                .candidate
                .task_cases
                .iter()
                .flat_map(|case| &case.attempts)
                .filter(|attempt| {
                    !matches!(
                        attempt.status,
                        agent_evolution_protocol::TaskAttemptStatus::InfrastructureFailure
                            | agent_evolution_protocol::TaskAttemptStatus::Invalid
                    )
                })
                .collect();
            let tokens = attempts
                .iter()
                .filter_map(|attempt| attempt.usage.tokens)
                .sum::<u64>();
            let reported_costs: Vec<_> = attempts
                .iter()
                .filter_map(|attempt| attempt.usage.cost)
                .collect();
            let cost = (!reported_costs.is_empty()).then(|| reported_costs.iter().sum::<f64>());
            EvolutionVelocityPoint {
                revision: record.report.candidate.genome_revision.clone(),
                generation: record.report.candidate_generation,
                net_capability_gain: gain,
                points_per_million_tokens: gain
                    .zip((tokens != 0).then_some(tokens))
                    .map(|(gain, tokens)| gain / tokens as f64 * 1_000_000.0),
                points_per_hundred_candidates: gain.map(|gain| gain * 100.0),
                points_per_monetary_unit: gain
                    .zip(cost)
                    .and_then(|(gain, cost)| (cost != 0.0).then_some(gain / cost)),
            }
        })
        .collect()
}

/// 按可信 TaskCase metadata 构建 Task Family × Generation 能力图。
fn capability_map(
    records: &[HistoryRecord<'_>],
    policy: &EvolutionVerdictPolicy,
) -> Vec<CapabilityMapRow> {
    let mut rows: BTreeMap<String, Vec<CapabilityMapCell>> = BTreeMap::new();
    for record in records
        .iter()
        .filter(|record| record.scorecard.comparison_validity.valid)
    {
        let Some(generation) = record.report.candidate_generation else {
            continue;
        };
        let mut families: BTreeMap<String, Vec<_>> = BTreeMap::new();
        for case in &record.report.candidate.task_cases {
            families
                .entry(case.metadata.task_family.clone())
                .or_default()
                .push((
                    case.metadata.dataset_kind,
                    aggregate_case(case, policy.min_valid_repeats_per_case),
                ));
        }
        for (family, cases) in families {
            let scored: Vec<_> = cases.iter().filter_map(|(_, case)| case.score).collect();
            let dataset_versions: BTreeSet<DatasetVersionId> = cases
                .iter()
                .filter_map(|(kind, _)| record.report.candidate.datasets.get(kind))
                .cloned()
                .collect();
            rows.entry(family).or_default().push(CapabilityMapCell {
                generation,
                score: (scored.len() == cases.len() && !cases.is_empty())
                    .then(|| scored.iter().sum::<f64>() / scored.len() as f64),
                scored_cases: scored.len() as u64,
                total_cases: cases.len() as u64,
                dataset_versions: dataset_versions.clone(),
            });
        }
    }
    rows.into_iter()
        .map(|(task_family, mut cells)| {
            cells.sort_by_key(|cell| cell.generation);
            CapabilityMapRow { task_family, cells }
        })
        .collect()
}

/// 构建包含拒绝、隔离和回滚 Candidate 的 Lineage 节点。
fn lineage_nodes(
    records: &[HistoryRecord<'_>],
    certificates: &[&EvolutionCertificate],
) -> Vec<LineageNode> {
    records
        .iter()
        .map(|record| {
            let certificate = certificates.iter().find(|certificate| {
                certificate.evaluation_report == record.report.report_id
                    && record.report.release_record.as_ref() == Some(&certificate.release_record)
            });
            let lifecycle = certificate
                .map(|certificate| certificate.lifecycle)
                .unwrap_or(record.report.lifecycle);
            let safety = &record.scorecard.safety.candidate;
            LineageNode {
                revision: record.report.candidate.genome_revision.clone(),
                parent: record.report.parent.genome_revision.clone(),
                generation: record.report.candidate_generation,
                mutation_surfaces: record.report.genome_diff.changed_surfaces.clone(),
                behavior_assessment: record.scorecard.behavior_assessment,
                gate_decision: record.report.gate_decision,
                lifecycle,
                capability_score: record.scorecard.capability.candidate_score,
                hidden_score: record.scorecard.datasets.hidden.candidate_score,
                repair_score: record.scorecard.datasets.repair.candidate_score,
                regression_retention: record.scorecard.datasets.regression.retention.retention,
                stability: record.scorecard.datasets.candidate_stability.stability,
                average_tokens: record.scorecard.resources.tokens.candidate,
                average_latency_ms: record.scorecard.resources.latency_ms.candidate,
                safety_failures: safety.critical_failures
                    + safety.high_failures
                    + safety.medium_failures,
                release: record.report.release_record.clone(),
                rolled_back: lifecycle == EvolutionLifecycle::RolledBack,
                rollback_record: certificate
                    .and_then(|certificate| certificate.rollback_record.as_ref().cloned()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        EvaluationEnvironment, EvaluationReportId, EvaluationRun, EvaluationRunId, EvaluationUsage,
        GenomeDiff, SafetyAttemptSummary, TaskAttemptResult, TaskAttemptStatus, TaskCaseMetadata,
        TaskCaseResult, EVALUATION_REPORT_SCHEMA_VERSION,
    };

    /// 构造绑定指定报告的最小历史 Certificate。
    fn certificate(report: &EvaluationReport) -> EvolutionCertificate {
        let empty_digest = || {
            agent_evolution_protocol::ArtifactDigest::from_sha256_hex("0".repeat(64))
                .expect("固定摘要应合法")
        };
        EvolutionCertificate {
            schema_version: crate::EVOLUTION_CERTIFICATE_SCHEMA_VERSION,
            parent_revision: report.parent.genome_revision.clone(),
            child_revision: report.candidate.genome_revision.clone(),
            source_episode_ids: vec![agent_evolution_protocol::EpisodeId::generate()],
            evolution_issue_id: agent_evolution_protocol::EvolutionIssueId::generate(),
            mutation_id: agent_evolution_protocol::MutationId::generate(),
            allowed_diff: GenomeDiff::default(),
            candidate_artifacts: Vec::new(),
            repair_dataset: report.candidate.datasets[&DatasetKind::Repair].clone(),
            regression_dataset: report.candidate.datasets[&DatasetKind::Regression].clone(),
            hidden_dataset: report.candidate.datasets[&DatasetKind::Hidden].clone(),
            safety_dataset: DatasetVersionId::generate(),
            repaired_task_case_ids: vec!["historical-repair".into()],
            evaluation_report: report.report_id.clone(),
            scorecard: agent_evolution_protocol::ArtifactRef {
                digest: empty_digest(),
                media_type: "application/json".into(),
                size_bytes: 0,
            },
            gate_decision: GateDecision::Pass,
            release_record: report.release_record.clone().expect("测试报告应已发布"),
            inheritance_verification: Some(agent_evolution_protocol::ArtifactRef {
                digest: empty_digest(),
                media_type: "application/json".into(),
                size_bytes: 0,
            }),
            post_promotion_run_ids: vec![agent_evolution_protocol::RunId::generate()],
            lifecycle: EvolutionLifecycle::InheritanceVerified,
            revision: 0,
            previous_certificate_digest: None,
            rollback_record: None,
            certificate_digest: empty_digest(),
        }
    }

    /// 构造同一 Lineage 的一代报告。
    fn report(
        parent: GenomeRevisionId,
        generation: u64,
        hidden_version: DatasetVersionId,
        repaired_passes: bool,
    ) -> EvaluationReport {
        let environment = EvaluationEnvironment {
            kernel_ref: "kernel".into(),
            model_provider: "fixture".into(),
            model: "model".into(),
            model_parameters_digest: "params".into(),
            tool_profile_digest: "tools".into(),
            execution_profile_digest: "execution".into(),
            plugin_set_digest: "plugins".into(),
            capability_owner_digest: "owners".into(),
            resource_budget_digest: "budget".into(),
            verifier_version: "verifier".into(),
            evaluation_policy_version: "policy".into(),
            environment_fixture_digest: "fixture".into(),
            repeat_count: 1,
        };
        let case = |id: &str, kind: DatasetKind, passed: bool| TaskCaseResult {
            metadata: TaskCaseMetadata {
                task_case_id: id.into(),
                task_family: "历史修复".into(),
                dataset_kind: kind,
                critical: false,
                deterministic: true,
                pass_threshold: None,
            },
            attempts: vec![TaskAttemptResult {
                task_case_id: id.into(),
                repeat_index: 0,
                status: if passed {
                    TaskAttemptStatus::Passed
                } else {
                    TaskAttemptStatus::Failed
                },
                verifier_passed: Some(passed),
                usage: EvaluationUsage {
                    tokens: Some(100),
                    cost: Some(1.0),
                    latency_ms: Some(10),
                    tool_calls: Some(1),
                    model_calls: Some(1),
                    react_steps: Some(1),
                    child_agents: Some(0),
                },
                safety: Some(SafetyAttemptSummary::default()),
                run_id: None,
            }],
        };
        let cases = vec![
            case("repair", DatasetKind::Repair, false),
            case("hidden", DatasetKind::Hidden, generation > 1),
            case(
                "historical-repair",
                DatasetKind::Regression,
                repaired_passes,
            ),
            case("regression", DatasetKind::Regression, true),
        ];
        let child = GenomeRevisionId::generate();
        let datasets: BTreeMap<DatasetKind, DatasetVersionId> = [
            (DatasetKind::Repair, DatasetVersionId::generate()),
            (DatasetKind::Hidden, hidden_version),
            (DatasetKind::Regression, DatasetVersionId::generate()),
        ]
        .into_iter()
        .collect();
        EvaluationReport {
            schema_version: EVALUATION_REPORT_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            lineage: Some("stable/general".into()),
            parent_generation: Some(generation - 1),
            candidate_generation: Some(generation),
            parent: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: parent,
                environment: environment.clone(),
                datasets: datasets.clone(),
                task_cases: cases.clone(),
            },
            candidate: EvaluationRun {
                run_id: EvaluationRunId::generate(),
                genome_revision: child,
                environment,
                datasets,
                task_cases: cases,
            },
            genome_diff: GenomeDiff::default(),
            allowed_mutation_surfaces: BTreeSet::new(),
            gate_decision: GateDecision::Pass,
            lifecycle: EvolutionLifecycle::InheritanceVerified,
            release_record: Some(ReleaseId::generate()),
            inheritance: None,
            artifact_integrity_verified: Some(true),
            audit_integrity_verified: Some(true),
            hidden_dataset_isolated: Some(true),
            generated_at_ms: generation,
        }
    }

    /// 修改 Candidate 中指定 Case 的确定性结果。
    fn set_candidate_case(report: &mut EvaluationReport, task_case_id: &str, passed: bool) {
        let case = report
            .candidate
            .task_cases
            .iter_mut()
            .find(|case| case.metadata.task_case_id == task_case_id)
            .expect("测试 Case 应存在");
        case.attempts[0].status = if passed {
            TaskAttemptStatus::Passed
        } else {
            TaskAttemptStatus::Failed
        };
        case.attempts[0].verifier_passed = Some(passed);
    }

    #[test]
    fn cumulative_gain_rejects_incompatible_dataset_versions() {
        let version_a = DatasetVersionId::generate();
        let version_b = DatasetVersionId::generate();
        let first = report(GenomeRevisionId::generate(), 1, version_a, true);
        let second = report(first.candidate.genome_revision.clone(), 2, version_b, true);
        let history = compute_history(
            &[first, second],
            &[],
            &EvolutionVerdictPolicy::default(),
            Some("stable/general"),
        )
        .expect("历史应可计算");
        assert_eq!(history.hidden_trends.len(), 2);
        assert!(history
            .hidden_trends
            .iter()
            .all(|segment| segment.points.len() == 1));
    }

    /// 累计 Hidden Gain 使用最早 Parent，而不是第一个 Candidate 作为基线。
    #[test]
    fn cumulative_gain_uses_initial_parent_and_current_stable() {
        let hidden = DatasetVersionId::generate();
        let mut first = report(GenomeRevisionId::generate(), 1, hidden.clone(), true);
        set_candidate_case(&mut first, "hidden", true);
        let second = report(first.candidate.genome_revision.clone(), 2, hidden, true);
        let history = compute_history(
            &[first, second],
            &[],
            &EvolutionVerdictPolicy::default(),
            Some("stable/general"),
        )
        .expect("历史应可计算");
        assert_eq!(history.hidden_trends.len(), 1);
        assert_eq!(history.hidden_trends[0].cumulative_gain_pp, Some(100.0));
    }

    #[test]
    fn capability_map_groups_by_task_family() {
        let report = report(
            GenomeRevisionId::generate(),
            1,
            DatasetVersionId::generate(),
            true,
        );
        let history = compute_history(&[report], &[], &EvolutionVerdictPolicy::default(), None)
            .expect("历史应可计算");
        assert_eq!(history.capability_map.len(), 1);
        assert_eq!(history.capability_map[0].task_family, "历史修复");
        assert_eq!(history.capability_map[0].cells[0].total_cases, 4);
    }

    #[test]
    fn fix_survival_tracks_promoted_repairs_and_later_regression() {
        let hidden = DatasetVersionId::generate();
        let first = report(GenomeRevisionId::generate(), 1, hidden.clone(), true);
        let certificate = certificate(&first);
        let second = report(
            first.candidate.genome_revision.clone(),
            2,
            hidden.clone(),
            true,
        );
        let third = report(
            second.candidate.genome_revision.clone(),
            3,
            hidden.clone(),
            true,
        );
        let fourth = report(third.candidate.genome_revision.clone(), 4, hidden, false);
        let history = compute_history(
            &[first, second, third, fourth],
            &[certificate],
            &EvolutionVerdictPolicy::default(),
            Some("stable/general"),
        )
        .expect("历史应可计算");
        assert_eq!(history.fix_survival[0].rate, Rate::new(1, 1));
        assert_eq!(history.fix_survival[1].rate, Rate::new(0, 1));
        assert_eq!(history.fix_survival[1].lost_repairs.len(), 1);
        assert_eq!(history.fix_survival[2].rate, Rate::new(0, 0));
    }

    /// 同代其他分支不能冒充已 Promotion 修复的后代。
    #[test]
    fn fix_survival_only_uses_descendant_lineage() {
        let hidden = DatasetVersionId::generate();
        let first = report(GenomeRevisionId::generate(), 1, hidden.clone(), true);
        let certificate = certificate(&first);
        let mut unrelated = report(GenomeRevisionId::generate(), 2, hidden.clone(), false);
        unrelated.generated_at_ms = 1;
        let descendant = report(first.candidate.genome_revision.clone(), 2, hidden, true);
        let history = compute_history(
            &[first, unrelated, descendant],
            &[certificate],
            &EvolutionVerdictPolicy::default(),
            Some("stable/general"),
        )
        .expect("历史应可计算");
        assert_eq!(history.fix_survival[0].rate, Rate::new(1, 1));
    }

    #[test]
    fn candidate_yield_excludes_invalid_comparisons() {
        let hidden = DatasetVersionId::generate();
        let first = report(GenomeRevisionId::generate(), 1, hidden.clone(), true);
        let mut rejected = report(
            first.candidate.genome_revision.clone(),
            2,
            hidden.clone(),
            true,
        );
        rejected.gate_decision = GateDecision::Reject;
        rejected.release_record = None;
        rejected.lifecycle = EvolutionLifecycle::Rejected;
        let mut invalid = report(rejected.candidate.genome_revision.clone(), 3, hidden, true);
        invalid.candidate.environment.kernel_ref = "other-kernel".into();
        let history = compute_history(
            &[first, rejected, invalid],
            &[],
            &EvolutionVerdictPolicy::default(),
            None,
        )
        .expect("历史应可计算");
        assert_eq!(history.funnel.valid_candidates, 2);
        assert_eq!(history.candidate_yield, Rate::new(1, 2));
        assert_eq!(history.capability_map[0].cells.len(), 2);
        assert_eq!(history.velocity.len(), 2);
        assert_eq!(history.hidden_trends[0].points.len(), 2);
    }

    #[test]
    fn rollback_rate_uses_promotions_as_denominator() {
        let hidden = DatasetVersionId::generate();
        let first = report(GenomeRevisionId::generate(), 1, hidden.clone(), true);
        let mut second = report(first.candidate.genome_revision.clone(), 2, hidden, true);
        second.lifecycle = EvolutionLifecycle::RolledBack;
        let mut rollback = certificate(&second);
        rollback.lifecycle = EvolutionLifecycle::RolledBack;
        rollback.rollback_record = Some(RollbackRecord {
            schema_version: crate::ROLLBACK_RECORD_SCHEMA_VERSION,
            release_record: rollback.release_record.clone(),
            category: RollbackCategory::Performance,
            reason: "资源性能回归".into(),
            evidence: Vec::new(),
            created_at_ms: 2,
        });
        let history = compute_history(
            &[first, second],
            &[rollback],
            &EvolutionVerdictPolicy::default(),
            None,
        )
        .expect("历史应可计算");
        assert_eq!(history.rollback_rate, Rate::new(1, 2));
        assert_eq!(history.rollback_breakdown.performance, 1);
        assert_eq!(
            history.lineage_nodes[1]
                .rollback_record
                .as_ref()
                .map(|record| record.category),
            Some(RollbackCategory::Performance)
        );
    }
}
