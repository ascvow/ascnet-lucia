//! 延迟反馈到 Outcome 修订的可信应用服务。
//!
//! 该模块只接受应用层已经认证来源的 [`FeedbackEvent`]。它校验 Episode/Run 绑定，
//! 从 Episode 的监督 CAS 恢复初始修订，并以只追加方式保存后续终态；普通 Agent、模型和
//! 插件不会因为构造同形 JSON 就获得调用该服务的权限。

use crate::{
    ArtifactStore, ArtifactStoreError, EpisodeStore, EpisodeStoreError, OutcomeRevisionError,
    OutcomeRevisionStore,
};
use agent_evolution_protocol::{
    ArtifactDigest, Episode, EpisodeId, FeedbackEvent, FeedbackId, FeedbackSignal, FeedbackSource,
    Outcome, OutcomeRevision, OutcomeRevisionId, OutcomeSource, RunId,
};
use std::sync::Arc;
use thiserror::Error;

/// 把可信延迟反馈应用到既有 Episode 的 Outcome 历史。
pub struct FeedbackProcessor<A, E, R>
where
    A: ArtifactStore,
    E: EpisodeStore,
    R: OutcomeRevisionStore,
{
    artifacts: Arc<A>,
    episodes: Arc<E>,
    revisions: Arc<R>,
}

impl<A, E, R> FeedbackProcessor<A, E, R>
where
    A: ArtifactStore,
    E: EpisodeStore,
    R: OutcomeRevisionStore,
{
    /// 使用 Evidence Plane 的 CAS、Episode Store 与 Outcome Revision Store 创建处理器。
    pub fn new(artifacts: Arc<A>, episodes: Arc<E>, revisions: Arc<R>) -> Self {
        Self {
            artifacts,
            episodes,
            revisions,
        }
    }

    /// 校验并应用一条由可信控制器提交的延迟反馈。
    ///
    /// `authenticated_source` 必须来自 Host 身份、CI 适配器或 Canary 控制器等调用上下文，
    /// 不能直接复制 `feedback.source`；两者不一致时不会产生任何修订。
    ///
    /// 相同 `feedback_id` 与相同内容重复提交时返回第一次生成的修订，不重复追加。不同内容
    /// 复用同一 ID、Run 绑定错误、非决定性 Note、未知来源或较弱证据覆盖较强终态时拒绝。
    ///
    /// # Errors
    ///
    /// Episode 或监督制品缺失、绑定或内容不一致、反馈不能决定终态、并发写入冲突，以及
    /// CAS、Episode Store、Outcome Revision Store 读写失败时返回 [`FeedbackError`]。
    pub async fn apply(
        &self,
        authenticated_source: FeedbackSource,
        feedback: FeedbackEvent,
    ) -> Result<OutcomeRevision, FeedbackError> {
        feedback
            .validate()
            .map_err(|error| FeedbackError::InvalidFeedback(error.to_string()))?;
        let outcome = decisive_outcome(&feedback)?;
        let source = trusted_outcome_source(authenticated_source, &feedback)?;

        let episode = self
            .episodes
            .get(&feedback.related_episode_id)
            .await
            .map_err(FeedbackError::Episode)?
            .ok_or_else(|| FeedbackError::EpisodeNotFound(feedback.related_episode_id.clone()))?;
        if episode.run_id != feedback.related_run_id {
            return Err(FeedbackError::RunMismatch {
                episode_id: episode.episode_id,
                expected: episode.run_id,
                actual: feedback.related_run_id,
            });
        }
        self.verify_feedback_evidence(&feedback).await?;

        let initial = self.load_initial_revision(&episode).await?;
        let mut history = self.revisions.history(&episode.episode_id).await?;
        if history.is_empty() {
            match self.revisions.append(&initial).await {
                Ok(()) => history.push(initial.clone()),
                Err(OutcomeRevisionError::ConcurrentUpdate(_))
                | Err(OutcomeRevisionError::AlreadyExists(_)) => {
                    history = self.revisions.history(&episode.episode_id).await?;
                }
                Err(error) => return Err(FeedbackError::Revision(error)),
            }
        }
        verify_initial_history(&episode.episode_id, &initial, &history)?;

        if let Some(existing) = find_feedback(&history, &feedback.feedback_id) {
            return if existing.feedback.as_ref() == Some(&feedback) {
                Ok(existing.clone())
            } else {
                Err(FeedbackError::FeedbackIdConflict(
                    feedback.feedback_id.clone(),
                ))
            };
        }

        let current = history
            .last()
            .expect("初始 Outcome 修订已校验并写入，历史必定非空");
        if current.outcome != Outcome::Unverifiable
            && source.trust_priority() < current.source.trust_priority()
        {
            return Err(FeedbackError::WeakerSource {
                feedback_id: feedback.feedback_id,
                incoming: source,
                current: current.source,
            });
        }

        let revision = OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: episode.episode_id.clone(),
            supersedes: Some(current.revision_id.clone()),
            outcome,
            source,
            reason: feedback_reason(&feedback),
            feedback: Some(feedback.clone()),
        };
        match self.revisions.append(&revision).await {
            Ok(()) => Ok(revision),
            Err(OutcomeRevisionError::ConcurrentUpdate(_))
            | Err(OutcomeRevisionError::StaleSupersedes { .. }) => {
                let concurrent = self.revisions.history(&episode.episode_id).await?;
                match find_feedback(&concurrent, &feedback.feedback_id) {
                    Some(existing) if existing.feedback.as_ref() == Some(&feedback) => {
                        Ok(existing.clone())
                    }
                    Some(_) => Err(FeedbackError::FeedbackIdConflict(feedback.feedback_id)),
                    None => Err(FeedbackError::ConcurrentFeedback(episode.episode_id)),
                }
            }
            Err(error) => Err(FeedbackError::Revision(error)),
        }
    }

    /// 验证反馈引用的脱敏证据确实存在于可信 CAS，且长度元数据没有被伪造。
    async fn verify_feedback_evidence(
        &self,
        feedback: &FeedbackEvent,
    ) -> Result<(), FeedbackError> {
        let Some(reference) = &feedback.evidence else {
            return Ok(());
        };
        let bytes = self
            .artifacts
            .get(&reference.digest)
            .await?
            .ok_or_else(|| FeedbackError::ArtifactNotFound(reference.digest.clone()))?;
        if bytes.len() as u64 != reference.size_bytes {
            return Err(FeedbackError::ArtifactSizeMismatch {
                digest: reference.digest.clone(),
                expected: reference.size_bytes,
                actual: bytes.len() as u64,
            });
        }
        Ok(())
    }

    /// 从 Episode Header 绑定的监督制品中恢复 Recorder 生成的初始 Outcome 修订。
    async fn load_initial_revision(
        &self,
        episode: &Episode,
    ) -> Result<OutcomeRevision, FeedbackError> {
        let supervision = episode
            .supervision
            .as_ref()
            .ok_or_else(|| FeedbackError::MissingSupervision(episode.episode_id.clone()))?;
        let reference = supervision
            .outcome_revision_ref
            .as_ref()
            .ok_or_else(|| FeedbackError::MissingInitialRevision(episode.episode_id.clone()))?;
        if reference.media_type != "application/json" {
            return Err(FeedbackError::InvalidInitialMediaType {
                episode_id: episode.episode_id.clone(),
                actual: reference.media_type.clone(),
            });
        }
        let bytes = self
            .artifacts
            .get(&reference.digest)
            .await?
            .ok_or_else(|| FeedbackError::ArtifactNotFound(reference.digest.clone()))?;
        if bytes.len() as u64 != reference.size_bytes {
            return Err(FeedbackError::ArtifactSizeMismatch {
                digest: reference.digest.clone(),
                expected: reference.size_bytes,
                actual: bytes.len() as u64,
            });
        }
        let revision: OutcomeRevision = serde_json::from_slice(&bytes).map_err(|source| {
            FeedbackError::InvalidInitialRevision {
                episode_id: episode.episode_id.clone(),
                source,
            }
        })?;
        revision
            .validate()
            .map_err(|error| FeedbackError::InvalidFeedback(error.to_string()))?;
        if revision.episode_id != episode.episode_id
            || revision.supersedes.is_some()
            || revision.feedback.is_some()
            || episode.outcome.as_ref() != Some(&revision.outcome)
        {
            return Err(FeedbackError::InitialRevisionMismatch(
                episode.episode_id.clone(),
            ));
        }
        Ok(revision)
    }
}

/// 把决定性反馈信号映射为 Episode 终态；Note 只用于审计，不能改变 Outcome。
fn decisive_outcome(feedback: &FeedbackEvent) -> Result<Outcome, FeedbackError> {
    match &feedback.signal {
        FeedbackSignal::ConfirmedSuccess => Ok(Outcome::Success),
        FeedbackSignal::ConfirmedFailure
        | FeedbackSignal::PartialFailure
        | FeedbackSignal::ConstraintViolation => Ok(Outcome::TaskFailure),
        FeedbackSignal::Note(_) => Err(FeedbackError::NonDecisiveSignal(
            feedback.feedback_id.clone(),
        )),
    }
}

/// 只允许具备明确可信边界的来源决定终态。
fn trusted_outcome_source(
    authenticated_source: FeedbackSource,
    feedback: &FeedbackEvent,
) -> Result<OutcomeSource, FeedbackError> {
    if authenticated_source != feedback.source {
        return Err(FeedbackError::SourceAuthenticationMismatch {
            feedback_id: feedback.feedback_id.clone(),
            authenticated: authenticated_source,
            claimed: feedback.source,
        });
    }
    match authenticated_source {
        FeedbackSource::User | FeedbackSource::DeterministicCheck | FeedbackSource::Canary => {
            Ok(feedback.source.outcome_source())
        }
        FeedbackSource::Other => Err(FeedbackError::UntrustedSource(feedback.feedback_id.clone())),
    }
}

/// 检查本地修订历史确实以 Episode 监督 CAS 中的初始修订开头。
fn verify_initial_history(
    episode_id: &EpisodeId,
    initial: &OutcomeRevision,
    history: &[OutcomeRevision],
) -> Result<(), FeedbackError> {
    if history.first() != Some(initial) {
        return Err(FeedbackError::InitialRevisionMismatch(episode_id.clone()));
    }
    Ok(())
}

/// 按强类型反馈 ID 在历史中寻找已经应用的反馈。
fn find_feedback<'a>(
    history: &'a [OutcomeRevision],
    feedback_id: &FeedbackId,
) -> Option<&'a OutcomeRevision> {
    history.iter().find(|revision| {
        revision
            .feedback
            .as_ref()
            .is_some_and(|feedback| &feedback.feedback_id == feedback_id)
    })
}

/// 生成不包含用户自由文本的稳定审计理由。
fn feedback_reason(feedback: &FeedbackEvent) -> String {
    let signal = match &feedback.signal {
        FeedbackSignal::ConfirmedSuccess => "明确确认任务成功",
        FeedbackSignal::ConfirmedFailure => "明确确认任务失败",
        FeedbackSignal::PartialFailure => "确认任务存在部分失败",
        FeedbackSignal::ConstraintViolation => "确认任务违反关键约束",
        FeedbackSignal::Note(_) => "补充非决定性说明",
    };
    format!("延迟反馈 {}：{signal}", feedback.feedback_id)
}

/// 延迟反馈应用错误。
#[derive(Debug, Error)]
pub enum FeedbackError {
    /// FeedbackEvent 自身不满足协议约束。
    #[error("延迟反馈不合法：{0}")]
    InvalidFeedback(String),
    /// 反馈指向的 Episode 不存在。
    #[error("延迟反馈指向的 Episode 不存在：{0}")]
    EpisodeNotFound(EpisodeId),
    /// FeedbackEvent 的 Run 与 Episode 的可信绑定不一致。
    #[error("Episode {episode_id} 绑定 Run {expected}，反馈却指向 {actual}")]
    RunMismatch {
        /// 被修订的 Episode。
        episode_id: EpisodeId,
        /// Episode 绑定的可信 Run。
        expected: RunId,
        /// FeedbackEvent 声明的 Run。
        actual: RunId,
    },
    /// Episode 没有可信监督制品引用。
    #[error("Episode {0} 缺少监督制品，不能应用延迟反馈")]
    MissingSupervision(EpisodeId),
    /// Episode 没有 Recorder 生成的初始 Outcome 修订。
    #[error("Episode {0} 缺少初始 Outcome 修订制品")]
    MissingInitialRevision(EpisodeId),
    /// 初始 Outcome 修订不是 JSON 制品。
    #[error("Episode {episode_id} 的初始 Outcome 修订媒体类型非法：{actual}")]
    InvalidInitialMediaType {
        /// 被修订的 Episode。
        episode_id: EpisodeId,
        /// 实际媒体类型。
        actual: String,
    },
    /// 引用的 CAS 制品不存在。
    #[error("延迟反馈引用的 CAS 制品不存在：{0}")]
    ArtifactNotFound(ArtifactDigest),
    /// CAS 引用声明的长度与实际内容不一致。
    #[error("CAS 制品 {digest} 长度不一致：期望 {expected}，实际 {actual}")]
    ArtifactSizeMismatch {
        /// 制品摘要。
        digest: ArtifactDigest,
        /// 引用声明长度。
        expected: u64,
        /// 实际读取长度。
        actual: u64,
    },
    /// 初始 Outcome 修订 JSON 无法解析。
    #[error("Episode {episode_id} 的初始 Outcome 修订损坏：{source}")]
    InvalidInitialRevision {
        /// 被修订的 Episode。
        episode_id: EpisodeId,
        /// JSON 解析错误。
        #[source]
        source: serde_json::Error,
    },
    /// 本地修订历史与 Episode 监督 CAS 中的初始修订不一致。
    #[error("Episode {0} 的 Outcome 修订历史与监督制品不一致")]
    InitialRevisionMismatch(EpisodeId),
    /// Note 没有足够语义改变 Outcome。
    #[error("延迟反馈 {0} 不是可决定 Outcome 的信号")]
    NonDecisiveSignal(FeedbackId),
    /// 未知来源不能改变 Outcome。
    #[error("延迟反馈 {0} 的来源未经认证，不能改变 Outcome")]
    UntrustedSource(FeedbackId),
    /// FeedbackEvent 声明来源与可信调用上下文不一致。
    #[error("延迟反馈 {feedback_id} 声明来源 {claimed:?}，可信调用上下文实际为 {authenticated:?}")]
    SourceAuthenticationMismatch {
        /// 被拒绝的反馈。
        feedback_id: FeedbackId,
        /// Host 或受信适配器认证出的来源。
        authenticated: FeedbackSource,
        /// FeedbackEvent 自行声明的来源。
        claimed: FeedbackSource,
    },
    /// 同一反馈 ID 被不同内容复用。
    #[error("延迟反馈 ID 被不同内容复用：{0}")]
    FeedbackIdConflict(FeedbackId),
    /// 较弱来源不能覆盖已经确定的较强终态。
    #[error("延迟反馈 {feedback_id} 的来源 {incoming:?} 不能覆盖 {current:?} 终态")]
    WeakerSource {
        /// 被拒绝的反馈。
        feedback_id: FeedbackId,
        /// 新反馈来源。
        incoming: OutcomeSource,
        /// 当前修订来源。
        current: OutcomeSource,
    },
    /// 其他反馈抢先提交，调用方需要重新读取并决定是否重试。
    #[error("Episode {0} 的 Outcome 在应用反馈期间发生并发变化")]
    ConcurrentFeedback(EpisodeId),
    /// Episode Store 访问失败。
    #[error(transparent)]
    Episode(EpisodeStoreError),
    /// Artifact CAS 访问失败。
    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),
    /// Outcome Revision Store 访问失败。
    #[error(transparent)]
    Revision(#[from] OutcomeRevisionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ArtifactStore, EpisodeRecorder, EpisodeRecorderConfig, EpisodeStore, FileArtifactStore,
        FileEpisodeStore, FileOutcomeRevisionStore,
    };
    use agent_core::{AgentEvent, AgentEventKind, EventSink};
    use agent_evolution_protocol::{FeedbackId, GenomeRevisionId};
    use serde_json::json;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-feedback-{}", Uuid::new_v4().simple()))
    }

    /// 用真实 Recorder 生成包含初始 Outcome 修订 CAS 的不可验证 Episode。
    async fn recorded_episode(
        root: &std::path::Path,
    ) -> (Arc<FileArtifactStore>, Arc<FileEpisodeStore>, Episode) {
        let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let config = EpisodeRecorderConfig::online("session-1", GenomeRevisionId::generate());
        let episode_id = config.episode_id.clone();
        let run_id = config.run_id.to_string();
        let recorder = EpisodeRecorder::new(config, artifacts.clone(), episodes.clone());
        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .await
            .expect("应记录运行开始");
        recorder
            .record(&AgentEvent::new(
                &run_id,
                AgentEventKind::RunFinished,
                1,
                json!({"steps_used": 1}),
            ))
            .await
            .expect("应收敛运行");
        let episode = episodes
            .get(&episode_id)
            .await
            .expect("应读取 Episode")
            .expect("Episode 应存在");
        (artifacts, episodes, episode)
    }

    fn failure_feedback(episode: &Episode) -> FeedbackEvent {
        FeedbackEvent {
            feedback_id: FeedbackId::generate(),
            source: FeedbackSource::User,
            related_episode_id: episode.episode_id.clone(),
            related_run_id: episode.run_id.clone(),
            signal: FeedbackSignal::ConfirmedFailure,
            evidence: None,
        }
    }

    /// 场景 D：延迟纠正从真实 Episode 恢复初始修订，追加失败且不覆盖历史。
    #[tokio::test]
    async fn delayed_feedback_appends_failure_and_preserves_history() {
        let root = temp_root();
        let (artifacts, episodes, episode) = recorded_episode(&root).await;
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let processor = FeedbackProcessor::new(artifacts, episodes.clone(), revisions.clone());
        let feedback = failure_feedback(&episode);

        let applied = processor
            .apply(FeedbackSource::User, feedback.clone())
            .await
            .expect("延迟反馈应被应用");
        assert_eq!(applied.outcome, Outcome::TaskFailure);
        assert_eq!(applied.source, OutcomeSource::UserFeedback);
        assert_eq!(applied.feedback, Some(feedback.clone()));

        let history = revisions
            .history(&episode.episode_id)
            .await
            .expect("应读取完整修订历史");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].outcome, Outcome::Unverifiable);
        assert_eq!(history[1].supersedes, Some(history[0].revision_id.clone()));
        assert_eq!(history[1], applied);
        assert_eq!(
            episodes
                .get(&episode.episode_id)
                .await
                .expect("应读取原 Episode")
                .expect("原 Episode 应存在")
                .outcome,
            Some(Outcome::Unverifiable),
            "Episode Header 不得被延迟反馈覆盖"
        );

        let repeated = processor
            .apply(FeedbackSource::User, feedback)
            .await
            .expect("同一反馈重放应幂等");
        assert_eq!(repeated, applied);
        assert_eq!(
            revisions
                .history(&episode.episode_id)
                .await
                .expect("应读取历史")
                .len(),
            2
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rejects_mismatched_run_before_writing_history() {
        let root = temp_root();
        let (artifacts, episodes, episode) = recorded_episode(&root).await;
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let processor = FeedbackProcessor::new(artifacts, episodes, revisions.clone());
        let mut feedback = failure_feedback(&episode);
        feedback.related_run_id = RunId::generate();

        assert!(matches!(
            processor.apply(FeedbackSource::User, feedback).await,
            Err(FeedbackError::RunMismatch { .. })
        ));
        assert!(revisions
            .history(&episode.episode_id)
            .await
            .expect("应读取历史")
            .is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn weaker_feedback_cannot_override_trusted_verifier() {
        let root = temp_root();
        let (artifacts, episodes, episode) = recorded_episode(&root).await;
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let processor = FeedbackProcessor::new(artifacts, episodes, revisions.clone());

        let first_feedback = failure_feedback(&episode);
        let first = processor
            .apply(FeedbackSource::User, first_feedback)
            .await
            .expect("应先恢复并追加修订");
        let verified = OutcomeRevision {
            revision_id: OutcomeRevisionId::generate(),
            episode_id: episode.episode_id.clone(),
            supersedes: Some(first.revision_id),
            outcome: Outcome::Success,
            source: OutcomeSource::TrustedVerifier,
            reason: "确定性 Verifier 后续确认成功".into(),
            feedback: None,
        };
        revisions
            .append(&verified)
            .await
            .expect("应追加 Verifier 修订");

        assert!(matches!(
            processor
                .apply(FeedbackSource::User, failure_feedback(&episode))
                .await,
            Err(FeedbackError::WeakerSource {
                current: OutcomeSource::TrustedVerifier,
                ..
            })
        ));
        let history = revisions
            .history(&episode.episode_id)
            .await
            .expect("应读取历史");
        assert_eq!(history.len(), 3);
        assert_eq!(history.last(), Some(&verified));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rejects_non_decisive_or_unknown_feedback_without_writes() {
        let root = temp_root();
        let (artifacts, episodes, episode) = recorded_episode(&root).await;
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let processor = FeedbackProcessor::new(artifacts, episodes, revisions.clone());
        let mut feedback = failure_feedback(&episode);
        feedback.signal = FeedbackSignal::Note("仅补充观察".into());
        assert!(matches!(
            processor.apply(FeedbackSource::User, feedback).await,
            Err(FeedbackError::NonDecisiveSignal(_))
        ));

        let mut feedback = failure_feedback(&episode);
        feedback.source = FeedbackSource::Other;
        assert!(matches!(
            processor.apply(FeedbackSource::Other, feedback).await,
            Err(FeedbackError::UntrustedSource(_))
        ));

        let feedback = failure_feedback(&episode);
        assert!(matches!(
            processor
                .apply(FeedbackSource::DeterministicCheck, feedback)
                .await,
            Err(FeedbackError::SourceAuthenticationMismatch { .. })
        ));
        assert!(revisions
            .history(&episode.episode_id)
            .await
            .expect("应读取历史")
            .is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn verifies_feedback_evidence_reference() {
        let root = temp_root();
        let (artifacts, episodes, episode) = recorded_episode(&root).await;
        let revisions = Arc::new(FileOutcomeRevisionStore::new(root.join("revisions")));
        let processor = FeedbackProcessor::new(artifacts.clone(), episodes, revisions);
        let mut feedback = failure_feedback(&episode);
        let mut reference = artifacts
            .put("application/json", br#"{"check":"failed"}"#)
            .await
            .expect("应写入反馈证据");
        reference.size_bytes += 1;
        feedback.evidence = Some(reference);

        assert!(matches!(
            processor.apply(FeedbackSource::User, feedback).await,
            Err(FeedbackError::ArtifactSizeMismatch { .. })
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
