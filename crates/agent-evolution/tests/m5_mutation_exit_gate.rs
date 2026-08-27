//! M5 Episode 选择、脱敏证据与有界 Prompt 变异的离线 Exit Gate。

use agent_evolution::{
    ArtifactStore, BoundedPromptMutator, EpisodeSelectionError, EpisodeSelector, EpisodeStore,
    EvolutionOutbox, EvolutionOutboxItem, FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox,
    FileIssueObservationStore, IssueObservation, IssueObservationStore, MutationEpisodeEvidence,
    MutationEvidence, MutationFailureEvidence, MutationProposalContext, PromptMutationDraft,
    PromptMutationError, PromptMutationGenerationError, PromptMutationGenerator,
    PromptMutationRequest, TASK_STRATEGY_MVP_CANDIDATE_COUNT,
};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, AttributionMethod, DataClass, DiagnosticStatus, Episode,
    EpisodeDataPolicy, EpisodeId, EventId, EvolutionEligibility, EvolutionIssueId, ExpectedEffect,
    FailureAttribution, FailureClassification, FailureDisposition, FailureKind, FailureRecord,
    FailureRecordId, GenomeDigest, GenomeRevisionId, MutationRisk, MutationSurface, Outcome,
    ReplayabilityGrade, RunId, TaskDescriptor, UsageSummary, EPISODE_SCHEMA_VERSION,
};
use async_trait::async_trait;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};
use uuid::Uuid;

/// 创建不会与并行测试冲突的本地临时目录。
fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lucia-m5-{label}-{}", Uuid::new_v4().simple()))
}

/// 构造固定 SHA-256 Genome 摘要。
fn genome_digest(seed: char) -> GenomeDigest {
    GenomeDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造固定 SHA-256 Artifact 摘要。
fn artifact_digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造携带指定媒体类型的 CAS 引用，用于验证 Selector 不会透传引用字段。
fn artifact(seed: char, media_type: &str) -> ArtifactRef {
    ArtifactRef {
        digest: artifact_digest(seed),
        media_type: media_type.to_string(),
        size_bytes: 32,
    }
}

/// 返回已完成脱敏且允许进入本地进化的 Episode 策略。
fn eligible_policy() -> EpisodeDataPolicy {
    let mut policy = EpisodeDataPolicy::for_class(DataClass::Internal);
    policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    policy.redaction_rules_version = Some("redaction-v1".to_string());
    policy
}

/// 返回仍在等待脱敏、因而不能进入 Mutator 的 Episode 策略。
fn pending_redaction_policy() -> EpisodeDataPolicy {
    let mut policy = EpisodeDataPolicy::for_class(DataClass::Internal);
    policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;
    policy
}

/// 构造一条包含泄漏探针的 Episode；Selector 只能保留显式白名单字段。
fn episode(episode_id: EpisodeId, outcome: Outcome, data_policy: EpisodeDataPolicy) -> Episode {
    Episode {
        schema_version: EPISODE_SCHEMA_VERSION,
        episode_id,
        run_id: RunId::generate(),
        session_id: "USER_SESSION_BODY_DO_NOT_LEAK".to_string(),
        genome_revision_id: GenomeRevisionId::generate(),
        task: TaskDescriptor {
            family: "code-edit".to_string(),
            input_ref: Some(artifact('1', "CAS_INPUT_REF_DO_NOT_LEAK")),
            tags: BTreeSet::from(["repair".to_string()]),
        },
        event_stream_ref: artifact('2', "RAW_TOOL_RESULT_DO_NOT_LEAK"),
        supervision: None,
        environment_ref: Some(artifact('3', "HIDDEN_DATASET_PATH_DO_NOT_LEAK")),
        outcome: Some(outcome),
        failures: vec![FailureClassification {
            kind: FailureKind::ToolExecution,
            evidence_event_ids: vec!["USER_CONTENT_EVENT_DO_NOT_LEAK".to_string()],
            confidence: 0.9,
            rule_derived: true,
            model_assisted: false,
        }],
        usage: UsageSummary {
            input_tokens: Some(10),
            output_tokens: Some(20),
            total_tokens: Some(30),
            react_steps: 2,
            elapsed_ms: Some(40),
        },
        replayability: ReplayabilityGrade::FixtureReproducible,
        data_policy,
        event_count: 1,
        started_at_ms: 10,
        finished_at_ms: 20,
    }
}

/// 构造一条可由 Issue Aggregator 稳定重建的失败观察。
fn failure_record(episode_id: EpisodeId, detected_at: EventId) -> FailureRecord {
    FailureRecord {
        record_id: FailureRecordId::generate(),
        episode_id,
        attribution: FailureAttribution {
            detected_at,
            suspected_origin: None,
            propagation_path: Vec::new(),
            decisive_step: None,
            failure_class: FailureKind::ToolExecution,
            confidence: 0.9,
            evidence: Vec::new(),
            method: AttributionMethod::DeterministicRule,
        },
        status: DiagnosticStatus::Observed,
    }
}

/// 构造待选择的 Outbox 记录。
fn outbox_item(
    outbox_id: &str,
    episode_id: EpisodeId,
    issue_id: EvolutionIssueId,
    outcome: Outcome,
    disposition: FailureDisposition,
) -> EvolutionOutboxItem {
    EvolutionOutboxItem {
        outbox_id: outbox_id.to_string(),
        episode_id,
        outcome,
        disposition,
        issue_id: Some(issue_id),
        issue_status: DiagnosticStatus::Clustered,
        created_at_ms: 20,
        consumed: false,
    }
}

/// 已脱敏且明确进入 EvolutionCandidate 的失败必须被选中，且选择不会提前消费 Outbox。
#[tokio::test]
async fn selects_only_eligible_redacted_failure_evidence() {
    let root = temp_root("selector");
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
    let observations = Arc::new(FileIssueObservationStore::new(root.join("observations")));
    let issue_id = EvolutionIssueId::generate();
    let digest = genome_digest('a');
    let first_episode_id = EpisodeId::generate();
    let selected_episode_id = EpisodeId::generate();
    let first_event = EventId::generate();
    let selected_event = EventId::generate();

    observations
        .append(&IssueObservation::new(
            issue_id.clone(),
            first_episode_id.clone(),
            &digest,
            failure_record(first_episode_id, first_event),
        ))
        .await
        .expect("应追加第一条观察");
    observations
        .append(&IssueObservation::new(
            issue_id.clone(),
            selected_episode_id.clone(),
            &digest,
            failure_record(selected_episode_id.clone(), selected_event.clone()),
        ))
        .await
        .expect("应追加第二条观察");
    let selected = episode(
        selected_episode_id.clone(),
        Outcome::TaskFailure,
        eligible_policy(),
    );
    episodes.append(&selected).await.expect("应追加 Episode");
    outbox
        .append(&outbox_item(
            "eligible",
            selected_episode_id.clone(),
            issue_id.clone(),
            Outcome::TaskFailure,
            FailureDisposition::EvolutionCandidate,
        ))
        .await
        .expect("应追加 Outbox");

    let evidence = EpisodeSelector::new(outbox.clone(), episodes, observations)
        .select()
        .await
        .expect("合法证据应可选择");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].issue_id, issue_id);
    assert_eq!(evidence[0].episodes.len(), 1);
    assert_eq!(evidence[0].episodes[0].episode_id, selected_episode_id);
    assert_eq!(evidence[0].episodes[0].usage.total_tokens, Some(30));
    assert!(evidence[0].episodes[0].failure.rule_derived);
    assert_eq!(outbox.pending().await.expect("应读取 Outbox").len(), 1);

    let encoded = serde_json::to_string(&evidence).expect("脱敏证据应可序列化");
    for forbidden in [
        "USER_SESSION_BODY_DO_NOT_LEAK",
        "CAS_INPUT_REF_DO_NOT_LEAK",
        "RAW_TOOL_RESULT_DO_NOT_LEAK",
        "HIDDEN_DATASET_PATH_DO_NOT_LEAK",
        "USER_CONTENT_EVENT_DO_NOT_LEAK",
        selected_event.as_str(),
        "input_ref",
        "event_stream_ref",
        "environment_ref",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "MutationEvidence 泄漏了禁止字段：{forbidden}"
        );
    }
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// NotEligible、待脱敏、非行为终态和非候选处置都不得触达 Issue Observation 输入。
#[tokio::test]
async fn excludes_ineligible_or_untrusted_outbox_items() {
    let root = temp_root("filters");
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let outbox = Arc::new(FileEvolutionOutbox::new(root.join("outbox")));
    let observations = Arc::new(FileIssueObservationStore::new(root.join("observations")));
    let issue_id = EvolutionIssueId::generate();

    let cases = [
        (
            "not-eligible",
            Outcome::TaskFailure,
            EpisodeDataPolicy::for_class(DataClass::Internal),
            FailureDisposition::EvolutionCandidate,
        ),
        (
            "pending-redaction",
            Outcome::TaskFailure,
            pending_redaction_policy(),
            FailureDisposition::EvolutionCandidate,
        ),
        (
            "success",
            Outcome::Success,
            eligible_policy(),
            FailureDisposition::EvolutionCandidate,
        ),
        (
            "safety",
            Outcome::SafetyFailure,
            eligible_policy(),
            FailureDisposition::EvolutionCandidate,
        ),
        (
            "infrastructure",
            Outcome::InfrastructureFailure,
            eligible_policy(),
            FailureDisposition::EvolutionCandidate,
        ),
        (
            "unverifiable",
            Outcome::Unverifiable,
            eligible_policy(),
            FailureDisposition::EvolutionCandidate,
        ),
        (
            "observe",
            Outcome::TaskFailure,
            eligible_policy(),
            FailureDisposition::Observe,
        ),
    ];

    for (index, (id, outcome, policy, disposition)) in cases.into_iter().enumerate() {
        let episode_id = EpisodeId::generate();
        episodes
            .append(&episode(episode_id.clone(), outcome.clone(), policy))
            .await
            .expect("应追加过滤用 Episode");
        let mut item = outbox_item(id, episode_id, issue_id.clone(), outcome, disposition);
        item.created_at_ms += index as u64;
        outbox.append(&item).await.expect("应追加过滤用 Outbox");
    }

    let selected = EpisodeSelector::new(outbox, episodes, observations)
        .select()
        .await
        .expect("不合资格记录应被安全忽略");
    assert!(selected.is_empty());
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Outbox/Episode Outcome 或 Outbox/Issue 绑定冲突必须失败关闭。
#[tokio::test]
async fn fails_closed_on_episode_or_issue_binding_conflict() {
    let outcome_root = temp_root("outcome-conflict");
    let episodes = Arc::new(FileEpisodeStore::new(outcome_root.join("episodes")));
    let outbox = Arc::new(FileEvolutionOutbox::new(outcome_root.join("outbox")));
    let observations = Arc::new(FileIssueObservationStore::new(
        outcome_root.join("observations"),
    ));
    let episode_id = EpisodeId::generate();
    episodes
        .append(&episode(
            episode_id.clone(),
            Outcome::TaskFailure,
            eligible_policy(),
        ))
        .await
        .expect("应追加 Episode");
    outbox
        .append(&outbox_item(
            "outcome-conflict",
            episode_id,
            EvolutionIssueId::generate(),
            Outcome::BudgetFailure,
            FailureDisposition::EvolutionCandidate,
        ))
        .await
        .expect("应追加 Outbox");
    assert!(matches!(
        EpisodeSelector::new(outbox, episodes, observations)
            .select()
            .await,
        Err(EpisodeSelectionError::OutcomeMismatch { .. })
    ));
    let _ = tokio::fs::remove_dir_all(outcome_root).await;

    let issue_root = temp_root("issue-conflict");
    let episodes = Arc::new(FileEpisodeStore::new(issue_root.join("episodes")));
    let outbox = Arc::new(FileEvolutionOutbox::new(issue_root.join("outbox")));
    let observations = Arc::new(FileIssueObservationStore::new(
        issue_root.join("observations"),
    ));
    let episode_id = EpisodeId::generate();
    episodes
        .append(&episode(
            episode_id.clone(),
            Outcome::TaskFailure,
            eligible_policy(),
        ))
        .await
        .expect("应追加 Episode");
    observations
        .append(&IssueObservation::new(
            EvolutionIssueId::generate(),
            episode_id.clone(),
            &genome_digest('b'),
            failure_record(episode_id.clone(), EventId::generate()),
        ))
        .await
        .expect("应追加冲突观察");
    let expected_issue = EvolutionIssueId::generate();
    outbox
        .append(&outbox_item(
            "issue-conflict",
            episode_id.clone(),
            expected_issue.clone(),
            Outcome::TaskFailure,
            FailureDisposition::EvolutionCandidate,
        ))
        .await
        .expect("应追加 Outbox");
    assert!(matches!(
        EpisodeSelector::new(outbox, episodes, observations)
            .select()
            .await,
        Err(EpisodeSelectionError::MissingIssueObservation {
            issue_id,
            episode_id: actual,
        }) if issue_id == expected_issue && actual == episode_id
    ));
    let _ = tokio::fs::remove_dir_all(issue_root).await;
}

/// 返回供 Prompt Mutator 边界测试使用的最小脱敏证据。
fn mutation_evidence() -> MutationEvidence {
    MutationEvidence {
        issue_id: EvolutionIssueId::generate(),
        genome_digest: genome_digest('c'),
        failure_kind: FailureKind::VerificationFailure,
        root_cause_hypothesis: "任务策略缺少验证步骤".to_string(),
        expected_behavior: "任务策略应执行必要验证".to_string(),
        confidence: 0.95,
        status: DiagnosticStatus::EligibleForEvolution,
        episodes: vec![MutationEpisodeEvidence {
            outbox_id: "outbox-safe".to_string(),
            episode_id: EpisodeId::generate(),
            genome_revision_id: GenomeRevisionId::generate(),
            outcome: Outcome::TaskFailure,
            task_family: "code-edit".to_string(),
            tags: BTreeSet::from(["repair".to_string()]),
            failure: MutationFailureEvidence {
                kind: FailureKind::VerificationFailure,
                confidence: 0.95,
                rule_derived: true,
                model_assisted: false,
            },
            usage: UsageSummary::default(),
            replayability: ReplayabilityGrade::Exact,
        }],
    }
}

/// 构造一个满足协议边界的 Prompt 草案。
fn draft(index: usize) -> PromptMutationDraft {
    PromptMutationDraft {
        hypothesis: format!("候选 {index} 补充验证策略"),
        prompt: format!("执行任务并应用验证策略 {index}"),
        expected_effects: vec![ExpectedEffect {
            task_family: "code-edit".to_string(),
            expected_behavior: format!("候选 {index} 应减少验证失败"),
        }],
    }
}

/// 离线脚本生成器；用于证明 Mutator 不依赖真实模型或网络。
#[derive(Clone)]
struct ScriptedGenerator {
    drafts: Vec<PromptMutationDraft>,
}

#[async_trait]
impl PromptMutationGenerator for ScriptedGenerator {
    async fn generate(
        &self,
        request: PromptMutationRequest<'_>,
    ) -> Result<Vec<PromptMutationDraft>, PromptMutationGenerationError> {
        assert_eq!(request.candidate_count, TASK_STRATEGY_MVP_CANDIDATE_COUNT);
        assert_eq!(
            request.mutation_surface,
            MutationSurface::TaskStrategyPrompt
        );
        assert!(!request.parent_prompt.is_empty());
        assert!(!request.evidence.episodes.is_empty());
        Ok(self.drafts.clone())
    }
}

/// 离线生成器必须产生三个唯一 Prompt，并通过 CAS 形成不含正文的正式 Proposal。
#[tokio::test]
async fn generates_three_unique_bounded_mutation_proposals_offline() {
    let root = temp_root("proposals");
    let artifacts = FileArtifactStore::new(root.join("artifacts"));
    let evidence = mutation_evidence();
    let generator = ScriptedGenerator {
        drafts: vec![draft(1), draft(2), draft(3)],
    };
    let mutator = BoundedPromptMutator::task_strategy_mvp(generator);
    let mutator_revision = artifacts
        .put("application/json", br#"{"generator":"script-v1"}"#)
        .await
        .expect("应写入 Mutator 修订");
    let context = MutationProposalContext {
        parent_revision_id: evidence.episodes[0].genome_revision_id.clone(),
        parent_genome_digest: evidence.genome_digest.clone(),
        mutator_revision,
        risk: MutationRisk::Low,
    };

    let proposals = mutator
        .propose("  Parent Task Strategy  ", &evidence, &context, &artifacts)
        .await
        .expect("三个合法草案应形成 Proposal");
    assert_eq!(proposals.len(), TASK_STRATEGY_MVP_CANDIDATE_COUNT);
    let mut prompt_digests = BTreeSet::new();
    for proposal in &proposals {
        proposal.validate().expect("正式 Proposal 应合法");
        assert_eq!(proposal.surface, MutationSurface::TaskStrategyPrompt);
        assert_eq!(proposal.issue_id, evidence.issue_id);
        assert_eq!(proposal.parent_genome_digest, evidence.genome_digest);
        assert!(prompt_digests.insert(proposal.patch.task_strategy_prompt().digest.clone()));
        let body = artifacts
            .get(&proposal.patch.task_strategy_prompt().digest)
            .await
            .expect("应读取 CAS")
            .expect("Prompt 制品应存在");
        assert_ne!(body, b"Parent Task Strategy");
    }
    let encoded = serde_json::to_string(&proposals).expect("Proposal 应可序列化");
    assert!(!encoded.contains("执行任务并应用验证策略"));
    assert_eq!(mutator.policy().allowed_surfaces().len(), 1);
    assert!(mutator
        .policy()
        .allows_surface(&MutationSurface::TaskStrategyPrompt));
    assert!(!mutator
        .policy()
        .allows_surface(&MutationSurface::ProtectedPrompt));
    assert_eq!(
        mutator.policy().candidate_count(),
        TASK_STRATEGY_MVP_CANDIDATE_COUNT
    );
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// 返回指定脚本草案触发的 Mutator 错误。
async fn mutation_error(drafts: Vec<PromptMutationDraft>) -> PromptMutationError {
    BoundedPromptMutator::task_strategy_mvp(ScriptedGenerator { drafts })
        .mutate("Parent Task Strategy", &mutation_evidence())
        .await
        .expect_err("越界草案必须被拒绝")
}

/// 数量、唯一性、非空、字节上限和 Parent 不变条件都必须由生成器外强制。
#[tokio::test]
async fn rejects_unbounded_or_unchanged_prompt_drafts() {
    assert!(matches!(
        mutation_error(vec![draft(1), draft(2)]).await,
        PromptMutationError::InvalidCandidateCount {
            expected: TASK_STRATEGY_MVP_CANDIDATE_COUNT,
            actual: 2,
        }
    ));
    assert!(matches!(
        mutation_error(vec![draft(1), draft(2), draft(3), draft(4)]).await,
        PromptMutationError::InvalidCandidateCount { actual: 4, .. }
    ));

    let mut duplicate = vec![draft(1), draft(2), draft(3)];
    duplicate[2].prompt = format!("  {}  ", duplicate[1].prompt);
    assert!(matches!(
        mutation_error(duplicate).await,
        PromptMutationError::DuplicatePrompt { .. }
    ));

    let mut blank = vec![draft(1), draft(2), draft(3)];
    blank[0].prompt = " \n\t ".to_string();
    assert!(matches!(
        mutation_error(blank).await,
        PromptMutationError::EmptyPrompt { candidate: 0 }
    ));

    let mut unchanged = vec![draft(1), draft(2), draft(3)];
    unchanged[0].prompt = "  Parent Task Strategy  ".to_string();
    assert!(matches!(
        mutation_error(unchanged).await,
        PromptMutationError::UnchangedPrompt { candidate: 0 }
    ));

    let mut overlong = vec![draft(1), draft(2), draft(3)];
    overlong[0].prompt = "x".repeat(64 * 1024 + 1);
    assert!(matches!(
        mutation_error(overlong).await,
        PromptMutationError::PromptTooLong { candidate: 0, .. }
    ));

    let mut empty_hypothesis = vec![draft(1), draft(2), draft(3)];
    empty_hypothesis[0].hypothesis = "  ".to_string();
    assert!(matches!(
        mutation_error(empty_hypothesis).await,
        PromptMutationError::EmptyHypothesis { candidate: 0 }
    ));

    let mut missing_effects = vec![draft(1), draft(2), draft(3)];
    missing_effects[0].expected_effects.clear();
    assert!(matches!(
        mutation_error(missing_effects).await,
        PromptMutationError::MissingExpectedEffects { candidate: 0 }
    ));
}
