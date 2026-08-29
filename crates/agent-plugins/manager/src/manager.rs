//! 受管理插件的安装、替换、启停、回滚与锁文件事务。

use crate::bundle::{
    copy_runtime_bundle, ensure_real_directory, hash_plugin_bundle, load_and_validate_manifest,
    pack_plugin_bundle, path_to_lock_string, remove_directory_if_empty, sort_lock_plugins,
    sort_version_history, unpack_plugin_bundle, upsert_version_history, validate_relative_path,
    validate_storage_component,
};
use crate::doctor::{push_issue, push_warning, DoctorReport};
use crate::types::{
    InstallOptions, InstalledPlugin, PluginLock, PluginManagerStateSnapshot, LOCK_FILE_NAME,
    LOCK_SCHEMA_VERSION,
};
use agent_plugin_host::manifest::{
    resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest, PluginRuntimeConfig,
};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
/// 管理本地 Lucia 插件目录。
#[derive(Debug, Clone)]
pub struct PluginManager {
    root: PathBuf,
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

    /// 从本地 bundle 安装并立即启用插件。
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
        self.install_with_source(bundle, options, None)
    }

    /// 从本地已验证 bundle 原子替换同 ID 插件，并默认启用新版本。
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

    /// 从本地已验证 bundle 原子替换同 ID 插件，并显式指定替换后的启用状态。
    ///
    /// 该入口供受信发布控制面使用；它不会下载、构建、执行或签名插件，也不会自行扩大
    /// capability selection。失败时原版本和锁记录保持不变，成功后旧版本进入人工版本历史。
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
        self.install_or_replace_with_source(bundle, options, source_description, false)
    }

    /// 从已验证目录替换同 ID 插件；新版本完成校验后把旧版本归档到人工版本历史。
    pub(crate) fn replace_with_source(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
    ) -> Result<InstalledPlugin> {
        self.install_or_replace_with_source(bundle, options, source_description, true)
    }

    /// 执行新增或原子替换，共享 bundle 校验、暂存目录和锁文件事务。
    fn install_or_replace_with_source(
        &self,
        bundle: impl AsRef<Path>,
        options: InstallOptions,
        source_description: Option<String>,
        replace: bool,
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

        let source_manifest_path = source.join("plugin.toml");
        let manifest = load_and_validate_manifest(source, &source_manifest_path)?;
        validate_storage_component(&manifest.plugin.id, "plugin.id")?;
        validate_storage_component(&manifest.plugin.version, "plugin.version")?;

        let mut lock = self.load_lock()?;
        let existing_index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == manifest.plugin.id);
        if existing_index.is_some() && !replace {
            bail!("插件 `{}` 已安装", manifest.plugin.id);
        }
        if existing_index.is_none() && replace {
            bail!("插件 `{}` 尚未安装，不能执行更新", manifest.plugin.id);
        }
        let previous = existing_index.map(|index| lock.plugins[index].clone());

        let plugin_parent = self.plugins_dir().join(&manifest.plugin.id);
        ensure_real_directory(&plugin_parent, true)?;
        let destination = plugin_parent.join(&manifest.plugin.version);
        if fs::symlink_metadata(&destination).is_ok() {
            bail!("插件目标目录已存在：{}", destination.display());
        }

        let staging = self.create_staging_dir()?;
        let install_result = (|| -> Result<InstalledPlugin> {
            copy_runtime_bundle(source, &staging, &manifest)?;
            let staging_manifest_path = staging.join("plugin.toml");
            let staged_manifest = load_and_validate_manifest(&staging, &staging_manifest_path)?;
            if staged_manifest.plugin.id != manifest.plugin.id
                || staged_manifest.plugin.version != manifest.plugin.version
            {
                bail!("复制后的插件身份与源 manifest 不一致");
            }
            let sha256 = hash_plugin_bundle(&staging)?;
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
                enabled: options.enabled,
                manifest: path_to_lock_string(&relative_manifest)?,
                sha256,
                source: source_description
                    .unwrap_or_else(|| canonical_source.to_string_lossy().into_owned()),
            };
            if let Some(index) = existing_index {
                lock.plugins[index] = installed.clone();
            } else {
                lock.plugins.push(installed.clone());
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

    /// 捕获当前锁文件及全部受管理 Bundle，供人工管理事务失败时恢复。
    ///
    /// 快照不会读取未被锁文件管理的目录，也不会执行 WASM。每个 Bundle 都会重新计算摘要，
    /// 因而篡改或缺失会在任何管理写操作发生前阻止事务开始。
    ///
    /// # Errors
    ///
    /// 锁文件无效、受管理 Bundle 缺失或篡改、归档超过安全上限或文件系统读取失败时返回错误。
    pub fn capture_state(&self) -> Result<PluginManagerStateSnapshot> {
        let lock = self.load_lock()?;
        let mut bundles = BTreeMap::new();
        for plugin in lock.plugins.iter().chain(&lock.version_history) {
            let manifest = self.resolve_locked_manifest(plugin)?;
            let bundle = manifest
                .parent()
                .ok_or_else(|| anyhow!("锁定 manifest 缺少 Bundle 根目录"))?;
            let archive = pack_plugin_bundle(bundle)
                .with_context(|| format!("归档插件 `{}` 的当前 Bundle 失败", plugin.id))?;
            let digest = format!("{:x}", Sha256::digest(&archive));
            if digest != plugin.sha256 {
                bail!("插件 `{}` 的归档摘要与锁文件不一致", plugin.id);
            }
            let key = (plugin.id.clone(), plugin.version.clone());
            if bundles.insert(key, archive).is_some() {
                bail!(
                    "插件 `{}` 版本 `{}` 在当前安装与版本历史中重复",
                    plugin.id,
                    plugin.version
                );
            }
        }
        Ok(PluginManagerStateSnapshot { lock, bundles })
    }

    /// 恢复此前捕获的人工插件管理状态。
    ///
    /// 恢复先补齐并验证旧 Bundle，再原子写回旧锁文件；因此失败不会让锁文件指向尚未恢复的
    /// Bundle。操作后产生但不再受管理的版本目录会尽力清理，清理失败只保留为可诊断残留。
    ///
    /// # Errors
    ///
    /// 快照归档损坏、目标路径被不一致内容占用、旧依赖或能力关系无法恢复，或锁文件提交失败
    /// 时返回错误。失败可能留下尚未被锁文件引用的新版本目录，但不会把它装载为运行依赖。
    pub fn restore_state(&self, snapshot: &PluginManagerStateSnapshot) -> Result<()> {
        self.ensure_layout()?;
        let current = self.load_lock()?;
        for plugin in snapshot
            .lock
            .plugins
            .iter()
            .chain(&snapshot.lock.version_history)
        {
            let archive = snapshot
                .bundles
                .get(&(plugin.id.clone(), plugin.version.clone()))
                .ok_or_else(|| {
                    anyhow!(
                        "插件 `{}` 版本 `{}` 的恢复快照缺少 Bundle",
                        plugin.id,
                        plugin.version
                    )
                })?;
            let archive_digest = format!("{:x}", Sha256::digest(archive));
            if archive_digest != plugin.sha256 {
                bail!("插件 `{}` 的恢复快照摘要无效", plugin.id);
            }
            let destination = self.plugins_dir().join(&plugin.id).join(&plugin.version);
            match fs::symlink_metadata(&destination) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        bail!(
                            "插件恢复目标必须是非符号链接目录：{}",
                            destination.display()
                        );
                    }
                    let observed = hash_plugin_bundle(&destination)?;
                    if observed != plugin.sha256 {
                        bail!("插件 `{}` 的恢复目标已被不一致内容占用", plugin.id);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let parent = destination
                        .parent()
                        .ok_or_else(|| anyhow!("插件恢复目标缺少父目录"))?;
                    ensure_real_directory(parent, true)?;
                    unpack_plugin_bundle(archive, &destination)
                        .with_context(|| format!("恢复插件 `{}` Bundle 失败", plugin.id))?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("检查插件恢复目标失败：{}", destination.display())
                    });
                }
            }
        }
        self.validate_enabled_plugins(&snapshot.lock)?;
        self.validate_version_history(&snapshot.lock)?;
        self.save_lock(&snapshot.lock)?;
        for plugin in current.plugins.iter().chain(&current.version_history) {
            let retained = snapshot
                .lock
                .plugins
                .iter()
                .chain(&snapshot.lock.version_history)
                .any(|item| item.id == plugin.id && item.version == plugin.version);
            if retained {
                continue;
            }
            if let Ok(manifest) = self.resolve_locked_manifest(plugin) {
                if let Some(bundle) = manifest.parent() {
                    let _ = fs::remove_dir_all(bundle);
                    if let Some(parent) = bundle.parent() {
                        let _ = remove_directory_if_empty(parent);
                    }
                }
            }
        }
        Ok(())
    }

    /// 启用指定插件，并在写入锁文件前检查依赖和能力冲突。
    pub fn enable(&self, plugin_id: &str) -> Result<InstalledPlugin> {
        self.set_enabled(plugin_id, true)
    }

    /// 禁用指定插件；仍被其他启用插件依赖时拒绝修改。
    pub fn disable(&self, plugin_id: &str) -> Result<InstalledPlugin> {
        self.set_enabled(plugin_id, false)
    }

    /// 为独占能力选择 owner，并原子启用目标插件。
    ///
    /// 目标插件必须已经安装并声明该能力。选择完成后允许其他已启用插件继续提供
    /// 同一独占能力，但 Host 只会把请求路由给选中的 owner。依赖或能力校验失败时
    /// 不会写入锁文件。
    pub fn select(&self, capability_id: &str, plugin_id: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        lock.plugins[index].enabled = true;
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
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        let removed = lock.plugins.remove(index);
        upsert_version_history(&mut lock.version_history, removed.clone());
        sort_version_history(&mut lock.version_history);
        lock.capability_selection
            .retain(|_, owner| owner != plugin_id);
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(removed)
    }

    /// 从 Installed Version History 人工回滚到指定固定版本。
    ///
    /// 回滚只切换锁文件中的当前版本，不下载、不构建、不执行插件。被替换的当前版本会进入
    /// 历史；已有插件保持当前启停状态，被移除插件恢复时采用其归档时状态。依赖、Capability
    /// Owner 和 Bundle 摘要会在原子写锁前重新校验。
    ///
    /// # Errors
    ///
    /// 历史版本不存在、Bundle 被篡改、依赖或 Capability 冲突、锁文件提交失败时返回错误，
    /// 当前安装状态保持不变。
    pub fn rollback(&self, plugin_id: &str, version: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        validate_storage_component(version, "插件版本")?;
        let mut lock = self.load_lock()?;
        let history_index = lock
            .version_history
            .iter()
            .position(|plugin| plugin.id == plugin_id && plugin.version == version)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 没有历史版本 `{version}`"))?;
        let mut target = lock.version_history.remove(history_index);
        self.inspect_locked_plugin(&target)
            .with_context(|| format!("历史插件 `{plugin_id}` 版本 `{version}` 校验失败"))?;

        if let Some(active_index) = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
        {
            let current = lock.plugins.remove(active_index);
            target.enabled = current.enabled;
            upsert_version_history(&mut lock.version_history, current);
        }
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
            match self.inspect_locked_plugin(plugin) {
                Ok(manifest) => {
                    if plugin.enabled {
                        enabled_manifests.push(manifest);
                    }
                }
                Err(error) => push_issue(
                    &mut report,
                    Some(&plugin.id),
                    format!("插件检查失败：{error:#}"),
                ),
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
            if let Err(error) = self.inspect_locked_plugin(plugin) {
                push_issue(
                    &mut report,
                    Some(&plugin.id),
                    format!("历史版本 `{}` 检查失败：{error:#}", plugin.version),
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

    fn set_enabled(&self, plugin_id: &str, enabled: bool) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        lock.plugins[index].enabled = enabled;
        if !enabled {
            lock.capability_selection
                .retain(|_, owner| owner != plugin_id);
        }
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;
        Ok(lock.plugins[index].clone())
    }

    fn inspect_locked_plugin(&self, plugin: &InstalledPlugin) -> Result<PluginManifest> {
        let manifest_path = self.resolve_locked_manifest(plugin)?;
        let bundle_path = manifest_path
            .parent()
            .ok_or_else(|| anyhow!("锁定 manifest 缺少父目录"))?;
        let manifest = load_and_validate_manifest(bundle_path, &manifest_path)?;
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

    /// 校验全部启用插件的完整性、依赖关系和能力选择。
    fn validate_enabled_plugins(&self, lock: &PluginLock) -> Result<()> {
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
            self.inspect_locked_plugin(plugin).with_context(|| {
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
        let lock: PluginLock = toml::from_str(&text)
            .with_context(|| format!("无法解析插件锁文件：{}", path.display()))?;
        if lock.schema_version != LOCK_SCHEMA_VERSION {
            bail!(
                "不支持插件锁文件版本 `{}`，当前版本 `{LOCK_SCHEMA_VERSION}`",
                lock.schema_version
            );
        }
        Ok(lock)
    }

    fn save_lock(&self, lock: &PluginLock) -> Result<()> {
        self.ensure_layout()?;
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
