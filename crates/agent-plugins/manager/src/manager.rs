//! 受管理插件的安装、替换、启停、回滚与锁文件事务。

use crate::bundle::{
    copy_runtime_bundle, ensure_real_directory, hash_plugin_bundle, path_to_lock_string,
    remove_directory_if_empty, sort_lock_plugins, sort_version_history, upsert_version_history,
    validate_relative_path, validate_storage_component,
};
use crate::doctor::{push_issue, push_warning, DoctorReport, DoctorSeverity};
use crate::types::{
    InstallOptions, InstalledPlugin, PluginBundleIdentity, PluginLock, LOCK_FILE_NAME,
    LOCK_SCHEMA_VERSION,
};
use agent_plugin_host::audit::snapshot_plugin_authorization_profile;
use agent_plugin_host::manifest::{
    resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest, PluginRuntimeConfig,
};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
/// 管理本地 Lucia 插件目录。
#[derive(Debug, Clone)]
pub struct PluginManager {
    root: PathBuf,
}

/// Manager 所有 Bundle 与锁文件写事务共享的跨进程排他锁文件名。
const MANAGER_TRANSACTION_LOCK_FILE: &str = ".manager.lock";

/// 授权摘要的域分隔符；修改序列化语义时必须同时升级该版本。
const AUTHORIZATION_PROFILE_DIGEST_DOMAIN: &[u8] = b"lucia-plugin-authorization-profile-v1\0";

/// 已获取的 Plugin Manager 跨进程写事务。
///
/// guard 只在 Manager 内部编排需要多步提交或回滚的 Registry 操作；句柄释放时
/// 操作系统自动解锁。
pub(crate) struct PluginManagerWriteGuard {
    _file: File,
}

/// 针对不可变 staging 副本执行的可发送 manifest 契约校验器。
pub(crate) type StagedManifestValidator<'a> =
    dyn Fn(&PluginManifest) -> Result<()> + Send + Sync + 'a;

/// Bundle 替换后的启用授权处理策略。
///
/// 普通人工替换必须重新审查精确 Bundle 身份；只有固定官方 Registry 的更新编排可以显式
/// 请求受限迁移，且 Manager 仍会校验来源主体和包含 ABI 固有权限的完整授权 profile 未变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BundleReplacementPolicy {
    /// 新 Bundle 固定保持禁用，等待用户重新审查来源、版本、摘要和能力。
    RequireBundleReview,
    /// 允许符合约束的官方 Registry 更新继承旧 Bundle 的启用状态。
    TrustedOfficialUpgrade,
}

impl PluginManager {
    /// 使用可配置根目录创建管理器；该调用本身不会访问文件系统。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回管理器根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回锁文件路径。
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_FILE_NAME)
    }

    /// 从本地 bundle 安装插件，默认保持禁用直到用户审查并明确启用。
    ///
    /// bundle 根目录必须包含 `plugin.toml` 及其指向的普通 WASM 文件。安装只持久化这两个
    /// 运行时文件，开发源码、构建缓存、测试和其他未声明文件不会进入受管理目录。
    /// 插件 ID 已存在、依赖不满足或能力冲突时不会修改现有安装状态。
    pub fn install(&self, bundle: impl AsRef<Path>) -> Result<InstalledPlugin> {
        self.install_with_options(bundle, InstallOptions::default())
    }

    /// 按指定选项从本地 bundle 安装插件。
    ///
    /// `options.enabled` 为 `false` 时仍会校验 manifest、WASM 路径和运行时 bundle 完整性，
    /// 但依赖与能力冲突会延迟到启用阶段检查。
    pub fn install_with_options(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
    ) -> Result<InstalledPlugin> {
        ensure_install_does_not_bypass_review(options)?;
        self.install_or_replace_with_source(bundle, options, None, None, None, None)
    }

    /// 从本地已验证 bundle 原子替换同 ID 插件，并默认保持禁用等待重新授权。
    ///
    /// 新 bundle 会先完成路径、manifest、WASM、依赖、能力和锁文件校验；任一步失败时保留
    /// 原版本及锁记录。成功提交新锁记录时，旧版本会进入 Installed Version History，保留
    /// 原 Bundle 用于人工回滚。
    ///
    /// # Errors
    ///
    /// 插件尚未安装、bundle 非法、依赖或能力冲突、锁文件无法原子更新时返回错误。
    pub fn replace(&self, bundle: impl AsRef<Path>) -> Result<InstalledPlugin> {
        self.replace_with_options(bundle, InstallOptions::default())
    }

    /// 从本地已验证 bundle 原子替换同 ID 插件，并显式指定安装校验选项。
    ///
    /// 人工替换始终产生新的待审 Bundle；即使 `options.enabled` 为 `true`，新身份也不会继承
    /// 旧 Bundle 的启用授权。失败时原版本和锁记录保持不变，成功后旧版本进入人工版本历史。
    ///
    /// # Errors
    ///
    /// 插件尚未安装、bundle 非法、依赖或能力校验失败、文件系统提交失败时返回错误。
    pub fn replace_with_options(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
    ) -> Result<InstalledPlugin> {
        self.replace_with_source(bundle, options, None)
    }

    /// 从目录安装 bundle，并允许可信获取层覆盖持久化来源描述。
    pub(crate) fn install_with_source(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
    ) -> Result<InstalledPlugin> {
        ensure_install_does_not_bypass_review(options)?;
        self.install_or_replace_with_source(bundle, options, source_description, None, None, None)
    }

    /// 可信启动迁移安装已经平台发行边界审核的 Bundle。
    ///
    /// 该入口不对外暴露，调用方必须是固定官方启动迁移，不得转发模型、插件或
    /// 人工安装请求。
    #[allow(dead_code)]
    pub(crate) fn install_trusted_startup_with_source(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
    ) -> Result<InstalledPlugin> {
        self.install_or_replace_with_source(bundle, options, source_description, None, None, None)
    }

    /// 在既有 Manager 事务内安装已获取 Bundle，并用 staged manifest 执行额外契约校验。
    pub(crate) fn install_with_source_in_transaction(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
        transaction: &PluginManagerWriteGuard,
        staged_manifest_validator: &StagedManifestValidator<'_>,
    ) -> Result<InstalledPlugin> {
        ensure_install_does_not_bypass_review(options)?;
        self.install_or_replace_with_source(
            bundle,
            options,
            source_description,
            None,
            Some(transaction),
            Some(staged_manifest_validator),
        )
    }

    /// 从已验证目录替换同 ID 插件；新版本完成校验后把旧版本归档到人工版本历史。
    pub(crate) fn replace_with_source(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
    ) -> Result<InstalledPlugin> {
        self.replace_with_source_policy(
            bundle,
            options,
            source_description,
            BundleReplacementPolicy::RequireBundleReview,
        )
    }

    /// 按内部授权策略替换已验证 Bundle；策略只由 Manager 自身或固定 Registry 链路选择。
    pub(crate) fn replace_with_source_policy(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
        replacement_policy: BundleReplacementPolicy,
    ) -> Result<InstalledPlugin> {
        self.install_or_replace_with_source(
            bundle,
            options,
            source_description,
            Some(replacement_policy),
            None,
            None,
        )
    }

    /// 替换 Bundle，并在进入锁文件事务前校验复制后的 staged manifest。
    pub(crate) fn replace_with_source_policy_validated(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
        replacement_policy: BundleReplacementPolicy,
        staged_manifest_validator: &StagedManifestValidator<'_>,
    ) -> Result<InstalledPlugin> {
        self.install_or_replace_with_source(
            bundle,
            options,
            source_description,
            Some(replacement_policy),
            None,
            Some(staged_manifest_validator),
        )
    }

    /// 在既有 Manager 事务内替换 Bundle，并在授权迁移前校验 staged manifest。
    pub(crate) fn replace_with_source_policy_in_transaction(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
        replacement_policy: BundleReplacementPolicy,
        transaction: &PluginManagerWriteGuard,
        staged_manifest_validator: &StagedManifestValidator<'_>,
    ) -> Result<InstalledPlugin> {
        self.install_or_replace_with_source(
            bundle,
            options,
            source_description,
            Some(replacement_policy),
            Some(transaction),
            Some(staged_manifest_validator),
        )
    }

    /// 执行新增或原子替换，共享 bundle 校验、暂存目录和锁文件事务。
    fn install_or_replace_with_source(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
        replacement_policy: Option<BundleReplacementPolicy>,
        transaction: Option<&PluginManagerWriteGuard>,
        staged_manifest_validator: Option<&StagedManifestValidator<'_>>,
    ) -> Result<InstalledPlugin> {
        self.ensure_layout()?;
        let source = bundle.as_ref();
        let source_metadata = fs::symlink_metadata(source)
            .with_context(|| format!("无法读取插件 bundle：{}", source.display()))?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
            bail!("插件 bundle 必须是非符号链接目录：{}", source.display());
        }
        let canonical_source = source
            .canonicalize()
            .with_context(|| format!("无法解析 bundle 路径：{}", source.display()))?;
        let canonical_root = self
            .root
            .canonicalize()
            .with_context(|| format!("无法解析插件管理根目录：{}", self.root.display()))?;
        if canonical_source.starts_with(&canonical_root)
            || canonical_root.starts_with(&canonical_source)
        {
            bail!("插件 bundle 与管理根目录不能相互包含");
        }
        let installed_source =
            source_description.unwrap_or_else(|| canonical_source.to_string_lossy().into_owned());
        let staging = self.create_staging_dir()?;
        let install_result = (|| -> Result<InstalledPlugin> {
            // 源目录可由另一个进程修改；先复制 manifest，再仅使用 staged
            // manifest 决定 WASM、授权、依赖和锁记录，不使用可变源身份做安全判断。
            let manifest = copy_runtime_bundle(source, &staging)?;
            validate_storage_component(&manifest.plugin.id, "plugin.id")?;
            validate_storage_component(&manifest.plugin.version, "plugin.version")?;
            if let Some(validator) = staged_manifest_validator {
                validator(&manifest)?;
            }
            let sha256 = hash_plugin_bundle(&staging)?;
            let next_authorization_digest = authorization_profile_digest(&manifest)?;

            // staging 已经与可变源目录脱钩；从这里开始持有跨进程写锁，使身份比较、依赖
            // 校验、目录提交和锁文件替换成为同一串行事务。
            let owned_transaction = if transaction.is_none() {
                Some(self.acquire_write_lock()?)
            } else {
                None
            };
            let _transaction = transaction
                .or(owned_transaction.as_ref())
                .expect("写事务必须由调用方或 Manager 获取");
            let mut lock = self.load_lock()?;
            let existing_index = lock
                .plugins
                .iter()
                .position(|plugin| plugin.id == manifest.plugin.id);
            if existing_index.is_some() && replacement_policy.is_none() {
                bail!("插件 `{}` 已安装", manifest.plugin.id);
            }
            if existing_index.is_none() && replacement_policy.is_some() {
                bail!("插件 `{}` 尚未安装，不能执行更新", manifest.plugin.id);
            }
            let previous = existing_index.map(|index| lock.plugins[index].clone());
            if let Some(index) = existing_index.filter(|index| {
                replacement_policy == Some(BundleReplacementPolicy::TrustedOfficialUpgrade)
                    && lock.plugins[*index].enabled
                    && options.enabled
            }) {
                // 已启用插件的可信更新不能借“能力变化后自动禁用”覆盖健康旧版本；先按
                // staging manifest 验证完整启用集合的依赖，再决定是否迁移授权。
                let manifests = lock
                    .plugins
                    .iter()
                    .enumerate()
                    .filter(|(_, plugin)| plugin.enabled)
                    .map(|(plugin_index, plugin)| {
                        if plugin_index == index {
                            Ok(manifest.clone())
                        } else {
                            self.inspect_locked_plugin(plugin)
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                resolve_plugin_load_order(&manifests).context("替换后启用插件的依赖关系无效")?;
            }
            let enabled = match replacement_policy {
                None => options.enabled,
                Some(BundleReplacementPolicy::RequireBundleReview) => false,
                Some(BundleReplacementPolicy::TrustedOfficialUpgrade) => {
                    let index = existing_index.expect("替换策略已经要求存在同 ID 插件");
                    let current_manifest = self.inspect_locked_plugin(&lock.plugins[index])?;
                    let previous_enabled = previous.as_ref().is_some_and(|plugin| plugin.enabled);
                    let authorization_profile_unchanged =
                        snapshot_plugin_authorization_profile(&current_manifest)?
                            == snapshot_plugin_authorization_profile(&manifest)?;
                    let approved_profile_matches = lock
                        .authorization_grants
                        .get(&manifest.plugin.id)
                        .is_some_and(|approved| approved == &next_authorization_digest);
                    let stable_manifest_identity_unchanged = current_manifest.plugin.id
                        == manifest.plugin.id
                        && current_manifest.plugin.name == manifest.plugin.name
                        && current_manifest.plugin.api_version == manifest.plugin.api_version;
                    let source_authority_unchanged = previous.as_ref().is_some_and(|previous| {
                        plugin_source_authority(&previous.source)
                            == plugin_source_authority(&installed_source)
                    });
                    // 官方标记只允许请求迁移；实际启用仍受旧状态、来源主体、稳定
                    // manifest 身份和包含 ABI 固有权限的完整授权 profile 共同约束。
                    previous_enabled
                        && options.enabled
                        && authorization_profile_unchanged
                        && approved_profile_matches
                        && stable_manifest_identity_unchanged
                        && source_authority_unchanged
                }
            };

            let plugin_parent = self.plugins_dir().join(&manifest.plugin.id);
            ensure_real_directory(&plugin_parent, true)?;
            let destination = plugin_parent.join(&manifest.plugin.version);
            if fs::symlink_metadata(&destination).is_ok() {
                bail!("插件目标目录已存在：{}", destination.display());
            }
            if hash_plugin_bundle(&staging)? != sha256 {
                bail!("staging Bundle 在校验后发生变化");
            }
            fs::rename(&staging, &destination)
                .with_context(|| format!("无法将插件移动到安装目录：{}", destination.display()))?;

            let relative_manifest = PathBuf::from("plugins")
                .join(&manifest.plugin.id)
                .join(&manifest.plugin.version)
                .join("plugin.toml");
            let installed = InstalledPlugin {
                id: manifest.plugin.id.clone(),
                name: manifest.plugin.name.clone(),
                version: manifest.plugin.version.clone(),
                api_version: manifest.plugin.api_version.clone(),
                enabled,
                manifest: path_to_lock_string(&relative_manifest)?,
                sha256,
                source: installed_source.clone(),
            };
            if let Some(index) = existing_index {
                lock.plugins[index] = installed.clone();
            } else {
                lock.plugins.push(installed.clone());
            }
            if installed.enabled {
                lock.authorization_grants
                    .insert(installed.id.clone(), next_authorization_digest);
            } else {
                lock.authorization_grants.remove(&installed.id);
                // 来源或能力身份变化后不能保留指向禁用插件的旧 owner 授权。
                lock.capability_selection
                    .retain(|_, owner| owner != &installed.id);
            }
            if let Some(previous) = previous.clone() {
                upsert_version_history(&mut lock.version_history, previous);
            }
            sort_lock_plugins(&mut lock.plugins);
            sort_version_history(&mut lock.version_history);
            if let Err(error) = self.validate_enabled_plugins(&lock) {
                let _ = fs::remove_dir_all(&destination);
                return Err(error);
            }
            if let Err(error) = self.save_lock(&lock) {
                let _ = fs::remove_dir_all(&destination);
                return Err(error);
            }
            Ok(installed)
        })();

        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        install_result
    }

    /// 返回锁文件中的全部插件，结果按 ID 排序。
    ///
    /// 该操作只读取锁文件，不执行完整性校验；需要校验时应调用 [`Self::doctor`]。
    pub fn list(&self) -> Result<Vec<InstalledPlugin>> {
        let mut plugins = self.load_lock()?.plugins;
        sort_lock_plugins(&mut plugins);
        Ok(plugins)
    }

    /// 读取并完整校验指定已安装插件的 manifest，供授权界面展示真实能力请求。
    ///
    /// # Errors
    ///
    /// 插件不存在、锁记录与 manifest 身份不一致，或 Bundle 摘要校验失败时返回错误。
    pub fn inspect(&self, plugin_id: &str) -> Result<PluginManifest> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let _manager_lock = self.acquire_write_lock()?;
        let lock = self.load_lock()?;
        let plugin = lock
            .plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        self.inspect_locked_plugin(plugin)
    }

    /// 仅在当前 Bundle 与调用方列表快照中的身份一致时返回经完整校验的 manifest。
    ///
    /// 身份比较、manifest 读取和 Bundle 摘要校验位于同一 Manager 写锁内，供授权界面确保
    /// 展示的来源、能力与后续确认绑定的是同一个 Bundle。普通只读诊断可使用 [`Self::inspect`]。
    ///
    /// # Errors
    ///
    /// 插件未安装、来源/版本/SHA-256 已在列表读取后变化，或 Bundle 完整性校验失败时返回
    /// 错误；该方法不会修改插件状态。
    pub fn inspect_if_matches(
        &self,
        plugin_id: &str,
        expected_identity: &PluginBundleIdentity,
    ) -> Result<PluginManifest> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let _manager_lock = self.acquire_write_lock()?;
        let lock = self.load_lock()?;
        let plugin = lock
            .plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        ensure_expected_bundle_identity(plugin, expected_identity)?;
        self.inspect_locked_plugin(plugin)
    }

    /// 返回不参与运行时加载的人工安装版本历史。
    ///
    /// 结果按插件 ID 和版本排序；每条记录仍绑定安装时的 Bundle SHA-256，调用方只能把它
    /// 展示为 Package/Installed Version History。
    ///
    /// # Errors
    ///
    /// 锁文件缺失时返回空列表，锁文件格式无效或读取失败时返回错误。
    pub fn list_version_history(&self) -> Result<Vec<InstalledPlugin>> {
        let mut history = self.load_lock()?.version_history;
        sort_version_history(&mut history);
        Ok(history)
    }

    /// 拒绝未绑定 Bundle 身份的旧式启用请求。
    ///
    /// 该入口仅保留 Rust 源码兼容性；新调用方必须使用 [`Self::enable_if_matches`]
    /// 传入用户确认时看到的不可变 Bundle 身份。
    ///
    /// # Errors
    ///
    /// 总是返回需要重新审查 Bundle 身份的错误，不会修改锁文件。
    #[deprecated(note = "请使用 enable_if_matches 绑定用户确认的 Bundle 身份")]
    pub fn enable(&self, plugin_id: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        bail!("启用插件必须绑定用户确认的 Bundle 身份；请刷新并重新审查来源、版本和能力")
    }

    /// 仅在当前 Bundle 仍与用户确认的不可变身份一致时启用插件。
    ///
    /// # Errors
    ///
    /// 插件未安装、来源/版本/SHA-256 已在确认后变化，或完整性、依赖及能力校验失败时返回
    /// 错误；失败不会修改锁文件。
    pub fn enable_if_matches(
        &self,
        plugin_id: &str,
        expected_identity: &PluginBundleIdentity,
    ) -> Result<InstalledPlugin> {
        self.set_enabled(plugin_id, true, Some(expected_identity))
    }

    /// 禁用指定插件；仍被其他启用插件依赖时拒绝修改。
    pub fn disable(&self, plugin_id: &str) -> Result<InstalledPlugin> {
        self.set_enabled(plugin_id, false, None)
    }

    /// 为独占能力选择已经启用的 owner。
    ///
    /// 目标插件必须已经安装、完成独立启用授权并声明该能力。选择完成后允许其他已启用
    /// 插件继续提供同一独占能力，但 Host 只会把请求路由给选中的 owner。依赖或能力校验
    /// 失败时不会写入锁文件。
    pub fn select(&self, capability_id: &str, plugin_id: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let _manager_lock = self.acquire_write_lock()?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        if !lock.plugins[index].enabled {
            bail!("插件 `{plugin_id}` 尚未启用；请先审查来源和能力并显式启用");
        }
        lock.capability_selection
            .insert(capability_id.to_owned(), plugin_id.to_owned());
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(lock.plugins[index].clone())
    }

    /// 原子启用插件并把指定独占能力路由给该插件。
    ///
    /// 该入口只能在调用方已经完成来源与能力授权确认后使用。调用方必须传入确认时看到的
    /// Bundle 身份；Manager 会在同一锁快照中比较身份，并完成依赖、Bundle 完整性与能力
    /// 冲突校验，全部通过后才一次写入锁文件。
    ///
    /// # Errors
    ///
    /// 插件未安装、Bundle 身份在确认后变化、Bundle 校验失败、依赖不满足、目标插件未声明
    /// 该独占能力，或锁文件无法原子更新时返回错误；失败不会持久化启用状态或 owner 选择。
    pub fn enable_and_select(
        &self,
        capability_id: &str,
        plugin_id: &str,
        expected_identity: &PluginBundleIdentity,
    ) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let _manager_lock = self.acquire_write_lock()?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        ensure_expected_bundle_identity(&lock.plugins[index], expected_identity)?;
        let authorization_digest =
            self.authorization_digest_for_locked_plugin(&lock.plugins[index])?;
        lock.plugins[index].enabled = true;
        lock.authorization_grants
            .insert(plugin_id.to_owned(), authorization_digest);
        lock.capability_selection
            .insert(capability_id.to_owned(), plugin_id.to_owned());
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(lock.plugins[index].clone())
    }

    /// 清除指定独占能力的 owner 选择，并返回原 owner。
    ///
    /// 清除后若仍有多个启用提供者，操作会失败且锁文件保持不变。
    pub fn clear_selection(&self, capability_id: &str) -> Result<Option<String>> {
        let _manager_lock = self.acquire_write_lock()?;
        let mut lock = self.load_lock()?;
        let previous = lock.capability_selection.remove(capability_id);
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(previous)
    }

    /// 从当前运行集合移除指定插件，并把原 Bundle 保留到人工安装版本历史。
    ///
    /// 仍被启用插件依赖时拒绝移除。成功后运行时不再加载该插件，但历史 Bundle 继续受摘要
    /// 约束，可由用户人工回滚。
    pub fn remove(&self, plugin_id: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let _manager_lock = self.acquire_write_lock()?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        let removed = lock.plugins.remove(index);
        upsert_version_history(&mut lock.version_history, removed.clone());
        sort_version_history(&mut lock.version_history);
        lock.authorization_grants.remove(plugin_id);
        lock.capability_selection
            .retain(|_, owner| owner != plugin_id);
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(removed)
    }

    /// 从 Installed Version History 人工回滚到指定固定版本。
    ///
    /// 回滚只切换锁文件中的当前版本，不下载、不构建、不执行插件。被替换的当前版本会进入
    /// 历史。人工选择不同版本不等于批准不同 Bundle，因此目标版本固定保持禁用，并清除旧
    /// Capability Owner。依赖、Bundle 摘要和版本历史会在原子写锁前重新校验。
    ///
    /// # Errors
    ///
    /// 历史版本不存在、Bundle 被篡改、依赖或 Capability 冲突、锁文件提交失败时返回错误，
    /// 当前安装状态保持不变。
    pub fn rollback(&self, plugin_id: &str, version: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        validate_storage_component(version, "插件版本")?;
        let _manager_lock = self.acquire_write_lock()?;
        let mut lock = self.load_lock()?;
        let history_index = lock
            .version_history
            .iter()
            .position(|plugin| plugin.id == plugin_id && plugin.version == version)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 没有历史版本 `{version}`"))?;
        let mut target = lock.version_history.remove(history_index);
        self.inspect_locked_plugin(&target)
            .with_context(|| format!("历史插件 `{plugin_id}` 版本 `{version}` 校验失败"))?;

        // 人工回滚改变精确 Bundle 身份，必须重新进入审批，不能继承任一历史启用位。
        target.enabled = false;
        lock.authorization_grants.remove(plugin_id);

        if let Some(active_index) = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
        {
            let current = lock.plugins.remove(active_index);
            upsert_version_history(&mut lock.version_history, current);
        }
        lock.capability_selection
            .retain(|_, owner| owner != plugin_id);
        lock.plugins.push(target.clone());
        sort_lock_plugins(&mut lock.plugins);
        sort_version_history(&mut lock.version_history);
        self.validate_enabled_plugins(&lock)?;
        self.validate_version_history(&lock)?;
        self.save_lock(&lock)?;
        Ok(target)
    }

    /// 检查锁文件、路径、manifest、WASM、SHA-256、依赖和能力冲突。
    ///
    /// 可归因于单个插件的问题会收集到报告中；锁文件无法解析等全局错误直接返回。
    pub fn doctor(&self) -> Result<DoctorReport> {
        let lock = self.load_lock()?;
        let mut report = DoctorReport {
            checked_plugins: lock.plugins.len(),
            issues: Vec::new(),
        };
        let mut ids = HashSet::new();
        let mut enabled_manifests = Vec::new();

        for plugin in &lock.plugins {
            if !ids.insert(plugin.id.clone()) {
                push_issue(
                    &mut report,
                    Some(&plugin.id),
                    format!("锁文件包含重复插件 ID `{}`", plugin.id),
                );
                continue;
            }
            let manifest = match self.inspect_locked_bundle(plugin) {
                Ok(manifest) => manifest,
                Err(error) => {
                    push_issue(
                        &mut report,
                        Some(&plugin.id),
                        format!("插件完整性检查失败：{error:#}"),
                    );
                    continue;
                }
            };
            if let Err(error) = manifest.validate() {
                if plugin.enabled {
                    push_issue(
                        &mut report,
                        Some(&plugin.id),
                        format!("插件检查失败：{error:#}"),
                    );
                } else {
                    push_warning(
                        &mut report,
                        Some(&plugin.id),
                        format!("已禁用插件与当前 Host 不兼容：{error:#}"),
                    );
                }
                continue;
            }
            if plugin.enabled {
                enabled_manifests.push(manifest);
            }
        }

        let mut versions = lock
            .plugins
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.version.clone()))
            .collect::<HashSet<_>>();
        for plugin in &lock.version_history {
            if !versions.insert((plugin.id.clone(), plugin.version.clone())) {
                push_issue(
                    &mut report,
                    Some(&plugin.id),
                    format!(
                        "版本 `{}` 同时出现在当前安装或版本历史的重复记录中",
                        plugin.version
                    ),
                );
                continue;
            }
            let manifest = match self.inspect_locked_bundle(plugin) {
                Ok(manifest) => manifest,
                Err(error) => {
                    push_issue(
                        &mut report,
                        Some(&plugin.id),
                        format!("历史版本 `{}` 完整性检查失败：{error:#}", plugin.version),
                    );
                    continue;
                }
            };
            if let Err(error) = manifest.validate() {
                push_warning(
                    &mut report,
                    Some(&plugin.id),
                    format!(
                        "历史版本 `{}` 与当前 Host 不兼容：{error:#}",
                        plugin.version
                    ),
                );
            }
        }

        if let Err(error) = resolve_plugin_load_order(&enabled_manifests) {
            push_issue(
                &mut report,
                None,
                format!("启用插件的依赖关系无效：{error:#}"),
            );
        }
        if let Err(error) =
            resolve_plugin_capabilities(&enabled_manifests, &lock.capability_selection)
        {
            push_issue(
                &mut report,
                None,
                format!("启用插件的能力声明无效：{error:#}"),
            );
        }
        self.inspect_unmanaged_entries(&lock, &mut report)?;
        Ok(report)
    }

    /// 返回经过完整性、依赖和能力检查的 Host 运行时配置。
    ///
    /// 该 API 是插件管理层与 Plugin Host 的边界；它不会实例化插件。
    pub fn runtime_config(&self) -> Result<PluginRuntimeConfig> {
        let report = self.doctor()?;
        if !report.is_healthy() {
            let messages = report
                .issues
                .iter()
                .filter(|issue| issue.severity == DoctorSeverity::Error)
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("；");
            bail!("插件目录诊断失败：{messages}");
        }
        let lock = self.load_lock()?;
        let manifest_paths = lock
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .map(|plugin| self.resolve_locked_manifest(plugin))
            .collect::<Result<Vec<_>>>()?;
        Ok(PluginRuntimeConfig {
            manifest_paths,
            capability_selection: lock.capability_selection,
            // 受管理插件的启停由锁文件 `enabled` 字段表达，不使用该列表。
            disabled_plugins: Vec::new(),
        })
    }

    fn set_enabled(
        &self,
        plugin_id: &str,
        enabled: bool,
        expected_identity: Option<&PluginBundleIdentity>,
    ) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let _manager_lock = self.acquire_write_lock()?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        if let Some(expected_identity) = expected_identity {
            ensure_expected_bundle_identity(&lock.plugins[index], expected_identity)?;
        }
        lock.plugins[index].enabled = enabled;
        if enabled {
            let authorization_digest =
                self.authorization_digest_for_locked_plugin(&lock.plugins[index])?;
            lock.authorization_grants
                .insert(plugin_id.to_owned(), authorization_digest);
        } else {
            lock.authorization_grants.remove(plugin_id);
            lock.capability_selection
                .retain(|_, owner| owner != plugin_id);
        }
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(lock.plugins[index].clone())
    }

    fn inspect_locked_plugin(&self, plugin: &InstalledPlugin) -> Result<PluginManifest> {
        let manifest = self.inspect_locked_bundle(plugin)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 校验锁定 Bundle 的路径、Manifest 身份和摘要，但不要求其 ABI 可由当前 Host 执行。
    ///
    /// 该入口用于保存旧 ABI 的版本历史和诊断已禁用插件；任何进入运行时或重新启用的 Bundle
    /// 仍必须通过 [`Self::inspect_locked_plugin`] 的当前 Host 校验。
    fn inspect_locked_bundle(&self, plugin: &InstalledPlugin) -> Result<PluginManifest> {
        let manifest_path = self.resolve_locked_manifest(plugin)?;
        let bundle_path = manifest_path
            .parent()
            .ok_or_else(|| anyhow!("锁定 manifest 缺少父目录"))?;
        let text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("无法读取插件 manifest：{}", manifest_path.display()))?;
        let manifest: PluginManifest = toml::from_str(&text)
            .with_context(|| format!("无法解析插件 manifest：{}", manifest_path.display()))?;
        if manifest.plugin.id != plugin.id
            || manifest.plugin.version != plugin.version
            || manifest.plugin.name != plugin.name
            || manifest.plugin.api_version != plugin.api_version
        {
            bail!("manifest 身份与锁文件记录不一致");
        }
        let actual_sha256 = hash_plugin_bundle(bundle_path)?;
        if actual_sha256 != plugin.sha256 {
            bail!(
                "SHA-256 不匹配：锁定值 `{}`，实际值 `{actual_sha256}`",
                plugin.sha256
            );
        }
        Ok(manifest)
    }

    /// 重新校验锁定 Bundle，并计算当前 Host 语义下的完整授权 profile 摘要。
    fn authorization_digest_for_locked_plugin(&self, plugin: &InstalledPlugin) -> Result<String> {
        let manifest = self.inspect_locked_plugin(plugin)?;
        authorization_profile_digest(&manifest)
    }

    /// 把旧 schema、缺失授权或 Host profile 已变化的启用记录降级为待重新审查。
    ///
    /// 该规范化只改变当前读取到的有效视图；任一后续写事务会用 v2 schema 持久化结果。
    /// Bundle 完整性错误仍由 doctor 单独报告，不能借降级隐藏损坏文件。
    fn normalize_authorization_state(&self, lock: &mut PluginLock) {
        let authorized = lock
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .filter_map(|plugin| {
                let expected = self.authorization_digest_for_locked_plugin(plugin).ok()?;
                lock.authorization_grants
                    .get(&plugin.id)
                    .is_some_and(|approved| approved == &expected)
                    .then(|| plugin.id.clone())
            })
            .collect::<HashSet<_>>();
        for plugin in &mut lock.plugins {
            if plugin.enabled && !authorized.contains(&plugin.id) {
                plugin.enabled = false;
            }
        }
        lock.authorization_grants
            .retain(|plugin_id, _| authorized.contains(plugin_id));
        lock.capability_selection
            .retain(|_, owner| authorized.contains(owner));
    }

    /// 校验每个有效启用记录都精确绑定当前完整授权 profile，且不存在游离授权摘要。
    fn validate_authorization_grants(&self, lock: &PluginLock) -> Result<()> {
        let mut enabled_ids = HashSet::new();
        for plugin in &lock.plugins {
            if !plugin.enabled {
                if lock.authorization_grants.contains_key(&plugin.id) {
                    bail!("禁用插件 `{}` 不能保留授权 profile 摘要", plugin.id);
                }
                continue;
            }
            enabled_ids.insert(plugin.id.as_str());
            let approved = lock
                .authorization_grants
                .get(&plugin.id)
                .ok_or_else(|| anyhow!("启用插件 `{}` 缺少授权 profile 摘要", plugin.id))?;
            let expected = self.authorization_digest_for_locked_plugin(plugin)?;
            if approved != &expected {
                bail!(
                    "插件 `{}` 的已批准授权 profile 与当前 Host 不一致",
                    plugin.id
                );
            }
        }
        for plugin_id in lock.authorization_grants.keys() {
            if !enabled_ids.contains(plugin_id.as_str()) {
                bail!("授权 profile 摘要指向未启用插件 `{plugin_id}`");
            }
        }
        Ok(())
    }

    /// 校验全部启用插件的完整性、依赖关系和能力选择。
    fn validate_enabled_plugins(&self, lock: &PluginLock) -> Result<()> {
        self.validate_authorization_grants(lock)?;
        let manifests = lock
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .map(|plugin| self.inspect_locked_plugin(plugin))
            .collect::<Result<Vec<_>>>()?;
        resolve_plugin_load_order(&manifests).context("启用插件的依赖关系无效")?;
        resolve_plugin_capabilities(&manifests, &lock.capability_selection)
            .context("启用插件的能力声明无效")?;
        Ok(())
    }

    /// 校验人工版本历史的身份唯一性与 Bundle 完整性，不把历史版本加入运行时依赖解析。
    fn validate_version_history(&self, lock: &PluginLock) -> Result<()> {
        let mut versions = lock
            .plugins
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.version.clone()))
            .collect::<HashSet<_>>();
        for plugin in &lock.version_history {
            if !versions.insert((plugin.id.clone(), plugin.version.clone())) {
                bail!(
                    "插件 `{}` 版本 `{}` 同时出现在当前安装或版本历史中",
                    plugin.id,
                    plugin.version
                );
            }
            self.inspect_locked_bundle(plugin).with_context(|| {
                format!(
                    "历史插件 `{}` 版本 `{}` 完整性校验失败",
                    plugin.id, plugin.version
                )
            })?;
        }
        Ok(())
    }

    fn resolve_locked_manifest(&self, plugin: &InstalledPlugin) -> Result<PathBuf> {
        validate_storage_component(&plugin.id, "插件 ID")?;
        validate_storage_component(&plugin.version, "插件版本")?;
        let relative = validate_relative_path(Path::new(&plugin.manifest), "锁定 manifest 路径")?;
        let expected = PathBuf::from("plugins")
            .join(&plugin.id)
            .join(&plugin.version)
            .join("plugin.toml");
        if relative != expected {
            bail!(
                "锁定 manifest 路径不符合标准目录：应为 `{}`",
                expected.display()
            );
        }
        Ok(self.root.join(relative))
    }

    /// 只读检查插件目录中未被锁文件管理的条目，并以警告形式加入报告。
    fn inspect_unmanaged_entries(
        &self,
        lock: &PluginLock,
        report: &mut DoctorReport,
    ) -> Result<()> {
        let plugins_dir = self.plugins_dir();
        let metadata = match fs::symlink_metadata(&plugins_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("无法检查插件目录元数据"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            push_issue(
                report,
                None,
                format!("插件根路径必须是非符号链接目录：{}", plugins_dir.display()),
            );
            return Ok(());
        }
        let managed = lock
            .plugins
            .iter()
            .chain(&lock.version_history)
            .map(|plugin| (plugin.id.as_str(), plugin.version.as_str()))
            .collect::<HashSet<_>>();
        for plugin_entry in fs::read_dir(&plugins_dir).context("无法读取受管理插件根目录")?
        {
            let plugin_entry = plugin_entry.context("无法读取插件目录项")?;
            if is_ignored_plugin_metadata(&plugin_entry.file_name()) {
                continue;
            }
            let plugin_path = plugin_entry.path();
            let plugin_id = plugin_entry.file_name().to_string_lossy().into_owned();
            let file_type = plugin_entry
                .file_type()
                .with_context(|| format!("无法检查插件目录项：{}", plugin_path.display()))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                push_warning(
                    report,
                    None,
                    format!(
                        "插件目录包含未受管理的非目录条目：{}",
                        plugin_path.display()
                    ),
                );
                continue;
            }
            for version_entry in fs::read_dir(&plugin_path)
                .with_context(|| format!("无法读取插件版本目录：{}", plugin_path.display()))?
            {
                let version_entry = version_entry.context("无法读取插件版本目录项")?;
                if is_ignored_plugin_metadata(&version_entry.file_name()) {
                    continue;
                }
                let version_path = version_entry.path();
                let version = version_entry.file_name().to_string_lossy().into_owned();
                if !managed.contains(&(plugin_id.as_str(), version.as_str())) {
                    push_warning(
                        report,
                        Some(&plugin_id),
                        format!("发现未被锁文件管理的插件目录：{}", version_path.display()),
                    );
                }
            }
        }
        Ok(())
    }

    /// 获取跨进程排他写锁；返回的文件句柄存活期间，其他 Manager 写事务会等待。
    ///
    /// 操作系统会在句柄 Drop 或进程退出时自动释放锁，不依赖可遗留的锁目录或 PID 文件。
    pub(crate) fn acquire_write_lock(&self) -> Result<PluginManagerWriteGuard> {
        self.ensure_layout()?;
        let lock_path = self.root.join(MANAGER_TRANSACTION_LOCK_FILE);
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "插件管理事务锁必须是非符号链接普通文件：{}",
                    lock_path.display()
                );
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("无法打开插件管理事务锁：{}", lock_path.display()))?;
        File::lock(&file)
            .with_context(|| format!("无法获取插件管理事务锁：{}", lock_path.display()))?;
        let metadata = fs::symlink_metadata(&lock_path)
            .with_context(|| format!("无法复核插件管理事务锁：{}", lock_path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("插件管理事务锁在打开后变为不安全路径");
        }
        ensure_open_lock_file_matches_path(&file, &metadata)?;
        Ok(PluginManagerWriteGuard { _file: file })
    }

    /// 在已获取的写事务中读取 Registry 批量安装的原始锁状态。
    pub(crate) fn lock_state_in_transaction(
        &self,
        _transaction: &PluginManagerWriteGuard,
    ) -> Result<PluginLock> {
        self.load_lock()
    }

    /// 在同一 Registry 写事务中回滚本次新安装的 Bundle。
    ///
    /// 调用方必须持有从规划到失败处理的同一 guard；因此旧锁状态不会覆盖其他
    /// 进程的禁用或撤权。只允许回滚该事务新增的 ID，不接受替换已有插件。
    pub(crate) fn rollback_new_installations_in_transaction(
        &self,
        original: &PluginLock,
        installed: &[InstalledPlugin],
        _transaction: &PluginManagerWriteGuard,
    ) -> Result<()> {
        let current = self.load_lock()?;
        let original_ids = original
            .plugins
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect::<HashSet<_>>();
        let installed_ids = installed
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect::<HashSet<_>>();
        for plugin in &current.plugins {
            let known = original_ids.contains(plugin.id.as_str())
                || installed_ids.contains(plugin.id.as_str());
            if !known {
                bail!(
                    "拒绝回滚：Registry 事务期间出现了未归属于本次安装的插件 `{}`",
                    plugin.id
                );
            }
        }
        let bundle_paths = installed
            .iter()
            .map(|plugin| {
                self.resolve_locked_manifest(plugin)?
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| anyhow!("已安装插件 `{}` 的 manifest 缺少父目录", plugin.id))
            })
            .collect::<Result<Vec<_>>>()?;
        self.validate_enabled_plugins(original)?;
        self.validate_version_history(original)?;
        self.save_lock(original)?;
        for bundle in bundle_paths {
            let _ = fs::remove_dir_all(&bundle);
            if let Some(parent) = bundle.parent() {
                let _ = remove_directory_if_empty(parent);
            }
        }
        Ok(())
    }

    fn load_lock(&self) -> Result<PluginLock> {
        let path = self.lock_path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PluginLock::default());
            }
            Err(error) => return Err(error).context("无法读取插件锁文件元数据"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("插件锁文件必须是非符号链接普通文件：{}", path.display());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("无法读取插件锁文件：{}", path.display()))?;
        let mut lock: PluginLock = toml::from_str(&text)
            .with_context(|| format!("无法解析插件锁文件：{}", path.display()))?;
        match lock.schema_version {
            // v1 没有持久授权摘要；旧启用位只能降级为待重新审查，不能推断用户已同意
            // 当前 Host 的完整 ABI 固有权限。
            1 => lock.schema_version = LOCK_SCHEMA_VERSION,
            LOCK_SCHEMA_VERSION => {}
            unsupported => {
                bail!("不支持插件锁文件版本 `{unsupported}`，当前版本 `{LOCK_SCHEMA_VERSION}`");
            }
        }
        self.normalize_authorization_state(&mut lock);
        Ok(lock)
    }

    fn save_lock(&self, lock: &PluginLock) -> Result<()> {
        self.ensure_layout()?;
        if lock.schema_version != LOCK_SCHEMA_VERSION {
            bail!("只能写入当前插件锁 schema `{LOCK_SCHEMA_VERSION}`");
        }
        self.validate_authorization_grants(lock)?;
        self.validate_version_history(lock)?;
        let text = toml::to_string_pretty(lock).context("无法序列化插件锁文件")?;
        let path = self.lock_path();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("插件锁文件必须是非符号链接普通文件：{}", path.display());
            }
        }
        let temporary = self.root.join(format!(".{LOCK_FILE_NAME}.tmp"));
        if let Ok(metadata) = fs::symlink_metadata(&temporary) {
            if metadata.file_type().is_symlink() {
                bail!("插件锁临时文件不能是符号链接：{}", temporary.display());
            }
            fs::remove_file(&temporary).context("无法清理旧的插件锁临时文件")?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("无法创建插件锁临时文件")?;
        if let Err(error) = (|| -> Result<()> {
            file.write_all(text.as_bytes())
                .context("无法写入插件锁临时文件")?;
            file.sync_all().context("无法同步插件锁临时文件")?;
            fs::rename(&temporary, &path).context("无法原子更新插件锁文件")?;
            Ok(())
        })() {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    fn ensure_layout(&self) -> Result<()> {
        ensure_real_directory(&self.root, true)?;
        ensure_real_directory(&self.plugins_dir(), true)
    }

    fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    fn create_staging_dir(&self) -> Result<PathBuf> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("系统时间早于 UNIX epoch")?
            .as_nanos();
        let staging = self
            .root
            .join(format!(".install-{}-{nonce}", std::process::id()));
        fs::create_dir(&staging)
            .with_context(|| format!("无法创建插件安装临时目录：{}", staging.display()))?;
        Ok(staging)
    }
}

/// macOS Finder 元数据不属于插件状态，目录诊断应稳定忽略。
fn is_ignored_plugin_metadata(name: &OsStr) -> bool {
    name == OsStr::new(".DS_Store")
}

/// 确认已锁定句柄与当前路径仍指向同一个普通文件。
///
/// macOS/Linux 等 Unix 平台比较设备号和 inode，阻止路径在打开与加锁之间被替换后让两个
/// Manager 分别锁住不同文件；其他平台至少复核句柄仍是普通文件并保持可编译。
pub(crate) fn ensure_open_lock_file_matches_path(
    file: &File,
    path_metadata: &fs::Metadata,
) -> Result<()> {
    let opened = file.metadata().context("无法读取已打开事务锁的文件身份")?;
    if !opened.is_file() || !path_metadata.is_file() {
        bail!("插件管理事务锁句柄或路径不是普通文件");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != path_metadata.dev() || opened.ino() != path_metadata.ino() {
            bail!("插件管理事务锁路径在打开后被替换");
        }
    }
    Ok(())
}

/// 对完整插件授权 profile 生成稳定、带域分隔的 SHA-256 摘要。
fn authorization_profile_digest(manifest: &PluginManifest) -> Result<String> {
    let profile = snapshot_plugin_authorization_profile(manifest)?;
    let encoded = serde_json::to_vec(&profile).context("无法序列化插件授权 profile")?;
    let mut digest = Sha256::new();
    digest.update(AUTHORIZATION_PROFILE_DIGEST_DOMAIN);
    digest.update(encoded);
    Ok(format!("{:x}", digest.finalize()))
}

/// 校验锁中的当前 Bundle 是否仍是用户确认过的来源、版本和内容。
fn ensure_expected_bundle_identity(
    plugin: &InstalledPlugin,
    expected_identity: &PluginBundleIdentity,
) -> Result<()> {
    let current_identity = plugin.bundle_identity();
    if &current_identity != expected_identity {
        bail!(
            "插件 `{}` 的 Bundle 身份已在确认后变化；请刷新并重新审查来源、版本和能力",
            plugin.id
        );
    }
    Ok(())
}

/// 防止人工或远程获取安装入口把“下载成功”当作“已授权”。
fn ensure_install_does_not_bypass_review(options: InstallOptions) -> Result<()> {
    if options.enabled {
        bail!("安装不能直接启用插件；请先安装为禁用状态，展示来源和能力后再绑定 Bundle 身份启用");
    }
    Ok(())
}

/// 返回授权绑定的来源主体；GitHub Release 的 tag 与 asset 变化不改变仓库主体。
pub(crate) fn plugin_source_authority(source: &str) -> &str {
    source
        .strip_prefix("github:")
        .and_then(|source| source.split_once('@').map(|(repository, _)| repository))
        .unwrap_or(source)
}
