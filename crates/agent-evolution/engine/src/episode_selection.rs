//! 从只追加证据平面选择可进入 Prompt 变异的 Episode。

use crate::{
    EpisodeStore, EpisodeStoreError, EvolutionOutbox, IssueAggregator, IssueObservation,
    IssueObservationError, IssueObservationStore, OutboxError,
};
use agent_evolution_protocol::{
    default_disposition, AttributionMethod, DiagnosticStatus, EpisodeId, EvolutionIssue,
    EvolutionIssueId, FailureDisposition, FailureKind, GenomeDigest, GenomeRevisionId, Outcome,
    ReplayabilityGrade, UsageSummary,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// 一条不含事件定位与原始内容的失败摘要。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MutationFailureEvidence {
    /// 稳定失败类别。
    pub kind: FailureKind,
    /// `[0, 1]` 范围内的归因置信度。
    pub confidence: f32,
    /// 是否由确定性归因方法产生。
    pub rule_derived: bool,
    /// 是否使用了模型辅助归因。
    pub model_assisted: bool,
}

/// 一个获准 Episode 向 Mutator 暴露的脱敏结构证据。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MutationEpisodeEvidence {
    /// 触发选择的未消费 Outbox 记录。
    pub outbox_id: String,
    /// 来源 Episode 标识。
    pub episode_id: EpisodeId,
    /// 运行时固定的 Genome 修订。
    pub genome_revision_id: GenomeRevisionId,
    /// 可信运行终态。
    pub outcome: Outcome,
    /// 稳定任务族；不包含任务正文。
    pub task_family: String,
    /// 调用方声明为非敏感的稳定标签。
    pub tags: BTreeSet<String>,
    /// 与当前 Issue 对应的失败摘要。
    pub failure: MutationFailureEvidence,
    /// 客观资源用量，不包含模型请求或响应正文。
    pub usage: UsageSummary,
    /// Episode 的可回放等级。
    pub replayability: ReplayabilityGrade,
}

/// 同一 Evolution Issue 下可安全提供给 Mutator 的聚合证据。
///
/// 本类型有意不包含 Task 输入引用、事件 ID、Event payload、Session 标识、原始模型响应、
/// 原始工具结果、环境引用以及任何 Hidden Dataset 或 Verifier 信息。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MutationEvidence {
    /// 聚合后的 Evolution Issue。
    pub issue_id: EvolutionIssueId,
    /// 失败所使用的 Genome 内容摘要。
    pub genome_digest: GenomeDigest,
    /// 结构化失败类别。
    pub failure_kind: FailureKind,
    /// 不含事件 ID 和原始内容的根因假设。
    pub root_cause_hypothesis: String,
    /// 本轮变异应实现的行为结果。
    pub expected_behavior: String,
    /// 聚合证据的最高置信度。
    pub confidence: f32,
    /// 当前可信 Issue 状态。
    pub status: DiagnosticStatus,
    /// 通过全部资格与绑定校验的 Episode；未获资格的 Episode 不会出现在此处。
    pub episodes: Vec<MutationEpisodeEvidence>,
}

/// 从 Outbox、Episode Store 与 Issue Observation Store 交叉选择变异证据。
pub struct EpisodeSelector<O, E, I>
where
    O: EvolutionOutbox,
    E: EpisodeStore,
    I: IssueObservationStore,
{
    outbox: Arc<O>,
    episodes: Arc<E>,
    observations: Arc<I>,
}

impl<O, E, I> EpisodeSelector<O, E, I>
where
    O: EvolutionOutbox,
    E: EpisodeStore,
    I: IssueObservationStore,
{
    /// 创建一个只读 Selector；成功选择不会提前消费 Outbox。
    pub fn new(outbox: Arc<O>, episodes: Arc<E>, observations: Arc<I>) -> Self {
        Self {
            outbox,
            episodes,
            observations,
        }
    }

    /// 选择全部当前可变异的脱敏证据，并按 Issue ID 稳定排序。
    ///
    /// 普通不合资格记录会被忽略；一旦已进入候选范围的记录出现 Episode、Outcome、Issue
    /// 或只追加观察绑定冲突，整个选择操作会失败关闭。该方法不标记 Outbox 已消费，消费
    /// 应由后续持久化 Cycle 状态机在候选制品落盘后完成。
    ///
    /// # Errors
    ///
    /// 存储读取失败、候选记录结构不完整、Episode 损坏或证据绑定不一致时返回
    /// [`EpisodeSelectionError`]。
    pub async fn select(&self) -> Result<Vec<MutationEvidence>, EpisodeSelectionError> {
        let pending = self.outbox.pending().await?;
        let observations = self.observations.all().await?;
        for observation in &observations {
            observation.validate()?;
        }
        validate_observation_bindings(&observations)?;

        let mut grouped = BTreeMap::<String, MutationEvidence>::new();
        let mut seen_episodes = BTreeSet::<(String, String)>::new();

        for item in pending {
            if item.disposition != FailureDisposition::EvolutionCandidate
                || !is_trusted_behavior_failure(&item.outcome)
                || !is_eligible_issue_status(item.issue_status)
            {
                continue;
            }
            if item.consumed {
                return Err(EpisodeSelectionError::ConsumedOutboxItem(item.outbox_id));
            }
            let issue_id = item
                .issue_id
                .clone()
                .ok_or_else(|| EpisodeSelectionError::MissingIssueId(item.outbox_id.clone()))?;
            let episode =
                self.episodes.get(&item.episode_id).await?.ok_or_else(|| {
                    EpisodeSelectionError::MissingEpisode(item.episode_id.clone())
                })?;
            episode
                .validate()
                .map_err(|error| EpisodeSelectionError::InvalidEpisode {
                    episode_id: item.episode_id.clone(),
                    reason: error.to_string(),
                })?;
            if episode.episode_id != item.episode_id {
                return Err(EpisodeSelectionError::EpisodeBindingMismatch {
                    expected: item.episode_id,
                    actual: episode.episode_id,
                });
            }
            if episode.outcome.as_ref() != Some(&item.outcome) {
                return Err(EpisodeSelectionError::OutcomeMismatch {
                    episode_id: episode.episode_id,
                    outbox: item.outcome,
                    episode: episode.outcome,
                });
            }
            if !episode.data_policy.permits_mutation_input() {
                continue;
            }

            let (issue, observation) = rebuild_issue(
                &observations,
                &issue_id,
                &episode.episode_id,
                item.issue_status,
            )?;
            let issue_key = issue_id.as_str().to_string();
            let episode_key = episode.episode_id.as_str().to_string();
            if !seen_episodes.insert((issue_key.clone(), episode_key)) {
                return Err(EpisodeSelectionError::DuplicateEligibleEpisode {
                    issue_id,
                    episode_id: episode.episode_id,
                });
            }

            let candidate = mutation_evidence(
                item.outbox_id,
                item.issue_status,
                episode,
                &issue,
                observation,
            );
            match grouped.get_mut(&issue_key) {
                Some(existing) => merge_evidence(existing, candidate)?,
                None => {
                    grouped.insert(issue_key, candidate);
                }
            }
        }

        Ok(grouped.into_values().collect())
    }
}

/// Selector 读取或校验证据时的失败。
#[derive(Debug, thiserror::Error)]
pub enum EpisodeSelectionError {
    /// Evolution Outbox 无法读取。
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    /// Episode Store 无法读取。
    #[error(transparent)]
    EpisodeStore(#[from] EpisodeStoreError),
    /// Issue Observation Store 无法读取或记录不合法。
    #[error(transparent)]
    IssueObservation(#[from] IssueObservationError),
    /// `pending` 违反接口契约返回了已消费记录。
    #[error("Outbox pending 返回了已消费记录：{0}")]
    ConsumedOutboxItem(String),
    /// 候选记录没有绑定 Issue。
    #[error("EvolutionCandidate Outbox 未绑定 Issue：{0}")]
    MissingIssueId(String),
    /// Outbox 指向不存在的 Episode。
    #[error("Outbox 指向的 Episode 不存在：{0}")]
    MissingEpisode(EpisodeId),
    /// Store 返回了与查询键不同的 Episode。
    #[error("Episode 绑定不一致：期望 {expected}，实际 {actual}")]
    EpisodeBindingMismatch {
        /// Outbox 声明的 Episode。
        expected: EpisodeId,
        /// Store 实际返回的 Episode。
        actual: EpisodeId,
    },
    /// Episode Header 本身不合法。
    #[error("Episode {episode_id} 不合法：{reason}")]
    InvalidEpisode {
        /// 不合法的 Episode。
        episode_id: EpisodeId,
        /// 稳定校验原因。
        reason: String,
    },
    /// Outbox 与 Episode 声明的可信终态不同。
    #[error("Episode {episode_id} 的 Outcome 绑定不一致：Outbox={outbox:?}，Episode={episode:?}")]
    OutcomeMismatch {
        /// 发生冲突的 Episode。
        episode_id: EpisodeId,
        /// Outbox 声明的终态。
        outbox: Outcome,
        /// Episode Header 声明的终态。
        episode: Option<Outcome>,
    },
    /// Issue Observation 没有提供当前 Episode 的绑定证据。
    #[error("Issue {issue_id} 缺少 Episode {episode_id} 的只追加观察")]
    MissingIssueObservation {
        /// Outbox 声明的 Issue。
        issue_id: EvolutionIssueId,
        /// 来源 Episode。
        episode_id: EpisodeId,
    },
    /// 同一 Issue ID 被绑定到不一致的失败指纹或重建结果。
    #[error("Issue {issue_id} 的只追加观察绑定冲突：{reason}")]
    IssueBindingConflict {
        /// 发生冲突的 Issue。
        issue_id: EvolutionIssueId,
        /// 不含原始证据内容的稳定原因。
        reason: &'static str,
    },
    /// Outbox 声明的 Issue 状态无法由只追加观察交叉验证。
    #[error("Issue {issue_id} 状态绑定不一致：Outbox={outbox:?}，重建={rebuilt:?}")]
    IssueStatusMismatch {
        /// 发生冲突的 Issue。
        issue_id: EvolutionIssueId,
        /// Outbox 声明的状态。
        outbox: DiagnosticStatus,
        /// 由观察重建的状态。
        rebuilt: DiagnosticStatus,
    },
    /// 同一 Issue/Episode 被多个待处理候选重复引用。
    #[error("Issue {issue_id} 重复引用 Episode {episode_id}")]
    DuplicateEligibleEpisode {
        /// 发生冲突的 Issue。
        issue_id: EvolutionIssueId,
        /// 被重复引用的 Episode。
        episode_id: EpisodeId,
    },
}

/// 只接受可归因于任务行为且适合修复的可信失败终态。
fn is_trusted_behavior_failure(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::TaskFailure | Outcome::BudgetFailure)
}

/// 只接受已经聚合、确认、显式获准或再次回归的 Issue。
fn is_eligible_issue_status(status: DiagnosticStatus) -> bool {
    matches!(
        status,
        DiagnosticStatus::Confirmed
            | DiagnosticStatus::Clustered
            | DiagnosticStatus::EligibleForEvolution
            | DiagnosticStatus::Regressed
    )
}

/// 校验 Issue ID 与稳定失败指纹在整份只追加日志中保持一一对应。
fn validate_observation_bindings(
    observations: &[IssueObservation],
) -> Result<(), EpisodeSelectionError> {
    let mut issue_to_fingerprint = BTreeMap::<String, String>::new();
    let mut fingerprint_to_issue = BTreeMap::<String, EvolutionIssueId>::new();
    for observation in observations {
        let issue_key = observation.issue_id.as_str().to_string();
        let fingerprint_key = observation.fingerprint.stable_key();
        if issue_to_fingerprint
            .insert(issue_key, fingerprint_key.clone())
            .is_some_and(|existing| existing != fingerprint_key)
        {
            return Err(EpisodeSelectionError::IssueBindingConflict {
                issue_id: observation.issue_id.clone(),
                reason: "同一 Issue ID 绑定了不同失败指纹",
            });
        }
        if fingerprint_to_issue
            .insert(fingerprint_key, observation.issue_id.clone())
            .is_some_and(|existing| existing != observation.issue_id)
        {
            return Err(EpisodeSelectionError::IssueBindingConflict {
                issue_id: observation.issue_id.clone(),
                reason: "同一失败指纹绑定了不同 Issue ID",
            });
        }
    }
    Ok(())
}

/// 从只追加观察重建 Issue，并核对 Outbox 的 Issue/Episode/状态绑定。
fn rebuild_issue<'a>(
    observations: &'a [IssueObservation],
    issue_id: &EvolutionIssueId,
    episode_id: &EpisodeId,
    outbox_status: DiagnosticStatus,
) -> Result<(EvolutionIssue, &'a IssueObservation), EpisodeSelectionError> {
    let matching = observations
        .iter()
        .filter(|observation| &observation.issue_id == issue_id)
        .collect::<Vec<_>>();
    let target = matching
        .iter()
        .copied()
        .find(|observation| &observation.episode_id == episode_id)
        .ok_or_else(|| EpisodeSelectionError::MissingIssueObservation {
            issue_id: issue_id.clone(),
            episode_id: episode_id.clone(),
        })?;
    let fingerprint = &target.fingerprint;
    if matching
        .iter()
        .any(|observation| &observation.fingerprint != fingerprint)
    {
        return Err(EpisodeSelectionError::IssueBindingConflict {
            issue_id: issue_id.clone(),
            reason: "同一 Issue ID 绑定了不同失败指纹",
        });
    }

    let mut unique_observations = BTreeSet::new();
    let mut aggregator = IssueAggregator::new();
    for observation in &matching {
        if !unique_observations.insert(observation.observation_id()) {
            return Err(EpisodeSelectionError::IssueBindingConflict {
                issue_id: issue_id.clone(),
                reason: "只追加观察键重复",
            });
        }
        aggregator.record_with_issue_id(
            &observation.record,
            &observation.episode_id,
            &observation.fingerprint.genome_digest,
            Some(observation.issue_id.clone()),
        );
    }
    let rebuilt = aggregator.issues();
    if rebuilt.len() != 1 || rebuilt[0].issue_id != *issue_id {
        return Err(EpisodeSelectionError::IssueBindingConflict {
            issue_id: issue_id.clone(),
            reason: "只追加观察无法唯一重建 Issue",
        });
    }
    let issue = rebuilt[0].clone();
    if default_disposition(
        issue.fingerprint.failure_class,
        issue.evidence_episode_ids.len(),
    ) != FailureDisposition::EvolutionCandidate
    {
        return Err(EpisodeSelectionError::IssueBindingConflict {
            issue_id: issue_id.clone(),
            reason: "只追加观察无法重建 EvolutionCandidate 处置",
        });
    }
    if !status_is_compatible(outbox_status, issue.status, &matching) {
        return Err(EpisodeSelectionError::IssueStatusMismatch {
            issue_id: issue_id.clone(),
            outbox: outbox_status,
            rebuilt: issue.status,
        });
    }
    Ok((issue, target))
}

/// 允许外部诊断状态在重建出的聚合基础上单向收窄或晋级。
fn status_is_compatible(
    outbox: DiagnosticStatus,
    rebuilt: DiagnosticStatus,
    observations: &[&IssueObservation],
) -> bool {
    match outbox {
        DiagnosticStatus::Clustered => rebuilt == DiagnosticStatus::Clustered,
        DiagnosticStatus::EligibleForEvolution => matches!(
            rebuilt,
            DiagnosticStatus::Clustered | DiagnosticStatus::EligibleForEvolution
        ),
        DiagnosticStatus::Confirmed => observations
            .iter()
            .any(|observation| observation.record.status == DiagnosticStatus::Confirmed),
        DiagnosticStatus::Regressed => observations
            .iter()
            .any(|observation| observation.record.status == DiagnosticStatus::Regressed),
        _ => false,
    }
}

/// 把已交叉验证的内部证据投影为不含原始内容的 Mutator 视图。
fn mutation_evidence(
    outbox_id: String,
    status: DiagnosticStatus,
    episode: agent_evolution_protocol::Episode,
    issue: &EvolutionIssue,
    observation: &IssueObservation,
) -> MutationEvidence {
    let kind = observation.record.attribution.failure_class;
    let method = observation.record.attribution.method;
    MutationEvidence {
        issue_id: issue.issue_id.clone(),
        genome_digest: issue.fingerprint.genome_digest.clone(),
        failure_kind: kind,
        root_cause_hypothesis: redacted_hypothesis(kind),
        expected_behavior: expected_behavior(kind),
        confidence: issue.confidence,
        status,
        episodes: vec![MutationEpisodeEvidence {
            outbox_id,
            episode_id: episode.episode_id,
            genome_revision_id: episode.genome_revision_id,
            outcome: episode.outcome.expect("前置校验已确认 Outcome 存在"),
            task_family: episode.task.family,
            tags: episode.task.tags,
            failure: MutationFailureEvidence {
                kind,
                confidence: observation.record.attribution.confidence,
                rule_derived: method.is_deterministic(),
                model_assisted: method == AttributionMethod::ModelAssisted,
            },
            usage: episode.usage,
            replayability: episode.replayability,
        }],
    }
}

/// 合并同一 Issue 的不同获准 Episode，拒绝跨指纹或跨失败类别聚合。
fn merge_evidence(
    existing: &mut MutationEvidence,
    incoming: MutationEvidence,
) -> Result<(), EpisodeSelectionError> {
    if existing.issue_id != incoming.issue_id
        || existing.genome_digest != incoming.genome_digest
        || existing.failure_kind != incoming.failure_kind
    {
        return Err(EpisodeSelectionError::IssueBindingConflict {
            issue_id: existing.issue_id.clone(),
            reason: "待变异证据的 Issue 摘要不一致",
        });
    }
    existing.confidence = existing.confidence.max(incoming.confidence);
    existing.status = existing.status.max(incoming.status);
    existing.episodes.extend(incoming.episodes);
    Ok(())
}

/// 只由失败类别生成结构化假设，避免把事件 ID 或归一化错误文本暴露给 Mutator。
fn redacted_hypothesis(kind: FailureKind) -> String {
    format!("任务策略可能导致 {kind:?} 类失败")
}

/// 返回不依赖用户内容、事件或工具结果的目标行为。
fn expected_behavior(kind: FailureKind) -> String {
    match kind {
        FailureKind::ContextLoss => "任务策略应保留完成任务所需的关键上下文",
        FailureKind::PlanningFailure => "任务策略应形成可执行且可验证的计划",
        FailureKind::ToolSelection => "任务策略应选择满足任务契约的工具",
        FailureKind::ToolArgument => "任务策略应生成符合工具契约的参数",
        FailureKind::ToolExecution => "任务策略应处理工具成功结果与可恢复错误",
        FailureKind::ModelFailure => "任务策略应处理协议合规的模型交互",
        FailureKind::VerificationFailure => "任务策略应执行并检查必要验证",
        FailureKind::TerminationFailure => "任务策略应在资源预算内完成并正常终止",
        FailureKind::PermissionFailure => "任务策略不得请求超出授权范围的能力",
        FailureKind::SandboxFailure => "任务策略应遵守隔离环境约束",
        FailureKind::PluginFailure => "任务策略应按插件契约处理调用结果",
        FailureKind::RuntimeFailure => "任务策略应避免依赖不可用的运行时行为",
        FailureKind::EnvironmentFailure => "任务策略应识别并报告不可用的外部环境",
        FailureKind::Unknown => "任务策略应满足任务契约并保留可验证证据",
    }
    .into()
}
