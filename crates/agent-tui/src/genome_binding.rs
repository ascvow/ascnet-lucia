//! 可信 Genome 与 TUI 真实运行组合之间的单一装配边界。

use agent_core::{
    model::{OpenAiProtocol, ProviderKind},
    AgentOptions, AgentRootConfig,
};
use agent_evolution::{ArtifactStore, FileArtifactStore};
#[cfg(feature = "plugins")]
use agent_evolution::{ContextPolicyRepository, SkillArtifactRepository};
#[cfg(feature = "plugins")]
use agent_evolution_protocol::{ArtifactDigest, PluginGenome, SkillId, SkillStatusV1};
use agent_evolution_protocol::{GenomeRevision, ModelGenome};
#[cfg(feature = "plugins")]
use agent_plugin_host::manifest::{
    resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest,
};
use agent_tool::{ExecutionPolicy, ExecutionProfile, ToolRegistry};
use anyhow::{anyhow, Context, Result};
#[cfg(feature = "plugins")]
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
#[cfg(feature = "plugins")]
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

#[cfg(feature = "plugins")]
const CONTEXT_POLICY_JSON_METADATA_KEY: &str = "context_policy_json";
#[cfg(feature = "plugins")]
const CONTEXT_POLICY_DIGEST_METADATA_KEY: &str = "context_policy_digest";
#[cfg(feature = "plugins")]
const SKILL_SET_JSON_METADATA_KEY: &str = "skill_set_json";
#[cfg(feature = "plugins")]
const SKILL_SET_SCHEMA_VERSION: u32 = 1;
#[cfg(feature = "plugins")]
const SKILL_CAPABILITY_ID: &str = "agent.skills";

/// 交给 Skill Guest 的版本化 Genome Skill Set 信封。
#[cfg(feature = "plugins")]
#[derive(Debug, Serialize)]
struct GenomeSkillSetV1 {
    /// 信封结构版本。
    schema_version: u32,
    /// 原 Candidate 的不可变 Genome Revision ID。
    genome_revision_id: String,
    /// 原 Candidate 的不可变 Genome 行为摘要。
    genome_digest: String,
    /// 本次 Genome 运行所在的可信执行平面。
    execution_profile: ExecutionProfile,
    /// 按强类型 Skill ID 排序的精确制品。
    skills: Vec<InjectedSkillArtifactV1>,
}

/// 一项来自真实 Artifact CAS 的 Skill 制品原始规范 JSON。
#[cfg(feature = "plugins")]
#[derive(Debug, Serialize)]
struct InjectedSkillArtifactV1 {
    /// Genome 引用与制品共同声明的稳定 Skill ID。
    skill_id: String,
    /// 规范 SkillArtifact JSON 的 CAS 摘要。
    artifact_digest: String,
    /// 仓库复核后的原始规范 JSON；Guest 会再次计算摘要。
    artifact_json: String,
}

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
    /// 当前尚无可信装配协议的 Planning 快照时返回错误。
    pub(crate) fn new(revision: GenomeRevision, artifacts: FileArtifactStore) -> Result<Self> {
        Self::new_with_policy(revision, artifacts, None)
    }

    /// 为原 Candidate Revision 创建 Evaluation 运行绑定，不改写其 Genome 或摘要。
    ///
    /// `fixture_root` 只用于构造 Evaluation 平面的文件系统边界；有效策略通过 `restrict`
    /// 单调收紧，原 `revision_id`、`digest` 与 Genome execution 全部原样保留。
    ///
    /// # Errors
    ///
    /// Revision 身份或策略表面无效，或原 Genome 已处于 Mutation 平面时返回错误。
    #[cfg(feature = "plugins")]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "供独立 Evaluation 运行装配入口调用")
    )]
    pub(crate) fn new_for_evaluation(
        revision: GenomeRevision,
        artifacts: FileArtifactStore,
        fixture_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        Self::new_with_policy(
            revision,
            artifacts,
            Some(ExecutionPolicy::evaluation(fixture_root)),
        )
    }

    /// 复用同一身份校验建立普通或受信平面覆盖绑定。
    fn new_with_policy(
        revision: GenomeRevision,
        artifacts: FileArtifactStore,
        runtime_policy: Option<ExecutionPolicy>,
    ) -> Result<Self> {
        verify_runtime_identity(&revision)?;
        verify_supported_policy_surfaces(&revision)?;
        let mut execution_policy = revision.genome.execution.clone();
        execution_policy.tools = execution_policy
            .tools
            .restrict(&revision.genome.tools.access);
        if let Some(runtime_policy) = runtime_policy {
            execution_policy = execution_policy.restrict(&runtime_policy);
            if execution_policy.profile() != ExecutionProfile::Evaluation {
                return Err(anyhow!(
                    "只有 Serve Candidate 可以单调收紧为 Evaluation 运行"
                ));
            }
        }
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

    /// 从真实 CAS 读取 Context Policy 与 Skill Set，并只向各自真实能力 owner 注入。
    ///
    /// 未声明快照时返回空映射并保持普通运行行为。Context Policy 正文和 SkillArtifact
    /// 都由版本化仓库校验；`bound_manifests` 必须是 [`Self::bind_plugins`] 已选择的真实
    /// manifest 集合，Skill Set 只允许存在一个 `agent.skills` provider。
    ///
    /// # Errors
    ///
    /// Context Loader owner 缺失、Skill provider 不唯一、引用与强类型 ID 错绑、Skill 终态
    /// 不允许进入当前执行平面，或 CAS 制品缺失、篡改、过大、非规范 JSON 时返回错误。
    #[cfg(feature = "plugins")]
    pub(crate) async fn plugin_activation_metadata(
        &self,
        bound_manifests: &[PathBuf],
    ) -> Result<HashMap<String, HashMap<String, String>>> {
        let mut metadata = HashMap::<String, HashMap<String, String>>::new();
        if let Some(reference) = self.revision.genome.context_policy.as_ref() {
            let owner = self
                .revision
                .genome
                .capability_owners
                .get(agent_plugin_host::manifest::CONTEXT_LOADER_CAPABILITY)
                .ok_or_else(|| {
                    anyhow!("Context Policy 已声明，但 Genome 缺少 Context Loader owner")
                })?;
            if reference.id != *owner {
                return Err(anyhow!(
                    "Context Policy ID `{}` 与 Context Loader owner `{owner}` 不一致",
                    reference.id
                ));
            }
            if !self
                .revision
                .genome
                .plugins
                .iter()
                .any(|plugin| plugin.id == *owner)
            {
                return Err(anyhow!(
                    "Context Loader owner `{owner}` 不在 Genome 插件组合中"
                ));
            }

            let policy = ContextPolicyRepository::new(&self.artifacts)
                .get(&reference.config_digest)
                .await
                .context("读取并校验 Genome Context Policy 制品失败")?;
            let policy_json = String::from_utf8(
                policy
                    .canonical_bytes()
                    .context("重新编码 Context Policy 规范 JSON 失败")?,
            )
            .context("Context Policy 规范 JSON 不是 UTF-8")?;
            metadata.entry(owner.clone()).or_default().extend([
                (CONTEXT_POLICY_JSON_METADATA_KEY.into(), policy_json),
                (
                    CONTEXT_POLICY_DIGEST_METADATA_KEY.into(),
                    reference.config_digest.to_string(),
                ),
            ]);
        }

        if !self.revision.genome.skills.is_empty() {
            let manifests = self.verified_bound_manifests(bound_manifests)?;
            let providers = manifests
                .iter()
                .filter(|manifest| {
                    manifest
                        .provides
                        .iter()
                        .any(|provided| provided.id == SKILL_CAPABILITY_ID)
                })
                .map(|manifest| manifest.plugin.id.as_str())
                .collect::<Vec<_>>();
            let [provider] = providers.as_slice() else {
                return Err(anyhow!(
                    "Genome Skill Set 要求唯一 `{SKILL_CAPABILITY_ID}` provider，实际为 {} 个",
                    providers.len()
                ));
            };

            let repository = SkillArtifactRepository::new(&self.artifacts);
            let mut skills = Vec::with_capacity(self.revision.genome.skills.len());
            for reference in &self.revision.genome.skills {
                let skill_id = SkillId::new(reference.id.clone())
                    .with_context(|| format!("Genome Skill ID 无效：{}", reference.id))?;
                let artifact = repository
                    .get(&reference.content)
                    .await
                    .with_context(|| format!("读取并校验 Genome Skill `{skill_id}` 制品失败"))?;
                if artifact.skill_id != skill_id {
                    return Err(anyhow!(
                        "Genome Skill ID `{skill_id}` 与制品 ID `{}` 不一致",
                        artifact.skill_id
                    ));
                }
                let final_status = artifact
                    .status_history
                    .last()
                    .map(|transition| transition.status);
                if !skill_status_is_loadable(self.execution_policy.profile(), final_status) {
                    return Err(anyhow!(
                        "Genome Skill `{skill_id}` 终态 {final_status:?} 不能进入 {:?} 运行",
                        self.execution_policy.profile()
                    ));
                }
                let artifact_json = String::from_utf8(
                    artifact
                        .canonical_bytes()
                        .context("重新编码 SkillArtifact 规范 JSON 失败")?,
                )
                .context("SkillArtifact 规范 JSON 不是 UTF-8")?;
                skills.push(InjectedSkillArtifactV1 {
                    skill_id: skill_id.to_string(),
                    artifact_digest: reference.content.to_string(),
                    artifact_json,
                });
            }
            let skill_set_json = serde_json::to_string(&GenomeSkillSetV1 {
                schema_version: SKILL_SET_SCHEMA_VERSION,
                genome_revision_id: self.revision.revision_id.to_string(),
                genome_digest: self.revision.digest.to_string(),
                execution_profile: self.execution_policy.profile(),
                skills,
            })
            .context("编码 Genome Skill Set JSON 失败")?;
            metadata
                .entry((*provider).to_string())
                .or_default()
                .insert(SKILL_SET_JSON_METADATA_KEY.into(), skill_set_json);
        }

        Ok(metadata)
    }

    /// 重新复核调用方传入的绑定 manifest，避免通过目录扫描伪造 Skill provider。
    ///
    /// # Errors
    ///
    /// manifest 数量、ID 或 bundle 与 Genome 固定快照不一致时返回错误。
    #[cfg(feature = "plugins")]
    fn verified_bound_manifests(&self, paths: &[PathBuf]) -> Result<Vec<PluginManifest>> {
        if paths.len() != self.revision.genome.plugins.len() {
            return Err(anyhow!("已绑定插件集合与 Genome 插件数量不一致"));
        }
        let expected = self
            .revision
            .genome
            .plugins
            .iter()
            .map(|plugin| (plugin.id.as_str(), plugin))
            .collect::<HashMap<_, _>>();
        let mut seen = BTreeSet::new();
        let mut manifests = Vec::with_capacity(paths.len());
        for path in paths {
            let manifest = PluginManifest::load(path)
                .with_context(|| format!("重新读取绑定插件 manifest 失败：{}", path.display()))?;
            let snapshot = expected.get(manifest.plugin.id.as_str()).ok_or_else(|| {
                anyhow!("绑定插件 `{}` 不在 Genome 插件组合中", manifest.plugin.id)
            })?;
            if !seen.insert(manifest.plugin.id.clone()) {
                return Err(anyhow!("绑定插件 ID 重复：{}", manifest.plugin.id));
            }
            verify_plugin_snapshot(snapshot, path, &manifest)?;
            manifests.push(manifest);
        }
        Ok(manifests)
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
            || self.revision.genome.context_policy.is_some()
            || !self.revision.genome.skills.is_empty()
        {
            return Err(anyhow!(
                "纯 Core 构建不能运行声明插件、Context Policy 或 Skill 行为的 Genome"
            ));
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

/// 判断 Skill 终态是否允许进入指定可信运行平面。
///
/// Serve 只能装载 Commit Gate 后的 Active；Evaluation 可运行隔离候选及已评测候选；
/// Deprecated、Deleted 和所有 Mutation 运行都不装配 Skill。
#[cfg(feature = "plugins")]
fn skill_status_is_loadable(profile: ExecutionProfile, status: Option<SkillStatusV1>) -> bool {
    match profile {
        ExecutionProfile::Serve => status == Some(SkillStatusV1::Active),
        ExecutionProfile::Evaluation => matches!(
            status,
            Some(SkillStatusV1::Quarantined | SkillStatusV1::Evaluated | SkillStatusV1::Active)
        ),
        ExecutionProfile::Mutation => false,
    }
}

/// 当前尚无跨插件可信快照服务的 Planning 字段不得被伪装成已装配行为。
fn verify_supported_policy_surfaces(revision: &GenomeRevision) -> Result<()> {
    let genome = &revision.genome;
    if genome.prompt.messages.is_empty() {
        return Err(anyhow!(
            "Evidence Genome 必须把完整系统 Prompt 固定为至少一个 CAS 制品"
        ));
    }
    if genome.planning_policy.is_some() {
        return Err(anyhow!(
            "当前 TUI 尚不能可信装配 Genome 的 Planning Policy 独立快照"
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
    #[cfg(feature = "plugins")]
    use agent_evolution_protocol::{EpisodeId, EvaluationReportId, MutationId, SkillRef};
    #[cfg(feature = "plugins")]
    use agent_evolution_protocol::{
        SkillArtifactV1, SkillOperationV1, SkillStatusTransitionV1, SkillTriggerPolicyV1,
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

    /// 构造 TUI 真实 CAS 装配测试使用的合法 SkillArtifact。
    #[cfg(feature = "plugins")]
    fn skill_artifact(skill_id: &str, final_status: SkillStatusV1) -> SkillArtifactV1 {
        let report_id = EvaluationReportId::generate();
        let mut status_history = vec![SkillStatusTransitionV1 {
            status: SkillStatusV1::Quarantined,
            recorded_at_ms: 1,
            evaluation_report_id: None,
        }];
        if matches!(
            final_status,
            SkillStatusV1::Evaluated | SkillStatusV1::Active
        ) {
            status_history.push(SkillStatusTransitionV1 {
                status: SkillStatusV1::Evaluated,
                recorded_at_ms: 2,
                evaluation_report_id: Some(report_id.clone()),
            });
        }
        if final_status == SkillStatusV1::Active {
            status_history.push(SkillStatusTransitionV1 {
                status: SkillStatusV1::Active,
                recorded_at_ms: 3,
                evaluation_report_id: Some(report_id),
            });
        }
        SkillArtifactV1 {
            schema_version: agent_evolution_protocol::SKILL_ARTIFACT_SCHEMA_VERSION,
            skill_id: SkillId::new(skill_id).expect("测试 Skill ID 应合法"),
            revision: 1,
            operation: SkillOperationV1::Create,
            name: format!("skill-{skill_id}"),
            description: "验证真实 CAS Skill 装配。".into(),
            instructions: "只执行 CAS 固定的测试指令。".into(),
            trigger_policy: SkillTriggerPolicyV1::default(),
            required_capabilities: BTreeSet::new(),
            source_episode_ids: BTreeSet::from([EpisodeId::generate()]),
            mutation_id: MutationId::generate(),
            status_history,
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
    #[tokio::test]
    async fn genome_selects_verified_plugins_and_context_policy() {
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
        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let policy = agent_evolution_protocol::ContextPolicyV1::default();
        let policy_artifact = ContextPolicyRepository::new(&artifacts)
            .put(&policy)
            .await
            .expect("应写入 Context Policy CAS");
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
        revision.genome.context_policy = Some(agent_evolution_protocol::PolicyRef {
            id: "selected".into(),
            config_digest: policy_artifact.digest.clone(),
        });
        revision.digest = revision.genome.digest().expect("应重算 Genome 摘要");
        let binding = GenomeRuntimeBinding::new(revision, artifacts).expect("应创建插件绑定");

        let (paths, owners) = binding
            .bind_plugins(&[extra.join("plugin.toml"), selected.join("plugin.toml")])
            .expect("应选择并验证 Genome 插件");

        assert_eq!(paths, vec![selected.join("plugin.toml")]);
        assert_eq!(
            owners.get("agent.context-loader").map(String::as_str),
            Some("selected")
        );
        let activation_metadata = binding
            .plugin_activation_metadata(&paths)
            .await
            .expect("应从真实 CAS 生成 Context Policy 激活元数据");
        let context_metadata = activation_metadata
            .get("selected")
            .expect("只应注入真实 owner");
        let policy_digest = policy_artifact.digest.to_string();
        assert_eq!(
            context_metadata
                .get(CONTEXT_POLICY_DIGEST_METADATA_KEY)
                .map(String::as_str),
            Some(policy_digest.as_str())
        );
        assert_eq!(
            serde_json::from_str::<agent_evolution_protocol::ContextPolicyV1>(
                context_metadata
                    .get(CONTEXT_POLICY_JSON_METADATA_KEY)
                    .expect("应注入策略 JSON")
            )
            .expect("注入策略应保持版本化 JSON"),
            policy
        );

        std::fs::write(selected.join("selected.wasm"), b"tampered").expect("应篡改目标 WASM");
        let error = binding
            .bind_plugins(&[selected.join("plugin.toml")])
            .expect_err("bundle 篡改必须拒绝");
        assert!(error.to_string().contains("bundle 摘要"));
        std::fs::remove_dir_all(root).expect("应清理插件绑定目录");
    }

    /// Genome Skill Set 只注入唯一真实 provider，并按运行平面执行状态门禁。
    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn genome_injects_verified_skill_set_for_unique_provider_and_profile() {
        let root = std::env::temp_dir().join(format!(
            "lucia-genome-skills-{}",
            agent_session::SessionId::generate()
        ));
        let provider = root.join("skill-provider");
        std::fs::create_dir_all(&provider).expect("应创建 Skill provider 目录");
        std::fs::write(provider.join("skill.wasm"), b"verified-skill-wasm")
            .expect("应写入 Skill provider WASM");
        std::fs::write(
            provider.join("plugin.toml"),
            r#"
                [plugin]
                id = "skill-provider"
                name = "Skill Provider"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "skill.wasm"

                [[provides]]
                id = "agent.skills"
                version = "1.0.0"
                mode = "multi"
            "#,
        )
        .expect("应写入 Skill provider manifest");

        let artifacts = FileArtifactStore::new(root.join("artifacts"));
        let active = skill_artifact("skill_runtime1", SkillStatusV1::Active);
        let active_ref = SkillArtifactRepository::new(&artifacts)
            .put(&active)
            .await
            .expect("应写入 Active SkillArtifact CAS");
        let bundle = agent_plugin_manager::hash_plugin_bundle(&provider)
            .expect("应计算 Skill provider bundle 摘要");
        let mut revision = revision(model(), fixed_prompt(), ToolProfileGenome::default());
        revision.genome.plugins = vec![PluginGenome {
            id: "skill-provider".into(),
            version: "1.0.0".into(),
            api_version: "0.7.0".into(),
            bundle: ArtifactDigest::from_sha256_hex(bundle).expect("bundle 摘要应合法"),
            config_digest: None,
        }];
        revision.genome.skills = vec![SkillRef {
            id: active.skill_id.to_string(),
            content: active_ref.digest.clone(),
        }];
        revision.digest = revision.genome.digest().expect("应重算 Genome 摘要");
        let binding =
            GenomeRuntimeBinding::new(revision, artifacts.clone()).expect("应创建 Skill 绑定");
        let (paths, _) = binding
            .bind_plugins(&[provider.join("plugin.toml")])
            .expect("应绑定真实 Skill provider");
        let metadata = binding
            .plugin_activation_metadata(&paths)
            .await
            .expect("Serve 应装配 Active Skill");
        assert_eq!(metadata.len(), 1);
        let skill_set: Value = serde_json::from_str(
            metadata
                .get("skill-provider")
                .and_then(|values| values.get(SKILL_SET_JSON_METADATA_KEY))
                .expect("只应向唯一真实 provider 注入 Skill Set"),
        )
        .expect("注入值应为版本化 JSON");
        assert_eq!(skill_set["schema_version"], SKILL_SET_SCHEMA_VERSION);
        assert_eq!(skill_set["execution_profile"], "serve");
        assert_eq!(skill_set["skills"][0]["skill_id"], active.skill_id.as_str());
        assert_eq!(
            skill_set["skills"][0]["artifact_digest"],
            active_ref.digest.to_string()
        );

        let quarantined = skill_artifact("skill_candidate1", SkillStatusV1::Quarantined);
        let quarantined_ref = SkillArtifactRepository::new(&artifacts)
            .put(&quarantined)
            .await
            .expect("应写入 Quarantined SkillArtifact CAS");
        let mut candidate_genome = binding.revision().genome.clone();
        candidate_genome.skills = vec![SkillRef {
            id: quarantined.skill_id.to_string(),
            content: quarantined_ref.digest,
        }];
        let candidate_revision =
            GenomeRevision::create(candidate_genome, GenomeMetadata::default())
                .expect("应创建保持 Serve execution 的 Candidate Revision");
        let candidate_revision_id = candidate_revision.revision_id.to_string();
        let candidate_digest = candidate_revision.digest.to_string();
        let evaluation = GenomeRuntimeBinding::new_for_evaluation(
            candidate_revision.clone(),
            artifacts.clone(),
            root.join("fixtures"),
        )
        .expect("应以原 Candidate 创建 Evaluation Skill 绑定");
        assert_eq!(evaluation.revision(), &candidate_revision);
        assert_eq!(
            evaluation.revision().genome.execution.profile(),
            ExecutionProfile::Serve
        );
        assert_eq!(
            evaluation.execution_policy().profile(),
            ExecutionProfile::Evaluation
        );
        let evaluation_metadata = evaluation
            .plugin_activation_metadata(&paths)
            .await
            .expect("Evaluation 应允许装配 Quarantined Candidate");
        let evaluation_set: Value = serde_json::from_str(
            evaluation_metadata["skill-provider"][SKILL_SET_JSON_METADATA_KEY].as_str(),
        )
        .expect("Evaluation Skill Set 应可解析");
        assert_eq!(evaluation_set["execution_profile"], "evaluation");
        assert_eq!(evaluation_set["genome_revision_id"], candidate_revision_id);
        assert_eq!(evaluation_set["genome_digest"], candidate_digest);

        let serve = GenomeRuntimeBinding::new(candidate_revision, artifacts.clone())
            .expect("应创建 Serve Skill 绑定");
        let error = serve
            .plugin_activation_metadata(&paths)
            .await
            .expect_err("Serve 必须拒绝 Quarantined Skill");
        assert!(error.to_string().contains("不能进入 Serve 运行"));

        let duplicate = root.join("skill-provider-2");
        std::fs::create_dir_all(&duplicate).expect("应创建重复 provider 目录");
        std::fs::write(duplicate.join("skill.wasm"), b"duplicate-skill-wasm")
            .expect("应写入重复 provider WASM");
        std::fs::write(
            duplicate.join("plugin.toml"),
            r#"
                [plugin]
                id = "skill-provider-2"
                name = "Duplicate Skill Provider"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "skill.wasm"

                [[provides]]
                id = "agent.skills"
                version = "1.0.0"
                mode = "multi"
            "#,
        )
        .expect("应写入重复 provider manifest");
        let duplicate_bundle = agent_plugin_manager::hash_plugin_bundle(&duplicate)
            .expect("应计算重复 provider bundle 摘要");
        let mut duplicate_revision = binding.revision().clone();
        duplicate_revision.genome.plugins.push(PluginGenome {
            id: "skill-provider-2".into(),
            version: "1.0.0".into(),
            api_version: "0.7.0".into(),
            bundle: ArtifactDigest::from_sha256_hex(duplicate_bundle)
                .expect("重复 provider bundle 摘要应合法"),
            config_digest: None,
        });
        duplicate_revision.digest = duplicate_revision
            .genome
            .digest()
            .expect("应重算重复 provider Genome 摘要");
        let duplicate_binding = GenomeRuntimeBinding::new(duplicate_revision, artifacts.clone())
            .expect("应创建重复 provider 绑定");
        let (duplicate_paths, _) = duplicate_binding
            .bind_plugins(&[provider.join("plugin.toml"), duplicate.join("plugin.toml")])
            .expect("multi provider 可完成通用能力解析");
        let error = duplicate_binding
            .plugin_activation_metadata(&duplicate_paths)
            .await
            .expect_err("Evidence Skill Set 必须拒绝多个真实 provider");
        assert!(error.to_string().contains("实际为 2 个"));

        let unrelated = root.join("unrelated-plugin");
        std::fs::create_dir_all(&unrelated).expect("应创建无 Skill 能力插件目录");
        std::fs::write(unrelated.join("plugin.wasm"), b"unrelated-wasm")
            .expect("应写入无 Skill 能力插件 WASM");
        std::fs::write(
            unrelated.join("plugin.toml"),
            r#"
                [plugin]
                id = "unrelated-plugin"
                name = "Unrelated Plugin"
                version = "1.0.0"
                api_version = "0.7.0"
                wasm = "plugin.wasm"
            "#,
        )
        .expect("应写入无 Skill 能力插件 manifest");
        let unrelated_bundle = agent_plugin_manager::hash_plugin_bundle(&unrelated)
            .expect("应计算无 Skill 能力插件 bundle 摘要");
        let mut missing_revision = binding.revision().clone();
        missing_revision.genome.plugins = vec![PluginGenome {
            id: "unrelated-plugin".into(),
            version: "1.0.0".into(),
            api_version: "0.7.0".into(),
            bundle: ArtifactDigest::from_sha256_hex(unrelated_bundle)
                .expect("无 Skill 能力插件 bundle 摘要应合法"),
            config_digest: None,
        }];
        missing_revision.digest = missing_revision
            .genome
            .digest()
            .expect("应重算缺少 provider 的 Genome 摘要");
        let missing_binding = GenomeRuntimeBinding::new(missing_revision, artifacts)
            .expect("应创建缺少 provider 的绑定");
        let (missing_paths, _) = missing_binding
            .bind_plugins(&[unrelated.join("plugin.toml")])
            .expect("无关插件仍应通过通用绑定");
        let error = missing_binding
            .plugin_activation_metadata(&missing_paths)
            .await
            .expect_err("Evidence Skill Set 必须拒绝缺少真实 provider");
        assert!(error.to_string().contains("实际为 0 个"));

        std::fs::remove_dir_all(root).expect("应清理 Skill 装配目录");
    }
}
