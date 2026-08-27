use agent_evolution::{
    diff_genomes, ArtifactStore, CandidateBuildError, CandidateBuilder, FileArtifactStore,
    FileGenomeStore, GenomeStore, MAX_TASK_STRATEGY_PROMPT_BYTES,
};
use agent_evolution_protocol::{
    AgentGenome, ArtifactDigest, ArtifactRef, EpisodeId, EvolutionCycleId, EvolutionIssueId,
    ExpectedEffect, GenomeDigest, GenomeMetadata, GenomeRevision, GenomeRevisionId,
    InvalidMutation, ModelGenome, MutationId, MutationPatch, MutationProposal, MutationRisk,
    MutationSurface, PromptArtifactRef, PromptGenome, PromptLayer, RuntimeIdentity,
    ToolProfileGenome, GENOME_SCHEMA_VERSION, MUTATION_PROPOSAL_SCHEMA_VERSION,
};
use agent_tool::{ExecutionPolicy, ToolAccess};
use std::{collections::BTreeSet, path::PathBuf};
use uuid::Uuid;

const PARENT_PROMPT: &[u8] = b"Inspect evidence before changing code.";
const CANDIDATE_PROMPT: &[u8] = b"Inspect evidence, then verify every bounded change.";

/// 测试结束时清理本用例独占的临时目录。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 创建不会与并发用例冲突的临时数据根。
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "lucia-candidate-builder-{}",
                Uuid::new_v4().simple()
            )),
        }
    }

    /// 返回临时数据根路径。
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    /// 尽力清理测试数据；清理失败不覆盖原始测试结论。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 绑定一组真实文件 Store 与已登记 Parent 的测试夹具。
struct Fixture {
    _temp_dir: TempDir,
    artifacts: FileArtifactStore,
    genomes: FileGenomeStore,
    parent: GenomeRevision,
}

impl Fixture {
    /// 创建包含指定数量 Task Strategy Prompt 的 Parent 修订。
    async fn new(task_strategy_count: usize) -> Self {
        let temp_dir = TempDir::new();
        let artifacts = FileArtifactStore::new(temp_dir.path().join("artifacts"));
        let genomes = FileGenomeStore::new(temp_dir.path().join("genomes"));
        let parent_prompt = artifacts
            .put("text/plain", PARENT_PROMPT)
            .await
            .expect("Parent Prompt 应写入真实 CAS");
        let parent = GenomeRevision::create(
            sample_genome(parent_prompt.digest, task_strategy_count),
            GenomeMetadata::default(),
        )
        .expect("Parent Genome 应合法");
        genomes
            .append(&parent)
            .await
            .expect("Parent Genome 应写入真实 Store");
        Self {
            _temp_dir: temp_dir,
            artifacts,
            genomes,
            parent,
        }
    }

    /// 为真实 Parent 和指定新 Prompt 引用构造合法 Proposal。
    fn proposal(&self, prompt: ArtifactRef) -> MutationProposal {
        MutationProposal {
            schema_version: MUTATION_PROPOSAL_SCHEMA_VERSION,
            mutation_id: MutationId::generate(),
            issue_id: EvolutionIssueId::generate(),
            parent_revision_id: self.parent.revision_id.clone(),
            parent_genome_digest: self.parent.digest.clone(),
            surface: MutationSurface::TaskStrategyPrompt,
            evidence_episode_ids: vec![EpisodeId::generate()],
            hypothesis: "补足工具失败后的验证步骤".to_string(),
            patch: MutationPatch::ReplaceTaskStrategyPrompt { prompt },
            expected_effects: vec![ExpectedEffect {
                task_family: "code-edit".to_string(),
                expected_behavior: "修改后执行边界验证".to_string(),
            }],
            risk: MutationRisk::Low,
            mutator_revision: ArtifactRef {
                digest: artifact_digest('e'),
                media_type: "application/json".to_string(),
                size_bytes: 1,
            },
        }
    }

    /// 调用 Builder，并要求失败后 Store 仍只包含 Parent 文件。
    async fn rejected(&self, proposal: &MutationProposal) -> Result<(), CandidateBuildError> {
        let error = CandidateBuilder::new(&self.genomes, &self.artifacts)
            .build(EvolutionCycleId::generate(), proposal)
            .await
            .expect_err("无效 Proposal 不得生成 Candidate");
        assert_eq!(revision_file_count(&self.genomes).await, 1);
        Err(error)
    }
}

/// 构造可区分 Task Strategy 数量的完整合法 Genome。
fn sample_genome(parent_prompt: ArtifactDigest, task_strategy_count: usize) -> AgentGenome {
    let mut messages = vec![PromptArtifactRef {
        layer: PromptLayer::Safety,
        artifact: artifact_digest('a'),
    }];
    messages.extend((0..task_strategy_count).map(|index| PromptArtifactRef {
        layer: PromptLayer::TaskStrategy,
        artifact: if index == 0 {
            parent_prompt.clone()
        } else {
            artifact_digest('b')
        },
    }));

    AgentGenome {
        schema_version: GENOME_SCHEMA_VERSION,
        runtime: RuntimeIdentity {
            package_version: "0.1.0".to_string(),
            git_commit: "candidate-builder-test".to_string(),
            git_dirty: false,
            target_triple: "test-target".to_string(),
            features: BTreeSet::new(),
        },
        model: ModelGenome {
            provider: "fixture".to_string(),
            provider_kind: "fixture".to_string(),
            model: "deterministic".to_string(),
            base_url: None,
            protocol: None,
            max_tokens: Some(64),
            temperature: None,
            stream: false,
            provider_options_digest: None,
        },
        prompt: PromptGenome { messages },
        plugins: Vec::new(),
        capability_owners: Default::default(),
        tools: ToolProfileGenome {
            native_tools: BTreeSet::new(),
            access: ToolAccess::All,
        },
        context_policy: None,
        planning_policy: None,
        skills: Vec::new(),
        execution: ExecutionPolicy::serve(),
    }
}

/// 生成只用于错误路径的合法 Artifact 摘要。
fn artifact_digest(seed: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("摘要应合法")
}

/// 生成只用于错误路径的合法 Genome 摘要。
fn genome_digest(seed: char) -> GenomeDigest {
    GenomeDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("摘要应合法")
}

/// 统计真实 Genome Store 中已提交的 JSON 修订文件。
async fn revision_file_count(store: &FileGenomeStore) -> usize {
    let mut entries = tokio::fs::read_dir(store.root())
        .await
        .expect("Genome Store 根目录应存在");
    let mut count = 0;
    while let Some(entry) = entries.next_entry().await.expect("应读取 Store 目录") {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
            count += 1;
        }
    }
    count
}

/// 解析真实 Artifact CAS 的固定内容路径，用于模拟提交后的磁盘篡改。
fn artifact_path(store: &FileArtifactStore, digest: &ArtifactDigest) -> PathBuf {
    let hex = digest.as_str().trim_start_matches("sha256:");
    store.root().join("sha256").join(&hex[..2]).join(hex)
}

/// 成功路径必须只替换唯一 Task Strategy，并在 DTO 完成后登记新修订。
#[tokio::test]
async fn builds_and_persists_exact_task_strategy_candidate() {
    let fixture = Fixture::new(1).await;
    let prompt = fixture
        .artifacts
        .put("text/plain; charset=utf-8", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入真实 CAS");
    let proposal = fixture.proposal(prompt.clone());
    let cycle_id = EvolutionCycleId::generate();

    let candidate = CandidateBuilder::new(&fixture.genomes, &fixture.artifacts)
        .build(cycle_id.clone(), &proposal)
        .await
        .expect("合法 Proposal 应生成 Candidate");

    assert_eq!(candidate.cycle_id, cycle_id);
    assert_eq!(candidate.parent_revision_id, fixture.parent.revision_id);
    assert_eq!(candidate.parent_genome_digest, fixture.parent.digest);
    assert_eq!(candidate.mutation_id, proposal.mutation_id);
    assert_eq!(candidate.prompt, prompt);
    assert_eq!(
        candidate.changed_surfaces,
        BTreeSet::from([MutationSurface::TaskStrategyPrompt])
    );
    assert!(candidate.created_at_ms > 0);

    let persisted = fixture
        .genomes
        .get(&candidate.candidate_revision_id)
        .await
        .expect("应读取 Candidate 修订")
        .expect("Candidate 修订必须已追加");
    let unchanged_parent = fixture
        .genomes
        .get(&fixture.parent.revision_id)
        .await
        .expect("应复读 Parent")
        .expect("Parent 必须仍存在");
    assert_eq!(unchanged_parent, fixture.parent);
    assert_eq!(persisted.digest, candidate.candidate_genome_digest);
    assert_eq!(
        persisted.metadata.parent,
        Some(fixture.parent.revision_id.clone())
    );
    assert_eq!(
        persisted.metadata.mutation,
        Some(proposal.mutation_id.clone())
    );
    assert_eq!(persisted.metadata.created_at, None);
    assert_eq!(persisted.metadata.description, None);

    let mut expected_genome = fixture.parent.genome.clone();
    expected_genome.prompt.messages[1].artifact = prompt.digest.clone();
    assert_eq!(persisted.genome, expected_genome);
    let diff = diff_genomes(&fixture.parent, &persisted).expect("应生成可信差异");
    assert_eq!(
        diff.changed_surfaces,
        BTreeSet::from([MutationSurface::TaskStrategyPrompt])
    );
    assert_eq!(revision_file_count(&fixture.genomes).await, 2);

    let candidate_json = serde_json::to_string(&candidate).expect("Candidate 应可序列化");
    let genome_json = serde_json::to_string(&persisted).expect("Genome 应可序列化");
    let prompt_text = std::str::from_utf8(CANDIDATE_PROMPT).expect("测试 Prompt 是 UTF-8");
    assert!(!candidate_json.contains(prompt_text));
    assert!(!genome_json.contains(prompt_text));
}

/// 相同 Cycle 与 Proposal 在提交点后重试必须返回同一 Candidate，且不能追加孤立 Revision。
#[tokio::test]
async fn retries_candidate_build_idempotently() {
    let fixture = Fixture::new(1).await;
    let prompt = fixture
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");
    let proposal = fixture.proposal(prompt);
    let cycle_id = EvolutionCycleId::generate();
    let builder = CandidateBuilder::new(&fixture.genomes, &fixture.artifacts);

    let first = builder
        .build_at(cycle_id.clone(), &proposal, 1_000)
        .await
        .expect("首次构建应成功");
    let retried = builder
        .build_at(cycle_id, &proposal, 1_000)
        .await
        .expect("相同构建重试应幂等成功");

    assert_eq!(retried, first);
    assert_eq!(revision_file_count(&fixture.genomes).await, 2);
}

/// Builder 必须拒绝不存在或摘要错绑的 Parent，且不得追加孤立修订。
#[tokio::test]
async fn rejects_missing_or_mismatched_parent_without_append() {
    let fixture = Fixture::new(1).await;
    let prompt = fixture
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");

    let mut missing = fixture.proposal(prompt.clone());
    missing.parent_revision_id = GenomeRevisionId::generate();
    assert!(matches!(
        fixture.rejected(&missing).await,
        Err(CandidateBuildError::ParentNotFound(_))
    ));

    let mut mismatched = fixture.proposal(prompt);
    mismatched.parent_genome_digest = genome_digest('f');
    assert!(matches!(
        fixture.rejected(&mismatched).await,
        Err(CandidateBuildError::ParentDigestMismatch { .. })
    ));
}

/// Builder 必须拒绝缺失或重复的 Task Strategy，避免替换目标产生歧义。
#[tokio::test]
async fn rejects_missing_or_ambiguous_task_strategy_without_append() {
    let missing = Fixture::new(0).await;
    let prompt = missing
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");
    assert!(matches!(
        missing.rejected(&missing.proposal(prompt)).await,
        Err(CandidateBuildError::MissingTaskStrategy)
    ));

    let ambiguous = Fixture::new(2).await;
    let prompt = ambiguous
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");
    assert!(matches!(
        ambiguous.rejected(&ambiguous.proposal(prompt)).await,
        Err(CandidateBuildError::AmbiguousTaskStrategy)
    ));
}

/// Builder 必须拒绝空变化、无效文本、长度谎报和不存在的 CAS 引用。
#[tokio::test]
async fn rejects_invalid_prompt_references_without_append() {
    let fixture = Fixture::new(1).await;
    let parent_prompt = ArtifactRef {
        digest: fixture
            .parent
            .genome
            .prompt
            .task_strategy()
            .expect("Parent 应有唯一 Task Strategy")
            .clone(),
        media_type: "text/plain".to_string(),
        size_bytes: PARENT_PROMPT.len() as u64,
    };
    assert!(matches!(
        fixture.rejected(&fixture.proposal(parent_prompt)).await,
        Err(CandidateBuildError::UnchangedPrompt(_))
    ));

    let blank = fixture
        .artifacts
        .put("text/plain", b" \n\t")
        .await
        .expect("空白制品仍可进入 CAS");
    assert!(matches!(
        fixture.rejected(&fixture.proposal(blank)).await,
        Err(CandidateBuildError::BlankPrompt)
    ));

    let binary = fixture
        .artifacts
        .put("text/plain", &[0xff, 0xfe])
        .await
        .expect("二进制制品仍可进入 CAS");
    assert!(matches!(
        fixture.rejected(&fixture.proposal(binary)).await,
        Err(CandidateBuildError::PromptNotUtf8(_))
    ));

    let mut unsupported = fixture
        .artifacts
        .put("application/octet-stream", CANDIDATE_PROMPT)
        .await
        .expect("不支持类型仍可进入通用 CAS");
    assert!(matches!(
        fixture
            .rejected(&fixture.proposal(unsupported.clone()))
            .await,
        Err(CandidateBuildError::UnsupportedPromptMediaType(_))
    ));

    unsupported.media_type = "text/plain".to_string();
    unsupported.size_bytes = MAX_TASK_STRATEGY_PROMPT_BYTES + 1;
    assert!(matches!(
        fixture.rejected(&fixture.proposal(unsupported)).await,
        Err(CandidateBuildError::PromptTooLarge { .. })
    ));

    let mut wrong_size = fixture
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");
    wrong_size.size_bytes += 1;
    assert!(matches!(
        fixture.rejected(&fixture.proposal(wrong_size)).await,
        Err(CandidateBuildError::PromptSizeMismatch { .. })
    ));

    let missing = ArtifactRef {
        digest: artifact_digest('d'),
        media_type: "text/plain".to_string(),
        size_bytes: 1,
    };
    assert!(matches!(
        fixture.rejected(&fixture.proposal(missing)).await,
        Err(CandidateBuildError::PromptArtifactNotFound(_))
    ));
}

/// Builder 必须由真实 CAS 复核摘要，并拒绝提交后被篡改的 Prompt 内容。
#[tokio::test]
async fn rejects_tampered_prompt_artifact_without_append() {
    let fixture = Fixture::new(1).await;
    let prompt = fixture
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");
    tokio::fs::write(
        artifact_path(&fixture.artifacts, &prompt.digest),
        b"tampered",
    )
    .await
    .expect("测试应能篡改 CAS 文件");

    assert!(matches!(
        fixture.rejected(&fixture.proposal(prompt)).await,
        Err(CandidateBuildError::ArtifactStore(_))
    ));
}

/// Proposal 声明 Runtime 等受保护表面时必须在读取 Candidate CAS 前拒绝。
#[tokio::test]
async fn rejects_protected_surface_without_append() {
    let fixture = Fixture::new(1).await;
    let prompt = fixture
        .artifacts
        .put("text/plain", CANDIDATE_PROMPT)
        .await
        .expect("Candidate Prompt 应写入 CAS");
    let mut proposal = fixture.proposal(prompt);
    proposal.surface = MutationSurface::Runtime;

    assert!(matches!(
        fixture.rejected(&proposal).await,
        Err(CandidateBuildError::InvalidProposal(
            InvalidMutation::UnsupportedSurface(MutationSurface::Runtime)
        ))
    ));
}
