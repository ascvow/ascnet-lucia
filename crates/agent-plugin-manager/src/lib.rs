//! Lucia 插件的可信获取、安装、状态管理和完整性诊断。
//!
//! 管理器负责可信来源获取和本地 bundle 状态，但不负责实例化插件。

#![deny(missing_docs)]

mod github;
mod registry;

pub use github::{
    check_github_connectivity, GithubInstallOptions, GithubInstallResult, GithubPluginSource,
    DEFAULT_GITHUB_PUBLISHER,
};
pub use registry::{
    RegistryInstallResult, RegistryOutdatedPlugin, RegistryRequest, RegistrySearchResult,
    RegistryUpdateResult,
};

use agent_plugin_host::manifest::{
    resolve_plugin_capabilities, resolve_plugin_load_order, PluginManifest, PluginRuntimeConfig,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 当前 `plugins.lock.toml` 的格式版本。
pub const LOCK_SCHEMA_VERSION: u32 = 1;

/// 插件锁文件名。
pub const LOCK_FILE_NAME: &str = "plugins.lock.toml";
/// 可归档 bundle 与稳定摘要共享的编码域。
const BUNDLE_ARCHIVE_DOMAIN: &[u8] = b"lucia-plugin-bundle-v1\0";
/// 单个可归档 bundle 的最大编码大小，避免发布控制面无界分配内存。
const MAX_BUNDLE_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
/// 单个可归档 bundle 的最大条目数。
const MAX_BUNDLE_ARCHIVE_ENTRIES: usize = 16_384;
/// 单个 bundle 相对路径的最大 UTF-8 字节数。
const MAX_BUNDLE_ARCHIVE_PATH_BYTES: usize = 4 * 1024;

/// 插件安装结果和锁文件中的持久化记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledPlugin {
    /// 插件稳定 ID。
    pub id: String,
    /// 插件显示名称。
    pub name: String,
    /// 已安装的语义化版本。
    pub version: String,
    /// 插件 ABI 版本。
    pub api_version: String,
    /// 插件当前是否启用。
    pub enabled: bool,
    /// 相对于管理根目录的 manifest 路径。
    pub manifest: String,
    /// bundle 的 SHA-256 摘要。
    pub sha256: String,
    /// 本地 bundle 绝对路径或不可变 GitHub Release 来源。
    pub source: String,
}

/// 插件锁文件的可序列化结构。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginLock {
    /// 锁文件格式版本。
    pub schema_version: u32,
    /// 当前受管理的插件记录。
    #[serde(default)]
    pub plugins: Vec<InstalledPlugin>,
    /// 独占能力 ID 到启用插件 ID 的选择。
    #[serde(default)]
    pub capability_selection: HashMap<String, String>,
}

impl Default for PluginLock {
    fn default() -> Self {
        Self {
            schema_version: LOCK_SCHEMA_VERSION,
            plugins: Vec::new(),
            capability_selection: HashMap::new(),
        }
    }
}

/// 安装行为选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallOptions {
    /// 安装完成后是否立即启用插件。
    pub enabled: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 诊断问题的严重程度。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    /// 会阻止插件可靠加载的问题。
    Error,
    /// 不阻止加载但应由用户处理的问题。
    Warning,
}

/// 单项插件诊断结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorIssue {
    /// 问题严重程度。
    pub severity: DoctorSeverity,
    /// 与问题关联的插件 ID；全局问题没有插件 ID。
    pub plugin_id: Option<String>,
    /// 可直接展示给用户的问题说明。
    pub message: String,
}

/// 插件管理目录的完整诊断报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorReport {
    /// 锁文件中检查的插件数量。
    pub checked_plugins: usize,
    /// 检查发现的全部问题。
    pub issues: Vec<DoctorIssue>,
}

impl DoctorReport {
    /// 当报告不包含错误时返回 `true`。
    pub fn is_healthy(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|issue| issue.severity == DoctorSeverity::Error)
    }
}

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
    /// bundle 根目录必须包含 `plugin.toml`，且不得包含符号链接或特殊文件。
    /// 插件 ID 已存在、依赖不满足或能力冲突时不会修改现有安装状态。
    pub fn install(&self, bundle: impl AsRef<Path>) -> Result<InstalledPlugin> {
        self.install_with_options(bundle, InstallOptions::default())
    }

    /// 按指定选项从本地 bundle 安装插件。
    ///
    /// `options.enabled` 为 `false` 时仍会校验 manifest、WASM 路径和 bundle 完整性，
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
    /// 原版本及锁记录。成功提交新锁记录后才清理旧受管理目录。调用方负责在替换前保留可用于
    /// 回滚的旧 bundle，不得把本方法当作版本归档。
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
    /// capability selection。失败时原版本和锁记录保持不变，成功后旧受管理目录才会删除。
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

    /// 从已验证目录替换同 ID 插件；新版本完成校验并写入锁文件后才清理旧目录。
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
        scan_bundle(source)?;
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
            copy_bundle(source, &staging)?;
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
            sort_lock_plugins(&mut lock.plugins);
            if let Err(error) = self.validate_enabled_plugins(&lock) {
                let _ = fs::remove_dir_all(&destination);
                return Err(error);
            }
            if let Err(error) = self.save_lock(&lock) {
                let _ = fs::remove_dir_all(&destination);
                return Err(error);
            }
            if let Some(previous) = previous.as_ref() {
                let previous_manifest = self.resolve_locked_manifest(previous)?;
                if let Some(previous_bundle) = previous_manifest.parent() {
                    if previous_bundle != destination {
                        let _ = fs::remove_dir_all(previous_bundle);
                        if let Some(parent) = previous_bundle.parent() {
                            let _ = remove_directory_if_empty(parent);
                        }
                    }
                }
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

    /// 移除指定插件及其受管理目录。
    ///
    /// 仍被启用插件依赖时拒绝移除。锁文件会先原子更新，删除失败时会返回错误，
    /// 遗留目录可由后续安装或人工诊断处理。
    pub fn remove(&self, plugin_id: &str) -> Result<InstalledPlugin> {
        validate_storage_component(plugin_id, "插件 ID")?;
        let mut lock = self.load_lock()?;
        let index = lock
            .plugins
            .iter()
            .position(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未安装"))?;
        let removed = lock.plugins.remove(index);
        lock.capability_selection
            .retain(|_, owner| owner != plugin_id);
        self.validate_enabled_plugins(&lock)?;
        self.save_lock(&lock)?;

        let manifest_path = self.resolve_locked_manifest(&removed)?;
        let bundle_path = manifest_path
            .parent()
            .ok_or_else(|| anyhow!("插件 manifest 缺少父目录"))?;
        if let Err(error) = fs::remove_dir_all(bundle_path) {
            return Err(error).with_context(|| format!("无法删除插件 `{plugin_id}` 的安装目录"));
        }
        if let Some(parent) = bundle_path.parent() {
            remove_directory_if_empty(parent)?;
        }
        Ok(removed)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone)]
struct BundleEntry {
    source: PathBuf,
    relative: PathBuf,
    normalized: String,
    kind: BundleEntryKind,
}

fn scan_bundle(root: &Path) -> Result<Vec<BundleEntry>> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("无法读取 bundle：{}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("bundle 必须是非符号链接目录：{}", root.display());
    }
    let mut entries = Vec::new();
    scan_bundle_directory(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.normalized.cmp(&right.normalized));
    Ok(entries)
}

fn scan_bundle_directory(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<BundleEntry>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("无法读取 bundle 目录：{}", directory.display()))?
    {
        let entry = entry.context("无法读取 bundle 目录项")?;
        let source = entry.path();
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("无法读取 bundle 条目：{}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("插件 bundle 不允许符号链接：{}", source.display());
        }
        let relative = source
            .strip_prefix(root)
            .context("bundle 条目逃逸出根目录")?
            .to_path_buf();
        let normalized = normalized_relative_path(&relative)?;
        let kind = if metadata.is_dir() {
            BundleEntryKind::Directory
        } else if metadata.is_file() {
            BundleEntryKind::File
        } else {
            bail!("插件 bundle 不允许特殊文件：{}", source.display());
        };
        entries.push(BundleEntry {
            source: source.clone(),
            relative,
            normalized,
            kind,
        });
        if kind == BundleEntryKind::Directory {
            scan_bundle_directory(root, &source, entries)?;
        }
    }
    Ok(())
}

fn copy_bundle(source: &Path, destination: &Path) -> Result<()> {
    for entry in scan_bundle(source)? {
        let target = destination.join(&entry.relative);
        match entry.kind {
            BundleEntryKind::Directory => fs::create_dir_all(&target)
                .with_context(|| format!("无法创建 bundle 目录：{}", target.display()))?,
            BundleEntryKind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("无法创建 bundle 文件父目录：{}", parent.display())
                    })?;
                }
                let current = fs::symlink_metadata(&entry.source).with_context(|| {
                    format!("无法重新检查 bundle 文件：{}", entry.source.display())
                })?;
                if current.file_type().is_symlink() || !current.is_file() {
                    bail!(
                        "复制期间 bundle 文件类型发生变化：{}",
                        entry.source.display()
                    );
                }
                fs::copy(&entry.source, &target)
                    .with_context(|| format!("无法复制 bundle 文件：{}", entry.source.display()))?;
            }
        }
    }
    Ok(())
}

/// 把插件目录编码为可进入 Artifact CAS 的确定性 bundle 字节。
///
/// 编码覆盖排序后的目录、普通文件、规范相对路径、文件长度和文件正文；其 SHA-256 与
/// [`hash_plugin_bundle`] 完全一致，因此同一摘要可以同时约束 Release Archive、Plugin
/// Manager 锁文件和 Genome。输入包含符号链接、特殊文件、过多条目或编码超过 256 MiB
/// 时会拒绝，不会执行 bundle 内代码。
///
/// # Errors
///
/// bundle 路径或条目不安全、文件在读取期间改变、大小超过限制或文件系统读取失败时返回
/// 错误。
pub fn pack_plugin_bundle(root: &Path) -> Result<Vec<u8>> {
    let entries = scan_bundle(root)?;
    if entries.len() > MAX_BUNDLE_ARCHIVE_ENTRIES {
        bail!(
            "插件 bundle 条目数超过限制：{} > {}",
            entries.len(),
            MAX_BUNDLE_ARCHIVE_ENTRIES
        );
    }
    let mut archive = Vec::with_capacity(BUNDLE_ARCHIVE_DOMAIN.len());
    archive.extend_from_slice(BUNDLE_ARCHIVE_DOMAIN);
    for entry in entries {
        let path_bytes = entry.normalized.as_bytes();
        if path_bytes.len() > MAX_BUNDLE_ARCHIVE_PATH_BYTES {
            bail!("插件 bundle 路径过长：{}", entry.normalized);
        }
        append_archive_bytes(
            &mut archive,
            match entry.kind {
                BundleEntryKind::Directory => b"d",
                BundleEntryKind::File => b"f",
            },
        )?;
        append_archive_bytes(&mut archive, &(path_bytes.len() as u64).to_be_bytes())?;
        append_archive_bytes(&mut archive, path_bytes)?;
        if entry.kind == BundleEntryKind::File {
            let metadata = fs::symlink_metadata(&entry.source)
                .with_context(|| format!("无法重新检查 bundle 文件：{}", entry.source.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "归档期间 bundle 文件类型发生变化：{}",
                    entry.source.display()
                );
            }
            let size = usize::try_from(metadata.len()).context("bundle 文件长度超出平台上限")?;
            append_archive_bytes(&mut archive, &metadata.len().to_be_bytes())?;
            let expected_len = archive
                .len()
                .checked_add(size)
                .context("bundle 归档长度溢出")?;
            if expected_len > MAX_BUNDLE_ARCHIVE_BYTES {
                bail!("插件 bundle 归档超过 256 MiB 限制");
            }
            archive
                .try_reserve(size)
                .context("无法为插件 bundle 归档分配内存")?;
            let before = archive.len();
            let file = File::open(&entry.source)
                .with_context(|| format!("无法读取 bundle 文件：{}", entry.source.display()))?;
            file.take(metadata.len().saturating_add(1))
                .read_to_end(&mut archive)
                .context("无法编码插件 bundle")?;
            if archive.len() - before != size {
                bail!(
                    "归档期间 bundle 文件长度发生变化：{}",
                    entry.source.display()
                );
            }
        }
    }
    let encoded_digest = format!("{:x}", Sha256::digest(&archive));
    let observed_digest = hash_plugin_bundle(root)?;
    if encoded_digest != observed_digest {
        bail!("归档期间插件 bundle 内容发生变化");
    }
    Ok(archive)
}

/// 把受信 CAS 中的确定性 bundle 字节安全还原到一个尚不存在的绝对目录。
///
/// 目标父目录必须已存在且不是符号链接，目标只能是该父目录下的单级新目录。函数先完整解析
/// 并验证排序、路径、重复项、父子类型和总大小，再创建文件；失败时尽力移除本次创建的目标。
/// 成功后会验证 `plugin.toml`、WASM 路径和重算摘要，并返回与输入字节 SHA-256 相同的十六
/// 进制摘要。函数不会加载或执行 WASM。
///
/// # Errors
///
/// 编码版本未知、路径逃逸、条目冲突、目标不安全、manifest/WASM 无效、摘要不一致或文件
/// 系统写入失败时返回错误。
pub fn unpack_plugin_bundle(archive: &[u8], destination: &Path) -> Result<String> {
    if archive.len() > MAX_BUNDLE_ARCHIVE_BYTES {
        bail!("插件 bundle 归档超过 256 MiB 限制");
    }
    let entries = parse_plugin_bundle_archive(archive)?;
    let destination = prepare_bundle_destination(destination)?;
    fs::create_dir(&destination)
        .with_context(|| format!("无法创建 bundle 目标目录：{}", destination.display()))?;
    let extracted = (|| -> Result<String> {
        for entry in &entries {
            let target = destination.join(&entry.relative);
            match entry.kind {
                BundleEntryKind::Directory => fs::create_dir_all(&target)
                    .with_context(|| format!("无法创建 bundle 目录：{}", target.display()))?,
                BundleEntryKind::File => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("无法创建 bundle 文件父目录：{}", parent.display())
                        })?;
                    }
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)
                        .with_context(|| format!("无法创建 bundle 文件：{}", target.display()))?;
                    file.write_all(entry.contents)
                        .with_context(|| format!("无法写入 bundle 文件：{}", target.display()))?;
                    file.sync_all()
                        .with_context(|| format!("无法同步 bundle 文件：{}", target.display()))?;
                }
            }
        }
        load_and_validate_manifest(&destination, &destination.join("plugin.toml"))?;
        let actual = hash_plugin_bundle(&destination)?;
        let expected = format!("{:x}", Sha256::digest(archive));
        if actual != expected {
            bail!("还原后的插件 bundle 摘要与归档不一致");
        }
        Ok(actual)
    })();
    if extracted.is_err() {
        let _ = fs::remove_dir_all(&destination);
    }
    extracted
}

/// 已完成边界校验、引用原始归档正文的单个条目。
struct ParsedBundleArchiveEntry<'a> {
    relative: PathBuf,
    kind: BundleEntryKind,
    contents: &'a [u8],
}

/// 解析并验证确定性 bundle 编码，不产生任何文件系统副作用。
fn parse_plugin_bundle_archive(archive: &[u8]) -> Result<Vec<ParsedBundleArchiveEntry<'_>>> {
    if !archive.starts_with(BUNDLE_ARCHIVE_DOMAIN) {
        bail!("插件 bundle 归档编码域无效");
    }
    let mut cursor = BUNDLE_ARCHIVE_DOMAIN.len();
    let mut entries = Vec::new();
    let mut known = BTreeMap::<String, BundleEntryKind>::new();
    let mut previous: Option<String> = None;
    while cursor < archive.len() {
        if entries.len() >= MAX_BUNDLE_ARCHIVE_ENTRIES {
            bail!("插件 bundle 归档条目数超过限制");
        }
        let kind = match take_archive_bytes(archive, &mut cursor, 1)?[0] {
            b'd' => BundleEntryKind::Directory,
            b'f' => BundleEntryKind::File,
            _ => bail!("插件 bundle 归档包含未知条目类型"),
        };
        let path_len = read_archive_u64(archive, &mut cursor)?;
        let path_len = usize::try_from(path_len).context("bundle 路径长度超出平台上限")?;
        if path_len == 0 || path_len > MAX_BUNDLE_ARCHIVE_PATH_BYTES {
            bail!("插件 bundle 归档路径长度无效");
        }
        let normalized = std::str::from_utf8(take_archive_bytes(archive, &mut cursor, path_len)?)
            .context("插件 bundle 归档路径不是 UTF-8")?
            .to_string();
        let relative = validate_relative_path(Path::new(&normalized), "bundle 归档路径")?;
        if normalized_relative_path(&relative)? != normalized {
            bail!("插件 bundle 归档路径不是规范形式：{normalized}");
        }
        if previous.as_ref().is_some_and(|value| value >= &normalized) {
            bail!("插件 bundle 归档条目未严格排序或存在重复路径");
        }
        for ancestor in relative.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            let ancestor = normalized_relative_path(ancestor)?;
            if known.get(&ancestor) == Some(&BundleEntryKind::File) {
                bail!("插件 bundle 归档把文件作为父目录：{ancestor}");
            }
        }
        let contents = if kind == BundleEntryKind::File {
            let size = read_archive_u64(archive, &mut cursor)?;
            let size = usize::try_from(size).context("bundle 文件长度超出平台上限")?;
            take_archive_bytes(archive, &mut cursor, size)?
        } else {
            &[]
        };
        previous = Some(normalized.clone());
        known.insert(normalized.clone(), kind);
        entries.push(ParsedBundleArchiveEntry {
            relative,
            kind,
            contents,
        });
    }
    if entries.is_empty() {
        bail!("插件 bundle 归档不能为空");
    }
    if !known.contains_key("plugin.toml") {
        bail!("插件 bundle 归档缺少 plugin.toml");
    }
    Ok(entries)
}

/// 校验解包目标只能是已存在真实父目录下的一个新直接子目录。
fn prepare_bundle_destination(destination: &Path) -> Result<PathBuf> {
    if !destination.is_absolute() {
        bail!("bundle 解包目标必须是绝对路径：{}", destination.display());
    }
    let name = destination
        .file_name()
        .context("bundle 解包目标缺少目录名")?;
    let name = name.to_str().context("bundle 解包目录名必须是 UTF-8")?;
    validate_storage_component(name, "bundle 解包目录名")?;
    let parent = destination.parent().context("bundle 解包目标缺少父目录")?;
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("无法读取 bundle 解包父目录：{}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("bundle 解包父目录必须是真实目录：{}", parent.display());
    }
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("无法解析 bundle 解包父目录：{}", parent.display()))?;
    let target = canonical_parent.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        bail!("bundle 解包目标已存在：{}", target.display());
    }
    Ok(target)
}

/// 在有界归档缓冲区尾部追加字节。
fn append_archive_bytes(archive: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let expected = archive
        .len()
        .checked_add(bytes.len())
        .context("bundle 归档长度溢出")?;
    if expected > MAX_BUNDLE_ARCHIVE_BYTES {
        bail!("插件 bundle 归档超过 256 MiB 限制");
    }
    archive.extend_from_slice(bytes);
    Ok(())
}

/// 从归档游标读取固定长度切片。
fn take_archive_bytes<'a>(archive: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = cursor.checked_add(len).context("bundle 归档长度溢出")?;
    let bytes = archive
        .get(*cursor..end)
        .context("插件 bundle 归档被截断")?;
    *cursor = end;
    Ok(bytes)
}

/// 从归档游标读取大端 `u64`。
fn read_archive_u64(archive: &[u8], cursor: &mut usize) -> Result<u64> {
    let bytes: [u8; 8] = take_archive_bytes(archive, cursor, 8)?
        .try_into()
        .expect("固定八字节切片必须可转换");
    Ok(u64::from_be_bytes(bytes))
}

/// 计算插件 bundle 的稳定 SHA-256 摘要。
///
/// 摘要覆盖 bundle 内所有目录、普通文件、相对路径和文件内容；符号链接、特殊文件与
/// 非法路径会被拒绝。Plugin Manager 锁文件和 Genome 运行绑定必须复用该算法，避免
/// 安装完整性与运行身份采用不同口径。
///
/// # Errors
///
/// 根目录不是普通目录、存在不安全条目、路径无法规范化或文件读取失败时返回错误。
pub fn hash_plugin_bundle(root: &Path) -> Result<String> {
    let entries = scan_bundle(root)?;
    let mut digest = Sha256::new();
    digest.update(BUNDLE_ARCHIVE_DOMAIN);
    let mut buffer = [0_u8; 64 * 1024];
    for entry in entries {
        digest.update(match entry.kind {
            BundleEntryKind::Directory => [b'd'],
            BundleEntryKind::File => [b'f'],
        });
        let path_bytes = entry.normalized.as_bytes();
        digest.update((path_bytes.len() as u64).to_be_bytes());
        digest.update(path_bytes);
        if entry.kind == BundleEntryKind::File {
            let metadata = fs::metadata(&entry.source)
                .with_context(|| format!("无法读取文件长度：{}", entry.source.display()))?;
            digest.update(metadata.len().to_be_bytes());
            let mut file = File::open(&entry.source)
                .with_context(|| format!("无法读取 bundle 文件：{}", entry.source.display()))?;
            loop {
                let read = file.read(&mut buffer).context("无法计算 bundle SHA-256")?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn load_and_validate_manifest(bundle: &Path, manifest_path: &Path) -> Result<PluginManifest> {
    let entries = scan_bundle(bundle)?;
    if !entries.iter().any(|entry| {
        entry.kind == BundleEntryKind::File && entry.relative == Path::new("plugin.toml")
    }) {
        bail!("插件 bundle 根目录缺少 plugin.toml");
    }
    let manifest = PluginManifest::load(manifest_path).context("插件 manifest 无效")?;
    let wasm_relative = validate_relative_path(Path::new(&manifest.plugin.wasm), "plugin.wasm")?;
    let wasm_path = bundle.join(wasm_relative);
    let wasm_metadata = fs::symlink_metadata(&wasm_path)
        .with_context(|| format!("插件 WASM 不存在：{}", wasm_path.display()))?;
    if wasm_metadata.file_type().is_symlink() || !wasm_metadata.is_file() {
        bail!(
            "插件 WASM 必须是 bundle 内的普通文件：{}",
            wasm_path.display()
        );
    }
    Ok(manifest)
}

fn validate_storage_component(value: &str, label: &str) -> Result<()> {
    let mut components = Path::new(value).components();
    if value.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        bail!("{label} 不能包含目录分隔或路径跳转：`{value}`");
    }
    Ok(())
}

fn validate_relative_path(path: &Path, label: &str) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("{label} 必须是非空相对路径：{}", path.display());
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("{label} 不允许目录跳转：{}", path.display());
            }
        }
    }
    Ok(result)
}

fn normalized_relative_path(path: &Path) -> Result<String> {
    let validated = validate_relative_path(path, "bundle 相对路径")?;
    validated
        .components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("bundle 路径必须是 UTF-8：{}", path.display())),
            _ => unreachable!("路径已经完成相对路径校验"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn path_to_lock_string(path: &Path) -> Result<String> {
    normalized_relative_path(path)
}

fn ensure_real_directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("受管理目录不能是符号链接：{}", path.display());
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("受管理路径不是目录：{}", path.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir_all(path)
                .with_context(|| format!("无法创建受管理目录：{}", path.display()))
        }
        Err(error) => Err(error).with_context(|| format!("无法检查目录：{}", path.display())),
    }
}

fn remove_directory_if_empty(path: &Path) -> Result<()> {
    let mut entries =
        fs::read_dir(path).with_context(|| format!("无法检查目录是否为空：{}", path.display()))?;
    if entries.next().is_none() {
        fs::remove_dir(path).with_context(|| format!("无法删除空目录：{}", path.display()))?;
    }
    Ok(())
}

fn sort_lock_plugins(plugins: &mut [InstalledPlugin]) {
    plugins.sort_by(|left, right| left.id.cmp(&right.id));
}

fn push_issue(report: &mut DoctorReport, plugin_id: Option<&str>, message: String) {
    report.issues.push(DoctorIssue {
        severity: DoctorSeverity::Error,
        plugin_id: plugin_id.map(str::to_owned),
        message,
    });
}

/// 向诊断报告追加不会阻止插件加载的警告。
fn push_warning(report: &mut DoctorReport, plugin_id: Option<&str>, message: String) {
    report.issues.push(DoctorIssue {
        severity: DoctorSeverity::Warning,
        plugin_id: plugin_id.map(str::to_string),
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// 确定性归档必须与安装摘要一致，并可无损恢复为 Plugin Manager 可安装目录。
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
        assert_eq!(installed.sha256, expected);
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

        let error =
            unpack_plugin_bundle(&archive, &destination).expect_err("父目录逃逸归档必须拒绝");

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

    /// 公共本地替换入口必须先提交新锁记录，再删除旧受管理 bundle。
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
        assert!(!directory.path.join("managed/plugins/echo/1.0.0").exists());
        assert!(directory
            .path
            .join("managed/plugins/echo/2.0.0/plugin.toml")
            .is_file());
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
                .join("managed/plugins/echo/1.0.0/assets/config.txt"),
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
                .join("managed/plugins/echo/1.0.0/assets/config.txt"),
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
    fn remove_updates_lock_and_deletes_bundle() {
        let directory = TestDirectory::new("remove");
        let bundle = create_bundle(&directory.path, "echo", "1.0.0", &[], None);
        let manager = PluginManager::new(directory.path.join("managed"));
        manager.install(bundle).expect("插件应安装成功");

        let removed = manager.remove("echo").expect("插件应移除成功");
        assert_eq!(removed.id, "echo");
        assert!(manager.list().expect("锁文件应可读").is_empty());
        assert!(!directory.path.join("managed/plugins/echo").exists());
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
        let manager = PluginManager::new(directory.path.join("managed"));

        let error = manager.install(bundle).expect_err("符号链接必须被拒绝");
        assert!(error.to_string().contains("不允许符号链接"));
    }
}
