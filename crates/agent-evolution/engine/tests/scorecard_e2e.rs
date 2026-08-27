//! 固定 Evaluation Artifact 驱动的完整 Scorecard 验收场景。

use agent_evolution::{
    compute_scorecard, ArtifactStore, BehaviorAssessment, EvolutionCertificate,
    EvolutionCertificateInput, EvolutionVerdictPolicy, FileArtifactStore,
    FileEvaluationReportStore, FileEvolutionArchive, HeadlineVerdict,
};
use agent_evolution_protocol::{
    DatasetKind, DatasetVersionId, EpisodeId, EvaluationEnvironment, EvaluationReport,
    EvaluationReportId, EvaluationRun, EvaluationRunId, EvaluationUsage, EvolutionIssueId,
    EvolutionLifecycle, GateDecision, GenomeDiff, GenomeRevisionId, InheritanceVerification,
    MutationId, MutationSurface, ReleaseId, RunId, SafetyAttemptSummary, TaskAttemptResult,
    TaskAttemptStatus, TaskCaseMetadata, TaskCaseResult, EVALUATION_REPORT_SCHEMA_VERSION,
};
use std::collections::BTreeMap;

/// 构造一个有两个 Repeat 的 Case；`score` 只允许 0、0.5 或 1。
fn case(id: String, kind: DatasetKind, score: f64, critical: bool) -> TaskCaseResult {
    let passed = if score == 1.0 {
        2
    } else if score == 0.5 {
        1
    } else {
        0
    };
    TaskCaseResult {
        metadata: TaskCaseMetadata {
            task_case_id: id.clone(),
            task_family: format!("{kind:?}"),
            dataset_kind: kind,
            critical,
            deterministic: false,
            pass_threshold: Some(0.8),
        },
        attempts: (0..2)
            .map(|repeat_index| {
                let success = repeat_index < passed;
                TaskAttemptResult {
                    task_case_id: id.clone(),
                    repeat_index,
                    status: if success {
                        TaskAttemptStatus::Passed
                    } else {
                        TaskAttemptStatus::Failed
                    },
                    verifier_passed: Some(success),
                    usage: EvaluationUsage {
                        tokens: Some(1_000),
                        cost: Some(0.10),
                        latency_ms: Some(1_000),
                        tool_calls: Some(4),
                        model_calls: Some(2),
                        react_steps: Some(3),
                        child_agents: Some(0),
                    },
                    safety: Some(SafetyAttemptSummary::default()),
                    run_id: None,
                }
            })
            .collect(),
    }
}

/// 按目标通过 Case 数构造 100 个等权 Case。
fn dataset(kind: DatasetKind, passed: usize, critical: usize) -> Vec<TaskCaseResult> {
    (0..100)
        .map(|index| {
            case(
                format!("{kind:?}-{index:03}"),
                kind,
                if index < passed { 1.0 } else { 0.0 },
                index < critical,
            )
        })
        .collect()
}

/// 构造用于精确控制 Stability 的 Safety Case；安全计数仍全部为零。
fn stability_cases(flaky: usize) -> Vec<TaskCaseResult> {
    (0..100)
        .map(|index| {
            case(
                format!("Safety-{index:03}"),
                DatasetKind::Safety,
                if index < flaky { 0.5 } else { 1.0 },
                false,
            )
        })
        .collect()
}

/// 创建与开发 Prompt Candidate D 数字一致的真实报告输入。
fn evolved_report() -> EvaluationReport {
    let environment = EvaluationEnvironment {
        kernel_ref: "kernel-fixture-v1".into(),
        model_provider: "fixture".into(),
        model: "paired-deterministic-seed".into(),
        model_parameters_digest: "params-v1".into(),
        tool_profile_digest: "tools-v1".into(),
        execution_profile_digest: "evaluation-v1".into(),
        plugin_set_digest: "plugins-v1".into(),
        capability_owner_digest: "owners-v1".into(),
        plugin_environment_digest: "plugins-and-owners-v1".into(),
        resource_budget_digest: "budget-v1".into(),
        verifier_version: "verifier-v1".into(),
        evaluation_policy_version: "evaluation-v1".into(),
        environment_fixture_digest: "fixture-v1".into(),
        repeat_count: 2,
    };
    let mut parent_cases = dataset(DatasetKind::Repair, 30, 0);
    parent_cases.extend(dataset(DatasetKind::Hidden, 58, 0));
    parent_cases.extend(dataset(DatasetKind::Regression, 100, 10));
    parent_cases.extend(stability_cases(32));
    let mut candidate_cases = dataset(DatasetKind::Repair, 92, 0);
    candidate_cases.extend(dataset(DatasetKind::Hidden, 79, 0));
    candidate_cases.extend(dataset(DatasetKind::Regression, 99, 10));
    candidate_cases.extend(stability_cases(12));
    let datasets: BTreeMap<_, _> = [
        (DatasetKind::Repair, DatasetVersionId::generate()),
        (DatasetKind::Hidden, DatasetVersionId::generate()),
        (DatasetKind::Regression, DatasetVersionId::generate()),
        (DatasetKind::Safety, DatasetVersionId::generate()),
    ]
    .into_iter()
    .collect();
    let candidate_revision = GenomeRevisionId::generate();
    EvaluationReport {
        schema_version: EVALUATION_REPORT_SCHEMA_VERSION,
        report_id: EvaluationReportId::generate(),
        lineage: Some("stable/general".into()),
        parent_generation: Some(6),
        candidate_generation: Some(7),
        parent: EvaluationRun {
            run_id: EvaluationRunId::generate(),
            genome_revision: GenomeRevisionId::generate(),
            environment: environment.clone(),
            datasets: datasets.clone(),
            task_cases: parent_cases,
        },
        candidate: EvaluationRun {
            run_id: EvaluationRunId::generate(),
            genome_revision: candidate_revision.clone(),
            environment,
            datasets,
            task_cases: candidate_cases,
        },
        genome_diff: GenomeDiff {
            changed_surfaces: [MutationSurface::TaskStrategyPrompt].into_iter().collect(),
            summary: vec!["只修改 Task Strategy Prompt 制品".into()],
            artifact: None,
        },
        allowed_mutation_surfaces: [MutationSurface::TaskStrategyPrompt].into_iter().collect(),
        gate_decision: GateDecision::Pass,
        lifecycle: EvolutionLifecycle::InheritanceVerified,
        release_record: Some(ReleaseId::generate()),
        inheritance: Some(InheritanceVerification {
            expected_genome: candidate_revision.clone(),
            observed_genome_after_restart: Some(candidate_revision),
            restart_cases_passed: 10,
            restart_cases_total: 10,
            new_session_cases_passed: 10,
            new_session_cases_total: 10,
            old_session_parent_preserved: Some(true),
            stable_reference_verified: true,
            genome_digest_verified: true,
            verified: true,
        }),
        artifact_integrity_verified: Some(true),
        audit_integrity_verified: Some(true),
        hidden_dataset_isolated: Some(true),
        generated_at_ms: 1,
    }
}

/// 验证完整 Scorecard；设置 `LUCIA_EVOLUTION_E2E_ROOT` 时额外导出可供 CLI 验收的归档。
#[tokio::test]
async fn full_evolved_fixture_matches_scorecard_contract() {
    let report = evolved_report();
    let scorecard = compute_scorecard(&report, &EvolutionVerdictPolicy::default())
        .expect("完整 Fixture 应生成评分卡");
    assert_eq!(
        scorecard.behavior_assessment,
        BehaviorAssessment::GeneralizedImprovement
    );
    assert_eq!(scorecard.headline_verdict, HeadlineVerdict::Evolved);
    assert_eq!(scorecard.datasets.repair.parent_score, Some(0.30));
    assert_eq!(scorecard.datasets.repair.candidate_score, Some(0.92));
    assert_eq!(scorecard.datasets.hidden.parent_score, Some(0.58));
    assert_eq!(scorecard.datasets.hidden.candidate_score, Some(0.79));
    assert_eq!(
        scorecard.datasets.regression.retention.retention.ratio(),
        Some(0.99)
    );
    assert_eq!(scorecard.datasets.parent_stability.stability, Some(0.92));
    assert_eq!(scorecard.datasets.candidate_stability.stability, Some(0.97));
    let parent = scorecard
        .capability
        .parent_score
        .expect("Parent 分数应存在");
    let candidate = scorecard
        .capability
        .candidate_score
        .expect("Candidate 分数应存在");
    assert!((parent - 64.2).abs() < 1e-9, "Parent = {parent}");
    assert!((candidate - 87.4).abs() < 1e-9, "Candidate = {candidate}");
    assert!((scorecard.capability.net_gain.expect("应有增益") - 23.2).abs() < 1e-9);
    assert!(scorecard
        .inheritance
        .as_ref()
        .expect("应有继承指标")
        .rate()
        .is_complete());

    if let Some(root) = std::env::var_os("LUCIA_EVOLUTION_E2E_ROOT") {
        let root = std::path::PathBuf::from(root);
        FileEvaluationReportStore::new(&root)
            .append(&report)
            .await
            .expect("应导出 EvaluationReport");
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let candidate_artifact = artifacts
            .put("application/json", br#"{"candidate":true}"#)
            .await
            .expect("应导出 Candidate Artifact");
        let scorecard_artifact = artifacts
            .put(
                "application/json",
                &serde_json::to_vec_pretty(&scorecard).expect("Scorecard 应可序列化"),
            )
            .await
            .expect("应导出 Scorecard Artifact");
        let inheritance_artifact = artifacts
            .put(
                "application/json",
                &serde_json::to_vec_pretty(report.inheritance.as_ref().expect("应有继承证据"))
                    .expect("继承证据应可序列化"),
            )
            .await
            .expect("应导出继承证据 Artifact");
        let promotion_certificate = EvolutionCertificate::create(
            EvolutionCertificateInput {
                parent_revision: report.parent.genome_revision.clone(),
                child_revision: report.candidate.genome_revision.clone(),
                source_episode_ids: vec![EpisodeId::generate()],
                evolution_issue_id: EvolutionIssueId::generate(),
                mutation_id: MutationId::generate(),
                allowed_diff: report.genome_diff.clone(),
                candidate_artifacts: vec![candidate_artifact],
                repair_dataset: report.candidate.datasets[&DatasetKind::Repair].clone(),
                regression_dataset: report.candidate.datasets[&DatasetKind::Regression].clone(),
                hidden_dataset: report.candidate.datasets[&DatasetKind::Hidden].clone(),
                safety_dataset: report.candidate.datasets[&DatasetKind::Safety].clone(),
                repaired_task_case_ids: vec!["Repair-000".into()],
                scorecard: scorecard_artifact,
                release_record: report.release_record.clone().expect("应有 Release"),
            },
            &scorecard,
        )
        .expect("应生成 Promotion Certificate");
        let certificate = promotion_certificate
            .with_inheritance(
                report.inheritance.as_ref().expect("应有继承证据"),
                inheritance_artifact,
                vec![RunId::generate()],
            )
            .expect("应生成继承状态修订");
        certificate
            .verify(&artifacts)
            .await
            .expect("Certificate 应通过 CAS 验证");
        let archive = FileEvolutionArchive::new(&root);
        archive
            .append_scorecard(&scorecard)
            .await
            .expect("应归档 Scorecard");
        archive
            .append_certificate(&promotion_certificate)
            .await
            .expect("应归档 Promotion Certificate");
        archive
            .append_certificate(&certificate)
            .await
            .expect("应归档继承状态修订");
        println!(
            "fixture_root={} report={} release={}",
            root.display(),
            report.report_id,
            certificate.release_record
        );
    }
}
