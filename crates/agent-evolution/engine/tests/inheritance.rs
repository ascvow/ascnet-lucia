//! Stable Genome 的真实跨进程继承验证。

use agent_evolution::{
    verify_inheritance, FileStableGenomePublisher, GenomeResolver, GenomeSelector, GenomeStore,
    InheritanceObservation, InheritanceObservationKind, InheritanceVerificationInput,
};
use agent_evolution_protocol::{
    AgentGenome, GenomeDigest, GenomeMetadata, GenomeRevision, GenomeRevisionId, ModelGenome,
    PromptGenome, RunId, RuntimeIdentity, ToolProfileGenome, GENOME_SCHEMA_VERSION,
};
use agent_tool::ExecutionPolicy;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

const LINEAGE: &str = "stable/general";

/// 构造不会与并发测试冲突的临时 Evolution 数据根。
fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("lucia-inheritance-{}", Uuid::new_v4().simple()))
}

/// 构造带可区分模型名的合法不可变 Genome 修订。
fn revision(model: &str) -> GenomeRevision {
    GenomeRevision::create(
        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "inheritance-test".into(),
                git_dirty: false,
                target_triple: "test-target".into(),
                features: BTreeSet::new(),
            },
            model: ModelGenome {
                provider: "fixture".into(),
                provider_kind: "fixture".into(),
                model: model.into(),
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

/// 登记 Parent/Candidate，并通过可信发布器更新 Stable 引用。
async fn promoted_fixture() -> (PathBuf, GenomeRevision, GenomeRevision) {
    let root = temp_root();
    let publisher = FileStableGenomePublisher::new(&root);
    let parent = revision("parent");
    let candidate = revision("candidate");
    publisher
        .resolver()
        .store()
        .append(&parent)
        .await
        .expect("应登记 Parent");
    publisher
        .resolver()
        .store()
        .append(&candidate)
        .await
        .expect("应登记 Candidate");
    publisher
        .publish(LINEAGE, &parent, 1)
        .await
        .expect("应发布 Parent");
    publisher
        .publish(LINEAGE, &candidate, 2)
        .await
        .expect("应 Promotion Candidate");
    (root, parent, candidate)
}

/// 构造覆盖重启、新 Session 和旧 Session 的真实绑定观察。
fn complete_observations(
    parent: &GenomeRevisionId,
    candidate: &GenomeRevisionId,
) -> Vec<InheritanceObservation> {
    vec![
        InheritanceObservation {
            kind: InheritanceObservationKind::Restart,
            observed_genome: candidate.clone(),
            run_id: Some(RunId::generate()),
        },
        InheritanceObservation {
            kind: InheritanceObservationKind::NewSession,
            observed_genome: candidate.clone(),
            run_id: Some(RunId::generate()),
        },
        InheritanceObservation {
            kind: InheritanceObservationKind::ExistingSession,
            observed_genome: parent.clone(),
            run_id: None,
        },
    ]
}

/// 子进程测试入口：在全新进程中重新构造 Resolver 并输出实际 Stable Revision。
#[test]
fn inheritance_child_process() {
    let Some(root) = std::env::var_os("LUCIA_INHERITANCE_CHILD_ROOT") else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("应创建子进程 Runtime");
    let observed = runtime
        .block_on(async {
            agent_evolution::FileGenomeResolver::new(root)
                .resolve(&GenomeSelector::Stable(LINEAGE.into()))
                .await
        })
        .expect("子进程应解析 Stable Genome");
    println!("INHERITED_GENOME={}", observed.revision_id);
}

/// Promotion 的 Stable 引用必须在全新子进程中仍能解析到 Candidate。
#[tokio::test]
async fn promotion_persists_across_restart() {
    let (root, _parent, candidate) = promoted_fixture().await;
    let output = Command::new(std::env::current_exe().expect("应定位测试二进制"))
        .args(["--exact", "inheritance_child_process", "--nocapture"])
        .env("LUCIA_INHERITANCE_CHILD_ROOT", &root)
        .output()
        .expect("应启动继承验证子进程");
    assert!(output.status.success(), "子进程失败：{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("子进程输出应为 UTF-8");
    assert!(stdout.contains(&format!("INHERITED_GENOME={}", candidate.revision_id)));
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Promotion 后新 Session 的实际绑定必须计入 Candidate 继承通过数。
#[tokio::test]
async fn new_session_uses_promoted_genome() {
    let (root, parent, candidate) = promoted_fixture().await;
    let publisher = FileStableGenomePublisher::new(&root);
    let verification = verify_inheritance(
        publisher.resolver(),
        &InheritanceVerificationInput {
            lineage: LINEAGE.into(),
            parent_genome: parent.revision_id.clone(),
            expected_genome: candidate.revision_id.clone(),
            expected_digest: candidate.digest.clone(),
            observations: complete_observations(&parent.revision_id, &candidate.revision_id),
        },
    )
    .await
    .expect("继承验证应执行");
    assert_eq!(verification.new_session_cases_passed, 1);
    assert_eq!(verification.new_session_cases_total, 1);
    assert!(verification.verified);
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Promotion 前已存在的 Session 必须继续绑定 Parent，不能在 Run 中途漂移。
#[tokio::test]
async fn existing_session_keeps_parent_genome() {
    let (root, parent, candidate) = promoted_fixture().await;
    let publisher = FileStableGenomePublisher::new(&root);
    let verification = verify_inheritance(
        publisher.resolver(),
        &InheritanceVerificationInput {
            lineage: LINEAGE.into(),
            parent_genome: parent.revision_id.clone(),
            expected_genome: candidate.revision_id.clone(),
            expected_digest: candidate.digest.clone(),
            observations: complete_observations(&parent.revision_id, &candidate.revision_id),
        },
    )
    .await
    .expect("继承验证应执行");
    assert_eq!(verification.old_session_parent_preserved, Some(true));
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Stable 引用摘要与不可变 Registry 不一致时必须失败，不能只比较 Revision ID。
#[tokio::test]
async fn digest_mismatch_fails_inheritance() {
    let (root, parent, candidate) = promoted_fixture().await;
    let stable_path = stable_path(&root, LINEAGE);
    let mut reference: agent_evolution::StableGenomeRef = serde_json::from_slice(
        &tokio::fs::read(&stable_path)
            .await
            .expect("应读取 Stable 引用"),
    )
    .expect("Stable 引用应可解析");
    reference.digest = GenomeDigest::from_sha256_hex("0".repeat(64)).expect("摘要应合法");
    tokio::fs::write(
        &stable_path,
        serde_json::to_vec_pretty(&reference).expect("应序列化篡改引用"),
    )
    .await
    .expect("应写入篡改引用");
    let result = verify_inheritance(
        FileStableGenomePublisher::new(&root).resolver(),
        &InheritanceVerificationInput {
            lineage: LINEAGE.into(),
            parent_genome: parent.revision_id.clone(),
            expected_genome: candidate.revision_id.clone(),
            expected_digest: candidate.digest.clone(),
            observations: complete_observations(&parent.revision_id, &candidate.revision_id),
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(agent_evolution::InheritanceVerificationError::Resolver(
            agent_evolution::GenomeResolverError::StableDigestMismatch { .. }
        ))
    ));
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// 计算与 Resolver 相同的 Stable 引用固定路径。
fn stable_path(root: &Path, lineage: &str) -> PathBuf {
    root.join("stable")
        .join(format!("{:x}.json", Sha256::digest(lineage.as_bytes())))
}
