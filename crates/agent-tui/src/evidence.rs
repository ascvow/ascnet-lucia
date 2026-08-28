//! TUI 对 Evolution Evidence Plane 的可信装配。

use super::*;
use crate::genome_binding::GenomeRuntimeBinding;
#[cfg(test)]
use crate::genome_binding::{
    current_git_commit, current_git_dirty, current_target_triple, current_tui_features,
};
use crate::genome_session::GenomeSessionRuntime;
use agent_evolution::{
    load_episode_evidence, EpisodeRecorderConfig, EpisodeRecorderHub, EvolutionPipeline,
    FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox, FileIssueObservationStore,
    FileOutcomeRevisionStore, RegisteredEpisodeRun, RuntimeHealthRecorder,
};
#[cfg(test)]
use agent_evolution::{ArtifactStore, FileGenomeStore, GenomeStore};
#[cfg(feature = "plugins")]
use agent_evolution_protocol::Outcome;
use agent_evolution_protocol::{GenomeRevision, OutcomeResolution};
#[cfg(feature = "plugins")]
use agent_runtime::{
    AgentRuntimeError, RuntimeResult, RuntimeRunContext, RuntimeRunFinalizer,
    RuntimeRunObservation, RuntimeRunObserver, RuntimeRunTermination,
};

/// TUI 进程固定使用的证据运行配置。
///
/// 启动时已从不可变 Genome Store 解析并校验修订；每次用户输入只生成新的 Run 与 Episode
/// ID，不允许在活动运行中替换 Genome。
#[derive(Clone)]
pub(crate) struct EvidenceRuntime {
    hub: Arc<EpisodeRecorderHub>,
    binding: Arc<GenomeRuntimeBinding>,
    artifacts: FileArtifactStore,
    episodes: FileEpisodeStore,
    pipeline: Arc<EvolutionPipeline<FileEvolutionOutbox, FileOutcomeRevisionStore>>,
    health: Option<RuntimeHealthRecorder>,
}

impl EvidenceRuntime {
    /// 使用 Session 层已验证的 Genome 绑定、Artifact CAS 和 Recorder Hub 创建运行配置。
    ///
    /// # Errors
    ///
    /// Genome 与当前 Kernel 不兼容，或声明当前 TUI 尚不能可信装配的行为表面时返回错误。
    pub(crate) fn new(
        hub: Arc<EpisodeRecorderHub>,
        binding: Arc<GenomeRuntimeBinding>,
        artifacts: FileArtifactStore,
        episodes: FileEpisodeStore,
        root: &Path,
    ) -> Result<Self> {
        Ok(Self {
            hub,
            binding,
            artifacts,
            episodes,
            pipeline: Arc::new(
                EvolutionPipeline::new(
                    Arc::new(FileEvolutionOutbox::new(root.join("outbox"))),
                    Arc::new(FileOutcomeRevisionStore::new(
                        root.join("outcome-revisions"),
                    )),
                )
                .with_issue_observation_store(Arc::new(
                    FileIssueObservationStore::new(root.join("issue-observations")),
                )),
            ),
            health: None,
        })
    }

    /// 使用测试 Revision 构造 Evidence，生产装配必须复用 Session Genome Binding。
    ///
    /// # Errors
    ///
    /// Revision 与当前 Kernel 不兼容，或声明无法可信装配的行为表面时返回错误。
    #[cfg(test)]
    pub(crate) fn new_for_test(
        hub: Arc<EpisodeRecorderHub>,
        revision: GenomeRevision,
        artifacts: FileArtifactStore,
        episodes: FileEpisodeStore,
        root: &Path,
    ) -> Result<Self> {
        let binding = Arc::new(GenomeRuntimeBinding::new(revision, artifacts.clone())?);
        Self::new(hub, binding, artifacts, episodes, root)
    }

    /// 绑定从同一 Stable 可信解析的 Promotion 后健康记录器。
    fn with_runtime_health(mut self, health: Option<RuntimeHealthRecorder>) -> Self {
        self.health = health;
        self
    }

    /// 返回应挂到主 Agent 的多运行事件 sink。
    pub(crate) fn hub(&self) -> Arc<EpisodeRecorderHub> {
        Arc::clone(&self.hub)
    }

    /// 返回启动时已经固定的 Genome Revision。
    pub(crate) fn revision(&self) -> &GenomeRevision {
        self.binding.revision()
    }

    /// 创建供 Agent Runtime 子运行使用的 Host 可信观察器。
    #[cfg(feature = "plugins")]
    pub(crate) fn runtime_run_observer(&self) -> Arc<dyn RuntimeRunObserver> {
        Arc::new(RuntimeEvidenceObserver {
            evidence: self.clone(),
        })
    }

    /// 为一次已经提交用户输入的主会话预登记 Episode。
    ///
    /// # Errors
    ///
    /// Run ID 发生碰撞或同一 ID 仍在 Hub 中活动时返回错误。
    pub(crate) async fn register_run(
        &self,
        session_id: impl Into<String>,
    ) -> Result<RegisteredEpisodeRun> {
        let mut config =
            EpisodeRecorderConfig::online(session_id, self.revision().revision_id.clone());
        // TUI 在 Core 返回后统一决定 Outcome；不能让后续 UI/JSONL sink 的收尾错误被
        // Recorder 提前固化为正常完成。
        config.finalize_on_run_finished = false;
        self.hub
            .register(config)
            .await
            .context("登记 Episode 运行失败")
    }

    /// 用可信 Outcome 输入收敛 Episode，并立即运行失败归因、聚合与 Outbox 路由。
    ///
    /// Pipeline 只读取刚由 Recorder 提交且经 CAS 重新校验的证据，不接受调用方提供的
    /// Incident、修订或 GenomeDigest。
    ///
    /// # Errors
    ///
    /// Recorder 收敛、CAS 恢复、Outcome 修订或 Outbox 写入失败时返回错误。
    pub(crate) async fn close_run(
        &self,
        run: RegisteredEpisodeRun,
        resolution: OutcomeResolution,
    ) -> Result<agent_evolution_protocol::EpisodeId> {
        let episode_id = run
            .close_with_resolution(resolution)
            .await
            .context("收敛 Episode 失败")?;
        let evidence = load_episode_evidence(&self.episodes, &self.artifacts, &episode_id)
            .await
            .context("恢复 Episode 监督证据失败")?;
        self.pipeline
            .process_episode(
                &evidence.episode,
                &evidence.incidents,
                &self.revision().digest,
                evidence.initial_outcome_revision.as_ref(),
            )
            .await
            .context("处理 Episode Evolution Pipeline 失败")?;
        if let Some(health) = &self.health {
            health
                .record_first_episode(&evidence.episode)
                .await
                .context("记录 Promotion 后 Runtime 健康观察失败")?;
        }
        Ok(episode_id)
    }
}

/// 把 Runtime 维护的 Agent 身份映射为独立 Episode 会话并预登记 Run。
#[cfg(feature = "plugins")]
struct RuntimeEvidenceObserver {
    evidence: EvidenceRuntime,
}

#[cfg(feature = "plugins")]
#[async_trait]
impl RuntimeRunObserver for RuntimeEvidenceObserver {
    async fn begin(&self, context: RuntimeRunContext) -> RuntimeResult<RuntimeRunObservation> {
        let run = self
            .evidence
            .register_run(format!("runtime-agent:{}", context.agent_id))
            .await
            .map_err(|error| AgentRuntimeError::RunObservation(error.to_string()))?;
        let run_id = run.run_id().to_string();
        RuntimeRunObservation::new(
            run_id,
            self.evidence.hub(),
            Arc::new(RuntimeEpisodeFinalizer {
                run: tokio::sync::Mutex::new(Some(run)),
                evidence: self.evidence.clone(),
            }),
        )
    }
}

/// 使用 Runtime 可信终态收敛一次子 Agent Episode。
#[cfg(feature = "plugins")]
struct RuntimeEpisodeFinalizer {
    run: tokio::sync::Mutex<Option<RegisteredEpisodeRun>>,
    evidence: EvidenceRuntime,
}

#[cfg(feature = "plugins")]
#[async_trait]
impl RuntimeRunFinalizer for RuntimeEpisodeFinalizer {
    async fn finish(&self, termination: RuntimeRunTermination) -> RuntimeResult<()> {
        let run =
            self.run.lock().await.take().ok_or_else(|| {
                AgentRuntimeError::RunObservation("Runtime Episode 已经收敛".into())
            })?;
        let outcome = match termination {
            RuntimeRunTermination::Completed => Outcome::Unverifiable,
            RuntimeRunTermination::Cancelled => Outcome::Cancelled,
            RuntimeRunTermination::Failed => run.interrupted_outcome().await,
        };
        self.evidence
            .close_run(run, OutcomeResolution::runtime(outcome))
            .await
            .map(|_| ())
            .map_err(|error| AgentRuntimeError::RunObservation(error.to_string()))
    }
}

/// 从已解析的 Session Genome Runtime 可选装配 Evidence Plane。
///
/// 未启用时不创建目录；启用时复用行为装配阶段已校验的精确 Revision，不再次按 Stable
/// 选择或改变 Session 行为。
///
/// # Errors
///
/// 当前 Session 为 Legacy、Stable 在装配期间变化，或证据根目录无法读取时返回错误。
pub(crate) async fn load_evidence_runtime(
    settings: &EvidenceSettings,
    genome_runtime: &GenomeSessionRuntime,
) -> Result<Option<EvidenceRuntime>> {
    if !settings.enabled {
        return Ok(None);
    }
    let binding = genome_runtime
        .binding_arc()
        .ok_or_else(|| anyhow!("Session 未绑定精确 Genome Revision，不具备 Evidence 资格"))?;
    let root = genome_runtime
        .registry_root()
        .ok_or_else(|| anyhow!("Evidence Session 缺少 Genome Registry 根目录"))?
        .to_path_buf();
    let health = match genome_runtime.stable_lineage() {
        Some(lineage) => RuntimeHealthRecorder::from_stable(&root, lineage)
            .await
            .with_context(|| format!("装配 Stable Runtime 健康观察失败：{lineage}"))?,
        None => None,
    };
    if health
        .as_ref()
        .is_some_and(|recorder| recorder.revision_id() != &binding.revision().revision_id)
    {
        return Err(anyhow!("Stable 在 Evidence Runtime 装配期间发生变化"));
    }

    let artifact_store = FileArtifactStore::new(root.join("artifacts"));
    let artifacts = Arc::new(artifact_store.clone());
    let episode_store = FileEpisodeStore::new(root.join("episodes"));
    let episodes = Arc::new(episode_store.clone());
    let mut hub = EpisodeRecorderHub::new(artifacts, episodes);
    if let Some(home) = std::env::var_os("HOME").and_then(|value| value.into_string().ok()) {
        hub = hub.with_home(home);
    }
    Ok(Some(
        EvidenceRuntime::new(Arc::new(hub), binding, artifact_store, episode_store, &root)?
            .with_runtime_health(health),
    ))
}

/// 构造仅供 TUI 测试使用、且完整 Prompt 已进入 CAS 的合法 Genome 修订。
#[cfg(test)]
pub(crate) async fn test_genome_revision(
    execution: agent_tool::ExecutionPolicy,
    artifacts: &FileArtifactStore,
) -> GenomeRevision {
    use agent_evolution_protocol::{
        AgentGenome, GenomeMetadata, ModelGenome, PromptArtifactRef, PromptGenome, PromptLayer,
        RuntimeIdentity, ToolProfileGenome, GENOME_SCHEMA_VERSION,
    };
    use std::collections::BTreeMap;

    let prompt = artifacts
        .put(
            "text/plain",
            agent_core::agent::DEFAULT_REACT_SYSTEM_PROMPT.as_bytes(),
        )
        .await
        .expect("应写入测试 Prompt CAS");

    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: env!("CARGO_PKG_VERSION").into(),
                git_commit: current_git_commit().into(),
                git_dirty: current_git_dirty(),
                target_triple: current_target_triple(),
                features: current_tui_features(),
            },
            model: ModelGenome {
                provider: "test".into(),
                provider_kind: "test".into(),
                model: "fixture".into(),
                base_url: None,
                protocol: None,
                max_tokens: Some(64),
                temperature: None,
                stream: false,
                provider_options_digest: None,
            },
            prompt: PromptGenome {
                messages: vec![PromptArtifactRef {
                    layer: PromptLayer::HostProtocol,
                    artifact: prompt.digest,
                }],
            },
            plugins: Vec::new(),
            capability_owners: BTreeMap::new(),
            tools: ToolProfileGenome::default(),
            context_policy: None,
            planning_policy: None,
            skills: Vec::new(),
            execution,
        },
        GenomeMetadata::default(),
    )
    .expect("测试 Genome 应合法")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "plugins")]
    use agent_core::{
        Agent, AgentOptions, ChatModel, ModelGateway, ModelRequest, ModelResponse, ProviderAdapter,
    };
    #[cfg(feature = "plugins")]
    use agent_evolution::{EpisodeQuery, EpisodeStore};
    use agent_evolution::{
        FileRuntimeHealthObservationStore, FileStableGenomePublisher, RUNTIME_HEALTH_DIRECTORY,
    };
    #[cfg(feature = "plugins")]
    use agent_evolution_protocol::Outcome;
    use agent_evolution_protocol::{EvaluationReportId, ReleaseId};
    #[cfg(feature = "plugins")]
    use agent_runtime::{
        AgentOutcome, AgentPermissions, AgentRuntime, AgentSpawnRequest, AgentTemplate,
        RuntimeLimits,
    };
    use agent_tool::ExecutionPolicy;
    use sha2::{Digest, Sha256};
    #[cfg(feature = "plugins")]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(feature = "plugins")]
    use tokio::sync::Notify;

    /// 使用已登记 Revision 构造 Session Genome Runtime，隔离 Evidence 开关测试。
    fn genome_runtime(
        root: &Path,
        revision: GenomeRevision,
        stable_lineage: Option<&str>,
    ) -> GenomeSessionRuntime {
        let binding =
            GenomeRuntimeBinding::new(revision, FileArtifactStore::new(root.join("artifacts")))
                .expect("测试 Genome 应可装配");
        GenomeSessionRuntime::Genome {
            binding: Arc::new(binding),
            registry_root: root.to_path_buf(),
            stable_lineage: stable_lineage.map(str::to_string),
        }
    }

    /// 返回固定公开响应的 Runtime Evidence 端到端测试模型。
    #[cfg(feature = "plugins")]
    struct RuntimeEvidenceModel;

    /// 暴露模型已进入信号并持续阻塞，用于验证取消时的真实 Episode 收敛。
    #[cfg(feature = "plugins")]
    struct BlockingRuntimeEvidenceModel {
        entered: Arc<AtomicBool>,
        release: Arc<Notify>,
    }

    #[cfg(feature = "plugins")]
    #[async_trait]
    impl ChatModel for RuntimeEvidenceModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse::text("Runtime 子 Agent 已完成"))
        }
    }

    #[cfg(feature = "plugins")]
    #[async_trait]
    impl ProviderAdapter for RuntimeEvidenceModel {
        fn name(&self) -> &'static str {
            "runtime-evidence-fixture"
        }
    }

    #[cfg(feature = "plugins")]
    #[async_trait]
    impl ChatModel for BlockingRuntimeEvidenceModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            self.entered.store(true, Ordering::Release);
            self.release.notified().await;
            Ok(ModelResponse::text("取消后不应完成"))
        }
    }

    #[cfg(feature = "plugins")]
    #[async_trait]
    impl ProviderAdapter for BlockingRuntimeEvidenceModel {
        fn name(&self) -> &'static str {
            "blocking-runtime-evidence-fixture"
        }
    }

    /// 未启用 Evidence 时不得要求 Genome 配置或创建目录。
    #[tokio::test]
    async fn disabled_evidence_has_no_side_effects() {
        let root = std::env::temp_dir().join(format!(
            "lucia-disabled-evidence-{}",
            agent_session::SessionId::generate()
        ));
        let settings = EvidenceSettings::default();

        assert!(
            load_evidence_runtime(&settings, &GenomeSessionRuntime::Unconfigured)
                .await
                .expect("禁用时应直接返回")
                .is_none()
        );
        assert!(!root.exists());
    }

    /// 启用 Evidence 但未指定 Genome 时必须在启动阶段失败。
    #[tokio::test]
    async fn enabled_evidence_requires_registered_genome() {
        let settings = EvidenceSettings {
            enabled: true,
            root_dir: None,
            genome_revision_id: None,
            genome_stable: None,
        };
        let result = load_evidence_runtime(&settings, &GenomeSessionRuntime::Unconfigured).await;
        let Err(error) = result else {
            panic!("缺少 Genome 必须失败");
        };

        assert!(error.to_string().contains("Evidence 资格"));
    }

    /// 启用时必须从不可变 Store 读取并验证 Revision，而不是只接受配置中的 ID 文本。
    #[tokio::test]
    async fn enabled_evidence_resolves_registered_genome() {
        let root = std::env::temp_dir().join(format!(
            "lucia-enabled-evidence-{}",
            agent_session::SessionId::generate()
        ));
        let evidence_root = root.join("evolution");
        let genomes = FileGenomeStore::new(evidence_root.join("genomes"));
        let fixture_root = root.join("fixtures");
        let artifacts = FileArtifactStore::new(evidence_root.join("artifacts"));
        let revision =
            test_genome_revision(ExecutionPolicy::evaluation(&fixture_root), &artifacts).await;
        genomes.append(&revision).await.expect("应登记 Genome");
        let settings = EvidenceSettings {
            enabled: true,
            root_dir: Some(evidence_root.clone()),
            genome_revision_id: Some(revision.revision_id.to_string()),
            genome_stable: None,
        };

        let session_runtime = genome_runtime(&evidence_root, revision.clone(), None);
        let runtime = load_evidence_runtime(&settings, &session_runtime)
            .await
            .expect("应加载 Evidence")
            .expect("Evidence 应启用");
        assert_eq!(runtime.hub().active_runs().await, 0);
        assert_eq!(runtime.revision(), &revision);
        assert_eq!(
            session_runtime
                .binding()
                .expect("应有 Genome 绑定")
                .execution_policy(),
            &revision.genome.execution
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 新会话可通过 Stable lineage 解析当前修订，且绑定的仍是精确 Revision ID。
    #[tokio::test]
    async fn enabled_evidence_resolves_stable_genome() {
        let root = std::env::temp_dir().join(format!(
            "lucia-stable-evidence-{}",
            agent_session::SessionId::generate()
        ));
        let evidence_root = root.join("evolution");
        let genomes = FileGenomeStore::new(evidence_root.join("genomes"));
        let artifacts = FileArtifactStore::new(evidence_root.join("artifacts"));
        let revision = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        genomes.append(&revision).await.expect("应登记 Genome");
        let stable = agent_evolution::StableGenomeRef::new("stable/general", &revision, 1)
            .expect("应构造 Stable 引用");
        let stable_root = evidence_root.join("stable");
        tokio::fs::create_dir_all(&stable_root)
            .await
            .expect("应创建 Stable 目录");
        let stable_name = format!("{:x}.json", Sha256::digest(b"stable/general"));
        tokio::fs::write(
            stable_root.join(stable_name),
            serde_json::to_vec_pretty(&stable).expect("应序列化 Stable 引用"),
        )
        .await
        .expect("应写入 Stable 引用");
        let settings = EvidenceSettings {
            enabled: true,
            root_dir: Some(evidence_root.clone()),
            genome_revision_id: None,
            genome_stable: Some("stable/general".into()),
        };

        let session_runtime =
            genome_runtime(&evidence_root, revision.clone(), Some("stable/general"));
        let runtime = load_evidence_runtime(&settings, &session_runtime)
            .await
            .expect("应加载 Stable Evidence")
            .expect("Evidence 应启用");
        assert_eq!(runtime.revision().revision_id, revision.revision_id);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Promotion 后通过 Stable 启动的 TUI 必须从真实关闭 Episode 写入首份健康观察。
    #[tokio::test]
    async fn promoted_stable_records_health_from_closed_episode() {
        let root = std::env::temp_dir().join(format!(
            "lucia-promoted-health-evidence-{}",
            agent_session::SessionId::generate()
        ));
        let evidence_root = root.join("evolution");
        let genomes = FileGenomeStore::new(evidence_root.join("genomes"));
        let artifacts = FileArtifactStore::new(evidence_root.join("artifacts"));
        let parent = test_genome_revision(
            ExecutionPolicy::evaluation(&root.join("fixtures")),
            &artifacts,
        )
        .await;
        let candidate = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        genomes.append(&parent).await.expect("应登记 Parent Genome");
        genomes
            .append(&candidate)
            .await
            .expect("应登记 Candidate Genome");
        let publisher = FileStableGenomePublisher::new(&evidence_root);
        let stable = publisher
            .publish("stable/general", &parent, 1)
            .await
            .expect("应初始化 Stable");
        let release_id = ReleaseId::generate();
        publisher
            .publish_bound(
                &stable,
                &candidate,
                2,
                release_id.clone(),
                EvaluationReportId::generate(),
                None,
            )
            .await
            .expect("应提交 Promotion Stable");
        let settings = EvidenceSettings {
            enabled: true,
            root_dir: Some(evidence_root.clone()),
            genome_revision_id: None,
            genome_stable: Some("stable/general".into()),
        };
        let session_runtime =
            genome_runtime(&evidence_root, candidate.clone(), Some("stable/general"));
        let runtime = load_evidence_runtime(&settings, &session_runtime)
            .await
            .expect("应加载 Promotion Stable")
            .expect("Evidence 应启用");
        let run = runtime
            .register_run("promoted-health-session")
            .await
            .expect("应登记真实运行");
        runtime
            .hub()
            .record(&AgentEvent::new(
                run.run_id().to_string(),
                AgentEventKind::RunStarted,
                0,
                serde_json::json!({}),
            ))
            .await
            .expect("应记录真实运行事件");
        runtime
            .close_run(
                run,
                OutcomeResolution::runtime(agent_evolution_protocol::Outcome::Unverifiable),
            )
            .await
            .expect("应从真实 Episode 收敛健康观察");

        let observation =
            FileRuntimeHealthObservationStore::new(evidence_root.join(RUNTIME_HEALTH_DIRECTORY))
                .expect("健康根应合法")
                .load(&release_id)
                .await
                .expect("应读取发布后健康观察");
        assert_eq!(
            observation.observation().observed_revision_id,
            candidate.revision_id
        );
        assert_eq!(observation.observation().checks_passed, 2);
        assert_eq!(observation.observation().checks_total, 2);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// TUI 注入的可信观察器必须为真实 Runtime 子 Agent 创建独立且完整的 Episode。
    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn runtime_child_run_persists_independent_episode() {
        let root = std::env::temp_dir().join(format!(
            "lucia-runtime-evidence-{}",
            agent_session::SessionId::generate()
        ));
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let hub = Arc::new(EpisodeRecorderHub::new(
            Arc::new(artifacts.clone()),
            episodes.clone(),
        ));
        let revision = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        let genome_revision_id = revision.revision_id.clone();
        let evidence = EvidenceRuntime::new_for_test(
            Arc::clone(&hub),
            revision,
            artifacts,
            FileEpisodeStore::new(root.join("episodes")),
            &root,
        )
        .expect("应创建 Runtime Evidence");
        let runtime = AgentRuntime::new_with_run_observer(
            RuntimeLimits::default(),
            evidence.runtime_run_observer(),
        )
        .expect("创建带 TUI Evidence 观察器的 Runtime");
        let mut gateway = ModelGateway::new();
        gateway
            .register("runtime-evidence-fixture", Arc::new(RuntimeEvidenceModel))
            .expect("注册 Runtime Evidence 测试模型");
        let agent = Agent::new(
            gateway,
            AgentOptions::default().with_model_route("runtime-evidence-fixture", "fixture-model"),
        );
        let root_agent = runtime
            .attach_root(
                AgentTemplate::from_agent(&agent),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载 Runtime 根 Agent");
        let api = runtime.api(&root_agent.id).await.expect("绑定 Runtime API");
        let child = api
            .spawn(AgentSpawnRequest::new("记录独立 Runtime Episode"))
            .await
            .expect("派生 Runtime 子 Agent");

        assert!(matches!(
            api.wait(&child.id).await.expect("等待 Runtime 子 Agent"),
            AgentOutcome::Succeeded { .. }
        ));
        let stored = episodes
            .query(&EpisodeQuery {
                outcome: None,
                session_id: Some(format!("runtime-agent:{}", child.id)),
            })
            .await
            .expect("查询 Runtime 子 Agent Episode");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].session_id, format!("runtime-agent:{}", child.id));
        assert_eq!(stored[0].genome_revision_id, genome_revision_id);
        assert_eq!(stored[0].outcome, Some(Outcome::Unverifiable));
        assert!(!stored[0].run_id.as_str().is_empty());
        assert_eq!(hub.active_runs().await, 0);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 取消真实 Runtime 子 Agent 后必须持久化 Cancelled Episode，并释放 Hub 路由。
    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn cancelled_runtime_child_closes_episode_before_terminal_state() {
        let root = std::env::temp_dir().join(format!(
            "lucia-cancelled-runtime-evidence-{}",
            agent_session::SessionId::generate()
        ));
        let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let hub = Arc::new(EpisodeRecorderHub::new(
            Arc::new(artifacts.clone()),
            episodes.clone(),
        ));
        let revision = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        let genome_revision_id = revision.revision_id.clone();
        let evidence = EvidenceRuntime::new_for_test(
            Arc::clone(&hub),
            revision,
            artifacts,
            FileEpisodeStore::new(root.join("episodes")),
            &root,
        )
        .expect("应创建取消测试 Evidence");
        let runtime = AgentRuntime::new_with_run_observer(
            RuntimeLimits::default(),
            evidence.runtime_run_observer(),
        )
        .expect("创建带取消证据观察器的 Runtime");
        let entered = Arc::new(AtomicBool::new(false));
        let mut gateway = ModelGateway::new();
        gateway
            .register(
                "blocking-runtime-evidence-fixture",
                Arc::new(BlockingRuntimeEvidenceModel {
                    entered: Arc::clone(&entered),
                    release: Arc::new(Notify::new()),
                }),
            )
            .expect("注册阻塞 Runtime Evidence 测试模型");
        let agent = Agent::new(
            gateway,
            AgentOptions::default()
                .with_model_route("blocking-runtime-evidence-fixture", "fixture-model"),
        );
        let root_agent = runtime
            .attach_root(
                AgentTemplate::from_agent(&agent),
                AgentPermissions::default(),
            )
            .await
            .expect("挂载取消测试根 Agent");
        let api = runtime.api(&root_agent.id).await.expect("绑定取消测试 API");
        let child = api
            .spawn(AgentSpawnRequest::new("取消并收敛 Runtime Episode"))
            .await
            .expect("派生待取消 Runtime 子 Agent");
        for _ in 0..100 {
            if entered.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(entered.load(Ordering::Acquire));

        assert!(api.cancel(&child.id).await.expect("取消 Runtime 子 Agent"));
        assert_eq!(
            api.wait(&child.id).await.expect("等待取消终态"),
            AgentOutcome::Cancelled
        );
        let stored = episodes
            .query(&EpisodeQuery {
                outcome: Some(Outcome::Cancelled),
                session_id: Some(format!("runtime-agent:{}", child.id)),
            })
            .await
            .expect("查询取消 Runtime Episode");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].genome_revision_id, genome_revision_id);
        assert_eq!(stored[0].outcome, Some(Outcome::Cancelled));
        assert_eq!(hub.active_runs().await, 0);
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
