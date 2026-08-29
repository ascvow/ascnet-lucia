//! 插件安装、回滚与诊断的回归测试。

use super::*;
use crate::bundle::BUNDLE_ARCHIVE_DOMAIN;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lucia-plugin-manager-{label}-{}-{id}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).expect("应能清理旧测试目录");
        }
        fs::create_dir_all(&path).expect("应能创建测试目录");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_bundle(
    parent: &Path,
    id: &str,
    version: &str,
    dependencies: &[(&str, &str)],
    exclusive_capability: Option<&str>,
) -> PathBuf {
    let bundle = parent.join(format!("{id}-{version}"));
    fs::create_dir_all(bundle.join("assets")).expect("应能创建测试 bundle");
    fs::write(bundle.join("plugin.wasm"), format!("wasm:{id}:{version}"))
        .expect("应能写入测试 WASM");
    fs::write(bundle.join("assets/config.txt"), "测试配置").expect("应能写入测试资源");

    let mut manifest = format!(
        "[plugin]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"{version}\"\napi_version = \"0.7.0\"\nwasm = \"plugin.wasm\"\n"
    );
    for (dependency_id, requirement) in dependencies {
        manifest.push_str(&format!(
            "\n[[dependencies]]\nid = \"{dependency_id}\"\nversion = \"{requirement}\"\n"
        ));
    }
    if let Some(capability) = exclusive_capability {
        manifest.push_str(&format!(
            "\n[[provides]]\nid = \"{capability}\"\nversion = \"1.0.0\"\nmode = \"exclusive\"\n"
        ));
    }
    fs::write(bundle.join("plugin.toml"), manifest).expect("应能写入测试 manifest");
    bundle
}

/// 确定性归档必须无损恢复源目录，安装后摘要只覆盖提取出的运行时 Bundle。
#[test]
fn packed_bundle_round_trips_with_manager_digest() {
    let directory = TestDirectory::new("bundle-archive-round-trip");
    let bundle = create_bundle(&directory.path, "echo", "1.2.3", &[], None);

    let archive = pack_plugin_bundle(&bundle).expect("应编码确定性 bundle");
    let expected = hash_plugin_bundle(&bundle).expect("应计算原 bundle 摘要");
    assert_eq!(format!("{:x}", Sha256::digest(&archive)), expected);

    let restored = directory.path.join("restored");
    let actual = unpack_plugin_bundle(&archive, &restored).expect("应安全还原 bundle");
    assert_eq!(actual, expected);
    assert_eq!(
        fs::read(restored.join("plugin.toml")).expect("应读取还原 manifest"),
        fs::read(bundle.join("plugin.toml")).expect("应读取原 manifest")
    );
    let manager = PluginManager::new(directory.path.join("managed"));
    let installed = manager.install(restored).expect("还原 bundle 应可安装");
    let installed_bundle = directory.path.join("managed/plugins/echo/1.2.3");
    assert_eq!(
        installed.sha256,
        hash_plugin_bundle(&installed_bundle).expect("应计算运行时安装摘要")
    );
    assert_ne!(installed.sha256, expected);
    assert!(!installed_bundle.join("assets").exists());
}

/// 解包器必须在创建目标目录前拒绝归档中的父目录逃逸。
#[test]
fn packed_bundle_rejects_parent_path_escape() {
    let directory = TestDirectory::new("bundle-archive-escape");
    let mut archive = BUNDLE_ARCHIVE_DOMAIN.to_vec();
    let path = b"../plugin.toml";
    archive.push(b'f');
    archive.extend_from_slice(&(path.len() as u64).to_be_bytes());
    archive.extend_from_slice(path);
    archive.extend_from_slice(&0_u64.to_be_bytes());
    let destination = directory.path.join("escaped");

    let error = unpack_plugin_bundle(&archive, &destination).expect_err("父目录逃逸归档必须拒绝");

    assert!(error.to_string().contains("不允许目录跳转"));
    assert!(!destination.exists());
}

/// 解包器必须拒绝截断的文件正文，并保持目标目录不存在。
#[test]
fn packed_bundle_rejects_truncated_contents() {
    let directory = TestDirectory::new("bundle-archive-truncated");
    let mut archive = BUNDLE_ARCHIVE_DOMAIN.to_vec();
    let path = b"plugin.toml";
    archive.push(b'f');
    archive.extend_from_slice(&(path.len() as u64).to_be_bytes());
    archive.extend_from_slice(path);
    archive.extend_from_slice(&10_u64.to_be_bytes());
    archive.extend_from_slice(b"short");
    let destination = directory.path.join("truncated");

    let error = unpack_plugin_bundle(&archive, &destination).expect_err("截断归档必须拒绝");

    assert!(error.to_string().contains("归档被截断"));
    assert!(!destination.exists());
}

#[test]
fn install_creates_lock_and_verified_runtime_config() {
    let directory = TestDirectory::new("install");
    let bundle = create_bundle(&directory.path, "echo", "1.2.3", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));

    let installed = manager.install(&bundle).expect("插件应安装成功");
    assert_eq!(installed.id, "echo");
    assert!(installed.enabled);
    assert_eq!(installed.sha256.len(), 64);
    assert!(manager.lock_path().is_file());
    assert_eq!(manager.list().expect("应能列出插件"), vec![installed]);
    let installed_bundle = directory.path.join("managed/plugins/echo/1.2.3");
    assert!(installed_bundle.join("plugin.toml").is_file());
    assert!(installed_bundle.join("plugin.wasm").is_file());
    assert!(!installed_bundle.join("assets").exists());

    let runtime = manager.runtime_config().expect("运行时配置应通过校验");
    assert_eq!(runtime.manifest_paths.len(), 1);
    assert!(runtime.manifest_paths[0].ends_with("plugins/echo/1.2.3/plugin.toml"));
    assert!(runtime.capability_selection.is_empty());
}

#[test]
fn replace_keeps_previous_plugin_when_new_version_is_invalid() {
    let directory = TestDirectory::new("replace-rollback");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[("missing", "^1")], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(&first).expect("旧版本应安装成功");

    let error = manager
        .replace_with_source(&second, InstallOptions::default(), Some("registry".into()))
        .expect_err("依赖不满足的新版本应拒绝替换");

    assert!(error.to_string().contains("依赖关系无效"));
    let installed = manager.list().expect("锁文件应保持可读");
    assert_eq!(installed[0].version, "1.0.0");
    assert!(directory
        .path
        .join("managed/plugins/echo/1.0.0/plugin.toml")
        .is_file());
    assert!(!directory.path.join("managed/plugins/echo/2.0.0").exists());
}

/// 公共本地替换入口必须提交新锁记录，并把旧 Bundle 保留到人工版本历史。
#[test]
fn public_replace_switches_lock_before_removing_previous_bundle() {
    let directory = TestDirectory::new("replace-success");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(&first).expect("旧版本应安装成功");

    let updated = manager.replace(&second).expect("新版本应原子替换成功");

    assert_eq!(updated.version, "2.0.0");
    assert_eq!(
        updated.source,
        second
            .canonicalize()
            .expect("应解析路径")
            .display()
            .to_string()
    );
    assert_eq!(manager.list().expect("锁文件应可读")[0].version, "2.0.0");
    assert!(directory
        .path
        .join("managed/plugins/echo/1.0.0/plugin.toml")
        .is_file());
    assert!(directory
        .path
        .join("managed/plugins/echo/2.0.0/plugin.toml")
        .is_file());
    let history = manager.list_version_history().expect("应读取人工版本历史");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].version, "1.0.0");
}

/// 人工回滚只切换固定版本，当前版本进入历史且两个 Bundle 都保持可复现。
#[test]
fn rollback_switches_installed_version_and_preserves_history() {
    let directory = TestDirectory::new("manual-version-rollback");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(&first).expect("旧版本应安装成功");
    manager.replace(&second).expect("新版本应替换成功");

    let rolled_back = manager
        .rollback("echo", "1.0.0")
        .expect("历史版本应可人工回滚");

    assert_eq!(rolled_back.version, "1.0.0");
    assert_eq!(manager.list().expect("锁文件应可读")[0].version, "1.0.0");
    let history = manager.list_version_history().expect("应读取人工版本历史");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].version, "2.0.0");
    assert!(directory
        .path
        .join("managed/plugins/echo/1.0.0/plugin.toml")
        .is_file());
    assert!(directory
        .path
        .join("managed/plugins/echo/2.0.0/plugin.toml")
        .is_file());
    assert!(manager.doctor().expect("回滚后诊断应执行").is_healthy());
}

/// 历史 Bundle 被篡改后不得通过人工回滚切换为运行依赖。
#[test]
fn rollback_rejects_tampered_history_bundle() {
    let directory = TestDirectory::new("manual-version-rollback-tampered");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(&first).expect("旧版本应安装成功");
    manager.replace(&second).expect("新版本应替换成功");
    fs::write(
        directory
            .path
            .join("managed/plugins/echo/1.0.0/plugin.wasm"),
        "历史版本已篡改",
    )
    .expect("应能篡改测试历史文件");

    let error = manager
        .rollback("echo", "1.0.0")
        .expect_err("被篡改历史必须拒绝回滚");

    assert!(error.to_string().contains("历史插件"));
    assert_eq!(
        manager.list().expect("锁文件应保持可读")[0].version,
        "2.0.0"
    );
}

/// 人工基线提交失败时必须能够恢复被替换的固定版本与原锁状态。
#[test]
fn captured_state_restores_previous_bundle_and_lock() {
    let directory = TestDirectory::new("management-state-restore");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(&first).expect("旧版本应安装成功");
    let before = manager.list().expect("应读取旧锁状态");
    let snapshot = manager.capture_state().expect("应捕获可恢复状态");

    manager.replace(&second).expect("新版本应替换成功");
    manager.disable("echo").expect("新版本应可禁用");
    manager
        .restore_state(&snapshot)
        .expect("应恢复人工管理状态");

    assert_eq!(manager.list().expect("恢复后锁文件应可读"), before);
    assert!(directory
        .path
        .join("managed/plugins/echo/1.0.0/plugin.toml")
        .is_file());
    assert!(!directory.path.join("managed/plugins/echo/2.0.0").exists());
    assert_eq!(
        manager
            .runtime_config()
            .expect("恢复后运行时配置应健康")
            .manifest_paths
            .len(),
        1
    );
}

#[test]
fn disabled_install_can_be_enabled_and_disabled() {
    let directory = TestDirectory::new("toggle");
    let bundle = create_bundle(&directory.path, "toggle", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));

    let installed = manager
        .install_with_options(&bundle, InstallOptions { enabled: false })
        .expect("禁用状态安装应成功");
    assert!(!installed.enabled);
    assert!(manager
        .runtime_config()
        .expect("配置应有效")
        .manifest_paths
        .is_empty());
    assert!(manager.enable("toggle").expect("应能启用").enabled);
    assert!(!manager.disable("toggle").expect("应能禁用").enabled);
}

#[test]
fn dependency_prevents_provider_disable_and_remove() {
    let directory = TestDirectory::new("dependency");
    let provider = create_bundle(&directory.path, "command", "1.1.0", &[], None);
    let consumer = create_bundle(
        &directory.path,
        "consumer",
        "1.0.0",
        &[("command", "^1.0")],
        None,
    );
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(provider).expect("依赖插件应安装成功");
    manager.install(consumer).expect("使用方应安装成功");

    assert!(manager.disable("command").is_err());
    assert!(manager.remove("command").is_err());
    manager.disable("consumer").expect("应能先禁用使用方");
    manager.disable("command").expect("随后可禁用依赖方");
    manager.remove("command").expect("随后可移除依赖方");
}

#[test]
fn exclusive_capability_conflict_rolls_back_install() {
    let directory = TestDirectory::new("capability");
    let first = create_bundle(
        &directory.path,
        "first",
        "1.0.0",
        &[],
        Some("agent.context-loader"),
    );
    let second = create_bundle(
        &directory.path,
        "second",
        "1.0.0",
        &[],
        Some("agent.context-loader"),
    );
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(first).expect("第一个插件应安装成功");

    let error = manager.install(&second).expect_err("独占能力冲突必须失败");
    assert!(error.to_string().contains("能力声明无效"));
    assert_eq!(manager.list().expect("锁文件应保持原状").len(), 1);
    assert!(!directory.path.join("managed/plugins/second/1.0.0").exists());

    manager
        .install_with_options(&second, InstallOptions { enabled: false })
        .expect("禁用状态可以安装冲突插件");
    let selected = manager
        .select("agent.context-loader", "second")
        .expect("显式选择应原子启用新 owner");
    assert!(selected.enabled);
    let runtime = manager.runtime_config().expect("选择后的配置应有效");
    assert_eq!(runtime.manifest_paths.len(), 2);
    assert_eq!(
        runtime
            .capability_selection
            .get("agent.context-loader")
            .map(String::as_str),
        Some("second")
    );
    assert!(manager.clear_selection("agent.context-loader").is_err());
    manager.disable("first").expect("应能禁用未选中的提供者");
    assert_eq!(
        manager
            .clear_selection("agent.context-loader")
            .expect("只剩一个提供者时应能清除选择"),
        Some("second".into())
    );
}

#[test]
fn doctor_detects_bundle_tampering() {
    let directory = TestDirectory::new("tamper");
    let bundle = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(bundle).expect("插件应安装成功");
    fs::write(
        directory
            .path
            .join("managed/plugins/echo/1.0.0/plugin.wasm"),
        "已篡改",
    )
    .expect("应能篡改测试文件");

    let report = manager.doctor().expect("诊断应生成报告");
    assert!(!report.is_healthy());
    assert!(report.issues[0].message.contains("SHA-256 不匹配"));
    assert!(manager.runtime_config().is_err());
}

/// 锁文件之外的遗留安装目录应被报告为警告，但不阻止已锁定插件加载。
#[test]
fn doctor_warns_about_unmanaged_plugin_directories() {
    let directory = TestDirectory::new("unmanaged");
    let manager = PluginManager::new(directory.path.join("managed"));
    fs::create_dir_all(directory.path.join("managed/plugins/orphan/1.0.0"))
        .expect("创建遗留插件目录");

    let report = manager.doctor().expect("诊断应生成报告");

    assert!(report.is_healthy());
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].severity, DoctorSeverity::Warning);
    assert!(report.issues[0].message.contains("未被锁文件管理"));
}

/// 被篡改的禁用插件不能绕过完整性检查重新启用。
#[test]
fn tampered_disabled_plugin_cannot_be_enabled() {
    let directory = TestDirectory::new("tampered-enable");
    let bundle = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_with_options(&bundle, InstallOptions { enabled: false })
        .expect("插件应以禁用状态安装");
    fs::write(
        directory
            .path
            .join("managed/plugins/echo/1.0.0/plugin.wasm"),
        "已篡改",
    )
    .expect("应能篡改测试文件");

    let error = manager
        .enable("echo")
        .expect_err("篡改后的插件必须拒绝启用");
    assert!(error.to_string().contains("SHA-256 不匹配"));
    assert!(!manager.list().expect("锁文件应保持可读")[0].enabled);
}

#[test]
fn remove_updates_lock_and_archives_bundle() {
    let directory = TestDirectory::new("remove");
    let bundle = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(bundle).expect("插件应安装成功");

    let removed = manager.remove("echo").expect("插件应移除成功");
    assert_eq!(removed.id, "echo");
    assert!(manager.list().expect("锁文件应可读").is_empty());
    assert!(directory
        .path
        .join("managed/plugins/echo/1.0.0/plugin.toml")
        .is_file());
    assert_eq!(
        manager.list_version_history().expect("应读取人工版本历史")[0].version,
        "1.0.0"
    );
}

#[test]
fn manifest_wasm_path_cannot_escape_bundle() {
    let directory = TestDirectory::new("escape");
    let bundle = create_bundle(&directory.path, "escape", "1.0.0", &[], None);
    fs::write(
        bundle.join("plugin.toml"),
        "[plugin]\nid = \"escape\"\nname = \"escape\"\nversion = \"1.0.0\"\napi_version = \"0.7.0\"\nwasm = \"../outside.wasm\"\n",
    )
    .expect("应能改写测试 manifest");
    let manager = PluginManager::new(directory.path.join("managed"));

    let error = manager.install(bundle).expect_err("目录逃逸必须被拒绝");
    assert!(error.to_string().contains("目录跳转"));
}

#[test]
fn plugin_id_cannot_be_used_as_storage_path() {
    let directory = TestDirectory::new("id-escape");
    let bundle = create_bundle(&directory.path, "safe", "1.0.0", &[], None);
    fs::write(
        bundle.join("plugin.toml"),
        "[plugin]\nid = \"../escape\"\nname = \"escape\"\nversion = \"1.0.0\"\napi_version = \"0.7.0\"\nwasm = \"plugin.wasm\"\n",
    )
    .expect("应能改写测试 manifest");
    let manager = PluginManager::new(directory.path.join("managed"));

    let error = manager.install(bundle).expect_err("恶意插件 ID 必须被拒绝");
    assert!(error.to_string().contains("路径跳转"));
}

#[cfg(unix)]
#[test]
fn bundle_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("symlink");
    let bundle = create_bundle(&directory.path, "linked", "1.0.0", &[], None);
    symlink("plugin.wasm", bundle.join("linked.wasm")).expect("应能创建测试符号链接");
    fs::write(
        bundle.join("plugin.toml"),
        "[plugin]\nid = \"linked\"\nname = \"linked\"\nversion = \"1.0.0\"\napi_version = \"0.7.0\"\nwasm = \"linked.wasm\"\n",
    )
    .expect("应能把 manifest 指向符号链接");
    let manager = PluginManager::new(directory.path.join("managed"));

    let error = manager.install(bundle).expect_err("符号链接必须被拒绝");
    assert!(error.to_string().contains("不允许符号链接"));
}
