use agent_evaluation::evaluate_plugin_source;
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, ComponentInterfaceSnapshot, EpisodeId, EvaluationReportId,
    EvolutionCycleId, GenomeDigest, MutationId, PluginAuditCheck, PluginBuildAttestation,
    PluginEvaluationEvidence, PluginEvaluationGateInput, PluginEvaluationKind, PluginFilePatch,
    PluginHostAuditEvidence, PluginMutationKind, PluginMutationProposal, PluginSourceArtifact,
    PluginSourceFile, PluginSourceGateDecision, PluginSourceGateFailure, PreapprovedPluginProfile,
    COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION, PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
    PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION, PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
    PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION, PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
    PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
};
use std::collections::BTreeSet;

/// 构造固定测试摘要。
fn digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造固定 Genome 摘要。
fn genome_digest(seed: char) -> GenomeDigest {
    GenomeDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造与真实 Component 摘要绑定的接口扫描结果。
fn interface(component_digest: ArtifactDigest) -> ComponentInterfaceSnapshot {
    ComponentInterfaceSnapshot {
        schema_version: COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
        plugin_id: "example.plugin".into(),
        component_digest,
        world: "example:plugin/world@1.0.0".into(),
        imports: Vec::new(),
        exports: vec!["example:plugin/run".into()],
        scanner_revision: digest('f'),
    }
}

/// 构造不含 Parent 的预批准 Create 提案。
fn proposal() -> PluginMutationProposal {
    let source = PluginSourceArtifact::new(
        "example.plugin",
        vec![PluginSourceFile {
            path: "src/lib.rs".into(),
            digest: digest('a'),
            size_bytes: 16,
        }],
    )
    .expect("测试源码应合法");
    PluginMutationProposal {
        schema_version: PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
        cycle_id: EvolutionCycleId::generate(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        plugin_id: "example.plugin".into(),
        parent_genome_digest: genome_digest('1'),
        candidate_genome_digest: genome_digest('2'),
        mutation: PluginMutationKind::Create {
            preapproved_profile: PreapprovedPluginProfile::PureCompute,
        },
        candidate_source: source,
        patches: vec![PluginFilePatch::Create {
            path: "src/lib.rs".into(),
            new_digest: digest('a'),
        }],
        claimed_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
        claimed_interface: interface(digest('b')),
        evidence_episode_ids: vec![EpisodeId::generate()],
        rationale: "根据可信失败证据创建受限纯计算插件".into(),
        created_at_ms: 10,
    }
}

/// 构造与提案和真实扫描结果精确绑定的构建证明。
fn build_attestation(proposal: &PluginMutationProposal) -> PluginBuildAttestation {
    let component_digest = digest('c');
    PluginBuildAttestation {
        schema_version: PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
        build_id: "build-m8-gate-1".into(),
        plugin_id: proposal.plugin_id.clone(),
        mutation_id: proposal.mutation_id.clone(),
        candidate_id: proposal.candidate_id.clone(),
        proposal_digest: proposal.digest().expect("测试提案摘要应可计算"),
        source_digest: proposal
            .candidate_source
            .digest()
            .expect("测试源码摘要应可计算"),
        component_digest: component_digest.clone(),
        component_size_bytes: 128,
        interface: interface(component_digest),
        capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
        build_environment_digest: digest('d'),
        builder_revision: digest('e'),
        build_log_digest: digest('f'),
        reproducible: true,
        built_at_ms: 20,
    }
}

/// 构造一项结构自洽的 Host 审计检查。
fn audit(seed: char, completed_at_ms: u64) -> PluginAuditCheck {
    PluginAuditCheck {
        schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
        report_digest: digest(seed),
        verifier_revision: digest('f'),
        passed: true,
        check_count: 2,
        failure_count: 0,
        completed_at_ms,
    }
}

/// 构造全部证据通过且摘要精确绑定的 Gate 输入。
fn gate_input() -> PluginEvaluationGateInput {
    let proposal = proposal();
    let build_attestation = build_attestation(&proposal);
    let component_digest = build_attestation.component_digest.clone();
    let bundle_digest = digest('e');
    let host_audit = PluginHostAuditEvidence {
        schema_version: PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
        plugin_id: proposal.plugin_id.clone(),
        mutation_id: proposal.mutation_id.clone(),
        candidate_id: proposal.candidate_id.clone(),
        component_digest: component_digest.clone(),
        manifest_digest: digest('d'),
        interface_digest: build_attestation
            .interface
            .digest()
            .expect("测试接口摘要应可计算"),
        capability_profile_digest: build_attestation
            .capabilities
            .digest()
            .expect("测试能力摘要应可计算"),
        bundle_digest: bundle_digest.clone(),
        host_smoke: audit('1', 30),
        manifest_audit: audit('2', 31),
        import_audit: audit('3', 32),
        interface_audit: audit('4', 33),
        owner_audit: audit('5', 34),
        runtime_audit: audit('6', 35),
    };
    let evaluation = |kind, report_seed, completed_at_ms| PluginEvaluationEvidence {
        schema_version: PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
        kind,
        plugin_id: proposal.plugin_id.clone(),
        mutation_id: proposal.mutation_id.clone(),
        candidate_id: proposal.candidate_id.clone(),
        component_digest: component_digest.clone(),
        bundle_digest: bundle_digest.clone(),
        dataset_digest: digest('a'),
        report_digest: digest(report_seed),
        evaluator_revision: digest('f'),
        case_count: 10,
        failure_count: 0,
        completed_at_ms,
    };
    let safety_evaluation = evaluation(PluginEvaluationKind::Safety, 'b', 40);
    let agent_evaluation = evaluation(PluginEvaluationKind::Agent, 'c', 41);
    PluginEvaluationGateInput {
        schema_version: PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
        report_id: EvaluationReportId::generate(),
        proposal,
        build_attestation,
        bundle_digest,
        host_audit,
        safety_evaluation,
        agent_evaluation,
        evaluated_at_ms: 50,
    }
}

/// 把一项 Host 检查改为结构自洽的失败结果。
fn fail(check: &mut PluginAuditCheck) {
    check.passed = false;
    check.failure_count = 1;
}

/// 全部可信证据通过时，源码 Gate 最多产生 Canary 报告。
#[test]
fn complete_trusted_evidence_only_enters_canary() {
    let input = gate_input();
    let report = evaluate_plugin_source(&input).expect("完整可信证据应产生报告");
    assert_eq!(report.decision, PluginSourceGateDecision::Canary);
    assert!(report.failures.is_empty());
    report
        .validate_for_input(&input)
        .expect("报告全部摘要必须可由输入复核");
}

/// 任一构建、Host、Safety 或 Agent 硬失败都只能进入人工审批。
#[test]
fn every_required_failure_is_preserved_for_approval() {
    let mut input = gate_input();
    input.build_attestation.reproducible = false;
    fail(&mut input.host_audit.host_smoke);
    fail(&mut input.host_audit.manifest_audit);
    fail(&mut input.host_audit.import_audit);
    fail(&mut input.host_audit.interface_audit);
    fail(&mut input.host_audit.owner_audit);
    fail(&mut input.host_audit.runtime_audit);
    input.safety_evaluation.failure_count = 1;
    input.agent_evaluation.failure_count = 1;

    let report = evaluate_plugin_source(&input).expect("硬失败应产生人工审批报告");
    assert_eq!(report.decision, PluginSourceGateDecision::RequireApproval);
    assert_eq!(
        report.failures,
        BTreeSet::from([
            PluginSourceGateFailure::NonReproducibleBuild,
            PluginSourceGateFailure::HostSmoke,
            PluginSourceGateFailure::ManifestAudit,
            PluginSourceGateFailure::ImportAudit,
            PluginSourceGateFailure::InterfaceAudit,
            PluginSourceGateFailure::OwnerAudit,
            PluginSourceGateFailure::RuntimeAudit,
            PluginSourceGateFailure::SafetyEvaluation,
            PluginSourceGateFailure::AgentEvaluation,
        ])
    );
}

/// Bundle 摘要错绑时失败关闭，不能降级为人工审批报告。
#[test]
fn evidence_rebinding_is_rejected_before_report_generation() {
    let mut input = gate_input();
    input.host_audit.bundle_digest = digest('9');
    assert!(evaluate_plugin_source(&input).is_err());
}

/// 决策类型在反序列化层拒绝 Stable 与 AutoPromote。
#[test]
fn source_gate_type_has_no_stable_or_auto_promote_path() {
    assert!(serde_json::from_str::<PluginSourceGateDecision>("\"stable\"").is_err());
    assert!(serde_json::from_str::<PluginSourceGateDecision>("\"auto_promote\"").is_err());
    assert_eq!(
        serde_json::from_str::<PluginSourceGateDecision>("\"canary\"")
            .expect("Canary 应为合法源码 Gate 决策"),
        PluginSourceGateDecision::Canary
    );
}
