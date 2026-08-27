//! M8 插件部署 Store 的跨进程恢复、状态机并发与路径安全回归测试。

use agent_evaluation::{
    FilePluginDeploymentStore, PluginCanaryDeploymentBindingV1,
    PluginCanaryDeploymentPersistenceView, PluginDeploymentId, PluginDeploymentStateV1,
    PluginDeploymentStoreError,
};
use agent_evolution::{FileArtifactStore, StableGenomeRef, STABLE_GENOME_REF_SCHEMA_VERSION};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, GenomeDigest, GenomeRevisionId, MutationId, ReleaseId,
};
use agent_plugin_manager::InstalledPlugin;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 单个测试使用的稳定部署身份、安装结果和旧 bundle。
struct DeploymentFixture {
    temp: TempDir,
    store_root: PathBuf,
    artifact_root: PathBuf,
    binding: PluginCanaryDeploymentBindingV1,
    installed: InstalledPlugin,
    previous_bundle: Vec<u8>,
}

impl DeploymentFixture {
    /// 创建摘要互不相同且满足状态机约束的测试输入。
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("应创建测试目录");
        let store_root = temp.path().join("deployment-store");
        let artifact_root = temp.path().join("artifacts");
        let release_id = ReleaseId::generate();
        let candidate_digest = artifact_digest('b');
        let parent_revision_id = GenomeRevisionId::generate();
        let parent_revision_digest = genome_digest('a');
        Self {
            temp,
            store_root,
            artifact_root,
            binding: PluginCanaryDeploymentBindingV1 {
                deployment_id: PluginDeploymentId::for_canary_release(release_id.clone()),
                canary_release_id: release_id,
                mutation_id: MutationId::generate(),
                candidate_id: CandidateId::generate(),
                parent_stable: StableGenomeRef {
                    schema_version: STABLE_GENOME_REF_SCHEMA_VERSION,
                    lineage: "stable/plugins".to_string(),
                    revision_id: parent_revision_id.clone(),
                    digest: parent_revision_digest.clone(),
                    generation: 1,
                    release_id: None,
                    evaluation_report_id: None,
                    previous_revision_id: None,
                    rollback_of: None,
                },
                parent_revision_id,
                parent_revision_digest,
                candidate_revision_id: GenomeRevisionId::generate(),
                candidate_revision_digest: genome_digest('b'),
                admission_digest: artifact_digest('c'),
            },
            installed: InstalledPlugin {
                id: "example.plugin".to_string(),
                name: "测试插件".to_string(),
                version: "2.0.0".to_string(),
                api_version: "1".to_string(),
                enabled: true,
                manifest: "plugins/example/plugin.toml".to_string(),
                sha256: candidate_digest.hex().to_string(),
                source: "/tmp/example-plugin.bundle".to_string(),
            },
            previous_bundle: b"previous-stable-plugin-bundle".to_vec(),
        }
    }

    /// 创建当前 Fixture 对应的 CAS 句柄。
    fn artifacts(&self) -> FileArtifactStore {
        FileArtifactStore::new(&self.artifact_root)
    }

    /// 创建当前 Fixture 对应的部署 Store 句柄。
    fn store<'a>(&self, artifacts: &'a FileArtifactStore) -> FilePluginDeploymentStore<'a> {
        FilePluginDeploymentStore::new(&self.store_root, artifacts).expect("应创建部署 Store")
    }

    /// 返回模拟真实 Candidate 安装完成后的只读部署视图。
    fn view(&self) -> TestDeploymentView {
        TestDeploymentView {
            binding: self.binding.clone(),
            installed: self.installed.clone(),
            previous_bundle: self.previous_bundle.clone(),
        }
    }
}

/// 不依赖真实 Plugin Manager 副作用的持久化视图测试替身。
#[derive(Clone)]
struct TestDeploymentView {
    binding: PluginCanaryDeploymentBindingV1,
    installed: InstalledPlugin,
    previous_bundle: Vec<u8>,
}

impl PluginCanaryDeploymentPersistenceView for TestDeploymentView {
    fn persistence_binding(
        &self,
    ) -> Result<PluginCanaryDeploymentBindingV1, PluginDeploymentStoreError> {
        Ok(self.binding.clone())
    }

    fn installed(&self) -> &InstalledPlugin {
        &self.installed
    }

    fn previous_bundle_bytes(&self) -> &[u8] {
        &self.previous_bundle
    }
}

/// 构造固定合法的 Artifact SHA-256 摘要。
fn artifact_digest(character: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 构造固定合法的 Genome SHA-256 摘要。
fn genome_digest(character: char) -> GenomeDigest {
    GenomeDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
}

/// 按生产实现的摘要寻址规则返回 Deployment 状态目录。
fn deployment_directory(root: &Path, deployment_id: &PluginDeploymentId) -> PathBuf {
    root.join("deployments").join(format!(
        "{:x}",
        Sha256::digest(deployment_id.as_str().as_bytes())
    ))
}

/// 返回 FileArtifactStore 中指定摘要的物理文件路径。
fn artifact_path(root: &Path, digest: &ArtifactDigest) -> PathBuf {
    let hex = digest.hex();
    root.join("sha256").join(&hex[..2]).join(hex)
}

/// Store 重新创建后仍应恢复完整历史、当前状态和旧 bundle。
#[tokio::test]
async fn rebuild_store_recovers_history_and_previous_bundle() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    {
        let store = fixture.store(&artifacts);
        assert!(store.root().is_absolute());
        let planned = store
            .append_planned(&fixture.binding, &fixture.previous_bundle)
            .await
            .expect("应追加 Planned");
        let repeated = store
            .append_planned(&fixture.binding, &fixture.previous_bundle)
            .await
            .expect("相同 Planned 应幂等");
        assert_eq!(planned, repeated);
        store
            .append_canary_installed(&fixture.view())
            .await
            .expect("应追加 CanaryInstalled");
    }

    let rebuilt = fixture.store(&artifacts);
    let history = rebuilt
        .history(&fixture.binding.deployment_id)
        .await
        .expect("重建后应读取历史");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].state, PluginDeploymentStateV1::Planned);
    assert_eq!(history[1].state, PluginDeploymentStateV1::CanaryInstalled);
    assert_eq!(
        rebuilt
            .load(&fixture.binding.deployment_id)
            .await
            .expect("应加载最新状态")
            .expect("Deployment 应存在")
            .state,
        PluginDeploymentStateV1::CanaryInstalled
    );
    assert_eq!(
        rebuilt
            .previous_bundle(&history[1])
            .await
            .expect("应从 CAS 恢复旧 bundle"),
        fixture.previous_bundle
    );
}

/// CAS 读取必须同时复核内容摘要、声明长度和摘要引用。
#[tokio::test]
async fn previous_bundle_rechecks_content_length_and_digest() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    let planned = store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");
    let serialized = serde_json::to_vec(&planned).expect("应序列化 Planned");
    assert!(
        !serialized
            .windows(fixture.previous_bundle.len())
            .any(|window| window == fixture.previous_bundle),
        "状态 JSON 不得内嵌旧 bundle 原始字节"
    );
    let path = artifact_path(&fixture.artifact_root, &planned.previous_bundle.digest);

    std::fs::write(&path, b"tampered").expect("应篡改测试 CAS 文件");
    let error = store
        .previous_bundle(&planned)
        .await
        .expect_err("CAS 内容摘要不匹配应拒绝");
    assert!(matches!(error, PluginDeploymentStoreError::Artifact(_)));

    std::fs::write(&path, &fixture.previous_bundle).expect("应恢复测试 CAS 文件");
    let mut wrong_length = planned.clone();
    wrong_length.previous_bundle.size_bytes += 1;
    let error = store
        .previous_bundle(&wrong_length)
        .await
        .expect_err("ArtifactRef 长度不匹配应拒绝");
    assert!(matches!(
        error,
        PluginDeploymentStoreError::BindingMismatch(_)
    ));

    let mut wrong_digest = planned;
    wrong_digest.previous_bundle.digest = artifact_digest('f');
    wrong_digest.installed_digest = wrong_digest.previous_bundle.digest.clone();
    let error = store
        .previous_bundle(&wrong_digest)
        .await
        .expect_err("不存在的摘要引用应拒绝");
    assert!(matches!(
        error,
        PluginDeploymentStoreError::MissingArtifact(_)
    ));
}

/// CanaryInstalled 不得改写 Planned 已落盘的稳定身份或旧 bundle。
#[tokio::test]
async fn canary_installed_rejects_planned_identity_mismatch() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");

    let mut view = fixture.view();
    view.binding.candidate_revision_digest = genome_digest('d');
    let error = store
        .append_canary_installed(&view)
        .await
        .expect_err("错绑 Candidate Revision 应拒绝");
    assert!(matches!(
        error,
        PluginDeploymentStoreError::BindingMismatch(_)
    ));

    let mut view = fixture.view();
    view.previous_bundle = b"another-stable-bundle".to_vec();
    let error = store
        .append_canary_installed(&view)
        .await
        .expect_err("错绑旧 bundle 应拒绝");
    assert!(matches!(
        error,
        PluginDeploymentStoreError::BindingMismatch(_)
    ));
}

/// Store 必须自行解析安装记录中的纯十六进制摘要并拒绝无效值。
#[tokio::test]
async fn canary_installed_validates_installed_bundle_digest() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");

    let mut view = fixture.view();
    view.installed.sha256 = format!("sha256:{}", view.installed.sha256);
    let error = store
        .append_canary_installed(&view)
        .await
        .expect_err("带算法前缀的安装摘要应拒绝");
    assert!(matches!(
        error,
        PluginDeploymentStoreError::InvalidRecord(_)
    ));
}

/// 同一 Deployment 的并发终态竞争最多只允许一个调用成功。
#[tokio::test]
async fn concurrent_terminal_append_has_single_winner() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let first_store = fixture.store(&artifacts);
    let second_store = fixture.store(&artifacts);
    first_store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");
    first_store
        .append_canary_installed(&fixture.view())
        .await
        .expect("应追加 CanaryInstalled");

    let (promoted, rolled_back) = tokio::join!(
        first_store.mark_promoted(&fixture.binding.deployment_id),
        second_store.mark_rolled_back(&fixture.binding.deployment_id)
    );
    assert_eq!(
        usize::from(promoted.is_ok()) + usize::from(rolled_back.is_ok()),
        1
    );
    let history = first_store
        .history(&fixture.binding.deployment_id)
        .await
        .expect("竞争后历史应保持有效");
    assert_eq!(history.len(), 3);
    assert!(history.last().expect("应有终态").state.is_terminal());
}

/// 已持久化终态不得重复追加，也不得改写为另一终态。
#[tokio::test]
async fn duplicate_terminal_is_rejected() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");
    store
        .append_canary_installed(&fixture.view())
        .await
        .expect("应追加 CanaryInstalled");
    store
        .mark_promoted(&fixture.binding.deployment_id)
        .await
        .expect("应追加 Promoted");

    for result in [
        store.mark_promoted(&fixture.binding.deployment_id).await,
        store.mark_rolled_back(&fixture.binding.deployment_id).await,
    ] {
        assert!(matches!(
            result,
            Err(PluginDeploymentStoreError::DuplicateTerminal { .. })
        ));
    }
}

/// 两条合法状态路径必须持久化与当前安装相符的 bundle 摘要。
#[tokio::test]
async fn promoted_and_rolled_back_paths_preserve_expected_digest() {
    for promoted in [true, false] {
        let fixture = DeploymentFixture::new();
        let artifacts = fixture.artifacts();
        let store = fixture.store(&artifacts);
        let planned = store
            .append_planned(&fixture.binding, &fixture.previous_bundle)
            .await
            .expect("应追加 Planned");
        let installed = store
            .append_canary_installed(&fixture.view())
            .await
            .expect("应追加 CanaryInstalled");
        let terminal = if promoted {
            store
                .mark_promoted(&fixture.binding.deployment_id)
                .await
                .expect("应追加 Promoted")
        } else {
            store
                .mark_rolled_back(&fixture.binding.deployment_id)
                .await
                .expect("应追加 RolledBack")
        };

        assert_eq!(planned.installed_digest, planned.previous_bundle.digest);
        assert_eq!(installed.installed_digest, artifact_digest('b'));
        if promoted {
            assert_eq!(terminal.state, PluginDeploymentStateV1::Promoted);
            assert_eq!(terminal.installed_digest, installed.installed_digest);
        } else {
            assert_eq!(terminal.state, PluginDeploymentStateV1::RolledBack);
            assert_eq!(terminal.installed_digest, planned.previous_bundle.digest);
        }
    }
}

/// 相对路径、点号逃逸和 Store 根符号链接必须在构造阶段拒绝。
#[test]
fn store_rejects_unsafe_roots() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    assert!(matches!(
        FilePluginDeploymentStore::new("relative-store", &artifacts),
        Err(PluginDeploymentStoreError::UnsafeRoot(_))
    ));
    assert!(matches!(
        FilePluginDeploymentStore::new(
            fixture.temp.path().join("nested").join("..").join("store"),
            &artifacts,
        ),
        Err(PluginDeploymentStoreError::UnsafeRoot(_))
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let real = fixture.temp.path().join("real-store");
        std::fs::create_dir(&real).expect("应创建真实目录");
        let link = fixture.temp.path().join("store-link");
        symlink(&real, &link).expect("应创建根目录符号链接");
        assert!(matches!(
            FilePluginDeploymentStore::new(link, &artifacts),
            Err(PluginDeploymentStoreError::UnsafeRoot(_))
        ));
    }
}

/// Store 内目录或状态文件符号链接必须在追加或加载时拒绝。
#[cfg(unix)]
#[tokio::test]
async fn store_rejects_internal_symlinks() {
    use std::os::unix::fs::symlink;

    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    let outside = fixture.temp.path().join("outside");
    std::fs::create_dir(&outside).expect("应创建根外目录");
    symlink(&outside, fixture.store_root.join("locks")).expect("应创建内部目录符号链接");
    let error = store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect_err("内部目录符号链接应拒绝");
    assert!(matches!(error, PluginDeploymentStoreError::UnsafePath(_)));

    std::fs::remove_file(fixture.store_root.join("locks")).expect("应移除测试符号链接");
    let planned = store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("移除符号链接后应追加 Planned");
    let state_path = deployment_directory(&fixture.store_root, &fixture.binding.deployment_id)
        .join("00-planned.json");
    let outside_record = outside.join("record.json");
    std::fs::write(
        &outside_record,
        serde_json::to_vec(&planned).expect("应序列化记录"),
    )
    .expect("应写入根外记录");
    std::fs::remove_file(&state_path).expect("应移除原状态文件");
    symlink(&outside_record, &state_path).expect("应创建状态文件符号链接");
    let error = store
        .history(&fixture.binding.deployment_id)
        .await
        .expect_err("状态文件符号链接应拒绝");
    assert!(matches!(error, PluginDeploymentStoreError::UnsafePath(_)));
}

/// 截断 JSON、未知文件名和错误 Deployment 目录必须失败关闭。
#[tokio::test]
async fn load_rejects_corrupted_or_misplaced_state_files() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");
    let directory = deployment_directory(&fixture.store_root, &fixture.binding.deployment_id);
    let planned_path = directory.join("00-planned.json");
    let original = std::fs::read(&planned_path).expect("应读取原状态");

    std::fs::write(&planned_path, b"{\"schema_version\":").expect("应写入截断 JSON");
    assert!(matches!(
        store.history(&fixture.binding.deployment_id).await,
        Err(PluginDeploymentStoreError::Json(_))
    ));
    std::fs::write(&planned_path, &original).expect("应恢复原状态");

    let unknown = directory.join("unexpected.json");
    std::fs::write(&unknown, b"{}").expect("应写入未知文件");
    assert!(matches!(
        store.history(&fixture.binding.deployment_id).await,
        Err(PluginDeploymentStoreError::UnsafePath(_))
    ));
    std::fs::remove_file(&unknown).expect("应移除未知文件");

    let other_release = ReleaseId::generate();
    let other_id = PluginDeploymentId::for_canary_release(other_release);
    let other_directory = deployment_directory(&fixture.store_root, &other_id);
    std::fs::create_dir_all(&other_directory).expect("应创建错误 Deployment 目录");
    std::fs::write(other_directory.join("00-planned.json"), original).expect("应复制错放状态记录");
    assert!(matches!(
        store.history(&other_id).await,
        Err(PluginDeploymentStoreError::BindingMismatch(_))
    ));
}

/// CanaryInstalled 只允许追加一次，重复快照不得按 create-new-or-same 接受。
#[tokio::test]
async fn duplicate_canary_installed_is_rejected() {
    let fixture = DeploymentFixture::new();
    let artifacts = fixture.artifacts();
    let store = fixture.store(&artifacts);
    store
        .append_planned(&fixture.binding, &fixture.previous_bundle)
        .await
        .expect("应追加 Planned");
    store
        .append_canary_installed(&fixture.view())
        .await
        .expect("应追加 CanaryInstalled");
    assert!(matches!(
        store.append_canary_installed(&fixture.view()).await,
        Err(PluginDeploymentStoreError::DuplicateState(
            PluginDeploymentStateV1::CanaryInstalled
        ))
    ));
}
