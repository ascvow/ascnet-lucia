//! M8 插件 Gate、真实签名、Canary、Stable、回滚和归档端到端测试。

use agent_evaluation::{
    evaluate_plugin_source, FilePluginReleaseArchive, PluginReleaseController, PluginReleaseError,
    PluginRollbackRequestV1, TrustedPluginKeyring, TrustedPluginSigner,
};
use agent_evolution::FileArtifactStore;
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, ComponentInterfaceSnapshot, EpisodeId, EvaluationReportId,
    EvolutionCycleId, GenomeDigest, MutationId, PluginAuditCheck, PluginBuildAttestation,
    PluginCanaryRecord, PluginCanaryState, PluginEvaluationEvidence, PluginEvaluationGateInput,
    PluginEvaluationKind, PluginEvaluationReport, PluginFilePatch, PluginHostAuditEvidence,
    PluginMutationKind, PluginMutationProposal, PluginReleaseEnvelope, PluginReleaseStage,
    PluginSourceArtifact, PluginSourceFile, PluginSourceGateDecision, PreapprovedPluginProfile,
    ReleaseId, SignaturePurpose, COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
    PLUGIN_AUDIT_CHECK_SCHEMA_VERSION, PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
    PLUGIN_CANARY_RECORD_SCHEMA_VERSION, PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
    PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION, PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
    PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION, PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// 完整 M8 Candidate、Gate 报告及其真实制品字节。
struct ReleaseFixture {
    input: PluginEvaluationGateInput,
    report: PluginEvaluationReport,
    component: Vec<u8>,
    bundle: Vec<u8>,
}

/// 三类用途隔离的真实 Ed25519 签名器与公钥 Keyring。
struct SigningFixture {
    build_signer: TrustedPluginSigner,
    release_signer: TrustedPluginSigner,
    build_keys: TrustedPluginKeyring,
    approval_keys: TrustedPluginKeyring,
    release_keys: TrustedPluginKeyring,
}

impl SigningFixture {
    /// 创建固定私钥种子但用途隔离的测试签名控制面。
    fn new() -> Self {
        let build_signer = TrustedPluginSigner::from_secret_bytes(
            "m8-builder-v1",
            SignaturePurpose::BuildAttestation,
            &[11; 32],
        )
        .expect("构建签名器应创建成功");
        let release_signer = TrustedPluginSigner::from_secret_bytes(
            "m8-release-v1",
            SignaturePurpose::PluginRelease,
            &[22; 32],
        )
        .expect("发布签名器应创建成功");
        let mut build_keys = TrustedPluginKeyring::new();
        build_keys
            .insert(build_signer.verifying_key())
            .expect("构建公钥应登记成功");
        let mut release_keys = TrustedPluginKeyring::new();
        release_keys
            .insert(release_signer.verifying_key())
            .expect("发布公钥应登记成功");
        Self {
            build_signer,
            release_signer,
            build_keys,
            approval_keys: TrustedPluginKeyring::new(),
            release_keys,
        }
    }

    /// 为 Candidate 生成用途隔离、摘要精确绑定的 Release 信封。
    fn release(
        &self,
        fixture: &ReleaseFixture,
        stage: PluginReleaseStage,
        lineage: Option<ReleaseId>,
        rollback_target: Option<ArtifactDigest>,
        issued_at_ms: u64,
    ) -> PluginReleaseEnvelope {
        let attestation = fixture.input.build_attestation.clone();
        let attestation_signature = self
            .build_signer
            .sign(
                fixture.input.proposal.plugin_id.clone(),
                fixture.input.proposal.mutation_id.clone(),
                attestation.digest().expect("构建证明摘要应可计算"),
                attestation.built_at_ms + 1,
                100_000,
            )
            .expect("构建证明应完成真实签名");
        let (canary_of, rollback_of) = match stage {
            PluginReleaseStage::Canary => (None, None),
            PluginReleaseStage::Stable => (lineage, None),
            PluginReleaseStage::Rollback => (None, lineage),
        };
        let mut release = PluginReleaseEnvelope {
            schema_version: PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
            release_id: ReleaseId::generate(),
            stage,
            plugin_id: fixture.input.proposal.plugin_id.clone(),
            mutation_id: fixture.input.proposal.mutation_id.clone(),
            candidate_id: fixture.input.proposal.candidate_id.clone(),
            proposal_digest: fixture.input.proposal.digest().expect("提案摘要应可计算"),
            source_digest: fixture
                .input
                .proposal
                .candidate_source
                .digest()
                .expect("源码摘要应可计算"),
            bundle_digest: fixture.input.bundle_digest.clone(),
            evaluation_report_digest: fixture
                .report
                .digest_for_input(&fixture.input)
                .expect("Gate 报告摘要应可计算"),
            attestation,
            attestation_signature: attestation_signature.clone(),
            baseline_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
            expansion_request: None,
            approval: None,
            canary_of,
            rollback_of,
            rollback_target_component_digest: rollback_target,
            issued_at_ms,
            // 外层签名不进入 Release signing digest，先使用结构合法信封占位。
            signature: attestation_signature,
        };
        let signing_digest = release.signing_digest().expect("Release 摘要应可计算");
        release.signature = self
            .release_signer
            .sign(
                release.plugin_id.clone(),
                release.mutation_id.clone(),
                signing_digest,
                issued_at_ms,
                100_000,
            )
            .expect("Release 应完成真实签名");
        release.validate().expect("签名后的 Release 应合法");
        release
    }
}

/// 计算真实字节的协议 SHA-256 摘要。
fn bytes_digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 摘要应合法")
}

/// 构造固定 Genome 摘要。
fn genome_digest(seed: u8) -> GenomeDigest {
    GenomeDigest::from_sha256_hex(format!("{seed:02x}").repeat(32)).expect("Genome 摘要应合法")
}

/// 构造一项结构自洽的受信审计检查。
fn audit(seed: u8, completed_at_ms: u64) -> PluginAuditCheck {
    PluginAuditCheck {
        schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
        report_digest: bytes_digest(&[seed]),
        verifier_revision: bytes_digest(b"m8-audit-verifier-v1"),
        passed: true,
        check_count: 3,
        failure_count: 0,
        completed_at_ms,
    }
}

/// 生成与真实 Component 字节和可选 Parent 精确绑定的完整 Gate Fixture。
fn release_fixture(tag: u8, base_time_ms: u64, parent: Option<&ReleaseFixture>) -> ReleaseFixture {
    let source_bytes = format!("pub fn run() -> u8 {{ {tag} }}").into_bytes();
    let source_digest = bytes_digest(&source_bytes);
    let source = PluginSourceArtifact::new(
        "example.plugin",
        vec![PluginSourceFile {
            path: "src/lib.rs".into(),
            digest: source_digest.clone(),
            size_bytes: source_bytes.len() as u64,
        }],
    )
    .expect("Candidate 源码清单应合法");
    let (mutation, patches, parent_genome_digest) = match parent {
        Some(parent) => {
            let parent_source = parent.input.proposal.candidate_source.clone();
            let parent_digest = parent_source.files[0].digest.clone();
            (
                PluginMutationKind::Update {
                    parent_source: Box::new(parent_source),
                    parent_capabilities: Box::new(
                        parent.input.build_attestation.capabilities.clone(),
                    ),
                },
                vec![PluginFilePatch::Update {
                    path: "src/lib.rs".into(),
                    old_digest: parent_digest,
                    new_digest: source_digest,
                }],
                parent.input.proposal.candidate_genome_digest.clone(),
            )
        }
        None => (
            PluginMutationKind::Create {
                preapproved_profile: PreapprovedPluginProfile::PureCompute,
            },
            vec![PluginFilePatch::Create {
                path: "src/lib.rs".into(),
                new_digest: source_digest,
            }],
            genome_digest(1),
        ),
    };
    let component = vec![0, b'a', b's', b'm', tag, 1, 2, 3];
    let bundle = format!("bundle-v1-{tag}").into_bytes();
    let component_digest = bytes_digest(&component);
    let bundle_digest = bytes_digest(&bundle);
    let interface = ComponentInterfaceSnapshot {
        schema_version: COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
        plugin_id: "example.plugin".into(),
        component_digest: component_digest.clone(),
        world: "example:plugin/world@1.0.0".into(),
        imports: Vec::new(),
        exports: vec!["example:plugin/run".into()],
        scanner_revision: bytes_digest(b"m8-interface-scanner-v1"),
    };
    let proposal = PluginMutationProposal {
        schema_version: PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
        cycle_id: EvolutionCycleId::generate(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        plugin_id: "example.plugin".into(),
        parent_genome_digest,
        candidate_genome_digest: genome_digest(tag.saturating_add(2)),
        mutation,
        candidate_source: source,
        patches,
        claimed_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
        claimed_interface: interface.clone(),
        evidence_episode_ids: vec![EpisodeId::generate()],
        rationale: "根据可信失败证据生成受限插件 Candidate".into(),
        created_at_ms: base_time_ms,
    };
    let build_attestation = PluginBuildAttestation {
        schema_version: PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
        build_id: format!("build-m8-{tag}"),
        plugin_id: proposal.plugin_id.clone(),
        mutation_id: proposal.mutation_id.clone(),
        candidate_id: proposal.candidate_id.clone(),
        proposal_digest: proposal.digest().expect("提案摘要应可计算"),
        source_digest: proposal
            .candidate_source
            .digest()
            .expect("源码摘要应可计算"),
        component_digest: component_digest.clone(),
        component_size_bytes: component.len() as u64,
        interface: interface.clone(),
        capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
        build_environment_digest: bytes_digest(b"m8-build-environment-v1"),
        builder_revision: bytes_digest(b"m8-builder-v1"),
        build_log_digest: bytes_digest(&[tag, 9]),
        reproducible: true,
        built_at_ms: base_time_ms + 10,
    };
    let host_audit = PluginHostAuditEvidence {
        schema_version: PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
        plugin_id: proposal.plugin_id.clone(),
        mutation_id: proposal.mutation_id.clone(),
        candidate_id: proposal.candidate_id.clone(),
        component_digest: component_digest.clone(),
        manifest_digest: bytes_digest(&[tag, 1]),
        interface_digest: interface.digest().expect("接口摘要应可计算"),
        capability_profile_digest: build_attestation
            .capabilities
            .digest()
            .expect("能力摘要应可计算"),
        bundle_digest: bundle_digest.clone(),
        host_smoke: audit(tag, base_time_ms + 20),
        manifest_audit: audit(tag.saturating_add(1), base_time_ms + 21),
        import_audit: audit(tag.saturating_add(2), base_time_ms + 22),
        interface_audit: audit(tag.saturating_add(3), base_time_ms + 23),
        owner_audit: audit(tag.saturating_add(4), base_time_ms + 24),
        runtime_audit: audit(tag.saturating_add(5), base_time_ms + 25),
    };
    let evaluation = |kind, suffix, completed_at_ms| PluginEvaluationEvidence {
        schema_version: PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
        kind,
        plugin_id: proposal.plugin_id.clone(),
        mutation_id: proposal.mutation_id.clone(),
        candidate_id: proposal.candidate_id.clone(),
        component_digest: component_digest.clone(),
        bundle_digest: bundle_digest.clone(),
        dataset_digest: bytes_digest(b"m8-dataset-v1"),
        report_digest: bytes_digest(&[tag, suffix]),
        evaluator_revision: bytes_digest(b"m8-evaluator-v1"),
        case_count: 12,
        failure_count: 0,
        completed_at_ms,
    };
    let safety_evaluation = evaluation(PluginEvaluationKind::Safety, 7, base_time_ms + 30);
    let agent_evaluation = evaluation(PluginEvaluationKind::Agent, 8, base_time_ms + 31);
    let input = PluginEvaluationGateInput {
        schema_version: PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
        report_id: EvaluationReportId::generate(),
        proposal,
        build_attestation,
        bundle_digest,
        host_audit,
        safety_evaluation,
        agent_evaluation,
        evaluated_at_ms: base_time_ms + 40,
    };
    let report = evaluate_plugin_source(&input).expect("完整证据应通过源码 Gate");
    ReleaseFixture {
        input,
        report,
        component,
        bundle,
    }
}

/// 从 Planned 快照生成结构合法的 Running 观察。
fn running_canary(planned: &PluginCanaryRecord, started_at_ms: u64) -> PluginCanaryRecord {
    let mut running = planned.clone();
    running.state = PluginCanaryState::Running;
    running.started_at_ms = Some(started_at_ms);
    running
}

/// 从 Running 快照生成成功或失败的真实健康终态。
fn terminal_canary(
    running: &PluginCanaryRecord,
    succeeded: bool,
    finished_at_ms: u64,
) -> PluginCanaryRecord {
    let mut terminal = running.clone();
    terminal.state = if succeeded {
        PluginCanaryState::Succeeded
    } else {
        PluginCanaryState::Failed
    };
    terminal.finished_at_ms = Some(finished_at_ms);
    terminal.observed_runs = 2;
    terminal.passed_runs = u64::from(succeeded) * 2 + u64::from(!succeeded);
    terminal.failed_runs = u64::from(!succeeded);
    terminal.health_report_digest = Some(bytes_digest(if succeeded {
        b"m8-health-succeeded"
    } else {
        b"m8-health-failed"
    }));
    terminal
}

/// 完整 Gate、真实签名、成功 Canary、Stable、失败 Canary 和旧 Stable 回滚必须形成可复核 lineage。
#[tokio::test]
async fn completes_signed_stable_and_health_rollback_with_full_archive() {
    let temp = TempDir::new().expect("创建测试目录");
    let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
    let signing = SigningFixture::new();
    let archive = FilePluginReleaseArchive::new(temp.path().join("release-archive"), &artifacts)
        .expect("创建发布归档");
    let controller = PluginReleaseController::new(
        &archive,
        &signing.build_keys,
        &signing.approval_keys,
        &signing.release_keys,
    );

    let baseline = release_fixture(1, 100, None);
    let baseline_canary = signing.release(&baseline, PluginReleaseStage::Canary, None, None, 150);
    let admission = controller
        .admit_canary(
            &baseline.input,
            &baseline.report,
            &baseline_canary,
            &baseline.component,
            &baseline.bundle,
        )
        .await
        .expect("完整 Gate 与真实签名应进入 Canary");
    let running = running_canary(&admission.canary, 151);
    controller
        .record_canary_observation(&baseline.input, &baseline.report, &running)
        .await
        .expect("Running 观察应归档");
    let succeeded = terminal_canary(&running, true, 160);
    controller
        .record_canary_observation(&baseline.input, &baseline.report, &succeeded)
        .await
        .expect("成功健康终态应归档");
    let baseline_stable = signing.release(
        &baseline,
        PluginReleaseStage::Stable,
        Some(baseline_canary.release_id.clone()),
        None,
        170,
    );
    let stable_record = controller
        .promote_stable(
            &baseline.input,
            &baseline.report,
            &succeeded,
            &baseline_stable,
            &baseline.component,
            &baseline.bundle,
        )
        .await
        .expect("已归档成功 Canary 应进入 Stable");

    let candidate = release_fixture(2, 200, Some(&baseline));
    let candidate_canary = signing.release(&candidate, PluginReleaseStage::Canary, None, None, 250);
    let admission = controller
        .admit_canary(
            &candidate.input,
            &candidate.report,
            &candidate_canary,
            &candidate.component,
            &candidate.bundle,
        )
        .await
        .expect("Update Candidate 应进入 Canary");
    let running = running_canary(&admission.canary, 251);
    controller
        .record_canary_observation(&candidate.input, &candidate.report, &running)
        .await
        .expect("Update Running 应归档");
    let failed = terminal_canary(&running, false, 260);
    controller
        .record_canary_observation(&candidate.input, &candidate.report, &failed)
        .await
        .expect("健康失败应归档");
    let rollback = signing.release(
        &candidate,
        PluginReleaseStage::Rollback,
        Some(candidate_canary.release_id.clone()),
        Some(baseline_stable.attestation.component_digest.clone()),
        270,
    );
    let wrong_rollback = signing.release(
        &candidate,
        PluginReleaseStage::Rollback,
        Some(candidate_canary.release_id.clone()),
        Some(bytes_digest(b"wrong-stable-component")),
        269,
    );
    assert!(matches!(
        controller
            .rollback_failed_canary(PluginRollbackRequestV1 {
                input: &candidate.input,
                report: &candidate.report,
                failed: &failed,
                rollback: &wrong_rollback,
                rollback_target_release_id: &baseline_stable.release_id,
                candidate_component_bytes: &candidate.component,
                bundle_bytes: &candidate.bundle,
                rollback_target_bytes: b"wrong-stable-component",
            })
            .await,
        Err(PluginReleaseError::InvalidRollbackTarget)
    ));
    let rollback_record = controller
        .rollback_failed_canary(PluginRollbackRequestV1 {
            input: &candidate.input,
            report: &candidate.report,
            failed: &failed,
            rollback: &rollback,
            rollback_target_release_id: &baseline_stable.release_id,
            candidate_component_bytes: &candidate.component,
            bundle_bytes: &candidate.bundle,
            rollback_target_bytes: &baseline.component,
        })
        .await
        .expect("失败 Canary 应回滚到先前受信 Stable Component");

    assert_eq!(stable_record.release.stage, PluginReleaseStage::Stable);
    assert_eq!(rollback_record.release.stage, PluginReleaseStage::Rollback);
    assert_eq!(
        rollback_record
            .rollback_target_artifact
            .as_ref()
            .map(|artifact| &artifact.digest),
        Some(&baseline_stable.attestation.component_digest)
    );
    let history = archive
        .canary_history(&failed.canary_id)
        .await
        .expect("完整 Canary lineage 应可读取");
    assert_eq!(history.len(), 4);
    assert_eq!(
        history.last().map(|record| record.state),
        Some(PluginCanaryState::RolledBack)
    );
    assert!(archive
        .evaluation(&candidate.report.report_id)
        .await
        .expect("评测索引应可验证")
        .is_some());
    assert!(archive
        .release(&rollback.release_id)
        .await
        .expect("Rollback 索引应可验证")
        .is_some());
}

/// RequireApproval、伪签名、跨 Candidate 重放和绕过 Canary 的 Stable 必须在发布副作用前拒绝。
#[tokio::test]
async fn rejects_approval_fake_signature_replay_and_direct_stable() {
    let temp = TempDir::new().expect("创建测试目录");
    let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
    let signing = SigningFixture::new();
    let archive = FilePluginReleaseArchive::new(temp.path().join("release-archive"), &artifacts)
        .expect("创建发布归档");
    let controller = PluginReleaseController::new(
        &archive,
        &signing.build_keys,
        &signing.approval_keys,
        &signing.release_keys,
    );
    let fixture = release_fixture(3, 300, None);
    let canary = signing.release(&fixture, PluginReleaseStage::Canary, None, None, 350);

    let mut approval_input = fixture.input.clone();
    approval_input.host_audit.host_smoke.passed = false;
    approval_input.host_audit.host_smoke.failure_count = 1;
    let approval_report = evaluate_plugin_source(&approval_input)
        .expect("Host smoke 失败应生成 RequireApproval 报告");
    assert_eq!(
        approval_report.decision,
        PluginSourceGateDecision::RequireApproval
    );
    controller
        .archive_evaluation(&approval_input, &approval_report)
        .await
        .expect("RequireApproval 报告仍应完整归档");
    assert!(matches!(
        controller
            .admit_canary(
                &approval_input,
                &approval_report,
                &canary,
                &fixture.component,
                &fixture.bundle,
            )
            .await,
        Err(PluginReleaseError::CanaryRequired)
    ));

    let mut fake_signature = canary.clone();
    fake_signature
        .signature
        .signature_hex
        .replace_range(0..2, "ff");
    assert!(matches!(
        controller
            .admit_canary(
                &fixture.input,
                &fixture.report,
                &fake_signature,
                &fixture.component,
                &fixture.bundle,
            )
            .await,
        Err(PluginReleaseError::Signature(_))
    ));

    let replay_target = release_fixture(4, 400, None);
    assert!(controller
        .admit_canary(
            &replay_target.input,
            &replay_target.report,
            &canary,
            &replay_target.component,
            &replay_target.bundle,
        )
        .await
        .is_err());

    let direct_stable = signing.release(
        &fixture,
        PluginReleaseStage::Stable,
        Some(canary.release_id.clone()),
        None,
        360,
    );
    assert!(matches!(
        controller
            .admit_canary(
                &fixture.input,
                &fixture.report,
                &direct_stable,
                &fixture.component,
                &fixture.bundle,
            )
            .await,
        Err(PluginReleaseError::CanaryRequired)
    ));
    assert!(archive
        .release(&canary.release_id)
        .await
        .expect("拒绝后归档读取应正常")
        .is_none());
}

/// Stable/Rollback 必须引用已经归档的真实终态和先前受信 Stable，而非调用方临时构造的快照或目标。
#[tokio::test]
async fn rejects_unarchived_terminal_and_wrong_rollback_target() {
    let temp = TempDir::new().expect("创建测试目录");
    let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
    let signing = SigningFixture::new();
    let archive = FilePluginReleaseArchive::new(temp.path().join("release-archive"), &artifacts)
        .expect("创建发布归档");
    let controller = PluginReleaseController::new(
        &archive,
        &signing.build_keys,
        &signing.approval_keys,
        &signing.release_keys,
    );
    let fixture = release_fixture(5, 500, None);
    let canary = signing.release(&fixture, PluginReleaseStage::Canary, None, None, 550);
    let admission = controller
        .admit_canary(
            &fixture.input,
            &fixture.report,
            &canary,
            &fixture.component,
            &fixture.bundle,
        )
        .await
        .expect("Canary 应进入 Planned");
    let fabricated_running = running_canary(&admission.canary, 551);
    let fabricated_success = terminal_canary(&fabricated_running, true, 560);
    let stable = signing.release(
        &fixture,
        PluginReleaseStage::Stable,
        Some(canary.release_id.clone()),
        None,
        570,
    );
    assert!(matches!(
        controller
            .promote_stable(
                &fixture.input,
                &fixture.report,
                &fabricated_success,
                &stable,
                &fixture.component,
                &fixture.bundle,
            )
            .await,
        Err(PluginReleaseError::CanaryTerminalNotArchived)
    ));

    controller
        .record_canary_observation(&fixture.input, &fixture.report, &fabricated_running)
        .await
        .expect("Running 应归档");
    let failed = terminal_canary(&fabricated_running, false, 561);
    controller
        .record_canary_observation(&fixture.input, &fixture.report, &failed)
        .await
        .expect("Failed 应归档");
    let wrong_target = bytes_digest(b"untrusted-rollback-target");
    let rollback = signing.release(
        &fixture,
        PluginReleaseStage::Rollback,
        Some(canary.release_id.clone()),
        Some(wrong_target),
        580,
    );
    assert!(matches!(
        controller
            .rollback_failed_canary(PluginRollbackRequestV1 {
                input: &fixture.input,
                report: &fixture.report,
                failed: &failed,
                rollback: &rollback,
                rollback_target_release_id: &stable.release_id,
                candidate_component_bytes: &fixture.component,
                bundle_bytes: &fixture.bundle,
                rollback_target_bytes: b"untrusted-rollback-target",
            })
            .await,
        Err(PluginReleaseError::ReleaseNotFound(_))
    ));
}

/// Canary 的跨句柄并发追加必须只有一个分支成功，后续读取仍是一条有效单调状态链。
#[tokio::test]
async fn serializes_cross_handle_canary_appends_without_fork() {
    let temp = TempDir::new().expect("创建测试目录");
    let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
    let root = temp.path().join("release-archive");
    let first = FilePluginReleaseArchive::new(root.clone(), &artifacts).expect("创建第一归档句柄");
    let second = FilePluginReleaseArchive::new(root, &artifacts).expect("创建第二归档句柄");
    let planned = PluginCanaryRecord {
        schema_version: PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
        canary_id: "canary-concurrency-test".into(),
        release_id: ReleaseId::generate(),
        release_digest: bytes_digest(b"release"),
        plugin_id: "example.plugin".into(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest: bytes_digest(b"component"),
        state: PluginCanaryState::Planned,
        started_at_ms: None,
        finished_at_ms: None,
        observed_runs: 0,
        passed_runs: 0,
        failed_runs: 0,
        health_report_digest: None,
        rollback_release_id: None,
    };
    first
        .append_canary(&planned)
        .await
        .expect("Planned 应先归档");
    let mut passed = running_canary(&planned, 10);
    passed.observed_runs = 1;
    passed.passed_runs = 1;
    let mut failed = running_canary(&planned, 10);
    failed.observed_runs = 1;
    failed.failed_runs = 1;
    let (left, right) = tokio::join!(first.append_canary(&passed), second.append_canary(&failed));
    assert_ne!(left.is_ok(), right.is_ok());
    let history = first
        .canary_history(&planned.canary_id)
        .await
        .expect("并发竞争后历史仍应可验证");
    assert_eq!(history.len(), 2);
}

/// 崩溃遗留的未提交临时文件不得被恢复流程误认成正式 Canary 状态。
#[tokio::test]
async fn ignores_uncommitted_canary_temporary_files() {
    let temp = TempDir::new().expect("创建测试目录");
    let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
    let root = temp.path().join("release-archive");
    let archive = FilePluginReleaseArchive::new(root.clone(), &artifacts).expect("创建归档");
    let planned = PluginCanaryRecord {
        schema_version: PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
        canary_id: "canary-crash-recovery-test".into(),
        release_id: ReleaseId::generate(),
        release_digest: bytes_digest(b"release"),
        plugin_id: "example.plugin".into(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest: bytes_digest(b"component"),
        state: PluginCanaryState::Planned,
        started_at_ms: None,
        finished_at_ms: None,
        observed_runs: 0,
        passed_runs: 0,
        failed_runs: 0,
        health_report_digest: None,
        rollback_release_id: None,
    };
    archive
        .append_canary(&planned)
        .await
        .expect("Planned 应先归档");

    let canary_directory = root.join("canaries").join(format!(
        "{:x}",
        Sha256::digest(planned.canary_id.as_bytes())
    ));
    std::fs::write(canary_directory.join(".append-999-1.tmp"), b"partial-json")
        .expect("模拟崩溃遗留临时文件");

    let history = archive
        .canary_history(&planned.canary_id)
        .await
        .expect("恢复读取应忽略未提交临时文件");
    assert_eq!(history.as_slice(), std::slice::from_ref(&planned));

    std::fs::write(
        canary_directory.join(format!("{}.json", "0".repeat(64))),
        serde_json::to_vec(&planned).expect("序列化伪造记录"),
    )
    .expect("写入摘要错绑的伪造正式记录");
    assert!(archive.canary_history(&planned.canary_id).await.is_err());
}

/// 归档中间目录被替换为符号链接时必须失败关闭，不得向根目录外写入索引。
#[cfg(unix)]
#[tokio::test]
async fn rejects_intermediate_symlink_escape() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("创建测试目录");
    let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
    let root = temp.path().join("release-archive");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).expect("创建外部目录");
    let archive = FilePluginReleaseArchive::new(root.clone(), &artifacts).expect("创建归档");
    symlink(&outside, root.join("canaries")).expect("创建攻击符号链接");
    let planned = PluginCanaryRecord {
        schema_version: PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
        canary_id: "canary-symlink-test".into(),
        release_id: ReleaseId::generate(),
        release_digest: bytes_digest(b"release"),
        plugin_id: "example.plugin".into(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest: bytes_digest(b"component"),
        state: PluginCanaryState::Planned,
        started_at_ms: None,
        finished_at_ms: None,
        observed_runs: 0,
        passed_runs: 0,
        failed_runs: 0,
        health_report_digest: None,
        rollback_release_id: None,
    };
    assert!(archive.append_canary(&planned).await.is_err());
    assert!(std::fs::read_dir(&outside)
        .expect("外部目录应可读取")
        .next()
        .is_none());
}
