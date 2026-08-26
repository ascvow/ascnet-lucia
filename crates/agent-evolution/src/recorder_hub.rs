//! 应用层按 Run ID 路由单次 Episode Recorder。

use crate::{
    ArtifactStore, EpisodeRecorder, EpisodeRecorderConfig, EpisodeRecorderError, EpisodeStore,
};
use agent_core::{AgentEvent, EventSink};
use agent_evolution_protocol::{EpisodeId, Outcome, OutcomeResolution, RunId};
use anyhow::Result as AnyResult;
use async_trait::async_trait;
use std::{
    collections::{hash_map::Entry, HashMap},
    sync::Arc,
};
use tokio::sync::RwLock;

/// 多运行共享的 Episode Recorder 路由器。
///
/// `Agent` 可以长期持有该 sink；应用层必须在启动每次运行前调用 [`Self::register`]，
/// 使事件只进入对应的单次 Recorder。未登记运行会被拒绝，避免静默漏记证据。
pub struct EpisodeRecorderHub {
    artifacts: Arc<dyn ArtifactStore>,
    episodes: Arc<dyn EpisodeStore>,
    home: Option<String>,
    active: RwLock<HashMap<String, Arc<EpisodeRecorder>>>,
}

impl EpisodeRecorderHub {
    /// 创建使用固定 CAS 与 Episode Store 的路由器。
    pub fn new(artifacts: Arc<dyn ArtifactStore>, episodes: Arc<dyn EpisodeStore>) -> Self {
        Self {
            artifacts,
            episodes,
            home: None,
            active: RwLock::new(HashMap::new()),
        }
    }

    /// 指定宿主主目录，所有后续 Recorder 都会据此强化私有路径脱敏。
    pub fn with_home(mut self, home: impl Into<String>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// 在 Agent 启动前登记单次运行，并返回负责显式收敛的句柄。
    ///
    /// # Errors
    ///
    /// 同一 Run ID 仍处于活动状态时返回错误。
    pub async fn register(
        self: &Arc<Self>,
        config: EpisodeRecorderConfig,
    ) -> Result<RegisteredEpisodeRun, EpisodeRecorderHubError> {
        let run_id = config.run_id.clone();
        let mut recorder = EpisodeRecorder::new(
            config,
            Arc::clone(&self.artifacts),
            Arc::clone(&self.episodes),
        );
        if let Some(home) = &self.home {
            recorder = recorder.with_home(home.clone());
        }
        let recorder = Arc::new(recorder);
        let mut active = self.active.write().await;
        match active.entry(run_id.to_string()) {
            Entry::Occupied(_) => return Err(EpisodeRecorderHubError::DuplicateRun(run_id)),
            Entry::Vacant(entry) => {
                entry.insert(Arc::clone(&recorder));
            }
        }
        Ok(RegisteredEpisodeRun {
            hub: Arc::clone(self),
            run_id,
            recorder,
        })
    }

    /// 返回当前活动运行数，供健康检查与测试使用。
    pub async fn active_runs(&self) -> usize {
        self.active.read().await.len()
    }

    /// 从活动路由中移除指定运行。
    async fn unregister(&self, run_id: &RunId) {
        self.active.write().await.remove(run_id.as_str());
    }
}

#[async_trait]
impl EventSink for EpisodeRecorderHub {
    async fn record(&self, event: &AgentEvent) -> AnyResult<()> {
        let recorder = self
            .active
            .read()
            .await
            .get(&event.run_id)
            .cloned()
            .ok_or_else(|| EpisodeRecorderHubError::UnregisteredRun(event.run_id.clone()))?;
        recorder.record(event).await
    }
}

/// 一次已经预登记的 Episode 运行。
///
/// 正常 `RunFinished` 会由 Recorder 自动持久化；调用方仍必须调用 [`Self::close`]，以便
/// 确认 Episode 已落盘、为异常退出补写终态并释放 Hub 路由。
#[must_use = "已登记的 Episode 运行必须在 Agent 退出后调用 close"]
pub struct RegisteredEpisodeRun {
    hub: Arc<EpisodeRecorderHub>,
    run_id: RunId,
    recorder: Arc<EpisodeRecorder>,
}

impl RegisteredEpisodeRun {
    /// 返回必须传给 `Agent::run_session_with_id` 的运行标识。
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// 根据本运行已记录的确定性事件推断异常退出终态。
    pub async fn interrupted_outcome(&self) -> Outcome {
        self.recorder.interrupted_outcome().await
    }

    /// 在收敛前提交 Host 可信的 Outcome Resolver 输入。
    ///
    /// # Errors
    ///
    /// 输入不合法、运行尚无事件、已收敛或 Recorder 拒绝事件时返回错误。
    pub async fn record_outcome_resolution(
        &self,
        resolution: OutcomeResolution,
    ) -> Result<(), EpisodeRecorderHubError> {
        self.recorder
            .record_outcome_resolution(resolution)
            .await
            .map_err(EpisodeRecorderHubError::Recorder)
    }

    /// 使用 Host 可信 Outcome 输入收敛并释放当前运行。
    ///
    /// 无论成功与否都会释放 Hub 路由；失败表示没有形成可依赖的完整证据。
    ///
    /// # Errors
    ///
    /// Outcome 输入、CAS、监督证据或 Episode Header 持久化失败时返回错误。
    pub async fn close_with_resolution(
        self,
        resolution: OutcomeResolution,
    ) -> Result<EpisodeId, EpisodeRecorderHubError> {
        let result = self
            .recorder
            .finish_with_resolution(resolution)
            .await
            .map_err(EpisodeRecorderHubError::Recorder);
        self.hub.unregister(&self.run_id).await;
        result
    }

    /// 确认自动收敛结果，或使用调用方提供的异常终态显式收敛。
    ///
    /// 无论成功与否都会释放 Hub 路由；错误表示本次运行没有形成可依赖的完整证据，
    /// 应由应用层明确报告，不能静默继续。
    ///
    /// # Errors
    ///
    /// Recorder 未收到事件，或 CAS、监督证据和 Episode Header 持久化失败时返回错误。
    pub async fn close(
        self,
        abnormal_outcome: Outcome,
    ) -> Result<EpisodeId, EpisodeRecorderHubError> {
        let result = match self.recorder.episode_id().await {
            Some(id) => Ok(id),
            None => self
                .recorder
                .finish(abnormal_outcome)
                .await
                .map_err(EpisodeRecorderHubError::Recorder),
        };
        self.hub.unregister(&self.run_id).await;
        result
    }
}

/// Episode Recorder Hub 错误。
#[derive(Debug, thiserror::Error)]
pub enum EpisodeRecorderHubError {
    /// 同一 Run ID 被重复登记。
    #[error("Episode 运行已经登记：{0}")]
    DuplicateRun(RunId),
    /// sink 收到了应用层没有预登记的运行。
    #[error("Episode sink 收到未登记运行：{0}")]
    UnregisteredRun(String),
    /// 单次 Recorder 记录或收敛失败。
    #[error(transparent)]
    Recorder(#[from] EpisodeRecorderError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileArtifactStore, FileEpisodeStore};
    use agent_core::{AgentEventKind, EventSink};
    use agent_evolution_protocol::{GenomeRevisionId, Outcome};
    use serde_json::json;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// 构造不会与并发测试冲突的临时证据根目录。
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lucia-recorder-hub-{}", Uuid::new_v4().simple()))
    }

    /// 创建使用真实文件存储的 Hub。
    fn hub(root: &std::path::Path) -> Arc<EpisodeRecorderHub> {
        Arc::new(EpisodeRecorderHub::new(
            Arc::new(FileArtifactStore::new(root.join("artifacts"))),
            Arc::new(FileEpisodeStore::new(root.join("episodes"))),
        ))
    }

    /// Hub 必须按 Run ID 隔离并发事件，并在关闭后释放全部路由。
    #[tokio::test]
    async fn routes_concurrent_runs_and_closes_them() {
        let root = temp_root();
        let hub = hub(&root);
        let first = hub
            .register(EpisodeRecorderConfig::online(
                "session-1",
                GenomeRevisionId::generate(),
            ))
            .await
            .expect("应登记首个运行");
        let second = hub
            .register(EpisodeRecorderConfig::online(
                "session-2",
                GenomeRevisionId::generate(),
            ))
            .await
            .expect("应登记第二个运行");

        for run in [&first, &second] {
            hub.record(&AgentEvent::new(
                run.run_id().to_string(),
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .await
            .expect("开始事件应路由");
            hub.record(&AgentEvent::new(
                run.run_id().to_string(),
                AgentEventKind::RunFinished,
                0,
                json!({"steps_used": 1}),
            ))
            .await
            .expect("结束事件应路由");
        }

        first
            .close(Outcome::InfrastructureFailure)
            .await
            .expect("首个运行应确认收敛");
        second
            .close(Outcome::InfrastructureFailure)
            .await
            .expect("第二个运行应确认收敛");
        assert_eq!(hub.active_runs().await, 0);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 未登记运行必须失败，避免事件被静默丢弃。
    #[tokio::test]
    async fn rejects_unregistered_run() {
        let root = temp_root();
        let hub = hub(&root);
        let error = hub
            .record(&AgentEvent::new(
                "unknown",
                AgentEventKind::RunStarted,
                0,
                json!({}),
            ))
            .await
            .expect_err("未登记运行应失败");

        assert!(error.to_string().contains("未登记运行"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 步骤上限事件必须由 Recorder 推断为预算失败，不依赖错误文本匹配。
    #[tokio::test]
    async fn infers_budget_failure_from_recorded_event() {
        let root = temp_root();
        let hub = hub(&root);
        let mut config =
            EpisodeRecorderConfig::online("budget-session", GenomeRevisionId::generate());
        config.finalize_on_run_finished = false;
        let run = hub.register(config).await.expect("应登记预算测试运行");
        for (step, kind) in [
            (0, AgentEventKind::RunStarted),
            (1, AgentEventKind::StepLimitReached),
        ] {
            hub.record(&AgentEvent::new(
                run.run_id().to_string(),
                kind,
                step,
                json!({"max_steps": 1}),
            ))
            .await
            .expect("事件应路由");
        }

        assert_eq!(run.interrupted_outcome().await, Outcome::BudgetFailure);
        run.close(Outcome::BudgetFailure)
            .await
            .expect("预算运行应收敛");
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
