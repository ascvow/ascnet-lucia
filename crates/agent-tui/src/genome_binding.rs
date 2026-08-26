//! 可信 Genome 与 TUI 真实运行组合之间的单一装配边界。

use agent_core::{
    model::{OpenAiProtocol, ProviderKind},
    AgentOptions, AgentRootConfig,
};
use agent_evolution::{ArtifactStore, FileArtifactStore};
#[cfg(feature = "plugins")]
use agent_evolution_protocol::{ArtifactDigest, PluginGenome};
use agent_evolution_protocol::{GenomeRevision, ModelGenome};
#[cfg(feature = "plugins")]
use agent_plugin_host::manifest::{
    resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest,
};
use agent_tool::{ExecutionPolicy, ToolRegistry};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
#[cfg(feature = "plugins")]
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

/// 启动后不可替换的 Genome 运行装配器。
///
/// Revision 已由 Resolver 校验摘要；本类型继续把其中的行为字段变成真实模型、Prompt、
/// 工具和插件组合。普通 TOML 只保留凭据等非 Genome 数据，不能覆盖这些行为输入。
#[derive(Debug, Clone)]
pub(crate) struct GenomeRuntimeBinding {
    revision: GenomeRevision,
    artifacts: FileArtifactStore,
    execution_policy: ExecutionPolicy,
}

impl GenomeRuntimeBinding {
    /// 为已验证 Revision 创建运行装配器，并检查当前 TUI 能证明的 Kernel 身份。
    ///
    /// # Errors
    ///
    /// 包版本、Git 身份、目标平台或 feature 集合与当前二进制不一致，或 Genome 声明了
    /// 当前尚无可信装配协议的 Context、Planning、Skill 快照时返回错误。
    pub(crate) fn new(revision: GenomeRevision, artifacts: FileArtifactStore) -> Result<Self> {
        verify_runtime_identity(&revision)?;
        verify_supported_policy_surfaces(&revision)?;
        let mut execution_policy = revision.genome.execution.clone();
        execution_policy.tools = execution_policy
            .tools
            .restrict(&revision.genome.tools.access);
        Ok(Self {
            revision,
            artifacts,
            execution_policy,
        })
    }

    /// 返回本次进程固定使用的 Genome Revision。
    pub(crate) fn revision(&self) -> &GenomeRevision {
        &self.revision
    }

    /// 返回合并 Tool Profile 后的有效执行策略。
    pub(crate) fn execution_policy(&self) -> &ExecutionPolicy {
        &self.execution_policy
    }

    /// 用 Genome 的适配器行为覆盖普通模型配置，同时保留 API Key 来源。
    ///
    /// `api_key` 与 `api_key_env` 不进入 Genome；额外 Header 可能改变请求语义且可能携带
    /// Secret，因此 Evidence 模式暂不接受，避免产生无法由 Revision 解释的行为。
    ///
    /// # Errors
    ///
    /// provider 类型或协议值不受支持，或配置含额外 Header 时返回错误。
    pub(crate) fn bind_model_config(&self, mut config: AgentRootConfig) -> Result<AgentRootConfig> {
        if !config.model.extra_headers.is_empty() {
            return Err(anyhow!(
                "Evidence 模式不允许未进入 Genome 的 model.extra_headers"
            ));
        }
        let model = &self.revision.genome.model;
        config.model.name = model.provider.clone();
        config.model.provider = parse_provider_kind(&model.provider_kind)?;
        config.model.model = model.model.clone();
        config.model.base_url = model.base_url.clone();
        config.model.openai_protocol = parse_model_protocol(model)?;
        Ok(config)
    }

    /// 校验显式 Demo 路由，并把其余行为字段从 Genome 装配进 Agent 选项。
    ///
    /// # Errors
    ///
    /// Genome 未声明内置脚本模型，或 Prompt、Provider Options 制品不可读取时返回错误。
    pub(crate) async fn bind_demo_options(&self, options: AgentOptions) -> Result<AgentOptions> {
        let model = &self.revision.genome.model;
        if model.provider != "default"
            || model.provider_kind != "scripted-demo"
            || model.base_url.is_some()
            || model.protocol.is_some()
        {
            return Err(anyhow!(
                "Evidence Genome 的模型路由与 --demo 内置脚本模型不一致"
            ));
        }
        self.bind_agent_options(options).await
    }

    /// 把 Genome 中的模型请求参数、Prompt CAS 和资源边界装配为真实 Agent 选项。
    ///
    /// 普通配置中的 system prompt、provider options 和采样参数不会进入 Evidence Run；
    /// 完整 system prompt 必须由 Genome 按顺序引用 CAS 制品。
    ///
    /// # Errors
    ///
    /// Prompt 或 Provider Options 制品缺失、摘要不符、格式非法，或温度无法解析时返回错误。
    pub(crate) async fn bind_agent_options(
        &self,
        mut options: AgentOptions,
    ) -> Result<AgentOptions> {
        let model = &self.revision.genome.model;
        options.provider = model.provider.clone();
        options.model = model.model.clone();
        options.max_steps = self.execution_policy.limits.max_steps.unwrap_or(0);
        options.max_tokens = model.max_tokens;
        options.stream = model.stream;
        options.temperature = parse_temperature(model.temperature.as_deref())?;
        options.provider_options = self.load_provider_options().await?;
        options.system_prompt = self.assemble_system_prompt().await?;
        Ok(options.with_execution_policy(self.execution_policy.clone()))
    }

    /// 按 Genome 中的原生工具集合收缩 TUI 已注册工具，未知工具会拒绝启动。
    ///
    /// # Errors
    ///
    /// Genome 引用了当前 Kernel 未注册的工具或包含重复工具名时返回错误。
    pub(crate) fn bind_native_tools(&self, tools: &ToolRegistry) -> Result<ToolRegistry> {
        tools
            .subset(&self.revision.genome.tools.native_tools)
            .context("Genome 原生工具集合与当前 Kernel 不兼容")
    }

    /// 从已发现插件中选择并验证 Genome 固定的 bundle 与能力 owner。
    ///
    /// 额外发现的插件不会进入运行组合；缺失或摘要不符的插件会在 component 实例化前
    /// 拒绝启动。当前 Host 的插件配置都位于 bundle 内并已被 bundle 摘要覆盖，因此不
    /// 接受另行声明的 `config_digest`。
    ///
    /// # Errors
    ///
    /// manifest、bundle、依赖或能力解析失败，或任一快照与 Genome 不一致时返回错误。
    #[cfg(feature = "plugins")]
    pub(crate) fn bind_plugins(
        &self,
        discovered: &[PathBuf],
    ) -> Result<(Vec<PathBuf>, HashMap<String, String>)> {
        let mut by_id = HashMap::new();
        for path in discovered {
            let manifest = PluginManifest::load(path)
                .with_context(|| format!("读取候选插件 manifest 失败：{}", path.display()))?;
            if by_id
                .insert(manifest.plugin.id.clone(), (path.clone(), manifest))
                .is_some()
            {
                return Err(anyhow!("发现重复插件 ID，不能绑定 Genome"));
            }
        }

        let mut selected_paths = Vec::with_capacity(self.revision.genome.plugins.len());
        let mut selected_manifests = Vec::with_capacity(self.revision.genome.plugins.len());
        for expected in &self.revision.genome.plugins {
            if expected.config_digest.is_some() {
                return Err(anyhow!(
                    "插件 `{}` 声明了当前 Host 尚无独立装配协议的 config_digest",
                    expected.id
                ));
            }
            let (path, manifest) = by_id
                .get(&expected.id)
                .ok_or_else(|| anyhow!("Genome 固定的插件 `{}` 未被发现", expected.id))?;
            verify_plugin_snapshot(expected, path, manifest)?;
            selected_paths.push(path.clone());
            selected_manifests.push(manifest.clone());
        }

        resolve_plugin_load_order(&selected_manifests).context("Genome 插件依赖关系无效")?;
        let selections = self
            .revision
            .genome
            .capability_owners
            .iter()
            .map(|(capability, owner)| (capability.clone(), owner.clone()))
            .collect::<HashMap<_, _>>();
        let resolved = resolve_plugin_capabilities(&selected_manifests, &selections)
            .context("Genome 插件能力 owner 无法解析")?;
        let actual_owners = resolved
            .exclusive_owners()
            .map(|(capability, owner)| (capability.to_string(), owner.to_string()))
            .collect::<BTreeMap<_, _>>();
        if actual_owners != self.revision.genome.capability_owners {
            return Err(anyhow!(
                "Genome capability_owners 与真实独占能力解析结果不一致"
            ));
        }
        Ok((selected_paths, selections))
    }

    /// 纯 Core 构建必须拒绝声明了插件行为的 Genome。
    ///
    /// # Errors
    ///
    /// Genome 包含插件或能力 owner 时返回错误。
    #[cfg(not(feature = "plugins"))]
    pub(crate) fn verify_core_only_plugins(&self) -> Result<()> {
        if !self.revision.genome.plugins.is_empty()
            || !self.revision.genome.capability_owners.is_empty()
        {
            return Err(anyhow!("纯 Core 构建不能运行声明插件行为的 Genome"));
        }
        Ok(())
    }

    /// 从 CAS 读取并解析服务商专属选项；未声明时固定为空对象。
    async fn load_provider_options(&self) -> Result<Value> {
        let Some(digest) = self.revision.genome.model.provider_options_digest.as_ref() else {
            return Ok(Value::Object(Default::default()));
        };
        let bytes = self
            .artifacts
            .get(digest)
            .await
            .context("读取 Genome Provider Options 制品失败")?
            .ok_or_else(|| anyhow!("Genome Provider Options 制品不存在：{digest}"))?;
        let value: Value =
            serde_json::from_slice(&bytes).context("解析 Provider Options JSON 失败")?;
        if !value.is_object() {
            return Err(anyhow!("Genome Provider Options 制品必须是 JSON 对象"));
        }
        Ok(value)
    }

    /// 按 Genome 顺序读取 UTF-8 Prompt 制品并确定性拼接。
    async fn assemble_system_prompt(&self) -> Result<String> {
        let mut parts = Vec::with_capacity(self.revision.genome.prompt.messages.len());
        for message in &self.revision.genome.prompt.messages {
            let bytes = self
                .artifacts
                .get(&message.artifact)
                .await
                .with_context(|| format!("读取 {:?} Prompt 制品失败", message.layer))?
                .ok_or_else(|| anyhow!("Prompt 制品不存在：{}", message.artifact))?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("{:?} Prompt 制品不是 UTF-8", message.layer))?;
            parts.push(text);
        }
        Ok(parts.join("\n\n"))
    }
}

/// 校验当前可证明的 Runtime Identity，避免同一 Revision 跨 Kernel 静默复用。
fn verify_runtime_identity(revision: &GenomeRevision) -> Result<()> {
    let runtime = &revision.genome.runtime;
    if runtime.package_version != env!("CARGO_PKG_VERSION") {
        return Err(anyhow!(
            "Genome package_version `{}` 与当前 Lucia `{}` 不一致",
            runtime.package_version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    let target = current_target_triple();
    if runtime.target_triple != target {
        return Err(anyhow!(
            "Genome target_triple `{}` 与当前目标 `{target}` 不一致",
            runtime.target_triple
        ));
    }
    let features = current_tui_features();
    if runtime.features != features {
        return Err(anyhow!("Genome features 与当前 Lucia 构建不一致"));
    }
    verify_git_commit(&runtime.git_commit, current_git_commit())?;
    let git_dirty = current_git_dirty();
    if runtime.git_dirty != git_dirty {
        return Err(anyhow!(
            "Genome git_dirty `{}` 与当前 Lucia 构建 `{git_dirty}` 不一致",
            runtime.git_dirty
        ));
    }
    Ok(())
}

/// 复核 Genome 声明的提交号与编译产物一致，并拒绝无法唯一标识源码的归档构建。
fn verify_git_commit(declared: &str, build: &str) -> Result<()> {
    if build == "unknown" {
        return Err(anyhow!(
            "当前 Lucia 编译产物缺少可验证的 Git commit，不能启动 Evidence"
        ));
    }
    if declared != build {
        return Err(anyhow!(
            "Genome git_commit `{declared}` 与当前 Lucia 构建 `{build}` 不一致"
        ));
    }
    Ok(())
}

/// 当前尚无跨插件可信快照服务的策略字段不得被伪装成已装配行为。
fn verify_supported_policy_surfaces(revision: &GenomeRevision) -> Result<()> {
    let genome = &revision.genome;
    if genome.prompt.messages.is_empty() {
        return Err(anyhow!(
            "Evidence Genome 必须把完整系统 Prompt 固定为至少一个 CAS 制品"
        ));
    }
    if genome.context_policy.is_some()
        || genome.planning_policy.is_some()
        || !genome.skills.is_empty()
    {
        return Err(anyhow!(
            "当前 TUI 尚不能可信装配 Genome 的 Context、Planning 或 Skill 独立快照"
        ));
    }
    Ok(())
}

/// 把 Genome 的 provider 类型字符串解析为 Core 适配器类型。
fn parse_provider_kind(value: &str) -> Result<ProviderKind> {
    match value {
        "open-ai" => Ok(ProviderKind::OpenAi),
        "open-ai-compatible" => Ok(ProviderKind::OpenAiCompatible),
        "anthropic" => Ok(ProviderKind::Anthropic),
        other => Err(anyhow!("Genome provider_kind 不受支持：`{other}`")),
    }
}

/// 解析模型协议；OpenAI 类必须显式固定协议，Anthropic 不接受无意义协议字段。
fn parse_model_protocol(model: &ModelGenome) -> Result<OpenAiProtocol> {
    match (model.provider_kind.as_str(), model.protocol.as_deref()) {
        ("open-ai" | "open-ai-compatible", Some("responses")) => Ok(OpenAiProtocol::Responses),
        ("open-ai" | "open-ai-compatible", Some("chat-completions")) => {
            Ok(OpenAiProtocol::ChatCompletions)
        }
        ("open-ai" | "open-ai-compatible", Some(other)) => {
            Err(anyhow!("Genome OpenAI 协议不受支持：`{other}`"))
        }
        ("open-ai" | "open-ai-compatible", None) => {
            Err(anyhow!("Genome 的 OpenAI 模型必须显式固定 protocol"))
        }
        ("anthropic", None) => Ok(OpenAiProtocol::Responses),
        ("anthropic", Some(_)) => Err(anyhow!("Anthropic Genome 不应声明 OpenAI protocol")),
        (other, _) => Err(anyhow!("Genome provider_kind 不受支持：`{other}`")),
    }
}

/// 把稳定字符串温度转换为 Core 请求类型，并拒绝非有限值。
fn parse_temperature(value: Option<&str>) -> Result<Option<f32>> {
    value
        .map(|text| {
            let temperature = text
                .parse::<f32>()
                .with_context(|| format!("Genome temperature 不是有效数字：`{text}`"))?;
            if !temperature.is_finite() {
                return Err(anyhow!("Genome temperature 必须是有限数字"));
            }
            Ok(temperature)
        })
        .transpose()
}

/// 返回当前 TUI Cargo feature 快照。
pub(crate) fn current_tui_features() -> BTreeSet<String> {
    let mut features = BTreeSet::new();
    if cfg!(feature = "plugins") {
        features.insert("plugins".to_string());
    }
    features
}

/// 返回当前分发目标使用的 Rust target triple。
pub(crate) fn current_target_triple() -> String {
    env!("LUCIA_BUILD_TARGET").to_string()
}

/// 返回编译产物固定的 Git 提交号；源码归档构建会返回 `unknown`。
pub(crate) fn current_git_commit() -> &'static str {
    env!("LUCIA_BUILD_GIT_COMMIT")
}

/// 返回编译产物固定的工作树状态；无法证明干净的构建会被标记为 dirty。
pub(crate) fn current_git_dirty() -> bool {
    env!("LUCIA_BUILD_GIT_DIRTY") == "true"
}

/// 复核单个实际插件的身份与完整 bundle 摘要。
#[cfg(feature = "plugins")]
fn verify_plugin_snapshot(
    expected: &PluginGenome,
    manifest_path: &Path,
    manifest: &PluginManifest,
) -> Result<()> {
    if manifest.plugin.id != expected.id
        || manifest.plugin.version != expected.version
        || manifest.plugin.api_version != expected.api_version
    {
        return Err(anyhow!(
            "插件 `{}` 的 manifest 身份与 Genome 不一致",
            expected.id
        ));
    }
    let bundle_root = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("插件 manifest 缺少 bundle 根目录"))?;
    let hex = agent_plugin_manager::hash_plugin_bundle(bundle_root)
        .with_context(|| format!("计算插件 `{}` bundle 摘要失败", expected.id))?;
    let actual =
        ArtifactDigest::from_sha256_hex(hex).context("Plugin Manager 返回了非法 bundle 摘要")?;
    if actual != expected.bundle {
        return Err(anyhow!(
            "插件 `{}` 的 bundle 摘要与 Genome 不一致",
            expected.id
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "plugins")]
    use agent_evolution_protocol::PluginGenome;
    use agent_evolution_protocol::{
        AgentGenome, ArtifactDigest, GenomeMetadata, ModelGenome, PromptArtifactRef, PromptGenome,
        PromptLayer, RuntimeIdentity, ToolProfileGenome, GENOME_SCHEMA_VERSION,
    };
    use agent_tool::{JsonTool, ToolSpec};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// 构造绑定测试使用的当前 Kernel Genome。
    fn revision(
        model: ModelGenome,
        prompt: PromptGenome,
        tools: ToolProfileGenome,
    ) -> GenomeRevision {
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
                model,
                prompt,
                plugins: Vec::new(),
                capability_owners: BTreeMap::new(),
                tools,
                context_policy: None,
                planning_policy: None,
                skills: Vec::new(),
                execution: ExecutionPolicy::serve(),
            },
            GenomeMetadata::default(),
        )
        .expect("绑定测试 Genome 应合法")
    }

    /// 创建无需联网的标准模型快照。
    fn model() -> ModelGenome {
        ModelGenome {
            provider: "default".into(),
            provider_kind: "open-ai".into(),
            model: "genome-model".into(),
            base_url: Some("https://example.invalid/v1".into()),
            protocol: Some("responses".into()),
            max_tokens: Some(512),
            temperature: Some("0.25".into()),
            stream: false,
            provider_options_digest: None,
        }
    }

    /// 构造无需在当前测试读取正文的固定 Prompt 引用。
    fn fixed_prompt() -> PromptGenome {
        PromptGenome {
            messages: vec![PromptArtifactRef {
                layer: PromptLayer::HostProtocol,
                artifact: ArtifactDigest::from_sha256_hex("0".repeat(64))
                    .expect("固定 Prompt 摘要应合法"),
            }],
        }
    }

    /// 模型参数、Prompt 与 Provider Options 必须来自 Genome，而不是普通配置。
    #[tokio::test]
    async fn genome_authoritatively_binds_model_and_prompt() {
        let root = std::env::temp_dir().join(format!(
            "lucia-genome-binding-{}",
            agent_session::SessionId::generate()
        ));
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let prompt = artifacts
            .put("text/plain", "固定策略".as_bytes())
            .await
            .expect("应写入 Prompt");
        let provider_options = artifacts
            .put("application/json", br#"{"reasoning":{"effort":"low"}}"#)
            .await
            .expect("应写入 Provider Options");
        let mut model = model();
        model.provider_options_digest = Some(provider_options.digest);
        let revision = revision(
            model,
            PromptGenome {
                messages: vec![PromptArtifactRef {
                    layer: PromptLayer::TaskStrategy,
                    artifact: prompt.digest,
                }],
            },
            ToolProfileGenome::default(),
        );
        let binding = GenomeRuntimeBinding::new(revision, artifacts).expect("应创建运行绑定");
        let ordinary = AgentOptions {
            system_prompt: "普通配置提示".into(),
            max_tokens: Some(1),
            stream: true,
            ..AgentOptions::default()
        };

        let bound = binding
            .bind_agent_options(ordinary)
            .await
            .expect("应从 Genome 装配 Agent");

        assert_eq!(bound.provider, "default");
        assert_eq!(bound.model, "genome-model");
        assert_eq!(bound.max_tokens, Some(512));
        assert_eq!(bound.temperature, Some(0.25));
        assert!(!bound.stream);
        assert_eq!(bound.system_prompt, "固定策略");
        assert_eq!(
            bound.provider_options,
            json!({"reasoning": {"effort": "low"}})
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Genome 只能选择当前 Kernel 已注册的原生工具，并可排除普通模式的额外工具。
    #[test]
    fn genome_selects_exact_native_tool_subset() {
        let mut tools = ToolRegistry::new();
        for name in ["first", "second"] {
            tools
                .register(JsonTool::new(
                    ToolSpec::new(name, "测试工具", json!({"type": "object"})),
                    |_args| async { Ok(json!({})) },
                ))
                .expect("应注册测试工具");
        }
        let binding = GenomeRuntimeBinding::new(
            revision(
                model(),
                fixed_prompt(),
                ToolProfileGenome {
                    native_tools: BTreeSet::from(["second".to_string()]),
                    ..ToolProfileGenome::default()
                },
            ),
            FileArtifactStore::new("/tmp/lucia-unused-artifacts"),
        )
        .expect("应创建工具绑定");

        let selected = binding.bind_native_tools(&tools).expect("应选择工具子集");
        assert_eq!(
            selected
                .specs()
                .into_iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>(),
            vec!["second"]
        );
    }

    /// 不匹配当前构建的 Runtime Identity 必须在读取任何运行输入前失败。
    #[test]
    fn mismatched_runtime_identity_is_rejected() {
        let mut revision = revision(model(), fixed_prompt(), ToolProfileGenome::default());
        revision.genome.runtime.package_version = "9.9.9".into();
        revision.digest = revision.genome.digest().expect("应重算摘要");

        let error = GenomeRuntimeBinding::new(
            revision,
            FileArtifactStore::new("/tmp/lucia-unused-artifacts"),
        )
        .expect_err("不匹配 Kernel 必须拒绝");
        assert!(error.to_string().contains("package_version"));
    }

    /// Genome 不能用任意非空提交号冒充当前编译产物。
    #[test]
    fn mismatched_git_commit_is_rejected() {
        let mut revision = revision(model(), fixed_prompt(), ToolProfileGenome::default());
        revision.genome.runtime.git_commit = if current_git_commit() == "unknown" {
            "deadbeef".into()
        } else {
            "unknown".into()
        };
        revision.digest = revision.genome.digest().expect("应重算摘要");

        let error = GenomeRuntimeBinding::new(
            revision,
            FileArtifactStore::new("/tmp/lucia-unused-artifacts"),
        )
        .expect_err("不匹配提交号必须拒绝");
        assert!(error.to_string().contains("git_commit"));
    }

    /// 缺少可验证提交号的源码归档构建不能进入 Evidence 平面。
    #[test]
    fn unknown_build_commit_is_rejected() {
        let error = verify_git_commit("unknown", "unknown").expect_err("未知构建必须拒绝");
        assert!(error.to_string().contains("缺少可验证的 Git commit"));
    }

    /// Genome 的 dirty 声明必须与构建时工作树状态完全一致。
    #[test]
    fn mismatched_git_dirty_is_rejected() {
        let mut revision = revision(model(), fixed_prompt(), ToolProfileGenome::default());
        revision.genome.runtime.git_dirty = !current_git_dirty();
        revision.digest = revision.genome.digest().expect("应重算摘要");

        let error = GenomeRuntimeBinding::new(
            revision,
            FileArtifactStore::new("/tmp/lucia-unused-artifacts"),
        )
        .expect_err("不匹配 dirty 状态必须拒绝");
        assert!(error.to_string().contains("git_dirty"));
    }

    /// Evidence 只装配 Genome 中的插件，并复核 bundle 与独占能力 owner。
    #[cfg(feature = "plugins")]
    #[test]
    fn genome_selects_verified_plugins_and_capability_owners() {
        let root = std::env::temp_dir().join(format!(
            "lucia-genome-plugins-{}",
            agent_session::SessionId::generate()
        ));
        let selected = root.join("selected");
        let extra = root.join("extra");
        std::fs::create_dir_all(&selected).expect("应创建目标插件目录");
        std::fs::create_dir_all(&extra).expect("应创建额外插件目录");
        std::fs::write(selected.join("selected.wasm"), b"selected").expect("应写入目标 WASM");
        std::fs::write(
            selected.join("plugin.toml"),
            r#"
                [plugin]
                id = "selected"
                name = "目标插件"
                version = "1.2.3"
                api_version = "0.7.0"
                wasm = "selected.wasm"

                [[provides]]
                id = "agent.context-loader"
                version = "1.0.0"
                mode = "exclusive"
            "#,
        )
        .expect("应写入目标 manifest");
        std::fs::write(extra.join("extra.wasm"), b"extra").expect("应写入额外 WASM");
        std::fs::write(
            extra.join("plugin.toml"),
            r#"
                [plugin]
                id = "extra"
                name = "额外插件"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "extra.wasm"
            "#,
        )
        .expect("应写入额外 manifest");

        let bundle =
            agent_plugin_manager::hash_plugin_bundle(&selected).expect("应计算目标 bundle 摘要");
        let mut revision = revision(model(), fixed_prompt(), ToolProfileGenome::default());
        revision.genome.plugins = vec![PluginGenome {
            id: "selected".into(),
            version: "1.2.3".into(),
            api_version: "0.7.0".into(),
            bundle: ArtifactDigest::from_sha256_hex(bundle).expect("bundle 摘要应合法"),
            config_digest: None,
        }];
        revision.genome.capability_owners =
            BTreeMap::from([("agent.context-loader".into(), "selected".into())]);
        revision.digest = revision.genome.digest().expect("应重算 Genome 摘要");
        let binding =
            GenomeRuntimeBinding::new(revision, FileArtifactStore::new(root.join("artifacts")))
                .expect("应创建插件绑定");

        let (paths, owners) = binding
            .bind_plugins(&[extra.join("plugin.toml"), selected.join("plugin.toml")])
            .expect("应选择并验证 Genome 插件");

        assert_eq!(paths, vec![selected.join("plugin.toml")]);
        assert_eq!(
            owners.get("agent.context-loader").map(String::as_str),
            Some("selected")
        );

        std::fs::write(selected.join("selected.wasm"), b"tampered").expect("应篡改目标 WASM");
        let error = binding
            .bind_plugins(&[selected.join("plugin.toml")])
            .expect_err("bundle 篡改必须拒绝");
        assert!(error.to_string().contains("bundle 摘要"));
        std::fs::remove_dir_all(root).expect("应清理插件绑定目录");
    }
}
