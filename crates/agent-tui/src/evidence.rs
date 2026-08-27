//! TUI 对 Evolution Evidence Plane 的可信装配。

use super::*;
use crate::genome_binding::GenomeRuntimeBinding;
#[cfg(test)]
use crate::genome_binding::{
    current_git_commit, current_git_dirty, current_target_triple, current_tui_features,
};
use agent_context::NativeContextPolicy;
use agent_evolution::{
    load_episode_evidence, EpisodeRecorderConfig, EpisodeRecorderHub, EvolutionPipeline,
    FileArtifactStore, FileEpisodeStore, FileEvolutionOutbox, FileGenomeResolver,
    FileIssueObservationStore, FileOutcomeRevisionStore, GenomeResolver, GenomeSelector,
    RegisteredEpisodeRun, RuntimeHealthRecorder,
};
#[cfg(test)]
use agent_evolution::{ArtifactStore, FileGenomeStore, GenomeStore};
#[cfg(feature = "plugins")]
use agent_evolution_protocol::Outcome;
use agent_evolution_protocol::{GenomeRevision, GenomeRevisionId, OutcomeResolution};
#[cfg(feature = "plugins")]
use agent_runtime::{
    AgentRuntimeError, RuntimeResult, RuntimeRunContext, RuntimeRunFinalizer,
    RuntimeRunObservation, RuntimeRunObserver, RuntimeRunTermination,
};
use agent_session::SessionBehaviorBinding;
use agent_skill::SkillCatalog;

/// Session Store 中用于标识 Agent Genome 修订绑定的协议名。
const GENOME_SESSION_BEHAVIOR_KIND: &str = "agent_genome";

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
    /// 使用已验证的 Genome 修订、Artifact CAS 和 Recorder Hub 创建运行配置。
    ///
    /// # Errors
    ///
    /// Genome 与当前 Kernel 不兼容，或声明当前 TUI 尚不能可信装配的行为表面时返回错误。
    pub(crate) fn new(
        hub: Arc<EpisodeRecorderHub>,
        revision: GenomeRevision,
        artifacts: FileArtifactStore,
        episodes: FileEpisodeStore,
        root: &Path,
    ) -> Result<Self> {
        Ok(Self {
            hub,
            binding: Arc::new(GenomeRuntimeBinding::new(revision, artifacts.clone())?),
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

    /// 绑定从同一 Stable 可信解析的 Promotion 后健康记录器。
    fn with_runtime_health(mut self, health: Option<RuntimeHealthRecorder>) -> Self {
        self.health = health;
        self
    }

    /// 返回应挂到主 Agent 的多运行事件 sink。
    pub(crate) fn hub(&self) -> Arc<EpisodeRecorderHub> {
        Arc::clone(&self.hub)
    }

    /// 返回该可信 Genome 为本次 TUI 进程声明的执行策略。
    pub(crate) fn execution_policy(&self) -> &agent_tool::ExecutionPolicy {
        self.binding.execution_policy()
    }

    /// 返回启动时已经固定的 Genome Revision。
    pub(crate) fn revision(&self) -> &GenomeRevision {
        self.binding.revision()
    }

    /// 用 Genome 的模型行为绑定普通配置，同时保留凭据来源。
    ///
    /// # Errors
    ///
    /// 模型适配器类型、协议或额外 Header 无法安全绑定时返回错误。
    pub(crate) fn bind_model_config(&self, config: AgentRootConfig) -> Result<AgentRootConfig> {
        self.binding.bind_model_config(config)
    }

    /// 用 Genome 固定显式 Demo 运行的模型、Prompt 与资源行为。
    ///
    /// # Errors
    ///
    /// Demo 路由与 Genome 不符或引用制品不可读取时返回错误。
    pub(crate) async fn bind_demo_options(&self, options: AgentOptions) -> Result<AgentOptions> {
        self.binding.bind_demo_options(options).await
    }

    /// 用 Genome 固定真实模型选项、Prompt 与资源行为。
    ///
    /// # Errors
    ///
    /// 引用制品缺失、格式非法或模型参数无法解析时返回错误。
    pub(crate) async fn bind_agent_options(&self, options: AgentOptions) -> Result<AgentOptions> {
        self.binding.bind_agent_options(options).await
    }

    /// 按 Genome 选择当前 Kernel 的原生工具子集。
    ///
    /// # Errors
    ///
    /// Genome 引用了未注册的原生工具时返回错误。
    pub(crate) fn bind_native_tools(&self, tools: &ToolRegistry) -> Result<ToolRegistry> {
        self.binding.bind_native_tools(tools)
    }

    /// 从 Genome CAS 装配原生 Skill 目录。
    ///
    /// # Errors
    ///
    /// Skill 引用、状态、强类型 ID 或 CAS 完整性校验失败时返回错误。
    pub(crate) async fn bind_skill_catalog(&self) -> Result<SkillCatalog> {
        self.binding.bind_skill_catalog().await
    }

    /// 从 Genome CAS 装配原生上下文压缩策略。
    ///
    /// # Errors
    ///
    /// 策略 owner 不是原生稳定 ID，或 CAS 制品未通过完整性与协议校验时返回错误。
    pub(crate) async fn bind_context_policy(&self) -> Result<Option<NativeContextPolicy>> {
        self.binding.bind_context_policy().await
    }

    /// 从发现结果中选择并验证 Genome 固定的插件与能力 owner。
    ///
    /// # Errors
    ///
    /// 插件缺失、bundle 被篡改或能力 owner 不一致时返回错误。
    #[cfg(feature = "plugins")]
    pub(crate) fn bind_plugins(
        &self,
        manifests: &[PathBuf],
    ) -> Result<(Vec<PathBuf>, HashMap<String, String>)> {
        self.binding.bind_plugins(manifests)
    }

    /// 读取 Genome Context Policy，并生成按真实插件 ID 隔离的激活元数据。
    ///
    /// # Errors
    ///
    /// 引用或能力 owner 错绑，或真实 CAS 制品无法通过完整性与协议校验时返回错误。
    #[cfg(feature = "plugins")]
    pub(crate) async fn plugin_activation_metadata(
        &self,
    ) -> Result<HashMap<String, HashMap<String, String>>> {
        self.binding.plugin_activation_metadata().await
    }

    /// 纯 Core 构建拒绝任何插件行为快照。
    ///
    /// # Errors
    ///
    /// Genome 声明插件或能力 owner 时返回错误。
    #[cfg(not(feature = "plugins"))]
    pub(crate) fn verify_core_only_plugins(&self) -> Result<()> {
        self.binding.verify_core_only_plugins()
    }

    /// 为新会话固定当前 Genome，或校验已有会话已经固定到同一修订。
    ///
    /// 修订号为零且尚未绑定的记录视为未持久化新会话，可以写入绑定；已经持久化但
    /// 缺少绑定的旧会话会被拒绝，避免在 Evidence 模式下静默认领当前 Genome。
    ///
    /// # Errors
    ///
    /// 绑定字段无法构造、已有会话缺少绑定，或绑定的协议与修订不匹配时返回错误。
    pub(crate) fn bind_or_validate_session(&self, record: &mut SessionRecord) -> Result<()> {
        let expected = SessionBehaviorBinding::new(
            GENOME_SESSION_BEHAVIOR_KIND,
            self.revision().revision_id.to_string(),
        )
        .context("构造 Evidence Session 行为绑定失败")?;
        match record.behavior_binding.as_ref() {
            Some(binding) if binding == &expected => Ok(()),
            Some(_) => Err(anyhow!(
                "会话 `{}` 的行为修订与当前 Evidence Genome 不匹配",
                record.id
            )),
            None if record.revision == 0 => {
                record.behavior_binding = Some(expected);
                Ok(())
            }
            None => Err(anyhow!(
                "已有会话 `{}` 缺少行为修订绑定，不能在 Evidence 模式下恢复",
                record.id
            )),
        }
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

/// 从 TUI 配置加载并验证 Evidence Plane。
///
/// 未启用时不创建目录；启用时要求指定精确 Revision ID 或可信 Stable lineage。
///
/// # Errors
///
/// 选择器不合法、两种选择器同时存在、Genome 不存在或被篡改，以及证据根目录无法读取时
/// 返回错误。
pub(crate) async fn load_evidence_runtime(
    settings: &EvidenceSettings,
    config_path: &Path,
    lucia_home: &Path,
) -> Result<Option<EvidenceRuntime>> {
    if !settings.enabled {
        return Ok(None);
    }
    let root = settings
        .root_dir
        .as_deref()
        .map(|path| resolve_config_relative_path(config_path, path))
        .unwrap_or_else(|| lucia_home.join("evolution"));
    let selector = match (
        settings.genome_revision_id.as_deref(),
        settings.genome_stable.as_deref(),
    ) {
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "evidence.genome_revision_id 与 evidence.genome_stable 只能配置一个"
            ));
        }
        (Some(revision), None) => GenomeSelector::Revision(
            GenomeRevisionId::new(revision)
                .context("evidence.genome_revision_id 不是合法的 Genome Revision ID")?,
        ),
        (None, Some(lineage)) => GenomeSelector::Stable(lineage.to_string()),
        (None, None) => {
            return Err(anyhow!(
                "启用 evidence 时必须配置 evidence.genome_revision_id 或 evidence.genome_stable"
            ));
        }
    };
    let revision = FileGenomeResolver::new(&root)
        .resolve(&selector)
        .await
        .with_context(|| format!("解析 Evidence Genome 失败：{selector:?}"))?;
    let health = match &selector {
        GenomeSelector::Stable(lineage) => RuntimeHealthRecorder::from_stable(&root, lineage)
            .await
            .with_context(|| format!("装配 Stable Runtime 健康观察失败：{lineage}"))?,
        GenomeSelector::Revision(_) => None,
    };
    if health
        .as_ref()
        .is_some_and(|recorder| recorder.revision_id() != &revision.revision_id)
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
        EvidenceRuntime::new(
            Arc::new(hub),
            revision,
            artifact_store,
            episode_store,
            &root,
        )?
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
            genome_stable: None,
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
        let fixture_root = root.join("fixtures");
        let artifacts = FileArtifactStore::new(evidence_root.join("artifacts"));
        let revision =
            test_genome_revision(ExecutionPolicy::evaluation(&fixture_root), &artifacts).await;
        genomes.append(&revision).await.expect("应登记 Genome");
        let settings = EvidenceSettings {
            enabled: true,
            root_dir: Some(evidence_root),
            genome_revision_id: Some(revision.revision_id.to_string()),
            genome_stable: None,
        };

        let runtime = load_evidence_runtime(&settings, &root.join("config.toml"), &root)
            .await
            .expect("应加载 Evidence")
            .expect("Evidence 应启用");
        assert_eq!(runtime.hub().active_runs().await, 0);
        assert_eq!(runtime.revision(), &revision);
        assert_eq!(runtime.execution_policy(), &revision.genome.execution);
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
            root_dir: Some(evidence_root),
            genome_revision_id: None,
            genome_stable: Some("stable/general".into()),
        };

        let runtime = load_evidence_runtime(&settings, &root.join("config.toml"), &root)
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
        let runtime = load_evidence_runtime(&settings, &root.join("config.toml"), &root)
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

    /// 新会话应固定当前 Genome，已有会话则必须带有完全一致的绑定。
    #[tokio::test]
    async fn evidence_binds_new_session_and_rejects_unbound_or_mismatched_history() {
        let root = std::env::temp_dir().join(format!(
            "lucia-evidence-binding-{}",
            agent_session::SessionId::generate()
        ));
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let revision = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        let revision_id = revision.revision_id.to_string();
        let evidence = EvidenceRuntime::new(
            Arc::new(EpisodeRecorderHub::new(
                Arc::new(artifacts.clone()),
                Arc::new(FileEpisodeStore::new(root.join("episodes"))),
            )),
            revision,
            artifacts,
            FileEpisodeStore::new(root.join("episodes")),
            &root,
        )
        .expect("应创建 Evidence");
        let mut draft =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建测试会话");

        evidence
            .bind_or_validate_session(&mut draft)
            .expect("新会话应绑定 Genome");
        assert_eq!(
            draft.behavior_binding,
            Some(
                SessionBehaviorBinding::new(GENOME_SESSION_BEHAVIOR_KIND, revision_id)
                    .expect("应构造预期绑定")
            )
        );

        let mut legacy =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建旧会话");
        legacy.revision = 1;
        assert!(evidence
            .bind_or_validate_session(&mut legacy)
            .expect_err("旧会话缺少绑定必须拒绝")
            .to_string()
            .contains("缺少行为修订绑定"));

        let mut mismatched = draft;
        mismatched.behavior_binding = Some(
            SessionBehaviorBinding::new(
                GENOME_SESSION_BEHAVIOR_KIND,
                GenomeRevisionId::generate().to_string(),
            )
            .expect("应构造不匹配绑定"),
        );
        assert!(evidence
            .bind_or_validate_session(&mut mismatched)
            .expect_err("不同 Genome 绑定必须拒绝")
            .to_string()
            .contains("不匹配"));
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
        let evidence = EvidenceRuntime::new(
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
        let evidence = EvidenceRuntime::new(
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
