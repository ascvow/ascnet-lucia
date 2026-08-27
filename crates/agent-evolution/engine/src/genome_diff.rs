//! 可信 Parent/Candidate Genome 差异生成与允许表面校验。
//!
//! 本模块只读取已登记的 [`GenomeRevision`] 行为字段，并在比较前验证两侧摘要。
//! Candidate 自报的差异不进入此路径，差异摘要也只包含固定字段名，不泄漏制品正文。

use agent_evolution_protocol::{
    AgentGenome, GenomeDiff, GenomeRevision, GenomeRevisionError, MutationSurface,
    PluginEnvironmentDigest, PluginEnvironmentDigestError, PromptArtifactRef, PromptLayer,
};
use std::collections::BTreeSet;
use thiserror::Error;

/// 可信 Genome 差异生成或允许表面校验失败。
#[derive(Debug, Error)]
pub enum GenomeDiffError {
    /// Parent 修订未通过 Genome 结构与摘要校验。
    #[error("Parent Genome 修订无效：{source}")]
    InvalidParent {
        /// 原始修订校验错误。
        #[source]
        source: GenomeRevisionError,
    },
    /// Candidate 修订未通过 Genome 结构与摘要校验。
    #[error("Candidate Genome 修订无效：{source}")]
    InvalidCandidate {
        /// 原始修订校验错误。
        #[source]
        source: GenomeRevisionError,
    },
    /// 真实差异包含可信策略未授权的变异表面。
    #[error("Genome 差异包含未授权表面：{unauthorized:?}")]
    UnauthorizedMutationSurfaces {
        /// 真实差异中超出允许集合的表面，按协议顺序稳定排列。
        unauthorized: BTreeSet<MutationSurface>,
        /// 由可信比较得出的全部变化表面，不包含 Candidate 自报数据。
        changed_surfaces: BTreeSet<MutationSurface>,
        /// 调用方提供的可信允许集合。
        allowed: BTreeSet<MutationSurface>,
    },
    /// Parent 或 Candidate 的冻结插件环境无法计算摘要。
    #[error("计算冻结插件环境摘要失败：{0}")]
    InvalidPluginEnvironment(#[from] PluginEnvironmentDigestError),
    /// Candidate 改变了 Parent 冻结的插件环境。
    #[error("Candidate 改变了冻结插件环境：parent={parent}，candidate={candidate}")]
    FrozenPluginEnvironmentChanged {
        /// Parent 的插件环境摘要。
        parent: PluginEnvironmentDigest,
        /// Candidate 的插件环境摘要。
        candidate: PluginEnvironmentDigest,
    },
    /// 可信允许集合包含只读兼容用途的遗留插件变异表面。
    #[error("插件变异表面仅用于读取历史数据，不能用于新进化周期")]
    UnsupportedLegacyPluginSurface,
}

/// 从两份真实 Genome 修订生成可信差异。
///
/// 比较前会重新计算并校验 Parent 与 Candidate 的摘要，然后逐个检查全部行为字段。
/// 修订 ID、登记时间和描述等非行为元数据不会产生差异。返回的摘要采用固定表面顺序，
/// 且只说明发生变化的字段类别，不包含 Prompt、Skill 或插件配置正文。
///
/// # Errors
///
/// Parent 或 Candidate 的结构、规范摘要无效时返回 [`GenomeDiffError`]。
pub fn diff_genomes(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
) -> Result<GenomeDiff, GenomeDiffError> {
    parent
        .validate()
        .map_err(|source| GenomeDiffError::InvalidParent { source })?;
    candidate
        .validate()
        .map_err(|source| GenomeDiffError::InvalidCandidate { source })?;

    let AgentGenome {
        schema_version: parent_schema_version,
        runtime: parent_runtime,
        model: parent_model,
        prompt: parent_prompt,
        plugins: parent_plugins,
        capability_owners: parent_capability_owners,
        tools: parent_tools,
        context_policy: parent_context_policy,
        planning_policy: parent_planning_policy,
        skills: parent_skills,
        execution: parent_execution,
    } = &parent.genome;
    let AgentGenome {
        schema_version: candidate_schema_version,
        runtime: candidate_runtime,
        model: candidate_model,
        prompt: candidate_prompt,
        plugins: candidate_plugins,
        capability_owners: candidate_capability_owners,
        tools: candidate_tools,
        context_policy: candidate_context_policy,
        planning_policy: candidate_planning_policy,
        skills: candidate_skills,
        execution: candidate_execution,
    } = &candidate.genome;

    let mut changed_surfaces = BTreeSet::new();
    if parent_schema_version != candidate_schema_version || parent_runtime != candidate_runtime {
        changed_surfaces.insert(MutationSurface::Runtime);
    }
    if parent_model != candidate_model {
        changed_surfaces.insert(MutationSurface::Model);
    }

    compare_prompt_layers(
        &parent_prompt.messages,
        &candidate_prompt.messages,
        &mut changed_surfaces,
    );

    if parent_plugins != candidate_plugins
        || parent_capability_owners != candidate_capability_owners
    {
        changed_surfaces.insert(MutationSurface::Plugin);
    }
    if parent_tools != candidate_tools {
        changed_surfaces.insert(MutationSurface::ToolProfile);
    }
    if parent_context_policy != candidate_context_policy {
        changed_surfaces.insert(MutationSurface::ContextPolicy);
    }
    if parent_planning_policy != candidate_planning_policy {
        changed_surfaces.insert(MutationSurface::PlanningPolicy);
    }
    if parent_skills != candidate_skills {
        changed_surfaces.insert(MutationSurface::Skill);
    }
    if parent_execution != candidate_execution {
        changed_surfaces.insert(MutationSurface::ExecutionProfile);
    }

    let summary = trusted_surface_order()
        .into_iter()
        .filter(|surface| changed_surfaces.contains(surface))
        .map(surface_summary)
        .map(str::to_owned)
        .collect();

    Ok(GenomeDiff {
        changed_surfaces,
        summary,
        artifact: None,
    })
}

/// 校验 Parent/Candidate 的真实差异是否完全位于可信允许表面内。
///
/// 此函数内部调用 [`diff_genomes`]，不接收也不信任 Candidate 自报的
/// `GenomeDiff.changed_surfaces`。校验成功时返回同一份可信差异，便于调用方直接写入
/// Evaluation 制品。
///
/// # Errors
///
/// 修订无效时透传 [`GenomeDiffError::InvalidParent`] 或
/// [`GenomeDiffError::InvalidCandidate`]；存在越界表面时返回
/// [`GenomeDiffError::UnauthorizedMutationSurfaces`]。
pub fn verify_allowed_genome_diff(
    parent: &GenomeRevision,
    candidate: &GenomeRevision,
    allowed_surfaces: &BTreeSet<MutationSurface>,
) -> Result<GenomeDiff, GenomeDiffError> {
    if allowed_surfaces.contains(&MutationSurface::Plugin) {
        return Err(GenomeDiffError::UnsupportedLegacyPluginSurface);
    }
    let diff = diff_genomes(parent, candidate)?;
    let parent_plugin_environment = parent.genome.plugin_environment_snapshot().digest()?;
    let candidate_plugin_environment = candidate.genome.plugin_environment_snapshot().digest()?;
    if parent_plugin_environment != candidate_plugin_environment {
        return Err(GenomeDiffError::FrozenPluginEnvironmentChanged {
            parent: parent_plugin_environment,
            candidate: candidate_plugin_environment,
        });
    }
    let unauthorized = diff
        .changed_surfaces
        .difference(allowed_surfaces)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !unauthorized.is_empty() {
        return Err(GenomeDiffError::UnauthorizedMutationSurfaces {
            unauthorized,
            changed_surfaces: diff.changed_surfaces,
            allowed: allowed_surfaces.clone(),
        });
    }
    Ok(diff)
}

/// 按 Prompt 层比较制品位置和摘要，并把变化映射到唯一责任表面。
fn compare_prompt_layers(
    parent: &[PromptArtifactRef],
    candidate: &[PromptArtifactRef],
    changed_surfaces: &mut BTreeSet<MutationSurface>,
) {
    for (surface, layers) in [
        (
            MutationSurface::TaskStrategyPrompt,
            &[PromptLayer::TaskStrategy][..],
        ),
        (
            MutationSurface::ProtectedPrompt,
            &[
                PromptLayer::HostProtocol,
                PromptLayer::Identity,
                PromptLayer::Safety,
                PromptLayer::ToolContract,
            ][..],
        ),
        (MutationSurface::Skill, &[PromptLayer::Skill][..]),
    ] {
        if prompt_projection(parent, layers) != prompt_projection(candidate, layers) {
            changed_surfaces.insert(surface);
        }
    }
}

/// 提取指定层在完整 Prompt 注入序列中的位置与制品摘要。
///
/// 保留绝对位置可以识别跨层重排；仅比较每层内部顺序会漏掉 Safety 与 Task Strategy
/// 互换位置这类行为变化。
fn prompt_projection<'a>(
    messages: &'a [PromptArtifactRef],
    layers: &[PromptLayer],
) -> Vec<(
    usize,
    PromptLayer,
    &'a agent_evolution_protocol::ArtifactDigest,
)> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| layers.contains(&message.layer))
        .map(|(index, message)| (index, message.layer, &message.artifact))
        .collect()
}

/// 返回摘要使用的固定表面顺序，避免协议枚举声明顺序变化影响审计输出。
fn trusted_surface_order() -> [MutationSurface; 10] {
    [
        MutationSurface::Runtime,
        MutationSurface::Model,
        MutationSurface::TaskStrategyPrompt,
        MutationSurface::ProtectedPrompt,
        MutationSurface::Plugin,
        MutationSurface::ToolProfile,
        MutationSurface::ContextPolicy,
        MutationSurface::PlanningPolicy,
        MutationSurface::Skill,
        MutationSurface::ExecutionProfile,
    ]
}

/// 返回不含任何行为值的固定表面摘要。
fn surface_summary(surface: MutationSurface) -> &'static str {
    match surface {
        MutationSurface::Runtime => "Runtime 或 Genome schema 变化",
        MutationSurface::Model => "模型行为配置变化",
        MutationSurface::TaskStrategyPrompt => "Task Strategy Prompt 变化",
        MutationSurface::ProtectedPrompt => "受保护 Prompt 变化",
        MutationSurface::Plugin => "插件或能力 owner 快照变化",
        MutationSurface::ToolProfile => "工具 Profile 变化",
        MutationSurface::ContextPolicy => "上下文策略变化",
        MutationSurface::PlanningPolicy => "计划策略变化",
        MutationSurface::Skill => "Skill 制品或 Prompt 变化",
        MutationSurface::ExecutionProfile => "执行 Profile 或资源策略变化",
        MutationSurface::Other(_) => "未知行为表面变化",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        ArtifactDigest, GenomeMetadata, ModelGenome, PluginGenome, PolicyRef, PromptGenome,
        RuntimeIdentity, SkillRef, ToolProfileGenome, GENOME_SCHEMA_VERSION,
    };
    use agent_tool::{ExecutionPolicy, ToolAccess};

    /// 构造测试使用的确定性摘要。
    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
    }

    /// 构造覆盖全部可信差异表面的合法 Genome。
    fn sample_genome() -> AgentGenome {
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "parent".into(),
                git_dirty: false,
                target_triple: "aarch64-apple-darwin".into(),
                features: ["plugins".to_string()].into_iter().collect(),
            },
            model: ModelGenome {
                provider: "default".into(),
                provider_kind: "open-ai".into(),
                model: "model-a".into(),
                base_url: None,
                protocol: Some("responses".into()),
                max_tokens: Some(4096),
                temperature: Some("0.2".into()),
                stream: true,
                provider_options_digest: None,
            },
            prompt: PromptGenome {
                messages: vec![
                    PromptArtifactRef {
                        layer: PromptLayer::Safety,
                        artifact: digest('a'),
                    },
                    PromptArtifactRef {
                        layer: PromptLayer::TaskStrategy,
                        artifact: digest('b'),
                    },
                    PromptArtifactRef {
                        layer: PromptLayer::Skill,
                        artifact: digest('c'),
                    },
                ],
            },
            plugins: vec![
                PluginGenome {
                    id: "context".into(),
                    version: "0.1.0".into(),
                    api_version: "0.7.0".into(),
                    bundle: digest('d'),
                    manifest_digest: Some(digest('f')),
                    config_digest: None,
                    capability_profile_digest: Some(digest('1')),
                    load_order: Some(0),
                    hook_order: vec!["before-model".into()],
                },
                PluginGenome {
                    id: "permission".into(),
                    version: "0.1.0".into(),
                    api_version: "0.7.0".into(),
                    bundle: digest('e'),
                    manifest_digest: Some(digest('2')),
                    config_digest: None,
                    capability_profile_digest: Some(digest('3')),
                    load_order: Some(1),
                    hook_order: vec!["before-tool".into()],
                },
            ],
            capability_owners: [("agent.tool-policy".to_string(), "permission".to_string())]
                .into_iter()
                .collect(),
            tools: ToolProfileGenome {
                native_tools: ["read_file".to_string()].into_iter().collect(),
                access: ToolAccess::allowlist(["read_file"]),
            },
            context_policy: None,
            planning_policy: None,
            skills: Vec::new(),
            execution: ExecutionPolicy::serve(),
        }
    }

    /// 用行为配置创建一份摘要一致的修订。
    fn revision(genome: AgentGenome) -> GenomeRevision {
        GenomeRevision::create(genome, GenomeMetadata::default()).expect("测试修订应合法")
    }

    #[test]
    fn only_task_strategy_prompt_is_allowed() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.prompt.messages[1].artifact = digest('f');
        let candidate = revision(candidate_genome);
        let allowed = [MutationSurface::TaskStrategyPrompt].into_iter().collect();

        let diff = verify_allowed_genome_diff(&parent, &candidate, &allowed)
            .expect("只修改任务策略 Prompt 应通过");

        assert_eq!(diff.changed_surfaces, allowed);
        assert_eq!(diff.summary, vec!["Task Strategy Prompt 变化"]);
    }

    #[test]
    fn runtime_change_is_rejected() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.runtime.git_commit = "candidate".into();
        let candidate = revision(candidate_genome);

        assert_unauthorized(&parent, &candidate, MutationSurface::Runtime);
    }

    #[test]
    fn capability_owner_change_is_rejected_as_frozen_environment() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome
            .capability_owners
            .insert("agent.tool-policy".into(), "context".into());
        let candidate = revision(candidate_genome);

        assert!(matches!(
            verify_allowed_genome_diff(&parent, &candidate, &BTreeSet::new()),
            Err(GenomeDiffError::FrozenPluginEnvironmentChanged { .. })
        ));
    }

    #[test]
    fn execution_profile_change_is_rejected() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.execution = ExecutionPolicy::evaluation("/tmp/evolution-fixture");
        let candidate = revision(candidate_genome);

        assert_unauthorized(&parent, &candidate, MutationSurface::ExecutionProfile);
    }

    #[test]
    fn safety_prompt_change_is_rejected() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.prompt.messages[0].artifact = digest('f');
        let candidate = revision(candidate_genome);

        assert_unauthorized(&parent, &candidate, MutationSurface::ProtectedPrompt);
    }

    #[test]
    fn protected_prompt_layer_change_is_rejected() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.prompt.messages[0].layer = PromptLayer::Identity;
        let candidate = revision(candidate_genome);

        assert_unauthorized(&parent, &candidate, MutationSurface::ProtectedPrompt);
    }

    #[test]
    fn multiple_surfaces_have_stable_complete_summary() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.runtime.git_commit = "candidate".into();
        candidate_genome.model.model = "model-b".into();
        candidate_genome.prompt.messages[0].artifact = digest('f');
        candidate_genome.prompt.messages[1].artifact = digest('1');
        candidate_genome.prompt.messages[2].artifact = digest('2');
        candidate_genome.plugins[0].bundle = digest('3');
        candidate_genome
            .tools
            .native_tools
            .insert("write_file".into());
        candidate_genome.context_policy = Some(PolicyRef {
            id: "context".into(),
            config_digest: digest('4'),
        });
        candidate_genome.planning_policy = Some(PolicyRef {
            id: "plan".into(),
            config_digest: digest('5'),
        });
        candidate_genome.skills.push(SkillRef {
            id: "rust".into(),
            content: digest('6'),
        });
        candidate_genome.execution = ExecutionPolicy::evaluation("/tmp/evolution-fixture");
        let candidate = revision(candidate_genome);

        let diff = diff_genomes(&parent, &candidate).expect("多表面差异应可生成");

        assert_eq!(diff.changed_surfaces.len(), 10);
        assert_eq!(
            diff.summary,
            vec![
                "Runtime 或 Genome schema 变化",
                "模型行为配置变化",
                "Task Strategy Prompt 变化",
                "受保护 Prompt 变化",
                "插件或能力 owner 快照变化",
                "工具 Profile 变化",
                "上下文策略变化",
                "计划策略变化",
                "Skill 制品或 Prompt 变化",
                "执行 Profile 或资源策略变化",
            ]
        );
    }

    #[test]
    fn partial_allowlist_reports_complete_trusted_diff() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.runtime.git_commit = "candidate".into();
        candidate_genome.prompt.messages[1].artifact = digest('f');
        let candidate = revision(candidate_genome);
        let allowed = [MutationSurface::TaskStrategyPrompt].into_iter().collect();

        let error = verify_allowed_genome_diff(&parent, &candidate, &allowed)
            .expect_err("部分允许集合不得掩盖 Runtime 越界变化");

        match error {
            GenomeDiffError::UnauthorizedMutationSurfaces {
                unauthorized,
                changed_surfaces,
                allowed: actual_allowed,
            } => {
                assert_eq!(
                    unauthorized,
                    [MutationSurface::Runtime].into_iter().collect()
                );
                assert_eq!(
                    changed_surfaces,
                    [
                        MutationSurface::Runtime,
                        MutationSurface::TaskStrategyPrompt,
                    ]
                    .into_iter()
                    .collect()
                );
                assert_eq!(actual_allowed, allowed);
            }
            other => panic!("应返回未授权表面错误，实际为 {other:?}"),
        }
    }

    /// 断言任意插件环境字段变化都返回冻结依赖错误。
    fn assert_frozen_plugin_change(mut mutate: impl FnMut(&mut AgentGenome)) {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        mutate(&mut candidate_genome);
        let candidate = revision(candidate_genome);
        let allowed = [MutationSurface::TaskStrategyPrompt].into_iter().collect();
        assert!(matches!(
            verify_allowed_genome_diff(&parent, &candidate, &allowed),
            Err(GenomeDiffError::FrozenPluginEnvironmentChanged { .. })
        ));
    }

    /// Bundle 变化不能成为 Candidate。
    #[test]
    fn plugin_bundle_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| genome.plugins[0].bundle = digest('9'));
    }

    /// 版本变化不能成为 Candidate。
    #[test]
    fn plugin_version_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| genome.plugins[0].version = "0.2.0".into());
    }

    /// 不透明配置变化不能成为 Candidate。
    #[test]
    fn plugin_config_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| genome.plugins[0].config_digest = Some(digest('9')));
    }

    /// 插件集合变化不能成为 Candidate。
    #[test]
    fn plugin_set_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| {
            genome.plugins.remove(0);
            genome
                .capability_owners
                .retain(|_, owner| owner != "context");
        });
    }

    /// 加载顺序变化不能成为 Candidate。
    #[test]
    fn plugin_load_order_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| genome.plugins[0].load_order = Some(2));
    }

    /// Hook 顺序变化不能成为 Candidate。
    #[test]
    fn plugin_hook_order_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| {
            genome.plugins[0].hook_order.push("after-model".into());
        });
    }

    /// Capability Profile 即插件权限变化不能成为 Candidate。
    #[test]
    fn plugin_permission_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| {
            genome.plugins[0].capability_profile_digest = Some(digest('9'));
        });
    }

    /// Manifest 变化不能成为 Candidate。
    #[test]
    fn plugin_manifest_change_rejects_candidate() {
        assert_frozen_plugin_change(|genome| {
            genome.plugins[0].manifest_digest = Some(digest('9'));
        });
    }

    /// 旧 Policy 即使声明 Plugin 表面也不能重新开启插件进化。
    #[test]
    fn legacy_plugin_surface_cannot_be_enabled() {
        let parent = revision(sample_genome());
        let candidate = revision(parent.genome.clone());
        let allowed = [MutationSurface::Plugin].into_iter().collect();
        assert!(matches!(
            verify_allowed_genome_diff(&parent, &candidate, &allowed),
            Err(GenomeDiffError::UnsupportedLegacyPluginSurface)
        ));
    }

    #[test]
    fn cross_layer_prompt_reorder_marks_each_affected_surface() {
        let parent = revision(sample_genome());
        let mut candidate_genome = parent.genome.clone();
        candidate_genome.prompt.messages.swap(0, 1);
        let candidate = revision(candidate_genome);

        let diff = diff_genomes(&parent, &candidate).expect("跨层重排应生成可信差异");

        assert_eq!(
            diff.changed_surfaces,
            [
                MutationSurface::TaskStrategyPrompt,
                MutationSurface::ProtectedPrompt,
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            diff.summary,
            vec!["Task Strategy Prompt 变化", "受保护 Prompt 变化"]
        );
    }

    #[test]
    fn invalid_candidate_digest_is_rejected_before_comparison() {
        let parent = revision(sample_genome());
        let mut candidate = parent.clone();
        candidate.genome.prompt.messages[1].artifact = digest('f');

        assert!(matches!(
            diff_genomes(&parent, &candidate),
            Err(GenomeDiffError::InvalidCandidate { .. })
        ));
    }

    /// 断言只允许 Task Strategy 时指定表面会被结构化拒绝。
    fn assert_unauthorized(
        parent: &GenomeRevision,
        candidate: &GenomeRevision,
        expected: MutationSurface,
    ) {
        let allowed = [MutationSurface::TaskStrategyPrompt].into_iter().collect();
        let error =
            verify_allowed_genome_diff(parent, candidate, &allowed).expect_err("越界差异应被拒绝");

        match error {
            GenomeDiffError::UnauthorizedMutationSurfaces {
                unauthorized,
                changed_surfaces,
                allowed: actual_allowed,
            } => {
                let expected_surfaces = [expected].into_iter().collect();
                assert_eq!(unauthorized, expected_surfaces);
                assert_eq!(changed_surfaces, expected_surfaces);
                assert_eq!(actual_allowed, allowed);
            }
            other => panic!("应返回未授权表面错误，实际为 {other:?}"),
        }
    }
}
