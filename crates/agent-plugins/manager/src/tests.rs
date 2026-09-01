//! 插件安装、回滚与诊断的回归测试。

use super::*;
use crate::bundle::BUNDLE_ARCHIVE_DOMAIN;
use crate::manager::{
    ensure_open_lock_file_matches_path, plugin_source_authority, BundleReplacementPolicy,
};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

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
        "[plugin]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"{version}\"\napi_version = \"0.8.0\"\nwasm = \"plugin.wasm\"\n"
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

/// 测试基线通过明确的可信启动入口构造已启用插件，不经过人工安装 API。
fn install_enabled_for_startup(
    manager: &PluginManager,
    bundle: impl AsRef<Path>,
) -> Result<InstalledPlugin> {
    manager.install_trusted_startup_with_source(
        bundle,
        InstallOptions { enabled: true },
        Some("trusted-startup:test".into()),
    )
}

/// 使用当前不可变 Bundle 身份模拟用户完成审查后的启用。
fn enable_reviewed(manager: &PluginManager, plugin_id: &str) -> Result<InstalledPlugin> {
    let identity = manager
        .list()?
        .into_iter()
        .find(|plugin| plugin.id == plugin_id)
        .ok_or_else(|| anyhow::anyhow!("插件 `{plugin_id}` 未安装"))?
        .bundle_identity();
    manager.enable_if_matches(plugin_id, &identity)
}

/// 直接读取磁盘锁文件，供授权迁移测试区分持久状态与 Manager 的有效降级视图。
fn read_persisted_lock(manager: &PluginManager) -> PluginLock {
    let text = fs::read_to_string(manager.lock_path()).expect("应读取持久锁文件");
    toml::from_str(&text).expect("持久锁文件应可解析")
}

/// 写入构造的旧版或不完整锁状态，模拟应用升级前已经存在的真实文件。
fn write_persisted_lock(manager: &PluginManager, lock: &PluginLock) {
    let text = toml::to_string_pretty(lock).expect("测试锁状态应可序列化");
    fs::write(manager.lock_path(), text).expect("应写入测试锁文件");
}

/// 把已安装测试 Bundle 改写为指定旧 ABI，并同步锁身份与摘要。
fn rewrite_installed_api_version(manager: &PluginManager, plugin_id: &str, api_version: &str) {
    let mut lock = read_persisted_lock(manager);
    let plugin = lock
        .plugins
        .iter_mut()
        .find(|plugin| plugin.id == plugin_id)
        .expect("应找到待改写插件");
    let manifest_path = manager.root().join(&plugin.manifest);
    let manifest = fs::read_to_string(&manifest_path)
        .expect("应读取已安装 manifest")
        .replace(
            &format!("api_version = \"{}\"", plugin.api_version),
            &format!("api_version = \"{api_version}\""),
        );
    fs::write(&manifest_path, manifest).expect("应改写已安装 manifest ABI");
    plugin.api_version = api_version.to_string();
    plugin.sha256 = hash_plugin_bundle(manifest_path.parent().expect("manifest 应有父目录"))
        .expect("应更新旧 ABI Bundle 摘要");
    write_persisted_lock(manager, &lock);
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
    assert!(!installed.enabled);
    assert_eq!(installed.sha256.len(), 64);
    assert!(manager.lock_path().is_file());
    assert_eq!(manager.list().expect("应能列出插件"), vec![installed]);
    let installed_bundle = directory.path.join("managed/plugins/echo/1.2.3");
    assert!(installed_bundle.join("plugin.toml").is_file());
    assert!(installed_bundle.join("plugin.wasm").is_file());
    assert!(!installed_bundle.join("assets").exists());

    assert!(manager
        .runtime_config()
        .expect("禁用插件运行时配置应通过校验")
        .manifest_paths
        .is_empty());
    enable_reviewed(&manager, "echo").expect("审查能力后应能显式启用");
    let runtime = manager.runtime_config().expect("运行时配置应通过校验");
    assert_eq!(runtime.manifest_paths.len(), 1);
    assert!(runtime.manifest_paths[0].ends_with("plugins/echo/1.2.3/plugin.toml"));
    assert!(runtime.capability_selection.is_empty());
}

/// 可信官方升级仍必须先通过依赖校验，失败时保留健康旧版本。
#[test]
fn replace_keeps_previous_plugin_when_new_version_is_invalid() {
    let directory = TestDirectory::new("replace-rollback");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[("missing", "^1")], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("registry".into()),
        )
        .expect("旧版本应安装成功");

    let error = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("registry".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
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
    install_enabled_for_startup(&manager, &first).expect("旧版本应安装成功");

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

/// 同来源、同能力的第三方新 Bundle 也必须禁用，不能把旧版本授权视为插件 ID 授权。
#[test]
fn third_party_replace_requires_fresh_bundle_review() {
    let directory = TestDirectory::new("replace-third-party-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/vendor/echo@v1#echo.zip".into()),
        )
        .expect("第三方旧版本应安装并启用");

    let updated = manager
        .replace_with_source(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/vendor/echo@v2#echo.zip".into()),
        )
        .expect("第三方新 Bundle 应安全安装");

    assert!(!updated.enabled);
    assert!(manager
        .runtime_config()
        .expect("禁用更新后的运行时配置应有效")
        .manifest_paths
        .is_empty());
}

/// 可信官方升级只有在来源主体和完整能力身份未变化时才能迁移旧启用状态。
#[test]
fn trusted_official_upgrade_preserves_matching_enable_authorization() {
    let directory = TestDirectory::new("replace-official-authorization-migration");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v1#echo.zip".into()),
        )
        .expect("官方旧版本应安装并启用");

    let updated = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v2#echo.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("匹配授权边界的官方升级应成功");

    assert!(updated.enabled);
    assert_eq!(
        manager
            .runtime_config()
            .expect("官方升级后的运行时配置应有效")
            .manifest_paths
            .len(),
        1
    );
}

/// 可信官方升级只能迁移既有启用授权，不能把已禁用插件自动变为启用。
#[test]
fn trusted_official_upgrade_preserves_disabled_state() {
    let directory = TestDirectory::new("replace-official-disabled");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: false },
            Some("github:https://github.com/ascvow/echo@v1#echo.zip".into()),
        )
        .expect("已禁用官方旧版本应安装成功");

    let updated = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v2#echo.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("官方新版本应安全安装");

    assert!(!updated.enabled);
    assert!(manager
        .runtime_config()
        .expect("禁用状态的运行时配置应有效")
        .manifest_paths
        .is_empty());
}

/// 可信官方升级若 capability profile 变化，必须自动禁用并等待用户重新确认。
#[test]
fn replace_disables_plugin_when_capability_profile_changes() {
    let directory = TestDirectory::new("replace-capability-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let mut manifest = fs::read_to_string(second.join("plugin.toml")).expect("读取新 manifest");
    manifest.push_str("\n[capabilities]\nprocess_exec = true\n");
    fs::write(second.join("plugin.toml"), manifest).expect("写入新增能力");
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v1#echo.zip".into()),
        )
        .expect("旧版本应安装并启用");

    let updated = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v2#echo.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("能力变化应安全安装但保持禁用");

    assert!(!updated.enabled);
    assert!(manager
        .runtime_config()
        .expect("禁用更新的运行时配置应有效")
        .manifest_paths
        .is_empty());
}

/// 官方升级的稳定 manifest 身份变化时不得沿用旧启用授权。
#[test]
fn trusted_upgrade_disables_plugin_when_stable_manifest_identity_changes() {
    let directory = TestDirectory::new("replace-stable-identity-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manifest_path = second.join("plugin.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .expect("应读取待升级 manifest")
        .replace("name = \"echo\"", "name = \"Echo Renamed\"");
    fs::write(&manifest_path, manifest).expect("应修改待升级稳定身份");
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v1#echo.zip".into()),
        )
        .expect("官方旧版本应安装并启用");

    let updated = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v2#echo.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("身份变化的 Bundle 应安全替换");

    assert!(!updated.enabled);
    assert_eq!(updated.name, "Echo Renamed");
}

/// 可信官方升级新增或改变跨插件服务依赖后不得沿用旧启用状态。
#[test]
fn replace_disables_plugin_when_service_dependencies_change() {
    let directory = TestDirectory::new("replace-service-dependency-review");
    let provider = create_bundle(&directory.path, "provider", "1.0.0", &[], None);
    let first = create_bundle(&directory.path, "consumer", "1.0.0", &[], None);
    let second = create_bundle(
        &directory.path,
        "consumer",
        "2.0.0",
        &[("provider", "^1.0")],
        None,
    );
    let manager = PluginManager::new(directory.path.join("managed"));
    install_enabled_for_startup(&manager, &provider).expect("服务提供方应安装并启用");
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/consumer@v1#consumer.zip".into()),
        )
        .expect("旧消费方应安装并启用");

    let updated = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/consumer@v2#consumer.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("依赖变化应安全安装但保持禁用");

    assert!(!updated.enabled);
    let runtime = manager
        .runtime_config()
        .expect("禁用消费方后的运行时配置应有效");
    assert_eq!(runtime.manifest_paths.len(), 1);
    assert!(runtime.manifest_paths[0].ends_with("plugins/provider/1.0.0/plugin.toml"));
}

/// 可信官方升级的来源主体变化时不得沿用旧启用授权；同仓库 Release 仍视为同一主体。
#[test]
fn replace_disables_plugin_when_source_authority_changes() {
    let directory = TestDirectory::new("replace-source-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/first/echo@v1#echo.zip".into()),
        )
        .expect("旧来源应安装并启用");

    let updated = manager
        .replace_with_source_policy(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/other/echo@v2#echo.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("来源变化应安全安装");

    assert!(!updated.enabled);
    assert_eq!(
        plugin_source_authority("github:https://github.com/first/echo@v1#echo.zip"),
        plugin_source_authority("github:https://github.com/first/echo@v2#echo.zip")
    );
}

/// 不同来源主体的历史版本不得通过回滚继承当前版本的启用授权。
#[test]
fn rollback_disables_plugin_when_source_authority_changes() {
    let directory = TestDirectory::new("rollback-source-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/first/echo@v1#echo.zip".into()),
        )
        .expect("旧来源应安装并启用");
    manager
        .replace_with_source(
            &second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/other/echo@v2#echo.zip".into()),
        )
        .expect("不同来源的新版本应保持禁用");
    enable_reviewed(&manager, "echo").expect("人工审查后应能启用新来源");

    let rolled_back = manager
        .rollback("echo", "1.0.0")
        .expect("历史版本应可安全回滚");

    assert!(!rolled_back.enabled);
    assert!(manager
        .runtime_config()
        .expect("禁用回滚配置应有效")
        .manifest_paths
        .is_empty());
}

/// capability profile 不同的历史版本不得通过回滚继承当前版本的启用授权。
#[test]
fn rollback_disables_plugin_when_capability_profile_changes() {
    let directory = TestDirectory::new("rollback-capability-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let mut manifest = fs::read_to_string(second.join("plugin.toml")).expect("读取新 manifest");
    manifest.push_str("\n[capabilities]\nprocess_exec = true\n");
    fs::write(second.join("plugin.toml"), manifest).expect("写入新增能力");
    let manager = PluginManager::new(directory.path.join("managed"));
    let authority = "github:https://github.com/first/echo@stable#echo.zip";
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some(authority.into()),
        )
        .expect("旧能力版本应安装并启用");
    manager
        .replace_with_source(
            &second,
            InstallOptions { enabled: true },
            Some(authority.into()),
        )
        .expect("能力变化的新版本应保持禁用");
    enable_reviewed(&manager, "echo").expect("人工审查后应能启用新能力");

    let rolled_back = manager
        .rollback("echo", "1.0.0")
        .expect("旧能力版本应可安全回滚");

    assert!(!rolled_back.enabled);
}

/// 同来源、同能力的人工回滚仍改变精确 Bundle 身份，必须重新进入禁用态。
#[test]
fn rollback_requires_fresh_review_even_when_profile_matches() {
    let directory = TestDirectory::new("rollback-same-profile-review");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            &first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/vendor/echo@v1#echo.zip".into()),
        )
        .expect("旧版本应安装并启用");
    manager
        .replace_with_source(
            &second,
            InstallOptions { enabled: false },
            Some("github:https://github.com/vendor/echo@v2#echo.zip".into()),
        )
        .expect("新版本应安装并等待审查");
    enable_reviewed(&manager, "echo").expect("新 Bundle 经审查后应能启用");

    let rolled_back = manager
        .rollback("echo", "1.0.0")
        .expect("匹配来源与能力的历史版本应可切换");

    assert!(!rolled_back.enabled);
    assert!(manager
        .runtime_config()
        .expect("回滚后的禁用运行时配置应有效")
        .manifest_paths
        .is_empty());
}

/// 移除已经撤销启用授权，恢复归档版本时必须重新进入禁用态。
#[test]
fn rollback_of_removed_plugin_requires_fresh_enable() {
    let directory = TestDirectory::new("rollback-removed-review");
    let bundle = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    install_enabled_for_startup(&manager, &bundle).expect("插件应安装并启用");
    manager.remove("echo").expect("插件应移入版本历史");

    let rolled_back = manager.rollback("echo", "1.0.0").expect("移除版本应可恢复");

    assert!(!rolled_back.enabled);
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
    assert!(
        manager
            .enable_if_matches("toggle", &installed.bundle_identity())
            .expect("身份未变化时应能启用")
            .enabled
    );
    assert!(!manager.disable("toggle").expect("应能禁用").enabled);
}

/// 用户确认启用时必须持久化完整授权摘要，禁用后立即撤销该授权。
#[test]
fn enable_persists_authorization_digest_and_disable_revokes_it() {
    let directory = TestDirectory::new("persist-authorization-digest");
    let bundle = create_bundle(&directory.path, "reviewed", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    let installed = manager
        .install_with_options(&bundle, InstallOptions { enabled: false })
        .expect("插件应以待审状态安装");

    manager
        .enable_if_matches("reviewed", &installed.bundle_identity())
        .expect("确认身份后应启用");
    let approved = read_persisted_lock(&manager);
    assert_eq!(approved.schema_version, LOCK_SCHEMA_VERSION);
    assert_eq!(approved.authorization_grants.len(), 1);
    assert_eq!(approved.authorization_grants["reviewed"].len(), 64);

    manager.disable("reviewed").expect("禁用应撤销授权");
    let disabled = read_persisted_lock(&manager);
    assert!(disabled.authorization_grants.is_empty());
}

/// v1 启用位没有完整授权证据，读取、运行和后续写事务都必须一致降级为禁用。
#[test]
fn legacy_lock_enabled_plugin_degrades_to_disabled_review_state() {
    let directory = TestDirectory::new("legacy-authorization-migration");
    let bundle = create_bundle(&directory.path, "legacy", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    install_enabled_for_startup(&manager, bundle).expect("应构造旧版启用插件");
    let mut legacy = read_persisted_lock(&manager);
    legacy.schema_version = 1;
    legacy.authorization_grants.clear();
    legacy
        .capability_selection
        .insert("agent.tool-policy".into(), "legacy".into());
    write_persisted_lock(&manager, &legacy);

    assert!(!manager.list().expect("旧锁应安全读取")[0].enabled);
    let runtime = manager
        .runtime_config()
        .expect("旧启用位降级后运行配置应有效");
    assert!(runtime.manifest_paths.is_empty());
    assert!(runtime.capability_selection.is_empty());

    manager.disable("legacy").expect("后续写事务应持久化迁移");
    let migrated = read_persisted_lock(&manager);
    assert_eq!(migrated.schema_version, LOCK_SCHEMA_VERSION);
    assert!(!migrated.plugins[0].enabled);
    assert!(migrated.authorization_grants.is_empty());
    assert!(migrated.capability_selection.is_empty());
}

/// 缺失或与当前 Host profile 不匹配的摘要不得被官方升级当作可迁移授权。
#[test]
fn official_upgrade_requires_persisted_matching_authorization_digest() {
    let directory = TestDirectory::new("official-upgrade-approved-digest");
    let first = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "echo", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager
        .install_trusted_startup_with_source(
            first,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v1#echo.zip".into()),
        )
        .expect("应构造已启用官方插件");
    let mut stale = read_persisted_lock(&manager);
    stale
        .authorization_grants
        .insert("echo".into(), "0".repeat(64));
    write_persisted_lock(&manager, &stale);

    assert!(!manager.list().expect("不匹配摘要应安全降级")[0].enabled);
    let updated = manager
        .replace_with_source_policy(
            second,
            InstallOptions { enabled: true },
            Some("github:https://github.com/ascvow/echo@v2#echo.zip".into()),
            BundleReplacementPolicy::TrustedOfficialUpgrade,
        )
        .expect("官方升级应完成但不得继承无证据的启用位");

    assert!(!updated.enabled);
    assert!(read_persisted_lock(&manager)
        .authorization_grants
        .is_empty());
}

/// 公开安装与旧式启用 API 不得跳过 Bundle 身份确认。
#[test]
#[allow(deprecated)]
fn public_install_and_legacy_enable_cannot_bypass_bundle_review() {
    let directory = TestDirectory::new("public-enable-boundary");
    let bundle = create_bundle(&directory.path, "reviewed", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));

    let install_error = manager
        .install_with_options(&bundle, InstallOptions { enabled: true })
        .expect_err("公开安装不得直接启用");
    assert!(install_error.to_string().contains("不能直接启用"));
    assert!(manager.list().expect("拒绝后锁文件应可读").is_empty());

    let installed = manager
        .install_with_options(&bundle, InstallOptions { enabled: false })
        .expect("禁用安装应成功");
    let enable_error = manager
        .enable("reviewed")
        .expect_err("旧式启用不得缺少确认身份");
    assert!(enable_error.to_string().contains("Bundle 身份"));
    assert!(!manager.list().expect("拒绝后插件应可读")[0].enabled);
    assert!(
        manager
            .enable_if_matches("reviewed", &installed.bundle_identity())
            .expect("绑定身份后应能启用")
            .enabled
    );
}

/// 两个 Manager 实例的写操作必须共享同一跨进程文件锁。
#[test]
fn manager_write_lock_serializes_independent_instances() {
    let directory = TestDirectory::new("cross-instance-write-lock");
    let bundle = create_bundle(&directory.path, "locked", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    let installed = manager
        .install_with_options(&bundle, InstallOptions { enabled: false })
        .expect("禁用插件应安装");
    let guard = manager.acquire_write_lock().expect("主线程应获取写锁");
    let second = PluginManager::new(manager.root());
    let identity = installed.bundle_identity();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        started_tx.send(()).expect("应通知已开始等待写锁");
        let result = second.enable_if_matches("locked", &identity);
        done_tx.send(result).expect("应返回第二个写结果");
    });
    started_rx.recv().expect("第二个 Manager 应已启动");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "持有事务锁时其他 Manager 不得提交写入"
    );
    drop(guard);

    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("释放锁后第二个 Manager 应完成")
            .expect("绑定身份的启用应成功")
            .enabled
    );
    worker.join().expect("写锁测试线程不应 panic");
}

/// 打开事务锁后若路径被替换为另一个普通文件，句柄身份复核必须拒绝继续提交。
#[cfg(unix)]
#[test]
fn manager_write_lock_rejects_replaced_path_identity() {
    let directory = TestDirectory::new("replaced-write-lock");
    let manager = PluginManager::new(directory.path.join("managed"));
    drop(manager.acquire_write_lock().expect("应先创建事务锁"));
    let lock_path = manager.root().join(".manager.lock");
    let opened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("应打开原事务锁");
    let replaced = manager.root().join(".manager.lock.replaced");
    fs::rename(&lock_path, &replaced).expect("应替换事务锁路径");
    fs::write(&lock_path, b"new-lock").expect("应创建新的普通锁文件");
    let current_metadata = fs::symlink_metadata(&lock_path).expect("应读取新锁路径身份");

    let error = ensure_open_lock_file_matches_path(&opened, &current_metadata)
        .expect_err("打开句柄与当前路径不是同一 inode 时必须拒绝");

    assert!(error.to_string().contains("路径在打开后被替换"));
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
    install_enabled_for_startup(&manager, provider).expect("依赖插件应安装成功");
    install_enabled_for_startup(&manager, consumer).expect("使用方应安装成功");

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
    install_enabled_for_startup(&manager, first).expect("第一个插件应安装成功");

    let error = install_enabled_for_startup(&manager, &second).expect_err("独占能力冲突必须失败");
    assert!(error.to_string().contains("能力声明无效"));
    assert_eq!(manager.list().expect("锁文件应保持原状").len(), 1);
    assert!(!directory.path.join("managed/plugins/second/1.0.0").exists());

    let second_installed = manager
        .install_with_options(&second, InstallOptions { enabled: false })
        .expect("禁用状态可以安装冲突插件");
    let error = manager
        .select("agent.context-loader", "second")
        .expect_err("owner 选择不得绕过独立启用授权");
    assert!(error.to_string().contains("尚未启用"));
    let selected = manager
        .enable_and_select(
            "agent.context-loader",
            "second",
            &second_installed.bundle_identity(),
        )
        .expect("审查来源和能力后应能原子启用并选择 owner");
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

/// 原子启用与选择失败时不得残留启用状态或 Capability Owner。
#[test]
fn enable_and_select_keeps_lock_unchanged_when_target_does_not_provide_capability() {
    let directory = TestDirectory::new("enable-select-rollback-capability");
    let provider = create_bundle(
        &directory.path,
        "provider",
        "1.0.0",
        &[],
        Some("agent.context-loader"),
    );
    let other = create_bundle(&directory.path, "other", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    install_enabled_for_startup(&manager, provider).expect("能力提供者应安装并启用");
    let other_installed = manager
        .install_with_options(other, InstallOptions { enabled: false })
        .expect("无关插件应以禁用状态安装");

    let before = manager.list().expect("操作前锁状态应可读");
    let error = manager
        .enable_and_select(
            "agent.context-loader",
            "other",
            &other_installed.bundle_identity(),
        )
        .expect_err("未声明能力的插件不得成为 owner");

    assert!(format!("{error:#}").contains("未声明该能力"));
    assert_eq!(manager.list().expect("失败后锁状态应可读"), before);
    let runtime = manager.runtime_config().expect("失败后运行时配置应健康");
    assert_eq!(runtime.manifest_paths.len(), 1);
    assert!(runtime.capability_selection.is_empty());
}

/// 依赖校验失败时，原子启用与选择不得只提交其中一半。
#[test]
fn enable_and_select_keeps_lock_unchanged_when_dependency_is_missing() {
    let directory = TestDirectory::new("enable-select-rollback-dependency");
    let provider = create_bundle(
        &directory.path,
        "provider",
        "1.0.0",
        &[],
        Some("agent.context-loader"),
    );
    let dependent = create_bundle(
        &directory.path,
        "dependent",
        "1.0.0",
        &[("missing", "^1")],
        Some("agent.context-loader"),
    );
    let manager = PluginManager::new(directory.path.join("managed"));
    install_enabled_for_startup(&manager, provider).expect("现有能力提供者应安装并启用");
    let dependent_installed = manager
        .install_with_options(dependent, InstallOptions { enabled: false })
        .expect("缺少依赖的插件应能以禁用状态安装");

    let before = manager.list().expect("操作前锁状态应可读");
    let error = manager
        .enable_and_select(
            "agent.context-loader",
            "dependent",
            &dependent_installed.bundle_identity(),
        )
        .expect_err("缺少依赖时不得启用或选择 owner");

    assert!(error.to_string().contains("依赖关系无效"));
    assert_eq!(manager.list().expect("失败后锁状态应可读"), before);
    assert!(manager
        .runtime_config()
        .expect("失败后运行时配置应健康")
        .capability_selection
        .is_empty());
}

/// 审查能力前若列表中的 Bundle 已被替换，Manager 必须拒绝把新 manifest 绑定到旧身份。
#[test]
fn review_inspection_rejects_replaced_bundle_identity() {
    let directory = TestDirectory::new("review-identity-changed");
    let first = create_bundle(&directory.path, "review", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "review", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    let installed = manager
        .install_with_options(first, InstallOptions { enabled: false })
        .expect("旧 Bundle 应以禁用状态安装");
    manager
        .inspect_if_matches("review", &installed.bundle_identity())
        .expect("身份未变化时应能读取审查 manifest");

    manager
        .replace_with_options(second, InstallOptions { enabled: false })
        .expect("并行管理操作应能替换 Bundle");
    let error = manager
        .inspect_if_matches("review", &installed.bundle_identity())
        .expect_err("旧列表身份不得读取替换后 Bundle 的权限声明");

    assert!(error.to_string().contains("Bundle 身份已在确认后变化"));
    assert!(!manager.list().expect("拒绝后插件应可读")[0].enabled);
}

/// 两次确认之间 Bundle 被替换时，任何审批绑定的启用操作都必须拒绝旧确认。
#[test]
fn approval_bound_enable_operations_reject_replaced_bundle_identity() {
    let directory = TestDirectory::new("enable-identity-changed");
    let first = create_bundle(
        &directory.path,
        "provider",
        "1.0.0",
        &[],
        Some("agent.context-loader"),
    );
    let second = create_bundle(
        &directory.path,
        "provider",
        "2.0.0",
        &[],
        Some("agent.context-loader"),
    );
    let manager = PluginManager::new(directory.path.join("managed"));
    let confirmed = manager
        .install_with_options(first, InstallOptions { enabled: false })
        .expect("旧 Bundle 应以禁用状态安装")
        .bundle_identity();
    manager
        .replace_with_options(second, InstallOptions { enabled: false })
        .expect("并行管理操作应能替换 Bundle");
    let before = manager.list().expect("替换后锁状态应可读");

    let enable_error = manager
        .enable_if_matches("provider", &confirmed)
        .expect_err("普通启用不得接受旧 Bundle 的确认");
    assert!(enable_error
        .to_string()
        .contains("Bundle 身份已在确认后变化"));
    assert_eq!(manager.list().expect("拒绝后锁状态应可读"), before);

    let owner_error = manager
        .enable_and_select("agent.context-loader", "provider", &confirmed)
        .expect_err("Owner 启用不得接受旧 Bundle 的确认");
    assert!(owner_error
        .to_string()
        .contains("Bundle 身份已在确认后变化"));
    assert_eq!(manager.list().expect("再次拒绝后锁状态应可读"), before);
    assert!(!manager.list().expect("目标应保持禁用")[0].enabled);
    assert!(manager
        .runtime_config()
        .expect("拒绝后运行时配置应有效")
        .capability_selection
        .is_empty());
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

/// macOS Finder 元数据不应污染插件目录诊断。
#[test]
fn doctor_ignores_macos_finder_metadata() {
    let directory = TestDirectory::new("finder-metadata");
    let manager = PluginManager::new(directory.path.join("managed"));
    fs::create_dir_all(directory.path.join("managed/plugins/workflow")).expect("创建插件目录");
    fs::write(directory.path.join("managed/plugins/.DS_Store"), "finder")
        .expect("写入根目录 Finder 元数据");
    fs::write(
        directory.path.join("managed/plugins/workflow/.DS_Store"),
        "finder",
    )
    .expect("写入插件目录 Finder 元数据");

    let report = manager.doctor().expect("诊断应忽略 Finder 元数据");

    assert!(report.issues.is_empty(), "{:#?}", report.issues);
}

/// 已禁用旧 ABI Bundle 应保留诊断警告，但不能阻断不加载插件的运行时配置。
#[test]
fn disabled_legacy_api_warns_without_blocking_runtime() {
    let directory = TestDirectory::new("disabled-legacy-api");
    let bundle = create_bundle(&directory.path, "workflow", "1.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(bundle).expect("当前 ABI Bundle 应可安装");
    rewrite_installed_api_version(&manager, "workflow", "0.7.0");

    let report = manager.doctor().expect("旧 ABI 禁用插件应可诊断");

    assert!(report.is_healthy());
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].severity, DoctorSeverity::Warning);
    assert!(report.issues[0]
        .message
        .contains("已禁用插件与当前 Host 不兼容"));
    assert!(manager
        .runtime_config()
        .expect("旧 ABI 禁用插件不应阻断启动")
        .manifest_paths
        .is_empty());
}

/// 更新到当前 ABI 时必须允许把完整但不可执行的旧 ABI Bundle 保留到版本历史。
#[test]
fn replacement_preserves_legacy_api_in_version_history() {
    let directory = TestDirectory::new("replace-legacy-api");
    let first = create_bundle(&directory.path, "workflow", "1.0.0", &[], None);
    let second = create_bundle(&directory.path, "workflow", "2.0.0", &[], None);
    let manager = PluginManager::new(directory.path.join("managed"));
    manager.install(first).expect("当前 ABI Bundle 应可安装");
    rewrite_installed_api_version(&manager, "workflow", "0.7.0");

    let updated = manager
        .replace(second)
        .expect("当前 ABI Bundle 应可替换旧 ABI");
    let history = manager.list_version_history().expect("应读取版本历史");

    assert_eq!(updated.version, "2.0.0");
    assert!(!updated.enabled);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].version, "1.0.0");
    assert_eq!(history[0].api_version, "0.7.0");
    assert!(manager.doctor().expect("历史旧 ABI 应可诊断").is_healthy());
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
    let confirmed = manager.list().expect("应读取待确认插件")[0].bundle_identity();
    fs::write(
        directory
            .path
            .join("managed/plugins/echo/1.0.0/plugin.wasm"),
        "已篡改",
    )
    .expect("应能篡改测试文件");

    let error = manager
        .enable_if_matches("echo", &confirmed)
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
        "[plugin]\nid = \"escape\"\nname = \"escape\"\nversion = \"1.0.0\"\napi_version = \"0.8.0\"\nwasm = \"../outside.wasm\"\n",
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
        "[plugin]\nid = \"../escape\"\nname = \"escape\"\nversion = \"1.0.0\"\napi_version = \"0.8.0\"\nwasm = \"plugin.wasm\"\n",
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
        "[plugin]\nid = \"linked\"\nname = \"linked\"\nversion = \"1.0.0\"\napi_version = \"0.8.0\"\nwasm = \"linked.wasm\"\n",
    )
    .expect("应能把 manifest 指向符号链接");
    let manager = PluginManager::new(directory.path.join("managed"));

    let error = manager.install(bundle).expect_err("符号链接必须被拒绝");
    assert!(error.to_string().contains("不允许符号链接"));
}
