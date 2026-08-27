use agent_evolution::{
    PluginDependencyPolicy, PluginDependencyPolicyError, PluginWorkspaceManifest,
};
use agent_evolution_protocol::{ArtifactDigest, PluginSourceArtifact, PluginSourceFile};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};
use uuid::Uuid;

const PLUGIN_ID: &str = "example.plugin";
const PLUGIN_SCOPE: &str = "plugins/example";

/// 自动清理依赖策略测试创建的专用目录。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 创建一个名称不可预测的真实临时目录。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lucia-plugin-dependency-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir(&path).expect("测试临时目录应可创建");
        Self(path)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 计算与生产实现一致的强类型 SHA-256 摘要。
fn digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes))).expect("摘要应合法")
}

/// 返回一个只包含本地 path dependency 的合法插件源码树。
fn valid_files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (
            format!("{PLUGIN_SCOPE}/Cargo.toml"),
            br#"[package]
name = "example-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
helper = { path = "crates/helper" }
"#
            .to_vec(),
        ),
        (
            format!("{PLUGIN_SCOPE}/Cargo.lock"),
            br#"version = 3

[[package]]
name = "example-plugin"
version = "0.1.0"
dependencies = [
 "helper",
]

[[package]]
name = "helper"
version = "0.1.0"
"#
            .to_vec(),
        ),
        (
            format!("{PLUGIN_SCOPE}/crates/helper/Cargo.toml"),
            br#"[package]
name = "helper"
version = "0.1.0"
edition = "2021"
"#
            .to_vec(),
        ),
        (
            format!("{PLUGIN_SCOPE}/crates/helper/src/lib.rs"),
            b"pub fn helper() -> u32 { 1 }\n".to_vec(),
        ),
        (
            format!("{PLUGIN_SCOPE}/src/lib.rs"),
            b"pub fn value() -> u32 { helper::helper() }\n".to_vec(),
        ),
    ])
}

/// 将字节映射物化，并构造与真实文件精确绑定的工作区清单。
fn materialize(files: BTreeMap<String, Vec<u8>>) -> (TestRoot, PluginWorkspaceManifest) {
    let root = TestRoot::new();
    for (relative, bytes) in &files {
        let path = root.0.join(relative);
        fs::create_dir_all(path.parent().expect("测试文件应有父目录")).expect("测试父目录应可创建");
        fs::write(&path, bytes).expect("测试文件应可写入");
    }
    let source_files = files
        .iter()
        .map(|(path, bytes)| PluginSourceFile {
            path: path.clone(),
            digest: digest(bytes),
            size_bytes: bytes.len() as u64,
        })
        .collect::<Vec<_>>();
    let source_digest = PluginSourceArtifact::new(PLUGIN_ID, source_files.clone())
        .expect("测试源码清单应合法")
        .digest()
        .expect("测试源码摘要应可计算");
    (
        root,
        PluginWorkspaceManifest {
            root: PathBuf::new(),
            plugin_id: PLUGIN_ID.to_string(),
            plugin_scope: PLUGIN_SCOPE.to_string(),
            parent_source_digest: digest(b"parent"),
            source_digest,
            patch_digest: digest(b"patch"),
            files: source_files,
        },
    )
}

/// 用实际临时目录替换构造期占位根路径。
fn bind_root(root: &TestRoot, mut manifest: PluginWorkspaceManifest) -> PluginWorkspaceManifest {
    manifest.root = root.0.clone();
    manifest
}

/// 纯本地依赖图应生成稳定 lock、package 和依赖计划绑定。
#[test]
fn accepts_only_local_scoped_dependencies_and_binds_plan() {
    let (root, manifest) = materialize(valid_files());
    let plan =
        PluginDependencyPolicy::validate(bind_root(&root, manifest)).expect("合法本地依赖应通过");
    assert_eq!(plan.plugin_id(), PLUGIN_ID);
    assert_eq!(plan.package_name(), "example-plugin");
    assert_eq!(
        plan.local_dependency_manifests(),
        &[format!("{PLUGIN_SCOPE}/crates/helper/Cargo.toml")]
    );
    assert_ne!(plan.cargo_lock_digest(), plan.dependency_digest());
}

/// build.rs、替代构建脚本和任意 .cargo 配置都必须拒绝。
#[test]
fn rejects_build_scripts_and_cargo_configuration() {
    for (index, (path, bytes)) in [
        (format!("{PLUGIN_SCOPE}/build.rs"), b"fn main() {}".to_vec()),
        (
            format!("{PLUGIN_SCOPE}/.cargo/config.toml"),
            b"[net]\noffline = false\n".to_vec(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut files = valid_files();
        files.insert(path, bytes);
        let (root, manifest) = materialize(files);
        assert!(matches!(
            PluginDependencyPolicy::validate(bind_root(&root, manifest)),
            Err(PluginDependencyPolicyError::ForbiddenPath(_))
        ));
        assert!(root.0.exists(), "策略失败不得自行删除调用方工作区 {index}");
    }

    let mut files = valid_files();
    files.insert(
        format!("{PLUGIN_SCOPE}/Cargo.toml"),
        b"[package]\nname = \"example-plugin\"\nbuild = \"src/custom.rs\"\n".to_vec(),
    );
    let (root, manifest) = materialize(files);
    assert!(matches!(
        PluginDependencyPolicy::validate(bind_root(&root, manifest)),
        Err(PluginDependencyPolicyError::ForbiddenCargoItem { .. })
    ));
}

/// proc-macro 字段和 proc-macro crate-type 两种形式都不能进入构建面。
#[test]
fn rejects_proc_macro_crates() {
    for manifest_text in [
        "[package]\nname = \"example-plugin\"\n[lib]\nproc-macro = true\n",
        "[package]\nname = \"example-plugin\"\n[lib]\ncrate-type = [\"proc-macro\"]\n",
    ] {
        let mut files = valid_files();
        files.insert(
            format!("{PLUGIN_SCOPE}/Cargo.toml"),
            manifest_text.as_bytes().to_vec(),
        );
        let (root, manifest) = materialize(files);
        assert!(matches!(
            PluginDependencyPolicy::validate(bind_root(&root, manifest)),
            Err(PluginDependencyPolicyError::ForbiddenCargoItem { .. })
        ));
    }
}

/// version、Git、registry 和 workspace 依赖都不是纯本地依赖，必须 fail-closed。
#[test]
fn rejects_registry_git_and_workspace_dependencies() {
    for dependency in [
        "serde = \"1\"",
        "helper = { git = \"https://example.invalid/helper\" }",
        "helper = { registry = \"private\", version = \"1\" }",
        "helper = { workspace = true }",
        "helper = { path = \"crates/helper\", version = \"1\" }",
    ] {
        let mut files = valid_files();
        files.insert(
            format!("{PLUGIN_SCOPE}/Cargo.toml"),
            format!(
                "[package]\nname = \"example-plugin\"\nversion = \"0.1.0\"\n[dependencies]\n{dependency}\n"
            )
            .into_bytes(),
        );
        let (root, manifest) = materialize(files);
        assert!(
            PluginDependencyPolicy::validate(bind_root(&root, manifest)).is_err(),
            "应拒绝依赖 `{dependency}`"
        );
    }
}

/// path dependency 解析后必须仍在插件 scope 且指向清单内的 Cargo.toml。
#[test]
fn rejects_outside_or_missing_path_dependencies() {
    for dependency in [
        "outside = { path = \"../../../outside\" }",
        "missing = { path = \"crates/missing\" }",
        "absolute = { path = \"/tmp/outside\" }",
    ] {
        let mut files = valid_files();
        files.insert(
            format!("{PLUGIN_SCOPE}/Cargo.toml"),
            format!(
                "[package]\nname = \"example-plugin\"\nversion = \"0.1.0\"\n[dependencies]\n{dependency}\n"
            )
            .into_bytes(),
        );
        let (root, manifest) = materialize(files);
        assert!(
            PluginDependencyPolicy::validate(bind_root(&root, manifest)).is_err(),
            "应拒绝依赖 `{dependency}`"
        );
    }
}

/// patch、replace 和 build-dependencies 都能改变可信依赖/执行面，必须拒绝。
#[test]
fn rejects_patch_replace_and_build_dependencies() {
    for section in [
        "[patch.crates-io]\nhelper = { path = \"crates/helper\" }",
        "[replace]\n\"helper:0.1.0\" = { path = \"crates/helper\" }",
        "[build-dependencies]\nhelper = { path = \"crates/helper\" }",
    ] {
        let mut files = valid_files();
        files.insert(
            format!("{PLUGIN_SCOPE}/Cargo.toml"),
            format!("[package]\nname = \"example-plugin\"\nversion = \"0.1.0\"\n{section}\n")
                .into_bytes(),
        );
        let (root, manifest) = materialize(files);
        assert!(matches!(
            PluginDependencyPolicy::validate(bind_root(&root, manifest)),
            Err(PluginDependencyPolicyError::ForbiddenCargoItem { .. })
        ));
    }
}

/// 固定锁文件中出现 registry/Git source 或 checksum 时必须拒绝远程依赖图。
#[test]
fn rejects_remote_sources_in_cargo_lock() {
    for remote in [
        "source = \"registry+https://github.com/rust-lang/crates.io-index\"",
        "source = \"git+https://example.invalid/repo\"",
        "checksum = \"abcd\"",
    ] {
        let mut files = valid_files();
        files.insert(
            format!("{PLUGIN_SCOPE}/Cargo.lock"),
            format!("version = 3\n[[package]]\nname = \"remote\"\nversion = \"1.0.0\"\n{remote}\n")
                .into_bytes(),
        );
        let (root, manifest) = materialize(files);
        assert!(matches!(
            PluginDependencyPolicy::validate(bind_root(&root, manifest)),
            Err(PluginDependencyPolicyError::RemoteCargoLockSource)
        ));
    }
}

/// 额外文件、缺失文件和物化后摘要不符都必须在依赖解析前失败。
#[test]
fn rejects_workspace_manifest_mismatch() {
    let (root, manifest) = materialize(valid_files());
    fs::write(root.0.join(format!("{PLUGIN_SCOPE}/extra.rs")), b"extra").expect("额外文件应可创建");
    assert!(matches!(
        PluginDependencyPolicy::validate(bind_root(&root, manifest)),
        Err(PluginDependencyPolicyError::ExtraWorkspaceFile(_))
    ));

    let (root, manifest) = materialize(valid_files());
    fs::write(
        root.0.join(format!("{PLUGIN_SCOPE}/src/lib.rs")),
        b"tampered",
    )
    .expect("测试文件应可篡改");
    assert!(matches!(
        PluginDependencyPolicy::validate(bind_root(&root, manifest)),
        Err(PluginDependencyPolicyError::WorkspaceFileChanged(_))
    ));
}
