//! M6 Context Policy CAS 与可信 Candidate Builder 的独立集成测试。

use agent_evolution::{
    diff_genomes, ArtifactStore, ContextCandidateBuildError, ContextCandidateBuilder,
    ContextPolicyRepository, FileArtifactStore, FileGenomeStore, GenomeStore,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ContextPolicyMutationProposalV1, ContextPolicyV1, EpisodeId,
    EvolutionCycleId, GenomeMetadata, GenomeRevision, ModelGenome, MutationId, MutationSurface,
    PolicyRef, PromptGenome, RuntimeIdentity, ToolProfileGenome,
    CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION, GENOME_SCHEMA_VERSION, NATIVE_CONTEXT_POLICY_ID,
};
use agent_tool::{ExecutionPolicy, ToolAccess};
use std::{collections::BTreeSet, path::PathBuf};
use uuid::Uuid;

/// 不依赖额外 dev-dependency 的临时目录，并在测试结束时尽力清理。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 创建只属于当前测试进程的目录路径，真实目录由 Store 首次写入时创建。
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!("lucia-m6-context-{}", Uuid::new_v4())),
        }
    }

    /// 返回临时根路径。
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    /// 尽力清理测试数据；清理失败不覆盖原始测试结果。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 绑定真实 Artifact CAS、Genome Store 和已登记 Parent 的测试夹具。
struct Fixture {
    _temp_dir: TempDir,
    artifacts: FileArtifactStore,
    genomes: FileGenomeStore,
    parent: GenomeRevision,
    parent_policy: ContextPolicyV1,
    parent_policy_digest: ArtifactDigest,
}

impl Fixture {
    /// 创建能力 owner、PolicyRef 和 CAS 内容彼此一致的 Parent。
    async fn new() -> Self {
        let temp_dir = TempDir::new();
        let artifacts = FileArtifactStore::new(temp_dir.path().join("artifacts"));
        let genomes = FileGenomeStore::new(temp_dir.path().join("genomes"));
        let parent_policy = ContextPolicyV1::default();
        let parent_policy_artifact = ContextPolicyRepository::new(&artifacts)
            .put(&parent_policy)
            .await
            .expect("Parent Policy 应写入真实 CAS");
        let parent = GenomeRevision::create(
            sample_genome(parent_policy_artifact.digest.clone()),
            GenomeMetadata::default(),
        )
        .expect("Parent Genome 应合法");
        genomes.append(&parent).await.expect("Parent Genome 应登记");
        Self {
            _temp_dir: temp_dir,
            artifacts,
            genomes,
            parent,
            parent_policy,
            parent_policy_digest: parent_policy_artifact.digest,
        }
    }

    /// 创建只调整最近原文消息数的合法 Context 提案。
    fn proposal(&self) -> ContextPolicyMutationProposalV1 {
        let mut candidate_policy = self.parent_policy.clone();
        candidate_policy.recent_message_count += 1;
        ContextPolicyMutationProposalV1 {
            schema_version: CONTEXT_POLICY_PROPOSAL_SCHEMA_VERSION,
            mutation_id: MutationId::generate(),
            parent_revision_id: self.parent.revision_id.clone(),
            parent_genome_digest: self.parent.digest.clone(),
            parent_policy_digest: self.parent_policy_digest.clone(),
            candidate_policy,
            evidence_episode_ids: BTreeSet::from([EpisodeId::generate()]),
            hypothesis: "增加一条近期原文消息以提升事实召回".into(),
        }
    }
}

/// 构造包含真实 Context Loader owner 的合法 Genome。
fn sample_genome(policy_digest: ArtifactDigest) -> AgentGenome {
    AgentGenome {
        schema_version: GENOME_SCHEMA_VERSION,
        runtime: RuntimeIdentity {
            package_version: "0.1.0".into(),
            git_commit: "m6-parent".into(),
            git_dirty: false,
            target_triple: "aarch64-apple-darwin".into(),
            features: BTreeSet::from(["plugins".into()]),
        },
        model: ModelGenome {
            provider: "fixture".into(),
            provider_kind: "fixture".into(),
            model: "deterministic".into(),
            base_url: None,
            protocol: None,
            max_tokens: Some(4_096),
            temperature: None,
            stream: false,
            provider_options_digest: None,
        },
        prompt: PromptGenome::default(),
        plugins: Vec::new(),
        capability_owners: Default::default(),
        tools: ToolProfileGenome {
            native_tools: BTreeSet::new(),
            access: ToolAccess::All,
        },
        context_policy: Some(PolicyRef {
            id: NATIVE_CONTEXT_POLICY_ID.into(),
            config_digest: policy_digest,
        }),
        planning_policy: None,
        skills: Vec::new(),
        execution: ExecutionPolicy::serve(),
    }
}

/// Builder 必须写真实策略 CAS、登记不可变 Revision，并保持唯一 Context Policy 差异。
#[tokio::test]
async fn builds_context_only_candidate_with_real_stores() {
    let fixture = Fixture::new().await;
    let proposal = fixture.proposal();
    let cycle_id = EvolutionCycleId::generate();
    let builder = ContextCandidateBuilder::new(&fixture.genomes, &fixture.artifacts);

    let candidate = builder
        .build_at(cycle_id, &proposal, 42)
        .await
        .expect("Context Candidate 应构建成功");
    let revision = fixture
        .genomes
        .get(&candidate.candidate_revision_id)
        .await
        .expect("读取 Candidate 不应失败")
        .expect("Candidate Revision 应已登记");
    let diff = diff_genomes(&fixture.parent, &revision).expect("应产生可信 Diff");

    assert_eq!(
        diff.changed_surfaces,
        BTreeSet::from([MutationSurface::ContextPolicy])
    );
    assert_eq!(candidate.changed_surfaces, diff.changed_surfaces);
    assert_eq!(
        revision.metadata.parent,
        Some(fixture.parent.revision_id.clone())
    );
    assert_eq!(
        revision.metadata.mutation,
        Some(proposal.mutation_id.clone())
    );
    assert_eq!(
        revision
            .genome
            .context_policy
            .as_ref()
            .expect("Candidate 应保留 Context Policy owner")
            .id,
        NATIVE_CONTEXT_POLICY_ID
    );
    assert_eq!(
        ContextPolicyRepository::new(&fixture.artifacts)
            .get(&candidate.candidate_policy_digest)
            .await
            .expect("Candidate Policy 应可从 CAS 复读"),
        proposal.candidate_policy
    );
}

/// 相同输入和时间的崩溃恢复重试必须返回同一 Candidate，不能追加冲突内容。
#[tokio::test]
async fn build_is_idempotent_for_same_trusted_inputs() {
    let fixture = Fixture::new().await;
    let proposal = fixture.proposal();
    let cycle_id = EvolutionCycleId::generate();
    let builder = ContextCandidateBuilder::new(&fixture.genomes, &fixture.artifacts);

    let first = builder
        .build_at(cycle_id.clone(), &proposal, 7)
        .await
        .expect("首次构建应成功");
    let second = builder
        .build_at(cycle_id, &proposal, 7)
        .await
        .expect("相同输入重试应幂等成功");

    assert_eq!(first, second);
}

/// PolicyRef 必须绑定 Kernel 原生上下文能力的稳定 owner。
#[tokio::test]
async fn rejects_policy_owner_mismatch() {
    let fixture = Fixture::new().await;
    let mut invalid_parent = fixture.parent.clone();
    invalid_parent
        .genome
        .context_policy
        .as_mut()
        .expect("测试 Parent 应声明 Context Policy")
        .id = "legacy-context-owner".into();
    invalid_parent = GenomeRevision::create(invalid_parent.genome, GenomeMetadata::default())
        .expect("错绑 owner 仍是结构合法 Genome");
    fixture
        .genomes
        .append(&invalid_parent)
        .await
        .expect("测试 Parent 应登记");
    let mut proposal = fixture.proposal();
    proposal.parent_revision_id = invalid_parent.revision_id;
    proposal.parent_genome_digest = invalid_parent.digest;

    let error = ContextCandidateBuilder::new(&fixture.genomes, &fixture.artifacts)
        .build_at(EvolutionCycleId::generate(), &proposal, 1)
        .await
        .expect_err("错绑 owner 必须失败关闭");

    assert!(matches!(
        error,
        ContextCandidateBuildError::ContextPolicyOwnerMismatch { .. }
    ));
}

/// Repository 必须拒绝可解析但非规范的 Context Policy JSON。
#[tokio::test]
async fn rejects_non_canonical_policy_artifact() {
    let temp_dir = TempDir::new();
    let artifacts = FileArtifactStore::new(temp_dir.path().join("artifacts"));
    let non_canonical =
        serde_json::to_vec_pretty(&ContextPolicyV1::default()).expect("测试 JSON 应可序列化");
    let artifact = artifacts
        .put("application/json", &non_canonical)
        .await
        .expect("非规范测试制品应可写入 CAS");

    let error = ContextPolicyRepository::new(&artifacts)
        .get(&artifact.digest)
        .await
        .expect_err("非规范字节必须被 Repository 拒绝");

    assert!(error.to_string().contains("不是规范 JSON"));
}
