//! TUI 对 Evolution Evidence Plane 的可信装配。

use super::*;
use agent_evolution::{
    EpisodeRecorderConfig, EpisodeRecorderHub, FileArtifactStore, FileEpisodeStore,
    FileGenomeStore, GenomeStore, RegisteredEpisodeRun,
};
use agent_evolution_protocol::GenomeRevisionId;
#[cfg(feature = "plugins")]
use agent_evolution_protocol::Outcome;
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
    genome_revision_id: GenomeRevisionId,
}

impl EvidenceRuntime {
    /// 使用已验证的 Genome 修订和 Recorder Hub 创建运行配置。
    pub(crate) fn new(hub: Arc<EpisodeRecorderHub>, genome_revision_id: GenomeRevisionId) -> Self {
        Self {
            hub,
            genome_revision_id,
        }
    }

    /// 返回应挂到主 Agent 的多运行事件 sink。
    pub(crate) fn hub(&self) -> Arc<EpisodeRecorderHub> {
        Arc::clone(&self.hub)
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
        let mut config = EpisodeRecorderConfig::online(session_id, self.genome_revision_id.clone());
        // TUI 在 Core 返回后统一决定 Outcome；不能让后续 UI/JSONL sink 的收尾错误被
        // Recorder 提前固化为正常完成。
        config.finalize_on_run_finished = false;
        self.hub
            .register(config)
            .await
            .context("登记 Episode 运行失败")
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
            }),
        )
    }
}

/// 使用 Runtime 可信终态收敛一次子 Agent Episode。
#[cfg(feature = "plugins")]
struct RuntimeEpisodeFinalizer {
    run: tokio::sync::Mutex<Option<RegisteredEpisodeRun>>,
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
        run.close(outcome)
            .await
            .map(|_| ())
            .map_err(|error| AgentRuntimeError::RunObservation(error.to_string()))
    }
}

/// 从 TUI 配置加载并验证 Evidence Plane。
///
/// 未启用时不创建目录；启用时要求指定已经存在于不可变 Genome Store 的 Revision ID。
///
/// # Errors
///
/// 修订 ID 不合法、Genome 不存在或被篡改，以及证据根目录无法读取时返回错误。
pub(crate) async fn load_evidence_runtime(
    settings: &EvidenceSettings,
    config_path: &Path,
    lucia_home: &Path,
) -> Result<Option<EvidenceRuntime>> {
    if !settings.enabled {
        return Ok(None);
    }
    let revision_text = settings
        .genome_revision_id
        .as_deref()
        .ok_or_else(|| anyhow!("启用 evidence 时必须配置 evidence.genome_revision_id"))?;
    let revision_id = GenomeRevisionId::new(revision_text)
        .context("evidence.genome_revision_id 不是合法的 Genome Revision ID")?;
    let root = settings
        .root_dir
        .as_deref()
        .map(|path| resolve_config_relative_path(config_path, path))
        .unwrap_or_else(|| lucia_home.join("evolution"));
    let genomes = FileGenomeStore::new(root.join("genomes"));
    genomes
        .get(&revision_id)
        .await
        .with_context(|| format!("验证 Genome 修订失败：{revision_id}"))?
        .ok_or_else(|| anyhow!("Genome 修订不存在：{revision_id}"))?;

    let artifacts = Arc::new(FileArtifactStore::new(root.join("artifacts")));
    let episodes = Arc::new(FileEpisodeStore::new(root.join("episodes")));
    let mut hub = EpisodeRecorderHub::new(artifacts, episodes);
    if let Some(home) = std::env::var_os("HOME").and_then(|value| value.into_string().ok()) {
        hub = hub.with_home(home);
    }
    Ok(Some(EvidenceRuntime::new(Arc::new(hub), revision_id)))
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
    use agent_evolution_protocol::{
        AgentGenome, GenomeMetadata, GenomeRevision, ModelGenome, PromptGenome, RuntimeIdentity,
        ToolProfileGenome, GENOME_SCHEMA_VERSION,
    };
    #[cfg(feature = "plugins")]
    use agent_runtime::{
        AgentOutcome, AgentPermissions, AgentRuntime, AgentSpawnRequest, AgentTemplate,
        RuntimeLimits,
    };
    use agent_tool::ExecutionPolicy;
    use std::collections::{BTreeMap, BTreeSet};
    #[cfg(feature = "plugins")]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(feature = "plugins")]
    use tokio::sync::Notify;

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

    /// 构造用于验证 TUI 启动绑定的最小 Genome 修订。
    fn revision() -> GenomeRevision {
        GenomeRevision::create(
            AgentGenome {
                schema_version: GENOME_SCHEMA_VERSION,
                runtime: RuntimeIdentity {
                    package_version: "0.1.0".into(),
                    git_commit: "test".into(),
                    git_dirty: true,
                    target_triple: "test-target".into(),
                    features: BTreeSet::new(),
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
                prompt: PromptGenome::default(),
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools: ToolProfileGenome::default(),
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("测试 Genome 应合法")
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
            load_evidence_runtime(&settings, &root.join("config.toml"), &root)
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
        };
        let result = load_evidence_runtime(
            &settings,
            Path::new("/tmp/lucia/config.toml"),
            Path::new("/tmp/lucia"),
        )
        .await;
        let Err(error) = result else {
            panic!("缺少 Genome 必须失败");
        };

        assert!(error.to_string().contains("genome_revision_id"));
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
        let revision = revision();
        genomes.append(&revision).await.expect("应登记 Genome");
        let settings = EvidenceSettings {
            enabled: true,
            root_dir: Some(evidence_root),
            genome_revision_id: Some(revision.revision_id.to_string()),
        };

        let runtime = load_evidence_runtime(&settings, &root.join("config.toml"), &root)
            .await
            .expect("应加载 Evidence")
            .expect("Evidence 应启用");
        assert_eq!(runtime.hub().active_runs().await, 0);
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
        let hub = Arc::new(EpisodeRecorderHub::new(
            Arc::new(FileArtifactStore::new(root.join("artifacts"))),
            episodes.clone(),
        ));
        let genome_revision_id = GenomeRevisionId::generate();
        let evidence = EvidenceRuntime::new(Arc::clone(&hub), genome_revision_id.clone());
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
        let hub = Arc::new(EpisodeRecorderHub::new(
            Arc::new(FileArtifactStore::new(root.join("artifacts"))),
            episodes.clone(),
        ));
        let genome_revision_id = GenomeRevisionId::generate();
        let evidence = EvidenceRuntime::new(Arc::clone(&hub), genome_revision_id.clone());
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
