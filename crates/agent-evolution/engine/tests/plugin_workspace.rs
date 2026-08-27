use agent_evolution::{
    PluginWorkspaceEntry, PluginWorkspaceError, PluginWorkspaceMaterializer,
    PluginWorkspaceRequest, MAX_PLUGIN_WORKSPACE_FILES, MAX_PLUGIN_WORKSPACE_FILE_BYTES,
};
use agent_evolution_protocol::{
    ArtifactDigest, PluginFilePatch, PluginSourceArtifact, PluginSourceFile,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};
use uuid::Uuid;

const PLUGIN_ID: &str = "example.plugin";
const PLUGIN_SCOPE: &str = "plugins/example";
const LIB_PATH: &str = "plugins/example/src/lib.rs";

/// 为单个测试创建并回收独占临时根目录。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 创建名称不可预测的空临时目录。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lucia-plugin-workspace-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir(&path).expect("测试临时根目录应可创建");
        Self(path)
    }

    /// 返回测试根下一个尚不存在的目标路径。
    fn destination(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 计算与生产协议一致的 SHA-256 强类型摘要。
fn digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes))).expect("测试摘要应合法")
}

/// 构造普通文件条目。
fn regular(bytes: impl AsRef<[u8]>) -> PluginWorkspaceEntry {
    PluginWorkspaceEntry::RegularFile(bytes.as_ref().to_vec())
}

/// 返回含不可变构建配置和两个可变源码文件的 Parent 映射。
fn parent_files() -> BTreeMap<String, PluginWorkspaceEntry> {
    BTreeMap::from([
        (
            "plugins/example/Cargo.toml".into(),
            regular(b"[package]\nname = \"example\"\n"),
        ),
        (
            "plugins/example/plugin.toml".into(),
            regular(b"[plugin]\nid = \"example.plugin\"\n"),
        ),
        (LIB_PATH.into(), regular(b"pub fn value() -> u32 { 1 }\n")),
        (
            "plugins/example/src/old.rs".into(),
            regular(b"pub const OLD: bool = true;\n"),
        ),
    ])
}

/// 从测试 Parent 的真实普通文件字节重建协议源码摘要。
fn source_digest(entries: &BTreeMap<String, PluginWorkspaceEntry>) -> ArtifactDigest {
    let files = entries
        .iter()
        .map(|(path, entry)| {
            let PluginWorkspaceEntry::RegularFile(bytes) = entry else {
                panic!("摘要辅助函数只接受普通文件")
            };
            PluginSourceFile {
                path: path.clone(),
                digest: digest(bytes),
                size_bytes: bytes.len() as u64,
            }
        })
        .collect();
    PluginSourceArtifact::new(PLUGIN_ID, files)
        .expect("测试 Parent 应是合法源码树")
        .digest()
        .expect("测试 Parent 应可计算摘要")
}

/// Parent 测试映射变化后同步其受信前置摘要。
fn refresh_parent_digest(request: &mut PluginWorkspaceRequest) {
    request.expected_parent_source_digest = source_digest(&request.parent_files);
}

/// 构造同时包含 Create、Update 与 Delete 的合法请求。
fn valid_request() -> PluginWorkspaceRequest {
    let old_lib = b"pub fn value() -> u32 { 1 }\n";
    let new_lib = b"pub fn value() -> u32 { 2 }\n";
    let old_file = b"pub const OLD: bool = true;\n";
    let generated = b"pub const GENERATED: bool = true;\n";
    let parent_files = parent_files();
    let expected_parent_source_digest = source_digest(&parent_files);
    PluginWorkspaceRequest {
        plugin_id: PLUGIN_ID.into(),
        plugin_scope: PLUGIN_SCOPE.into(),
        expected_parent_source_digest,
        parent_files,
        replacement_files: BTreeMap::from([
            (
                "plugins/example/src/generated.rs".into(),
                regular(generated),
            ),
            (LIB_PATH.into(), regular(new_lib)),
        ]),
        patches: vec![
            PluginFilePatch::Create {
                path: "plugins/example/src/generated.rs".into(),
                new_digest: digest(generated),
            },
            PluginFilePatch::Update {
                path: LIB_PATH.into(),
                old_digest: digest(old_lib),
                new_digest: digest(new_lib),
            },
            PluginFilePatch::Delete {
                path: "plugins/example/src/old.rs".into(),
                old_digest: digest(old_file),
            },
        ],
    }
}

/// scope 外、路径穿越和绝对路径都必须在创建目标目录前失败。
#[test]
fn rejects_scope_escape_and_unsafe_paths_without_partial_directory() {
    let root = TestRoot::new();
    for (index, path) in [
        "plugins/other/src/lib.rs",
        "plugins/example/../other/lib.rs",
        "/tmp/plugin.rs",
        "C:/temp/plugin.rs",
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = valid_request();
        request.patches = vec![PluginFilePatch::Create {
            path: path.into(),
            new_digest: digest(b"escape"),
        }];
        request.replacement_files = BTreeMap::from([(path.into(), regular(b"escape"))]);
        let destination = root.destination(&format!("unsafe-{index}"));
        assert!(
            PluginWorkspaceMaterializer::validate_and_materialize(request, &destination).is_err(),
            "应拒绝路径 `{path}`"
        );
        assert!(!destination.exists(), "验证失败不得创建目标目录");
    }
}

/// Parent 或新文件声明符号链接语义时必须 fail-closed。
#[test]
fn rejects_symbolic_link_semantics() {
    let mut parent_link = valid_request();
    parent_link.parent_files.insert(
        LIB_PATH.into(),
        PluginWorkspaceEntry::SymbolicLink {
            target: "../../outside.rs".into(),
        },
    );
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(parent_link),
        Err(PluginWorkspaceError::SymbolicLinkEntry { .. })
    ));

    let mut replacement_link = valid_request();
    replacement_link.replacement_files.insert(
        LIB_PATH.into(),
        PluginWorkspaceEntry::SymbolicLink {
            target: "/etc/passwd".into(),
        },
    );
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(replacement_link),
        Err(PluginWorkspaceError::SymbolicLinkEntry { .. })
    ));
}

/// Parent 条目数和单个输入文件字节数都必须受到硬上限约束。
#[test]
fn rejects_file_count_and_single_file_size_overflow() {
    let mut too_many = valid_request();
    too_many.parent_files = (0..=MAX_PLUGIN_WORKSPACE_FILES)
        .map(|index| {
            (
                format!("plugins/example/src/generated-{index:04}.rs"),
                regular([]),
            )
        })
        .collect();
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(too_many),
        Err(PluginWorkspaceError::TooManyEntries {
            field: "parent_files",
            ..
        })
    ));

    let mut too_large = valid_request();
    too_large.replacement_files.insert(
        LIB_PATH.into(),
        regular(vec![0_u8; MAX_PLUGIN_WORKSPACE_FILE_BYTES as usize + 1]),
    );
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(too_large),
        Err(PluginWorkspaceError::FileTooLarge { .. })
    ));
}

/// Update 的旧摘要、新摘要都必须与真实字节精确绑定。
#[test]
fn rejects_old_and_new_digest_misbinding() {
    let mut wrong_parent = valid_request();
    wrong_parent.expected_parent_source_digest = digest(b"stale-parent");
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(wrong_parent),
        Err(PluginWorkspaceError::ParentSourceDigestMismatch { .. })
    ));

    let mut wrong_old = valid_request();
    let PluginFilePatch::Update { old_digest, .. } = &mut wrong_old.patches[1] else {
        panic!("第二项应为 Update")
    };
    *old_digest = digest(b"wrong-parent");
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(wrong_old),
        Err(PluginWorkspaceError::OldDigestMismatch { .. })
    ));

    let mut wrong_new = valid_request();
    let PluginFilePatch::Update { new_digest, .. } = &mut wrong_new.patches[1] else {
        panic!("第二项应为 Update")
    };
    *new_digest = digest(b"wrong-candidate");
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(wrong_new),
        Err(PluginWorkspaceError::NewDigestMismatch { .. })
    ));
}

/// 多余补丁或没有补丁的新内容都必须被拒绝。
#[test]
fn rejects_extra_patch_and_unused_replacement() {
    let mut extra_patch = valid_request();
    extra_patch.patches.push(PluginFilePatch::Delete {
        path: "plugins/example/src/zghost.rs".into(),
        old_digest: digest(b"ghost"),
    });
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(extra_patch),
        Err(PluginWorkspaceError::PatchTargetMissing(_))
    ));

    let mut unused = valid_request();
    unused
        .replacement_files
        .insert("plugins/example/src/zunused.rs".into(), regular(b"unused"));
    assert!(matches!(
        PluginWorkspaceMaterializer::validate(unused),
        Err(PluginWorkspaceError::UnusedReplacement(_))
    ));
}

/// Cargo 依赖配置、插件 manifest、WIT、SDK、build.rs 和 .cargo 都是受保护表面。
#[test]
fn rejects_dependency_build_wit_sdk_and_manifest_changes() {
    for path in [
        "plugins/example/Cargo.toml",
        "plugins/example/plugin.toml",
        "plugins/example/build.rs",
        "plugins/example/.cargo/config.toml",
        "plugins/example/wit/plugin.wit",
        "plugins/example/sdk/src/lib.rs",
    ] {
        let old = if path.ends_with("Cargo.toml") {
            b"[patch.crates-io]\nagent-plugin = { path = \"../../sdk\" }\n".as_slice()
        } else {
            b"protected".as_slice()
        };
        let new = b"changed";
        let mut request = valid_request();
        request.parent_files.insert(path.into(), regular(old));
        refresh_parent_digest(&mut request);
        request.replacement_files = BTreeMap::from([(path.into(), regular(new))]);
        request.patches = vec![PluginFilePatch::Update {
            path: path.into(),
            old_digest: digest(old),
            new_digest: digest(new),
        }];
        assert!(matches!(
            PluginWorkspaceMaterializer::validate(request),
            Err(PluginWorkspaceError::ProtectedPath { .. })
        ));
    }
}

/// 调用方提供的目标已存在时必须原样保留，不能覆盖或清空。
#[test]
fn rejects_preexisting_destination_without_modification() {
    let root = TestRoot::new();
    let destination = root.destination("existing");
    fs::create_dir(&destination).expect("预存目录应可创建");
    let sentinel = destination.join("sentinel.txt");
    fs::write(&sentinel, b"owned-by-caller").expect("哨兵文件应可写入");

    assert!(matches!(
        PluginWorkspaceMaterializer::validate_and_materialize(valid_request(), &destination),
        Err(PluginWorkspaceError::DestinationExists(_))
    ));
    assert_eq!(
        fs::read(&sentinel).expect("哨兵文件应保留"),
        b"owned-by-caller"
    );
}

/// 相同字节与结构化补丁必须得到相同源码摘要和 Patch Plan 摘要。
#[test]
fn produces_idempotent_source_and_patch_digests() {
    let first = PluginWorkspaceMaterializer::validate(valid_request()).expect("首次计划应通过");
    let second = PluginWorkspaceMaterializer::validate(valid_request()).expect("重试计划应通过");
    assert_eq!(first.parent_source_digest(), second.parent_source_digest());
    assert_eq!(first.source_digest(), second.source_digest());
    assert_eq!(first.patch_digest(), second.patch_digest());
    assert_eq!(first.files(), second.files());
}

/// 合法三类补丁应物化完整 Candidate，并返回可独立复核的排序清单和摘要。
#[test]
fn materializes_valid_candidate_with_trusted_manifest() {
    let root = TestRoot::new();
    let destination = root.destination("candidate");
    let manifest =
        PluginWorkspaceMaterializer::validate_and_materialize(valid_request(), &destination)
            .expect("合法请求应成功物化");

    assert_eq!(manifest.root, destination);
    assert_eq!(manifest.plugin_id, PLUGIN_ID);
    assert_eq!(manifest.plugin_scope, PLUGIN_SCOPE);
    assert_eq!(
        fs::read(destination.join(LIB_PATH)).expect("应读取更新文件"),
        b"pub fn value() -> u32 { 2 }\n"
    );
    assert_eq!(
        fs::read(destination.join("plugins/example/src/generated.rs")).expect("应读取新增文件"),
        b"pub const GENERATED: bool = true;\n"
    );
    assert!(!destination.join("plugins/example/src/old.rs").exists());
    assert_eq!(
        fs::read(destination.join("plugins/example/Cargo.toml"))
            .expect("未修改 Cargo 配置必须保留"),
        b"[package]\nname = \"example\"\n"
    );

    let paths = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
    let rebuilt = PluginSourceArtifact::new(PLUGIN_ID, manifest.files.clone())
        .expect("物化清单应为合法源码树");
    assert_eq!(
        rebuilt.digest().expect("应计算源码摘要"),
        manifest.source_digest
    );
}
