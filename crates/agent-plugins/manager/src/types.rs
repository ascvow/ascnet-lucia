//! 插件锁文件与安装记录的稳定数据结构。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
/// 当前 `plugins.lock.toml` 的格式版本。
pub const LOCK_SCHEMA_VERSION: u32 = 1;

/// 插件锁文件名。
pub const LOCK_FILE_NAME: &str = "plugins.lock.toml";

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
    /// 已归档但不参与运行时加载的人工安装版本历史。
    ///
    /// 该字段只支持人工回滚，不表示自动升级链路。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_history: Vec<InstalledPlugin>,
    /// 独占能力 ID 到启用插件 ID 的选择。
    #[serde(default)]
    pub capability_selection: HashMap<String, String>,
}

/// 一次人工插件管理操作前捕获的可恢复状态。
///
/// 快照包含锁文件和每个受管理 Bundle 的确定性归档，仅用于应用层在后续人工基线提交失败时
/// 执行补偿恢复。它不会自行安装或选择版本。
#[derive(Debug, Clone)]
pub struct PluginManagerStateSnapshot {
    /// 快照时刻的锁文件内容。
    pub(crate) lock: PluginLock,
    /// 快照时刻各插件 Bundle 的归档字节，按 (插件 ID, 版本) 索引。
    pub(crate) bundles: BTreeMap<(String, String), Vec<u8>>,
}

impl Default for PluginLock {
    fn default() -> Self {
        Self {
            schema_version: LOCK_SCHEMA_VERSION,
            plugins: Vec::new(),
            version_history: Vec::new(),
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
